//! Scene and world identity, and who may moderate a scene.
//!
//! The old gatekeeper resolved a world's scene over HTTP (`/world/{name}/about`
//! of the worlds service), a parcel's scene over HTTP (`/content/entities/
//! active`), a place over the places database and ownership over the squid
//! index. On v2 the first three are the ONE database: a world's active scene
//! is the newest `worlds.world_scenes` row (what `/world/{name}/about` itself
//! serves as `scenesUrn[0]`), a parcel's scene is `content.active_pointers`,
//! a place is `explore.place`. The client and the authoritative host
//! therefore name the same room from the same active deployment
//! (DATABASE.md, "Presence is not proof of permission"). Ownership is read
//! from the marketplace subgraph through the marketplace module (ENS names
//! for a world, parcel and estate owners by coordinates for a Genesis
//! place, the old crate's squid `nft` lookups) through the worlds
//! permission core ([`crate::interfaces::AccessPolicy`]), which also answers world
//! access and the protected wallets of a world place where the old
//! gatekeeper fetched the worlds service's `/permissions` document; the
//! LAND operators still come from a configured upstream through the guarded
//! client, and an unreadable surface never clears a protection.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use sqlx::PgPool;
use sqlx::Row;

use crate::access::{Access, LandOperators, SceneRef, WorldAdmission};
use crate::shared::cache::TtlMap;
use crate::world_identity::{Parcel, ParcelSet, SceneId, WorldName, is_world_realm_name};
use crate::{Address, AuthorityNotEstablished, PermissionDecision};

use crate::gatekeeper::CommsState;
use crate::gatekeeper::errors::{ApiError, service_unavailable};
use crate::gatekeeper::livekit::{BANNED_ADDRESSES_FIELD, KICK_CONCURRENCY, SCENE_ADMINS_FIELD};
use crate::rooms::{ResolveError, ResolvedRoom, RoomRequest, gatekeeper_scene_room};

pub const WORLD_PERMISSIONS_UNAVAILABLE_MSG: &str = "the world permission store is unavailable, so this ban target cannot be checked for protection";
pub const WORLD_ACCESS_UNAVAILABLE_MSG: &str =
    "the world permission store is unavailable, so world access cannot be decided";
pub const LAND_OPERATORS_UNAVAILABLE_MSG: &str = "the land permission service is unavailable, so this ban target cannot be checked for protection";
pub const PLACE_LOOKUP_UNAVAILABLE_MSG: &str =
    "the place catalog is unavailable, so this request cannot be bound to its room";
const PLACE_MISMATCH_MSG: &str =
    "place_id does not match the realm and parcel this request is signed for";
pub const NOT_OWNER_OR_ADMIN_MSG: &str = "signer is not an owner or admin of this scene";

const LEASE_AUTHORIZATIONS_TTL: Duration = Duration::from_secs(300);
const NAMES_TTL: Duration = Duration::from_secs(300);
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(10);

#[path = "scene_lookup.rs"]
mod lookup;
#[cfg(test)]
use lookup::listing_key_candidate;
pub use lookup::{
    PlaceInfo, PlaceLookup, place_from_metadata, realm_name_from_metadata,
    resolve_listing_place_id, resolve_world_scene_id, try_load_place_info,
};
pub(crate) use lookup::{meta_str, participant_room, resolve_adapter_room};
#[path = "scene_policy.rs"]
mod policy;
pub(crate) use policy::try_get_lease_holders_for_parcels;
pub use policy::{
    SceneCaches, ensure_target_not_protected, get_extra_addresses, get_lease_holders_for_parcels,
    has_world_access_permission, names_for, scene_owner_or_admin_decision, try_get_extra_addresses,
};
#[path = "scene_effects.rs"]
mod effects;
pub use effects::{RoomContext, add_admin, add_ban, remove_admin, remove_ban, resolve_rooms};
pub(crate) use effects::{kick, live_world_rooms, readmit_world_rooms_in_inventory};
#[cfg(test)]
pub(crate) use effects::{readmit_until_settled, readmit_world_rooms};

#[cfg(test)]
use effects::{RoomPlan, context_matches_place, plan};
#[cfg(test)]
pub(crate) use lookup::test_support;
#[cfg(test)]
use policy::{parse_xy, try_fetch_land_operators};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interfaces::test_support::{StubPolicy, StubScenes};
    use serde_json::json;

    #[tokio::test]
    async fn land_authority_fault_is_503_and_missing_evidence_stays_distinct() {
        let state_with = |policy: StubPolicy| {
            CommsState::new(
                sqlx::PgPool::connect_lazy("postgres://postgres@127.0.0.1:1/postgres").unwrap(),
                crate::http::Outbound::for_tests(),
                crate::gatekeeper::Settings::for_tests(),
                None,
                None,
                Arc::new(policy),
                Arc::new(StubScenes::default()),
                Arc::new(crate::presence::Presence::new()),
                Arc::new(crate::interfaces::test_support::StubDirectory::default()),
            )
        };
        let no_source = state_with(StubPolicy::default());
        assert!(
            try_fetch_land_operators(&no_source, "1,2")
                .await
                .unwrap()
                .is_none()
        );
        let unknown = state_with(StubPolicy {
            land: Ok(Some(Arc::new(None))),
            ..Default::default()
        });
        assert!(
            try_fetch_land_operators(&unknown, "1,2")
                .await
                .unwrap()
                .unwrap()
                .is_none()
        );
        let unavailable = state_with(StubPolicy {
            land: Err(
                crate::shared::identity::AuthorityNotEstablished::UndeterminedStoreUnavailable {
                    store: "LAND operators reader",
                    reason_for_operators: "unavailable".into(),
                },
            ),
            ..Default::default()
        });
        let error = try_fetch_land_operators(&unavailable, "1,2")
            .await
            .unwrap_err();
        assert_eq!(error.code, 503);
        assert_eq!(error.message, LAND_OPERATORS_UNAVAILABLE_MSG);
    }

    #[test]
    fn parse_xy_handles_coords() {
        assert_eq!(parse_xy("-100,37"), Some((-100, 37)));
        assert_eq!(parse_xy(" 12 , -5 "), Some((12, -5)));
        assert_eq!(parse_xy("bad"), None);
        assert_eq!(parse_xy(""), None);
    }

    fn ctx(realm: Option<&str>, scene: Option<&str>) -> RoomContext {
        RoomContext {
            place_id: "place-1".into(),
            realm_name: realm.map(str::to_string),
            scene_id: scene.map(str::to_string),
            parcel: None,
        }
    }

    fn ctx_at(realm: Option<&str>, parcel: Option<&str>) -> RoomContext {
        RoomContext {
            parcel: parcel.map(str::to_string),
            ..ctx(realm, Some("bafkreiabc"))
        }
    }

    fn genesis_place(positions: &[&str]) -> PlaceInfo {
        PlaceInfo {
            world: false,
            world_name: None,
            positions: positions.iter().map(|p| p.to_string()).collect(),
            base_position: positions.first().map(|p| p.to_string()),
        }
    }

    fn world_place(name: &str) -> PlaceInfo {
        PlaceInfo {
            world: true,
            world_name: Some(name.into()),
            positions: vec!["0,0".into()],
            base_position: Some("0,0".into()),
        }
    }

    #[test]
    fn the_room_plan_matches_upstream_get_room_name() {
        assert_eq!(
            plan(&ctx(Some("main"), Some("bafkreiabc"))),
            RoomPlan::Scene("scene:main:bafkreiabc".into())
        );
        assert_eq!(
            plan(&ctx(Some("foo.dcl.eth"), Some("bafkreiabc"))),
            RoomPlan::World {
                world: "foo.dcl.eth".into(),
                scene_id: Some("bafkreiabc".into())
            }
        );
        assert_eq!(
            plan(&ctx(Some("foo.dcl.eth"), None)),
            RoomPlan::World {
                world: "foo.dcl.eth".into(),
                scene_id: None
            }
        );
        for realm in ["localpreview", "preview", "LocalPreview", "PREVIEW"] {
            assert_eq!(
                plan(&ctx(Some(realm), Some("b64-scene"))),
                RoomPlan::Scene(format!("scene:{realm}:b64-scene"))
            );
        }
        assert_eq!(plan(&ctx(Some("main"), None)), RoomPlan::Skip);
        assert_eq!(plan(&ctx(None, Some("bafkreiabc"))), RoomPlan::FromPlace);
        assert_eq!(plan(&ctx(None, None)), RoomPlan::FromPlace);
    }

    #[test]
    fn a_context_must_stand_on_the_authorized_place() {
        assert!(context_matches_place(
            &ctx_at(Some("Foo.dcl.eth"), None),
            &world_place("foo.dcl.eth")
        ));
        assert!(!context_matches_place(
            &ctx_at(Some("other.dcl.eth"), None),
            &world_place("foo.dcl.eth")
        ));
        assert!(!context_matches_place(
            &ctx_at(Some("foo.dcl.eth"), Some("10,20")),
            &genesis_place(&["10,20"])
        ));
        assert!(context_matches_place(
            &ctx_at(Some("main"), Some("10,21")),
            &genesis_place(&["10,20", "10,21"])
        ));
        assert!(!context_matches_place(
            &ctx_at(Some("main"), Some("99,99")),
            &genesis_place(&["10,20", "10,21"])
        ));
        assert!(!context_matches_place(
            &ctx_at(Some("main"), None),
            &genesis_place(&["10,20"])
        ));
        assert!(!context_matches_place(
            &ctx_at(Some("main"), Some("0,0")),
            &world_place("foo.dcl.eth")
        ));
        assert!(context_matches_place(
            &ctx_at(None, None),
            &genesis_place(&["10,20"])
        ));
    }

    #[test]
    fn context_reads_the_realm_and_scene_upstream_signs_over() {
        let flat = RoomContext::from_metadata(
            &json!({ "realmName": "main", "sceneId": "bafkreiabc" }),
            "place-1",
        );
        assert_eq!(plan(&flat), RoomPlan::Scene("scene:main:bafkreiabc".into()));
        let nested = RoomContext::from_metadata(
            &json!({ "realm": { "serverName": "main" }, "sceneId": "bafkreiabc" }),
            "place-1",
        );
        assert_eq!(
            plan(&nested),
            RoomPlan::Scene("scene:main:bafkreiabc".into())
        );
        assert_eq!(
            plan(&RoomContext::from_metadata(&json!({}), "place-1")),
            RoomPlan::FromPlace
        );
        let with_parcel = RoomContext::from_metadata(
            &json!({ "realmName": "main", "sceneId": "bafkreiabc", "parcel": "10,20" }),
            "place-1",
        );
        assert_eq!(with_parcel.parcel.as_deref(), Some("10,20"));
    }

    #[test]
    fn listing_key_and_place_from_metadata() {
        assert_eq!(
            listing_key_candidate(&json!({ "realmName": "foo.dcl.eth", "sceneId": "bafkreiabc" }))
                .as_deref(),
            Some("bafkreiabc")
        );
        assert_eq!(
            listing_key_candidate(&json!({ "realmName": "foo.dcl.eth", "sceneId": "foo.dcl.eth" }))
                .as_deref(),
            Some("foo.dcl.eth")
        );
        assert_eq!(
            listing_key_candidate(&json!({ "realmName": "foo.dcl.eth" })).as_deref(),
            Some("foo.dcl.eth")
        );
        assert_eq!(
            listing_key_candidate(&json!({ "realm": { "serverName": "bar.eth" } })).as_deref(),
            Some("bar.eth")
        );
        assert_eq!(
            listing_key_candidate(&json!({ "realmName": "main", "sceneId": "bafkreixyz" }))
                .as_deref(),
            Some("bafkreixyz")
        );
        assert_eq!(listing_key_candidate(&json!({})), None);
        assert_eq!(listing_key_candidate(&json!({ "realmName": "main" })), None);
        assert_eq!(
            place_from_metadata(&json!({ "realmName": "foo.dcl.eth", "sceneId": "x" })).as_deref(),
            Some("foo.dcl.eth")
        );
        assert_eq!(
            place_from_metadata(&json!({ "realmName": "main", "sceneId": "x" })).as_deref(),
            Some("x")
        );
    }

    /// The lease document is read only from `LEASE_AUTHORIZATIONS_URL`
    /// (the mirror posture's): without one no holder is listed and nothing
    /// is fetched.
    #[tokio::test]
    async fn lease_holders_come_only_from_the_configured_document() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let stub = axum::Router::new().route(
            "/authorizations.json",
            axum::routing::get(move || {
                counter.fetch_add(1, Ordering::SeqCst);
                async {
                    axum::Json(json!([
                        { "addresses": ["0xAAAAaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"], "plots": ["1,2"] },
                        { "addresses": ["0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"], "plots": ["9,9"] }
                    ]))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, stub).await.unwrap() });
        let state_with = |url: Option<String>| {
            let settings = crate::gatekeeper::Settings {
                lease_authorizations_url: url,
                ..crate::gatekeeper::Settings::for_tests()
            };
            let pool = sqlx::PgPool::connect_lazy("postgres://postgres@127.0.0.1:1/postgres")
                .expect("a lazy pool never connects");
            CommsState::new(
                pool,
                crate::http::Outbound::for_tests(),
                settings,
                None,
                None,
                Arc::new(StubPolicy::default()),
                Arc::new(StubScenes::default()),
                Arc::new(crate::presence::Presence::new()),
                Arc::new(crate::interfaces::test_support::StubDirectory::default()),
            )
        };
        let parcels = vec!["1,2".to_string(), "3,4".to_string()];

        let off = state_with(None);
        assert!(
            get_lease_holders_for_parcels(&off, &parcels)
                .await
                .is_empty()
        );
        assert_eq!(hits.load(Ordering::SeqCst), 0, "no document is fetched");

        let on = state_with(Some(format!("http://{addr}/authorizations.json")));
        assert_eq!(
            get_lease_holders_for_parcels(&on, &parcels).await,
            BTreeSet::from(["0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()])
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }
}
