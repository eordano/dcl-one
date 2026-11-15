//! `/track <base64>`: the host page's analytics events, queued into the engine's Segment batch
//! with the common fields. The payload is one base64 token (the console splits on whitespace) of
//! a JSON array of `{ event, properties, timestamp, messageId }`, stamped by the page when each
//! happened.

use base64::Engine;
use bevy::prelude::*;
use bevy_console::ConsoleCommand;
use console::DoAddConsoleCommand;
use serde::Deserialize;
use uuid::Uuid;

use crate::{
    data_definition::SegmentEvent,
    segment_system::{SegmentConfig, SegmentMetricsEvents},
};

pub struct PageEventsPlugin;

impl Plugin for PageEventsPlugin {
    fn build(&self, app: &mut App) {
        app.add_console_command::<TrackCommand, _>(track_cmd);
    }
}

/// Queue analytics events from the host page: base64 of a JSON array of
/// `{event, properties, timestamp, messageId}`
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/track")]
struct TrackCommand {
    payload: String,
}

#[derive(Deserialize)]
struct TrackItem {
    event: String,
    properties: serde_json::Map<String, serde_json::Value>,
    timestamp: String,
    /// The page's, so a batch it sends again after a failed hand-over keeps its ids.
    #[serde(rename = "messageId", default)]
    message_id: Option<String>,
}

// Always an ok reply: the page takes a failed command as "the engine never got it" and re-sends
// the same batch, which would never get better.
fn track_cmd(
    mut input: ConsoleCommand<TrackCommand>,
    mut metrics: ResMut<SegmentMetricsEvents>,
    config: Res<SegmentConfig>,
) {
    if let Some(Ok(command)) = input.take() {
        match decode(&command.payload) {
            Ok(items) => {
                let count = items.len();
                for item in items {
                    metrics.add_event_with_id(
                        SegmentEvent::Page {
                            name: item.event,
                            properties: item.properties,
                            timestamp: item.timestamp,
                        },
                        config.common(),
                        item.message_id
                            .unwrap_or_else(|| Uuid::new_v4().to_string()),
                    );
                }
                input.reply_ok(format!("queued {count}"));
            }
            Err(e) => {
                warn!("/track: {e}");
                input.reply_ok(format!("rejected: {e}"));
            }
        }
    }
}

fn decode(payload: &str) -> Result<Vec<TrackItem>, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .map_err(|e| format!("not base64: {e}"))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("not a JSON array of events: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_definition::SegmentEventCommonExplorerFields;

    // The payload exactly as react-web/src/lib/analytics.ts encodes it, and the batch item
    // Segment receives for it: the page's time and messageId, the common fields, the event's own
    // on top (its session over the engine's).
    #[test]
    fn a_page_event_keeps_its_time_and_id_and_gets_the_common_fields() {
        let payload = base64::engine::general_purpose::STANDARD.encode(
            r#"[{"event":"Screen Viewed","properties":{"screen_name":"LOBBY","session_id":"page"},"timestamp":"2026-07-02T13:38:08.243Z","messageId":"m-page"}]"#,
        );
        let items = decode(&payload).unwrap();
        assert_eq!(items.len(), 1);
        let item = items.into_iter().next().unwrap();
        let common = SegmentEventCommonExplorerFields::new(
            "engine".into(),
            "1.0.0".into(),
            Some("landing-visitor".into()),
        );
        let mut metrics = SegmentMetricsEvents::default();
        metrics.add_event_with_id(
            SegmentEvent::Page {
                name: item.event,
                properties: item.properties,
                timestamp: item.timestamp,
            },
            &common,
            item.message_id.unwrap(),
        );
        // built the day it happened: a page event is pruned by its own time
        let that_day = chrono::DateTime::parse_from_rfc3339("2026-07-02T14:00:00Z")
            .unwrap()
            .to_utc();
        let batch = metrics.next_batch("user", that_day);
        let json: serde_json::Value = serde_json::from_str(&batch[0]).unwrap();
        assert_eq!(json["event"], "Screen Viewed");
        assert_eq!(json["timestamp"], "2026-07-02T13:38:08.243Z");
        assert_eq!(json["messageId"], "m-page");
        assert_eq!(json["properties"]["screen_name"], "LOBBY");
        assert_eq!(json["properties"]["session_id"], "page");
        assert_eq!(
            json["properties"]["campaign_anon_user_id"],
            "landing-visitor"
        );
        assert!(json["properties"]["dcl_renderer_type"]
            .as_str()
            .is_some_and(|t| t.starts_with("dao-bevy-")));

        assert!(decode("not base64!").is_err());
        assert!(decode(&base64::engine::general_purpose::STANDARD.encode("{}")).is_err());
    }
}
