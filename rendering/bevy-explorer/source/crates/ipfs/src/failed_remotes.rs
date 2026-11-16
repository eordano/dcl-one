//! Downloads that failed, by url: a repeat request is answered with the failure instead of
//! going out again.

use std::collections::HashMap;

use reqwest::StatusCode;
use web_time::{Duration, Instant};

/// How long any failure is answered from memory before its url is asked for again.
pub(crate) const RETRY_AFTER: Duration = Duration::from_secs(10);

/// Most urls held at once.
const LIMIT: usize = 4096;

/// A client error about the url itself, which asking again does not change. Every other status
/// keeps the retry window: 401, 403 and 407 come from a CDN, firewall or proxy; 408, 421, 425 and
/// 429 ask to come back later or on another connection; 409, 413, 423 and 431 are about the
/// request or the server's state.
pub(crate) fn is_definitive(status: StatusCode) -> bool {
    matches!(status.as_u16(), 400 | 404 | 405 | 410 | 414 | 451)
}

/// Why a request is answered from memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Repeat {
    /// It failed less than [`RETRY_AFTER`] ago.
    Recent,
    /// Its server answered this definitive status for a content hash earlier this session.
    Session(StatusCode),
}

struct Failed {
    at: Instant,
    session: Option<StatusCode>,
}

pub(crate) struct FailedRemotes {
    entries: HashMap<String, Failed>,
    limit: usize,
}

impl Default for FailedRemotes {
    fn default() -> Self {
        Self::with_limit(LIMIT)
    }
}

impl FailedRemotes {
    fn with_limit(limit: usize) -> Self {
        Self {
            entries: HashMap::new(),
            limit,
        }
    }

    /// Remember that `remote` failed at `now`; `session` is the definitive status that keeps it
    /// for the session.
    pub(crate) fn record(&mut self, remote: String, session: Option<StatusCode>, now: Instant) {
        let session =
            session.or_else(|| self.entries.get(&remote).and_then(|failed| failed.session));
        if !self.entries.contains_key(&remote) && self.entries.len() >= self.limit {
            self.entries
                .retain(|_, failed| failed.session.is_some() || !expired(failed.at, now));
            if self.entries.len() >= self.limit {
                // a short-lived entry goes first: a session entry is the costlier one to lose
                let oldest = self
                    .entries
                    .iter()
                    .min_by_key(|(_, failed)| (failed.session.is_some(), failed.at))
                    .map(|(url, _)| url.clone());
                if let Some(oldest) = oldest {
                    self.entries.remove(&oldest);
                }
            }
        }
        self.entries.insert(remote, Failed { at: now, session });
    }

    /// Whether `remote` is answered from memory at `now`. A session entry answers only a
    /// `content` read (one keyed by hash); any other read of the url treats it as a short-lived
    /// one and leaves it in place. An expired short-lived entry is dropped.
    pub(crate) fn repeat(&mut self, remote: &str, now: Instant, content: bool) -> Option<Repeat> {
        let failed = self.entries.get(remote)?;
        if let Some(status) = failed.session {
            if content {
                return Some(Repeat::Session(status));
            }
            return (!expired(failed.at, now)).then_some(Repeat::Recent);
        }
        if !expired(failed.at, now) {
            return Some(Repeat::Recent);
        }
        self.entries.remove(remote);
        None
    }

    pub(crate) fn forget(&mut self, remote: &str) {
        self.entries.remove(remote);
    }

    /// Urls answered as missing for the rest of the session.
    pub(crate) fn session_len(&self) -> usize {
        self.entries
            .values()
            .filter(|failed| failed.session.is_some())
            .count()
    }
}

fn expired(at: Instant, now: Instant) -> bool {
    now.checked_duration_since(at).unwrap_or_default() > RETRY_AFTER
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "https://a/contents/x";

    fn code(n: u16) -> StatusCode {
        StatusCode::from_u16(n).unwrap()
    }

    #[test]
    fn only_client_errors_about_the_url_are_definitive() {
        for n in [400, 404, 405, 410, 414, 451] {
            assert!(is_definitive(code(n)), "{n}");
        }
        for n in [
            401, 403, 407, 408, 409, 413, 421, 423, 425, 429, 431, 500, 502, 503, 504,
        ] {
            assert!(!is_definitive(code(n)), "{n}");
        }
    }

    #[test]
    fn a_later_short_lived_failure_keeps_a_session_entry() {
        let t0 = Instant::now();
        let mut failed = FailedRemotes::default();
        failed.record(URL.into(), Some(StatusCode::NOT_FOUND), t0);
        failed.record(URL.into(), None, t0 + Duration::from_secs(1));
        assert_eq!(
            failed.repeat(URL, t0 + Duration::from_secs(3600), true),
            Some(Repeat::Session(StatusCode::NOT_FOUND))
        );
        assert_eq!(failed.session_len(), 1);
    }

    #[test]
    fn a_definitive_failure_is_answered_for_the_session() {
        let t0 = Instant::now();
        let mut failed = FailedRemotes::default();
        failed.record(URL.into(), Some(StatusCode::NOT_FOUND), t0);
        let later = t0 + Duration::from_secs(3600);
        assert_eq!(
            failed.repeat(URL, later, true),
            Some(Repeat::Session(StatusCode::NOT_FOUND))
        );
        assert_eq!(
            failed.repeat(URL, later, true),
            Some(Repeat::Session(StatusCode::NOT_FOUND))
        );
        assert_eq!(failed.session_len(), 1);
    }

    #[test]
    fn a_session_failure_answers_other_reads_only_within_the_retry_window() {
        let t0 = Instant::now();
        let mut failed = FailedRemotes::default();
        failed.record(URL.into(), Some(StatusCode::NOT_FOUND), t0);
        assert_eq!(
            failed.repeat(URL, t0 + Duration::from_secs(5), false),
            Some(Repeat::Recent)
        );
        assert_eq!(
            failed.repeat(URL, t0 + Duration::from_secs(11), false),
            None
        );
        assert_eq!(
            failed.repeat(URL, t0 + Duration::from_secs(11), true),
            Some(Repeat::Session(StatusCode::NOT_FOUND))
        );
        assert_eq!(failed.session_len(), 1);
    }

    #[test]
    fn other_failures_are_asked_for_again_after_the_retry_window() {
        let t0 = Instant::now();
        let url = "https://a/x";
        let mut failed = FailedRemotes::default();
        failed.record(url.into(), None, t0);
        assert_eq!(
            failed.repeat(url, t0 + Duration::from_secs(10), true),
            Some(Repeat::Recent)
        );
        assert_eq!(failed.repeat(url, t0 + Duration::from_secs(11), true), None);
        assert_eq!(failed.repeat(url, t0 + Duration::from_secs(1), true), None);
        assert_eq!(failed.session_len(), 0);
    }

    #[test]
    fn forget_drops_a_session_failure() {
        let t0 = Instant::now();
        let mut failed = FailedRemotes::default();
        failed.record(URL.into(), Some(StatusCode::NOT_FOUND), t0);
        failed.forget(URL);
        assert_eq!(failed.repeat(URL, t0, true), None);
    }

    #[test]
    fn the_map_stays_within_its_limit() {
        let t0 = Instant::now();
        let secs = |n| t0 + Duration::from_secs(n);
        let nf = Some(StatusCode::NOT_FOUND);
        let session = Some(Repeat::Session(StatusCode::NOT_FOUND));
        let mut failed = FailedRemotes::with_limit(3);
        failed.record("a".into(), None, t0);
        failed.record("b".into(), None, t0);
        failed.record("c".into(), nf, t0);

        failed.record("d".into(), nf, secs(11));
        assert_eq!(failed.entries.len(), 2);
        assert_eq!(failed.repeat("c", secs(11), true), session);
        assert_eq!(failed.repeat("d", secs(11), true), session);

        failed.record("e".into(), nf, secs(12));
        failed.record("f".into(), nf, secs(13));
        assert_eq!(failed.entries.len(), 3);
        assert_eq!(failed.repeat("c", secs(13), true), None);
        for url in ["d", "e", "f"] {
            assert_eq!(failed.repeat(url, secs(13), true), session, "{url}");
        }

        failed.record("f".into(), nf, secs(14));
        assert_eq!(failed.entries.len(), 3);
        assert_eq!(failed.repeat("d", secs(14), true), session);
    }

    #[test]
    fn a_full_map_drops_a_short_lived_entry_before_a_session_entry() {
        let t0 = Instant::now();
        let secs = |n| t0 + Duration::from_secs(n);
        let nf = Some(StatusCode::NOT_FOUND);
        let session = Some(Repeat::Session(StatusCode::NOT_FOUND));
        let mut failed = FailedRemotes::with_limit(3);
        failed.record("a".into(), nf, t0);
        failed.record("b".into(), nf, secs(1));
        failed.record("c".into(), None, secs(2));

        failed.record("d".into(), nf, secs(3));
        assert_eq!(failed.entries.len(), 3);
        assert_eq!(failed.repeat("a", secs(3), true), session);
        assert_eq!(failed.repeat("b", secs(3), true), session);
        assert_eq!(failed.repeat("c", secs(3), true), None);
    }
}
