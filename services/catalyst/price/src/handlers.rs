//! `GET /api/v3/simple/price` in CoinGecko's shape over the newest snapshot,
//! overlaid by the operator's overrides, and the bearer-guarded override
//! console. A request for MANA with no snapshot and no override answers the
//! unavailable envelope: an empty quote object is not a price.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::header::{
    AGE, CACHE_CONTROL, CONTENT_TYPE, HeaderValue, RETRY_AFTER, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Map, Number, Value, json};

use super::store::{PriceOverride, PriceSnapshot, SNAPSHOT_MAX_AGE};
use super::{MANA_ID, SharedState, unavailable};

struct PriceQuery {
    ids: Vec<String>,
    vs: Vec<String>,
    include_market_cap: bool,
    include_24hr_vol: bool,
    include_24hr_change: bool,
    include_last_updated_at: bool,
    precision: Option<u32>,
}

#[derive(Debug)]
pub enum PriceError {
    MissingVsCurrencies,
    InvalidPrecision,
    Internal,
}

impl IntoResponse for PriceError {
    fn into_response(self) -> Response {
        match self {
            PriceError::MissingVsCurrencies => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "error": "Missing parameter vs_currencies" })),
            )
                .into_response(),
            PriceError::InvalidPrecision => (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "Invalid value for precision" })),
            )
                .into_response(),
            PriceError::Internal => {
                let body = Json(
                    json!({ "status": { "error_code": 500, "error_message": "Internal Server Error" } }),
                );
                let mut resp = (StatusCode::INTERNAL_SERVER_ERROR, body).into_response();
                resp.headers_mut()
                    .insert(RETRY_AFTER, HeaderValue::from_static("30"));
                resp
            }
        }
    }
}

fn parse_bool(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1")
}

fn parse_query(pairs: Vec<(String, String)>) -> Result<PriceQuery, PriceError> {
    let mut q = PriceQuery {
        ids: Vec::new(),
        vs: Vec::new(),
        include_market_cap: false,
        include_24hr_vol: false,
        include_24hr_change: false,
        include_last_updated_at: false,
        precision: None,
    };
    let mut saw_vs = false;
    let split = |v: &str| -> Vec<String> {
        v.split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect()
    };
    for (k, v) in pairs {
        match k.as_str() {
            "ids" => q.ids.extend(split(&v)),
            "vs_currencies" => {
                saw_vs = true;
                q.vs.extend(split(&v));
            }
            "include_market_cap" => q.include_market_cap = parse_bool(&v),
            "include_24hr_vol" => q.include_24hr_vol = parse_bool(&v),
            "include_24hr_change" => q.include_24hr_change = parse_bool(&v),
            "include_last_updated_at" => q.include_last_updated_at = parse_bool(&v),
            "precision" => {
                let raw = v.trim();
                if raw.eq_ignore_ascii_case("full") {
                    q.precision = None;
                } else if let Ok(n) = raw.parse::<i64>() {
                    if !(0..=18).contains(&n) {
                        return Err(PriceError::InvalidPrecision);
                    }
                    q.precision = Some(n as u32);
                }
            }
            _ => {}
        }
    }
    if !saw_vs || q.vs.is_empty() {
        return Err(PriceError::MissingVsCurrencies);
    }
    Ok(q)
}

pub async fn simple_price(
    State(state): State<SharedState>,
    Query(pairs): Query<Vec<(String, String)>>,
) -> Result<Response, PriceError> {
    let q = parse_query(pairs)?;

    let now = chrono::Utc::now();
    let newest = state.prices.latest().await.map_err(|err| {
        tracing::error!(%err, "failed to read latest price snapshot");
        PriceError::Internal
    })?;
    // A snapshot past the freshness budget is no quote: a dead poller must
    // not degrade into a weeks-old price under a 30 s cache header.
    let (snapshot, stale_age) = match newest {
        Some(snap) if snap.is_fresh_at(now) => (Some(snap), None),
        Some(snap) => (None, Some(snap.age_at(now))),
        None => (None, None),
    };
    let overrides = state
        .overrides
        .for_pairs(&q.ids, &q.vs)
        .await
        .map_err(|err| {
            tracing::error!(%err, "failed to read the price overrides");
            PriceError::Internal
        })?;

    let mut out = Map::new();
    for id in &q.ids {
        let mut inner = Map::new();
        if id == MANA_ID
            && let Some(snap) = snapshot.as_deref()
        {
            mana_quote(&mut inner, snap, &q);
        }
        for o in overrides.iter().filter(|o| o.token_id == *id) {
            if let Some(v) = o
                .value
                .parse::<f64>()
                .ok()
                .and_then(|raw| number(raw, q.precision))
            {
                inner.insert(o.vs_currency.clone(), v);
            }
        }
        if id == MANA_ID
            && inner.is_empty()
            && !q
                .vs
                .iter()
                .any(|cur| ["usd", "eth", "btc"].contains(&cur.as_str()))
        {
            // A currency the archive never held: the old empty object.
            out.insert(id.clone(), Value::Object(inner));
            continue;
        }
        if inner.is_empty() {
            if id == MANA_ID {
                return Ok(match stale_age {
                    Some(age) => unavailable(&format!(
                        "the newest MANA spot on this node is {} s old, past the {} s freshness budget (the mana-price-poll job is off or failing), and no override is set",
                        age.as_secs(),
                        SNAPSHOT_MAX_AGE.as_secs()
                    )),
                    None => unavailable(
                        "no MANA spot has been taken on this node yet and no override is set",
                    ),
                });
            }
            continue;
        }
        out.insert(id.clone(), Value::Object(inner));
    }

    let mut resp = Json(Value::Object(out)).into_response();
    let headers = resp.headers_mut();
    headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=utf-8"),
    );
    headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static("max-age=30, public, must-revalidate, s-maxage=60"),
    );
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    if let Some(snap) = &snapshot
        && let Ok(age) = HeaderValue::from_str(&snap.age_at(now).as_secs().to_string())
    {
        // The quote's age on this node, so a caller can fail closed before the budget does.
        headers.insert(AGE, age);
    }
    Ok(resp)
}

fn mana_quote(inner: &mut Map<String, Value>, snap: &PriceSnapshot, q: &PriceQuery) {
    for cur in &q.vs {
        if let Some(num) = spot(snap, cur, q.precision) {
            inner.insert(cur.clone(), num);
            if q.include_market_cap
                && let Some(v) = usd_only(cur, snap.market_cap_usd, q.precision)
            {
                inner.insert(format!("{cur}_market_cap"), v);
            }
            if q.include_24hr_vol
                && let Some(v) = usd_only(cur, snap.volume_24h_usd, q.precision)
            {
                inner.insert(format!("{cur}_24h_vol"), v);
            }
            if q.include_24hr_change
                && let Some(v) = usd_only(cur, snap.price_change_24h_pct, q.precision)
            {
                inner.insert(format!("{cur}_24h_change"), v);
            }
        }
    }
    if q.include_last_updated_at {
        let ts = snap.source_updated_at.unwrap_or(snap.taken_at).timestamp();
        inner.insert("last_updated_at".to_string(), Value::Number(ts.into()));
    }
}

fn spot(snap: &PriceSnapshot, cur: &str, precision: Option<u32>) -> Option<Value> {
    let raw = match cur {
        "usd" => snap.mana_usd,
        "eth" => snap.mana_eth,
        "btc" => snap.mana_btc,
        _ => None,
    }?;
    number(raw, precision)
}

fn usd_only(cur: &str, raw: Option<f64>, precision: Option<u32>) -> Option<Value> {
    if cur != "usd" {
        return None;
    }
    number(raw?, precision)
}

fn number(raw: f64, precision: Option<u32>) -> Option<Value> {
    let v = match precision {
        Some(p) => {
            let factor = 10f64.powi(p as i32);
            (raw * factor).round() / factor
        }
        None => raw,
    };
    Number::from_f64(v).map(Value::Number)
}

fn is_admin(state: &SharedState, headers: &HeaderMap) -> bool {
    crate::auth::admin_bearer::matches(state.settings.admin_token.as_deref(), headers)
}

fn forbidden() -> Response {
    (StatusCode::FORBIDDEN, Json(json!({ "error": "Forbidden" }))).into_response()
}

fn bad_request(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response()
}

fn internal(err: sqlx::Error) -> Response {
    tracing::error!(%err, "price-override store query failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "Internal Server Error" })),
    )
        .into_response()
}

fn override_json(o: &PriceOverride) -> Value {
    json!({
        "token_id": o.token_id,
        "vs_currency": o.vs_currency,
        "value": o.value,
        "note": o.note,
        "updated_by": o.updated_by,
        "updated_at": o.updated_at.to_rfc3339(),
    })
}

fn normalize(s: &str) -> String {
    s.trim().to_ascii_lowercase()
}

/// A plain decimal (optional sign, digits, at most one point), at most 78 chars.
pub(crate) fn validate_decimal(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() || s.len() > 78 {
        return None;
    }
    let body = s.strip_prefix(['+', '-']).unwrap_or(s);
    let mut seen_dot = false;
    let mut any_digit = false;
    for c in body.chars() {
        match c {
            '0'..='9' => any_digit = true,
            '.' if !seen_dot => seen_dot = true,
            _ => return None,
        }
    }
    any_digit.then(|| s.to_string())
}

fn admin_identity(headers: &HeaderMap) -> String {
    headers
        .get("x-catalyrst-admin")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.chars().take(100).collect())
        .unwrap_or_else(|| "console".to_string())
}

/// The console's listing carries every override's operator identity and
/// note; the old service bound 127.0.0.1 and nginx exposed only the price
/// route, so on the public listener it takes the console bearer too.
pub async fn list_overrides(State(state): State<SharedState>, headers: HeaderMap) -> Response {
    if !is_admin(&state, &headers) {
        return forbidden();
    }
    match state.overrides.list().await {
        Ok(rows) => {
            let items: Vec<_> = rows.iter().map(override_json).collect();
            Json(json!({ "overrides": items })).into_response()
        }
        Err(err) => internal(err),
    }
}

#[derive(Deserialize)]
pub struct SetOverrideBody {
    pub value: String,
    #[serde(default)]
    pub note: Option<String>,
}

pub async fn set_override(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path((token, vs)): Path<(String, String)>,
    Json(body): Json<SetOverrideBody>,
) -> Response {
    if !is_admin(&state, &headers) {
        return forbidden();
    }
    let Some(value) = validate_decimal(&body.value) else {
        return bad_request("value must be a decimal number");
    };
    let token = normalize(&token);
    let vs = normalize(&vs);
    if token.is_empty() || vs.is_empty() {
        return bad_request("token and vs_currency are required");
    }
    let admin = admin_identity(&headers);
    match state
        .overrides
        .set(
            &token,
            &vs,
            &value,
            body.note.as_deref(),
            &admin,
            chrono::Utc::now(),
        )
        .await
    {
        Ok(o) => (StatusCode::OK, Json(override_json(&o))).into_response(),
        Err(err) => internal(err),
    }
}

pub async fn clear_override(
    State(state): State<SharedState>,
    headers: HeaderMap,
    Path((token, vs)): Path<(String, String)>,
) -> Response {
    if !is_admin(&state, &headers) {
        return forbidden();
    }
    let token = normalize(&token);
    let vs = normalize(&vs);
    let admin = admin_identity(&headers);
    match state
        .overrides
        .clear(&token, &vs, &admin, chrono::Utc::now())
        .await
    {
        Ok(removed) => (StatusCode::OK, Json(json!({ "removed": removed }))).into_response(),
        Err(err) => internal(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_validation() {
        assert_eq!(validate_decimal(" 0.25 ").as_deref(), Some("0.25"));
        assert_eq!(validate_decimal("-1").as_deref(), Some("-1"));
        assert_eq!(validate_decimal("+3.").as_deref(), Some("+3."));
        assert!(validate_decimal("").is_none());
        assert!(validate_decimal(".").is_none());
        assert!(validate_decimal("1e5").is_none());
        assert!(validate_decimal("1.2.3").is_none());
        assert!(validate_decimal(&"9".repeat(79)).is_none());
    }

    #[test]
    fn query_parsing_follows_coingecko() {
        let pairs = |s: &str| -> Vec<(String, String)> {
            s.split('&')
                .map(|p| {
                    let (k, v) = p.split_once('=').unwrap();
                    (k.to_string(), v.to_string())
                })
                .collect()
        };
        let q = parse_query(pairs(
            "ids=Decentraland,%20&vs_currencies=USD,eth&include_market_cap=1&precision=2",
        ))
        .unwrap();
        assert_eq!(q.ids, vec!["decentraland", "%20"]);
        assert_eq!(q.vs, vec!["usd", "eth"]);
        assert!(q.include_market_cap && !q.include_24hr_vol);
        assert_eq!(q.precision, Some(2));
        assert!(matches!(
            parse_query(pairs("ids=decentraland")),
            Err(PriceError::MissingVsCurrencies)
        ));
        assert!(matches!(
            parse_query(pairs("vs_currencies=")),
            Err(PriceError::MissingVsCurrencies)
        ));
        assert!(matches!(
            parse_query(pairs("vs_currencies=usd&precision=19")),
            Err(PriceError::InvalidPrecision)
        ));
        assert_eq!(
            parse_query(pairs("vs_currencies=usd&precision=full"))
                .unwrap()
                .precision,
            None
        );
        assert_eq!(
            parse_query(pairs("vs_currencies=usd&precision=x"))
                .unwrap()
                .precision,
            None
        );
    }

    #[test]
    fn numbers_round_to_the_precision() {
        assert_eq!(number(0.123456, Some(2)), Some(json!(0.12)));
        assert_eq!(number(0.125, None), Some(json!(0.125)));
        assert_eq!(number(f64::NAN, None), None);
    }
}
