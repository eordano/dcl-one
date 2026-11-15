//! The metadata of each entity type against the `@dcl/schemas` 27.2 schema
//! the reference validator applies after ADR-45: required fields, JSON
//! types, enums, numeric ranges, patterns, array bounds and the
//! standard/base/third-party item variants and the linked-wearable
//! `mappings`. Not carried: the spring bones shape (checked by
//! [`super::spring_bones`]).
//!
//! The reference's sentence is matched exactly; after it comes only the
//! first failure, as a path and an ajv-style reason. The reference lists
//! every ajv error (`allErrors`) without a path, including the errors of
//! each failed `oneOf` branch and the schemas' `errorMessage` overrides, in
//! ajv's evaluation order, which a hand-written check does not reproduce.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value};

use super::error::ValidationResponse;
use super::types::*;
use crate::auth::is_eth_address;
use crate::entity::is_ipfs_v2;
use crate::shared::js::is_js_whitespace;

type Check = Result<(), String>;
type Obj = Map<String, Value>;

const ROOT: &str = "metadata";

/// The reference `metadataSchemaValidateFn`.
pub fn validate_metadata(entity: &Entity) -> ValidationResponse {
    if entity.timestamp < adr_timestamps::ADR_45 {
        return ValidationResponse::Ok;
    }
    let metadata = entity.metadata.as_ref().unwrap_or(&Value::Null);
    let result = match entity.entity_type {
        EntityType::Scene => scene(metadata),
        EntityType::Profile => profile(metadata),
        EntityType::Wearable => wearable(metadata),
        EntityType::Store => store(metadata),
        EntityType::Emote => emote(metadata),
        EntityType::Outfits => outfits(metadata),
    };
    match result {
        Ok(()) => ValidationResponse::Ok,
        Err(reason) => ValidationResponse::from_errors(vec![
            format!(
                "The metadata for this entity type ({}) is not valid.",
                entity.entity_type
            ),
            reason,
        ]),
    }
}

/// A scene's metadata against `Scene.validate`: the first failure, as a
/// path and an ajv-style reason.
pub fn validate_scene_metadata(metadata: &Value) -> Result<(), String> {
    scene(metadata)
}

/// The ADR-287 properties that make an emote social, all or none.
pub(super) const SOCIAL_EMOTE_PROPS: [&str; 3] =
    ["startAnimation", "randomizeOutcomes", "outcomes"];

const MAX_SOCIAL_EMOTE_OUTCOMES: usize = 3;

// The primitives.

fn child(p: &str, key: &str) -> String {
    format!("{p}.{key}")
}

fn idx(p: &str, i: usize) -> String {
    format!("{p}[{i}]")
}

fn obj<'a>(v: &'a Value, p: &str) -> Result<&'a Obj, String> {
    v.as_object().ok_or_else(|| format!("{p} must be object"))
}

fn req<'a>(m: &'a Obj, key: &str, p: &str) -> Result<&'a Value, String> {
    m.get(key)
        .ok_or_else(|| format!("{p} must have required property '{key}'"))
}

fn string<'a>(v: &'a Value, p: &str) -> Result<&'a str, String> {
    v.as_str().ok_or_else(|| format!("{p} must be string"))
}

fn non_empty(v: &Value, p: &str) -> Check {
    if string(v, p)?.is_empty() {
        return Err(format!("{p} must NOT have fewer than 1 characters"));
    }
    Ok(())
}

fn number(v: &Value, p: &str) -> Result<f64, String> {
    v.as_f64().ok_or_else(|| format!("{p} must be number"))
}

fn integer(v: &Value, p: &str) -> Check {
    match v.as_f64() {
        Some(n) if n.fract() == 0.0 => Ok(()),
        _ => Err(format!("{p} must be integer")),
    }
}

fn boolean(v: &Value, p: &str) -> Check {
    v.as_bool()
        .map(|_| ())
        .ok_or_else(|| format!("{p} must be boolean"))
}

fn array<'a>(v: &'a Value, p: &str) -> Result<&'a Vec<Value>, String> {
    v.as_array().ok_or_else(|| format!("{p} must be array"))
}

fn min_items(a: &[Value], n: usize, p: &str) -> Check {
    if a.len() < n {
        return Err(format!("{p} must NOT have fewer than {n} items"));
    }
    Ok(())
}

fn max_items(a: &[Value], n: usize, p: &str) -> Check {
    if a.len() > n {
        return Err(format!("{p} must NOT have more than {n} items"));
    }
    Ok(())
}

/// `uniqueItems` over string items, scanned from the end as ajv does.
fn unique(a: &[Value], p: &str) -> Check {
    let mut seen: HashMap<&str, usize> = HashMap::with_capacity(a.len());
    for (i, x) in a.iter().enumerate().rev() {
        let Some(s) = x.as_str() else { continue };
        if let Some(j) = seen.insert(s, i) {
            return Err(format!(
                "{p} must NOT have duplicate items (items ## {j} and {i} are identical)"
            ));
        }
    }
    Ok(())
}

fn each(a: &[Value], p: &str, f: impl Fn(&Value, &str) -> Check) -> Check {
    a.iter().enumerate().try_for_each(|(i, x)| f(x, &idx(p, i)))
}

fn strings(v: &Value, p: &str) -> Check {
    each(array(v, p)?, p, |x, p| string(x, p).map(|_| ()))
}

fn non_empty_strings(v: &Value, p: &str) -> Check {
    each(array(v, p)?, p, non_empty)
}

fn one_of(v: &Value, allowed: &[&str], p: &str) -> Check {
    if !allowed.contains(&string(v, p)?) {
        return Err(format!("{p} must be equal to one of the allowed values"));
    }
    Ok(())
}

fn unit(v: &Value, p: &str) -> Check {
    let n = number(v, p)?;
    if n < 0.0 {
        return Err(format!("{p} must be >= 0"));
    }
    if n > 1.0 {
        return Err(format!("{p} must be <= 1"));
    }
    Ok(())
}

fn no_extra_keys(m: &Obj, allowed: &[&str], p: &str) -> Check {
    match m.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(k) => Err(format!("{p} must NOT have additional property '{k}'")),
        None => Ok(()),
    }
}

/// A required property checked by `f`.
fn required(m: &Obj, key: &str, p: &str, f: impl Fn(&Value, &str) -> Check) -> Check {
    f(req(m, key, p)?, &child(p, key))
}

/// An optional property that may not be `null`.
fn optional(m: &Obj, key: &str, p: &str, f: impl Fn(&Value, &str) -> Check) -> Check {
    m.get(key).map_or(Ok(()), |v| f(v, &child(p, key)))
}

/// An optional property that may be `null`.
fn nullable(m: &Obj, key: &str, p: &str, f: impl Fn(&Value, &str) -> Check) -> Check {
    match m.get(key) {
        None | Some(Value::Null) => Ok(()),
        Some(v) => f(v, &child(p, key)),
    }
}

fn str_check(v: &Value, p: &str) -> Check {
    string(v, p).map(|_| ())
}

fn num_check(v: &Value, p: &str) -> Check {
    number(v, p).map(|_| ())
}

fn pattern(v: &Value, p: &str, matches: fn(&str) -> bool) -> Check {
    if !matches(string(v, p)?) {
        return Err(format!("{p} must match the expected pattern"));
    }
    Ok(())
}

/// `^(?:https?):\/\/[^\s/$.?#].[^\s]*$` with the JS `u` flag.
fn is_link_url(s: &str) -> bool {
    let Some(rest) = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))
    else {
        return false;
    };
    let mut chars = rest.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| !is_js_whitespace(c) && !"/$.?#".contains(c));
    first_ok
        && chars.next().is_some_and(|c| !is_js_line_terminator(c))
        && !chars.any(is_js_whitespace)
}

/// What JS `.` does not match.
fn is_js_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// `^(?:0|-?[1-9][0-9]*),(?:0|-?[1-9][0-9]*)$`
fn is_parcel(s: &str) -> bool {
    fn coord(c: &str) -> bool {
        let digits = c.strip_prefix('-').unwrap_or(c);
        if c == "0" {
            return true;
        }
        !digits.is_empty() && !digits.starts_with('0') && digits.bytes().all(|b| b.is_ascii_digit())
    }
    s.split_once(',').is_some_and(|(x, y)| coord(x) && coord(y))
}

fn color(v: &Value, p: &str) -> Check {
    let m = obj(v, p)?;
    for key in ["r", "g", "b"] {
        required(m, key, p, unit)?;
    }
    Ok(())
}

fn colored(v: &Value, p: &str) -> Check {
    required(obj(v, p)?, "color", p, color)
}

// The shared enums, pinned in `dcl-schemas/enums.json`.

const WEARABLE_CATEGORIES: &[&str] = &[
    "eyebrows",
    "eyes",
    "facial_hair",
    "hair",
    "body_shape",
    "mouth",
    "upper_body",
    "lower_body",
    "feet",
    "earring",
    "eyewear",
    "hat",
    "helmet",
    "mask",
    "tiara",
    "top_head",
    "skin",
    "hands_wear",
];
const BODY_SHAPES: &[&str] = &[
    "urn:decentraland:off-chain:base-avatars:BaseMale",
    "urn:decentraland:off-chain:base-avatars:BaseFemale",
];
const RARITIES: &[&str] = &[
    "unique",
    "mythic",
    "exotic",
    "legendary",
    "epic",
    "rare",
    "uncommon",
    "common",
];
const HIDEABLE_BODY_PARTS: &[&str] = &["head", "hands"];
const LOCALES: &[&str] = &["en", "es"];
const EMOTE_CATEGORIES: &[&str] = &[
    "dance",
    "stunt",
    "greetings",
    "fun",
    "poses",
    "reactions",
    "horror",
    "miscellaneous",
];
const SCENE_PERMISSIONS: &[&str] = &[
    "ALLOW_TO_MOVE_PLAYER_INSIDE_SCENE",
    "ALLOW_TO_TRIGGER_AVATAR_EMOTE",
    "ALLOW_MEDIA_HOSTNAMES",
    "USE_WEB3_API",
    "USE_FETCH",
    "USE_WEBSOCKET",
    "OPEN_EXTERNAL_LINK",
];
const VOICE_CHAT_TOGGLES: &[&str] = &["enabled", "disabled"];
const PORTABLE_EXPERIENCES_TOGGLES: &[&str] = &["enabled", "disabled", "hideUi"];
const NEARBY_VOICE_CHAT_TOGGLES: &[&str] = &["enabled", "disabled"];
const SOURCE_ROTATIONS: &[&str] = &["north", "east", "south", "west"];

fn hideable(v: &Value, p: &str) -> Check {
    let s = string(v, p)?;
    if WEARABLE_CATEGORIES.contains(&s) || HIDEABLE_BODY_PARTS.contains(&s) {
        return Ok(());
    }
    Err(format!("{p} must match exactly one schema in oneOf"))
}

fn hideables(v: &Value, p: &str) -> Check {
    each(array(v, p)?, p, hideable)
}

fn force_render(v: &Value, p: &str) -> Check {
    each(array(v, p)?, p, |x, p| one_of(x, WEARABLE_CATEGORIES, p))
}

// Profile.

fn profile(v: &Value) -> Check {
    let m = obj(v, ROOT)?;
    required(m, "avatars", ROOT, |v, p| each(array(v, p)?, p, avatar))
}

fn avatar(v: &Value, p: &str) -> Check {
    let m = obj(v, p)?;
    required(m, "name", p, str_check)?;
    required(m, "description", p, str_check)?;
    required(m, "ethAddress", p, |v, p| pattern(v, p, is_eth_address))?;
    required(m, "version", p, num_check)?;
    required(m, "tutorialStep", p, num_check)?;
    required(m, "avatar", p, avatar_body)?;
    required(m, "hasClaimedName", p, boolean)?;
    optional(m, "userId", p, str_check)?;
    nullable(m, "nameColor", p, color)?;
    nullable(m, "links", p, |v, p| {
        let links = array(v, p)?;
        max_items(links, 5, p)?;
        each(links, p, |v, p| {
            let m = obj(v, p)?;
            required(m, "title", p, str_check)?;
            required(m, "url", p, |v, p| {
                if string(v, p)?.chars().count() > 2083 {
                    return Err(format!("{p} must NOT have more than 2083 characters"));
                }
                pattern(v, p, is_link_url)
            })
        })
    })?;
    for key in [
        "country",
        "employmentStatus",
        "gender",
        "pronouns",
        "relationshipStatus",
        "sexualOrientation",
        "language",
        "profession",
        "realName",
        "hobbies",
        "email",
    ] {
        nullable(m, key, p, str_check)?;
    }
    nullable(m, "birthdate", p, num_check)?;
    for key in ["blocked", "muted", "interests"] {
        nullable(m, key, p, strings)?;
    }
    nullable(m, "hasConnectedWeb3", p, boolean)
}

fn avatar_body(v: &Value, p: &str) -> Check {
    let m = obj(v, p)?;
    required(m, "bodyShape", p, str_check)?;
    for key in ["eyes", "hair", "skin"] {
        required(m, key, p, colored)?;
    }
    optional(m, "wearables", p, strings)?;
    nullable(m, "forceRender", p, force_render)?;
    nullable(m, "emotes", p, |v, p| {
        each(array(v, p)?, p, |v, p| {
            let m = obj(v, p)?;
            required(m, "slot", p, num_check)?;
            required(m, "urn", p, str_check)
        })
    })?;
    nullable(m, "snapshots", p, |v, p| {
        let m = obj(v, p)?;
        required(m, "face256", p, |v, p| pattern(v, p, is_ipfs_v2))?;
        required(m, "body", p, |v, p| pattern(v, p, is_ipfs_v2))
    })
}

// Outfits.

fn outfits(v: &Value) -> Check {
    let m = obj(v, ROOT)?;
    required(m, "outfits", ROOT, |v, p| {
        each(array(v, p)?, p, |v, p| {
            let m = obj(v, p)?;
            required(m, "slot", p, num_check)?;
            required(m, "outfit", p, |v, p| {
                let m = obj(v, p)?;
                required(m, "bodyShape", p, str_check)?;
                for key in ["eyes", "hair", "skin"] {
                    required(m, key, p, colored)?;
                }
                required(m, "wearables", p, strings)?;
                nullable(m, "forceRender", p, force_render)
            })
        })
    })?;
    required(m, "namesForExtraSlots", ROOT, |v, p| {
        strings(v, p)?;
        unique(array(v, p)?, p)
    })
}

// Store.

fn store(v: &Value) -> Check {
    let m = obj(v, ROOT)?;
    required(m, "id", ROOT, str_check)?;
    required(m, "version", ROOT, num_check)?;
    required(m, "owner", ROOT, str_check)?;
    required(m, "description", ROOT, str_check)?;
    required(m, "links", ROOT, |v, p| {
        each(array(v, p)?, p, |v, p| {
            let m = obj(v, p)?;
            required(m, "name", p, str_check)?;
            required(m, "url", p, str_check)
        })
    })?;
    required(m, "images", ROOT, |v, p| {
        each(array(v, p)?, p, |v, p| {
            let m = obj(v, p)?;
            required(m, "name", p, str_check)?;
            required(m, "file", p, str_check)
        })
    })
}

// Scene.

fn scene(v: &Value) -> Check {
    let m = obj(v, ROOT)?;
    required(m, "main", ROOT, non_empty)?;
    required(m, "scene", ROOT, |v, p| {
        let m = obj(v, p)?;
        required(m, "base", p, |v, p| pattern(v, p, is_parcel))?;
        required(m, "parcels", p, |v, p| {
            let parcels = array(v, p)?;
            each(parcels, p, |v, p| pattern(v, p, is_parcel))?;
            min_items(parcels, 1, p)?;
            unique(parcels, p)?;
            if !parcels.contains(&m["base"]) {
                return Err(format!("{p} must contain at least 1 valid item(s)"));
            }
            Ok(())
        })
    })?;
    nullable(m, "isPortableExperience", ROOT, boolean)?;
    nullable(m, "menuBarIcon", ROOT, str_check)?;
    nullable(m, "display", ROOT, |v, p| {
        let m = obj(v, p)?;
        for key in ["title", "favicon", "description", "navmapThumbnail"] {
            nullable(m, key, p, str_check)?;
        }
        Ok(())
    })?;
    nullable(m, "owner", ROOT, str_check)?;
    nullable(m, "creator", ROOT, |v, p| {
        let s = string(v, p)?;
        if !s.is_empty() && !is_eth_address(s) {
            return Err("Creator must be a valid wallet address".to_string());
        }
        Ok(())
    })?;
    nullable(m, "contact", ROOT, |v, p| {
        let m = obj(v, p)?;
        for key in ["name", "email", "im", "url"] {
            nullable(m, key, p, str_check)?;
        }
        Ok(())
    })?;
    nullable(m, "tags", ROOT, non_empty_strings)?;
    nullable(m, "source", ROOT, scene_source)?;
    nullable(m, "spawnPoints", ROOT, |v, p| {
        each(array(v, p)?, p, spawn_point)
    })?;
    nullable(m, "requiredPermissions", ROOT, |v, p| {
        let a = array(v, p)?;
        each(a, p, |v, p| one_of(v, SCENE_PERMISSIONS, p))?;
        unique(a, p)
    })?;
    nullable(m, "featureToggles", ROOT, |v, p| {
        let m = obj(v, p)?;
        nullable(m, "voiceChat", p, |v, p| one_of(v, VOICE_CHAT_TOGGLES, p))?;
        nullable(m, "portableExperiences", p, |v, p| {
            one_of(v, PORTABLE_EXPERIENCES_TOGGLES, p)
        })?;
        nullable(m, "nearbyVoiceChat", p, |v, p| {
            one_of(v, NEARBY_VOICE_CHAT_TOGGLES, p)
        })
    })?;
    nullable(m, "worldConfiguration", ROOT, world_configuration)?;
    nullable(m, "allowedMediaHostnames", ROOT, strings)?;
    nullable(m, "landscapeTerrain", ROOT, boolean)?;
    media_hostnames(m)
}

fn scene_source(v: &Value, p: &str) -> Check {
    let m = obj(v, p)?;
    nullable(m, "version", p, num_check)?;
    required(m, "origin", p, str_check)?;
    required(m, "projectId", p, str_check)?;
    nullable(m, "point", p, |v, p| {
        let m = obj(v, p)?;
        required(m, "x", p, integer)?;
        required(m, "y", p, integer)
    })?;
    nullable(m, "rotation", p, |v, p| one_of(v, SOURCE_ROTATIONS, p))?;
    nullable(m, "layout", p, |v, p| {
        let m = obj(v, p)?;
        required(m, "rows", p, integer)?;
        required(m, "cols", p, integer)
    })?;
    nullable(m, "isEmpty", p, boolean)
}

fn spawn_point(v: &Value, p: &str) -> Check {
    let m = obj(v, p)?;
    nullable(m, "name", p, str_check)?;
    required(m, "position", p, |v, p| {
        let m = obj(v, p)?;
        let axes = ["x", "y", "z"];
        for key in axes {
            req(m, key, p)?;
        }
        let single = axes.iter().all(|k| m[*k].is_number());
        let multi = axes.iter().all(|k| {
            m[*k]
                .as_array()
                .is_some_and(|a| !a.is_empty() && a.iter().all(Value::is_number))
        });
        if single == multi {
            return Err(format!("{p} must match exactly one schema in oneOf"));
        }
        Ok(())
    })?;
    nullable(m, "default", p, boolean)?;
    nullable(m, "cameraTarget", p, |v, p| {
        let m = obj(v, p)?;
        for key in ["x", "y", "z"] {
            required(m, key, p, num_check)?;
        }
        Ok(())
    })
}

fn world_configuration(v: &Value, p: &str) -> Check {
    let m = obj(v, p)?;
    nullable(m, "name", p, str_check)?;
    nullable(m, "skybox", p, num_check)?;
    nullable(m, "minimapVisible", p, boolean)?;
    nullable(m, "miniMapConfig", p, |v, p| {
        let m = obj(v, p)?;
        nullable(m, "visible", p, boolean)?;
        nullable(m, "dataImage", p, str_check)?;
        nullable(m, "estateImage", p, str_check)
    })?;
    nullable(m, "skyboxConfig", p, |v, p| {
        let m = obj(v, p)?;
        nullable(m, "fixedTime", p, num_check)?;
        // `null` matches all three length branches of the oneOf, so it fails.
        optional(m, "textures", p, |v, p| {
            let fits =
                strings(v, p).is_ok() && v.as_array().is_some_and(|a| [0, 1, 6].contains(&a.len()));
            if !fits {
                return Err("customSkybox must be an array of 0, 1 or 6 strings".to_string());
            }
            Ok(())
        })
    })?;
    nullable(m, "fixedAdapter", p, str_check)?;
    nullable(m, "placesConfig", p, |v, p| {
        nullable(obj(v, p)?, "optOut", p, boolean)
    })
}

/// The `requiredPermissions` dependency: `ALLOW_MEDIA_HOSTNAMES` needs a
/// non-empty `allowedMediaHostnames`; without it the list must be empty.
fn media_hostnames(m: &Obj) -> Check {
    let Some(permissions) = m.get("requiredPermissions") else {
        return Ok(());
    };
    let p = child(ROOT, "allowedMediaHostnames");
    let allows_media = permissions
        .as_array()
        .is_some_and(|a| a.iter().any(|x| x == "ALLOW_MEDIA_HOSTNAMES"));
    match (allows_media, m.get("allowedMediaHostnames")) {
        (true, None) => Err(format!(
            "{ROOT} must have required property 'allowedMediaHostnames'"
        )),
        (true, Some(v)) => min_items(array(v, &p)?, 1, &p),
        (false, None | Some(Value::Null)) => Ok(()),
        (false, Some(v)) => max_items(array(v, &p)?, 0, &p),
    }
}

// Items.

fn i18n(v: &Value, p: &str) -> Check {
    let a = array(v, p)?;
    min_items(a, 1, p)?;
    each(a, p, |v, p| {
        let m = obj(v, p)?;
        required(m, "code", p, |v, p| one_of(v, LOCALES, p))?;
        required(m, "text", p, str_check)
    })?;
    let codes: Vec<&Value> = a.iter().map(|x| &x["code"]).collect();
    if codes
        .iter()
        .enumerate()
        .any(|(i, c)| codes[..i].contains(c))
    {
        return Err(format!("{p} array should not have duplicates for \"code\""));
    }
    Ok(())
}

fn metrics(v: &Value, p: &str) -> Check {
    let m = obj(v, p)?;
    for key in [
        "triangles",
        "materials",
        "textures",
        "meshes",
        "bodies",
        "entities",
    ] {
        required(m, key, p, num_check)?;
    }
    Ok(())
}

fn merkle_proof(v: &Value, p: &str) -> Check {
    let m = obj(v, p)?;
    required(m, "entityHash", p, str_check)?;
    required(m, "hashingKeys", p, strings)?;
    required(m, "index", p, num_check)?;
    required(m, "proof", p, strings)
}

fn content_map(v: &Value, p: &str) -> Check {
    obj(v, p)?
        .iter()
        .try_for_each(|(k, v)| str_check(v, &child(p, k)))
}

/// The properties every item type shares, checked when present.
fn item_properties(m: &Obj) -> Check {
    nullable(m, "menuBarIcon", ROOT, str_check)?;
    for key in ["id", "name", "description", "thumbnail", "image"] {
        optional(m, key, ROOT, str_check)?;
    }
    optional(m, "i18n", ROOT, i18n)?;
    nullable(m, "metrics", ROOT, metrics)?;
    optional(m, "collectionAddress", ROOT, str_check)?;
    optional(m, "rarity", ROOT, |v, p| one_of(v, RARITIES, p))?;
    optional(m, "merkleProof", ROOT, merkle_proof)?;
    optional(m, "content", ROOT, content_map)
}

fn has_all(m: &Obj, keys: &[&str]) -> bool {
    keys.iter().all(|k| m.contains_key(*k))
}

fn has_none(m: &Obj, keys: &[&str]) -> bool {
    !keys.iter().any(|k| m.contains_key(*k))
}

/// `isBaseAvatar` / `isBaseEmote`: `urn:decentraland:<any>:<registry>:<name>`.
fn is_base_item(m: &Obj, registries: &[&str]) -> bool {
    let Some(id) = m.get("id").and_then(Value::as_str) else {
        return false;
    };
    let parts: Vec<&str> = id.split(':').collect();
    parts.len() == 5 && parts[1] == "decentraland" && registries.contains(&parts[3])
}

/// `isThirdParty`: a valid merkle proof whose hashing keys are all present.
fn is_third_party(m: &Obj) -> bool {
    let shape = required(m, "merkleProof", ROOT, merkle_proof)
        .and_then(|_| required(m, "content", ROOT, content_map))
        .and_then(|_| optional(m, "mappings", ROOT, mappings));
    shape.is_ok()
        && m["merkleProof"]["hashingKeys"]
            .as_array()
            .is_some_and(|keys| {
                keys.iter()
                    .all(|k| k.as_str().is_some_and(|k| m.contains_key(k)))
            })
}

// Linked-wearable mappings.

const CONTRACT_NETWORKS: &[&str] = &[
    "mainnet",
    "base_mainnet",
    "base_sepolia",
    "monad_mainnet",
    "monad_testnet",
    "ape_mainnet",
    "ape_caldera",
    "matic",
    "sepolia",
    "amoy",
];
const MAPPING_TYPES: &[&str] = &["single", "any", "multiple", "range"];

fn is_token_id(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn token_id(v: &Value, p: &str) -> Check {
    pattern(v, p, is_token_id)
}

/// A decimal token id ordered by value, as the reference's `BigInt`.
#[derive(Clone, Copy)]
struct TokenId<'a>(&'a str);

impl PartialEq for TokenId<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for TokenId<'_> {}

impl Ord for TokenId<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        let a = self.0.trim_start_matches('0');
        let b = other.0.trim_start_matches('0');
        a.len().cmp(&b.len()).then_with(|| a.cmp(b))
    }
}

impl PartialOrd for TokenId<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// `Mappings`: networks to contracts to lists of `Mapping`, no two of one
/// contract overlapping.
fn mappings(v: &Value, p: &str) -> Check {
    let invalid = || {
        Err(format!(
            "{p} must pass \"_isMappingsValid\" keyword validation"
        ))
    };
    // Nullable, but the reference's overlap check throws on `null`.
    if v.is_null() {
        return invalid();
    }
    let m = obj(v, p)?;
    min_properties(m, p)?;
    no_extra_keys(m, CONTRACT_NETWORKS, p)?;
    for (network, contracts) in m {
        let p = child(p, network);
        let contracts = obj(contracts, &p)?;
        min_properties(contracts, &p)?;
        if let Some(k) = contracts.keys().find(|k| !is_eth_address(k)) {
            return Err(format!("{p} must NOT have additional property '{k}'"));
        }
        for (contract, list) in contracts {
            let p = child(&p, contract);
            each(array(list, &p)?, &p, mapping)?;
        }
    }
    if m.values().any(|contracts| {
        let mut by_contract: HashMap<String, Vec<&Obj>> = HashMap::new();
        for (contract, list) in contracts.as_object().into_iter().flatten() {
            by_contract
                .entry(contract.to_lowercase())
                .or_default()
                .extend(
                    list.as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_object),
                );
        }
        by_contract.values().any(|list| mappings_overlap(list))
    }) {
        return invalid();
    }
    Ok(())
}

fn min_properties(m: &Obj, p: &str) -> Check {
    if m.is_empty() {
        return Err(format!("{p} must NOT have fewer than 1 properties"));
    }
    Ok(())
}

fn mapping(v: &Value, p: &str) -> Check {
    let m = obj(v, p)?;
    required(m, "type", p, |v, p| one_of(v, MAPPING_TYPES, p))?;
    match m["type"].as_str().unwrap_or_default() {
        "single" => {
            required(m, "id", p, token_id)?;
            no_extra_keys(m, &["type", "id"], p)
        }
        "any" => no_extra_keys(m, &["type"], p),
        "range" => {
            required(m, "from", p, token_id)?;
            required(m, "to", p, token_id)?;
            no_extra_keys(m, &["type", "from", "to"], p)?;
            let (from, to) = (m["from"].as_str(), m["to"].as_str());
            if TokenId(from.unwrap_or_default()) > TokenId(to.unwrap_or_default()) {
                return Err(format!(
                    "{p} must pass \"_fromLessThanOrEqualTo\" keyword validation"
                ));
            }
            Ok(())
        }
        _ => {
            required(m, "ids", p, |v, p| {
                let ids = array(v, p)?;
                each(ids, p, token_id)?;
                min_items(ids, 1, p)?;
                unique(ids, p)
            })?;
            no_extra_keys(m, &["type", "ids"], p)
        }
    }
}

/// The reference's `createMappingsHelper` refusal: `any` overlaps
/// everything, ids match as strings and fall in ranges by value.
fn mappings_overlap(list: &[&Obj]) -> bool {
    let mut ids: Vec<TokenId> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut ranges: Vec<(TokenId, TokenId)> = Vec::new();
    for m in list {
        let str_of = |key: &str| m.get(key).and_then(Value::as_str).unwrap_or_default();
        match str_of("type") {
            "any" if list.len() > 1 => return true,
            "single" => ids.push(TokenId(str_of("id"))),
            "multiple" => ids.extend(
                m.get("ids")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(TokenId),
            ),
            "range" => ranges.push((TokenId(str_of("from")), TokenId(str_of("to")))),
            _ => {}
        }
    }
    if ids.iter().any(|id| !seen.insert(id.0)) {
        return true;
    }
    ids.sort_unstable();
    ranges.sort_unstable();
    let mut furthest: Option<TokenId> = None;
    ranges.iter().any(|&(from, to)| {
        let overlaps_previous = furthest.is_some_and(|end| from <= end);
        furthest = furthest.max(Some(to));
        let first_at_or_after = ids.partition_point(|id| *id < from);
        overlaps_previous || ids.get(first_at_or_after).is_some_and(|id| *id <= to)
    })
}

/// `@dcl/schemas` 27.2 also counts the scene emotes as base emotes.
const BASE_EMOTE_REGISTRIES: &[&str] = &["base-emotes", "base-scene-emotes"];

const BASE_ITEM_PROPS: [&str; 6] = ["id", "name", "description", "i18n", "thumbnail", "image"];

fn wearable(v: &Value) -> Check {
    let m = obj(v, ROOT)?;
    item_properties(m)?;
    optional(m, "data", ROOT, wearable_data)?;
    let standard_keys = [
        &BASE_ITEM_PROPS[..],
        &["data", "collectionAddress", "rarity"],
    ]
    .concat();
    let third_party_keys = [&BASE_ITEM_PROPS[..], &["data", "merkleProof", "content"]].concat();
    let variants = [
        has_all(m, &["id", "i18n"])
            && has_none(
                m,
                &[
                    "merkleProof",
                    "content",
                    "collectionAddress",
                    "rarity",
                    "mappings",
                ],
            )
            && is_base_item(m, &["base-avatars"]),
        has_all(m, &standard_keys) && has_none(m, &["merkleProof", "content", "mappings"]),
        has_all(m, &third_party_keys)
            && has_none(m, &["collectionAddress", "rarity"])
            && is_third_party(m),
    ];
    if variants.iter().filter(|v| **v).count() != 1 {
        return Err("either standard XOR thirdparty properties conditions must be met".to_string());
    }
    Ok(())
}

fn wearable_data(v: &Value, p: &str) -> Check {
    let m = obj(v, p)?;
    required(m, "replaces", p, hideables)?;
    required(m, "hides", p, hideables)?;
    required(m, "tags", p, non_empty_strings)?;
    required(m, "representations", p, |v, p| {
        let reps = array(v, p)?;
        min_items(reps, 1, p)?;
        each(reps, p, |v, p| {
            representation(v, p)?;
            let m = obj(v, p)?;
            required(m, "overrideHides", p, hideables)?;
            required(m, "overrideReplaces", p, hideables)
        })
    })?;
    required(m, "category", p, |v, p| one_of(v, WEARABLE_CATEGORIES, p))?;
    nullable(m, "removesDefaultHiding", p, hideables)?;
    nullable(m, "blockVrmExport", p, boolean)?;
    nullable(m, "outlineCompatible", p, boolean)
}

/// The representation fields wearables and emotes share.
fn representation(v: &Value, p: &str) -> Check {
    let m = obj(v, p)?;
    required(m, "bodyShapes", p, |v, p| {
        let a = array(v, p)?;
        each(a, p, |v, p| one_of(v, BODY_SHAPES, p))?;
        min_items(a, 1, p)?;
        unique(a, p)
    })?;
    required(m, "mainFile", p, non_empty)?;
    required(m, "contents", p, |v, p| {
        let a = array(v, p)?;
        strings(v, p)?;
        min_items(a, 1, p)?;
        unique(a, p)?;
        if !a.contains(&m["mainFile"]) {
            return Err(format!(
                "{p} should contain mainFile: {}",
                m["mainFile"].as_str().unwrap_or_default()
            ));
        }
        Ok(())
    })
}

fn emote(v: &Value) -> Check {
    let m = obj(v, ROOT)?;
    for key in BASE_ITEM_PROPS {
        req(m, key, ROOT)?;
    }
    item_properties(m)?;
    optional(m, "mappings", ROOT, mappings)?;
    let Some(data) = m.get("emoteDataADR74") else {
        return Err("emote should have \"emoteDataADR74\" and match its schema".to_string());
    };
    emote_data(data, &child(ROOT, "emoteDataADR74"))?;
    let variants = [
        has_all(m, &["id", "i18n"])
            && has_none(
                m,
                &["merkleProof", "content", "collectionAddress", "rarity"],
            )
            && is_base_item(m, BASE_EMOTE_REGISTRIES),
        has_all(m, &["collectionAddress", "rarity"]) && has_none(m, &["merkleProof", "content"]),
        has_all(m, &["merkleProof", "content"])
            && has_none(m, &["collectionAddress", "rarity"])
            && is_third_party(m),
    ];
    if variants.iter().filter(|v| **v).count() != 1 {
        return Err("emote should have either standard or thirdparty properties".to_string());
    }
    Ok(())
}

fn emote_data(v: &Value, p: &str) -> Check {
    let m = obj(v, p)?;
    required(m, "category", p, |v, p| one_of(v, EMOTE_CATEGORIES, p))?;
    required(m, "tags", p, non_empty_strings)?;
    required(m, "representations", p, |v, p| {
        let reps = array(v, p)?;
        min_items(reps, 1, p)?;
        each(reps, p, representation)
    })?;
    required(m, "loop", p, boolean)?;
    nullable(m, "startAnimation", p, start_animation)?;
    nullable(m, "randomizeOutcomes", p, boolean)?;
    nullable(m, "outcomes", p, |v, p| {
        let a = array(v, p)?;
        min_items(a, 1, p)?;
        max_items(a, MAX_SOCIAL_EMOTE_OUTCOMES, p)?;
        each(a, p, outcome)
    })?;
    if SOCIAL_EMOTE_PROPS.iter().any(|k| m.contains_key(*k))
        && let Some(missing) = SOCIAL_EMOTE_PROPS.iter().find(|k| !m.contains_key(**k))
    {
        return Err(format!(
            "{p} must have property {missing} when a social emote property is present"
        ));
    }
    Ok(())
}

fn clip(v: &Value, p: &str) -> Check {
    let m = obj(v, p)?;
    required(m, "animation", p, non_empty)?;
    no_extra_keys(m, &["animation"], p)
}

fn start_animation(v: &Value, p: &str) -> Check {
    let m = obj(v, p)?;
    required(m, "loop", p, boolean)?;
    required(m, "Armature", p, clip)?;
    nullable(m, "Armature_Prop", p, clip)?;
    nullable(m, "audio", p, str_check)?;
    no_extra_keys(m, &["loop", "Armature", "Armature_Prop", "audio"], p)
}

fn outcome(v: &Value, p: &str) -> Check {
    let m = obj(v, p)?;
    required(m, "title", p, non_empty)?;
    required(m, "loop", p, boolean)?;
    required(m, "clips", p, |v, p| {
        let clips = obj(v, p)?;
        if clips.is_empty() {
            return Err(format!("{p} must contain at least one armature animation"));
        }
        for key in ["Armature", "Armature_Prop", "Armature_Other"] {
            nullable(clips, key, p, clip)?;
        }
        Ok(())
    })?;
    nullable(m, "audio", p, str_check)?;
    no_extra_keys(m, &["title", "loop", "clips", "audio"], p)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const NOW: Timestamp = 1_780_000_000_000;
    const ADDRESS: &str = "0xaaaa000000000000000000000000000000000001";
    const COLLECTION: &str = "0xbbbb000000000000000000000000000000000002";

    fn entity(entity_type: EntityType, metadata: Value) -> Entity {
        Entity {
            id: "bafkreitest".into(),
            entity_type,
            pointers: vec!["x".into()],
            timestamp: NOW,
            content: vec![],
            version: "v3".into(),
            metadata: Some(metadata),
        }
    }

    fn errors(entity_type: EntityType, metadata: Value) -> Vec<String> {
        match validate_metadata(&entity(entity_type, metadata)) {
            ValidationResponse::Ok => vec![],
            ValidationResponse::Failed { errors }
            | ValidationResponse::Unavailable { errors }
            | ValidationResponse::Unverifiable { errors } => errors,
        }
    }

    fn assert_valid(entity_type: EntityType, metadata: Value) {
        assert_eq!(errors(entity_type, metadata), Vec::<String>::new());
    }

    fn assert_invalid(entity_type: EntityType, metadata: Value, reason: &str) {
        assert_eq!(
            errors(entity_type, metadata),
            vec![
                format!("The metadata for this entity type ({entity_type}) is not valid."),
                reason.to_string()
            ]
        );
    }

    fn color() -> Value {
        json!({"color": {"r": 0.1, "g": 0.2, "b": 0.3}})
    }

    fn profile() -> Value {
        json!({"avatars": [{
            "name": "alice", "description": "", "ethAddress": ADDRESS, "version": 3,
            "tutorialStep": 256, "hasClaimedName": false, "userId": ADDRESS,
            "links": [{"title": "site", "url": "https://example.com/me"}],
            "avatar": {
                "bodyShape": "urn:decentraland:off-chain:base-avatars:BaseFemale",
                "eyes": color(), "hair": color(), "skin": color(),
                "wearables": ["urn:decentraland:off-chain:base-avatars:f_hat"],
                "emotes": [{"slot": 0, "urn": "wave"}]
            }
        }]})
    }

    fn scene() -> Value {
        json!({
            "main": "bin/index.js",
            "scene": {"base": "-1,0", "parcels": ["-1,0", "0,0"]},
            "display": {"title": "plaza", "navmapThumbnail": null},
            "spawnPoints": [{"name": "spawn", "position": {"x": [1, 2], "y": [0], "z": [3]}, "default": true}],
            "requiredPermissions": ["ALLOW_MEDIA_HOSTNAMES"],
            "allowedMediaHostnames": ["media.example.com"],
            "source": {"origin": "editor", "projectId": "p1", "point": {"x": 1, "y": 2}}
        })
    }

    fn i18n() -> Value {
        json!([{"code": "en", "text": "Hat"}])
    }

    fn representation() -> Value {
        json!({
            "bodyShapes": ["urn:decentraland:off-chain:base-avatars:BaseMale"],
            "mainFile": "male/hat.glb",
            "contents": ["male/hat.glb"]
        })
    }

    fn wearable() -> Value {
        let mut representation = representation();
        representation["overrideHides"] = json!([]);
        representation["overrideReplaces"] = json!(["hands"]);
        json!({
            "id": format!("urn:decentraland:matic:collections-v2:{COLLECTION}:0"),
            "name": "hat", "description": "", "i18n": i18n(),
            "thumbnail": "thumbnail.png", "image": "image.png",
            "collectionAddress": COLLECTION, "rarity": "epic",
            "data": {
                "replaces": [], "hides": ["hair"], "tags": ["hat"],
                "representations": [representation], "category": "hat"
            }
        })
    }

    fn emote() -> Value {
        json!({
            "id": format!("urn:decentraland:matic:collections-v2:{COLLECTION}:1"),
            "name": "high five", "description": "", "i18n": i18n(),
            "thumbnail": "thumbnail.png", "image": "image.png",
            "collectionAddress": COLLECTION, "rarity": "rare",
            "emoteDataADR74": {
                "category": "greetings", "tags": [], "loop": false,
                "representations": [representation()],
                "startAnimation": {"loop": true, "Armature": {"animation": "Start"}},
                "randomizeOutcomes": false,
                "outcomes": [{"title": "Five", "loop": false, "clips": {"Armature": {"animation": "Five"}}}]
            }
        })
    }

    fn third_party(mut item: Value) -> Value {
        let m = item.as_object_mut().unwrap();
        m.remove("collectionAddress");
        m.remove("rarity");
        m.insert("content".into(), json!({"male/hat.glb": "bafkreihash"}));
        m.insert(
            "merkleProof".into(),
            json!({"proof": [], "index": 0, "entityHash": "abc", "hashingKeys": ["id", "content"]}),
        );
        item
    }

    #[test]
    fn well_formed_metadata_of_every_type_passes() {
        assert_valid(EntityType::Profile, profile());
        assert_valid(EntityType::Scene, scene());
        assert_valid(EntityType::Wearable, wearable());
        assert_valid(EntityType::Wearable, third_party(wearable()));
        assert_valid(
            EntityType::Wearable,
            json!({"id": "urn:decentraland:off-chain:base-avatars:f_hat", "i18n": i18n()}),
        );
        assert_valid(EntityType::Emote, emote());
        assert_valid(EntityType::Emote, third_party(emote()));
        let mut scene_emote = emote();
        scene_emote["id"] = json!("urn:decentraland:off-chain:base-scene-emotes:throw");
        let m = scene_emote.as_object_mut().unwrap();
        m.remove("collectionAddress");
        m.remove("rarity");
        assert_valid(EntityType::Emote, scene_emote);
        assert_valid(
            EntityType::Store,
            json!({"id": "s", "version": 1, "owner": ADDRESS, "description": "", "links": [], "images": []}),
        );
        assert_valid(
            EntityType::Outfits,
            json!({"outfits": [{"slot": 0, "outfit": {
                "bodyShape": "urn:decentraland:off-chain:base-avatars:BaseMale",
                "eyes": color(), "hair": color(), "skin": color(), "wearables": []
            }}], "namesForExtraSlots": []}),
        );
    }

    /// The copied enums equal the ones extracted from the pinned package, so
    /// re-extracting a release that adds a value fails here.
    #[test]
    fn copied_enums_match_the_pinned_dcl_schemas() {
        use sha2::{Digest, Sha256};

        const ENUMS: &[u8] = include_bytes!("dcl-schemas/enums.json");
        let lock: Value =
            serde_json::from_str(include_str!("dcl-schemas/upstream.lock.json")).unwrap();
        assert_eq!(
            lock["files"]["enums.json"],
            hex::encode(Sha256::digest(ENUMS)),
            "upstream.lock.json records the vendored enums"
        );
        assert_eq!(
            lock["files"]["extract-enums.js"],
            hex::encode(Sha256::digest(include_bytes!(
                "dcl-schemas/extract-enums.js"
            ))),
            "upstream.lock.json records the extraction script"
        );
        let upstream: Map<String, Value> = serde_json::from_slice(ENUMS).unwrap();
        let copied: [(&str, &[&str]); 13] = [
            ("wearableCategories", WEARABLE_CATEGORIES),
            ("hideableBodyParts", HIDEABLE_BODY_PARTS),
            ("bodyShapes", BODY_SHAPES),
            ("rarities", RARITIES),
            ("locales", LOCALES),
            ("emoteCategories", EMOTE_CATEGORIES),
            ("scenePermissions", SCENE_PERMISSIONS),
            ("voiceChatToggles", VOICE_CHAT_TOGGLES),
            ("portableExperiencesToggles", PORTABLE_EXPERIENCES_TOGGLES),
            ("nearbyVoiceChatToggles", NEARBY_VOICE_CHAT_TOGGLES),
            ("sceneSourceRotations", SOURCE_ROTATIONS),
            ("mappingNetworks", CONTRACT_NETWORKS),
            ("mappingTypes", MAPPING_TYPES),
        ];
        assert_eq!(
            upstream.keys().collect::<HashSet<_>>(),
            copied
                .iter()
                .map(|(k, _)| k.to_string())
                .collect::<Vec<_>>()
                .iter()
                .collect(),
            "every extracted enum is compared"
        );
        for (name, values) in copied {
            assert_eq!(upstream[name], json!(values), "{name}");
        }
    }

    #[test]
    fn metadata_is_checked_from_adr_45_on() {
        let mut e = entity(EntityType::Scene, json!({}));
        e.timestamp = adr_timestamps::ADR_45 - 1;
        assert!(validate_metadata(&e).is_ok());
        e.timestamp = adr_timestamps::ADR_45;
        assert_eq!(
            validate_metadata(&e).errors().unwrap()[1],
            "metadata must have required property 'main'"
        );
    }

    #[test]
    fn missing_metadata_is_not_an_object() {
        let mut e = entity(EntityType::Store, json!({}));
        e.metadata = None;
        assert_eq!(
            validate_metadata(&e).errors().unwrap()[1],
            "metadata must be object"
        );
    }

    #[test]
    fn profile_fields_are_required_and_typed() {
        let mut p = profile();
        p["avatars"][0]
            .as_object_mut()
            .unwrap()
            .remove("ethAddress");
        assert_invalid(
            EntityType::Profile,
            p,
            "metadata.avatars[0] must have required property 'ethAddress'",
        );
        let mut p = profile();
        p["avatars"][0]["avatar"]["eyes"]["color"]["r"] = json!(2);
        assert_invalid(
            EntityType::Profile,
            p,
            "metadata.avatars[0].avatar.eyes.color.r must be <= 1",
        );
        let mut p = profile();
        p["avatars"][0]["links"][0]["url"] = json!("javascript:alert(1)");
        assert_invalid(
            EntityType::Profile,
            p,
            "metadata.avatars[0].links[0].url must match the expected pattern",
        );
        for (url, valid) in [
            ("https://a b", true),
            ("https://\u{85}b", true),
            ("https://ab\u{feff}c", false),
            ("https://a\nb", false),
            ("https://a", false),
        ] {
            assert_eq!(is_link_url(url), valid, "{url:?}");
        }
        let mut p = profile();
        p["avatars"][0]["userId"] = Value::Null;
        assert_invalid(
            EntityType::Profile,
            p,
            "metadata.avatars[0].userId must be string",
        );
    }

    #[test]
    fn scene_fields_are_required_and_typed() {
        let mut s = scene();
        s.as_object_mut().unwrap().remove("main");
        assert_invalid(
            EntityType::Scene,
            s,
            "metadata must have required property 'main'",
        );
        let mut s = scene();
        s["scene"]["base"] = json!("5,5");
        assert_invalid(
            EntityType::Scene,
            s,
            "metadata.scene.parcels must contain at least 1 valid item(s)",
        );
        let mut s = scene();
        s["scene"]["parcels"] = json!(["-1,0", "01,0"]);
        assert_invalid(
            EntityType::Scene,
            s,
            "metadata.scene.parcels[1] must match the expected pattern",
        );
        let mut s = scene();
        s["spawnPoints"][0]["position"] = json!({"x": 1, "y": [0], "z": 3});
        assert_invalid(
            EntityType::Scene,
            s,
            "metadata.spawnPoints[0].position must match exactly one schema in oneOf",
        );
        let mut s = scene();
        s["allowedMediaHostnames"] = json!([]);
        assert_invalid(
            EntityType::Scene,
            s,
            "metadata.allowedMediaHostnames must NOT have fewer than 1 items",
        );
        let mut s = scene();
        s["requiredPermissions"] = json!([]);
        assert_invalid(
            EntityType::Scene,
            s,
            "metadata.allowedMediaHostnames must NOT have more than 0 items",
        );
        for textures in [Value::Null, json!(["a", "b"]), json!([1])] {
            let mut s = scene();
            s["worldConfiguration"] = json!({"skyboxConfig": {"textures": textures}});
            assert_invalid(
                EntityType::Scene,
                s,
                "customSkybox must be an array of 0, 1 or 6 strings",
            );
        }
        let mut s = scene();
        s["creator"] = json!("bob");
        assert_invalid(
            EntityType::Scene,
            s,
            "Creator must be a valid wallet address",
        );
    }

    #[test]
    fn a_wearable_is_exactly_one_of_base_standard_or_third_party() {
        let mut w = wearable();
        w.as_object_mut().unwrap().remove("rarity");
        assert_invalid(
            EntityType::Wearable,
            w,
            "either standard XOR thirdparty properties conditions must be met",
        );
        let mut w = third_party(wearable());
        w["merkleProof"]["hashingKeys"] = json!(["id", "missing"]);
        assert_invalid(
            EntityType::Wearable,
            w,
            "either standard XOR thirdparty properties conditions must be met",
        );
        let mut w = wearable();
        w["data"]["representations"] = json!([]);
        assert_invalid(
            EntityType::Wearable,
            w,
            "metadata.data.representations must NOT have fewer than 1 items",
        );
        let mut w = wearable();
        w["data"]["category"] = json!("cape");
        assert_invalid(
            EntityType::Wearable,
            w,
            "metadata.data.category must be equal to one of the allowed values",
        );
        let mut w = wearable();
        w["i18n"] = json!([{"code": "en", "text": "a"}, {"code": "en", "text": "b"}]);
        assert_invalid(
            EntityType::Wearable,
            w,
            "metadata.i18n array should not have duplicates for \"code\"",
        );
    }

    #[test]
    fn an_emote_needs_its_adr_74_data() {
        let mut e = emote();
        e.as_object_mut().unwrap().remove("emoteDataADR74");
        assert_invalid(
            EntityType::Emote,
            e,
            "emote should have \"emoteDataADR74\" and match its schema",
        );
        let mut e = emote();
        e["emoteDataADR74"]["representations"][0]["contents"] = json!(["other.glb"]);
        assert_invalid(
            EntityType::Emote,
            e,
            "metadata.emoteDataADR74.representations[0].contents should contain mainFile: male/hat.glb",
        );
        let mut e = emote();
        e["emoteDataADR74"]
            .as_object_mut()
            .unwrap()
            .remove("randomizeOutcomes");
        assert_invalid(
            EntityType::Emote,
            e,
            "metadata.emoteDataADR74 must have property randomizeOutcomes when a social emote property is present",
        );
        let mut e = emote();
        let four = vec![e["emoteDataADR74"]["outcomes"][0].clone(); 4];
        e["emoteDataADR74"]["outcomes"] = Value::Array(four);
        assert_invalid(
            EntityType::Emote,
            e,
            "metadata.emoteDataADR74.outcomes must NOT have more than 3 items",
        );
        let mut e = emote();
        e["emoteDataADR74"]["outcomes"][0]["extra"] = json!(1);
        assert_invalid(
            EntityType::Emote,
            e,
            "metadata.emoteDataADR74.outcomes[0] must NOT have additional property 'extra'",
        );
        let mut e = emote();
        e.as_object_mut().unwrap().remove("image");
        assert_invalid(
            EntityType::Emote,
            e,
            "metadata must have required property 'image'",
        );
    }

    /// Each case agrees with `Emote.validate` of `@dcl/schemas` 27.2.0; `@`
    /// is a contract address.
    #[test]
    fn linked_wearable_mappings_follow_the_reference() {
        const CONTRACT: &str = "0xcccc000000000000000000000000000000000003";
        let cases = [
            (r#"{"bad": 1}"#, false),
            (r#"{}"#, false),
            (r#"{"mainnet": {}}"#, false),
            (r#"{"mainnet": {"0xnotanaddress": []}}"#, false),
            (
                r#"{"mainnet": {"@": [{"type": "single", "id": "1"}, {"type": "range", "from": "2", "to": "10"}, {"type": "multiple", "ids": ["11", "12"]}]}}"#,
                true,
            ),
            (
                r#"{"mainnet": {"@": [{"type": "single", "id": "5"}, {"type": "range", "from": "2", "to": "10"}]}}"#,
                false,
            ),
            (
                r#"{"mainnet": {"@": [{"type": "any"}, {"type": "single", "id": "1"}]}}"#,
                false,
            ),
            (r#"{"amoy": {"@": [{"type": "any"}]}}"#, true),
            (
                r#"{"mainnet": {"@": [{"type": "range", "from": "10", "to": "9"}]}}"#,
                false,
            ),
            (
                r#"{"mainnet": {"@": [{"type": "range", "from": "1", "to": "5"}, {"type": "range", "from": "5", "to": "9"}]}}"#,
                false,
            ),
            (
                r#"{"mainnet": {"@": [{"type": "range", "from": "1", "to": "4"}, {"type": "range", "from": "5", "to": "9"}]}}"#,
                true,
            ),
            (
                r#"{"mainnet": {"@": [{"type": "range", "from": "1", "to": "100"}, {"type": "range", "from": "5", "to": "9"}]}}"#,
                false,
            ),
            (
                r#"{"mainnet": {"@": [{"type": "single", "id": "01"}, {"type": "single", "id": "1"}]}}"#,
                true,
            ),
            (
                r#"{"mainnet": {"@": [{"type": "single", "id": "01"}, {"type": "range", "from": "1", "to": "1"}]}}"#,
                false,
            ),
            (
                r#"{"mainnet": {"@": [{"type": "multiple", "ids": ["7", "8"]}, {"type": "single", "id": "8"}]}}"#,
                false,
            ),
            (
                r#"{"mainnet": {"0xCCCC000000000000000000000000000000000003": [{"type": "single", "id": "1"}], "@": [{"type": "single", "id": "1"}]}}"#,
                false,
            ),
            (
                r#"{"mainnet": {"@": [{"type": "single", "id": "1"}]}, "matic": {"@": [{"type": "single", "id": "1"}]}}"#,
                true,
            ),
            (
                r#"{"mainnet": {"@": [{"type": "multiple", "ids": ["1", "1"]}]}}"#,
                false,
            ),
            (
                r#"{"mainnet": {"@": [{"type": "single", "id": "1", "extra": 1}]}}"#,
                false,
            ),
            (r#"{"mainnet": {"@": [{"type": "bogus"}]}}"#, false),
            (
                r#"{"mainnet": {"@": [{"type": "single", "id": "x1"}]}}"#,
                false,
            ),
            (
                r#"{"mainnet": {"@": [{"type": "range", "from": "99999999999999999999", "to": "100000000000000000000"}]}}"#,
                true,
            ),
            (r#"null"#, false),
        ];
        for (mappings, valid) in cases {
            let mappings: Value = serde_json::from_str(&mappings.replace('@', CONTRACT)).unwrap();
            let mut e = emote();
            e["mappings"] = mappings.clone();
            assert_eq!(errors(EntityType::Emote, e).is_empty(), valid, "{mappings}");
        }
        let mut e = emote();
        e["mappings"] = json!({"bad": 1});
        assert_invalid(
            EntityType::Emote,
            e,
            "metadata.mappings must NOT have additional property 'bad'",
        );
        let linked = json!({"amoy": {CONTRACT: [{"type": "any"}]}});
        let mut w = third_party(wearable());
        w["mappings"] = linked.clone();
        assert_valid(EntityType::Wearable, w.clone());
        w["mappings"] =
            json!({"amoy": {CONTRACT: [{"type": "any"}, {"type": "single", "id": "1"}]}});
        assert_invalid(
            EntityType::Wearable,
            w,
            "either standard XOR thirdparty properties conditions must be met",
        );
        let mut w = wearable();
        w["mappings"] = linked;
        assert_invalid(
            EntityType::Wearable,
            w,
            "either standard XOR thirdparty properties conditions must be met",
        );
    }

    #[test]
    fn store_and_outfits_fields_are_required() {
        assert_invalid(
            EntityType::Store,
            json!({"id": "s", "version": 1, "owner": ADDRESS, "description": "", "links": []}),
            "metadata must have required property 'images'",
        );
        assert_invalid(
            EntityType::Outfits,
            json!({"outfits": []}),
            "metadata must have required property 'namesForExtraSlots'",
        );
    }

    #[test]
    fn duplicate_items_are_found_in_linear_time() {
        let names: Vec<String> = (0..80_000).map(|i| format!("n{i}")).collect();
        let outfits = |names: &[String]| json!({"outfits": [], "namesForExtraSlots": names});
        let started = std::time::Instant::now();
        assert_valid(EntityType::Outfits, outfits(&names));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(3),
            "took {:?}",
            started.elapsed()
        );
        let mut repeated = names[..3].to_vec();
        repeated.push("n0".into());
        assert_invalid(
            EntityType::Outfits,
            outfits(&repeated),
            "metadata.namesForExtraSlots must NOT have duplicate items (items ## 3 and 0 are identical)",
        );
    }

    fn start_animation_err(v: &Value) -> Option<String> {
        start_animation(v, "startAnimation").err()
    }

    fn outcome_err(v: &Value) -> Option<String> {
        outcome(v, "outcome").err()
    }

    #[test]
    fn social_emote_parts_match_adr_287() {
        assert!(
            start_animation_err(&json!({"loop": true, "Armature": {"animation": "a"}})).is_none()
        );
        assert!(start_animation_err(&json!({"loop": true})).is_some());
        assert!(
            start_animation_err(&json!({"loop": true, "Armature": {"animation": ""}})).is_some()
        );
        assert!(outcome_err(&json!({"title": "t", "loop": false, "clips": {}})).is_some());
        assert!(
            outcome_err(&json!({"title": "t", "loop": false, "clips": {"Armature_Other": {"animation": "a"}}}))
                .is_none()
        );
    }
}
