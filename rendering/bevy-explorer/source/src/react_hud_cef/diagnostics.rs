use bevy::prelude::*;
use bevy_console::ConsoleCommand;
use cef_offscreen::prelude::{RenderPaintElementType, RenderTexture};
use console::{AddDebugSection, DebugSection, DoAddConsoleCommand};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(2);
// The HUD's CEF browser requests at most 60 paints per second.
const MAX_SAMPLES: usize = 128;

pub(super) struct CefHudDiagnosticsPlugin;

impl Plugin for CefHudDiagnosticsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<HudPaintStats>()
            .add_console_command::<CefHudPerfCommand, _>(cef_hud_perf)
            .add_debug_section(DebugSection {
                title: "CEF HUD",
                query: "/cef_hud_perf",
                fields: &[
                    ("Paint buffer (px) (Width / Height)", "paintSize"),
                    ("Paint rate (paints/s, last 2 s)", "paintsPerSecond"),
                    ("Bitmap traffic (MiB/s, last 2 s)", "bitmapMiBPerSecond"),
                    ("CEF bitmap copy (ms) (Mean / Max)", "timings.copy"),
                    (
                        "Copy to engine handoff (ms) (Mean / Max)",
                        "timings.handoff",
                    ),
                    (
                        "Bevy image preparation (ms) (Mean / Max)",
                        "timings.prepare",
                    ),
                ],
                actions: &[],
                inputs: &[],
            });
    }
}

/// Read timings for fullscreen CEF HUD paints applied to the Bevy image.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/cef_hud_perf")]
struct CefHudPerfCommand;

fn cef_hud_perf(mut cmd: ConsoleCommand<CefHudPerfCommand>, stats: Res<HudPaintStats>) {
    if let Some(Ok(_)) = cmd.take() {
        cmd.reply_ok(stats.readings(Instant::now()).to_string());
    }
}

struct PaintSample {
    at: Instant,
    size: [u32; 2],
    bytes: u64,
    copy: Duration,
    handoff: Duration,
    prepare: Duration,
}

#[derive(Default, Resource)]
pub(super) struct HudPaintStats {
    samples: VecDeque<PaintSample>,
}

impl HudPaintStats {
    pub(super) fn record(
        &mut self,
        texture: &RenderTexture,
        engine_started_at: Instant,
        prepare: Duration,
    ) {
        if texture.ty != RenderPaintElementType::View {
            return;
        }
        if self.samples.len() == MAX_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(PaintSample {
            at: texture.bitmap_copied_at,
            size: [texture.width, texture.height],
            bytes: texture.buffer.len() as u64,
            copy: texture.bitmap_copy_time,
            handoff: engine_started_at.saturating_duration_since(texture.bitmap_copied_at),
            prepare,
        });
    }

    fn readings(&self, now: Instant) -> Value {
        let latest = self.samples.back();
        let mut count = 0;
        let mut bytes = 0;
        let mut copy = Timing::default();
        let mut handoff = Timing::default();
        let mut prepare = Timing::default();
        for sample in self
            .samples
            .iter()
            .rev()
            .take_while(|sample| now.saturating_duration_since(sample.at) < WINDOW)
        {
            count += 1;
            bytes += sample.bytes;
            copy.add(sample.copy);
            handoff.add(sample.handoff);
            prepare.add(sample.prepare);
        }
        let seconds = WINDOW.as_secs_f64();
        json!({
            "paintSize": latest.map(|sample| sample.size),
            "paintsPerSecond": latest.map(|_| f64::from(count) / seconds),
            "bitmapMiBPerSecond": latest.map(|_| round_hundredths(bytes as f64 / (1024.0 * 1024.0 * seconds))),
            "timings": {
                "copy": copy.summary(count),
                "handoff": handoff.summary(count),
                "prepare": prepare.summary(count),
            },
            "unavailable": {
                "": "Waiting for the first fullscreen CEF HUD paint.",
                "timings": if latest.is_none() {
                    "Waiting for the first fullscreen CEF HUD paint."
                } else {
                    "No fullscreen CEF HUD paint in the last 2 seconds."
                },
            },
        })
    }
}

#[derive(Default)]
struct Timing {
    sum: Duration,
    max: Duration,
}

impl Timing {
    fn add(&mut self, value: Duration) {
        self.sum += value;
        self.max = self.max.max(value);
    }

    fn summary(&self, count: u32) -> Option<[f64; 2]> {
        (count != 0).then(|| {
            [
                round_hundredths((self.sum / count).as_secs_f64() * 1000.0),
                round_hundredths(self.max.as_secs_f64() * 1000.0),
            ]
        })
    }
}

fn round_hundredths(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paint(at: Instant, kind: RenderPaintElementType, copy_ms: u64) -> RenderTexture {
        RenderTexture {
            webview: Entity::PLACEHOLDER,
            ty: kind,
            width: 512,
            height: 512,
            buffer: vec![0; 1024 * 1024],
            bitmap_copy_time: Duration::from_millis(copy_ms),
            bitmap_copied_at: at,
        }
    }

    #[test]
    fn reports_recent_fullscreen_paints_and_then_idle() {
        let mut stats = HudPaintStats::default();
        let now = Instant::now();
        assert!(stats.readings(now)["paintsPerSecond"].is_null());
        let view = RenderPaintElementType::View;
        let prepare = Duration::from_millis(4);
        stats.record(&paint(now, view, 1), now, prepare);
        let second = now + Duration::from_secs(1);
        stats.record(
            &paint(second, view, 3),
            second + Duration::from_millis(2),
            prepare,
        );
        stats.record(
            &paint(second, RenderPaintElementType::Popup, 50),
            second,
            prepare,
        );

        let active = stats.readings(second);
        assert_eq!(active["paintsPerSecond"], 1.0);
        assert_eq!(active["bitmapMiBPerSecond"], 1.0);
        assert_eq!(active["timings"]["copy"], json!([2.0, 3.0]));
        assert_eq!(active["timings"]["handoff"], json!([1.0, 2.0]));
        assert_eq!(active["timings"]["prepare"], json!([4.0, 4.0]));

        let idle = stats.readings(second + WINDOW);
        assert_eq!(idle["paintSize"], json!([512, 512]));
        assert_eq!(idle["paintsPerSecond"], 0.0);
        assert_eq!(idle["bitmapMiBPerSecond"], 0.0);
        assert!(idle["timings"]["copy"].is_null());
    }
}
