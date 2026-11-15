//! The JavaScript semantics the storage routes' wire contract inherits from
//! world-storage-service (Node.js): `JSON.parse` key order, `JSON.stringify`,
//! `Number(string)`, `String.prototype.trim`, truthiness, `Date.parse` and
//! `Buffer.from(_, 'base64')`. Local time is read as UTC, as the upstream
//! service runs.

use std::borrow::Cow;
use std::ops::Deref;

pub use crate::shared::js::{is_js_whitespace, js_trim, number_from_str, number_to_string};

use super::js_json::{self, write_js_string};

/// A `JSON.parse` result: numbers are doubles and an object keeps its own
/// keys in JavaScript order (array-index keys ascending, then insertion
/// order; a repeated key keeps its first position and its last value).
/// Containers clone, drop, compare and print iteratively, so any parsed
/// depth is safe.
#[derive(Clone)]
pub enum JsValue {
    Null,
    Bool(bool),
    Number(f64),
    String(JsString),
    Array(JsArray),
    Object(JsObject),
}

/// A JavaScript string. It reads as the UTF-8 text Node writes (a lone
/// surrogate as U+FFFD); the UTF-16 units are kept when they hold a lone
/// surrogate, so the string stays distinct and `JSON.stringify` escapes it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct JsString {
    text: String,
    ill_formed: Option<Box<[u16]>>,
}

impl JsString {
    /// Whether the units are valid UTF-16 (no lone surrogate).
    pub fn is_well_formed(&self) -> bool {
        self.ill_formed.is_none()
    }

    pub fn from_units(units: Vec<u16>) -> Self {
        match String::from_utf16(&units) {
            Ok(text) => Self {
                text,
                ill_formed: None,
            },
            Err(_) => Self {
                text: String::from_utf16_lossy(&units),
                ill_formed: Some(units.into_boxed_slice()),
            },
        }
    }

    pub fn units(&self) -> Cow<'_, [u16]> {
        match &self.ill_formed {
            Some(units) => Cow::Borrowed(units),
            None => Cow::Owned(self.text.encode_utf16().collect()),
        }
    }
}

// JSON can represent lone UTF-16 surrogates only as escapes. Serialize the
// already escaped JSON token without passing it through a UTF-8 string value.
impl serde::Serialize for JsString {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut json = String::new();
        write_js_string(&mut json, &self.units());
        let raw =
            serde_json::value::RawValue::from_string(json).map_err(serde::ser::Error::custom)?;
        serde::Serialize::serialize(&raw, serializer)
    }
}

impl Deref for JsString {
    type Target = str;

    fn deref(&self) -> &str {
        &self.text
    }
}

impl From<&str> for JsString {
    fn from(text: &str) -> Self {
        Self {
            text: text.to_string(),
            ill_formed: None,
        }
    }
}

impl std::fmt::Display for JsString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

impl PartialEq<str> for JsString {
    fn eq(&self, other: &str) -> bool {
        self.ill_formed.is_none() && self.text == other
    }
}

#[derive(Default)]
pub struct JsArray(Vec<JsValue>);

impl Deref for JsArray {
    type Target = [JsValue];

    fn deref(&self) -> &[JsValue] {
        &self.0
    }
}

#[derive(Default)]
pub struct JsObject(Vec<(JsString, JsValue)>);

impl JsObject {
    pub fn get(&self, key: &str) -> Option<&JsValue> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(|(k, _)| &**k)
    }
}

impl PartialEq for JsValue {
    fn eq(&self, other: &Self) -> bool {
        values_eq(vec![(self, other)])
    }
}

impl PartialEq for JsArray {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len() && values_eq(self.iter().zip(other.iter()).collect())
    }
}

impl PartialEq for JsObject {
    fn eq(&self, other: &Self) -> bool {
        self.0.len() == other.0.len()
            && self.0.iter().zip(&other.0).all(|((a, _), (b, _))| a == b)
            && values_eq(
                self.0
                    .iter()
                    .zip(&other.0)
                    .map(|((_, a), (_, b))| (a, b))
                    .collect(),
            )
    }
}

/// Structural equality over the stacked pairs (`NaN` unequal, as `f64`).
fn values_eq(mut pending: Vec<(&JsValue, &JsValue)>) -> bool {
    while let Some(pair) = pending.pop() {
        match pair {
            (JsValue::Array(a), JsValue::Array(b)) if a.len() == b.len() => {
                pending.extend(a.iter().zip(b.iter()));
            }
            (JsValue::Object(a), JsValue::Object(b)) if a.0.len() == b.0.len() => {
                for ((ka, va), (kb, vb)) in a.0.iter().zip(&b.0) {
                    if ka != kb {
                        return false;
                    }
                    pending.push((va, vb));
                }
            }
            (JsValue::Null, JsValue::Null) => {}
            (JsValue::Bool(a), JsValue::Bool(b)) if a == b => {}
            (JsValue::Number(a), JsValue::Number(b)) if a == b => {}
            (JsValue::String(a), JsValue::String(b)) if a == b => {}
            _ => return false,
        }
    }
    true
}

/// Debug output is the `JSON.stringify` text (non-finite numbers as `null`).
impl std::fmt::Debug for JsValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.stringify())
    }
}

impl std::fmt::Debug for JsArray {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl std::fmt::Debug for JsObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map()
            .entries(self.0.iter().map(|(k, v)| (&**k, v)))
            .finish()
    }
}

impl Clone for JsArray {
    fn clone(&self) -> Self {
        Self(clone_values(
            self.0.iter().rev().map(CloneTask::Visit).collect(),
        ))
    }
}

impl Clone for JsObject {
    fn clone(&self) -> Self {
        let values = clone_values(
            self.0
                .iter()
                .rev()
                .map(|(_, v)| CloneTask::Visit(v))
                .collect(),
        );
        Self(self.0.iter().map(|(k, _)| k.clone()).zip(values).collect())
    }
}

enum CloneTask<'a> {
    Visit(&'a JsValue),
    Array(usize),
    Object(&'a JsObject),
}

/// Clones the values the stacked tasks visit, in visiting order.
fn clone_values(mut tasks: Vec<CloneTask<'_>>) -> Vec<JsValue> {
    let mut out = Vec::new();
    while let Some(task) = tasks.pop() {
        match task {
            CloneTask::Visit(JsValue::Array(a)) => {
                tasks.push(CloneTask::Array(a.len()));
                tasks.extend(a.iter().rev().map(CloneTask::Visit));
            }
            CloneTask::Visit(JsValue::Object(o)) => {
                tasks.push(CloneTask::Object(o));
                tasks.extend(o.0.iter().rev().map(|(_, v)| CloneTask::Visit(v)));
            }
            CloneTask::Visit(JsValue::Null) => out.push(JsValue::Null),
            CloneTask::Visit(JsValue::Bool(b)) => out.push(JsValue::Bool(*b)),
            CloneTask::Visit(JsValue::Number(n)) => out.push(JsValue::Number(*n)),
            CloneTask::Visit(JsValue::String(s)) => out.push(JsValue::String(s.clone())),
            CloneTask::Array(len) => {
                let items = out.split_off(out.len() - len);
                out.push(JsValue::Array(JsArray(items)));
            }
            CloneTask::Object(o) => {
                let values = out.split_off(out.len() - o.0.len());
                let entries = o.0.iter().map(|(k, _)| k.clone()).zip(values).collect();
                out.push(JsValue::Object(JsObject(entries)));
            }
        }
    }
    out
}

/// Empties every nested container before it drops, so dropping is shallow.
fn drop_values(mut pending: Vec<JsValue>) {
    while let Some(mut value) = pending.pop() {
        match &mut value {
            JsValue::Array(a) => pending.append(&mut a.0),
            JsValue::Object(o) => pending.extend(o.0.drain(..).map(|(_, v)| v)),
            _ => {}
        }
    }
}

impl Drop for JsArray {
    fn drop(&mut self) {
        drop_values(std::mem::take(&mut self.0));
    }
}

impl Drop for JsObject {
    fn drop(&mut self) {
        drop_values(self.0.drain(..).map(|(_, v)| v).collect());
    }
}

impl JsValue {
    /// `JSON.parse`, over the string's UTF-16 units.
    pub fn parse(raw: &str) -> Option<Self> {
        let source: Vec<u16> = raw.encode_utf16().collect();
        js_json::parse(&source)
            .ok()
            .map(js_json::Doc::into_js_value)
    }

    pub(super) fn array(items: Vec<JsValue>) -> Self {
        Self::Array(JsArray(items))
    }

    pub(super) fn object(entries: Vec<(JsString, JsValue)>) -> Self {
        Self::Object(JsObject(entries))
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn truthy(&self) -> bool {
        match self {
            Self::Null => false,
            Self::Bool(b) => *b,
            Self::Number(n) => *n != 0.0 && !n.is_nan(),
            Self::String(s) => !s.is_empty(),
            Self::Array(_) | Self::Object(_) => true,
        }
    }

    /// `String(value)`; an array joins its items with `,`, `null` as empty.
    pub fn to_js_string(&self) -> String {
        let items = match self {
            Self::Null => return "null".to_string(),
            Self::Bool(b) => return b.to_string(),
            Self::Number(n) => return number_to_string(*n),
            Self::String(s) => return s.to_string(),
            Self::Object(_) => return "[object Object]".to_string(),
            Self::Array(items) => items,
        };
        let mut out = String::new();
        let mut stack = vec![(items.iter(), false)];
        while let Some((iter, joined)) = stack.last_mut() {
            let Some(item) = iter.next() else {
                stack.pop();
                continue;
            };
            if std::mem::replace(joined, true) {
                out.push(',');
            }
            match item {
                Self::Null => {}
                Self::Array(inner) => stack.push((inner.iter(), false)),
                scalar => out.push_str(&scalar.to_js_string()),
            }
        }
        out
    }

    /// `JSON.stringify(value)`.
    pub fn stringify(&self) -> String {
        enum Open<'a> {
            Array(std::slice::Iter<'a, JsValue>),
            Object(std::slice::Iter<'a, (JsString, JsValue)>),
        }
        let mut out = String::new();
        let mut stack: Vec<(Open<'_>, bool)> = Vec::new();
        let mut next = Some(self);
        loop {
            match next.take() {
                Some(Self::Null) => out.push_str("null"),
                Some(Self::Bool(b)) => out.push_str(if *b { "true" } else { "false" }),
                Some(Self::Number(n)) => out.push_str(&js_json::js_number(*n)),
                Some(Self::String(s)) => write_js_string(&mut out, &s.units()),
                Some(Self::Array(a)) => {
                    out.push('[');
                    stack.push((Open::Array(a.iter()), false));
                }
                Some(Self::Object(o)) => {
                    out.push('{');
                    stack.push((Open::Object(o.0.iter()), false));
                }
                None => {}
            }
            let Some((open, written)) = stack.last_mut() else {
                return out;
            };
            let (key, value) = match open {
                Open::Array(iter) => (None, iter.next()),
                Open::Object(iter) => match iter.next() {
                    Some((k, v)) => (Some(k), Some(v)),
                    None => (None, None),
                },
            };
            let Some(value) = value else {
                out.push(if matches!(open, Open::Array(_)) {
                    ']'
                } else {
                    '}'
                });
                stack.pop();
                continue;
            };
            if std::mem::replace(written, true) {
                out.push(',');
            }
            if let Some(key) = key {
                write_js_string(&mut out, &key.units());
                out.push(':');
            }
            next = Some(value);
        }
    }
}

/// crypto-middleware's `safe()`: at most 64 UTF-16 code units, then `…`.
pub fn safe(value: &str) -> String {
    const MAX: usize = 64;
    if value.encode_utf16().count() <= MAX {
        return value.to_string();
    }
    let mut units = 0;
    let mut out = String::new();
    for c in value.chars() {
        units += c.len_utf16();
        if units > MAX {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

/// `Buffer.from(input, 'base64')`: both alphabets, characters outside them
/// skipped, the first `=` ends the input, a trailing lone sextet dropped, and
/// the output capped at the size Node reserves from the input's length.
pub fn node_base64_decode(input: &str) -> Vec<u8> {
    let bytes = input.as_bytes();
    let capacity = {
        let mut size = bytes.len();
        if size < 2 {
            0
        } else {
            if bytes[size - 1] == b'=' {
                size -= 1;
                if bytes[size - 1] == b'=' {
                    size -= 1;
                }
            }
            let rem = size % 4;
            let mut out = size / 4 * 3;
            if rem != 0 && !(out == 0 && rem == 1) {
                out += 1 + usize::from(rem == 3);
            }
            out
        }
    };
    let mut sextets = Vec::with_capacity(bytes.len());
    for &b in bytes {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => continue,
        };
        sextets.push(v);
    }
    let mut out = Vec::with_capacity(sextets.len() * 3 / 4);
    for chunk in sextets.chunks(4) {
        if chunk.len() >= 2 {
            out.push((chunk[0] << 2) | (chunk[1] >> 4));
        }
        if chunk.len() >= 3 {
            out.push((chunk[1] << 4) | (chunk[2] >> 2));
        }
        if chunk.len() == 4 {
            out.push((chunk[2] << 6) | chunk[3]);
        }
    }
    out.truncate(capacity);
    out
}

const MS_PER_DAY: f64 = 86_400_000.0;
const MAX_TIME_MS: f64 = 8.64e15;

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    // Howard Hinnant's algorithm; month 1..=12, day may overflow the month.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468 + (day - 1)
}

struct Fields {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    millis: i64,
    /// Minutes east of UTC; `None` is local time, read as UTC.
    offset_minutes: Option<i64>,
}

impl Fields {
    fn time_value(&self) -> Option<f64> {
        if !(1..=12).contains(&self.month) || !(1..=31).contains(&self.day) {
            return None;
        }
        let midnight_24 =
            self.hour == 24 && self.minute == 0 && self.second == 0 && self.millis == 0;
        if !(self.hour < 24 || midnight_24) || self.minute > 59 || self.second > 59 {
            return None;
        }
        let days = days_from_civil(self.year, self.month, self.day) as f64;
        let ms = days * MS_PER_DAY
            + (self.hour * 3_600_000 + self.minute * 60_000 + self.second * 1000 + self.millis)
                as f64
            - (self.offset_minutes.unwrap_or(0) * 60_000) as f64;
        (ms.abs() <= MAX_TIME_MS).then_some(ms)
    }
}

/// `Date.parse`: the ECMAScript date-time string format, then the legacy
/// forms V8 accepts (`toUTCString`, `toString`, `Mon DD YYYY`, `M/D/YYYY`,
/// `YYYY/MM/DD`, with times, AM/PM and `GMT`/`UTC`/`Z`/`+hhmm` zones).
/// `None` is NaN.
pub fn date_parse(raw: &str) -> Option<f64> {
    parse_iso(raw)
        .or_else(|| parse_legacy(raw))
        .and_then(|f| f.time_value())
}

fn take_digits(s: &[u8], i: &mut usize, exactly: usize) -> Option<i64> {
    let end = *i + exactly;
    if end > s.len() || !s[*i..end].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let v = std::str::from_utf8(&s[*i..end]).ok()?.parse().ok()?;
    *i = end;
    Some(v)
}

fn parse_iso(raw: &str) -> Option<Fields> {
    let s = raw.as_bytes();
    let mut i = 0;
    let year = match s.first()? {
        b'+' | b'-' => {
            let negative = s[0] == b'-';
            i = 1;
            let y = take_digits(s, &mut i, 6)?;
            if negative && y == 0 {
                return None;
            }
            if negative { -y } else { y }
        }
        _ => take_digits(s, &mut i, 4)?,
    };
    let mut f = Fields {
        year,
        month: 1,
        day: 1,
        hour: 0,
        minute: 0,
        second: 0,
        millis: 0,
        offset_minutes: Some(0),
    };
    if s.get(i) == Some(&b'-') {
        i += 1;
        f.month = take_digits(s, &mut i, 2)?;
        if s.get(i) == Some(&b'-') {
            i += 1;
            f.day = take_digits(s, &mut i, 2)?;
        }
    }
    if i == s.len() {
        return Some(f);
    }
    if !matches!(s[i], b'T' | b't') {
        return None;
    }
    i += 1;
    f.offset_minutes = None;
    f.hour = take_digits(s, &mut i, 2)?;
    if s.get(i) != Some(&b':') {
        return None;
    }
    i += 1;
    f.minute = take_digits(s, &mut i, 2)?;
    if s.get(i) == Some(&b':') {
        i += 1;
        f.second = take_digits(s, &mut i, 2)?;
        if s.get(i) == Some(&b'.') {
            i += 1;
            let start = i;
            while i < s.len() && s[i].is_ascii_digit() {
                i += 1;
            }
            if i == start {
                return None;
            }
            let frac = std::str::from_utf8(&s[start..i.min(start + 3)]).ok()?;
            f.millis = format!("{frac:0<3}").parse().ok()?;
        }
    }
    match s.get(i) {
        None => {}
        Some(b'Z' | b'z') if i + 1 == s.len() => f.offset_minutes = Some(0),
        Some(sign @ (b'+' | b'-')) => {
            let sign = if *sign == b'-' { -1 } else { 1 };
            i += 1;
            let h = take_digits(s, &mut i, 2)?;
            if s.get(i) != Some(&b':') {
                return None;
            }
            i += 1;
            let m = take_digits(s, &mut i, 2)?;
            if i != s.len() || h > 23 || m > 59 {
                return None;
            }
            f.offset_minutes = Some(sign * (h * 60 + m));
        }
        _ => return None,
    }
    Some(f)
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

fn parse_legacy(raw: &str) -> Option<Fields> {
    let s: Vec<char> = raw.chars().collect();
    let mut i = 0;
    let mut numbers: Vec<(i64, usize)> = Vec::new();
    let mut named_month: Option<i64> = None;
    let mut time: Option<(i64, i64, i64, i64)> = None;
    let mut meridian: Option<bool> = None;
    let mut offset: Option<i64> = None;
    let read_number = |i: &mut usize| -> Option<(i64, usize)> {
        let start = *i;
        while *i < s.len() && s[*i].is_ascii_digit() {
            *i += 1;
        }
        let len = *i - start;
        if len == 0 || len > 9 {
            return None;
        }
        let v: String = s[start..*i].iter().collect();
        Some((v.parse().ok()?, len))
    };
    while i < s.len() {
        let c = s[i];
        if c.is_whitespace() || c == ',' {
            i += 1;
        } else if c == '(' {
            let mut depth = 0;
            while i < s.len() {
                match s[i] {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
                i += 1;
                if depth == 0 {
                    break;
                }
            }
        } else if c.is_ascii_digit() {
            let (n, len) = read_number(&mut i)?;
            if s.get(i) == Some(&':') {
                if time.is_some() {
                    return None;
                }
                i += 1;
                let (minute, _) = read_number(&mut i)?;
                let (mut second, mut millis) = (0, 0);
                if s.get(i) == Some(&':') {
                    i += 1;
                    second = read_number(&mut i)?.0;
                    if s.get(i) == Some(&'.') {
                        i += 1;
                        let start = i;
                        while i < s.len() && s[i].is_ascii_digit() {
                            i += 1;
                        }
                        let frac: String = s[start..i.min(start + 3)].iter().collect();
                        millis = format!("{frac:0<3}").parse().ok()?;
                    }
                }
                time = Some((n, minute, second, millis));
            } else {
                if numbers.len() == 3 {
                    return None;
                }
                numbers.push((n, len));
                if matches!(s.get(i), Some('/' | '-' | '.')) {
                    i += 1;
                }
            }
        } else if (c == '+' || c == '-') && (time.is_some() || offset == Some(0)) {
            let sign = if c == '-' { -1 } else { 1 };
            i += 1;
            let (n, len) = read_number(&mut i)?;
            let minutes = if s.get(i) == Some(&':') {
                i += 1;
                n * 60 + read_number(&mut i)?.0
            } else if len <= 2 {
                n * 60
            } else {
                (n / 100) * 60 + n % 100
            };
            offset = Some(sign * minutes);
        } else if c.is_alphabetic() {
            let start = i;
            while i < s.len() && s[i].is_alphabetic() {
                i += 1;
            }
            let word: String = s[start..i].iter().collect::<String>().to_lowercase();
            let prefix: String = word.chars().take(3).collect();
            match word.as_str() {
                "am" | "pm" => meridian = Some(word == "pm"),
                "ut" | "utc" | "gmt" | "z" => offset = Some(0),
                "t" => {}
                _ if word.len() >= 3 && MONTHS.contains(&prefix.as_str()) => {
                    named_month = Some(MONTHS.iter().position(|m| *m == prefix)? as i64 + 1)
                }
                _ if word.len() >= 3 && WEEKDAYS.contains(&prefix.as_str()) => {}
                _ if numbers.is_empty() && time.is_none() => {}
                _ => return None,
            }
        } else {
            return None;
        }
    }

    let is_day = |n: i64| (1..=31).contains(&n);
    let (year, month, day) = match named_month {
        None => match numbers.as_slice() {
            [(a, _), (b, _), (c, _)] if !is_day(*a) => (*a, *b, *c),
            [(a, _), (b, _), (c, _)] => (*c, *a, *b),
            _ => return None,
        },
        Some(month) => match numbers.as_slice() {
            [(a, _), (b, _)] if !is_day(*a) => (*a, month, *b),
            [(a, _), (b, _)] => (*b, month, *a),
            _ => return None,
        },
    };
    let year = match year {
        0..=49 => year + 2000,
        50..=99 => year + 1900,
        y => y,
    };
    let (mut hour, minute, second, millis) = time.unwrap_or((0, 0, 0, 0));
    if let Some(pm) = meridian {
        if !(0..=12).contains(&hour) {
            return None;
        }
        hour = hour % 12 + if pm { 12 } else { 0 };
    }
    Some(Fields {
        year,
        month,
        day,
        hour,
        minute,
        second,
        millis,
        offset_minutes: offset,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_parse_keeps_javascript_key_order() {
        let v = JsValue::parse(r#"{"b":1,"a":2,"10":3,"2":4,"b":5,"01":6}"#).unwrap();
        assert_eq!(v.stringify(), r#"{"2":4,"10":3,"b":5,"a":2,"01":6}"#);
        let v = JsValue::parse(r#"[1.0,1e21,1.5e-7,-0,"é\n",null,true]"#).unwrap();
        assert_eq!(v.stringify(), r#"[1,1e+21,1.5e-7,0,"é\n",null,true]"#);
    }

    /// What `JSON.parse` accepts and `serde_json` refuses: lone surrogates,
    /// overflowing numbers and nesting past 128.
    #[test]
    fn json_parse_accepts_what_v8_does() {
        let v = JsValue::parse(r#"{"a":"x\ud800","\udc00":1,"\udc01":2}"#).unwrap();
        assert_eq!(v.stringify(), r#"{"a":"x\ud800","\udc00":1,"\udc01":2}"#);
        let JsValue::Object(o) = &v else {
            panic!("an object")
        };
        assert_eq!(o.get("a").and_then(JsValue::as_str), Some("x\u{fffd}"));
        assert_ne!(o.get("a"), Some(&JsValue::String("x\u{fffd}".into())));
        assert_eq!(
            o.keys().count(),
            3,
            "distinct lone surrogates stay distinct"
        );
        assert_eq!(o.get("\u{fffd}"), None);

        let v = JsValue::parse("[1e400,-1e400]").unwrap();
        assert_eq!(v.stringify(), "[null,null]");
        assert_eq!(v.to_js_string(), "Infinity,-Infinity");

        let depth = 200_000;
        let text = format!("{}null{}", "[{\"a\":".repeat(depth), "}]".repeat(depth));
        let v = JsValue::parse(&text).unwrap();
        let copy = v.clone();
        assert_eq!(copy.stringify(), text);
        assert_eq!(copy, v);
        assert_ne!(
            copy,
            JsValue::parse(&text.replacen("null", "0", 1)).unwrap()
        );
        assert_eq!(format!("{copy:?}"), text);
        drop(copy);
        let v = JsValue::parse(&format!("{}2{}", "[".repeat(depth), "]".repeat(depth))).unwrap();
        assert_eq!(v.to_js_string(), "2");
        assert_eq!(
            JsValue::parse("[1,null,[2,[3]],{}]")
                .unwrap()
                .to_js_string(),
            "1,,2,3,[object Object]"
        );
    }

    #[test]
    fn safe_truncates_to_64_utf16_units() {
        assert_eq!(safe("short"), "short");
        let long = "x".repeat(65);
        assert_eq!(safe(&long), format!("{}…", "x".repeat(64)));
    }

    #[test]
    fn node_base64_is_lenient() {
        let standard = "eyJhIjoiPz4/In0="; // {"a":"?>?"}
        assert_eq!(node_base64_decode(standard), br#"{"a":"?>?"}"#);
        assert_eq!(node_base64_decode("eyJhIjoiPz4_In0"), br#"{"a":"?>?"}"#);
        assert_eq!(node_base64_decode("eyJh\nIjoi Pz4/In0="), br#"{"a":"?>?"}"#);
        assert_eq!(node_base64_decode("YQ==YWJj"), b"a");
        assert_eq!(node_base64_decode("Y"), b"");
        assert_eq!(node_base64_decode("!!!"), b"");
    }

    #[test]
    fn date_parse_accepts_what_v8_does() {
        let at = |s: &str| date_parse(s);
        let base = 1_893_456_000_000.0; // 2030-01-01T00:00:00Z
        for s in [
            "2030-01-01T00:00:00.000Z",
            "2030-01-01T00:00:00Z",
            "2030-01-01T00:00Z",
            "2030-01-01T00:00:00",
            "2030-01-01",
            "2030-01-01T01:00:00+01:00",
            "2030-01-01t00:00:00z",
            "Tue, 01 Jan 2030 00:00:00 GMT",
            "Tue Jan 01 2030 00:00:00 GMT+0000 (Coordinated Universal Time)",
            "Tue Jan 01 2030 01:00:00 GMT+0100",
            "January 1, 2030",
            "Jan 1 2030 12:00:00 AM",
            "1/1/2030",
            "2030/01/01",
            "2030-01-01 00:00:00",
            "2029-12-31T24:00:00Z",
        ] {
            assert_eq!(at(s), Some(base), "{s}");
        }
        assert_eq!(at("2030-01-01T00:00:00.1239Z"), Some(base + 123.0));
        assert_eq!(at("2019-02-31"), at("2019-03-03"), "V8 rolls the day over");
        for s in [
            "tomorrow",
            "",
            "2030-13-01",
            "2030-01-01T25:00:00Z",
            "1700000000000",
            "-000000-01-01",
        ] {
            assert_eq!(at(s), None, "{s}");
        }
    }
}
