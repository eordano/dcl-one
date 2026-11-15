//! The Decentraland Foundation display name
//! (`modules/decentralandFoundationAddresses.ts`): the wallets the feature
//! flag `events-decentraland-foundation-addresses` lists (its variant
//! payload, a JSON array of addresses) answer `user_name` as "Decentraland
//! Foundation" on every event record (`Event/model.ts:493`,
//! `getEventCreatorDisplayName`). Upstream refreshed the list from the
//! feature-flags service every five minutes; this node reads the `events`
//! feature-flags document the explorer API's feature-flags mirror fetches
//! beside the explorer one (`api::feature_flags::run_pass`, row
//! `explore_api.feature_flags` app `events`) on the same cadence, lazily
//! from the event reads, and keeps the last list when the row is absent, as
//! upstream kept its cache when a refresh failed.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use serde_json::Value;
use sqlx::PgPool;

use crate::auth::normalize_eth_address;

/// `decentralandFoundationAddresses.ts:8`.
pub const FOUNDATION_NAME: &str = "Decentraland Foundation";
/// `Flags.DCLFoundationAddresses` (`modules/features.ts:5`).
pub const FLAG: &str = "events-decentraland-foundation-addresses";
/// The feature-flags application the flag lives in.
pub const APP: &str = "events";
/// `server.ts`: `refreshFoundationAddresses` ran every five minutes.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// The listed wallets, lowercased; empty means "no override".
pub type Names = Arc<HashSet<String>>;

struct State {
    names: Names,
    refreshed_at: Option<Instant>,
}

pub struct Foundation {
    state: RwLock<State>,
}

impl Default for Foundation {
    fn default() -> Self {
        Self {
            state: RwLock::new(State {
                names: Arc::new(HashSet::new()),
                refreshed_at: None,
            }),
        }
    }
}

impl Foundation {
    /// A fixed list (tests, or a deployment without the flags document).
    pub fn with_addresses<I: IntoIterator<Item = S>, S: AsRef<str>>(addresses: I) -> Self {
        let this = Self::default();
        this.replace(
            addresses
                .into_iter()
                .filter_map(|a| normalize_eth_address(a.as_ref()))
                .collect(),
        );
        this
    }

    pub fn snapshot(&self) -> Names {
        self.state.read().names.clone()
    }

    fn replace(&self, names: HashSet<String>) {
        let mut state = self.state.write();
        state.names = Arc::new(names);
        state.refreshed_at = Some(Instant::now());
    }

    /// `parseAddressesFromVariantPayload` (`:13-29`) over the flag's variant:
    /// disabled or unparseable payloads empty the list (`:41-44`).
    pub fn apply_document(&self, document: &Value) {
        self.replace(addresses_in(document));
    }

    /// Reads the mirrored `events` flags document once per
    /// [`REFRESH_INTERVAL`]; a missing row or a database error keeps the
    /// current list (`:48-52`).
    pub async fn refresh_if_stale(&self, pool: &PgPool) {
        let due = {
            let state = self.state.read();
            state
                .refreshed_at
                .is_none_or(|at| at.elapsed() >= REFRESH_INTERVAL)
        };
        if !due {
            return;
        }
        // Mark first so concurrent readers do not all query.
        self.state.write().refreshed_at = Some(Instant::now());
        let row: Result<Option<(Value,)>, sqlx::Error> =
            sqlx::query_as("SELECT document FROM explore_api.feature_flags WHERE app = $1")
                .bind(APP)
                .fetch_optional(pool)
                .await;
        match row {
            Ok(Some((document,))) => self.apply_document(&document),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%error, "foundation addresses refresh failed; keeping the last list");
            }
        }
    }
}

fn addresses_in(document: &Value) -> HashSet<String> {
    let variant = document.get("variants").and_then(|v| v.get(FLAG));
    let Some(variant) = variant else {
        return HashSet::new();
    };
    if variant.get("enabled").and_then(Value::as_bool) != Some(true) {
        return HashSet::new();
    }
    let Some(payload) = variant
        .get("payload")
        .and_then(|p| p.get("value"))
        .and_then(Value::as_str)
    else {
        return HashSet::new();
    };
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(payload) else {
        return HashSet::new();
    };
    items
        .iter()
        .filter_map(Value::as_str)
        .filter_map(normalize_eth_address)
        .collect()
}

/// `getEventCreatorDisplayName(address, currentName)` (`:55-66`).
pub fn display_name(
    names: &Names,
    address: Option<&str>,
    current: Option<String>,
) -> Option<String> {
    if names.is_empty() {
        return current;
    }
    match address {
        Some(address) if names.contains(&address.to_ascii_lowercase()) => {
            Some(FOUNDATION_NAME.to_string())
        }
        _ => current,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_flag_payload_lists_the_foundation_wallets() {
        let f = Foundation::default();
        let wallet = "0xAbCdEf0000000000000000000000000000000001";
        f.apply_document(&json!({
            "flags": {},
            "variants": { FLAG: { "enabled": true, "payload": {
                "type": "json",
                "value": format!("[\"{wallet}\", \"nope\", 3]")
            } } }
        }));
        let names = f.snapshot();
        assert_eq!(
            display_name(&names, Some(wallet), Some("Alice".into())),
            Some(FOUNDATION_NAME.into())
        );
        assert_eq!(
            display_name(
                &names,
                Some("0x0000000000000000000000000000000000000002"),
                Some("Bob".into())
            ),
            Some("Bob".into())
        );
        // A disabled flag, an invalid payload or no variant: no override.
        f.apply_document(&json!({ "variants": { FLAG: { "enabled": false } } }));
        assert_eq!(display_name(&f.snapshot(), Some(wallet), None), None);
        f.apply_document(
            &json!({ "variants": { FLAG: { "enabled": true, "payload": { "value": "{" } } } }),
        );
        assert!(f.snapshot().is_empty());
        f.apply_document(&json!({}));
        assert!(f.snapshot().is_empty());
    }
}
