// from: credits-server@d9c6a261fb7acf3beb926e10f03d1aed33e9771b:src/logic/goals-handlers/take-photo-handler.ts, src/logic/credits-granter.ts
//! Seasonal MANA goals. Photo facts, receipts, progress, signatures and inbox
//! delivery records share the existing database; paid USD balances are untouched.
pub mod flags;
mod reads;
#[cfg(test)]
pub(crate) mod tests;

use super::{
    SharedState,
    digest::{CreditIdentity, TreasuryWallet},
    errors::ApiError,
};
use crate::jobs::{Jobs, PeriodicCfg};
use alloy_primitives::{Address, B256, U256, keccak256};
use chrono::{Datelike, NaiveDateTime, TimeDelta, Timelike};
use serde_json::{Value, json};
use sqlx::{PgConnection, Row};
use std::{str::FromStr, sync::Arc, time::Duration};
use uuid::Uuid;

pub use reads::{
    claim, claim_preflight, enroll, flag_reason, opt_out, progress, season_document, season_status,
    status, user_credits,
};
const GOAL: &str = "take-photo";
const WEEKLY_CAP: &str = "40000000000000000000000";
const NOTIFICATION_NAMESPACE: Uuid = Uuid::from_u128(0x6b70ef60_e939_4c58_82bf_9c0eb8b58552);

#[derive(Debug, sqlx::FromRow)]
struct Season {
    id: i32,
    name: String,
    start_date: NaiveDateTime,
    end_date: NaiveDateTime,
    max_mana_wei: String,
    grace_days: i32,
}
impl Season {
    fn week(&self, now: NaiveDateTime) -> (i32, NaiveDateTime, NaiveDateTime) {
        fn monday(t: NaiveDateTime) -> NaiveDateTime {
            t.date().and_hms_opt(0, 0, 0).unwrap()
                - TimeDelta::days(i64::from(t.weekday().num_days_from_monday()))
        }
        let start = monday(now);
        let number = ((start - monday(self.start_date)).num_days() / 7 + 1) as i32;
        (
            number,
            start.max(self.start_date),
            (start + TimeDelta::days(7)).min(self.end_date),
        )
    }
}
async fn current(conn: &mut PgConnection, now: NaiveDateTime) -> Result<Option<Season>, ApiError> {
    Ok(sqlx::query_as("SELECT id,name,start_date,end_date,max_mana_wei::text,grace_days FROM credits.seasons WHERE start_date <= $1 AND end_date >= $1 ORDER BY start_date DESC LIMIT 1")
        .bind(now).fetch_optional(conn).await?)
}
pub(super) async fn wallet_lock(conn: &mut PgConnection, address: &str) -> Result<(), ApiError> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('credits-goals:' || $1, 0))")
        .bind(address)
        .execute(conn)
        .await?;
    Ok(())
}
fn uint(value: &str) -> Result<U256, ApiError> {
    U256::from_str(value).map_err(|_| ApiError::Internal("invalid stored MANA wei".into()))
}
/// Floating point is used only at the source-compatible JSON presentation boundary.
fn wire_number(wei: &str) -> Result<f64, ApiError> {
    uint(wei)?;
    wei.parse()
        .map_err(|_| ApiError::Internal("invalid MANA presentation".into()))
}
fn mana_number(wei: &str) -> Result<f64, ApiError> {
    Ok(wire_number(wei)? / 1e18)
}

/// Reads Credits' own contextual snapshots and node overrides. Missing documents
/// are retryable, distinct from an observed disabled program. No bundled defaults
/// may turn an unavailable monetary policy into a terminal skip.
async fn policy(
    state: &SharedState,
    conn: &mut PgConnection,
    address: &str,
) -> Result<Option<&'static str>, ApiError> {
    let [mut dapps, mut explorer] = state.goal_flags.documents();
    let overrides = sqlx::query("SELECT app,name,value FROM explore_api.feature_flags_local WHERE app IN ('dapps','explorer')")
        .fetch_all(&mut *conn).await?;
    for row in overrides {
        let app: String = row.get("app");
        let name: String = row.get("name");
        if (app == "dapps" && name != "dapps-credits-server")
            || (app == "explorer" && name != "explorer-alfa-marketplace-credits")
        {
            continue;
        }
        let value = row.get::<bool, _>("value");
        // Enabling an unavailable Explorer policy supplies no evidence of its
        // allowlist. A false override explicitly disables that gate; a genuine
        // fetched document with an empty payload retains the source's open gate.
        if app == "explorer" && value && explorer.is_none() {
            continue;
        }
        let target = if app == "dapps" {
            &mut dapps
        } else {
            &mut explorer
        };
        let doc = target.get_or_insert_with(|| json!({"flags": {}, "variants": {}}));
        doc["flags"][name] = json!(value);
    }
    let dapps =
        dapps.ok_or_else(|| ApiError::service_unavailable("credits dapps flags unavailable"))?;
    if dapps["flags"]["dapps-credits-server"].as_bool() != Some(true) {
        return Ok(Some("disabled"));
    }
    let explorer = explorer
        .ok_or_else(|| ApiError::service_unavailable("credits explorer flags unavailable"))?;
    let snapshot = crate::explore::api::feature_flags::Snapshot::from_document(explorer);
    if snapshot
        .variant_values("explorer", "alfa-marketplace-credits")
        .is_some_and(|list| {
            !list
                .iter()
                .filter(|a| Address::from_str(a).is_ok())
                .any(|a| a == address)
        })
    {
        return Ok(Some("not-allowed"));
    }
    Ok(None)
}

pub fn qualifies(payload: &Value, address: &str) -> bool {
    let Some(users) = payload["metadata"]["users"].as_array() else {
        return false;
    };
    let emoting = |u: &&Value| u["isEmoting"].as_bool() == Some(true);
    users.iter().filter(emoting).any(|u| {
        u["address"]
            .as_str()
            .is_some_and(|a| a.eq_ignore_ascii_case(address))
    }) && users.iter().filter(emoting).any(|u| {
        u["address"]
            .as_str()
            .is_some_and(|a| !a.eq_ignore_ascii_case(address))
    })
}
async fn initialize(
    conn: &mut PgConnection,
    address: &str,
    season: i32,
    week: i32,
) -> Result<(), ApiError> {
    sqlx::query("INSERT INTO credits.goal_progress(address,season_id,week_id,goal_id,title,description,thumbnail,total_steps,reward_wei) SELECT $1,season_id,$3,goal_id,title,description,thumbnail,total_steps,reward_wei FROM credits.season_goals WHERE season_id=$2 AND $3 BETWEEN start_week AND end_week AND EXISTS(SELECT 1 FROM credits.wallets WHERE address=$1) ON CONFLICT DO NOTHING")
        .bind(address).bind(season).bind(week).execute(conn).await?;
    Ok(())
}

/// Receipts alone acknowledge events. Each bounded scan round captures a fixed
/// upper id and advances monotonically; periodic reconciliation finds events
/// whose sequence id committed late without rescanning the retention window on
/// every worker tick.
pub async fn process_batch(state: &SharedState, limit: i64) -> Result<u64, ApiError> {
    use std::sync::atomic::Ordering;
    let limit = limit.clamp(1, 500);
    let mut after = state.goal_scan_after.load(Ordering::Relaxed);
    let mut horizon = state.goal_scan_horizon.load(Ordering::Relaxed);
    const RECONCILE_INTERVAL_MS: i64 = 5 * 60 * 1000;
    let current_horizon =
        sqlx::query_scalar::<_, i64>("SELECT COALESCE(max(id),0) FROM camera_reel.photo_events")
            .fetch_one(&state.credits.pool)
            .await?;
    let select = |after, horizon| {
        sqlx::query_scalar::<_,i64>("SELECT e.id FROM camera_reel.photo_events e WHERE e.id>$2 AND e.id<=$3 AND e.kind='photo-taken' AND NOT EXISTS(SELECT 1 FROM credits.photo_goal_receipts r WHERE r.photo_id=e.photo_id) ORDER BY e.id LIMIT $1").bind(limit).bind(after).bind(horizon).fetch_all(&state.credits.pool)
    };
    if horizon == 0 && state.goal_scan_retry.swap(false, Ordering::Relaxed) {
        // Retry the events a previous round failed, even while new photos
        // keep arriving above the cursor.
        after = 0;
        horizon = current_horizon;
    } else if horizon == 0 && current_horizon > after {
        horizon = current_horizon;
    } else if horizon == 0 {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let reconciled_at = state.goal_scan_reconcile_at_ms.load(Ordering::Relaxed);
        if now_ms.saturating_sub(reconciled_at) < RECONCILE_INTERVAL_MS {
            return Ok(0);
        }
        // Sequence values may commit out of order. Revisit old unreceipted
        // events periodically, without rescanning the full retention window
        // on every five-second worker tick.
        after = 0;
        horizon = current_horizon;
        state
            .goal_scan_reconcile_at_ms
            .store(now_ms, Ordering::Relaxed);
    }
    if horizon == 0 {
        return Ok(0);
    }
    let ids = select(after, horizon).await?;
    state.goal_scan_after.store(after, Ordering::Relaxed);
    state.goal_scan_horizon.store(horizon, Ordering::Relaxed);
    let round_complete = ids.len() < limit as usize || ids.last().is_some_and(|id| *id >= horizon);
    let mut processed = 0;
    let mut error = None;
    // A failed event keeps no receipt and is retried by the next round; the
    // cursor moves past it so later photos are never starved.
    for id in ids {
        match process_event(state, id, super::store::now()).await {
            Ok(applied) => processed += u64::from(applied),
            Err(e) => {
                tracing::warn!(event_id=id,error=%e,"photo goal event remains retryable");
                state.goal_scan_retry.store(true, Ordering::Relaxed);
                error = Some(e);
            }
        }
        after = id;
        state.goal_scan_after.store(after, Ordering::Relaxed);
    }
    if round_complete {
        state.goal_scan_after.store(horizon, Ordering::Relaxed);
        state.goal_scan_horizon.store(0, Ordering::Relaxed);
        state
            .goal_scan_reconcile_at_ms
            .store(chrono::Utc::now().timestamp_millis(), Ordering::Relaxed);
    }
    if let Some(error) = error {
        return Err(error);
    }

    Ok(processed)
}

async fn process_event(state: &SharedState, id: i64, now: NaiveDateTime) -> Result<bool, ApiError> {
    let mut tx = state.credits.pool.begin().await?;
    let Some(event) = sqlx::query("SELECT photo_id,user_address,payload,created_at FROM camera_reel.photo_events WHERE id=$1 AND kind='photo-taken' FOR UPDATE")
        .bind(id).fetch_optional(&mut *tx).await? else { return Ok(false); };
    let photo: Uuid = event.get("photo_id");
    let address: String = event.get::<String, _>("user_address").to_lowercase();
    wallet_lock(&mut tx, &address).await?;
    if sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM credits.photo_goal_receipts WHERE photo_id=$1 OR event_id=$2)",
    )
    .bind(photo)
    .bind(id)
    .fetch_one(&mut *tx)
    .await?
    {
        return Ok(false);
    }
    let occurred_at: NaiveDateTime = event
        .get::<chrono::DateTime<chrono::Utc>, _>("created_at")
        .naive_utc();
    let season = current(&mut tx, occurred_at).await?;
    let mut outcome = if season.is_none() {
        Some("no-season")
    } else {
        policy(state, &mut tx, &address).await?
    };
    if outcome.is_none() {
        let eligible = sqlx::query("SELECT COALESCE((SELECT has_started_program FROM credits.wallets WHERE address=$1),false) AND NOT EXISTS(SELECT 1 FROM credits.program_opt_outs WHERE address=$1) AS enrolled, EXISTS(SELECT 1 FROM credits.wallet_flags WHERE address=$1) OR COALESCE((SELECT is_blocked_for_claiming FROM credits.wallets WHERE address=$1),false) AS blocked, EXISTS(SELECT 1 FROM social.subscriptions WHERE address=$1) AS subscribed")
            .bind(&address).fetch_one(&mut *tx).await?;
        outcome = if !eligible.get::<bool, _>("enrolled") {
            Some("not-enrolled")
        } else if eligible.get::<bool, _>("blocked") {
            Some("flagged")
        } else if !eligible.get::<bool, _>("subscribed") {
            Some("no-subscription")
        } else if !qualifies(&event.get::<Value, _>("payload"), &address) {
            Some("not-qualifying")
        } else {
            None
        };
    }
    let mut ordinal = None;
    let season_id = season.as_ref().map(|s| s.id);
    let week = season.as_ref().map(|s| s.week(occurred_at).0);
    if outcome.is_none() {
        let season = season.as_ref().unwrap();
        let week = week.unwrap();
        initialize(&mut tx, &address, season.id, week).await?;
        let progress = sqlx::query("SELECT completed_steps,total_steps,reward_wei::text AS reward_wei FROM credits.goal_progress WHERE address=$1 AND season_id=$2 AND week_id=$3 AND goal_id=$4 FOR UPDATE")
            .bind(&address).bind(season.id).bind(week).bind(GOAL).fetch_optional(&mut *tx).await?;
        match progress {
            None => outcome = Some("no-goal"),
            Some(p) if p.get::<i32, _>("completed_steps") == p.get::<i32, _>("total_steps") => {
                outcome = Some("already-complete")
            }
            Some(p) => {
                let steps = p.get::<i32, _>("completed_steps") + 1;
                let complete = steps == p.get::<i32, _>("total_steps");
                sqlx::query("UPDATE credits.goal_progress SET completed_steps=$5,completed_at=$6 WHERE address=$1 AND season_id=$2 AND week_id=$3 AND goal_id=$4")
                    .bind(&address).bind(season.id).bind(week).bind(GOAL).bind(steps).bind(complete.then_some(now)).execute(&mut *tx).await?;
                if complete {
                    let reward_wei: String = p.get("reward_wei");
                    grant(&mut tx, state, &address, season, week, &reward_wei, now).await?;
                }
                ordinal = Some(steps);
            }
        }
    }
    sqlx::query("INSERT INTO credits.photo_goal_receipts(photo_id,event_id,address,processed_at,outcome,season_id,week_id,ordinal) VALUES($1,$2,$3,$4,$5,$6,$7,$8)")
        .bind(photo).bind(id).bind(&address).bind(now).bind(outcome.unwrap_or("counted")).bind(season_id).bind(week).bind(ordinal).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(true)
}

async fn grant(
    conn: &mut PgConnection,
    state: &SharedState,
    address: &str,
    season: &Season,
    week: i32,
    amount: &str,
    now: NaiveDateTime,
) -> Result<(), ApiError> {
    let signer = state
        .settings
        .credits_signer_key
        .as_deref()
        .ok_or_else(|| {
            ApiError::service_unavailable("CREDITS_SIGNER_KEY_FILE is required for goal completion")
        })?;
    let signer = TreasuryWallet::from_hex(signer)
        .map_err(|_| ApiError::service_unavailable("invalid Credits signer"))?;
    let manager = state
        .settings
        .credits_manager_contract
        .as_deref()
        .and_then(|v| Address::from_str(v).ok())
        .ok_or_else(|| {
            ApiError::service_unavailable(
                "CREDITS_MANAGER_CONTRACT is required for goal completion",
            )
        })?;
    let chain = u64::try_from(state.settings.usage.chain_id)
        .ok()
        .filter(|v| *v > 0)
        .ok_or_else(|| ApiError::service_unavailable("invalid Credits chain"))?;
    // Progress snapshots the assignment so edits to its week range or reward
    // cannot strand an in-flight goal or change its promised payout.
    let expires = (season.end_date + TimeDelta::days(i64::from(season.grace_days)))
        .with_nanosecond(0)
        .unwrap();
    let expires_seconds = u64::try_from(expires.and_utc().timestamp())
        .map_err(|_| ApiError::Internal("invalid goal expiry".into()))?;
    if expires <= now {
        return Err(ApiError::service_unavailable(
            "goal expiry must be in the future",
        ));
    }
    let key = format!("{address}-{GOAL}-{}-{week}", season.id);
    let salt: B256 = keccak256(Uuid::new_v4().to_string().as_bytes());
    let credit = CreditIdentity {
        sender: Address::from_str(address)
            .map_err(|_| ApiError::Internal("invalid photo owner".into()))?,
        chain_id: chain,
        manager,
        value: uint(amount)?,
        expires_at: expires_seconds,
        salt,
    };
    let signature = signer.sign_credit(&credit).map_err(ApiError::Internal)?;
    sqlx::query("INSERT INTO credits.goal_grants(address,season_id,week_id,goal_id,salt,chain_id,contract,credit_hash,amount_wei,signature,created_at,expires_at) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9::numeric,$10,$11,$12)")
        .bind(address).bind(season.id).bind(week).bind(GOAL).bind(format!("{salt:#x}")).bind(chain as i64).bind(format!("{manager:#x}"))
        .bind(format!("{:#x}", credit.credit_hash())).bind(amount).bind(signature).bind(now).bind(expires).execute(&mut *conn).await?;
    let notification = catalyst_social::notifications::NewNotification {
        id: Uuid::new_v5(&NOTIFICATION_NAMESPACE, key.as_bytes()),
        address: catalyst_social::Address::parse(address)
            .map_err(|_| ApiError::Internal("invalid photo owner".into()))?,
        kind: "credits_goal_completed".into(),
        occurred_at: now,
        metadata: json!({"goalId":GOAL,"creditsObtained":mana_number(amount)?,"image":"https://assets-cdn.decentraland.org/credits/notification-icon.png","title":"Weekly Goal Completed!","description":"Claim your Credits to unlock them"}),
    };
    catalyst_social::notifications::store::insert_keyed_notification(conn, &key, &notification)
        .await?;
    Ok(())
}

const BACKLOG_SAMPLE_INTERVAL: Duration = Duration::from_secs(5 * 60);
const BACKLOG_SAMPLE_TIMEOUT: Duration = Duration::from_secs(1);

/// This global anti-join is diagnostic, not part of acknowledging photo facts.
/// Report an age only when the bounded query actually supplies a sample.
async fn sample_oldest(pool: &sqlx::PgPool) -> Option<String> {
    let sample = tokio::time::timeout(BACKLOG_SAMPLE_TIMEOUT, async {
        let mut tx = pool.begin().await?;
        // The server also bounds its work if the async deadline drops the future.
        // Transaction-local configuration never leaks to another pooled caller.
        sqlx::query("SELECT set_config('statement_timeout','1000',true)")
            .execute(&mut *tx).await?;
        let oldest = sqlx::query_scalar::<_, Option<chrono::DateTime<chrono::Utc>>>(
            "SELECT min(e.created_at) FROM camera_reel.photo_events e WHERE e.kind='photo-taken' AND NOT EXISTS(SELECT 1 FROM credits.photo_goal_receipts r WHERE r.photo_id=e.photo_id)",
        ).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        Ok::<_, sqlx::Error>(oldest)
    }).await;
    match sample {
        Ok(Ok(Some(oldest))) => {
            let age = (chrono::Utc::now() - oldest).num_seconds().max(0);
            tracing::info!(age_seconds = age, "photo goals backlog diagnostic sample");
            if age >= 25 * 24 * 60 * 60 {
                tracing::warn!(
                    age_seconds = age,
                    "sampled photo goals backlog is approaching the 30 day source retention limit"
                );
            }
            Some(format!(
                "sampled oldest unreceipted photo age: {age} seconds"
            ))
        }
        Ok(Ok(None)) => {
            tracing::info!("photo goals backlog diagnostic sample found no unreceipted events");
            Some("diagnostic sample found no unreceipted photo events".into())
        }
        Ok(Err(error)) => {
            tracing::warn!(%error, "photo goals backlog diagnostic failed; no sample available");
            None
        }
        Err(_) => {
            tracing::warn!("photo goals backlog diagnostic timed out; no sample available");
            None
        }
    }
}

async fn process_pass(
    state: &SharedState,
    last_sample: &parking_lot::Mutex<Option<std::time::Instant>>,
) -> Result<(u64, Option<String>), ApiError> {
    let result = process_batch(state, 100).await;
    let should_sample = {
        let mut last = last_sample.lock();
        let now = std::time::Instant::now();
        if last.is_none_or(|at| now.duration_since(at) >= BACKLOG_SAMPLE_INTERVAL) {
            // Failed samples are rate limited too; release the timer before I/O.
            *last = Some(now);
            true
        } else {
            false
        }
    };
    let sample = if should_sample {
        sample_oldest(&state.credits.pool).await
    } else {
        None
    };
    result.map(|processed| (processed, sample))
}

pub fn spawn_job(state: &SharedState, jobs: &Jobs) {
    flags::spawn(state, jobs);
    let state = Arc::clone(state);
    let last_sample = Arc::new(parking_lot::Mutex::new(None));
    jobs.spawn_periodic_leased(
        "credits-photo-goals",
        PeriodicCfg::every(Duration::from_secs(5)),
        move |ctx| {
            let state = state.clone();
            let last_sample = last_sample.clone();
            async move {
                if crate::shared::writes::ops_refusal(state.settings.writes_enabled).is_some() {
                    return Ok(());
                }
                let (n, sample) = process_pass(&state, &last_sample).await?;
                let message = format!("{n} photo goal receipts committed");
                ctx.progress(match sample {
                    Some(sample) => format!("{message}; {sample}"),
                    None => message,
                });
                Ok(())
            }
        },
    );
}

/// The source defaults a failed paused() read to false. This view never gates
/// photo progression or claims; signing has no network I/O in its transaction.
async fn paused(state: &SharedState) -> bool {
    use alloy_sol_types::SolValue;
    let Some(chain) = &state.goals_chain else {
        return false;
    };
    let Some(manager) = state
        .settings
        .credits_manager_contract
        .as_deref()
        .and_then(|v| Address::from_str(v).ok())
    else {
        return false;
    };
    match chain.eth_call(manager, &keccak256(b"paused()")[..4]).await {
        Ok(bytes) => bool::abi_decode(&bytes).unwrap_or(false),
        Err(error) => {
            tracing::warn!(%error,"credits season paused view unavailable");
            false
        }
    }
}
