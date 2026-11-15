#![recursion_limit = "256"]
//! The explore parity suite: every entry of `tests/fixtures/explore_parity/*.fixture.json`
//! (what the old places and events services answer, derived from their pinned source)
//! is sent to the explore router over a seeded scratch database and compared per its
//! `body_match`. `known_gaps.json` is the ratchet: an entry listed there must still
//! fail and an entry not listed must pass, so the list only shrinks. The README beside
//! the fixtures documents the schema, the seed and how to regenerate the list.
//! Needs CATALYST_TEST_DATABASE_URL; skips without it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Request};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sqlx::PgPool;
use tower::ServiceExt;

use catalyst::auth::signed_fetch::build_payload_v6;
use catalyst::auth::{Wallet, create_simple_auth_chain};
use catalyst::explore::events::mirror as events_mirror;
use catalyst::explore::places::interactions as places_interactions;
use catalyst::explore::places::mirror as places_mirror;
use catalyst::explore::places::voting_power::MIN_USER_ACTIVITY;
use catalyst::explore::{ExploreState, Settings, router};

#[path = "support/db.rs"]
mod db;

const PLACES_ADMIN_TOKEN: &str = "harness-places-admin-token";
const DATA_TEAM_TOKEN: &str = "harness-data-team-token";
const EVENTS_ADMIN_TOKEN: &str = "harness-events-admin-token";

const PLACE_ENABLED: &str = "6b7c1d2e-0000-4000-8000-000000000001";
const PLACE_DISABLED: &str = "6b7c1d2e-0000-4000-8000-000000000002";
const PLACE_WORLD_BACKED: &str = "6b7c1d2e-0000-4000-8000-000000000003";
const PLACE_HIGHLIGHTED: &str = "6b7c1d2e-0000-4000-8000-000000000004";
const PLACE_EXCLUDED: &str = "6b7c1d2e-0000-4000-8000-000000000005";
const PLACE_MISSING: &str = "6b7c1d2e-0000-4000-8000-000000000009";
const ENTITY_MISSING: &str = "6b7c1d2e-0000-4000-8000-00000000000e";

const WORLD_LISTED: &str = "harbour.dcl.eth";
const WORLD_HIGHLIGHTED: &str = "lighthouse.dcl.eth";
const WORLD_EXCLUDED: &str = "quarry.dcl.eth";
const WORLD_MISSING: &str = "nowhere.dcl.eth";

const EVENT_APPROVED: &str = "9a1e0000-0000-4000-8000-000000000001";
const EVENT_PENDING: &str = "9a1e0000-0000-4000-8000-000000000002";
const EVENT_REJECTED: &str = "9a1e0000-0000-4000-8000-000000000003";
const EVENT_DELETED: &str = "9a1e0000-0000-4000-8000-000000000004";
const EVENT_PAST: &str = "9a1e0000-0000-4000-8000-000000000005";
const EVENT_ATTENDED: &str = "9a1e0000-0000-4000-8000-000000000006";
const EVENT_RECURRENT: &str = "9a1e0000-0000-4000-8000-000000000007";
const EVENT_COMMUNITY: &str = "9a1e0000-0000-4000-8000-000000000008";
const EVENT_MISSING: &str = "9a1e0000-0000-4000-8000-000000000009";
/// Happening now: the only event `list=live` answers.
const EVENT_LIVE: &str = "9a1e0000-0000-4000-8000-00000000000a";
/// Hosted in a world: the only event `world=true` answers.
const EVENT_WORLD: &str = "9a1e0000-0000-4000-8000-00000000000b";
const COMMUNITY_ID: &str = "c0a80000-0000-4000-8000-000000000001";

const SCHEDULE_EXISTING: &str = "5c3d0000-0000-4000-8000-000000000001";
const SCHEDULE_MISSING: &str = "5c3d0000-0000-4000-8000-000000000009";

/// Hardhat's published development keys: deterministic addresses, no value.
/// Every signing wallet but `no_profile` gets a profile deployment in the seed
/// (gatsby `withAuthProfile()` wants one); `no_profile` is the wallet without.
const KEYS: [(&str, &str); 11] = [
    (
        "edit_and_approve_any",
        "1111111111111111111111111111111111111111111111111111111111111111",
    ),
    (
        "user",
        "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
    ),
    (
        "owner",
        "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d",
    ),
    (
        "admin",
        "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a",
    ),
    (
        "edit_any",
        "7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6",
    ),
    (
        "approve_any",
        "47e179ec197488593b187f80a00eb0da91f1b9d0b13f8733639f19c30a34926a",
    ),
    (
        "approve_own",
        "8b3a350cf5c34c9194ca85829a2df0ec3153be0318b5e2d3348e872092edffba",
    ),
    (
        "edit_any_schedule",
        "92db14e403b83dfe3df233f83dfa3a0d7096f21ca9b0d6d6b8d88b2b4ec1564e",
    ),
    (
        "edit_any_profile",
        "4bbbf85ce3377467afe5d46f804f221813b2bb87f24d81f60f1fcdbf7cbf4356",
    ),
    (
        "subject",
        "dbda1821b80551c9d65939329250298aa3472ba22feea921c0cf5d620ea67b97",
    ),
    (
        "no_profile",
        "2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6",
    ),
];

/// The signing wallets whose profile deployment the seed holds.
const PROFILED_SIGNERS: [&str; 8] = [
    "user",
    "owner",
    "admin",
    "edit_any",
    "approve_any",
    "approve_own",
    "edit_any_schedule",
    "edit_any_profile",
];

struct Identities {
    wallets: BTreeMap<&'static str, Wallet>,
}

impl Identities {
    fn new() -> Self {
        let wallets = KEYS
            .iter()
            .map(|(name, key)| (*name, Wallet::from_hex(key).expect("test key")))
            .collect();
        Self { wallets }
    }

    fn wallet(&self, name: &str) -> &Wallet {
        self.wallets
            .get(name)
            .unwrap_or_else(|| panic!("no test identity {name}"))
    }

    fn address(&self, name: &str) -> String {
        self.wallet(name).address()
    }

    /// The wallet an `auth` value signs with, if it signs.
    fn signer_for(auth: &str) -> Option<&'static str> {
        Some(match auth {
            "signed:user" => "user",
            "signed:owner" => "owner",
            "signed:admin-wallet" => "admin",
            "signed:edit-any" => "edit_any",
            "signed:approve-any" => "approve_any",
            "signed:edit-and-approve-any" => "edit_and_approve_any",
            "signed:approve-own" => "approve_own",
            "signed:edit-any-schedule" => "edit_any_schedule",
            "signed:edit-any-profile" => "edit_any_profile",
            "signed:no-profile" => "no_profile",
            _ => return None,
        })
    }
}

// ── fixtures ───────────────────────────────────────────────────────────────

#[derive(Deserialize, Clone)]
struct Entry {
    id: String,
    request: RequestSpec,
    expected: Expected,
    #[serde(default)]
    harness: Harness,
    #[serde(default)]
    source: Source,
    /// The entry as written, so every placeholder it uses is checked up front.
    #[serde(skip)]
    raw: String,
}

#[derive(Deserialize, Clone, Default)]
struct Source {
    #[serde(default)]
    kind: String,
}

#[derive(Deserialize, Clone)]
struct RequestSpec {
    method: String,
    path: String,
    #[serde(default)]
    query: BTreeMap<String, String>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    body: Value,
    #[serde(default = "none_auth")]
    auth: String,
}

fn none_auth() -> String {
    "none".to_string()
}

#[derive(Deserialize, Clone)]
struct Expected {
    status: u16,
    #[serde(default)]
    content_type: Option<String>,
    #[serde(default)]
    body: Value,
    #[serde(default)]
    body_match: Option<String>,
    #[serde(default)]
    shape: Option<Shape>,
    #[serde(default)]
    headers: BTreeMap<String, String>,
}

#[derive(Deserialize, Clone, Default)]
struct Shape {
    #[serde(default)]
    envelope: Option<Vec<String>>,
    #[serde(default)]
    data_type: Option<String>,
    #[serde(default)]
    item_keys: Option<Vec<String>>,
    #[serde(default)]
    optional_keys: Vec<String>,
    #[serde(default)]
    data_subset: Option<Value>,
    #[serde(default)]
    invariants: Vec<String>,
    #[serde(default)]
    root: Option<String>,
    /// An empty `data` list is a mismatch unless the fixture says so here (or
    /// pins `data.length == 0`): an unseeded answer must not pass a list fixture.
    #[serde(default)]
    allow_empty: bool,
}

#[derive(Deserialize, Clone, Default)]
struct Harness {
    #[serde(default)]
    skip: Option<String>,
    #[serde(default)]
    place_id: Option<String>,
    #[serde(default)]
    world_id: Option<String>,
    #[serde(default)]
    event_id: Option<String>,
    #[serde(default)]
    schedule_id: Option<String>,
    #[serde(default)]
    address: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    signature: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    viewer_state: Vec<String>,
    #[serde(default)]
    multipart: Option<Multipart>,
}

#[derive(Deserialize, Clone)]
struct Multipart {
    #[serde(default = "poster_field")]
    field: String,
    kind: String,
}

fn poster_field() -> String {
    "poster".to_string()
}

#[derive(Deserialize, Default)]
struct KnownGaps {
    #[serde(default)]
    entries: Vec<KnownGap>,
}

#[derive(Deserialize, serde::Serialize)]
struct KnownGap {
    id: String,
    observed: String,
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/explore_parity")
}

fn load_entries() -> Vec<Entry> {
    let dir = fixtures_dir();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.to_string_lossy().ends_with(".fixture.json"))
        .collect();
    files.sort();
    let mut parsed = Vec::new();
    for file in files {
        let text =
            std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        let values: Vec<Value> = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{}: not a JSON array of entries: {e}", file.display()));
        parsed.push((file, values));
    }
    // A `$defs` entry holds a key set that other entries' `item_keys` name.
    let defs: BTreeMap<String, Value> = parsed
        .iter()
        .flat_map(|(_, values)| values)
        .filter(|v| is_definition(v.get("id").and_then(Value::as_str).unwrap_or_default()))
        .filter_map(|v| {
            let keys = v.pointer("/expected/body/item_keys")?.as_array()?;
            Some((v["id"].as_str()?.to_string(), Value::Array(keys.clone())))
        })
        .collect();
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for (file, values) in parsed {
        // Entry by entry, so a typo names the entry it sits in.
        for (i, mut value) in values.into_iter().enumerate() {
            let id = value
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("<entry #{i} without id>"));
            if let Some(Value::String(name)) = value.pointer("/expected/shape/item_keys") {
                let keys = defs
                    .get(name)
                    .unwrap_or_else(|| panic!("{id}: item_keys names no $defs entry: {name}"))
                    .clone();
                *value.pointer_mut("/expected/shape/item_keys").unwrap() = keys;
            }
            let raw = value.to_string();
            let mut entry: Entry = serde_json::from_value(value)
                .unwrap_or_else(|e| panic!("{}: entry {id}: {e}", file.display()));
            entry.raw = raw;
            assert!(
                seen.insert(entry.id.clone()),
                "duplicate fixture id {}",
                entry.id
            );
            out.push(entry);
        }
    }
    out
}

/// `known_gaps.json` as `id -> observed`; a repeated id is an error.
fn load_known_gaps() -> BTreeMap<String, String> {
    let path = fixtures_dir().join("known_gaps.json");
    let doc: KnownGaps = match std::fs::read_to_string(&path) {
        Ok(text) => {
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        }
        Err(_) => KnownGaps::default(),
    };
    let mut known = BTreeMap::new();
    for gap in doc.entries {
        assert!(
            known.insert(gap.id.clone(), gap.observed).is_none(),
            "known_gaps.json lists {} twice",
            gap.id
        );
    }
    known
}

// ── placeholders ───────────────────────────────────────────────────────────

struct Resolution {
    id: String,
    place_id: String,
    world_id: String,
    event_id: String,
    schedule_id: String,
    address: String,
    entity_id: String,
}

type Seed = fn(&str) -> Result<&'static str, String>;

fn place_for(kind: &str) -> Result<&'static str, String> {
    Ok(match kind {
        "enabled" => PLACE_ENABLED,
        "disabled" => PLACE_DISABLED,
        "world_backed" => PLACE_WORLD_BACKED,
        "highlighted" => PLACE_HIGHLIGHTED,
        "excluded" => PLACE_EXCLUDED,
        "missing" => PLACE_MISSING,
        other => return Err(format!("unknown place seed {other:?}")),
    })
}

fn world_for(kind: &str) -> Result<&'static str, String> {
    Ok(match kind {
        "listed" => WORLD_LISTED,
        "highlighted" => WORLD_HIGHLIGHTED,
        "excluded" => WORLD_EXCLUDED,
        "missing" => WORLD_MISSING,
        other => return Err(format!("unknown world seed {other:?}")),
    })
}

fn event_for(kind: &str) -> Result<&'static str, String> {
    Ok(match kind {
        "approved" => EVENT_APPROVED,
        "pending" => EVENT_PENDING,
        "rejected" => EVENT_REJECTED,
        "deleted" => EVENT_DELETED,
        "past" => EVENT_PAST,
        "attended" => EVENT_ATTENDED,
        "recurrent" => EVENT_RECURRENT,
        "community" => EVENT_COMMUNITY,
        "missing" => EVENT_MISSING,
        other => return Err(format!("unknown event seed {other:?}")),
    })
}

fn schedule_for(kind: &str) -> Result<&'static str, String> {
    Ok(match kind {
        "existing" => SCHEDULE_EXISTING,
        "missing" => SCHEDULE_MISSING,
        other => return Err(format!("unknown schedule seed {other:?}")),
    })
}

impl Resolution {
    /// Resolves the entry's `harness` seeds and checks every placeholder the
    /// entry uses, so a typo is reported against the entry instead of aborting
    /// the suite.
    fn new(entry: &Entry, ids: &Identities) -> Result<Self, String> {
        let h = &entry.harness;
        let signer = Identities::signer_for(&entry.request.auth).unwrap_or("user");
        let address = match h.address.as_deref().unwrap_or("subject") {
            "signer" => ids.address(signer),
            "subject" | "subject_with_row" => ids.address("subject"),
            other => return Err(format!("unknown address seed {other:?}")),
        };
        let res = Self {
            id: entry.id.clone(),
            place_id: place_for(h.place_id.as_deref().unwrap_or("enabled"))?.to_string(),
            world_id: world_for(h.world_id.as_deref().unwrap_or("listed"))?.to_string(),
            event_id: event_for(h.event_id.as_deref().unwrap_or("approved"))?.to_string(),
            schedule_id: schedule_for(h.schedule_id.as_deref().unwrap_or("existing"))?.to_string(),
            address,
            entity_id: ENTITY_MISSING.to_string(),
        };
        res.try_text(&entry.raw)?;
        Ok(res)
    }

    fn try_text(&self, s: &str) -> Result<String, String> {
        let mut out = s
            .replace("{place_id}", &self.place_id)
            .replace("{world_id}", &self.world_id)
            .replace("{event_id}", &self.event_id)
            .replace("{schedule_id}", &self.schedule_id)
            .replace("{address}", &self.address)
            .replace("{entity_id}", &self.entity_id);
        for (prefix, f) in [
            ("{place_id:", place_for as Seed),
            ("{world_id:", world_for),
            ("{event_id:", event_for),
            ("{schedule_id:", schedule_for),
        ] {
            while let Some(start) = out.find(prefix) {
                let rest = &out[start + prefix.len()..];
                let end = rest
                    .find('}')
                    .ok_or_else(|| format!("unclosed placeholder {prefix}"))?;
                let value = f(&rest[..end])?.to_string();
                out.replace_range(start..start + prefix.len() + end + 1, &value);
            }
        }
        Ok(out)
    }

    /// Infallible after `new` checked the whole entry.
    fn text(&self, s: &str) -> String {
        self.try_text(s)
            .unwrap_or_else(|e| panic!("{}: {e}", self.id))
    }

    fn value(&self, v: &Value) -> Value {
        match v {
            Value::String(s) => Value::String(self.text(s)),
            Value::Array(a) => Value::Array(a.iter().map(|x| self.value(x)).collect()),
            Value::Object(o) => Value::Object(
                o.iter()
                    .map(|(k, x)| (self.text(k), self.value(x)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

// ── seeding ────────────────────────────────────────────────────────────────

fn place_json(id: &str, base: &str, title: &str, categories: &[&str]) -> Value {
    let positions: Vec<String> = match base {
        "10,10" => vec!["10,10".into(), "10,11".into()],
        other => vec![other.to_string()],
    };
    json!({
        "id": id,
        "title": title,
        "description": format!("{title} description"),
        "image": "https://cdn.example/plaza.png",
        "owner": "0x00000000000000000000000000000000000000aa",
        "world_name": null,
        "content_rating": "T",
        "categories": categories,
        "likes": 4,
        "dislikes": 1,
        "favorites": 2,
        "like_rate": 0.8,
        "like_score": 0.5,
        "created_at": "2026-01-01T00:00:00.000Z",
        "updated_at": "2026-01-01T00:00:00.000Z",
        "disabled": false,
        "disabled_at": null,
        "disabled_reason": null,
        "highlighted_image": null,
        "positions": positions,
        "base_position": base,
        "contact_name": "Harness",
        "contact_email": null,
        "highlighted": false,
        "exclude_from_ranking": false,
        "world": false,
        "world_id": null,
        "deployment_id": "bafkreiharnessplaza000000000000000000000000000000000000000000",
        "deployed_at": "2026-01-01T00:00:00.000Z",
        "textsearch": null,
        "creator_address": "0x00000000000000000000000000000000000000aa",
        "sdk": "7",
        "ranking": 0,
        "user_count": 0,
        "user_visits": 0,
        "user_favorite": false,
        "user_like": false,
        "user_dislike": false
    })
}

fn world_json(name: &str, title: &str) -> Value {
    json!({
        "id": name,
        "title": title,
        "description": format!("{title} world"),
        "image": "https://cdn.example/world.png",
        "owner": "0x00000000000000000000000000000000000000bb",
        "world_name": name,
        "content_rating": "RP",
        "categories": ["social"],
        "likes": 1,
        "dislikes": 0,
        "favorites": 0,
        "like_rate": 1.0,
        "like_score": 0.2,
        "created_at": "2026-01-01T00:00:00.000Z",
        "updated_at": "2026-01-01T00:00:00.000Z",
        "show_in_places": true,
        "single_player": false,
        "skybox_time": null,
        "is_private": false,
        "highlighted": false,
        "highlighted_image": null,
        "ranking": 0,
        "exclude_from_ranking": false,
        "settings_version": 1,
        "disabled": false,
        "disabled_at": null,
        "contact_name": null,
        "base_position": "0,0",
        "user_favorite": false,
        "user_like": false,
        "user_dislike": false,
        "world": true,
        "user_visits": 0,
        "user_count": 0,
        "deployed_at": "2026-01-01T00:00:00.000Z"
    })
}

fn event_json(id: &str, owner: &str, start: &str, finish: &str) -> Value {
    json!({
        "id": id,
        "name": format!("Harness event {}", &id[id.len() - 1..]),
        "image": "https://cdn.example/poster.png",
        "image_vertical": null,
        "description": "A harness event",
        "contact": "harness@example.org",
        "details": "harness details",
        "start_at": start,
        "finish_at": finish,
        "next_start_at": start,
        "next_finish_at": finish,
        "duration": 3600000,
        "all_day": false,
        "x": 10,
        "y": 10,
        "server": null,
        "url": "https://decentraland.org/jump/events?position=10%2C10",
        "user": owner,
        "estate_id": null,
        "estate_name": null,
        "user_name": "Harness Owner",
        "approved": true,
        "rejected": false,
        "highlighted": false,
        "trending": false,
        "created_at": "2026-01-01T00:00:00.000Z",
        "updated_at": "2026-01-01T00:00:00.000Z",
        "recurrent": false,
        "recurrent_frequency": null,
        "recurrent_setpos": null,
        "recurrent_monthday": null,
        "recurrent_weekday_mask": 0,
        "recurrent_month_mask": 0,
        "recurrent_interval": 1,
        "recurrent_count": null,
        "recurrent_until": null,
        "recurrent_dates": [start],
        "total_attendees": 0,
        "latest_attendees": [],
        "categories": ["art"],
        "schedules": [],
        "approved_by": "admin",
        "rejected_by": null,
        "rejection_reason": null,
        "deleted_by_user": false,
        "deleted_by_admin": false,
        "deleted_by": null,
        "deleted_at": null,
        "deleted_reason": null,
        "world": false,
        "place_id": null,
        "community_id": null,
        "featured_item": null,
        "scene_name": null,
        "coordinates": [10, 10],
        "attending": false,
        "position": [10, 10],
        "live": false
    })
}

fn set(v: &mut Value, pairs: &[(&str, Value)]) {
    for (k, x) in pairs {
        v[*k] = x.clone();
    }
}

const RESET_TABLES: &[&str] = &[
    "explore.user_favorites",
    "explore.user_likes",
    "explore.place_interaction_counts",
    "explore.place_moderation",
    "explore.place_ranking",
    "explore.place_reports",
    "explore.place_world_local",
    "explore.place",
    "explore.event_attendance_local",
    "explore.event_moderation",
    "explore.events_local",
    "explore.event",
    "explore.schedules_local",
    "explore.moderators",
    "explore.event_profile_settings",
    "explore.event_profile_subscriptions",
    "explore.place_content_ratings",
    "explore.moderation_hooks",
    "explore.moderation_hook_activation",
    "explore.seen_nonces",
    "explore.signed_actions",
    "explore.poster",
];

/// A profile deployment this node holds for `address`, as the content
/// deployers store it: a `profile` entity whose pointer is the lowercase
/// address, read back by `/lambdas/profiles` and gatsby's `withAuthProfile()`
/// port (`explore/src/events/profiles.rs::local_profile`). The seed deletes
/// every profile deployment first, so the entity id may be random.
async fn deploy_profile(pool: &PgPool, address: &str) {
    let address = address.to_ascii_lowercase();
    let entity_id = format!("bafyprofile{}", uuid::Uuid::new_v4().simple());
    let metadata = json!({
        "v": {
            "avatars": [{
                "name": "Harness",
                "userId": address,
                "avatar": { "snapshots": { "face256": "", "body": "" } }
            }]
        }
    });
    sqlx::query(
        "INSERT INTO content.deployments (deployer_address, version, entity_type, entity_id, entity_metadata, entity_timestamp, entity_pointers, local_timestamp, auth_chain) \
         VALUES ($1, 'v3', 'profile', $2, $3::json, timezone('UTC', now()), ARRAY[$1], timezone('UTC', now()), '[]')",
    )
    .bind(&address)
    .bind(&entity_id)
    .bind(metadata)
    .execute(pool)
    .await
    .expect("seed profile deployment");
    sqlx::query(
        "INSERT INTO content.active_pointers (pointer, entity_id, entity_type) VALUES ($1, $2, 'profile')",
    )
    .bind(&address)
    .bind(&entity_id)
    .execute(pool)
    .await
    .expect("seed profile pointer");
}

/// Empties the explore tables and seeds the rows the placeholders name; `owner`
/// owns every seeded event, `viewer_state` adds the requesting wallet's facts
/// (its place facts on `place_id`, the `harness.place_id` row).
async fn reseed(
    pool: &PgPool,
    ids: &Identities,
    owner: &str,
    viewer: &str,
    place_id: &str,
    viewer_state: &[String],
) {
    for table in RESET_TABLES {
        sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM {table}")))
            .execute(pool)
            .await
            .unwrap_or_else(|e| panic!("reset {table}: {e}"));
    }
    for table in ["content.active_pointers", "content.deployments"] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE entity_type = 'profile'"
        )))
        .execute(pool)
        .await
        .unwrap_or_else(|e| panic!("reset {table}: {e}"));
    }
    for signer in PROFILED_SIGNERS {
        deploy_profile(pool, &ids.address(signer)).await;
    }
    let mut places = vec![
        place_json(PLACE_ENABLED, "10,10", "Harness Plaza", &["art", "poi"]),
        place_json(PLACE_DISABLED, "20,20", "Harness Closed", &["art"]),
        place_json(PLACE_WORLD_BACKED, "0,0", "Harbour Scene", &["social"]),
        place_json(PLACE_HIGHLIGHTED, "30,30", "Harness Featured", &["game"]),
        place_json(PLACE_EXCLUDED, "40,40", "Harness Excluded", &["art"]),
    ];
    set(
        &mut places[1],
        &[
            ("disabled", json!(true)),
            ("disabled_at", json!("2026-02-01T00:00:00.000Z")),
            ("disabled_reason", json!("moderation")),
        ],
    );
    set(
        &mut places[2],
        &[
            ("world", json!(true)),
            ("world_name", json!(WORLD_LISTED)),
            ("world_id", json!(WORLD_LISTED)),
        ],
    );
    set(
        &mut places[3],
        &[("highlighted", json!(true)), ("ranking", json!(100))],
    );
    set(&mut places[4], &[("exclude_from_ranking", json!(true))]);
    for place in &places {
        sqlx::query(
            "INSERT INTO explore.place (id, base_position, title, description, creator_address, content_rating, categories, likes, dislikes, favorites, deployed_at, disabled, highlighted, raw) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11::timestamptz, $12, $13, $14)",
        )
        .bind(place["id"].as_str().unwrap())
        .bind(place["base_position"].as_str().unwrap())
        .bind(place["title"].as_str().unwrap())
        .bind(place["description"].as_str().unwrap())
        .bind(place["owner"].as_str().unwrap())
        .bind(place["content_rating"].as_str().unwrap())
        .bind(
            place["categories"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c.as_str().unwrap().to_string())
                .collect::<Vec<String>>(),
        )
        .bind(place["likes"].as_i64().unwrap() as i32)
        .bind(place["dislikes"].as_i64().unwrap() as i32)
        .bind(place["favorites"].as_i64().unwrap() as i32)
        .bind(place["deployed_at"].as_str().unwrap())
        .bind(place["disabled"].as_bool().unwrap())
        .bind(place["highlighted"].as_bool().unwrap())
        .bind(place)
        .execute(pool)
        .await
        .expect("seed place");
    }
    let mut worlds = vec![
        world_json(WORLD_LISTED, "Harbour"),
        world_json(WORLD_HIGHLIGHTED, "Lighthouse"),
        world_json(WORLD_EXCLUDED, "Quarry"),
    ];
    set(&mut worlds[1], &[("highlighted", json!(true))]);
    set(&mut worlds[2], &[("exclude_from_ranking", json!(true))]);
    for world in &worlds {
        assert!(
            places_mirror::upsert_world(pool, world)
                .await
                .expect("seed world")
        );
    }

    let mut events = vec![
        event_json(
            EVENT_APPROVED,
            owner,
            "2031-01-01T10:00:00.000Z",
            "2031-01-01T11:00:00.000Z",
        ),
        event_json(
            EVENT_PENDING,
            owner,
            "2031-01-02T10:00:00.000Z",
            "2031-01-02T11:00:00.000Z",
        ),
        event_json(
            EVENT_REJECTED,
            owner,
            "2031-01-03T10:00:00.000Z",
            "2031-01-03T11:00:00.000Z",
        ),
        event_json(
            EVENT_DELETED,
            owner,
            "2031-01-04T10:00:00.000Z",
            "2031-01-04T11:00:00.000Z",
        ),
        event_json(
            EVENT_PAST,
            owner,
            "2020-01-01T10:00:00.000Z",
            "2020-01-01T11:00:00.000Z",
        ),
        event_json(
            EVENT_ATTENDED,
            owner,
            "2031-01-06T10:00:00.000Z",
            "2031-01-06T11:00:00.000Z",
        ),
        event_json(
            EVENT_RECURRENT,
            owner,
            "2031-02-01T10:00:00.000Z",
            "2031-02-22T11:00:00.000Z",
        ),
        event_json(
            EVENT_COMMUNITY,
            owner,
            "2031-01-08T10:00:00.000Z",
            "2031-01-08T11:00:00.000Z",
        ),
    ];
    let now = chrono::Utc::now();
    let stamp =
        |at: chrono::DateTime<chrono::Utc>| at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    events.push(event_json(
        EVENT_LIVE,
        owner,
        &stamp(now - chrono::Duration::minutes(30)),
        &stamp(now + chrono::Duration::minutes(30)),
    ));
    set(&mut events[8], &[("live", json!(true))]);
    events.push(event_json(
        EVENT_WORLD,
        owner,
        "2031-01-09T10:00:00.000Z",
        "2031-01-09T11:00:00.000Z",
    ));
    set(
        &mut events[9],
        &[
            ("world", json!(true)),
            ("server", json!("harbour.dcl.eth")),
            ("x", json!(0)),
            ("y", json!(0)),
            ("coordinates", json!([0, 0])),
            ("position", json!([0, 0])),
            (
                "url",
                json!("https://decentraland.org/jump/events?realm=harbour.dcl.eth"),
            ),
        ],
    );
    set(
        &mut events[1],
        &[("approved", json!(false)), ("approved_by", Value::Null)],
    );
    set(
        &mut events[2],
        &[
            ("approved", json!(false)),
            ("approved_by", Value::Null),
            ("rejected", json!(true)),
            ("rejected_by", json!("admin")),
            ("rejection_reason", json!("Spam")),
        ],
    );
    set(
        &mut events[3],
        &[
            ("deleted_by_user", json!(true)),
            ("deleted_by", json!(owner)),
            ("deleted_at", json!("2026-03-01T00:00:00.000Z")),
        ],
    );
    set(
        &mut events[5],
        &[
            ("total_attendees", json!(1)),
            ("latest_attendees", json!([viewer])),
        ],
    );
    set(
        &mut events[6],
        &[
            ("x", json!(30)),
            ("y", json!(30)),
            ("coordinates", json!([30, 30])),
            ("position", json!([30, 30])),
            ("recurrent", json!(true)),
            ("recurrent_frequency", json!("WEEKLY")),
            ("recurrent_count", json!(4)),
            (
                "recurrent_dates",
                json!([
                    "2031-02-01T10:00:00.000Z",
                    "2031-02-08T10:00:00.000Z",
                    "2031-02-15T10:00:00.000Z",
                    "2031-02-22T10:00:00.000Z"
                ]),
            ),
        ],
    );
    set(&mut events[7], &[("community_id", json!(COMMUNITY_ID))]);
    for event in &events {
        assert!(
            events_mirror::upsert_event(pool, event)
                .await
                .expect("seed event")
        );
        let id = event["id"].as_str().unwrap();
        sqlx::query("UPDATE explore.event SET origin = 'local' WHERE id = $1")
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO explore.events_local (id, signer, signed_payload) VALUES ($1, $2, '{\"create\": {}}')",
        )
        .bind(id)
        .bind(owner)
        .execute(pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO explore.event_attendance_local (event_id, signer, action) VALUES ($1, $2, 'going')",
    )
    .bind(EVENT_ATTENDED)
    .bind(ids.address("user"))
    .execute(pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO explore.schedules_local (id, name, description, image, theme, background, active_since, active_until, active) \
         VALUES ($1, 'Harness Season', NULL, NULL, NULL, '[]'::jsonb, '2026-01-01T00:00:00Z'::timestamptz, '2036-01-01T00:00:00Z'::timestamptz, true)",
    )
    .bind(SCHEDULE_EXISTING)
    .execute(pool)
    .await
    .unwrap();

    sqlx::query("INSERT INTO explore.moderators (address, added_at) VALUES ($1, 0)")
        .bind(ids.address("admin"))
        .execute(pool)
        .await
        .unwrap();
    for (name, permission) in [
        ("edit_any", "edit_any_event"),
        ("approve_any", "approve_any_event"),
        ("approve_own", "approve_own_event"),
        ("edit_any_schedule", "edit_any_schedule"),
        ("edit_any_profile", "edit_any_profile"),
    ] {
        sqlx::query(
            "INSERT INTO explore.event_profile_settings (\"user\", permissions, permissions_granted) VALUES ($1, jsonb_build_array($2::text), true)",
        )
        .bind(ids.address(name))
        .bind(permission)
        .execute(pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO explore.event_profile_settings (\"user\", email, email_verified, permissions, permissions_granted) VALUES ($1, 'subject@example.org', false, '[\"edit_any_event\"]'::jsonb, true)",
    )
    .bind(ids.address("subject"))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO explore.event_profile_settings (\"user\", permissions, permissions_granted) VALUES ($1, '[\"edit_any_event\",\"approve_any_event\"]'::jsonb, true)",
    )
    .bind(ids.address("edit_and_approve_any"))
    .execute(pool)
    .await
    .unwrap();

    for state in viewer_state {
        match state.as_str() {
            "favorited" => {
                sqlx::query(
                    "INSERT INTO explore.user_favorites (\"user\", entity_id) VALUES ($1, $2)",
                )
                .bind(viewer)
                .bind(place_id)
                .execute(pool)
                .await
                .unwrap();
                sqlx::query(
                    "INSERT INTO explore.place_interaction_counts (entity_id, favorites) VALUES ($1, 1)",
                )
                .bind(place_id)
                .execute(pool)
                .await
                .unwrap();
            }
            "liked" => {
                sqlx::query(
                    "INSERT INTO explore.user_likes (\"user\", entity_id, \"like\") VALUES ($1, $2, true)",
                )
                .bind(viewer)
                .bind(place_id)
                .execute(pool)
                .await
                .unwrap();
                sqlx::query(
                    "INSERT INTO explore.place_interaction_counts (entity_id, likes) VALUES ($1, 1)",
                )
                .bind(place_id)
                .execute(pool)
                .await
                .unwrap();
            }
            // A like by a wallet whose voting power reaches the rate threshold, written
            // as the PATCH writes it, so the counter row holds the recounted score the
            // place answers and the lists must order on.
            "liked_qualified" => {
                places_interactions::set_like(
                    pool,
                    place_id,
                    viewer,
                    Some(true),
                    MIN_USER_ACTIVITY,
                )
                .await
                .expect("qualified like");
            }
            "settings_row" => {
                sqlx::query(
                    "INSERT INTO explore.event_profile_settings (\"user\", email, email_verified) VALUES ($1, 'user@example.org', true) \
                     ON CONFLICT (\"user\") DO UPDATE SET email = EXCLUDED.email, email_verified = true",
                )
                .bind(viewer)
                .execute(pool)
                .await
                .unwrap();
            }
            "subscribed" => {
                sqlx::query(
                    "INSERT INTO explore.event_profile_subscriptions (endpoint, \"user\", p256dh, auth) VALUES ('https://push.example.org/harness', $1, 'harness-p256dh', 'harness-auth')",
                )
                .bind(viewer.to_ascii_lowercase())
                .execute(pool)
                .await
                .unwrap();
            }
            other => panic!("unknown viewer_state {other}"),
        }
    }
}

// ── request building ───────────────────────────────────────────────────────

fn signed_headers(headers: &mut HeaderMap, wallet: &Wallet, method: &str, path: &str, valid: bool) {
    let timestamp = chrono::Utc::now().timestamp_millis().to_string();
    let metadata = "{}";
    let mut payload = build_payload_v6(method, path, &timestamp, metadata);
    if !valid {
        payload.push_str(":tampered");
    }
    let chain = create_simple_auth_chain(wallet, &payload).expect("sign");
    for (i, link) in chain.as_array().unwrap().iter().enumerate() {
        headers.insert(
            HeaderName::from_bytes(format!("x-identity-auth-chain-{i}").as_bytes()).unwrap(),
            HeaderValue::from_str(&link.to_string()).unwrap(),
        );
    }
    headers.insert(
        "x-identity-timestamp",
        HeaderValue::from_str(&timestamp).unwrap(),
    );
    headers.insert("x-identity-metadata", HeaderValue::from_static("{}"));
}

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";
const JPEG: &[u8] = b"\xFF\xD8\xFF\xE0\0\x10JFIF\0";
const GIF: &[u8] = b"GIF89a";

fn multipart_body(spec: &Multipart) -> Result<(Vec<u8>, &'static str), String> {
    fn part(body: &mut Vec<u8>, field: &str, content_type: &str, data: &[u8]) {
        body.extend_from_slice(
            format!(
                "--b\r\nContent-Disposition: form-data; name=\"{field}\"; filename=\"p\"\r\nContent-Type: {content_type}\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(data);
        body.extend_from_slice(b"\r\n");
    }
    let mut body = Vec::new();
    let field = spec.field.as_str();
    match spec.kind.as_str() {
        "png" => part(
            &mut body,
            field,
            "image/png",
            &[PNG, &[7u8; 64][..]].concat(),
        ),
        "jpeg" => part(
            &mut body,
            field,
            "image/jpeg",
            &[JPEG, &[7u8; 64][..]].concat(),
        ),
        "gif" => part(
            &mut body,
            field,
            "image/gif",
            &[GIF, &[7u8; 64][..]].concat(),
        ),
        "oversize" => part(
            &mut body,
            field,
            "image/png",
            &[PNG, &vec![7u8; 600_000][..]].concat(),
        ),
        "garbage" => part(
            &mut body,
            field,
            "image/png",
            b"this is not an image at all",
        ),
        "empty" => part(&mut body, field, "image/png", b""),
        "none" => part(
            &mut body,
            "other",
            "image/png",
            &[PNG, &[7u8; 64][..]].concat(),
        ),
        "two_files" => {
            part(
                &mut body,
                field,
                "image/png",
                &[PNG, &[7u8; 64][..]].concat(),
            );
            part(
                &mut body,
                field,
                "image/png",
                &[PNG, &[9u8; 64][..]].concat(),
            );
        }
        other => return Err(format!("unknown multipart kind {other:?}")),
    }
    body.extend_from_slice(b"--b--\r\n");
    Ok((body, "multipart/form-data; boundary=b"))
}

fn generated_body(spec: &str) -> Result<Value, String> {
    match spec.split_once(':') {
        Some(("ranking_entries", n)) => {
            let n: usize = n
                .parse()
                .map_err(|e| format!("body generator {spec:?}: {e}"))?;
            let entries: Vec<Value> = (0..n)
                .map(|i| {
                    json!({
                        "entity_type": "place",
                        "id": format!("6b7c1d2e-0000-4000-8000-{i:012x}"),
                        "ranking": 1
                    })
                })
                .collect();
            Ok(json!({ "replaces": ["place"], "entries": entries }))
        }
        _ => Err(format!("unknown body generator {spec:?}")),
    }
}

/// Builds the request; an error is a fixture defect, reported against the entry.
fn build_request(
    entry: &Entry,
    res: &Resolution,
    ids: &Identities,
) -> Result<Request<Body>, String> {
    let spec = &entry.request;
    let path = res.text(&spec.path);
    let (path_only, mut query_pairs): (String, Vec<(String, String)>) = match path.split_once('?') {
        Some((p, q)) => (
            p.to_string(),
            q.split('&')
                .filter(|s| !s.is_empty())
                .map(|kv| match kv.split_once('=') {
                    Some((k, v)) => (k.to_string(), v.to_string()),
                    None => (kv.to_string(), String::new()),
                })
                .collect(),
        ),
        None => (path.clone(), Vec::new()),
    };
    for (k, v) in &spec.query {
        query_pairs.push((
            urlencoding::encode(&res.text(k)).into_owned(),
            urlencoding::encode(&res.text(v)).into_owned(),
        ));
    }
    let uri = if query_pairs.is_empty() {
        path_only.clone()
    } else {
        let qs: Vec<String> = query_pairs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        format!("{path_only}?{}", qs.join("&"))
    };
    let method = spec.method.to_uppercase();
    let mut headers = HeaderMap::new();
    for (k, v) in &spec.headers {
        if v.starts_with('<') {
            continue;
        }
        headers.insert(
            HeaderName::from_bytes(k.to_lowercase().as_bytes())
                .map_err(|e| format!("header name {k:?}: {e}"))?,
            HeaderValue::from_str(&res.text(v)).map_err(|e| format!("header {k}: {e}"))?,
        );
    }
    let valid = entry.harness.signature.as_deref() != Some("invalid");
    match spec.auth.as_str() {
        "none" => {}
        "bearer:places-admin" => bearer(&mut headers, PLACES_ADMIN_TOKEN),
        "bearer:data-team" => bearer(&mut headers, DATA_TEAM_TOKEN),
        "bearer:events-admin" => bearer(&mut headers, EVENTS_ADMIN_TOKEN),
        "bearer:wrong" | "bearer:unconfigured" => {
            if !headers.contains_key("authorization") {
                bearer(&mut headers, "wrong");
            }
        }
        auth => {
            let signer =
                Identities::signer_for(auth).ok_or_else(|| format!("unknown auth {auth:?}"))?;
            signed_headers(&mut headers, ids.wallet(signer), &method, &path_only, valid);
        }
    }

    let body = match (&entry.harness.multipart, &entry.harness.body, &spec.body) {
        (Some(mp), _, _) => {
            let (bytes, ct) = multipart_body(mp)?;
            headers.insert("content-type", HeaderValue::from_static(ct));
            Body::from(bytes)
        }
        (None, Some(generator), _) => {
            headers.insert("content-type", HeaderValue::from_static("application/json"));
            Body::from(generated_body(generator)?.to_string())
        }
        (None, None, Value::Null) => Body::empty(),
        (None, None, Value::String(s)) if s.starts_with('<') => Body::empty(),
        (None, None, Value::String(s)) => Body::from(res.text(s)),
        (None, None, other) => {
            if !headers.contains_key("content-type") {
                headers.insert("content-type", HeaderValue::from_static("application/json"));
            }
            Body::from(res.value(other).to_string())
        }
    };
    let mut builder = Request::builder().method(method.as_str()).uri(&uri);
    for (k, v) in &headers {
        builder = builder.header(k, v);
    }
    builder
        .body(body)
        .map_err(|e| format!("request {method} {uri}: {e}"))
}

fn bearer(headers: &mut HeaderMap, token: &str) {
    headers.insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );
}

// ── comparison ─────────────────────────────────────────────────────────────

struct Answer {
    status: u16,
    headers: BTreeMap<String, String>,
    text: String,
    json: Option<Value>,
}

fn is_wildcard(v: &Value) -> bool {
    matches!(v, Value::String(s) if s.starts_with('<') && s.ends_with('>'))
}

/// `"Invalid signature: <verifier message>"`, `"poster/<uuid>.png"`: the literal
/// pieces around `<...>` wildcards must appear in `actual` in order, the first as
/// a prefix (trailing blanks ignored) and the last as a suffix. `None` when the
/// string holds no wildcard and compares literally.
fn pattern_match(expected: &str, actual: &str) -> Option<bool> {
    let mut literals = Vec::new();
    let mut rest = expected;
    while let Some(open) = rest.find('<') {
        let Some(close) = rest[open..].find('>') else {
            break;
        };
        literals.push(&rest[..open]);
        rest = &rest[open + close + 1..];
    }
    if literals.is_empty() {
        return None;
    }
    literals.push(rest);
    let last = literals.len() - 1;
    let mut pos = 0;
    for (i, lit) in literals.iter().enumerate() {
        let lit = if i == 0 { lit.trim_end() } else { lit };
        if lit.is_empty() {
            continue;
        }
        if i == 0 {
            if !actual.starts_with(lit) {
                return Some(false);
            }
            pos = lit.len();
        } else if i == last {
            if !actual.ends_with(lit) || actual.len() - lit.len() < pos {
                return Some(false);
            }
        } else {
            match actual[pos..].find(lit) {
                Some(at) => pos += at + lit.len(),
                None => return Some(false),
            }
        }
    }
    Some(true)
}

fn json_eq(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) if a.is_number() && b.is_number() => x == y,
        _ => a == b,
    }
}

fn subset(expected: &Value, actual: &Value, path: &str, out: &mut Vec<String>) {
    if is_wildcard(expected) {
        return;
    }
    match (expected, actual) {
        (Value::Object(e), Value::Object(a)) => {
            for (k, ev) in e {
                match a.get(k) {
                    Some(av) => subset(ev, av, &format!("{path}.{k}"), out),
                    None => out.push(format!("{path}.{k} missing")),
                }
            }
        }
        (Value::Array(e), Value::Array(a)) => {
            if e.len() != a.len() {
                out.push(format!(
                    "{path} has {} items, expected {}",
                    a.len(),
                    e.len()
                ));
                return;
            }
            for (i, (ev, av)) in e.iter().zip(a).enumerate() {
                subset(ev, av, &format!("{path}[{i}]"), out);
            }
        }
        _ => {
            if let (Some(pattern), Some(text)) = (expected.as_str(), actual.as_str())
                && let Some(matched) = pattern_match(pattern, text)
            {
                if !matched {
                    out.push(format!(
                        "{path} = {} (expected a value matching {pattern:?})",
                        truncate(&actual.to_string())
                    ));
                }
            } else if !json_eq(expected, actual) {
                out.push(format!(
                    "{path} = {} (expected {})",
                    truncate(&actual.to_string()),
                    truncate(&expected.to_string())
                ));
            }
        }
    }
}

fn truncate(s: &str) -> String {
    if s.chars().count() > 160 {
        let cut: String = s.chars().take(160).collect();
        format!("{cut}…")
    } else {
        s.to_string()
    }
}

fn key_set(v: &Value) -> Option<BTreeSet<String>> {
    v.as_object().map(|o| o.keys().cloned().collect())
}

fn check_keys(
    actual: &Value,
    expected: &[String],
    optional: &[String],
    path: &str,
    out: &mut Vec<String>,
) {
    let Some(keys) = key_set(actual) else {
        out.push(format!("{path} is not an object"));
        return;
    };
    let expected: BTreeSet<String> = expected.iter().cloned().collect();
    let optional: BTreeSet<String> = optional.iter().cloned().collect();
    let missing: Vec<&String> = expected.difference(&keys).collect();
    let extra: Vec<&String> = keys
        .difference(&expected)
        .filter(|k| !optional.contains(*k))
        .collect();
    if !missing.is_empty() {
        out.push(format!("{path} missing keys {missing:?}"));
    }
    if !extra.is_empty() {
        out.push(format!("{path} extra keys {extra:?}"));
    }
}

fn get_path<'a>(root: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = root;
    for seg in path.split('.') {
        if seg.is_empty() {
            continue;
        }
        cur = match cur {
            Value::Object(o) => o.get(seg)?,
            Value::Array(a) => a.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

fn parse_literal(s: &str) -> Option<Value> {
    let s = s.trim();
    if let Some(inner) = s.strip_prefix('\'').and_then(|r| r.strip_suffix('\'')) {
        return Some(Value::String(inner.to_string()));
    }
    serde_json::from_str(s).ok()
}

fn strip_trailing_note(s: &str) -> &str {
    match s.find(" (") {
        Some(i) => s[..i].trim(),
        None => s.trim(),
    }
}

/// Invariants starting with this are documentation and are not evaluated.
const DOC_PREFIX: &str = "doc:";

fn is_path(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

/// Splits `lhs <op> rhs` on the first of `ops` (longest first) found with
/// surrounding blanks.
fn split_op<'a>(s: &'a str, ops: &[&'static str]) -> Option<(&'a str, &'static str, &'a str)> {
    ops.iter()
        .filter_map(|op| s.find(&format!(" {op} ")).map(|at| (at, *op)))
        .min_by_key(|(at, _)| *at)
        .map(|(at, op)| (&s[..at], op, &s[at + op.len() + 2..]))
}

fn compare_op(op: &str, actual: &Value, lit: &Value) -> bool {
    match op {
        "==" | "===" => json_eq(actual, lit),
        _ => !json_eq(actual, lit),
    }
}

fn count_op(op: &str, actual: i64, n: i64) -> Option<bool> {
    Some(match op {
        "==" => actual == n,
        "!=" => actual != n,
        ">=" => actual >= n,
        "<=" => actual <= n,
        "<" => actual < n,
        ">" => actual > n,
        _ => return None,
    })
}

const EQ_OPS: [&str; 4] = ["!==", "===", "!=", "=="];

/// Evaluates one invariant in the grammar the fixtures README lists. Anything
/// else fails the entry: documentation goes in `notes` or behind `doc:`, so a
/// mistyped check can never pass silently. A missing path or a wrong type fails too.
fn check_invariant(inv: &str, body: &Value, text: &str) -> Result<(), String> {
    let inv = inv.trim();
    if inv.starts_with(DOC_PREFIX) {
        return Ok(());
    }
    let unparseable = || {
        Err(format!(
            "not in the README grammar (documentation goes in notes or behind {DOC_PREFIX:?})"
        ))
    };
    let data = || get_path(body, "data").ok_or_else(|| "data missing".to_string());
    let items = || {
        data()?
            .as_array()
            .ok_or_else(|| "data is not an array".to_string())
    };
    if let Some(rest) = inv.strip_prefix("body contains ") {
        let Some(Value::String(needle)) = parse_literal(rest.trim()) else {
            return unparseable();
        };
        return if text.contains(needle.as_str()) {
            Ok(())
        } else {
            Err(format!("body does not contain {needle:?}"))
        };
    }
    if let Some(rest) = inv.strip_prefix("data.length ") {
        let Some((op, n)) = rest.split_once(' ') else {
            return unparseable();
        };
        let Ok(n) = strip_trailing_note(n).parse::<i64>() else {
            return unparseable();
        };
        let len = items()?.len() as i64;
        return match count_op(op, len, n) {
            Some(true) => Ok(()),
            Some(false) => Err(format!("data.length is {len}, expected {op} {n}")),
            None => unparseable(),
        };
    }
    if let Some(rest) = inv.strip_prefix("total ") {
        let Some((op, n)) = rest.split_once(' ') else {
            return unparseable();
        };
        let Ok(n) = strip_trailing_note(n).parse::<i64>() else {
            return unparseable();
        };
        let total = get_path(body, "total").ok_or_else(|| "total missing".to_string())?;
        let total = total
            .as_i64()
            .ok_or_else(|| format!("total is not an integer: {total}"))?;
        return match count_op(op, total, n) {
            Some(true) => Ok(()),
            Some(false) => Err(format!("total is {total}, expected {op} {n}")),
            None => unparseable(),
        };
    }
    if let Some(rest) = inv.strip_prefix("every item.") {
        // every item.<path> non-increasing: an order check; null sorts last (NULLS LAST).
        if let Some(key) = strip_trailing_note(rest).strip_suffix(" non-increasing") {
            if !is_path(key) {
                return unparseable();
            }
            let mut previous = f64::INFINITY;
            for (i, item) in items()?.iter().enumerate() {
                let value = match get_path(item, key) {
                    Some(Value::Null) => f64::NEG_INFINITY,
                    Some(v) => v
                        .as_f64()
                        .ok_or_else(|| format!("data[{i}].{key} is not a number: {v}"))?,
                    None => return Err(format!("data[{i}].{key} missing")),
                };
                if value > previous {
                    return Err(format!(
                        "data[{i}].{key} = {value} rises above data[{}].{key} = {previous}",
                        i - 1
                    ));
                }
                previous = value;
            }
            return Ok(());
        }
        let Some((key, op, lit)) = split_op(rest, &EQ_OPS) else {
            return unparseable();
        };
        let Some(lit) = parse_literal(strip_trailing_note(lit)) else {
            return unparseable();
        };
        if !is_path(key) {
            return unparseable();
        }
        for (i, item) in items()?.iter().enumerate() {
            let actual = get_path(item, key).ok_or_else(|| format!("data[{i}].{key} missing"))?;
            if !compare_op(op, actual, &lit) {
                return Err(format!("data[{i}].{key} = {actual}, expected {op} {lit}"));
            }
        }
        return Ok(());
    }
    if inv.starts_with('\'') {
        // 'key' in data / 'key' not in data, several joined by " and " or ", "
        let mut all = Ok(());
        for clause in inv.split(" and ").flat_map(|c| c.split(", ")) {
            let clause = strip_trailing_note(clause);
            let Some((key, tail)) = clause.strip_prefix('\'').and_then(|c| c.split_once('\''))
            else {
                return unparseable();
            };
            let wanted = match tail.trim() {
                "in data" => true,
                "not in data" => false,
                _ => return unparseable(),
            };
            let present = data()?
                .as_object()
                .ok_or_else(|| "data is not an object".to_string())?
                .contains_key(key);
            if present != wanted {
                all = Err(if wanted {
                    format!("'{key}' not in data")
                } else {
                    format!("'{key}' in data")
                });
            }
        }
        return all;
    }
    if let Some(rest) = inv.strip_prefix("data.") {
        if let Some((path, keys)) = rest.split_once(" keys: ") {
            if !is_path(path) {
                return unparseable();
            }
            let expected: Vec<String> = keys.split(',').map(|k| k.trim().to_string()).collect();
            let target = get_path(body, &format!("data.{path}"))
                .ok_or_else(|| format!("data.{path} missing"))?;
            let mut out = Vec::new();
            check_keys(target, &expected, &[], &format!("data.{path}"), &mut out);
            return if out.is_empty() {
                Ok(())
            } else {
                Err(out.join("; "))
            };
        }
        // data.<path> <op> <literal>, several joined by " && "
        for clause in inv.split(" && ") {
            let Some((path, op, lit)) = clause
                .strip_prefix("data.")
                .and_then(|c| split_op(c, &EQ_OPS))
            else {
                return unparseable();
            };
            if !is_path(path) {
                return unparseable();
            }
            let Some(lit) = parse_literal(strip_trailing_note(lit)) else {
                return unparseable();
            };
            let actual = get_path(body, &format!("data.{path}"))
                .ok_or_else(|| format!("data.{path} missing"))?;
            if !compare_op(op, actual, &lit) {
                return Err(format!(
                    "data.{path} = {}, expected {op} {lit}",
                    truncate(&actual.to_string())
                ));
            }
        }
        return Ok(());
    }
    unparseable()
}

/// Whether the invariants pin the list empty, so an empty `data` is the expectation.
fn declares_empty(invariants: &[String]) -> bool {
    invariants.iter().any(|inv| {
        matches!(
            strip_trailing_note(inv),
            "data.length == 0" | "data.length <= 0" | "data.length < 1" | "total == 0"
        )
    })
}

/// Reduces an HTML answer to `{"title", "meta": {name-or-property: content}}`.
/// The title element may carry attributes: gatsby's `replaceHelmetMetadata`
/// stamps `data-react-helmet="true"` on it (`Gatsby/utils.js:82-85`), as the
/// router does.
fn html_meta(text: &str) -> Value {
    let mut meta = Map::new();
    let title = text
        .find("<title")
        .and_then(|s| {
            let rest = &text[s + 6..];
            let open = rest.find('>')?;
            let inner = &rest[open + 1..];
            inner.find("</title>").map(|e| &inner[..e])
        })
        .unwrap_or("")
        .trim()
        .to_string();
    for tag in text.split("<meta ").skip(1) {
        let tag = &tag[..tag.find('>').unwrap_or(tag.len())];
        let attr = |name: &str| -> Option<String> {
            let start = tag.find(&format!("{name}=\""))? + name.len() + 2;
            let end = tag[start..].find('"')?;
            Some(tag[start..start + end].to_string())
        };
        let key = attr("property").or_else(|| attr("name"));
        if let (Some(key), Some(content)) = (key, attr("content")) {
            meta.insert(key, Value::String(content));
        }
    }
    json!({ "title": title, "meta": meta })
}

fn compare(entry: &Entry, res: &Resolution, answer: &Answer) -> Vec<String> {
    let exp = &entry.expected;
    let mut out = Vec::new();
    if answer.status != exp.status {
        out.push(format!(
            "status {} (expected {}): {}",
            answer.status,
            exp.status,
            truncate(&answer.text)
        ));
        return out;
    }
    if let Some(ct) = &exp.content_type {
        let actual = answer
            .headers
            .get("content-type")
            .cloned()
            .unwrap_or_default();
        if !actual.starts_with(ct.as_str()) {
            out.push(format!("content-type {actual:?} (expected {ct}*)"));
        }
    }
    for (k, v) in &exp.headers {
        let v = res.text(v);
        match answer.headers.get(&k.to_lowercase()) {
            Some(actual) if *actual == v => {}
            Some(actual) => out.push(format!("header {k} = {actual:?} (expected {v:?})")),
            None => out.push(format!("header {k} missing (expected {v:?})")),
        }
    }
    let expected_body = res.value(&exp.body);
    let is_html = exp
        .content_type
        .as_deref()
        .is_some_and(|c| c.starts_with("text/html"));
    // A `text/plain` or `text/html` answer that is not JSON (express's redirect
    // page, for one) is compared as text: `subset` and `shape` never parse it,
    // `body contains '<text>'` reads it, and an expected JSON body is a mismatch.
    let is_text = !is_html
        && answer.json.is_none()
        && answer
            .headers
            .get("content-type")
            .is_some_and(|c| c.starts_with("text/"));
    let body = if is_html {
        html_meta(&answer.text)
    } else {
        answer.json.clone().unwrap_or(Value::Null)
    };
    match exp.body_match.as_deref().unwrap_or("exact") {
        "exact" => {
            if expected_body.is_string() && answer.json.is_none() {
                if expected_body.as_str() != Some(answer.text.as_str()) {
                    out.push(format!(
                        "body {:?} (expected {:?})",
                        truncate(&answer.text),
                        expected_body
                    ));
                }
            } else if answer.json.is_none() && !is_html {
                out.push(format!("body is not JSON: {}", truncate(&answer.text)));
            } else if !json_eq(&expected_body, &body) {
                let mut diffs = Vec::new();
                subset(&expected_body, &body, "body", &mut diffs);
                if let (Some(e), Some(a)) = (key_set(&expected_body), key_set(&body)) {
                    let extra: Vec<&String> = a.difference(&e).collect();
                    if !extra.is_empty() {
                        diffs.push(format!("body extra keys {extra:?}"));
                    }
                }
                if diffs.is_empty() {
                    diffs.push(format!(
                        "body {} (expected {})",
                        truncate(&body.to_string()),
                        truncate(&expected_body.to_string())
                    ));
                }
                out.extend(diffs);
            }
        }
        "subset" => {
            if is_text {
                match &expected_body {
                    Value::Null => {}
                    Value::String(s) if answer.text.contains(s.as_str()) => {}
                    other => out.push(format!(
                        "text body {:?} (expected {})",
                        truncate(&answer.text),
                        truncate(&other.to_string())
                    )),
                }
            } else if answer.json.is_none() && !is_html {
                out.push(format!("body is not JSON: {}", truncate(&answer.text)));
            } else {
                subset(&expected_body, &body, "body", &mut out);
            }
        }
        "shape" => {
            let shape = exp.shape.clone().unwrap_or_default();
            if let Some(root) = &shape.root {
                let text = answer.text.trim_start();
                let after_decl = match text.strip_prefix("<?xml") {
                    Some(rest) => rest.find("?>").map(|i| &rest[i + 2..]).unwrap_or(rest),
                    None => text,
                };
                let name: String = after_decl
                    .trim_start()
                    .trim_start_matches('<')
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect();
                if &name != root {
                    out.push(format!("xml root <{name}> (expected <{root}>)"));
                }
            } else if answer.json.is_none() && !is_text {
                out.push(format!("body is not JSON: {}", truncate(&answer.text)));
                return out;
            }
            if let Some(envelope) = &shape.envelope {
                check_keys(&body, envelope, &[], "body", &mut out);
            }
            // Top-level values other than `data` (`ok`, `total`, ...) must be
            // present and equal; `data` follows below.
            if let Some(obj) = expected_body.as_object() {
                for (k, v) in obj {
                    if k == "data" || is_wildcard(v) {
                        continue;
                    }
                    match body.get(k) {
                        Some(actual) => subset(v, actual, &format!("body.{k}"), &mut out),
                        None => out.push(format!("body.{k} missing")),
                    }
                }
            }
            let expected_data = expected_body.get("data").filter(|v| !is_wildcard(v));
            let wants_data = shape.data_type.is_some()
                || shape.item_keys.is_some()
                || shape.data_subset.is_some()
                || shape.allow_empty
                || expected_data.is_some();
            let data = body.get("data");
            if let Some(kind) = &shape.data_type {
                let ok = matches!(
                    (kind.as_str(), data),
                    ("array", Some(Value::Array(_)))
                        | ("object", Some(Value::Object(_)))
                        | ("null", Some(Value::Null))
                );
                if !ok {
                    out.push(format!(
                        "data is not {kind}: {}",
                        truncate(&data.map(|d| d.to_string()).unwrap_or_default())
                    ));
                }
            }
            if let Some(data) = data {
                // `expected.body.data` is a subset match on the listed keys and
                // values (wildcards and patterns as in `subset`); an empty array
                // only says "an array", whose items `item_keys`, `data_subset` and
                // the invariants describe.
                if let Some(ed) = expected_data {
                    match (ed, data) {
                        (Value::Array(e), Value::Array(_)) if e.is_empty() => {}
                        _ => subset(ed, data, "body.data", &mut out),
                    }
                }
                if let Some(item_keys) = &shape.item_keys {
                    match data {
                        Value::Array(items) => {
                            for (i, item) in items.iter().enumerate() {
                                check_keys(
                                    item,
                                    item_keys,
                                    &shape.optional_keys,
                                    &format!("data[{i}]"),
                                    &mut out,
                                );
                                if out.len() > 6 {
                                    break;
                                }
                            }
                        }
                        Value::Object(_) => {
                            check_keys(data, item_keys, &shape.optional_keys, "data", &mut out)
                        }
                        other => out.push(format!(
                            "data is {} (expected items)",
                            truncate(&other.to_string())
                        )),
                    }
                }
                if let Some(sub) = &shape.data_subset {
                    let sub = res.value(sub);
                    match data {
                        Value::Array(items) => {
                            for (i, item) in items.iter().enumerate() {
                                subset(&sub, item, &format!("data[{i}]"), &mut out);
                            }
                        }
                        other => subset(&sub, other, "data", &mut out),
                    }
                }
                if let Value::Array(items) = data
                    && items.is_empty()
                    && !shape.allow_empty
                    && !declares_empty(&shape.invariants)
                {
                    out.push(
                        "data is empty: a list fixture needs rows (declare shape.allow_empty or pin data.length == 0 when empty is the expectation)"
                            .to_string(),
                    );
                }
            } else if wants_data {
                out.push("body.data missing".to_string());
            }
            for inv in &shape.invariants {
                let inv = res.text(inv);
                if let Err(e) = check_invariant(&inv, &body, &answer.text) {
                    out.push(format!("invariant {inv:?}: {e}"));
                }
            }
        }
        other => out.push(format!("unknown body_match {other}")),
    }
    out.dedup();
    out
}

// ── the run ────────────────────────────────────────────────────────────────

enum Outcome {
    Pass,
    Fail(String),
    Skip(String),
    /// A defect in the fixture itself (typo, unknown seed, unexpected skip):
    /// never a known gap, always fails the suite.
    Invalid(String),
}

/// The entries whose `harness.skip` the suite accepts besides the `$defs`
/// entries (README "Skipped and accepted entries"); any other skip is a
/// fixture defect.
const SKIPPED: [&str; 1] = ["worlds.put_ranking.data_team_race"];

fn is_definition(id: &str) -> bool {
    id.contains(".$defs.")
}

fn is_skipped(id: &str) -> bool {
    is_definition(id) || SKIPPED.contains(&id)
}

/// Entries whose expectation is dcl-one's accepted answer rather than the pinned
/// upstream's (upstream 500s dcl-one never reproduces, the stored rating
/// code `RP` upstream's PUT schema refuses, and `GET /api/status` with the
/// node's mirror state). They are checked like every
/// other entry but are never a known gap: a failure is a regression.
const ACCEPTED_ENTRIES: [&str; 14] = [
    "places.status.shape",
    "events.status.envelope",
    "events.sitemap.events_page_negative",
    "events.get_schedule.not_uuid",
    "events.create.missing_categories_500",
    "events.attendees.post_duplicate",
    "events.create.x_decimal",
    "events.patch.duration_decimal",
    "events.schedules.post_image_not_http",
    "events.profiles.list_edit_any_profile",
    "events.profiles.get_by_id_edit_any_profile",
    "events.profiles.patch_by_id_edit_any_profile",
    "places.put_rating.rp_accepted",
    "worlds.put_rating.rp_accepted",
];

/// Masks the values that differ between runs (uuids, timestamps, long numbers)
/// so an observed difference can be compared with the one `known_gaps.json` holds.
fn normalize_observed(s: &str) -> String {
    let uuid = regex::Regex::new(
        r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}",
    )
    .unwrap();
    let timestamp =
        regex::Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})")
            .unwrap();
    let digits = regex::Regex::new(r"\d{10,}").unwrap();
    let s = uuid.replace_all(s, "<uuid>");
    let s = timestamp.replace_all(&s, "<timestamp>");
    digits.replace_all(&s, "<n>").into_owned()
}

fn flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v == "1")
}

struct States {
    configured: Arc<ExploreState>,
    unconfigured: Arc<ExploreState>,
}

async fn states(pool: &PgPool, ids: &Identities) -> States {
    let mut settings = Settings::for_tests();
    settings.user_writes_enabled = true;
    settings.operator_writes_enabled = true;
    settings.places_admin_token = Some(PLACES_ADMIN_TOKEN.into());
    settings.data_team_token = Some(DATA_TEAM_TOKEN.into());
    settings.events_admin_token = Some(EVENTS_ADMIN_TOKEN.into());
    settings.places_admin_addresses = vec![ids.address("admin")];
    let mut bare = settings.clone();
    bare.places_admin_token = None;
    bare.data_team_token = None;
    bare.events_admin_token = None;
    let dir = std::env::temp_dir().join(format!(
        "catalyst-explore-parity-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let storage = Arc::new(
        catalyst::storage::Storage::from_config(&dir)
            .await
            .expect("blob store"),
    );
    States {
        configured: Arc::new(ExploreState::new(pool.clone(), settings, None).with_storage(storage)),
        unconfigured: Arc::new(ExploreState::new(pool.clone(), bare, None)),
    }
}

async fn run_entry(entry: &Entry, pool: &PgPool, ids: &Identities, states: &States) -> Outcome {
    let id = entry.id.as_str();
    if let Some(reason) = &entry.harness.skip {
        return if is_skipped(id) {
            Outcome::Skip(reason.clone())
        } else {
            Outcome::Invalid(format!(
                "unexpected skip ({reason}); a skipped entry must be listed in SKIPPED (tests/explore_parity.rs) and the README"
            ))
        };
    }
    let kind = entry.source.kind.as_str();
    if !(kind == "source" || (kind == "accepted" && ACCEPTED_ENTRIES.contains(&id))) {
        return Outcome::Invalid(format!(
            "source.kind {kind:?}: only entries derived from the pinned source (or the ACCEPTED_ENTRIES as \"accepted\") are tracked"
        ));
    }
    let res = match Resolution::new(entry, ids) {
        Ok(res) => res,
        Err(e) => return Outcome::Invalid(e),
    };
    let signer = Identities::signer_for(&entry.request.auth).unwrap_or("user");
    let owner = if entry.request.auth == "signed:approve-own" {
        ids.address("approve_own")
    } else {
        ids.address("owner")
    };
    reseed(
        pool,
        ids,
        &owner,
        &ids.address(signer),
        &res.place_id,
        &entry.harness.viewer_state,
    )
    .await;
    let unconfigured = entry.harness.state.as_deref() == Some("unconfigured")
        || entry.request.auth == "bearer:unconfigured";
    let state = if unconfigured {
        states.unconfigured.clone()
    } else {
        states.configured.clone()
    };
    let request = match build_request(entry, &res, ids) {
        Ok(request) => request,
        Err(e) => return Outcome::Invalid(e),
    };
    // As `serve` mounts it: trailing slashes trimmed (src/serve.rs NormalizePath).
    let app = tower_http::normalize_path::NormalizePath::trim_trailing_slash(router(state));
    let response = app.oneshot(request).await.expect("router response");
    let status = response.status().as_u16();
    let headers: BTreeMap<String, String> = response
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let bytes = axum::body::to_bytes(response.into_body(), 8 << 20)
        .await
        .expect("body");
    let text = String::from_utf8_lossy(&bytes).into_owned();
    let json = serde_json::from_slice(&bytes).ok();
    let answer = Answer {
        status,
        headers,
        text,
        json,
    };
    let mismatches = compare(entry, &res, &answer);
    if mismatches.is_empty() {
        Outcome::Pass
    } else {
        Outcome::Fail(mismatches.join("; "))
    }
}

#[tokio::test]
async fn explore_routes_match_the_upstream_fixtures_except_the_known_gaps() {
    let Some(db) = db::scratch_database().await else {
        // Past libtest's capture, so a plain `cargo test` shows the skip.
        use std::io::Write;
        let _ = writeln!(
            std::io::stderr(),
            "SKIPPED explore parity: CATALYST_TEST_DATABASE_URL is unset, no fixture ran"
        );
        return;
    };
    let pool = db.pool.clone();
    let ids = Identities::new();
    let states = states(&pool, &ids).await;
    let entries = load_entries();
    let mut known = load_known_gaps();
    for id in known.keys() {
        assert!(
            entries.iter().any(|e| e.id == *id),
            "known_gaps.json names {id}, which no fixture defines"
        );
    }
    // Skipped and accepted entries are never gaps: listed, they are treated as
    // unlisted (so the run still reports them), dropped by a write and an error otherwise.
    let misfiled: Vec<String> = known
        .keys()
        .filter(|id| is_skipped(id) || ACCEPTED_ENTRIES.contains(&id.as_str()))
        .cloned()
        .collect();
    for id in &misfiled {
        known.remove(id);
    }
    // Writing may only remove ids; adding one needs the second flag and is
    // printed with its reason.
    let write = flag("EXPLORE_PARITY_WRITE_GAPS");
    let allow_new = flag("EXPLORE_PARITY_ALLOW_NEW_GAPS");
    assert!(
        write || !allow_new,
        "EXPLORE_PARITY_ALLOW_NEW_GAPS=1 needs EXPLORE_PARITY_WRITE_GAPS=1"
    );

    let mut rows: Vec<(String, &'static str, String)> = Vec::new();
    let mut unexpected_failures: Vec<(String, String)> = Vec::new();
    let mut unexpected_passes = Vec::new();
    let mut changed = Vec::new();
    let mut invalid = Vec::new();
    let mut skipped_ids = Vec::new();
    let mut still_failing: BTreeMap<String, String> = BTreeMap::new();
    let (mut passed, mut failed) = (0, 0);
    for entry in &entries {
        let outcome = run_entry(entry, &pool, &ids, &states).await;
        let id = entry.id.clone();
        let stored = known.get(&id);
        match outcome {
            Outcome::Pass => {
                passed += 1;
                if stored.is_some() {
                    unexpected_passes.push(id.clone());
                    rows.push((id, "FIXED", String::new()));
                } else {
                    rows.push((id, "PASS", String::new()));
                }
            }
            Outcome::Fail(reason) => {
                failed += 1;
                let observed = normalize_observed(&reason);
                match stored {
                    Some(previous) if normalize_observed(previous) == observed => {
                        rows.push((id.clone(), "GAP", observed.clone()));
                        still_failing.insert(id, observed);
                    }
                    Some(previous) => {
                        changed.push(id.clone());
                        rows.push((
                            id.clone(),
                            "CHANGED",
                            format!("{observed} (was: {})", normalize_observed(previous)),
                        ));
                        still_failing.insert(id, observed);
                    }
                    None => {
                        rows.push((id.clone(), "FAIL", observed.clone()));
                        unexpected_failures.push((id, observed));
                    }
                }
            }
            Outcome::Skip(reason) => {
                skipped_ids.push(id.clone());
                rows.push((id, "SKIP", reason));
            }
            Outcome::Invalid(reason) => {
                invalid.push(id.clone());
                rows.push((id, "BAD", reason));
            }
        }
    }
    db.teardown().await;

    // An accepted entry is never written as a gap, with or without the flag.
    let (new_gaps, unblessable): (Vec<_>, Vec<_>) = unexpected_failures
        .iter()
        .partition(|(id, _)| allow_new && !ACCEPTED_ENTRIES.contains(&id.as_str()));
    if write {
        for id in &misfiled {
            eprintln!("removed {id}: a skipped or accepted entry is never a gap");
        }
        for id in &unexpected_passes {
            eprintln!("removed {id}: passes now");
        }
        for id in &changed {
            eprintln!("refreshed {id}: the observed difference changed");
        }
        for (id, observed) in &new_gaps {
            eprintln!("added {id}: {observed}");
        }
        let added: BTreeMap<&String, &String> = new_gaps.iter().map(|(id, o)| (id, o)).collect();
        let gaps: Vec<KnownGap> = entries
            .iter()
            .filter_map(|e| {
                still_failing
                    .get(&e.id)
                    .or_else(|| added.get(&e.id).copied())
                    .map(|observed| KnownGap {
                        id: e.id.clone(),
                        observed: observed.clone(),
                    })
            })
            .collect();
        let path = fixtures_dir().join("known_gaps.json");
        let doc = json!({
            "about": "Fixture ids the explore router does not answer as the pinned source does (tests/explore_parity.rs); README.md, 'Known gaps', records each with the pinned source lines it diverges from and the answer the router gives. EXPLORE_PARITY_WRITE_GAPS=1 rewrites the list from a run and may only drop ids or refresh their observed difference; EXPLORE_PARITY_ALLOW_NEW_GAPS=1 (with it) also adds the entries that fail now and prints each added id, which the README then records the same way.",
            "entries": gaps,
        });
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string_pretty(&doc).unwrap()),
        )
        .unwrap();
        eprintln!("wrote {} known gaps to {}", gaps.len(), path.display());
    }

    eprintln!(
        "explore parity: {} entries, {passed} pass, {failed} fail ({} known, {} fixed, {} changed, {} unlisted), {} skip [{}], {} invalid",
        entries.len(),
        known.len(),
        unexpected_passes.len(),
        changed.len(),
        unexpected_failures.len(),
        skipped_ids.len(),
        skipped_ids.join(", "),
        invalid.len()
    );
    let clean = invalid.is_empty()
        && unblessable.is_empty()
        && (write || (unexpected_passes.is_empty() && changed.is_empty() && misfiled.is_empty()));
    if !clean {
        for (id, verdict, reason) in &rows {
            if reason.is_empty() {
                eprintln!("{verdict:7} {id}");
            } else {
                eprintln!("{verdict:7} {id}: {reason}");
            }
        }
    }
    assert!(
        invalid.is_empty(),
        "fixture defects (BAD above; never known gaps): {invalid:#?}"
    );
    let unblessable: Vec<&String> = unblessable.iter().map(|(id, _)| id).collect();
    assert!(
        unblessable.is_empty(),
        "entries failing but not in known_gaps.json (fix the router or the fixture; a real gap is added with EXPLORE_PARITY_WRITE_GAPS=1 EXPLORE_PARITY_ALLOW_NEW_GAPS=1 and recorded in the fixtures README under 'Known gaps' with the pinned source lines it diverges from; ACCEPTED_ENTRIES are never gaps): {unblessable:#?}"
    );
    if !write {
        assert!(
            misfiled.is_empty(),
            "known_gaps.json lists skipped or accepted entries, which are never gaps; remove them (EXPLORE_PARITY_WRITE_GAPS=1): {misfiled:#?}"
        );
        assert!(
            unexpected_passes.is_empty(),
            "entries in known_gaps.json that now pass; remove them (EXPLORE_PARITY_WRITE_GAPS=1): {unexpected_passes:#?}"
        );
        assert!(
            changed.is_empty(),
            "known gaps whose observed difference changed (CHANGED above); check none got worse, then refresh with EXPLORE_PARITY_WRITE_GAPS=1: {changed:#?}"
        );
    }
}

/// The fixtures the timing run replays, one per route family.
const TIMED: [&str; 36] = [
    "places.get_places.default_list",
    "places.get_places.search_adds_rank",
    "places.get_places.positions_disabled_excluded",
    "places.get_places.order_by_most_active",
    "places.get_places.with_realms_detail",
    "places.get_places.owner_filter",
    "places.get_places.categories_comma_split",
    "places.get_place.ok",
    "places.post_places.valid_ids",
    "places.post_places_status.valid_ids",
    "places.get_map.default",
    "places.get_map.positions",
    "places.get_map_places.default",
    "places.get_destinations.default",
    "places.get_destinations.pointer_only_places",
    "places.get_destinations.names_like_only_worlds",
    "places.post_destinations.valid_ids",
    "places.get_worlds.default",
    "places.get_categories.default",
    "places.patch_favorites.set",
    "places.patch_likes.like",
    "places.put_rating.admin_wallet",
    "places.get_place.invalid_uuid",
    "places.unknown_route.json_404",
    "events.get_events.default_anonymous",
    "events.get_events.owner_signed_owner",
    "events.get_events.list_live",
    "events.get_events.positions_match",
    "events.get_event.approved_anonymous",
    "events.get_events_attending.signed",
    "events.post_search.empty_body",
    "events.get_categories.list",
    "events.get_schedules.list",
    "events.profiles.me_get",
    "events.create.minimal_success",
    "events.get_events.unknown_param",
];

const SCALE_PLACES_SQL: &str = "INSERT INTO explore.place (id, base_position, title, description, creator_address, content_rating, categories, likes, dislikes, favorites, deployed_at, disabled, highlighted, raw) \
SELECT s.id, s.pos, s.title, s.title || ' description', s.owner, 'T', s.cats, s.likes, s.dislikes, s.favs, \
       timezone('UTC', now()) - (s.g * interval '1 minute'), false, s.g % 97 = 0, \
       jsonb_build_object('id', s.id, 'title', s.title, 'description', s.title || ' description', \
         'image', 'https://cdn.example/plaza.png', 'owner', s.owner, 'world_name', NULL, 'content_rating', 'T', \
         'categories', to_jsonb(s.cats), 'likes', s.likes, 'dislikes', s.dislikes, 'favorites', s.favs, \
         'like_rate', 0.8, 'like_score', (s.g % 100) / 100.0, 'created_at', '2026-01-01T00:00:00.000Z', \
         'updated_at', '2026-01-01T00:00:00.000Z', 'disabled', false, 'disabled_at', NULL, 'disabled_reason', NULL, \
         'highlighted_image', NULL, 'positions', jsonb_build_array(s.pos), 'base_position', s.pos, \
         'contact_name', 'Scaled', 'contact_email', NULL, 'highlighted', s.g % 97 = 0, 'exclude_from_ranking', false, \
         'world', false, 'world_id', NULL, 'deployed_at', '2026-01-01T00:00:00.000Z', 'sdk', '7', 'user_count', 0, 'user_visits', s.g % 1000) \
FROM (SELECT g, 'a0000000-0000-4000-8000-' || lpad(g::text, 12, '0') AS id, \
        ((g % 300) - 150)::text || ',' || ((g / 300) - 150)::text AS pos, 'Scaled place ' || g AS title, \
        '0x00000000000000000000000000000000000000aa' AS owner, \
        ARRAY['art', (ARRAY['game', 'social', 'poi', 'parkour'])[1 + g % 4]] AS cats, \
        (g * 7) % 50 AS likes, (g * 3) % 10 AS dislikes, (g * 5) % 30 AS favs \
      FROM generate_series($1::int, $2::int) g) s";

fn env_number<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Adds `EXPLORE_PARITY_TIMING_PLACES` places (10,000) and `_EVENTS` events (3,000)
/// on top of the seed, so the timings see production-sized tables.
async fn scale_dataset(pool: &PgPool, owner: &str) {
    // In batches: the removal trigger takes a lock per row.
    let places = env_number("EXPLORE_PARITY_TIMING_PLACES", 10_000i32);
    for from in (1..=places).step_by(1_000) {
        sqlx::query(SCALE_PLACES_SQL)
            .bind(from)
            .bind((from + 999).min(places))
            .execute(pool)
            .await
            .expect("scaled places");
    }
    for i in 0..env_number("EXPLORE_PARITY_TIMING_EVENTS", 3_000usize) {
        let id = format!("b0000000-0000-4000-8000-{i:012}");
        let (month, day) = (1 + (i / 28) % 12, 1 + i % 28);
        let start = format!("2031-{month:02}-{day:02}T10:00:00.000Z");
        let finish = format!("2031-{month:02}-{day:02}T11:00:00.000Z");
        let mut event = event_json(&id, owner, &start, &finish);
        let (x, y) = ((i % 150) as i64, (i / 150) as i64);
        set(
            &mut event,
            &[
                ("x", json!(x)),
                ("y", json!(y)),
                ("coordinates", json!([x, y])),
                ("position", json!([x, y])),
            ],
        );
        assert!(
            events_mirror::upsert_event(pool, &event)
                .await
                .expect("scaled event")
        );
    }
}

/// Route timings on the scaled dataset, printed as a table. Runs only with
/// `EXPLORE_PARITY_TIMING=1`; `EXPLORE_PARITY_TIMING_N` samples (200) after
/// `EXPLORE_PARITY_TIMING_WARMUP` (20) warm-ups; `EXPLORE_PARITY_KEEP_DB=1`
/// keeps the database and `EXPLORE_PARITY_TIMING_DB=<name>` reuses a kept one.
#[tokio::test]
async fn explore_route_timings() {
    if !flag("EXPLORE_PARITY_TIMING") {
        return;
    }
    let db = match std::env::var("EXPLORE_PARITY_TIMING_DB") {
        Ok(name) => db::existing_database(&name).await,
        Err(_) if flag("EXPLORE_PARITY_KEEP_DB") => db::kept_database().await,
        Err(_) => db::scratch_database().await,
    };
    let Some(db) = db else {
        return;
    };
    let pool = db.pool.clone();
    let ids = Identities::new();
    let states = states(&pool, &ids).await;
    let entries = load_entries();
    let owner = ids.address("owner");
    if std::env::var("EXPLORE_PARITY_TIMING_DB").is_err() {
        reseed(
            &pool,
            &ids,
            &owner,
            &ids.address("user"),
            PLACE_ENABLED,
            &[],
        )
        .await;
        scale_dataset(&pool, &owner).await;
    }
    let app = tower_http::normalize_path::NormalizePath::trim_trailing_slash(router(
        states.configured.clone(),
    ));
    let n = env_number("EXPLORE_PARITY_TIMING_N", 200usize);
    let warmup = env_number("EXPLORE_PARITY_TIMING_WARMUP", 20usize);
    println!("| fixture | status | bytes | p50 µs | p95 µs | max µs |");
    println!("|---|---|---|---|---|---|");
    for id in TIMED {
        let entry = entries
            .iter()
            .find(|e| e.id == id)
            .unwrap_or_else(|| panic!("no fixture {id}"));
        let res = Resolution::new(entry, &ids).unwrap_or_else(|e| panic!("{id}: {e}"));
        let mut samples = Vec::with_capacity(n);
        let (mut status, mut bytes) = (0, 0);
        for i in 0..n + warmup {
            let request = build_request(entry, &res, &ids).unwrap_or_else(|e| panic!("{id}: {e}"));
            let app = app.clone();
            let start = std::time::Instant::now();
            let response = app.oneshot(request).await.expect("router response");
            status = response.status().as_u16();
            let body = axum::body::to_bytes(response.into_body(), 8 << 20)
                .await
                .expect("body");
            let elapsed = start.elapsed();
            bytes = body.len();
            if i >= warmup {
                samples.push(elapsed.as_micros());
            }
        }
        samples.sort_unstable();
        println!(
            "| {id} | {status} | {bytes} | {} | {} | {} |",
            samples[n / 2],
            samples[n * 95 / 100],
            samples[n - 1]
        );
    }
    if flag("EXPLORE_PARITY_KEEP_DB") || std::env::var("EXPLORE_PARITY_TIMING_DB").is_ok() {
        let name: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(&pool)
            .await
            .unwrap();
        println!("kept database {name}");
        std::mem::forget(db);
    }
}
