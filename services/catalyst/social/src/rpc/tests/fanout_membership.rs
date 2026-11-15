//! Whole-audience failure boundaries executed from source updates.ts plus its
//! actual membership generator. A SQL fault lives only in this owned database.
use super::*;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[tokio::test]
async fn community_voice_membership_pages_match_executed_source_failure_boundaries() {
    let Some(database) = OwnedDatabase::new("fanout", crate::db::scratch::Schema::Full, 8).await
    else {
        return;
    };
    let pool = &database.pool;
    let source: Value = serde_json::from_str(include_str!(concat!(
        env!("DCL_ONE_ROOT"),
        "/services/catalyst/tests/fixtures/conformance/social/source-contracts/voice-membership.json"
    )))
    .unwrap();
    let community: Uuid = source["community"].as_str().unwrap().parse().unwrap();
    let creator = source["creator"].as_str().unwrap();
    let subscribers = source["subscribers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        subscribers.len(),
        514,
        "three target pages plus excluded creator"
    );
    sqlx::query(
        "INSERT INTO social.communities(id,name,owner_address) VALUES($1,'Membership boundary',$2)",
    )
    .bind(community)
    .bind(creator.to_lowercase())
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO social.community_members(community_id,member_address,role) SELECT $1,address,CASE WHEN address=$3 THEN 'owner' ELSE 'member' END FROM unnest($2::text[]) AS address")
        .bind(community).bind(&subscribers).bind(creator.to_lowercase()).execute(pool).await.unwrap();
    // Replacing this relation is safe only in the owned database. No global
    // pool state, production seam, migration or shared test relation changes.
    // nextval remains observable even when the selected page raises an error.
    sqlx::raw_sql("ALTER TABLE social.community_members RENAME TO fanout_member_rows;
        CREATE SEQUENCE social.fanout_member_reads;
        CREATE TABLE social.fanout_member_fault(fail_at bigint NOT NULL);
        INSERT INTO social.fanout_member_fault VALUES(0);
        CREATE FUNCTION social.fanout_member_page() RETURNS SETOF social.fanout_member_rows
        LANGUAGE plpgsql AS $$
        BEGIN
            IF nextval('social.fanout_member_reads')=(SELECT fail_at FROM social.fanout_member_fault) THEN
                RAISE EXCEPTION 'fixture later membership page failure';
            END IF;
            RETURN QUERY SELECT * FROM social.fanout_member_rows;
        END $$;
        CREATE VIEW social.community_members AS SELECT * FROM social.fanout_member_page();")
        .execute(pool).await.unwrap();
    for case in source["cases"].as_array().unwrap() {
        let name = case["id"].as_str().unwrap();
        sqlx::query("UPDATE social.communities SET private=$2 WHERE id=$1")
            .bind(community)
            .bind(case["private"].as_bool().unwrap())
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("UPDATE social.fanout_member_fault SET fail_at=$1")
            .bind(case["fail_at"].as_i64().unwrap())
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("ALTER SEQUENCE social.fanout_member_reads RESTART WITH 1")
            .execute(pool)
            .await
            .unwrap();
        let pubsub = crate::rpc::pubsub::PubSub::new();
        let mut receivers = subscribers
            .iter()
            .map(|address| (address, pubsub.subscribe(address)))
            .collect::<Vec<_>>();
        let update = CommunityVoiceChatUpdate::decode(
            hex::decode(case["input_hex"].as_str().unwrap())
                .unwrap()
                .as_slice(),
        )
        .unwrap();
        let scope = match case["notification_scope"].as_str().unwrap() {
            "all" => Scope::All,
            "members" => Scope::Members,
            other => panic!("unknown source scope {other}"),
        };
        pubsub
            .deliver_community_voice(pool, scope, Some(creator), update)
            .await;
        let mut groups = BTreeMap::<String, Vec<String>>::new();
        for (address, receiver) in &mut receivers {
            while let Ok(event) = receiver.try_recv() {
                let SocialEvent::CommunityVoice(update) = &*event else {
                    panic!("unexpected event in {name}")
                };
                groups
                    .entry(hex::encode(update.encode_to_vec()))
                    .or_default()
                    .push((*address).clone());
            }
        }
        let actual = groups
            .into_iter()
            .map(|(wire_hex, mut addresses)| {
                addresses.sort();
                json!({"wire_hex":wire_hex,"addresses":addresses})
            })
            .collect::<Vec<_>>();
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            case["emissions"],
            "{name}: every recipient and protobuf payload"
        );
        let (last, called): (i64, bool) =
            sqlx::query_as("SELECT last_value,is_called FROM social.fanout_member_reads")
                .fetch_one(pool)
                .await
                .unwrap();
        let reads = if called { last } else { 0 };
        // Physical query widths differ: source pages100 results, local probes
        //256 keys. Both must stop on the injected second failure, and public
        //ENDED must not access membership at all.
        let expected = if case["ended"] == true && scope == Scope::All {
            0
        } else if case["fail_at"] == 2 {
            2
        } else {
            3
        };
        assert_eq!(
            reads, expected,
            "{name}: later pages must not run after failure"
        );
        assert!(
            case["member_reads"]
                .as_array()
                .unwrap()
                .iter()
                .all(|read| read["emitted_before_read"] == 0 && read["includes_creator"] == false)
        );
    }
    database.drop().await;
}
