//! Cross-replica signalling over two links to an owned core-NATS process.
use super::*;
use crate::comms::gatekeeper::bus::{BrokerBus, InProcessBus, NatsBus};
use crate::rpc::fanout::{Event, Scope};
use tokio::sync::broadcast;

#[path = "fanout_membership.rs"]
mod membership;
#[path = "community_raw_source_cases.rs"]
mod raw_voice;

/// Separate fixtures keep a fake media view from polling another test's rooms.
pub(crate) use crate::db::scratch::OwnedDatabase;

struct Broker(std::process::Child);
impl Drop for Broker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
impl Broker {
    fn start() -> (Self, String) {
        let binary =
            std::env::var("PULSE_NATS_SERVER_BIN").unwrap_or_else(|_| "nats-server".to_string());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let child = std::process::Command::new(binary)
            .args(["-a", "127.0.0.1", "-p", &port.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        (Self(child), format!("nats://127.0.0.1:{port}"))
    }
}
fn replica(pool: &PgPool) -> Arc<State> {
    Arc::new(State::with_context(
        Settings::default(),
        test_ctx(
            Db::new(pool.clone()),
            Arc::new(FakeVoiceRooms::new(pool.clone())),
        ),
        None,
    ))
}
async fn receive(rx: &mut broadcast::Receiver<Arc<SocialEvent>>) -> Arc<SocialEvent> {
    tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap()
}
async fn silent(rx: &mut broadcast::Receiver<Arc<SocialEvent>>) {
    assert!(
        tokio::time::timeout(Duration::from_millis(100), rx.recv())
            .await
            .is_err()
    );
}
fn started(community: Uuid, name: &str, image: Option<String>) -> CommunityVoiceChatUpdate {
    CommunityVoiceChatUpdate {
        community_id: community.to_string(),
        community_name: name.into(),
        community_image: image,
        status: CommunityVoiceChatStatus::CommunityVoiceChatStarted as i32,
        created_at: 123,
        ..Default::default()
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_replicas_route_mutations_and_preserve_large_community_snapshots() {
    let Some(database) = OwnedDatabase::new("fanout", crate::db::scratch::Schema::Full, 8).await
    else {
        return;
    };
    let pool = database.pool.clone();
    let (_broker, url) = Broker::start();
    let left = replica(&pool);
    let right = replica(&pool);
    let a = addr("bus-a");
    let b = addr("bus-b");
    let outsider = addr("bus-out");
    let community = seed_community(&pool, true, &a, "owner").await;
    sqlx::query("INSERT INTO social.community_members(community_id,member_address,role) VALUES($1,$2,'member')")
        .bind(community).bind(&b).execute(&pool).await.unwrap();
    let mut local = left.ctx.pubsub().subscribe(&b);
    let mut remote = right.ctx.pubsub().subscribe(&b);
    let mut public = right.ctx.pubsub().subscribe(&outsider);
    let mut excluded = right.ctx.pubsub().subscribe(&a);
    let jobs = Jobs::new();
    let one = Arc::new(BrokerBus::new(Some(url.clone()), "social-test-one"));
    let two = Arc::new(BrokerBus::new(Some(url), "social-test-two"));
    left.start_fanout(&jobs, one.clone());
    right.start_fanout(&jobs, two.clone());
    one.connect();
    two.connect();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !one.is_ready() || !two.is_ready() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    left.ctx.pubsub().publish(
        &b,
        SocialEvent::Block(BlockUpdate {
            address: a.clone(),
            is_blocked: true,
            stream_closed: None,
        }),
    );
    assert!(matches!(&*receive(&mut local).await,SocialEvent::Block(u) if u.address==a));
    assert!(matches!(&*receive(&mut remote).await,SocialEvent::Block(u) if u.address==a));
    silent(&mut local).await;
    silent(&mut public).await;
    // A long legitimate profile is fetched at RPC delivery, outside the bus
    // envelope, while the raw address event also remains available for JSON.
    let profile = seed_profile(&pool, &a, "Current remote profile").await;
    let picture = "https://profiles.example/".to_string() + &"u".repeat(140_000);
    let metadata = serde_json::json!({"v":{"avatars":[{"userId":a,"name":"Current remote profile","hasClaimedName":true,"avatar":{"snapshots":{"face256":picture}}}]}});
    sqlx::query("UPDATE content.deployments SET entity_metadata=$2 WHERE entity_id=$1")
        .bind(&profile)
        .bind(metadata)
        .execute(&pool)
        .await
        .unwrap();
    let cancel = tokio_util::sync::CancellationToken::new();
    let mut rpc = SocialServiceImpl
        .subscribe_to_friendship_updates(procedure_ctx(&right.ctx, &b, 8001), cancel.clone())
        .await
        .unwrap();
    let request = FriendshipUpdate {
        stream_closed: None,
        update: Some(friendship_update::Update::Request(
            friendship_update::RequestResponse {
                id: Uuid::new_v4().to_string(),
                created_at: 123,
                message: Some("🦊".repeat(256)),
                friend: Some(FriendProfile {
                    address: a.clone(),
                    profile_picture_url: picture.clone(),
                    ..Default::default()
                }),
            },
        )),
    };
    left.ctx
        .pubsub()
        .publish(&b, SocialEvent::Friendship(request));
    let _ = receive(&mut local).await;
    assert!(matches!(
        &*receive(&mut remote).await,
        SocialEvent::Friendship(_)
    ));
    let delivered = next_update(&mut rpc).await;
    let Some(friendship_update::Update::Request(request)) = delivered.update else {
        panic!("request expected")
    };
    assert_eq!(request.friend.unwrap().profile_picture_url, picture);
    assert_eq!(request.message.unwrap().chars().count(), 256);
    cancel.cancel();
    drop_profile(&pool, &a, &profile).await;
    // Topology is already derived independently in every serve: it must not be rebroadcast.
    left.ctx.pubsub().publish(
        &b,
        SocialEvent::FriendConnectivity(FriendConnectivityUpdate::default()),
    );
    assert!(matches!(
        &*receive(&mut local).await,
        SocialEvent::FriendConnectivity(_)
    ));
    silent(&mut remote).await;
    let first = started(community, "first", Some("u".repeat(140_000)));
    left.ctx
        .pubsub()
        .community_voice(&pool, community, first.clone(), true, Some(&a))
        .await;
    assert!(
        matches!(&*receive(&mut local).await,SocialEvent::CommunityVoice(u) if u.community_image==first.community_image && u.is_member)
    );
    assert!(
        matches!(&*receive(&mut remote).await,SocialEvent::CommunityVoice(u) if u.community_image==first.community_image && u.is_member)
    );
    assert!(
        matches!(&*receive(&mut public).await,SocialEvent::CommunityVoice(u) if u.community_image==first.community_image && !u.is_member)
    );
    silent(&mut excluded).await;
    // The immutable cache holds both queued START snapshots across END/new START.
    let second = started(community, "second", Some("v".repeat(140_000)));
    let event_one = Event::community_voice(Scope::All, None, &first).unwrap();
    let event_two = Event::community_voice(Scope::All, None, &second).unwrap();
    let id_one = left
        .ctx
        .db()
        .store_social_event_payload(event_one.encode_stored().unwrap())
        .await
        .unwrap();
    left.ctx
        .db()
        .record_voice_notification(community, &community.to_string(), Scope::All)
        .await
        .unwrap();
    let entry = left
        .ctx
        .db()
        .voice_notification(&community.to_string())
        .await
        .unwrap()
        .unwrap();
    left.ctx
        .db()
        .clear_voice_notification(&entry)
        .await
        .unwrap();
    left.ctx
        .db()
        .record_voice_notification(community, &community.to_string(), Scope::Members)
        .await
        .unwrap();
    let id_two = left
        .ctx
        .db()
        .store_social_event_payload(event_two.encode_stored().unwrap())
        .await
        .unwrap();
    Event::Reference { event_id: id_one }
        .deliver(&right.ctx)
        .await;
    Event::Reference { event_id: id_two }
        .deliver(&right.ctx)
        .await;
    for expected in [&first, &second] {
        assert!(
            matches!(&*receive(&mut remote).await,SocialEvent::CommunityVoice(u) if u.community_name==expected.community_name && u.community_image==expected.community_image)
        );
        let _ = receive(&mut public).await;
        let _ = receive(&mut excluded).await;
    }
    // A public hint never overrides a privacy change at delivery.
    sqlx::query("UPDATE social.communities SET private=true WHERE id=$1")
        .bind(community)
        .execute(&pool)
        .await
        .unwrap();
    Event::Reference { event_id: id_one }
        .deliver(&right.ctx)
        .await;
    assert!(matches!(&*receive(&mut remote).await,SocialEvent::CommunityVoice(u) if u.is_member));
    let _ = receive(&mut excluded).await;
    silent(&mut public).await;
    jobs.shutdown(Duration::from_secs(2)).await;
    drop_community(&pool, community).await;
    database.drop().await;
}

#[tokio::test]
async fn notification_generation_survives_context_restart_and_expires_like_source_cache() {
    let Some(database) = OwnedDatabase::new("fanout", crate::db::scratch::Schema::Full, 8).await
    else {
        return;
    };
    let pool = database.pool.clone();
    let a = addr("scope");
    let public = addr("scope-out");
    let community = seed_community(&pool, true, &a, "owner").await;
    let db = Db::new(pool.clone());
    let old = db
        .record_voice_notification(community, &community.to_string(), Scope::All)
        .await
        .unwrap();
    let old_entry = db
        .voice_notification(&community.to_string())
        .await
        .unwrap()
        .unwrap();
    let current = db
        .record_voice_notification(community, &community.to_string(), Scope::Members)
        .await
        .unwrap();
    assert_ne!(old, current);
    assert!(!db.clear_voice_notification(&old_entry).await.unwrap());
    assert!(!db.refresh_voice_notification(&old_entry).await.unwrap());
    let current_entry = db
        .voice_notification(&community.to_string())
        .await
        .unwrap()
        .unwrap();
    assert!(db.clear_voice_notification(&current_entry).await.unwrap());
    db.record_voice_notification(community, &community.to_string(), Scope::All)
        .await
        .unwrap();
    // A new process sees the persisted public scope and emits ENDED once.
    let restarted = replica(&pool);
    let mut rx = restarted.ctx.pubsub().subscribe(&public);
    restarted.ctx.poll_community_voice_notifications().await;
    assert!(
        matches!(&*receive(&mut rx).await,SocialEvent::CommunityVoice(u) if u.status==CommunityVoiceChatStatus::CommunityVoiceChatEnded as i32)
    );
    restarted.ctx.poll_community_voice_notifications().await;
    silent(&mut rx).await;
    db.record_voice_notification(community, &community.to_string(), Scope::All)
        .await
        .unwrap();
    sqlx::query("UPDATE social.community_voice_notifications SET expires_at=now()-interval '1 second' WHERE community_id=$1")
        .bind(community).execute(&pool).await.unwrap();
    restarted.ctx.poll_community_voice_notifications().await;
    silent(&mut rx).await;
    assert!(
        db.voice_notification(&community.to_string())
            .await
            .unwrap()
            .is_none()
    );
    drop_community(&pool, community).await;
    database.drop().await;
}

#[tokio::test]
async fn outbound_queue_and_disconnected_bus_are_bounded_best_effort() {
    let Some(pool) = scratch_pool().await else {
        return;
    };
    let state = replica(&pool);
    let me = addr("bound");
    let actor = addr("actor");
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    state.ctx.pubsub().attach_outbound(tx);
    for _ in 0..1000 {
        state.ctx.pubsub().publish(
            &me,
            SocialEvent::Block(BlockUpdate {
                address: actor.clone(),
                is_blocked: true,
                stream_closed: None,
            }),
        );
    }
    assert!(rx.try_recv().is_ok());
    assert!(rx.try_recv().is_err());
    let left = replica(&pool);
    let right = replica(&pool);
    let jobs = Jobs::new();
    let bus = Arc::new(InProcessBus::new());
    left.start_fanout(&jobs, bus.clone());
    right.start_fanout(&jobs, bus.clone());
    let mut remote = right.ctx.pubsub().subscribe(&me);
    bus.set_connected(false);
    left.ctx.pubsub().publish(
        &me,
        SocialEvent::Block(BlockUpdate {
            address: actor.clone(),
            is_blocked: true,
            stream_closed: None,
        }),
    );
    silent(&mut remote).await;
    bus.set_connected(true);
    left.ctx.pubsub().publish(
        &me,
        SocialEvent::Block(BlockUpdate {
            address: actor,
            is_blocked: false,
            stream_closed: None,
        }),
    );
    assert!(matches!(&*receive(&mut remote).await,SocialEvent::Block(u) if !u.is_blocked));
    silent(&mut remote).await;
    jobs.shutdown(Duration::from_secs(2)).await;
}

#[tokio::test]
async fn notification_and_spill_queries_use_bounded_index_paths() {
    let Some(database) = OwnedDatabase::new("fanout", crate::db::scratch::Schema::Full, 8).await
    else {
        return;
    };
    let pool = database.pool.clone();
    let mut tx = pool.begin().await.unwrap();
    let owner = addr("plans");
    sqlx::query("INSERT INTO social.communities(id,name,owner_address) SELECT ('a0000000-0000-4000-8000-'||lpad(to_hex(n),12,'0'))::uuid,'plan fixture',$1 FROM generate_series(1,5000) AS n")
        .bind(&owner).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO social.community_voice_notifications(raw_community_id,community_id,notification_scope,generation,expires_at) SELECT raw,id,'all',gen_random_uuid(),now()+interval '24 hours' FROM (SELECT id FROM social.communities WHERE owner_address=$1 ORDER BY id LIMIT 1250) AS rooms CROSS JOIN LATERAL unnest(ARRAY[id::text,upper(id::text)]) AS raw")
        .bind(&owner).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO social.event_payloads(id,payload,expires_at) SELECT gen_random_uuid(),decode('01','hex'),now()+CASE WHEN n<=10 THEN interval '-1 hour' ELSE interval '1 hour' END FROM generate_series(1,5000) AS n")
        .execute(&mut *tx).await.unwrap();
    sqlx::query("ANALYZE social.community_voice_notifications")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("ANALYZE social.event_payloads")
        .execute(&mut *tx)
        .await
        .unwrap();
    let rooms:Vec<String>=sqlx::query_scalar("EXPLAIN(ANALYZE,BUFFERS) SELECT raw_community_id,generation FROM social.community_voice_notifications WHERE raw_community_id>'' ORDER BY raw_community_id LIMIT 64")
        .fetch_all(&mut *tx).await.unwrap();
    let cache:Vec<String>=sqlx::query_scalar("EXPLAIN(ANALYZE,BUFFERS) SELECT id FROM social.event_payloads WHERE expires_at<=timezone('UTC',now()) ORDER BY expires_at,id LIMIT 64 FOR UPDATE SKIP LOCKED")
        .fetch_all(&mut *tx).await.unwrap();
    println!(
        "notification pages (5000 communities,1250 tracked,2500 raw aliases):\n{}",
        rooms.join("\n")
    );
    println!(
        "spill cleanup (5000 snapshots,10 expired):\n{}",
        cache.join("\n")
    );
    assert!(
        rooms
            .join("\n")
            .contains("community_voice_notifications_pkey")
    );
    assert!(cache.join("\n").contains("event_payloads_expiry_idx"));
    tx.rollback().await.unwrap();
    database.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn healthy_real_broker_bursts_reach_the_exact_subscription_queue_bound() {
    let Some(database) = OwnedDatabase::new("fanout", crate::db::scratch::Schema::Full, 8).await
    else {
        return;
    };
    let (_broker, url) = Broker::start();
    let left = replica(&database.pool);
    let right = replica(&database.pool);
    use std::sync::atomic::{AtomicUsize, Ordering};
    let started = std::time::Instant::now();
    let me = addr("burst-recipient");
    let mut local = left.ctx.pubsub().subscribe_updates::<BlockUpdate>(&me);
    let mut remote = right.ctx.pubsub().subscribe_updates::<BlockUpdate>(&me);
    let jobs = Jobs::new();
    let one = Arc::new(BrokerBus::new(Some(url.clone()), "social-burst-one"));
    let two = Arc::new(BrokerBus::new(Some(url), "social-burst-two"));
    let origin_frames = Arc::new(AtomicUsize::new(0));
    let remote_frames = Arc::new(AtomicUsize::new(0));
    let taps: Vec<_> = [
        (one.clone(), origin_frames.clone()),
        (two.clone(), remote_frames.clone()),
    ]
    .into_iter()
    .map(|(bus, count)| {
        bus.subscribe(
            crate::comms::protocol::bus::SOCIAL_EVENTS_SUBJECT,
            None,
            Arc::new(move |_, _| {
                count.fetch_add(1, Ordering::Relaxed);
            }),
        )
    })
    .collect();
    left.start_fanout(&jobs, one.clone());
    right.start_fanout(&jobs, two.clone());
    one.connect();
    two.connect();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !one.is_ready() || !two.is_ready() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let ready_after = started.elapsed();
    for n in 0..1000 {
        left.ctx
            .pubsub()
            .publish_mutation(
                &me,
                SocialEvent::Block(BlockUpdate {
                    stream_closed: None,
                    address: format!("0x{n:040x}"),
                    is_blocked: true,
                }),
            )
            .await;
    }
    let submitted_after = started.elapsed();
    let delivered = tokio::time::timeout(Duration::from_secs(10), async {
        while remote.len() < 1000 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    eprintln!(
        "targeted broker burst: ready={ready_after:?} submitted={submitted_after:?} elapsed={:?} origin_frames={} remote_frames={} local_queue={} remote_queue={} ready={}/{}",
        started.elapsed(),
        origin_frames.load(Ordering::Relaxed),
        remote_frames.load(Ordering::Relaxed),
        local.len(),
        remote.len(),
        one.is_ready(),
        two.is_ready()
    );
    assert!(
        delivered.is_ok(),
        "healthy burst delivery timed out; see stage counts above (harness deadline, no protocol latency promise)"
    );
    assert_eq!(local.len(), 1000);
    for n in 0..1000 {
        let address = format!("0x{n:040x}");
        assert!(
            matches!(&*local.recv().await.unwrap(),SocialEvent::Block(update) if update.address==address)
        );
        assert!(
            matches!(&*remote.recv().await.unwrap(),SocialEvent::Block(update) if update.address==address)
        );
    }
    assert_eq!(
        remote.len(),
        0,
        "publisher loopback must not duplicate delivery"
    );
    drop(taps);
    jobs.shutdown(Duration::from_secs(2)).await;
    drop((left, right));
    database.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_broker_community_burst_keeps_sql_work_bounded_and_control_delivery_live() {
    use crate::comms::gatekeeper::bus::PublishOutcome;
    use crate::comms::protocol::bus::SOCIAL_EVENTS_SUBJECT;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let Some(database) = OwnedDatabase::new("fanout", crate::db::scratch::Schema::Full, 8).await
    else {
        return;
    };
    let (_broker, url) = Broker::start();
    let right = replica(&database.pool);
    let actor = addr("community-burst-actor");
    let recipient = addr("community-burst-recipient");
    let community = seed_community(&database.pool, true, &actor, "owner").await;
    sqlx::query("INSERT INTO social.community_members(community_id,member_address,role) VALUES($1,$2,'member')")
        .bind(community).bind(&recipient).execute(&database.pool).await.unwrap();
    let mut remote = right
        .ctx
        .pubsub()
        .subscribe_updates::<CommunityMemberConnectivityUpdate>(&recipient);
    let jobs = Jobs::new();
    let one = Arc::new(BrokerBus::new(
        Some(url.clone()),
        "social-community-burst-one",
    ));
    let two = Arc::new(BrokerBus::new(Some(url), "social-community-burst-two"));
    let controls = Arc::new(AtomicUsize::new(0));
    let control_count = controls.clone();
    let _control = two.subscribe(
        "social.test.control",
        None,
        Arc::new(move |_, _| {
            control_count.fetch_add(1, Ordering::SeqCst);
        }),
    );
    right.start_fanout(&jobs, two.clone());
    one.connect();
    two.connect();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !one.is_ready() || !two.is_ready() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let source = Uuid::new_v4();
    let event = Event::CommunityMember {
        community,
        actor,
        status: ConnectivityStatus::Online as i32,
    }
    .encode(source)
    .unwrap();
    let started = std::time::Instant::now();
    for n in 0..1000 {
        assert_eq!(
            one.publish(SOCIAL_EVENTS_SUBJECT, event.clone()).await,
            PublishOutcome::Submitted
        );
        if n % 10 == 0 {
            assert_eq!(
                one.publish("social.test.control", vec![1]).await,
                PublishOutcome::Submitted
            );
        }
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while controls.load(Ordering::SeqCst) < 100 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("control subject must keep running while community SQL is busy");
    println!(
        "community burst: control100 delivered after {:?}; community queued{}",
        started.elapsed(),
        remote.len()
    );
    let complete = tokio::time::timeout(Duration::from_secs(10), async {
        while remote.len() < 1000 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
    assert!(
        complete.is_ok(),
        "normal community burst lost events: received{}of1000",
        remote.len()
    );
    for _ in 0..1000 {
        assert!(matches!(
            &*remote.recv().await.unwrap(),
            SocialEvent::CommunityMemberStatus(_)
        ));
    }
    jobs.shutdown(Duration::from_secs(2)).await;
    drop(right);
    database.drop().await;
}
