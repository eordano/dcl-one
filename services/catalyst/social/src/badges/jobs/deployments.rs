//! `content.deployments` → the deployment badges, in `id` order. A profile's
//! wallet is its pointer, a scene's or store's the deployer (the auth
//! chain's owner), lowercased. A wearable's rarity is the one its active
//! entity carries (upstream fetched the entities from a catalyst), else the
//! mirrored item's. The cursor starts at the table's head; a deployment
//! stored more than `deployment_max_lag` after its entity timestamp (history
//! a syncing node catches up on) is passed without effect.

use std::collections::{HashMap, HashSet};

use chrono::NaiveDateTime;
use serde_json::Value;

use super::super::evaluators::{self as ev, BusinessStep};
use super::{IdSource, Keyed, Pass, award, completed, read_ids};
use crate::identity::Address;

pub const SOURCE: &str = "deployments";

pub(crate) const READ: IdSource = IdSource {
    name: SOURCE,
    select: "SELECT d.id, d.entity_type, lower(d.deployer_address) AS deployer, \
                    d.entity_pointers AS pointers, d.entity_timestamp, d.local_timestamp, \
                    CASE WHEN d.entity_type = 'profile' THEN d.entity_metadata::jsonb END AS metadata \
             FROM content.deployments d",
    key: "d.id",
    head: "SELECT coalesce(max(id), 0)::bigint FROM content.deployments",
};

#[derive(sqlx::FromRow)]
struct Row {
    id: i32,
    entity_type: String,
    deployer: String,
    pointers: Vec<String>,
    entity_timestamp: NaiveDateTime,
    local_timestamp: NaiveDateTime,
    metadata: Option<Value>,
}

impl Keyed for Row {
    fn key(&self) -> i64 {
        i64::from(self.id)
    }
}

/// The wallet a deployment counts for.
fn wallet(row: &Row) -> Option<Address> {
    let raw = if row.entity_type == "profile" {
        row.pointers.first()?.as_str()
    } else {
        row.deployer.as_str()
    };
    Address::parse(raw).ok()
}

/// A profile's first avatar, inside the stored `{"v": ...}` envelope or not.
fn avatar(metadata: &Value) -> &Value {
    let doc = metadata.get("v").unwrap_or(metadata);
    &doc["avatars"][0]
}

/// The distinct on-chain item URNs a profile equips.
fn equipped(metadata: &Value) -> Vec<String> {
    let mut out: Vec<String> = avatar(metadata)["avatar"]["wearables"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(ev::wearable_item_urn)
        .collect();
    out.sort();
    out.dedup();
    out
}

pub(super) async fn run(pass: &Pass) -> anyhow::Result<u64> {
    let passed = super::drain(|| batch(pass)).await?;
    super::report_id_lag(&pass.pool, &READ).await;
    Ok(passed)
}

pub(crate) async fn batch(pass: &Pass) -> Result<(u64, bool), sqlx::Error> {
    let mut tx = pass.pool.begin().await?;
    let Some(read) = read_ids::<Row>(&mut tx, pass, &READ).await? else {
        tx.commit().await?;
        return Ok((0, false));
    };
    if read.is_idle() {
        tx.rollback().await?;
        return Ok((0, false));
    }
    let rows = &read.rows;
    let now = crate::shared::now_utc();
    let lag = pass
        .settings
        .deployment_max_lag
        .and_then(|l| chrono::Duration::from_std(l).ok());
    let live: Vec<(&Row, Address)> = rows
        .iter()
        .filter(|r| matches!(r.entity_type.as_str(), "profile" | "scene" | "store"))
        .filter(|r| !pass.too_old(r.entity_timestamp, now))
        .filter(|r| lag.is_none_or(|lag| r.local_timestamp - r.entity_timestamp <= lag))
        .filter_map(|r| match wallet(r) {
            Some(a) => Some((r, a)),
            None => {
                tracing::debug!(id = r.id, "deployment without a wallet address: passed");
                None
            }
        })
        .collect();
    let addresses: Vec<String> = live
        .iter()
        .map(|(_, a)| a.as_str().to_string())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let done = completed(&mut tx, &addresses).await?;
    let urns: Vec<String> = live
        .iter()
        .filter_map(|(r, _)| r.metadata.as_ref())
        .flat_map(equipped)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let rarity: HashMap<String, Option<String>> = if urns.is_empty() {
        HashMap::new()
    } else {
        sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT u.urn, lower(coalesce(m.doc->'v'->>'rarity', m.doc->>'rarity', i.rarity)) \
             FROM unnest($1::text[]) AS u(urn) \
             LEFT JOIN content.active_pointers ap ON ap.pointer = u.urn \
             LEFT JOIN content.deployments d ON d.entity_id = ap.entity_id \
             LEFT JOIN LATERAL (SELECT d.entity_metadata::jsonb AS doc) m ON true \
             LEFT JOIN shop.items i ON i.urn = u.urn",
        )
        .bind(&urns)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .collect()
    };
    let notify = Some(pass.asset_base.as_str());
    for (row, address) in &live {
        let skip = |badge: &str| done.contains(&(address.as_str().to_string(), badge.to_string()));
        match row.entity_type.as_str() {
            "profile" => {
                let Some(metadata) = row.metadata.as_ref() else {
                    continue;
                };
                let description = avatar(metadata)["description"].as_str();
                if !skip(ev::PROFILE_PRO) && description.is_some_and(|d| !d.is_empty()) {
                    award(&mut tx, address, ev::PROFILE_PRO, notify, |p| {
                        ev::profile_pro(p, description)
                    })
                    .await?;
                }
                let worn: Vec<(String, Option<String>)> = equipped(metadata)
                    .into_iter()
                    .map(|urn| {
                        let r = rarity.get(&urn).cloned().flatten();
                        (urn, r)
                    })
                    .collect();
                for (badge, wanted) in ev::RARITY_BADGES {
                    let count = worn
                        .iter()
                        .filter(|(_, r)| r.as_deref() == Some(wanted))
                        .count();
                    if count >= ev::WEARABLES_OF_A_RARITY && !skip(badge) {
                        award(&mut tx, address, badge, notify, |p| {
                            ev::rarity_badge(p, wanted, &worn)
                        })
                        .await?;
                    }
                }
            }
            "scene" if !skip(ev::LAND_ARCHITECT) => {
                award(
                    &mut tx,
                    address,
                    ev::LAND_ARCHITECT,
                    notify,
                    ev::land_architect,
                )
                .await?;
            }
            "store" if !skip(ev::OPEN_FOR_BUSINESS) => {
                award(&mut tx, address, ev::OPEN_FOR_BUSINESS, notify, |p| {
                    ev::open_for_business(p, BusinessStep::Store)
                })
                .await?;
            }
            _ => {}
        }
    }
    read.finish(&mut tx).await?;
    tx.commit().await?;
    Ok((rows.len() as u64, read.more))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn profiles_read_with_or_without_the_envelope() {
        let wrapped = json!({"v": {"avatars": [{"description": "d", "avatar": {"wearables": [
            "urn:decentraland:matic:collections-v2:0xAA:1:42",
            "urn:decentraland:matic:collections-v2:0xaa:1",
            "urn:decentraland:off-chain:base-avatars:eyes_00"
        ]}}]}});
        assert_eq!(avatar(&wrapped)["description"], json!("d"));
        assert_eq!(
            equipped(&wrapped),
            vec!["urn:decentraland:matic:collections-v2:0xaa:1".to_string()]
        );
        let bare = json!({"avatars": [{"description": "e"}]});
        assert_eq!(avatar(&bare)["description"], json!("e"));
    }
}
