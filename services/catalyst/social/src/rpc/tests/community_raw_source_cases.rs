//! Exact Redis-key spelling survives domain actions, cache-reader rebuilds and
//! polling. Media/membership still use the canonical UUID. Source dependencies
//! and target media are controlled; SQL and both target stream parsers are real.
use super::*;
use crate::rpc::service::ops;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

struct Observer {
    address: String,
    raw: broadcast::Receiver<Arc<SocialEvent>>,
    rpc: Generator<CommunityVoiceChatUpdate>,
    stop: CancellationToken,
}
impl Drop for Observer {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
impl Observer {
    async fn open(ctx: &Context, address: &str, transport: u32) -> Self {
        let stop = CancellationToken::new();
        let rpc = SocialServiceImpl
            .subscribe_to_community_voice_chat_updates(
                procedure_ctx(ctx, address, transport),
                stop.clone(),
            )
            .await
            .unwrap();
        Self {
            address: address.into(),
            raw: ctx.pubsub().subscribe(address),
            rpc,
            stop,
        }
    }
}
struct Observed {
    phase: String,
    address: String,
    rpc: CommunityVoiceChatUpdate,
    json: Value,
}
async fn collect(
    ctx: &Context,
    observers: &mut [Observer],
    phase: &str,
    trace: &mut Vec<Observed>,
) {
    for observer in observers {
        while let Ok(event) = observer.raw.try_recv() {
            let SocialEvent::CommunityVoice(value) = event.as_ref() else {
                continue;
            };
            let rpc = next_update(&mut observer.rpc).await;
            assert_eq!(&rpc, value, "{phase}: real RPC generator preserves payload");
            let json = crate::rpc::stream::captured_event_data(
                ctx,
                &observer.address,
                "community-voice",
                &event,
            )
            .await
            .unwrap();
            trace.push(Observed {
                phase: phase.into(),
                address: observer.address.clone(),
                rpc,
                json,
            });
        }
    }
}
async fn observers(ctx: &Context, member: &str, guest: &str) -> Vec<Observer> {
    vec![
        Observer::open(ctx, member, 91).await,
        Observer::open(ctx, guest, 92).await,
    ]
}
async fn check_read(ctx: &Context, case: &Value, step: &str) {
    let expected = case["reads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|read| read["step"] == step)
        .unwrap();
    let mut actual: Vec<(String, String)> = ctx
        .db()
        .voice_notifications_page("")
        .await
        .unwrap()
        .into_iter()
        .filter(|entry| entry.fresh)
        .map(|entry| {
            (
                entry.raw_community,
                if entry.scope == Scope::All {
                    "all".into()
                } else {
                    "members".into()
                },
            )
        })
        .collect();
    let mut expected: Vec<(String, String)> = expected["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry["communityId"].as_str().unwrap().into(),
                entry["notificationScope"].as_str().unwrap().into(),
            )
        })
        .collect();
    actual.sort();
    expected.sort();
    assert_eq!(actual, expected, "{}: {step}", case["scenario"]);
}

#[tokio::test]
async fn raw_alias_lifecycles_match_source_rpc_json_audiences_and_restart_reads() {
    let fixture: Value = serde_json::from_str(include_str!(concat!(env!("DCL_ONE_ROOT"), "/services/catalyst/tests/fixtures/conformance/social/community-raw-lifecycle-source-cases.json"))).unwrap();
    assert_eq!(fixture["cases"].as_array().unwrap().len(), 4);
    assert_eq!(
        Settings::default().community_voice_chat_polling_job_interval_ms,
        fixture["polling_job"]["interval_ms"].as_u64().unwrap()
    );
    let caller = "0x1111111111111111111111111111111111111111";
    let member = "0x2222222222222222222222222222222222222222";
    let guest = "0x5555555555555555555555555555555555555555";
    let raw = "a3333333-b333-4c33-8d33-e33333333333";
    let upper = raw.to_uppercase();
    let cid: Uuid = raw.parse().unwrap();
    for case in fixture["cases"].as_array().unwrap() {
        let Some(database) =
            OwnedDatabase::new("fanout", crate::db::scratch::Schema::Full, 8).await
        else {
            return;
        };
        let pool = database.pool.clone();
        sqlx::query(
            "INSERT INTO social.communities(id,name,owner_address) VALUES($1,'Community',$2)",
        )
        .bind(cid)
        .bind(caller)
        .execute(&pool)
        .await
        .unwrap();
        for (address, role) in [(caller, "owner"), (member, "member")] {
            sqlx::query("INSERT INTO social.community_members(community_id,member_address,role) VALUES($1,$2,$3)").bind(cid).bind(address).bind(role).execute(&pool).await.unwrap();
        }
        let voice = Arc::new(FakeVoiceRooms::new(pool.clone()));
        let mut ctx = test_ctx(Db::new(pool.clone()), voice.clone());
        let mut watchers = observers(&ctx, member, guest).await;
        let mut trace = Vec::new();
        ops::start_community_voice_chat(&ctx, caller, &upper, false)
            .await
            .unwrap();
        collect(&ctx, &mut watchers, "start-upper", &mut trace).await;
        let original = ctx.db().voice_notification(&upper).await.unwrap().unwrap();
        ctx = test_ctx(Db::new(pool.clone()), voice.clone());
        watchers = observers(&ctx, member, guest).await;
        check_read(&ctx, case, "restart-after-start").await;
        let restarted = ctx.db().voice_notification(&upper).await.unwrap().unwrap();
        assert_eq!(
            (original.generation, original.expires_at),
            (restarted.generation, restarted.expires_at),
            "rebuild preserves exact stored facts"
        );
        ctx.poll_community_voice_notifications().await;
        collect(&ctx, &mut watchers, "active-poll", &mut trace).await;
        let refreshed = ctx.db().voice_notification(&upper).await.unwrap().unwrap();
        assert_eq!(refreshed.generation, original.generation);
        assert_eq!(refreshed.scope, original.scope);
        assert!(refreshed.expires_at >= original.expires_at);
        let scenario = case["scenario"].as_str().unwrap();
        if scenario == "uppercase-start-lowercase-end" {
            ops::end_community_voice_chat(&ctx, caller, raw)
                .await
                .unwrap();
            collect(&ctx, &mut watchers, "end-lower", &mut trace).await;
            ctx = test_ctx(Db::new(pool.clone()), voice.clone());
            watchers = observers(&ctx, member, guest).await;
            check_read(&ctx, case, "restart-after-end-other-spelling").await;
            assert_eq!(
                ctx.db()
                    .voice_notification(&upper)
                    .await
                    .unwrap()
                    .unwrap()
                    .generation,
                original.generation
            );
        } else {
            if scenario != "two-alias-cache-entries" {
                sqlx::query("UPDATE social.communities SET private=true WHERE id=$1")
                    .bind(cid)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            voice.live.lock().clear();
            ops::start_community_voice_chat(&ctx, caller, raw, false)
                .await
                .unwrap();
            collect(&ctx, &mut watchers, "start-lower", &mut trace).await;
            ctx = test_ctx(Db::new(pool.clone()), voice.clone());
            watchers = observers(&ctx, member, guest).await;
            check_read(&ctx, case, "restart-two-aliases").await;
            let lower = ctx.db().voice_notification(raw).await.unwrap().unwrap();
            assert_ne!(lower.generation, original.generation);
            if scenario != "aliases-own-scope-and-expiry" {
                ctx.poll_community_voice_notifications().await;
                collect(&ctx, &mut watchers, "active-sibling-poll", &mut trace).await;
                check_read(&ctx, case, "after-active-sibling-poll").await;
                assert_eq!(
                    ctx.db()
                        .voice_notification(raw)
                        .await
                        .unwrap()
                        .unwrap()
                        .generation,
                    lower.generation
                );
                assert_eq!(
                    ctx.db()
                        .voice_notification(&upper)
                        .await
                        .unwrap()
                        .unwrap()
                        .generation,
                    original.generation
                );
            }
            if scenario == "aliases-own-scope-and-expiry" {
                sqlx::query("UPDATE social.community_voice_notifications SET expires_at=timezone('UTC',now())+CASE WHEN raw_community_id=$1 THEN interval '-1 second' ELSE interval '1 hour' END WHERE community_id=$2").bind(&upper).bind(cid).execute(&pool).await.unwrap();
                check_read(&ctx, case, "upper-expired-lower-fresh").await;
            }
            if scenario == "explicit-end-restart-keeps-other-alias" {
                ops::end_community_voice_chat(&ctx, caller, &upper)
                    .await
                    .unwrap();
                collect(&ctx, &mut watchers, "end-upper", &mut trace).await;
                check_read(&ctx, case, "after-upper-end").await;
                assert_eq!(
                    ctx.db()
                        .voice_notification(raw)
                        .await
                        .unwrap()
                        .unwrap()
                        .generation,
                    lower.generation
                );
                ops::start_community_voice_chat(&ctx, caller, &upper, false)
                    .await
                    .unwrap();
                collect(&ctx, &mut watchers, "restart-upper", &mut trace).await;
                assert_ne!(
                    ctx.db()
                        .voice_notification(&upper)
                        .await
                        .unwrap()
                        .unwrap()
                        .generation,
                    original.generation
                );
                ctx = test_ctx(Db::new(pool.clone()), voice.clone());
                watchers = observers(&ctx, member, guest).await;
                check_read(&ctx, case, "restart-after-new-upper-start").await;
            }
            voice.live.lock().clear();
            if scenario == "explicit-end-restart-keeps-other-alias" {
                use crate::comms::interfaces::SocialDirectory;
                let directory = crate::comms_directory::Directory::new(pool.clone());
                directory
                    .community_voice_room_closed(crate::CommunityId::from_uuid(cid))
                    .await;
                check_read(&ctx, case, "media-closed-before-poll").await;
                let before = trace.len();
                collect(&ctx, &mut watchers, "media-closed-before-poll", &mut trace).await;
                assert_eq!(trace.len(), before, "no eager UUID close event");
            }
        }
        ctx.poll_community_voice_notifications().await;
        collect(&ctx, &mut watchers, "inactive-poll", &mut trace).await;
        check_read(&ctx, case, "after-inactive-poll").await;
        assert!(
            ctx.db()
                .voice_notifications_page("")
                .await
                .unwrap()
                .is_empty()
        );
        ctx.poll_community_voice_notifications().await;
        collect(&ctx, &mut watchers, "second-inactive-poll", &mut trace).await;
        let expected = case["wire"].as_array().unwrap();
        assert_eq!(
            trace.len(),
            expected.len(),
            "{scenario}: exact recipient count"
        );
        for event in &trace {
            let source = expected
                .iter()
                .find(|source| {
                    source["phase"] == event.phase
                        && source["address"] == event.address
                        && source["rpc"]["communityId"] == event.rpc.community_id
                        && source["rpc"]["status"] == event.rpc.status
                })
                .unwrap_or_else(|| {
                    panic!(
                        "{scenario}: unexpected {} {} {}",
                        event.phase, event.address, event.rpc.community_id
                    )
                });
            let expected = CommunityVoiceChatUpdate::decode(
                hex::decode(source["rpc_hex"].as_str().unwrap())
                    .unwrap()
                    .as_slice(),
            )
            .unwrap();
            let mut actual = event.rpc.clone();
            assert!(actual.created_at > 0);
            assert_eq!(actual.ended_at.is_some(), expected.ended_at.is_some());
            actual.created_at = expected.created_at;
            actual.ended_at = expected.ended_at;
            assert_eq!(actual, expected, "{scenario}: {} RPC", event.phase);
            let mut actual = event.json.clone();
            actual["createdAt"] = source["json"]["createdAt"].clone();
            if actual.get("endedAt").is_some() {
                actual["endedAt"] = source["json"]["endedAt"].clone();
            }
            assert_eq!(actual, source["json"], "{scenario}: {} JSON", event.phase);
        }
        drop(watchers);
        database.drop().await;
    }
}

#[tokio::test]
async fn exact_key_cas_restore_and_pages_preserve_sibling_generations_and_ttls() {
    let Some(database) = OwnedDatabase::new("fanout", crate::db::scratch::Schema::Full, 8).await
    else {
        return;
    };
    let pool = &database.pool;
    let cid: Uuid = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".parse().unwrap();
    sqlx::query(
        "INSERT INTO social.communities(id,name,owner_address) VALUES($1,'aliases','owner')",
    )
    .bind(cid)
    .execute(pool)
    .await
    .unwrap();
    let db = Db::new(pool.clone());
    let lower = cid.to_string();
    let upper = lower.to_uppercase();
    db.record_voice_notification(cid, &lower, Scope::Members)
        .await
        .unwrap();
    db.record_voice_notification(cid, &upper, Scope::All)
        .await
        .unwrap();
    sqlx::query("UPDATE social.community_voice_notifications SET expires_at=timezone('UTC',now())+CASE WHEN raw_community_id=$1 THEN interval '1 hour' ELSE interval '2 hours' END WHERE community_id=$2").bind(&upper).bind(cid).execute(pool).await.unwrap();
    let first = db.voice_notification(&upper).await.unwrap().unwrap();
    let sibling = db.voice_notification(&lower).await.unwrap().unwrap();
    assert_ne!(first.expires_at, sibling.expires_at);
    assert!(db.clear_voice_notification(&first).await.unwrap());
    let replacement = db
        .record_voice_notification(cid, &upper, Scope::Members)
        .await
        .unwrap();
    assert!(!db.clear_voice_notification(&first).await.unwrap());
    assert!(!db.refresh_voice_notification(&first).await.unwrap());
    db.restore_voice_notification(&first).await.unwrap();
    let current = db.voice_notification(&upper).await.unwrap().unwrap();
    assert_eq!(current.generation, replacement);
    assert_eq!(current.scope, Scope::Members);
    let retained = db.voice_notification(&lower).await.unwrap().unwrap();
    assert_eq!(
        (retained.generation, retained.expires_at, retained.scope),
        (sibling.generation, sibling.expires_at, sibling.scope)
    );
    assert!(db.clear_voice_notification(&sibling).await.unwrap());
    db.restore_voice_notification(&sibling).await.unwrap();
    let restored = db.voice_notification(&lower).await.unwrap().unwrap();
    assert_eq!(
        (restored.generation, restored.expires_at, restored.scope),
        (sibling.generation, sibling.expires_at, sibling.scope)
    );
    // Both interleavings leave the new generation, including a late restore.
    for _ in 0..8 {
        let previous = db.voice_notification(&upper).await.unwrap().unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let update = async {
            barrier.wait().await;
            db.record_voice_notification(cid, &upper, Scope::All)
                .await
                .unwrap()
        };
        let cleanup = async {
            barrier.wait().await;
            db.clear_voice_notification(&previous).await.unwrap();
            db.restore_voice_notification(&previous).await.unwrap();
        };
        let (next, ()) = tokio::join!(update, cleanup);
        assert_eq!(
            db.voice_notification(&upper)
                .await
                .unwrap()
                .unwrap()
                .generation,
            next
        );
        assert_eq!(
            db.voice_notification(&lower)
                .await
                .unwrap()
                .unwrap()
                .generation,
            sibling.generation
        );
    }
    // One canonical community can occupy more than two bounded pages.
    sqlx::query("DELETE FROM social.community_voice_notifications WHERE community_id=$1")
        .bind(cid)
        .execute(pool)
        .await
        .unwrap();
    let letters: Vec<usize> = lower
        .char_indices()
        .filter_map(|(index, ch)| ch.is_ascii_alphabetic().then_some(index))
        .take(8)
        .collect();
    for mask in 0..130_u16 {
        let mut raw = lower.as_bytes().to_vec();
        for (bit, index) in letters.iter().enumerate() {
            if mask & (1 << bit) != 0 {
                raw[*index] = raw[*index].to_ascii_uppercase();
            }
        }
        db.record_voice_notification(cid, std::str::from_utf8(&raw).unwrap(), Scope::All)
            .await
            .unwrap();
    }
    let mut cursor = String::new();
    let mut lengths = Vec::new();
    let mut keys = std::collections::BTreeSet::new();
    loop {
        let page = db.voice_notifications_page(&cursor).await.unwrap();
        lengths.push(page.len());
        for entry in &page {
            assert_eq!(entry.community, cid);
            assert!(keys.insert(entry.raw_community.clone()));
            cursor.clone_from(&entry.raw_community);
        }
        if page.len() < 64 {
            break;
        }
    }
    assert_eq!(lengths, vec![64, 64, 2]);
    assert_eq!(keys.len(), 130);
    database.drop().await;
}

#[tokio::test]
async fn raw_start_preserves_source_metadata_and_online_audience_in_native_inbox() {
    let Some(database) = OwnedDatabase::new("fanout", crate::db::scratch::Schema::Full, 8).await
    else {
        return;
    };
    let pool = &database.pool;
    let fixture: Value = serde_json::from_str(include_str!(concat!(env!("DCL_ONE_ROOT"), "/services/catalyst/tests/fixtures/conformance/social/community-raw-lifecycle-source-cases.json"))).unwrap();
    let source = &fixture["cases"][0];
    let raw = source["broadcasts"][0]["event"]["metadata"]["communityId"]
        .as_str()
        .unwrap();
    let cid: Uuid = raw.parse().unwrap();
    let caller = "0x1111111111111111111111111111111111111111";
    let member = "0x2222222222222222222222222222222222222222";
    let offline = "0x6666666666666666666666666666666666666666";
    sqlx::query("INSERT INTO social.communities(id,name,owner_address) VALUES($1,'Community',$2)")
        .bind(cid)
        .bind(caller)
        .execute(pool)
        .await
        .unwrap();
    for (address, role) in [(caller, "owner"), (member, "member"), (offline, "member")] {
        sqlx::query("INSERT INTO social.community_members(community_id,member_address,role) VALUES($1,$2,$3)").bind(cid).bind(address).bind(role).execute(pool).await.unwrap();
    }
    let directory =
        std::env::temp_dir().join(format!("raw-community-broadcast-{}", Uuid::new_v4()));
    let http = crate::http::Outbound::for_tests();
    let state = Arc::new(crate::communities::State::new(
        pool.clone(),
        Arc::new(
            crate::storage::Storage::from_config(&directory)
                .await
                .unwrap(),
        ),
        crate::communities::Settings::for_tests(),
        None,
        &http,
        crate::communities::State::explore_for_tests(pool),
    ));
    let ctx = test_ctx(
        Db::new(pool.clone()),
        Arc::new(FakeVoiceRooms::new(pool.clone())),
    );
    ctx.set_communities(state.clone());
    ctx.topology()
        .receive(&pulse_frame(vec![
            caller.into(),
            member.into(),
            "0x5555555555555555555555555555555555555555".into(),
        ]))
        .unwrap();
    ops::start_community_voice_chat(&ctx, caller, raw, false)
        .await
        .unwrap();
    let rows: Vec<(String,Value)> = sqlx::query_as("SELECT address,metadata FROM social.notifications WHERE type='community_voice_chat_started'").fetch_all(pool).await.unwrap();
    assert_eq!(
        rows,
        vec![(
            member.into(),
            source["broadcasts"][0]["event"]["metadata"].clone()
        )],
        "inbox retains raw metadata and excludes opener/offline member"
    );
    let deliveries: Vec<(String,String,i32)> = sqlx::query_as("SELECT n.address,d.state,d.attempts FROM social.notifications n JOIN social.notification_deliveries d ON d.notification_id=n.id WHERE n.type='community_voice_chat_started'").fetch_all(pool).await.unwrap();
    assert_eq!(deliveries, vec![(member.into(), "skipped".into(), 0)]);
    let _ = std::fs::remove_dir_all(directory);
    database.drop().await;
}

#[tokio::test]
async fn active_raw_aliases_refresh_through_real_comms_presence_and_end_after_moderator_expiry() {
    use crate::comms::gatekeeper::voice::rooms::VoiceRooms;
    use crate::comms::gatekeeper::{CommsState, Settings as CommsSettings};
    use crate::comms::interfaces::test_support::{StubDirectory, StubPolicy, StubScenes};
    let Some(database) = OwnedDatabase::new("fanout", crate::db::scratch::Schema::Full, 8).await
    else {
        return;
    };
    let pool = &database.pool;
    let owner = addr("real-alias-owner");
    let member = addr("real-alias-member");
    let guest = addr("real-alias-guest");
    let cid = seed_community(pool, true, &owner, "owner").await;
    sqlx::query("INSERT INTO social.community_members(community_id,member_address,role) VALUES($1,$2,'member')").bind(cid).bind(&member).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO social.community_voice_chat_users(community_id,address,is_moderator,status) VALUES($1,$2,true,'connected'),($1,$3,false,'connected')").bind(cid).bind(&owner).bind(&member).execute(pool).await.unwrap();
    let runtime = Arc::new(CommsState::new(
        pool.clone(),
        crate::http::Outbound::for_tests(),
        CommsSettings::for_tests(),
        None,
        None,
        Arc::new(StubPolicy::default()),
        Arc::new(StubScenes::default()),
        Arc::new(crate::comms::presence::Presence::new()),
        Arc::new(StubDirectory::default()),
    ));
    let ctx = test_ctx(Db::new(pool.clone()), runtime.clone());
    let lower = cid.to_string();
    let upper = lower.to_uppercase();
    ctx.db()
        .record_voice_notification(cid, &upper, Scope::All)
        .await
        .unwrap();
    ctx.db()
        .record_voice_notification(cid, &lower, Scope::Members)
        .await
        .unwrap();
    sqlx::query("UPDATE social.community_voice_notifications SET expires_at=timezone('UTC',now())+interval '1 hour' WHERE community_id=$1").bind(cid).execute(pool).await.unwrap();
    let old_upper = ctx.db().voice_notification(&upper).await.unwrap().unwrap();
    let old_lower = ctx.db().voice_notification(&lower).await.unwrap().unwrap();
    assert!(runtime.community_voice_chat_active(cid).await.unwrap());
    let restarted = test_ctx(Db::new(pool.clone()), runtime.clone());
    let mut to_member = restarted.pubsub().subscribe(&member);
    let mut to_guest = restarted.pubsub().subscribe(&guest);
    restarted.poll_community_voice_notifications().await;
    for original in [&old_upper, &old_lower] {
        let current = restarted
            .db()
            .voice_notification(&original.raw_community)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (current.generation, current.scope),
            (original.generation, original.scope)
        );
        assert!(current.expires_at > original.expires_at + chrono::Duration::hours(22));
    }
    assert!(to_member.try_recv().is_err());
    assert!(to_guest.try_recv().is_err());
    // A connected ordinary participant does not keep a room active once the
    // only moderator's interrupted presence expires. This is Comms' real predicate.
    sqlx::query("UPDATE social.community_voice_chat_users SET status='connection_interrupted',status_updated_at=timezone('UTC',now())-interval '1 year' WHERE community_id=$1 AND is_moderator").bind(cid).execute(pool).await.unwrap();
    assert!(!runtime.community_voice_chat_active(cid).await.unwrap());
    restarted.poll_community_voice_notifications().await;
    let mut member_keys = Vec::new();
    while let Ok(event) = to_member.try_recv() {
        let SocialEvent::CommunityVoice(value) = event.as_ref() else {
            panic!("unexpected event")
        };
        member_keys.push(value.community_id.clone());
    }
    member_keys.sort();
    let mut expected = vec![upper.clone(), lower];
    expected.sort();
    assert_eq!(member_keys, expected);
    let guest_event = to_guest.try_recv().unwrap();
    assert!(
        matches!(guest_event.as_ref(),SocialEvent::CommunityVoice(value) if value.community_id==upper)
    );
    assert!(to_guest.try_recv().is_err());
    assert!(
        restarted
            .db()
            .voice_notifications_page("")
            .await
            .unwrap()
            .is_empty()
    );
    database.drop().await;
}
