//! The marketplace-api wire vocabulary the old `catalyrst-market` routes
//! shared: the lenient query-string reader upstream's handlers used, the
//! `{ok:false, message}` error envelope, the `{data, total}` envelopes, the
//! `Number.parseInt` page parameters, the network/chain enums as the wire
//! spells them and the decimal-string arithmetic the rankings and volume
//! routes fold wei with.

use std::collections::BTreeMap;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub use super::is_valid_eth_address as is_address;

/// The lenient reader upstream's `getValue`/`getList`/`getNumber` helpers
/// implemented: an absent or invalid value falls back to the default, never
/// an error. `key[]` and `key` are the same list.
pub struct Params<'a> {
    pairs: &'a [(String, String)],
}

impl<'a> Params<'a> {
    pub fn new(pairs: &'a [(String, String)]) -> Self {
        Self { pairs }
    }

    pub fn get_string(&self, key: &str, default: Option<&str>) -> Option<String> {
        self.get_first(key)
            .map(|s| s.to_string())
            .or_else(|| default.map(|s| s.to_string()))
    }

    pub fn get_list(&self, key: &str, valid_values: &[&str]) -> Vec<String> {
        let bracket_key = format!("{key}[]");
        self.pairs
            .iter()
            .filter(|(k, v)| {
                (k == key || *k == bracket_key)
                    && (valid_values.is_empty() || valid_values.contains(&v.as_str()))
            })
            .map(|(_, v)| v.clone())
            .collect()
    }

    pub fn get_number(&self, key: &str, default: Option<f64>) -> Option<f64> {
        match self.get_first(key) {
            Some(s) => s.parse::<f64>().ok().or(default),
            None => default,
        }
    }

    /// Presence, not truth: upstream reads its boolean flags as "the key is there".
    pub fn get_boolean(&self, key: &str) -> bool {
        self.pairs.iter().any(|(k, _)| k == key)
    }

    pub fn get_value(
        &self,
        key: &str,
        valid_values: &[&str],
        default: Option<&str>,
    ) -> Option<String> {
        if let Some(v) = self.get_first(key)
            && (valid_values.is_empty() || valid_values.contains(&v))
        {
            return Some(v.to_string());
        }
        default.map(|s| s.to_string())
    }

    pub fn get_address(&self, key: &str, lowercase: bool, default: Option<&str>) -> Option<String> {
        match self.get_first(key) {
            Some(s) if is_address(s) => Some(if lowercase {
                s.to_lowercase()
            } else {
                s.to_string()
            }),
            _ => default.map(|s| s.to_string()),
        }
    }

    pub fn get_address_list(&self, key: &str, lowercase: bool) -> Vec<String> {
        self.get_list(key, &[])
            .into_iter()
            .filter(|v| is_address(v))
            .map(|v| if lowercase { v.to_lowercase() } else { v })
            .collect()
    }

    fn get_first(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// `{"ok": false, "message": ...}`: the marketplace-server error envelope
/// every market route answers with (distinct from the lambdas' `{error, message}`).
#[derive(Debug)]
pub enum ApiError {
    Http { code: u16, message: String },
    InvalidParameter(InvalidParameterError),
    Database(sqlx::Error),
    Internal(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidParameterError {
    pub parameter: String,
    pub value: String,
}

impl InvalidParameterError {
    pub fn new(parameter: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            parameter: parameter.into(),
            value: value.into(),
        }
    }
}

impl std::fmt::Display for InvalidParameterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "The value of the {} parameter is invalid: {}",
            self.parameter, self.value
        )
    }
}

impl From<InvalidParameterError> for ApiError {
    fn from(e: InvalidParameterError) -> Self {
        ApiError::InvalidParameter(e)
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        ApiError::Database(e)
    }
}

impl ApiError {
    pub fn http(code: u16, message: impl Into<String>) -> Self {
        ApiError::Http {
            code,
            message: message.into(),
        }
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::http(400, message)
    }
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::http(401, message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::http(404, message)
    }
    pub fn internal(message: impl Into<String>) -> Self {
        ApiError::Internal(message.into())
    }
    pub fn message(&self) -> String {
        match self {
            ApiError::Http { message, .. } => message.clone(),
            ApiError::InvalidParameter(e) => e.to_string(),
            ApiError::Database(_) => "database error".to_string(),
            ApiError::Internal(s) => s.clone(),
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let code = match &self {
            ApiError::Http { code, .. } => *code,
            ApiError::InvalidParameter(_) => 400,
            ApiError::Database(e) => {
                tracing::error!(error = %e, "marketplace query failed");
                500
            }
            ApiError::Internal(s) => {
                tracing::error!(error = %s, "marketplace internal error");
                500
            }
        };
        let status = StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (
            status,
            Json(json!({ "ok": false, "message": self.message() })),
        )
            .into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "market/"))]
pub struct DataTotal<T> {
    pub data: Vec<T>,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub total: i64,
}

#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "market/"))]
pub struct DataOnly<T> {
    pub data: T,
}

/// `/v1/orders` stringifies its total, as upstream does.
#[derive(Debug, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "market/"))]
pub struct DataTotalString<T> {
    pub data: Vec<T>,
    pub total: String,
}

#[derive(Debug, Clone, Copy)]
pub struct PageInput {
    pub limit: i64,
    pub offset: i64,
}

const MAX_LIMIT: i64 = 100;

fn limit_or_max(requested: Option<i64>, max: i64) -> i64 {
    match requested {
        Some(n) if n > 0 && n <= max => n,
        _ => max,
    }
}

/// `limit`/`offset`/`page` as the favorites and bids routes read them: a
/// missing or unusable limit is the max (100), a page multiplies the limit.
pub fn get_pagination_params(pairs: &[(String, String)]) -> PageInput {
    let first = |key: &str| {
        pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };
    let limit = limit_or_max(first("limit").and_then(|s| s.parse().ok()), MAX_LIMIT);
    let offset = match first("offset").and_then(|s| s.parse::<i64>().ok()) {
        Some(n) if n >= 0 => n,
        _ => match first("page").and_then(|s| s.parse::<i64>().ok()) {
            Some(p) if p >= 0 => p.saturating_mul(limit),
            _ => 0,
        },
    };
    PageInput { limit, offset }
}

pub fn get_parameter(
    name: &str,
    pairs: &[(String, String)],
    values: Option<&[&str]>,
) -> Result<Option<String>, InvalidParameterError> {
    let parameter = pairs
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone());
    if let (Some(allowed), Some(v)) = (values, &parameter)
        && !allowed.contains(&v.as_str())
    {
        return Err(InvalidParameterError::new(name, v.clone()));
    }
    Ok(parameter)
}

/// Upstream `getNumberParameter`: an absent or empty value is not an error,
/// and the value is read with `Number.parseInt` semantics.
pub fn get_number_parameter(
    name: &str,
    pairs: &[(String, String)],
) -> Result<Option<i64>, InvalidParameterError> {
    let raw = match get_parameter(name, pairs, None)? {
        Some(v) if !v.is_empty() => v,
        _ => return Ok(None),
    };
    js_parse_int_auto_radix(&raw)
        .map(Some)
        .ok_or_else(|| InvalidParameterError::new(name, raw))
}

/// `Number.parseInt` without a radix: leading whitespace (U+FEFF included),
/// an optional sign, an optional `0x` prefix, then the leading digits. A
/// value too wide saturates, as JS yields a lossy float there. Unlike
/// `shared::query_params::js_parse_int` (radix 10), `0x10` is 16 here.
pub fn js_parse_int_auto_radix(raw: &str) -> Option<i64> {
    let trimmed = raw.trim_start_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    let (negative, rest) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    let (radix, rest) = match rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
        Some(rest) => (16u32, rest),
        None => (10u32, rest),
    };
    let digits: String = rest.chars().take_while(|c| c.is_digit(radix)).collect();
    if digits.is_empty() {
        return None;
    }
    Some(match i64::from_str_radix(&digits, radix) {
        Ok(value) if negative => -value,
        Ok(value) => value,
        Err(_) if negative => i64::MIN,
        Err(_) => i64::MAX,
    })
}

pub const MAX_PAGE_LIMIT: i64 = 1000;

pub fn clamp_first(first: Option<i64>, default: i64) -> i64 {
    first.unwrap_or(default).clamp(0, MAX_PAGE_LIMIT)
}

pub fn clamp_skip(skip: Option<i64>) -> i64 {
    skip.unwrap_or(0).max(0)
}

pub fn where_from(filters: &[String]) -> String {
    let non_empty: Vec<&str> = filters
        .iter()
        .map(|f| f.trim())
        .filter(|f| !f.is_empty())
        .collect();
    if non_empty.is_empty() {
        String::new()
    } else {
        format!(" WHERE {} ", non_empty.join(" AND "))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "market/", rename_all = "snake_case")
)]
pub enum NftCategory {
    Parcel,
    Estate,
    Wearable,
    Ens,
    Emote,
}

impl NftCategory {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "parcel" => Self::Parcel,
            "estate" => Self::Estate,
            "wearable" => Self::Wearable,
            "ens" => Self::Ens,
            "emote" => Self::Emote,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Parcel => "parcel",
            Self::Estate => "estate",
            Self::Wearable => "wearable",
            Self::Ens => "ens",
            Self::Emote => "emote",
        }
    }
}

pub const NFT_CATEGORY_VALUES: &[&str] = &["parcel", "estate", "wearable", "ens", "emote"];

/// The wire spelling (`ETHEREUM` | `MATIC`); the mirrors store the subgraphs'
/// lowercase `ethereum` | `matic` and `db_name` gives that column value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "market/", rename_all = "UPPERCASE")
)]
pub enum Network {
    Ethereum,
    Matic,
}

impl Network {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ETHEREUM" | "ethereum" | "mainnet" => Some(Self::Ethereum),
            "MATIC" | "POLYGON" | "matic" | "polygon" => Some(Self::Matic),
            _ => None,
        }
    }

    /// The `network` column of the mirrors (the subgraphs' spelling).
    pub fn db_name(self) -> &'static str {
        match self {
            Self::Ethereum => "ethereum",
            Self::Matic => "matic",
        }
    }

    pub fn wire(self) -> &'static str {
        match self {
            Self::Ethereum => "ETHEREUM",
            Self::Matic => "MATIC",
        }
    }

    pub fn chain_id(self) -> ChainId {
        match self {
            Self::Ethereum => ethereum_chain_id(),
            Self::Matic => polygon_chain_id(),
        }
    }

    pub fn graph(self) -> super::graph::Network {
        match self {
            Self::Ethereum => super::graph::Network::Ethereum,
            Self::Matic => super::graph::Network::Matic,
        }
    }
}

impl From<super::graph::Network> for Network {
    fn from(n: super::graph::Network) -> Self {
        match n {
            super::graph::Network::Ethereum => Network::Ethereum,
            super::graph::Network::Matic => Network::Matic,
        }
    }
}

pub const NETWORK_VALUES: &[&str] = &["ETHEREUM", "MATIC"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(into = "u64", try_from = "u64")]
pub enum ChainId {
    EthereumMainnet = 1,
    EthereumSepolia = 11_155_111,
    MaticMainnet = 137,
    MaticAmoy = 80_002,
}

impl From<ChainId> for u64 {
    fn from(c: ChainId) -> u64 {
        c as u64
    }
}

impl TryFrom<u64> for ChainId {
    type Error = String;
    fn try_from(v: u64) -> Result<Self, Self::Error> {
        Ok(match v {
            1 => ChainId::EthereumMainnet,
            11_155_111 => ChainId::EthereumSepolia,
            137 => ChainId::MaticMainnet,
            80_002 => ChainId::MaticAmoy,
            _ => return Err(format!("unknown chain id: {v}")),
        })
    }
}

/// The chain each network's contracts live on: the mainnets, unless the old
/// `ETHEREUM_CHAIN_ID` / `POLYGON_CHAIN_ID` name a testnet.
pub fn ethereum_chain_id() -> ChainId {
    match std::env::var("ETHEREUM_CHAIN_ID").as_deref() {
        Ok("11155111") => ChainId::EthereumSepolia,
        _ => ChainId::EthereumMainnet,
    }
}

pub fn polygon_chain_id() -> ChainId {
    match std::env::var("POLYGON_CHAIN_ID").as_deref() {
        Ok("80002") => ChainId::MaticAmoy,
        _ => ChainId::MaticMainnet,
    }
}

/// `(network, chainId)` from a stored or subgraph network spelling; anything
/// unknown reads as Ethereum, as the old row mappers did.
pub fn network_and_chain(raw: &str) -> (Network, ChainId) {
    let network = Network::parse(raw).unwrap_or(Network::Ethereum);
    (network, network.chain_id())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "market/", rename_all = "camelCase")
)]
pub struct Contract {
    pub name: String,
    pub address: String,
    pub category: NftCategory,
    pub network: Network,
    #[serde(rename = "chainId")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub chain_id: ChainId,
}

/// `unisex` asks for both shapes, the same request as male + female.
pub(crate) fn body_shapes_for_genders(genders: &[String]) -> Option<Vec<String>> {
    let has_unisex = genders.iter().any(|g| g == "unisex");
    let has_male = has_unisex || genders.iter().any(|g| g == "male");
    let has_female = has_unisex || genders.iter().any(|g| g == "female");
    let mut out = Vec::new();
    if has_male {
        out.push("BaseMale".to_string());
    }
    if has_female {
        out.push("BaseFemale".to_string());
    }
    if out.is_empty() { None } else { Some(out) }
}

pub(crate) fn emote_play_mode_clause(modes: &[String]) -> Option<bool> {
    if modes.is_empty() || modes.len() == 2 {
        return None;
    }
    Some(modes.iter().any(|m| m == "loop"))
}

fn sanitize(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return "0".to_string();
    }
    let no_lead = trimmed.trim_start_matches('0');
    if no_lead.is_empty() {
        "0".to_string()
    } else {
        no_lead.to_string()
    }
}

pub fn bn_add(a: &str, b: &str) -> String {
    let a = sanitize(a);
    let b = sanitize(b);
    let a_bytes: Vec<u8> = a.bytes().rev().collect();
    let b_bytes: Vec<u8> = b.bytes().rev().collect();
    let mut out = Vec::with_capacity(a_bytes.len().max(b_bytes.len()) + 1);
    let mut carry: u32 = 0;
    for i in 0..a_bytes.len().max(b_bytes.len()) {
        let av = a_bytes.get(i).map(|c| (c - b'0') as u32).unwrap_or(0);
        let bv = b_bytes.get(i).map(|c| (c - b'0') as u32).unwrap_or(0);
        let s = av + bv + carry;
        carry = s / 10;
        out.push(b'0' + (s % 10) as u8);
    }
    if carry > 0 {
        out.push(b'0' + carry as u8);
    }
    while out.len() > 1 && *out.last().unwrap() == b'0' {
        out.pop();
    }
    out.into_iter().rev().map(|b| b as char).collect()
}

pub fn bn_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let a = sanitize(a);
    let b = sanitize(b);
    a.len().cmp(&b.len()).then_with(|| a.cmp(&b))
}

/// ethers v6 `formatEther`: wei -> a decimal with at least one fraction digit.
pub fn format_ether(wei: &str) -> String {
    let digits = sanitize(wei);
    let padded = if digits.len() < 19 {
        format!("{digits:0>19}")
    } else {
        digits
    };
    let split = padded.len() - 18;
    let whole = &padded[..split];
    let fraction = padded[split..].trim_end_matches('0');
    let fraction = if fraction.is_empty() { "0" } else { fraction };
    format!("{whole}.{fraction}")
}

/// A histogram key that sorts numerically when it is all digits; the prices
/// and stats routes emit `{"<wei>": count}` in ascending numeric order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumericKey(pub String);

impl Serialize for NumericKey {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl PartialOrd for NumericKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for NumericKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let a_num = self.0.bytes().all(|c| c.is_ascii_digit());
        let b_num = other.0.bytes().all(|c| c.is_ascii_digit());
        if a_num && b_num {
            self.0
                .len()
                .cmp(&other.0.len())
                .then_with(|| self.0.cmp(&other.0))
        } else {
            self.0.cmp(&other.0)
        }
    }
}

pub type Histogram = BTreeMap<NumericKey, i64>;

pub fn consolidate(values: impl IntoIterator<Item = String>) -> Histogram {
    let mut tally = Histogram::new();
    for v in values {
        *tally.entry(NumericKey(v)).or_insert(0) += 1;
    }
    tally
}

/// `mainnet` spelled `ethereum`, the one URN normalization every market route applied.
pub fn fix_urn(urn: &str) -> String {
    urn.replace("mainnet", "ethereum")
}

/// A `contract-item` id split at its dash: the wire's item id when the item
/// part is decimal (an unbounded blockchain id), else `None`.
pub fn split_item_wire_id(wire: &str) -> Option<(String, String)> {
    let (contract, item) = wire.trim().split_once('-')?;
    let contract = contract.to_ascii_lowercase();
    let is_addr = contract
        .strip_prefix("0x")
        .is_some_and(|h| h.len() == 40 && h.bytes().all(|b| b.is_ascii_hexdigit()));
    let is_item = !item.is_empty() && item.len() <= 78 && item.bytes().all(|b| b.is_ascii_digit());
    (is_addr && is_item).then(|| (contract, item.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(raw: &[(&str, &str)]) -> Vec<(String, String)> {
        raw.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn an_absent_or_empty_number_parameter_is_not_an_error() {
        assert_eq!(get_number_parameter("power", &pairs(&[])).unwrap(), None);
        assert_eq!(
            get_number_parameter("power", &pairs(&[("power", "")])).unwrap(),
            None
        );
    }

    #[test]
    fn a_number_parameter_is_read_leniently_like_parse_int() {
        for (raw, want) in [
            ("12", 12),
            ("12abc", 12),
            (" 7 ", 7),
            ("+5", 5),
            ("-3", -3),
            ("1e3", 1),
            ("08", 8),
            ("0x10", 16),
            ("\u{feff}12", 12),
            ("99999999999999999999", i64::MAX),
            ("-99999999999999999999", i64::MIN),
        ] {
            assert_eq!(
                get_number_parameter("power", &pairs(&[("power", raw)])).unwrap(),
                Some(want),
                "{raw}"
            );
        }
    }

    #[test]
    fn a_number_parameter_without_leading_digits_is_invalid() {
        for raw in ["abc", "-", "0x", "e3"] {
            let err = get_number_parameter("power", &pairs(&[("power", raw)])).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("The value of the power parameter is invalid: {raw}")
            );
        }
    }

    #[test]
    fn pagination_params_honor_limit_offset_page() {
        let p = get_pagination_params(&pairs(&[("limit", "24"), ("offset", "48")]));
        assert_eq!((p.limit, p.offset), (24, 48));
        let p = get_pagination_params(&pairs(&[("limit", "24"), ("page", "2")]));
        assert_eq!((p.limit, p.offset), (24, 48));
        let p = get_pagination_params(&pairs(&[("offset", "-5")]));
        assert_eq!((p.limit, p.offset), (100, 0));
        let p = get_pagination_params(&pairs(&[("limit", "500")]));
        assert_eq!((p.limit, p.offset), (100, 0));
    }

    #[test]
    fn clamp_first_caps_and_floors() {
        assert_eq!(clamp_first(None, 100), 100);
        assert_eq!(clamp_first(Some(2), 100), 2);
        assert_eq!(clamp_first(Some(-1), 100), 0);
        assert_eq!(clamp_first(Some(10_000_000), 100), MAX_PAGE_LIMIT);
        assert_eq!(clamp_skip(Some(-1)), 0);
    }

    #[test]
    fn params_read_lists_flags_and_addresses_leniently() {
        let p = pairs(&[
            ("rarity", "epic"),
            ("rarity[]", "rare"),
            ("rarity", "bogus"),
            ("isOnSale", "false"),
            ("creator", "0xNotAnAddress"),
            ("owner", "0xAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
        ]);
        let p = Params::new(&p);
        assert_eq!(
            p.get_list("rarity", &["epic", "rare"]),
            vec!["epic", "rare"]
        );
        assert_eq!(p.get_list("rarity", &[]).len(), 3);
        assert!(p.get_boolean("isOnSale"), "presence, not truth");
        assert_eq!(p.get_address("creator", true, None), None);
        assert_eq!(
            p.get_address("owner", true, None).as_deref(),
            Some("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(
            p.get_value("sortBy", &["newest"], Some("cheapest"))
                .as_deref(),
            Some("cheapest")
        );
        assert_eq!(p.get_number("first", Some(20.0)), Some(20.0));
    }

    #[test]
    fn the_error_envelope_is_ok_false_message() {
        let v = serde_json::to_value(
            json!({ "ok": false, "message": ApiError::bad_request("nope").message() }),
        )
        .unwrap();
        assert_eq!(v, json!({ "ok": false, "message": "nope" }));
        assert_eq!(
            ApiError::from(InvalidParameterError::new("first", "abc")).message(),
            "The value of the first parameter is invalid: abc"
        );
    }

    #[test]
    fn gender_filters_preserve_shape_order_and_ignore_unknown_values() {
        for (genders, expected) in [
            (vec![], None),
            (vec!["unknown"], None),
            (vec!["male"], Some(vec!["BaseMale"])),
            (vec!["female"], Some(vec!["BaseFemale"])),
            (vec!["unisex"], Some(vec!["BaseMale", "BaseFemale"])),
            (
                vec!["female", "unknown", "male", "male"],
                Some(vec!["BaseMale", "BaseFemale"]),
            ),
        ] {
            let genders = genders.into_iter().map(String::from).collect::<Vec<_>>();
            let expected =
                expected.map(|shapes| shapes.into_iter().map(String::from).collect::<Vec<_>>());
            assert_eq!(body_shapes_for_genders(&genders), expected);
        }
    }

    #[test]
    fn emote_modes_preserve_empty_and_two_entry_no_filter_behavior() {
        for (modes, expected) in [
            (vec![], None),
            (vec!["loop"], Some(true)),
            (vec!["simple"], Some(false)),
            (vec!["unknown"], Some(false)),
            (vec!["loop", "simple"], None),
            (vec!["loop", "loop"], None),
            (vec!["simple", "unknown", "loop"], Some(true)),
        ] {
            let modes = modes.into_iter().map(String::from).collect::<Vec<_>>();
            assert_eq!(emote_play_mode_clause(&modes), expected);
        }
    }

    #[test]
    fn decimal_strings_add_compare_and_format() {
        assert_eq!(
            bn_add("999999999999999999999", "1"),
            "1000000000000000000000"
        );
        assert_eq!(bn_add("", "0"), "0");
        assert_eq!(bn_cmp("10", "9"), std::cmp::Ordering::Greater);
        assert_eq!(format_ether("1500000000000000000"), "1.5");
        assert_eq!(format_ether("1"), "0.000000000000000001");
        assert_eq!(format_ether("0"), "0.0");
        assert_eq!(format_ether("  001000000000000000000 "), "1.0");
    }

    #[test]
    fn numeric_keys_order_by_value() {
        let out = consolidate(
            ["999", "1000", "999", "0", "100"]
                .into_iter()
                .map(String::from),
        );
        let keys: Vec<&str> = out.keys().map(|k| k.0.as_str()).collect();
        assert_eq!(keys, vec!["0", "100", "999", "1000"]);
        assert_eq!(out.get(&NumericKey("999".into())), Some(&2));
        let s = serde_json::to_string(&out).unwrap();
        assert!(s.find("\"999\"").unwrap() < s.find("\"1000\"").unwrap());
    }

    #[test]
    fn networks_and_chains_render_as_the_wire_spells_them() {
        assert_eq!(
            serde_json::to_value(Network::Matic).unwrap(),
            json!("MATIC")
        );
        assert_eq!(
            serde_json::to_value(ChainId::MaticMainnet).unwrap(),
            json!(137)
        );
        assert_eq!(network_and_chain("matic").0, Network::Matic);
        assert_eq!(network_and_chain("POLYGON").0, Network::Matic);
        assert_eq!(network_and_chain("nope").0, Network::Ethereum);
        assert_eq!(Network::Ethereum.db_name(), "ethereum");
        assert_eq!(
            serde_json::to_value(NftCategory::Wearable).unwrap(),
            json!("wearable")
        );
    }

    #[test]
    fn item_wire_ids_split_only_when_well_formed() {
        assert_eq!(
            split_item_wire_id("0xC5AAAA47D4ED16932D9970352AFBD5E0DF5A4925-0"),
            Some((
                "0xc5aaaa47d4ed16932d9970352afbd5e0df5a4925".into(),
                "0".into()
            ))
        );
        assert_eq!(split_item_wire_id("0x016a-12"), None);
        assert_eq!(
            split_item_wire_id("0xc5aaaa47d4ed16932d9970352afbd5e0df5a4925-abc"),
            None
        );
        assert_eq!(
            fix_urn("urn:decentraland:mainnet:collections-v1:x:y"),
            "urn:decentraland:ethereum:collections-v1:x:y"
        );
    }
}
