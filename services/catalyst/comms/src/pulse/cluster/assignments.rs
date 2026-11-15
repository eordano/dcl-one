//! Pulse's process-local assignment and unfinished takeover ledger.
//! Lossy feed publication never owns or discards these authority facts.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use crate::protocol::pulse::decentraland::pulse::PeerClusterSnapshot;

#[derive(Debug, Clone)]
pub struct CurrentAssignment {
    pub wallet: String,
    pub session: String,
    pub cluster_id: String,
    pub realm: String,
    pub owner_epoch: u64,
    pub selected_origin: i32,
    pub owner_peer: u32,
    pub registration: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
}

impl PartialEq for CurrentAssignment {
    fn eq(&self, other: &Self) -> bool {
        (
            &self.wallet,
            &self.session,
            &self.cluster_id,
            &self.realm,
            self.owner_epoch,
            self.selected_origin,
            self.owner_peer,
        ) == (
            &other.wallet,
            &other.session,
            &other.cluster_id,
            &other.realm,
            other.owner_epoch,
            other.selected_origin,
            other.owner_peer,
        )
    }
}
impl Eq for CurrentAssignment {}

// Distinct native, WebTransport and control-position wallets may coexist.
// Reserve a second complete cohort for departures inside the retention window.
// This is finite backpressure, never permission to discard unfinished recovery.
const SIMULTANEOUS_WALLETS: usize =
    super::control::SUPPORTED_TRANSPORT_PEERS + super::control::DEFAULT_CONTROL_POSITION_CAPACITY;
const RECOVERY_CHURN_RESERVE: usize = SIMULTANEOUS_WALLETS;
const RECOVERY_WALLETS: usize = SIMULTANEOUS_WALLETS + RECOVERY_CHURN_RESERVE;
const RECOVERY_ROOMS: usize = 32;
const RETENTION_PASSES: u64 = 300;
const FLOOR_GRACE_SECONDS: u64 = 15;

struct RecoveryRecord {
    current: Option<(CurrentAssignment, u64)>,
    potential: Option<CurrentAssignment>,
    retained: Option<(CurrentAssignment, u64)>,
    pending: Vec<(
        crate::protocol::pulse::decentraland::pulse::PendingTakeover,
        u64,
    )>,
    floor: u64,
    last_pass: u64,
    blocked: bool,
    legacy_invalidated: bool,
}

pub(crate) struct AssignmentView {
    observed: tokio::time::Instant,
    valid_for: Duration,
    pass: u64,
    next_version: u64,
    hint_cursor: usize,
    records: HashMap<String, RecoveryRecord>,
}

impl AssignmentView {
    pub(crate) fn invalidate_legacy(&mut self, wallet: &str) {
        if let Some(record) = self.records.get_mut(wallet)
            && record
                .current
                .as_ref()
                .is_some_and(|(a, _)| a.selected_origin != 1)
        {
            record.legacy_invalidated = true;
        }
    }
}

/// Pulse's nonpersistent authority. Lossy hints never own recovery state.
pub struct CurrentAssignments {
    incarnation: String,
    native: crate::pulse::snapshot::NativeCurrent,
    view: Mutex<AssignmentView>,
}

impl Default for CurrentAssignments {
    fn default() -> Self {
        Self {
            incarnation: uuid::Uuid::new_v4().to_string(),
            native: Default::default(),
            view: Mutex::new(AssignmentView {
                observed: tokio::time::Instant::now(),
                valid_for: Duration::ZERO,
                pass: 0,
                next_version: 0,
                hint_cursor: 0,
                records: HashMap::new(),
            }),
        }
    }
}

fn unix_seconds() -> u64 {
    chrono::Utc::now().timestamp().max(0) as u64
}

impl CurrentAssignments {
    pub(crate) fn native_current(&self) -> crate::pulse::snapshot::NativeCurrent {
        self.native.clone()
    }
    pub(crate) fn lifecycle_view(&self) -> Option<std::sync::MutexGuard<'_, AssignmentView>> {
        self.view.lock().ok()
    }

    pub fn replace(
        &self,
        pass: u64,
        valid_for: Duration,
        assignments: Vec<CurrentAssignment>,
    ) -> Vec<PeerClusterSnapshot> {
        self.replace_checked(pass, valid_for, assignments, None)
    }

    pub(crate) fn replace_checked(
        &self,
        pass: u64,
        valid_for: Duration,
        assignments: Vec<CurrentAssignment>,
        control: Option<&Mutex<super::control::ControlPositions>>,
    ) -> Vec<PeerClusterSnapshot> {
        let Ok(native) = self.native.lock() else {
            return Vec::new();
        };
        let control = match control {
            Some(control) => match control.lock() {
                Ok(guard) => Some(guard),
                Err(_) => return Vec::new(),
            },
            None => None,
        };
        let Ok(mut view) = self.view.lock() else {
            return Vec::new();
        };
        let assignments = assignments
            .into_iter()
            .filter(|a| selection_live(a, &native, control.as_deref()))
            .collect::<Vec<_>>();
        view.observed = tokio::time::Instant::now();
        view.valid_for = valid_for;
        view.pass = pass;
        let mut desired: HashMap<_, _> = assignments
            .into_iter()
            .map(|mut a| {
                a.wallet.make_ascii_lowercase();
                a.session.make_ascii_lowercase();
                (a.wallet.clone(), a)
            })
            .collect();
        let now = unix_seconds();
        // Account for every departure before reclaiming or admitting any new
        // identity. Wallet sort order must not keep safely expired records in
        // front of a new wallet that sorts earlier.
        let mut departed = Vec::new();
        for (wallet, record) in &mut view.records {
            if !desired.contains_key(wallet)
                && let Some(previous) = record.current.take()
            {
                record.retained = Some(previous);
                departed.push(wallet.clone());
            }
        }
        view.records.retain(|_, r| {
            r.current.is_some()
                || r.blocked
                || !r.pending.is_empty()
                || pass.saturating_sub(r.last_pass) <= RETENTION_PASSES
                || now <= r.floor.saturating_add(FLOOR_GRACE_SECONDS)
        });
        let mut wallets: Vec<_> = view.records.keys().cloned().collect();
        for wallet in desired.keys() {
            if !view.records.contains_key(wallet) {
                wallets.push(wallet.clone());
            }
        }
        wallets.sort();
        let mut hints: Vec<_> = departed
            .into_iter()
            .filter_map(|wallet| self.snapshot(&mut view, &wallet, &native, control.as_deref()))
            .collect();
        let mut refused = 0;
        for wallet in wallets {
            let assignment = desired.remove(&wallet);
            if !view.records.contains_key(&wallet) {
                if view.records.len() >= RECOVERY_WALLETS {
                    refused += 1;
                    continue;
                }
                view.records.insert(
                    wallet.clone(),
                    RecoveryRecord {
                        current: None,
                        potential: None,
                        retained: None,
                        pending: Vec::new(),
                        floor: 0,
                        last_pass: pass,
                        blocked: false,
                        legacy_invalidated: false,
                    },
                );
            }
            let unchanged =
                view.records[&wallet].current.as_ref().map(|(a, _)| a) == assignment.as_ref();
            if unchanged
                && (assignment.is_none()
                    || (!view.records[&wallet].blocked
                        && !view.records[&wallet].legacy_invalidated))
            {
                if assignment.is_some() {
                    view.records.get_mut(&wallet).unwrap().last_pass = pass;
                }
                continue;
            }
            let version = if assignment.is_some() {
                view.next_version.checked_add(1)
            } else {
                Some(view.next_version)
            };
            if assignment.is_some()
                && let Some(version) = version
            {
                view.next_version = version;
            }
            let record = view.records.get_mut(&wallet).unwrap();
            if let Some(previous) = record.current.take() {
                record.retained = Some(previous);
            }
            record.blocked = false;
            if let Some(previous) = record.potential.as_ref().filter(|previous| {
                assignment
                    .as_ref()
                    .is_some_and(|a| !a.session.eq_ignore_ascii_case(&previous.session))
            }) {
                if record.pending.len() >= RECOVERY_ROOMS {
                    record.blocked = true;
                } else {
                    record.pending.push((
                        crate::protocol::pulse::decentraland::pulse::PendingTakeover {
                            takeover_id: uuid::Uuid::new_v4().to_string(),
                            displaced_session: previous.session.clone(),
                            displaced_cluster_id: previous.cluster_id.clone(),
                            displaced_realm: previous.realm.clone(),
                        },
                        record.floor,
                    ));
                    record.potential = None;
                }
            }
            if let Some(assignment) = assignment {
                record.last_pass = pass;
                if let Some(version) = version {
                    record.retained = Some((assignment.clone(), version));
                    record.current = Some((assignment, version));
                    record.legacy_invalidated = false;
                } else {
                    record.blocked = true;
                }
            }
            if let Some(snapshot) = self.snapshot(&mut view, &wallet, &native, control.as_deref()) {
                hints.push(snapshot);
            }
        }
        crate::pulse::metrics::recovery_capacity(view.records.len(), refused);
        if refused > 0 {
            tracing::warn!(
                refused,
                retained = view.records.len(),
                capacity = RECOVERY_WALLETS,
                "Pulse recovery ledger exhausted; new wallet assignments withheld without evicting safety state"
            );
        }
        hints
    }

    fn snapshot(
        &self,
        view: &mut AssignmentView,
        wallet: &str,
        native: &HashMap<String, crate::pulse::snapshot::NativeRegistration>,
        control: Option<&super::control::ControlPositions>,
    ) -> Option<PeerClusterSnapshot> {
        use prost::Message;
        let record = view.records.get_mut(wallet)?;
        let active = view.observed.elapsed() < view.valid_for
            && !record.blocked
            && !record.legacy_invalidated
            && record
                .current
                .as_ref()
                .is_some_and(|(a, _)| selection_live(a, native, control));
        let (assignment, version) = if active {
            record.current.as_ref()?
        } else if !record.pending.is_empty() {
            record.retained.as_ref()?
        } else {
            return None;
        };
        let mut snapshot = PeerClusterSnapshot {
            selected_origin: assignment.selected_origin,
            selected_identity: Some(assignment.owner_epoch),
            assignment_state: if active { 1 } else { 2 },
            wallet: assignment.wallet.clone(),
            session: assignment.session.clone(),
            cluster_id: assignment.cluster_id.clone(),
            realm: assignment.realm.clone(),
            pass: view.pass,
            source_incarnation: self.incarnation.clone(),
            assignment_version: *version,
            pending_takeovers: record.pending.iter().map(|(p, _)| p.clone()).collect(),
            token_not_before: (!record.blocked || !record.pending.is_empty())
                .then_some(record.floor),
        };
        if snapshot.encoded_len() > crate::protocol::bus::cluster::MAX_REPLY_BYTES {
            // No incomplete recovery plan can become ready.
            return None;
        }
        if active && snapshot.token_not_before.is_some() && snapshot.pending_takeovers.is_empty() {
            record.potential = Some(assignment.clone());
        }
        if record.blocked {
            snapshot.token_not_before = None;
        }
        Some(snapshot)
    }

    pub fn lookup(&self, wallet: &str, session: &str) -> Option<PeerClusterSnapshot> {
        self.lookup_checked(wallet, session, None)
    }
    pub(crate) fn lookup_checked(
        &self,
        wallet: &str,
        session: &str,
        control: Option<&Mutex<super::control::ControlPositions>>,
    ) -> Option<PeerClusterSnapshot> {
        let native = self.native.lock().ok()?;
        let control = match control {
            Some(control) => Some(control.lock().ok()?),
            None => None,
        };
        let mut view = self.view.lock().ok()?;
        let snapshot = self.snapshot(&mut view, wallet, &native, control.as_deref())?;
        (snapshot.session == session).then_some(snapshot)
    }

    #[cfg(test)]
    pub(crate) fn retained_hints(&self, budget: usize) -> Vec<PeerClusterSnapshot> {
        self.retained_hints_checked(budget, None)
    }
    pub(crate) fn retained_hints_checked(
        &self,
        budget: usize,
        control: Option<&Mutex<super::control::ControlPositions>>,
    ) -> Vec<PeerClusterSnapshot> {
        let Ok(native) = self.native.lock() else {
            return Vec::new();
        };
        let control = match control {
            Some(control) => match control.lock() {
                Ok(guard) => Some(guard),
                Err(_) => return Vec::new(),
            },
            None => None,
        };
        let Ok(mut view) = self.view.lock() else {
            return Vec::new();
        };
        let mut wallets: Vec<_> = view
            .records
            .iter()
            .filter(|(_, r)| !r.pending.is_empty())
            .map(|(w, _)| w.clone())
            .collect();
        wallets.sort();
        if wallets.is_empty() {
            view.hint_cursor = 0;
            return Vec::new();
        }
        let count = budget.min(wallets.len());
        let start = view.hint_cursor % wallets.len();
        view.hint_cursor = (start + count) % wallets.len();
        (0..count)
            .filter_map(|offset| {
                self.snapshot(
                    &mut view,
                    &wallets[(start + offset) % wallets.len()],
                    &native,
                    control.as_deref(),
                )
            })
            .collect()
    }

    pub fn complete(
        &self,
        wallet: &str,
        completion: &crate::protocol::pulse::decentraland::pulse::TakeoverCompleted,
    ) -> Option<PeerClusterSnapshot> {
        self.complete_checked(wallet, completion, None)
    }
    pub(crate) fn complete_checked(
        &self,
        wallet: &str,
        completion: &crate::protocol::pulse::decentraland::pulse::TakeoverCompleted,
        control: Option<&Mutex<super::control::ControlPositions>>,
    ) -> Option<PeerClusterSnapshot> {
        let native = self.native.lock().ok()?;
        let control = match control {
            Some(control) => Some(control.lock().ok()?),
            None => None,
        };
        let mut view = self.view.lock().ok()?;
        let record = view.records.get_mut(wallet)?;
        let index = record
            .pending
            .iter()
            .position(|(p, _)| p.takeover_id == completion.takeover_id)?;
        if completion.revoke_before == 0
            || completion.revoke_before > crate::protocol::bus::cluster::MAX_CONSUMER_SECONDS
            || completion.revoke_before <= record.pending[index].1
        {
            return None;
        }
        record.pending.remove(index);
        record.floor = record.floor.max(completion.revoke_before);
        self.snapshot(&mut view, wallet, &native, control.as_deref())
    }
}

fn selection_live(
    a: &CurrentAssignment,
    native: &HashMap<String, crate::pulse::snapshot::NativeRegistration>,
    control: Option<&super::control::ControlPositions>,
) -> bool {
    if a.owner_epoch == 0 {
        return false;
    }
    if a.selected_origin == 1 {
        native.get(&a.wallet.to_ascii_lowercase()).is_some_and(|r| {
            r.peer == a.owner_peer
                && r.session == a.session.to_ascii_lowercase()
                && r.registration == a.owner_epoch
        })
    } else {
        !native.contains_key(&a.wallet.to_ascii_lowercase())
            && matches!(a.selected_origin, 2 | 3)
            && (a.selected_origin != 3 || a.owner_epoch <= i64::MAX as u64)
            && control.is_none_or(|control| {
                control.allows_selected(
                    &a.wallet,
                    &a.session,
                    &a.realm,
                    a.owner_epoch,
                    a.selected_origin,
                )
            })
    }
}

#[cfg(test)]
mod current_tests {
    use super::*;
    #[test]
    fn native_arrival_departure_without_pass_or_lookup_invalidates_all_legacy_variants() {
        for origin in [2, 3] {
            for same_session in [false, true] {
                for usable in [false, true] {
                    let current = std::sync::Arc::new(CurrentAssignments::default());
                    let mut identity = crate::pulse::snapshot::IdentityBoard::new(2);
                    identity.attach_assignments(current.clone());
                    let mut previous = assignment("predecessor", 8);
                    previous.selected_origin = origin;
                    current.replace(1, Duration::from_secs(2), vec![previous]);
                    let mut legacy = assignment("legacy", 9);
                    legacy.selected_origin = origin;
                    current.replace(2, Duration::from_secs(2), vec![legacy.clone()]);
                    let before = current.lookup("wallet", "legacy").unwrap();
                    assert_eq!(before.pending_takeovers.len(), 1);
                    let potential = current.view.lock().unwrap().records["wallet"]
                        .potential
                        .clone();
                    // `usable` deliberately changes only board/grid availability: registration
                    // must invalidate before either collection or a lookup can observe it.
                    let mut board = crate::pulse::snapshot::SnapshotBoard::new(2, 2);
                    if usable {
                        board.set_active(1);
                    }
                    identity.set_with_session(
                        1,
                        "wallet".into(),
                        if same_session { "legacy" } else { "native" }.into(),
                    );
                    identity.remove(1);
                    let retained = current.lookup("wallet", "legacy").unwrap();
                    assert_eq!(retained.assignment_state, 2);
                    assert_eq!(retained.pending_takeovers, before.pending_takeovers);
                    assert_eq!(retained.token_not_before, before.token_not_before);
                    assert_eq!(
                        current.view.lock().unwrap().records["wallet"].potential,
                        potential
                    );
                    let op = retained.pending_takeovers[0].takeover_id.clone();
                    assert!(
                        current
                            .complete(
                                "wallet",
                                &crate::protocol::pulse::decentraland::pulse::TakeoverCompleted {
                                    takeover_id: op,
                                    revoke_before: 100
                                }
                            )
                            .is_none()
                    );
                    assert!(
                        current.lookup("wallet", "legacy").is_none(),
                        "cleanup cannot clear native invalidation"
                    );
                    current.replace(3, Duration::from_secs(2), vec![legacy.clone()]);
                    let after = current.lookup("wallet", "legacy").unwrap();
                    assert_eq!(after.assignment_state, 1);
                    assert!(after.assignment_version > before.assignment_version);
                    assert_eq!(after.token_not_before, Some(100));
                    current.replace(4, Duration::from_secs(2), vec![legacy]);
                    assert_eq!(
                        current
                            .lookup("wallet", "legacy")
                            .unwrap()
                            .assignment_version,
                        after.assignment_version
                    );
                }
            }
        }
    }

    #[test]
    fn native_current_peer_and_exhausted_legacy_revalidation_never_reauthorize_old_stamps() {
        let current = std::sync::Arc::new(CurrentAssignments::default());
        let mut identity = crate::pulse::snapshot::IdentityBoard::new(2);
        identity.attach_assignments(current.clone());
        let legacy = assignment("legacy", 9);
        current.replace(1, Duration::from_secs(2), vec![legacy.clone()]);
        identity.set_with_session(0, "wallet".into(), "native".into());
        identity.set_with_session(1, "wallet".into(), "native".into());
        let mut stale = assignment("native", identity.registration_by_peer(0));
        stale.selected_origin = 1;
        current.replace(2, Duration::from_secs(2), vec![stale]);
        assert!(current.lookup("wallet", "native").is_none());
        identity.remove(0);
        assert_eq!(identity.peer_by_wallet("wallet"), Some(1));
        identity.remove(1);
        current.view.lock().unwrap().next_version = u64::MAX;
        current.replace(3, Duration::from_secs(2), vec![legacy]);
        assert!(current.lookup("wallet", "legacy").is_none());
        assert_eq!(current.view.lock().unwrap().next_version, u64::MAX);
    }
    use crate::protocol::pulse::decentraland::pulse::TakeoverCompleted;

    #[tokio::test]
    async fn bounded_retry_batches_rotate_across_all_departed_pending_wallets() {
        let current = CurrentAssignments::default();
        let assignments = |session: &str| {
            (0..3)
                .map(|n| {
                    let mut a = assignment(session, 1);
                    a.wallet = format!("w{n}");
                    a
                })
                .collect()
        };
        current.replace(1, Duration::from_secs(2), assignments("a"));
        current.replace(2, Duration::from_secs(2), assignments("b"));
        current.replace(3, Duration::from_secs(2), vec![]);
        assert!(current.retained_hints(0).is_empty());
        let mut hinted = std::collections::HashSet::new();
        for _ in 0..3 {
            let batch = current.retained_hints(1);
            assert_eq!(batch.len(), 1);
            hinted.insert(batch[0].wallet.clone());
        }
        assert_eq!(
            hinted.len(),
            3,
            "a capacity-one outbox still reaches every retained wallet"
        );
    }

    fn cohort(start: usize, count: usize, session: &str) -> Vec<CurrentAssignment> {
        (start..start + count)
            .map(|n| {
                let mut a = assignment(session, 1);
                a.wallet = format!("w{n:06}");
                a
            })
            .collect()
    }

    #[tokio::test]
    async fn configured_transport_and_control_capacity_plus_one_churn_cohort_fit() {
        let current = CurrentAssignments::default();
        let simultaneous = crate::pulse::cluster::control::SUPPORTED_TRANSPORT_PEERS
            + crate::pulse::cluster::control::DEFAULT_CONTROL_POSITION_CAPACITY;
        assert_eq!(simultaneous, 4095 + 4096 + 16382);
        current.replace(1, Duration::from_secs(2), cohort(0, simultaneous, "a"));
        assert_eq!(current.view.lock().unwrap().records.len(), simultaneous);
        current.replace(
            2,
            Duration::from_secs(2),
            cohort(simultaneous, simultaneous, "b"),
        );
        assert_eq!(current.view.lock().unwrap().records.len(), simultaneous * 2);
        for n in [0, simultaneous - 1] {
            let wallet = format!("w{n:06}");
            let record = &current.view.lock().unwrap().records[&wallet];
            assert!(
                record.potential.is_some(),
                "churn cannot forget potentially admitted predecessors"
            );
        }
        // An old owner returning inside retention still requires its predecessor cleanup.
        let mut returning = cohort(0, 1, "c");
        returning.extend(cohort(simultaneous, simultaneous, "b"));
        current.replace(3, Duration::from_secs(2), returning);
        let returning = current.lookup("w000000", "c").unwrap();
        assert_eq!(returning.pending_takeovers.len(), 1);
        assert_eq!(returning.pending_takeovers[0].displaced_session, "a");
    }

    #[tokio::test]
    async fn safely_expired_departures_release_capacity_before_any_new_wallet_admission() {
        let current = CurrentAssignments::default();
        current.replace(1, Duration::from_secs(2), cohort(0, RECOVERY_WALLETS, "a"));
        let mut early = assignment("new", 1);
        early.wallet = "a-before-old-wallets".into();
        current.replace(RETENTION_PASSES + 2, Duration::from_secs(2), vec![early]);
        assert!(current.lookup("a-before-old-wallets", "new").is_some());
        assert_eq!(current.view.lock().unwrap().records.len(), 1);
    }

    #[tokio::test]
    async fn inactive_pending_blocked_and_unexpired_floors_survive_reclamation() {
        let current = CurrentAssignments::default();
        current.replace(1, Duration::from_secs(2), cohort(0, 4, "a"));
        let floor = unix_seconds() + FLOOR_GRACE_SECONDS;
        {
            let mut view = current.view.lock().unwrap();
            view.records.get_mut("w000000").unwrap().blocked = true;
            let pending = view.records.get_mut("w000001").unwrap();
            pending.pending.push((
                crate::protocol::pulse::decentraland::pulse::PendingTakeover {
                    takeover_id: "pending".into(),
                    displaced_session: "a".into(),
                    displaced_cluster_id: "room".into(),
                    displaced_realm: "realm".into(),
                },
                0,
            ));
            view.records.get_mut("w000002").unwrap().floor = floor;
        }
        current.replace(RETENTION_PASSES + 2, Duration::from_secs(2), vec![]);
        let view = current.view.lock().unwrap();
        assert_eq!(view.records.len(), 3);
        assert!(view.records["w000000"].blocked);
        assert_eq!(view.records["w000001"].pending.len(), 1);
        assert_eq!(view.records["w000002"].floor, floor);
    }

    #[tokio::test]
    async fn refusing_unconsumable_completion_preserves_pending_floor_and_version() {
        let current = CurrentAssignments::default();
        pass(&current, 1, "a");
        let before = pass(&current, 2, "b");
        assert!(
            current
                .complete(
                    "wallet",
                    &TakeoverCompleted {
                        takeover_id: before.pending_takeovers[0].takeover_id.clone(),
                        revoke_before: crate::protocol::bus::cluster::MAX_CONSUMER_SECONDS + 1,
                    }
                )
                .is_none()
        );
        assert_eq!(current.lookup("wallet", "b").unwrap(), before);
    }

    #[tokio::test]
    async fn room_capacity_blocks_without_truncation_and_matching_completion_releases_space() {
        let current = CurrentAssignments::default();
        pass(&current, 1, "a");
        {
            let mut view = current.view.lock().unwrap();
            let record = view.records.get_mut("wallet").unwrap();
            for n in 0..RECOVERY_ROOMS {
                record.pending.push((
                    crate::protocol::pulse::decentraland::pulse::PendingTakeover {
                        takeover_id: format!("old-{n}"),
                        displaced_session: "old".into(),
                        displaced_cluster_id: format!("room-{n}"),
                        displaced_realm: "realm".into(),
                    },
                    0,
                ));
            }
        }
        let blocked = pass(&current, 2, "b");
        assert_eq!(blocked.pending_takeovers.len(), 32);
        assert_eq!(blocked.token_not_before, None);
        current.complete(
            "wallet",
            &TakeoverCompleted {
                takeover_id: "old-0".into(),
                revoke_before: 100,
            },
        );
        let resumed = pass(&current, 3, "b");
        assert_eq!(resumed.pending_takeovers.len(), 32);
        assert_eq!(resumed.token_not_before, Some(100));
        assert!(
            resumed
                .pending_takeovers
                .iter()
                .any(|p| p.displaced_session == "a")
        );
        assert!(
            resumed
                .pending_takeovers
                .iter()
                .any(|p| p.takeover_id == "old-31")
        );
    }

    #[tokio::test]
    async fn pulse_accepts_consumer_maximum_future_cutoff_without_adding_clock_skew_policy() {
        let current = CurrentAssignments::default();
        pass(&current, 1, "a");
        let b = pass(&current, 2, "b");
        let ready = current
            .complete(
                "wallet",
                &TakeoverCompleted {
                    takeover_id: b.pending_takeovers[0].takeover_id.clone(),
                    revoke_before: crate::protocol::bus::cluster::MAX_CONSUMER_SECONDS,
                },
            )
            .unwrap();
        assert_eq!(
            ready.token_not_before,
            Some(crate::protocol::bus::cluster::MAX_CONSUMER_SECONDS)
        );
        current.replace(3, Duration::from_secs(2), vec![]);
        current.replace(1000, Duration::from_secs(2), vec![]);
        assert_eq!(
            current.view.lock().unwrap().records.len(),
            1,
            "confirmed floor clock grace also governs retirement"
        );
    }

    fn assignment(session: &str, epoch: u64) -> CurrentAssignment {
        CurrentAssignment {
            wallet: "wallet".into(),
            session: session.into(),
            realm: "realm".into(),
            cluster_id: "room".into(),
            owner_epoch: epoch,
            selected_origin: 2,
            owner_peer: 0,
            registration: None,
        }
    }

    #[tokio::test]
    async fn same_slot_same_strings_registration_replacement_invalidates_before_next_pass() {
        let current = std::sync::Arc::new(CurrentAssignments::default());
        let mut identity = crate::pulse::snapshot::IdentityBoard::new(1);
        identity.attach_assignments(current.clone());
        identity.set_with_session(0, "wallet".into(), "a".into());
        let mut a = assignment("a", identity.registration_by_peer(0));
        a.registration = Some(identity.registration_guard(0));
        a.selected_origin = 1;
        current.replace(1, Duration::from_secs(2), vec![a.clone()]);
        let before = current.lookup("wallet", "a").unwrap();
        identity.remove(0);
        assert!(current.lookup("wallet", "a").is_none());
        identity.set_with_session(0, "wallet".into(), "a".into());
        assert!(current.lookup("wallet", "a").is_none());
        a.owner_epoch = identity.registration_by_peer(0);
        current.replace(2, Duration::from_secs(2), vec![a]);
        assert!(
            current.lookup("wallet", "a").unwrap().assignment_version > before.assignment_version
        );
    }

    #[tokio::test]
    async fn departed_pending_identity_keeps_floor_and_releases_without_ever_becoming_ready() {
        let current = CurrentAssignments::default();
        pass(&current, 1, "a");
        let b = pass(&current, 2, "b");
        current.replace(1000, Duration::from_secs(2), vec![]);
        current.replace(2000, Duration::from_secs(2), vec![]);
        let inactive = current.lookup("wallet", "b").unwrap();
        assert_eq!(inactive.token_not_before, Some(0));
        assert_eq!(inactive.pending_takeovers, b.pending_takeovers);
        assert!(current.lookup("wallet", "a").is_none());
        assert_eq!(current.retained_hints(1).len(), 1);
        assert!(
            current
                .complete(
                    "wallet",
                    &TakeoverCompleted {
                        takeover_id: b.pending_takeovers[0].takeover_id.clone(),
                        revoke_before: 100
                    }
                )
                .is_none()
        );
        assert!(current.lookup("wallet", "b").is_none());
        assert!(current.retained_hints(1).is_empty());
        current.replace(3000, Duration::from_secs(2), vec![]);
        assert!(current.view.lock().unwrap().records.is_empty());
    }

    fn pass(current: &CurrentAssignments, n: u64, session: &str) -> PeerClusterSnapshot {
        current.replace(n, Duration::from_secs(2), vec![assignment(session, n)]);
        current.lookup("wallet", session).unwrap()
    }

    #[tokio::test]
    async fn ready_pass_exposure_retains_a_but_never_blocked_b_in_a_b_c() {
        let current = CurrentAssignments::default();
        current.replace(1, Duration::from_secs(2), vec![assignment("a", 1)]);
        // A is potentially admitted at completed-pass Snapshot, even without a lookup.
        let b = pass(&current, 2, "b");
        let c = pass(&current, 3, "c");
        assert_eq!(b.pending_takeovers, c.pending_takeovers);
        assert_eq!(c.pending_takeovers.len(), 1);
        assert_eq!(c.pending_takeovers[0].displaced_session, "a");
        let id = c.pending_takeovers[0].takeover_id.clone();
        let ready = current
            .complete(
                "wallet",
                &TakeoverCompleted {
                    takeover_id: id.clone(),
                    revoke_before: 100,
                },
            )
            .unwrap();
        assert!(ready.pending_takeovers.is_empty());
        assert_eq!(ready.token_not_before, Some(100));
        assert!(
            current
                .complete(
                    "wallet",
                    &TakeoverCompleted {
                        takeover_id: id,
                        revoke_before: 999
                    }
                )
                .is_none()
        );
        assert_eq!(
            current.lookup("wallet", "c").unwrap().token_not_before,
            Some(100)
        );
        let d = pass(&current, 4, "d");
        assert_eq!(d.pending_takeovers[0].displaced_session, "c");
    }

    #[tokio::test(start_paused = true)]
    async fn unfinished_work_survives_staleness_absence_and_retention() {
        let current = CurrentAssignments::default();
        pass(&current, 1, "a");
        let b = pass(&current, 2, "b");
        tokio::time::advance(Duration::from_secs(50)).await;
        assert_eq!(
            current.lookup("wallet", "b").unwrap().pending_takeovers,
            b.pending_takeovers
        );
        current.replace(1000, Duration::from_secs(2), vec![]);
        let c = pass(&current, 2000, "c");
        assert_eq!(b.pending_takeovers, c.pending_takeovers);
    }

    #[tokio::test]
    async fn refresh_and_same_session_moves_preserve_no_cleanup_but_invalidate_identity() {
        let current = CurrentAssignments::default();
        let a = assignment("a", 1);
        current.replace(1, Duration::from_secs(2), vec![a.clone()]);
        let first = current.lookup("wallet", "a").unwrap();
        current.replace(2, Duration::from_secs(2), vec![a.clone()]);
        assert_eq!(
            current.lookup("wallet", "a").unwrap().assignment_version,
            first.assignment_version
        );
        let mut moved = a.clone();
        moved.cluster_id = "next".into();
        current.replace(3, Duration::from_secs(2), vec![moved]);
        let second = current.lookup("wallet", "a").unwrap();
        assert!(second.assignment_version > first.assignment_version);
        assert!(second.pending_takeovers.is_empty());
        current.replace(4, Duration::from_secs(2), vec![a]);
        assert!(
            current.lookup("wallet", "a").unwrap().assignment_version > second.assignment_version
        );
        let mut registration = assignment("a", 2);
        registration.owner_peer = 8;
        current.replace(5, Duration::from_secs(2), vec![registration]);
        assert!(
            current.lookup("wallet", "a").unwrap().assignment_version > second.assignment_version
        );
    }

    #[tokio::test]
    async fn expiry_never_reuses_version_and_checked_wrap_never_admits() {
        let current = CurrentAssignments::default();
        let first = pass(&current, 1, "a");
        current.replace(2, Duration::from_secs(2), vec![]);
        current.replace(1000, Duration::from_secs(2), vec![]);
        let next = pass(&current, 1001, "a");
        assert!(next.assignment_version > first.assignment_version);
        current.view.lock().unwrap().next_version = u64::MAX;
        current.replace(1002, Duration::from_secs(2), vec![assignment("b", 3)]);
        assert!(current.lookup("wallet", "b").is_none());
        assert_eq!(current.view.lock().unwrap().next_version, u64::MAX);
    }

    #[tokio::test]
    async fn completions_validate_operation_prior_floor_and_do_not_advance_unknown_ids() {
        let current = CurrentAssignments::default();
        pass(&current, 1, "a");
        let b = pass(&current, 2, "b");
        let id = b.pending_takeovers[0].takeover_id.clone();
        for cutoff in [0, crate::protocol::bus::cluster::MAX_CONSUMER_SECONDS + 1] {
            assert!(
                current
                    .complete(
                        "wallet",
                        &TakeoverCompleted {
                            takeover_id: id.clone(),
                            revoke_before: cutoff
                        }
                    )
                    .is_none()
            );
        }
        // Inject a second independent retained operation to check out-of-order aggregate handling.
        {
            let mut view = current.view.lock().unwrap();
            let record = view.records.get_mut("wallet").unwrap();
            let mut other = record.pending[0].0.clone();
            other.takeover_id = "independent".into();
            record.pending.push((other, 0));
        }
        current
            .complete(
                "wallet",
                &TakeoverCompleted {
                    takeover_id: id,
                    revoke_before: 100,
                },
            )
            .unwrap();
        let ready = current
            .complete(
                "wallet",
                &TakeoverCompleted {
                    takeover_id: "independent".into(),
                    revoke_before: 90,
                },
            )
            .unwrap();
        assert_eq!(ready.token_not_before, Some(100));
        let c = pass(&current, 3, "c");
        assert!(
            current
                .complete(
                    "wallet",
                    &TakeoverCompleted {
                        takeover_id: c.pending_takeovers[0].takeover_id.clone(),
                        revoke_before: 100
                    }
                )
                .is_none()
        );
        assert!(
            current
                .complete(
                    "wallet",
                    &TakeoverCompleted {
                        takeover_id: "stale-incarnation".into(),
                        revoke_before: 999
                    }
                )
                .is_none()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn saturation_never_truncates_or_evicts_unfinished_plans() {
        let current = CurrentAssignments::default();
        let mut assignments = Vec::new();
        for i in 0..RECOVERY_WALLETS {
            let mut a = assignment("a", 1);
            a.wallet = format!("w{i:06}");
            assignments.push(a);
        }
        current.replace(1, Duration::from_secs(2), assignments.clone());
        let mut extra = assignment("extra", 1);
        extra.wallet = "excess".into();
        assignments.push(extra);
        let recorded = crate::pulse::metrics::render_recorded(|| {
            current.replace(2, Duration::from_secs(2), assignments);
        });
        assert!(recorded.contains("pulse_recovery_refused_total 1"));
        assert!(recorded.contains(&format!("pulse_recovery_wallets {RECOVERY_WALLETS}")));
        assert!(current.lookup("excess", "extra").is_none());
        // Fill every retained record with pending work, depart, and age past the
        // ordinary inactive window. Exhaustion must still refuse without loss.
        current.replace(3, Duration::from_secs(2), cohort(0, RECOVERY_WALLETS, "b"));
        let mut refused = assignment("new", 1);
        refused.wallet = "excess".into();
        current.replace(1000, Duration::from_secs(2), vec![refused]);
        assert!(current.lookup("excess", "new").is_none());
        assert_eq!(current.view.lock().unwrap().records.len(), RECOVERY_WALLETS);
        assert!(
            current
                .view
                .lock()
                .unwrap()
                .records
                .values()
                .all(|r| r.pending.len() == 1)
        );
        let oversized = CurrentAssignments::default();
        let mut a = assignment("a", 1);
        a.realm = "r".repeat(65536);
        oversized.replace(1, Duration::from_secs(2), vec![a]);
        assert!(oversized.lookup("wallet", "a").is_none());
        assert!(
            oversized.view.lock().unwrap().records["wallet"]
                .potential
                .is_none()
        );
    }

    #[tokio::test]
    async fn ready_snapshot_carries_explicit_zero_floor_and_assignment_identity() {
        use prost::Message;
        let current = CurrentAssignments::default();
        current.replace(
            1,
            Duration::from_secs(2),
            vec![CurrentAssignment {
                wallet: "0xab".into(),
                session: "0xcd".into(),
                cluster_id: "C1".into(),
                realm: "realm".into(),
                owner_epoch: 1,
                selected_origin: 2,
                owner_peer: 0,
                registration: None,
            }],
        );
        let bytes = current.lookup("0xab", "0xcd").unwrap().encode_to_vec();
        assert!(
            bytes.windows(2).any(|pair| pair == [0x40, 0]),
            "ready snapshot must encode present zero floor at tag 8"
        );
        assert!(
            bytes.windows(2).any(|pair| pair == [0x48, 1]),
            "first assignment has a positive version at tag 9"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_current_view_expires_at_its_deadline_and_is_replace_only() {
        let current = CurrentAssignments::default();
        let assignment = CurrentAssignment {
            wallet: "0xAB".into(),
            session: "0xCD".into(),
            cluster_id: "C1".into(),
            realm: "realm".into(),
            owner_epoch: 1,
            selected_origin: 2,
            owner_peer: 0,
            registration: None,
        };
        current.replace(7, Duration::from_secs(2), vec![assignment.clone()]);
        assert_eq!(current.lookup("0xab", "0xcd").unwrap().pass, 7);
        assert!(current.lookup("0xab", "0xce").is_none());
        tokio::time::advance(Duration::from_millis(1999)).await;
        assert!(current.lookup("0xab", "0xcd").is_some());
        tokio::time::advance(Duration::from_millis(1)).await;
        assert!(current.lookup("0xab", "0xcd").is_none());
        current.replace(8, Duration::from_secs(2), vec![assignment]);
        assert_eq!(current.lookup("0xab", "0xcd").unwrap().pass, 8);
        current.replace(9, Duration::from_secs(2), vec![]);
        assert!(current.lookup("0xab", "0xcd").is_none());
    }
}
