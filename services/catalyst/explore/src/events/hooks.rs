//! The events channel of the moderation outbox (`crate::moderation_hooks`):
//! authoring captures the public labels of a notice in its transaction, the
//! worker renders and posts the message. The messages are the old events
//! service's (`Slack/utils.ts`): new, approved, rejected, deleted and edited
//! events, the reminder job's upcoming notice and a profile's permission
//! change. Its `notifyEventError` (`Slack/utils.ts:261-294`) had no caller at
//! `b60d5bbd` and is not ported.

use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::moderation_hooks::{Channel, capture_at, enqueue};

/// The sites the notices link to.
#[derive(Clone, Debug)]
pub struct Links {
    /// `BASE_URL` (`Slack/utils.ts:17`): the review link
    /// `/whats-on/admin/pending-events?id=`, and here also the edit link's
    /// site (`eventSlackUrl`, `:19-21`, hard-coded `https://decentraland.org`
    /// upstream; equal on a decentraland.org deployment).
    pub base: String,
    /// `JUMP_IN_SITE_URL` (`:13-16`): a stored url under it is the generated
    /// jump-in link the submission notice labels with the place.
    pub jump_in_site_url: String,
    /// The events asset mount a stored poster path (`/poster/<file>`, the
    /// default asset) is served under: Slack's `image_url` must be absolute.
    pub asset_base: String,
}

impl Links {
    /// A stored poster path on the asset mount; any other image as stored.
    fn image(&self, image: &str) -> String {
        if image.starts_with('/') {
            format!("{}{image}", self.asset_base.trim_end_matches('/'))
        } else {
            image.to_owned()
        }
    }

    /// `eventReviewSlackUrl` (`Slack/utils.ts:23-27`).
    fn review(&self, event_id: &str) -> String {
        format!(
            "{}/whats-on/admin/pending-events?id={}",
            self.base.trim_end_matches('/'),
            urlencoding::encode(event_id)
        )
    }

    /// `eventSlackUrl` (`Slack/utils.ts:19-21`).
    fn edit(&self, event_id: &str) -> String {
        format!(
            "{}/whats-on/edit-hangout/{}?openPreview",
            self.base.trim_end_matches('/'),
            urlencoding::encode(event_id)
        )
    }
}

fn flag(raw: &Value, key: &str) -> bool {
    raw.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn text<'a>(raw: &'a Value, key: &str) -> &'a str {
    raw.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// The public labels a notice renders, bounded so the outbox row stays small:
/// the texts the old messages print (`Slack/utils.ts:33-257`), the actors and
/// the location. Never the organizer's `contact` or `details`.
fn labels(after: &Value) -> Value {
    let mut event = serde_json::Map::new();
    for (key, limit) in [
        ("name", 500),
        ("user_name", 500),
        ("rejection_reason", 500),
        ("deleted_reason", 500),
        ("user", 42),
        ("approved_by", 42),
        ("rejected_by", 42),
        ("deleted_by", 42),
        ("scene_name", 500),
        ("estate_name", 500),
        ("url", 2000),
        ("image", 2000),
        ("description", 1800),
    ] {
        if let Some(value) = after.get(key).and_then(Value::as_str) {
            event.insert(
                key.into(),
                json!(value.chars().take(limit).collect::<String>()),
            );
        }
    }
    for key in ["x", "y"] {
        if let Some(value) = after.get(key).and_then(Value::as_i64) {
            event.insert(key.into(), json!(value));
        }
    }
    Value::Object(event)
}

pub(super) async fn record(
    tx: &mut Transaction<'_, Postgres>,
    event_id: &str,
    before: &Value,
    after: &Value,
    local: bool,
) -> Result<(), sqlx::Error> {
    let newly_deleted = (flag(after, "deleted_by_user") || flag(after, "deleted_by_admin"))
        && !flag(before, "deleted_by_user")
        && !flag(before, "deleted_by_admin");
    let kind = if newly_deleted {
        if !flag(after, "deleted_by_admin") {
            return Ok(());
        }
        "deleted"
    } else if before.is_null() && local {
        "submitted"
    } else if flag(after, "approved") && !flag(before, "approved") {
        "approved"
    } else if flag(after, "rejected") && !flag(before, "rejected") {
        "rejected"
    } else if local
        && [
            "name",
            "description",
            "image",
            "image_vertical",
            "start_at",
            "duration",
            "url",
            "world",
            "server",
            "x",
            "y",
            "categories",
            "schedules",
            "recurrent_dates",
            "community_id",
            "featured_item",
        ]
        .iter()
        .any(|key| before.get(*key) != after.get(*key))
    {
        "edited"
    } else {
        return Ok(());
    };
    let event = labels(after);
    let at = capture_at(tx).await?;
    let creator = after
        .get("user")
        .and_then(Value::as_str)
        .unwrap_or(event_id)
        .to_ascii_lowercase();
    if kind == "edited" {
        // Serialize per creator across different event row locks. Administrative
        // transitions never wait behind this throttle or lose their alert.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 7421))")
            .bind(&creator)
            .execute(&mut **tx)
            .await?;
        let updated = sqlx::query("UPDATE explore.moderation_hooks SET payload=$2 WHERE channel='events' AND subject=$1 AND kind='edited' AND completed_at IS NULL AND attempts=0")
            .bind(event_id).bind(&event).execute(&mut **tx).await?.rows_affected();
        if updated > 0 {
            return Ok(());
        }
    }
    sqlx::query("INSERT INTO explore.moderation_hooks (id,channel,subject,kind,payload,creator_address,edit_bucket,created_at,next_attempt_at) \
        SELECT $1,'events',$2,$3,$4,$5,CASE WHEN $3='edited' THEN date_trunc('minute',$6::timestamp) END,$6,$6 \
        WHERE ($3<>'edited' OR (SELECT count(*) FROM explore.moderation_hooks \
            WHERE channel='events' AND creator_address=$5 AND kind='edited' AND created_at>$6-interval '1 minute')<2) \
        AND ($3<>'edited' OR NOT EXISTS (SELECT 1 FROM explore.moderation_hooks h \
            WHERE h.channel='events' AND h.subject=$2 AND h.kind='edited' AND h.edit_bucket>=date_trunc('minute',$6::timestamp)-interval '1 minute' \
              AND h.created_at>$6-interval '60 seconds')) ON CONFLICT DO NOTHING")
        .bind(Uuid::new_v4()).bind(event_id).bind(kind).bind(event)
        .bind(creator).bind(at).execute(&mut **tx).await?;
    Ok(())
}

/// The old `notifyProfileSettingUpdate` (`Slack/utils.ts:322-358`): a
/// profile's permission set changed. The notice is about a wallet, not an
/// event: the wallet is the row's subject.
pub(super) async fn record_profile_permissions(
    tx: &mut Transaction<'_, Postgres>,
    user: &str,
    permissions: &[String],
) -> Result<(), sqlx::Error> {
    let user = user.to_ascii_lowercase();
    let payload = json!({"user": user, "permissions": permissions});
    enqueue(
        tx,
        Channel::Events,
        &user,
        "profile_permissions",
        &payload,
        Some(&user),
    )
    .await
}

/// The old `notifyUpcomingEvent` (`Slack/utils.ts:222-257`, called from
/// `Event/cron.ts:76` once per occurrence whose reminders went out): one row
/// per occurrence, carrying the count the message prints. No throttle: the
/// reminder job completes an occurrence once (`social.event_notification_intents`
/// is unique per occurrence and kind).
pub(super) async fn record_upcoming(
    tx: &mut Transaction<'_, Postgres>,
    event_id: &str,
    event: &Value,
    notifications: i64,
) -> Result<(), sqlx::Error> {
    let payload = json!({
        "name": text(event, "name").chars().take(500).collect::<String>(),
        "notifications": notifications,
    });
    let creator = event
        .get("user")
        .and_then(Value::as_str)
        .unwrap_or(event_id)
        .to_ascii_lowercase();
    enqueue(
        tx,
        Channel::Events,
        event_id,
        "upcoming",
        &payload,
        Some(&creator),
    )
    .await
}

/// Slack's mrkdwn escapes (`&`, `<`, `>`): user content spliced into a
/// mrkdwn block renders as text, never as a link, mention or command.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// An actor in a `` `code` `` span: no backtick can close it early.
fn actor(text: &str) -> String {
    escape(&text.replace('`', ""))
}

fn section(kind: &str, text: String) -> Value {
    json!({"type":"section","text":{"type":kind,"text":text}})
}

/// `` _by:_ `actor` `` with upstream's self-moderation suffix
/// (`Slack/utils.ts:85-86,105-111,118-119,143-149,179-185`).
fn by_block(event: &Value, key: &str, self_suffix: bool) -> Option<Value> {
    let by = text(event, key);
    if by.is_empty() {
        return None;
    }
    let suffix = if self_suffix && text(event, "user").eq_ignore_ascii_case(by) {
        " _(him/her self)_"
    } else {
        ""
    };
    Some(section(
        "mrkdwn",
        format!("\n_by:_ `{}`{suffix}", actor(by)),
    ))
}

/// The Slack message of a notice about an event (every kind but
/// `profile_permissions`, which [`permissions_message`] renders from the
/// row's `subject`).
fn message(event_id: &str, kind: &str, event: &Value, links: &Links) -> Value {
    let id = escape(event_id);
    let edit = escape(&links.edit(event_id));
    let name = text(event, "name");
    let blocks: Vec<Value> = match kind {
        // `notifyNewEvent` (`Slack/utils.ts:33-81`).
        "submitted" => {
            let review = escape(&links.review(event_id));
            let user_name = text(event, "user_name");
            let description = text(event, "description");
            let mut details = section(
                "plain_text",
                format!(
                    "{name} by {}\n\n{}",
                    if user_name.is_empty() {
                        "Guest"
                    } else {
                        user_name
                    },
                    if description.is_empty() {
                        "No description"
                    } else {
                        description
                    }
                ),
            );
            let image = text(event, "image");
            if !image.is_empty() {
                let scene = text(event, "scene_name");
                details["accessory"] = json!({"type":"image","image_url":links.image(image),
                    "alt_text":if scene.is_empty(){"Decentraland"}else{scene}});
            }
            let mut at = vec![format!("at <{review}|Events page>")];
            let url = text(event, "url");
            if !url.is_empty() {
                if url.starts_with(links.jump_in_site_url.trim_end_matches('/')) {
                    let place = ["estate_name", "scene_name"]
                        .iter()
                        .map(|key| text(event, key))
                        .find(|value| !value.is_empty())
                        .unwrap_or("Decentraland");
                    at.push(format!(
                        "at <{}|{} ({},{})>",
                        escape(url),
                        escape(place),
                        event.get("x").and_then(Value::as_i64).unwrap_or(0),
                        event.get("y").and_then(Value::as_i64).unwrap_or(0)
                    ));
                } else {
                    at.push(format!("at {}", escape(url)));
                }
            }
            vec![
                section(
                    "mrkdwn",
                    format!(":tada: New event submitted: <{review}|{id}>"),
                ),
                details,
                section("mrkdwn", at.join("\n")),
            ]
        }
        // `notifyApprovedEvent` (`Slack/utils.ts:83-114`).
        "approved" => [
            Some(section(
                "mrkdwn",
                format!(":white_check_mark: new event approved: *<{edit}|{id}>*"),
            )),
            Some(section("plain_text", format!("Name: {name}"))),
            by_block(event, "approved_by", true),
        ]
        .into_iter()
        .flatten()
        .collect(),
        // `notifyRejectedEvent` (`Slack/utils.ts:116-152`).
        "rejected" => [
            Some(section(
                "mrkdwn",
                format!(":x: new event rejected: *<{edit}|{id}>*"),
            )),
            Some(section(
                "plain_text",
                name_and_reason(name, text(event, "rejection_reason")),
            )),
            by_block(event, "rejected_by", true),
        ]
        .into_iter()
        .flatten()
        .collect(),
        // `notifyDeletedEvent` (`Slack/utils.ts:154-188`).
        "deleted" => [
            Some(section(
                "mrkdwn",
                format!(":wastebasket: event deleted by admin: *<{edit}|{id}>*"),
            )),
            Some(section(
                "plain_text",
                name_and_reason(name, text(event, "deleted_reason")),
            )),
            by_block(event, "deleted_by", false),
        ]
        .into_iter()
        .flatten()
        .collect(),
        // `notifyUpcomingEvent` (`Slack/utils.ts:222-257`).
        "upcoming" => vec![
            section(
                "mrkdwn",
                format!(":runner: *<{edit}|{id}>* is about to start"),
            ),
            section(
                "plain_text",
                format!(
                    "Name: {name}\nSent: {} push notifications :bell:",
                    event
                        .get("notifications")
                        .and_then(Value::as_i64)
                        .unwrap_or(0)
                ),
            ),
        ],
        // `notifyEditedEvent` (`Slack/utils.ts:190-220`); the per-event
        // minute throttle is `record`'s edit bucket.
        _ => {
            let user_name = text(event, "user_name");
            vec![
                section(
                    "mrkdwn",
                    format!(
                        ":pencil2: user {} just edited his event: *<{edit}|{id}>*",
                        if user_name.is_empty() {
                            "Guest".to_string()
                        } else {
                            escape(user_name)
                        }
                    ),
                ),
                section("plain_text", name.to_string()),
            ]
        }
    };
    json!({ "blocks": blocks })
}

/// `["Name: " + name, reason && "Reason: " + reason].filter(Boolean).join("\n")`.
fn name_and_reason(name: &str, reason: &str) -> String {
    if reason.is_empty() {
        format!("Name: {name}")
    } else {
        format!("Name: {name}\nReason: {reason}")
    }
}

/// `intl/en.json` `page.users.permissions` at `b60d5bbd`, as the old
/// message printed them (the two approve descriptions are swapped there).
const PERMISSION_LABELS: [(&str, &str, &str); 6] = [
    (
        "approve_own_event",
        "Approve events",
        "User can approve events from any other user",
    ),
    (
        "approve_any_event",
        "Approve own events",
        "User can approve events created by it self",
    ),
    (
        "edit_any_event",
        "Edit events",
        "User can edit event's details from any other user",
    ),
    (
        "edit_any_schedule",
        "Edit schedules",
        "User can create, edit and remove schedules",
    ),
    (
        "edit_any_profile",
        "Edit Users",
        "User can grant permissions to any other user",
    ),
    (
        "test_any_notification",
        "Test notifications",
        "User can send test notifications to it self",
    ),
];

/// `Slack/utils.ts:325-357`: the lost-all text, or the new set one line per
/// permission: `- *Name*: _description_` for a known permission, `- *perm*`
/// otherwise. The wallet is a checked address, so the only user content in
/// the mrkdwn is its hex.
fn permissions_message(user: &str, event: &Value) -> Value {
    let permissions: Vec<String> = event
        .get("permissions")
        .and_then(Value::as_array)
        .map(|p| {
            p.iter()
                .filter_map(|p| p.as_str())
                .filter(|p| p.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
                .map(|p| {
                    PERMISSION_LABELS
                        .iter()
                        .find(|(id, _, _)| *id == p)
                        .map(|(_, name, description)| format!("- *{name}*: _{description}_"))
                        .unwrap_or_else(|| format!("- *{p}*"))
                })
                .collect()
        })
        .unwrap_or_default();
    let user: String = user
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(42)
        .collect();
    let text = if permissions.is_empty() {
        format!(":magic_wand: user `{user}` just lost all his/her permissions")
    } else {
        format!(
            ":magic_wand: user `{user}` has new permissions:\n\n{}",
            permissions.join("\n")
        )
    };
    json!({"blocks":[{"type":"section","text":{"type":"mrkdwn","text":text}}]})
}

/// The message of a claimed events row: the permission notice for its
/// wallet, else the event notice for its id.
pub(crate) fn render(subject: &str, kind: &str, payload: &Value, links: &Links) -> Value {
    if kind == "profile_permissions" {
        permissions_message(subject, payload)
    } else {
        message(subject, kind, payload, links)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::moderation_hooks::{CLEANUP_LIMIT, Endpoints, TEST_LOCK, cleanup, run_once};
    use chrono::{NaiveDateTime, Utc};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::Duration;

    /// The events webhook alone.
    fn to(endpoint: Option<&str>) -> Endpoints {
        Endpoints {
            events: endpoint.map(str::to_owned),
            places: None,
        }
    }

    fn links() -> Links {
        Links {
            base: "https://example.org".into(),
            jump_in_site_url: "https://decentraland.org/jump".into(),
            asset_base: "https://assets.example.org/events/".into(),
        }
    }

    fn block_text(message: &Value, index: usize) -> &str {
        message["blocks"][index]["text"]["text"].as_str().unwrap()
    }

    /// `Slack/utils.ts:322-358`: the lost-all text and the one-line-per-permission
    /// text with `intl/en.json`'s names, the fallback line for an unknown one.
    #[test]
    fn profile_permission_notices_follow_the_old_texts() {
        let lost = permissions_message("0xabc", &json!({"permissions":[]}));
        assert_eq!(
            block_text(&lost, 0),
            ":magic_wand: user `0xabc` just lost all his/her permissions"
        );
        let gained = permissions_message(
            "0xabc <@here>",
            &json!({"permissions":["edit_any_event","<script>","custom_grant"]}),
        );
        assert_eq!(
            block_text(&gained, 0),
            ":magic_wand: user `0xabchere` has new permissions:\n\n- *Edit events*: _User can edit event's details from any other user_\n- *custom_grant*"
        );
    }

    /// `Slack/utils.ts:33-81`: the three submission blocks, the review link
    /// (`eventReviewSlackUrl`, `:23-27`) and the place label of a generated
    /// jump-in url; a custom url is printed as is.
    #[test]
    fn new_event_notices_follow_notify_new_event() {
        let event = labels(
            &json!({"name":"Launch & <party>","user_name":"Host","description":"Come <b>all</b>",
            "image":"https://img.example.org/p.png","scene_name":"Plaza","estate_name":"",
            "url":"https://decentraland.org/jump/events?position=10%2C-20","x":10,"y":-20,"contact":"secret"}),
        );
        let notice = message("ev-1", "submitted", &event, &links());
        assert_eq!(
            block_text(&notice, 0),
            ":tada: New event submitted: <https://example.org/whats-on/admin/pending-events?id=ev-1|ev-1>"
        );
        assert_eq!(
            notice["blocks"][1],
            json!({"type":"section","text":{"type":"plain_text","text":"Launch & <party> by Host\n\nCome <b>all</b>"},
                "accessory":{"type":"image","image_url":"https://img.example.org/p.png","alt_text":"Plaza"}})
        );
        assert_eq!(
            block_text(&notice, 2),
            "at <https://example.org/whats-on/admin/pending-events?id=ev-1|Events page>\nat <https://decentraland.org/jump/events?position=10%2C-20|Plaza (10,-20)>"
        );
        assert!(!notice.to_string().contains("secret"));
        let custom = message(
            "ev-2",
            "submitted",
            &labels(&json!({"name":"Guest event","url":"https://example.com/x?a=1&b=<2>"})),
            &links(),
        );
        assert_eq!(
            block_text(&custom, 1),
            "Guest event by Guest\n\nNo description"
        );
        assert!(custom["blocks"][1].get("accessory").is_none());
        assert_eq!(
            block_text(&custom, 2),
            "at <https://example.org/whats-on/admin/pending-events?id=ev-2|Events page>\nat https://example.com/x?a=1&amp;b=&lt;2&gt;"
        );
        // A stored poster is a path on the events mount (`/poster/<file>`,
        // the default asset); Slack's `image_url` must be absolute.
        for (stored, absolute) in [
            (
                "/poster/abc.webp",
                "https://assets.example.org/events/poster/abc.webp",
            ),
            (
                "/assets/event-default.webp",
                "https://assets.example.org/events/assets/event-default.webp",
            ),
            (
                "https://img.example.org/p.png",
                "https://img.example.org/p.png",
            ),
        ] {
            let notice = message(
                "ev-3",
                "submitted",
                &labels(&json!({"name":"Poster","image":stored})),
                &links(),
            );
            assert_eq!(
                notice["blocks"][1]["accessory"]["image_url"], absolute,
                "{stored}"
            );
        }
    }

    /// `Slack/utils.ts:83-220`: approved (self-approval suffix), rejected and
    /// deleted with their reasons and actors, edited by a named or Guest
    /// user; every one links `eventSlackUrl` (`:19-21`).
    #[test]
    fn moderation_notices_follow_the_old_texts_and_links() {
        let edit = "https://example.org/whats-on/edit-hangout/ev-1?openPreview";
        let approved = message(
            "ev-1",
            "approved",
            &labels(&json!({"name":"Launch","user":"0xAbC","approved_by":"0xabc"})),
            &links(),
        );
        assert_eq!(
            block_text(&approved, 0),
            format!(":white_check_mark: new event approved: *<{edit}|ev-1>*")
        );
        assert_eq!(block_text(&approved, 1), "Name: Launch");
        assert_eq!(block_text(&approved, 2), "\n_by:_ `0xabc` _(him/her self)_");
        let rejected = message(
            "ev-1",
            "rejected",
            &labels(
                &json!({"name":"Launch","user":"0xabc","rejected_by":"ad`min","rejection_reason":"Spam"}),
            ),
            &links(),
        );
        assert_eq!(
            block_text(&rejected, 0),
            format!(":x: new event rejected: *<{edit}|ev-1>*")
        );
        assert_eq!(block_text(&rejected, 1), "Name: Launch\nReason: Spam");
        assert_eq!(block_text(&rejected, 2), "\n_by:_ `admin`");
        let deleted = message(
            "ev-1",
            "deleted",
            &labels(&json!({"name":"Launch","user":"0xabc","deleted_by":"0xabc"})),
            &links(),
        );
        assert_eq!(
            block_text(&deleted, 0),
            format!(":wastebasket: event deleted by admin: *<{edit}|ev-1>*")
        );
        assert_eq!(block_text(&deleted, 1), "Name: Launch");
        assert_eq!(
            block_text(&deleted, 2),
            "\n_by:_ `0xabc`",
            "deletion carries no self suffix"
        );
        let unattributed = message(
            "ev-1",
            "deleted",
            &labels(&json!({"name":"Launch"})),
            &links(),
        );
        assert_eq!(unattributed["blocks"].as_array().unwrap().len(), 2);
        let edited = message(
            "ev-1",
            "edited",
            &labels(&json!({"name":"Launch","user_name":"Host <!channel>"})),
            &links(),
        );
        assert_eq!(
            block_text(&edited, 0),
            format!(":pencil2: user Host &lt;!channel&gt; just edited his event: *<{edit}|ev-1>*")
        );
        assert_eq!(block_text(&edited, 1), "Launch");
        let guest = message("ev-1", "edited", &labels(&json!({})), &links());
        assert_eq!(
            block_text(&guest, 0),
            format!(":pencil2: user Guest just edited his event: *<{edit}|ev-1>*")
        );
        // `Slack/utils.ts:222-257`
        let upcoming = message(
            "ev-1",
            "upcoming",
            &json!({"name":"Launch","notifications":3}),
            &links(),
        );
        assert_eq!(
            block_text(&upcoming, 0),
            format!(":runner: *<{edit}|ev-1>* is about to start")
        );
        assert_eq!(
            block_text(&upcoming, 1),
            "Name: Launch\nSent: 3 push notifications :bell:"
        );
    }

    /// `Event/cron.ts:71-76`: no notice without reminders; one row per
    /// occurrence with the count, delivered with the upstream text.
    #[tokio::test]
    async fn upcoming_notices_are_queued_per_reminded_occurrence_and_delivered() {
        let _guard = TEST_LOCK.lock().await;
        let Some(pool) = crate::db::scratch::pool(6).await else {
            return;
        };
        let captured = Arc::new(tokio::sync::Mutex::new(Vec::<Value>::new()));
        let copy = captured.clone();
        let app = axum::Router::new().route(
            "/hook",
            axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
                let copy = copy.clone();
                async move {
                    copy.lock().await.push(body);
                    axum::http::StatusCode::OK
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/hook", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let id = format!("upcoming-{}", Uuid::new_v4());
        let event = json!({"name":"Reminded","user":"0xHOST","contact":"secret"});
        let mut tx = pool.begin().await.unwrap();
        super::super::notifications::record_upcoming_notice(&mut tx, &id, &event, 0)
            .await
            .unwrap();
        super::super::notifications::record_upcoming_notice(&mut tx, &id, &event, 2)
            .await
            .unwrap();
        super::super::notifications::record_upcoming_notice(&mut tx, &id, &event, 5)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let rows: Vec<(String, Value)> = sqlx::query_as(
            "SELECT kind,payload FROM explore.moderation_hooks WHERE subject=$1 ORDER BY payload->>'notifications'",
        )
        .bind(&id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows.len(), 2, "zero reminders queue nothing");
        assert!(rows.iter().all(|(kind, _)| kind == "upcoming"));
        assert_eq!(rows[0].1["notifications"], 2);
        assert_eq!(rows[0].1["name"], "Reminded");
        assert!(rows[0].1.get("contact").is_none());
        assert_eq!(
            run_once(
                &pool,
                &crate::http::Outbound::for_tests(),
                &to(Some(&endpoint)),
                &links(),
                Utc::now().naive_utc() + chrono::Duration::seconds(1)
            )
            .await
            .unwrap(),
            2
        );
        let payloads = captured.lock().await;
        assert!(
            payloads.iter().any(|payload| block_text(payload, 1)
                == "Name: Reminded\nSent: 5 push notifications :bell:")
        );
        assert!(payloads.iter().all(|payload| block_text(payload, 0)
            == format!(":runner: *<https://example.org/whats-on/edit-hangout/{id}?openPreview|{id}>* is about to start")));
        drop(payloads);
        sqlx::query("DELETE FROM explore.moderation_hooks WHERE subject=$1")
            .bind(&id)
            .execute(&pool)
            .await
            .unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn concurrent_first_captures_and_backward_clock_samples_never_precede_activation() {
        let Some(pool) = crate::db::scratch::isolated_pool(6).await else {
            return;
        };
        sqlx::query("DELETE FROM explore.moderation_hook_activation")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE FUNCTION explore.test_hook_capture_barrier() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF current_setting('application_name')='hook-capture-race' THEN PERFORM pg_advisory_xact_lock(742119); END IF; RETURN NEW; END $$").execute(&pool).await.unwrap();
        sqlx::query("CREATE TRIGGER test_hook_capture_barrier BEFORE INSERT ON explore.moderation_hook_activation FOR EACH ROW EXECUTE FUNCTION explore.test_hook_capture_barrier()").execute(&pool).await.unwrap();
        let prefix = Uuid::new_v4().to_string();
        let mut held = pool.acquire().await.unwrap();
        sqlx::query("SELECT pg_advisory_lock(742119)")
            .execute(&mut *held)
            .await
            .unwrap();
        let (ready, waiting) = tokio::sync::oneshot::channel();
        let clone = pool.clone();
        let slow_id = format!("{prefix}-slow");
        let slow = tokio::spawn(async move {
            let mut tx = clone.begin().await.unwrap();
            sqlx::query("SET LOCAL application_name='hook-capture-race'")
                .execute(&mut *tx)
                .await
                .unwrap();
            let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            ready.send(pid).unwrap();
            record(
                &mut tx,
                &slow_id,
                &Value::Null,
                &json!({"name":"Slow first capture"}),
                true,
            )
            .await
            .unwrap();
            tx.commit().await.unwrap();
        });
        let pid = waiting.await.unwrap();
        tokio::time::timeout(Duration::from_secs(3),async {
            loop {
                let blocked:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=$1 AND wait_event='advisory')")
                    .bind(pid).fetch_one(&pool).await.unwrap();
                if blocked {break;}
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        record(
            &mut tx,
            &format!("{prefix}-fast"),
            &Value::Null,
            &json!({"name":"Concurrent activation winner"}),
            true,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        sqlx::query("SELECT pg_advisory_unlock(742119)")
            .execute(&mut *held)
            .await
            .unwrap();
        slow.await.unwrap();
        let concurrent_eligible:bool=sqlx::query_scalar("SELECT bool_and(h.created_at>=a.activated_at) FROM explore.moderation_hooks h CROSS JOIN explore.moderation_hook_activation a WHERE h.subject LIKE $1")
            .bind(format!("{prefix}-%")).fetch_one(&pool).await.unwrap();
        // A future cutoff simulates the wall clock stepping backward after activation.
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("UPDATE explore.moderation_hook_activation SET activated_at=(clock_timestamp() AT TIME ZONE 'UTC')+interval '1 minute'")
            .execute(&mut *tx).await.unwrap();
        record(
            &mut tx,
            &format!("{prefix}-clock"),
            &Value::Null,
            &json!({"name":"Backward clock capture"}),
            true,
        )
        .await
        .unwrap();
        let clock_eligible:bool=sqlx::query_scalar("SELECT h.created_at=a.activated_at FROM explore.moderation_hooks h CROSS JOIN explore.moderation_hook_activation a WHERE h.subject=$1")
            .bind(format!("{prefix}-clock")).fetch_one(&mut *tx).await.unwrap();
        tx.rollback().await.unwrap();
        assert!(
            concurrent_eligible,
            "a delayed first writer must not fall behind another writer's activation"
        );
        assert!(
            clock_eligible,
            "capture timestamps must never fall behind the durable cutoff"
        );
    }

    #[tokio::test]
    async fn capture_activates_with_the_database_clock_before_the_first_worker_tick() {
        let Some(pool) = crate::db::scratch::isolated_pool(6).await else {
            return;
        };
        sqlx::query("DELETE FROM explore.moderation_hook_activation")
            .execute(&pool)
            .await
            .unwrap();
        let id = Uuid::new_v4().to_string();
        let mut tx = pool.begin().await.unwrap();
        record(
            &mut tx,
            &id,
            &Value::Null,
            &json!({"name":"First captured alert"}),
            true,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let (created,activated):(NaiveDateTime,NaiveDateTime)=sqlx::query_as("SELECT h.created_at,a.activated_at FROM explore.moderation_hooks h CROSS JOIN explore.moderation_hook_activation a WHERE h.subject=$1")
            .bind(&id).fetch_one(&pool).await.unwrap();
        assert!(created >= activated);
        run_once(
            &pool,
            &crate::http::Outbound::for_tests(),
            &to(Some("http://127.0.0.1:1/unreachable")),
            &links(),
            Utc::now().naive_utc() + chrono::Duration::minutes(1),
        )
        .await
        .unwrap();
        let (attempts, completed): (i32, Option<NaiveDateTime>) = sqlx::query_as(
            "SELECT attempts,completed_at FROM explore.moderation_hooks WHERE subject=$1",
        )
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            attempts, 1,
            "first captured intent is not discarded on leader activation"
        );
        assert_eq!(completed, None);
    }

    #[tokio::test]
    async fn operator_alerts_precede_a_submission_flood_and_edits() {
        let _guard = TEST_LOCK.lock().await;
        let Some(pool) = crate::db::scratch::pool(6).await else {
            return;
        };
        let captured = Arc::new(tokio::sync::Mutex::new(Vec::<Value>::new()));
        let copy = captured.clone();
        let app = axum::Router::new().route(
            "/hook",
            axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
                let copy = copy.clone();
                async move {
                    copy.lock().await.push(body);
                    axum::http::StatusCode::NO_CONTENT
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/hook", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let prefix = Uuid::new_v4().to_string();
        let mut tx = pool.begin().await.unwrap();
        for index in 0..12 {
            record(
                &mut tx,
                &format!("{prefix}-{index}"),
                &json!({"name":"Before"}),
                &json!({"name":"Edited"}),
                true,
            )
            .await
            .unwrap();
        }
        record(
            &mut tx,
            &format!("{prefix}-submitted"),
            &Value::Null,
            &json!({"name":"New submission"}),
            true,
        )
        .await
        .unwrap();
        for index in 0..300 {
            record(
                &mut tx,
                &format!("{prefix}-flood-{index}"),
                &Value::Null,
                &json!({"name":"Flood submission", "user":prefix}),
                true,
            )
            .await
            .unwrap();
        }
        for (kind, before, after) in [
            (
                "approved",
                json!({"approved":false}),
                json!({"name":"Operator approved", "approved":true}),
            ),
            (
                "rejected",
                json!({"approved":true}),
                json!({"name":"Operator rejected", "approved":false,"rejected":true}),
            ),
            (
                "deleted",
                json!({"approved":true}),
                json!({"name":"Operator deleted", "deleted_by_admin":true}),
            ),
        ] {
            record(
                &mut tx,
                &format!("{prefix}-operator-{kind}"),
                &before,
                &after,
                true,
            )
            .await
            .unwrap();
        }
        tx.commit().await.unwrap();
        assert_eq!(
            run_once(
                &pool,
                &crate::http::Outbound::for_tests(),
                &to(Some(&endpoint)),
                &links(),
                Utc::now().naive_utc() + chrono::Duration::seconds(1)
            )
            .await
            .unwrap(),
            10
        );
        let payloads = captured.lock().await;
        let first: Vec<&str> = payloads
            .iter()
            .take(3)
            .map(|body| body["blocks"][0]["text"]["text"].as_str().unwrap())
            .collect();
        assert!(
            first
                .iter()
                .any(|text| text.starts_with(":white_check_mark: new event approved:"))
        );
        assert!(
            first
                .iter()
                .any(|text| text.starts_with(":x: new event rejected:"))
        );
        assert!(
            first
                .iter()
                .any(|text| text.starts_with(":wastebasket: event deleted by admin:"))
        );
        assert!(
            payloads[3]["blocks"][0]["text"]["text"]
                .as_str()
                .unwrap()
                .starts_with(":tada: New event submitted:")
        );
        sqlx::query("DELETE FROM explore.moderation_hooks WHERE subject LIKE $1")
            .bind(format!("{prefix}-%"))
            .execute(&pool)
            .await
            .unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn creator_throttle_limits_edits_without_dropping_submissions() {
        let Some(pool) = crate::db::scratch::pool(6).await else {
            return;
        };
        let actor = Uuid::new_v4().to_string();
        let mut tx = pool.begin().await.unwrap();
        for i in 0..5 {
            record(
                &mut tx,
                &format!("{actor}-{i}"),
                &Value::Null,
                &json!({"name":"Submission","user":actor}),
                true,
            )
            .await
            .unwrap();
        }
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM explore.moderation_hooks WHERE creator_address=$1",
        )
        .bind(&actor)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        assert_eq!(
            count, 5,
            "every submitted event retains its moderation alert"
        );
        for i in 0..3 {
            record(
                &mut tx,
                &format!("{actor}-edit-{i}"),
                &json!({"name":"Before","user":actor}),
                &json!({"name":"Edited","user":actor}),
                true,
            )
            .await
            .unwrap();
        }
        let edits:i64=sqlx::query_scalar("SELECT count(*) FROM explore.moderation_hooks WHERE creator_address=$1 AND kind='edited'")
            .bind(&actor).fetch_one(&mut *tx).await.unwrap();
        assert_eq!(
            edits, 2,
            "only edited notices use the rolling creator budget"
        );
        record(
            &mut tx,
            &format!("{actor}-after-edits"),
            &Value::Null,
            &json!({"name":"Submission after two edits","user":actor}),
            true,
        )
        .await
        .unwrap();
        let submissions:i64=sqlx::query_scalar("SELECT count(*) FROM explore.moderation_hooks WHERE creator_address=$1 AND kind='submitted'")
            .bind(&actor).fetch_one(&mut *tx).await.unwrap();
        assert_eq!(
            submissions, 6,
            "a full edit budget cannot discard a new submission"
        );
        let id = format!("{actor}-admin");
        record(
            &mut tx,
            &id,
            &json!({"approved":false}),
            &json!({"name":"Moderated","user":actor,"approved":true}),
            true,
        )
        .await
        .unwrap();
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM explore.moderation_hooks WHERE creator_address=$1",
        )
        .bind(&actor)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        assert_eq!(count, 9, "operator transitions bypass user throttles");
        let other = Uuid::new_v4().to_string();
        let before = json!({"name":"Before","user":other});
        record(
            &mut tx,
            &other,
            &before,
            &json!({"name":"Edit one","user":other}),
            true,
        )
        .await
        .unwrap();
        record(
            &mut tx,
            &other,
            &before,
            &json!({"name":"Edit two","user":other}),
            true,
        )
        .await
        .unwrap();
        let names: Vec<String> = sqlx::query_scalar(
            "SELECT payload->>'name' FROM explore.moderation_hooks WHERE subject=$1",
        )
        .bind(&other)
        .fetch_all(&mut *tx)
        .await
        .unwrap();
        assert_eq!(
            names,
            vec!["Edit two"],
            "an unsent edit retains the latest public labels"
        );
        tx.rollback().await.unwrap();
    }

    #[tokio::test]
    async fn disabled_hook_capture_keeps_the_optional_outbox_empty() {
        let Some(pool) = crate::db::scratch::pool(6).await else {
            return;
        };
        let mut tx = pool.begin().await.unwrap();
        let prefix = Uuid::new_v4().to_string();
        let mut settings = crate::Settings::for_tests();
        for (index, (writes, endpoint)) in [
            (false, Some("http://127.0.0.1/hook")),
            (true, None),
            (true, Some("  ")),
            (true, Some("http://127.0.0.1/hook")),
        ]
        .into_iter()
        .enumerate()
        {
            settings.operator_writes_enabled = writes;
            settings.events_slack_webhook = endpoint.map(str::to_owned);
            super::super::notifications::record_transition(
                &mut tx,
                &format!("{prefix}-{index}"),
                None,
                &json!({"name":"Public event"}),
                true,
                &settings,
            )
            .await
            .unwrap();
        }
        let captured: Vec<String> = sqlx::query_scalar(
            "SELECT subject FROM explore.moderation_hooks WHERE subject LIKE $1",
        )
        .bind(format!("{prefix}-%"))
        .fetch_all(&mut *tx)
        .await
        .unwrap();
        assert_eq!(captured, vec![format!("{prefix}-3")]);
        tx.rollback().await.unwrap();
    }

    #[tokio::test]
    async fn disabled_delivery_cleans_expired_history_in_bounded_unlocked_batches_and_retains_retries()
     {
        let Some(pool) = crate::db::scratch::isolated_pool(6).await else {
            return;
        };
        let now = Utc::now().naive_utc();
        let prefix = Uuid::new_v4().to_string();
        // This integration test owns the activation singleton of its own database.
        sqlx::query("DELETE FROM explore.moderation_hook_activation")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO explore.moderation_hook_activation(activated_at) VALUES($1-interval '50 days')").bind(now).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO explore.moderation_hooks(id,channel,subject,kind,payload,created_at,completed_at) SELECT gen_random_uuid(),'events',$1||'-completed','approved','{}',$2-interval '40 days',$2-interval '31 days' FROM generate_series(1,$3)")
            .bind(&prefix).bind(now).bind(CLEANUP_LIMIT+5).execute(&pool).await.unwrap();
        for (suffix, age, completed_age, attempts, retry_age) in [
            ("before-activation", 60, None, 0, 60),
            ("exhausted", 40, None, 6, 31),
            ("retry", 40, None, 2, 31),
            ("recent-completion", 40, Some(1), 1, 1),
            ("recent-edit", 0, Some(0), 1, 0),
            ("locked", 70, Some(31), 1, 31),
        ] {
            sqlx::query("INSERT INTO explore.moderation_hooks(id,channel,subject,kind,payload,created_at,completed_at,attempts,next_attempt_at) VALUES($1,'events',$2,'edited','{}',$3-$4::int4*interval '1 day',CASE WHEN $5::int4 IS NOT NULL THEN $3-$5::int4*interval '1 day' END,$6,$3-$7::int4*interval '1 day')")
                .bind(Uuid::new_v4()).bind(format!("{prefix}-{suffix}")).bind(now).bind(age).bind(completed_age).bind(attempts).bind(retry_age).execute(&pool).await.unwrap();
        }
        let mut held = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM explore.moderation_hooks WHERE subject=$1 FOR UPDATE")
            .bind(format!("{prefix}-locked"))
            .fetch_one(&mut *held)
            .await
            .unwrap();
        let before: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM explore.moderation_hooks WHERE subject LIKE $1",
        )
        .bind(format!("{prefix}-%"))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            run_once(
                &pool,
                &crate::http::Outbound::for_tests(),
                &to(None),
                &links(),
                now
            )
            .await
            .unwrap(),
            0
        );
        let after: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM explore.moderation_hooks WHERE subject LIKE $1",
        )
        .bind(format!("{prefix}-%"))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(before - after, CLEANUP_LIMIT);
        assert_eq!(cleanup(&pool, now).await.unwrap(), 7);
        let retained: Vec<String> = sqlx::query_scalar(
            "SELECT subject FROM explore.moderation_hooks WHERE subject LIKE $1 ORDER BY subject",
        )
        .bind(format!("{prefix}-%"))
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            retained,
            ["locked", "recent-completion", "recent-edit", "retry"]
                .map(|suffix| format!("{prefix}-{suffix}"))
        );
        held.rollback().await.unwrap();
        assert_eq!(cleanup(&pool, now).await.unwrap(), 1);
    }

    #[tokio::test]
    async fn edit_throttle_covers_the_rolling_minute_across_bucket_boundaries() {
        let Some(pool) = crate::db::scratch::pool(6).await else {
            return;
        };
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("INSERT INTO explore.moderation_hooks (id,channel,subject,kind,payload,edit_bucket,created_at) \
            VALUES ($1,'events','rolling-hook','edited','{}',date_trunc('minute',now() AT TIME ZONE 'UTC')-interval '1 minute', \
                (now() AT TIME ZONE 'UTC')-interval '30 seconds')")
            .bind(Uuid::new_v4()).execute(&mut *tx).await.unwrap();
        let before = json!({"name":"Before"});
        let after = json!({"name":"After"});
        record(&mut tx, "rolling-hook", &before, &after, true)
            .await
            .unwrap();
        let one: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM explore.moderation_hooks WHERE subject='rolling-hook'",
        )
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        assert_eq!(
            one, 1,
            "last bucket still suppresses an edit within 60 seconds"
        );
        sqlx::query("UPDATE explore.moderation_hooks SET edit_bucket=edit_bucket-interval '1 minute',created_at=created_at-interval '1 minute' WHERE subject='rolling-hook'")
            .execute(&mut *tx).await.unwrap();
        record(&mut tx, "rolling-hook", &before, &after, true)
            .await
            .unwrap();
        let two: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM explore.moderation_hooks WHERE subject='rolling-hook'",
        )
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        assert_eq!(
            two, 1,
            "an unsent edit is coalesced even after the throttle window"
        );
        tx.rollback().await.unwrap();
    }

    #[tokio::test]
    async fn moderation_hooks_skip_disabled_history_retry_boundedly_and_keep_private_fields_out() {
        let _guard = TEST_LOCK.lock().await;
        let Some(pool) = crate::db::scratch::pool(6).await else {
            return;
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let payloads = Arc::new(tokio::sync::Mutex::new(Vec::<Value>::new()));
        let (captured_calls, captured_payloads) = (calls.clone(), payloads.clone());
        let app = axum::Router::new().route(
            "/hook",
            axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
                let (calls, payloads) = (captured_calls.clone(), captured_payloads.clone());
                async move {
                    payloads.lock().await.push(body);
                    if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        axum::http::StatusCode::SERVICE_UNAVAILABLE
                    } else {
                        axum::http::StatusCode::OK
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/hook", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let now = chrono::DateTime::parse_from_rfc3339("2099-08-01T18:00:00Z")
            .unwrap()
            .naive_utc();
        let client = crate::http::Outbound::for_tests();
        assert_eq!(
            run_once(&pool, &client, &to(Some(&endpoint)), &links(), now)
                .await
                .unwrap(),
            0
        );
        let mut tx = pool.begin().await.unwrap();
        let raw = json!({"name":"Public @channel <attack>","contact":"secret contact","details":"secret details","user_name":"Host"});
        record(&mut tx, "hook-event", &Value::Null, &raw, true)
            .await
            .unwrap();
        let mut edited = raw.clone();
        edited["name"] = json!("New public name");
        record(&mut tx, "hook-event", &raw, &edited, true)
            .await
            .unwrap();
        record(&mut tx, "hook-event", &raw, &edited, true)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        sqlx::query("UPDATE explore.moderation_hooks SET created_at=$1,next_attempt_at=$1 WHERE subject='hook-event'")
            .bind(now+chrono::Duration::seconds(1)).execute(&pool).await.unwrap();
        let queued: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM explore.moderation_hooks WHERE subject='hook-event'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(queued, 2, "owner edits are throttled once per event/minute");
        // Any pre-existing disabled backlog remains behind the activation cutoff.
        sqlx::query("INSERT INTO explore.moderation_hooks (id,channel,subject,kind,payload,created_at) VALUES ($1,'events','old-hook','submitted','{}',(SELECT activated_at-interval '1 day' FROM explore.moderation_hook_activation WHERE singleton))")
            .bind(Uuid::new_v4()).execute(&pool).await.unwrap();
        assert_eq!(
            run_once(
                &pool,
                &client,
                &to(Some(&endpoint)),
                &links(),
                now + chrono::Duration::seconds(1)
            )
            .await
            .unwrap(),
            1
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "only two current records, including failed first delivery"
        );
        assert_eq!(
            run_once(
                &pool,
                &client,
                &to(Some(&endpoint)),
                &links(),
                now + chrono::Duration::seconds(2)
            )
            .await
            .unwrap(),
            0
        );
        assert_eq!(
            run_once(
                &pool,
                &client,
                &to(Some(&endpoint)),
                &links(),
                now + chrono::Duration::minutes(2)
            )
            .await
            .unwrap(),
            1
        );
        for payload in payloads.lock().await.iter() {
            assert!(!payload.to_string().contains("secret"));
            assert_eq!(payload["blocks"][1]["text"]["type"], "plain_text");
        }
        // Exhaustion is durable; the worker never retries a sixth failed attempt.
        sqlx::query("UPDATE explore.moderation_hooks SET completed_at=NULL,attempts=5,next_attempt_at=$1 WHERE subject='hook-event'")
            .bind(now).execute(&pool).await.unwrap();
        let exhausted = run_once(
            &pool,
            &client,
            &to(Some("http://127.0.0.1:1/unreachable")),
            &links(),
            now + chrono::Duration::minutes(3),
        )
        .await
        .unwrap();
        assert_eq!(exhausted, 0);
        let pending:i64=sqlx::query_scalar("SELECT count(*) FROM explore.moderation_hooks WHERE subject='hook-event' AND completed_at IS NULL").fetch_one(&pool).await.unwrap();
        assert_eq!(
            pending, 2,
            "exhausted failures are distinct from successful completion"
        );
        let mut tx = pool.begin().await.unwrap();
        let mut state = raw.clone();
        record(&mut tx, "hook-lifecycle", &Value::Null, &state, true)
            .await
            .unwrap();
        for (key, value) in [
            ("approved", true),
            ("rejected", true),
            ("deleted_by_admin", true),
        ] {
            let before = state.clone();
            state[key] = json!(value);
            record(&mut tx, "hook-lifecycle", &before, &state, true)
                .await
                .unwrap();
        }
        let kinds:Vec<String>=sqlx::query_scalar("SELECT kind FROM explore.moderation_hooks WHERE subject='hook-lifecycle' ORDER BY kind")
            .fetch_all(&mut *tx).await.unwrap();
        assert_eq!(kinds, vec!["approved", "deleted", "rejected", "submitted"]);
        tx.rollback().await.unwrap();
        let rolled_back: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM explore.moderation_hooks WHERE subject='hook-lifecycle'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            rolled_back, 0,
            "integration intents share authoring transaction rollback"
        );
        server.abort();
    }
}
