//! Per-type notification emails: notifications-workers' processor templates
//! (`email-templates/`, Apache-2.0, pinned in its `upstream.lock.json`) with
//! its helpers, rendered into the fields of the one generic SendGrid template.

use std::sync::LazyLock;

use chrono::DateTime;
use handlebars::{Handlebars, handlebars_helper, no_escape};
use serde_json::{Map, Value as Json, json};

macro_rules! templates {
    ($first:literal $(, $rest:literal)* $(,)?) => {
        /// The types with a template: the old `EmailableNotificationTypeEnum`.
        pub const EMAILABLE_TYPES: &[&str] = &[$first $(, $rest)*];
        /// [`EMAILABLE_TYPES`] as a SQL list literal.
        pub const EMAILABLE_SQL_LIST: &str = concat!("'", $first, "'" $(, ",'", $rest, "'")*);
        const SOURCES: &[(&str, &str, &str)] = &[
            (
                $first,
                include_str!(concat!("email-templates/", $first, ".subject.handlebars")),
                include_str!(concat!("email-templates/", $first, ".content.handlebars")),
            )
            $(, (
                $rest,
                include_str!(concat!("email-templates/", $rest, ".subject.handlebars")),
                include_str!(concat!("email-templates/", $rest, ".content.handlebars")),
            ))*
        ];
    };
}

templates!(
    "ban_lifted",
    "ban_warning",
    "banned",
    "bid_accepted",
    "bid_received",
    "credits_on_demand_granted",
    "credits_reminder_claim_credits",
    "credits_reminder_complete_goals",
    "credits_reminder_do_not_miss_out",
    "credits_reminder_usage",
    "credits_reminder_usage_24_hours",
    "event_approved",
    "event_deleted",
    "event_rejected",
    "events_started",
    "events_starts_soon",
    "governance_announcement",
    "governance_authored_proposal_finished",
    "governance_coauthor_requested",
    "governance_new_comment_on_project_update",
    "governance_new_comment_on_proposal",
    "governance_pitch_passed",
    "governance_proposal_enacted",
    "governance_tender_passed",
    "governance_voting_ended_voter",
    "item_sold",
    "referral_invited_users_accepted",
    "referral_new_tier_reached",
    "rental_ended",
    "rental_started",
    "reward_assignment",
    "reward_campaign_out_of_funds",
    "reward_campaign_out_of_stock",
    "royalties_earned",
    "streaming_key_expired",
    "streaming_key_reset",
    "streaming_key_revoke",
    "streaming_place_updated",
    "streaming_time_exceeded",
    "tip_received",
    "worlds_access_restored",
    "worlds_access_restricted",
    "worlds_missing_resources",
    "worlds_permission_granted",
    "worlds_permission_revoked",
);

pub fn is_emailable(kind: &str) -> bool {
    EMAILABLE_TYPES.contains(&kind)
}

/// JavaScript truthiness of a JSON value.
fn truthy(v: &Json) -> bool {
    match v {
        Json::Null => false,
        Json::Bool(b) => *b,
        Json::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Json::String(s) => !s.is_empty(),
        Json::Array(_) | Json::Object(_) => true,
    }
}

/// `(Number(mana || '0') / 1e18).toFixed(2)`.
pub fn format_mana(mana: &Json) -> String {
    let n = match mana {
        Json::String(s) if !s.is_empty() => match s.trim() {
            "" => 0.0,
            t => t.parse::<f64>().unwrap_or(f64::NAN),
        },
        Json::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Json::Bool(true) => 1.0,
        Json::Null | Json::Bool(false) | Json::String(_) => 0.0,
        Json::Array(_) | Json::Object(_) => f64::NAN,
    };
    to_fixed_2(n / 1e18)
}

/// `Number.prototype.toFixed(2)`: exact ties round away from zero, not to even.
fn to_fixed_2(x: f64) -> String {
    if x.is_nan() {
        return "NaN".into();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if x == 0.0 {
        return "0.00".into();
    }
    // A two-decimal tie is an odd multiple of 1/8.
    let eighths = x * 8.0;
    if eighths.fract() == 0.0 && eighths % 2.0 != 0.0 {
        let r = (x.abs() * 100.0 + 0.5).floor() / 100.0;
        return format!("{}{r:.2}", if x < 0.0 { "-" } else { "" });
    }
    format!("{x:.2}")
}

/// `JSON.stringify(v || '""')` without its first and last characters.
pub fn escape(v: &Json) -> String {
    let s = if truthy(v) {
        v.to_string()
    } else {
        Json::String("\"\"".into()).to_string()
    };
    // Every JSON text starts and ends with an ASCII byte.
    if s.len() >= 2 {
        s[1..s.len() - 1].to_string()
    } else {
        s
    }
}

pub fn array_to_string(v: &Json) -> String {
    match v {
        Json::Array(items) => items
            .iter()
            .map(|i| match i {
                Json::String(s) => s.clone(),
                Json::Null => String::new(),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join("/"),
        _ => String::new(),
    }
}

fn epoch_ms(v: &Json) -> Option<f64> {
    match v {
        Json::Number(n) => n.as_f64(),
        Json::String(s) => DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|t| t.timestamp_millis() as f64),
        _ => None,
    }
}

/// Whole hours under a day, else whole days, as `N HOURS` / `1 DAY`.
pub fn duration(from: &Json, to: &Json) -> String {
    let ms = match (epoch_ms(from), epoch_ms(to)) {
        (Some(f), Some(t)) => t - f,
        _ => return "NaN DAYS".into(),
    };
    let hours = (ms / 3_600_000.0 + 0.5).floor();
    if hours < 24.0 {
        return if hours == 1.0 {
            "1 HOUR".into()
        } else {
            format!("{hours} HOURS")
        };
    }
    let days = (ms / 86_400_000.0 + 0.5).floor();
    if days == 1.0 {
        "1 DAY".into()
    } else {
        format!("{days} DAYS")
    }
}

handlebars_helper!(format_mana_helper: |mana: Json| format_mana(mana));
handlebars_helper!(escape_helper: |v: Json| escape(v));
handlebars_helper!(land_helper: |v: Json| v.clone());
handlebars_helper!(insert_helper: |text: Json, default: Json| if truthy(text) { text.clone() } else { default.clone() });
handlebars_helper!(array_to_string_helper: |v: Json| array_to_string(v));
handlebars_helper!(duration_helper: |from: Json, to: Json| duration(from, to));

fn registry(escaped: bool) -> Handlebars<'static> {
    let mut hb = Handlebars::new();
    if !escaped {
        hb.register_escape_fn(no_escape);
    }
    hb.register_helper("formatMana", Box::new(format_mana_helper));
    hb.register_helper("escape", Box::new(escape_helper));
    hb.register_helper("land", Box::new(land_helper));
    hb.register_helper("insert", Box::new(insert_helper));
    hb.register_helper("arrayToString", Box::new(array_to_string_helper));
    hb.register_helper("duration", Box::new(duration_helper));
    hb
}

struct Registries {
    subjects: Handlebars<'static>,
    contents: Handlebars<'static>,
}

static REGISTRIES: LazyLock<Registries> = LazyLock::new(|| {
    let mut subjects = registry(false);
    let mut contents = registry(true);
    for (kind, subject, content) in SOURCES {
        subjects
            .register_template_string(kind, subject)
            .expect("embedded subject template compiles");
        contents
            .register_template_string(kind, content)
            .expect("embedded content template compiles");
    }
    Registries { subjects, contents }
});

/// A rendered notification: the subject template's JSON fields
/// (`subject`, `actionButton*`, `title*`, `banner*`) and the HTML content.
#[derive(Debug, Clone, PartialEq)]
pub struct Rendered {
    pub fields: Map<String, Json>,
    pub content: String,
}

/// `None` when `kind` has no template; `Err` when a template fails or the
/// subject is not a JSON object.
pub fn render(kind: &str, address: &str, metadata: &Json) -> Result<Option<Rendered>, String> {
    if !is_emailable(kind) {
        return Ok(None);
    }
    let ctx = json!({ "type": kind, "address": address, "metadata": metadata });
    let r = &*REGISTRIES;
    let subject = r
        .subjects
        .render(kind, &ctx)
        .map_err(|e| format!("{kind} subject: {e}"))?;
    let content = r
        .contents
        .render(kind, &ctx)
        .map_err(|e| format!("{kind} content: {e}"))?;
    let fields = serde_json::from_str::<Map<String, Json>>(&subject)
        .map_err(|e| format!("{kind} subject is not a JSON object: {e}"))?;
    Ok(Some(Rendered { fields, content }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURES: &str = include_str!(concat!(
        env!("DCL_ONE_ROOT"),
        "/services/catalyst/tests/fixtures/notifications/email-metadata.json"
    ));

    #[test]
    fn format_mana_matches_to_fixed() {
        assert_eq!(format_mana(&json!("1500000000000000000")), "1.50");
        assert_eq!(format_mana(&json!("125000000000000000")), "0.13");
        assert_eq!(format_mana(&json!("375000000000000000")), "0.38");
        assert_eq!(format_mana(&json!("1234")), "0.00");
        assert_eq!(format_mana(&json!(2e18)), "2.00");
        assert_eq!(format_mana(&Json::Null), "0.00");
        assert_eq!(format_mana(&json!("")), "0.00");
        assert_eq!(format_mana(&json!("abc")), "NaN");
    }

    #[test]
    fn escape_is_json_string_without_quotes() {
        assert_eq!(escape(&json!("a\"b\\c\nd")), "a\\\"b\\\\c\\nd");
        assert_eq!(escape(&json!("Café 💰")), "Café 💰");
        assert_eq!(escape(&Json::Null), "\\\"\\\"");
        assert_eq!(escape(&json!("")), "\\\"\\\"");
        assert_eq!(escape(&json!(12)), "");
        assert_eq!(escape(&json!(5)), "5");
    }

    #[test]
    fn land_is_identity_and_insert_falls_back() {
        let hb = registry(true);
        let ctx = json!({"a": "x", "b": "", "d": "dflt"});
        let render = |t: &str| hb.render_template(t, &ctx).unwrap();
        assert_eq!(render("{{land a}}"), "x");
        assert_eq!(render("{{insert a d}}"), "x");
        assert_eq!(render("{{insert b d}}"), "dflt");
        assert_eq!(render("{{insert missing d}}"), "dflt");
        assert_eq!(render("{{insert missing missing}}"), "");
    }

    #[test]
    fn array_to_string_joins_with_slash() {
        assert_eq!(
            array_to_string(&json!(["access", "streaming"])),
            "access/streaming"
        );
        assert_eq!(array_to_string(&json!([])), "");
        assert_eq!(array_to_string(&Json::Null), "");
    }

    #[test]
    fn duration_rounds_to_hours_then_days() {
        let d = |a: &str, b: &str| duration(&json!(a), &json!(b));
        assert_eq!(
            d("2024-06-01T12:00:00.000Z", "2024-06-08T12:00:00.000Z"),
            "7 DAYS"
        );
        assert_eq!(d("2024-06-01T12:00:00Z", "2024-06-02T12:00:00Z"), "1 DAY");
        assert_eq!(d("2024-06-01T12:00:00Z", "2024-06-01T13:00:00Z"), "1 HOUR");
        assert_eq!(d("2024-06-01T12:00:00Z", "2024-06-01T17:29:00Z"), "5 HOURS");
        assert_eq!(d("2024-06-01T12:00:00Z", "2024-06-02T23:00:00Z"), "1 DAY");
        assert_eq!(d("2024-06-01T12:00:00Z", "2024-06-03T00:00:00Z"), "2 DAYS");
        assert_eq!(duration(&json!(0), &json!(7_200_000)), "2 HOURS");
        assert_eq!(duration(&Json::Null, &json!("x")), "NaN DAYS");
    }

    #[test]
    fn helpers_html_escape_in_content_but_not_in_subjects() {
        let ctx = json!({"metadata": {"userName": "<b>&"}});
        assert_eq!(
            registry(true)
                .render_template("{{escape metadata.userName}}", &ctx)
                .unwrap(),
            "&lt;b&gt;&amp;"
        );
        assert_eq!(
            registry(false)
                .render_template("{{escape metadata.userName}}", &ctx)
                .unwrap(),
            "<b>&"
        );
    }

    #[test]
    fn the_embedded_list_is_the_template_directory() {
        let dir = concat!(
            env!("DCL_ONE_ROOT"),
            "/services/catalyst/social/src/notifications/email-templates"
        );
        let mut on_disk: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| {
                let name = e.unwrap().file_name().into_string().unwrap();
                name.strip_suffix(".subject.handlebars").map(str::to_string)
            })
            .collect();
        on_disk.sort();
        assert_eq!(on_disk, EMAILABLE_TYPES);
        assert!(EMAILABLE_TYPES.iter().all(|t| {
            t.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        }));
        assert!(!is_emailable("land_transfer_received") && !is_emailable("friend_first_wear"));
        assert!(!is_emailable("event_created") && !is_emailable("badge_granted"));
    }

    #[test]
    fn every_emailable_type_renders_its_fixture() {
        let fixtures: Map<String, Json> = serde_json::from_str(FIXTURES).unwrap();
        for kind in EMAILABLE_TYPES {
            let f = fixtures
                .get(*kind)
                .unwrap_or_else(|| panic!("no fixture for {kind}"));
            let address = f["address"].as_str().unwrap_or_default();
            let r = render(kind, address, &f["metadata"])
                .unwrap_or_else(|e| panic!("{e}"))
                .unwrap();
            assert!(
                r.fields
                    .get("subject")
                    .and_then(Json::as_str)
                    .is_some_and(|s| !s.is_empty()),
                "{kind}"
            );
            assert!(!r.content.trim().is_empty(), "{kind}");
        }
        assert_eq!(render("event_created", "0xa", &json!({})), Ok(None));
    }

    #[test]
    fn tip_email_formats_the_wei_amount() {
        let r = render(
            "tip_received",
            "0xa",
            &json!({"amount": "2.5", "manaAmount": "2500000000000000000", "senderAddress": "0xb"}),
        )
        .unwrap()
        .unwrap();
        assert!(r.content.contains("2.50 MANA"), "{}", r.content);
        let big = render(
            "tip_received",
            "0xa",
            &json!({"amount": "1500", "manaAmount": "1.5e+21", "senderAddress": "0xb"}),
        )
        .unwrap()
        .unwrap();
        assert!(big.content.contains("1500.00 MANA"), "{}", big.content);
    }

    #[test]
    fn world_permission_emails_list_the_metadata_permissions() {
        let metadata = json!({"world": "w.dcl.eth", "permissions": ["deployment", "streaming"]});
        for kind in ["worlds_permission_granted", "worlds_permission_revoked"] {
            let r = render(kind, "0xa", &metadata).unwrap().unwrap();
            assert!(
                r.content.contains("deployment/streaming"),
                "{kind}: {}",
                r.content
            );
        }
    }

    #[test]
    fn rendering_matches_the_old_processor() {
        let r = render(
            "banned",
            "0xa",
            &json!({"userName": "Q\"uote", "reason": "Spam <x>", "bannedAt": "2024-06-01T12:00:00.000Z", "expiresAt": "2024-06-08T12:00:00.000Z"}),
        )
        .unwrap()
        .unwrap();
        assert_eq!(r.fields["subject"], "Account Temporarily Suspended");
        assert_eq!(r.fields["titleHighlight"], "Q\"uote");
        assert!(
            r.content.contains("<b>BAN PERIOD:</b> 7 DAYS<br>"),
            "{}",
            r.content
        );
        assert!(r.content.contains("<b>REASON:</b> Spam &lt;x&gt;"));
        assert!(
            r.content.starts_with("<p style"),
            "standalone block lines are dropped"
        );

        let r = render(
            "bid_received",
            "0xa",
            &json!({"price": "1500000000000000000", "nftName": "Space Jams"}),
        )
        .unwrap()
        .unwrap();
        assert!(r.content.contains(">1.50</td>"), "{}", r.content);
    }
}
