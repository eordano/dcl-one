//! Vocabulary every domain shares: signed-fetch verification, identities,
//! the dRPC framework, LiveKit tokens, write gates, the descriptive-data
//! `as_of` rule, the audit writer, caches and HTTP/upstream helpers. It
//! sits below the domains and uses only the leaf crates.

// The crates this one builds on, under their module names (AGENTS.md).
pub(crate) use dcl_auth as auth;
pub(crate) use dcl_http as http;

pub mod as_of;
pub mod audit;
pub mod cache;
pub mod client_ip;
pub mod collections;
pub mod cors;
pub mod drain;
pub mod errors;
pub mod health;
pub mod http_util;
pub mod identity;
pub mod js;
pub mod livekit;
pub mod nul_guard;
pub mod openapi;
pub mod parcel;
pub mod query_params;
pub mod rpc;
pub mod segment_signature;
pub mod signed_fetch;
#[cfg(any(test, feature = "test-support"))]
pub mod test_http;
#[cfg(any(test, feature = "test-support"))]
pub mod test_keys;
#[cfg(any(test, feature = "test-support"))]
pub mod test_wait;
pub mod upstream;
pub mod writes;

/// Current UTC wall time for database timestamp binds.
pub fn now_utc() -> chrono::NaiveDateTime {
    chrono::Utc::now().naive_utc()
}
