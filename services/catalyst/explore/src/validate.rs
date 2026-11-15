//! The old services' request validation, ported as a declarative subset of
//! the ajv JSON schemas they compiled (decentraland-gatsby `Schema/index.js`:
//! ajv with ajv-formats plus the `address`, `int`, `uint` and `boolean`
//! formats), so a malformed query or body answers the same 400 with the same
//! ajv error objects and messages. `allErrors` is off: the first failing
//! keyword stops the walk, except `oneOf`, which reports every branch (ajv).
//!
//! Two envelopes wrap the same errors (the fixtures hold both):
//! - [`Flavour::Wkc`]: `Router.validator` (`Route/wkc/routes/Router.js:52-80`),
//!   `"Error validating input:\n- …"` with `data: {messages, body, errors}`;
//!   the places list, by-id, map, world and destination reads and the
//!   favorites/likes routes.
//! - [`Flavour::Gatsby`]: `createValidator` (`Route/validate.js:9-41`, and the
//!   places `shared/validate.ts` wrapper), `"Invalid data was sent to the
//!   server"` with `data: {errors, messages, body}`; every events route and
//!   the places admin and ranking writes.
//!
//! The schemas below are the upstream ones (`decentraland/places@e660fbc2`,
//! `decentraland/events@b60d5bbd`), cited per item.

use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};

use regex::Regex;
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::auth::is_eth_address;
use crate::shared::errors::ApiError;

/// Which validator wrapper upstream used on the route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavour {
    /// `Router.validator`: `Error validating input:\n- …`.
    Wkc,
    /// `createValidator`: `Invalid data was sent to the server`.
    Gatsby,
}

pub const GATSBY_INVALID_DATA: &str = "Invalid data was sent to the server";
pub const WKC_INVALID_INPUT_PREFIX: &str = "Error validating input:\n- ";

/// One ajv `ErrorObject` as upstream serialised it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AjvError {
    pub instance_path: String,
    pub schema_path: String,
    pub keyword: &'static str,
    pub params: Value,
    pub message: String,
}

impl AjvError {
    /// gatsby's message line: `instancePath.slice(1) + message + JSON(params)`
    /// (`Router.js:57-71`, `validate.js:11-25`).
    pub fn line(&self) -> String {
        let mut out = String::new();
        if !self.instance_path.is_empty() {
            out.push_str(&self.instance_path[1..]);
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&self.message);
        out.push(' ');
        out.push_str(&self.params.to_string());
        out
    }
}

/// The string formats the old services registered (`Schema/index.js:15-46`,
/// ajv-formats in "full" mode).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// validator.js `isInt(value, {min: 0})`: `^[-+]?[0-9]+$` and not negative.
    Uint,
    /// validator.js `isInt`: `^[-+]?[0-9]+$`.
    Int,
    /// ajv-formats `uuid`, case-insensitive, optional `urn:uuid:` prefix.
    Uuid,
    /// ajv-formats `date-time`: RFC 3339 date, `T`/space, time with zone.
    DateTime,
    /// validator.js `isEthereumAddress`: `^(0x)[0-9a-fA-F]{40}$`.
    Address,
    /// `0`, `1`, `true`, `false`, `True`, `False`.
    Boolean,
    /// ajv-formats `uri` (full): an absolute URI with a scheme, no whitespace.
    Uri,
    /// ajv-formats `email` (its fast regex, case-insensitive).
    Email,
}

impl Format {
    pub fn name(self) -> &'static str {
        match self {
            Format::Uint => "uint",
            Format::Int => "int",
            Format::Uuid => "uuid",
            Format::DateTime => "date-time",
            Format::Address => "address",
            Format::Boolean => "boolean",
            Format::Uri => "uri",
            Format::Email => "email",
        }
    }

    pub fn accepts(self, value: &str) -> bool {
        match self {
            Format::Int => is_js_int(value).is_some(),
            Format::Uint => is_js_int(value).is_some_and(|negative| !negative),
            Format::Uuid => is_uuid(value),
            Format::DateTime => is_date_time(value),
            Format::Address => is_eth_address(value),
            Format::Boolean => matches!(value, "0" | "1" | "true" | "false" | "True" | "False"),
            Format::Uri => is_uri(value),
            Format::Email => is_email(value),
        }
    }
}

/// ajv-formats `email` (`formats.js`, the fast regex): a local part of
/// atoms, `@`, dot-separated labels without a trailing hyphen.
fn is_email(value: &str) -> bool {
    static EMAIL: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r"(?i)^[a-z0-9!#$%&'*+/=?^_`{|}~-]+(?:\.[a-z0-9!#$%&'*+/=?^_`{|}~-]+)*@(?:[a-z0-9](?:[a-z0-9-]*[a-z0-9])?\.)+[a-z0-9](?:[a-z0-9-]*[a-z0-9])?$",
        )
        .unwrap()
    });
    EMAIL.is_match(value)
}

/// ajv-formats `uri` (full mode): `scheme:` then a hierarchical part with no
/// whitespace; `javascript:alert(1)` passes it (the events schema's
/// `pattern: ^https?://` is what refuses it, as upstream's test shows).
fn is_uri(value: &str) -> bool {
    static SCHEME: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^[A-Za-z][A-Za-z0-9+.-]*:").unwrap());
    !value.is_empty()
        && !value.chars().any(char::is_whitespace)
        && SCHEME.is_match(value)
        && (value.contains('/') || value.contains(':'))
}

/// `Some(is_negative)` when `value` is an integer literal (leading zeroes
/// allowed, as validator.js defaults); `-0`, `+0` and `000` are not negative.
fn is_js_int(value: &str) -> Option<bool> {
    let (negative, digits) = match value.as_bytes().first()? {
        b'-' => (true, &value[1..]),
        b'+' => (false, &value[1..]),
        _ => (false, value),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(negative && digits.bytes().any(|b| b != b'0'))
}

fn is_uuid(value: &str) -> bool {
    let body = value
        .strip_prefix("urn:uuid:")
        .or_else(|| value.strip_prefix("URN:UUID:"))
        .unwrap_or(value);
    let groups: Vec<&str> = body.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8usize, 4, 4, 4, 12])
            .all(|(g, len)| g.len() == len && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// ajv-formats `date-time` (full): `date` + `[Tt ]` + `time` with a zone, the
/// calendar checked (leap years, 60th second only at 23:59 UTC).
fn is_date_time(value: &str) -> bool {
    let Some(split) = value.find(['T', 't', ' ']) else {
        return false;
    };
    let (date, time) = (&value[..split], &value[split + 1..]);
    let Some((year, month, day)) = parse_date(date) else {
        return false;
    };
    if chrono::NaiveDate::from_ymd_opt(year, month, day).is_none() {
        return false;
    }
    static TIME: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^(?i)([0-9]{2}):([0-9]{2}):([0-9]{2}(?:\.[0-9]+)?)(z|([+-])([0-9]{2})(?::?([0-9]{2}))?)$")
            .unwrap()
    });
    let Some(caps) = TIME.captures(time) else {
        return false;
    };
    let hour: u32 = caps[1].parse().unwrap_or(99);
    let minute: u32 = caps[2].parse().unwrap_or(99);
    let second: f64 = caps[3].parse().unwrap_or(99.0);
    if hour > 23 || minute > 59 || second >= 61.0 {
        return false;
    }
    if second < 60.0 {
        return true;
    }
    let (tz_hour, tz_minute) = match (caps.get(5), caps.get(6), caps.get(7)) {
        (Some(sign), Some(h), m) => {
            let h: i32 = h.as_str().parse().unwrap_or(0);
            let m: i32 = m.map(|m| m.as_str().parse().unwrap_or(0)).unwrap_or(0);
            if sign.as_str() == "+" {
                (h, m)
            } else {
                (-h, -m)
            }
        }
        _ => (0, 0),
    };
    let utc_minutes =
        (hour as i32 * 60 + minute as i32 - tz_hour * 60 - tz_minute).rem_euclid(24 * 60);
    utc_minutes == 23 * 60 + 59
}

fn parse_date(date: &str) -> Option<(i32, u32, u32)> {
    let mut parts = date.split('-');
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return None;
    }
    if ![y, m, d]
        .iter()
        .all(|p| p.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    Some((y.parse().ok()?, m.parse().ok()?, d.parse().ok()?))
}

/// A string schema's keywords, in ajv's evaluation order.
#[derive(Debug, Clone, Default)]
pub struct StringRules {
    pub enumeration: Option<Vec<String>>,
    pub max_length: Option<usize>,
    pub min_length: Option<usize>,
    pub pattern: Option<&'static str>,
    pub format: Option<Format>,
}

#[derive(Debug, Clone)]
pub enum Kind {
    String(StringRules),
    Number {
        minimum: Option<f64>,
        maximum: Option<f64>,
    },
    /// `type: "integer"`: a number without a fractional part.
    Integer {
        minimum: Option<f64>,
        maximum: Option<f64>,
    },
    Boolean,
    Array {
        items: Box<Prop>,
        max_items: Option<usize>,
        min_items: Option<usize>,
        unique_items: bool,
    },
    Object(ObjectSchema),
    /// A type-less `enum` (the events `TruthyEnum` filters).
    Enum(Vec<Value>),
    /// `oneOf` over single-value enums (the events `list`).
    OneOfEnums(Vec<Value>),
}

#[derive(Debug, Clone)]
pub struct Prop {
    pub kind: Kind,
    pub nullable: bool,
}

impl Prop {
    pub fn nullable(mut self) -> Self {
        self.nullable = true;
        self
    }
}

#[derive(Debug, Clone)]
pub struct ObjectSchema {
    pub properties: Vec<(&'static str, Prop)>,
    pub required: Vec<&'static str>,
    pub additional_properties: bool,
}

impl ObjectSchema {
    pub fn new(properties: Vec<(&'static str, Prop)>) -> Self {
        Self {
            properties,
            required: Vec::new(),
            additional_properties: true,
        }
    }
    pub fn required(mut self, keys: &[&'static str]) -> Self {
        self.required = keys.to_vec();
        self
    }
    pub fn closed(mut self) -> Self {
        self.additional_properties = false;
        self
    }
    /// gatsby `schema.params(properties)`: every key required, additional
    /// properties allowed (`Schema/schema.js:28-35`).
    pub fn params(properties: Vec<(&'static str, Prop)>) -> Self {
        let required = properties.iter().map(|(k, _)| *k).collect();
        Self {
            properties,
            required,
            additional_properties: true,
        }
    }
}

pub fn string() -> Prop {
    Prop {
        kind: Kind::String(StringRules::default()),
        nullable: false,
    }
}
pub fn string_format(format: Format) -> Prop {
    Prop {
        kind: Kind::String(StringRules {
            format: Some(format),
            ..Default::default()
        }),
        nullable: false,
    }
}
pub fn string_pattern(pattern: &'static str) -> Prop {
    Prop {
        kind: Kind::String(StringRules {
            pattern: Some(pattern),
            ..Default::default()
        }),
        nullable: false,
    }
}
pub fn string_enum(values: &[&str]) -> Prop {
    Prop {
        kind: Kind::String(StringRules {
            enumeration: Some(values.iter().map(|v| v.to_string()).collect()),
            ..Default::default()
        }),
        nullable: false,
    }
}
pub fn string_length(min: Option<usize>, max: Option<usize>) -> Prop {
    Prop {
        kind: Kind::String(StringRules {
            min_length: min,
            max_length: max,
            ..Default::default()
        }),
        nullable: false,
    }
}
pub fn boolean() -> Prop {
    Prop {
        kind: Kind::Boolean,
        nullable: false,
    }
}
pub fn number(minimum: Option<f64>) -> Prop {
    Prop {
        kind: Kind::Number {
            minimum,
            maximum: None,
        },
        nullable: false,
    }
}
pub fn number_range(minimum: Option<f64>, maximum: Option<f64>) -> Prop {
    Prop {
        kind: Kind::Number { minimum, maximum },
        nullable: false,
    }
}
pub fn integer(minimum: Option<f64>, maximum: Option<f64>) -> Prop {
    Prop {
        kind: Kind::Integer { minimum, maximum },
        nullable: false,
    }
}
/// A string with several keywords at once, in ajv's order: maxLength,
/// minLength, pattern, format.
pub fn string_rules(rules: StringRules) -> Prop {
    Prop {
        kind: Kind::String(rules),
        nullable: false,
    }
}
/// A type-less `enum` over arbitrary JSON values (`null` included).
pub fn enumeration_values(values: Vec<Value>) -> Prop {
    Prop {
        kind: Kind::Enum(values),
        nullable: false,
    }
}
pub fn array(items: Prop) -> Prop {
    Prop {
        kind: Kind::Array {
            items: Box::new(items),
            max_items: None,
            min_items: None,
            unique_items: false,
        },
        nullable: false,
    }
}
pub fn array_max(items: Prop, max: usize) -> Prop {
    let mut p = array(items);
    if let Kind::Array { max_items, .. } = &mut p.kind {
        *max_items = Some(max);
    }
    p
}
pub fn array_min(items: Prop, min: usize) -> Prop {
    let mut p = array(items);
    if let Kind::Array { min_items, .. } = &mut p.kind {
        *min_items = Some(min);
    }
    p
}
pub fn object(schema: ObjectSchema) -> Prop {
    Prop {
        kind: Kind::Object(schema),
        nullable: false,
    }
}
pub fn enumeration(values: &[&str]) -> Prop {
    Prop {
        kind: Kind::Enum(values.iter().map(|v| json!(v)).collect()),
        nullable: false,
    }
}
pub fn one_of_enums(values: &[&str]) -> Prop {
    Prop {
        kind: Kind::OneOfEnums(values.iter().map(|v| json!(v)).collect()),
        nullable: false,
    }
}

/// The compiled form of a schema pattern. The patterns are the schemas' own
/// literals, so each compiles once and lives for the process.
fn regex_for(pattern: &'static str) -> &'static Regex {
    static CACHE: LazyLock<RwLock<HashMap<&'static str, &'static Regex>>> =
        LazyLock::new(Default::default);
    if let Some(re) = CACHE.read().unwrap_or_else(|e| e.into_inner()).get(pattern) {
        return re;
    }
    let mut cache = CACHE.write().unwrap_or_else(|e| e.into_inner());
    cache.entry(pattern).or_insert_with(|| {
        // ajv compiles with the `u` flag, where `\d` is ASCII; Rust's `\d` is Unicode.
        let rust = pattern.replace("\\d", "[0-9]");
        Box::leak(Box::new(
            Regex::new(&rust).unwrap_or_else(|e| panic!("schema pattern {pattern:?}: {e}")),
        ))
    })
}

/// A schema number as JSON.stringify prints it: `0`, not `0.0`.
fn number_json(n: f64) -> Value {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        json!(n as i64)
    } else {
        json!(n)
    }
}

struct Walk {
    errors: Vec<AjvError>,
}

impl Walk {
    fn push(
        &mut self,
        instance: &str,
        schema: &str,
        keyword: &'static str,
        params: Value,
        message: String,
    ) {
        self.errors.push(AjvError {
            instance_path: instance.to_string(),
            schema_path: schema.to_string(),
            keyword,
            params,
            message,
        });
    }

    fn type_error(&mut self, instance: &str, schema: &str, expected: &str, nullable: bool) {
        let shown = if nullable {
            format!("{expected},null")
        } else {
            expected.to_string()
        };
        self.push(
            instance,
            &format!("{schema}/type"),
            "type",
            json!({ "type": shown }),
            format!("must be {shown}"),
        );
    }

    /// `false` once an error stopped the walk (allErrors off).
    fn prop(&mut self, prop: &Prop, value: &Value, instance: &str, schema: &str) -> bool {
        if value.is_null() && prop.nullable {
            return true;
        }
        match &prop.kind {
            Kind::Enum(values) => {
                if !values.contains(value) {
                    self.push(
                        instance,
                        &format!("{schema}/enum"),
                        "enum",
                        json!({ "allowedValues": values }),
                        "must be equal to one of the allowed values".to_string(),
                    );
                    return false;
                }
            }
            Kind::OneOfEnums(values) => {
                // ajv reports every failing branch, then the oneOf itself.
                if !values.contains(value) {
                    for (i, allowed) in values.iter().enumerate() {
                        self.push(
                            instance,
                            &format!("{schema}/oneOf/{i}/enum"),
                            "enum",
                            json!({ "allowedValues": [allowed] }),
                            "must be equal to one of the allowed values".to_string(),
                        );
                    }
                    self.push(
                        instance,
                        &format!("{schema}/oneOf"),
                        "oneOf",
                        json!({ "passingSchemas": null }),
                        "must match exactly one schema in oneOf".to_string(),
                    );
                    return false;
                }
            }
            Kind::String(rules) => {
                let Some(s) = value.as_str() else {
                    self.type_error(instance, schema, "string", prop.nullable);
                    return false;
                };
                if let Some(values) = &rules.enumeration
                    && !values.iter().any(|v| v == s)
                {
                    self.push(
                        instance,
                        &format!("{schema}/enum"),
                        "enum",
                        json!({ "allowedValues": values }),
                        "must be equal to one of the allowed values".to_string(),
                    );
                    return false;
                }
                let chars = s.chars().count();
                if let Some(max) = rules.max_length
                    && chars > max
                {
                    self.push(
                        instance,
                        &format!("{schema}/maxLength"),
                        "maxLength",
                        json!({ "limit": max }),
                        format!("must NOT have more than {max} characters"),
                    );
                    return false;
                }
                if let Some(min) = rules.min_length
                    && chars < min
                {
                    self.push(
                        instance,
                        &format!("{schema}/minLength"),
                        "minLength",
                        json!({ "limit": min }),
                        format!("must NOT have fewer than {min} characters"),
                    );
                    return false;
                }
                if let Some(pattern) = rules.pattern
                    && !regex_for(pattern).is_match(s)
                {
                    self.push(
                        instance,
                        &format!("{schema}/pattern"),
                        "pattern",
                        json!({ "pattern": pattern }),
                        format!("must match pattern \"{pattern}\""),
                    );
                    return false;
                }
                if let Some(format) = rules.format
                    && !format.accepts(s)
                {
                    self.push(
                        instance,
                        &format!("{schema}/format"),
                        "format",
                        json!({ "format": format.name() }),
                        format!("must match format \"{}\"", format.name()),
                    );
                    return false;
                }
            }
            Kind::Number { minimum, maximum } | Kind::Integer { minimum, maximum } => {
                let integer = matches!(prop.kind, Kind::Integer { .. });
                let n = match value.as_f64() {
                    Some(n) if !integer || n.fract() == 0.0 => n,
                    _ => {
                        self.type_error(
                            instance,
                            schema,
                            if integer { "integer" } else { "number" },
                            prop.nullable,
                        );
                        return false;
                    }
                };
                // ajv's limitNumber rule order: maximum, then minimum.
                if let Some(max) = maximum
                    && n > *max
                {
                    let limit = number_json(*max);
                    self.push(
                        instance,
                        &format!("{schema}/maximum"),
                        "maximum",
                        json!({ "comparison": "<=", "limit": limit }),
                        format!("must be <= {limit}"),
                    );
                    return false;
                }
                if let Some(min) = minimum
                    && n < *min
                {
                    let limit = number_json(*min);
                    self.push(
                        instance,
                        &format!("{schema}/minimum"),
                        "minimum",
                        json!({ "comparison": ">=", "limit": limit }),
                        format!("must be >= {limit}"),
                    );
                    return false;
                }
            }
            Kind::Boolean => {
                if !value.is_boolean() {
                    self.type_error(instance, schema, "boolean", prop.nullable);
                    return false;
                }
            }
            Kind::Array {
                items,
                max_items,
                min_items,
                unique_items,
            } => {
                let Some(list) = value.as_array() else {
                    self.type_error(instance, schema, "array", prop.nullable);
                    return false;
                };
                if let Some(max) = max_items
                    && list.len() > *max
                {
                    self.push(
                        instance,
                        &format!("{schema}/maxItems"),
                        "maxItems",
                        json!({ "limit": max }),
                        format!("must NOT have more than {max} items"),
                    );
                    return false;
                }
                if let Some(min) = min_items
                    && list.len() < *min
                {
                    self.push(
                        instance,
                        &format!("{schema}/minItems"),
                        "minItems",
                        json!({ "limit": min }),
                        format!("must NOT have fewer than {min} items"),
                    );
                    return false;
                }
                if *unique_items {
                    for i in (1..list.len()).rev() {
                        if let Some(j) = (0..i).find(|j| list[*j] == list[i]) {
                            self.push(
                                instance,
                                &format!("{schema}/uniqueItems"),
                                "uniqueItems",
                                json!({ "i": i, "j": j }),
                                format!(
                                    "must NOT have duplicate items (items ## {j} and {i} are identical)"
                                ),
                            );
                            return false;
                        }
                    }
                }
                for (i, item) in list.iter().enumerate() {
                    if !self.prop(
                        items,
                        item,
                        &format!("{instance}/{i}"),
                        &format!("{schema}/items"),
                    ) {
                        return false;
                    }
                }
            }
            Kind::Object(object) => {
                if !value.is_object() {
                    self.type_error(instance, schema, "object", prop.nullable);
                    return false;
                }
                return self.object(object, value, instance, schema);
            }
        }
        true
    }

    /// ajv's object order: `required`, then `additionalProperties`, then the
    /// declared `properties` in schema order.
    fn object(&mut self, schema: &ObjectSchema, value: &Value, instance: &str, path: &str) -> bool {
        let Some(map) = value.as_object() else {
            self.type_error(instance, path, "object", false);
            return false;
        };
        for key in &schema.required {
            if !map.contains_key(*key) {
                self.push(
                    instance,
                    &format!("{path}/required"),
                    "required",
                    json!({ "missingProperty": key }),
                    format!("must have required property '{key}'"),
                );
                return false;
            }
        }
        if !schema.additional_properties {
            for key in map.keys() {
                if !schema.properties.iter().any(|(k, _)| k == key) {
                    self.push(
                        instance,
                        &format!("{path}/additionalProperties"),
                        "additionalProperties",
                        json!({ "additionalProperty": key }),
                        "must NOT have additional properties".to_string(),
                    );
                    return false;
                }
            }
        }
        for (key, prop) in &schema.properties {
            if let Some(v) = map.get(*key)
                && !self.prop(
                    prop,
                    v,
                    &format!("{instance}/{key}"),
                    &format!("{path}/properties/{key}"),
                )
            {
                return false;
            }
        }
        true
    }
}

/// The ajv errors `data` raises against `schema`; empty when it validates.
pub fn errors(schema: &ObjectSchema, data: &Value) -> Vec<AjvError> {
    let mut walk = Walk { errors: Vec::new() };
    walk.object(schema, data, "", "#");
    walk.errors
}

/// Validates `data` (the query object or body a handler built the way the
/// upstream route did) and answers upstream's 400 in the route's `flavour`.
pub fn validate(schema: &ObjectSchema, data: &Value, flavour: Flavour) -> Result<(), ApiError> {
    let errors = errors(schema, data);
    if errors.is_empty() {
        return Ok(());
    }
    Err(validation_error(errors, data, flavour))
}

pub fn validation_error(errors: Vec<AjvError>, data: &Value, flavour: Flavour) -> ApiError {
    let messages: Vec<String> = errors.iter().map(AjvError::line).collect();
    match flavour {
        // Router.js:72-76
        Flavour::Wkc => ApiError::with_data(
            400,
            format!("{WKC_INVALID_INPUT_PREFIX}{}", messages.join("\n- ")),
            json!({ "messages": messages, "body": data, "errors": errors }),
        ),
        // validate.js:26-30
        Flavour::Gatsby => ApiError::with_data(
            400,
            GATSBY_INVALID_DATA,
            json!({ "errors": errors, "messages": messages, "body": data }),
        ),
    }
}

/// Builds the object a wkc route handed its validator from the URL's search
/// params (`ctx.url.searchParams.get(...)`: the first value or `null`;
/// `getAll`: every value).
pub struct QueryBody<'a> {
    pairs: &'a [(String, String)],
    body: Map<String, Value>,
}

impl<'a> QueryBody<'a> {
    pub fn new(pairs: &'a [(String, String)]) -> Self {
        Self {
            pairs,
            body: Map::new(),
        }
    }
    fn first(&self, key: &str) -> Option<&'a str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
    /// `searchParams.get(key)`: the value or `null`.
    pub fn get(mut self, key: &'static str) -> Self {
        let value = self.first(key).map(Value::from).unwrap_or(Value::Null);
        self.body.insert(key.to_string(), value);
        self
    }
    /// `searchParams.get(key)?.toLowerCase()`: omitted when absent.
    pub fn get_lower(mut self, key: &'static str) -> Self {
        if let Some(v) = self.first(key) {
            self.body
                .insert(key.to_string(), Value::from(v.to_lowercase()));
        }
        self
    }
    /// `searchParams.getAll(key)`.
    pub fn get_all(mut self, key: &'static str) -> Self {
        let values: Vec<Value> = self
            .pairs
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| Value::from(v.as_str()))
            .collect();
        self.body.insert(key.to_string(), Value::Array(values));
        self
    }
    /// gatsby `oneOf(searchParams.get(key), options) || default`
    /// (`Schema/utils.js:50-52`).
    pub fn one_of(mut self, key: &'static str, options: &[&str], default: &str) -> Self {
        let chosen = self
            .first(key)
            .filter(|v| options.contains(v))
            .unwrap_or(default);
        self.body.insert(key.to_string(), Value::from(chosen));
        self
    }
    /// A fixed value (`order_by: PlaceListOrderBy.MOST_ACTIVE`).
    pub fn fixed(mut self, key: &'static str, value: &str) -> Self {
        self.body.insert(key.to_string(), Value::from(value));
        self
    }
    /// `searchParams.get(key) || default`.
    pub fn get_or(mut self, key: &'static str, default: &str) -> Self {
        let value = self.first(key).filter(|v| !v.is_empty()).unwrap_or(default);
        self.body.insert(key.to_string(), Value::from(value));
        self
    }
    pub fn finish(self) -> Value {
        Value::Object(self.body)
    }
}

/// `req.query` as express's `qs` parser builds it: a repeated key or a `[]`
/// suffix makes an array, anything else a string.
pub fn express_query(pairs: &[(String, String)]) -> Map<String, Value> {
    let mut out: Map<String, Value> = Map::new();
    for (key, value) in pairs {
        let (name, bracket) = match key.strip_suffix("[]") {
            Some(name) => (name, true),
            None => (key.as_str(), false),
        };
        match out.get_mut(name) {
            None => {
                let v = if bracket {
                    Value::Array(vec![Value::from(value.as_str())])
                } else {
                    Value::from(value.as_str())
                };
                out.insert(name.to_string(), v);
            }
            Some(Value::Array(list)) => list.push(Value::from(value.as_str())),
            Some(existing) => {
                let first = existing.take();
                *existing = Value::Array(vec![first, Value::from(value.as_str())]);
            }
        }
    }
    out
}

pub const POSITION_PATTERN: &str = "^-?\\d{1,3},-?\\d{1,3}$";
pub const ADDRESS_PATTERN: &str = "^0x[a-fA-F0-9]{40}$";

fn boolean_query() -> Prop {
    string_format(Format::Boolean).nullable()
}

fn uint_query() -> Prop {
    string_format(Format::Uint).nullable()
}

fn positions_query() -> Prop {
    array_max(string_pattern(POSITION_PATTERN), 1000).nullable()
}

fn names_query() -> Prop {
    array_max(string(), 1000).nullable()
}

fn order_query() -> Prop {
    string_enum(&["asc", "desc"]).nullable()
}

/// places `Place/schemas.ts:5-11`: `getPlaceParamsSchema`.
pub static PLACE_PARAMS: LazyLock<ObjectSchema> =
    LazyLock::new(|| ObjectSchema::params(vec![("place_id", string_format(Format::Uuid))]));

/// places `Place/schemas.ts:13-123`: `getPlaceListQuerySchema`, the
/// `/api/map` query and the `POST /api/places` query.
pub static PLACE_LIST_QUERY: LazyLock<ObjectSchema> = LazyLock::new(|| {
    ObjectSchema::new(vec![
        ("limit", uint_query()),
        ("offset", uint_query()),
        ("positions", positions_query()),
        ("only_favorites", boolean_query()),
        ("only_highlighted", boolean_query()),
        (
            "order_by",
            string_enum(&["like_score", "most_active", "updated_at", "created_at"]).nullable(),
        ),
        ("order", order_query()),
        ("with_realms_detail", boolean_query()),
        ("search", string().nullable()),
        ("categories", array(string()).nullable()),
        ("owner", string_pattern(ADDRESS_PATTERN).nullable()),
        (
            "creator_address",
            string_pattern(ADDRESS_PATTERN).nullable(),
        ),
        ("sdk", string().nullable()),
        ("names", names_query()),
    ])
});

/// places `Place/schemas.ts:334-350` / `World/schemas.ts:337-353`:
/// `updateRatingBodySchema` (the route's enum reads `PR`).
pub static UPDATE_RATING_BODY: LazyLock<ObjectSchema> = LazyLock::new(|| {
    ObjectSchema::new(vec![
        ("content_rating", string_enum(&["PR", "E", "T", "A", "R"])),
        ("comment", string()),
    ])
    .required(&["content_rating"])
    .closed()
});

/// places `Place/schemas.ts:352-365` / `World/schemas.ts:315-328`.
pub static UPDATE_RANKING_BODY: LazyLock<ObjectSchema> = LazyLock::new(|| {
    ObjectSchema::new(vec![("ranking", number(None).nullable())])
        .required(&["ranking"])
        .closed()
});

/// places `Place/schemas.ts:367-378` / `World/schemas.ts:295-306`.
pub static UPDATE_HIGHLIGHT_BODY: LazyLock<ObjectSchema> = LazyLock::new(|| {
    ObjectSchema::new(vec![("highlighted", boolean())])
        .required(&["highlighted"])
        .closed()
});

/// places `Place/schemas.ts:380-392`.
pub static UPDATE_DISABLED_BODY: LazyLock<ObjectSchema> = LazyLock::new(|| {
    ObjectSchema::new(vec![("disabled", boolean())])
        .required(&["disabled"])
        .closed()
});

/// places `UserFavorite/schema.ts:10-21` / `World/schemas.ts:254-265`.
pub static UPDATE_FAVORITES_BODY: LazyLock<ObjectSchema> = LazyLock::new(|| {
    ObjectSchema::new(vec![("favorites", boolean())])
        .required(&["favorites"])
        .closed()
});

/// places `UserLikes/schema.ts:10-22` / `World/schemas.ts:274-286`.
pub static UPDATE_LIKE_BODY: LazyLock<ObjectSchema> =
    LazyLock::new(|| ObjectSchema::new(vec![("like", boolean().nullable())]).closed());

/// places `World/schemas.ts:5-81`: `getWorldListQuerySchema`.
pub static WORLD_LIST_QUERY: LazyLock<ObjectSchema> = LazyLock::new(|| {
    ObjectSchema::new(vec![
        ("limit", uint_query()),
        ("offset", uint_query()),
        ("names", names_query()),
        ("only_favorites", boolean_query()),
        ("only_highlighted", boolean_query()),
        ("only_excluded_from_ranking", boolean_query()),
        (
            "order_by",
            string_enum(&["like_score", "most_active", "created_at"]).nullable(),
        ),
        ("order", order_query()),
        ("search", string().nullable()),
        ("categories", array(string()).nullable()),
        ("owner", string_pattern(ADDRESS_PATTERN).nullable()),
    ])
});

/// places `World/schemas.ts:240-245` and the `world_id` params of
/// `:247-335`: a plain string, so it never fails.
pub static WORLD_PARAMS: LazyLock<ObjectSchema> =
    LazyLock::new(|| ObjectSchema::params(vec![("world_id", string())]));

/// places `UserFavorite/schema.ts:3-8` / `UserLikes/schema.ts:3-8`.
pub static ENTITY_PARAMS: LazyLock<ObjectSchema> =
    LazyLock::new(|| ObjectSchema::params(vec![("entity_id", string())]));

/// places `Destination/schemas.ts:6-135`: `getDestinationsListQuerySchema`.
pub static DESTINATIONS_LIST_QUERY: LazyLock<ObjectSchema> = LazyLock::new(|| {
    ObjectSchema::new(vec![
        ("limit", uint_query()),
        ("offset", uint_query()),
        ("pointer", positions_query()),
        ("world_names", names_query()),
        ("names", names_query()),
        ("only_favorites", boolean_query()),
        ("only_highlighted", boolean_query()),
        (
            "order_by",
            string_enum(&["like_score", "most_active", "updated_at", "created_at"]).nullable(),
        ),
        ("order", order_query()),
        ("with_realms_detail", boolean_query()),
        ("with_connected_users", boolean_query()),
        ("with_live_events", boolean_query()),
        ("search", string().nullable()),
        ("categories", array(string()).nullable()),
        ("owner", string_pattern(ADDRESS_PATTERN).nullable()),
        (
            "creator_address",
            string_pattern(ADDRESS_PATTERN).nullable(),
        ),
        ("only_worlds", boolean_query()),
        ("only_places", boolean_query()),
        ("sdk", string().nullable()),
    ])
});

/// places `Destination/schemas.ts:144-196`: `replaceRankingBodySchema`.
pub static REPLACE_RANKING_BODY: LazyLock<ObjectSchema> = LazyLock::new(|| {
    let mut replaces = array_max(string_enum(&["place", "world"]), 2);
    if let Kind::Array { unique_items, .. } = &mut replaces.kind {
        *unique_items = true;
    }
    let entry = ObjectSchema::new(vec![
        ("entity_type", string_enum(&["place", "world"])),
        ("id", string_length(Some(1), Some(255))),
        ("ranking", number(Some(0.0))),
    ])
    .required(&["entity_type", "id", "ranking"])
    .closed();
    ObjectSchema::new(vec![
        ("replaces", replaces.nullable()),
        ("entries", array_max(object(entry), 5000)),
    ])
    .required(&["entries"])
    .closed()
});

/// places `Map/schemas.ts:88-177`: `getAllPlacesListQuerySchema`.
pub static MAP_ALL_PLACES_QUERY: LazyLock<ObjectSchema> = LazyLock::new(|| {
    ObjectSchema::new(vec![
        ("limit", uint_query()),
        ("offset", uint_query()),
        ("positions", positions_query()),
        ("names", names_query()),
        ("only_favorites", boolean_query()),
        ("only_highlighted", boolean_query()),
        (
            "order_by",
            string_enum(&["like_score", "most_active", "updated_at", "created_at"]).nullable(),
        ),
        ("order", order_query()),
        ("with_realms_detail", boolean_query()),
        ("search", string().nullable()),
        ("categories", array(string()).nullable()),
        (
            "creator_address",
            string_pattern(ADDRESS_PATTERN).nullable(),
        ),
        ("sdk", string().nullable()),
    ])
});

/// events `Event/schemas.ts:11-21`: `getEventParamsSchema`.
pub static EVENT_PARAMS: LazyLock<ObjectSchema> =
    LazyLock::new(|| ObjectSchema::new(vec![("event_id", string_format(Format::Uuid))]).closed());

const TRUTHY_STRINGS: &[&str] = &["1", "true"];
const BOOLEAN_STRINGS: &[&str] = &["1", "true", "0", "false"];
const ADMIN_FLAG_STRINGS: &[&str] = &["true", "false", "1", "0"];

/// events `Event/schemas.ts:23-171`: `getEventListQuery`
/// (`additionalProperties: false`; the dcl-one-only `recurrent`,
/// `with_recurrent_dates` and `allow_deleted` are therefore refused here and
/// stay on `/v1/events` only).
pub static EVENT_LIST_QUERY: LazyLock<ObjectSchema> = LazyLock::new(|| {
    ObjectSchema::new(vec![
        ("limit", string_format(Format::Uint)),
        ("offset", string_format(Format::Uint)),
        ("position", string_pattern(POSITION_PATTERN)),
        (
            "positions",
            array_max(string_pattern(POSITION_PATTERN), 1000),
        ),
        ("estate_id", string_format(Format::Int)),
        ("creator", string_format(Format::Address)),
        ("only_attendee", enumeration(TRUTHY_STRINGS)),
        ("search", string()),
        ("schedule", string_format(Format::Uuid)),
        (
            "list",
            one_of_enums(&["all", "active", "live", "upcoming", "highlight"]),
        ),
        ("highlighted", enumeration(TRUTHY_STRINGS)),
        ("owner", enumeration(TRUTHY_STRINGS)),
        ("order", enumeration(&["asc", "desc"])),
        ("world", enumeration(BOOLEAN_STRINGS)),
        (
            "world_names",
            array_min(string_pattern("^.*\\.dcl\\.eth$"), 1),
        ),
        ("places_ids", array_max(string_length(Some(1), None), 100)),
        ("community_id", string_format(Format::Uuid)),
        ("from", string_format(Format::DateTime)),
        ("to", string_format(Format::DateTime)),
        ("with_connected_users", enumeration(ADMIN_FLAG_STRINGS)),
        ("approved", enumeration(ADMIN_FLAG_STRINGS)),
        ("rejected", enumeration(ADMIN_FLAG_STRINGS)),
        ("deleted", enumeration(ADMIN_FLAG_STRINGS)),
    ])
    .closed()
});

/// events `Event/schemas.ts:518-675`: `newEventSchema`, the ajv body of
/// `POST /api/events` (`createEvent.ts:36-38,81`) and of the merged document
/// a `PATCH /api/events/:id` builds (`updateEvent.ts:63-65,250-254`).
/// `additionalProperties: false`; nullable types as upstream spells them.
pub static NEW_EVENT_BODY: LazyLock<ObjectSchema> = LazyLock::new(|| {
    let text = |max: usize| {
        string_rules(StringRules {
            min_length: Some(0),
            max_length: Some(max),
            ..Default::default()
        })
    };
    let http_uri = || {
        string_rules(StringRules {
            max_length: Some(2048),
            pattern: Some("^https?://"),
            format: Some(Format::Uri),
            ..Default::default()
        })
    };
    ObjectSchema::new(vec![
        ("name", text(150)),
        ("description", text(5000).nullable()),
        ("approved", boolean()),
        ("rejected", boolean()),
        (
            "rejection_reason",
            string_length(None, Some(500)).nullable(),
        ),
        ("deleted_by_user", boolean()),
        ("deleted_by_admin", boolean()),
        ("deleted_reason", string_length(None, Some(500)).nullable()),
        ("highlighted", boolean()),
        ("trending", boolean()),
        ("image", http_uri().nullable()),
        ("image_vertical", http_uri().nullable()),
        ("start_at", string_format(Format::DateTime)),
        ("duration", number(Some(0.0))),
        ("all_day", boolean()),
        ("recurrent", boolean()),
        (
            "recurrent_frequency",
            enumeration_values(vec![
                json!("YEARLY"),
                json!("MONTHLY"),
                json!("WEEKLY"),
                json!("DAILY"),
                json!("HOURLY"),
                Value::Null,
            ]),
        ),
        ("recurrent_setpos", integer(Some(-1.0), None).nullable()),
        ("recurrent_monthday", integer(None, None).nullable()),
        ("recurrent_weekday_mask", integer(Some(0.0), None)),
        ("recurrent_month_mask", integer(Some(0.0), None)),
        ("recurrent_interval", integer(Some(1.0), Some(1000.0))),
        (
            "recurrent_count",
            integer(Some(0.0), Some(1000.0)).nullable(),
        ),
        (
            "recurrent_until",
            string_format(Format::DateTime).nullable(),
        ),
        ("x", number_range(Some(-170.0), Some(170.0))),
        ("y", number_range(Some(-170.0), Some(170.0))),
        ("server", string().nullable()),
        ("contact", text(100).nullable()),
        ("details", text(5000).nullable()),
        ("url", http_uri()),
        ("categories", array(string()).nullable()),
        (
            "schedules",
            array(string_format(Format::Uuid)).nullable(),
        ),
        ("world", boolean()),
        (
            "community_id",
            string_format(Format::Uuid).nullable(),
        ),
        (
            "featured_item",
            string_rules(StringRules {
                max_length: Some(160),
                pattern: Some(
                    "^urn:decentraland:(matic|ethereum|amoy|sepolia):collections-v2:0x[a-fA-F0-9]{40}(:\\d+)?$",
                ),
                ..Default::default()
            })
            .nullable(),
        ),
    ])
    .required(&["name", "start_at", "duration", "x", "y"])
    .closed()
});

/// The event body's whole-number fields, checked after [`NEW_EVENT_BODY`].
/// The old schema typed `x`, `y` (`Event/schemas.ts:622-631`) and `duration`
/// (`:580-583`) as `number`, but its columns are INTEGER and BIGINT
/// (migrations `1591117940223_add-position-props-to-events.ts:9-16`,
/// `1592405071787_update-duration-prop-on-events.ts:8-10`), so a fractional
/// value failed the insert with a database error. This node answers it as
/// the `type: integer` ajv error instead. Fixtures `events.create.x_decimal`,
/// `events.patch.duration_decimal`.
pub static EVENT_INTEGER_FIELDS: LazyLock<ObjectSchema> = LazyLock::new(|| {
    ObjectSchema::new(vec![
        ("duration", integer(Some(0.0), None)),
        ("x", integer(Some(-170.0), Some(170.0))),
        ("y", integer(Some(-170.0), Some(170.0))),
    ])
});

/// events `Schedule/schema.ts:86-142`: `createScheduleSchema`, the body of
/// `POST /api/schedules` (`Schedule/routes.ts:21-22,51`); additional
/// properties pass, `theme` is a type-less enum beside `null`, `image` is
/// `["string", "null"]` in the `uri` format. dcl-one adds the event poster's
/// `pattern: ^https?://` (`Event/schemas.ts:562-567`) to the schedule image:
/// the `uri` format alone admits `javascript:` and `data:` schemes, which
/// clients render as a link. Fixtures `events.schedules.post_*`.
pub static CREATE_SCHEDULE_BODY: LazyLock<ObjectSchema> =
    LazyLock::new(|| schedule_body().required(&["name", "active_since", "active_until"]));

/// `updateScheduleSchema` (`Schedule/schema.ts:144-147`): the create schema
/// with nothing required (`PATCH /api/schedules/:id`, `routes.ts:23-24,76`).
/// Fixture `events.schedules.patch_invalid_theme`.
pub static UPDATE_SCHEDULE_BODY: LazyLock<ObjectSchema> = LazyLock::new(schedule_body);

fn schedule_body() -> ObjectSchema {
    let text = |max: usize| {
        string_rules(StringRules {
            min_length: Some(0),
            max_length: Some(max),
            ..Default::default()
        })
    };
    ObjectSchema::new(vec![
        ("name", text(50)),
        ("description", text(255)),
        (
            "image",
            string_rules(StringRules {
                pattern: Some("^https?://"),
                format: Some(Format::Uri),
                ..Default::default()
            })
            .nullable(),
        ),
        (
            "theme",
            enumeration_values(vec![
                Value::Null,
                json!("mvmf_2022"),
                json!("mvfw_2023"),
                json!("pride_2023"),
            ]),
        ),
        ("background", array(text(255))),
        ("active", boolean()),
        ("active_since", string_format(Format::DateTime)),
        ("active_until", string_format(Format::DateTime)),
    ])
}

/// events `ProfileSettings/types.ts:79-101`: `updateMyProfileSettingsSchema`,
/// the body of `PATCH /api/profiles/me/settings`; `additionalProperties:
/// false`, `email` a string in the `email` format (never null), four
/// booleans. Fixtures `events.profiles.me_patch_*`.
pub static UPDATE_MY_PROFILE_SETTINGS_BODY: LazyLock<ObjectSchema> = LazyLock::new(|| {
    ObjectSchema::new(vec![
        ("email", string_format(Format::Email)),
        ("email_verified", boolean()),
        ("use_local_time", boolean()),
        ("notify_by_email", boolean()),
        ("notify_by_browser", boolean()),
    ])
    .closed()
});

/// `updateProfileSettingsSchema` (`ProfileSettings/types.ts:59-77`): the body
/// of `PATCH /api/profiles/:profile_id/settings`, `permissions` alone, each
/// item one of the five grantable values in upstream's order. Fixtures
/// `events.profiles.patch_by_id_*`.
pub static UPDATE_PROFILE_SETTINGS_BODY: LazyLock<ObjectSchema> = LazyLock::new(|| {
    ObjectSchema::new(vec![(
        "permissions",
        array(enumeration(&[
            "approve_any_event",
            "approve_own_event",
            "edit_any_event",
            "edit_any_schedule",
            "edit_any_profile",
        ])),
    )])
    .closed()
});

/// `profileSubscriptionSchema` (`ProfileSubscription/types.ts:10-25`): the
/// body of `POST /api/profiles/subscriptions`, three required strings and
/// nothing else. Fixtures `events.subscriptions.post_*`.
pub static PROFILE_SUBSCRIPTION_BODY: LazyLock<ObjectSchema> = LazyLock::new(|| {
    ObjectSchema::new(vec![
        ("endpoint", string()),
        ("p256dh", string()),
        ("auth", string()),
    ])
    .required(&["endpoint", "p256dh", "auth"])
    .closed()
});

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    async fn rendered(err: ApiError) -> (u16, Value) {
        let resp = err.into_response();
        let status = resp.status().as_u16();
        let envelope = resp
            .extensions()
            .get::<crate::shared::errors::ErrorEnvelope>()
            .cloned()
            .unwrap();
        (status, envelope.upstream_body())
    }

    fn place_list_body(list: &[(&str, &str)]) -> Value {
        let p = pairs(list);
        QueryBody::new(&p)
            .get_all("positions")
            .get("offset")
            .get("limit")
            .one_of(
                "order_by",
                &["like_score", "updated_at", "created_at"],
                "like_score",
            )
            .one_of("order", &["asc", "desc"], "desc")
            .get_lower("owner")
            .finish()
    }

    /// Capture `places.get_places.invalid_limit`: the wkc flavour, the
    /// format error object and the message line.
    #[tokio::test]
    async fn wkc_flavour_reports_a_uint_format_error_like_the_capture() {
        let body = place_list_body(&[("limit", "abc")]);
        let err = validate(&PLACE_LIST_QUERY, &body, Flavour::Wkc).unwrap_err();
        let (status, v) = rendered(err).await;
        assert_eq!(status, 400);
        assert_eq!(
            v["error"],
            "Error validating input:\n- limit must match format \"uint\" {\"format\":\"uint\"}"
        );
        assert_eq!(v["code"], "bad_request");
        assert_eq!(
            v["data"]["messages"],
            json!(["limit must match format \"uint\" {\"format\":\"uint\"}"])
        );
        assert_eq!(
            v["data"]["errors"],
            json!([{
                "instancePath": "/limit",
                "schemaPath": "#/properties/limit/format",
                "keyword": "format",
                "params": {"format": "uint"},
                "message": "must match format \"uint\""
            }])
        );
        assert_eq!(v["data"]["body"]["limit"], "abc");
        assert_eq!(v["data"]["body"]["offset"], Value::Null);
        assert_eq!(v["data"]["body"]["positions"], json!([]));
        assert_eq!(v["data"]["body"]["order_by"], "like_score");
        assert!(v.get("message").is_none() && v.get("stack").is_none());
    }

    /// Captures `places.get_places.user_visits`, `invalid_owner` and
    /// `invalid_position`: enum and pattern errors, each with its params.
    #[test]
    fn wkc_enum_and_pattern_messages_match_the_captures() {
        let p = pairs(&[("limit", "3"), ("order_by", "user_visits")]);
        let body = QueryBody::new(&p)
            .get("limit")
            .fixed("order_by", "user_visits")
            .get_or("order", "desc")
            .finish();
        let errs = errors(&PLACE_LIST_QUERY, &body);
        assert_eq!(errs.len(), 1);
        assert_eq!(
            errs[0].line(),
            "order_by must be equal to one of the allowed values {\"allowedValues\":[\"like_score\",\"most_active\",\"updated_at\",\"created_at\"]}"
        );
        assert_eq!(errs[0].schema_path, "#/properties/order_by/enum");

        let errs = errors(
            &PLACE_LIST_QUERY,
            &place_list_body(&[("owner", "notanaddress")]),
        );
        assert_eq!(
            errs[0].line(),
            "owner must match pattern \"^0x[a-fA-F0-9]{40}$\" {\"pattern\":\"^0x[a-fA-F0-9]{40}$\"}"
        );
        assert!(
            errors(
                &PLACE_LIST_QUERY,
                &place_list_body(&[("owner", "0xABCDEFabcdef0123456789ABCDEFabcdef012345")])
            )
            .is_empty()
        );

        let errs = errors(&PLACE_LIST_QUERY, &place_list_body(&[("positions", "abc")]));
        assert_eq!(
            errs[0].line(),
            "positions/0 must match pattern \"^-?\\d{1,3},-?\\d{1,3}$\" {\"pattern\":\"^-?\\\\d{1,3},-?\\\\d{1,3}$\"}"
        );
        assert_eq!(errs[0].instance_path, "/positions/0");
        assert_eq!(errs[0].schema_path, "#/properties/positions/items/pattern");
        assert!(
            errors(
                &PLACE_LIST_QUERY,
                &place_list_body(&[("positions", "-120,45"), ("positions", "0,0")])
            )
            .is_empty()
        );
        let many: Vec<(&str, &str)> = (0..1001).map(|_| ("positions", "0,0")).collect();
        let errs = errors(&PLACE_LIST_QUERY, &place_list_body(&many));
        assert_eq!(
            errs[0].line(),
            "positions must NOT have more than 1000 items {\"limit\":1000}"
        );
    }

    /// Captures `places.get_place.invalid_uuid` (wkc) and
    /// `places.put_featured.invalid_uuid` (gatsby): the same error, two
    /// envelopes.
    #[tokio::test]
    async fn the_uuid_param_error_renders_in_both_flavours() {
        let body = json!({ "place_id": "not-a-uuid" });
        let (status, v) = rendered(validate(&PLACE_PARAMS, &body, Flavour::Wkc).unwrap_err()).await;
        assert_eq!(status, 400);
        assert_eq!(
            v["error"],
            "Error validating input:\n- place_id must match format \"uuid\" {\"format\":\"uuid\"}"
        );
        let (_, v) = rendered(validate(&PLACE_PARAMS, &body, Flavour::Gatsby).unwrap_err()).await;
        assert_eq!(v["error"], "Invalid data was sent to the server");
        assert_eq!(
            v["data"],
            json!({
                "errors": [{
                    "instancePath": "/place_id",
                    "schemaPath": "#/properties/place_id/format",
                    "keyword": "format",
                    "params": {"format": "uuid"},
                    "message": "must match format \"uuid\""
                }],
                "messages": ["place_id must match format \"uuid\" {\"format\":\"uuid\"}"],
                "body": {"place_id": "not-a-uuid"}
            })
        );
        for ok in [
            "3ca25728-5b41-48f6-8e24-5cb0e3f2bb5d",
            "3CA25728-5B41-48F6-8E24-5CB0E3F2BB5D",
            "urn:uuid:3ca25728-5b41-48f6-8e24-5cb0e3f2bb5d",
        ] {
            assert!(validate(&PLACE_PARAMS, &json!({"place_id": ok}), Flavour::Wkc).is_ok());
        }
        assert!(
            validate(&PLACE_PARAMS, &json!({}), Flavour::Wkc).is_err(),
            "params are required"
        );
    }

    /// Captures `events.get_events.list_invalid` (every oneOf branch, then
    /// the oneOf), `unknown_param` and `positions_not_array`; the
    /// `only_attendee` enum error takes the same shape.
    #[test]
    fn gatsby_list_errors_follow_ajv_order_and_shape() {
        let q = |list: &[(&str, &str)]| {
            let mut body = express_query(&pairs(list));
            body.insert("places_ids".into(), json!([]));
            Value::Object(body)
        };
        let errs = errors(&EVENT_LIST_QUERY, &q(&[("list", "nope"), ("limit", "1")]));
        assert_eq!(errs.len(), 6);
        assert_eq!(
            errs[0],
            AjvError {
                instance_path: "/list".into(),
                schema_path: "#/properties/list/oneOf/0/enum".into(),
                keyword: "enum",
                params: json!({"allowedValues": ["all"]}),
                message: "must be equal to one of the allowed values".into(),
            }
        );
        assert_eq!(errs[4].schema_path, "#/properties/list/oneOf/4/enum");
        assert_eq!(errs[4].params, json!({"allowedValues": ["highlight"]}));
        assert_eq!(
            errs[5].line(),
            "list must match exactly one schema in oneOf {\"passingSchemas\":null}"
        );

        let errs = errors(&EVENT_LIST_QUERY, &q(&[("foo", "bar"), ("limit", "1")]));
        assert_eq!(
            errs,
            vec![AjvError {
                instance_path: "".into(),
                schema_path: "#/additionalProperties".into(),
                keyword: "additionalProperties",
                params: json!({"additionalProperty": "foo"}),
                message: "must NOT have additional properties".into(),
            }]
        );
        assert_eq!(
            errs[0].line(),
            "must NOT have additional properties {\"additionalProperty\":\"foo\"}"
        );

        let errs = errors(
            &EVENT_LIST_QUERY,
            &q(&[("positions", "0,0"), ("limit", "2")]),
        );
        assert_eq!(
            errs[0].line(),
            "positions must be array {\"type\":\"array\"}"
        );
        assert_eq!(errs[0].schema_path, "#/properties/positions/type");
        assert!(errors(&EVENT_LIST_QUERY, &q(&[("positions[]", "0,0")])).is_empty());
        assert!(
            errors(
                &EVENT_LIST_QUERY,
                &q(&[("positions", "0,0"), ("positions", "1,1")])
            )
            .is_empty()
        );

        let errs = errors(&EVENT_LIST_QUERY, &q(&[("only_attendee", "")]));
        assert_eq!(
            errs[0].line(),
            "only_attendee must be equal to one of the allowed values {\"allowedValues\":[\"1\",\"true\"]}"
        );
        for (key, value, line) in [
            (
                "creator",
                "nope",
                "creator must match format \"address\" {\"format\":\"address\"}",
            ),
            (
                "estate_id",
                "abc",
                "estate_id must match format \"int\" {\"format\":\"int\"}",
            ),
            (
                "from",
                "yesterday",
                "from must match format \"date-time\" {\"format\":\"date-time\"}",
            ),
            (
                "schedule",
                "not-a-uuid",
                "schedule must match format \"uuid\" {\"format\":\"uuid\"}",
            ),
            (
                "limit",
                "abc",
                "limit must match format \"uint\" {\"format\":\"uint\"}",
            ),
            (
                "world_names",
                "foo",
                "world_names must be array {\"type\":\"array\"}",
            ),
            (
                "world_names[]",
                "foo",
                "world_names/0 must match pattern \"^.*\\.dcl\\.eth$\" {\"pattern\":\"^.*\\\\.dcl\\\\.eth$\"}",
            ),
            (
                "order",
                "sideways",
                "order must be equal to one of the allowed values {\"allowedValues\":[\"asc\",\"desc\"]}",
            ),
        ] {
            let errs = errors(&EVENT_LIST_QUERY, &q(&[(key, value)]));
            assert_eq!(errs.len(), 1, "{key}={value}");
            assert_eq!(errs[0].line(), line);
        }
        assert!(
            errors(
                &EVENT_LIST_QUERY,
                &q(&[
                    ("from", "2031-01-01T00:00:00.000Z"),
                    ("to", "2031-01-02 10:00:00+02:00")
                ])
            )
            .is_empty()
        );
        assert!(
            errors(
                &EVENT_LIST_QUERY,
                &q(&[
                    ("world_names[]", "x.dcl.eth"),
                    ("creator", "0xF39Fd6e51aad88F6F4ce6aB8827279cffFb92266"),
                    ("estate_id", "6112"),
                    ("list", "all"),
                    ("only_attendee", "true"),
                    ("limit", "500")
                ])
            )
            .is_empty()
        );
        let errs = errors(&EVENT_LIST_QUERY, &q(&[("recurrent", "true")]));
        assert_eq!(
            errs[0].keyword, "additionalProperties",
            "dcl-one extras are refused on /api/events"
        );
        let errs = errors(&EVENT_PARAMS, &json!({"event_id": "not-a-uuid"}));
        assert_eq!(
            errs[0].line(),
            "event_id must match format \"uuid\" {\"format\":\"uuid\"}"
        );
    }

    #[test]
    fn body_schemas_report_required_type_and_limits() {
        let errs = errors(&UPDATE_RATING_BODY, &json!({}));
        assert_eq!(
            errs[0].line(),
            "must have required property 'content_rating' {\"missingProperty\":\"content_rating\"}"
        );
        assert_eq!(errs[0].schema_path, "#/required");
        let errs = errors(&UPDATE_RATING_BODY, &json!({"content_rating": "RP"}));
        assert_eq!(
            errs[0].params,
            json!({"allowedValues": ["PR", "E", "T", "A", "R"]})
        );
        assert_eq!(errs[0].schema_path, "#/properties/content_rating/enum");
        assert!(
            errors(
                &UPDATE_RATING_BODY,
                &json!({"content_rating": "T", "comment": "ok"})
            )
            .is_empty()
        );
        let errs = errors(
            &UPDATE_RATING_BODY,
            &json!({"content_rating": "T", "extra": 1}),
        );
        assert_eq!(errs[0].keyword, "additionalProperties");

        let errs = errors(&UPDATE_FAVORITES_BODY, &json!({"favorites": "yes"}));
        assert_eq!(
            errs,
            vec![AjvError {
                instance_path: "/favorites".into(),
                schema_path: "#/properties/favorites/type".into(),
                keyword: "type",
                params: json!({"type": "boolean"}),
                message: "must be boolean".into(),
            }]
        );
        assert!(errors(&UPDATE_LIKE_BODY, &json!({"like": null})).is_empty());
        assert!(errors(&UPDATE_LIKE_BODY, &json!({})).is_empty());
        assert_eq!(
            errors(&UPDATE_LIKE_BODY, &json!({"like": "yes"}))[0].line(),
            "like must be boolean,null {\"type\":\"boolean,null\"}"
        );
        assert!(errors(&UPDATE_RANKING_BODY, &json!({"ranking": null})).is_empty());
        assert!(errors(&UPDATE_RANKING_BODY, &json!({"ranking": 2.5})).is_empty());
        assert_eq!(
            errors(&UPDATE_RANKING_BODY, &json!({"ranking": "1"}))[0].keyword,
            "type"
        );

        let errs = errors(
            &REPLACE_RANKING_BODY,
            &json!({"replaces": ["place", "world", "place"], "entries": []}),
        );
        assert_eq!(
            errs[0].line(),
            "replaces must NOT have more than 2 items {\"limit\":2}"
        );
        assert_eq!(errs[0].schema_path, "#/properties/replaces/maxItems");
        let errs = errors(
            &REPLACE_RANKING_BODY,
            &json!({"replaces": ["place", "place"], "entries": []}),
        );
        assert_eq!(
            errs[0].line(),
            "replaces must NOT have duplicate items (items ## 0 and 1 are identical) {\"i\":1,\"j\":0}"
        );
        let errs = errors(
            &REPLACE_RANKING_BODY,
            &json!({"entries": [{"entity_type": "place", "id": "x", "ranking": -1}]}),
        );
        assert_eq!(
            errs[0].line(),
            "entries/0/ranking must be >= 0 {\"comparison\":\">=\",\"limit\":0}"
        );
        assert_eq!(
            errs[0].schema_path,
            "#/properties/entries/items/properties/ranking/minimum"
        );
        let errs = errors(
            &REPLACE_RANKING_BODY,
            &json!({"entries": [{"entity_type": "place", "ranking": 1}]}),
        );
        assert_eq!(
            errs[0].line(),
            "entries/0 must have required property 'id' {\"missingProperty\":\"id\"}"
        );
        assert!(errors(&REPLACE_RANKING_BODY, &json!({"entries": []})).is_empty());
        let errs = errors(&REPLACE_RANKING_BODY, &json!([]));
        assert_eq!(errs[0].line(), "must be object {\"type\":\"object\"}");
    }

    #[test]
    fn formats_follow_validator_js_and_ajv_formats() {
        for ok in ["0", "7", "007", "+5", "-0"] {
            assert!(Format::Uint.accepts(ok), "{ok}");
        }
        for bad in ["-1", "1.5", "abc", "", " 1", "1e3"] {
            assert!(!Format::Uint.accepts(bad), "{bad}");
        }
        assert!(Format::Int.accepts("-12") && !Format::Int.accepts("12a"));
        assert!(Format::Address.accepts("0xF39Fd6e51aad88F6F4ce6aB8827279cffFb92266"));
        assert!(!Format::Address.accepts("F39Fd6e51aad88F6F4ce6aB8827279cffFb92266"));
        assert!(!Format::Address.accepts("0xF39Fd6e51aad88F6F4ce6aB8827279cffFb9226"));
        for ok in ["0", "1", "true", "false", "True", "False"] {
            assert!(Format::Boolean.accepts(ok));
        }
        assert!(!Format::Boolean.accepts("yes") && !Format::Boolean.accepts("TRUE"));
        for ok in [
            "2031-01-01T00:00:00Z",
            "2031-01-01t00:00:00z",
            "2031-01-01T00:00:00.123+05:30",
            "2031-01-01 00:00:00-0300",
            "2024-02-29T12:00:00Z",
            "2031-01-01T23:59:60Z",
            "2031-01-02T01:59:60+02:00",
        ] {
            assert!(Format::DateTime.accepts(ok), "{ok}");
        }
        for bad in [
            "2031-01-01",
            "2031-01-01T00:00:00",
            "2023-02-29T00:00:00Z",
            "2031-13-01T00:00:00Z",
            "2031-01-01T24:00:00Z",
            "2031-01-01T00:00:60Z",
            "yesterday",
        ] {
            assert!(!Format::DateTime.accepts(bad), "{bad}");
        }
    }

    #[test]
    fn express_query_builds_arrays_from_repeats_and_brackets() {
        let q = express_query(&pairs(&[
            ("a", "1"),
            ("b[]", "x"),
            ("c", "1"),
            ("c", "2"),
            ("b[]", "y"),
        ]));
        assert_eq!(
            Value::Object(q),
            json!({"a": "1", "b": ["x", "y"], "c": ["1", "2"]})
        );
    }

    /// The old schema's `number` fields that land in integer columns: a
    /// fraction is the `integer` type error, a whole float passes as ajv
    /// read it. Fixtures `events.create.x_decimal`, `events.patch.duration_decimal`.
    #[test]
    fn event_integer_fields_refuse_fractions_only() {
        let body = json!({"name": "e", "start_at": "2099-06-01T18:00:00Z", "x": 10.5, "y": 0, "duration": 3600000});
        assert!(
            errors(&NEW_EVENT_BODY, &body).is_empty(),
            "number admits 10.5"
        );
        let fraction = errors(&EVENT_INTEGER_FIELDS, &body);
        assert_eq!(fraction.len(), 1);
        assert_eq!(fraction[0].schema_path, "#/properties/x/type");
        assert_eq!(
            fraction[0].line(),
            "x must be integer {\"type\":\"integer\"}"
        );
        let duration = errors(
            &EVENT_INTEGER_FIELDS,
            &json!({"x": 10, "y": -20, "duration": 90000000.5}),
        );
        assert_eq!(duration[0].instance_path, "/duration");
        assert_eq!(duration[0].params, json!({"type": "integer"}));
        assert!(
            errors(
                &EVENT_INTEGER_FIELDS,
                &json!({"x": 11.0, "y": -20, "duration": 7200000.0})
            )
            .is_empty()
        );
        assert!(errors(&EVENT_INTEGER_FIELDS, &json!({"name": "no numbers"})).is_empty());
    }

    /// Fixtures `events.schedules.post_missing_name`, `post_name_too_long`,
    /// `post_image_not_uri`, `post_image_not_http`, `post_theme_invalid`,
    /// `post_active_since_not_datetime`, `patch_invalid_theme`.
    #[test]
    fn schedule_bodies_answer_the_upstream_ajv_errors() {
        let full = json!({
            "name": "Season",
            "active_since": "2031-01-01T00:00:00.000Z",
            "active_until": "2031-02-01T00:00:00.000Z"
        });
        assert!(errors(&CREATE_SCHEDULE_BODY, &full).is_empty());
        assert!(errors(&UPDATE_SCHEDULE_BODY, &json!({})).is_empty());
        let missing = errors(
            &CREATE_SCHEDULE_BODY,
            &json!({"active_since": "2031-01-01T00:00:00.000Z", "active_until": "2031-02-01T00:00:00.000Z"}),
        );
        assert_eq!(missing[0].schema_path, "#/required");
        assert_eq!(
            missing[0].line(),
            "must have required property 'name' {\"missingProperty\":\"name\"}"
        );
        let mut body = full.clone();
        body["name"] = json!("x".repeat(51));
        let long = errors(&CREATE_SCHEDULE_BODY, &body);
        assert_eq!(long[0].schema_path, "#/properties/name/maxLength");
        assert_eq!(
            long[0].line(),
            "name must NOT have more than 50 characters {\"limit\":50}"
        );
        let mut body = full.clone();
        body["image"] = json!("https://not a uri");
        let image = errors(&CREATE_SCHEDULE_BODY, &body);
        assert_eq!(image[0].schema_path, "#/properties/image/format");
        assert_eq!(image[0].params, json!({"format": "uri"}));
        // dcl-one: the poster pattern refuses non-http(s) schemes that the
        // `uri` format admits (fixture `events.schedules.post_image_not_http`).
        for unsafe_image in [
            "javascript:alert(1)",
            "data:text/html,<b>x</b>",
            "ftp://host/a.png",
        ] {
            body["image"] = json!(unsafe_image);
            let scheme = errors(&UPDATE_SCHEDULE_BODY, &body);
            assert_eq!(
                scheme[0].line(),
                "image must match pattern \"^https?://\" {\"pattern\":\"^https?://\"}",
                "{unsafe_image}"
            );
        }
        body["image"] = json!("https://cdn.example.org/season.png");
        assert!(errors(&CREATE_SCHEDULE_BODY, &body).is_empty());
        body["image"] = Value::Null;
        assert!(errors(&CREATE_SCHEDULE_BODY, &body).is_empty());
        let theme = errors(&UPDATE_SCHEDULE_BODY, &json!({"theme": "nope"}));
        assert_eq!(theme[0].instance_path, "/theme");
        assert_eq!(theme[0].schema_path, "#/properties/theme/enum");
        assert_eq!(
            theme[0].params,
            json!({"allowedValues": [null, "mvmf_2022", "mvfw_2023", "pride_2023"]})
        );
        assert!(errors(&UPDATE_SCHEDULE_BODY, &json!({"theme": null})).is_empty());
        let mut body = full.clone();
        body["active_since"] = json!("soon");
        let date = errors(&CREATE_SCHEDULE_BODY, &body);
        assert_eq!(date[0].schema_path, "#/properties/active_since/format");
        assert_eq!(
            date[0].line(),
            "active_since must match format \"date-time\" {\"format\":\"date-time\"}"
        );
        // additionalProperties is open on the schedule schemas.
        assert!(errors(&UPDATE_SCHEDULE_BODY, &json!({"foo": 1})).is_empty());
    }

    /// Fixtures `events.profiles.me_patch_email_invalid`, `me_patch_email_null_rejected`,
    /// `me_patch_unknown_field`, `me_patch_permissions_rejected`,
    /// `patch_by_id_unknown_field`, `patch_by_id_invalid_permission`.
    #[test]
    fn profile_settings_bodies_answer_the_upstream_ajv_errors() {
        let invalid = errors(
            &UPDATE_MY_PROFILE_SETTINGS_BODY,
            &json!({"email": "not-an-email"}),
        );
        assert_eq!(invalid[0].schema_path, "#/properties/email/format");
        assert_eq!(
            invalid[0].line(),
            "email must match format \"email\" {\"format\":\"email\"}"
        );
        let null = errors(&UPDATE_MY_PROFILE_SETTINGS_BODY, &json!({"email": null}));
        assert_eq!(null[0].schema_path, "#/properties/email/type");
        assert_eq!(null[0].message, "must be string");
        assert!(
            errors(
                &UPDATE_MY_PROFILE_SETTINGS_BODY,
                &json!({"email": "A.b+c@sub.example.org", "notify_by_email": true})
            )
            .is_empty()
        );
        for (body, key) in [
            (json!({"foo": 1}), "foo"),
            (json!({"permissions": ["edit_any_event"]}), "permissions"),
        ] {
            let errs = errors(&UPDATE_MY_PROFILE_SETTINGS_BODY, &body);
            assert_eq!(errs[0].schema_path, "#/additionalProperties");
            assert_eq!(errs[0].params, json!({"additionalProperty": key}));
        }
        let email = errors(
            &UPDATE_PROFILE_SETTINGS_BODY,
            &json!({"email": "x@example.com"}),
        );
        assert_eq!(
            email[0].line(),
            "must NOT have additional properties {\"additionalProperty\":\"email\"}"
        );
        let grant = errors(
            &UPDATE_PROFILE_SETTINGS_BODY,
            &json!({"permissions": ["test_any_notification"]}),
        );
        assert_eq!(grant[0].instance_path, "/permissions/0");
        assert_eq!(grant[0].schema_path, "#/properties/permissions/items/enum");
        assert_eq!(
            grant[0].params,
            json!({"allowedValues": ["approve_any_event", "approve_own_event", "edit_any_event", "edit_any_schedule", "edit_any_profile"]})
        );
        assert!(errors(&UPDATE_PROFILE_SETTINGS_BODY, &json!({"permissions": []})).is_empty());
    }

    /// Fixtures `events.subscriptions.post_missing_field`, `post_unknown_field`.
    #[test]
    fn subscription_bodies_answer_the_upstream_ajv_errors() {
        let missing = errors(
            &PROFILE_SUBSCRIPTION_BODY,
            &json!({"endpoint": "https://push.example/abc"}),
        );
        assert_eq!(
            missing[0].line(),
            "must have required property 'p256dh' {\"missingProperty\":\"p256dh\"}"
        );
        let extra = errors(
            &PROFILE_SUBSCRIPTION_BODY,
            &json!({"endpoint": "e", "p256dh": "k", "auth": "a", "foo": 1}),
        );
        assert_eq!(extra[0].schema_path, "#/additionalProperties");
        assert_eq!(extra[0].params, json!({"additionalProperty": "foo"}));
        let typed = errors(
            &PROFILE_SUBSCRIPTION_BODY,
            &json!({"endpoint": "e", "p256dh": 1, "auth": "a"}),
        );
        assert_eq!(
            typed[0].line(),
            "p256dh must be string {\"type\":\"string\"}"
        );
        assert!(
            errors(
                &PROFILE_SUBSCRIPTION_BODY,
                &json!({"endpoint": "e", "p256dh": "k", "auth": "a"})
            )
            .is_empty()
        );
    }
}
