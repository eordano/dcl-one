//! The shape checks of a world publish: the entity and scene schemas,
//! canonical parcel sets, bounds, the entity file cap, the auth chain fields
//! of the multipart form, the navmap thumbnail, minimap and skybox rules.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::auth::{canonicalize_pointer, is_canonical_pointer, parse_pointer};
use crate::content::create_entity::auth_chain_field_max_index;
use crate::content::entity::{is_ipfs_v1, is_ipfs_v2};
use crate::content::validator::navmap_thumbnail_error;

pub(super) const MAX_ENTITY_FILE_SIZE_BYTES: usize = 5 * 1024 * 1024;

/// File parts one deploy may upload, the entity file included (busboy's `files`).
pub const MAX_DEPLOY_FILES: usize = 10_000;
/// No per-file cap below the request's payload cap, as the old service.
pub const MAX_DEPLOY_FILE_BYTES: usize = super::deploy::MAX_UPLOAD_SIZE_BYTES;

/// The world parcel bounds, for scene pointers and the settings spawn coordinates.
pub(super) const MIN_PARCEL_COORDINATE: i64 = -150;
pub(super) const MAX_PARCEL_COORDINATE: i64 = 150;
/// Every in-bounds parcel once; a longer pointer list can never be valid.
pub(super) const MAX_SCENE_POINTERS: usize = ((MAX_PARCEL_COORDINATE - MIN_PARCEL_COORDINATE + 1)
    * (MAX_PARCEL_COORDINATE - MIN_PARCEL_COORDINATE + 1))
    as usize;

pub(crate) fn canon_pointer(s: &str) -> String {
    canonicalize_pointer(s)
}

pub(super) fn canon_pointer_set(values: &[Value]) -> Vec<String> {
    let mut out: Vec<String> = values
        .iter()
        .filter_map(|v| v.as_str().map(canon_pointer))
        .collect();
    out.sort();
    out.dedup();
    out
}

/// True iff `values` is a non-empty array of unique, already-canonical parcel coordinate
/// strings -- the shape `@dcl/schemas` `SceneParcels` enforces. Non-canonical spellings and
/// duplicates are rejected rather than normalized, so a deployment is authorized and sized
/// against exactly the literal set of parcels it is placed on.
pub(super) fn is_canonical_parcel_set(values: &[Value]) -> bool {
    if values.is_empty() {
        return false;
    }
    let mut seen = std::collections::HashSet::with_capacity(values.len());
    for v in values {
        match v.as_str() {
            Some(s) if is_canonical_pointer(s) => {
                if !seen.insert(s) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

pub(super) fn validate_parcel_in_bounds(parcel: &str) -> Result<(), String> {
    let (x, y) = match parse_pointer(parcel) {
        Some(xy) => xy,
        None => return Err(format!("Invalid coordinate format: {parcel}")),
    };
    if !(MIN_PARCEL_COORDINATE..=MAX_PARCEL_COORDINATE).contains(&x) {
        return Err(format!(
            "Coordinate X value {x} is out of bounds. Must be between {MIN_PARCEL_COORDINATE} and {MAX_PARCEL_COORDINATE}."
        ));
    }
    if !(MIN_PARCEL_COORDINATE..=MAX_PARCEL_COORDINATE).contains(&y) {
        return Err(format!(
            "Coordinate Y value {y} is out of bounds. Must be between {MIN_PARCEL_COORDINATE} and {MAX_PARCEL_COORDINATE}."
        ));
    }
    Ok(())
}

/// Upstream's `requireString` refusal of an absent field.
pub(super) const STRING_EXPECTED: &str = "A string was expected";

/// Upstream's `extractAuthChain`: the `authChain[i][type|payload|signature]` fields up to the
/// largest index any field name carries, each one required. The field cap bounds the walk.
pub(super) fn extract_auth_chain_from_fields(
    fields: &BTreeMap<String, String>,
) -> Result<Value, String> {
    let biggest_index = auth_chain_field_max_index(fields.keys())
        .ok_or_else(|| "Missing auth chain".to_string())?;

    let mut chain = Vec::new();
    for i in 0..=biggest_index {
        let field = |part: &str| {
            fields
                .get(&format!("authChain[{i}][{part}]"))
                .ok_or_else(|| STRING_EXPECTED.to_string())
        };
        let (payload, signature, link_type) =
            (field("payload")?, field("signature")?, field("type")?);
        chain.push(json!({ "type": link_type, "payload": payload, "signature": signature }));
    }
    Ok(Value::Array(chain))
}

pub(super) fn entity_file_too_large_error() -> String {
    format!(
        "The entity file is too large. The maximum allowed size is {MAX_ENTITY_FILE_SIZE_BYTES} bytes."
    )
}

/// `@dcl/schemas` `IPFSv1` / `IPFSv2`.
fn is_ipfs_hash(s: &str) -> bool {
    is_ipfs_v1(s) || is_ipfs_v2(s)
}

/// `uniqueItems` over string items, scanned from the end as ajv does.
fn unique_items(items: &[Value]) -> Result<(), String> {
    let mut seen: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::with_capacity(items.len());
    for (i, item) in items.iter().enumerate().rev() {
        let Some(s) = item.as_str() else { continue };
        if let Some(j) = seen.insert(s, i) {
            return Err(format!(
                "must NOT have duplicate items (items ## {j} and {i} are identical)"
            ));
        }
    }
    Ok(())
}

/// The first failure of `@dcl/schemas` `Entity.validate`, in ajv's words
/// (the old service listed every one; the id is the authenticated one).
pub(super) fn entity_schema_error(entity: &Value) -> Option<String> {
    let check = || -> Result<(), String> {
        let m = entity.as_object().ok_or("must be object")?;
        for key in ["version", "type", "pointers", "timestamp", "content"] {
            if !m.contains_key(key) {
                return Err(format!("must have required property '{key}'"));
            }
        }
        if m["version"].as_str().ok_or("must be string")? != "v3" {
            return Err("must be equal to one of the allowed values".to_string());
        }
        m["type"].as_str().ok_or("must be string")?;
        let pointers = m["pointers"].as_array().ok_or("must be array")?;
        if pointers.len() > MAX_SCENE_POINTERS {
            return Err(format!(
                "must NOT have more than {MAX_SCENE_POINTERS} items"
            ));
        }
        for p in pointers {
            if p.as_str().ok_or("must be string")?.is_empty() {
                return Err("must NOT have fewer than 1 characters".to_string());
            }
        }
        unique_items(pointers)?;
        if m["timestamp"].as_f64().ok_or("must be number")? < 0.0 {
            return Err("must be >= 0".to_string());
        }
        for item in m["content"].as_array().ok_or("must be array")? {
            let c = item.as_object().ok_or("must be object")?;
            for key in ["file", "hash"] {
                if !c.contains_key(key) {
                    return Err(format!("must have required property '{key}'"));
                }
            }
            if c["file"].as_str().ok_or("must be string")?.is_empty() {
                return Err("must NOT have fewer than 1 characters".to_string());
            }
            if !is_ipfs_hash(c["hash"].as_str().ok_or("must be string")?) {
                return Err("must match exactly one schema in oneOf".to_string());
            }
        }
        match m.get("metadata") {
            None | Some(Value::Null | Value::Object(_)) => Ok(()),
            Some(_) => Err("must be object".to_string()),
        }
    };
    check().err()
}

fn content_has_file(entity: &Value, file: &str) -> bool {
    matches!(
        entity.get("content"),
        Some(Value::Array(items)) if items.iter().any(|item| item.get("file").and_then(|f| f.as_str()) == Some(file))
    )
}

fn world_configuration(entity: &Value) -> Option<&Value> {
    entity.pointer("/metadata/worldConfiguration")
}

/// `validateMiniMapImages`: each configured minimap image is a content file.
pub(super) fn missing_minimap_images(entity: &Value) -> Vec<String> {
    let config = world_configuration(entity).and_then(|w| w.get("miniMapConfig"));
    ["dataImage", "estateImage"]
        .iter()
        .filter_map(|key| config?.get(*key)?.as_str())
        .filter(|file| !file.is_empty() && !content_has_file(entity, file))
        .map(|file| format!("The file {file} is not present in the entity."))
        .collect()
}

/// `validateSkyboxTextures`: each skybox texture is a content file.
pub(super) fn missing_skybox_textures(entity: &Value) -> Vec<String> {
    world_configuration(entity)
        .and_then(|w| w.pointer("/skyboxConfig/textures"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|file| !file.is_empty() && !content_has_file(entity, file))
        .map(|file| format!("The texture file {file} is not present in the entity."))
        .collect()
}

pub(super) fn validate_navmap_thumbnail(entity: &Value, errors: &mut Vec<String>) {
    let Some(metadata) = entity.get("metadata") else {
        return;
    };
    if let Some(message) = navmap_thumbnail_error(metadata, |file| content_has_file(entity, file)) {
        errors.push(message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::validator::is_relative_thumbnail_path;

    #[test]
    fn canon_pointer_set_normalizes_and_sorts() {
        let a = canon_pointer_set(&[json!("1,2"), json!("0,0"), json!(" 0,0 ")]);
        assert_eq!(a, vec!["0,0".to_string(), "1,2".to_string()]);
    }

    #[test]
    fn canon_pointer_numerically_normalizes() {
        assert_eq!(canon_pointer("00,00"), "0,0");
        assert_eq!(canon_pointer("-0,-0"), "0,0");
        assert_eq!(canon_pointer(" 01 , 002 "), "1,2");
        assert_eq!(canon_pointer("-05,10"), "-5,10");
        assert_eq!(canon_pointer("00,00"), canon_pointer("0,0"));
        assert_eq!(canon_pointer("not-a-parcel"), "not-a-parcel");
        assert_eq!(canon_pointer("1,2,3"), "1,2,3");
        assert_eq!(canon_pointer("1e2,3"), "1e2,3");
    }

    #[test]
    fn canon_pointer_set_treats_leading_zeros_as_equal() {
        let pointers = canon_pointer_set(&[json!("00,00"), json!("01,00")]);
        let parcels = canon_pointer_set(&[json!("0,0"), json!("1,0")]);
        assert_eq!(pointers, parcels);
    }

    #[test]
    fn parcel_bounds_validation_matches_upstream() {
        assert!(validate_parcel_in_bounds("0,0").is_ok());
        assert!(validate_parcel_in_bounds("-150,150").is_ok());
        assert!(validate_parcel_in_bounds("150,-150").is_ok());
        assert!(
            validate_parcel_in_bounds("151,0")
                .unwrap_err()
                .contains("Coordinate X value 151 is out of bounds")
        );
        assert!(
            validate_parcel_in_bounds("0,-151")
                .unwrap_err()
                .contains("Coordinate Y value -151 is out of bounds")
        );
        assert!(
            validate_parcel_in_bounds("garbage")
                .unwrap_err()
                .contains("Invalid coordinate format")
        );
    }

    #[test]
    fn is_canonical_parcel_set_rejects_noncanonical_and_duplicates() {
        assert!(is_canonical_parcel_set(&[json!("0,0"), json!("0,1")]));
        assert!(is_canonical_parcel_set(&[json!("-5,10")]));
        assert!(!is_canonical_parcel_set(&[]));
        assert!(!is_canonical_parcel_set(&[json!("0,0"), json!("0,0")]));
        assert!(!is_canonical_parcel_set(&[json!("00,0")]));
        assert!(!is_canonical_parcel_set(&[json!(" 0,1 ")]));
        assert!(!is_canonical_parcel_set(&[json!("-0,0")]));
        assert!(!is_canonical_parcel_set(&[json!(0)]));
    }

    #[test]
    fn pointers_equal_scene_parcels_after_canonicalization() {
        let pointers = canon_pointer_set(&[json!("0,0"), json!(" 1,1 "), json!("0,0")]);
        let parcels = canon_pointer_set(&[json!("1,1"), json!("0,0")]);
        assert_eq!(pointers, parcels);
        let mismatch = canon_pointer_set(&[json!("0,0"), json!("2,2")]);
        assert_ne!(pointers, mismatch);
    }

    #[test]
    fn extract_auth_chain_reads_the_indexed_fields() {
        let mut f = BTreeMap::new();
        f.insert("authChain[0][type]".into(), "SIGNER".into());
        f.insert("authChain[0][payload]".into(), "0xabc".into());
        f.insert("authChain[0][signature]".into(), "".into());
        let v = extract_auth_chain_from_fields(&f).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 1);
        assert_eq!(v[0]["type"], "SIGNER");
    }

    /// Upstream `extract-auth-chain.ts`; tests `source-upload-limits.spec.ts` ("Missing auth chain").
    #[test]
    fn extract_auth_chain_refuses_in_upstreams_words() {
        let mut json_only = BTreeMap::new();
        json_only.insert(
            "authChain".to_string(),
            r#"[{"type":"SIGNER","payload":"0x1"}]"#.to_string(),
        );
        assert_eq!(
            extract_auth_chain_from_fields(&json_only),
            Err("Missing auth chain".to_string())
        );
        assert_eq!(
            extract_auth_chain_from_fields(&BTreeMap::new()),
            Err("Missing auth chain".to_string())
        );
        let mut gap = BTreeMap::new();
        gap.insert("authChain[0][type]".to_string(), "SIGNER".to_string());
        gap.insert("authChain[0][payload]".to_string(), "0xp".to_string());
        assert_eq!(
            extract_auth_chain_from_fields(&gap),
            Err(STRING_EXPECTED.to_string())
        );
        let mut far = BTreeMap::new();
        far.insert(
            format!("authChain[{}][type]", u64::MAX),
            "SIGNER".to_string(),
        );
        assert_eq!(
            extract_auth_chain_from_fields(&far),
            Err(STRING_EXPECTED.to_string())
        );
        // The regex finds an index after an earlier `authChain[` that has none.
        let mut later = BTreeMap::new();
        later.insert(
            "authChain[x]authChain[0][type]".to_string(),
            "SIGNER".to_string(),
        );
        assert_eq!(
            extract_auth_chain_from_fields(&later),
            Err(STRING_EXPECTED.to_string())
        );
    }

    #[test]
    fn thumbnail_relative_paths_are_accepted() {
        assert!(is_relative_thumbnail_path("thumb.png"));
        assert!(is_relative_thumbnail_path("images/thumbnail.png"));
        assert!(is_relative_thumbnail_path("dir/na:me.png"));
    }

    #[test]
    fn thumbnail_non_relative_paths_are_rejected() {
        for value in [
            "https://example.com/image.png",
            "https://example.com/x\"><script>alert(1)</script><meta name=\"y",
            "//evil.example/x.png",
            "/thumb.png",
            "data:text/html,<b>x</b>",
            "javascript:alert(1)",
            "HtTpS://evil.example/x.png",
            " thumb.png",
            "thumb.png ",
            "thumb\nname.png",
            "thumb\".png",
            "thumb<img>.png",
            "\u{FEFF}thumb.png",
            "thumb.png\u{FEFF}",
        ] {
            assert!(
                !is_relative_thumbnail_path(value),
                "expected rejection: {value:?}"
            );
        }
    }

    #[test]
    fn thumbnail_validation_pushes_upstream_error_strings() {
        let mut errors = Vec::new();
        let entity = json!({
            "content": [{ "file": "https://example.com/image.png", "hash": "bafyx" }],
            "metadata": { "display": { "navmapThumbnail": "https://example.com/image.png" } }
        });
        validate_navmap_thumbnail(&entity, &mut errors);
        assert_eq!(
            errors,
            vec!["Scene thumbnail 'https://example.com/image.png' must be a relative path to a file included in the deployment.".to_string()]
        );

        let mut errors = Vec::new();
        let entity = json!({
            "content": [{ "file": "other.png", "hash": "bafyx" }],
            "metadata": { "display": { "navmapThumbnail": "thumb.png" } }
        });
        validate_navmap_thumbnail(&entity, &mut errors);
        assert_eq!(
            errors,
            vec![
                "Scene thumbnail 'thumb.png' must be a file included in the deployment."
                    .to_string()
            ]
        );

        let mut errors = Vec::new();
        let entity = json!({
            "content": [{ "file": "images/thumbnail.png", "hash": "bafyx" }],
            "metadata": { "display": { "navmapThumbnail": "images/thumbnail.png" } }
        });
        validate_navmap_thumbnail(&entity, &mut errors);
        assert!(errors.is_empty());

        let mut errors = Vec::new();
        validate_navmap_thumbnail(&json!({ "metadata": {} }), &mut errors);
        validate_navmap_thumbnail(
            &json!({ "metadata": { "display": { "navmapThumbnail": null } } }),
            &mut errors,
        );
        validate_navmap_thumbnail(
            &json!({ "metadata": { "display": { "navmapThumbnail": "" } } }),
            &mut errors,
        );
        assert!(errors.is_empty());
    }

    #[test]
    fn thumbnail_bom_prefixed_path_is_rejected_even_when_content_matches() {
        let mut errors = Vec::new();
        let entity = json!({
            "content": [{ "file": "\u{FEFF}thumb.png", "hash": "bafyx" }],
            "metadata": { "display": { "navmapThumbnail": "\u{FEFF}thumb.png" } }
        });
        validate_navmap_thumbnail(&entity, &mut errors);
        assert_eq!(
            errors,
            vec!["Scene thumbnail '\u{FEFF}thumb.png' must be a relative path to a file included in the deployment.".to_string()]
        );
    }

    #[test]
    fn thumbnail_non_string_values_fail_validation() {
        let mut errors = Vec::new();
        let entity = json!({
            "content": [{ "file": "thumb.png", "hash": "bafyx" }],
            "metadata": { "display": { "navmapThumbnail": ["https://x\"><script>alert(1)</script><meta name=\"y"] } }
        });
        validate_navmap_thumbnail(&entity, &mut errors);
        assert_eq!(
            errors,
            vec![
                "Scene thumbnail '[\"https://x\\\"><script>alert(1)</script><meta name=\\\"y\"]' must be a relative path to a file included in the deployment."
                    .to_string()
            ]
        );

        for (value, rendered) in [
            (json!(5), "5"),
            (json!({"a": 1}), "{\"a\":1}"),
            (json!(true), "true"),
        ] {
            let mut errors = Vec::new();
            validate_navmap_thumbnail(
                &json!({ "metadata": { "display": { "navmapThumbnail": value } } }),
                &mut errors,
            );
            assert_eq!(
                errors,
                vec![format!(
                    "Scene thumbnail '{rendered}' must be a relative path to a file included in the deployment."
                )]
            );
        }

        let mut errors = Vec::new();
        for falsy in [json!(false), json!(0), json!(0.0)] {
            validate_navmap_thumbnail(
                &json!({ "metadata": { "display": { "navmapThumbnail": falsy } } }),
                &mut errors,
            );
        }
        assert!(errors.is_empty(), "falsy values must skip: {errors:?}");
    }

    #[test]
    fn entity_file_cap_matches_upstream_constant_and_message() {
        assert_eq!(MAX_ENTITY_FILE_SIZE_BYTES, 5 * 1024 * 1024);
        assert_eq!(
            entity_file_too_large_error(),
            "The entity file is too large. The maximum allowed size is 5242880 bytes."
        );
    }
}
