//! Full moderation snapshots over the existing moderator HTTP route.
use crate::Address;
use futures::StreamExt;
use reqwest::header::{AUTHORIZATION, ETAG, HeaderValue, IF_NONE_MATCH};
use serde::Deserialize;
use std::time::Duration;

pub const DEFAULT_POLL_SECONDS: u64 = 30;
const MAX_BODY: usize = 16 * 1024 * 1024;
pub struct BanPoller {
    client: reqwest::Client,
    url: reqwest::Url,
    authorization: HeaderValue,
    etag: Option<HeaderValue>,
    interval: Duration,
}
impl BanPoller {
    pub fn from_lookup(env: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let token = env("COMMS_MODERATOR_TOKEN")
            .filter(|v| !v.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("COMMS_MODERATOR_TOKEN is required for Pulse bans"))?;
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| anyhow::anyhow!("invalid COMMS_MODERATOR_TOKEN"))?;
        authorization.set_sensitive(true);
        let url = env("PULSE_BANS_URL")
            .and_then(|v| reqwest::Url::parse(&v).ok())
            .ok_or_else(|| {
                anyhow::anyhow!("PULSE_BANS_URL requires the moderator bans endpoint")
            })?;
        let loopback = url
            .host_str()
            .and_then(|h| h.trim_matches(['[', ']']).parse::<std::net::IpAddr>().ok())
            .is_some_and(|ip| ip.is_loopback());
        if (url.scheme() != "https" && !(url.scheme() == "http" && loopback))
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            anyhow::bail!("PULSE_BANS_URL requires HTTPS (loopback HTTP is permitted)");
        }
        let seconds = match env("PULSE_BANS_POLL_INTERVAL_SECONDS") {
            Some(v) => v.parse::<u64>().ok().filter(|n| *n > 0).ok_or_else(|| {
                anyhow::anyhow!("PULSE_BANS_POLL_INTERVAL_SECONDS must be positive")
            })?,
            None => DEFAULT_POLL_SECONDS,
        };
        let interval = Duration::from_secs(seconds);
        if std::time::Instant::now().checked_add(interval).is_none() {
            anyhow::bail!("PULSE_BANS_POLL_INTERVAL_SECONDS is too large");
        }
        let client = crate::http::HttpClientCfg::default()
            .with_total_timeout(Duration::from_secs(10))
            .builder()
            .no_proxy()
            .build()?;
        Ok(Self {
            client,
            url,
            authorization,
            etag: None,
            interval,
        })
    }
    async fn poll_once(&mut self) -> anyhow::Result<Option<Vec<String>>> {
        let mut request = self
            .client
            .get(self.url.clone())
            .header(AUTHORIZATION, self.authorization.clone());
        if let Some(etag) = &self.etag {
            request = request.header(IF_NONE_MATCH, etag.clone());
        }
        let response = request
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("ban HTTP request failed"))?;
        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            if self.etag.is_none() {
                anyhow::bail!("unsolicited ban 304");
            }
            return Ok(None);
        }
        if response.status() != reqwest::StatusCode::OK {
            anyhow::bail!("ban HTTP response was not 200");
        }
        let etag = response.headers().get(ETAG).cloned();
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| anyhow::anyhow!("ban HTTP body failed"))?;
            if body.len().saturating_add(chunk.len()) > MAX_BODY {
                anyhow::bail!("ban snapshot too large");
            }
            body.extend_from_slice(&chunk);
        }
        let parsed: Snapshot =
            serde_json::from_slice(&body).map_err(|_| anyhow::anyhow!("malformed ban snapshot"))?;
        let addresses = parsed.addresses()?;
        self.etag = etag;
        Ok(Some(addresses))
    }
    pub fn spawn(mut self) -> tokio::sync::watch::Receiver<Option<Vec<String>>> {
        let (tx, rx) = tokio::sync::watch::channel(None);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tx.closed() => break,
                    result = self.poll_once() => match result {
                        Ok(Some(addresses)) => { if tx.send(Some(addresses)).is_err() { break; } },
                        Ok(None) => {},
                        Err(error) => tracing::warn!(%error, "bans poll failed; retaining previous snapshot"),
                    }
                }
                tokio::select! { _ = tx.closed() => break, _ = tokio::time::sleep(self.interval) => {} }
            }
        });
        rx
    }
}
#[derive(Deserialize)]
struct Snapshot {
    data: Vec<Entry>,
}
#[derive(Deserialize)]
struct Entry {
    #[serde(rename = "bannedAddress")]
    banned_address: String,
}
impl Snapshot {
    fn addresses(self) -> anyhow::Result<Vec<String>> {
        self.data
            .into_iter()
            .map(|entry| {
                Address::parse(&entry.banned_address)
                    .map(Address::into_string)
                    .map_err(|_| anyhow::anyhow!("malformed banned wallet in snapshot"))
            })
            .collect()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        extract::State,
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::get,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    const WALLET: &str = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    fn config(url: &str, extra: Option<(&str, &str)>) -> anyhow::Result<BanPoller> {
        BanPoller::from_lookup(&|key| match key {
            "COMMS_MODERATOR_TOKEN" => Some("fixture-secret".into()),
            "PULSE_BANS_URL" => Some(url.into()),
            _ => extra.filter(|(k, _)| *k == key).map(|(_, v)| v.into()),
        })
    }
    #[test]
    fn bans_config_requires_token_and_positive_interval() {
        assert!(BanPoller::from_lookup(&|_| None).is_err());
        let poller = config("https://gatekeeper.example/comms-gatekeeper/bans", None).unwrap();
        assert_eq!(poller.interval, Duration::from_secs(30));
        assert!(poller.authorization.is_sensitive());
        assert_eq!(
            config(
                "http://127.0.0.1/bans",
                Some(("PULSE_BANS_POLL_INTERVAL_SECONDS", "7"))
            )
            .unwrap()
            .interval,
            Duration::from_secs(7)
        );
        for bad in ["0", "-1", "oops"] {
            assert!(
                config(
                    "http://127.0.0.1/bans",
                    Some(("PULSE_BANS_POLL_INTERVAL_SECONDS", bad))
                )
                .is_err()
            );
        }
        assert!(config("http://gatekeeper.example/bans", None).is_err());
        assert!(config("https://secret@gatekeeper.example/bans", None).is_err());
    }
    #[test]
    fn bans_snapshot_normalizes_case_prefix_and_whitespace() {
        let body = serde_json::json!({"data":[
            {"bannedAddress": " 0XAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n"},
            {"bannedAddress": WALLET},
        ]});
        assert_eq!(
            serde_json::from_value::<Snapshot>(body)
                .unwrap()
                .addresses()
                .unwrap(),
            [WALLET, WALLET]
        );
    }
    #[test]
    fn bans_snapshot_rejects_malformed_entries_atomically() {
        for bad in [
            "",
            "0xabc",
            "0xgggggggggggggggggggggggggggggggggggggggg",
            "\u{0800}",
        ] {
            let body =
                serde_json::json!({"data":[{"bannedAddress": WALLET},{"bannedAddress":bad}]});
            assert!(
                serde_json::from_value::<Snapshot>(body)
                    .unwrap()
                    .addresses()
                    .is_err()
            );
        }
        for bad in [r#"{"data":[{}]}"#, r#"{"data":[{"bannedAddress":null}]}"#] {
            assert!(serde_json::from_str::<Snapshot>(bad).is_err());
        }
    }
    async fn fixture(
        State(step): State<Arc<AtomicUsize>>,
        headers: HeaderMap,
    ) -> axum::response::Response {
        assert_eq!(headers[AUTHORIZATION], "Bearer fixture-secret");
        let step = step.fetch_add(1, Ordering::SeqCst);
        if step > 0 {
            assert_eq!(headers[IF_NONE_MATCH], "\"one\"");
        }
        match step {
            0 => (
                [(ETAG, "\"one\"")],
                serde_json::json!({"data":[{"bannedAddress":WALLET}]}).to_string(),
            )
                .into_response(),
            1 => StatusCode::NOT_MODIFIED.into_response(),
            2 => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            3 => (
                [(ETAG, "\"invalid\"")],
                serde_json::json!({"data":[{"bannedAddress":WALLET},{"bannedAddress":"\u{0800}"}]})
                    .to_string(),
            )
                .into_response(),
            _ => ([(ETAG, "\"two\"")], r#"{"data":[]}"#).into_response(),
        }
    }
    #[tokio::test]
    async fn bans_http_conditional_poll_retains_validator_on_errors_and_accepts_unban() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/bans", listener.local_addr().unwrap());
        let router = Router::new()
            .route("/bans", get(fixture))
            .with_state(Arc::new(AtomicUsize::new(0)));
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let mut poller = config(&url, None).unwrap();
        assert_eq!(poller.poll_once().await.unwrap(), Some(vec![WALLET.into()]));
        assert_eq!(poller.poll_once().await.unwrap(), None);
        assert!(poller.poll_once().await.is_err());
        assert!(poller.poll_once().await.is_err());
        assert_eq!(poller.etag.as_ref().unwrap(), "\"one\"");
        assert_eq!(poller.poll_once().await.unwrap(), Some(vec![]));
        task.abort();
    }
}
