//! The upstream content server parses query strings with `qs`: repeated keys
//! and `key[0]=` indexes both build arrays, so axum's `Query` extractor is not
//! used on the history routes.

use std::collections::HashMap;

use crate::js::is_js_whitespace;

pub const MAX_PAGE_SIZE: u32 = 1000;

pub const MAX_QUERY_PARAMS: usize = 2000;

pub type QueryParams = HashMap<String, Vec<String>>;

/// Every entity type a content server stores, in the upstream order.
pub const ENTITY_TYPES: &[&str] = &["scene", "profile", "wearable", "store", "emote", "outfits"];

pub fn parse_query_string(raw: &str) -> QueryParams {
    let mut map: QueryParams = HashMap::new();
    if raw.is_empty() {
        return map;
    }
    for pair in raw.split('&').take(MAX_QUERY_PARAMS) {
        let mut parts = pair.splitn(2, '=');
        let key = parts.next().unwrap_or("");
        let value = parts.next().unwrap_or("");

        let decoded_key = urlencoding_decode(key);
        let decoded_value = urlencoding_decode(value);

        let normalized_key = strip_array_index(&decoded_key);

        map.entry(normalized_key).or_default().push(decoded_value);
    }
    map
}

pub fn qs_get_array(params: &QueryParams, key: &str) -> Vec<String> {
    params.get(key).cloned().unwrap_or_default()
}

/// The content server's `qsGetArray`: a missing key, a bare key or a single
/// empty value (`?pointer=`) is absent; empty strings inside a repeated list
/// are kept and match nothing.
pub fn qs_get_filter_array(params: &QueryParams, key: &str) -> Vec<String> {
    match params.get(key) {
        Some(values) if !(values.len() == 1 && values[0].is_empty()) => values.clone(),
        _ => Vec::new(),
    }
}

pub fn qs_get_string(params: &QueryParams, key: &str) -> Option<String> {
    params.get(key).and_then(|v| v.first()).cloned()
}

pub fn qs_get_number(params: &QueryParams, key: &str) -> Option<i64> {
    qs_get_string(params, key).and_then(|s| s.parse::<i64>().ok())
}

/// Trimmed and case-insensitive: each type name, plus `<type>s` for a name
/// that does not already end in `s` (`scenes` is `scene`; `outfitss` is
/// nothing), as upstream builds `ENTITY_TYPES_BY_NAME`.
pub fn parse_entity_type(raw: &str) -> Option<&'static str> {
    let lowered = raw.trim().to_lowercase();
    ENTITY_TYPES
        .iter()
        .copied()
        .find(|t| *t == lowered || (!t.ends_with('s') && lowered.strip_suffix('s') == Some(*t)))
}

pub fn camel_to_snake(s: &str) -> String {
    let mut result = String::with_capacity(s.len() + 4);
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() {
            if i > 0 {
                result.push('_');
            }
            result.extend(ch.to_lowercase());
        } else {
            result.push(ch);
        }
    }
    result
}

#[derive(Debug, Clone)]
pub struct Pagination {
    pub page_size: i64,
    pub page_num: i64,
    pub offset: i64,
    pub limit: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OversizePolicy {
    Reject,
    Clamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NonPositivePolicy {
    Reject,
    ClampToOne,
    PassThrough,
}

pub fn parse_pagination_with(
    params: &QueryParams,
    max_page_size: i64,
    oversize: OversizePolicy,
    non_positive: NonPositivePolicy,
) -> Result<Pagination, String> {
    let mut page_size = qs_get_number(params, "pageSize").unwrap_or(100);
    let mut page_num = qs_get_number(params, "pageNum").unwrap_or(1);

    match non_positive {
        NonPositivePolicy::Reject => {
            if page_size < 1 {
                return Err("pageSize must be a positive integer".to_string());
            }
            if page_num < 1 {
                return Err("pageNum must be a positive integer".to_string());
            }
        }
        NonPositivePolicy::ClampToOne => {
            if page_size < 1 {
                page_size = 1;
            }
            if page_num < 1 {
                page_num = 1;
            }
        }
        NonPositivePolicy::PassThrough => {}
    }

    if page_size > max_page_size {
        match oversize {
            OversizePolicy::Reject => {
                return Err(format!("max allowed pageSize is {}", max_page_size));
            }
            OversizePolicy::Clamp => page_size = max_page_size,
        }
    }

    let offset = page_num.saturating_sub(1).saturating_mul(page_size);
    let limit = page_size;
    Ok(Pagination {
        page_size,
        page_num,
        offset,
        limit,
    })
}

/// JavaScript's `parseInt`: leading white space, an optional sign, then the
/// longest run of digits; `None` where JavaScript answers `NaN`. With
/// `hex_prefix` (`parseInt(s)` with no radix) a `0x`/`0X` reads hex digits.
/// A value past `i64` saturates.
pub fn js_parse_int(s: &str, hex_prefix: bool) -> Option<i64> {
    let s = s.trim_start_matches(is_js_whitespace);
    let (negative, s) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let (radix, s) = match s.get(..2) {
        Some("0x" | "0X") if hex_prefix => (16, &s[2..]),
        _ => (10, s),
    };
    let digits: Vec<u32> = s.chars().map_while(|c| c.to_digit(radix)).collect();
    if digits.is_empty() {
        return None;
    }
    let magnitude = digits.iter().fold(0i64, |acc, &d| {
        acc.saturating_mul(i64::from(radix))
            .saturating_add(i64::from(d))
    });
    Some(if negative { -magnitude } else { magnitude })
}

/// JavaScript `parseInt(s, 10)` ([`js_parse_int`] without the hex prefix):
/// `"10abc"` is 10, `"1.5"` is 1, `None` is `NaN`. A value past `i64`
/// saturates (JS keeps the float, so only an echoed value differs, never
/// the slice).
pub fn parse_int_js(s: &str) -> Option<i64> {
    js_parse_int(s, false)
}

/// The `[start, end)` of JavaScript's `elements.slice(offset, offset +
/// limit)` over `len` elements: a negative index counts from the end.
pub fn js_slice_range(len: usize, offset: i64, limit: i64) -> (usize, usize) {
    let len = len as i64;
    let js_index = |i: i64| if i < 0 { (len + i).max(0) } else { i.min(len) };
    let (start, end) = (js_index(offset), js_index(offset.saturating_add(limit)));
    if start < end {
        (start as usize, end as usize)
    } else {
        (0, 0)
    }
}

/// lamb2's `paginationObject` with `fetchAndPaginate`'s slicing: each of
/// `pageSize` (default 100) and `pageNum` (default 1) is `parseInt(v, 10)`,
/// `None` for `NaN` (answered as `null`); a `pageSize` over the maximum is
/// the 400; the page is `slice(offset, offset + pageSize)` with
/// `offset = (pageNum - 1) * pageSize`, so a zero, negative or `NaN` value
/// is not clamped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsPagination {
    pub page_size: Option<i64>,
    pub page_num: Option<i64>,
}

impl JsPagination {
    pub fn parse(params: &QueryParams, max_page_size: i64) -> Result<Self, String> {
        let read = |key: &str, default: i64| match qs_get_string(params, key) {
            Some(v) => js_parse_int(&v, false),
            None => Some(default),
        };
        let page_size = read("pageSize", 100);
        if page_size.is_some_and(|s| s > max_page_size) {
            return Err(format!("max allowed pageSize is {max_page_size}"));
        }
        Ok(Self {
            page_size,
            page_num: read("pageNum", 1),
        })
    }

    /// `slice`'s arguments, `None` when one is `NaN` (an empty page).
    fn bounds(&self) -> Option<(i64, i64)> {
        let size = self.page_size?;
        let start = self.page_num?.saturating_sub(1).saturating_mul(size);
        Some((start, start.saturating_add(size)))
    }

    /// The page of a list of `len` elements, as `Array.prototype.slice`
    /// takes it: a negative bound counts from the end.
    pub fn range(&self, len: usize) -> std::ops::Range<usize> {
        let Some((start, end)) = self.bounds() else {
            return 0..0;
        };
        let len = len as i64;
        let at = |i: i64| if i < 0 { (len + i).max(0) } else { i.min(len) } as usize;
        let (start, end) = (at(start), at(end));
        if start < end { start..end } else { 0..0 }
    }

    /// `(offset, limit)` for a query that pages in SQL, when the page does
    /// not depend on the list's length (no negative bound); otherwise
    /// `None`, and the caller counts first and takes [`Self::range`].
    pub fn sql_window(&self) -> Option<(i64, i64)> {
        match self.bounds() {
            None => Some((0, 0)),
            Some((start, end)) if start >= 0 && end >= 0 => Some((start, (end - start).max(0))),
            Some(_) => None,
        }
    }

    /// `(offset, limit)` for a JavaScript `slice`, `None` when either is `NaN`.
    pub fn offset_limit(&self) -> Option<(i64, i64)> {
        let (size, num) = (self.page_size?, self.page_num?);
        Some((num.saturating_sub(1).saturating_mul(size), size))
    }

    /// The `[start, end)` of `elements.slice(offset, offset + limit)` over
    /// `len` elements: a negative index counts from the end, a `NaN` is 0.
    pub fn slice_range(&self, len: usize) -> (usize, usize) {
        let r = self.range(len);
        (r.start, r.end)
    }
}

/// [`JsPagination::parse`], lamb2's `paginationObject`
/// (decentraland/lamb2:src/logic/pagination.ts:4-19).
pub fn parse_pagination_js(
    params: &QueryParams,
    max_page_size: i64,
) -> Result<JsPagination, String> {
    JsPagination::parse(params, max_page_size)
}

fn urlencoding_decode(s: &str) -> String {
    let mut bytes: Vec<u8> = Vec::with_capacity(s.len());
    let mut buf = [0u8; 4];
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch == '%' {
            let hi = chars.next().unwrap_or('0');
            let lo = chars.next().unwrap_or('0');
            let hex = format!("{}{}", hi, lo);
            if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                bytes.push(byte);
            } else {
                bytes.push(b'%');
                bytes.extend_from_slice(hi.encode_utf8(&mut buf).as_bytes());
                bytes.extend_from_slice(lo.encode_utf8(&mut buf).as_bytes());
            }
        } else if ch == '+' {
            bytes.push(b' ');
        } else {
            bytes.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn urlencoding_encode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(byte as char);
            }
            _ => {
                result.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    result
}

fn strip_array_index(key: &str) -> String {
    if let Some(bracket_pos) = key.find('[')
        && key.ends_with(']')
    {
        let inside = &key[bracket_pos + 1..key.len() - 1];
        if inside.chars().all(|c| c.is_ascii_digit()) {
            return key[..bracket_pos].to_string();
        }
    }
    key.to_string()
}

/// The value `qs` gives a handler that casts it to a string: a repeated key
/// is an array, which JavaScript joins with commas. `None` for a missing key
/// or a single empty value (both falsy).
fn qs_js_string(params: &QueryParams, key: &str) -> Option<String> {
    match params.get(key) {
        Some(values) if !values.is_empty() && !(values.len() == 1 && values[0].is_empty()) => {
            Some(values.join(","))
        }
        _ => None,
    }
}

/// The content server's `qsGetNumber`: absent when missing or empty, else
/// `parseInt`, absent on `NaN`.
pub fn qs_get_js_number(params: &QueryParams, key: &str) -> Option<i64> {
    qs_js_string(params, key).and_then(|s| js_parse_int(&s, false))
}

/// PostgreSQL's `timestamptz` range in Unix milliseconds (4714-11-24 BC to
/// 294276-12-31 AD).
const PG_TIMESTAMP_MS: std::ops::RangeInclusive<i64> = -210_866_803_200_000..=9_224_318_015_999_000;

/// [`qs_get_js_number`] for a millisecond bound (`from`, `to`), clamped to
/// what `to_timestamp` accepts. Upstream hands any value to PostgreSQL and
/// answers its "timestamp out of range" as a 500; no deployment lies outside
/// that range, so the clamped bound selects the same rows.
pub fn qs_get_js_timestamp(params: &QueryParams, key: &str) -> Option<i64> {
    qs_get_js_number(params, key)
        .map(|ms| ms.clamp(*PG_TIMESTAMP_MS.start(), *PG_TIMESTAMP_MS.end()))
}

/// The content server's `qsGetBoolean`: absent when missing or empty, else
/// whether the value is exactly `true` (a repeated key is an array, never
/// `'true'`).
pub fn qs_get_js_bool(params: &QueryParams, key: &str) -> Option<bool> {
    let values = params.get(key)?;
    match values.as_slice() {
        [] => None,
        [only] if only.is_empty() => None,
        [only] => Some(only == "true"),
        _ => Some(false),
    }
}

/// The content server's `paginationObject` (`src/controllers/utils.ts`): a
/// present `pageSize`/`pageNum` is read with `parseInt` (`10abc` is 10), and
/// one that reads as `NaN` (empty included) or below 1 is a 400.
pub fn parse_content_pagination(
    params: &QueryParams,
    max_page_size: i64,
) -> Result<Pagination, String> {
    let read = |key: &str, default: i64| match params.get(key).and_then(|v| v.first()) {
        Some(raw) => js_parse_int(raw, false),
        None => Some(default),
    };
    let page_size = match read("pageSize", 100) {
        Some(n) if n >= 1 => n,
        _ => return Err("pageSize must be a positive integer".to_string()),
    };
    let page_num = match read("pageNum", 1) {
        Some(n) if n >= 1 => n,
        _ => return Err("pageNum must be a positive integer".to_string()),
    };
    if page_size > max_page_size {
        return Err(format!("max allowed pageSize is {}", max_page_size));
    }
    Ok(Pagination {
        page_size,
        page_num,
        offset: page_num.saturating_sub(1).saturating_mul(page_size),
        limit: page_size,
    })
}

/// `qs.stringify(..., {arrayFormat: 'repeat'})` over keys in the given order:
/// each value percent-encoded as RFC 3986, empty values kept (`key=`).
pub fn to_ordered_query_string(pairs: &[(&str, Vec<String>)]) -> String {
    pairs
        .iter()
        .flat_map(|(key, values)| {
            values
                .iter()
                .map(move |v| format!("{}={}", urlencoding_encode(key), urlencoding_encode(v)))
        })
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_repeated_keys() {
        let params = parse_query_string("cid=abc&cid=def&cid=ghi");
        assert_eq!(qs_get_array(&params, "cid"), vec!["abc", "def", "ghi"]);
    }

    #[test]
    fn parse_indexed_array() {
        let params = parse_query_string("entityType[0]=scene&entityType[1]=profile");
        assert_eq!(
            qs_get_array(&params, "entityType"),
            vec!["scene", "profile"]
        );
    }

    #[test]
    fn parse_just_over_endpoint_cap_is_visible_to_handlers() {
        let query: String = (0..1001)
            .map(|i| format!("cid={}", i))
            .collect::<Vec<_>>()
            .join("&");
        let params = parse_query_string(&query);
        assert_eq!(qs_get_array(&params, "cid").len(), 1001);
    }

    #[test]
    fn parse_bounds_pathological_flood_to_param_limit() {
        let query: String = (0..3000)
            .map(|i| format!("cid={}", i))
            .collect::<Vec<_>>()
            .join("&");
        let params = parse_query_string(&query);
        assert!(qs_get_array(&params, "cid").len() <= MAX_QUERY_PARAMS);
    }

    #[test]
    fn js_parse_int_reads_a_numeric_prefix() {
        for (raw, want) in [
            ("10", Some(10)),
            ("10abc", Some(10)),
            ("1.5", Some(1)),
            ("  7", Some(7)),
            ("-3", Some(-3)),
            ("+4", Some(4)),
            ("", None),
            ("abc", None),
            ("-", None),
            ("0x10", Some(0)),
            ("99999999999999999999999", Some(i64::MAX)),
        ] {
            assert_eq!(js_parse_int(raw, false), want, "{raw:?}");
        }
    }

    #[test]
    fn js_number_and_bool_follow_qs_get_number_and_boolean() {
        let params = parse_query_string("a=10abc&b=&c=x&d=5&d=6&t=true&f=TRUE&e=&r=true&r=true");
        assert_eq!(qs_get_js_number(&params, "a"), Some(10));
        assert_eq!(qs_get_js_number(&params, "b"), None);
        assert_eq!(qs_get_js_number(&params, "c"), None);
        assert_eq!(qs_get_js_number(&params, "d"), Some(5));
        assert_eq!(qs_get_js_number(&params, "missing"), None);
        assert_eq!(qs_get_js_bool(&params, "t"), Some(true));
        assert_eq!(qs_get_js_bool(&params, "f"), Some(false));
        assert_eq!(qs_get_js_bool(&params, "e"), None);
        assert_eq!(qs_get_js_bool(&params, "r"), Some(false));
        assert_eq!(qs_get_js_bool(&params, "missing"), None);
    }

    #[test]
    fn js_pagination_reads_like_pagination_object() {
        let p = parse_content_pagination(&parse_query_string("pageSize=10abc&pageNum=1.5"), 1000)
            .unwrap();
        assert_eq!((p.page_size, p.page_num, p.offset), (10, 1, 0));
        let p = parse_content_pagination(&parse_query_string(""), 1000).unwrap();
        assert_eq!((p.page_size, p.page_num), (100, 1));
        for (query, message) in [
            ("pageSize=", "pageSize must be a positive integer"),
            ("pageSize=abc", "pageSize must be a positive integer"),
            ("pageSize=0", "pageSize must be a positive integer"),
            ("pageNum=", "pageNum must be a positive integer"),
            ("pageNum=x", "pageNum must be a positive integer"),
            ("pageSize=1001", "max allowed pageSize is 1000"),
        ] {
            assert_eq!(
                parse_content_pagination(&parse_query_string(query), 1000).unwrap_err(),
                message,
                "{query}"
            );
        }
    }

    #[test]
    fn ordered_query_string_keeps_order_and_empty_values() {
        let pairs = [
            ("pointer", vec!["0,0".to_string(), String::new()]),
            ("from", vec!["1".to_string()]),
        ];
        assert_eq!(
            to_ordered_query_string(&pairs),
            "pointer=0%2C0&pointer=&from=1"
        );
    }

    #[test]
    fn parse_pagination_defaults() {
        let params = parse_query_string("");
        let p = parse_pagination_with(
            &params,
            1000,
            OversizePolicy::Reject,
            NonPositivePolicy::Reject,
        )
        .unwrap();
        assert_eq!(p.page_size, 100);
        assert_eq!(p.page_num, 1);
        assert_eq!(p.offset, 0);
        assert_eq!(p.limit, 100);
    }

    #[test]
    fn camel_to_snake_works() {
        assert_eq!(camel_to_snake("localTimestamp"), "local_timestamp");
        assert_eq!(camel_to_snake("entityTimestamp"), "entity_timestamp");
    }

    #[test]
    fn entity_types_parse_singular_plural_and_case() {
        assert_eq!(parse_entity_type("scenes"), Some("scene"));
        assert_eq!(parse_entity_type("Profile"), Some("profile"));
        assert_eq!(parse_entity_type("outfits"), Some("outfits"));
        assert_eq!(parse_entity_type("outfit"), None);
        assert_eq!(parse_entity_type("totallybogus"), None);
        assert_eq!(parse_entity_type(" Emotes "), Some("emote"));
        assert_eq!(parse_entity_type("outfitss"), None);
        assert_eq!(parse_entity_type("sceness"), None);
        assert_eq!(parse_entity_type("s"), None);
        assert_eq!(parse_entity_type(""), None);
    }

    #[test]
    fn filter_array_treats_a_single_empty_value_as_absent() {
        let get = |q: &str| qs_get_filter_array(&parse_query_string(q), "pointer");
        assert!(get("").is_empty());
        assert!(get("pointer=").is_empty());
        assert!(get("pointer").is_empty());
        assert_eq!(get("pointer=a&pointer="), vec!["a", ""]);
        assert_eq!(get("pointer=&pointer="), vec!["", ""]);
        assert_eq!(get("pointer=a"), vec!["a"]);
    }

    #[test]
    fn pagination_oversize_clamp() {
        let params = parse_query_string("pageSize=99999");
        let p = parse_pagination_with(
            &params,
            1000,
            OversizePolicy::Clamp,
            NonPositivePolicy::ClampToOne,
        )
        .unwrap();
        assert_eq!(p.page_size, 1000);
        assert_eq!(p.limit, 1000);
    }

    #[test]
    fn pagination_oversize_reject() {
        let params = parse_query_string("pageSize=99999");
        let err = parse_pagination_with(
            &params,
            1000,
            OversizePolicy::Reject,
            NonPositivePolicy::PassThrough,
        )
        .unwrap_err();
        assert_eq!(err, "max allowed pageSize is 1000");
    }

    #[test]
    fn pagination_non_positive_clamp_to_one() {
        let params = parse_query_string("pageSize=0&pageNum=-3");
        let p = parse_pagination_with(
            &params,
            1000,
            OversizePolicy::Clamp,
            NonPositivePolicy::ClampToOne,
        )
        .unwrap();
        assert_eq!(p.page_size, 1);
        assert_eq!(p.page_num, 1);
        assert_eq!(p.offset, 0);
    }

    #[test]
    fn pagination_strict_rejects_non_positive() {
        let zero = parse_query_string("pageSize=0");
        assert_eq!(
            parse_pagination_with(
                &zero,
                1000,
                OversizePolicy::Reject,
                NonPositivePolicy::Reject
            )
            .unwrap_err(),
            "pageSize must be a positive integer"
        );
        let neg = parse_query_string("pageNum=-1");
        assert_eq!(
            parse_pagination_with(
                &neg,
                1000,
                OversizePolicy::Reject,
                NonPositivePolicy::Reject
            )
            .unwrap_err(),
            "pageNum must be a positive integer"
        );
    }

    #[test]
    fn pagination_passthrough_keeps_values() {
        let params = parse_query_string("pageSize=100&pageNum=0");
        let p = parse_pagination_with(
            &params,
            i64::MAX,
            OversizePolicy::Reject,
            NonPositivePolicy::PassThrough,
        )
        .unwrap();
        assert_eq!(p.page_num, 0);
        assert_eq!(p.offset, -100);
    }

    #[test]
    fn pagination_huge_pagenum_saturates() {
        let params = parse_query_string(&format!("pageNum={}&pageSize=100", i64::MAX));
        let p = parse_pagination_with(
            &params,
            1000,
            OversizePolicy::Reject,
            NonPositivePolicy::PassThrough,
        )
        .unwrap();
        assert_eq!(p.offset, i64::MAX);
    }

    #[test]
    fn decode_reassembles_multibyte_utf8() {
        let params = parse_query_string("textSearch=caf%C3%A9&name=%E4%B8%AD%E6%96%87");
        assert_eq!(
            qs_get_string(&params, "textSearch").as_deref(),
            Some("caf\u{E9}")
        );
        assert_eq!(
            qs_get_string(&params, "name").as_deref(),
            Some("\u{4E2D}\u{6587}")
        );
    }

    #[test]
    fn js_parse_int_reads_as_javascript_does() {
        for (s, hex, want) in [
            ("12", false, Some(12)),
            (" \t-7", false, Some(-7)),
            ("+3", false, Some(3)),
            ("1.5", false, Some(1)),
            ("12abc", false, Some(12)),
            ("0x10", false, Some(0)),
            ("0x10", true, Some(16)),
            ("-0X1f", true, Some(-31)),
            ("0x", true, None),
            ("", false, None),
            ("abc", false, None),
            ("-", false, None),
            ("99999999999999999999999", false, Some(i64::MAX)),
        ] {
            assert_eq!(js_parse_int(s, hex), want, "{s:?} hex={hex}");
        }
    }

    /// `paginationObject` plus `elements.slice(offset, offset + limit)`.
    #[test]
    fn js_pagination_slices_as_javascript_does() {
        let page = |q: &str| JsPagination::parse(&parse_query_string(q), i64::MAX).unwrap();
        let p = page("");
        assert_eq!((p.page_size, p.page_num), (Some(100), Some(1)));
        assert_eq!(p.range(150), 0..100);
        assert_eq!(p.sql_window(), Some((0, 100)));
        assert_eq!(page("pageSize=2&pageNum=2").range(5), 2..4);
        assert_eq!(page("pageSize=0").range(5), 0..0);
        // pageNum 0: slice(-2, 0), empty; pageNum -1: slice(-4, -2).
        assert_eq!(page("pageSize=2&pageNum=0").range(5), 0..0);
        assert_eq!(page("pageSize=2&pageNum=-1").range(5), 1..3);
        assert_eq!(page("pageSize=2&pageNum=-1").sql_window(), None);
        // A negative size: slice(-4, -6) is empty; slice(0, -1) all but the last.
        assert_eq!(page("pageSize=-2&pageNum=3").range(5), 0..0);
        assert_eq!(page("pageSize=-1&pageNum=1").range(5), 0..4);
        let nan = page("pageSize=abc&pageNum=1");
        assert_eq!((nan.page_size, nan.range(5)), (None, 0..0));
        assert_eq!(nan.sql_window(), Some((0, 0)));
        assert_eq!(page("pageSize=2.9&pageNum=2x").range(5), 2..4);
        assert_eq!(
            JsPagination::parse(&parse_query_string("pageSize=1001"), 1000).unwrap_err(),
            "max allowed pageSize is 1000"
        );
        assert!(JsPagination::parse(&parse_query_string("pageSize=x"), 1000).is_ok());
    }

    #[test]
    fn decode_plus_is_space_and_malformed_percent_preserved() {
        let params = parse_query_string("q=a+b&bad=%ZZ");
        assert_eq!(qs_get_string(&params, "q").as_deref(), Some("a b"));
        assert_eq!(qs_get_string(&params, "bad").as_deref(), Some("%ZZ"));
    }

    #[test]
    fn parse_int_js_reads_the_leading_digits() {
        assert_eq!(parse_int_js("10abc"), Some(10));
        assert_eq!(parse_int_js("1.5"), Some(1));
        assert_eq!(parse_int_js(" -7x"), Some(-7));
        assert_eq!(parse_int_js("+3"), Some(3));
        assert_eq!(parse_int_js("abc"), None);
        assert_eq!(parse_int_js(""), None);
        assert_eq!(parse_int_js("-"), None);
        assert_eq!(parse_int_js("99999999999999999999"), Some(i64::MAX));
        assert_eq!(parse_int_js("\u{feff}\u{2028}5"), Some(5), "JS whitespace");
        assert_eq!(parse_int_js("\u{85}5"), None, "U+0085 is not JS whitespace");
    }

    #[test]
    fn js_pagination_keeps_nan_and_slices_like_javascript() {
        let p = |qs: &str| parse_pagination_js(&parse_query_string(qs), 1000);
        assert_eq!(
            p("").unwrap(),
            JsPagination {
                page_size: Some(100),
                page_num: Some(1)
            }
        );
        assert_eq!(
            p("pageSize=10abc&pageNum=2.9").unwrap(),
            JsPagination {
                page_size: Some(10),
                page_num: Some(2)
            }
        );
        let nan = p("pageSize=abc").unwrap();
        assert_eq!(nan.page_size, None);
        assert_eq!(nan.slice_range(50), (0, 0), "a NaN pages nothing");
        assert_eq!(
            p("pageSize=").unwrap().page_size,
            None,
            "present but empty is NaN"
        );
        assert_eq!(
            p("pageSize=1001").unwrap_err(),
            "max allowed pageSize is 1000"
        );
        // pageNum=0: slice(-10, 0) is empty; pageNum=-1 with 25 elements:
        // slice(-20, -10) is [5, 15).
        assert_eq!(p("pageSize=10&pageNum=0").unwrap().slice_range(25), (0, 0));
        assert_eq!(
            p("pageSize=10&pageNum=-1").unwrap().slice_range(25),
            (5, 15)
        );
        assert_eq!(
            p("pageSize=10&pageNum=3").unwrap().slice_range(25),
            (20, 25)
        );
    }
}
