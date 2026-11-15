//! The `mana-price-poll` job: one CoinGecko `/simple/price` read per pass
//! through the ONE client (`http::Outbound`: the SSRF guard on the configured
//! base, the body cap), validated before it is stored (never a partial or
//! timestamp-less quote), appended to `economy.price_snapshots` and published
//! to the in-memory cell the route serves.
//!
//! When CoinGecko fails (its CDN has refused whole networks), the pass takes
//! the spot from the configured exchanges in order (`PRICE_FALLBACK_SOURCES`:
//! Coinbase, Binance, Kraken), the first that answers a positive MANA/USD.
//! Such a row carries the exchange as its `source`, the fetch time as its
//! quote time (the spot endpoints are live and carry none), MANA/ETH, BTC and
//! POL derived from the same exchange's USD legs where it answered them
//! (Binance quotes USDT, taken as USD), and no market cap, volume or change.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, TimeZone, Utc};
use serde::Deserialize;
use sqlx::postgres::PgPool;

use super::MANA_ID;
use super::store::{PriceSnapshot, Prices, SOURCE_COINGECKO, snapshot_columns, snapshot_from_row};
use crate::http::{Outbound, read_body_capped};
use serde_json::Value;

const MATIC_ID: &str = "matic-network";
const BODY_CAP: usize = 64 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Default, Deserialize)]
struct CoinQuote {
    usd: Option<f64>,
    eth: Option<f64>,
    btc: Option<f64>,
    usd_market_cap: Option<f64>,
    usd_24h_vol: Option<f64>,
    usd_24h_change: Option<f64>,
    last_updated_at: Option<i64>,
}

type SimplePriceResponse = HashMap<String, CoinQuote>;

#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotRow {
    pub source_updated_at: Option<DateTime<Utc>>,
    pub mana_usd: Option<f64>,
    pub mana_eth: Option<f64>,
    pub mana_btc: Option<f64>,
    pub mana_matic: Option<f64>,
    pub matic_usd: Option<f64>,
    pub mana_market_cap_usd: Option<f64>,
    pub mana_volume_24h_usd: Option<f64>,
    pub mana_price_change_24h_pct: Option<f64>,
}

fn map_snapshot(resp: &SimplePriceResponse) -> SnapshotRow {
    let default = CoinQuote::default();
    let mana = resp.get(MANA_ID).unwrap_or(&default);
    let matic = resp.get(MATIC_ID).unwrap_or(&default);

    let mana_matic = match (mana.usd, matic.usd) {
        (Some(m), Some(t)) if t != 0.0 => Some(m / t),
        _ => None,
    };
    let source_updated_at = mana
        .last_updated_at
        .and_then(|ts| Utc.timestamp_opt(ts, 0).single());

    SnapshotRow {
        source_updated_at,
        mana_usd: mana.usd,
        mana_eth: mana.eth,
        mana_btc: mana.btc,
        mana_matic,
        matic_usd: matic.usd,
        mana_market_cap_usd: mana.usd_market_cap,
        mana_volume_24h_usd: mana.usd_24h_vol,
        mana_price_change_24h_pct: mana.usd_24h_change,
    }
}

pub fn simple_price_url(base: &str) -> String {
    format!(
        "{}/simple/price?ids={},{}&vs_currencies=usd,eth,btc\
         &include_market_cap=true&include_24hr_vol=true\
         &include_24hr_change=true&include_last_updated_at=true",
        base.trim_end_matches('/'),
        MANA_ID,
        MATIC_ID,
    )
}

async fn fetch_snapshot(http: &Outbound, base: &str) -> anyhow::Result<SnapshotRow> {
    let resp = http
        .get(simple_price_url(base))
        .header("Accept", "application/json")
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .context("coingecko request")?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("coingecko answered HTTP {status}");
    }
    let bytes = read_body_capped(resp, BODY_CAP).await?;
    let resp: SimplePriceResponse = serde_json::from_slice(&bytes).context("coingecko body")?;
    let row = map_snapshot(&resp);
    validate_snapshot(&row)?;
    ensure_fresh_quote(&row, Utc::now())?;
    Ok(row)
}

/// Never replace a valid quote with a partial provider response, or mark a
/// timestamp-less quote as fresh via the snapshot insertion time.
fn validate_snapshot(row: &SnapshotRow) -> anyhow::Result<()> {
    anyhow::ensure!(
        row.mana_usd
            .is_some_and(|price| price.is_finite() && price > 0.0),
        "CoinGecko response missing a positive MANA/USD quote"
    );
    anyhow::ensure!(
        row.source_updated_at.is_some(),
        "CoinGecko response missing MANA quote timestamp"
    );
    Ok(())
}

/// A provider quote older than the freshness budget is never stored as new.
fn ensure_fresh_quote(row: &SnapshotRow, now: DateTime<Utc>) -> anyhow::Result<()> {
    anyhow::ensure!(
        row.source_updated_at
            .is_some_and(|at| quote_is_fresh(at, now)),
        "CoinGecko MANA quote is older than the snapshot freshness budget"
    );
    Ok(())
}

/// A quote from the future is not fresh beyond a minute of clock skew.
fn quote_is_fresh(quoted_at: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    match (now - quoted_at).to_std() {
        Ok(age) => age <= crate::store::SNAPSHOT_MAX_AGE,
        Err(_) => (quoted_at - now) <= chrono::Duration::seconds(60),
    }
}

/// How close another source must be for an outlier to stand.
pub const CORROBORATION_FRACTION: f64 = 0.05;

/// Whether another configured source than `source` quotes `next` within
/// [`CORROBORATION_FRACTION`]; the first source that answers decides.
async fn corroborated(http: &Outbound, sources: &Sources, source: &str, next: f64) -> bool {
    for (exchange, base) in &sources.fallbacks {
        if exchange.name() == source {
            continue;
        }
        if let Ok(row) = fetch_exchange(http, *exchange, base).await
            && let Some(other) = row.mana_usd.filter(|v| *v > 0.0)
        {
            return ((next - other) / other).abs() <= CORROBORATION_FRACTION;
        }
    }
    false
}

/// The largest move against the previous fresh snapshot a pass publishes
/// alone; a larger one needs another source to agree. With no
/// `PRICE_FALLBACK_SOURCES` configured nothing can agree, so the move is held
/// only until the previous snapshot ages out of the freshness budget.
pub const MAX_MOVE_FRACTION: f64 = 0.3;

/// Whether `next` moved from a still-fresh `previous` by more than [`MAX_MOVE_FRACTION`].
pub fn is_outlier(previous: Option<&PriceSnapshot>, next: f64, now: DateTime<Utc>) -> bool {
    let Some(prev) = previous.filter(|p| p.is_fresh_at(now)) else {
        return false;
    };
    let Some(prev_usd) = prev.mana_usd.filter(|p| *p > 0.0) else {
        return false;
    };
    ((next - prev_usd) / prev_usd).abs() > MAX_MOVE_FRACTION
}

/// Returns the stored row (numeric-rounded, the bound `taken_at`) so the
/// published snapshot is exactly what a DB read would serve.
pub async fn insert_snapshot(
    pool: &PgPool,
    source: &str,
    row: &SnapshotRow,
    taken_at: DateTime<Utc>,
) -> Result<(i64, PriceSnapshot), sqlx::Error> {
    let rec = sqlx::query(concat!(
        "INSERT INTO economy.price_snapshots \
            (taken_at, source, source_updated_at, mana_usd, mana_eth, mana_btc, mana_matic, \
             matic_usd, mana_market_cap_usd, mana_volume_24h_usd, mana_price_change_24h_pct) \
         VALUES ($1, $2, $3, \
             $4::double precision, $5::double precision, $6::double precision, \
             $7::double precision, $8::double precision, $9::double precision, \
             $10::double precision, $11::double precision) \
         RETURNING id, ",
        snapshot_columns!()
    ))
    .bind(taken_at.naive_utc())
    .bind(source)
    .bind(row.source_updated_at.map(|d| d.naive_utc()))
    .bind(row.mana_usd)
    .bind(row.mana_eth)
    .bind(row.mana_btc)
    .bind(row.mana_matic)
    .bind(row.matic_usd)
    .bind(row.mana_market_cap_usd)
    .bind(row.mana_volume_24h_usd)
    .bind(row.mana_price_change_24h_pct)
    .fetch_one(pool)
    .await?;
    use sqlx::Row;
    Ok((rec.get::<i64, _>("id"), snapshot_from_row(&rec)))
}

/// An exchange the pass falls back to when CoinGecko fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exchange {
    Coinbase,
    Binance,
    Kraken,
}

impl Exchange {
    pub const ALL: [Exchange; 3] = [Exchange::Coinbase, Exchange::Binance, Exchange::Kraken];

    /// The `source` its rows carry, and its name in `PRICE_FALLBACK_SOURCES`.
    pub fn name(self) -> &'static str {
        match self {
            Exchange::Coinbase => "coinbase",
            Exchange::Binance => "binance",
            Exchange::Kraken => "kraken",
        }
    }

    pub fn parse(name: &str) -> Option<Exchange> {
        Exchange::ALL.into_iter().find(|e| e.name() == name)
    }
}

/// Where a pass reads the spot: CoinGecko, then each fallback in order.
#[derive(Clone, Debug)]
pub struct Sources {
    pub coingecko: String,
    pub fallbacks: Vec<(Exchange, String)>,
}

impl Sources {
    pub fn coingecko_only(base: &str) -> Self {
        Self {
            coingecko: base.to_string(),
            fallbacks: Vec::new(),
        }
    }
}

/// The USD legs one exchange answered, MANA required.
#[derive(Debug, Default, PartialEq)]
struct Legs {
    mana: Option<f64>,
    eth: Option<f64>,
    btc: Option<f64>,
    pol: Option<f64>,
}

fn positive(v: Option<f64>) -> Option<f64> {
    v.filter(|p| p.is_finite() && *p > 0.0)
}

fn legs_row(legs: &Legs, at: DateTime<Utc>) -> SnapshotRow {
    let per = |leg: Option<f64>| match (positive(legs.mana), positive(leg)) {
        (Some(m), Some(l)) => Some(m / l),
        _ => None,
    };
    SnapshotRow {
        source_updated_at: Some(at),
        mana_usd: positive(legs.mana),
        mana_eth: per(legs.eth),
        mana_btc: per(legs.btc),
        mana_matic: per(legs.pol),
        matic_usd: positive(legs.pol),
        mana_market_cap_usd: None,
        mana_volume_24h_usd: None,
        mana_price_change_24h_pct: None,
    }
}

async fn get_json(http: &Outbound, url: String) -> anyhow::Result<Value> {
    let resp = http
        .get(url)
        .header("Accept", "application/json")
        .timeout(REQUEST_TIMEOUT)
        .send()
        .await
        .context("request")?;
    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("answered HTTP {status}");
    }
    let bytes = read_body_capped(resp, BODY_CAP).await?;
    serde_json::from_slice(&bytes).context("body")
}

fn number(v: &Value) -> Option<f64> {
    v.as_str()
        .and_then(|s| s.parse().ok())
        .or_else(|| v.as_f64())
}

/// Coinbase spot: `GET /v2/prices/{PAIR}/spot` -> `{"data":{"amount":"0.09"}}`, one per leg.
async fn coinbase_legs(http: &Outbound, base: &str) -> anyhow::Result<Legs> {
    let leg = |pair: &'static str| {
        let url = format!("{base}/v2/prices/{pair}/spot");
        async move {
            get_json(http, url)
                .await
                .ok()
                .and_then(|v| number(&v["data"]["amount"]))
        }
    };
    let mana = get_json(http, format!("{base}/v2/prices/MANA-USD/spot"))
        .await
        .context("MANA-USD")?;
    let (eth, btc, pol) = tokio::join!(leg("ETH-USD"), leg("BTC-USD"), leg("POL-USD"));
    Ok(Legs {
        mana: number(&mana["data"]["amount"]),
        eth,
        btc,
        pol,
    })
}

/// Binance: `GET /api/v3/ticker/price?symbols=[...]` -> `[{"symbol","price"}]`, USDT legs.
async fn binance_legs(http: &Outbound, base: &str) -> anyhow::Result<Legs> {
    let v = get_json(
        http,
        format!(
            "{base}/api/v3/ticker/price?symbols=%5B%22MANAUSDT%22,%22ETHUSDT%22,%22BTCUSDT%22,%22POLUSDT%22%5D"
        ),
    )
    .await?;
    let price = |symbol: &str| {
        v.as_array()?
            .iter()
            .find(|t| t["symbol"] == symbol)
            .and_then(|t| number(&t["price"]))
    };
    Ok(Legs {
        mana: price("MANAUSDT"),
        eth: price("ETHUSDT"),
        btc: price("BTCUSDT"),
        pol: price("POLUSDT"),
    })
}

/// Kraken: `GET /0/public/Ticker?pair=...` -> `{"error":[],"result":{PAIR:{"c":[last,vol]}}}`;
/// the result keys are Kraken's canonical names (`XETHZUSD`, `XXBTZUSD`).
async fn kraken_legs(http: &Outbound, base: &str) -> anyhow::Result<Legs> {
    let v = get_json(
        http,
        format!("{base}/0/public/Ticker?pair=MANAUSD,ETHUSD,XBTUSD,POLUSD"),
    )
    .await?;
    if let Some(errors) = v["error"].as_array().filter(|e| !e.is_empty()) {
        anyhow::bail!("kraken error {errors:?}");
    }
    let result = v["result"].as_object().context("no result")?;
    let last = |asset: &str| {
        result
            .iter()
            .find(|(pair, _)| pair.starts_with(asset) || pair.starts_with(&format!("X{asset}")))
            .and_then(|(_, t)| number(&t["c"][0]))
    };
    Ok(Legs {
        mana: last("MANA"),
        eth: last("ETH"),
        btc: last("XBT"),
        pol: last("POL"),
    })
}

async fn fetch_exchange(
    http: &Outbound,
    exchange: Exchange,
    base: &str,
) -> anyhow::Result<SnapshotRow> {
    let base = base.trim_end_matches('/');
    let legs = match exchange {
        Exchange::Coinbase => coinbase_legs(http, base).await,
        Exchange::Binance => binance_legs(http, base).await,
        Exchange::Kraken => kraken_legs(http, base).await,
    }?;
    let row = legs_row(&legs, Utc::now());
    anyhow::ensure!(row.mana_usd.is_some(), "no positive MANA/USD quote");
    Ok(row)
}

/// One pass: fetch (CoinGecko, else the fallbacks in order), validate, append, publish.
pub async fn run_pass(http: &Outbound, sources: &Sources, prices: &Prices) -> anyhow::Result<()> {
    let (source, row) = match fetch_snapshot(http, &sources.coingecko).await {
        Ok(row) => (SOURCE_COINGECKO, row),
        Err(coingecko) => {
            let coingecko = format!("coingecko fetch failed: {coingecko:#}");
            let mut failures = vec![coingecko.clone()];
            let mut found = None;
            for (exchange, base) in &sources.fallbacks {
                match fetch_exchange(http, *exchange, base).await {
                    Ok(row) => {
                        found = Some((exchange.name(), row));
                        break;
                    }
                    Err(e) => failures.push(format!("{} fetch failed: {e:#}", exchange.name())),
                }
            }
            let Some(found) = found else {
                anyhow::bail!("{}", failures.join("; "));
            };
            tracing::warn!(source = found.0, error = %coingecko, "coingecko failed; took the MANA spot from a fallback exchange");
            found
        }
    };
    let previous = prices
        .latest()
        .await
        .context("previous snapshot read for the outlier check")?;
    if let Some(next) = row.mana_usd
        && is_outlier(previous.as_deref(), next, Utc::now())
        && !corroborated(http, sources, source, next).await
    {
        anyhow::bail!(
            "{source} MANA/USD {next} moved more than {}% from the previous snapshot and no other source agrees; not published",
            MAX_MOVE_FRACTION * 100.0
        );
    }
    let (id, stored) = insert_snapshot(prices.pool(), source, &row, Utc::now())
        .await
        .context("failed to insert snapshot")?;
    prices.publish(stored);
    tracing::info!(
        snapshot_id = id,
        source,
        mana_usd = ?row.mana_usd,
        mana_eth = ?row.mana_eth,
        mana_btc = ?row.mana_btc,
        mana_matic = ?row.mana_matic,
        "wrote mana_price snapshot"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(usd: f64, taken_at: DateTime<Utc>, quoted_at: DateTime<Utc>) -> PriceSnapshot {
        PriceSnapshot {
            mana_usd: Some(usd),
            mana_eth: None,
            mana_btc: None,
            market_cap_usd: None,
            volume_24h_usd: None,
            price_change_24h_pct: None,
            source_updated_at: Some(quoted_at),
            taken_at,
        }
    }

    #[test]
    fn a_large_move_against_a_fresh_snapshot_is_held_back() {
        let now = Utc::now();
        let fresh = snapshot(0.4, now, now);
        assert!(!is_outlier(Some(&fresh), 0.5, now), "+25% publishes");
        assert!(is_outlier(Some(&fresh), 0.6, now), "+50% is held");
        assert!(is_outlier(Some(&fresh), 0.2, now), "-50% is held");
        let stale = snapshot(
            0.4,
            now - chrono::Duration::hours(1),
            now - chrono::Duration::hours(1),
        );
        assert!(
            !is_outlier(Some(&stale), 0.6, now),
            "a stale previous never blocks"
        );
        assert!(!is_outlier(None, 0.6, now));
    }

    #[test]
    fn freshness_counts_from_the_provider_quote() {
        let now = Utc::now();
        let frozen = snapshot(0.4, now, now - chrono::Duration::hours(2));
        assert!(
            !frozen.is_fresh_at(now),
            "a just-taken row of an old quote is stale"
        );
        assert!(snapshot(0.4, now, now).is_fresh_at(now));
        assert!(quote_is_fresh(now - chrono::Duration::minutes(5), now));
        assert!(!quote_is_fresh(now - chrono::Duration::hours(1), now));
        assert!(
            quote_is_fresh(now + chrono::Duration::seconds(30), now),
            "clock skew"
        );
        assert!(
            !quote_is_fresh(now + chrono::Duration::minutes(5), now),
            "a quote from the future is not fresh"
        );
    }

    #[test]
    fn maps_simple_price_payload_to_snapshot_row() {
        let json = serde_json::json!({
            "decentraland": {
                "usd": 0.42,
                "eth": 0.00012,
                "btc": 0.0000065,
                "usd_market_cap": 850000000.0,
                "usd_24h_vol": 12345678.0,
                "usd_24h_change": -3.21,
                "last_updated_at": 1_700_000_000_i64
            },
            "matic-network": {
                "usd": 0.84,
                "last_updated_at": 1_700_000_001_i64
            }
        });
        let resp: SimplePriceResponse = serde_json::from_value(json).unwrap();
        let row = map_snapshot(&resp);

        assert_eq!(row.mana_usd, Some(0.42));
        assert_eq!(row.mana_eth, Some(0.00012));
        assert_eq!(row.mana_btc, Some(0.0000065));
        assert_eq!(row.matic_usd, Some(0.84));
        assert_eq!(row.mana_market_cap_usd, Some(850000000.0));
        assert_eq!(row.mana_volume_24h_usd, Some(12345678.0));
        assert_eq!(row.mana_price_change_24h_pct, Some(-3.21));
        assert_eq!(row.mana_matic, Some(0.5));
        assert_eq!(
            row.source_updated_at,
            Some(Utc.timestamp_opt(1_700_000_000, 0).single().unwrap())
        );
    }

    #[test]
    fn rejects_partial_or_invalid_quotes_before_persisting() {
        for payload in [
            serde_json::json!({}),
            serde_json::json!({"decentraland": {"usd": 0.42}}),
            serde_json::json!({"decentraland": {"usd": 0.0, "last_updated_at": 1_700_000_000}}),
            serde_json::json!({"decentraland": {"usd": -0.1, "last_updated_at": 1_700_000_000}}),
        ] {
            let response = serde_json::from_value(payload).unwrap();
            assert!(validate_snapshot(&map_snapshot(&response)).is_err());
        }
        let response = serde_json::from_value(serde_json::json!({
            "decentraland": {"usd": 0.42, "last_updated_at": 1_700_000_000}
        }))
        .unwrap();
        assert!(validate_snapshot(&map_snapshot(&response)).is_ok());
    }

    #[test]
    fn mana_matic_is_none_when_matic_missing_or_zero() {
        let resp: SimplePriceResponse =
            serde_json::from_value(serde_json::json!({ "decentraland": { "usd": 0.42 } })).unwrap();
        assert_eq!(map_snapshot(&resp).mana_matic, None);

        let resp: SimplePriceResponse = serde_json::from_value(
            serde_json::json!({ "decentraland": { "usd": 0.42 }, "matic-network": { "usd": 0.0 } }),
        )
        .unwrap();
        assert_eq!(map_snapshot(&resp).mana_matic, None);
    }

    #[test]
    fn missing_decentraland_yields_all_none() {
        let resp: SimplePriceResponse = serde_json::from_value(serde_json::json!({})).unwrap();
        let row = map_snapshot(&resp);
        assert_eq!(row.mana_usd, None);
        assert_eq!(row.mana_matic, None);
        assert_eq!(row.source_updated_at, None);
    }

    #[test]
    fn the_request_is_the_old_simple_price_shape() {
        let url = simple_price_url("https://api.coingecko.com/api/v3/");
        assert!(url.starts_with("https://api.coingecko.com/api/v3/simple/price?ids=decentraland,matic-network&vs_currencies=usd,eth,btc"), "{url}");
        assert!(url.contains("include_last_updated_at=true"));
    }

    #[test]
    fn exchange_legs_derive_the_cross_rates_and_leave_the_market_fields_empty() {
        let at = Utc.timestamp_opt(1_700_000_000, 0).single().unwrap();
        let row = legs_row(
            &Legs {
                mana: Some(0.09),
                eth: Some(3000.0),
                btc: Some(90_000.0),
                pol: Some(0.18),
            },
            at,
        );
        assert_eq!(row.mana_usd, Some(0.09));
        assert_eq!(row.mana_eth, Some(0.09 / 3000.0));
        assert_eq!(row.mana_btc, Some(0.09 / 90_000.0));
        assert_eq!(row.mana_matic, Some(0.5));
        assert_eq!(row.matic_usd, Some(0.18));
        assert_eq!(row.source_updated_at, Some(at));
        assert_eq!(row.mana_market_cap_usd, None);
        assert!(validate_snapshot(&row).is_ok());

        let only_mana = legs_row(
            &Legs {
                mana: Some(0.09),
                eth: Some(0.0),
                ..Legs::default()
            },
            at,
        );
        assert_eq!(only_mana.mana_eth, None);
        assert_eq!(only_mana.matic_usd, None);
        assert_eq!(
            legs_row(
                &Legs {
                    mana: Some(-1.0),
                    ..Legs::default()
                },
                at
            )
            .mana_usd,
            None
        );
    }

    #[test]
    fn exchanges_parse_by_their_source_name() {
        for e in Exchange::ALL {
            assert_eq!(Exchange::parse(e.name()), Some(e));
        }
        assert_eq!(Exchange::parse("coingecko"), None);
    }
}
