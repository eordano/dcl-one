//! Shared island safety facts. Stable wallet locks serialize first admission
//! with first cleanup; remote calls never run while these transactions live.
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

use super::subscriber::GatewayError;
use crate::protocol::pulse::decentraland::pulse::PendingTakeover;

const CAPACITY: i64 = 10_000;
const CAPACITY_LOCK: i64 = 4_638_100_557_221;
// LiveKit verifier leeway plus NumericDate precision and scheduling margin.
// Keep token expiry and quarantine coverage aligned with these margins:
// durable_tests::durable_unknown_never_enters_quarantine_final_and_restart_waits_all_issuer_tokens
// and subscriber::tests::subscriber_caps_programmatic_token_ttl_to_its_quarantine_budget.
pub(super) const LIVEKIT_TOKEN_LEEWAY: std::time::Duration = std::time::Duration::from_secs(60);
pub(super) const TOKEN_EXPIRY_SAFETY: std::time::Duration = std::time::Duration::from_secs(2);
const FULL_MAX_ISSUER_WAIT_SECONDS: u64 = super::MAX_ISLAND_TOKEN_TTL_SECONDS
    + LIVEKIT_TOKEN_LEEWAY.as_secs()
    + TOKEN_EXPIRY_SAFETY.as_secs();
const SETTLED_RETENTION_SECONDS: i64 = FULL_MAX_ISSUER_WAIT_SECONDS as i64;

#[derive(Clone)]
pub struct SafetyStore {
    pub(crate) pool: PgPool,
    quarantines:
        std::sync::Arc<std::sync::Mutex<std::collections::HashMap<Uuid, tokio::time::Instant>>>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SafetySnapshot {
    pub generation: u64,
    pub token_floor: u64,
}
pub struct SafetyLease {
    pub snapshot: SafetySnapshot,
    pub(crate) tx: Transaction<'static, Postgres>,
}
impl SafetyLease {
    pub async fn commit(self) -> Result<(), GatewayError> {
        self.tx.commit().await.map_err(|_| GatewayError::Unknown)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchState {
    Dispatched,
    Confirmed,
    Rejected,
    Quarantine,
    SettledLate,
    Superseded,
}
impl DispatchState {
    fn decode(value: &str) -> Result<Self, GatewayError> {
        match value {
            "dispatched" => Ok(Self::Dispatched),
            "confirmed" => Ok(Self::Confirmed),
            "rejected" => Ok(Self::Rejected),
            "quarantine" => Ok(Self::Quarantine),
            "settled_late" => Ok(Self::SettledLate),
            "superseded" => Ok(Self::Superseded),
            _ => Err(GatewayError::Unknown),
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Self::Dispatched => "dispatched",
            Self::Confirmed => "confirmed",
            Self::Rejected => "rejected",
            Self::Quarantine => "quarantine",
            Self::SettledLate => "settled_late",
            Self::Superseded => "superseded",
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchPhase {
    Initial,
    QuarantineFinal,
}
impl DispatchPhase {
    fn decode(value: &str) -> Result<Self, GatewayError> {
        match value {
            "initial" => Ok(Self::Initial),
            "quarantine_final" => Ok(Self::QuarantineFinal),
            _ => Err(GatewayError::Unknown),
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Self::Initial => "initial",
            Self::QuarantineFinal => "quarantine_final",
        }
    }
}
#[derive(Debug, Clone)]
pub struct Dispatch {
    pub dispatch_id: Uuid,
    pub cutoff: u64,
    pub state: DispatchState,
    pub quarantine_until: Option<chrono::NaiveDateTime>,
    pub phase: DispatchPhase,
    pub fresh: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settlement {
    Confirmed,
    Rejected,
    SettledLate,
    Quarantine(u64),
}

#[derive(Clone, Copy)]
pub(crate) enum RemovalOutcome {
    Positive,
    Rejected,
    Unknown,
}

#[derive(Clone, Copy)]
pub(crate) enum QuarantinePolicy {
    Disabled,
    Enabled { applies: bool },
}

/// Classify only the completed RPC outcome. Callers retain retry, room repair,
/// cancellation and persistence ordering, and choose quarantine applicability.
pub(crate) fn classify_removal(
    outcome: RemovalOutcome,
    phase: DispatchPhase,
    quarantine: QuarantinePolicy,
    now_ms: u64,
    cutoff_seconds: u64,
) -> Option<Settlement> {
    match outcome {
        RemovalOutcome::Unknown => None,
        RemovalOutcome::Rejected => Some(if phase == DispatchPhase::QuarantineFinal {
            Settlement::SettledLate
        } else {
            Settlement::Rejected
        }),
        RemovalOutcome::Positive => Some(match quarantine {
            QuarantinePolicy::Enabled { applies: true } if phase == DispatchPhase::Initial => {
                Settlement::Quarantine(FULL_MAX_ISSUER_WAIT_SECONDS)
            }
            QuarantinePolicy::Disabled
                if now_ms.saturating_add(1000) >= cutoff_seconds.saturating_mul(1000) =>
            {
                Settlement::SettledLate
            }
            _ => Settlement::Confirmed,
        }),
    }
}

impl SafetyStore {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            quarantines: Default::default(),
        }
    }
    /// INSERT before SELECT FOR UPDATE serializes an absent wallet with its
    /// first hazard; an EXISTS probe cannot provide that guarantee.
    pub(crate) async fn lock(&self, wallet: &str) -> Result<SafetyLease, GatewayError> {
        if wallet.is_empty() || wallet != wallet.to_ascii_lowercase() {
            return Err(GatewayError::Unknown);
        }
        let mut tx = self.pool.begin().await.map_err(|_| GatewayError::Unknown)?;
        for setting in [
            "SET LOCAL lock_timeout = '1500ms'",
            "SET LOCAL statement_timeout = '1500ms'",
            "SET LOCAL idle_in_transaction_session_timeout = '5000ms'",
        ] {
            sqlx::query(setting)
                .execute(&mut *tx)
                .await
                .map_err(|_| GatewayError::Unknown)?;
        }
        sqlx::query(
            "INSERT INTO social.island_admission_fences(wallet) VALUES ($1) ON CONFLICT DO NOTHING",
        )
        .bind(wallet)
        .execute(&mut *tx)
        .await
        .map_err(|_| GatewayError::Unknown)?;
        let row=sqlx::query("SELECT generation,token_floor FROM social.island_admission_fences WHERE wallet=$1 FOR UPDATE")
            .bind(wallet).fetch_one(&mut *tx).await.map_err(|_|GatewayError::Unknown)?;
        let generation: i64 = row
            .try_get("generation")
            .map_err(|_| GatewayError::Unknown)?;
        let token_floor: i64 = row
            .try_get("token_floor")
            .map_err(|_| GatewayError::Unknown)?;
        if generation <= 0
            || token_floor < 0
            || token_floor as u64 > crate::protocol::bus::cluster::MAX_CONSUMER_SECONDS
        {
            return Err(GatewayError::Unknown);
        }
        Ok(SafetyLease {
            snapshot: SafetySnapshot {
                generation: generation as u64,
                token_floor: token_floor as u64,
            },
            tx,
        })
    }
    pub async fn authorize(
        &self,
        wallet: &str,
        expected: Option<SafetySnapshot>,
        credential_not_before: u64,
    ) -> Result<SafetyLease, GatewayError> {
        let mut lease = self.lock(wallet).await?;
        if expected.is_some_and(|e| e != lease.snapshot)
            || credential_not_before < lease.snapshot.token_floor
            || credential_not_before > crate::protocol::bus::cluster::MAX_CONSUMER_SECONDS
        {
            return Err(GatewayError::Unknown);
        }
        let hazard:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM social.island_cleanup_dispatches WHERE wallet=$1 AND state IN ('dispatched','quarantine','settled_late')) OR EXISTS(SELECT 1 FROM social.island_takeover_receipts r LEFT JOIN social.island_cleanup_dispatches d ON d.dispatch_id=r.latest_dispatch AND d.wallet=r.wallet WHERE r.wallet=$1 AND NOT r.acknowledged AND r.retired_by_source IS NULL AND (d.dispatch_id IS NULL OR d.state<>'confirmed'))")
            .bind(wallet).fetch_one(&mut *lease.tx).await.map_err(|_|GatewayError::Unknown)?;
        if hazard {
            return Err(GatewayError::Unknown);
        }
        Ok(lease)
    }
    pub async fn snapshot(&self, wallet: &str) -> Result<SafetySnapshot, GatewayError> {
        let lease = self
            .authorize(
                wallet,
                None,
                crate::protocol::bus::cluster::MAX_CONSUMER_SECONDS,
            )
            .await?;
        let snapshot = lease.snapshot;
        lease.commit().await?;
        Ok(snapshot)
    }
    pub async fn prepare_takeover(
        &self,
        wallet: &str,
        source: &str,
        successor: &str,
        operation: &PendingTakeover,
    ) -> Result<(), GatewayError> {
        if source.is_empty()
            || successor.is_empty()
            || operation.takeover_id.is_empty()
            || operation.displaced_session.is_empty()
            || operation.displaced_cluster_id.is_empty()
        {
            return Err(GatewayError::Unknown);
        }
        let mut lease = self.lock(wallet).await?;
        let exists:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM social.island_takeover_receipts WHERE wallet=$1 AND source_incarnation=$2 AND takeover_id=$3)")
            .bind(wallet).bind(source).bind(&operation.takeover_id).fetch_one(&mut *lease.tx).await.map_err(|_|GatewayError::Unknown)?;
        if !exists {
            self.capacity(&mut lease.tx).await?;
        }
        let inserted=sqlx::query("INSERT INTO social.island_takeover_receipts(wallet,source_incarnation,takeover_id,predecessor_session,predecessor_cluster,predecessor_realm,last_successor_session) VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT DO NOTHING")
            .bind(wallet).bind(source).bind(&operation.takeover_id).bind(&operation.displaced_session).bind(&operation.displaced_cluster_id).bind(&operation.displaced_realm).bind(successor)
            .execute(&mut *lease.tx).await.map_err(|_|GatewayError::Unknown)?.rows_affected();
        let matches:bool=sqlx::query_scalar("SELECT predecessor_session=$4 AND predecessor_cluster=$5 AND predecessor_realm=$6 AND retired_by_source IS NULL FROM social.island_takeover_receipts WHERE wallet=$1 AND source_incarnation=$2 AND takeover_id=$3")
            .bind(wallet).bind(source).bind(&operation.takeover_id).bind(&operation.displaced_session).bind(&operation.displaced_cluster_id).bind(&operation.displaced_realm)
            .fetch_one(&mut *lease.tx).await.map_err(|_|GatewayError::Unknown)?;
        if !matches {
            return Err(GatewayError::Unknown);
        }
        sqlx::query("UPDATE social.island_takeover_receipts SET last_successor_session=$4 WHERE wallet=$1 AND source_incarnation=$2 AND takeover_id=$3")
            .bind(wallet).bind(source).bind(&operation.takeover_id).bind(successor).execute(&mut *lease.tx).await.map_err(|_|GatewayError::Unknown)?;
        if inserted > 0 {
            Self::advance(&mut lease.tx, wallet, None).await?;
        }
        lease.commit().await
    }
    pub(crate) async fn capacity(
        &self,
        tx: &mut Transaction<'static, Postgres>,
    ) -> Result<(), GatewayError> {
        // Rare new-obligation insertion only. Scheduling/attempt progress has
        // capacity credit; unknown state is never evicted to make room.
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(CAPACITY_LOCK)
            .execute(&mut **tx)
            .await
            .map_err(|_| GatewayError::Unknown)?;
        let count:i64=sqlx::query_scalar("SELECT (SELECT count(*) FROM social.island_takeover_receipts WHERE NOT acknowledged AND retired_by_source IS NULL)+(SELECT count(*) FROM (SELECT wallet,intent_root_key,intent_target_key,intent_generation FROM social.island_cleanup_dispatches WHERE purpose='island_enforcement' AND state IN ('dispatched','quarantine','settled_late') GROUP BY wallet,intent_root_key,intent_target_key,intent_generation) obligations)")
            .fetch_one(&mut **tx).await.map_err(|_|GatewayError::Unknown)?;
        if count >= CAPACITY {
            metrics::counter!("comms_island_safety_capacity_refused_total").increment(1);
            return Err(GatewayError::Unknown);
        }
        Ok(())
    }
    pub async fn begin_takeover(
        &self,
        wallet: &str,
        source: &str,
        operation: &PendingTakeover,
        pulse_floor: u64,
    ) -> Result<Option<Dispatch>, GatewayError> {
        let mut lease = self.lock(wallet).await?;
        let row=sqlx::query("SELECT latest_dispatch,predecessor_session,predecessor_cluster,predecessor_realm,acknowledged,retired_by_source FROM social.island_takeover_receipts WHERE wallet=$1 AND source_incarnation=$2 AND takeover_id=$3")
            .bind(wallet).bind(source).bind(&operation.takeover_id).fetch_optional(&mut *lease.tx).await.map_err(|_|GatewayError::Unknown)?.ok_or(GatewayError::Unknown)?;
        if row.get::<String, _>("predecessor_session") != operation.displaced_session
            || row.get::<String, _>("predecessor_cluster") != operation.displaced_cluster_id
            || row.get::<String, _>("predecessor_realm") != operation.displaced_realm
            || row.get::<bool, _>("acknowledged")
            || row.get::<Option<String>, _>("retired_by_source").is_some()
        {
            return Err(GatewayError::Unknown);
        }
        let latest: Option<Uuid> = row.get("latest_dispatch");
        if let Some(id) = latest {
            let current = Self::dispatch(&mut lease.tx, id).await?;
            if !matches!(
                current.state,
                DispatchState::Rejected | DispatchState::SettledLate
            ) {
                lease.commit().await?;
                return Ok(Some(current));
            }
        }
        let room = crate::rooms::island_room_name(&operation.displaced_cluster_id);
        let result = self
            .insert_dispatch(
                &mut lease,
                wallet,
                &room,
                wallet,
                pulse_floor,
                latest,
                "pulse_takeover",
                Some(source),
                Some(&operation.takeover_id),
                None,
            )
            .await?;
        if let Some(dispatch) = &result {
            sqlx::query("UPDATE social.island_takeover_receipts SET latest_dispatch=$4 WHERE wallet=$1 AND source_incarnation=$2 AND takeover_id=$3")
                .bind(wallet).bind(source).bind(&operation.takeover_id).bind(dispatch.dispatch_id).execute(&mut *lease.tx).await.map_err(|_|GatewayError::Unknown)?;
        }
        lease.commit().await?;
        Ok(result)
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn insert_dispatch(
        &self,
        lease: &mut SafetyLease,
        wallet: &str,
        room: &str,
        identity: &str,
        floor: u64,
        previous: Option<Uuid>,
        purpose: &str,
        source: Option<&str>,
        takeover: Option<&str>,
        enforcement: Option<(&str, &str, &str, Uuid)>,
    ) -> Result<Option<Dispatch>, GatewayError> {
        if floor >= crate::protocol::bus::cluster::MAX_CONSUMER_SECONDS {
            return Err(GatewayError::Unknown);
        }
        let other:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM social.island_cleanup_dispatches WHERE wallet=$1 AND state IN ('dispatched','quarantine','settled_late') AND ($2::uuid IS NULL OR dispatch_id<>$2))")
            .bind(wallet).bind(previous).fetch_one(&mut *lease.tx).await.map_err(|_|GatewayError::Unknown)?;
        if other {
            return Ok(None);
        }
        let (now,prior):(i64,i64)=sqlx::query_as("SELECT floor(extract(epoch FROM timezone('UTC',clock_timestamp())))::bigint,COALESCE((SELECT max(cutoff) FROM social.island_cleanup_dispatches WHERE wallet=$1),0)")
            .bind(wallet).fetch_one(&mut *lease.tx).await.map_err(|_|GatewayError::Unknown)?;
        let cutoff = (now as u64)
            .checked_add(5)
            .ok_or(GatewayError::Unknown)?
            .max(floor + 1)
            .max(
                lease
                    .snapshot
                    .token_floor
                    .checked_add(1)
                    .ok_or(GatewayError::Unknown)?,
            )
            .max((prior as u64).checked_add(1).ok_or(GatewayError::Unknown)?);
        if cutoff > crate::protocol::bus::cluster::MAX_CONSUMER_SECONDS
            || cutoff.saturating_sub(now as u64) >= 59
        {
            return Err(GatewayError::Unknown);
        }
        if let Some(previous) = previous {
            sqlx::query("UPDATE social.island_cleanup_dispatches SET state='superseded',quarantine_until=NULL WHERE dispatch_id=$1 AND state='settled_late'")
                .bind(previous).execute(&mut *lease.tx).await.map_err(|_|GatewayError::Unknown)?;
        }
        let phase = if let Some(previous) = previous {
            Self::dispatch(&mut lease.tx, previous).await?.phase
        } else {
            DispatchPhase::Initial
        };
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO social.island_cleanup_dispatches(dispatch_id,wallet,purpose,pulse_source,pulse_takeover_id,enforcement_cause,intent_root_key,intent_target_key,intent_generation,room_name,participant_identity,cutoff,fence_generation,phase,state) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,'dispatched')")
            .bind(id).bind(wallet).bind(purpose).bind(source).bind(takeover).bind(enforcement.map(|v|v.0)).bind(enforcement.map(|v|v.1)).bind(enforcement.map(|v|v.2)).bind(enforcement.map(|v|v.3))
            .bind(room).bind(identity).bind(cutoff as i64).bind(i64::try_from(lease.snapshot.generation).map_err(|_|GatewayError::Unknown)?.checked_add(1).ok_or(GatewayError::Unknown)?).bind(phase.as_str())
            .execute(&mut *lease.tx).await.map_err(|_|GatewayError::Unknown)?;
        Self::advance(&mut lease.tx, wallet, None).await?;
        Ok(Some(Dispatch {
            dispatch_id: id,
            cutoff,
            state: DispatchState::Dispatched,
            quarantine_until: None,
            phase,
            fresh: true,
        }))
    }
    pub(crate) async fn dispatch(
        tx: &mut Transaction<'static, Postgres>,
        id: Uuid,
    ) -> Result<Dispatch, GatewayError> {
        let row=sqlx::query("SELECT cutoff,state,quarantine_until,phase FROM social.island_cleanup_dispatches WHERE dispatch_id=$1").bind(id).fetch_one(&mut **tx).await.map_err(|_|GatewayError::Unknown)?;
        let cutoff: i64 = row.try_get("cutoff").map_err(|_| GatewayError::Unknown)?;
        if cutoff <= 0 || cutoff as u64 > crate::protocol::bus::cluster::MAX_CONSUMER_SECONDS {
            return Err(GatewayError::Unknown);
        }
        Ok(Dispatch {
            dispatch_id: id,
            cutoff: cutoff as u64,
            state: DispatchState::decode(row.try_get("state").map_err(|_| GatewayError::Unknown)?)?,
            quarantine_until: row
                .try_get("quarantine_until")
                .map_err(|_| GatewayError::Unknown)?,
            phase: DispatchPhase::decode(row.try_get("phase").map_err(|_| GatewayError::Unknown)?)?,
            fresh: false,
        })
    }
    async fn advance(
        tx: &mut Transaction<'static, Postgres>,
        wallet: &str,
        floor: Option<u64>,
    ) -> Result<(), GatewayError> {
        sqlx::query("UPDATE social.island_admission_fences SET generation=generation+1,token_floor=GREATEST(token_floor,COALESCE($2,token_floor)),updated_at=timezone('UTC',clock_timestamp()) WHERE wallet=$1")
            .bind(wallet).bind(floor.map(|v|v as i64)).execute(&mut **tx).await.map_err(|_|GatewayError::Unknown)?;
        Ok(())
    }
    /// An immutable dispatch UUID, never its scheduling lease, owns settlement.
    pub async fn settle(&self, id: Uuid, outcome: Settlement) -> Result<bool, GatewayError> {
        let wallet: Option<String> = sqlx::query_scalar(
            "SELECT wallet FROM social.island_cleanup_dispatches WHERE dispatch_id=$1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| GatewayError::Unknown)?;
        let Some(wallet) = wallet else {
            return Ok(false);
        };
        let mut lease = self.lock(&wallet).await?;
        let dispatch = Self::dispatch(&mut lease.tx, id).await?;
        if dispatch.state != DispatchState::Dispatched {
            return Ok(false);
        }
        let (state, quarantine) = match outcome {
            Settlement::Confirmed => (DispatchState::Confirmed, None),
            Settlement::Rejected => (DispatchState::Rejected, None),
            Settlement::SettledLate => (DispatchState::SettledLate, None),
            Settlement::Quarantine(seconds) => (
                DispatchState::Quarantine,
                Some(i64::try_from(seconds).map_err(|_| GatewayError::Unknown)?),
            ),
        };
        sqlx::query("UPDATE social.island_cleanup_dispatches SET state=$2,settled_at=timezone('UTC',clock_timestamp()),quarantine_until=CASE WHEN $3::bigint IS NULL THEN NULL ELSE timezone('UTC',clock_timestamp())+$3*interval '1 second' END WHERE dispatch_id=$1 AND state='dispatched'")
            .bind(id).bind(state.as_str()).bind(quarantine).execute(&mut *lease.tx).await.map_err(|_|GatewayError::Unknown)?;
        Self::advance(
            &mut lease.tx,
            &wallet,
            matches!(outcome, Settlement::Confirmed).then_some(dispatch.cutoff),
        )
        .await?;
        lease.commit().await?;
        if matches!(outcome, Settlement::Quarantine(_)) {
            self.quarantines
                .lock()
                .unwrap()
                .insert(id, tokio::time::Instant::now());
        }
        Ok(true)
    }
    pub async fn acknowledge(
        &self,
        wallet: &str,
        source: &str,
        pending: &[PendingTakeover],
    ) -> Result<(), GatewayError> {
        let mut lease = self.lock(wallet).await?;
        if source.is_empty() {
            return Err(GatewayError::Unknown);
        }
        // A historical origin observed again is ambiguous while retained
        // evidence identifies it. This is no permanent source-history claim.
        let retired_origin:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM social.island_takeover_receipts WHERE wallet=$1 AND source_incarnation=$2 AND retired_by_source IS NOT NULL)")
            .bind(wallet).bind(source).fetch_one(&mut *lease.tx).await.map_err(|_|GatewayError::Unknown)?;
        if retired_origin {
            return Err(GatewayError::Unknown);
        }
        let ids: Vec<&str> = pending.iter().map(|v| v.takeover_id.as_str()).collect();
        sqlx::query("UPDATE social.island_takeover_receipts r SET acknowledged=true,claim_id=NULL,claim_until=NULL FROM social.island_cleanup_dispatches d WHERE r.wallet=$1 AND r.source_incarnation=$2 AND r.retired_by_source IS NULL AND NOT(r.takeover_id=ANY($3)) AND r.latest_dispatch=d.dispatch_id AND d.wallet=r.wallet AND d.state='confirmed'")
            .bind(wallet).bind(source).bind(ids).execute(&mut *lease.tx).await.map_err(|_|GatewayError::Unknown)?;
        // This lock re-reads the latest exact immutable attempt, never a
        // pre-lock observation. Local retirement cannot masquerade as ack.
        sqlx::query("UPDATE social.island_takeover_receipts r SET retired_by_source=$2,claim_id=NULL,claim_until=NULL FROM social.island_cleanup_dispatches d,social.island_admission_fences f WHERE r.wallet=$1 AND r.source_incarnation<>$2 AND NOT r.acknowledged AND r.retired_by_source IS NULL AND r.latest_dispatch=d.dispatch_id AND d.wallet=r.wallet AND d.purpose='pulse_takeover' AND d.pulse_source=r.source_incarnation AND d.pulse_takeover_id=r.takeover_id AND d.room_name='island-'||r.predecessor_cluster AND d.participant_identity=r.wallet AND d.state='confirmed' AND d.settled_at IS NOT NULL AND f.wallet=r.wallet AND f.token_floor>=d.cutoff")
            .bind(wallet).bind(source).execute(&mut *lease.tx).await.map_err(|_|GatewayError::Unknown)?;
        lease.commit().await
    }
}
impl SafetyStore {
    /// Wallet lock first, then the optimistic realm owner row. No network wait
    /// is allowed while this lease lives.
    pub(crate) async fn authorize_stamp(
        &self,
        stamp: &super::fence::PulseAdmissionStamp,
        audience: Option<&str>,
        fence: Option<&super::fence::TokenFence>,
        require_unowned: bool,
    ) -> Result<SafetyLease, GatewayError> {
        if !stamp.valid() {
            return Err(GatewayError::Unknown);
        }
        let expected = stamp.safety_snapshot().ok_or(GatewayError::Unknown)?;
        let mut lease = self
            .authorize(&stamp.wallet, Some(expected), stamp.credential_not_before)
            .await?;
        // Read-only replicas and final queue consumers also observe source
        // identity. Retained local retirement forbids its old origin everywhere.
        let retired_origin:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM social.island_takeover_receipts WHERE wallet=$1 AND source_incarnation=$2 AND retired_by_source IS NOT NULL)")
            .bind(&stamp.wallet).bind(&stamp.source_incarnation).fetch_one(&mut *lease.tx).await.map_err(|_|GatewayError::Unknown)?;
        if retired_origin {
            return Err(GatewayError::Unknown);
        }
        if let Some(audience) = audience {
            let row=sqlx::query("SELECT owner_session,owner_epoch,assignment_revision,fencing_token,updated_at>=clock_timestamp()-interval '90 seconds' AS fresh FROM social.archipelago_v4_assignments WHERE deployment_audience=$1 AND owner_address=$2 AND lane_key='realm' FOR UPDATE")
                .bind(audience).bind(&stamp.wallet).fetch_optional(&mut *lease.tx).await.map_err(|_|GatewayError::Unknown)?;
            if let Some(row) = row {
                if require_unowned
                    || row.get::<String, _>("owner_session") != stamp.session
                    || !row.get::<bool, _>("fresh")
                {
                    return Err(GatewayError::Unknown);
                }
                if let Some(fence) = fence {
                    let epoch: i64 = row.get("owner_epoch");
                    let revision: i64 = row.get("assignment_revision");
                    let token: i64 = row.get("fencing_token");
                    if epoch <= 0
                        || revision <= 0
                        || token <= 0
                        || fence.audience != audience
                        || fence.lane != "realm"
                        || fence.owner_session != stamp.session
                        || fence.owner_epoch != epoch as u64
                        || fence.fencing_token != token as u64
                        || !(fence.assignment_revision == revision as u64
                            || fence.assignment_revision == (revision as u64).saturating_add(1))
                    {
                        return Err(GatewayError::Unknown);
                    }
                }
            } else if fence.is_some() {
                return Err(GatewayError::Unknown);
            }
        } else if fence.is_some() {
            return Err(GatewayError::Unknown);
        }
        // Ban writes use their own policy lock. Read after the last possibly
        // blocking owner-row acquisition so a lock wait cannot stale policy.
        // A SELECT projection can precede its FOR UPDATE wait. Re-evaluate
        // expiry after locking, on the database clock, alongside final bans.
        let denied:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM social.user_bans b WHERE (b.banned_address=$1 OR b.banned_device_id=(SELECT NULLIF(device_id,'') FROM social.player_connections WHERE address=$1)) AND b.lifted_at IS NULL AND (b.expires_at IS NULL OR b.expires_at>timezone('UTC',clock_timestamp()))) OR EXISTS(SELECT 1 FROM social.archipelago_v4_assignments WHERE deployment_audience=$2 AND owner_address=$1 AND lane_key='realm' AND updated_at<clock_timestamp()-interval '90 seconds')")
            .bind(&stamp.wallet).bind(audience).fetch_one(&mut *lease.tx).await.map_err(|_|GatewayError::Unknown)?;
        if denied {
            return Err(GatewayError::Unknown);
        }
        Ok(lease)
    }

    /// A final self-hosted removal is authorized only by this exact positively
    /// settled quarantine UUID. Unknown dispatched attempts are never eligible.
    pub(crate) async fn begin_quarantine_final(
        &self,
        id: Uuid,
    ) -> Result<Option<Dispatch>, GatewayError> {
        let wallet: Option<String> = sqlx::query_scalar(
            "SELECT wallet FROM social.island_cleanup_dispatches WHERE dispatch_id=$1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| GatewayError::Unknown)?;
        let Some(wallet) = wallet else {
            return Ok(None);
        };
        let mut lease = self.lock(&wallet).await?;
        let eligible:bool=sqlx::query_scalar("SELECT state='quarantine' AND settled_at IS NOT NULL AND quarantine_until IS NOT NULL FROM social.island_cleanup_dispatches WHERE dispatch_id=$1 AND wallet=$2")
            .bind(id).bind(&wallet).fetch_one(&mut *lease.tx).await.map_err(|_|GatewayError::Unknown)?;
        if !eligible {
            return Ok(None);
        }
        let previous = Self::dispatch(&mut lease.tx, id).await?;
        let first_seen = *self
            .quarantines
            .lock()
            .unwrap()
            .entry(id)
            .or_insert_with(tokio::time::Instant::now);
        let durable_due:bool=sqlx::query_scalar("SELECT quarantine_until<=timezone('UTC',clock_timestamp()) FROM social.island_cleanup_dispatches WHERE dispatch_id=$1")
            .bind(id).fetch_one(&mut *lease.tx).await.map_err(|_|GatewayError::Unknown)?;
        // A fresh process observes a full maximum issuer lifetime, leeway and
        // allowance; it cannot derive a monotonic clock from an old wall time.
        if !durable_due || first_seen.elapsed().as_secs() < FULL_MAX_ISSUER_WAIT_SECONDS {
            return Ok(None);
        }
        let now: i64 = sqlx::query_scalar(
            "SELECT floor(extract(epoch FROM timezone('UTC',clock_timestamp())))::bigint",
        )
        .fetch_one(&mut *lease.tx)
        .await
        .map_err(|_| GatewayError::Unknown)?;
        let cutoff = (now as u64)
            .checked_add(5)
            .ok_or(GatewayError::Unknown)?
            .max(
                previous
                    .cutoff
                    .checked_add(1)
                    .ok_or(GatewayError::Unknown)?,
            )
            .max(
                lease
                    .snapshot
                    .token_floor
                    .checked_add(1)
                    .ok_or(GatewayError::Unknown)?,
            );
        if cutoff > crate::protocol::bus::cluster::MAX_CONSUMER_SECONDS
            || cutoff.saturating_sub(now as u64) >= 59
        {
            return Err(GatewayError::Unknown);
        }
        let changed=sqlx::query("UPDATE social.island_cleanup_dispatches SET state='superseded',quarantine_until=NULL WHERE dispatch_id=$1 AND wallet=$2 AND state='quarantine' AND settled_at IS NOT NULL")
            .bind(id).bind(&wallet).execute(&mut *lease.tx).await.map_err(|_|GatewayError::Unknown)?.rows_affected();
        if changed != 1 {
            return Err(GatewayError::Unknown);
        }
        let next = Uuid::new_v4();
        sqlx::query("INSERT INTO social.island_cleanup_dispatches(dispatch_id,wallet,purpose,pulse_source,pulse_takeover_id,enforcement_cause,intent_root_key,intent_target_key,intent_generation,room_name,participant_identity,cutoff,fence_generation,phase,state) SELECT $2,wallet,purpose,pulse_source,pulse_takeover_id,enforcement_cause,intent_root_key,intent_target_key,intent_generation,room_name,participant_identity,$3,$4,'quarantine_final','dispatched' FROM social.island_cleanup_dispatches WHERE dispatch_id=$1 AND wallet=$5 AND state='superseded' AND settled_at IS NOT NULL")
            .bind(id).bind(next).bind(cutoff as i64).bind(i64::try_from(lease.snapshot.generation).map_err(|_|GatewayError::Unknown)?.checked_add(1).ok_or(GatewayError::Unknown)?).bind(&wallet).execute(&mut *lease.tx).await.map_err(|_|GatewayError::Unknown)?;
        sqlx::query("UPDATE social.island_takeover_receipts SET latest_dispatch=$2 WHERE wallet=$3 AND latest_dispatch=$1")
            .bind(id).bind(next).bind(&wallet).execute(&mut *lease.tx).await.map_err(|_|GatewayError::Unknown)?;
        Self::advance(&mut lease.tx, &wallet, None).await?;
        lease.commit().await?;
        self.quarantines.lock().unwrap().remove(&id);
        Ok(Some(Dispatch {
            dispatch_id: next,
            cutoff,
            state: DispatchState::Dispatched,
            quarantine_until: None,
            phase: DispatchPhase::QuarantineFinal,
            fresh: true,
        }))
    }

    pub(crate) async fn due_takeovers(&self) -> Result<Vec<(String, String)>, GatewayError> {
        let claim = Uuid::new_v4();
        sqlx::query_as("WITH due AS (SELECT r.wallet,r.source_incarnation,r.takeover_id FROM social.island_takeover_receipts r LEFT JOIN social.island_cleanup_dispatches d ON d.dispatch_id=r.latest_dispatch AND d.wallet=r.wallet WHERE NOT r.acknowledged AND r.retired_by_source IS NULL AND r.next_attempt_at<=timezone('UTC',clock_timestamp()) AND (r.claim_until IS NULL OR r.claim_until<=timezone('UTC',clock_timestamp())) AND (d.dispatch_id IS NULL OR d.state IN ('confirmed','rejected','settled_late') OR (d.state='quarantine' AND d.quarantine_until<=timezone('UTC',clock_timestamp()))) ORDER BY r.next_attempt_at,r.wallet,r.source_incarnation,r.takeover_id FOR UPDATE OF r SKIP LOCKED LIMIT 16) UPDATE social.island_takeover_receipts r SET claim_id=$1,claim_until=timezone('UTC',clock_timestamp())+interval '15 seconds',next_attempt_at=timezone('UTC',clock_timestamp())+interval '1 second' FROM due WHERE r.wallet=due.wallet AND r.source_incarnation=due.source_incarnation AND r.takeover_id=due.takeover_id RETURNING r.wallet,r.last_successor_session")
            .bind(claim).fetch_all(&self.pool).await.map_err(|_|GatewayError::Unknown)
    }

    /// Resume an established positive closure after its scheduling intent has
    /// disappeared. Initial definite rejection is deliberately ineligible.
    pub(crate) async fn resume_settled_enforcement(
        &self,
        id: Uuid,
    ) -> Result<Option<Dispatch>, GatewayError> {
        let wallet:Option<String>=sqlx::query_scalar("SELECT wallet FROM social.island_cleanup_dispatches WHERE dispatch_id=$1 AND purpose='island_enforcement' AND state='settled_late' AND settled_at IS NOT NULL")
            .bind(id).fetch_optional(&self.pool).await.map_err(|_|GatewayError::Unknown)?;
        let Some(wallet) = wallet else {
            return Ok(None);
        };
        let mut lease = self.lock(&wallet).await?;
        let row=sqlx::query("SELECT room_name,participant_identity,enforcement_cause,intent_root_key,intent_target_key,intent_generation FROM social.island_cleanup_dispatches WHERE dispatch_id=$1 AND wallet=$2 AND purpose='island_enforcement' AND state='settled_late' AND settled_at IS NOT NULL")
            .bind(id).bind(&wallet).fetch_optional(&mut *lease.tx).await.map_err(|_|GatewayError::Unknown)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let room: String = row.get("room_name");
        let identity: String = row.get("participant_identity");
        let cause: String = row.get("enforcement_cause");
        let root: String = row.get("intent_root_key");
        let target: String = row.get("intent_target_key");
        let generation: Uuid = row.get("intent_generation");
        let next = self
            .insert_dispatch(
                &mut lease,
                &wallet,
                &room,
                &identity,
                0,
                Some(id),
                "island_enforcement",
                None,
                None,
                Some((&cause, &root, &target, generation)),
            )
            .await?;
        lease.commit().await?;
        Ok(next)
    }

    #[cfg(test)]
    pub(crate) fn elapse_quarantine_for_test(&self, id: Uuid, seconds: u64) {
        self.quarantines.lock().unwrap().insert(
            id,
            tokio::time::Instant::now() - std::time::Duration::from_secs(seconds),
        );
    }
}

impl crate::gatekeeper::CommsState {
    pub(crate) async fn verify_island_stamp(
        self: &std::sync::Arc<Self>,
        stamp: &super::fence::PulseAdmissionStamp,
    ) -> bool {
        use super::subscriber::ClusterGateway;
        use prost::Message;
        #[cfg(test)]
        if stamp.safety_generation == 0 {
            return match self.island_subscriber().upgrade() {
                Some(subscriber) => subscriber.verify_stamp(stamp).await,
                None => false,
            };
        }
        if !stamp.valid() || stamp.safety_snapshot().is_none() {
            return false;
        }
        if !matches!(
            super::gateway::StateGateway::new(self.clone())
                .is_denied(&stamp.wallet)
                .await,
            Ok(false)
        ) {
            return false;
        }
        let lookup = crate::protocol::bus::cluster::ClusterLookup {
            wallet: stamp.wallet.clone(),
            session: stamp.session.clone(),
        };
        let Ok(payload) = self.bus.request(&lookup.subject(), lookup.encode()).await else {
            return false;
        };
        let Ok(snapshot) = crate::protocol::pulse::decentraland::pulse::PeerClusterSnapshot::decode(
            payload.as_slice(),
        ) else {
            return false;
        };
        if !crate::protocol::bus::cluster::valid_reply(&lookup, &snapshot)
            || !stamp.matches_snapshot(&snapshot)
        {
            return false;
        }
        self.island_stamp_ready_sql(stamp).await
    }
    pub(crate) async fn island_stamp_ready_sql(
        self: &std::sync::Arc<Self>,
        stamp: &super::fence::PulseAdmissionStamp,
    ) -> bool {
        #[cfg(test)]
        if stamp.safety_generation == 0 {
            return match self.island_subscriber().upgrade() {
                Some(subscriber) => subscriber.stamp_ready(stamp).await,
                None => false,
            };
        }
        let Ok(lease) = self
            .island_safety
            .authorize_stamp(
                stamp,
                self.cluster.control_v4_audience.as_deref(),
                None,
                false,
            )
            .await
        else {
            return false;
        };
        if lease.commit().await.is_err() {
            return false;
        }
        let now = chrono::Utc::now().timestamp_millis().max(0) as u64;
        super::subscriber::floor_wait_ms(stamp.token_not_before, now) == Some(0)
            && super::subscriber::floor_wait_ms(stamp.credential_not_before, now) == Some(0)
    }
    pub(crate) async fn island_fenced_stamp_ready_sql(
        self: &std::sync::Arc<Self>,
        stamp: &super::fence::PulseAdmissionStamp,
        fence: &super::fence::TokenFence,
        presented_device: Option<&str>,
    ) -> bool {
        let Ok(mut lease) = self
            .island_safety
            .authorize_stamp(
                stamp,
                self.cluster.control_v4_audience.as_deref(),
                Some(fence),
                false,
            )
            .await
        else {
            return false;
        };
        let exact:Result<bool,_>=sqlx::query_scalar("SELECT assignment_revision=$4 AND owner_epoch=$5 AND fencing_token=$6 AND owner_session=$3 FROM social.archipelago_v4_assignments WHERE deployment_audience=$1 AND owner_address=$2 AND lane_key='realm'")
            .bind(&fence.audience).bind(&stamp.wallet).bind(&fence.owner_session).bind(i64::try_from(fence.assignment_revision).unwrap_or(-1)).bind(i64::try_from(fence.owner_epoch).unwrap_or(-1)).bind(i64::try_from(fence.fencing_token).unwrap_or(-1)).fetch_one(&mut *lease.tx).await;
        if !matches!(exact, Ok(true)) {
            return false;
        }
        if let Some(device) = presented_device {
            let banned:Result<bool,_>=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM social.user_bans WHERE banned_device_id=$1 AND lifted_at IS NULL AND (expires_at IS NULL OR expires_at>timezone('UTC',clock_timestamp())))")
                .bind(device).fetch_one(&mut *lease.tx).await;
            if !matches!(banned, Ok(false)) {
                return false;
            }
        }
        if lease.commit().await.is_err() {
            return false;
        }
        let now = chrono::Utc::now().timestamp_millis().max(0) as u64;
        super::subscriber::floor_wait_ms(stamp.token_not_before, now) == Some(0)
            && super::subscriber::floor_wait_ms(stamp.credential_not_before, now) == Some(0)
    }
}

impl SafetyStore {
    /// Bounded primary maintenance also visits inactive settled wallets. Each
    /// wallet is locked separately; no cross-wallet deletion occurs while
    /// another wallet lock is held. Persistent fences/floors are never deleted.
    pub(crate) async fn compact_settled(&self) -> Result<(), GatewayError> {
        let wallets:Vec<String>=sqlx::query_scalar("SELECT d.wallet FROM social.island_cleanup_dispatches d JOIN social.island_admission_fences f ON f.wallet=d.wallet WHERE d.state IN ('confirmed','rejected','superseded') AND d.settled_at IS NOT NULL AND (d.state<>'confirmed' OR f.token_floor>=d.cutoff) AND d.cutoff+$1<floor(extract(epoch FROM timezone('UTC',clock_timestamp()))) AND d.settled_at+$1*interval '1 second'<timezone('UTC',clock_timestamp()) AND NOT EXISTS(SELECT 1 FROM social.island_takeover_receipts r WHERE r.wallet=d.wallet AND r.latest_dispatch=d.dispatch_id AND NOT r.acknowledged AND r.retired_by_source IS NULL) GROUP BY d.wallet ORDER BY min(d.settled_at),d.wallet LIMIT 16")
            .bind(SETTLED_RETENTION_SECONDS).fetch_all(&self.pool).await.map_err(|_|GatewayError::Unknown)?;
        for wallet in wallets {
            let result=async {
            let mut lease = self.lock(&wallet).await?;
            sqlx::query("WITH due AS (SELECT r.wallet,r.source_incarnation,r.takeover_id FROM social.island_takeover_receipts r JOIN social.island_cleanup_dispatches d ON d.wallet=r.wallet AND d.dispatch_id=r.latest_dispatch JOIN social.island_admission_fences f ON f.wallet=r.wallet WHERE r.wallet=$1 AND (r.acknowledged OR r.retired_by_source IS NOT NULL) AND d.state='confirmed' AND d.settled_at IS NOT NULL AND f.token_floor>=d.cutoff AND d.cutoff+$2<floor(extract(epoch FROM timezone('UTC',clock_timestamp()))) AND d.settled_at+$2*interval '1 second'<timezone('UTC',clock_timestamp()) ORDER BY r.created_at,r.source_incarnation,r.takeover_id FOR UPDATE OF r SKIP LOCKED LIMIT 64) DELETE FROM social.island_takeover_receipts r USING due WHERE r.wallet=due.wallet AND r.source_incarnation=due.source_incarnation AND r.takeover_id=due.takeover_id")
                .bind(&wallet).bind(SETTLED_RETENTION_SECONDS).execute(&mut *lease.tx).await.map_err(|_|GatewayError::Unknown)?;
            // A known rejection never advanced a floor and had no remote
            // effect. Superseded attempts are positively settled, while the
            // immutable logical receipt/newer attempt retains the obligation.
            // Confirmed attempts additionally require their durable floor.
            sqlx::query("WITH due AS (SELECT d.dispatch_id FROM social.island_cleanup_dispatches d JOIN social.island_admission_fences f ON f.wallet=d.wallet WHERE d.wallet=$1 AND d.state IN ('confirmed','rejected','superseded') AND d.settled_at IS NOT NULL AND (d.state<>'confirmed' OR f.token_floor>=d.cutoff) AND d.cutoff+$2<floor(extract(epoch FROM timezone('UTC',clock_timestamp()))) AND d.settled_at+$2*interval '1 second'<timezone('UTC',clock_timestamp()) AND NOT EXISTS(SELECT 1 FROM social.island_takeover_receipts r WHERE r.latest_dispatch=d.dispatch_id) ORDER BY d.settled_at,d.dispatch_id FOR UPDATE OF d SKIP LOCKED LIMIT 64) DELETE FROM social.island_cleanup_dispatches d USING due WHERE d.dispatch_id=due.dispatch_id")
                .bind(&wallet).bind(SETTLED_RETENTION_SECONDS).execute(&mut *lease.tx).await.map_err(|_|GatewayError::Unknown)?;
            lease.commit().await
            }.await;
            if result.is_err() {
                metrics::counter!("comms_island_cleanup_maintenance_failed_total").increment(1);
                tracing::warn!(%wallet,"island compaction wallet unavailable; retaining all evidence");
            }
        }
        Ok(())
    }
}
#[cfg(test)]
mod durable_tests {
    use super::*;

    #[test]
    fn stored_dispatch_values_decode_exactly_and_unknown_values_fail_closed() {
        for (label, value) in [
            ("dispatched", DispatchState::Dispatched),
            ("confirmed", DispatchState::Confirmed),
            ("rejected", DispatchState::Rejected),
            ("quarantine", DispatchState::Quarantine),
            ("settled_late", DispatchState::SettledLate),
            ("superseded", DispatchState::Superseded),
        ] {
            assert_eq!(DispatchState::decode(label).unwrap(), value);
            assert_eq!(value.as_str(), label);
        }
        for (label, value) in [
            ("initial", DispatchPhase::Initial),
            ("quarantine_final", DispatchPhase::QuarantineFinal),
        ] {
            assert_eq!(DispatchPhase::decode(label).unwrap(), value);
            assert_eq!(value.as_str(), label);
        }
        for unknown in ["", "CONFIRMED", "confirmed ", "future", "initial"] {
            assert!(matches!(
                DispatchState::decode(unknown),
                Err(GatewayError::Unknown)
            ));
        }
        for unknown in ["", "INITIAL", "initial ", "future", "confirmed"] {
            assert!(matches!(
                DispatchPhase::decode(unknown),
                Err(GatewayError::Unknown)
            ));
        }
    }

    #[test]
    fn settlement_classification_preserves_boundaries_and_caller_applicability() {
        use QuarantinePolicy::{Disabled, Enabled};
        use RemovalOutcome::{Positive, Rejected, Unknown};
        for (now, expected) in [
            (98_999, Settlement::Confirmed),
            (99_000, Settlement::SettledLate),
            (200_000, Settlement::SettledLate),
        ] {
            assert_eq!(
                classify_removal(Positive, DispatchPhase::Initial, Disabled, now, 100),
                Some(expected)
            );
        }
        assert_eq!(
            classify_removal(
                Positive,
                DispatchPhase::Initial,
                Enabled { applies: true },
                200_000,
                100
            ),
            Some(Settlement::Quarantine(122))
        );
        // Cross-room subscriber success must not inherit enforcement quarantine.
        assert_eq!(
            classify_removal(
                Positive,
                DispatchPhase::Initial,
                Enabled { applies: false },
                200_000,
                100
            ),
            Some(Settlement::Confirmed)
        );
        assert_eq!(
            classify_removal(
                Positive,
                DispatchPhase::QuarantineFinal,
                Enabled { applies: true },
                200_000,
                100
            ),
            Some(Settlement::Confirmed)
        );
        for policy in [
            Disabled,
            Enabled { applies: true },
            Enabled { applies: false },
        ] {
            assert_eq!(
                classify_removal(Rejected, DispatchPhase::Initial, policy, 200_000, 100),
                Some(Settlement::Rejected)
            );
            assert_eq!(
                classify_removal(Rejected, DispatchPhase::QuarantineFinal, policy, 0, 100),
                Some(Settlement::SettledLate)
            );
            for phase in [DispatchPhase::Initial, DispatchPhase::QuarantineFinal] {
                assert_eq!(classify_removal(Unknown, phase, policy, 200_000, 100), None);
            }
        }
    }

    async fn fixture() -> Option<(SafetyStore, String)> {
        let configured = std::env::var("CATALYST_TEST_DATABASE_URL")
            .ok()
            .is_some_and(|value| !value.trim().is_empty());
        if !configured {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "durable safety PostgreSQL bodies require CATALYST_TEST_DATABASE_URL in CI"
            );
            eprintln!(
                "local skip: durable safety PostgreSQL fixture requires CATALYST_TEST_DATABASE_URL"
            );
            return None;
        }
        let pool = crate::db::scratch::pool(8)
            .await
            .expect("configured durable safety PostgreSQL fixture");
        Some((SafetyStore::new(pool), uuid::Uuid::new_v4().to_string()))
    }

    fn takeover(id: &str) -> crate::protocol::pulse::decentraland::pulse::PendingTakeover {
        crate::protocol::pulse::decentraland::pulse::PendingTakeover {
            takeover_id: id.into(),
            displaced_session: "old-session".into(),
            displaced_cluster_id: "old-room".into(),
            displaced_realm: "realm".into(),
        }
    }

    #[tokio::test]
    async fn durable_unknown_survives_new_reader_policy_deletion_and_other_sources() {
        let Some((store, wallet)) = fixture().await else {
            return;
        };
        let op = takeover("one");
        store
            .prepare_takeover(&wallet, "source-a", "successor", &op)
            .await
            .unwrap();
        let initial = store
            .begin_takeover(&wallet, "source-a", &op, 0)
            .await
            .unwrap()
            .unwrap();
        assert!(initial.fresh);
        assert!(
            store
                .authorize(&wallet, None, initial.cutoff)
                .await
                .is_err()
        );
        let restarted = SafetyStore::new(store.pool.clone());
        let replay = restarted
            .begin_takeover(&wallet, "source-a", &op, 9000)
            .await
            .unwrap()
            .unwrap();
        assert!(!replay.fresh);
        assert_eq!(initial.dispatch_id, replay.dispatch_id);
        assert_eq!(initial.cutoff, replay.cutoff);
        let second = takeover("two");
        restarted
            .prepare_takeover(&wallet, "source-b", "next", &second)
            .await
            .unwrap();
        assert!(
            restarted
                .begin_takeover(&wallet, "source-b", &second, 0)
                .await
                .unwrap()
                .is_none()
        );
        let mut retargeted = op.clone();
        retargeted.displaced_cluster_id = "another-room".into();
        assert!(
            restarted
                .prepare_takeover(&wallet, "source-a", "next", &retargeted)
                .await
                .is_err()
        );
        assert!(
            restarted
                .authorize(&wallet, None, initial.cutoff)
                .await
                .is_err()
        );
        assert!(
            restarted
                .authorize(&format!("other-{wallet}"), None, 0)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn durable_stable_absent_wallet_lock_rejects_candidate_after_first_dispatch() {
        let Some((store, wallet)) = fixture().await else {
            return;
        };
        let lease = store.authorize(&wallet, None, 0).await.unwrap();
        let stamp = lease.snapshot;
        let worker_pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_with(
                store
                    .pool
                    .connect_options()
                    .as_ref()
                    .clone()
                    .application_name(&wallet),
            )
            .await
            .unwrap();
        let worker = SafetyStore::new(worker_pool);
        let w = wallet.clone();
        let entered = std::sync::Arc::new(tokio::sync::Notify::new());
        let signal = entered.clone();
        let task = tokio::spawn(async move {
            signal.notify_one();
            worker
                .prepare_takeover(&w, "source", "next", &takeover("one"))
                .await
                .unwrap();
            worker
                .begin_takeover(&w, "source", &takeover("one"), 0)
                .await
                .unwrap()
                .unwrap()
        });
        entered.notified().await;
        // Competing INSERT/SELECT FOR UPDATE must wait for the first mint's
        // stable row, even though that row did not exist before this lease.
        tokio::time::timeout(std::time::Duration::from_secs(1),async {
            loop {
                let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE application_name=$1 AND wait_event_type='Lock')")
                    .bind(&wallet).fetch_one(&store.pool).await.unwrap();
                if waiting {break;}
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }).await.expect("competing connection actually reached the wallet lock");
        assert!(!task.is_finished());
        lease.commit().await.unwrap();
        let dispatch = task.await.unwrap();
        assert!(dispatch.fresh);
        assert!(
            store
                .authorize(&wallet, Some(stamp), dispatch.cutoff)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn durable_positive_settlement_keeps_floor_after_ack_and_rejects_stale_generation() {
        let Some((store, wallet)) = fixture().await else {
            return;
        };
        let first = store.authorize(&wallet, None, 0).await.unwrap();
        let old = first.snapshot;
        first.commit().await.unwrap();
        let op = takeover("one");
        store
            .prepare_takeover(&wallet, "source", "next", &op)
            .await
            .unwrap();
        let dispatch = store
            .begin_takeover(&wallet, "source", &op, 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            store
                .settle(dispatch.dispatch_id, Settlement::Confirmed)
                .await
                .unwrap()
        );
        assert!(
            !store
                .settle(dispatch.dispatch_id, Settlement::Rejected)
                .await
                .unwrap()
        );
        store.acknowledge(&wallet, "source", &[]).await.unwrap();
        let current = store
            .authorize(&wallet, None, dispatch.cutoff)
            .await
            .unwrap();
        assert_eq!(current.snapshot.token_floor, dispatch.cutoff);
        assert!(current.snapshot.generation > old.generation);
        current.commit().await.unwrap();
        assert!(
            store
                .authorize(&wallet, Some(old), dispatch.cutoff)
                .await
                .is_err()
        );
        assert!(
            store
                .authorize(&wallet, None, dispatch.cutoff - 1)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn durable_two_connections_dispatch_once_and_unknown_has_no_lease_expiry() {
        let Some((store, wallet)) = fixture().await else {
            return;
        };
        let op = takeover("one");
        store
            .prepare_takeover(&wallet, "source", "next", &op)
            .await
            .unwrap();
        let (left, right) = tokio::join!(
            store.begin_takeover(&wallet, "source", &op, 0),
            store.begin_takeover(&wallet, "source", &op, 0),
        );
        let left = left.unwrap().unwrap();
        let right = right.unwrap().unwrap();
        assert_eq!(left.dispatch_id, right.dispatch_id);
        assert_ne!(left.fresh, right.fresh);
        sqlx::query("UPDATE social.island_takeover_receipts SET claim_id=$2,claim_until=timestamp '1970-01-01',next_attempt_at=timestamp '1970-01-01' WHERE wallet=$1")
            .bind(&wallet).bind(uuid::Uuid::new_v4()).execute(&store.pool).await.unwrap();
        let next = store
            .begin_takeover(&wallet, "source", &op, 1_000_000)
            .await
            .unwrap()
            .unwrap();
        assert!(!next.fresh);
        assert_eq!(next.dispatch_id, left.dispatch_id);
        assert!(store.authorize(&wallet, None, next.cutoff).await.is_err());
    }

    #[tokio::test]
    async fn durable_closed_pool_never_falls_back_to_local_readiness() {
        let Some((store, wallet)) = fixture().await else {
            return;
        };
        store.pool.close().await;
        assert!(store.authorize(&wallet, None, 0).await.is_err());
    }
    #[tokio::test]
    async fn durable_unknown_never_enters_quarantine_final_and_restart_waits_all_issuer_tokens() {
        let Some((store, wallet)) = fixture().await else {
            return;
        };
        let op = takeover("quarantine");
        store
            .prepare_takeover(&wallet, "source", "next", &op)
            .await
            .unwrap();
        let initial = store
            .begin_takeover(&wallet, "source", &op, 0)
            .await
            .unwrap()
            .unwrap();
        assert!(
            store
                .begin_quarantine_final(initial.dispatch_id)
                .await
                .unwrap()
                .is_none()
        );
        // Only a positive settled initial response may establish quarantine.
        store
            .settle(initial.dispatch_id, Settlement::Quarantine(122))
            .await
            .unwrap();
        sqlx::query("UPDATE social.island_cleanup_dispatches SET quarantine_until=timestamp '1970-01-01' WHERE dispatch_id=$1")
            .bind(initial.dispatch_id).execute(&store.pool).await.unwrap();
        let restarted = SafetyStore::new(store.pool.clone());
        assert!(
            restarted
                .begin_quarantine_final(initial.dispatch_id)
                .await
                .unwrap()
                .is_none()
        );
        // A configured island TTL shorter than fixed60s refresh lifetime must
        // still not permit a final removal after only61 seconds of observation.
        store.quarantines.lock().unwrap().insert(
            initial.dispatch_id,
            tokio::time::Instant::now() - std::time::Duration::from_secs(61),
        );
        assert!(
            store
                .begin_quarantine_final(initial.dispatch_id)
                .await
                .unwrap()
                .is_none()
        );
        store.quarantines.lock().unwrap().insert(
            initial.dispatch_id,
            tokio::time::Instant::now() - std::time::Duration::from_secs(123),
        );
        let final_attempt = store
            .begin_quarantine_final(initial.dispatch_id)
            .await
            .unwrap()
            .unwrap();
        assert_ne!(final_attempt.dispatch_id, initial.dispatch_id);
        assert_eq!(final_attempt.phase, DispatchPhase::QuarantineFinal);
        assert!(
            store
                .begin_quarantine_final(final_attempt.dispatch_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            restarted
                .authorize(&wallet, None, final_attempt.cutoff)
                .await
                .is_err()
        );
        assert!(
            !store
                .settle(initial.dispatch_id, Settlement::Confirmed)
                .await
                .unwrap()
        );
        assert!(
            store
                .settle(final_attempt.dispatch_id, Settlement::Confirmed)
                .await
                .unwrap()
        );
        let lease = store
            .authorize(&wallet, None, final_attempt.cutoff)
            .await
            .unwrap();
        assert_eq!(lease.snapshot.token_floor, final_attempt.cutoff);
    }

    async fn isolated() -> Option<(SafetyStore, sqlx::PgPool, String)> {
        let (base, _) = fixture().await?;
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_with(
                base.pool
                    .connect_options()
                    .as_ref()
                    .clone()
                    .database("postgres"),
            )
            .await
            .unwrap();
        let name = format!("dcl_island_safety_{}", Uuid::new_v4().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
            .execute(&admin)
            .await
            .unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(8)
            .connect_with(base.pool.connect_options().as_ref().clone().database(&name))
            .await
            .unwrap();
        Some((SafetyStore::new(pool), admin, name))
    }

    async fn teardown(store: SafetyStore, admin: sqlx::PgPool, name: String) {
        assert!(
            name.starts_with("dcl_island_safety_")
                && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
        );
        store.pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE \"{name}\" WITH (FORCE)"
        )))
        .execute(&admin)
        .await
        .unwrap();
        admin.close().await;
    }

    #[tokio::test]
    async fn durable_capacity_budgets_logical_obligations_and_existing_work_keeps_progress() {
        let Some((store, admin, name)) = isolated().await else {
            return;
        };
        crate::db::apply_schema(&store.pool).await.unwrap();
        sqlx::query("INSERT INTO social.island_admission_fences(wallet) SELECT 'capacity-'||n FROM generate_series(1,$1) n").bind(CAPACITY).execute(&store.pool).await.unwrap();
        sqlx::query("INSERT INTO social.island_takeover_receipts(wallet,source_incarnation,takeover_id,predecessor_session,predecessor_cluster,predecessor_realm,last_successor_session) SELECT wallet,'source','one','old-session','old-room','realm','next' FROM social.island_admission_fences").execute(&store.pool).await.unwrap();
        let op = takeover("one");
        assert!(
            store
                .prepare_takeover("excess", "source", "next", &op)
                .await
                .is_err()
        );
        store
            .prepare_takeover("capacity-1", "source", "next", &op)
            .await
            .unwrap();
        let initial = store
            .begin_takeover("capacity-1", "source", &op, 0)
            .await
            .unwrap()
            .unwrap();
        store
            .settle(initial.dispatch_id, Settlement::Quarantine(122))
            .await
            .unwrap();
        sqlx::query("UPDATE social.island_cleanup_dispatches SET quarantine_until=timestamp '1970-01-01' WHERE dispatch_id=$1").bind(initial.dispatch_id).execute(&store.pool).await.unwrap();
        store.elapse_quarantine_for_test(initial.dispatch_id, 123);
        let final_attempt = store
            .begin_quarantine_final(initial.dispatch_id)
            .await
            .unwrap()
            .expect("existing obligation keeps a replacement slot at global capacity");
        assert_eq!(final_attempt.phase, DispatchPhase::QuarantineFinal);
        assert!(
            store
                .authorize("capacity-1", None, final_attempt.cutoff)
                .await
                .is_err()
        );
        store
            .settle(final_attempt.dispatch_id, Settlement::Confirmed)
            .await
            .unwrap();
        store
            .acknowledge("capacity-1", "source", &[])
            .await
            .unwrap();
        store
            .prepare_takeover("excess", "source", "next", &op)
            .await
            .unwrap();
        let lease = store
            .authorize("capacity-1", None, final_attempt.cutoff)
            .await
            .unwrap();
        assert_eq!(lease.snapshot.token_floor, final_attempt.cutoff);
        lease.commit().await.unwrap();
        teardown(store, admin, name).await;
    }

    #[tokio::test]
    async fn durable_compaction_keeps_newer_unknown_and_persistent_confirmed_floor() {
        let Some((store, wallet)) = fixture().await else {
            return;
        };
        let op = takeover("one");
        store
            .prepare_takeover(&wallet, "source", "next", &op)
            .await
            .unwrap();
        let current = store
            .begin_takeover(&wallet, "source", &op, 0)
            .await
            .unwrap()
            .unwrap();
        let old = Uuid::new_v4();
        sqlx::query("INSERT INTO social.island_cleanup_dispatches(dispatch_id,wallet,purpose,pulse_source,pulse_takeover_id,room_name,participant_identity,cutoff,fence_generation,phase,state,settled_at) VALUES ($1,$2,'pulse_takeover','source','one','island-old-room',$2,100,1,'initial','superseded',timestamp '1970-01-01')")
            .bind(old).bind(&wallet).execute(&store.pool).await.unwrap();
        store.compact_settled().await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM social.island_cleanup_dispatches WHERE dispatch_id=$1"
            )
            .bind(old)
            .fetch_one(&store.pool)
            .await
            .unwrap(),
            0
        );
        assert_eq!(
            sqlx::query_scalar::<_, String>(
                "SELECT state FROM social.island_cleanup_dispatches WHERE dispatch_id=$1"
            )
            .bind(current.dispatch_id)
            .fetch_one(&store.pool)
            .await
            .unwrap(),
            "dispatched"
        );
        assert!(
            store
                .authorize(&wallet, None, current.cutoff)
                .await
                .is_err()
        );
        let settled_wallet = format!("settled-{wallet}");
        let settled = Uuid::new_v4();
        sqlx::query("INSERT INTO social.island_admission_fences(wallet,generation,token_floor) VALUES ($1,9,100)").bind(&settled_wallet).execute(&store.pool).await.unwrap();
        sqlx::query("INSERT INTO social.island_cleanup_dispatches(dispatch_id,wallet,purpose,pulse_source,pulse_takeover_id,room_name,participant_identity,cutoff,fence_generation,phase,state,settled_at) VALUES ($1,$2,'pulse_takeover','source','settled','island-old',$2,100,9,'initial','confirmed',timestamp '1970-01-01')")
            .bind(settled).bind(&settled_wallet).execute(&store.pool).await.unwrap();
        sqlx::query("INSERT INTO social.island_takeover_receipts(wallet,source_incarnation,takeover_id,predecessor_session,predecessor_cluster,predecessor_realm,last_successor_session,latest_dispatch) VALUES ($1,'source','settled','old','old','realm','next',$2)")
            .bind(&settled_wallet).bind(settled).execute(&store.pool).await.unwrap();
        store.compact_settled().await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM social.island_cleanup_dispatches WHERE dispatch_id=$1"
            )
            .bind(settled)
            .fetch_one(&store.pool)
            .await
            .unwrap(),
            1,
            "unacknowledged positive replay remains"
        );
        store
            .acknowledge(&settled_wallet, "source", &[])
            .await
            .unwrap();
        store.compact_settled().await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM social.island_cleanup_dispatches WHERE dispatch_id=$1"
            )
            .bind(settled)
            .fetch_one(&store.pool)
            .await
            .unwrap(),
            0
        );
        assert!(store.authorize(&settled_wallet, None, 99).await.is_err());
        let lease = store.authorize(&settled_wallet, None, 100).await.unwrap();
        assert_eq!(lease.snapshot.token_floor, 100);
    }

    #[tokio::test]
    async fn durable_enforcement_missing_cause_is_a_check_violation_not_a_null_pass() {
        let Some((store, wallet)) = fixture().await else {
            return;
        };
        store
            .authorize(&wallet, None, 0)
            .await
            .unwrap()
            .commit()
            .await
            .unwrap();
        let error=sqlx::query("INSERT INTO social.island_cleanup_dispatches(dispatch_id,wallet,purpose,intent_root_key,intent_target_key,intent_generation,room_name,participant_identity,cutoff,fence_generation,phase,state) VALUES ($1,$2,'island_enforcement','root','target',$3,'island-room',$2,100,1,'initial','dispatched')")
            .bind(Uuid::new_v4()).bind(&wallet).bind(Uuid::new_v4()).execute(&store.pool).await.unwrap_err();
        assert_eq!(
            error.as_database_error().unwrap().code().as_deref(),
            Some("23514")
        );
        assert!(store.authorize(&wallet, None, 0).await.is_ok());
    }

    #[tokio::test]
    async fn durable_object_pass_upgrade_preserves_assignments_and_existing_floor_and_matches_fresh()
     {
        let Some((store, admin, name)) = isolated().await else {
            return;
        };
        let sql = crate::db::SCHEMA_SQL.replace("\r\n", "\n");
        let start = sql
            .find("-- Workload: rare island cleanup transitions")
            .unwrap();
        let end = start + sql[start..].find("-- ════ schemas/50_create.sql").unwrap();
        let ddl = &sql[start..end];
        let mut old = sql.clone();
        old.replace_range(start..end, "");
        // Both strings are derived only from the embedded reviewed schema.
        sqlx::raw_sql(sqlx::AssertSqlSafe(old.as_str()))
            .execute(&store.pool)
            .await
            .unwrap();
        let steps = crate::db::schema_migrate::parse_steps(crate::db::MIGRATIONS_SQL);
        crate::db::schema_migrate::record_steps(&store.pool, &steps, true)
            .await
            .unwrap();
        let fence = crate::db::schema_migrate::statements(ddl)
            .into_iter()
            .find(|s| s.contains("CREATE TABLE social.island_admission_fences ("))
            .unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(fence.as_str()))
            .execute(&store.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO social.island_admission_fences(wallet,generation,token_floor) VALUES ('preserved',7,100)").execute(&store.pool).await.unwrap();
        sqlx::query("INSERT INTO social.archipelago_v4_assignments(deployment_audience,owner_address,lane_key,authority_incarnation,assignment_revision,owner_session,owner_epoch,fencing_token) VALUES ('upgrade','wallet','realm','authority',8,'session',9,10)").execute(&store.pool).await.unwrap();
        crate::db::schema_migrate::migrate(
            &store.pool,
            crate::db::SCHEMA_SQL,
            crate::db::MIGRATIONS_SQL,
        )
        .await
        .unwrap();
        assert_eq!(sqlx::query_as::<_,(i64,i64)>("SELECT generation,token_floor FROM social.island_admission_fences WHERE wallet='preserved'").fetch_one(&store.pool).await.unwrap(),(7,100));
        assert_eq!(sqlx::query_as::<_,(i64,i64,i64)>("SELECT assignment_revision,owner_epoch,fencing_token FROM social.archipelago_v4_assignments WHERE deployment_audience='upgrade'").fetch_one(&store.pool).await.unwrap(),(8,9,10));
        let Some((fresh, fa, fnm)) = isolated().await else {
            panic!("same prerequisite disappeared");
        };
        crate::db::apply_schema(&fresh.pool).await.unwrap();
        let shape = "SELECT c.relname||':'||pg_get_constraintdef(k.oid) FROM pg_constraint k JOIN pg_class c ON c.oid=k.conrelid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='social' AND c.relname IN ('island_admission_fences','island_cleanup_dispatches','island_takeover_receipts') UNION ALL SELECT tablename||':'||indexdef FROM pg_indexes WHERE schemaname='social' AND tablename IN ('island_admission_fences','island_cleanup_dispatches','island_takeover_receipts') ORDER BY 1";
        let upgraded: Vec<String> = sqlx::query_scalar(shape)
            .fetch_all(&store.pool)
            .await
            .unwrap();
        let created: Vec<String> = sqlx::query_scalar(shape)
            .fetch_all(&fresh.pool)
            .await
            .unwrap();
        assert_eq!(upgraded, created);
        teardown(fresh, fa, fnm).await;
        teardown(store, admin, name).await;
    }
    #[tokio::test]
    async fn durable_different_source_retires_only_exact_confirmed_obligation_without_ack() {
        let Some((store, wallet)) = fixture().await else {
            return;
        };
        let op = takeover("confirmed");
        store
            .prepare_takeover(&wallet, "source-a", "next", &op)
            .await
            .unwrap();
        let d = store
            .begin_takeover(&wallet, "source-a", &op, 0)
            .await
            .unwrap()
            .unwrap();
        store
            .settle(d.dispatch_id, Settlement::Confirmed)
            .await
            .unwrap();
        let floor = store.snapshot(&wallet).await.unwrap();
        store.acknowledge(&wallet, "source-b", &[]).await.unwrap();
        let ack: bool = sqlx::query_scalar(
            "SELECT acknowledged FROM social.island_takeover_receipts WHERE wallet=$1",
        )
        .bind(&wallet)
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert!(
            !ack,
            "local retirement cannot fabricate remote acknowledgment"
        );
        assert!(
            !store
                .due_takeovers()
                .await
                .unwrap()
                .iter()
                .any(|(w, _)| w == &wallet),
            "obsolete confirmed completion must not be replayed"
        );
        assert_eq!(
            sqlx::query_scalar::<_, Option<String>>(
                "SELECT retired_by_source FROM social.island_takeover_receipts WHERE wallet=$1"
            )
            .bind(&wallet)
            .fetch_one(&store.pool)
            .await
            .unwrap(),
            Some("source-b".into())
        );
        assert_eq!(store.snapshot(&wallet).await.unwrap(), floor);
        assert!(store.authorize(&wallet, None, d.cutoff - 1).await.is_err());
        assert!(
            store
                .acknowledge(&wallet, "source-a", std::slice::from_ref(&op))
                .await
                .is_err(),
            "observed retired origin requires maintenance"
        );
        assert!(
            store
                .begin_takeover(&wallet, "source-a", &op, 0)
                .await
                .is_err()
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM social.island_cleanup_dispatches WHERE wallet=$1"
            )
            .bind(&wallet)
            .fetch_one(&store.pool)
            .await
            .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn durable_source_change_never_retires_unfinished_or_uncertain_cleanup() {
        let Some((store, base)) = fixture().await else {
            return;
        };
        for mode in [
            "prepared",
            "rejected",
            "unknown",
            "quarantine",
            "settled_late",
        ] {
            let wallet = format!("{mode}-{base}");
            let op = takeover(mode);
            store
                .prepare_takeover(&wallet, "source-a", "next", &op)
                .await
                .unwrap();
            if mode != "prepared" {
                let d = store
                    .begin_takeover(&wallet, "source-a", &op, 0)
                    .await
                    .unwrap()
                    .unwrap();
                match mode {
                    "rejected" => {
                        store
                            .settle(d.dispatch_id, Settlement::Rejected)
                            .await
                            .unwrap();
                    }
                    "quarantine" => {
                        store
                            .settle(d.dispatch_id, Settlement::Quarantine(122))
                            .await
                            .unwrap();
                    }
                    "settled_late" => {
                        store
                            .settle(d.dispatch_id, Settlement::SettledLate)
                            .await
                            .unwrap();
                    }
                    _ => {}
                }
            }
            store.acknowledge(&wallet, "source-b", &[]).await.unwrap();
            assert!(
                store.snapshot(&wallet).await.is_err(),
                "{mode} must remain closed"
            );
            assert!(
                !sqlx::query_scalar::<_, bool>(
                    "SELECT acknowledged FROM social.island_takeover_receipts WHERE wallet=$1"
                )
                .bind(&wallet)
                .fetch_one(&store.pool)
                .await
                .unwrap()
            );
        }
    }

    #[tokio::test]
    async fn durable_retirement_rechecks_latest_attempt_after_wallet_lock_wait() {
        let Some((store, wallet)) = fixture().await else {
            return;
        };
        let op = takeover("race");
        store
            .prepare_takeover(&wallet, "source-a", "next", &op)
            .await
            .unwrap();
        let old = store
            .begin_takeover(&wallet, "source-a", &op, 0)
            .await
            .unwrap()
            .unwrap();
        store
            .settle(old.dispatch_id, Settlement::Confirmed)
            .await
            .unwrap();
        let label = format!("retirement-waiter-{}", Uuid::new_v4());
        let other_pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_with(
                store
                    .pool
                    .connect_options()
                    .as_ref()
                    .clone()
                    .application_name(&label),
            )
            .await
            .unwrap();
        let other = SafetyStore::new(other_pool.clone());
        let mut lease = store.lock(&wallet).await.unwrap();
        let target = wallet.clone();
        let work = tokio::spawn(async move { other.acknowledge(&target, "source-b", &[]).await });
        tokio::time::timeout(std::time::Duration::from_secs(1),async{loop{
            let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND application_name=$1 AND wait_event_type='Lock' AND query LIKE '%SELECT generation,token_floor%')").bind(&label).fetch_one(&store.pool).await.unwrap();
            if waiting {break;}tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }}).await.unwrap();
        // Actual core attempt insertion and pointer change share the wallet transaction.
        let next = store
            .insert_dispatch(
                &mut lease,
                &wallet,
                "island-old-room",
                &wallet,
                old.cutoff,
                Some(old.dispatch_id),
                "pulse_takeover",
                Some("source-a"),
                Some("race"),
                None,
            )
            .await
            .unwrap()
            .unwrap();
        sqlx::query(
            "UPDATE social.island_takeover_receipts SET latest_dispatch=$2 WHERE wallet=$1",
        )
        .bind(&wallet)
        .bind(next.dispatch_id)
        .execute(&mut *lease.tx)
        .await
        .unwrap();
        lease.commit().await.unwrap();
        work.await.unwrap().unwrap();
        other_pool.close().await;
        assert_eq!(sqlx::query_as::<_,(Option<String>,Uuid)>("SELECT retired_by_source,latest_dispatch FROM social.island_takeover_receipts WHERE wallet=$1").bind(&wallet).fetch_one(&store.pool).await.unwrap(),(None,next.dispatch_id));
        assert!(store.snapshot(&wallet).await.is_err());
    }

    #[tokio::test]
    async fn durable_confirmed_retirement_frees_exactly_one_existing_capacity_slot() {
        let Some((store, admin, name)) = isolated().await else {
            return;
        };
        crate::db::apply_schema(&store.pool).await.unwrap();
        sqlx::query("INSERT INTO social.island_admission_fences(wallet) SELECT 'retirement-'||n FROM generate_series(1,$1) n").bind(CAPACITY).execute(&store.pool).await.unwrap();
        sqlx::query("INSERT INTO social.island_takeover_receipts(wallet,source_incarnation,takeover_id,predecessor_session,predecessor_cluster,predecessor_realm,last_successor_session) SELECT wallet,'source-a','one','old-session','old-room','realm','next' FROM social.island_admission_fences").execute(&store.pool).await.unwrap();
        let op = takeover("one");
        assert!(
            store
                .prepare_takeover("extra", "source-b", "next", &op)
                .await
                .is_err()
        );
        let d = store
            .begin_takeover("retirement-1", "source-a", &op, 0)
            .await
            .unwrap()
            .unwrap();
        store
            .settle(d.dispatch_id, Settlement::Confirmed)
            .await
            .unwrap();
        assert!(
            store
                .prepare_takeover("extra", "source-b", "next", &op)
                .await
                .is_err(),
            "confirmed alone consumes original obligation"
        );
        store
            .acknowledge("retirement-1", "source-b", &[])
            .await
            .unwrap();
        store
            .prepare_takeover("extra", "source-b", "next", &op)
            .await
            .unwrap();
        assert!(
            store
                .prepare_takeover("second-extra", "source-b", "next", &op)
                .await
                .is_err()
        );
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM social.island_takeover_receipts WHERE NOT acknowledged AND retired_by_source IS NULL").fetch_one(&store.pool).await.unwrap(),CAPACITY);
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT token_floor FROM social.island_admission_fences WHERE wallet='retirement-1'"
            )
            .fetch_one(&store.pool)
            .await
            .unwrap(),
            d.cutoff as i64
        );
        teardown(store, admin, name).await;
    }

    #[tokio::test]
    async fn durable_receipt_retirement_additive_upgrade_preserves_authority_and_matches_fresh() {
        let Some((store, admin, name)) = isolated().await else {
            return;
        };
        let old=crate::db::SCHEMA_SQL.replace("\r\n","\n").replace("    -- Local confirmed-obligation disposal; never a Pulse acknowledgment.\n    retired_by_source text,\n","")
            .replace(",\n    CONSTRAINT island_takeover_receipts_retirement_check CHECK (retired_by_source IS NULL OR\n        (retired_by_source <> '' AND retired_by_source <> source_incarnation AND NOT acknowledged AND latest_dispatch IS NOT NULL))","");
        sqlx::raw_sql(sqlx::AssertSqlSafe(old.as_str()))
            .execute(&store.pool)
            .await
            .unwrap();
        let mut steps = crate::db::schema_migrate::parse_steps(crate::db::MIGRATIONS_SQL);
        steps.retain(|s| !s.name.starts_with("0013_"));
        crate::db::schema_migrate::record_steps(&store.pool, &steps, true)
            .await
            .unwrap();
        sqlx::query("INSERT INTO social.island_admission_fences(wallet,generation,token_floor) VALUES ('preserved',7,100)").execute(&store.pool).await.unwrap();
        sqlx::query("INSERT INTO social.island_takeover_receipts(wallet,source_incarnation,takeover_id,predecessor_session,predecessor_cluster,predecessor_realm,last_successor_session) VALUES ('preserved','source','pending','old','room','realm','next')").execute(&store.pool).await.unwrap();
        crate::db::schema_migrate::migrate(
            &store.pool,
            crate::db::SCHEMA_SQL,
            crate::db::MIGRATIONS_SQL,
        )
        .await
        .unwrap();
        let first_step: (String,bool,chrono::NaiveDateTime)=sqlx::query_as("SELECT sha256,created_fresh,applied_at FROM meta.schema_steps WHERE step='0013_island_receipt_retirement'").fetch_one(&store.pool).await.unwrap();
        assert!(
            !first_step.1,
            "existing schema applies the step, not a fresh stamp"
        );
        crate::db::schema_migrate::migrate(
            &store.pool,
            crate::db::SCHEMA_SQL,
            crate::db::MIGRATIONS_SQL,
        )
        .await
        .unwrap();
        assert_eq!(sqlx::query_as::<_,(String,bool,chrono::NaiveDateTime)>("SELECT sha256,created_fresh,applied_at FROM meta.schema_steps WHERE step='0013_island_receipt_retirement'").fetch_one(&store.pool).await.unwrap(),first_step,"repeat preserves the exact original migration record");
        assert_eq!(sqlx::query_as::<_,(i64,i64,Option<String>)>("SELECT f.generation,f.token_floor,r.retired_by_source FROM social.island_admission_fences f JOIN social.island_takeover_receipts r USING(wallet) WHERE f.wallet='preserved'").fetch_one(&store.pool).await.unwrap(),(7,100,None));
        let Some((fresh, fa, fnm)) = isolated().await else {
            panic!("same fixture prerequisite");
        };
        crate::db::apply_schema(&fresh.pool).await.unwrap();
        let shape = "SELECT 'constraint:'||pg_get_constraintdef(k.oid) FROM pg_constraint k WHERE k.conrelid='social.island_takeover_receipts'::regclass UNION ALL SELECT column_name||':'||data_type||':'||is_nullable||':'||COALESCE(column_default,'') FROM information_schema.columns WHERE table_schema='social' AND table_name='island_takeover_receipts' ORDER BY 1";
        assert_eq!(
            sqlx::query_scalar::<_, String>(shape)
                .fetch_all(&store.pool)
                .await
                .unwrap(),
            sqlx::query_scalar::<_, String>(shape)
                .fetch_all(&fresh.pool)
                .await
                .unwrap()
        );
        teardown(fresh, fa, fnm).await;
        teardown(store, admin, name).await;
    }
    #[tokio::test]
    async fn durable_retired_origin_cannot_authorize_replica_or_queued_stamp_while_evidence_exists()
    {
        let Some((store, wallet)) = fixture().await else {
            return;
        };
        let op = takeover("one");
        // Stamp needs protocol-valid wallet/session shape, unlike internal store keys.
        let wallet = format!("0x00000000{}", Uuid::parse_str(&wallet).unwrap().simple());
        store
            .prepare_takeover(&wallet, "source-a", "next", &op)
            .await
            .unwrap();
        let d = store
            .begin_takeover(&wallet, "source-a", &op, 0)
            .await
            .unwrap()
            .unwrap();
        store
            .settle(d.dispatch_id, Settlement::Confirmed)
            .await
            .unwrap();
        let safety = store.snapshot(&wallet).await.unwrap();
        let snapshot = crate::protocol::pulse::decentraland::pulse::PeerClusterSnapshot {
            selected_origin: 1,
            selected_identity: Some(1),
            assignment_state: 1,
            wallet: wallet.clone(),
            session: "0x1111111111111111111111111111111111111111".into(),
            cluster_id: "current".into(),
            realm: "realm".into(),
            source_incarnation: "source-a".into(),
            assignment_version: 1,
            pass: 1,
            token_not_before: Some(0),
            pending_takeovers: vec![],
        };
        let mut stamp =
            super::super::fence::PulseAdmissionStamp::from_snapshot(&snapshot, d.cutoff).unwrap();
        stamp.safety_generation = safety.generation;
        stamp.safety_token_floor = safety.token_floor;
        store
            .authorize_stamp(&stamp, None, None, false)
            .await
            .unwrap()
            .commit()
            .await
            .unwrap();
        store.acknowledge(&wallet, "source-b", &[]).await.unwrap();
        assert!(
            store
                .authorize_stamp(&stamp, None, None, false)
                .await
                .is_err(),
            "all shared stamp consumers must reject observed retired origin"
        );
        stamp.source_incarnation = "source-b".into();
        store
            .authorize_stamp(&stamp, None, None, false)
            .await
            .unwrap()
            .commit()
            .await
            .unwrap();
        // Disposal is still bounded by both normal ages, and leaves floor forever.
        store.compact_settled().await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM social.island_takeover_receipts WHERE wallet=$1"
            )
            .bind(&wallet)
            .fetch_one(&store.pool)
            .await
            .unwrap(),
            1
        );
        sqlx::query("UPDATE social.island_cleanup_dispatches SET cutoff=100,settled_at=timestamp '1970-01-01' WHERE dispatch_id=$1").bind(d.dispatch_id).execute(&store.pool).await.unwrap();
        store.compact_settled().await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM social.island_takeover_receipts WHERE wallet=$1"
            )
            .bind(&wallet)
            .fetch_one(&store.pool)
            .await
            .unwrap(),
            0
        );
        assert_eq!(store.snapshot(&wallet).await.unwrap(), safety);
        assert!(
            store
                .authorize(&wallet, None, safety.token_floor - 1)
                .await
                .is_err()
        );
    }
}
