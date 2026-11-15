//! The platform ban: an account-level ban of the local user's wallet from all of Decentraland,
//! read from the comms gatekeeper's public `GET /users/{address}/bans`. The one fact this crate
//! owns rather than reads from another crate.

use alloy_core::primitives::Address;
use bevy::{
    app::{App, Plugin, PostUpdate, Update},
    ecs::{
        change_detection::DetectChanges,
        event::{Event, EventReader, EventWriter},
        resource::Resource,
        schedule::{common_conditions::resource_changed, IntoScheduleConfigs},
        system::{Local, Res, ResMut},
    },
    log::{info, warn},
    tasks::{IoTaskPool, Task},
    time::Time,
};
use bevy_console::ConsoleCommand;
use common::{
    base_domain::{self, Service},
    structs::CurrentRealm,
    util::{TaskCompat, TaskExt},
};
use console::DoAddConsoleCommand;
use serde::{Deserialize, Serialize};
use wallet::Wallet;

use crate::{AdmissionView, BanScope, SceneAdmission};

/// Seconds between two checks of a wallet that has not changed.
pub const RECHECK_SECS: f64 = 300.0;
/// The least a server's `Retry-After` is honoured as.
pub const MIN_RETRY_SECS: f64 = 5.0;
const REQUEST_TIMEOUT_SECS: u64 = 10;

#[derive(Resource)]
pub struct PlatformBanStatus {
    state: PlatformBanState,
    source: Option<PlatformBanSource>,
    http: reqwest::Client,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PlatformBanState {
    Unknown,
    Checked {
        ban: Option<PlatformBanInfo>,
        checked_at: f64,
    },
    Failed {
        error: String,
        checked_at: f64,
    },
}

/// The gatekeeper's `ban` object, as served; dates stay ISO strings.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformBanInfo {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub custom_message: Option<String>,
    #[serde(default)]
    pub banned_at: String,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub banned_by: String,
    /// What a withheld record matched (`device`); `None` for a record as served.
    #[serde(default)]
    pub matched_on: Option<String>,
}

/// How the platform ban became known.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformBanSource {
    BanStatusEndpoint,
    SceneAdmissionRefusal,
}

/// `GET /users/{address}/bans`: `{"data":{"isBanned":bool,"ban":{..}?,"matchedOn":"device"?}}`.
#[derive(Deserialize)]
struct BanStatusEnvelope {
    data: BanStatusBody,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BanStatusBody {
    is_banned: bool,
    #[serde(default)]
    ban: Option<PlatformBanInfo>,
    #[serde(default)]
    matched_on: Option<String>,
}

impl PlatformBanInfo {
    /// Decodes the ban-status body: `Ok(None)` when not banned, whatever else it carries; a ban
    /// without its record is a ban whose record was withheld.
    #[cfg(test)]
    pub fn from_json(body: &[u8]) -> Result<Option<Self>, String> {
        let envelope: BanStatusEnvelope =
            serde_json::from_slice(body).map_err(|e| format!("bad ban status body: {e}"))?;
        Ok(Self::from_body(envelope.data))
    }

    fn from_body(body: BanStatusBody) -> Option<Self> {
        if !body.is_banned {
            return None;
        }
        Some(body.ban.unwrap_or_else(|| {
            Self::withheld(body.matched_on.as_deref().unwrap_or("another match"))
        }))
    }

    /// A ban with no record; `matched_on` is what it matched.
    pub fn withheld(matched_on: &str) -> Self {
        Self {
            id: String::new(),
            reason: String::new(),
            custom_message: None,
            banned_at: String::new(),
            expires_at: None,
            banned_by: String::new(),
            matched_on: Some(matched_on.to_owned()),
        }
    }

    /// A ban known only from a scene-admission refusal: the message is all there is.
    pub fn from_refusal(message: &str) -> Self {
        Self {
            id: String::new(),
            reason: message.to_owned(),
            custom_message: None,
            banned_at: String::new(),
            expires_at: None,
            banned_by: String::new(),
            matched_on: None,
        }
    }
}

impl PlatformBanStatus {
    pub fn new() -> Self {
        Self {
            state: PlatformBanState::Unknown,
            source: None,
            http: platform::http_client_builder()
                .build()
                .expect("reqwest client"),
        }
    }

    pub fn state(&self) -> &PlatformBanState {
        &self.state
    }

    /// `Some` only while a ban is known.
    pub fn source(&self) -> Option<PlatformBanSource> {
        match &self.state {
            PlatformBanState::Checked { ban: Some(_), .. } => self.source,
            _ => None,
        }
    }

    /// `Some` only for a checked ban; unknown and failed are `None`.
    pub fn ban(&self) -> Option<&PlatformBanInfo> {
        match &self.state {
            PlatformBanState::Checked { ban, .. } => ban.as_ref(),
            _ => None,
        }
    }

    /// Records an answer; the same answer again only moves `checked_at`.
    pub fn apply_platform_ban(
        &mut self,
        ban: Option<PlatformBanInfo>,
        source: PlatformBanSource,
        checked_at: f64,
    ) {
        self.source = ban.is_some().then_some(source);
        self.state = PlatformBanState::Checked { ban, checked_at };
    }

    /// Records a failed check. A ban learnt from a scene-admission refusal is kept: a failed poll
    /// is no evidence against it.
    pub fn platform_ban_check_failed(&mut self, error: String, checked_at: f64) {
        if self.source() == Some(PlatformBanSource::SceneAdmissionRefusal) {
            warn!("[ban_block] platform ban check failed, keeping the refusal: {error}");
            return;
        }
        self.state = PlatformBanState::Failed { error, checked_at };
        self.source = None;
    }

    /// Forgets the answer.
    pub fn reset_platform_ban(&mut self) {
        self.state = PlatformBanState::Unknown;
        self.source = None;
    }
}

/// True while the known ban came from a scene-admission refusal and the admission still shows
/// that refusal.
fn refusal_outranks_not_banned(
    source: Option<PlatformBanSource>,
    admission: &SceneAdmission,
) -> bool {
    source == Some(PlatformBanSource::SceneAdmissionRefusal)
        && matches!(
            admission.view(),
            AdmissionView::Refused {
                scope: BanScope::Platform,
                ..
            }
        )
}

/// Records a platform-scoped admission refusal as the platform ban while none is known.
fn cross_feed_admission_refusal(
    admission: Res<SceneAdmission>,
    mut status: ResMut<PlatformBanStatus>,
    time: Res<Time>,
) {
    let AdmissionView::Refused {
        scope: BanScope::Platform,
        message,
        ..
    } = admission.view()
    else {
        return;
    };
    if status.ban().is_some() {
        return;
    }
    status.apply_platform_ban(
        Some(PlatformBanInfo::from_refusal(message)),
        PlatformBanSource::SceneAdmissionRefusal,
        time.elapsed_secs_f64(),
    );
}

impl Default for PlatformBanStatus {
    fn default() -> Self {
        Self::new()
    }
}

/// `/ban_block_refresh` asks for the platform ban to be checked again now.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/ban_block_refresh")]
struct BanBlockRefreshCommand;

/// A check was asked for.
#[derive(Event)]
struct PlatformBanRefreshRequested;

fn ban_block_refresh(
    mut input: ConsoleCommand<BanBlockRefreshCommand>,
    wallet: Res<Wallet>,
    mut refresh: EventWriter<PlatformBanRefreshRequested>,
) {
    if let Some(Ok(_)) = input.take() {
        if wallet.address().is_none() {
            input.reply_failed("not signed in");
            return;
        }
        refresh.write(PlatformBanRefreshRequested);
        input.reply_ok("platform ban check requested");
    }
}

pub(crate) struct PlatformBanPlugin;

impl Plugin for PlatformBanPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(PlatformBanStatus::new())
            .add_event::<PlatformBanRefreshRequested>()
            .add_console_command::<BanBlockRefreshCommand, _>(ban_block_refresh)
            .add_systems(
                Update,
                check_platform_ban.run_if(|| !common::structs::server_mode()),
            )
            .add_systems(
                PostUpdate,
                cross_feed_admission_refusal.run_if(resource_changed::<SceneAdmission>),
            );
    }
}

/// Why a check gave no answer, with the delay the server asked for before the next one.
#[derive(Debug, PartialEq)]
pub struct CheckFailure {
    pub error: String,
    pub retry_after_secs: Option<f64>,
}

/// One ban-status request on its way, for the address it was sent for.
struct InFlightBanCheck {
    address: Address,
    task: Task<Result<Option<PlatformBanInfo>, CheckFailure>>,
}

/// When the platform ban was last checked and when it may be checked next, for one address.
#[derive(Default)]
struct BanCheckSchedule {
    address: Option<Address>,
    last_checked: Option<f64>,
    retry_after: Option<f64>,
    /// A trigger that arrived while a request was in flight.
    retrigger: bool,
    in_flight: Option<InFlightBanCheck>,
}

impl BanCheckSchedule {
    /// Takes the schedule to `address`, dropping everything about the previous one. True when
    /// the address changed.
    fn follow(&mut self, address: Option<Address>) -> bool {
        if self.address == address {
            return false;
        }
        *self = Self {
            address,
            ..Self::default()
        };
        true
    }

    /// Whether a check starts now: a trigger, or the period or the server's delay elapsed. A
    /// kept trigger waits out the server's delay.
    fn starts(&mut self, now: f64, triggered: bool) -> bool {
        let kept = self.retry_after.is_none() && self.retrigger;
        let start = triggered || kept || due(now, self.last_checked, self.retry_after);
        if start {
            self.last_checked = Some(now);
            self.retry_after = None;
            self.retrigger = false;
        }
        start
    }

    /// Records a failed check's delay, floored at `MIN_RETRY_SECS`.
    fn delay(&mut self, retry_after_secs: Option<f64>) {
        self.retry_after = retry_after_secs.map(|secs| secs.max(MIN_RETRY_SECS));
    }
}

/// Whether a check is due: never checked, the server's retry delay passed, or the recheck period
/// passed.
pub fn due(now: f64, last_checked: Option<f64>, retry_after: Option<f64>) -> bool {
    match (last_checked, retry_after) {
        (None, _) => true,
        (Some(last), Some(wait)) => now - last >= wait,
        (Some(last), None) => now - last >= RECHECK_SECS,
    }
}

/// Polls the one request in flight, dropping an answer for an address that is no longer the
/// wallet's, and starts the next one when a check is due. A trigger that lands while a request
/// is in flight starts another as soon as it completes.
fn check_platform_ban(
    mut status: ResMut<PlatformBanStatus>,
    wallet: Res<Wallet>,
    realm: Option<Res<CurrentRealm>>,
    admission: Res<SceneAdmission>,
    time: Res<Time>,
    mut refresh: EventReader<PlatformBanRefreshRequested>,
    mut schedule: Local<BanCheckSchedule>,
) {
    let now = time.elapsed_secs_f64();
    if schedule.follow(wallet.address()) {
        status.reset_platform_ban();
    }
    let Some(address) = wallet.address() else {
        return;
    };
    let refresh_requested = refresh.read().count() > 0;
    let triggered =
        refresh_requested || realm.is_some_and(|realm| realm.is_changed() && realm.connected);

    if let Some(in_flight) = schedule.in_flight.as_mut() {
        let Some(result) = in_flight.task.complete() else {
            schedule.retrigger |= triggered;
            return;
        };
        let for_address = in_flight.address;
        schedule.in_flight = None;
        if for_address == address {
            match result {
                Ok(None) if refusal_outranks_not_banned(status.source(), &admission) => {
                    info!("[ban_block] platform ban check: not banned, keeping the refusal");
                }
                Ok(ban) => {
                    info!(
                        "[ban_block] platform ban check: {}",
                        if ban.is_some() {
                            "banned"
                        } else {
                            "not banned"
                        }
                    );
                    status.apply_platform_ban(ban, PlatformBanSource::BanStatusEndpoint, now);
                }
                Err(failure) => {
                    schedule.delay(failure.retry_after_secs);
                    warn!("[ban_block] platform ban check failed: {}", failure.error);
                    status.platform_ban_check_failed(failure.error, now);
                }
            }
        }
    }

    if !schedule.starts(now, triggered) {
        return;
    }
    let url = base_domain::url(
        Service::CommsGatekeeper,
        &format!("/users/{address:#x}/bans"),
    );
    let request = status
        .http
        .get(url)
        .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS));
    schedule.in_flight = Some(InFlightBanCheck {
        address,
        task: IoTaskPool::get().spawn_compat(fetch_ban_status(request)),
    });
}

/// The decoded answer, or the failure with the server's `Retry-After` in seconds when it sent
/// one.
async fn fetch_ban_status(
    request: reqwest::RequestBuilder,
) -> Result<Option<PlatformBanInfo>, CheckFailure> {
    let failed = |error: String| CheckFailure {
        error,
        retry_after_secs: None,
    };
    let response = platform::TracedJsonResponse::send(request)
        .await
        .map_err(|e| failed(format!("request failed: {e}")))?;
    let status = response.status();
    if status == reqwest::StatusCode::OK {
        return response
            .json::<BanStatusEnvelope>()
            .await
            .map(|envelope| PlatformBanInfo::from_body(envelope.data))
            .map_err(|e| failed(format!("bad ban status body: {e}")));
    }
    let retry_after_secs = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(retry_after_secs);
    response.discard();
    Err(CheckFailure {
        error: format!("status {status}"),
        retry_after_secs,
    })
}

/// A `Retry-After` value as seconds from now, capped at the platform's longest honoured delay.
fn retry_after_secs(value: &str) -> Option<f64> {
    let since_epoch = web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .ok()?;
    platform::http::parse_retry_after(value, std::time::UNIX_EPOCH + since_epoch)
        .map(|wait| wait.min(platform::http::MAX_RETRY_AFTER).as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(n: u8) -> Address {
        Address::from_slice(&[n; 20])
    }

    #[test]
    fn the_ban_status_body_decodes_as_served() {
        let banned = br#"{"data":{"isBanned":true,"ban":{"id":"b1","bannedAddress":"0xab","bannedBy":"0xcd","reason":"spam","customMessage":"go away","bannedAt":"2026-01-01T00:00:00.000Z","expiresAt":null,"liftedAt":null,"liftedBy":null,"createdAt":"2026-01-01T00:00:00.000Z"}}}"#;
        let ban = PlatformBanInfo::from_json(banned).unwrap().unwrap();
        assert_eq!(ban.reason, "spam");
        assert_eq!(ban.matched_on, None);
        assert_eq!(ban.custom_message.as_deref(), Some("go away"));
        assert_eq!(ban.expires_at, None);
        assert_eq!(
            PlatformBanInfo::from_json(br#"{"data":{"isBanned":false}}"#).unwrap(),
            None
        );
        assert_eq!(
            PlatformBanInfo::from_json(br#"{"data":{"isBanned":false,"ban":{"reason":"stale"}}}"#)
                .unwrap(),
            None
        );
        assert!(PlatformBanInfo::from_json(b"not json").is_err());
    }

    #[test]
    fn a_device_matched_ban_is_a_ban_with_its_record_withheld() {
        let withheld =
            PlatformBanInfo::from_json(br#"{"data":{"isBanned":true,"matchedOn":"device"}}"#)
                .unwrap()
                .unwrap();
        assert_eq!(withheld, PlatformBanInfo::withheld("device"));
        assert_eq!(withheld.matched_on.as_deref(), Some("device"));
        assert_eq!(withheld.reason, "");
        let unexplained = PlatformBanInfo::from_json(br#"{"data":{"isBanned":true}}"#)
            .unwrap()
            .unwrap();
        assert_eq!(unexplained, PlatformBanInfo::withheld("another match"));
    }

    const PLATFORM_BANNED: &str = "Access denied, platform-banned user";

    #[test]
    fn an_answer_is_recorded_with_its_source() {
        let mut status = PlatformBanStatus::new();
        assert_eq!(status.ban(), None);
        status.apply_platform_ban(None, PlatformBanSource::BanStatusEndpoint, 1.0);
        assert_eq!(status.ban(), None);
        assert_eq!(status.source(), None);
        assert!(matches!(
            status.state(),
            PlatformBanState::Checked { ban: None, .. }
        ));
        let ban = PlatformBanInfo::from_refusal(PLATFORM_BANNED);
        status.apply_platform_ban(
            Some(ban.clone()),
            PlatformBanSource::SceneAdmissionRefusal,
            3.0,
        );
        assert_eq!(status.ban(), Some(&ban));
        assert_eq!(
            status.source(),
            Some(PlatformBanSource::SceneAdmissionRefusal)
        );
        status.reset_platform_ban();
        assert_eq!(status.state(), &PlatformBanState::Unknown);
        assert_eq!(status.source(), None);
    }

    #[test]
    fn a_failed_check_fails_open_but_keeps_a_refusal() {
        let mut status = PlatformBanStatus::new();
        status.platform_ban_check_failed("status 500".into(), 1.0);
        assert_eq!(status.ban(), None);
        assert!(matches!(status.state(), PlatformBanState::Failed { .. }));

        let ban = PlatformBanInfo::from_refusal("banned");
        status.apply_platform_ban(
            Some(ban.clone()),
            PlatformBanSource::SceneAdmissionRefusal,
            3.0,
        );
        status.platform_ban_check_failed("status 500".into(), 4.0);
        assert_eq!(status.ban(), Some(&ban));
    }

    #[test]
    fn a_not_banned_is_dropped_only_while_the_refusal_is_on_screen() {
        let refusal = SceneAdmission::from_refusal(403, Some(PLATFORM_BANNED), false, "s");
        let scene_ban =
            SceneAdmission::from_refusal(403, Some("User is banned from this scene"), false, "s");
        let from_refusal = Some(PlatformBanSource::SceneAdmissionRefusal);
        let from_endpoint = Some(PlatformBanSource::BanStatusEndpoint);
        assert!(refusal_outranks_not_banned(from_refusal, &refusal));
        assert!(!refusal_outranks_not_banned(
            from_refusal,
            &SceneAdmission::unknown()
        ));
        assert!(!refusal_outranks_not_banned(
            from_refusal,
            &SceneAdmission::allowed("s")
        ));
        assert!(!refusal_outranks_not_banned(from_refusal, &scene_ban));
        assert!(!refusal_outranks_not_banned(from_endpoint, &refusal));
        assert!(!refusal_outranks_not_banned(None, &refusal));
    }

    /// An app with the admission, the status and the clock, running the cross-feed.
    fn cross_feed_app() -> bevy::app::App {
        let mut app = bevy::app::App::new();
        app.init_resource::<Time>()
            .init_resource::<SceneAdmission>()
            .insert_resource(PlatformBanStatus::new())
            .add_systems(
                Update,
                cross_feed_admission_refusal.run_if(resource_changed::<SceneAdmission>),
            );
        app
    }

    fn set_admission(app: &mut bevy::app::App, admission: SceneAdmission) {
        use bevy::ecs::change_detection::DetectChangesMut;
        app.world_mut()
            .resource_mut::<SceneAdmission>()
            .set_if_neq(admission);
        app.update();
    }

    #[test]
    fn a_platform_refusal_marks_the_ban_while_none_is_known() {
        let mut app = cross_feed_app();
        app.update();
        assert_eq!(
            app.world().resource::<PlatformBanStatus>().state(),
            &PlatformBanState::Unknown
        );
        set_admission(
            &mut app,
            SceneAdmission::from_refusal(403, Some(PLATFORM_BANNED), false, "s"),
        );
        let status = app.world().resource::<PlatformBanStatus>();
        assert_eq!(
            status.ban().map(|b| b.reason.as_str()),
            Some(PLATFORM_BANNED)
        );
        assert_eq!(
            status.source(),
            Some(PlatformBanSource::SceneAdmissionRefusal)
        );
        // a scene change makes the admission unknown: the ban stands
        set_admission(&mut app, SceneAdmission::unknown());
        assert!(app.world().resource::<PlatformBanStatus>().ban().is_some());
        // a scene ban is not a platform ban
        let mut app = cross_feed_app();
        set_admission(
            &mut app,
            SceneAdmission::from_refusal(403, Some("User is banned from this scene"), false, "s"),
        );
        assert_eq!(
            app.world().resource::<PlatformBanStatus>().state(),
            &PlatformBanState::Unknown
        );
    }

    #[test]
    fn a_ban_the_endpoint_reported_is_not_replaced_by_a_refusal() {
        let mut app = cross_feed_app();
        let record = PlatformBanInfo {
            reason: "spam".into(),
            ..PlatformBanInfo::from_refusal("")
        };
        app.world_mut()
            .resource_mut::<PlatformBanStatus>()
            .apply_platform_ban(
                Some(record.clone()),
                PlatformBanSource::BanStatusEndpoint,
                1.0,
            );
        set_admission(
            &mut app,
            SceneAdmission::from_refusal(403, Some(PLATFORM_BANNED), false, "s"),
        );
        let status = app.world().resource::<PlatformBanStatus>();
        assert_eq!(status.ban(), Some(&record));
        assert_eq!(status.source(), Some(PlatformBanSource::BanStatusEndpoint));
    }

    #[test]
    fn a_check_is_due_at_first_then_on_the_period_or_the_retry_delay() {
        assert!(due(0.0, None, None));
        assert!(!due(10.0, Some(0.0), None));
        assert!(due(RECHECK_SECS, Some(0.0), None));
        assert!(!due(4.0, Some(0.0), Some(5.0)));
        assert!(due(5.0, Some(0.0), Some(5.0)));
        assert_eq!(retry_after_secs("7"), Some(7.0));
        assert_eq!(retry_after_secs("3600"), Some(60.0));
        assert_eq!(retry_after_secs("soon"), None);
    }

    #[test]
    fn the_schedule_follows_the_wallet_and_honours_triggers_and_delays() {
        let mut schedule = BanCheckSchedule::default();
        assert!(schedule.follow(Some(address(1))));
        assert!(
            schedule.starts(0.0, false),
            "a new address is checked at once"
        );
        assert!(!schedule.starts(1.0, false));
        assert!(schedule.starts(1.0, true), "a trigger starts a check");

        // a trigger during a request is kept for when the request completes
        schedule.retrigger = true;
        assert!(schedule.starts(2.0, false));
        assert!(!schedule.retrigger);
        assert!(!schedule.starts(3.0, false));

        // a server delay is floored and then honoured, by a kept trigger too
        schedule.delay(Some(0.0));
        schedule.retrigger = true;
        assert_eq!(schedule.retry_after, Some(MIN_RETRY_SECS));
        assert!(!schedule.starts(2.0 + MIN_RETRY_SECS - 1.0, false));
        assert!(schedule.retrigger);
        assert!(schedule.starts(2.0 + MIN_RETRY_SECS, false));
        assert!(!schedule.retrigger);

        // the same address is no change; another drops the schedule
        assert!(!schedule.follow(Some(address(1))));
        assert!(schedule.last_checked.is_some());
        assert!(schedule.follow(Some(address(2))));
        assert_eq!(schedule.last_checked, None);
        assert!(schedule.follow(None));
        assert_eq!(schedule.address, None);
    }
}
