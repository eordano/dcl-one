//! `POST /forward-onboarding-checkpoint` (`forward-onboarding-checkpoint.ts`):
//! `properties.checkpointId` read with JavaScript's `Number` must be 1..7,
//! the notifier's answer; nothing is recorded. The notifier lifted the
//! properties as the auth server's checkpoint body, but a Segment event
//! names its user as it likes: the onboarding state takes a Segment
//! checkpoint only from `/forward`, after the auth chain verified
//! (`explorer`).

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use serde_json::{Value, json};

use super::{SharedState, gate, refused, reply};

pub(super) async fn forward(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Err(response) = gate(&state, &headers, &body) {
        return response;
    }
    let checkpoint_id = js_number(
        serde_json::from_slice::<Value>(&body)
            .ok()
            .as_ref()
            .and_then(|body| body.get("properties"))
            .and_then(|properties| properties.get("checkpointId")),
    );
    if checkpoint_id.is_nan() || !(1.0..=7.0).contains(&checkpoint_id) {
        tracing::warn!(
            checkpoint_id,
            "Invalid or missing checkpointId in event properties"
        );
        return refused(StatusCode::BAD_REQUEST, "Invalid checkpointId");
    }
    reply(StatusCode::OK, json!({ "ok": true }))
}

/// JavaScript's `Number(value)` for a JSON value (`undefined` and objects are `NaN`).
fn js_number(value: Option<&Value>) -> f64 {
    match value {
        None | Some(Value::Object(_)) => f64::NAN,
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::String(s)) => catalyst_shared::js::number_from_str(s),
        Some(Value::Array(items)) => match items.as_slice() {
            [] => 0.0,
            [one] => js_number(Some(one)),
            _ => f64::NAN,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::js_number;
    use serde_json::json;

    #[test]
    fn number_semantics_match_javascript_for_the_checkpoint_id() {
        assert!(js_number(None).is_nan());
        assert_eq!(js_number(Some(&json!(null))), 0.0);
        assert_eq!(js_number(Some(&json!(3))), 3.0);
        assert_eq!(js_number(Some(&json!("3"))), 3.0);
        assert_eq!(js_number(Some(&json!(" 7 "))), 7.0);
        assert_eq!(js_number(Some(&json!(""))), 0.0);
        assert!(js_number(Some(&json!("three"))).is_nan());
        assert!(js_number(Some(&json!({}))).is_nan());
        assert_eq!(js_number(Some(&json!(true))), 1.0);
        assert_eq!(js_number(Some(&json!("0x5"))), 5.0);
        assert!(js_number(Some(&json!("inf"))).is_nan());
        assert!(js_number(Some(&json!("infinity"))).is_nan());
        assert_eq!(js_number(Some(&json!("\u{3000}4\u{FEFF}"))), 4.0);
    }
}
