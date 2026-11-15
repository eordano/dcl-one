//! The entity file as a deploy parses it: the six deployable types, the
//! `content` mapping and the lenient shape (`id`/`version` optional) that the
//! write path accepts and fills in from the upload; a missing `version` is `v3`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntityType {
    Scene,
    Profile,
    Wearable,
    Store,
    Emote,
    Outfits,
}

impl EntityType {
    pub fn as_str(&self) -> &'static str {
        match self {
            EntityType::Scene => "scene",
            EntityType::Profile => "profile",
            EntityType::Wearable => "wearable",
            EntityType::Store => "store",
            EntityType::Emote => "emote",
            EntityType::Outfits => "outfits",
        }
    }
}

impl std::fmt::Display for EntityType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentMapping {
    pub file: String,
    pub hash: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entity {
    #[serde(default)]
    pub id: String,

    #[serde(rename = "type")]
    pub entity_type: EntityType,

    pub pointers: Vec<String>,

    pub timestamp: i64,

    // The Explorer deploys a profile without snapshots as an entity with no `content`.
    #[serde(default)]
    pub content: Vec<ContentMapping>,

    #[serde(default = "default_version", deserialize_with = "version_or_default")]
    pub version: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// `@dcl/schemas` `IPFSv2.validate`, `^(ba)[a-zA-Z0-9]{57}$`: a base32
/// CIDv1 of any codec and hash, as the entity and content hashes since ADR-45.
pub fn is_ipfs_v2(hash: &str) -> bool {
    hash.len() == 59 && hash.starts_with("ba") && hash.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// `@dcl/schemas` `IPFSv1.validate`, `^(Qm)[a-zA-Z0-9]{44}$`: a base58 CIDv0.
pub fn is_ipfs_v1(hash: &str) -> bool {
    hash.len() == 46 && hash.starts_with("Qm") && hash.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// The version of an entity file that names none.
pub const DEFAULT_ENTITY_VERSION: &str = "v3";

fn default_version() -> String {
    DEFAULT_ENTITY_VERSION.to_string()
}

/// `null` reads as absent, like the reference's `version ?? 'v3'`.
fn version_or_default<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.unwrap_or_else(default_version))
}

/// Local structs also accept declaration-order sequences; sync reads object properties only.
pub(crate) enum MetadataInput {
    LocalEntity,
    SyncObject,
}

impl<'de> serde::de::Visitor<'de> for MetadataInput {
    type Value = crate::OrderedJson;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("an entity metadata projection")
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut metadata = crate::OrderedJson::Null;
        while let Some(key) = map.next_key::<String>()? {
            if key == "metadata" {
                metadata = map.next_value()?;
            } else {
                map.next_value::<serde::de::IgnoredAny>()?;
            }
        }
        Ok(metadata)
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut metadata = crate::OrderedJson::Null;
        let mut index = 0;
        loop {
            if matches!(self, Self::LocalEntity) && index == 6 {
                let Some(value) = seq.next_element()? else {
                    break;
                };
                metadata = value;
            } else if seq.next_element::<serde::de::IgnoredAny>()?.is_none() {
                break;
            }
            index += 1;
        }
        Ok(metadata)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(crate::OrderedJson::Null)
    }
    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E> {
        Ok(crate::OrderedJson::Null)
    }
    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E> {
        Ok(crate::OrderedJson::Null)
    }
    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E> {
        Ok(crate::OrderedJson::Null)
    }
    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E> {
        Ok(crate::OrderedJson::Null)
    }
    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E> {
        Ok(crate::OrderedJson::Null)
    }
}

/// Projects metadata without imposing stricter parsing on fields admission ignores.
pub(crate) fn deployment_metadata(
    bytes: &[u8],
    input: MetadataInput,
) -> Result<crate::OrderedJson, serde_json::Error> {
    use crate::OrderedJson;
    use serde::Deserializer;
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let metadata = deserializer.deserialize_any(input)?;
    deserializer.end()?;
    Ok(if metadata.is_null() {
        metadata
    } else {
        OrderedJson::Object(vec![("v".into(), metadata)])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipfs_hashes_follow_the_schema_patterns() {
        let v2 = |tail: &str| format!("ba{tail}");
        for (hash, want) in [
            (v2(&"a".repeat(57)), true),
            (
                "bafkreihdwdcefgh4dqkjv67uzcmw7ojee6xedzdetojuzjevtenosa7776".to_string(),
                true,
            ),
            // Another CID prefix, still 59 base32 characters.
            (format!("bag{}", "a".repeat(56)), true),
            ("bafkrei".to_string(), false),
            (format!("bafy{}", "a".repeat(56)), false),
            (v2(&"a".repeat(56)), false),
            (v2(&"a".repeat(58)), false),
            (format!("ba{}-", "a".repeat(56)), false),
            (format!("ba{}\u{e9}", "a".repeat(55)), false),
            (format!("Ba{}", "a".repeat(57)), false),
            (
                "QmYwAPJzv5CZsnA625s3Xf2nemtYgPpHdWEz79ojWnPbdG".to_string(),
                false,
            ),
        ] {
            assert_eq!(is_ipfs_v2(&hash), want, "{hash:?}");
        }
        assert!(is_ipfs_v1("QmYwAPJzv5CZsnA625s3Xf2nemtYgPpHdWEz79ojWnPbdG"));
        assert!(!is_ipfs_v1("QmYwAPJzv5CZsnA625s3Xf2nemtYgPpHdWEz79ojWnPbd"));
        assert!(!is_ipfs_v1(
            "QmYwAPJzv5CZsnA625s3Xf2nemtYgPpHdWEz79ojWnPb_G"
        ));
        assert!(!is_ipfs_v1(
            "qmYwAPJzv5CZsnA625s3Xf2nemtYgPpHdWEz79ojWnPbdG"
        ));
    }

    #[test]
    fn deployment_metadata_retains_order_and_exact_scalars() {
        let raw = br#"{"metadata":{"z":[{"z":9007199254740993,"a":18446744073709551615}],"10":10,"2":2,"01":1,"0":0,"-0":0,"4294967294":4,"4294967295":5,"a":1,"\u0061":2,"__proto__":{"$serde_json::private::Number":"12"}}}"#;
        let projected = deployment_metadata(raw, MetadataInput::LocalEntity).unwrap();
        let text = serde_json::to_string(&projected).unwrap();
        assert_eq!(
            text,
            r#"{"v":{"0":0,"2":2,"10":10,"4294967294":4,"z":[{"z":9007199254740993,"a":18446744073709551615}],"01":1,"-0":0,"4294967295":5,"a":2,"__proto__":{"$serde_json::private::Number":"12"}}}"#
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap()["v"],
            serde_json::from_slice::<serde_json::Value>(raw).unwrap()["metadata"]
        );
        for (raw, expected) in [
            ("{}", "null"),
            (r#"{"metadata":null}"#, "null"),
            (r#"{"metadata":false}"#, r#"{"v":false}"#),
            (r#"{"metadata":0}"#, r#"{"v":0}"#),
            (r#"{"metadata":""}"#, r#"{"v":""}"#),
        ] {
            assert_eq!(
                serde_json::to_string(
                    &deployment_metadata(raw.as_bytes(), MetadataInput::LocalEntity).unwrap()
                )
                .unwrap(),
                expected
            );
        }
        let ignored = br#"{"type":"scene","pointers":["0,0"],"timestamp":1,"content":[],"metadata":{"z":1,"a":2},"unused":1e400}"#;
        let semantic: Entity = serde_json::from_slice(ignored).unwrap();
        assert_eq!(semantic.metadata, Some(serde_json::json!({"z":1,"a":2})));
        assert_eq!(
            serde_json::to_string(
                &deployment_metadata(ignored, MetadataInput::LocalEntity).unwrap()
            )
            .unwrap(),
            r#"{"v":{"z":1,"a":2}}"#
        );
        let positional = br#"["","scene",["0,0"],1,[],"v3",{"z":1,"a":2}]"#;
        let semantic: Entity = serde_json::from_slice(positional).unwrap();
        assert_eq!(semantic.metadata, Some(serde_json::json!({"z":1,"a":2})));
        assert_eq!(
            serde_json::to_string(
                &deployment_metadata(positional, MetadataInput::LocalEntity).unwrap()
            )
            .unwrap(),
            r#"{"v":{"z":1,"a":2}}"#
        );
        let deep = format!(
            r#"{{"type":"scene","pointers":["0,0"],"timestamp":1,"content":[{{"file":"a","hash":"b","unused":1e400}}],"metadata":{{"z":1,"a":2}},"unused":{}0{}}}"#,
            "[".repeat(160),
            "]".repeat(160)
        );
        assert!(serde_json::from_str::<Entity>(&deep).is_ok());
        assert_eq!(
            serde_json::to_string(
                &deployment_metadata(deep.as_bytes(), MetadataInput::LocalEntity).unwrap()
            )
            .unwrap(),
            r#"{"v":{"z":1,"a":2}}"#
        );
        for local in [true, false] {
            let project = |raw: &str| {
                deployment_metadata(
                    raw.as_bytes(),
                    if local {
                        MetadataInput::LocalEntity
                    } else {
                        MetadataInput::SyncObject
                    },
                )
            };
            for (raw, expected) in [
                (
                    r#"{"metadata":{"before":1},"meta\u0064ata":{"z":2,"a":3}}"#,
                    r#"{"v":{"z":2,"a":3}}"#,
                ),
                (r#"{"meta\u0064ata":{"z":1},"metadata":null}"#, "null"),
                (r#"{"metadata":false}"#, r#"{"v":false}"#),
                (r#"{"metadata":0}"#, r#"{"v":0}"#),
                (r#"{"metadata":""}"#, r#"{"v":""}"#),
                ("null", "null"),
                ("false", "null"),
                ("0", "null"),
                ("1.5", "null"),
                (r#""scalar""#, "null"),
                ("[]", "null"),
                ("[1,2,3]", "null"),
            ] {
                assert_eq!(
                    serde_json::to_string(&project(raw).unwrap()).unwrap(),
                    expected
                );
            }
            for raw in [
                r#"{"metadata":1e400}"#,
                r#"{"metadata":"\ud800"}"#,
                r#"{"metadata":{},"unused":1e}"#,
                r#"{"metadata":{},"unused":[1,]}"#,
                r#"{"metadata":{}} false"#,
                r#"[1,2,3] false"#,
            ] {
                assert!(project(raw).is_err(), "{raw}");
            }
        }
        assert!(
            deployment_metadata(positional, MetadataInput::SyncObject)
                .unwrap()
                .is_null()
        );
        for value in ["null", "false", "0", r#""""#] {
            let raw = format!(r#"["","scene",["0,0"],1,[],"v3",{value}]"#);
            let projected =
                deployment_metadata(raw.as_bytes(), MetadataInput::LocalEntity).unwrap();
            let expected = if value == "null" {
                "null".into()
            } else {
                format!(r#"{{"v":{value}}}"#)
            };
            assert_eq!(serde_json::to_string(&projected).unwrap(), expected);
        }
        let absent = br#"["","scene",["0,0"],1,[],"v3"]"#;
        assert!(
            serde_json::from_slice::<Entity>(absent)
                .unwrap()
                .metadata
                .is_none()
        );
        assert!(
            deployment_metadata(absent, MetadataInput::LocalEntity)
                .unwrap()
                .is_null()
        );
        let extra = br#"["","scene",["0,0"],1,[],"v3",{},0]"#;
        assert!(serde_json::from_slice::<Entity>(extra).is_err());
    }

    #[test]
    fn an_entity_without_content_has_no_files() {
        let entity: Entity = serde_json::from_str(
            r#"{"version":"v3","type":"profile","pointers":["0xab"],"timestamp":1,"metadata":{}}"#,
        )
        .unwrap();
        assert!(entity.content.is_empty());
        assert_eq!(entity.entity_type, EntityType::Profile);
    }

    #[test]
    fn entity_type_round_trips_lowercase_on_the_wire() {
        let json = serde_json::to_string(&EntityType::Outfits).unwrap();
        assert_eq!(json, "\"outfits\"");
        let parsed: EntityType = serde_json::from_str("\"scene\"").unwrap();
        assert_eq!(parsed, EntityType::Scene);
        assert!(serde_json::from_str::<EntityType>("\"Scene\"").is_err());
        assert_eq!(EntityType::Wearable.to_string(), "wearable");
    }

    #[test]
    fn entity_id_and_version_default_when_absent() {
        let entity: Entity = serde_json::from_value(serde_json::json!({
            "type": "profile",
            "pointers": ["0xabc"],
            "timestamp": 1,
            "content": [{"file": "body.png", "hash": "bafy"}]
        }))
        .unwrap();
        assert_eq!(entity.id, "");
        assert_eq!(entity.version, "v3");
        assert_eq!(entity.content[0].hash, "bafy");
        assert!(entity.metadata.is_none());
    }

    #[test]
    fn a_null_version_defaults_and_a_given_one_is_kept() {
        let base = serde_json::json!({"type": "profile", "pointers": ["0xabc"], "timestamp": 1});
        let mut null = base.clone();
        null["version"] = serde_json::Value::Null;
        assert_eq!(
            serde_json::from_value::<Entity>(null).unwrap().version,
            "v3"
        );
        let mut v2 = base;
        v2["version"] = serde_json::json!("v2");
        assert_eq!(serde_json::from_value::<Entity>(v2).unwrap().version, "v2");
    }
}
