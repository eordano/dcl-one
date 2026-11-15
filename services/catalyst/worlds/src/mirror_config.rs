//! Worlds mirror config.
use super::*;

pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(6 * 3600);
pub const DEFAULT_CONCURRENCY: usize = 4;
pub const DEFAULT_BLOB_CONCURRENCY: usize = 8;

/// The validated `WORLDS_CONTENT_MIRROR_*` configuration.
#[derive(Clone, Debug)]
pub struct MirrorSettings {
    /// The upstream worlds content server, no trailing slash.
    pub source: String,
    /// Pause between passes; the v1 stack's timer ran every six hours.
    pub interval: Duration,
    /// Worlds mirrored at once (one batch; the cursor moves per batch).
    pub concurrency: usize,
    /// Files fetched at once per scene.
    pub blob_concurrency: usize,
    /// A bounded run: only the first N catalogue names (name order).
    pub limit: Option<usize>,
    /// Only these worlds (lowercased); empty means the whole catalogue.
    pub names: Vec<String>,
    /// This node's own origins: a source or a scene base URL that names one
    /// of them is a mirror loop.
    own_origins: Vec<String>,
    /// Tests only (`for_tests`): a loopback stub may stand in for the
    /// upstream. `parse` never sets it.
    allow_private_source: bool,
    /// `WORLDS_READ_THROUGH_TRUST_UPSTREAM_GRANTS`: a read-through fill also accepts a scene
    /// signed by a deployer the upstream lists as granted ([`Signatures::upstream_grant`]).
    pub trust_upstream_grants: bool,
    /// `WORLDS_CONTENT_MIRROR_ENABLED`: the periodic pass runs. Off, the source only serves
    /// the read-through ([`crate::WorldsState::fill_world`]): a world is mirrored when read.
    pub scheduled: bool,
}

fn origin_of(url: &url::Url) -> Option<String> {
    let host = url.host_str()?.to_ascii_lowercase();
    let port = url.port_or_known_default()?;
    Some(format!("{}://{host}:{port}", url.scheme()))
}

fn is_loopback_host(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

impl MirrorSettings {
    /// The mirror `serve` runs: the periodic pass with `WORLDS_CONTENT_MIRROR_ENABLED`, else
    /// the read-through alone when `READ_THROUGH_ENABLED` and a `WORLDS_CONTENT_MIRROR_SOURCE`
    /// are set (worlds are mirrored one at a time as they are read, none crawled); `None`
    /// otherwise.
    pub fn from_config(
        config: &crate::config::Config,
        own_urls: &[String],
    ) -> Result<Option<Self>> {
        let worlds = &config.worlds_content;
        let scheduled = worlds.worlds_content_mirror_enabled;
        if !scheduled
            && !(config.content.read_through_enabled
                && !worlds.worlds_content_mirror_source.trim().is_empty())
        {
            return Ok(None);
        }
        let mut settings = Self::parse(
            &worlds.worlds_content_mirror_source,
            own_urls,
            Duration::from_secs(worlds.worlds_content_mirror_interval_secs),
            worlds.worlds_content_mirror_concurrency,
            worlds.worlds_content_mirror_blob_concurrency,
            worlds.worlds_content_mirror_limit,
            &worlds.worlds_content_mirror_names,
        )
        .context("WORLDS_CONTENT_MIRROR_* configuration")?;
        settings.scheduled = scheduled;
        settings.trust_upstream_grants = worlds.worlds_read_through_trust_upstream_grants;
        Ok(Some(settings))
    }

    /// `WORLDS_CONTENT_MIRROR_SOURCE` must be an https:// URL to a publicly
    /// routable host (the SSRF guard of [`crate::http`]; plaintext would let
    /// a MITM plant worlds and files) and must not name this node
    /// (`own_urls`: PUBLIC_URL, CONTENT_URL, the bind address).
    #[allow(clippy::too_many_arguments)]
    pub fn parse(
        source: &str,
        own_urls: &[String],
        interval: Duration,
        concurrency: usize,
        blob_concurrency: usize,
        limit: usize,
        names: &str,
    ) -> Result<Self> {
        Self::parse_with(
            source,
            own_urls,
            interval,
            concurrency,
            blob_concurrency,
            limit,
            names,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn parse_with(
        source: &str,
        own_urls: &[String],
        interval: Duration,
        concurrency: usize,
        blob_concurrency: usize,
        limit: usize,
        names: &str,
        allow_private_source: bool,
    ) -> Result<Self> {
        let source = source.trim().trim_end_matches('/').to_string();
        if source.is_empty() {
            bail!(
                "WORLDS_CONTENT_MIRROR_SOURCE is empty: the mirror has nothing to do without an upstream"
            );
        }
        let parsed = url::Url::parse(&source).map_err(|e| {
            anyhow!("WORLDS_CONTENT_MIRROR_SOURCE {source:?} is not a valid URL: {e}")
        })?;
        let host = parsed.host_str().unwrap_or("");
        match parsed.scheme() {
            "https" => {}
            "http" if allow_private_source && is_loopback_host(host) => {}
            "http" => bail!(
                "WORLDS_CONTENT_MIRROR_SOURCE is http:// to {host}: a plaintext upstream lets a MITM plant \
                 worlds and files; use https://"
            ),
            other => bail!("WORLDS_CONTENT_MIRROR_SOURCE scheme {other}:// is not http(s)"),
        }
        if !allow_private_source && !is_safe_http_url(&source) {
            bail!(
                "WORLDS_CONTENT_MIRROR_SOURCE host {host} is refused by the SSRF guard (loopback, private, \
                 internal or single-label); a mirror source is an explicit public upstream, never a local URL"
            );
        }
        let own_origins: Vec<String> = own_urls
            .iter()
            .filter_map(|u| url::Url::parse(u.trim()).ok())
            .filter_map(|u| origin_of(&u))
            .collect();
        let source_origin = origin_of(&parsed).unwrap_or_default();
        if own_origins.contains(&source_origin) {
            bail!(
                "WORLDS_CONTENT_MIRROR_SOURCE {source} is this node itself; a node cannot mirror worlds from its own routes"
            );
        }
        if concurrency == 0 || blob_concurrency == 0 {
            bail!(
                "WORLDS_CONTENT_MIRROR_CONCURRENCY and WORLDS_CONTENT_MIRROR_BLOB_CONCURRENCY must be at least 1"
            );
        }
        if interval.is_zero() {
            bail!("WORLDS_CONTENT_MIRROR_INTERVAL_SECS must be at least 1");
        }
        let names: Vec<String> = names
            .split(',')
            .map(|n| n.trim().to_ascii_lowercase())
            .filter(|n| !n.is_empty())
            .collect();
        Ok(Self {
            source,
            interval,
            concurrency,
            blob_concurrency,
            limit: (limit > 0).then_some(limit),
            names,
            own_origins,
            allow_private_source,
            trust_upstream_grants: false,
            scheduled: true,
        })
    }

    /// A loopback stub as the upstream, unguarded; the guard itself is
    /// covered by `parse`'s tests.
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_tests(source: &str) -> Self {
        Self::parse_with(source, &[], Duration::from_secs(60), 2, 4, 0, "", true).unwrap()
    }

    /// Tests only: the node's own URLs (`PUBLIC_URL`, ...) for the loop checks.
    #[cfg(any(test, feature = "test-support"))]
    pub fn with_own_urls(mut self, own_urls: &[String]) -> Self {
        self.own_origins = own_urls
            .iter()
            .filter_map(|u| url::Url::parse(u.trim()).ok())
            .filter_map(|u| origin_of(&u))
            .collect();
        self
    }

    pub fn allows_private_source(&self) -> bool {
        self.allow_private_source
    }

    /// Tests only: the read-through alone, as `from_config` builds it without
    /// `WORLDS_CONTENT_MIRROR_ENABLED`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn read_through_only(mut self) -> Self {
        self.scheduled = false;
        self
    }

    /// Tests only: `WORLDS_READ_THROUGH_TRUST_UPSTREAM_GRANTS`.
    #[cfg(any(test, feature = "test-support"))]
    pub fn trusting_upstream_grants(mut self, trust: bool) -> Self {
        self.trust_upstream_grants = trust;
        self
    }

    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = (limit > 0).then_some(limit);
        self
    }

    pub fn with_concurrency(mut self, concurrency: usize) -> Self {
        self.concurrency = concurrency.max(1);
        self
    }

    /// A scene's `baseUrl` (from the upstream's /about) is fetched only when
    /// it is a publicly routable http(s) URL that is not this node: the
    /// upstream chooses it, so it gets the SSRF guard (the socket-level half
    /// runs in [`crate::http::Outbound`] at fetch time). A test stub's own
    /// origin passes only under `for_tests`.
    pub fn base_url_allowed(&self, base: &str) -> bool {
        let Ok(parsed) = url::Url::parse(base) else {
            return false;
        };
        let Some(origin) = origin_of(&parsed) else {
            return false;
        };
        if self.own_origins.contains(&origin) {
            return false;
        }
        if self.allow_private_source {
            let source_origin = url::Url::parse(&self.source)
                .ok()
                .and_then(|u| origin_of(&u));
            if source_origin.as_deref() == Some(origin.as_str()) {
                return true;
            }
        }
        is_safe_http_url(base)
    }
}
