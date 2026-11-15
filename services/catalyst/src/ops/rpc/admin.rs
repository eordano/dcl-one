//! `/admin/rpc/*` behind `CATALYRST_RPC_ADMIN_TOKEN`: the method allowlist
//! (list, add, remove, reset to `READ_ONLY_METHODS`; an add of anything
//! but a read method is refused, `super::is_read_method`) and the network map
//! (list, upsert, remove), edited in memory. An upsert refuses a private
//! host by the same guard the readers use, or, under the private opt-in,
//! by the narrow local deny-list. Every mutation records one
//! `ops.admin_audit` row after the effect (actor `token:CATALYRST_RPC_ADMIN_TOKEN`, the credential's
//! variable name, as the credits bearer writes its; domain `rpc`,
//! the upstream reduced to its host).

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{READ_ONLY_METHODS, Shared};

/// The audit actor: `token:<the credential's variable name>`, the shape every
/// admin bearer in the slice writes, so `/admin/api/audit?actor=` filters on one rule.
pub const AUDIT_ACTOR: &str = "token:CATALYRST_RPC_ADMIN_TOKEN";
pub const AUDIT_DOMAIN: &str = "rpc";

pub fn routes() -> Router<Shared> {
    Router::new()
        .route("/admin/rpc/config", get(get_config))
        .route(
            "/admin/rpc/methods",
            get(list_methods).post(add_method).delete(remove_method),
        )
        .route("/admin/rpc/methods/reset", post(reset_methods))
        .route(
            "/admin/rpc/networks",
            get(list_networks).post(upsert_network),
        )
        .route(
            "/admin/rpc/networks/{network}",
            axum::routing::delete(remove_network),
        )
}

pub(crate) fn authorize_admin(state: &Shared, headers: &HeaderMap) -> Result<(), StatusCode> {
    crate::auth::admin_bearer::check(state.settings.admin_token.as_deref(), headers)
        .map_err(|_| StatusCode::FORBIDDEN)
}

fn forbidden(status: StatusCode) -> Response {
    (status, Json(json!({ "error": "forbidden" }))).into_response()
}

fn bad_request(message: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response()
}

/// The same row `ops::admin::audit::record_after` writes (the console's
/// module, the ONE writer once it is declared): best effort, after the
/// in-memory effect, the failure logged with the action and never the detail.
async fn audit(state: &Shared, action: &str, target: &str, detail: Value) {
    let res = sqlx::query(
        "INSERT INTO ops.admin_audit (actor, domain, action, target, detail, result) VALUES ($1, $2, $3, $4, $5, 'ok')",
    )
    .bind(AUDIT_ACTOR)
    .bind(AUDIT_DOMAIN)
    .bind(action)
    .bind(target)
    .bind(&detail)
    .execute(&state.pool)
    .await;
    if let Err(e) = res {
        tracing::error!(error = %e, action = %format!("{AUDIT_DOMAIN}.{action}"), "failed to write the ops.admin_audit row (mutation already applied)");
    }
}

#[derive(Deserialize)]
pub(crate) struct MethodBody {
    method: String,
}

#[derive(Deserialize)]
pub(crate) struct NetworkBody {
    network: String,
    url: String,
}

pub(crate) async fn get_config(State(state): State<Shared>, headers: HeaderMap) -> Response {
    if let Err(s) = authorize_admin(&state, &headers) {
        return forbidden(s);
    }
    Json(json!({ "methods": state.methods_snapshot(), "networks": state.upstreams_snapshot() }))
        .into_response()
}

pub(crate) async fn list_methods(State(state): State<Shared>, headers: HeaderMap) -> Response {
    if let Err(s) = authorize_admin(&state, &headers) {
        return forbidden(s);
    }
    Json(json!({ "methods": state.methods_snapshot() })).into_response()
}

pub(crate) async fn add_method(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<MethodBody>,
) -> Response {
    if let Err(s) = authorize_admin(&state, &headers) {
        return forbidden(s);
    }
    let method = body.method.trim().to_string();
    if method.is_empty() {
        return bad_request("method must be non-empty");
    }
    if !super::is_read_method(&method) {
        return bad_request(
            "the relay is read-only: only read methods (eth_get*, eth_call, eth_estimateGas, eth_chainId, eth_blockNumber, eth_gasPrice, eth_feeHistory, eth_maxPriorityFeePerGas, eth_syncing, eth_protocolVersion, net_*, web3_*) may be allowed",
        );
    }
    let added = state
        .allowed_methods
        .write()
        .expect("allowed_methods lock poisoned")
        .insert(method.clone());
    tracing::info!(%method, added, "admin amended method allowlist (add)");
    audit(
        &state,
        "methods.add",
        &method,
        json!({ "method": method, "added": added }),
    )
    .await;
    Json(json!({ "method": method, "added": added, "methods": state.methods_snapshot() }))
        .into_response()
}

pub(crate) async fn remove_method(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<MethodBody>,
) -> Response {
    if let Err(s) = authorize_admin(&state, &headers) {
        return forbidden(s);
    }
    let method = body.method.trim().to_string();
    let removed = state
        .allowed_methods
        .write()
        .expect("allowed_methods lock poisoned")
        .remove(&method);
    tracing::info!(%method, removed, "admin amended method allowlist (remove)");
    audit(
        &state,
        "methods.remove",
        &method,
        json!({ "method": method, "removed": removed }),
    )
    .await;
    Json(json!({ "method": method, "removed": removed, "methods": state.methods_snapshot() }))
        .into_response()
}

pub(crate) async fn reset_methods(State(state): State<Shared>, headers: HeaderMap) -> Response {
    if let Err(s) = authorize_admin(&state, &headers) {
        return forbidden(s);
    }
    *state
        .allowed_methods
        .write()
        .expect("allowed_methods lock poisoned") =
        READ_ONLY_METHODS.iter().map(|m| m.to_string()).collect();
    tracing::info!("admin reset method allowlist to defaults");
    audit(
        &state,
        "methods.reset",
        "defaults",
        json!({ "methods": READ_ONLY_METHODS.len() }),
    )
    .await;
    Json(json!({ "methods": state.methods_snapshot() })).into_response()
}

pub(crate) async fn list_networks(State(state): State<Shared>, headers: HeaderMap) -> Response {
    if let Err(s) = authorize_admin(&state, &headers) {
        return forbidden(s);
    }
    Json(json!({ "networks": state.upstreams_snapshot() })).into_response()
}

/// Why `url` is not an acceptable upstream, or `None`. Without the opt-in
/// the readers' SSRF guard decides; with it only the host's own stack, the
/// reserved ranges and the metadata endpoint are refused.
pub fn upstream_refusal(url: &str, allow_private: bool) -> Option<&'static str> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Some("url must be http(s)");
    }
    if is_blocked_upstream_host_with(url, allow_private) {
        return Some("upstream host not allowed (private, internal or single-label host)");
    }
    None
}

fn is_blocked_upstream_host_with(url: &str, allow_private: bool) -> bool {
    if allow_private {
        return blocks_local_upstream_only(url);
    }
    !crate::http::is_safe_http_url(url)
}

/// The narrow deny-list a self-hosted deployment opts into: a compose
/// service name (`http://geth:8545`) or any other internal DNS name has to
/// stay reachable, so only the host's own stack, the reserved IP ranges and
/// the cloud metadata endpoint are refused.
///
/// The host is read by the same URL parser the HTTP client uses, so every
/// spelling it normalizes to an address (`127.1`, `2130706433`,
/// `0x7f.0.0.1`, `[::127.0.0.1]`) is judged as that address. An address
/// literal is judged by the readers' own IP deny-list
/// (`dcl_http::is_blocked_ip`), so the opt-in widens names, never literal
/// addresses. A URL the parser refuses is refused.
pub fn blocks_local_upstream_only(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return true;
    };
    match parsed.host() {
        None => true,
        Some(url::Host::Domain(name)) => {
            let name = name.trim_end_matches('.').to_ascii_lowercase();
            name.is_empty()
                || name == "localhost"
                || name.ends_with(".localhost")
                || name == "metadata.google.internal"
        }
        Some(url::Host::Ipv4(ip)) => crate::http::is_blocked_ip(ip.into()),
        Some(url::Host::Ipv6(ip)) => crate::http::is_blocked_ip(ip.into()),
    }
}

/// The host of an upstream URL for logs and the audit row (a gateway key
/// sits in the path of most hosted RPC URLs).
pub fn host_of(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| "<unparseable>".into())
}

pub(crate) async fn upsert_network(
    State(state): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<NetworkBody>,
) -> Response {
    if let Err(s) = authorize_admin(&state, &headers) {
        return forbidden(s);
    }
    let network = body.network.trim().to_ascii_lowercase();
    let url = body.url.trim().to_string();
    if network.is_empty() || url.is_empty() {
        return bad_request("network and url must be non-empty");
    }
    if let Some(reason) = upstream_refusal(&url, state.settings.allow_private_upstream) {
        return bad_request(reason);
    }
    state
        .upstreams
        .write()
        .expect("upstreams lock poisoned")
        .insert(network.clone(), url.clone());
    let host = host_of(&url);
    tracing::info!(%network, %host, "admin upserted network upstream");
    audit(
        &state,
        "networks.upsert",
        &network,
        json!({ "network": network, "host": host }),
    )
    .await;
    Json(json!({ "network": network, "url": url, "networks": state.upstreams_snapshot() }))
        .into_response()
}

pub(crate) async fn remove_network(
    State(state): State<Shared>,
    headers: HeaderMap,
    Path(network): Path<String>,
) -> Response {
    if let Err(s) = authorize_admin(&state, &headers) {
        return forbidden(s);
    }
    let network = network.trim().to_ascii_lowercase();
    let removed = state
        .upstreams
        .write()
        .expect("upstreams lock poisoned")
        .remove(&network)
        .is_some();
    tracing::info!(%network, removed, "admin removed network upstream");
    audit(
        &state,
        "networks.remove",
        &network,
        json!({ "network": network, "removed": removed }),
    )
    .await;
    Json(json!({ "network": network, "removed": removed, "networks": state.upstreams_snapshot() }))
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::rpc::{Settings, State as RpcState};

    fn state_with(token: Option<&str>) -> Shared {
        let mut settings = Settings::for_tests(&[]);
        settings.admin_token = token.map(|t| t.to_string());
        RpcState::new(
            settings,
            crate::http::Outbound::for_tests(),
            crate::ops::rpc::tests::lazy_pool(),
        )
    }

    fn hdr(value: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(v) = value {
            h.insert("authorization", v.parse().unwrap());
        }
        h
    }

    #[tokio::test]
    async fn unset_token_fails_closed() {
        let st = state_with(None);
        assert_eq!(
            authorize_admin(&st, &hdr(Some("Bearer anything"))),
            Err(StatusCode::FORBIDDEN)
        );
    }

    #[tokio::test]
    async fn missing_header_is_forbidden() {
        let st = state_with(Some("secret"));
        assert_eq!(authorize_admin(&st, &hdr(None)), Err(StatusCode::FORBIDDEN));
    }

    #[tokio::test]
    async fn wrong_token_is_forbidden() {
        let st = state_with(Some("secret"));
        assert_eq!(
            authorize_admin(&st, &hdr(Some("Bearer nope"))),
            Err(StatusCode::FORBIDDEN)
        );
    }

    #[tokio::test]
    async fn correct_token_authorizes() {
        let st = state_with(Some("secret"));
        assert_eq!(authorize_admin(&st, &hdr(Some("Bearer secret"))), Ok(()));
    }

    #[test]
    fn default_upstream_guard_refuses_internal_hosts() {
        for url in [
            "http://geth:8545",
            "http://erigon:8545",
            "http://eth.internal/rpc",
            "http://127.0.0.1:8545",
            "http://10.0.0.5:8545",
        ] {
            assert!(
                is_blocked_upstream_host_with(url, false),
                "{url} must be refused by default"
            );
        }
        assert!(!is_blocked_upstream_host_with(
            "https://rpc.example.com",
            false
        ));
        assert_eq!(
            upstream_refusal("ftp://rpc.example.com", false),
            Some("url must be http(s)")
        );
        assert_eq!(upstream_refusal("https://rpc.example.com", false), None);
    }

    #[test]
    fn opted_in_upstream_guard_allows_service_names_but_not_the_host_itself() {
        for url in ["http://geth:8545", "http://eth.internal/rpc"] {
            assert!(
                !is_blocked_upstream_host_with(url, true),
                "{url} must be reachable for a self-hosted deployment"
            );
        }
        for url in [
            "http://127.0.0.1:8545",
            "http://localhost:8545",
            "http://10.0.0.5:8545",
            "http://169.254.169.254/",
            "http://metadata.google.internal/",
            "http://[::1]:8545",
            "http://[fd00::1]:8545",
        ] {
            assert!(
                is_blocked_upstream_host_with(url, true),
                "{url} must stay refused even with the opt-in"
            );
        }
        assert!(!is_blocked_upstream_host_with(
            "https://rpc.example.com",
            true
        ));
    }

    #[test]
    fn opted_in_upstream_guard_reads_the_host_as_the_http_client_does() {
        for url in [
            "http://2130706433:8545/",
            "http://127.1:8545/",
            "http://0x7f.0.0.1:8545/",
            "http://0x7f.1:8545/",
            "http://0177.0.0.1:8545/",
            "http://167772165:8545/",
            "http://0xa9fea9fe/",
            "http://[::127.0.0.1]:8545/",
            "http://[::ffff:7f00:1]:8545/",
            "http://[64:ff9b::7f00:1]:8545/",
            "http://[::ffff:10.0.0.5]:8545/",
            "http://localhost.:8545/",
            "http://metadata.google.internal./",
            "http://LOCALHOST:8545/",
            "http://user@127.0.0.1:8545/",
            "http://",
        ] {
            assert!(
                is_blocked_upstream_host_with(url, true),
                "{url} must stay refused even with the opt-in"
            );
        }
        for url in [
            "http://geth:8545",
            "http://8.8.8.8/",
            "http://[2001:db8::1]/",
        ] {
            assert!(
                !is_blocked_upstream_host_with(url, true),
                "{url} must be reachable for a self-hosted deployment"
            );
        }
    }

    #[test]
    fn opted_in_upstream_guard_refuses_every_non_public_address_literal() {
        for url in [
            "http://0.1.2.3:8545/",
            "http://100.64.0.1:8545/",
            "http://192.0.2.1:8545/",
            "http://198.18.0.1:8545/",
            "http://255.255.255.255:8545/",
            "http://224.0.0.1:8545/",
            "http://240.0.0.1:8545/",
            "http://[fec0::1]:8545/",
            "http://[ff02::1]:8545/",
            "http://[64:ff9b:1::a00:5]:8545/",
            "http://[2002:a00:5::1]:8545/",
            "http://[2002:7f00:1::]:8545/",
        ] {
            assert!(
                is_blocked_upstream_host_with(url, true),
                "{url} must stay refused even with the opt-in"
            );
        }
        for url in ["http://[2002:808:808::1]:8545/", "http://[::808:808]:8545/"] {
            assert!(
                !is_blocked_upstream_host_with(url, true),
                "{url} wraps a public IPv4 address and must be reachable"
            );
        }
    }

    #[tokio::test]
    async fn admin_auth_probe_empty_configured_token_must_reject() {
        let st = state_with(Some(""));
        assert_eq!(
            authorize_admin(&st, &hdr(Some("Bearer "))),
            Err(StatusCode::FORBIDDEN)
        );
        assert_eq!(
            authorize_admin(&st, &hdr(Some("Bearer anything"))),
            Err(StatusCode::FORBIDDEN)
        );
        let st = state_with(Some("the-real-token"));
        assert_eq!(
            authorize_admin(&st, &hdr(Some("Bearer "))),
            Err(StatusCode::FORBIDDEN)
        );
        assert_eq!(
            authorize_admin(&st, &hdr(Some("Bearer the-real-token"))),
            Ok(())
        );
    }

    #[tokio::test]
    async fn methods_mutation_round_trips() {
        let st = state_with(Some("t"));
        assert!(!st.is_method_allowed("eth_feeHistory"));
        st.allowed_methods
            .write()
            .unwrap()
            .insert("eth_feeHistory".into());
        assert!(st.is_method_allowed("eth_feeHistory"));
    }

    /// The relay is read-only: the admin may widen it only by a read method
    /// (an allowlist, so a method no deny-list names, a filter the upstream
    /// would keep, a dev chain's cheat code, is refused too).
    #[tokio::test]
    async fn the_admin_may_allow_only_read_methods() {
        let st = state_with(Some("t"));
        for method in [
            "eth_sendRawTransaction",
            "eth_sendTransaction",
            "eth_signTypedData_v4",
            "personal_unlockAccount",
            "admin_addPeer",
            "debug_setHead",
            " ETH_SENDRAWTRANSACTION ",
            "eth_newFilter",
            "eth_subscribe",
            "txpool_content",
            "anvil_setBalance",
        ] {
            let res = add_method(
                State(st.clone()),
                hdr(Some("Bearer t")),
                Json(MethodBody {
                    method: method.to_string(),
                }),
            )
            .await;
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{method}");
            assert!(!st.is_method_allowed(method.trim()), "{method}");
        }
        st.allowed_methods
            .write()
            .unwrap()
            .insert("eth_sendRawTransaction".into());
        assert!(
            !st.is_method_allowed("eth_sendRawTransaction"),
            "refused at the relay even when it is in the set"
        );
        let res = add_method(
            State(st.clone()),
            hdr(Some("Bearer t")),
            Json(MethodBody {
                method: "eth_feeHistory".to_string(),
            }),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK, "a read stays addable");
        assert!(st.is_method_allowed("eth_feeHistory"));
    }

    #[test]
    fn the_audit_row_carries_the_host_never_the_url() {
        assert_eq!(
            host_of("https://mainnet.infura.io/v3/SECRET"),
            "mainnet.infura.io"
        );
        assert_eq!(host_of("http://geth:8545"), "geth");
        assert_eq!(host_of("nonsense"), "<unparseable>");
    }
}
