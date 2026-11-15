//! Community federation feeds.

use super::*;

fn hash_all(hashes: &[String]) -> String {
    let mut h = Sha256::new();
    for s in hashes {
        h.update(s.as_bytes());
    }
    hex::encode(h.finalize())
}

pub async fn snapshot_view(pool: &PgPool) -> sqlx::Result<Value> {
    let latest = |kinds: &'static [&'static str]| async move {
        sqlx::query_scalar::<_, Option<i64>>(
            "SELECT max(seq) FROM social.community_signed_actions a WHERE kind = ANY($1) AND a.public_at_entry",
        )
        .bind(kinds)
        .fetch_one(pool)
        .await
        .map(|v| v.unwrap_or(0))
    };
    let (role, posts, places) = (
        latest(&["create", "role"]).await?,
        latest(&["post"]).await?,
        latest(&["place"]).await?,
    );
    let communities: Vec<(String, String, Option<String>, i64)> = sqlx::query_as(
        "SELECT a.community_id, a.signer, \
           CASE WHEN COALESCE(c.private, false) THEN a.payload->>'name' ELSE COALESCE(c.name, a.payload->>'name') END, a.signed_at \
         FROM social.community_signed_actions a \
         LEFT JOIN social.communities c ON c.id = a.community_uuid WHERE a.kind = 'create' AND a.public_at_entry ORDER BY a.seq LIMIT $1",
    )
    .bind(SNAPSHOT_COMMUNITIES)
    .fetch_all(pool)
    .await?;
    let role_hashes: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT signature_hash FROM social.community_signed_actions a WHERE kind IN ('create', 'role') AND {PUBLIC_ONLY} ORDER BY signature_hash"))).fetch_all(pool).await?;
    Ok(json!({
        "latest_role_seq": role,
        "latest_posts_seq": posts,
        "latest_places_seq": places,
        "communities": communities.into_iter().map(|(community_id, creator, name, signed_at)| json!({ "community_id": community_id, "creator": creator, "name": name.unwrap_or_default(), "signed_at": signed_at })).collect::<Vec<_>>(),
        "role_log_hash": hash_all(&role_hashes),
    }))
}

/// `GET /federation/communities/snapshot`.
pub async fn snapshot(State(state): State<SharedState>) -> Response {
    snapshot_view(&state.pool)
        .await
        .map(|v| Json(v).into_response())
        .unwrap_or_else(db_err)
}

#[derive(Debug, Deserialize)]
pub struct ChangesQuery {
    #[serde(default)]
    pub since: i64,
    #[serde(default)]
    pub limit: Option<i64>,
}

pub async fn changes_view(pool: &PgPool, since: i64, limit: i64) -> sqlx::Result<Value> {
    let limit = limit.clamp(1, MAX_CHANGES);
    let rows = |kinds: &'static [&'static str]| async move {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT signature_hash, community_id, kind, signer, target, role, action, content_hash, signed_at, payload, seq \
             FROM social.community_signed_actions a WHERE kind = ANY($1) AND seq > $2 AND {PUBLIC_ONLY} ORDER BY seq ASC LIMIT $3"
        )))
        .bind(kinds)
        .bind(since)
        .bind(limit)
        .fetch_all(pool)
        .await
    };
    let role: Vec<Value> = rows(&["create", "role"])
        .await?
        .iter()
        .map(|r| {
            let (community_id, signer, kind): (String, String, String) = (r.get("community_id"), r.get("signer"), r.get("kind"));
            let payload = if kind == "create" { json!({ "type": "CommunityCreate", "community_id": community_id, "creator": signer, "role": "owner" }) } else { r.get("payload") };
            json!({
                "kind": "role",
                "signature_hash": r.get::<String, _>("signature_hash"),
                "community_id": community_id,
                "signer": signer,
                "target": r.get::<Option<String>, _>("target"),
                "role": r.get::<Option<String>, _>("role"),
                "signed_at": r.get::<i64, _>("signed_at"),
                "payload": payload,
                "seq": r.get::<i64, _>("seq"),
            })
        })
        .collect();
    let posts: Vec<Value> = rows(&["post"])
        .await?
        .iter()
        .map(|r| {
            json!({
                "kind": "post",
                "signature_hash": r.get::<String, _>("signature_hash"),
                "community_id": r.get::<String, _>("community_id"),
                "author": r.get::<String, _>("signer"),
                "content_hash": r.get::<Option<String>, _>("content_hash"),
                "signed_at": r.get::<i64, _>("signed_at"),
                "seq": r.get::<i64, _>("seq"),
            })
        })
        .collect();
    let places: Vec<Value> = rows(&["place"])
        .await?
        .iter()
        .map(|r| {
            json!({
                "kind": "place",
                "signature_hash": r.get::<String, _>("signature_hash"),
                "community_id": r.get::<String, _>("community_id"),
                "place_id": r.get::<Option<String>, _>("target"),
                "action": r.get::<Option<String>, _>("action"),
                "signer": r.get::<String, _>("signer"),
                "signed_at": r.get::<i64, _>("signed_at"),
                "seq": r.get::<i64, _>("seq"),
            })
        })
        .collect();
    Ok(json!({ "role": role, "posts": posts, "places": places }))
}

/// `GET /federation/communities/changes?since=&limit=` (500 by default, 1..=5000 per feed).
pub async fn changes(State(state): State<SharedState>, Query(q): Query<ChangesQuery>) -> Response {
    changes_view(&state.pool, q.since, q.limit.unwrap_or(DEFAULT_CHANGES))
        .await
        .map(|v| Json(v).into_response())
        .unwrap_or_else(db_err)
}
