//! Community federation content.

use super::*;

fn content_hash(body: &[u8]) -> String {
    hex::encode(Sha256::digest(body))
}

/// The blob of a body a public-at-entry post names and its signer uploaded.
async fn published_cid(pool: &PgPool, hash: &str) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT cc.cid FROM social.community_contents cc WHERE cc.content_hash = $1 \
           AND EXISTS (SELECT 1 FROM social.community_signed_actions a \
                       JOIN social.community_content_uploads u \
                         ON u.content_hash = a.content_hash AND u.uploader = a.signer \
                       WHERE a.content_hash = cc.content_hash AND a.kind = 'post' AND {PUBLIC_ONLY})"
    )))
    .bind(hash)
    .fetch_optional(pool)
    .await
}

/// A body `uploader` uploaded, the only one their post may take.
pub(super) async fn uploaded_body(
    state: &CommunitiesState,
    conn: &mut PgConnection,
    hash: &str,
    uploader: &Address,
) -> sqlx::Result<Option<Bytes>> {
    if !is_valid_hash(hash) {
        return Ok(None);
    }
    let cid: Option<String> = sqlx::query_scalar(
        "SELECT cc.cid FROM social.community_contents cc \
         JOIN social.community_content_uploads u ON u.content_hash = cc.content_hash \
         WHERE cc.content_hash = $1 AND u.uploader = $2",
    )
    .bind(hash)
    .bind(uploader)
    .fetch_optional(conn)
    .await?;
    Ok(match cid {
        Some(cid) => state.storage.retrieve(&cid).await.ok().flatten(),
        None => None,
    })
}

/// `POST /federation/communities/content` (signed by a community owner or
/// moderator): stores a post body and registers its sha256 (the wire's
/// content hash) against the blob. Bodies no post names are collected by
/// `community-content-gc`.
pub async fn put_content(
    State(state): State<SharedState>,
    caller: super::super::auth::Caller,
    headers: HeaderMap,
    original: OriginalUri,
    body: Bytes,
) -> Response {
    let signer = match caller
        .require_signer(
            &state,
            &headers,
            "post",
            &super::super::signed_path(&original, "/federation/communities/content"),
        )
        .await
    {
        Ok(signer) => signer,
        Err(e) => return e.into_response(),
    };
    if body.len() > MAX_CONTENT_BYTES {
        return err_json(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("body exceeds {MAX_CONTENT_BYTES} bytes"),
        );
    }
    match std::str::from_utf8(&body) {
        Ok(raw) => {
            if let Err(refused) = super::super::posts::validate_text(raw) {
                return refused;
            }
        }
        Err(_) => return bad("content body is not valid UTF-8"),
    }
    match is_poster(&state.pool, &signer).await {
        Ok(true) => {}
        Ok(false) => {
            return err_json(
                StatusCode::FORBIDDEN,
                "only a community owner or moderator may upload post content",
            );
        }
        Err(e) => return db_err(e),
    }
    if CONTENT_LIMITER.check(signer.as_str()) == RateLimitDecision::Deny {
        return err_json(StatusCode::TOO_MANY_REQUESTS, "rate limit exceeded");
    }
    let (hash, size) = (content_hash(&body), body.len());
    let for_cid = body.clone();
    let cid = match tokio::task::spawn_blocking(move || crate::auth::hash::hash_bytes_v1(&for_cid))
        .await
    {
        Ok(cid) => cid,
        Err(_) => return err_json(StatusCode::INTERNAL_SERVER_ERROR, "io error"),
    };
    match register_upload(&state, &hash, &cid, body, &signer).await {
        Ok(None) => Json(json!({ "ok": true, "content_hash": hash, "size": size })).into_response(),
        Ok(Some(refused)) => refused,
        Err(e) => match e.downcast::<sqlx::Error>() {
            Ok(db) => upload_lock_err(db),
            Err(e) => {
                tracing::error!(error = %e, "community content store io error");
                err_json(StatusCode::INTERNAL_SERVER_ERROR, "io error")
            }
        },
    }
}

/// A wait on the uploads lock past its timeout is a retryable 503.
pub(super) fn upload_lock_err(e: sqlx::Error) -> Response {
    if e.as_database_error()
        .and_then(|d| d.code())
        .is_some_and(|code| code == "55P03")
    {
        return err_json(StatusCode::SERVICE_UNAVAILABLE, "content store busy; retry");
    }
    db_err(e)
}

/// Serializes uploads with the collector's deletes until the transaction ends.
/// The wait is bounded so queued writers cannot hold pool connections long.
pub(super) async fn lock_uploads(conn: &mut PgConnection) -> sqlx::Result<()> {
    sqlx::query("SET LOCAL lock_timeout = '2s'")
        .execute(&mut *conn)
        .await?;
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended('social.community_content_uploads', 0))",
    )
    .execute(conn)
    .await
    .map(|_| ())
}

/// Stores and registers the body and binds it to its uploader under the
/// collector's lock, within the per-uploader and global budgets of unposted
/// bodies. A repeated upload re-stores a lost blob and refreshes both rows;
/// a body whose post by this uploader arrived first is bound as posted.
async fn register_upload(
    state: &CommunitiesState,
    hash: &str,
    cid: &str,
    body: Bytes,
    uploader: &Address,
) -> anyhow::Result<Option<Response>> {
    let size = i32::try_from(body.len())?;
    let mut tx = state.pool.begin().await?;
    lock_uploads(&mut tx).await?;
    let (mine, all): (i64, i64) = sqlx::query_as(
        "SELECT COALESCE(sum(size) FILTER (WHERE uploader = $1), 0)::bigint, COALESCE(sum(size), 0)::bigint \
         FROM social.community_content_uploads \
         WHERE posted_at IS NULL AND NOT (content_hash = $2 AND uploader = $1)",
    )
    .bind(uploader)
    .bind(hash)
    .fetch_one(&mut *tx)
    .await?;
    let size_i64 = i64::from(size);
    if mine + size_i64 > MAX_UPLOADER_UNPOSTED_BYTES || all + size_i64 > MAX_UNPOSTED_CONTENT_BYTES
    {
        return Ok(Some(err_json(
            StatusCode::TOO_MANY_REQUESTS,
            "content upload budget exhausted; retry later",
        )));
    }
    if !state.storage.exist(cid).await? {
        state.storage.store(cid, body).await?;
    }
    sqlx::query(
        "INSERT INTO social.community_contents (content_hash, cid, size) VALUES ($1, $2, $3) \
                 ON CONFLICT (content_hash) DO UPDATE SET stored_at = timezone('UTC', now())",
    )
    .bind(hash)
    .bind(cid)
    .bind(size)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO social.community_content_uploads (content_hash, uploader, size, posted_at) \
         VALUES ($1, $2, $3, CASE WHEN EXISTS (SELECT 1 FROM social.community_signed_actions a \
                 WHERE a.content_hash = $1 AND a.kind = 'post' AND a.signer = $2) THEN timezone('UTC', now()) END) \
         ON CONFLICT (content_hash, uploader) DO UPDATE SET uploaded_at = timezone('UTC', now()), \
           posted_at = COALESCE(social.community_content_uploads.posted_at, EXCLUDED.posted_at)",
    )
        .bind(hash)
        .bind(uploader)
        .bind(size)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(None)
}

/// Only a wallet that may post somewhere uploads.
async fn is_poster(pool: &PgPool, signer: &Address) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM social.community_members m \
           JOIN social.communities c ON c.id = m.community_id \
           WHERE m.member_address = $1 AND m.role IN ('owner', 'moderator') \
             AND c.active AND NOT c.suspended)",
    )
    .bind(signer)
    .fetch_one(pool)
    .await
}

/// `GET /federation/communities/content/{hash}`: the body of a public post.
pub async fn get_content(State(state): State<SharedState>, Path(hash): Path<String>) -> Response {
    let hash = hash.to_ascii_lowercase();
    if !is_valid_hash(&hash) {
        return bad(format!("invalid content hash: {hash}"));
    }
    let cid = match published_cid(&state.pool, &hash).await {
        Ok(Some(cid)) => cid,
        Ok(None) => return err_json(StatusCode::NOT_FOUND, "content not found"),
        Err(e) => return db_err(e),
    };
    match state.storage.retrieve(&cid).await {
        Ok(Some(bytes)) => {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
            headers.insert(
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            );
            if let Ok(v) = HeaderValue::from_str(&hash) {
                headers.insert("x-content-hash", v);
            }
            (StatusCode::OK, headers, bytes).into_response()
        }
        Ok(None) => err_json(StatusCode::NOT_FOUND, "content not found"),
        Err(e) => {
            tracing::error!(error = %e, "community content store io error");
            err_json(StatusCode::INTERNAL_SERVER_ERROR, "io error")
        }
    }
}

/// `POST /federation/communities/content/gc` (the admin bearer): drops the
/// registered bodies no logged post names, once older than the blob
/// collector's age floor, and their blobs when no other domain holds them.
pub async fn gc_content(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !state.admin_bearer(&headers) {
        return err_json(StatusCode::UNAUTHORIZED, "admin bearer token required");
    }
    match collect(&state).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "community content gc failed");
            err_json(StatusCode::INTERNAL_SERVER_ERROR, "db error")
        }
    }
}

pub(super) async fn collect(state: &CommunitiesState) -> anyhow::Result<Value> {
    let floor = chrono::Utc::now().naive_utc()
        - chrono::Duration::from_std(crate::storage::gc::MIN_ORPHAN_AGE)?;
    let scanned: i64 = sqlx::query_scalar("SELECT count(*) FROM social.community_contents")
        .fetch_one(&state.pool)
        .await?;
    let referenced: i64 = sqlx::query_scalar("SELECT count(DISTINCT content_hash) FROM social.community_signed_actions WHERE content_hash IS NOT NULL").fetch_one(&state.pool).await?;
    let mut tx = state.pool.begin().await?;
    lock_uploads(&mut tx).await?;
    sqlx::query(
        "DELETE FROM social.community_content_uploads WHERE posted_at IS NULL AND uploaded_at < $1",
    )
    .bind(floor)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let mut removed = 0i64;
    loop {
        let mut tx = state.pool.begin().await?;
        lock_uploads(&mut tx).await?;
        let dropped: Vec<String> = sqlx::query_scalar(
            "DELETE FROM social.community_contents WHERE content_hash IN ( \
               SELECT cc.content_hash FROM social.community_contents cc \
               WHERE cc.stored_at < $1 \
                 AND NOT EXISTS (SELECT 1 FROM social.community_signed_actions a WHERE a.content_hash = cc.content_hash) \
                 AND NOT EXISTS (SELECT 1 FROM social.community_content_uploads u WHERE u.content_hash = cc.content_hash) \
               ORDER BY cc.content_hash LIMIT $2) \
             RETURNING cid",
        )
        .bind(floor)
        .bind(GC_CHUNK)
        .fetch_all(&mut *tx)
        .await?;
        if dropped.is_empty() {
            break;
        }
        removed += dropped.len() as i64;
        let held = crate::storage::gc::referenced_among_on(
            &mut tx,
            crate::storage::gc::Pins::new(&state.storage, &[]),
            &dropped,
        )
        .await?;
        for cid in dropped.iter().filter(|c| !held.contains(*c)) {
            let registered_again: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM social.community_contents WHERE cid = $1)",
            )
            .bind(cid)
            .fetch_one(&mut *tx)
            .await?;
            if !registered_again {
                state.storage.delete_strict(cid).await?;
            }
        }
        tx.commit().await?;
    }
    Ok(
        json!({ "ok": true, "scanned": scanned, "kept": scanned - removed, "removed": removed, "referenced": referenced }),
    )
}

/// `community-nonce-sweep`: drops the nonces whose replay window closed;
/// `community-content-gc`: drops bodies no post names (see [`collect`]).
pub fn spawn_job(state: &SharedState, jobs: &Jobs) {
    let content = state.clone();
    let cfg = PeriodicCfg::every(Duration::from_secs(900))
        .with_jitter(Duration::from_secs(60))
        .with_error_backoff(Duration::from_secs(1800));
    jobs.spawn_periodic_leased("community-content-gc", cfg, move |_ctx| {
        let state = content.clone();
        async move {
            collect(&state).await?;
            Ok(())
        }
    });
    let pool = state.pool.clone();
    let cfg = PeriodicCfg::every(Duration::from_secs(300))
        .with_jitter(Duration::from_secs(30))
        .with_error_backoff(Duration::from_secs(900));
    jobs.spawn_periodic_leased("community-nonce-sweep", cfg, move |_ctx| {
        let pool = pool.clone();
        async move {
            crate::fed::sweep_nonces(&pool, NONCES, chrono::Utc::now().timestamp()).await?;
            Ok(())
        }
    });
}
