//! The default outbound HTTP handle: a hardened [`reqwest::Client`] built through
//! [`HttpClientCfg`] and wrapped in the shared [`Outbound`] `serve` hands to
//! readers (peer sync and the RPC validator take its `raw` client; mirrors and
//! upstream readers send through the guard). Provider adapters that construct
//! an Outbound with distinct transport settings own separate HTTP pools. The SSRF guard
//! ([`is_safe_http_url`] on the string, [`resolve_and_pin`] on the socket),
//! the retry ladder ([`retry_with_backoff`]) and the capped body reader.

use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;

#[cfg(feature = "db")]
pub mod api_error;
mod body;
pub mod jsonrpc;

pub use body::read_body_capped;

pub const USER_AGENT: &str = concat!("catalyst/", env!("CARGO_PKG_VERSION"));

/// Redirect following is opt-in: for user-supplied URLs each hop has to be
/// re-checked against the SSRF guards, which reqwest's own redirect policy
/// cannot do. Configured upstream endpoints (peers, mirror sources) may
/// follow a bounded number.
#[derive(Clone, Debug)]
pub struct HttpClientCfg {
    /// Whole-request budget; `None` for readers that stream large bodies
    /// (snapshots) and bound them by `read_timeout` and the body cap instead.
    pub total_timeout: Option<Duration>,
    pub connect_timeout: Option<Duration>,
    /// Longest pause between two bytes of a response.
    pub read_timeout: Option<Duration>,
    pub pool_idle_timeout: Duration,
    pub pool_max_idle_per_host: usize,
    pub max_redirects: Option<usize>,
    pub tcp_nodelay: bool,
    pub user_agent: Option<String>,
}

impl Default for HttpClientCfg {
    fn default() -> Self {
        Self {
            total_timeout: Some(Duration::from_secs(30)),
            connect_timeout: Some(Duration::from_secs(10)),
            read_timeout: None,
            pool_idle_timeout: Duration::from_secs(90),
            pool_max_idle_per_host: 16,
            max_redirects: None,
            tcp_nodelay: true,
            user_agent: None,
        }
    }
}

impl HttpClientCfg {
    /// The client `serve` builds once for every upstream reader, with the old
    /// sync client's limits: no whole-request budget (snapshot files are
    /// gigabytes), 8 s connect, 25 s read, `CONNECTIONS_MAX_IDLE` idle
    /// connections per host, at most two redirects.
    pub fn serve(connections_max_idle: usize) -> Self {
        Self {
            total_timeout: None,
            connect_timeout: Some(Duration::from_secs(8)),
            read_timeout: Some(Duration::from_secs(25)),
            pool_max_idle_per_host: connections_max_idle,
            max_redirects: Some(2),
            ..Self::default()
        }
    }

    pub fn with_total_timeout(mut self, timeout: Duration) -> Self {
        self.total_timeout = Some(timeout);
        self
    }

    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = Some(timeout);
        self
    }

    pub fn with_pool(mut self, idle_timeout: Duration, max_idle_per_host: usize) -> Self {
        self.pool_idle_timeout = idle_timeout;
        self.pool_max_idle_per_host = max_idle_per_host;
        self
    }

    pub fn following_redirects(mut self, max: usize) -> Self {
        self.max_redirects = Some(max);
        self
    }

    pub fn with_user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.user_agent = Some(user_agent.into());
        self
    }

    pub fn builder(&self) -> reqwest::ClientBuilder {
        let redirect = match self.max_redirects {
            Some(max) => guarded_redirect_policy(max),
            None => reqwest::redirect::Policy::none(),
        };
        let mut builder = reqwest::Client::builder()
            .user_agent(self.user_agent.as_deref().unwrap_or(USER_AGENT))
            .pool_idle_timeout(self.pool_idle_timeout)
            .pool_max_idle_per_host(self.pool_max_idle_per_host)
            .tcp_nodelay(self.tcp_nodelay)
            .dns_resolver(ReservedNamesResolver::default())
            .redirect(redirect);
        if let Some(connect) = self.connect_timeout {
            builder = builder.connect_timeout(connect);
        }
        if let Some(total) = self.total_timeout {
            builder = builder.timeout(total);
        }
        if let Some(read) = self.read_timeout {
            builder = builder.read_timeout(read);
        }
        builder
    }

    pub fn build(&self) -> reqwest::Result<reqwest::Client> {
        self.builder().build()
    }
}

/// What RFC 6761 fixes for a name before any resolver is asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReservedName {
    /// `invalid` and every `.invalid` name: NXDOMAIN.
    Invalid,
    /// `localhost` and every `.localhost` name: the loopback.
    Loopback,
}

pub fn reserved_name(host: &str) -> Option<ReservedName> {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if host == "invalid" || host.ends_with(".invalid") {
        Some(ReservedName::Invalid)
    } else if host == "localhost" || host.ends_with(".localhost") {
        Some(ReservedName::Loopback)
    } else {
        None
    }
}

type SystemLookup = dyn Fn(String) -> reqwest::dns::Resolving + Send + Sync;

/// The resolver every [`HttpClientCfg`] client asks. A `.invalid` name fails
/// as NXDOMAIN and a `localhost`/`.localhost` one is the loopback without a
/// query leaving the process (an island's placeholders and its own names never
/// reach a DNS server); every other name goes to `system`, the
/// `getaddrinfo` lookup reqwest's default resolver makes.
#[derive(Clone)]
pub struct ReservedNamesResolver {
    system: Arc<SystemLookup>,
}

impl Default for ReservedNamesResolver {
    fn default() -> Self {
        Self::over(system_lookup)
    }
}

impl ReservedNamesResolver {
    pub fn over(
        system: impl Fn(String) -> reqwest::dns::Resolving + Send + Sync + 'static,
    ) -> Self {
        Self {
            system: Arc::new(system),
        }
    }
}

fn system_lookup(host: String) -> reqwest::dns::Resolving {
    Box::pin(async move {
        let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
        Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
    })
}

impl reqwest::dns::Resolve for ReservedNamesResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        match reserved_name(&host) {
            Some(ReservedName::Invalid) => Box::pin(async move {
                Err(format!("{host}: NXDOMAIN, an .invalid name never resolves (RFC 6761)").into())
            }),
            Some(ReservedName::Loopback) => Box::pin(async {
                let loopback = [
                    SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 0),
                    SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST), 0),
                ];
                Ok(Box::new(loopback.into_iter()) as reqwest::dns::Addrs)
            }),
            None => (self.system)(host),
        }
    }
}

/// Follows at most `max` hops, and only to a public http(s) host once the
/// chain started at one: a public upstream (or a MITM on a plaintext one)
/// cannot steer a fetch onto the loopback or the metadata address with a
/// `Location` header. A chain that started on a private host (a development
/// peer on the loopback) keeps following private hops.
fn guarded_redirect_policy(max: usize) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() > max {
            return attempt.error(format!("more than {max} redirects"));
        }
        let started_public = attempt
            .previous()
            .first()
            .is_none_or(|u| is_safe_http_url(u.as_str()));
        if started_public && !is_safe_http_url(attempt.url().as_str()) {
            let host = attempt.url().host_str().unwrap_or("?").to_string();
            return attempt.error(format!(
                "redirect to {host} refused: not a public http(s) host"
            ));
        }
        attempt.follow()
    })
}

pub fn try_http_client(cfg: &HttpClientCfg) -> reqwest::Result<reqwest::Client> {
    cfg.build()
}

/// Falls back to a default client so a TLS-backend hiccup degrades the
/// timeouts rather than taking the service down at startup.
pub fn http_client(name: &str, cfg: &HttpClientCfg) -> reqwest::Client {
    match try_http_client(cfg) {
        Ok(client) => client,
        Err(err) => {
            tracing::warn!(client = name, %err, "http client build failed; using defaults");
            reqwest::Client::new()
        }
    }
}

const INTERNAL_HOST_SUFFIXES: [&str; 8] = [
    ".localhost",
    ".local",
    ".internal",
    ".intranet",
    ".lan",
    ".home",
    ".corp",
    ".home.arpa",
];

/// True for anything a server must not be talked into fetching: reserved and
/// private IP ranges, single-label names, and the reserved internal DNS
/// suffixes.
///
/// Two check classes are merged because neither alone is sufficient. The
/// `IpAddr` range checks catch every literal form the standard parser accepts
/// (including IPv4-mapped IPv6), but reject nothing that is not a literal; the
/// string checks catch names that never reach a resolver (`router`,
/// `nas.local`) and the URL-spec IPv4 forms (`0x7f000001`, `0177.0.0.1`,
/// `127.1`) that `IpAddr` refuses to parse.
pub fn is_private_or_internal_host(host: &str) -> bool {
    let host = host.trim();
    let unbracketed = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    let normalized = unbracketed.trim_end_matches('.').to_ascii_lowercase();

    if normalized.is_empty() {
        return true;
    }
    if let Ok(ip) = normalized.parse::<IpAddr>() {
        return is_blocked_ip(ip);
    }
    if let Some(ip) = spec_normalized_ip(&normalized) {
        return is_blocked_ip(ip);
    }
    is_internal_hostname(&normalized)
}

/// True only for an `http`/`https` URL whose host is publicly routable.
///
/// A clicked link reaches an unrestricted URL opener on the viewer's machine
/// and a server-side fetch reaches the machine's own network, so anything that
/// is not plainly safe fails closed. Whitespace anywhere in the target is
/// rejected outright rather than percent-encoded away, since it is only ever
/// there to smuggle a second URL past a parser.
pub fn is_safe_http_url(url: &str) -> bool {
    let trimmed = url.trim_matches(is_js_whitespace);
    if trimmed.chars().any(is_js_whitespace) {
        return false;
    }
    let Ok(parsed) = url::Url::parse(trimmed) else {
        return false;
    };
    if parsed.scheme() != "https" && parsed.scheme() != "http" {
        return false;
    }
    match parsed.host_str() {
        Some(host) => !is_private_or_internal_host(host),
        None => false,
    }
}

/// Resolves `url`'s host and returns the address to pin the connection to, or
/// `None` if the host is not publicly routable.
///
/// Pinning is what closes the DNS-rebinding window: the guard's verdict and
/// the socket the request actually opens come from the same lookup, so a name
/// that answers differently a millisecond later cannot be dialled. Every
/// answer must be public -- one blocked address in the set rejects the host.
pub async fn resolve_and_pin(url: &reqwest::Url) -> Option<SocketAddr> {
    let host = url.host_str()?;
    let port = url.port_or_known_default().unwrap_or(80);
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);

    if let Ok(ip) = bare.parse::<IpAddr>() {
        return (!is_blocked_ip(ip)).then_some(SocketAddr::new(ip, port));
    }
    if is_private_or_internal_host(host) || reserved_name(bare).is_some() {
        return None;
    }

    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((bare, port)).await.ok()?.collect();
    if addrs.is_empty() || addrs.iter().any(|a| is_blocked_ip(a.ip())) {
        return None;
    }
    Some(addrs[0])
}

/// Whether `ip` is outside the public internet: loopback, RFC 1918, link-local,
/// unspecified, broadcast, multicast, documentation, shared (`100.64.0.0/10`),
/// `0.0.0.0/8`, benchmarking and reserved IPv4; loopback, unspecified,
/// multicast, unique-local, link-local, site-local and local-use NAT64 IPv6.
/// An IPv6 address that embeds an IPv4 one is judged by the embedded address.
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                || v4.octets()[0] == 0
                || {
                    let o = v4.octets();
                    (o[0] == 100 && (o[1] & 0xc0) == 0x40)
                        // Benchmarking 198.18.0.0/15 and reserved 240.0.0.0/4.
                        || (o[0] == 198 && (o[1] & 0xfe) == 18)
                        || o[0] >= 240
                }
        }
        IpAddr::V6(v6) => {
            if let Some(embedded) = embedded_ipv4(v6) {
                return is_blocked_ip(IpAddr::V4(embedded));
            }
            let s = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00
                || (s[0] & 0xffc0) == 0xfe80
                // Deprecated site-local fec0::/10.
                || (s[0] & 0xffc0) == 0xfec0
                // Local-use NAT64 64:ff9b:1::/48, whose embedding is operator-chosen.
                || (s[0] == 0x64 && s[1] == 0xff9b && s[2] == 1)
        }
    }
}

/// The IPv4 address an IPv6 one reaches: IPv4-mapped `::ffff:a.b.c.d`,
/// IPv4-compatible `::a.b.c.d`, NAT64 `64:ff9b::a.b.c.d` and 6to4 `2002:aabb:ccdd::/48`.
fn embedded_ipv4(v6: std::net::Ipv6Addr) -> Option<std::net::Ipv4Addr> {
    if let Some(mapped) = v6.to_ipv4_mapped() {
        return Some(mapped);
    }
    let s = v6.segments();
    let low = || std::net::Ipv4Addr::from((u32::from(s[6]) << 16) | u32::from(s[7]));
    match s {
        [0, 0, 0, 0, 0, 0, _, _] if !v6.is_loopback() && !v6.is_unspecified() => Some(low()),
        [0x64, 0xff9b, 0, 0, 0, 0, _, _] => Some(low()),
        [0x2002, hi, lo, ..] => Some(std::net::Ipv4Addr::from(
            (u32::from(hi) << 16) | u32::from(lo),
        )),
        _ => None,
    }
}

fn spec_normalized_ip(host: &str) -> Option<IpAddr> {
    let parsed = url::Url::parse(&format!("http://{host}/")).ok()?;
    let normalized = parsed.host_str()?;
    if normalized == host {
        return None;
    }
    normalized
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(normalized)
        .parse::<IpAddr>()
        .ok()
}

fn is_internal_hostname(host: &str) -> bool {
    if let Some([a, b, _, _]) = parse_dotted_quad(host) {
        return a == 0
            || a == 127
            || a == 10
            || (a == 169 && b == 254)
            || (a == 172 && (16..=31).contains(&b))
            || (a == 192 && b == 168)
            || (a == 100 && (64..=127).contains(&b));
    }

    if host.contains(':') {
        return host == "::1"
            || host == "::"
            || ["fe8", "fe9", "fea", "feb"]
                .iter()
                .any(|p| host.starts_with(p))
            || host.starts_with("fc")
            || host.starts_with("fd")
            || host.starts_with("::ffff:");
    }

    if !host.contains('.') {
        return true;
    }
    INTERNAL_HOST_SUFFIXES
        .iter()
        .any(|suffix| host.ends_with(suffix))
}

fn parse_dotted_quad(host: &str) -> Option<[u32; 4]> {
    let mut octets = [0u32; 4];
    let mut parts = host.split('.');
    for octet in &mut octets {
        let p = parts.next()?;
        if p.is_empty() || p.len() > 3 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *octet = p.parse().ok()?;
    }
    parts.next().is_none().then_some(octets)
}

fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{000B}' | '\u{000C}' | '\r' | ' ' | '\u{00A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

const RATE_LIMIT_ATTEMPTS: u32 = 5;
const RATE_LIMIT_BASE_MS: u64 = 2000;
const MAX_SLEEP: Duration = Duration::from_secs(30);

pub enum RetryDecision<T> {
    Done(T),
    Retry {
        after: Option<Duration>,
        rate_limited: bool,
    },
}

/// Rate limiting gets its own, longer ladder and its own attempt budget: a
/// 429 means the upstream is fine and we are early, which deserves more
/// patience than a transient failure. `Ok(None)` means the budget ran out;
/// `Err` only ever comes from `op` itself.
pub async fn retry_with_backoff<T, F, Fut>(
    label: &str,
    attempts: u32,
    base_ms: u64,
    mut op: F,
) -> Result<Option<T>>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<RetryDecision<T>>>,
{
    let max_attempts = attempts.max(RATE_LIMIT_ATTEMPTS);
    let mut backoff_ms = base_ms;
    let mut rl_backoff_ms = RATE_LIMIT_BASE_MS;
    let mut plain_failures = 0u32;
    for attempt in 1..=max_attempts {
        match op().await? {
            RetryDecision::Done(v) => return Ok(Some(v)),
            RetryDecision::Retry {
                after,
                rate_limited,
            } => {
                if !rate_limited {
                    plain_failures += 1;
                    if plain_failures >= attempts {
                        return Ok(None);
                    }
                }
                if attempt == max_attempts {
                    return Ok(None);
                }
                let ladder = if rate_limited {
                    let d = Duration::from_millis(rl_backoff_ms);
                    rl_backoff_ms = rl_backoff_ms.saturating_mul(2);
                    d
                } else {
                    let d = Duration::from_millis(backoff_ms);
                    backoff_ms = backoff_ms.saturating_mul(2);
                    d
                };
                let wait = after.unwrap_or(ladder).min(MAX_SLEEP);
                tracing::warn!(
                    retry = label,
                    attempt,
                    of = if rate_limited { max_attempts } else { attempts },
                    rate_limited,
                    wait_ms = wait.as_millis() as u64,
                    "retrying after backoff"
                );
                tokio::time::sleep(wait).await;
            }
        }
    }
    Ok(None)
}

/// Parses a `Retry-After` header, in either the delta-seconds or the
/// HTTP-date form.
///
/// An unparseable value still yields a short wait rather than `None`: the
/// header's presence is itself the signal to slow down.
pub fn parse_retry_after(header: &str) -> Option<Duration> {
    let trimmed = header.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(secs) = trimmed.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    if let Ok(date) = chrono::DateTime::parse_from_rfc2822(trimmed) {
        let delta = date.with_timezone(&chrono::Utc) - chrono::Utc::now();
        return Some(delta.to_std().unwrap_or(Duration::ZERO));
    }
    Some(Duration::from_secs(5))
}

pub fn is_transient_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// How long a pinned address is reused before the host is resolved again.
const PIN_TTL: Duration = Duration::from_secs(60);
const PIN_CACHE_MAX: usize = 256;

/// The handle every upstream reader holds (the mirrors, the explorer-boot
/// readers, the lambdas' subgraph/RPC/trades readers). One shared pooled
/// client underneath ([`Outbound::raw`], which peer sync keeps using), plus
/// the guard that the string check alone cannot give: the host of every
/// request is resolved through [`resolve_and_pin`] and the connection is
/// opened to that address (a per-host client built with
/// `ClientBuilder::resolve`, cached for [`PIN_TTL`]), so a public name that
/// answers with a private address, or answers differently a moment later, is
/// never dialled. Redirects are re-issued by hand through the same guard
/// (`max_redirects` hops), never followed blindly.
///
/// Profiles for trusted provider configuration and tests may skip the guard
/// and pinning. They use the direct client and manual redirect handling so
/// the selected host policy applies to the entire redirect chain.
#[derive(Clone)]
pub struct Outbound {
    cfg: Arc<HttpClientCfg>,
    shared: reqwest::Client,
    /// Redirects off: the manual loop in [`Outbound::execute`] follows them.
    direct: reqwest::Client,
    pinned: Arc<Mutex<HashMap<String, Pinned>>>,
    max_redirects: usize,
    allow_private: bool,
    #[cfg(test)]
    test_pins: Arc<Mutex<HashMap<String, SocketAddr>>>,
}

struct Pinned {
    addr: SocketAddr,
    resolved_at: Instant,
    client: reqwest::Client,
}

#[derive(Debug, thiserror::Error)]
pub enum SendError {
    /// The guard refused the target before any byte left the process.
    #[error("{0}")]
    Refused(String),
    /// Transport or protocol failure; the URL is stripped from the text
    /// (`reqwest::Error::without_url`) so a gateway key in a configured
    /// upstream path never reaches a log line or a status route.
    #[error("{0}")]
    Http(reqwest::Error),
}

impl From<reqwest::Error> for SendError {
    fn from(e: reqwest::Error) -> Self {
        Self::Http(e.without_url())
    }
}

impl SendError {
    pub fn is_connect(&self) -> bool {
        matches!(self, Self::Http(e) if e.is_connect())
    }

    pub fn is_timeout(&self) -> bool {
        matches!(self, Self::Http(e) if e.is_timeout())
    }

    pub fn is_request(&self) -> bool {
        matches!(self, Self::Http(e) if e.is_request())
    }

    /// True for what a retry ladder should retry: a refused or unresolvable
    /// target is not one of them.
    pub fn is_transient(&self) -> bool {
        self.is_connect() || self.is_timeout() || self.is_request()
    }
}

impl std::fmt::Debug for Outbound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Outbound")
            .field("max_redirects", &self.max_redirects)
            .field("allow_private", &self.allow_private)
            .finish()
    }
}

impl Outbound {
    pub fn new(cfg: HttpClientCfg) -> reqwest::Result<Self> {
        let shared = cfg.build()?;
        let direct = HttpClientCfg {
            max_redirects: None,
            ..cfg.clone()
        }
        .build()?;
        Ok(Self {
            max_redirects: cfg.max_redirects.unwrap_or(0),
            cfg: Arc::new(cfg),
            shared,
            direct,
            pinned: Arc::new(Mutex::new(HashMap::new())),
            allow_private: false,
            #[cfg(test)]
            test_pins: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// A fixed provider profile using the same pool settings and host policy.
    /// The adapter supplies its SDK deadline separately. Construct once and
    /// retain it: this creates an independent HTTP transport pool.
    pub fn provider_profile(
        &self,
        connect_timeout: Option<Duration>,
        read_timeout: Option<Duration>,
    ) -> reqwest::Result<Self> {
        let mut profile = Self::new(HttpClientCfg {
            total_timeout: None,
            connect_timeout,
            read_timeout,
            ..(*self.cfg).clone()
        })?;
        profile.allow_private = self.allow_private;
        Ok(profile)
    }

    /// Tests only: `host` resolves to `addr` without DNS, so a loopback stub
    /// can stand in for a public name and the pinning path runs for real.
    #[cfg(test)]
    fn with_test_pin(self, host: &str, addr: SocketAddr) -> Self {
        self.test_pins
            .lock()
            .unwrap()
            .insert(host.to_ascii_lowercase(), addr);
        self
    }

    /// Default limits and the loopback admitted: for tests with a stub upstream.
    pub fn for_tests() -> Self {
        Self::new(HttpClientCfg::default())
            .expect("the default client builds")
            .allowing_private_hosts()
    }

    /// Use only for a provider endpoint supplied by trusted operator
    /// configuration, including its redirect targets. This profile permits
    /// private hosts and ordinary DNS, like provider SDKs. Never use it for a
    /// URL read from an HTTP request or another untrusted document.
    pub fn allowing_configured_provider_hosts(mut self) -> Self {
        self.allow_private = true;
        self
    }

    /// Tests only: send to private hosts through the direct client, unpinned.
    pub fn allowing_private_hosts(mut self) -> Self {
        self.allow_private = true;
        self
    }

    pub fn allows_private_hosts(&self) -> bool {
        self.allow_private
    }

    /// The shared pooled client, unguarded: for peer sync, whose sources are
    /// operator-configured content servers (a development peer may be on the
    /// loopback), and the RPC validator.
    pub fn raw(&self) -> &reqwest::Client {
        &self.shared
    }

    pub fn request(&self, method: reqwest::Method, url: impl reqwest::IntoUrl) -> GuardedRequest {
        GuardedRequest {
            outbound: self.clone(),
            builder: self.shared.request(method, url),
            max_redirects: None,
            fetch_redirects: false,
        }
    }

    pub fn get(&self, url: impl reqwest::IntoUrl) -> GuardedRequest {
        self.request(reqwest::Method::GET, url)
    }

    pub fn post(&self, url: impl reqwest::IntoUrl) -> GuardedRequest {
        self.request(reqwest::Method::POST, url)
    }

    /// Sends `req` through the guard, following up to `max_redirects` hops
    /// with the guard re-run on each. A body that cannot be cloned (a
    /// stream) is sent once and its redirect, if any, returned as is.
    pub async fn execute(&self, req: reqwest::Request) -> Result<reqwest::Response, SendError> {
        self.execute_with_redirect_limit(req, self.max_redirects, false)
            .await
    }

    async fn execute_with_redirect_limit(
        &self,
        mut req: reqwest::Request,
        max_redirects: usize,
        fetch_redirects: bool,
    ) -> Result<reqwest::Response, SendError> {
        for hop in 0..=max_redirects {
            let client = if self.allow_private {
                self.direct.clone()
            } else {
                self.client_for(req.url()).await?
            };
            let resp = match req.try_clone() {
                Some(copy) => client.execute(copy).await?,
                None => return Ok(client.execute(req).await?),
            };
            let status = resp.status();
            if !status.is_redirection()
                || (fetch_redirects && !matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308))
            {
                return Ok(resp);
            }
            if hop == max_redirects {
                return if fetch_redirects && resp.headers().contains_key(reqwest::header::LOCATION)
                {
                    Err(SendError::Refused("redirect count exceeded".into()))
                } else {
                    Ok(resp)
                };
            }
            let Some(location) = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
            else {
                return Ok(resp);
            };
            let next = req.url().join(location).map_err(|e| {
                SendError::Refused(format!("redirect target {location:?} is not a URL: {e}"))
            })?;
            let method = req.method().clone();
            match status.as_u16() {
                303 if !fetch_redirects || method != reqwest::Method::HEAD => {
                    *req.method_mut() = reqwest::Method::GET;
                    *req.body_mut() = None;
                }
                301 | 302 if method == reqwest::Method::POST => {
                    *req.method_mut() = reqwest::Method::GET;
                    *req.body_mut() = None;
                }
                301 | 302 | 303 | 307 | 308 => {}
                _ => return Ok(resp),
            }
            if fetch_redirects && req.method() != method {
                for name in [
                    "content-encoding",
                    "content-language",
                    "content-location",
                    "content-type",
                    "content-length",
                ] {
                    req.headers_mut().remove(name);
                }
            }
            if if fetch_redirects {
                next.origin() != req.url().origin()
            } else {
                next.host_str() != req.url().host_str()
            } {
                let headers = req.headers_mut();
                headers.remove(reqwest::header::AUTHORIZATION);
                headers.remove(reqwest::header::COOKIE);
            }
            tracing::debug!(from = %req.url(), to = %next, status = status.as_u16(), "following a redirect through the guard");
            *req.url_mut() = next;
        }
        unreachable!("the loop returns on its last hop")
    }

    /// The client to dial `url` with: the guard's verdict and the socket come
    /// from the same lookup. A literal public address needs no pin.
    async fn client_for(&self, url: &reqwest::Url) -> Result<reqwest::Client, SendError> {
        let Some(host) = url.host_str() else {
            return Err(SendError::Refused("URL has no host".into()));
        };
        if !matches!(url.scheme(), "http" | "https") {
            return Err(SendError::Refused(format!(
                "scheme {}:// is not http(s)",
                url.scheme()
            )));
        }
        if !is_safe_http_url(url.as_str()) {
            return Err(SendError::Refused(format!(
                "{host} refused by the SSRF guard (private, internal or single-label host)"
            )));
        }
        let bare = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        if bare.parse::<IpAddr>().is_ok() {
            return Ok(self.direct.clone());
        }
        let key = host.to_ascii_lowercase();
        if let Some(p) = self.pinned.lock().unwrap().get(&key)
            && p.resolved_at.elapsed() < PIN_TTL
        {
            return Ok(p.client.clone());
        }
        #[cfg(test)]
        let test_pin = self.test_pins.lock().unwrap().get(&key).copied();
        #[cfg(not(test))]
        let test_pin: Option<SocketAddr> = None;
        let addr = match test_pin {
            Some(addr) => addr,
            None => resolve_and_pin(url).await.ok_or_else(|| {
                SendError::Refused(format!(
                    "{host} does not resolve to a public address (or does not resolve)"
                ))
            })?,
        };
        let mut pinned = self.pinned.lock().unwrap();
        if let Some(p) = pinned.get_mut(&key)
            && p.addr.ip() == addr.ip()
        {
            p.resolved_at = Instant::now();
            return Ok(p.client.clone());
        }
        let client = HttpClientCfg {
            max_redirects: None,
            ..(*self.cfg).clone()
        }
        .builder()
        .resolve(host, addr)
        .build()?;
        if pinned.len() >= PIN_CACHE_MAX {
            pinned.retain(|_, p| p.resolved_at.elapsed() < PIN_TTL);
            if pinned.len() >= PIN_CACHE_MAX
                && let Some(oldest) = pinned
                    .iter()
                    .min_by_key(|(_, p)| p.resolved_at)
                    .map(|(k, _)| k.clone())
            {
                pinned.remove(&oldest);
            }
        }
        tracing::debug!(host, %addr, "pinned an upstream host");
        pinned.insert(
            key,
            Pinned {
                addr,
                resolved_at: Instant::now(),
                client: client.clone(),
            },
        );
        Ok(client)
    }

    #[cfg(test)]
    fn pinned_hosts(&self) -> Vec<String> {
        let mut hosts: Vec<String> = self.pinned.lock().unwrap().keys().cloned().collect();
        hosts.sort();
        hosts
    }
}

/// A request under construction against an [`Outbound`]: the builder
/// methods the readers use, then [`GuardedRequest::send`], which is the only
/// way to put it on the wire.
pub struct GuardedRequest {
    outbound: Outbound,
    builder: reqwest::RequestBuilder,
    max_redirects: Option<usize>,
    fetch_redirects: bool,
}

impl GuardedRequest {
    fn map(mut self, f: impl FnOnce(reqwest::RequestBuilder) -> reqwest::RequestBuilder) -> Self {
        self.builder = f(self.builder);
        self
    }

    pub fn header<K, V>(self, key: K, value: V) -> Self
    where
        reqwest::header::HeaderName: TryFrom<K>,
        <reqwest::header::HeaderName as TryFrom<K>>::Error: Into<axum::http::Error>,
        reqwest::header::HeaderValue: TryFrom<V>,
        <reqwest::header::HeaderValue as TryFrom<V>>::Error: Into<axum::http::Error>,
    {
        self.map(|b| b.header(key, value))
    }

    pub fn headers(self, headers: reqwest::header::HeaderMap) -> Self {
        self.map(|b| b.headers(headers))
    }

    pub fn timeout(self, timeout: Duration) -> Self {
        self.map(|b| b.timeout(timeout))
    }

    /// Restricts this request's manual redirect loop without changing its
    /// host guard, pinned clients or the defaults of other requests.
    pub fn max_redirects(mut self, maximum: usize) -> Self {
        self.max_redirects = Some(maximum.min(self.outbound.max_redirects));
        self
    }

    /// Opts this request into Fetch redirect semantics and its own hop limit.
    /// Every hop retains the existing host guard and shared pinned clients.
    pub fn following_redirects(mut self, maximum: usize) -> Self {
        self.max_redirects = Some(maximum);
        self.fetch_redirects = true;
        self
    }

    pub fn json<T: serde::Serialize + ?Sized>(self, json: &T) -> Self {
        self.map(|b| b.json(json))
    }

    pub fn body<T: Into<reqwest::Body>>(self, body: T) -> Self {
        self.map(|b| b.body(body))
    }

    pub async fn send(self) -> Result<reqwest::Response, SendError> {
        let req = self.builder.build()?;
        self.outbound
            .execute_with_redirect_limit(
                req,
                self.max_redirects.unwrap_or(self.outbound.max_redirects),
                self.fetch_redirects,
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_agent_is_the_package_version() {
        assert_eq!(
            USER_AGENT,
            format!("catalyst/{}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[tokio::test]
    async fn reserved_names_never_reach_the_system_resolver() {
        use reqwest::dns::Resolve;
        let asked = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = asked.clone();
        let resolver = ReservedNamesResolver::over(move |host| {
            seen.lock().unwrap().push(host);
            Box::pin(async {
                Ok(
                    Box::new(std::iter::once(SocketAddr::from(([192, 0, 2, 1], 0))))
                        as reqwest::dns::Addrs,
                )
            })
        });
        for host in ["fixtures.invalid", "Peer.Example.INVALID.", "invalid"] {
            let answer = resolver.resolve(host.parse().unwrap()).await;
            assert!(answer.is_err(), "{host} resolved");
        }
        for host in ["localhost", "catalyst.localhost", "Node.LOCALHOST."] {
            let addrs: Vec<SocketAddr> = resolver
                .resolve(host.parse().unwrap())
                .await
                .unwrap()
                .collect();
            assert!(
                !addrs.is_empty() && addrs.iter().all(|a| a.ip().is_loopback()),
                "{host}: {addrs:?}"
            );
        }
        assert!(
            asked.lock().unwrap().is_empty(),
            "asked the system resolver for {:?}",
            asked.lock().unwrap()
        );
        let addrs: Vec<SocketAddr> = resolver
            .resolve("peer.example.org".parse().unwrap())
            .await
            .unwrap()
            .collect();
        assert_eq!(addrs, [SocketAddr::from(([192, 0, 2, 1], 0))]);
        assert_eq!(*asked.lock().unwrap(), ["peer.example.org"]);
    }

    #[tokio::test]
    async fn every_built_client_answers_reserved_names_itself() {
        let client = HttpClientCfg::default()
            .with_total_timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let err = client
            .get("http://catalyst-test.invalid/")
            .send()
            .await
            .unwrap_err();
        let mut chain = Vec::new();
        let mut source: Option<&dyn std::error::Error> = Some(&err);
        while let Some(e) = source {
            chain.push(e.to_string());
            source = e.source();
        }
        assert!(chain.iter().any(|e| e.contains("RFC 6761")), "{chain:?}");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n")
                .await
                .unwrap();
        });
        let resp = client
            .get(format!("http://catalyst-test.localhost:{port}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 204);
    }

    #[tokio::test]
    async fn the_guard_pins_no_reserved_name() {
        for url in [
            "http://catalyst-test.invalid/",
            "https://catalyst-test.localhost/",
        ] {
            let parsed = reqwest::Url::parse(url).unwrap();
            assert!(resolve_and_pin(&parsed).await.is_none(), "{url}");
        }
    }

    #[test]
    fn reserved_name_reads_rfc_6761_suffixes_only() {
        assert_eq!(reserved_name("a.invalid"), Some(ReservedName::Invalid));
        assert_eq!(reserved_name("A.INVALID."), Some(ReservedName::Invalid));
        assert_eq!(reserved_name("localhost"), Some(ReservedName::Loopback));
        assert_eq!(reserved_name("x.localhost."), Some(ReservedName::Loopback));
        for public in [
            "invalid.example.org",
            "notinvalid",
            "localhost.example.org",
            "mylocalhost",
        ] {
            assert_eq!(reserved_name(public), None, "{public}");
        }
    }

    #[test]
    fn defaults_do_not_follow_redirects() {
        assert!(HttpClientCfg::default().max_redirects.is_none());
    }

    #[test]
    fn builders_produce_a_client() {
        assert!(try_http_client(&HttpClientCfg::default()).is_ok());
        let cfg = HttpClientCfg::default()
            .with_total_timeout(Duration::from_secs(5))
            .with_connect_timeout(Duration::from_secs(2))
            .with_pool(Duration::from_secs(30), 4)
            .following_redirects(3)
            .with_user_agent("catalyst-test/1");
        assert_eq!(cfg.max_redirects, Some(3));
        assert!(try_http_client(&cfg).is_ok());
        let _ = http_client("test", &cfg);
    }

    #[test]
    fn the_serve_client_keeps_the_old_sync_limits() {
        let cfg = HttpClientCfg::serve(25);
        assert_eq!(
            cfg.total_timeout, None,
            "snapshot downloads have no whole-request budget"
        );
        assert_eq!(cfg.connect_timeout, Some(Duration::from_secs(8)));
        assert_eq!(cfg.read_timeout, Some(Duration::from_secs(25)));
        assert_eq!(cfg.pool_max_idle_per_host, 25);
        assert_eq!(cfg.max_redirects, Some(2));
        assert!(cfg.tcp_nodelay);
        assert!(cfg.build().is_ok());
    }

    #[test]
    fn reserved_ranges_and_internal_names_are_blocked() {
        for host in [
            "0.0.0.0",
            "127.0.0.1",
            "10.1.2.3",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.0.1",
            "100.64.0.1",
            "100.127.255.255",
            "255.255.255.255",
            "224.0.0.1",
            "192.0.2.1",
            "::1",
            "::",
            "fe80::1",
            "fd00::1",
            "fc00::1",
            "[::1]",
            "[::ffff:127.0.0.1]",
            "64:ff9b::a9fe:a9fe",
            "[64:ff9b::7f00:1]",
            "64:ff9b:1::808:808",
            "2002:a9fe:a9fe::1",
            "2002:7f00:1::",
            "::a9fe:a9fe",
            "::10.0.0.1",
            "fec0::1",
            "feff::1",
            "198.18.0.1",
            "198.19.255.255",
            "240.0.0.1",
            "250.1.2.3",
            "localhost",
            "router",
            "svc.internal",
            "office.intranet",
            "box.home",
            "vpn.corp",
            "gw.home.arpa",
            "app.localhost",
            "printer.lan",
            "nas.local",
            "localhost.",
            "nas.local.",
            "router.",
            "127.0.0.1.",
            "LOCALHOST",
            "NAS.Local",
        ] {
            assert!(is_private_or_internal_host(host), "{host} must be blocked");
        }
    }

    #[test]
    fn public_hosts_are_allowed() {
        for host in [
            "8.8.8.8",
            "example.com",
            "example.com.",
            "decentraland.org..",
            "172.15.0.1",
            "172.32.0.1",
            "100.63.0.1",
            "100.128.0.1",
            "events.decentraland.org",
            "2600::1",
            "[2600::1]",
            "64:ff9b::808:808",
            "2002:808:808::1",
            "::8.8.8.8",
            "198.17.0.1",
            "198.20.0.1",
            "223.1.1.1",
        ] {
            assert!(
                !is_private_or_internal_host(host),
                "{host} must be reachable"
            );
        }
    }

    #[test]
    fn obfuscated_ip_literals_are_blocked() {
        for host in [
            "2130706433",
            "0x7f000001",
            "0177.0.0.1",
            "127.1",
            "0x7f.1",
            "017700000001",
        ] {
            assert!(
                is_private_or_internal_host(host),
                "{host} must resolve to loopback and be blocked"
            );
        }
    }

    #[test]
    fn safe_urls_pass() {
        for url in [
            "https://decentraland.org/",
            "http://example.com/path?q=1",
            "https://example.com./",
            "https://8.8.8.8/",
            "https://[2600::1]/",
        ] {
            assert!(is_safe_http_url(url), "{url} must be safe");
        }
    }

    #[test]
    fn unsafe_schemes_and_hosts_are_rejected() {
        for url in [
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            "data:text/html,<script>alert(1)</script>",
            "file:///etc/passwd",
            "smb://attacker/share",
            "ftp://example.com/",
            "//example.com/",
            "example.com",
            "",
            "http://169.254.169.254/latest/meta-data/",
            "http://2130706433/",
            "http://0x7f000001/",
            "http://0177.0.0.1/",
            "http://192.168.1.1/",
            "http://localhost:8080/",
            "http://router/",
            "http://printer.lan/",
            "http://nas.local/",
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://localhost./",
            "http://nas.local./",
            "http://router./",
            "http://10.0.0.1/",
            "http://100.100.0.1/",
        ] {
            assert!(!is_safe_http_url(url), "{url} must be rejected");
        }
    }

    #[test]
    fn whitespace_smuggling_is_rejected() {
        for url in [
            "https://example.com/ javascript:alert(1)",
            "https://exa\tmple.com/",
            "https://example.com/\nhttp://127.0.0.1/",
            "https://example.com/\u{2028}x",
            "java\nscript:alert(1)",
        ] {
            assert!(!is_safe_http_url(url), "{url} must be rejected");
        }
    }

    #[test]
    fn surrounding_whitespace_is_trimmed_not_rejected() {
        assert!(is_safe_http_url("  https://decentraland.org/  "));
        assert!(is_safe_http_url("\n\thttps://decentraland.org/\r\n"));
    }

    #[tokio::test]
    async fn pinning_rejects_literal_private_addresses() {
        for url in [
            "http://127.0.0.1/x",
            "http://169.254.169.254/latest/",
            "http://[::1]/x",
            "http://[::ffff:10.0.0.1]/x",
            "http://router/x",
            "http://svc.internal/x",
        ] {
            let parsed = reqwest::Url::parse(url).unwrap();
            assert!(
                resolve_and_pin(&parsed).await.is_none(),
                "{url} must not be pinned"
            );
        }
    }

    #[tokio::test]
    async fn pinning_accepts_a_public_literal_and_keeps_the_port() {
        let parsed = reqwest::Url::parse("http://8.8.8.8:8080/x").unwrap();
        let pinned = resolve_and_pin(&parsed).await.expect("public literal pins");
        assert_eq!(pinned, "8.8.8.8:8080".parse::<SocketAddr>().unwrap());

        let parsed = reqwest::Url::parse("https://8.8.4.4/x").unwrap();
        let pinned = resolve_and_pin(&parsed).await.expect("public literal pins");
        assert_eq!(pinned.port(), 443, "https must pin the default port");
    }

    #[tokio::test]
    async fn pinning_accepts_a_public_ipv6_literal() {
        let parsed = reqwest::Url::parse("http://[2600::1]:81/x").unwrap();
        let pinned = resolve_and_pin(&parsed).await.expect("public literal pins");
        assert_eq!(pinned, "[2600::1]:81".parse::<SocketAddr>().unwrap());
    }

    /// A loopback stub standing in for public names: `/ok` answers, the
    /// `/to-*` routes redirect (to a literal loopback address, to another
    /// public name, to itself).
    async fn redirect_stub() -> SocketAddr {
        use axum::response::IntoResponse;
        use axum::routing::get;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let port = addr.port();
        let redirect = |to: String| async move {
            (axum::http::StatusCode::FOUND, [("location", to)]).into_response()
        };
        let app = axum::Router::new()
            .route("/ok", get(|| async { "fine" }))
            .route(
                "/to-private",
                get(move || redirect(format!("http://127.0.0.1:{port}/ok"))),
            )
            .route(
                "/to-public",
                get(move || redirect(format!("http://other.example.test:{port}/ok"))),
            )
            .route(
                "/to-self",
                get(move || redirect(format!("http://public.example.test:{port}/to-self"))),
            )
            .route(
                "/to-ftp",
                get(move || redirect("ftp://public.example.test/x".to_string())),
            );
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    }

    #[tokio::test]
    async fn configured_provider_redirects_permit_private_targets_without_changing_the_shared_guard()
     {
        use axum::{Router, routing::get};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let location = format!("http://localhost:{}/answer", address.port());
        let app = Router::new()
            .route(
                "/start",
                get(move || {
                    let location = location.clone();
                    async move { (axum::http::StatusCode::FOUND, [("location", location)]) }
                }),
            )
            .route("/answer", get(|| async { "provider answer" }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let cfg = HttpClientCfg::default().following_redirects(2);
        let mut profile = Outbound::new(cfg.clone())
            .unwrap()
            .allowing_configured_provider_hosts();
        // Resolve the public fixture name locally without global DNS or hosts.
        // Both clients retain their production redirect policies.
        profile.shared = cfg
            .builder()
            .resolve("provider.example", address)
            .build()
            .unwrap();
        profile.direct = HttpClientCfg {
            max_redirects: None,
            ..cfg
        }
        .builder()
        .resolve("provider.example", address)
        .build()
        .unwrap();
        let answer = profile
            .get(format!("http://provider.example:{}/start", address.port()))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(answer, "provider answer");
        server.abort();
    }

    #[tokio::test]
    async fn a_provider_profile_leaves_the_shared_read_deadline_and_guard_intact() {
        use axum::{Router, routing::get};
        let (entered, mut entries) = tokio::sync::mpsc::unbounded_channel();
        let release = Arc::new(tokio::sync::Notify::new());
        let server_release = release.clone();
        let app = Router::new().route(
            "/headers",
            get(move || {
                let entered = entered.clone();
                let release = server_release.clone();
                async move {
                    entered.send(()).unwrap();
                    release.notified().await;
                    "ok"
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/headers", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let guarded = Outbound::new(HttpClientCfg::serve(7)).unwrap();
        assert!(matches!(
            guarded.get(&url).send().await,
            Err(SendError::Refused(_))
        ));
        let profile = guarded
            .provider_profile(None, None)
            .unwrap()
            .allowing_configured_provider_hosts();
        assert_eq!(profile.cfg.pool_max_idle_per_host, 7);
        assert!(profile.cfg.connect_timeout.is_none());
        let provider_url = url.clone();
        let provider = tokio::spawn(async move { profile.get(provider_url).send().await });
        let shared = guarded.clone().allowing_private_hosts();
        let shared_url = url.clone();
        let reader = tokio::spawn(async move { shared.get(shared_url).send().await });
        entries.recv().await.unwrap();
        entries.recv().await.unwrap();
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(26)).await;
        for _ in 0..128 {
            tokio::task::yield_now().await;
        }
        assert!(
            reader.is_finished(),
            "ordinary readers keep their 25s inactivity deadline"
        );
        assert!(
            !provider.is_finished(),
            "provider timeout policy belongs to its adapter"
        );
        tokio::time::resume();
        assert!(reader.await.unwrap().unwrap_err().is_timeout());
        release.notify_waiters();
        assert!(provider.await.unwrap().unwrap().status().is_success());
        assert!(
            matches!(guarded.get(&url).send().await, Err(SendError::Refused(_))),
            "the shared host policy never changes"
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_request_redirect_limit_keeps_other_requests_defaults() {
        use axum::{Router, routing::get};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hits = Arc::new(AtomicUsize::new(0));
        let count = hits.clone();
        let app = Router::new()
            .route(
                "/jump",
                get(|| async { (axum::http::StatusCode::FOUND, [("location", "/answer")]) }),
            )
            .route(
                "/answer",
                get(move || {
                    let count = count.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        "ok"
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = Outbound::new(HttpClientCfg::default().following_redirects(2))
            .unwrap()
            .allowing_private_hosts();
        let url = format!("http://{address}/jump");
        let response = http.get(&url).max_redirects(0).send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "the override must not make a second request"
        );
        let response = http.get(&url).send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "the next request keeps the configured default"
        );
        server.abort();
    }

    #[tokio::test]
    async fn opt_in_fetch_redirects_match_the_executed_social_analytics_sdk() {
        use axum::{Router, extract::Request, response::IntoResponse};
        let fixture: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("DCL_ONE_ROOT"),
            "/library/dcl-http/tests/fixtures/social-analytics-source-cases.json"
        )))
        .unwrap();
        for case in fixture["redirects"].as_array().unwrap() {
            let requests = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
            let observed = requests.clone();
            let app = Router::new().fallback(move |request: Request| {
                let observed = observed.clone(); async move {
                    let path = request.uri().path().to_string();
                    let method = request.method().to_string();
                    let headers = request.headers().clone();
                    let bytes = axum::body::to_bytes(request.into_body(), 1 << 20).await.unwrap();
                    let header = |name: &str| headers.get(name).map(|value| value.to_str().unwrap().to_string());
                    observed.lock().unwrap().push(serde_json::json!({"path":path,"method":method,"content_type":header("content-type"),"content_length":header("content-length"),"token":header("x-token"),"body":String::from_utf8(bytes.to_vec()).unwrap()}));
                    let parts: Vec<_> = path.split('/').collect();
                    let status: u16 = parts[2].parse().unwrap();
                    let remaining: usize = parts[3].parse().unwrap();
                    if remaining == 0 { return "unused response body".into_response(); }
                    (axum::http::StatusCode::from_u16(status).unwrap(), [("location", format!("/jump/{status}/{}", remaining-1))]).into_response()
                }
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            // Defaults remain two hops; this one request opts into Fetch's 20.
            let http = Outbound::new(HttpClientCfg::default().following_redirects(2))
                .unwrap()
                .allowing_private_hosts();
            let first = &case["requests"][0];
            let result = http
                .post(format!(
                    "http://{address}{}",
                    first["path"].as_str().unwrap()
                ))
                .header("x-token", "fixture-token")
                .header("content-type", "application/json")
                .body(first["body"].as_str().unwrap().to_string())
                .following_redirects(20)
                .send()
                .await;
            assert_eq!(
                result.is_err(),
                !case["logs"].as_array().unwrap().is_empty(),
                "status={} hops={}",
                case["status"],
                case["hops"]
            );
            drop(result);
            assert_eq!(
                *requests.lock().unwrap(),
                *case["requests"].as_array().unwrap(),
                "status={} hops={}",
                case["status"],
                case["hops"]
            );
            server.abort();
        }
    }

    #[tokio::test]
    async fn opt_in_fetch_redirects_preserve_source_analytics_headers_across_public_hosts() {
        use axum::{Router, extract::Request, response::IntoResponse};
        let fixture: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("DCL_ONE_ROOT"),
            "/library/dcl-http/tests/fixtures/social-analytics-source-cases.json"
        )))
        .unwrap();
        let expected = &fixture["cross_host"];
        let requests = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let observed = requests.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().fallback(move |request: Request| {
            let observed = observed.clone(); async move {
                let path = request.uri().path().to_string();
                let method = request.method().to_string();
                let headers = request.headers().clone();
                let bytes = axum::body::to_bytes(request.into_body(), 1 << 20).await.unwrap();
                let header = |name: &str| headers.get(name).map(|value| value.to_str().unwrap().to_string());
                observed.lock().unwrap().push(serde_json::json!({"path":path,"method":method,"content_type":header("content-type"),"content_length":header("content-length"),"token":header("x-token"),"body":String::from_utf8(bytes.to_vec()).unwrap()}));
                if path.ends_with("/1") {
                    (axum::http::StatusCode::TEMPORARY_REDIRECT, [("location", format!("http://other.example.test:{}/jump/307/0",address.port()))]).into_response()
                } else { "unused".into_response() }
            }
        });
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let http = strict(address);
        let response = http
            .post(format!(
                "http://public.example.test:{}/jump/307/1",
                address.port()
            ))
            .header("x-token", "fixture-token")
            .header("content-type", "application/json")
            .body(expected["requests"][0]["body"].as_str().unwrap().to_owned())
            .following_redirects(20)
            .send()
            .await
            .unwrap();
        drop(response);
        assert_eq!(
            *requests.lock().unwrap(),
            *expected["requests"].as_array().unwrap()
        );
        assert_eq!(
            http.pinned_hosts(),
            vec!["other.example.test", "public.example.test"]
        );
        server.abort();
    }

    fn strict(addr: SocketAddr) -> Outbound {
        Outbound::new(HttpClientCfg::default().following_redirects(2))
            .unwrap()
            .with_test_pin("public.example.test", addr)
            .with_test_pin("other.example.test", addr)
    }

    #[tokio::test]
    async fn the_guard_refuses_private_hosts_before_any_byte_leaves() {
        let out = Outbound::new(HttpClientCfg::default()).unwrap();
        for url in [
            "http://127.0.0.1:9/x",
            "http://router/x",
            "https://169.254.169.254/latest/",
            "ftp://example.com/",
        ] {
            let err = out.get(url).send().await.unwrap_err();
            assert!(matches!(err, SendError::Refused(_)), "{url}: {err}");
            assert!(!err.is_transient(), "a refusal is never retried");
        }
        assert!(out.pinned_hosts().is_empty());
    }

    #[tokio::test]
    async fn a_pinned_public_name_is_dialled_at_its_pinned_address() {
        let addr = redirect_stub().await;
        let out = strict(addr);
        let resp = out
            .get(format!("http://public.example.test:{}/ok", addr.port()))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "fine");
        assert_eq!(out.pinned_hosts(), vec!["public.example.test"]);
        // The second call reuses the pinned client within the TTL.
        out.get(format!("http://public.example.test:{}/ok", addr.port()))
            .send()
            .await
            .unwrap();
        assert_eq!(out.pinned_hosts(), vec!["public.example.test"]);
    }

    #[tokio::test]
    async fn a_redirect_to_a_private_address_is_refused_and_not_followed() {
        let addr = redirect_stub().await;
        let out = strict(addr);
        let err = out
            .get(format!(
                "http://public.example.test:{}/to-private",
                addr.port()
            ))
            .send()
            .await
            .unwrap_err();
        assert!(
            matches!(&err, SendError::Refused(m) if m.contains("127.0.0.1")),
            "{err}"
        );
        let err = out
            .get(format!("http://public.example.test:{}/to-ftp", addr.port()))
            .send()
            .await
            .unwrap_err();
        assert!(matches!(err, SendError::Refused(_)), "{err}");
    }

    #[tokio::test]
    async fn a_redirect_to_another_public_name_is_followed_through_the_guard() {
        let addr = redirect_stub().await;
        let out = strict(addr);
        let resp = out
            .get(format!(
                "http://public.example.test:{}/to-public",
                addr.port()
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "fine");
        assert_eq!(
            out.pinned_hosts(),
            vec!["other.example.test", "public.example.test"],
            "both hops were pinned"
        );
    }

    #[tokio::test]
    async fn redirects_stop_at_the_hop_limit_with_the_last_answer() {
        let addr = redirect_stub().await;
        let out = strict(addr);
        let resp = out
            .get(format!(
                "http://public.example.test:{}/to-self",
                addr.port()
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            302,
            "the 3xx comes back for the caller to treat as a failure"
        );
    }

    #[tokio::test]
    async fn the_shared_client_refuses_a_public_to_private_redirect() {
        let addr = redirect_stub().await;
        let client = HttpClientCfg::default()
            .following_redirects(2)
            .builder()
            .resolve("public.example.test", addr)
            .build()
            .unwrap();
        let err = client
            .get(format!(
                "http://public.example.test:{}/to-private",
                addr.port()
            ))
            .send()
            .await
            .unwrap_err();
        assert!(err.is_redirect(), "{err}");
        // A chain that started on the loopback keeps following the loopback.
        let resp = client
            .get(format!("http://127.0.0.1:{}/to-private", addr.port()))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[tokio::test]
    async fn for_tests_sends_to_the_loopback_unpinned() {
        let addr = redirect_stub().await;
        let resp = Outbound::for_tests()
            .get(format!("http://{addr}/ok"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    #[test]
    fn parse_retry_after_integer_seconds() {
        assert_eq!(parse_retry_after("3"), Some(Duration::from_secs(3)));
        assert_eq!(parse_retry_after("0"), Some(Duration::from_secs(0)));
    }

    #[test]
    fn parse_retry_after_http_date_in_future() {
        let future = (chrono::Utc::now() + chrono::Duration::seconds(10)).to_rfc2822();
        let d = parse_retry_after(&future).expect("future date should parse");
        assert!(
            d > Duration::from_secs(8) && d <= Duration::from_secs(10),
            "expected ~10s, got {d:?}"
        );
    }

    #[test]
    fn parse_retry_after_http_date_in_past() {
        let past = (chrono::Utc::now() - chrono::Duration::seconds(60)).to_rfc2822();
        assert_eq!(parse_retry_after(&past), Some(Duration::ZERO));
    }

    #[test]
    fn parse_retry_after_garbage_falls_back() {
        assert_eq!(parse_retry_after("garbage"), Some(Duration::from_secs(5)));
    }

    #[test]
    fn parse_retry_after_empty_is_none() {
        assert_eq!(parse_retry_after(""), None);
        assert_eq!(parse_retry_after("   "), None);
    }

    #[test]
    fn transient_statuses() {
        assert!(is_transient_status(reqwest::StatusCode::TOO_MANY_REQUESTS));
        assert!(is_transient_status(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ));
        assert!(is_transient_status(reqwest::StatusCode::BAD_GATEWAY));
        assert!(!is_transient_status(reqwest::StatusCode::NOT_FOUND));
        assert!(!is_transient_status(reqwest::StatusCode::OK));
    }

    async fn drive_schedule(
        attempts: u32,
        base_ms: u64,
        after: Option<Duration>,
        rate_limited: bool,
    ) -> Vec<Duration> {
        let times: Mutex<Vec<Duration>> = Mutex::new(Vec::new());
        let times_ref = &times;
        let start = tokio::time::Instant::now();
        let result: Option<()> =
            retry_with_backoff("test", attempts, base_ms, move || async move {
                times_ref.lock().unwrap().push(start.elapsed());
                Ok(RetryDecision::Retry {
                    after,
                    rate_limited,
                })
            })
            .await
            .unwrap();
        assert!(result.is_none());
        times.into_inner().unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limited_ladder_is_2_4_8_16_over_5_attempts() {
        let times = drive_schedule(3, 1000, None, true).await;
        let expected: Vec<Duration> = [0u64, 2, 6, 14, 30]
            .iter()
            .map(|s| Duration::from_secs(*s))
            .collect();
        assert_eq!(times, expected);
    }

    #[tokio::test(start_paused = true)]
    async fn plain_transient_keeps_caller_schedule() {
        let times = drive_schedule(3, 1000, None, false).await;
        let expected: Vec<Duration> = [0u64, 1, 3]
            .iter()
            .map(|s| Duration::from_secs(*s))
            .collect();
        assert_eq!(times, expected);
    }

    #[tokio::test(start_paused = true)]
    async fn rate_limited_honours_retry_after_hint() {
        let times = drive_schedule(3, 1000, Some(Duration::from_secs(7)), true).await;
        assert_eq!(times.len(), 5);
        assert_eq!(times[1], Duration::from_secs(7));
        assert_eq!(times[2], Duration::from_secs(14));
    }

    #[tokio::test(start_paused = true)]
    async fn sleeps_are_clamped_to_30s() {
        let times = drive_schedule(3, 1000, Some(Duration::from_secs(120)), true).await;
        assert_eq!(times[1], Duration::from_secs(30));
    }

    #[tokio::test(start_paused = true)]
    async fn done_short_circuits() {
        let result =
            retry_with_backoff("test", 3, 1000, || async { Ok(RetryDecision::Done(42u32)) })
                .await
                .unwrap();
        assert_eq!(result, Some(42));
    }

    #[tokio::test(start_paused = true)]
    async fn operation_errors_propagate() {
        let r: Result<Option<u32>> = retry_with_backoff("test", 3, 1000, || async {
            Err(anyhow::anyhow!("hard failure"))
        })
        .await;
        assert_eq!(r.unwrap_err().to_string(), "hard failure");
    }

    fn sized(body: &'static str) -> reqwest::Response {
        reqwest::Response::from(axum::http::Response::new(body.to_string()))
    }

    fn chunked(chunks: Vec<&'static str>) -> reqwest::Response {
        let stream = futures::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<_, std::io::Error>(bytes::Bytes::from_static(c.as_bytes()))),
        );
        reqwest::Response::from(axum::http::Response::new(reqwest::Body::wrap_stream(
            stream,
        )))
    }

    #[tokio::test]
    async fn advertised_length_over_the_cap_fails_before_reading() {
        let err = read_body_capped(sized("hello world"), 4)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("advertises 11 bytes"), "{err}");
    }

    #[tokio::test]
    async fn streamed_body_over_the_cap_fails_mid_read() {
        let err = read_body_capped(chunked(vec!["aaaa", "bbbb", "cccc"]), 6)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("exceeds the 6 byte cap"), "{err}");
    }

    #[tokio::test]
    async fn streamed_body_under_the_cap_is_concatenated() {
        let out = read_body_capped(chunked(vec!["aa", "bb", "cc"]), 6)
            .await
            .unwrap();
        assert_eq!(out, b"aabbcc");
    }

    #[tokio::test]
    async fn a_body_exactly_at_the_cap_is_accepted() {
        let out = read_body_capped(sized("hello"), 5).await.unwrap();
        assert_eq!(out, b"hello");
    }
}
