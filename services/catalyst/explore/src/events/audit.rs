//! Audit preparation shared by legacy event, profile and schedule writes.
use axum::http::HeaderMap;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::fed;
use crate::shared::errors::ApiError;

#[derive(Clone, Copy)]
pub(super) enum SignedWrite {
    Event,
    Profile,
    Schedule,
}

impl SignedWrite {
    fn noun(self) -> &'static str {
        match self {
            Self::Event => "Event",
            Self::Profile => "Profile",
            Self::Schedule => "Schedule",
        }
    }

    fn action_type(self) -> &'static str {
        match self {
            Self::Event => "EventPrivilegedWrite",
            Self::Profile => "ProfileSettingsUpdate",
            Self::Schedule => "ScheduleUpsert",
        }
    }
}

/// The audit row of a plain signed-fetch write, bound to its exact bytes
/// when the client signed `body_sha256` and replay-protected when it signed
/// a `nonce` (neither is required). The hash of a request
/// without a nonce is its signer, timestamp and body, so an identical resend
/// is logged once and never refused.
#[allow(clippy::too_many_arguments)]
pub(super) async fn record_signed_request(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    headers: &HeaderMap,
    signer: &str,
    subject: &str,
    method: &str,
    path: &str,
    body: &[u8],
    kind: SignedWrite,
) -> Result<(), ApiError> {
    let noun = kind.noun();
    let metadata: Value = headers
        .get("x-identity-metadata")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| serde_json::from_str(h).ok())
        .unwrap_or_else(|| json!({}));
    let digest = hex::encode(Sha256::digest(body));
    if let Some(expected) = metadata.get("body_sha256").and_then(Value::as_str)
        && expected != digest
    {
        return Err(ApiError::bad_request(format!(
            "{noun} body must match signed body_sha256"
        )));
    }
    let nonce = match metadata.get("nonce") {
        None | Some(Value::Null) => None,
        Some(nonce) => Some(
            nonce
                .as_str()
                .filter(|nonce| {
                    uuid::Uuid::parse_str(nonce).is_ok_and(|id| id.to_string() == *nonce)
                })
                .ok_or_else(|| {
                    ApiError::bad_request(format!(
                        "Signed {} nonce must be a canonical UUID",
                        noun.to_ascii_lowercase()
                    ))
                })?,
        ),
    };
    let timestamp = headers
        .get("x-identity-timestamp")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.parse::<i64>().ok())
        .ok_or_else(|| ApiError::bad_request("Invalid timestamp"))?;
    let tag = format!("legacy-{}", noun.to_ascii_lowercase());
    let hash = hex::encode(Sha256::digest(match nonce {
        Some(nonce) => format!("{tag}:{signer}:{nonce}"),
        None => format!("{tag}:{signer}:{timestamp}:{method}:{path}:{digest}"),
    }));
    let mut payload = json!({"method":method,"path":path,"body_sha256":digest,"nonce":nonce});
    if !matches!(kind, SignedWrite::Event) {
        payload["body"] = serde_json::from_slice::<Value>(body).unwrap_or(Value::Null);
    }
    let fresh = fed::record_action(
        tx,
        &fed::Action {
            scope: fed::Scope::Events,
            signature_hash: &hash,
            signer,
            subject_id: Some(subject),
            action_type: kind.action_type(),
            payload: &payload,
            signed_at: timestamp / 1000,
        },
    )
    .await?;
    if !fresh && nonce.is_some() {
        return Err(ApiError::http(
            409,
            format!("{noun} request already applied"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::to_bytes, http::StatusCode, response::IntoResponse};

    const BODY: &[u8] = b"{ \"x\": 1 }\n";
    const DIGEST: &str = "a8e2131cfaf51b3a525a74979e4e9dbec75c29e42a0fe070dd816692eece5cd8";
    const NONCE: &str = "12345678-1234-1234-1234-123456789abc";

    fn headers(metadata: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-identity-timestamp", "1700000000123".parse().unwrap());
        headers.insert("x-identity-metadata", metadata.parse().unwrap());
        headers
    }

    async fn error(error: ApiError, status: StatusCode, message: &str) {
        let response = error.into_response();
        assert_eq!(response.status(), status);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap()["error"],
            message
        );
    }

    #[tokio::test]
    async fn exact_body_keys_and_payloads_preserve_each_write_kind() {
        let Some(pool) = crate::db::scratch::pool(2).await else {
            return;
        };
        let mut tx = pool.begin().await.unwrap();
        for (kind, expected) in [
            (
                SignedWrite::Event,
                "61aaba75cbe466001bead6591498101fdba7e413fe24886e176d176ee9a96caa",
            ),
            (
                SignedWrite::Profile,
                "647a80cf84980d0cc8603c4b1424e2bf1668c3430b5292719eb184f05c797328",
            ),
            (
                SignedWrite::Schedule,
                "d218cff2bb314fd78b6a654abc74641285f5ecda410874f3d5aecbb7bbc7e216",
            ),
        ] {
            let headers = headers(&json!({"body_sha256": DIGEST}).to_string());
            for _ in 0..2 {
                record_signed_request(
                    &mut tx, &headers, "0xabc", "subject", "patch", "/signed", BODY, kind,
                )
                .await
                .unwrap();
            }
            let (hash, payload, stamp): (String, Value, i64) = sqlx::query_as("SELECT signature_hash,message_payload,signed_at FROM explore.signed_actions WHERE signer='0xabc' AND action_type=$1")
                .bind(kind.action_type()).fetch_one(&mut *tx).await.unwrap();
            assert_eq!(hash, expected);
            assert_eq!(stamp, 1_700_000_000);
            let mut expected_payload =
                json!({"method":"patch","path":"/signed","body_sha256":DIGEST,"nonce":null});
            if !matches!(kind, SignedWrite::Event) {
                expected_payload["body"] = json!({"x":1});
            }
            assert_eq!(payload, expected_payload);
        }
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM explore.signed_actions WHERE signer='0xabc'")
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        assert_eq!(count, 3);
        tx.rollback().await.unwrap();
    }

    #[tokio::test]
    async fn nonce_replays_refuse_and_rollback_does_not_consume_the_nonce() {
        let Some(pool) = crate::db::scratch::pool(2).await else {
            return;
        };
        let headers = headers(&json!({"nonce": NONCE}).to_string());
        for (kind, expected) in [
            (
                SignedWrite::Event,
                "6c468a1ca24b7c6e98fe126777a6c4df765c7f2538c0da3b6d57b7730b133ab5",
            ),
            (
                SignedWrite::Profile,
                "fadf176523357c6e0a5ff256597c9810414db169b74527b3300cec5ba9ea9657",
            ),
            (
                SignedWrite::Schedule,
                "c7c556570c12f5afa73220fcf76ee98e533ca80ba475749ff9a89e9e171a7961",
            ),
        ] {
            for _ in 0..2 {
                let mut tx = pool.begin().await.unwrap();
                record_signed_request(
                    &mut tx, &headers, "0xabc", "subject", "patch", "/signed", BODY, kind,
                )
                .await
                .unwrap();
                let hash: String = sqlx::query_scalar("SELECT signature_hash FROM explore.signed_actions WHERE signer='0xabc' AND action_type=$1").bind(kind.action_type()).fetch_one(&mut *tx).await.unwrap();
                assert_eq!(hash, expected);
                let replay = record_signed_request(
                    &mut tx, &headers, "0xabc", "other", "delete", "/other", b"", kind,
                )
                .await
                .unwrap_err();
                error(
                    replay,
                    StatusCode::CONFLICT,
                    &format!("{} request already applied", kind.noun()),
                )
                .await;
                tx.rollback().await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn malformed_metadata_is_ignored_but_present_constraints_are_enforced() {
        let Some(pool) = crate::db::scratch::pool(2).await else {
            return;
        };
        let mut tx = pool.begin().await.unwrap();
        for kind in [
            SignedWrite::Event,
            SignedWrite::Profile,
            SignedWrite::Schedule,
        ] {
            record_signed_request(
                &mut tx,
                &headers("not-json"),
                "0xmetadata",
                "subject",
                "patch",
                "/signed",
                BODY,
                kind,
            )
            .await
            .unwrap();
            for (metadata, message) in [
                (
                    json!({"body_sha256":"wrong"}),
                    format!("{} body must match signed body_sha256", kind.noun()),
                ),
                (
                    json!({"nonce":NONCE.to_uppercase()}),
                    format!(
                        "Signed {} nonce must be a canonical UUID",
                        kind.noun().to_ascii_lowercase()
                    ),
                ),
                (
                    json!({"nonce":5}),
                    format!(
                        "Signed {} nonce must be a canonical UUID",
                        kind.noun().to_ascii_lowercase()
                    ),
                ),
            ] {
                let failure = record_signed_request(
                    &mut tx,
                    &headers(&metadata.to_string()),
                    "0xmetadata",
                    "subject",
                    "patch",
                    "/signed",
                    BODY,
                    kind,
                )
                .await
                .unwrap_err();
                error(failure, StatusCode::BAD_REQUEST, &message).await;
            }
            let mut invalid_time = headers("{}");
            invalid_time.insert("x-identity-timestamp", "invalid".parse().unwrap());
            let failure = record_signed_request(
                &mut tx,
                &invalid_time,
                "0xmetadata",
                "subject",
                "patch",
                "/signed",
                BODY,
                kind,
            )
            .await
            .unwrap_err();
            error(failure, StatusCode::BAD_REQUEST, "Invalid timestamp").await;
        }
        tx.rollback().await.unwrap();
    }
}
