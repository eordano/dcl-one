//! `/lambdas/profiles`: the profile entity comes from `content.deployments`
//! (peer sync), every wearable and emote it names is checked against the
//! collections subgraphs (token ids included, re-emitted `<urn>:<tokenId>`
//! as `ENSURE_ERC_721` does), its third-party wearables against their
//! mappings and the chain (`third_party::sanitize_third_party_urns`,
//! appended after the others as the old lambdas did) and `hasClaimedName`
//! against the marketplace subgraph. The answer is decentraland/lamb2's
//! `{ timestamp, avatars }`, the timestamp the entity's. With no collections
//! subgraph the items are served as deployed (a node with no ownership
//! source, such as an island); a reader that is configured but unreachable
//! fails the request closed (503) rather than serving cosmetics nobody
//! proved. The one exception is a linked third-party item whose chain read
//! failed: only that profile's linked items are left out.
//!
//! `POST /lambdas/profiles/metadata` serves the compact projection of the same
//! processed profiles, as asset-bundle-registry's `/profiles/metadata`.

pub mod registry;

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::content::queries::{active_entity_docs_by_pointers, retain_non_denylisted};

use super::graph;
use super::third_party;
use super::{MarketplaceState, SharedState, bad_request, lambdas_internal_error, not_found};

/// Bounds staleness for changes that arrive on chain rather than through a
/// deployment (a wearable or name bought, sold or transferred).
pub const PROFILE_CACHE_TTL: Duration = Duration::from_secs(10);
pub const PROFILE_CACHE_MAX_ENTRIES: usize = 50_000;
const PROFILE_BATCH_MAX: usize = 50;
const PROFILE_IDS_MAX: usize = 1000;
const METADATA_IDS_MSG: &str = "The ids property must be an array of non-empty strings";
/// Profiles whose linked third-party items are checked at once.
const THIRD_PARTY_CONCURRENCY: usize = 8;
const MISSING_PROFILE_IDS_MSG: &str =
    "No profile ids were specified. Expected ids:string[] in body";

/// The token ids an address holds of each on-chain URN it was asked about.
type OwnedTokens = HashMap<String, Vec<String>>;

fn is_base_wearable(urn: &str) -> bool {
    urn.contains("base-avatars")
}

/// `profiles.ts` `isBaseEmote`: a substring test for the off-chain base
/// emotes only; a `base-scene-emotes` URN goes through ownership like any
/// other item (and nobody owns one).
pub fn is_base_emote(urn: &str) -> bool {
    urn.contains("urn:decentraland:off-chain:base-emotes")
}

/// Pointers `default*` hold the curated default looks: they own nothing.
fn is_default_pointer(address: &str) -> bool {
    address.starts_with("default")
}

/// Distinct pointers looked up in one entity read by the uncapped profile
/// routes (`/profiles/metadata` here and on the Registry host): bounds the
/// one `POST /entities/active` a read-through node sends upstream.
pub(super) const PROFILE_LOOKUP_CHUNK: usize = 1000;

/// Counts profile answers that left something out because a lookup layer
/// failed, by `route` and `layer` (`entities`, `ownership`): those answers
/// are `200`s, as the Registry's, so 5xx alerting does not see them.
pub(super) const PROFILE_LOOKUP_FAILURES: &str = "profiles_lookup_failures_total";

pub(super) fn count_lookup_failure(route: &'static str, layer: &'static str) {
    metrics::counter!(PROFILE_LOOKUP_FAILURES, "route" => route, "layer" => layer).increment(1);
}

/// A pointer that can name a profile: an address (`0x` and 40 hex digits,
/// lowercased by the caller) or `default` followed by one to three digits.
/// The body bounds address lookups; the default namespace has 1,110 distinct
/// spellings. Other nonempty strings remain valid input but are not looked up.
pub(super) fn is_profile_pointer(pointer: &str) -> bool {
    let address = pointer
        .strip_prefix("0x")
        .is_some_and(|hex| hex.len() == 40 && hex.bytes().all(|b| b.is_ascii_hexdigit()));
    let default = pointer
        .strip_prefix("default")
        .is_some_and(|n| (1..=3).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_digit()));
    address || default
}

/// `splitUrnAndTokenId`: a seven-part non-third-party URN ends in its token id.
pub(crate) fn split_urn_and_token_id(urn: &str) -> (&str, Option<&str>) {
    let segment_count = urn.split(':').count();
    if segment_count == 7
        && !urn.contains("collections-thirdparty")
        && let Some(last_colon) = urn.rfind(':')
    {
        return (&urn[..last_colon], Some(&urn[last_colon + 1..]));
    }
    (urn, None)
}

fn collect_ownership_urns(metadata: &Value) -> Vec<String> {
    let mut to_check: Vec<String> = Vec::new();
    let Some(avatars) = metadata.get("avatars").and_then(|v| v.as_array()) else {
        return to_check;
    };
    for avatar_val in avatars {
        let Some(avatar_obj) = avatar_val.get("avatar") else {
            continue;
        };
        if let Some(wearables) = avatar_obj.get("wearables").and_then(|w| w.as_array()) {
            for wearable in wearables.iter().filter_map(Value::as_str) {
                if is_base_wearable(wearable) {
                    continue;
                }
                let (urn, _token_id) = split_urn_and_token_id(wearable);
                to_check.push(urn.to_string());
            }
        }
        if let Some(emotes) = avatar_obj.get("emotes").and_then(|e| e.as_array()) {
            for emote_urn in emotes
                .iter()
                .filter_map(|e| e.get("urn").and_then(|u| u.as_str()))
            {
                if !emote_urn.contains(':') || is_base_emote(emote_urn) {
                    continue;
                }
                let (urn, _token_id) = split_urn_and_token_id(emote_urn);
                to_check.push(urn.to_string());
            }
        }
    }
    to_check
}

/// An owned item as `profiles.ts` re-emits it: `<owned urn>:<the named token
/// id, or the first one held>`; `None` when the URN is not held or the named
/// token is not among the held ones. The URN must equal the inventory's
/// exactly, as lamb2's `Map` lookup does: the subgraphs spell L1 `ethereum`,
/// so a `mainnet` spelling is not owned.
fn owned_item(item: &str, owned: &OwnedTokens) -> Option<String> {
    let (urn, token_id) = split_urn_and_token_id(item);
    let held = owned.get(urn)?;
    let token = match token_id {
        Some(t) => Some(held.iter().find(|h| *h == t)?),
        None => held.first(),
    };
    Some(match token {
        Some(token) => format!("{urn}:{token}"),
        None => urn.to_string(),
    })
}

/// Keeps the base items and the owned ones (`owned: None`, no collections
/// reader: every item as deployed). An owned emote becomes exactly
/// `{ urn, slot }`. `emotes` is always present, `[]` when not deployed.
fn filter_avatars_by_ownership(avatars: &mut [Value], owned: Option<&OwnedTokens>) {
    for avatar_val in avatars.iter_mut() {
        let Some(avatar_obj) = avatar_val.get_mut("avatar").and_then(Value::as_object_mut) else {
            continue;
        };
        if let Some(owned) = owned
            && let Some(wearables) = avatar_obj.get("wearables").and_then(Value::as_array)
        {
            let validated: Vec<Value> = wearables
                .iter()
                .filter_map(Value::as_str)
                .filter_map(|wearable| {
                    if is_base_wearable(wearable) {
                        Some(wearable.to_string())
                    } else {
                        owned_item(wearable, owned)
                    }
                })
                .map(Value::String)
                .collect();
            avatar_obj.insert("wearables".to_string(), Value::Array(validated));
        }
        let emotes = avatar_obj
            .get("emotes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let validated: Vec<Value> = match owned {
            None => emotes,
            Some(owned) => emotes
                .into_iter()
                .filter_map(|emote| {
                    let Some(emote_urn) = emote.get("urn").and_then(Value::as_str) else {
                        return Some(emote);
                    };
                    if !emote_urn.contains(':') || is_base_emote(emote_urn) {
                        return Some(emote);
                    }
                    let urn = owned_item(emote_urn, owned)?;
                    let mut kept = serde_json::Map::new();
                    kept.insert("urn".to_string(), Value::String(urn));
                    if let Some(slot) = emote.get("slot") {
                        kept.insert("slot".to_string(), slot.clone());
                    }
                    Some(Value::Object(kept))
                })
                .collect(),
        };
        avatar_obj.insert("emotes".to_string(), Value::Array(validated));
    }
}

/// Every non-base wearable the profile's avatars wear, for the third-party check.
fn worn_wearables(metadata: &Value) -> Vec<String> {
    metadata
        .get("avatars")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|a| a.pointer("/avatar/wearables").and_then(Value::as_array))
        .flatten()
        .filter_map(Value::as_str)
        .filter(|w| !is_base_wearable(w))
        .map(str::to_string)
        .collect()
}

/// Appends the owner's confirmed third-party wearables to every avatar's
/// validated ones, without repeats (`profiles.ts`: `new Set(validated.concat(thirdParty))`).
fn append_third_party(avatars: &mut [Value], third_party: &[String]) {
    for avatar in avatars.iter_mut() {
        let Some(wearables) = avatar
            .pointer_mut("/avatar/wearables")
            .and_then(Value::as_array_mut)
        else {
            continue;
        };
        let mut seen: HashSet<String> = HashSet::new();
        let merged: Vec<Value> = std::mem::take(wearables)
            .into_iter()
            .chain(third_party.iter().map(|u| Value::String(u.clone())))
            .filter(|w| w.as_str().is_none_or(|u| seen.insert(u.to_string())))
            .collect();
        *wearables = merged;
    }
}

pub fn apply_claimed_name(metadata: &mut Value, owned_names: &[String]) {
    let Some(avatars) = metadata.get_mut("avatars").and_then(|v| v.as_array_mut()) else {
        return;
    };
    for avatar_val in avatars.iter_mut() {
        let claimed = avatar_val
            .get("name")
            .and_then(|v| v.as_str())
            .map(|name| owned_names.iter().any(|owned| owned == name))
            .unwrap_or(false);
        if let Some(obj) = avatar_val.as_object_mut() {
            obj.insert("hasClaimedName".to_string(), Value::Bool(claimed));
        }
    }
}

/// `face256` and `body` always point at `<base>entities/<id>/{face,body}.png`
/// and every other snapshot key is left as deployed, as `profiles.ts`
/// `addBaseUrlToSnapshots` does; a missing snapshots object is created. The
/// base is `PROFILE_CDN_BASE_URL` (lamb2's default is
/// `https://profile-images.decentraland.org`, so parity with it needs that
/// value) or, unset, this node's `profile-images`: sync and the read-through
/// never copy a profile's snapshot files, so their `contents/<hash>` would
/// 404, while `/profile-images` serves the uploaded file when this node holds
/// it and renders or fetches the image otherwise.
pub fn rewrite_snapshot_urls(
    entity_id: &str,
    metadata: &mut Value,
    cdn_base: Option<&str>,
    profile_images_public_url: &str,
) {
    let Some(avatars) = metadata.get_mut("avatars").and_then(|v| v.as_array_mut()) else {
        return;
    };
    let base = cdn_base.unwrap_or(profile_images_public_url);
    let base = if base.ends_with('/') {
        base.to_string()
    } else {
        format!("{base}/")
    };
    for avatar_val in avatars.iter_mut() {
        let Some(avatar_obj) = avatar_val.get_mut("avatar").and_then(|a| a.as_object_mut()) else {
            continue;
        };
        if !avatar_obj.get("snapshots").is_some_and(Value::is_object) {
            avatar_obj.insert(
                "snapshots".to_string(),
                json!({ "face256": "", "body": "" }),
            );
        }
        if let Some(snapshots) = avatar_obj
            .get_mut("snapshots")
            .and_then(|s| s.as_object_mut())
        {
            for (key, file) in [("body", "body.png"), ("face256", "face.png")] {
                snapshots.insert(
                    key.to_string(),
                    Value::String(format!("{base}entities/{entity_id}/{file}")),
                );
            }
        }
    }
}

fn link_url_regex() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?i)^(?:https?)://[^\s/$.?#].[^\s]*$").expect("LinkUrl regex is valid")
    })
}

/// `profiles.ts` `sanitizeLinks`: a link whose `url` passes `LinkUrl.validate`
/// is kept as deployed, else one whose URL-decoded `url` passes is kept with
/// the decoded value; nothing else is checked (this node never fetches these
/// URLs, so a loopback or private host is kept as lamb2 keeps it).
pub fn sanitize_links(metadata: &mut Value) {
    let Some(avatars) = metadata.get_mut("avatars").and_then(|v| v.as_array_mut()) else {
        return;
    };
    let re = link_url_regex();
    for avatar_val in avatars.iter_mut() {
        let Some(links) = avatar_val.get_mut("links").and_then(|l| l.as_array_mut()) else {
            continue;
        };
        let mut sanitized: Vec<Value> = Vec::new();
        for link in links.iter() {
            let Some(url) = link.get("url").and_then(|u| u.as_str()) else {
                continue;
            };
            if re.is_match(url) {
                sanitized.push(link.clone());
                continue;
            }
            if let Ok(decoded) = urlencoding::decode(url)
                && re.is_match(&decoded)
            {
                let mut link_clone = link.clone();
                if let Some(obj) = link_clone.as_object_mut() {
                    obj.insert("url".to_string(), Value::String(decoded.into_owned()));
                }
                sanitized.push(link_clone);
            }
        }
        avatar_val["links"] = Value::Array(sanitized);
    }
}

fn apply_pointer_identity(metadata: &mut Value, eth_address: &str) {
    if let Some(avatars) = metadata.get_mut("avatars").and_then(|v| v.as_array_mut()) {
        for avatar in avatars.iter_mut().filter(|a| a.is_object()) {
            avatar["userId"] = Value::String(eth_address.to_string());
            avatar["ethAddress"] = Value::String(eth_address.to_string());
        }
    }
}

pub fn entity_eth_address(entity: &Value) -> Option<String> {
    entity
        .get("pointers")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|v| v.as_str())
        .map(|s| s.to_lowercase())
}

/// The deployed metadata of a profile entity, links and snapshots fixed and
/// its avatars carrying the pointer's identity; `None` without metadata.
fn prepare_profile(state: &MarketplaceState, entity: &Value) -> Option<(String, Value)> {
    let mut metadata = entity.get("metadata").filter(|m| !m.is_null())?.clone();
    let eid = entity.get("id").and_then(|v| v.as_str()).unwrap_or("");
    let address = entity_eth_address(entity).unwrap_or_default();
    sanitize_links(&mut metadata);
    rewrite_snapshot_urls(
        eid,
        &mut metadata,
        state.settings.profile_cdn_base_url.as_deref(),
        &state.settings.profile_images_public_url,
    );
    if !address.is_empty() && !is_default_pointer(&address) {
        apply_pointer_identity(&mut metadata, &address);
    }
    Some((address, metadata))
}

/// One slot per entity (`None` without metadata): the pure passes, then one
/// ownership batch and one names batch per chunk of 128. Each profile is
/// `{ timestamp, avatars }` (`profiles.ts`), the entity's timestamp. `Err`
/// is a reader failure.
pub async fn process_profiles_positional(
    state: &MarketplaceState,
    entities: &[Value],
) -> Result<Vec<Option<Value>>, String> {
    let collections = graph::any_collections_reader(&state.index);
    state.warm_faces(
        entities
            .iter()
            .filter_map(|e| e.get("id").and_then(Value::as_str).map(str::to_string)),
    );
    let mut profiles = Vec::with_capacity(entities.len());
    for chunk in entities.chunks(128) {
        let mut prepared: Vec<Option<(String, Value)>> = chunk
            .iter()
            .map(|entity| prepare_profile(state, entity))
            .collect();
        let mut addresses: Vec<String> = Vec::new();
        let mut worn: Vec<Vec<String>> = Vec::new();
        let mut requested: HashSet<(String, String)> = HashSet::new();
        for (address, metadata) in prepared.iter().flatten() {
            if !is_default_pointer(address) && !addresses.contains(address) {
                addresses.push(address.clone());
                worn.push(worn_wearables(metadata));
                requested.extend(
                    collect_ownership_urns(metadata)
                        .into_iter()
                        .map(|urn| (address.clone(), urn)),
                );
            }
        }
        let mut owned: HashMap<String, OwnedTokens> = HashMap::new();
        let mut names_by_address: HashMap<String, Vec<String>> = HashMap::new();
        let mut third_party_by_address: HashMap<String, Vec<String>> = HashMap::new();
        if !addresses.is_empty() {
            // `owned_names` answers empty without a marketplace subgraph.
            let names_futs = addresses
                .iter()
                .map(|a| graph::owned_names(&state.index, a));
            // A profile whose linked items could not be checked keeps
            // its v1 items only; the others in the batch are unaffected.
            let third_party_futs = addresses.iter().zip(&worn).map(|(a, worn)| async move {
                let kept = if collections {
                    third_party::sanitize_third_party_urns(state, a, worn)
                        .await
                        .unwrap_or_else(|e| {
                            tracing::warn!(error = %e, address = %a, "profiles: third-party items could not be checked; keeping the v1 items only");
                            third_party::v1_third_party_urns(worn)
                        })
                } else {
                    Vec::new()
                };
                (a.clone(), kept)
            });
            // Collected first: a stream over the closure itself is not
            // `Send` for every lifetime the router asks of it.
            let third_party_futs: Vec<_> = third_party_futs.collect();
            use futures::stream::StreamExt;
            let (owned_res, names, third_party) = tokio::join!(
                async {
                    if collections && !requested.is_empty() {
                        graph::owned_tokens(&state.index, &requested).await
                    } else {
                        Ok(HashMap::new())
                    }
                },
                futures::future::join_all(names_futs),
                futures::stream::iter(third_party_futs)
                    .buffer_unordered(THIRD_PARTY_CONCURRENCY)
                    .collect::<HashMap<String, Vec<String>>>()
            );
            owned = owned_res?;
            for (address, names) in addresses.iter().zip(names) {
                names_by_address.insert(
                    address.clone(),
                    names?.iter().map(|n| n.name.clone()).collect(),
                );
            }
            third_party_by_address = third_party;
        }
        let nothing = OwnedTokens::new();
        for (address, metadata) in prepared.iter_mut().flatten() {
            let default = is_default_pointer(address);
            let owned = if default {
                Some(&nothing)
            } else if collections {
                Some(owned.get(address.as_str()).unwrap_or(&nothing))
            } else {
                None
            };
            if let Some(avatars) = metadata.get_mut("avatars").and_then(Value::as_array_mut) {
                filter_avatars_by_ownership(avatars, owned);
                if collections && !default {
                    append_third_party(
                        avatars,
                        third_party_by_address
                            .get(address.as_str())
                            .map(Vec::as_slice)
                            .unwrap_or(&[]),
                    );
                }
            }
            // A default profile claims no name (lamb2: nothing owned).
            let names = names_by_address
                .get(address.as_str())
                .filter(|_| !default)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            apply_claimed_name(metadata, names);
        }
        profiles.extend(chunk.iter().zip(prepared).map(|(entity, p)| {
            p.map(|(_, mut metadata)| {
                json!({
                    "timestamp": entity.get("timestamp").cloned().unwrap_or(Value::Null),
                    "avatars": metadata
                        .get_mut("avatars")
                        .filter(|a| a.is_array())
                        .map(Value::take)
                        .unwrap_or_else(|| json!([])),
                })
            })
        }));
    }
    Ok(profiles)
}

/// The active entities on `pointers`, through the read-through when one is
/// configured (the node's full view, as the lambdas' content client sees it),
/// without the denylisted ones.
pub(super) async fn active_entities(
    state: &MarketplaceState,
    pointers: &[String],
) -> Result<Vec<Value>, ()> {
    let mut docs = match &state.read_through {
        Some(read_through) => read_through.active_docs(false, pointers).await,
        None => active_entity_docs_by_pointers(&state.pool, pointers).await,
    }
    .map_err(|e| {
        tracing::error!(error = %e, "lambdas: active entities query failed");
    })?;
    retain_non_denylisted(&mut docs, &state.denylist);
    Ok(docs.iter().filter_map(|d| d.to_value()).collect())
}

/// Processes a batch and fills the per-id cache (`Null` for absent pointers).
async fn process_and_cache_profiles(
    state: &MarketplaceState,
    pointers: &[String],
    entities: &[Value],
) -> Result<Vec<(String, Value)>, String> {
    let processed = process_profiles_positional(state, entities).await?;
    let mut out = Vec::with_capacity(entities.len());
    let mut filled: HashSet<String> = Default::default();
    for (entity, profile) in entities.iter().zip(processed) {
        let key = entity_eth_address(entity).unwrap_or_default();
        let profile = profile.unwrap_or(Value::Null);
        if !key.is_empty() && filled.insert(key.clone()) {
            state.profile_cache.insert(key.clone(), profile.clone());
        }
        if !profile.is_null() {
            out.push((key, profile));
        }
    }
    for pointer in pointers {
        if filled.insert(pointer.clone()) {
            state.profile_cache.insert(pointer.clone(), Value::Null);
        }
    }
    Ok(out)
}

enum Failure {
    Db,
    Reader(String),
}

impl Failure {
    fn layer(&self) -> &'static str {
        match self {
            Failure::Db => "entities",
            Failure::Reader(_) => "ownership",
        }
    }

    fn log(&self, context: &str) {
        match self {
            Failure::Db => tracing::warn!("{context}: profile read failed"),
            Failure::Reader(e) => tracing::warn!(error = %e, "{context}: ownership reader failed"),
        }
    }

    fn into_response(self) -> Response {
        match self {
            Failure::Db => lambdas_internal_error(),
            // lamb2's retryable 503 envelope (`ServiceOverloadedError`).
            Failure::Reader(e) => {
                tracing::warn!(error = %e, "profiles: ownership reader failed");
                super::lambdas_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Service overloaded, retry shortly",
                    "ownership readers unavailable",
                )
            }
        }
    }
}

/// Per-id hits are reused across id sets; only misses are fetched; answer
/// `(pointer, profile)` in request order, each pointer once.
async fn profiles_via_cache(
    state: &MarketplaceState,
    pointers: &[String],
) -> Result<Vec<(String, Value)>, Failure> {
    let wanted = unique_pointers(pointers);
    let mut found: HashMap<String, Value> = Default::default();
    let mut missing: Vec<String> = Vec::new();
    for pointer in &wanted {
        match state.profile_cache.get_fresh(pointer) {
            Some(v) => {
                found.insert(pointer.clone(), v);
            }
            None => missing.push(pointer.clone()),
        }
    }
    if !missing.is_empty() {
        let entities = active_entities(state, &missing)
            .await
            .map_err(|()| Failure::Db)?;
        for (key, profile) in process_and_cache_profiles(state, &missing, &entities)
            .await
            .map_err(Failure::Reader)?
        {
            found.entry(key).or_insert(profile);
        }
    }
    Ok(wanted
        .into_iter()
        .filter_map(|p| {
            let profile = found.remove(&p).filter(|v| !v.is_null())?;
            Some((p, profile))
        })
        .collect())
}

/// Processed profiles of the lowercased `pointers` as `(pointer, profile)`,
/// in request order and each once: through the per-id cache for a small
/// batch, one database read and ownership pass for a larger one.
async fn processed_profiles(
    state: &MarketplaceState,
    pointers: &[String],
) -> Result<Vec<(String, Value)>, Failure> {
    if pointers.len() <= PROFILE_BATCH_MAX {
        return profiles_via_cache(state, pointers).await;
    }
    let entities = active_entities(state, pointers)
        .await
        .map_err(|()| Failure::Db)?;
    let mut by_pointer: HashMap<String, Value> = HashMap::new();
    for (pointer, profile) in process_and_cache_profiles(state, pointers, &entities)
        .await
        .map_err(Failure::Reader)?
    {
        by_pointer.entry(pointer).or_insert(profile);
    }
    Ok(unique_pointers(pointers)
        .into_iter()
        .filter_map(|p| {
            let profile = by_pointer.remove(&p)?;
            Some((p, profile))
        })
        .collect())
}

async fn fetch_profile_for_id(
    state: &MarketplaceState,
    id: &str,
) -> Result<Option<Value>, Failure> {
    let key = id.to_lowercase();
    let cached = state
        .profile_cache
        .get_or_fetch(key.clone(), || async {
            let entities = active_entities(state, std::slice::from_ref(&key))
                .await
                .map_err(|()| Failure::Db)?;
            let Some(entity) = entities.into_iter().next() else {
                return Ok(Value::Null);
            };
            let processed = process_profiles_positional(state, std::slice::from_ref(&entity))
                .await
                .map_err(Failure::Reader)?;
            Ok(processed
                .into_iter()
                .next()
                .flatten()
                .unwrap_or(Value::Null))
        })
        .await?;
    Ok(if cached.is_null() { None } else { Some(cached) })
}

#[derive(Debug, PartialEq, Eq)]
enum ProfileIdsCheck {
    MissingOrEmpty,
    Valid,
    TooMany,
}

/// `profilesHandler`: `ids` must be a non-empty array (anything else,
/// including a body that is not an object, is the "No profile ids" 400) of
/// at most 1000 entries.
fn validate_profile_ids(ids: Option<&[Value]>) -> ProfileIdsCheck {
    match ids {
        None | Some([]) => ProfileIdsCheck::MissingOrEmpty,
        Some(ids) if ids.len() > PROFILE_IDS_MAX => ProfileIdsCheck::TooMany,
        Some(_) => ProfileIdsCheck::Valid,
    }
}

fn early_profiles_response(check: ProfileIdsCheck) -> Option<Response> {
    match check {
        ProfileIdsCheck::MissingOrEmpty => Some(bad_request(MISSING_PROFILE_IDS_MSG)),
        ProfileIdsCheck::TooMany => Some(bad_request(&format!(
            "Too many profile ids were specified. Maximum allowed is {PROFILE_IDS_MAX}"
        ))),
        ProfileIdsCheck::Valid => None,
    }
}

/// `If-Modified-Since` read as `Date.parse` reads the forms clients send:
/// the HTTP date (RFC 7231 / 2822) and ISO 8601 (`toISOString()`; a date
/// alone, or a date-time without an offset, is UTC). `0` is ignored, as
/// lamb2 ignores a falsy timestamp.
fn parse_if_modified_since(value: &str) -> Option<i64> {
    let value = value.trim();
    let ms = chrono::DateTime::parse_from_rfc2822(value)
        .or_else(|_| chrono::DateTime::parse_from_rfc3339(value))
        .map(|dt| dt.timestamp_millis())
        .ok()
        .or_else(|| {
            ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M"]
                .iter()
                .find_map(|f| chrono::NaiveDateTime::parse_from_str(value, f).ok())
                .or_else(|| {
                    chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d")
                        .ok()
                        .and_then(|d| d.and_hms_opt(0, 0, 0))
                })
                .map(|dt| dt.and_utc().timestamp_millis())
        })?;
    (ms != 0).then_some(ms)
}

/// lamb2's 304 test: every entity found, rounded down to the second (the
/// header's precision), is at or before the header's time; vacuously true
/// when none is found.
fn not_modified_since(entities: &[Value], threshold: i64) -> bool {
    entities.iter().all(|entity| {
        entity
            .get("timestamp")
            .and_then(|t| t.as_i64().or_else(|| t.as_f64().map(|f| f as i64)))
            .is_some_and(|ts| ts.div_euclid(1000) * 1000 <= threshold)
    })
}

pub async fn profiles(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    // lamb2 reads `(await request.json()).ids`: a body that is not JSON, or
    // is `null`, throws before any check and is its bare 500.
    let body: Value = match serde_json::from_slice(&body) {
        Ok(Value::Null) | Err(_) => return lambdas_internal_error(),
        Ok(b) => b,
    };
    let ids = body.get("ids").and_then(Value::as_array).map(Vec::as_slice);
    if let Some(resp) = early_profiles_response(validate_profile_ids(ids)) {
        return resp;
    }
    // lamb2 sends the ids to the content server as they are; one that is not
    // a non-empty string fails that server's body schema (catalyst
    // `activeEntitiesBodySchema`), and `getProfiles` turns the failure into
    // `[]` for the whole batch.
    let Some(ids) = string_ids(ids.unwrap_or_default()) else {
        return Json(json!([])).into_response();
    };

    let modified_since: Option<i64> = headers
        .get("if-modified-since")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_if_modified_since);

    let pointers: Vec<String> = ids.iter().map(|id| id.to_lowercase()).collect();

    let cache_eligible = modified_since.is_none() && pointers.len() <= PROFILE_BATCH_MAX;
    if cache_eligible {
        return match profiles_via_cache(&state, &pointers).await {
            Ok(profiles) => {
                Json(profiles.into_iter().map(|(_, p)| p).collect::<Vec<_>>()).into_response()
            }
            Err(f) => f.into_response(),
        };
    }
    let entities = match active_entities(&state, &pointers).await {
        Ok(e) => e,
        Err(()) => return lambdas_internal_error(),
    };

    if let Some(threshold) = modified_since
        && not_modified_since(&entities, threshold)
    {
        return StatusCode::NOT_MODIFIED.into_response();
    }

    match process_and_cache_profiles(&state, &pointers, &entities).await {
        Ok(profiles) => {
            let profiles: Vec<Value> = profiles.into_iter().map(|(_, profile)| profile).collect();
            Json(Value::Array(profiles)).into_response()
        }
        Err(e) => Failure::Reader(e).into_response(),
    }
}

/// Compact form of a processed profile, the shape of `getMetadata` in
/// `decentraland/asset-bundle-registry@f9578d4a`
/// (`src/logic/sync/profile-sanitizer.ts`), in its key order: the first
/// avatar's `hasClaimedName`, `name` and `nameColor` as they are (absent stays
/// absent, `null` stays `null`) and its face snapshot.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileMetadata {
    pointer: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    has_claimed_name: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name_color: Option<Value>,
    thumbnail_url: String,
}

/// `None` for a profile without an avatar, where upstream's mapper would throw.
/// The thumbnail is `thumbnail_url` when given, else the first avatar's
/// `face256` snapshot.
pub fn profile_metadata(
    pointer: &str,
    profile: &Value,
    thumbnail_url: Option<&str>,
) -> Option<ProfileMetadata> {
    let avatar = profile
        .get("avatars")?
        .as_array()?
        .first()
        .filter(|avatar| avatar.is_object())?;
    let field = |key: &str| avatar.get(key).cloned();
    let thumbnail_url = thumbnail_url.unwrap_or_else(|| {
        avatar
            .pointer("/avatar/snapshots/face256")
            .and_then(Value::as_str)
            .unwrap_or_default()
    });
    Some(ProfileMetadata {
        pointer: pointer.to_string(),
        has_claimed_name: field("hasClaimedName"),
        name: field("name"),
        name_color: field("nameColor"),
        thumbnail_url: thumbnail_url.to_string(),
    })
}

/// Every id a non-empty string, or `None`.
fn string_ids(ids: &[Value]) -> Option<Vec<String>> {
    ids.iter()
        .map(|id| id.as_str().filter(|s| !s.is_empty()).map(str::to_string))
        .collect()
}

/// The request's pointers lowercased, each once, in request order, as
/// asset-bundle-registry's `getProfiles` keys them.
fn unique_pointers(ids: &[String]) -> Vec<String> {
    let mut seen = HashSet::with_capacity(ids.len());
    ids.iter()
        .map(|id| id.to_lowercase())
        .filter(|pointer| seen.insert(pointer.clone()))
        .collect()
}

/// `getProfilesMetadataHandler` of asset-bundle-registry, served from this
/// node's processed profiles (the same ownership filtering and claimed-name
/// check as `profiles`, so the compact and full answers agree). `ids` must be
/// an array of non-empty strings; a body that is not JSON is the bare 500
/// `request.json()` throws into. As the registry, the ids are not capped and
/// a failed read answers `200` with what was found: only ids that can name a
/// profile ([`is_profile_pointer`]) are looked up, [`PROFILE_LOOKUP_CHUNK`] at
/// a time, and a chunk whose read fails is left out, never served unvalidated.
pub async fn profiles_metadata(
    State(state): State<SharedState>,
    body: axum::body::Bytes,
) -> Response {
    let Ok(body) = serde_json::from_slice::<Value>(&body) else {
        return lambdas_internal_error();
    };
    let Some(ids) = body
        .get("ids")
        .and_then(Value::as_array)
        .and_then(|ids| string_ids(ids))
    else {
        return bad_request(METADATA_IDS_MSG);
    };
    let pointers: Vec<String> = unique_pointers(&ids)
        .into_iter()
        .filter(|pointer| is_profile_pointer(pointer))
        .collect();
    let mut compact: Vec<ProfileMetadata> = Vec::new();
    for chunk in pointers.chunks(PROFILE_LOOKUP_CHUNK) {
        match processed_profiles(&state, chunk).await {
            Ok(found) => compact.extend(
                found
                    .iter()
                    .filter_map(|(pointer, profile)| profile_metadata(pointer, profile, None)),
            ),
            Err(failure) => {
                failure.log("profiles/metadata: answering what was found");
                count_lookup_failure("lambdas_profiles_metadata", failure.layer());
            }
        }
    }
    Json(compact).into_response()
}

pub async fn profile_by_id(State(state): State<SharedState>, Path(id): Path<String>) -> Response {
    match fetch_profile_for_id(&state, &id).await {
        Ok(Some(metadata)) => Json(metadata).into_response(),
        Ok(None) => not_found("Profile not found"),
        Err(f) => f.into_response(),
    }
}

/// The singular alias never 404s: an unknown address is an empty profile.
pub async fn profile_alias(State(state): State<SharedState>, Path(id): Path<String>) -> Response {
    match fetch_profile_for_id(&state, &id).await {
        Ok(Some(processed)) => Json(processed).into_response(),
        Ok(None) => Json(json!({ "avatars": [], "timestamp": 0 })).into_response(),
        Err(f) => f.into_response(),
    }
}

#[cfg(test)]
mod tests {
    /// Only an address or a curated `default<n>` look is looked up by the
    /// uncapped routes: anything else cannot name a profile.
    #[test]
    fn profile_pointers_are_addresses_or_default_looks() {
        for pointer in [
            "0x0000000000000000000000000000000000000000",
            "0xabcdefabcdefabcdefabcdefabcdefabcdefabcd",
            "default1",
            "default160",
            "default000",
            "default999",
        ] {
            assert!(is_profile_pointer(pointer), "{pointer}");
        }
        for pointer in [
            "a0",
            "0x",
            "0xabc",
            "0x00000000000000000000000000000000000000000",
            "0xgggggggggggggggggggggggggggggggggggggggg",
            "default",
            "default1000",
            "defaultx",
            "bafkreiexample",
        ] {
            assert!(!is_profile_pointer(pointer), "{pointer}");
        }
    }

    use super::*;
    use std::sync::Arc;

    /// A peer that holds nothing and counts the `/entities/active` reads it is asked.
    async fn counting_peer() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use axum::routing::post;
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = calls.clone();
        let app = axum::Router::new().route(
            "/content/entities/active",
            post(move || async move {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Json(json!([]))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}/content"), calls)
    }

    #[tokio::test]
    async fn a_profile_this_node_does_not_hold_is_asked_of_the_read_through() {
        let Some(pool) = crate::db::scratch::pool(4).await else {
            eprintln!("skipping: CATALYST_TEST_DATABASE_URL not set");
            return;
        };
        let (source, calls) = counting_peer().await;
        let tmp = std::env::temp_dir().join(format!(
            "catalyst-shop-read-through-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let storage = Arc::new(crate::storage::Storage::from_config(&tmp).await.unwrap());
        let read_through = Arc::new(crate::content::read_through::ReadThrough::new(
            pool.clone(),
            storage,
            reqwest::Client::new(),
            vec![source],
            None,
            crate::content::read_through::Fills::new(1, std::time::Duration::from_secs(30)),
            crate::content::sync_deployer::SyncValidation::AuthChainOnly,
        ));
        let address = format!("0x{}{}", uuid::Uuid::new_v4().simple(), "00000000");

        let without = MarketplaceState::new(
            pool.clone(),
            crate::http::Outbound::for_tests(),
            super::super::Settings::for_tests(),
        );
        assert!(
            active_entities(&without, std::slice::from_ref(&address))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);

        let with = without.with_read_through(Some(read_through));
        assert!(
            active_entities(&with, std::slice::from_ref(&address))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        // The outfits read the same view (lamb2's content client).
        let with = Arc::new(with);
        let resp = super::super::lambdas_catalog::outfits(State(with), Path(address.clone())).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        let _ = std::fs::remove_dir_all(tmp);
    }

    /// Actual upstream reads filter invalid pointers, retain every valid pointer,
    /// cap each fill and reuse negative results on repeated HTTP requests.
    #[tokio::test]
    async fn registry_profile_routes_bound_actual_read_through_requests() {
        use axum::body::{Body, to_bytes};
        use axum::http::Request;
        use axum::routing::post;
        use tower::ServiceExt;

        let Some(pool) = crate::db::scratch::pool(4).await else {
            eprintln!("skipping: CATALYST_TEST_DATABASE_URL not set");
            return;
        };
        let requests = Arc::new(std::sync::Mutex::new(Vec::<Vec<String>>::new()));
        let observed = requests.clone();
        let peer = axum::Router::new().route(
            "/content/entities/active",
            post(move |Json(body): Json<Value>| {
                let observed = observed.clone();
                async move {
                    let pointers = serde_json::from_value(body["pointers"].clone()).unwrap();
                    observed.lock().unwrap().push(pointers);
                    Json(json!([]))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, peer).await.unwrap() });
        let tmp = std::env::temp_dir().join(format!(
            "catalyst-profile-budget-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let storage = Arc::new(crate::storage::Storage::from_config(&tmp).await.unwrap());
        let read_through = Arc::new(crate::content::read_through::ReadThrough::new(
            pool.clone(),
            storage,
            reqwest::Client::new(),
            vec![format!("http://{address}/content")],
            None,
            crate::content::read_through::Fills::new(1, Duration::from_secs(30)),
            crate::content::sync_deployer::SyncValidation::AuthChainOnly,
        ));
        let state = Arc::new(
            MarketplaceState::new(
                pool,
                crate::http::Outbound::for_tests(),
                super::super::Settings::for_tests(),
            )
            .with_read_through(Some(read_through)),
        );
        let app = super::super::router(state.clone())
            .nest("/asset-bundle-registry", registry::router(state));
        let pointer_tag = uuid::Uuid::new_v4().simple().to_string();
        for (route_index, route) in [
            "/asset-bundle-registry/profiles",
            "/asset-bundle-registry/profiles/metadata",
            "/lambdas/profiles/metadata",
        ]
        .into_iter()
        .enumerate()
        {
            requests.lock().unwrap().clear();
            let short_ids: Vec<_> = (0..30_000).map(|i| format!("a{i}")).collect();
            let valid_ids: Vec<_> = (0..1001)
                .map(|i| format!("0x{pointer_tag}{route_index:04x}{i:04x}"))
                .chain([format!("default{}", route_index + 1)])
                .collect();
            for ids in [&short_ids, &valid_ids] {
                let mut request_ids = ids.clone();
                if ids.len() == valid_ids.len() {
                    request_ids.push(valid_ids[0].to_uppercase());
                    request_ids.push("not-a-profile".into());
                }
                let body = json!({"ids": request_ids}).to_string();
                assert!(body.len() < 256 * 1024);
                for _ in 0..2 {
                    let response = app
                        .clone()
                        .oneshot(
                            Request::post(route)
                                .header("content-type", "application/json")
                                .body(Body::from(body.clone()))
                                .unwrap(),
                        )
                        .await
                        .unwrap();
                    assert_eq!(response.status(), StatusCode::OK);
                    assert_eq!(
                        to_bytes(response.into_body(), 1024).await.unwrap().as_ref(),
                        b"[]"
                    );
                }
                let calls = requests.lock().unwrap();
                if ids.len() == short_ids.len() {
                    assert!(
                        calls.is_empty(),
                        "{route}: invalid pointers must not reach upstream"
                    );
                } else {
                    assert_eq!(
                        calls.len(),
                        2,
                        "{route}: two bounded fills; repeat uses negative cache"
                    );
                    assert!(calls.iter().all(|call| call.len() <= PROFILE_LOOKUP_CHUNK));
                    let mut actual: Vec<_> = calls.iter().flatten().cloned().collect();
                    actual.sort();
                    let mut expected = valid_ids.clone();
                    expected.sort();
                    assert_eq!(
                        actual, expected,
                        "{route}: every valid pointer is queried once"
                    );
                }
            }
        }
        server.abort();
        let _ = std::fs::remove_dir_all(tmp);
    }

    /// Each URN held with the given token ids.
    fn owned(held: &[(&str, &[&str])]) -> OwnedTokens {
        held.iter()
            .map(|(urn, tokens)| {
                (
                    urn.to_string(),
                    tokens.iter().map(|t| t.to_string()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn split_urn_strips_token_id_for_7segment_collections_v2() {
        let (urn, tok) = split_urn_and_token_id("urn:decentraland:matic:collections-v2:0xabc:0:42");
        assert_eq!(urn, "urn:decentraland:matic:collections-v2:0xabc:0");
        assert_eq!(tok, Some("42"));
        let (urn, tok) = split_urn_and_token_id("urn:decentraland:matic:collections-v2:0xabc:0");
        assert_eq!(urn, "urn:decentraland:matic:collections-v2:0xabc:0");
        assert_eq!(tok, None);
        let tp = "urn:decentraland:matic:collections-thirdparty:tp:coll:item";
        assert_eq!(split_urn_and_token_id(tp).1, None);
    }

    /// `profiles.ts` `isBaseEmote` is a substring test for the base emotes.
    #[test]
    fn base_emotes_are_the_off_chain_base_emotes_only() {
        for urn in [
            "urn:decentraland:off-chain:base-emotes:wave",
            "urn:decentraland:off-chain:base-emotes:wave:extra",
            "urn:decentraland:off-chain:base-emotes-fake:wave",
        ] {
            assert!(is_base_emote(urn), "{urn}");
        }
        for urn in [
            "urn:decentraland:off-chain:base-scene-emotes:wave",
            "urn:decentraland:off-chain:base-avatars:wave",
            "URN:decentraland:off-chain:base-emotes:wave",
            "invalid",
        ] {
            assert!(!is_base_emote(urn), "{urn}");
        }
    }

    /// `profiles.ts` with `ENSURE_ERC_721`: a named token must be held; the
    /// owned item comes back `<urn>:<token>`, the first held when none is named.
    #[test]
    fn ownership_filter_checks_the_token_and_appends_it() {
        let held = owned(&[(
            "urn:decentraland:matic:collections-v2:0xowned:0",
            &["7", "99"],
        )]);
        let mut avatars = vec![json!({
            "avatar": {
                "wearables": [
                    "urn:decentraland:off-chain:base-avatars:eyebrows_00",
                    "urn:decentraland:matic:collections-v2:0xowned:0",
                    "urn:decentraland:matic:collections-v2:0xowned:0:99",
                    "urn:decentraland:matic:collections-v2:0xowned:0:5",
                    "urn:decentraland:matic:collections-v2:0xnope:0"
                ]
            }
        })];
        filter_avatars_by_ownership(&mut avatars, Some(&held));
        assert_eq!(
            avatars[0]["avatar"]["wearables"],
            json!([
                "urn:decentraland:off-chain:base-avatars:eyebrows_00",
                "urn:decentraland:matic:collections-v2:0xowned:0:7",
                "urn:decentraland:matic:collections-v2:0xowned:0:99",
            ]),
            "token 5 is not held, 0xnope not owned"
        );
        assert_eq!(
            avatars[0]["avatar"]["emotes"],
            json!([]),
            "emotes is always present"
        );
    }

    #[test]
    fn ownership_filter_reemits_owned_emotes_as_urn_and_slot() {
        let held = owned(&[("urn:decentraland:ethereum:collections-v1:0xc:dance", &["3"])]);
        let mut avatars = vec![json!({
            "avatar": {
                "emotes": [
                    {"slot": 0, "urn": "handsair"},
                    {"slot": 1, "urn": "urn:decentraland:off-chain:base-emotes:wave", "extra": 1},
                    {"slot": 2, "urn": "urn:decentraland:ethereum:collections-v1:0xc:dance", "extra": 1},
                    {"slot": 3, "urn": "urn:decentraland:matic:collections-v2:0xx:1"},
                    {"slot": 4, "urn": "urn:decentraland:mainnet:collections-v1:0xc:dance"},
                    {"slot": 5, "urn": "urn:decentraland:off-chain:base-scene-emotes:clap"}
                ]
            }
        })];
        filter_avatars_by_ownership(&mut avatars, Some(&held));
        assert_eq!(
            avatars[0]["avatar"]["emotes"],
            json!([
                {"slot": 0, "urn": "handsair"},
                {"slot": 1, "urn": "urn:decentraland:off-chain:base-emotes:wave", "extra": 1},
                {"slot": 2, "urn": "urn:decentraland:ethereum:collections-v1:0xc:dance:3"},
            ]),
            "the `mainnet` spelling and a base-scene emote are not owned"
        );
    }

    /// No collections reader: the items stay as deployed, `emotes` still present.
    #[test]
    fn without_a_reader_items_stay_as_deployed() {
        let mut avatars = vec![
            json!({ "avatar": { "wearables": ["urn:decentraland:matic:collections-v2:0xa:1"] } }),
        ];
        filter_avatars_by_ownership(&mut avatars, None);
        assert_eq!(
            avatars[0]["avatar"],
            json!({ "wearables": ["urn:decentraland:matic:collections-v2:0xa:1"], "emotes": [] })
        );
    }

    #[test]
    fn collect_ownership_urns_skips_base_items_and_strips_token_ids() {
        let metadata = json!({ "avatars": [{ "avatar": {
            "wearables": [
                "urn:decentraland:off-chain:base-avatars:eyebrows_00",
                "urn:decentraland:mainnet:collections-v1:0xc:hat",
                "urn:decentraland:matic:collections-v2:0xabc:0:42"
            ],
            "emotes": [ {"urn": "wave"}, {"urn": "urn:decentraland:off-chain:base-emotes:wave"}, {"urn": "urn:decentraland:matic:collections-v2:0xabc:9"} ]
        } }] });
        assert_eq!(
            collect_ownership_urns(&metadata),
            vec![
                "urn:decentraland:mainnet:collections-v1:0xc:hat",
                "urn:decentraland:matic:collections-v2:0xabc:0",
                "urn:decentraland:matic:collections-v2:0xabc:9",
            ]
        );
    }

    #[test]
    fn claimed_name_follows_the_owned_names() {
        let mut metadata =
            json!({ "avatars": [{ "name": "alice" }, { "name": "mallory" }, { "x": 1 }] });
        apply_claimed_name(&mut metadata, &["alice".to_string()]);
        assert_eq!(metadata["avatars"][0]["hasClaimedName"], true);
        assert_eq!(metadata["avatars"][1]["hasClaimedName"], false);
        assert_eq!(metadata["avatars"][2]["hasClaimedName"], false);
    }

    /// `addBaseUrlToSnapshots`: face and body always rewritten, every other
    /// key left as deployed, a missing object created.
    #[test]
    fn snapshots_point_at_the_cdn_or_at_our_profile_images() {
        let mut m = json!({ "avatars": [{ "avatar": { "snapshots": { "face256": "bafyface", "body": "https://x/already.png", "face128": "bafy128" } } }] });
        rewrite_snapshot_urls(
            "Qm1",
            &mut m,
            Some("https://cdn.example"),
            "http://localhost/profile-images/",
        );
        assert_eq!(
            m["avatars"][0]["avatar"]["snapshots"],
            json!({
                "face256": "https://cdn.example/entities/Qm1/face.png",
                "body": "https://cdn.example/entities/Qm1/body.png",
                "face128": "bafy128",
            })
        );

        let mut m = json!({ "avatars": [{ "avatar": { "snapshots": { "face256": "bafyface", "body": "https://x/already.png", "face128": "bafy128" } } }] });
        rewrite_snapshot_urls("Qm1", &mut m, None, "http://localhost/profile-images");
        assert_eq!(
            m["avatars"][0]["avatar"]["snapshots"],
            json!({
                "face256": "http://localhost/profile-images/entities/Qm1/face.png",
                "body": "http://localhost/profile-images/entities/Qm1/body.png",
                "face128": "bafy128",
            })
        );
    }

    #[test]
    fn snapshots_a_profile_never_uploaded_point_at_our_renders() {
        let mut m = json!({ "avatars": [{ "avatar": { "bodyShape": "urn:x" } }] });
        rewrite_snapshot_urls("Qm1", &mut m, None, "http://localhost/profile-images/");
        assert_eq!(
            m["avatars"][0]["avatar"]["snapshots"],
            json!({
                "face256": "http://localhost/profile-images/entities/Qm1/face.png",
                "body": "http://localhost/profile-images/entities/Qm1/body.png",
            })
        );
    }

    /// `sanitizeLinks`: `LinkUrl.validate`, then the decoded retry; the
    /// host is not checked.
    #[test]
    fn links_keep_what_link_url_accepts() {
        let mut m = json!({ "avatars": [{ "links": [
            { "title": "ok", "url": "https://decentraland.org/x" },
            { "title": "encoded", "url": "https%3A%2F%2Fdecentraland.org%2Fy" },
            { "title": "js", "url": "javascript:alert(1)" },
            { "title": "internal", "url": "http://127.0.0.1/" },
            { "title": "single label", "url": "HTTP://intranet" },
            { "title": "space", "url": "https://ab c" },
            { "title": "none" }
        ] }] });
        sanitize_links(&mut m);
        let urls: Vec<&str> = m["avatars"][0]["links"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["url"].as_str().unwrap())
            .collect();
        assert_eq!(
            urls,
            [
                "https://decentraland.org/x",
                "https://decentraland.org/y",
                "http://127.0.0.1/",
                "HTTP://intranet",
            ]
        );
    }

    fn metadata_input(with_color: bool) -> Value {
        let mut avatar = json!({
            "ethAddress": "0xabc", "userId": "0xabc", "name": "Ryumi", "hasClaimedName": true,
            "avatar": { "snapshots": {
                "face256": "https://cdn.example/entities/bafy/face.png",
                "body": "https://cdn.example/entities/bafy/body.png"
            } }
        });
        if with_color {
            avatar["nameColor"] = json!({ "r": 1.0, "g": 0.5, "b": 0.0 });
        }
        json!({ "avatars": [avatar], "timestamp": 1 })
    }

    fn metadata_json(pointer: &str, profile: &Value) -> String {
        serde_json::to_string(&profile_metadata(pointer, profile, None).unwrap()).unwrap()
    }

    #[test]
    fn profile_metadata_projects_the_first_avatar_in_upstream_key_order() {
        // The top-level keys follow `getMetadata`; `nameColor` is passed
        // through as a `Value`, whose keys serde_json writes sorted.
        assert_eq!(
            metadata_json("0xabc", &metadata_input(true)),
            r#"{"pointer":"0xabc","hasClaimedName":true,"name":"Ryumi","nameColor":{"b":0.0,"g":0.5,"r":1.0},"thumbnailUrl":"https://cdn.example/entities/bafy/face.png"}"#
        );
    }

    #[test]
    fn profile_metadata_leaves_absent_fields_absent_and_null_null() {
        assert_eq!(
            metadata_json("0xabc", &metadata_input(false)),
            r#"{"pointer":"0xabc","hasClaimedName":true,"name":"Ryumi","thumbnailUrl":"https://cdn.example/entities/bafy/face.png"}"#
        );
        let bare = json!({ "avatars": [{ "ethAddress": "0xabc", "nameColor": null }] });
        assert_eq!(
            metadata_json("0xabc", &bare),
            r#"{"pointer":"0xabc","nameColor":null,"thumbnailUrl":""}"#
        );
    }

    #[test]
    fn profile_metadata_skips_a_profile_without_avatars() {
        assert!(
            profile_metadata("0xabc", &json!({ "avatars": [], "timestamp": 1 }), None).is_none()
        );
        for first in [json!(null), json!(1), json!("avatar")] {
            let profile = json!({ "avatars": [first], "timestamp": 1 });
            assert!(
                profile_metadata("0xabc", &profile, None).is_none(),
                "{profile}"
            );
        }
    }

    #[test]
    fn unique_pointers_lowercase_and_keep_the_first() {
        let ids: Vec<String> = ["0xB", "0xa", "0xb", "0xA"].map(String::from).to_vec();
        assert_eq!(unique_pointers(&ids), ["0xb", "0xa"]);
    }

    #[test]
    fn validate_profile_ids_boundaries() {
        assert_eq!(validate_profile_ids(None), ProfileIdsCheck::MissingOrEmpty);
        let empty: Vec<Value> = Vec::new();
        assert_eq!(
            validate_profile_ids(Some(&empty)),
            ProfileIdsCheck::MissingOrEmpty
        );
        let ids = vec![json!("0xabc")];
        assert_eq!(validate_profile_ids(Some(&ids)), ProfileIdsCheck::Valid);
        let ids: Vec<Value> = (0..PROFILE_IDS_MAX).map(|i| json!(i)).collect();
        assert_eq!(validate_profile_ids(Some(&ids)), ProfileIdsCheck::Valid);
        let ids: Vec<Value> = (0..=PROFILE_IDS_MAX).map(|i| json!(i)).collect();
        assert_eq!(validate_profile_ids(Some(&ids)), ProfileIdsCheck::TooMany);
        const { assert!(PROFILE_BATCH_MAX < PROFILE_IDS_MAX) };
        let ids: Vec<Value> = (0..=PROFILE_BATCH_MAX).map(|i| json!(i)).collect();
        assert_eq!(
            validate_profile_ids(Some(&ids)),
            ProfileIdsCheck::Valid,
            "the batch max is only cache eligibility"
        );
    }

    #[test]
    fn early_profiles_response_is_400_for_missing_and_empty_ids() {
        assert_eq!(
            early_profiles_response(validate_profile_ids(Some(&[])))
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            early_profiles_response(validate_profile_ids(None))
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        let ids = vec![json!("0xabc")];
        assert!(early_profiles_response(validate_profile_ids(Some(&ids))).is_none());
    }

    async fn post_profiles(body: &'static str) -> (StatusCode, Value) {
        let state = super::super::tests::support::state_without_db();
        let resp = profiles(
            State(state),
            HeaderMap::new(),
            axum::body::Bytes::from_static(body.as_bytes()),
        )
        .await;
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    /// `profilesHandler`: `request.json()` throws on a body that is not JSON
    /// and `null.ids` throws, both the bare 500; any other body without an
    /// `ids` array is the "No profile ids" 400; a non-string id fails the
    /// content read, which `getProfiles` answers `[]`.
    #[tokio::test]
    async fn malformed_profile_bodies_answer_as_lamb2() {
        let internal = json!({ "error": "Internal Server Error" });
        let missing = json!({ "error": "Bad request", "message": MISSING_PROFILE_IDS_MSG });
        let empty = json!([]);
        for (body, status, expected) in [
            ("not json", StatusCode::INTERNAL_SERVER_ERROR, &internal),
            ("", StatusCode::INTERNAL_SERVER_ERROR, &internal),
            ("null", StatusCode::INTERNAL_SERVER_ERROR, &internal),
            ("5", StatusCode::BAD_REQUEST, &missing),
            ("[]", StatusCode::BAD_REQUEST, &missing),
            (r#"{"ids":"0xabc"}"#, StatusCode::BAD_REQUEST, &missing),
            (
                r#"{"ids":{"0":"0xabc"}}"#,
                StatusCode::BAD_REQUEST,
                &missing,
            ),
            (r#"{"ids":["0xabc",5]}"#, StatusCode::OK, &empty),
            (r#"{"ids":[null]}"#, StatusCode::OK, &empty),
            (r#"{"ids":[""]}"#, StatusCode::OK, &empty),
        ] {
            let got = post_profiles(body).await;
            assert_eq!((got.0, &got.1), (status, expected), "{body}");
        }
    }

    #[test]
    fn if_modified_since_reads_http_and_iso_dates() {
        let ms = 1_700_000_000_000;
        for value in [
            "Tue, 14 Nov 2023 22:13:20 GMT",
            "2023-11-14T22:13:20.000Z",
            "2023-11-14T22:13:20Z",
            "2023-11-14T22:13:20",
        ] {
            assert_eq!(parse_if_modified_since(value), Some(ms), "{value}");
        }
        assert_eq!(
            parse_if_modified_since("2023-11-14"),
            Some(1_699_920_000_000)
        );
        assert_eq!(
            parse_if_modified_since("Thu, 01 Jan 1970 00:00:00 GMT"),
            None
        );
        assert_eq!(parse_if_modified_since("yesterday"), None);
    }

    /// lamb2 rounds each timestamp down to the second; no entity is not modified.
    #[test]
    fn not_modified_rounds_to_the_second_and_holds_for_no_entity() {
        let entities = vec![json!({ "timestamp": 1_700_000_000_999i64 })];
        assert!(not_modified_since(&entities, 1_700_000_000_000));
        assert!(!not_modified_since(&entities, 1_699_999_999_999));
        assert!(not_modified_since(&[], 1));
    }
}
