//! The operator's content denylist, made durable: `content.denylist`
//! (`schemas/00_content.sql`).
//!
//! The old live node kept the list in a process `HashSet` that a restart
//! emptied. Here the table is the authority and the in-memory
//! [`super::Denylist`] is its projection: loaded at boot
//! ([`ContentState::load_denylist`]), replaced after every console edit
//! (in the edit's transaction's wake, [`add`]/[`remove`]) and by the
//! `content-denylist-reload` job every [`RELOAD_INTERVAL`] (the freshness
//! budget for an edit made outside this process, `psql` or a second node on
//! the same database). The read routes consult the projection exactly as
//! the old node consulted its set: `/contents/{hash}` answers 404 for a
//! listed hash, the active-entity reads drop a listed entity id,
//! `/pointer-changes` drops its deltas. A local deploy is refused when its
//! entity id, a content hash it declares or one of its pointers is listed
//! ([`refusal`]).
//!
//! An id is any non-blank string of at most 512 bytes without
//! whitespace or control characters (the old console's rule); entity ids
//! and hashes are compared exactly (CIDv0 is case-sensitive), pointers
//! case-insensitively (a pointer is stored lowercased).
//!
//! The remote lists (`DENYLIST_URLS`; on a mirror Decentraland's moderation
//! list, the asset-bundle-registry's `GET /denylist`, the upstream catalyst's
//! default) feed the same projection, so every read above hides their ids
//! too. They are fetched at boot and by the `content-denylist-remote` job
//! every [`REMOTE_RELOAD_INTERVAL`] ([`reload_remote`]), as the upstream
//! catalyst does (`decentraland/catalyst:src/components.ts:134-137`), and
//! held in memory only: they are not the operator's, so they never reach
//! `content.denylist` or the console. A fetch that fails, answers anything
//! but an array or names more than [`MAX_ENTRIES`] ids keeps that URL's last
//! good list, and never touches the operator's half; an empty array is a
//! good list and lifts every id the URL named.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use sqlx::PgExecutor;

use super::ContentState;
use super::entity::Entity;
use crate::http::Outbound;
use crate::jobs::{Jobs, PeriodicCfg};

/// How long an edit made outside this process can take to reach the reads.
pub const RELOAD_INTERVAL: Duration = Duration::from_secs(60);

/// The load's bound, and a remote list's. The list is curated by hand
/// (expected hundreds of rows); a table or remote list past this is a
/// mistake reported instead of held in memory.
pub const MAX_ENTRIES: i64 = 100_000;

/// The remote lists' cadence (the upstream catalyst's `denylistReloadJob`).
pub const REMOTE_RELOAD_INTERVAL: Duration = Duration::from_secs(120);

/// One remote fetch's bound, the boot fetch's included.
const REMOTE_TIMEOUT: Duration = Duration::from_secs(15);

/// A remote list's body cap (Decentraland's answers a few kilobytes).
const REMOTE_BODY_CAP: usize = 1024 * 1024;

pub fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 512 && s.chars().all(|c| !c.is_control() && !c.is_whitespace())
}

/// Every listed id, in id order, at most [`MAX_ENTRIES`].
pub async fn load<'e, E: PgExecutor<'e>>(executor: E) -> sqlx::Result<Vec<String>> {
    let ids: Vec<String> =
        sqlx::query_scalar("SELECT id FROM content.denylist ORDER BY id LIMIT $1")
            .bind(MAX_ENTRIES + 1)
            .fetch_all(executor)
            .await?;
    if ids.len() as i64 > MAX_ENTRIES {
        return Err(sqlx::Error::Protocol(format!(
            "content.denylist holds more than {MAX_ENTRIES} rows; refusing to project it"
        )));
    }
    Ok(ids)
}

/// One `content.denylist` row, `added_at` in Unix milliseconds (UTC).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Row {
    pub id: String,
    pub added_by: String,
    pub added_at_ms: i64,
}

/// Every listed row, newest first, at most [`MAX_ENTRIES`].
pub async fn rows<'e, E: PgExecutor<'e>>(executor: E) -> sqlx::Result<Vec<Row>> {
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, added_by, \
                (EXTRACT(EPOCH FROM added_at) * 1000)::bigint AS added_at_ms \
         FROM content.denylist ORDER BY added_at DESC, id LIMIT $1",
    )
    .bind(MAX_ENTRIES + 1)
    .fetch_all(executor)
    .await?;
    if rows.len() as i64 > MAX_ENTRIES {
        return Err(sqlx::Error::Protocol(format!(
            "content.denylist holds more than {MAX_ENTRIES} rows; refusing to serve it"
        )));
    }
    Ok(rows)
}

/// Inserts `id`; `false` when it was already listed. `actor` is the console
/// wallet recorded with the row.
pub async fn add<'e, E: PgExecutor<'e>>(executor: E, id: &str, actor: &str) -> sqlx::Result<bool> {
    let inserted: Option<String> = sqlx::query_scalar(
        "INSERT INTO content.denylist (id, added_by) VALUES ($1, $2) ON CONFLICT (id) DO NOTHING RETURNING id",
    )
    .bind(id)
    .bind(actor)
    .fetch_optional(executor)
    .await?;
    Ok(inserted.is_some())
}

/// Deletes `id`; `false` when it was not listed.
pub async fn remove<'e, E: PgExecutor<'e>>(executor: E, id: &str) -> sqlx::Result<bool> {
    Ok(sqlx::query("DELETE FROM content.denylist WHERE id = $1")
        .bind(id)
        .execute(executor)
        .await?
        .rows_affected()
        == 1)
}

/// Why a local deploy of `entity` is refused, one message per kind of
/// listed id; `None` when nothing it names is listed.
pub fn refusal(entity: &Entity, denylist: &super::Denylist) -> Option<Vec<String>> {
    let mut errors = Vec::new();
    if denylist.contains(&entity.id) {
        errors.push(format!(
            "The entity {} is denylisted on this node.",
            entity.id
        ));
    }
    let mut hashes: Vec<&str> = entity
        .content
        .iter()
        .map(|c| c.hash.as_str())
        .filter(|h| denylist.contains(h))
        .collect();
    hashes.sort_unstable();
    hashes.dedup();
    if !hashes.is_empty() {
        errors.push(format!(
            "These content files are denylisted on this node: {}",
            hashes.join(", ")
        ));
    }
    let mut pointers: Vec<String> = entity
        .pointers
        .iter()
        .map(|p| p.to_lowercase())
        .filter(|p| denylist.denies_pointer(p))
        .collect();
    pointers.sort_unstable();
    pointers.dedup();
    if !pointers.is_empty() {
        errors.push(format!(
            "These pointers are denylisted on this node: {}",
            pointers.join(", ")
        ));
    }
    (!errors.is_empty()).then_some(errors)
}

/// What logs and the job report carry of a remote URL: host and path, no
/// scheme, credentials or query.
fn shown(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(u) => format!("{}{}", u.host_str().unwrap_or("?"), u.path()),
        Err(_) => "<unparseable url>".to_string(),
    }
}

/// One entry of a remote list: its `entity_id` when that is a string. The
/// other fields and any other shape are skipped unread; `null` is refused.
struct RemoteEntry(Option<String>);

impl<'de> serde::Deserialize<'de> for RemoteEntry {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(Lenient { entry: true }).map(RemoteEntry)
    }
}

/// An `entity_id` value: the string, `None` for anything else.
struct EntityId(Option<String>);

impl<'de> serde::Deserialize<'de> for EntityId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(Lenient { entry: false }).map(EntityId)
    }
}

/// Reads an entry (`entry`: an object's `entity_id`, `null` refused) or an
/// `entity_id` value (a string), skipping everything else without building
/// it.
struct Lenient {
    entry: bool,
}

impl<'de> Visitor<'de> for Lenient {
    type Value = Option<String>;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a denylist entry")
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        if self.entry {
            return Err(E::custom("the denylist holds a null entry"));
        }
        Ok(None)
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        self.visit_unit()
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
        Ok(None)
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        Ok((!self.entry).then(|| v.to_string()))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(None)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut id = None;
        while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            if self.entry && key == "entity_id" {
                // A repeated key: the last one wins, as in JavaScript.
                id = map.next_value::<EntityId>()?.0;
            } else {
                map.next_value::<IgnoredAny>()?;
            }
        }
        Ok(id)
    }
}

/// The ids of one remote list's body: a JSON array whose entries carry
/// `entity_id` (`decentraland/catalyst:src/adapters/denylist/component.ts:67-77`).
/// As upstream: an entry whose `entity_id` is missing or not a string is
/// skipped, ids are trimmed and blank or `#` ones dropped, and a body that
/// is not an array or holds a `null` entry is an error. Beyond upstream: an
/// id this node could never list ([`valid_id`]) is dropped, and a list of
/// more than [`MAX_ENTRIES`] ids is an error.
pub fn parse_remote(body: &[u8]) -> anyhow::Result<Vec<String>> {
    let entries: Vec<RemoteEntry> = serde_json::from_slice(body).map_err(|e| {
        if e.is_data() {
            anyhow::anyhow!("the denylist body is not an array of entries: {e}")
        } else {
            anyhow::anyhow!("the denylist body is not JSON: {e}")
        }
    })?;
    let ids: Vec<String> = entries
        .into_iter()
        .filter_map(|RemoteEntry(id)| id)
        .filter_map(|id| {
            let id = id.trim();
            (!id.starts_with('#') && valid_id(id)).then(|| id.to_string())
        })
        .collect();
    anyhow::ensure!(
        ids.len() as i64 <= MAX_ENTRIES,
        "the denylist names {} ids, more than {MAX_ENTRIES}",
        ids.len()
    );
    Ok(ids)
}

/// Fetches one remote list through the outbound guard and replaces that
/// URL's ids in `denylist`; the number of ids now held for it. On any error
/// (transport, a non-2xx status, a body over the cap or that [`parse_remote`]
/// refuses) the URL's previous ids stay. An empty array is a good answer: it
/// lifts every id the URL named.
pub async fn reload_remote(
    denylist: &super::Denylist,
    client: &Outbound,
    url: &str,
) -> anyhow::Result<usize> {
    let shown = shown(url);
    let resp = client
        .get(url)
        .timeout(REMOTE_TIMEOUT)
        .header("accept", "application/json")
        .send()
        .await
        .with_context(|| format!("{shown}: request failed"))?;
    let status = resp.status();
    anyhow::ensure!(status.is_success(), "{shown} answered {status}");
    let body = crate::http::read_body_capped(resp, REMOTE_BODY_CAP)
        .await
        .with_context(|| format!("{shown}: reading the body"))?;
    let ids = parse_remote(&body).with_context(|| shown.clone())?;
    let n = ids.len();
    denylist.set_remote(url, ids);
    Ok(n)
}

impl ContentState {
    /// One pass over `urls` ([`reload_remote`] each, in order): the ids now
    /// held across them and the errors of the URLs that failed.
    pub async fn reload_remote_denylist(
        &self,
        client: &Outbound,
        urls: &[String],
    ) -> (usize, Vec<String>) {
        let mut errors = Vec::new();
        for url in urls {
            if let Err(err) = reload_remote(&self.denylist, client, url).await {
                errors.push(format!("{err:#}"));
            }
        }
        (self.denylist.remote_len(), errors)
    }

    /// The remote lists' job, every [`REMOTE_RELOAD_INTERVAL`] after the
    /// boot fetch; nothing without URLs. Each process fetches for itself,
    /// as each upstream catalyst does.
    pub fn spawn_remote_denylist(
        self: &Arc<Self>,
        jobs: &Jobs,
        client: Outbound,
        urls: Vec<String>,
    ) {
        if urls.is_empty() {
            return;
        }
        let state = self.clone();
        jobs.spawn_periodic(
            "content-denylist-remote",
            PeriodicCfg::every(REMOTE_RELOAD_INTERVAL).after_first_interval(),
            move |ctx| {
                let (state, client, urls) = (state.clone(), client.clone(), urls.clone());
                async move {
                    let (n, errors) = state.reload_remote_denylist(&client, &urls).await;
                    for err in &errors {
                        tracing::warn!(error = %err, "remote denylist fetch failed; keeping its last list");
                        ctx.error(err.clone());
                    }
                    ctx.progress(format!("{n} remote ids"));
                    Ok(())
                }
            },
        );
    }

    /// Replaces the operator half of the projection with the table.
    pub async fn load_denylist(&self) -> sqlx::Result<usize> {
        let ids = load(&self.pool).await?;
        let n = ids.len();
        self.denylist.set_operator(ids);
        Ok(n)
    }

    /// The reload job ([`RELOAD_INTERVAL`], the out-of-process edit's
    /// freshness budget). A failed pass keeps the previous projection.
    pub fn spawn_jobs(self: &Arc<Self>, jobs: &Jobs) {
        let state = self.clone();
        jobs.spawn_periodic(
            "content-denylist-reload",
            PeriodicCfg::every(RELOAD_INTERVAL)
                .after_first_interval()
                .with_error_backoff(Duration::from_secs(600)),
            move |ctx| {
                let state = state.clone();
                async move {
                    let n = state.load_denylist().await?;
                    ctx.progress(format!("{n} operator ids"));
                    Ok(())
                }
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Denylist;
    use crate::entity::{ContentMapping, EntityType};
    use crate::tests::support::require_test_db;

    fn entity(id: &str, pointers: &[&str], hashes: &[&str]) -> Entity {
        Entity {
            id: id.to_string(),
            entity_type: EntityType::Scene,
            pointers: pointers.iter().map(|p| p.to_string()).collect(),
            timestamp: 1,
            content: hashes
                .iter()
                .map(|h| ContentMapping {
                    file: format!("{h}.bin"),
                    hash: h.to_string(),
                })
                .collect(),
            version: "v3".into(),
            metadata: None,
        }
    }

    #[test]
    fn ids_follow_the_old_console_rule() {
        assert!(valid_id("bafkreiabc"));
        assert!(valid_id("0,0"));
        assert!(valid_id(&"a".repeat(512)));
        assert!(!valid_id(&"a".repeat(513)));
        assert!(!valid_id(""));
        assert!(!valid_id("a b"));
        assert!(!valid_id("a\u{0}"));
    }

    #[test]
    fn a_deploy_naming_a_listed_id_hash_or_pointer_is_refused() {
        let denylist = Denylist::default();
        let clean = entity("bafyentity", &["0,0"], &["bafyfile"]);
        assert!(refusal(&clean, &denylist).is_none());

        denylist.set_operator([
            "bafyentity".to_string(),
            "bafyfile".to_string(),
            "10,-3".to_string(),
            "0xABC".to_string(),
        ]);
        let named = entity(
            "bafyentity",
            &["0,0", "10,-3", "0xabc"],
            &["bafyfile", "bafyother", "bafyfile"],
        );
        assert_eq!(
            refusal(&named, &denylist).unwrap(),
            vec![
                "The entity bafyentity is denylisted on this node.".to_string(),
                "These content files are denylisted on this node: bafyfile".to_string(),
                "These pointers are denylisted on this node: 0xabc, 10,-3".to_string(),
            ]
        );
        assert!(refusal(&entity("bafyother", &["1,1"], &["bafyother"]), &denylist).is_none());
    }

    #[test]
    fn the_operator_half_never_lifts_a_withheld_blob_and_back() {
        let denylist = Denylist::default();
        denylist.add("bafyphoto");
        denylist.set_operator(["bafyphoto".to_string(), "bafyillegal".to_string()]);
        denylist.set_operator(Vec::<String>::new());
        assert!(
            denylist.contains("bafyphoto"),
            "the camera reel's entry survives an operator removal"
        );
        assert!(!denylist.contains("bafyillegal"));
        denylist.operator_insert("bafyillegal");
        denylist.remove("bafyillegal");
        assert!(
            denylist.contains("bafyillegal"),
            "a camera-reel un-reject never lifts an operator entry"
        );
        assert_eq!(
            denylist.list(),
            vec!["bafyillegal".to_string(), "bafyphoto".to_string()]
        );
        assert_eq!(denylist.operator_ids(), vec!["bafyillegal".to_string()]);
        assert!(denylist.operator_remove("bafyillegal"));
        assert!(!denylist.contains("bafyillegal"));
    }

    /// Two entries of the asset-bundle-registry's `GET /denylist` as
    /// production answered it (`captures/abr/AB-9-denylist.json`: every value
    /// a string, newest first; reasons and addresses replaced).
    const ABR_BODY: &str = r#"[
        {"entity_id":"bafkreid44mgpv2yb5uhrk5f2f7disnocovyfoc6tlxv5q4rzhmnisvahba","reason":"<reason>","created_by":"0x0000000000000000000000000000000000000b03","created_at":"1782756919692","updated_at":"1782756919692"},
        {"entity_id":"bafkreigprhrerqgdwrgckblk5ebxhrglusanoch7yybddgvlmlilzj2lmu","reason":"<reason>","created_by":"0x0000000000000000000000000000000000000b04","created_at":"1777576838997","updated_at":"1777576838997"}
    ]"#;
    const ABR_FIRST: &str = "bafkreid44mgpv2yb5uhrk5f2f7disnocovyfoc6tlxv5q4rzhmnisvahba";

    #[test]
    fn the_captured_registry_body_parses_to_its_entity_ids() {
        assert_eq!(
            parse_remote(ABR_BODY.as_bytes()).unwrap(),
            vec![
                ABR_FIRST.to_string(),
                "bafkreigprhrerqgdwrgckblk5ebxhrglusanoch7yybddgvlmlilzj2lmu".to_string(),
            ]
        );
        // Upstream's line rules: trimmed, blank and `#` ids dropped, entries
        // without a string `entity_id` skipped.
        let quirks = br##"[{"entity_id":" bafyspaced "},{"entity_id":""},{"entity_id":"#note"},
            {"entity_id":7},{"reason":"no id"},"bafybare",{"entity_id":"bafyok"}]"##;
        assert_eq!(
            parse_remote(quirks).unwrap(),
            vec!["bafyspaced".to_string(), "bafyok".to_string()]
        );
        assert!(
            parse_remote(b"{\"entity_id\":\"bafy\"}").is_err(),
            "not an array"
        );
        assert!(parse_remote(b"[null]").is_err(), "a null entry");
        assert!(parse_remote(b"<html>").is_err(), "not JSON");
        assert!(parse_remote(b"[]").unwrap().is_empty(), "an emptied list");
        // A repeated key keeps the last value, as `JSON.parse` does; nested
        // values and a non-string id are skipped unread.
        assert_eq!(
            parse_remote(
                br#"[{"entity_id":"bafyfirst","x":{"a":[1,{"b":null}]},"entity_id":"bafylast"},
                {"entity_id":{"nested":"bafyno"}},{"entity_id":null},[{"entity_id":"bafyno"}]]"#
            )
            .unwrap(),
            vec!["bafylast".to_string()]
        );
    }

    #[test]
    fn a_remote_list_holds_only_valid_ids_and_at_most_max_entries() {
        let long = "a".repeat(513);
        let body = format!(
            r#"[{{"entity_id":"bafy inner space"}},{{"entity_id":"{long}"}},{{"entity_id":"bafy\u0000"}},{{"entity_id":"bafyok"}}]"#
        );
        assert_eq!(
            parse_remote(body.as_bytes()).unwrap(),
            vec!["bafyok".to_string()]
        );

        let entries = |n: i64| {
            let ids: Vec<String> = (0..n)
                .map(|i| format!(r#"{{"entity_id":"b{i}"}}"#))
                .collect();
            format!("[{}]", ids.join(","))
        };
        assert_eq!(
            parse_remote(entries(MAX_ENTRIES).as_bytes()).unwrap().len() as i64,
            MAX_ENTRIES
        );
        let err = parse_remote(entries(MAX_ENTRIES + 1).as_bytes()).unwrap_err();
        assert!(err.to_string().contains("more than"), "{err:#}");
    }

    #[test]
    fn remote_ids_merge_with_the_operator_list_and_neither_lifts_the_other() {
        let denylist = Denylist::default();
        denylist.set_operator(["bafyoperator".to_string()]);
        denylist.set_remote("https://a.example/denylist", ["bafyremote".to_string()]);
        denylist.set_remote("https://b.example/denylist", ["bafyother".to_string()]);
        assert!(denylist.contains("bafyoperator"));
        assert!(denylist.contains("bafyremote") && denylist.contains("bafyother"));
        assert_eq!(denylist.remote_len(), 2);

        denylist.set_operator(Vec::<String>::new());
        assert!(
            denylist.contains("bafyremote"),
            "an operator reload keeps the remote ids"
        );
        denylist.operator_insert("bafyoperator");
        denylist.set_remote("https://a.example/denylist", Vec::<String>::new());
        assert!(
            denylist.contains("bafyoperator"),
            "a remote replace keeps the operator's"
        );
        assert!(!denylist.contains("bafyremote"));
        assert!(
            denylist.contains("bafyother"),
            "each URL keeps its own list"
        );
        // The remote ids are not this node's: not in the console's lists, and
        // never a pointer refusal.
        denylist.set_remote("https://a.example/denylist", ["0,0".to_string()]);
        assert_eq!(denylist.list(), vec!["bafyoperator".to_string()]);
        assert_eq!(denylist.operator_ids(), vec!["bafyoperator".to_string()]);
        assert!(!denylist.denies_pointer("0,0"));
    }

    /// A stub list on the loopback whose answer the test switches.
    async fn stub_list(answer: Arc<parking_lot::Mutex<(u16, String)>>) -> String {
        use axum::http::StatusCode;
        let app = axum::Router::new().route(
            "/denylist",
            axum::routing::get(move || {
                let answer = answer.clone();
                async move {
                    let (status, body) = answer.lock().clone();
                    (StatusCode::from_u16(status).unwrap(), body)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/denylist")
    }

    #[tokio::test]
    async fn a_failed_fetch_keeps_the_last_list_and_the_operators() {
        let answer = Arc::new(parking_lot::Mutex::new((200, ABR_BODY.to_string())));
        let url = stub_list(answer.clone()).await;
        let client = Outbound::for_tests();
        let denylist = Denylist::default();
        denylist.set_operator(["bafyoperator".to_string()]);

        assert_eq!(reload_remote(&denylist, &client, &url).await.unwrap(), 2);
        assert!(denylist.contains(ABR_FIRST));

        for (status, body) in [
            (500, ABR_BODY.to_string()),
            (200, "<html>maintenance</html>".to_string()),
            (200, "{\"ok\":false}".to_string()),
            (
                200,
                format!("[{}]", "{\"entity_id\":\"bafy\"},".repeat(70_000) + "{}"),
            ),
        ] {
            *answer.lock() = (status, body.clone());
            let err = reload_remote(&denylist, &client, &url).await.unwrap_err();
            assert!(denylist.contains(ABR_FIRST), "{status} {body}: {err:#}");
            assert!(denylist.contains("bafyoperator"), "{status} {body}");
        }

        // A list without the first entry lifts it on the next good pass, and
        // an emptied list lifts the rest.
        *answer.lock() = (200, r#"[{"entity_id":"bafyonly"}]"#.to_string());
        assert_eq!(reload_remote(&denylist, &client, &url).await.unwrap(), 1);
        assert!(!denylist.contains(ABR_FIRST) && denylist.contains("bafyonly"));
        *answer.lock() = (200, "[]".to_string());
        assert_eq!(reload_remote(&denylist, &client, &url).await.unwrap(), 0);
        assert!(!denylist.contains("bafyonly") && denylist.contains("bafyoperator"));
        *answer.lock() = (200, r#"[{"entity_id":"bafyonly"}]"#.to_string());
        assert_eq!(reload_remote(&denylist, &client, &url).await.unwrap(), 1);

        // An unreachable URL fails the same way, and the guarded production
        // client never dials the loopback at all.
        let closed = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            format!("http://{}/denylist", listener.local_addr().unwrap())
        };
        assert!(reload_remote(&denylist, &client, &closed).await.is_err());
        let guarded = Outbound::new(crate::http::HttpClientCfg::default()).unwrap();
        assert!(reload_remote(&denylist, &guarded, &url).await.is_err());
        assert!(denylist.contains("bafyonly") && denylist.contains("bafyoperator"));
    }

    /// The upstream capture (`AB-10-peer-denylisted-hash.json`): a hash the
    /// remote list names answers 404 to `GET /contents/{hash}` with
    /// `If-None-Match: *`, where it answered 304 before.
    #[tokio::test]
    async fn a_remote_listed_hash_is_a_404_on_contents() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        use crate::tests::support::{TEST_CID, TestNode};

        let node = TestNode::without_db("content-remote-denied").await;
        node.store(TEST_CID, bytes::Bytes::from_static(b"x")).await;
        let get = |if_none_match: bool| {
            let mut req = Request::get(format!("/content/contents/{TEST_CID}"));
            if if_none_match {
                req = req.header("if-none-match", "*");
            }
            node.router().oneshot(req.body(Body::empty()).unwrap())
        };
        assert_eq!(get(false).await.unwrap().status(), StatusCode::OK);
        assert_eq!(get(true).await.unwrap().status(), StatusCode::NOT_MODIFIED);

        let answer = Arc::new(parking_lot::Mutex::new((
            200,
            format!(r#"[{{"entity_id":"{TEST_CID}","reason":"<reason>"}}]"#),
        )));
        let url = stub_list(answer).await;
        let (n, errors) = node
            .state
            .reload_remote_denylist(&Outbound::for_tests(), &[url])
            .await;
        assert_eq!((n, errors), (1, Vec::<String>::new()));
        assert_eq!(get(false).await.unwrap().status(), StatusCode::NOT_FOUND);
        assert_eq!(get(true).await.unwrap().status(), StatusCode::NOT_FOUND);
    }

    /// The table is the authority: an edit lands as a row, a second add is
    /// not an insert, the load projects every row, a remove deletes it.
    #[tokio::test]
    async fn the_table_round_trips_and_projects() {
        require_test_db!(Some(pool) = crate::db::scratch::pool(2).await);
        let tag = uuid::Uuid::new_v4().simple().to_string();
        let (a, b) = (format!("bafy-deny-{tag}-a"), format!("bafy-deny-{tag}-b"));
        assert!(add(&pool, &a, "0xadmin").await.unwrap());
        assert!(!add(&pool, &a, "0xadmin").await.unwrap(), "already listed");
        assert!(add(&pool, &b, "0xadmin").await.unwrap());
        let loaded = load(&pool).await.unwrap();
        assert!(loaded.contains(&a) && loaded.contains(&b));
        let row: (String, chrono::NaiveDateTime) =
            sqlx::query_as("SELECT added_by, added_at FROM content.denylist WHERE id = $1")
                .bind(&a)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.0, "0xadmin");
        assert!(
            (chrono::Utc::now().naive_utc() - row.1).num_seconds().abs() < 60,
            "added_at is UTC"
        );
        assert!(remove(&pool, &a).await.unwrap());
        assert!(!remove(&pool, &a).await.unwrap());
        let err = sqlx::query("INSERT INTO content.denylist (id, added_by) VALUES ('', 'x')")
            .execute(&pool)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("denylist_id_check"), "{err}");
        remove(&pool, &b).await.unwrap();
        pool.close().await;
    }
}
