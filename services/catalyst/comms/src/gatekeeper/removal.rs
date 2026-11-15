//! Comms-owned durable ban removals; no network call runs in a SQL transaction.
use crate::gatekeeper::admission::cluster::safety::{DispatchPhase, DispatchState};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use chrono::NaiveDateTime;
use futures::StreamExt;
use rand::RngExt;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::CommsState;
use super::livekit::{RoomServiceError, address_from_identity};
use crate::Address;
use crate::jobs::Jobs;
use crate::shared::now_utc;

const PAGE: i64 = 16;
const IDENTITIES: usize = 64;
const QUANTUM: Duration = Duration::from_secs(20);
const CLAIM_SECONDS: i64 = 60;

#[derive(Clone, Debug, sqlx::FromRow)]
struct Intent {
    root_key: String,
    target_key: String,
    generation: Uuid,
    cause_kind: String,
    wallet: String,
    actor: String,
    ban_id: Option<Uuid>,
    place_id: Option<String>,
    scene_version: Option<NaiveDateTime>,
    scope: Value,
    room_name: Option<String>,
    room_sid: Option<String>,
    participant_identity: Option<String>,
    cursor_room: String,
    cursor_sid: String,
    cursor_identity: String,
    attempts: i32,
    next_attempt_at: NaiveDateTime,
    claim_id: Option<Uuid>,
    claim_until: Option<NaiveDateTime>,
    created_at: NaiveDateTime,
}

fn key(parts: &[&str]) -> String {
    hex::encode(Sha256::digest(
        serde_json::to_vec(parts).expect("string array"),
    ))
}

async fn lock_root(tx: &mut PgConnection, root_key: &str) -> Result<(), sqlx::Error> {
    // Pair with child INSERT's root lock. A following DELETE uses a fresh
    // snapshot, including every child inserted before this lock was acquired.
    let _:Option<Uuid>=sqlx::query_scalar("SELECT generation FROM social.livekit_removal_intents WHERE root_key=$1 AND target_key='' FOR UPDATE")
        .bind(root_key).fetch_optional(tx).await?;
    Ok(())
}

pub(super) async fn platform(
    tx: &mut PgConnection,
    id: Uuid,
    wallet: &str,
) -> Result<(), sqlx::Error> {
    root(
        tx,
        &key(&["platform", &id.to_string()]),
        "platform",
        wallet,
        Some(id),
        None,
        None,
        &[],
    )
    .await
}

pub(super) async fn scene(
    tx: &mut PgConnection,
    place: &str,
    wallet: &str,
    version: NaiveDateTime,
    rooms: &[String],
) -> Result<(), sqlx::Error> {
    let root_key = key(&["scene", place, wallet]);
    // The ban upsert and this replacement run in the same owning transaction.
    lock_root(tx, &root_key).await?;
    sqlx::query("DELETE FROM social.livekit_removal_intents WHERE root_key = $1")
        .bind(&root_key)
        .execute(&mut *tx)
        .await?;
    root(
        tx,
        &root_key,
        "scene",
        wallet,
        None,
        Some(place),
        Some(version),
        rooms,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn root(
    tx: &mut PgConnection,
    root_key: &str,
    cause: &str,
    wallet: &str,
    ban_id: Option<Uuid>,
    place: Option<&str>,
    version: Option<NaiveDateTime>,
    rooms: &[String],
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO social.livekit_removal_intents (root_key,target_key,generation,cause_kind,wallet,actor,ban_id,place_id,scene_version,scope,next_attempt_at) VALUES ($1,'',COALESCE((SELECT generation FROM social.livekit_removal_intents WHERE root_key=$1 LIMIT 1),$2),$3,$4,$4,$5,$6,$7,$8,$9) ON CONFLICT (root_key,target_key) DO UPDATE SET next_attempt_at=LEAST(social.livekit_removal_intents.next_attempt_at,EXCLUDED.next_attempt_at)")
        .bind(root_key).bind(Uuid::new_v4()).bind(cause).bind(wallet).bind(ban_id).bind(place).bind(version)
        .bind(serde_json::json!(rooms)).bind(now_utc()).execute(tx).await?;
    Ok(())
}

pub(super) async fn cancel_platform(tx: &mut PgConnection, id: Uuid) -> Result<(), sqlx::Error> {
    lock_root(tx, &key(&["platform", &id.to_string()])).await?;
    sqlx::query("DELETE FROM social.livekit_removal_intents WHERE root_key=$1 AND ban_id=$2")
        .bind(key(&["platform", &id.to_string()]))
        .bind(id)
        .execute(tx)
        .await?;
    Ok(())
}

pub(super) async fn cancel_scene(
    tx: &mut PgConnection,
    place: &str,
    wallet: &str,
    version: NaiveDateTime,
) -> Result<(), sqlx::Error> {
    lock_root(tx, &key(&["scene", place, wallet])).await?;
    sqlx::query(
        "DELETE FROM social.livekit_removal_intents WHERE root_key=$1 AND scene_version=$2",
    )
    .bind(key(&["scene", place, wallet]))
    .bind(version)
    .execute(tx)
    .await?;
    Ok(())
}

/// Existing cross-owner callbacks become durable only after this insert succeeds.
pub(super) async fn operator_block(pool: &PgPool, wallet: &Address) -> Result<(), sqlx::Error> {
    let mut connection = pool.acquire().await?;
    root(
        &mut connection,
        &key(&["block", wallet.as_str()]),
        "block",
        wallet.as_str(),
        None,
        None,
        None,
        &[],
    )
    .await
}

pub(crate) async fn connection(
    pool: &PgPool,
    room: &str,
    wallet: &Address,
) -> Result<(), sqlx::Error> {
    let mut connection = pool.acquire().await?;
    root(
        &mut connection,
        &key(&["connection", room, wallet.as_str()]),
        "connection",
        wallet.as_str(),
        None,
        None,
        None,
        &[room.to_owned()],
    )
    .await
}

pub(super) async fn room_closed(pool: &PgPool, room: &str, sid: &str) -> Result<(), sqlx::Error> {
    // Discovery roots have no captured instance and must finish their remaining
    // obligations. A close never cancels another SID reusing the room name.
    sqlx::query("DELETE FROM social.livekit_removal_intents WHERE room_name=$1 AND room_sid=$2 AND target_key<>''")
        .bind(room).bind(sid).execute(pool).await?;
    Ok(())
}

async fn claim(pool: &PgPool, only_roots: Option<&[String]>) -> Result<Vec<Intent>, sqlx::Error> {
    let now = now_utc();
    sqlx::query_as("WITH due AS (SELECT root_key,target_key FROM social.livekit_removal_intents WHERE next_attempt_at <= $1 AND (claim_until IS NULL OR claim_until <= $1) AND ($5::text[] IS NULL OR root_key=ANY($5)) ORDER BY next_attempt_at,created_at,root_key,target_key FOR UPDATE SKIP LOCKED LIMIT $4) UPDATE social.livekit_removal_intents t SET claim_id=$2,claim_until=$3 FROM due WHERE t.root_key=due.root_key AND t.target_key=due.target_key RETURNING t.*")
        .bind(now).bind(Uuid::new_v4()).bind(now + chrono::Duration::seconds(CLAIM_SECONDS)).bind(PAGE).bind(only_roots)
        .fetch_all(pool).await
}

async fn current(pool: &PgPool, row: &Intent) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM social.livekit_removal_intents WHERE root_key=$1 AND target_key=$2 AND generation=$3 AND claim_id=$4 AND claim_until>$5)")
        .bind(&row.root_key).bind(&row.target_key).bind(row.generation).bind(row.claim_id).bind(now_utc())
        .fetch_one(pool).await
}

async fn finish(pool: &PgPool, row: &Intent) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM social.livekit_removal_intents WHERE root_key=$1 AND target_key=$2 AND generation=$3 AND claim_id=$4 AND claim_until>$5")
        .bind(&row.root_key).bind(&row.target_key).bind(row.generation).bind(row.claim_id).bind(now_utc()).execute(pool).await?;
    Ok(())
}

fn ceiling_ms(attempt: u32) -> u64 {
    1_000u64
        .saturating_mul(1u64 << attempt.saturating_sub(1).min(5))
        .min(30_000)
}

async fn failed(pool: &PgPool, row: &Intent, error: &anyhow::Error) -> Result<(), sqlx::Error> {
    let attempt = row.attempts.saturating_add(1);
    let delay = rand::rng().random_range(0..=ceiling_ms(attempt as u32));
    let updated = sqlx::query("UPDATE social.livekit_removal_intents SET attempts=$5,next_attempt_at=$6,claim_id=NULL,claim_until=NULL WHERE root_key=$1 AND target_key=$2 AND generation=$3 AND claim_id=$4 AND claim_until>$7")
        .bind(&row.root_key).bind(&row.target_key).bind(row.generation).bind(row.claim_id).bind(attempt)
        .bind(now_utc() + chrono::Duration::milliseconds(delay as i64)).bind(now_utc()).execute(pool).await?.rows_affected();
    if updated != 0 {
        metrics::counter!("comms_livekit_removal_failures_total").increment(1);
        if attempt >= 10 {
            tracing::error!(attempt, age_seconds=(now_utc()-row.created_at).num_seconds(), cause=%row.cause_kind, error=%error, "LiveKit enforcement still pending; continuing retries");
        } else {
            tracing::warn!(attempt, cause=%row.cause_kind, error=%error, "LiveKit enforcement retained for retry");
        }
    }
    Ok(())
}

fn sfu(error: RoomServiceError) -> anyhow::Error {
    anyhow!("LiveKit request failed: {error:?}")
}

/// The `<world>:<base>` key of a world scene intent queued before the
/// `schemas/releases/world-scene-ban-keys.sql` release,
/// which names the scene's entity id (bans moved, queued intents did not).
async fn legacy_world_ban_key(
    state: &CommsState,
    row: &Intent,
) -> Result<Option<crate::access::SceneBanKey>> {
    let Some(scene) = row
        .place_id
        .as_deref()
        .and_then(|place| crate::world_identity::SceneId::parse(place).ok())
    else {
        return Ok(None);
    };
    let rooms = row.scope.as_array().map(Vec::as_slice).unwrap_or_default();
    let Some(world) = rooms
        .iter()
        .filter_map(Value::as_str)
        .filter_map(crate::rooms::decode_world_scene_room)
        .find_map(|(world, room_scene)| (room_scene == scene).then_some(world))
    else {
        return Ok(None);
    };
    let unavailable = |error| anyhow!("scene identity unavailable: {error:?}");
    let base = match state
        .scenes
        .world_scene(&world, &scene)
        .await
        .map_err(unavailable)?
    {
        Some(deployment) => Some(deployment.base),
        None => state
            .scenes
            .replaced_world_scene(&world, &scene)
            .await
            .map_err(unavailable)?
            .map(|replaced| replaced.base),
    };
    Ok(base.map(|base| crate::access::SceneBanKey::world(&world, base)))
}

async fn authorized(state: &CommsState, row: &Intent, actor: &Address) -> Result<bool> {
    match row.cause_kind.as_str() {
        "platform" => Ok(state
            .moderation
            .platform_removal_applies(
                row.ban_id.ok_or_else(|| anyhow!("missing ban reference"))?,
                actor,
            )
            .await?),
        "scene" => {
            if actor.as_str() != row.wallet {
                return Ok(false);
            }
            let place = row
                .place_id
                .as_deref()
                .ok_or_else(|| anyhow!("missing place"))?;
            let version = row
                .scene_version
                .ok_or_else(|| anyhow!("missing scene version"))?;
            if state
                .moderation
                .scene_removal_applies(place, actor, version)
                .await?
            {
                return Ok(true);
            }
            Ok(match legacy_world_ban_key(state, row).await? {
                Some(key) => {
                    state
                        .moderation
                        .scene_removal_applies(key.as_str(), actor, version)
                        .await?
                }
                None => false,
            })
        }
        "block" => {
            if actor.as_str() == row.wallet {
                state
                    .policy
                    .is_blocked(actor)
                    .await
                    .map_err(|_| anyhow!("operator block authority unavailable"))
            } else {
                Ok(false)
            }
        }
        "connection" => {
            if actor.as_str() != row.wallet {
                return Ok(false);
            }
            let ban = state
                .bans
                .ban(actor, None, None, now_utc())
                .await
                .map_err(|_| anyhow!("connection ban authority unavailable"))?;
            if ban.is_some() {
                return Ok(true);
            }
            state
                .policy
                .is_blocked(actor)
                .await
                .map_err(|_| anyhow!("operator block authority unavailable"))
        }
        _ => Err(anyhow!("unknown removal cause")),
    }
}

async fn add_target(
    state: &CommsState,
    row: &Intent,
    room: &str,
    sid: &str,
    identity: &str,
    actor: &Address,
) -> Result<()> {
    sqlx::query("WITH source AS (SELECT * FROM social.livekit_removal_intents WHERE root_key=$1 AND target_key=$2 AND generation=$3 AND claim_id=$4 AND claim_until>$10 FOR UPDATE) INSERT INTO social.livekit_removal_intents (root_key,target_key,generation,cause_kind,wallet,actor,ban_id,place_id,scene_version,scope,room_name,room_sid,participant_identity,next_attempt_at) SELECT root_key,$5,generation,cause_kind,wallet,$9,ban_id,place_id,scene_version,scope,$6,$7,$8,$10 FROM source ON CONFLICT (root_key,target_key) DO NOTHING")
        .bind(&row.root_key).bind(&row.target_key).bind(row.generation).bind(row.claim_id)
        .bind(key(&[room,sid,identity])).bind(room).bind(sid).bind(identity).bind(actor.as_str()).bind(now_utc())
        .execute(&state.pool).await?;
    Ok(())
}

async fn progress(
    pool: &PgPool,
    row: &Intent,
    room: &str,
    sid: &str,
    identity: &str,
) -> Result<()> {
    sqlx::query("UPDATE social.livekit_removal_intents SET cursor_room=$5,cursor_sid=$6,cursor_identity=$7,next_attempt_at=$8,claim_id=NULL,claim_until=NULL WHERE root_key=$1 AND target_key=$2 AND generation=$3 AND claim_id=$4 AND claim_until>$8")
        .bind(&row.root_key).bind(&row.target_key).bind(row.generation).bind(row.claim_id).bind(room).bind(sid).bind(identity).bind(now_utc()).execute(pool).await?;
    Ok(())
}

async fn discover(state: &CommsState, row: &Intent) -> Result<()> {
    let actor = Address::parse(&row.wallet).map_err(|_| anyhow!("invalid removal wallet"))?;
    if !authorized(state, row, &actor).await? {
        return finish(&state.pool, row).await.map_err(Into::into);
    }
    let service = state.room_service();
    let mut rooms: Vec<String> = if matches!(row.cause_kind.as_str(), "platform" | "block") {
        service.list_rooms().await.map_err(sfu)?
    } else {
        serde_json::from_value(row.scope.clone())?
    };
    rooms.sort();
    rooms.dedup();
    let room = if !row.cursor_sid.is_empty() {
        rooms
            .iter()
            .find(|room| **room == row.cursor_room)
            .or_else(|| {
                rooms
                    .iter()
                    .find(|room| room.as_str() > row.cursor_room.as_str())
            })
    } else {
        rooms
            .iter()
            .find(|room| room.as_str() > row.cursor_room.as_str())
    };
    let Some(room) = room else {
        return finish(&state.pool, row).await.map_err(Into::into);
    };
    let info = service.get_room_info(room).await.map_err(sfu)?;
    let Some(info) = info else {
        return progress(&state.pool, row, room, "", "").await;
    };
    let sid = info
        .get("sid")
        .and_then(Value::as_str)
        .filter(|sid| !sid.is_empty())
        .ok_or_else(|| anyhow!("room instance unknown"))?;
    let after = if row.cursor_sid == sid {
        row.cursor_identity.as_str()
    } else {
        ""
    };
    let mut participants = match service.list_participants(room).await {
        Ok(participants) => participants,
        Err(RoomServiceError::NotFound) => return progress(&state.pool, row, room, "", "").await,
        Err(error) => return Err(sfu(error)),
    };
    participants.sort_by(|a, b| a.identity.cmp(&b.identity));
    let batch: Vec<_> = participants
        .iter()
        .filter(|p| p.identity.as_str() > after)
        .take(IDENTITIES)
        .collect();
    for participant in &batch {
        if !current(&state.pool, row).await? {
            return Ok(());
        }
        let Some(wallet) = address_from_identity(&participant.identity) else {
            continue;
        };
        let actor = Address::parse(&wallet).map_err(|_| anyhow!("invalid participant wallet"))?;
        if authorized(state, row, &actor).await? {
            add_target(state, row, room, sid, &participant.identity, &actor).await?;
        }
    }
    if batch.len() == IDENTITIES {
        progress(
            &state.pool,
            row,
            room,
            sid,
            &batch.last().expect("full batch").identity,
        )
        .await
    } else {
        progress(&state.pool, row, room, "", "").await
    }
}

async fn remove(state: &CommsState, row: &Intent) -> Result<()> {
    let actor = Address::parse(&row.actor).map_err(|_| anyhow!("invalid target wallet"))?;
    let room = row
        .room_name
        .as_deref()
        .ok_or_else(|| anyhow!("missing room"))?;
    let sid = row
        .room_sid
        .as_deref()
        .ok_or_else(|| anyhow!("missing room instance"))?;
    let identity = row
        .participant_identity
        .as_deref()
        .ok_or_else(|| anyhow!("missing participant"))?;
    if matches!(
        crate::rooms::room_scope(room),
        Some(crate::rooms::RoomScope::Island(_))
    ) {
        // Preserve the captured room-instance check before starting a policy
        // dispatch. Independent durable closure recovery does not depend on
        // this scheduling intent surviving a room-close event.
        let info = state
            .room_service()
            .get_room_info(room)
            .await
            .map_err(sfu)?;
        let current_sid = info
            .as_ref()
            .and_then(|v| v.get("sid"))
            .and_then(Value::as_str);
        if current_sid != Some(sid) {
            return finish(&state.pool, row).await.map_err(Into::into);
        }
        return remove_island(state, row, &actor, room, identity).await;
    }
    let service = state.room_service();
    let info = service.get_room_info(room).await.map_err(sfu)?;
    if let Some(info) = info {
        let current_sid = info
            .get("sid")
            .and_then(Value::as_str)
            .filter(|sid| !sid.is_empty())
            .ok_or_else(|| anyhow!("room instance unknown"))?;
        if current_sid == sid
            && current(&state.pool, row).await?
            && authorized(state, row, &actor).await?
        {
            // Authority/generation checked immediately before the destructive
            // call. A completed remote request cannot be unsent after a lift.
            if row.cause_kind == "connection" {
                tokio::time::timeout(
                    Duration::from_secs(2),
                    service.remove_participant(room, identity),
                )
                .await
                .map_err(|_| anyhow!("connector kick deadline"))?
                .map_err(sfu)?;
            } else {
                service
                    .remove_participant(room, identity)
                    .await
                    .map_err(sfu)?;
            }
        }
    }
    finish(&state.pool, row).await?;
    Ok(())
}

async fn run_one(state: &CommsState, row: &Intent) -> Result<()> {
    if row.next_attempt_at > now_utc() || row.claim_until.is_none_or(|until| until <= now_utc()) {
        return Ok(());
    }
    if !current(&state.pool, row).await? {
        return Ok(());
    }
    if row.target_key.is_empty() {
        discover(state, row).await
    } else {
        remove(state, row).await
    }
}

async fn run_batch(state: &CommsState) -> Result<()> {
    if !state.livekit.configured {
        return Ok(());
    }
    let compaction = state.island_safety.compact_settled().await;
    let recovery = resume_island_quarantines(state).await;
    // Even selection-wide maintenance failure must give ordinary intents a
    // chance. It is still returned after the batch; per-wallet failures log
    // and retain their closed hazards without failing the shared job.
    let claimed = claim(&state.pool, None).await?;
    futures::stream::iter(claimed)
        .for_each_concurrent(super::livekit::KICK_CONCURRENCY, |row| async move {
            let result = tokio::time::timeout(QUANTUM, run_one(state, &row))
                .await
                .unwrap_or_else(|_| Err(anyhow!("removal quantum elapsed")));
            if let Err(error) = result
                && let Err(error) = failed(&state.pool, &row, &error).await
            {
                tracing::error!(%error,"could not save LiveKit removal retry; claim will expire");
            }
        })
        .await;
    compaction.map_err(|_| anyhow!("island safety maintenance unavailable"))?;
    recovery?;
    Ok(())
}

pub(super) fn spawn_jobs(state: &Arc<CommsState>, jobs: &Jobs) {
    let state = state.clone();
    jobs.spawn_leased("comms-livekit-ban-removal",move |ctx| {
        let state=state.clone();
        async move {
            let mut passes=0u64;
            loop {
                tokio::select! { _=ctx.cancel.cancelled()=>return Ok(()), result=run_batch(&state)=>result? }
                passes=passes.wrapping_add(1);
                if passes%60==1 {
                    let (count,oldest,attempts):(i64,Option<NaiveDateTime>,Option<i32>)=sqlx::query_as("SELECT count(*),min(created_at),max(attempts) FROM social.livekit_removal_intents").fetch_one(&state.pool).await?;
                    metrics::gauge!("comms_livekit_removal_pending").set(count as f64);
                    metrics::gauge!("comms_livekit_removal_oldest_seconds").set(oldest.map(|at|(now_utc()-at).num_seconds().max(0)).unwrap_or(0) as f64);
                    metrics::gauge!("comms_livekit_removal_max_attempts").set(attempts.unwrap_or(0) as f64);
                    ctx.progress(format!("{count} pending removal intents"));
                }
                ctx.pass_done();
                tokio::select! { _=ctx.cancel.cancelled()=>return Ok(()), _=tokio::time::sleep(Duration::from_secs(1))=>{} }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_ceiling_doubles_caps_and_never_overflows() {
        assert_eq!(
            (1..=7).map(ceiling_ms).collect::<Vec<_>>(),
            vec![1000, 2000, 4000, 8000, 16000, 30000, 30000]
        );
        assert_eq!(ceiling_ms(u32::MAX), 30000);
        assert_ne!(key(&["ab", "c"]), key(&["a", "bc"]));
        assert_ne!(
            key(&["room", "old", "wallet"]),
            key(&["room", "new", "wallet"])
        );
    }

    #[tokio::test]
    async fn durable_claim_retry_recovery_and_reban_fencing() {
        if std::env::var("CATALYST_TEST_DATABASE_URL")
            .ok()
            .is_none_or(|value| value.trim().is_empty())
        {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "durable island regressions require CATALYST_TEST_DATABASE_URL in CI"
            );
            eprintln!(
                "local skip: durable island PostgreSQL fixture requires CATALYST_TEST_DATABASE_URL"
            );
            return;
        }

        let Some(pool) = crate::db::scratch::pool(8).await else {
            return;
        };
        let wallet = format!("removal-fixture-{}", Uuid::new_v4());
        let place = Uuid::new_v4().to_string();
        let root_key = key(&["scene", &place, &wallet]);
        let version = now_utc();
        let rooms = vec!["exact-original-room".to_owned()];
        // A failed producer transaction must leave no orphan accepted work.
        let mut tx = pool.begin().await.unwrap();
        scene(&mut tx, &place, &wallet, version, &rooms)
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        assert!(
            claim(&pool, Some(std::slice::from_ref(&root_key)))
                .await
                .unwrap()
                .is_empty()
        );
        let mut tx = pool.begin().await.unwrap();
        scene(&mut tx, &place, &wallet, version, &rooms)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let old = claim(&pool, Some(std::slice::from_ref(&root_key)))
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(old.scope, serde_json::json!(rooms));
        assert!(
            claim(&pool, Some(std::slice::from_ref(&root_key)))
                .await
                .unwrap()
                .is_empty()
        );
        // Expiry makes abandoned work recoverable with a new claim, not lost.
        sqlx::query("UPDATE social.livekit_removal_intents SET claim_until=$2 WHERE root_key=$1")
            .bind(&root_key)
            .bind(now_utc() - chrono::Duration::seconds(1))
            .execute(&pool)
            .await
            .unwrap();
        let recovered = claim(&pool, Some(std::slice::from_ref(&root_key)))
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert_ne!(old.claim_id, recovered.claim_id);
        assert!(!current(&pool, &old).await.unwrap());
        finish(&pool, &old).await.unwrap();
        assert!(current(&pool, &recovered).await.unwrap());
        // Passing the alert threshold remains pending and releases its claim.
        let mut retry = recovered.clone();
        retry.attempts = 10;
        failed(&pool, &retry, &anyhow!("discovery unavailable"))
            .await
            .unwrap();
        let (attempts, claim_id): (i32, Option<Uuid>) = sqlx::query_as(
            "SELECT attempts,claim_id FROM social.livekit_removal_intents WHERE root_key=$1",
        )
        .bind(&root_key)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(attempts, 11);
        assert!(claim_id.is_none());
        // A new ban generation cannot be deleted or retried by the old worker.
        let newer = version + chrono::Duration::seconds(1);
        let mut tx = pool.begin().await.unwrap();
        scene(&mut tx, &place, &wallet, newer, &rooms)
            .await
            .unwrap();
        cancel_scene(&mut tx, &place, &wallet, version)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        finish(&pool, &recovered).await.unwrap();
        failed(&pool, &recovered, &anyhow!("stale failure"))
            .await
            .unwrap();
        let new = claim(&pool, Some(std::slice::from_ref(&root_key)))
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert_ne!(new.generation, recovered.generation);
        assert_eq!(new.attempts, 0);
        let mut tx = pool.begin().await.unwrap();
        cancel_scene(&mut tx, &place, &wallet, newer).await.unwrap();
        tx.commit().await.unwrap();
        assert!(!current(&pool, &new).await.unwrap());
    }
    #[tokio::test]
    async fn room_close_matches_only_captured_instance_and_keeps_discovery_root() {
        let Some(pool) = crate::db::scratch::pool(8).await else {
            return;
        };
        let wallet = Uuid::new_v4().to_string();
        let room = Uuid::new_v4().to_string();
        let root_key = key(&["block", &wallet]);
        let mut conn = pool.acquire().await.unwrap();
        root(
            &mut conn,
            &root_key,
            "block",
            &wallet,
            None,
            None,
            None,
            &[],
        )
        .await
        .unwrap();
        root(
            &mut conn,
            &root_key,
            "block",
            &wallet,
            None,
            None,
            None,
            &[],
        )
        .await
        .unwrap();
        for sid in ["old", "new"] {
            sqlx::query("INSERT INTO social.livekit_removal_intents(root_key,target_key,generation,cause_kind,wallet,actor,room_name,room_sid,participant_identity) SELECT root_key,$2,generation,cause_kind,wallet,actor,$3,$4,$5 FROM social.livekit_removal_intents WHERE root_key=$1 AND target_key=''")
                .bind(&root_key).bind(key(&[&room,sid,&wallet])).bind(&room).bind(sid).bind(&wallet).execute(&pool).await.unwrap();
        }
        room_closed(&pool, &room, "old").await.unwrap();
        let remaining:Vec<String>=sqlx::query_scalar("SELECT target_key FROM social.livekit_removal_intents WHERE root_key=$1 ORDER BY target_key")
            .bind(&root_key).fetch_all(&pool).await.unwrap();
        assert_eq!(remaining.len(), 2);
        assert!(remaining.contains(&String::new()));
        assert!(remaining.contains(&key(&[&room, "new", &wallet])));
        sqlx::query("DELETE FROM social.livekit_removal_intents WHERE root_key=$1")
            .bind(&root_key)
            .execute(&pool)
            .await
            .unwrap();
    }
    #[derive(Default)]
    struct Sfu {
        rooms: Value,
        rosters: std::collections::HashMap<String, Value>,
        failures: usize,
        rejections: usize,
        remove_calls: usize,
        missing_room: bool,
        create_calls: usize,
        create_failures: usize,
        remove_pause: Option<Arc<tokio::sync::Notify>>,
        remove_entered: Arc<tokio::sync::Notify>,
        removed: Vec<String>,
        reads: usize,
    }

    fn complete_sfu_removal(fixture: &mut Sfu, body: &Value) -> axum::response::Response {
        use axum::{Json, response::IntoResponse};
        fixture.reads += 1;
        if fixture.missing_room {
            return (
                axum::http::StatusCode::NOT_FOUND,
                Json(serde_json::json!({"code":"not_found","msg":"requested room does not exist"})),
            )
                .into_response();
        }
        if fixture.rejections > 0 {
            fixture.rejections -= 1;
            return (
                axum::http::StatusCode::TOO_MANY_REQUESTS,
                Json(serde_json::json!({"code":"resource_exhausted"})),
            )
                .into_response();
        }
        if fixture.failures > 0 {
            fixture.failures -= 1;
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"code":"unavailable"})),
            )
                .into_response();
        }
        fixture
            .removed
            .push(body["identity"].as_str().unwrap().to_owned());
        Json(serde_json::json!({})).into_response()
    }

    async fn sfu_fixture(
        pool: PgPool,
    ) -> (
        CommsState,
        Arc<std::sync::Mutex<Sfu>>,
        tokio::task::JoinHandle<()>,
    ) {
        use axum::{Json, extract::State, response::IntoResponse};
        let fixture = Arc::new(std::sync::Mutex::new(Sfu {
            rooms: serde_json::json!({}),
            ..Default::default()
        }));
        let app = axum::Router::new()
            .fallback(axum::routing::post(
                |State(fixture): State<Arc<std::sync::Mutex<Sfu>>>,
                 uri: axum::http::Uri,
                 Json(body): Json<Value>| async move {
                    if uri.path().ends_with("/RemoveParticipant") {
                        let (entered, pause) = {
                            let mut f = fixture.lock().unwrap();
                            f.remove_calls += 1;
                            (f.remove_entered.clone(), f.remove_pause.clone())
                        };
                        // Accepted remote work has its own lifetime. Dropping the
                        // caller/HTTP response must not cancel the delayed effect
                        // this fixture uses to model an uncertain SFU request.
                        let remote = tokio::spawn(async move {
                            entered.notify_one();
                            if let Some(pause) = pause {
                                pause.notified().await;
                            }
                            complete_sfu_removal(&mut fixture.lock().unwrap(), &body)
                        });
                        return remote.await.expect("accepted test SFU operation completes");
                    }
                    let mut fixture = fixture.lock().unwrap();
                    fixture.reads += 1;
                    let method = uri.path().rsplit('/').next().unwrap();
                    let room = body["room"].as_str().unwrap_or("");
                    let response = match method {
                        "CreateRoom" => {
                            assert_eq!(body["emptyTimeout"], 60);
                            assert!(body["name"].as_str().unwrap().starts_with("island-"));
                            fixture.create_calls += 1;
                            if fixture.create_failures > 0 {
                                fixture.create_failures -= 1;
                                return (
                                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                                    Json(serde_json::json!({"code":"unavailable"})),
                                )
                                    .into_response();
                            }
                            fixture.missing_room = false;
                            serde_json::json!({"name":body["name"]})
                        }
                        "ListRooms" => {
                            let mut result = fixture.rooms.clone();
                            if let (Some(filter), Some(rooms)) = (
                                body.get("names").and_then(Value::as_array),
                                result.get_mut("rooms").and_then(Value::as_array_mut),
                            ) {
                                rooms
                                    .retain(|room| filter.iter().any(|name| name == &room["name"]));
                            }
                            result
                        }
                        "ListParticipants" => fixture
                            .rosters
                            .get(room)
                            .cloned()
                            .unwrap_or_else(|| serde_json::json!({})),
                        _ => panic!("unexpected fixture method {method}"),
                    };
                    Json(response).into_response()
                },
            ))
            .with_state(fixture.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut settings = super::super::Settings::for_tests();
        settings.livekit.api_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let state = CommsState::new(
            pool,
            crate::http::Outbound::for_tests(),
            settings,
            None,
            None,
            Arc::new(crate::interfaces::test_support::StubPolicy {
                blocked: Ok(true),
                ..Default::default()
            }),
            Arc::new(crate::interfaces::test_support::StubScenes::default()),
            Arc::new(crate::presence::Presence::new()),
            Arc::new(crate::interfaces::test_support::StubDirectory::default()),
        );
        (state, fixture, server)
    }

    fn wallet() -> Address {
        Address::parse(&format!("0x00000000{}", Uuid::new_v4().simple())).unwrap()
    }

    async fn force_due(pool: &PgPool, root_key: &str) {
        sqlx::query("UPDATE social.livekit_removal_intents SET next_attempt_at=$2 WHERE root_key=$1 AND claim_id IS NULL")
            .bind(root_key).bind(now_utc()-chrono::Duration::seconds(1)).execute(pool).await.unwrap();
    }

    async fn step(state: &CommsState, root_key: &str) -> usize {
        let mut errors = 0;
        for row in claim(&state.pool, Some(&[root_key.to_owned()]))
            .await
            .unwrap()
        {
            if let Err(error) = run_one(state, &row).await {
                errors += 1;
                failed(&state.pool, &row, &error).await.unwrap();
            }
        }
        errors
    }

    #[tokio::test]
    async fn coordinator_unknown_discovery_retries_then_removal_replays_after_claim_recovery() {
        let Some(pool) = crate::db::scratch::pool(8).await else {
            return;
        };
        let (mut state, fixture, server) = sfu_fixture(pool.clone()).await;
        let actor = wallet();
        let room = Uuid::new_v4().to_string();
        let root_key = key(&["block", actor.as_str()]);
        operator_block(&pool, &actor).await.unwrap();
        fixture.lock().unwrap().rooms = serde_json::json!({"rooms":"unknown"});
        assert_eq!(step(&state, &root_key).await, 1);
        force_due(&pool, &root_key).await;
        fixture.lock().unwrap().rooms = serde_json::json!({"rooms":[{"name":room,"sid":"SID"}]});
        fixture
            .lock()
            .unwrap()
            .rosters
            .insert(room.clone(), serde_json::json!({"participants":false}));
        assert_eq!(step(&state, &root_key).await, 1);
        force_due(&pool, &root_key).await;
        // Authority failure also preserves the root and makes no destructive call.
        state.policy = Arc::new(crate::interfaces::test_support::StubPolicy {
            blocked: Err(crate::AuthorityNotEstablished::store_unavailable(
                "fixture",
                &sqlx::Error::PoolClosed,
            )),
            ..Default::default()
        });
        assert_eq!(step(&state, &root_key).await, 1);
        state.policy = Arc::new(crate::interfaces::test_support::StubPolicy {
            blocked: Ok(true),
            ..Default::default()
        });
        force_due(&pool, &root_key).await;
        let actual = format!("{}-tab-2", actor.as_str().to_uppercase());
        fixture.lock().unwrap().rosters.insert(
            room.clone(),
            serde_json::json!({"participants":[{"identity":actual}]}),
        );
        assert_eq!(step(&state, &root_key).await, 0);
        // A malformed filtered room lookup must not finish the captured target.
        fixture.lock().unwrap().rooms = serde_json::json!({"rooms":null});
        assert_eq!(step(&state, &root_key).await, 2);
        force_due(&pool, &root_key).await;
        fixture.lock().unwrap().rooms = serde_json::json!({"rooms":[{"name":room,"sid":"SID"}]});
        fixture.lock().unwrap().failures = 1;
        assert_eq!(step(&state, &root_key).await, 1);
        assert!(fixture.lock().unwrap().removed.is_empty());
        force_due(&pool, &root_key).await;
        let abandoned = claim(&pool, Some(std::slice::from_ref(&root_key)))
            .await
            .unwrap()
            .pop()
            .unwrap();
        assert!(!abandoned.target_key.is_empty());
        sqlx::query("UPDATE social.livekit_removal_intents SET claim_until=$2 WHERE root_key=$1")
            .bind(&root_key)
            .bind(now_utc() - chrono::Duration::seconds(1))
            .execute(&pool)
            .await
            .unwrap();
        // Recreate the coordinator from persisted rows, not an in-memory checkpoint.
        let (restarted, _, other_server) = sfu_fixture(pool.clone()).await;
        let mut restarted = restarted;
        restarted.livekit = state.livekit.clone();
        assert_eq!(step(&restarted, &root_key).await, 0);
        assert_eq!(fixture.lock().unwrap().removed, vec![actual]);
        assert!(!current(&pool, &abandoned).await.unwrap());
        // A confirmed protobuf empty inventory is terminal; unlike malformed data.
        operator_block(&pool, &actor).await.unwrap();
        fixture.lock().unwrap().rooms = serde_json::json!({});
        assert_eq!(step(&restarted, &root_key).await, 0);
        assert!(
            claim(&pool, Some(std::slice::from_ref(&root_key)))
                .await
                .unwrap()
                .is_empty()
        );
        server.abort();
        other_server.abort();
    }

    #[tokio::test]
    async fn coordinator_pages_large_roster_and_backlog_without_starving_targets_or_next_room() {
        let Some(pool) = crate::db::scratch::pool(8).await else {
            return;
        };
        let (state, fixture, server) = sfu_fixture(pool.clone()).await;
        let actor = wallet();
        let root_key = key(&["block", actor.as_str()]);
        operator_block(&pool, &actor).await.unwrap();
        let identities: Vec<_> = (0..65)
            .map(|i| format!("{}-tab-{i:03}", actor.as_str()))
            .collect();
        {
            let mut f = fixture.lock().unwrap();
            f.rooms = serde_json::json!({"rooms":[{"name":"a","sid":"A"},{"name":"b","sid":"B"}]});
            f.rosters.insert("a".into(),serde_json::json!({"participants":identities.iter().map(|identity|serde_json::json!({"identity":identity})).collect::<Vec<_>>()}));
            f.rosters.insert("b".into(),serde_json::json!({"participants":[{"identity":format!("{}-next-room",actor.as_str())}]}));
        }
        assert_eq!(step(&state, &root_key).await, 0);
        let (cursor,count):(String,i64)=sqlx::query_as("SELECT cursor_identity,(SELECT count(*) FROM social.livekit_removal_intents WHERE root_key=$1 AND target_key<>'') FROM social.livekit_removal_intents WHERE root_key=$1 AND target_key=''")
            .bind(&root_key).fetch_one(&pool).await.unwrap();
        assert_eq!(cursor, identities[63]);
        assert_eq!(count, 64);
        // Existing targets precede the rescheduled large root in due order;
        // each bounded page makes progress even while discovery has more work.
        let page = claim(&pool, Some(std::slice::from_ref(&root_key)))
            .await
            .unwrap();
        assert_eq!(page.len(), PAGE as usize);
        assert!(page.iter().all(|row| !row.target_key.is_empty()));
        for row in page {
            run_one(&state, &row).await.unwrap();
        }
        assert_eq!(fixture.lock().unwrap().removed.len(), PAGE as usize);
        for _ in 0..12 {
            assert_eq!(step(&state, &root_key).await, 0);
        }
        {
            let f = fixture.lock().unwrap();
            assert_eq!(f.removed.len(), 66);
            for identity in identities {
                assert!(f.removed.contains(&identity));
            }
            assert!(f.removed.contains(&format!("{}-next-room", actor.as_str())));
        }
        assert!(
            claim(&pool, Some(std::slice::from_ref(&root_key)))
                .await
                .unwrap()
                .is_empty()
        );
        server.abort();
    }

    #[tokio::test]
    async fn coordinator_selects_wallet_and_recorded_device_actual_identities_only() {
        let Some(pool) = crate::db::scratch::pool(8).await else {
            return;
        };
        let (state, fixture, server) = sfu_fixture(pool.clone()).await;
        let actor = wallet();
        let device_actor = wallet();
        let unrelated = wallet();
        let ban = Uuid::new_v4();
        let device = Uuid::new_v4().to_string();
        let root_key = key(&["platform", &ban.to_string()]);
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("INSERT INTO social.user_bans(id,banned_address,banned_by,reason,banned_device_id) VALUES ($1,$2,$2,'fixture',$3)")
            .bind(ban).bind(&actor).bind(&device).execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO social.player_connections(address,device_id) VALUES ($1,$2)")
            .bind(&device_actor)
            .bind(&device)
            .execute(&mut *tx)
            .await
            .unwrap();
        platform(&mut tx, ban, actor.as_str()).await.unwrap();
        tx.commit().await.unwrap();
        let direct = format!("{}-browser", actor.as_str().to_uppercase());
        let same_device = format!("{}-desktop", device_actor.as_str());
        {
            let mut f = fixture.lock().unwrap();
            f.rooms = serde_json::json!({"rooms":[{"name":"device-room","sid":"D"}]});
            f.rosters.insert("device-room".into(),serde_json::json!({"participants":[{"identity":direct},{"identity":same_device},{"identity":unrelated.as_str()}]}));
        }
        for _ in 0..4 {
            assert_eq!(step(&state, &root_key).await, 0);
        }
        let mut removed = fixture.lock().unwrap().removed.clone();
        removed.sort();
        let mut expected = vec![direct, same_device];
        expected.sort();
        assert_eq!(removed, expected);
        sqlx::query("DELETE FROM social.user_bans WHERE id=$1")
            .bind(ban)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM social.player_connections WHERE address=$1")
            .bind(&device_actor)
            .execute(&pool)
            .await
            .unwrap();
        server.abort();
    }
    #[tokio::test]
    async fn due_pages_keep_other_roots_and_captured_targets_moving_past_failing_discovery() {
        let Some(pool) = crate::db::scratch::pool(8).await else {
            return;
        };
        let (state, fixture, server) = sfu_fixture(pool.clone()).await;
        let blocked = wallet();
        let other = wallet();
        let bad = key(&["connection", "a", blocked.as_str()]);
        let good = key(&["connection", "b", other.as_str()]);
        connection(&pool, "a", &blocked).await.unwrap();
        connection(&pool, "b", &other).await.unwrap();
        // Both actor block checks apply; root a can never read its roster.
        {
            let mut f = fixture.lock().unwrap();
            f.rooms = serde_json::json!({"rooms":[{"name":"a","sid":"A"},{"name":"b","sid":"B"}]});
            f.rosters
                .insert("a".into(), serde_json::json!({"participants":"unknown"}));
            f.rosters.insert(
                "b".into(),
                serde_json::json!({"participants":[{"identity":other.as_str()}]}),
            );
        }
        // A backlog larger than one page is older than both roots. These exact
        // targets must finish, and neither root can monopolize the next page.
        for i in 0..20 {
            sqlx::query("INSERT INTO social.livekit_removal_intents(root_key,target_key,generation,cause_kind,wallet,actor,scope,room_name,room_sid,participant_identity,next_attempt_at) SELECT root_key,$2,generation,cause_kind,wallet,actor,scope,'b','B',$3,$4 FROM social.livekit_removal_intents WHERE root_key=$1 AND target_key=''")
                .bind(&bad).bind(format!("captured-{i}")).bind(format!("{}-old-{i}",blocked.as_str())).bind(now_utc()-chrono::Duration::seconds(2)).execute(&pool).await.unwrap();
        }
        let scope = vec![bad.clone(), good.clone()];
        for _ in 0..8 {
            let page = claim(&pool, Some(&scope)).await.unwrap();
            assert!(page.len() <= PAGE as usize);
            futures::stream::iter(page)
                .for_each_concurrent(8, |row| {
                    let state = &state;
                    async move {
                        if let Err(error) = run_one(state, &row).await {
                            failed(&state.pool, &row, &error).await.unwrap();
                        }
                    }
                })
                .await;
            force_due(&pool, &bad).await;
        }
        assert!(
            fixture
                .lock()
                .unwrap()
                .removed
                .contains(&other.as_str().to_owned())
        );
        assert_eq!(fixture.lock().unwrap().removed.len(), 21);
        let outstanding: Vec<(String, String)> = sqlx::query_as(
            "SELECT root_key,target_key FROM social.livekit_removal_intents WHERE root_key=ANY($1)",
        )
        .bind(&scope)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(outstanding, vec![(bad.clone(), String::new())]);
        sqlx::query("DELETE FROM social.livekit_removal_intents WHERE root_key=ANY($1)")
            .bind(&scope)
            .execute(&pool)
            .await
            .unwrap();
        server.abort();
    }
    async fn platform_island_target(
        state: &CommsState,
        actor: &Address,
        room: &str,
    ) -> (Uuid, Intent) {
        let ban = Uuid::new_v4();
        let mut tx = state.pool.begin().await.unwrap();
        sqlx::query("INSERT INTO social.user_bans(id,banned_address,banned_by,reason) VALUES ($1,$2,$2,'durable island fixture')").bind(ban).bind(actor).execute(&mut *tx).await.unwrap();
        platform(&mut tx, ban, actor.as_str()).await.unwrap();
        tx.commit().await.unwrap();
        let root_key = key(&["platform", &ban.to_string()]);
        let root = claim(&state.pool, Some(std::slice::from_ref(&root_key)))
            .await
            .unwrap()
            .pop()
            .unwrap();
        add_target(state, &root, room, "ISLAND-SID", actor.as_str(), actor)
            .await
            .unwrap();
        let target = claim(&state.pool, Some(&[root_key]))
            .await
            .unwrap()
            .into_iter()
            .find(|r| !r.target_key.is_empty())
            .unwrap();
        (ban, target)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn durable_island_cancelled_rpc_lift_claim_recovery_room_close_and_restart_never_repeat_rpc()
     {
        if std::env::var("CATALYST_TEST_DATABASE_URL")
            .ok()
            .is_none_or(|value| value.trim().is_empty())
        {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "durable island regressions require CATALYST_TEST_DATABASE_URL in CI"
            );
            eprintln!(
                "local skip: durable island PostgreSQL fixture requires CATALYST_TEST_DATABASE_URL"
            );
            return;
        }

        let pool = crate::db::scratch::pool(8)
            .await
            .expect("mandatory disposable PostgreSQL durable enforcement fixture");
        let (mut state, fixture, server) = sfu_fixture(pool.clone()).await;
        state.cluster.self_hosted_token_quarantine = false;
        state.policy = Arc::new(crate::interfaces::test_support::StubPolicy::default());
        let actor = wallet();
        let room = format!("island-{}", Uuid::new_v4());
        let (ban, row) = platform_island_target(&state, &actor, &room).await;
        let release = Arc::new(tokio::sync::Notify::new());
        let entered = {
            let mut f = fixture.lock().unwrap();
            f.rooms = serde_json::json!({"rooms":[{"name":room,"sid":"ISLAND-SID"}]});
            f.remove_pause = Some(release.clone());
            f.remove_entered.clone()
        };
        let state = Arc::new(state);
        let worker = state.clone();
        let target = row.clone();
        let task = tokio::spawn(async move { remove(&worker, &target).await });
        entered.notified().await;
        assert!(state.island_safety.snapshot(actor.as_str()).await.is_err());
        // The owner row can move during SFU RPC: no assignment/wallet SQL lock
        // may survive across this external await.
        sqlx::query("INSERT INTO social.archipelago_v4_assignments(deployment_audience,owner_address,lane_key,authority_incarnation,assignment_revision,owner_session,owner_epoch,fencing_token) VALUES ('durable-enforcement',$1,'realm','source',1,'new-owner',1,1)")
            .bind(&actor).execute(&pool).await.unwrap();
        state
            .moderation
            .lift_ban(&actor, "moderator")
            .await
            .unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM social.livekit_removal_intents WHERE ban_id=$1"
            )
            .bind(ban)
            .fetch_one(&pool)
            .await
            .unwrap(),
            0
        );
        // Cancel while the remote handler is still running. No result is
        // recorded, even if the detached remote request later succeeds.
        task.abort();
        let _ = task.await;
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if !fixture.lock().unwrap().removed.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        room_closed(&pool, &room, "ISLAND-SID").await.unwrap();
        let restarted =
            crate::gatekeeper::admission::cluster::safety::SafetyStore::new(pool.clone());
        assert!(restarted.authorize(actor.as_str(), None, 0).await.is_err());
        assert!(
            restarted
                .authorize(wallet().as_str(), None, 0)
                .await
                .is_ok()
        );
        // A distinct real policy generation cannot dispatch past the old
        // wallet-wide uncertainty, even though the old intent was deleted.
        let (_, replacement) = platform_island_target(&state, &actor, &room).await;
        assert!(remove(&state, &replacement).await.is_err());
        assert_eq!(fixture.lock().unwrap().remove_calls, 1);
        assert!(restarted.authorize(actor.as_str(), None, 0).await.is_err());
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn durable_late_positive_settlement_survives_lift_and_expired_scheduling_claim() {
        if std::env::var("CATALYST_TEST_DATABASE_URL")
            .ok()
            .is_none_or(|value| value.trim().is_empty())
        {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "durable island regressions require CATALYST_TEST_DATABASE_URL in CI"
            );
            eprintln!(
                "local skip: durable island PostgreSQL fixture requires CATALYST_TEST_DATABASE_URL"
            );
            return;
        }

        let pool = crate::db::scratch::pool(8)
            .await
            .expect("mandatory disposable PostgreSQL settlement fixture");
        let (mut state, fixture, server) = sfu_fixture(pool.clone()).await;
        state.cluster.self_hosted_token_quarantine = false;
        state.policy = Arc::new(crate::interfaces::test_support::StubPolicy::default());
        let actor = wallet();
        let room = format!("island-{}", Uuid::new_v4());
        let (_, row) = platform_island_target(&state, &actor, &room).await;
        let release = Arc::new(tokio::sync::Notify::new());
        let entered = {
            let mut f = fixture.lock().unwrap();
            f.rooms = serde_json::json!({"rooms":[{"name":room,"sid":"ISLAND-SID"}]});
            f.remove_pause = Some(release.clone());
            f.remove_entered.clone()
        };
        let state = Arc::new(state);
        let worker = state.clone();
        let target = row.clone();
        let task = tokio::spawn(async move { remove(&worker, &target).await });
        entered.notified().await;
        sqlx::query("UPDATE social.livekit_removal_intents SET claim_until=timestamp '1970-01-01' WHERE root_key=$1").bind(&row.root_key).execute(&pool).await.unwrap();
        state
            .moderation
            .lift_ban(&actor, "moderator")
            .await
            .unwrap();
        release.notify_one();
        task.await.unwrap().unwrap();
        let (status, cutoff): (String, i64) = sqlx::query_as(
            "SELECT state,cutoff FROM social.island_cleanup_dispatches WHERE wallet=$1",
        )
        .bind(&actor)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "confirmed");
        let lease = state
            .island_safety
            .authorize(actor.as_str(), None, cutoff as u64)
            .await
            .unwrap();
        assert_eq!(lease.snapshot.token_floor, cutoff as u64);
        lease.commit().await.unwrap();
        assert_eq!(fixture.lock().unwrap().remove_calls, 1);
        server.abort();
    }

    #[tokio::test]
    async fn durable_known_final_rejection_after_lift_restarts_same_closure_without_pulse_ack() {
        if std::env::var("CATALYST_TEST_DATABASE_URL")
            .ok()
            .is_none_or(|value| value.trim().is_empty())
        {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "durable island regressions require CATALYST_TEST_DATABASE_URL in CI"
            );
            eprintln!(
                "local skip: durable island PostgreSQL fixture requires CATALYST_TEST_DATABASE_URL"
            );
            return;
        }

        let pool = crate::db::scratch::pool(8)
            .await
            .expect("mandatory disposable PostgreSQL final-phase fixture");
        let (mut state, fixture, server) = sfu_fixture(pool.clone()).await;
        state.cluster.island_token_ttl_seconds = 1;
        let actor = wallet();
        let room = format!("island-{}", Uuid::new_v4());
        fixture.lock().unwrap().rooms =
            serde_json::json!({"rooms":[{"name":room,"sid":"ISLAND-SID"}]});
        let (_, row) = platform_island_target(&state, &actor, &room).await;
        assert!(remove(&state, &row).await.is_err());
        let initial:Uuid=sqlx::query_scalar("SELECT dispatch_id FROM social.island_cleanup_dispatches WHERE wallet=$1 AND state='quarantine'").bind(&actor).fetch_one(&pool).await.unwrap();
        sqlx::query("UPDATE social.island_cleanup_dispatches SET quarantine_until=timestamp '1970-01-01' WHERE dispatch_id=$1").bind(initial).execute(&pool).await.unwrap();
        state.island_safety.elapse_quarantine_for_test(initial, 123);
        let final_attempt = state
            .island_safety
            .begin_quarantine_final(initial)
            .await
            .unwrap()
            .unwrap();
        fixture.lock().unwrap().rejections = 1;
        assert!(
            !drive_island_dispatch(&state, &final_attempt, &room, actor.as_str(), false)
                .await
                .unwrap()
        );
        state
            .moderation
            .lift_ban(&actor, "moderator")
            .await
            .unwrap();
        assert!(state.island_safety.snapshot(actor.as_str()).await.is_err());
        let (mut restarted, _, other_server) = sfu_fixture(pool.clone()).await;
        restarted.livekit = state.livekit.clone();
        restarted.cluster.island_token_ttl_seconds = 1;
        restarted.policy = Arc::new(crate::interfaces::test_support::StubPolicy::default());
        let bus = Arc::new(crate::gatekeeper::bus::InProcessBus::new());
        let restarted = restarted.with_bus(bus.clone());
        resume_island_quarantines(&restarted).await.unwrap();
        let (phase,status,root,target,generation,cutoff):(String,String,String,String,Uuid,i64)=sqlx::query_as("SELECT phase,state,intent_root_key,intent_target_key,intent_generation,cutoff FROM social.island_cleanup_dispatches WHERE wallet=$1 ORDER BY fence_generation DESC LIMIT 1").bind(&actor).fetch_one(&pool).await.unwrap();
        assert_eq!(
            (phase.as_str(), status.as_str()),
            ("quarantine_final", "confirmed")
        );
        assert_eq!(
            (root, target, generation),
            (row.root_key, row.target_key, row.generation)
        );
        let lease = restarted
            .island_safety
            .authorize(actor.as_str(), None, cutoff as u64)
            .await
            .unwrap();
        lease.commit().await.unwrap();
        assert!(
            bus.published().is_empty(),
            "enforcement never fabricates Pulse completion"
        );
        assert_eq!(fixture.lock().unwrap().remove_calls, 3);
        server.abort();
        other_server.abort();
    }
    #[tokio::test]
    async fn durable_distinct_island_target_survives_wallet_contention_without_repeating_unknown() {
        let Some(pool) = crate::db::scratch::pool(8).await else {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "mandatory PostgreSQL in CI"
            );
            eprintln!(
                "local skip: durable contention regression requires CATALYST_TEST_DATABASE_URL"
            );
            return;
        };
        let (mut state, fixture, server) = sfu_fixture(pool.clone()).await;
        state.cluster.self_hosted_token_quarantine = false;
        let actor = wallet();
        let room_a = format!("island-{}", Uuid::new_v4());
        let room_b = format!("island-{}", Uuid::new_v4());
        {
            let mut f = fixture.lock().unwrap();
            f.rooms = serde_json::json!({"rooms":[{"name":room_a,"sid":"ISLAND-SID"},{"name":room_b,"sid":"SECOND-SID"}]});
            f.failures = 1;
        }
        let (_, first) = platform_island_target(&state, &actor, &room_a).await;
        let root: Intent = sqlx::query_as(
            "SELECT * FROM social.livekit_removal_intents WHERE root_key=$1 AND target_key=''",
        )
        .bind(&first.root_key)
        .fetch_one(&pool)
        .await
        .unwrap();
        add_target(&state, &root, &room_b, "SECOND-SID", actor.as_str(), &actor)
            .await
            .unwrap();
        let second = claim(&pool, Some(std::slice::from_ref(&first.root_key)))
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.room_name.as_deref() == Some(room_b.as_str()))
            .unwrap();
        assert!(remove(&state, &first).await.is_err());
        assert_eq!(fixture.lock().unwrap().remove_calls, 1);
        assert!(
            remove(&state, &second).await.is_err(),
            "contention must back off, never finish the undispatched target"
        );
        assert!(current(&pool, &second).await.unwrap());
        failed(&pool, &second, &anyhow!("wallet cleanup busy"))
            .await
            .unwrap();
        force_due(&pool, &second.root_key).await;
        let retried = claim(&pool, Some(std::slice::from_ref(&second.root_key)))
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.target_key == second.target_key)
            .unwrap();
        assert!(remove(&state, &retried).await.is_err());
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM social.livekit_removal_intents WHERE root_key=$1 AND target_key=$2").bind(&second.root_key).bind(&second.target_key).fetch_one(&pool).await.unwrap(),1);
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM social.island_cleanup_dispatches WHERE wallet=$1 AND state='dispatched'").bind(&actor).fetch_one(&pool).await.unwrap(),1);
        assert_eq!(
            fixture.lock().unwrap().remove_calls,
            1,
            "claim retry never retries unknown RPC"
        );
        server.abort();
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn durable_distinct_island_target_runs_once_after_other_target_positively_settles() {
        if std::env::var("CATALYST_TEST_DATABASE_URL")
            .ok()
            .is_none_or(|value| value.trim().is_empty())
        {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "mandatory PostgreSQL in CI"
            );
            eprintln!(
                "local skip: durable final-boundary regression requires CATALYST_TEST_DATABASE_URL"
            );
            return;
        }

        let pool = crate::db::scratch::pool(8)
            .await
            .expect("mandatory disposable PostgreSQL target progression fixture");
        let (mut state, fixture, server) = sfu_fixture(pool.clone()).await;
        state.cluster.self_hosted_token_quarantine = false;
        let actor = wallet();
        let room_a = format!("island-{}", Uuid::new_v4());
        let room_b = format!("island-{}", Uuid::new_v4());
        let release = Arc::new(tokio::sync::Notify::new());
        let entered = {
            let mut f = fixture.lock().unwrap();
            f.rooms = serde_json::json!({"rooms":[{"name":room_a,"sid":"ISLAND-SID"},{"name":room_b,"sid":"SECOND-SID"}]});
            f.remove_pause = Some(release.clone());
            f.remove_entered.clone()
        };
        let (_, first) = platform_island_target(&state, &actor, &room_a).await;
        let root: Intent = sqlx::query_as(
            "SELECT * FROM social.livekit_removal_intents WHERE root_key=$1 AND target_key=''",
        )
        .bind(&first.root_key)
        .fetch_one(&pool)
        .await
        .unwrap();
        add_target(&state, &root, &room_b, "SECOND-SID", actor.as_str(), &actor)
            .await
            .unwrap();
        let second = claim(&pool, Some(std::slice::from_ref(&first.root_key)))
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.room_name.as_deref() == Some(room_b.as_str()))
            .unwrap();
        let state = Arc::new(state);
        let worker = state.clone();
        let first_task = tokio::spawn(async move { remove(&worker, &first).await });
        entered.notified().await;
        assert!(remove(&state, &second).await.is_err());
        assert!(current(&pool, &second).await.unwrap());
        assert_eq!(fixture.lock().unwrap().remove_calls, 1);
        fixture.lock().unwrap().remove_pause = None;
        release.notify_one();
        first_task.await.unwrap().unwrap();
        remove(&state, &second).await.unwrap();
        assert_eq!(fixture.lock().unwrap().remove_calls, 2);
        assert!(!current(&pool, &second).await.unwrap());
        server.abort();
    }

    #[tokio::test]
    async fn durable_unleased_moderation_job_survives_unknown_recovered_island_and_removes_voice_target()
     {
        if std::env::var("CATALYST_TEST_DATABASE_URL")
            .ok()
            .is_none_or(|value| value.trim().is_empty())
        {
            assert_ne!(
                std::env::var("GITHUB_ACTIONS").as_deref(),
                Ok("true"),
                "mandatory PostgreSQL in CI"
            );
            eprintln!(
                "local skip: durable final-boundary regression requires CATALYST_TEST_DATABASE_URL"
            );
            return;
        }

        let base = crate::db::scratch::pool(2)
            .await
            .expect("mandatory disposable PostgreSQL job fixture");
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_with(base.connect_options().as_ref().clone().database("postgres"))
            .await
            .unwrap();
        let name = format!("dcl_island_job_{}", Uuid::new_v4().simple());
        assert!(name.bytes().all(|v| v.is_ascii_alphanumeric() || v == b'_'));
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
            .execute(&admin)
            .await
            .unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(8)
            .connect_with(base.connect_options().as_ref().clone().database(&name))
            .await
            .unwrap();
        crate::db::apply_schema(&pool).await.unwrap();
        let (state, fixture, server) = sfu_fixture(pool.clone()).await;
        let actor = wallet();
        let island = format!("island-{}", Uuid::new_v4());
        let voice = crate::rooms::private_voice_chat_room_name(&Uuid::new_v4());
        let other = wallet();
        {
            let mut f = fixture.lock().unwrap();
            f.rooms = serde_json::json!({"rooms":[{"name":island,"sid":"ISLAND-SID"},{"name":voice,"sid":"VOICE-SID"}]});
            f.rosters.insert(
                voice.clone(),
                serde_json::json!({"participants":[{"identity":other.as_str()}]}),
            );
        }
        let (_, target) = platform_island_target(&state, &actor, &island).await;
        assert!(remove(&state, &target).await.is_err());
        let initial:Uuid=sqlx::query_scalar("SELECT dispatch_id FROM social.island_cleanup_dispatches WHERE wallet=$1 AND state='quarantine'").bind(&actor).fetch_one(&pool).await.unwrap();
        sqlx::query("UPDATE social.island_cleanup_dispatches SET quarantine_until=timestamp '1970-01-01' WHERE dispatch_id=$1").bind(initial).execute(&pool).await.unwrap();
        state.island_safety.elapse_quarantine_for_test(initial, 123);
        fixture.lock().unwrap().failures = 1;
        connection(&pool, &voice, &other).await.unwrap();
        let state = Arc::new(state);
        let jobs = Jobs::new();
        assert!(jobs.leases().is_none());
        spawn_jobs(&state, &jobs);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let report = jobs.report();
                let job = report
                    .iter()
                    .find(|r| r.name == "comms-livekit-ban-removal")
                    .unwrap();
                assert_ne!(
                    job.state,
                    crate::jobs::JobState::Failed,
                    "{:?}",
                    job.last_error
                );
                if fixture
                    .lock()
                    .unwrap()
                    .removed
                    .contains(&other.as_str().to_owned())
                    && job.passes >= 2
                {
                    assert_eq!(job.state, crate::jobs::JobState::Running);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("ordinary voice removal still progresses after island RPC uncertainty");
        assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM social.island_cleanup_dispatches WHERE wallet=$1 AND phase='quarantine_final' AND state='dispatched'").bind(&actor).fetch_one(&pool).await.unwrap(),1);
        assert!(state.island_safety.snapshot(actor.as_str()).await.is_err());
        assert_eq!(fixture.lock().unwrap().remove_calls, 3);
        jobs.shutdown(Duration::from_secs(2)).await;
        server.abort();
        drop(state);
        pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE \"{name}\" WITH (FORCE)"
        )))
        .execute(&admin)
        .await
        .unwrap();
        admin.close().await;
    }
    #[tokio::test]
    async fn durable_vanished_final_room_requires_creation_and_new_positive_cutoff() {
        assert!(
            std::env::var("CATALYST_TEST_DATABASE_URL").is_ok(),
            "real PostgreSQL required"
        );
        for create_fails in [false, true] {
            let (pool, admin, db_name) = liveness_isolated_pool().await;
            let (state, fixture, server) = sfu_fixture(pool.clone()).await;
            let actor = wallet();
            let room = format!("island-{}", Uuid::new_v4());
            fixture.lock().unwrap().rooms =
                serde_json::json!({"rooms":[{"name":room,"sid":"ISLAND-SID"}]});
            let (_, row) = platform_island_target(&state, &actor, &room).await;
            assert!(remove(&state, &row).await.is_err());
            let initial:Uuid=sqlx::query_scalar("SELECT dispatch_id FROM social.island_cleanup_dispatches WHERE wallet=$1 AND state='quarantine'").bind(&actor).fetch_one(&pool).await.unwrap();
            sqlx::query("UPDATE social.island_cleanup_dispatches SET quarantine_until=timestamp '1970-01-01' WHERE dispatch_id=$1").bind(initial).execute(&pool).await.unwrap();
            state.island_safety.elapse_quarantine_for_test(initial, 123);
            state
                .moderation
                .lift_ban(&actor, "moderator")
                .await
                .unwrap();
            {
                let mut f = fixture.lock().unwrap();
                f.missing_room = true;
                f.create_failures = usize::from(create_fails);
            }
            // Actual primary callback is the sole final-attempt runner.
            run_batch(&state).await.unwrap();
            if create_fails {
                assert!(state.island_safety.snapshot(actor.as_str()).await.is_err());
                assert_eq!(fixture.lock().unwrap().create_calls, 1);
                run_batch(&state).await.unwrap();
            }
            let (status,phase,root,target,generation,cutoff):(String,String,String,String,Uuid,i64)=sqlx::query_as("SELECT state,phase,intent_root_key,intent_target_key,intent_generation,cutoff FROM social.island_cleanup_dispatches WHERE wallet=$1 ORDER BY fence_generation DESC LIMIT 1").bind(&actor).fetch_one(&pool).await.unwrap();
            assert_eq!(
                (status.as_str(), phase.as_str()),
                ("confirmed", "quarantine_final")
            );
            assert_eq!(
                (root, target, generation),
                (row.root_key, row.target_key, row.generation)
            );
            assert_eq!(
                fixture.lock().unwrap().create_calls,
                if create_fails { 2 } else { 1 }
            );
            let calls = fixture.lock().unwrap().remove_calls;
            assert_eq!(calls, if create_fails { 4 } else { 3 });
            let (mut restart, _, other_server) = sfu_fixture(pool.clone()).await;
            restart.livekit = state.livekit.clone();
            run_batch(&restart).await.unwrap();
            run_batch(&restart).await.unwrap();
            assert_eq!(fixture.lock().unwrap().remove_calls, calls);
            let lease = restart
                .island_safety
                .authorize(actor.as_str(), None, cutoff as u64)
                .await
                .unwrap();
            assert_eq!(lease.snapshot.token_floor, cutoff as u64);
            lease.commit().await.unwrap();
            assert_eq!(sqlx::query_scalar::<_,i64>("SELECT count(*) FROM social.island_cleanup_dispatches WHERE dispatch_id=$1 AND state='superseded' AND settled_at IS NOT NULL").bind(initial).fetch_one(&pool).await.unwrap(),1);
            server.abort();
            other_server.abort();
            liveness_drop_pool(&pool, &admin, &db_name).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn durable_contended_quarantine_wallet_does_not_starve_ordinary_primary_job() {
        assert!(
            std::env::var("CATALYST_TEST_DATABASE_URL").is_ok(),
            "real PostgreSQL required"
        );
        let (pool, admin, db_name) = liveness_isolated_pool().await;
        let (state, fixture, server) = sfu_fixture(pool.clone()).await;
        let actor = wallet();
        let room = format!("island-{}", Uuid::new_v4());
        fixture.lock().unwrap().rooms =
            serde_json::json!({"rooms":[{"name":room,"sid":"ISLAND-SID"}]});
        let (_, row) = platform_island_target(&state, &actor, &room).await;
        assert!(remove(&state, &row).await.is_err());
        let id:Uuid=sqlx::query_scalar("SELECT dispatch_id FROM social.island_cleanup_dispatches WHERE wallet=$1 AND state='quarantine'").bind(&actor).fetch_one(&pool).await.unwrap();
        sqlx::query("UPDATE social.island_cleanup_dispatches SET quarantine_until=timestamp '1970-01-01' WHERE dispatch_id=$1").bind(id).execute(&pool).await.unwrap();
        state.island_safety.elapse_quarantine_for_test(id, 123);
        state
            .moderation
            .lift_ban(&actor, "moderator")
            .await
            .unwrap();
        let voice = crate::rooms::private_voice_chat_room_name(&Uuid::new_v4());
        let other = wallet();
        fixture.lock().unwrap().rooms = serde_json::json!({"rooms":[{"name":voice,"sid":"VOICE"}]});
        fixture.lock().unwrap().rosters.insert(
            voice.clone(),
            serde_json::json!({"participants":[{"identity":other.as_str()}]}),
        );
        connection(&pool, &voice, &other).await.unwrap();
        let mut blocker = pool.begin().await.unwrap();
        sqlx::query("SELECT wallet FROM social.island_admission_fences WHERE wallet=$1 FOR UPDATE")
            .bind(&actor)
            .execute(&mut *blocker)
            .await
            .unwrap();
        let state = Arc::new(state);
        let jobs = Jobs::new();
        spawn_jobs(&state, &jobs);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                assert!(
                    jobs.report()
                        .iter()
                        .all(|r| r.state != crate::jobs::JobState::Failed),
                    "wallet contention must not fail whole job"
                );
                if fixture
                    .lock()
                    .unwrap()
                    .removed
                    .contains(&other.as_str().to_owned())
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("ordinary voice RPC despite held island wallet");
        assert_eq!(
            sqlx::query_scalar::<_, String>(
                "SELECT state FROM social.island_cleanup_dispatches WHERE dispatch_id=$1"
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap(),
            "quarantine"
        );
        blocker.rollback().await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if state.island_safety.snapshot(actor.as_str()).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("next maintenance progresses");
        jobs.shutdown(Duration::from_secs(2)).await;
        server.abort();
        liveness_drop_pool(&pool, &admin, &db_name).await;
    }

    #[tokio::test]
    async fn durable_initial_unknown_never_creates_missing_final_room() {
        let (pool, admin, name) = liveness_isolated_pool().await;
        let (state, fixture, server) = sfu_fixture(pool.clone()).await;
        let actor = wallet();
        let room = format!("island-{}", Uuid::new_v4());
        fixture.lock().unwrap().rooms =
            serde_json::json!({"rooms":[{"name":room,"sid":"ISLAND-SID"}]});
        let (_, row) = platform_island_target(&state, &actor, &room).await;
        fixture.lock().unwrap().failures = 1;
        assert!(remove(&state, &row).await.is_err());
        state
            .moderation
            .lift_ban(&actor, "moderator")
            .await
            .unwrap();
        fixture.lock().unwrap().missing_room = true;
        run_batch(&state).await.unwrap();
        run_batch(&state).await.unwrap();
        {
            let f = fixture.lock().unwrap();
            assert_eq!((f.remove_calls, f.create_calls), (1, 0));
        }
        assert!(state.island_safety.snapshot(actor.as_str()).await.is_err());
        server.abort();
        liveness_drop_pool(&pool, &admin, &name).await;
    }

    #[tokio::test]
    async fn durable_selection_outage_still_attempts_ordinary_intent_before_reporting_error() {
        let (pool, admin, name) = liveness_isolated_pool().await;
        let (mut state, fixture, server) = sfu_fixture(pool.clone()).await;
        let voice = crate::rooms::private_voice_chat_room_name(&Uuid::new_v4());
        let actor = wallet();
        fixture.lock().unwrap().rooms = serde_json::json!({"rooms":[{"name":voice,"sid":"VOICE"}]});
        fixture.lock().unwrap().rosters.insert(
            voice.clone(),
            serde_json::json!({"participants":[{"identity":actor.as_str()}]}),
        );
        connection(&pool, &voice, &actor).await.unwrap();
        // Test-only failure injection at the actual maintenance dependency.
        // Ordinary policy/intent SQL retains the original available pool.
        let unavailable = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy_with(pool.connect_options().as_ref().clone());
        unavailable.close().await;
        state.island_safety =
            crate::gatekeeper::admission::cluster::safety::SafetyStore::new(unavailable);
        assert!(run_batch(&state).await.is_err());
        assert!(run_batch(&state).await.is_err());
        assert!(
            fixture
                .lock()
                .unwrap()
                .removed
                .contains(&actor.as_str().to_owned())
        );
        server.abort();
        liveness_drop_pool(&pool, &admin, &name).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn durable_contended_compaction_wallet_does_not_starve_ordinary_primary_job() {
        let (pool, admin, name) = liveness_isolated_pool().await;
        let (mut state, fixture, server) = sfu_fixture(pool.clone()).await;
        state.cluster.self_hosted_token_quarantine = false;
        let actor = wallet();
        let room = format!("island-{}", Uuid::new_v4());
        fixture.lock().unwrap().rooms =
            serde_json::json!({"rooms":[{"name":room,"sid":"ISLAND-SID"}]});
        let (_, row) = platform_island_target(&state, &actor, &room).await;
        remove(&state, &row).await.unwrap();
        let id:Uuid=sqlx::query_scalar("SELECT dispatch_id FROM social.island_cleanup_dispatches WHERE wallet=$1 AND state='confirmed'").bind(&actor).fetch_one(&pool).await.unwrap();
        let floor = state.island_safety.snapshot(actor.as_str()).await.unwrap();
        state
            .moderation
            .lift_ban(&actor, "moderator")
            .await
            .unwrap();
        // Test-only aging of known-positive history; floor remains the real cutoff.
        sqlx::query("UPDATE social.island_cleanup_dispatches SET cutoff=100,settled_at=timestamp '1970-01-01' WHERE dispatch_id=$1").bind(id).execute(&pool).await.unwrap();
        let voice = crate::rooms::private_voice_chat_room_name(&Uuid::new_v4());
        let other = wallet();
        fixture.lock().unwrap().rooms = serde_json::json!({"rooms":[{"name":voice,"sid":"VOICE"}]});
        fixture.lock().unwrap().rosters.insert(
            voice.clone(),
            serde_json::json!({"participants":[{"identity":other.as_str()}]}),
        );
        connection(&pool, &voice, &other).await.unwrap();
        let mut blocker = pool.begin().await.unwrap();
        sqlx::query("SELECT wallet FROM social.island_admission_fences WHERE wallet=$1 FOR UPDATE")
            .bind(&actor)
            .execute(&mut *blocker)
            .await
            .unwrap();
        let state = Arc::new(state);
        let jobs = Jobs::new();
        spawn_jobs(&state, &jobs);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                assert!(
                    jobs.report()
                        .iter()
                        .all(|r| r.state != crate::jobs::JobState::Failed)
                );
                if fixture
                    .lock()
                    .unwrap()
                    .removed
                    .contains(&other.as_str().to_owned())
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("ordinary voice despite contended compaction wallet");
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM social.island_cleanup_dispatches WHERE dispatch_id=$1"
            )
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap(),
            1
        );
        blocker.rollback().await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if sqlx::query_scalar::<_, i64>(
                    "SELECT count(*) FROM social.island_cleanup_dispatches WHERE dispatch_id=$1",
                )
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap()
                    == 0
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("next bounded maintenance compacts released wallet");
        assert_eq!(
            state.island_safety.snapshot(actor.as_str()).await.unwrap(),
            floor
        );
        jobs.shutdown(Duration::from_secs(2)).await;
        server.abort();
        liveness_drop_pool(&pool, &admin, &name).await;
    }

    async fn liveness_isolated_pool() -> (sqlx::PgPool, sqlx::PgPool, String) {
        let base = crate::db::scratch::pool(2).await.expect("real PostgreSQL");
        let admin = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect_with(base.connect_options().as_ref().clone().database("postgres"))
            .await
            .unwrap();
        let name = format!("dcl_liveness_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
            .execute(&admin)
            .await
            .unwrap();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(8)
            .connect_with(base.connect_options().as_ref().clone().database(&name))
            .await
            .unwrap();
        crate::db::apply_schema(&pool).await.unwrap();
        (pool, admin, name)
    }
    async fn liveness_drop_pool(pool: &sqlx::PgPool, admin: &sqlx::PgPool, name: &str) {
        assert!(
            name.starts_with("dcl_liveness_")
                && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        );
        pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE \"{name}\" WITH (FORCE)"
        )))
        .execute(admin)
        .await
        .unwrap();
        admin.close().await;
    }
}
async fn begin_island_dispatch(
    state: &CommsState,
    row: &Intent,
    actor: &Address,
    room: &str,
    identity: &str,
) -> Result<Option<crate::gatekeeper::admission::cluster::safety::Dispatch>> {
    use crate::gatekeeper::admission::cluster::safety::SafetyStore;
    // All managed island JWTs use the canonical wallet identity. Do not send
    // an untracked name-only RPC for an unexpected identity in an island room.
    if identity != actor.as_str() {
        return Err(anyhow!("noncanonical island participant identity"));
    }
    let blocked = if matches!(row.cause_kind.as_str(), "connection" | "block") {
        state
            .policy
            .is_blocked(actor)
            .await
            .map_err(|_| anyhow!("operator block authority unavailable"))?
    } else {
        false
    };
    let store = &state.island_safety;
    let mut lease = store
        .lock(actor.as_str())
        .await
        .map_err(|_| anyhow!("island safety unavailable"))?;
    let latest:Option<Uuid>=sqlx::query_scalar("SELECT dispatch_id FROM social.island_cleanup_dispatches WHERE purpose='island_enforcement' AND wallet=$1 AND intent_root_key=$2 AND intent_target_key=$3 AND intent_generation=$4 AND room_name=$5 AND participant_identity=$6 ORDER BY fence_generation DESC LIMIT 1")
        .bind(actor).bind(&row.root_key).bind(&row.target_key).bind(row.generation).bind(room).bind(identity)
        .fetch_optional(&mut *lease.tx).await?;
    let mut existing_obligation = false;
    if let Some(id) = latest {
        let dispatch = SafetyStore::dispatch(&mut lease.tx, id)
            .await
            .map_err(|_| anyhow!("invalid island dispatch"))?;
        existing_obligation = dispatch.state == DispatchState::SettledLate;
        if matches!(
            dispatch.state,
            DispatchState::Dispatched | DispatchState::Quarantine | DispatchState::Confirmed
        ) {
            lease
                .commit()
                .await
                .map_err(|_| anyhow!("island safety commit unavailable"))?;
            return Ok(Some(dispatch));
        }
    }
    // Wallet -> existing root -> actual target. A discovery root may already
    // have finished while its captured targets remain; the target row lock
    // still serializes cancellation/lift before dispatch-marker commit.
    let root:Option<Uuid>=sqlx::query_scalar("SELECT generation FROM social.livekit_removal_intents WHERE root_key=$1 AND target_key='' FOR UPDATE")
        .bind(&row.root_key).fetch_optional(&mut *lease.tx).await?;
    if root.is_some_and(|generation| generation != row.generation) {
        return Ok(None);
    }
    let valid:Option<Uuid>=sqlx::query_scalar("SELECT generation FROM social.livekit_removal_intents WHERE root_key=$1 AND target_key=$2 AND generation=$3 AND claim_id=$4 AND claim_until>timezone('UTC',clock_timestamp()) AND room_name=$5 AND participant_identity=$6 AND actor=$7 FOR UPDATE")
        .bind(&row.root_key).bind(&row.target_key).bind(row.generation).bind(row.claim_id).bind(room).bind(identity).bind(actor)
        .fetch_optional(&mut *lease.tx).await?;
    if valid.is_none() {
        return Ok(None);
    }
    let active=match row.cause_kind.as_str() {
        "platform"=>sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM social.user_bans b WHERE b.id=$1 AND (b.banned_address=$2 OR b.banned_device_id=(SELECT NULLIF(device_id,'') FROM social.player_connections WHERE address=$2)) AND b.lifted_at IS NULL AND (b.expires_at IS NULL OR b.expires_at>timezone('UTC',clock_timestamp())))")
            .bind(row.ban_id).bind(actor).fetch_one(&mut *lease.tx).await?,
        "scene"=>sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM social.scene_bans WHERE place_id=$1 AND banned_address=$2 AND banned_at=$3)")
            .bind(&row.place_id).bind(actor).bind(row.scene_version).fetch_one(&mut *lease.tx).await?,
        "block"=>blocked,
        "connection"=>blocked || sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM social.user_bans b WHERE (b.banned_address=$1 OR b.banned_device_id=(SELECT NULLIF(device_id,'') FROM social.player_connections WHERE address=$1)) AND b.lifted_at IS NULL AND (b.expires_at IS NULL OR b.expires_at>timezone('UTC',clock_timestamp())))")
            .bind(actor).fetch_one(&mut *lease.tx).await?,
        _=>false,
    };
    if !active {
        return Ok(None);
    }
    // New enforcement consumes one logical slot under wallet -> capacity
    // lock order. A positively established existing obligation has credit.
    if !existing_obligation {
        store
            .capacity(&mut lease.tx)
            .await
            .map_err(|_| anyhow!("island safety capacity exhausted"))?;
    }
    let dispatch = store
        .insert_dispatch(
            &mut lease,
            actor.as_str(),
            room,
            identity,
            0,
            latest,
            "island_enforcement",
            None,
            None,
            Some((
                &row.cause_kind,
                &row.root_key,
                &row.target_key,
                row.generation,
            )),
        )
        .await
        .map_err(|_| anyhow!("island cleanup dispatch refused"))?;
    lease
        .commit()
        .await
        .map_err(|_| anyhow!("island dispatch commit unavailable"))?;
    dispatch
        .map(Some)
        .ok_or_else(|| anyhow!("another island cleanup is active for wallet"))
}

#[derive(Debug, thiserror::Error)]
#[error("island enforcement dispatch uncertain")]
struct IslandDispatchUncertain;

async fn drive_island_dispatch(
    state: &CommsState,
    dispatch: &crate::gatekeeper::admission::cluster::safety::Dispatch,
    room: &str,
    identity: &str,
    connection: bool,
) -> Result<bool> {
    use crate::gatekeeper::admission::cluster::safety::{
        QuarantinePolicy, RemovalOutcome, Settlement, classify_removal,
    };
    let mut dispatch = dispatch.clone();
    let service = state.room_service();
    for repair in 0..2 {
        let call =
            service.remove_participant_with_cutoff(room, identity, Some(dispatch.cutoff as i64));
        let outcome = if connection {
            tokio::time::timeout(Duration::from_secs(2), call)
                .await
                .map_err(|_| IslandDispatchUncertain)?
        } else {
            call.await
        };
        let classified = match outcome {
            Ok(_) => RemovalOutcome::Positive,
            Err(RoomServiceError::RemovalRejected | RoomServiceError::RemovalRoomMissing) => {
                RemovalOutcome::Rejected
            }
            Err(_) => RemovalOutcome::Unknown,
        };
        let quarantine = if state.cluster.self_hosted_token_quarantine {
            QuarantinePolicy::Enabled { applies: true }
        } else {
            QuarantinePolicy::Disabled
        };
        let settlement = classify_removal(
            classified,
            dispatch.phase,
            quarantine,
            chrono::Utc::now().timestamp_millis().max(0) as u64,
            dispatch.cutoff,
        )
        .ok_or(IslandDispatchUncertain)?;
        if !matches!(
            state
                .island_safety
                .settle(dispatch.dispatch_id, settlement)
                .await,
            Ok(true)
        ) {
            return Err(anyhow!("island settlement was not recorded"));
        }
        if matches!(outcome, Err(RoomServiceError::RemovalRoomMissing))
            && dispatch.phase == DispatchPhase::QuarantineFinal
            && repair == 0
        {
            // Definite absence is recorded, never treated as revocation. Establish
            // the captured room and require a NEW immutable positive final cutoff.
            service.ensure_cutoff_room(room).await.map_err(sfu)?;
            let Some(next) = state
                .island_safety
                .resume_settled_enforcement(dispatch.dispatch_id)
                .await
                .map_err(|_| anyhow!("island room repair unavailable"))?
            else {
                return Ok(false);
            };
            dispatch = next;
            continue;
        }
        return Ok(matches!(settlement, Settlement::Confirmed));
    }
    Ok(false)
}
async fn remove_island(
    state: &CommsState,
    row: &Intent,
    actor: &Address,
    room: &str,
    identity: &str,
) -> Result<()> {
    let Some(mut dispatch) = begin_island_dispatch(state, row, actor, room, identity).await? else {
        return finish(&state.pool, row).await.map_err(Into::into);
    };
    if dispatch.state == DispatchState::Confirmed {
        return finish(&state.pool, row).await.map_err(Into::into);
    }
    if dispatch.state == DispatchState::Quarantine {
        let Some(final_dispatch) = state
            .island_safety
            .begin_quarantine_final(dispatch.dispatch_id)
            .await
            .map_err(|_| anyhow!("island quarantine unavailable"))?
        else {
            return Err(anyhow!("island positive settlement quarantine pending"));
        };
        dispatch = final_dispatch;
    }
    if !dispatch.fresh {
        return Err(anyhow!(
            "island dispatched outcome requires controlled maintenance"
        ));
    }
    if drive_island_dispatch(
        state,
        &dispatch,
        room,
        identity,
        row.cause_kind == "connection",
    )
    .await?
    {
        finish(&state.pool, row).await?;
        return Ok(());
    }
    Err(anyhow!("island cleanup remains admission closed"))
}

async fn resume_island_quarantines(state: &CommsState) -> Result<()> {
    // This primary-only Jobs callback preserves positively settled cleanup
    // after intent lift/delete. It never selects unknown dispatched work.
    let due:Vec<(Uuid,String,String,String,String)>=sqlx::query_as("SELECT dispatch_id,room_name,participant_identity,enforcement_cause,state FROM social.island_cleanup_dispatches WHERE purpose='island_enforcement' AND settled_at IS NOT NULL AND ((state='quarantine' AND quarantine_until<=timezone('UTC',clock_timestamp())) OR state='settled_late') ORDER BY settled_at,dispatch_id LIMIT 16")
        .fetch_all(&state.pool).await?;
    for (id, room, identity, cause, status) in due {
        let result = async {
            let next = if status == "quarantine" {
                state.island_safety.begin_quarantine_final(id).await
            } else {
                state.island_safety.resume_settled_enforcement(id).await
            };
            if let Some(dispatch) =
                next.map_err(|_| anyhow!("island known-settled recovery unavailable"))?
            {
                drive_island_dispatch(state, &dispatch, &room, &identity, cause == "connection")
                    .await?;
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        if let Err(error) = result {
            metrics::counter!("comms_island_cleanup_maintenance_failed_total").increment(1);
            if error.is::<IslandDispatchUncertain>() {
                metrics::counter!("comms_island_cleanup_uncertain_total").increment(1);
            }
            tracing::warn!(dispatch_id=%id,%error,"island cleanup maintenance failed; admission remains closed");
        }
    }
    Ok(())
}
