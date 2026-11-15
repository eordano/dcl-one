//! The claim flow's two captchas: the slider challenge rendered as a PNG
//! (the answer is the marker column, regenerated from the stored `answer_x`)
//! and the hCaptcha token verified server-side through the ONE outbound
//! client when `CREDITS_CAPTCHA_SECRET` is set.

const WIDTH: u32 = 160;
const HEIGHT: u32 = 60;
const ANSWER_RANGE: u32 = 6001;

const MARKER_HALF_PX: i64 = 2;

pub const CLAIM_TOLERANCE: f64 = 3.0;

pub fn answer_for_seed(seed: u64) -> f64 {
    (4000 + seed % ANSWER_RANGE as u64) as f64 / 100.0
}

fn marker_pixel(answer_percent: f64) -> u32 {
    let pct = answer_percent.clamp(0.0, 100.0);
    let px = (pct / 100.0 * (WIDTH - 1) as f64).round() as u32;
    px.min(WIDTH - 1)
}

#[cfg(test)]
fn pixel_to_percent(px: u32) -> f64 {
    (px.min(WIDTH - 1) as f64) / (WIDTH - 1) as f64 * 100.0
}

fn lcg(state: &mut u64) -> u32 {
    *state = state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (*state >> 33) as u32
}

pub fn render_png(answer_x: f64) -> Vec<u8> {
    let marker = marker_pixel(answer_x) as i64;

    let mut rng = (answer_x.max(0.0) as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(0x1234_5678_9ABC_DEF0);

    let stride = (WIDTH * 3 + 1) as usize;
    let mut raw = Vec::with_capacity(stride * HEIGHT as usize);
    for _y in 0..HEIGHT {
        raw.push(0u8);
        for col in 0..WIDTH {
            let n = lcg(&mut rng);

            let mut r = (40 + (n & 0x7f)) as u8;
            let mut g = (60 + ((n >> 7) & 0x7f)) as u8;
            let mut b = (90 + ((n >> 14) & 0x7f)) as u8;

            let d = col as i64 - marker;
            if d.abs() <= MARKER_HALF_PX {
                r = 18;
                g = 18;
                b = 24;
            } else if d.abs() <= MARKER_HALF_PX + 2 {
                r = 0xff;
                g = 0xff;
                b = 0xff;
            }
            raw.extend_from_slice(&[r, g, b]);
        }
    }

    let mut png = Vec::new();
    png.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&WIDTH.to_be_bytes());
    ihdr.extend_from_slice(&HEIGHT.to_be_bytes());
    ihdr.push(8);
    ihdr.push(2);
    ihdr.push(0);
    ihdr.push(0);
    ihdr.push(0);
    write_chunk(&mut png, b"IHDR", &ihdr);

    let idat = zlib_store(&raw);
    write_chunk(&mut png, b"IDAT", &idat);

    write_chunk(&mut png, b"IEND", &[]);
    png
}

fn write_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc_input = Vec::with_capacity(4 + data.len());
    crc_input.extend_from_slice(kind);
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
}

fn zlib_store(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(0x78);
    out.push(0x01);

    let mut pos = 0usize;
    while pos < data.len() {
        let chunk = std::cmp::min(0xffff, data.len() - pos);
        let last = pos + chunk >= data.len();
        out.push(if last { 1 } else { 0 });
        let len = chunk as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(&data[pos..pos + chunk]);
        pos += chunk;
    }
    if data.is_empty() {
        out.extend_from_slice(&[1, 0, 0, 0xff, 0xff]);
    }

    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let mut a = 1u32;
    let mut b = 0u32;
    for &byte in data {
        a = (a + byte as u32) % MOD;
        b = (b + a) % MOD;
    }
    (b << 16) | a
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xedb8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_answer_rounds_hundredths_once() {
        assert_eq!(answer_for_seed(5098), 90.98);
    }

    #[tokio::test]
    async fn first_and_refreshed_challenges_use_the_persisted_answer() {
        let Some(db) = crate::test_db::OwnDatabase::create("captcha_answer_roundtrip").await else {
            return;
        };
        let address = "0x1111111111111111111111111111111111111111";
        let now = super::super::store::now();
        // Reproduce the old generator's double rounding deterministically.
        let generated = 40.0 + 5098.0 / 100.0;
        assert_ne!(generated, 90.98);
        let Challenge::Ready(first) =
            challenge_with_generator(&db.pool, address, now, || Ok(generated))
                .await
                .unwrap()
        else {
            panic!("unexpected cooldown")
        };
        assert_eq!(first, 90.98);
        let Challenge::Ready(refreshed) = challenge_with_generator(
            &db.pool,
            address,
            now + chrono::Duration::seconds(1),
            || panic!("an unexpired challenge must be reused"),
        )
        .await
        .unwrap() else {
            panic!("unexpected cooldown")
        };
        assert_eq!(first.to_bits(), refreshed.to_bits());
        assert_eq!(render_png(first), render_png(refreshed));
        db.drop().await;
    }

    #[test]
    fn percent_pixel_percent_round_trips_within_tolerance() {
        for answer in 0..=100u32 {
            let a = answer as f64;
            let recovered = pixel_to_percent(marker_pixel(a));
            assert!(
                (a - recovered).abs() <= CLAIM_TOLERANCE,
                "answer {answer} -> pixel {} -> percent {recovered} exceeds tolerance",
                marker_pixel(a)
            );
        }
    }

    #[test]
    fn marker_spans_full_image_width() {
        assert_eq!(marker_pixel(0.0), 0);
        assert_eq!(marker_pixel(100.0), WIDTH - 1);

        let mid = marker_pixel(50.0);
        assert!(
            (mid as i64 - ((WIDTH - 1) / 2) as i64).abs() <= 1,
            "50% should map to ~center, got {mid}"
        );
    }

    #[test]
    fn marker_monotonic_and_in_bounds() {
        let mut prev = 0u32;
        for answer in 0..=100u32 {
            let px = marker_pixel(answer as f64);
            assert!(px < WIDTH, "marker {px} out of bounds for answer {answer}");
            assert!(px >= prev, "marker not monotonic at answer {answer}");
            prev = px;
        }
    }

    #[test]
    fn marker_pixel_clamps_out_of_range() {
        assert_eq!(marker_pixel(-10.0), 0);
        assert_eq!(marker_pixel(250.0), WIDTH - 1);
    }

    #[test]
    fn render_png_emits_valid_signature() {
        let png = render_png(42.0);
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    }

    #[test]
    fn render_is_deterministic_per_answer() {
        assert_eq!(render_png(30.0), render_png(30.0));
        assert_ne!(render_png(30.0), render_png(70.0));
    }

    #[test]
    fn argmax_brightness_does_not_reveal_answer() {
        let answer = 64.0;
        let marker = marker_pixel(answer) as i64;

        let mut rng = (answer as u64)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(0x1234_5678_9ABC_DEF0);
        let mut col_brightness = vec![0u64; WIDTH as usize];
        for _y in 0..HEIGHT {
            for col in 0..WIDTH {
                let n = lcg(&mut rng);
                let d = col as i64 - marker;
                let (r, g, b) = if d.abs() <= MARKER_HALF_PX {
                    (18u32, 18u32, 24u32)
                } else if d.abs() <= MARKER_HALF_PX + 2 {
                    (255, 255, 255)
                } else {
                    (
                        40 + (n & 0x7f),
                        60 + ((n >> 7) & 0x7f),
                        90 + ((n >> 14) & 0x7f),
                    )
                };
                col_brightness[col as usize] += (r + g + b) as u64;
            }
        }

        let argmax = col_brightness
            .iter()
            .enumerate()
            .max_by_key(|(_, v)| **v)
            .map(|(i, _)| i as i64)
            .unwrap();

        assert!(
            (argmax - marker).abs() > MARKER_HALF_PX,
            "argmax column {argmax} should not be the dark marker at {marker}"
        );
    }
}

use super::errors::ApiError;
use crate::http::Outbound;

/// The hCaptcha verifier (`CREDITS_CAPTCHA_SECRET`, `CREDITS_CAPTCHA_VERIFY_URL`):
/// one `siteverify` form POST per claim through the guarded client, the
/// secret never logged.
#[derive(Clone)]
pub struct CaptchaProvider {
    secret: String,
    verify_url: String,
    client: Outbound,
}

#[derive(serde::Deserialize)]
struct SiteVerifyResponse {
    #[serde(default)]
    success: bool,
}

pub fn parse_verdict(body: &str) -> bool {
    serde_json::from_str::<SiteVerifyResponse>(body)
        .map(|r| r.success)
        .unwrap_or(false)
}

/// `application/x-www-form-urlencoded`, the encoding hCaptcha's `siteverify` takes.
pub fn form_encode(pairs: &[(&str, &str)]) -> String {
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        ser.append_pair(k, v);
    }
    ser.finish()
}

impl CaptchaProvider {
    pub fn new(secret: String, verify_url: String, client: Outbound) -> Self {
        Self {
            secret,
            verify_url,
            client,
        }
    }

    pub async fn verify(&self, token: &str, remoteip: Option<&str>) -> Result<bool, ApiError> {
        let mut form = vec![("secret", self.secret.as_str()), ("response", token)];
        if let Some(ip) = remoteip {
            form.push(("remoteip", ip));
        }

        let resp = self
            .client
            .post(&self.verify_url)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(form_encode(&form))
            .send()
            .await
            .map_err(|e| ApiError::Internal(format!("captcha provider request failed: {e}")))?;

        if !resp.status().is_success() {
            return Err(ApiError::Internal(format!(
                "captcha provider returned status {}",
                resp.status().as_u16()
            )));
        }

        let body = resp
            .text()
            .await
            .map_err(|e| ApiError::Internal(format!("captcha provider read failed: {e}")))?;

        Ok(parse_verdict(&body))
    }
}

#[cfg(test)]
mod verdict_tests {
    use super::*;

    #[test]
    fn verdict_true_on_success() {
        assert!(parse_verdict(r#"{"success":true}"#));
        assert!(parse_verdict(
            r#"{"success":true,"challenge_ts":"2026-06-21T00:00:00Z","hostname":"x"}"#
        ));
    }

    #[test]
    fn verdict_false_on_failure_or_garbage() {
        assert!(!parse_verdict(r#"{"success":false}"#));
        assert!(!parse_verdict(
            r#"{"success":false,"error-codes":["invalid-input-response"]}"#
        ));
        assert!(!parse_verdict(r#"{}"#));
        assert!(!parse_verdict("not json"));
        assert!(!parse_verdict(""));
    }
}

pub enum Challenge {
    Ready(f64),
    Blocked(u64),
}

/// The cache's TTL is measured from the latest failure, rather than a rolling
/// count of failures within a day. Success clears the complete failure history.
pub async fn cooldown(
    conn: &mut sqlx::PgConnection,
    address: &str,
    now: chrono::NaiveDateTime,
) -> Result<u64, super::errors::ApiError> {
    use sqlx::Row;
    let row=sqlx::query("SELECT failed_attempts,last_failed_at FROM credits.captcha_attempts WHERE address=$1 AND last_failed_at>$2-interval '1 day'").bind(address).bind(now).fetch_optional(conn).await?;
    Ok(row
        .filter(|r| r.get::<i16, _>("failed_attempts") >= 3)
        .map(|r| {
            ((r.get::<chrono::NaiveDateTime, _>("last_failed_at") + chrono::Duration::days(1) - now)
                .num_milliseconds()
                .max(0) as u64)
                .div_ceil(1000)
        })
        .unwrap_or(0))
}

pub async fn challenge(
    pool: &sqlx::PgPool,
    address: &str,
    now: chrono::NaiveDateTime,
) -> Result<Challenge, super::errors::ApiError> {
    challenge_with_generator(pool, address, now, || {
        let mut bytes = [0u8; 8];
        getrandom::fill(&mut bytes).map_err(|_| {
            super::errors::ApiError::Internal("captcha randomness unavailable".into())
        })?;
        Ok(answer_for_seed(u64::from_le_bytes(bytes)))
    })
    .await
}

async fn challenge_with_generator(
    pool: &sqlx::PgPool,
    address: &str,
    now: chrono::NaiveDateTime,
    generate: impl FnOnce() -> Result<f64, super::errors::ApiError>,
) -> Result<Challenge, super::errors::ApiError> {
    use sqlx::Row;
    let mut tx = pool.begin().await?;
    super::goals::wallet_lock(&mut tx, address).await?;
    let remaining = cooldown(&mut tx, address, now).await?;
    if remaining > 0 {
        return Ok(Challenge::Blocked(remaining));
    }
    let previous=sqlx::query("SELECT id,answer_x::float8 AS answer FROM credits.captcha_challenges WHERE address=$1 AND consumed_at IS NULL AND expires_at>$2 ORDER BY id DESC LIMIT 1 FOR UPDATE").bind(address).bind(now).fetch_optional(&mut *tx).await?;
    let expires = now + chrono::Duration::hours(2);
    let (id, answer) = if let Some(row) = previous {
        let id: i64 = row.get("id");
        let answer: f64 = row.get("answer");
        sqlx::query(
            "UPDATE credits.captcha_challenges SET generated_at=$2,expires_at=$3 WHERE id=$1",
        )
        .bind(id)
        .bind(now)
        .bind(expires)
        .execute(&mut *tx)
        .await?;
        (id, answer)
    } else {
        let answer = generate()?;
        // Render and return the same value that later GETs and solves read:
        // PostgreSQL's float8-to-NUMERIC conversion can round the input.
        sqlx::query_as::<_, (i64, f64)>("INSERT INTO credits.captcha_challenges(address,answer_x,generated_at,expires_at) VALUES($1,$2,$3,$4) RETURNING id,answer_x::float8")
            .bind(address).bind(answer).bind(now).bind(expires).fetch_one(&mut *tx).await?
    };
    sqlx::query("UPDATE credits.captcha_challenges SET consumed_at=$3 WHERE address=$1 AND consumed_at IS NULL AND id<>$2").bind(address).bind(id).bind(now).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Challenge::Ready(answer))
}

pub async fn solve(
    pool: &sqlx::PgPool,
    address: &str,
    submitted: f64,
    now: chrono::NaiveDateTime,
) -> Result<(bool, bool), super::errors::ApiError> {
    use sqlx::Row;
    let mut tx = pool.begin().await?;
    super::goals::wallet_lock(&mut tx, address).await?;
    if cooldown(&mut tx, address, now).await? > 0 {
        return Ok((false, true));
    }
    let row=sqlx::query("SELECT answer_x::float8 AS answer,generated_at FROM credits.captcha_challenges WHERE address=$1 AND consumed_at IS NULL AND expires_at>$2 ORDER BY id DESC LIMIT 1 FOR UPDATE").bind(address).bind(now).fetch_optional(&mut *tx).await?;
    let Some(row) = row else {
        return Ok((false, false));
    };
    // The source compares Date.now() values. PostgreSQL truncates timestamp
    // nanoseconds to microseconds, so subtracting full-precision Rust times
    // could accept an exactly-one-second solve after the storage round trip.
    let generated_at = row.get::<chrono::NaiveDateTime, _>("generated_at");
    let elapsed_ms = now.and_utc().timestamp_millis() - generated_at.and_utc().timestamp_millis();
    let success =
        (row.get::<f64, _>("answer") - submitted).abs() < CLAIM_TOLERANCE && elapsed_ms > 1000;
    sqlx::query("UPDATE credits.captcha_challenges SET consumed_at=$2 WHERE address=$1 AND consumed_at IS NULL").bind(address).bind(now).execute(&mut *tx).await?;
    let blocked = if success {
        sqlx::query("DELETE FROM credits.captcha_attempts WHERE address=$1")
            .bind(address)
            .execute(&mut *tx)
            .await?;
        false
    } else {
        let attempts:i16=sqlx::query_scalar("INSERT INTO credits.captcha_attempts(address,failed_attempts,last_failed_at) VALUES($1,1,$2) ON CONFLICT(address) DO UPDATE SET failed_attempts=CASE WHEN credits.captcha_attempts.last_failed_at<=$2-interval '1 day' THEN 1 ELSE LEAST(3,credits.captcha_attempts.failed_attempts+1) END,last_failed_at=$2 RETURNING failed_attempts").bind(address).bind(now).fetch_one(&mut *tx).await?;
        attempts >= 3
    };
    tx.commit().await?;
    Ok((success, blocked))
}
