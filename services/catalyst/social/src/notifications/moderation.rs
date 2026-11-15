//! Social-owned projection and persistence of comms moderation notices.

use super::store::{NewNotification, insert_notifications};
use crate::comms::interfaces::{ModerationNotification, ModerationNotificationKind as Kind};
use serde_json::json;
use uuid::Uuid;

const MODERATION_NS: Uuid = Uuid::from_bytes([
    0x12, 0xd2, 0x06, 0xe4, 0xdb, 0x6f, 0x44, 0x5c, 0x92, 0x32, 0x4f, 0x6c, 0x0c, 0x67, 0x12, 0xb1,
]);

fn project(notification: &ModerationNotification) -> NewNotification {
    let (kind, metadata) = match &notification.event {
        Kind::Banned {
            reason,
            banned_at,
            expires_at,
            custom_message,
        } => {
            let mut metadata =
                json!({"reason": reason, "bannedAt": banned_at, "expiresAt": expires_at});
            if let Some(message) = custom_message
                .as_ref()
                .filter(|message| !message.is_empty())
            {
                metadata["customMessage"] = json!(message);
            }
            ("banned", metadata)
        }
        Kind::BanLifted { lifted_at } => ("ban_lifted", json!({"liftedAt": lifted_at})),
        Kind::BanWarning { reason, warned_at } => (
            "ban_warning",
            json!({"reason": reason, "warnedAt": warned_at}),
        ),
        Kind::UserBannedFromScene { place_title } => {
            ("user_banned_from_scene", json!({"placeTitle": place_title}))
        }
        Kind::UserUnbannedFromScene { place_title } => (
            "user_unbanned_from_scene",
            json!({"placeTitle": place_title}),
        ),
    };
    let identity = serde_json::to_vec(&(
        kind,
        notification.recipient.as_str(),
        &notification.source_id,
    ))
    .expect("moderation identity contains only strings");
    NewNotification {
        id: Uuid::new_v5(&MODERATION_NS, &identity),
        address: notification.recipient.clone(),
        kind: kind.into(),
        metadata,
        occurred_at: notification.occurred_at,
    }
}

pub(crate) async fn insert(
    transaction: &mut sqlx::PgConnection,
    notification: &ModerationNotification,
) -> Result<(), sqlx::Error> {
    // The shared path writes the email delivery row once; a retry on the v5 id
    // refreshes payload and time and keeps the read state, as upstream's upsert.
    insert_notifications(transaction, std::slice::from_ref(&project(notification))).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comms::gatekeeper::moderation::Moderation;
    use crate::comms::gatekeeper::moderation::store::{CreateBan, CreateWarning};
    use crate::comms::interfaces::SocialDirectory;
    use crate::comms::interfaces::{ModerationNotification, ModerationNotificationKind as Kind};
    use serde_json::json;
    use std::sync::Arc;

    async fn pool() -> Option<sqlx::PgPool> {
        let pool = crate::db::scratch::pool(4).await;
        if pool.is_none() {
            eprintln!(
                "GUARDED moderation notifications PostgreSQL assertions: CATALYST_TEST_DATABASE_URL unset"
            );
        }
        pool
    }

    fn wallet() -> crate::Address {
        crate::Address::parse(&format!("0x00000000{}", Uuid::new_v4().simple())).unwrap()
    }

    fn ban(recipient: &crate::Address) -> CreateBan {
        CreateBan {
            banned_address: recipient.clone(),
            banned_by: wallet().into_string(),
            reason: "abuse".into(),
            custom_message: Some("contact support".into()),
            banned_device_id: Some("private-device".into()),
            duration_ms: None,
        }
    }

    fn notice(event: Kind) -> ModerationNotification {
        ModerationNotification {
            source_id: "source-transition-1".into(),
            recipient: crate::Address::parse("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap(),
            occurred_at: chrono::DateTime::from_timestamp_millis(1_700_000_000_123)
                .unwrap()
                .naive_utc(),
            event,
        }
    }

    #[test]
    fn five_moderation_payloads_match_pinned_worker_projection() {
        let cases = [
            (
                Kind::Banned {
                    reason: "abuse".into(),
                    banned_at: 1_700_000_000_123,
                    expires_at: Some(1_700_000_030_123),
                    custom_message: Some("Contact support".into()),
                },
                "banned",
                json!({"reason":"abuse","bannedAt":1700000000123_i64,"expiresAt":1700000030123_i64,"customMessage":"Contact support"}),
            ),
            (
                Kind::BanLifted {
                    lifted_at: 1_700_000_000_123,
                },
                "ban_lifted",
                json!({"liftedAt":1700000000123_i64}),
            ),
            (
                Kind::BanWarning {
                    reason: "abuse".into(),
                    warned_at: 1_700_000_000_123,
                },
                "ban_warning",
                json!({"reason":"abuse","warnedAt":1700000000123_i64}),
            ),
            (
                Kind::UserBannedFromScene {
                    place_title: "Scene A".into(),
                },
                "user_banned_from_scene",
                json!({"placeTitle":"Scene A"}),
            ),
            (
                Kind::UserUnbannedFromScene {
                    place_title: "Scene A".into(),
                },
                "user_unbanned_from_scene",
                json!({"placeTitle":"Scene A"}),
            ),
        ];
        for (event, kind, metadata) in cases {
            let projected = project(&notice(event));
            assert_eq!(projected.kind, kind);
            assert_eq!(projected.metadata, metadata);
            assert_eq!(
                projected.address.as_str(),
                "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            );
            assert_eq!(
                projected.occurred_at.and_utc().timestamp_millis(),
                1_700_000_000_123
            );
        }
    }

    #[test]
    fn permanent_ban_omits_absent_or_empty_custom_message() {
        for custom_message in [None, Some(String::new())] {
            let projected = project(&notice(Kind::Banned {
                reason: "abuse".into(),
                banned_at: 123,
                expires_at: None,
                custom_message,
            }));
            assert_eq!(
                projected.metadata,
                json!({"reason":"abuse","bannedAt":123,"expiresAt":null})
            );
        }
    }

    #[test]
    fn identity_uses_recipient_subtype_and_source_not_mutable_payload_or_time() {
        let mut event = notice(Kind::BanWarning {
            reason: "original".into(),
            warned_at: 123,
        });
        let original = project(&event).id;
        event.event = Kind::BanWarning {
            reason: "changed".into(),
            warned_at: 456,
        };
        event.occurred_at += chrono::Duration::seconds(1);
        assert_eq!(project(&event).id, original);
        event.event = Kind::BanLifted { lifted_at: 456 };
        assert_ne!(project(&event).id, original);
        event.event = Kind::BanWarning {
            reason: "original".into(),
            warned_at: 123,
        };
        event.recipient =
            crate::Address::parse("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
        assert_ne!(project(&event).id, original);
        event.recipient = notice(Kind::BanLifted { lifted_at: 1 }).recipient;
        event.source_id = "later-warning-row".into();
        assert_ne!(project(&event).id, original);
    }

    #[tokio::test]
    async fn directory_retry_refreshes_payload_and_time_keeps_read_state_and_one_delivery_row() {
        let Some(pool) = pool().await else { return };
        let directory = crate::comms_directory::Directory::new(pool.clone());
        let mut event = notice(Kind::BanWarning {
            reason: "original".into(),
            warned_at: 123,
        });
        event.recipient = wallet();
        event.source_id = Uuid::new_v4().to_string();
        let id = project(&event).id;
        let mut txn = pool.begin().await.unwrap();
        directory
            .notify_moderation_notification(&mut txn, &event)
            .await
            .unwrap();
        txn.commit().await.unwrap();
        sqlx::query("UPDATE social.notifications SET read=TRUE, read_at=$2 WHERE id=$1")
            .bind(id)
            .bind(event.occurred_at)
            .execute(&pool)
            .await
            .unwrap();
        event.event = Kind::BanWarning {
            reason: "retry".into(),
            warned_at: 456,
        };
        event.occurred_at += chrono::Duration::seconds(10);
        let mut txn = pool.begin().await.unwrap();
        directory
            .notify_moderation_notification(&mut txn, &event)
            .await
            .unwrap();
        txn.commit().await.unwrap();
        let rows: Vec<(
            serde_json::Value,
            chrono::NaiveDateTime,
            bool,
            Option<chrono::NaiveDateTime>,
        )> = sqlx::query_as(
            "SELECT metadata,occurred_at,read,read_at FROM social.notifications WHERE id=$1",
        )
        .bind(id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![(
                json!({"reason":"retry","warnedAt":456}),
                event.occurred_at,
                true,
                Some(
                    chrono::DateTime::from_timestamp_millis(1_700_000_000_123)
                        .unwrap()
                        .naive_utc()
                )
            )]
        );
        let delivery_rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM social.notification_deliveries WHERE notification_id=$1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            delivery_rows, 1,
            "moderation notices enter the email outbox once, like upstream"
        );
        event.source_id = Uuid::new_v4().to_string();
        let rolled_back_id = project(&event).id;
        let mut txn = pool.begin().await.unwrap();
        directory
            .notify_moderation_notification(&mut txn, &event)
            .await
            .unwrap();
        txn.rollback().await.unwrap();
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM social.notifications WHERE id=$1")
                .bind(rolled_back_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn concurrent_duplicate_source_commits_one_notice_and_keeps_identity_and_read_state() {
        async fn commit_concurrently(pool: &sqlx::PgPool, event: &ModerationNotification) {
            let barrier = Arc::new(tokio::sync::Barrier::new(2));
            let mut tasks = tokio::task::JoinSet::new();
            for _ in 0..2 {
                let (pool, event, barrier) = (pool.clone(), event.clone(), barrier.clone());
                tasks.spawn(async move {
                    let directory = crate::comms_directory::Directory::new(pool.clone());
                    let mut txn = pool.begin().await.unwrap();
                    barrier.wait().await;
                    directory
                        .notify_moderation_notification(&mut txn, &event)
                        .await
                        .unwrap();
                    // Each contender commits independently: waiting for both
                    // INSERTs before committing would deadlock on the unique key.
                    txn.commit().await.unwrap();
                });
            }
            tokio::time::timeout(std::time::Duration::from_secs(30), async {
                while let Some(result) = tasks.join_next().await {
                    result.unwrap();
                }
            })
            .await
            .expect("concurrent notice transactions must complete");
        }

        let Some(pool) = pool().await else { return };
        let mut event = notice(Kind::BanWarning {
            reason: "original".into(),
            warned_at: 123,
        });
        event.recipient = wallet();
        event.source_id = Uuid::new_v4().to_string();
        commit_concurrently(&pool, &event).await;
        type DeliveryRow = (
            Uuid,
            serde_json::Value,
            chrono::NaiveDateTime,
            chrono::NaiveDateTime,
            bool,
            Option<chrono::NaiveDateTime>,
        );
        let original: Vec<DeliveryRow> = sqlx::query_as("SELECT id,metadata,occurred_at,created_at,read,read_at FROM social.notifications WHERE address=$1")
            .bind(&event.recipient).fetch_all(&pool).await.unwrap();
        assert_eq!(
            original.len(),
            1,
            "racing first delivery must insert only one notice"
        );
        assert_eq!(original[0].1, json!({"reason":"original","warnedAt":123}));
        assert_eq!(original[0].2, event.occurred_at);
        assert!(!original[0].4);
        assert_eq!(original[0].5, None);

        let read_at = event.occurred_at + chrono::Duration::seconds(5);
        sqlx::query("UPDATE social.notifications SET read=TRUE,read_at=$2 WHERE id=$1")
            .bind(original[0].0)
            .bind(read_at)
            .execute(&pool)
            .await
            .unwrap();
        let preserved: Vec<DeliveryRow> = sqlx::query_as("SELECT id,metadata,occurred_at,created_at,read,read_at FROM social.notifications WHERE address=$1")
            .bind(&event.recipient).fetch_all(&pool).await.unwrap();
        assert!(preserved[0].4);
        assert_eq!(preserved[0].5, Some(read_at));
        event.event = Kind::BanWarning {
            reason: "concurrent retry".into(),
            warned_at: 456,
        };
        event.occurred_at += chrono::Duration::seconds(10);
        commit_concurrently(&pool, &event).await;
        let retried: Vec<DeliveryRow> = sqlx::query_as("SELECT id,metadata,occurred_at,created_at,read,read_at FROM social.notifications WHERE address=$1")
            .bind(&event.recipient).fetch_all(&pool).await.unwrap();
        let mut expected = preserved.clone();
        expected[0].1 = json!({"reason":"concurrent retry","warnedAt":456});
        expected[0].2 = event.occurred_at;
        assert_eq!(
            retried, expected,
            "concurrent retries keep identity, creation time and read state and refresh payload and time"
        );
        let deliveries: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM social.notification_deliveries WHERE notification_id=$1",
        )
        .bind(retried[0].0)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(deliveries, 1);
    }

    #[tokio::test]
    async fn platform_ban_lift_and_reban_keep_distinct_source_rows_and_recipient_notices() {
        let Some(pool) = pool().await else { return };
        let moderation = Moderation::with_social(
            pool.clone(),
            Arc::new(crate::comms_directory::Directory::new(pool.clone())),
        );
        let recipient = wallet();
        let actor = wallet();
        let mut first_input = ban(&recipient);
        first_input.reason = "first incident".into();
        let first = moderation.create_ban(first_input).await.unwrap();
        let lifted = moderation
            .lift_ban(&recipient, actor.as_str())
            .await
            .unwrap();
        let mut second_input = ban(&recipient);
        second_input.reason = "later incident".into();
        let second = moderation.create_ban(second_input).await.unwrap();
        assert_eq!(
            lifted.id, first.id,
            "the lift must identify the original durable ban"
        );
        assert_ne!(
            second.id, first.id,
            "reban must create a new durable source transition"
        );
        let source_rows: Vec<(Uuid, bool)> = sqlx::query_as(
            "SELECT id,lifted_at IS NOT NULL FROM social.user_bans WHERE banned_address=$1",
        )
        .bind(&recipient)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(source_rows.len(), 2);
        assert!(source_rows.contains(&(Uuid::parse_str(&first.id).unwrap(), true)));
        assert!(source_rows.contains(&(Uuid::parse_str(&second.id).unwrap(), false)));
        // The notification column is timestamp(3); normalize the independent
        // source timestamps with that schema precision before comparing them.
        let (first_time, lifted_time, second_time): (
            chrono::NaiveDateTime,
            chrono::NaiveDateTime,
            chrono::NaiveDateTime,
        ) = sqlx::query_as("SELECT $1::timestamp(3),$2::timestamp(3),$3::timestamp(3)")
            .bind(first.banned_at.naive_utc())
            .bind(lifted.lifted_at.unwrap().naive_utc())
            .bind(second.banned_at.naive_utc())
            .fetch_one(&pool)
            .await
            .unwrap();
        let rows: Vec<(Uuid, String, String, serde_json::Value, chrono::NaiveDateTime, bool)> = sqlx::query_as("SELECT id,type,address,metadata,occurred_at,read FROM social.notifications WHERE address=$1")
            .bind(&recipient).fetch_all(&pool).await.unwrap();
        assert_eq!(
            rows.len(),
            3,
            "ban, lift and reban must each retain a notice"
        );
        let ids: std::collections::BTreeSet<Uuid> = rows.iter().map(|row| row.0).collect();
        assert_eq!(
            ids.len(),
            3,
            "subtype and durable source identity must distinguish all three notices"
        );
        for (kind, metadata, occurred_at) in [
            (
                "banned",
                json!({"reason":"first incident","bannedAt":first.banned_at.timestamp_millis(),"expiresAt":null,"customMessage":"contact support"}),
                first_time,
            ),
            (
                "ban_lifted",
                json!({"liftedAt":lifted.lifted_at.unwrap().timestamp_millis()}),
                lifted_time,
            ),
            (
                "banned",
                json!({"reason":"later incident","bannedAt":second.banned_at.timestamp_millis(),"expiresAt":null,"customMessage":"contact support"}),
                second_time,
            ),
        ] {
            assert!(
                rows.iter().any(
                    |(_, saved_kind, saved_recipient, saved_metadata, saved_time, read)| saved_kind
                        .as_str()
                        == kind
                        && saved_recipient.as_str() == recipient.as_str()
                        && saved_metadata == &metadata
                        && *saved_time == occurred_at
                        && !*read
                ),
                "missing {kind} notice with the correct recipient, payload and source time"
            );
        }
        assert!(moderation.ban_status(&recipient).await.unwrap().is_banned);
    }

    #[tokio::test]
    async fn accepted_moderation_transitions_produce_exact_five_kinds_and_rejections_produce_none()
    {
        let Some(pool) = pool().await else { return };
        let moderation = Moderation::with_social(
            pool.clone(),
            Arc::new(crate::comms_directory::Directory::new(pool.clone())),
        );
        let recipient = wallet();
        let actor = wallet();
        let scene = Uuid::new_v4().to_string();
        let created = moderation.create_ban(ban(&recipient)).await.unwrap();
        assert!(moderation.create_ban(ban(&recipient)).await.is_err());
        let warned = moderation
            .create_warning(CreateWarning {
                warned_address: recipient.clone(),
                warned_by: actor.as_str().into(),
                reason: "warning".into(),
            })
            .await
            .unwrap();
        let lifted = moderation
            .lift_ban(&recipient, actor.as_str())
            .await
            .unwrap();
        assert!(
            moderation
                .lift_ban(&recipient, actor.as_str())
                .await
                .is_err()
        );
        moderation
            .scene_ban(&scene, &recipient, &actor, &[], "Exact Scene A")
            .await
            .unwrap();
        assert_eq!(
            moderation
                .scene_unban(&scene, &recipient, "Exact Scene A")
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            moderation
                .scene_unban(&scene, &recipient, "Exact Scene A")
                .await
                .unwrap(),
            0
        );
        let mut rows: Vec<(String, serde_json::Value)> =
            sqlx::query_as("SELECT type,metadata FROM social.notifications WHERE address=$1")
                .bind(&recipient)
                .fetch_all(&pool)
                .await
                .unwrap();
        // This asserts the exact notices, not their order under the database locale.
        rows.sort_by(|left, right| left.0.cmp(&right.0));
        assert_eq!(
            rows,
            vec![
                (
                    "ban_lifted".into(),
                    json!({"liftedAt":lifted.lifted_at.unwrap().timestamp_millis()})
                ),
                (
                    "ban_warning".into(),
                    json!({"reason":"warning","warnedAt":warned.warned_at.timestamp_millis()})
                ),
                (
                    "banned".into(),
                    json!({"reason":"abuse","bannedAt":created.banned_at.timestamp_millis(),"expiresAt":null,"customMessage":"contact support"})
                ),
                (
                    "user_banned_from_scene".into(),
                    json!({"placeTitle":"Exact Scene A"})
                ),
                (
                    "user_unbanned_from_scene".into(),
                    json!({"placeTitle":"Exact Scene A"})
                ),
            ]
        );
        let second = moderation
            .create_warning(CreateWarning {
                warned_address: recipient.clone(),
                warned_by: actor.as_str().into(),
                reason: "warning".into(),
            })
            .await
            .unwrap();
        assert_ne!(
            second.id, warned.id,
            "separate POST warnings are distinct source actions"
        );
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM social.notifications WHERE address=$1 AND type='ban_warning'",
        )
        .bind(&recipient)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 2);
        moderation
            .scene_ban(&scene, &recipient, &actor, &[], "Renamed Scene A")
            .await
            .unwrap();
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM social.notifications WHERE address=$1 AND type='user_banned_from_scene'").bind(&recipient).fetch_one(&pool).await.unwrap();
        assert_eq!(
            count, 2,
            "a later scene ban is a different durable transition"
        );
        assert!(
            !sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM social.notifications WHERE address=$1)"
            )
            .bind(&actor)
            .fetch_one(&pool)
            .await
            .unwrap()
        );
    }

    #[tokio::test]
    async fn notification_failure_rolls_back_bans_warnings_lifts_and_removal_intent() {
        let Some(pool) = pool().await else { return };
        let stub = Arc::new(crate::comms::interfaces::test_support::StubDirectory::default());
        stub.fail_moderation_notifications
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let failing = Moderation::with_social(pool.clone(), stub);
        let working = Moderation::with_social(
            pool.clone(),
            Arc::new(crate::comms_directory::Directory::new(pool.clone())),
        );
        let recipient = wallet();
        let actor = wallet();
        let scene = Uuid::new_v4().to_string();
        assert!(
            Moderation::new(pool.clone())
                .create_ban(ban(&recipient))
                .await
                .is_err(),
            "a read-only probe must not silently commit a notice-less ban"
        );
        // PostgreSQL rejects a JSONB \u0000 in the notice, after the real
        // scene-ban row and removal intent have been written in this transaction.
        assert!(
            working
                .scene_ban(&scene, &recipient, &actor, &[], "unpersistable\0title")
                .await
                .is_err()
        );
        assert!(failing.create_ban(ban(&recipient)).await.is_err());
        assert!(
            failing
                .create_warning(CreateWarning {
                    warned_address: recipient.clone(),
                    warned_by: actor.as_str().into(),
                    reason: "warning".into()
                })
                .await
                .is_err()
        );
        assert!(
            failing
                .scene_ban(&scene, &recipient, &actor, &[], "Scene")
                .await
                .is_err()
        );
        let counts: (i64,i64,i64,i64,i64) = sqlx::query_as("SELECT (SELECT count(*) FROM social.user_bans WHERE banned_address=$1),(SELECT count(*) FROM social.user_warnings WHERE warned_address=$1),(SELECT count(*) FROM social.scene_bans WHERE place_id=$2 AND banned_address=$1),(SELECT count(*) FROM social.livekit_removal_intents WHERE wallet=$1),(SELECT count(*) FROM social.notifications WHERE address=$1)").bind(&recipient).bind(&scene).fetch_one(&pool).await.unwrap();
        assert_eq!(counts, (0, 0, 0, 0, 0));
        working.create_ban(ban(&recipient)).await.unwrap();
        working
            .scene_ban(&scene, &recipient, &actor, &[], "Scene")
            .await
            .unwrap();
        assert!(failing.lift_ban(&recipient, actor.as_str()).await.is_err());
        assert!(
            failing
                .scene_unban(&scene, &recipient, "Scene")
                .await
                .is_err()
        );
        assert!(working.ban_status(&recipient).await.unwrap().is_banned);
        assert!(working.is_scene_banned(&scene, &recipient).await.unwrap());
        let counts: (i64,i64) = sqlx::query_as("SELECT (SELECT count(*) FROM social.livekit_removal_intents WHERE wallet=$1),(SELECT count(*) FROM social.notifications WHERE address=$1)").bind(&recipient).fetch_one(&pool).await.unwrap();
        assert_eq!(
            counts,
            (2, 2),
            "failed lifts preserve the ban and removal intent without a lifted notice"
        );
    }
}
