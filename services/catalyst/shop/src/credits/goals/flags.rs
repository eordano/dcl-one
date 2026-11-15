//! Credits owns its Referer-specific flag snapshots. Social's persisted dapps
//! document and the Marketplace's site context cannot authorize goal rewards.
use crate::{
    http::Outbound,
    jobs::{Jobs, PeriodicCfg},
};
use parking_lot::RwLock;
use serde_json::Value;
use std::{sync::Arc, time::Duration};

#[derive(Clone, Debug)]
pub struct Source {
    pub url: String,
    pub origin: String,
    pub refresh: Duration,
}
impl Default for Source {
    fn default() -> Self {
        Self {
            url: String::new(),
            origin: String::new(),
            refresh: Duration::from_secs(60),
        }
    }
}
impl Source {
    pub fn normalize(mut self) -> anyhow::Result<Self> {
        self.url = self.url.trim().trim_end_matches('/').to_string();
        self.origin = self.origin.trim().to_string();
        if !self.origin.is_empty()
            && !self.origin.to_ascii_lowercase().starts_with("https://")
            && !self.origin.to_ascii_lowercase().starts_with("http://")
        {
            self.origin = format!("https://{}", self.origin);
        }
        if !self.url.is_empty() {
            anyhow::ensure!(
                !self.origin.is_empty(),
                "CREDITS_SERVER_URL is required with CREDITS_FEATURE_FLAGS_URL"
            );
            let url = reqwest::Url::parse(&self.url)?;
            let origin = reqwest::Url::parse(&self.origin)?;
            anyhow::ensure!(
                matches!(url.scheme(), "https" | "http")
                    && matches!(origin.scheme(), "https" | "http"),
                "Credits feature flags require HTTP(S) URLs"
            );
            anyhow::ensure!(
                origin.username().is_empty() && origin.password().is_none(),
                "CREDITS_SERVER_URL must not contain credentials"
            );
        }
        self.refresh = self.refresh.max(Duration::from_secs(10));
        Ok(self)
    }
}

pub struct Flags {
    source: Source,
    http: Outbound,
    documents: RwLock<[Option<Value>; 2]>,
}
impl Flags {
    pub fn new(source: Source, http: Outbound) -> Self {
        Self {
            source,
            http,
            documents: RwLock::new([None, None]),
        }
    }
    pub(super) fn documents(&self) -> [Option<Value>; 2] {
        self.documents.read().clone()
    }
    pub async fn refresh(&self) -> anyhow::Result<()> {
        if self.source.url.is_empty() {
            return Ok(());
        }
        let fetch = |app: &'static str| async move {
            let url = format!("{}/{app}.json", self.source.url);
            #[cfg(not(test))]
            anyhow::ensure!(
                crate::http::is_safe_http_url(&url),
                "Credits feature flags URL refused by SSRF guard"
            );
            crate::explore::api::feature_flags::fetch_document(
                &self.http,
                reqwest::Url::parse(&url)?,
                &self.source.origin,
                Duration::from_secs(5),
            )
            .await
        };
        let (dapps, explorer) = tokio::join!(fetch("dapps"), fetch("explorer"));
        let mut docs = self.documents.write();
        let mut errors = Vec::new();
        for (i, result) in [dapps, explorer].into_iter().enumerate() {
            match result {
                Ok(doc) => docs[i] = Some(doc),
                Err(error) => {
                    // Do not keep a remotely enabled reward policy active forever
                    // when its source becomes unreachable.
                    docs[i] = None;
                    errors.push(error.to_string());
                }
            }
        }
        if !errors.is_empty() {
            anyhow::bail!(
                "Credits feature flags refresh failed: {}",
                errors.join("; ")
            );
        }
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn set_documents(&self, dapps: Option<Value>, explorer: Option<Value>) {
        *self.documents.write() = [dapps, explorer];
    }
}
pub(super) fn spawn(state: &super::SharedState, jobs: &Jobs) {
    let state = Arc::clone(state);
    jobs.spawn_periodic(
        "credits-goal-flags",
        PeriodicCfg::every(state.settings.goal_flags_source.refresh),
        move |_| {
            let state = state.clone();
            async move { state.goal_flags.refresh().await }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn credits_referer_normalization_matches_source_case_insensitive_schemes() {
        for (input, want) in [
            (" credits.example.org ", "https://credits.example.org"),
            ("HTTPS://credits.example.org", "HTTPS://credits.example.org"),
            ("http://credits.example.org", "http://credits.example.org"),
        ] {
            assert_eq!(
                Source {
                    origin: input.into(),
                    ..Default::default()
                }
                .normalize()
                .unwrap()
                .origin,
                want
            );
        }
    }
}
