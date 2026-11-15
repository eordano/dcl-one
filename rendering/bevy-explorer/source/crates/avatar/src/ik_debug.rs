use bevy::prelude::*;
use bevy_console::ConsoleCommand;
use console::{AddDebugSection, DebugInput, DebugInputKind, DebugSection, DoAddConsoleCommand};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};

use crate::{foot_ik::FootIkConfig, head_ik::HeadIkConfig};

/// The limits here govern both the editor and direct console writes.
struct Range {
    label: &'static str,
    field: &'static str,
    min: f64,
    max: f64,
    step: f64,
}

const fn range(label: &'static str, field: &'static str, min: f64, max: f64, step: f64) -> Range {
    Range {
        label,
        field,
        min,
        max,
        step,
    }
}

const FEET: &[Range] = &[
    range("Plant height (m)", "plant_y", 0.0, 2.0, 0.001),
    range("Raycast above foot (m)", "raycast_up", 0.0, 5.0, 0.01),
    range("Raycast below foot (m)", "raycast_down", 0.0, 5.0, 0.01),
    range("Maximum step up (m)", "max_step_up", 0.0, 3.0, 0.01),
    range("Maximum pelvis drop (m)", "max_pelvis_drop", 0.0, 3.0, 0.01),
    range(
        "Minimum transition (s)",
        "min_transition_seconds",
        0.001,
        5.0,
        0.001,
    ),
    range(
        "Maximum tilt (degrees)",
        "max_foot_tilt_deg",
        0.0,
        90.0,
        1.0,
    ),
    range(
        "Engagement transition (s)",
        "engage_transition_seconds",
        0.001,
        5.0,
        0.001,
    ),
    range(
        "Target velocity limit (m/s)",
        "target_velocity_limit",
        0.0,
        20.0,
        0.1,
    ),
];
const HEAD: &[Range] = &[
    range("Vertical limit (degrees)", "pitch_limit", 0.0, 89.0, 1.0),
    range("Horizontal limit (degrees)", "yaw_limit", 0.0, 180.0, 1.0),
    range("Horizontal reset (degrees)", "yaw_reset", 0.0, 180.0, 1.0),
    range(
        "Rotation smoothing (s)",
        "smooth_seconds",
        0.001,
        5.0,
        0.001,
    ),
];

pub(super) struct IkDebugPlugin;

impl Plugin for IkDebugPlugin {
    fn build(&self, app: &mut App) {
        app.add_console_command::<IkCommand, _>(ik_command);
        for (title, query, ranges) in [("Feet IK", "/ik feet", FEET), ("Head IK", "/ik head", HEAD)]
        {
            let enabled = format!("{query} enabled {{value}}");
            let commands: Vec<_> = ranges
                .iter()
                .map(|r| format!("{query} {} {{value}}", r.field))
                .collect();
            let mut inputs = vec![DebugInput {
                label: "Enabled",
                field: "enabled",
                command: &enabled,
                kind: DebugInputKind::Toggle,
            }];
            inputs.extend(ranges.iter().zip(&commands).map(|(r, command)| DebugInput {
                label: r.label,
                field: r.field,
                command,
                kind: DebugInputKind::Number {
                    min: r.min,
                    max: r.max,
                    step: r.step,
                },
            }));
            app.add_preview_debug_inputs(DebugSection {
                title,
                query,
                fields: &[],
                actions: &[],
                inputs: &inputs,
            });
        }
    }
}

/// Read or tune the active feet or head IK solver.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/ik")]
struct IkCommand {
    #[arg(value_parser = ["feet", "head"])]
    solver: String,
    #[arg(requires = "value")]
    field: Option<String>,
    #[arg(requires = "field")]
    value: Option<String>,
}

fn ik_command(
    mut cmd: ConsoleCommand<IkCommand>,
    preview: Option<Res<common::structs::PreviewMode>>,
    mut feet: ResMut<FootIkConfig>,
    mut head: ResMut<HeadIkConfig>,
) {
    let Some(Ok(request)) = cmd.take() else {
        return;
    };
    if request.field.is_some() && !preview.is_some_and(|p| p.is_preview) {
        cmd.reply_failed("solver overrides require preview mode");
        return;
    }
    let result = match request.solver.as_str() {
        "feet" => tune(&request, feet.reborrow(), FEET),
        _ => tune(&request, head.reborrow(), HEAD),
    };
    match result {
        Ok(state) => cmd.reply_ok(state.to_string()),
        Err(error) => cmd.reply_failed(error),
    }
}

fn tune<T: Serialize + DeserializeOwned>(
    request: &IkCommand,
    mut config: Mut<T>,
    ranges: &[Range],
) -> Result<Value, String> {
    let mut state = serde_json::to_value(&*config).map_err(|e| e.to_string())?;
    if let (Some(field), Some(raw)) = (&request.field, &request.value) {
        let value = if field == "enabled" {
            json!(raw.parse::<bool>().map_err(|_| "expected true or false")?)
        } else {
            let r = ranges
                .iter()
                .find(|r| r.field == field)
                .ok_or("unknown IK setting")?;
            let value = raw.parse::<f64>().map_err(|_| "expected a number")?;
            if !(r.min..=r.max).contains(&value) {
                return Err(format!("{} must be in {}..={}", r.label, r.min, r.max));
            }
            json!(value)
        };
        state[field.as_str()] = value;
        *config = serde_json::from_value(state.clone()).map_err(|e| e.to_string())?;
    }
    Ok(state)
}
