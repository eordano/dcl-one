use super::*;
use crate::{
    global_crdt::{ForeignAudioSource, NearbyPanning},
    livekit::{
        participant::{ActiveSpeaker, HostedBy, Local},
        room::Connected,
        track::{Microphone, Subscribed},
    },
    SceneRoom, Transport,
};
use bevy::ecs::relationship::Relationship;
use bevy_console::ConsoleCommand;
use console::{AddDebugSection, DebugInput, DebugInputKind, DebugSection, DoAddConsoleCommand};
use serde_json::json;
use std::collections::BTreeSet;

pub(super) struct MicDebugPlugin;

impl Plugin for MicDebugPlugin {
    fn build(&self, app: &mut App) {
        app.add_console_command::<MicrophoneInfo, _>(microphone_info)
            .add_console_command::<NearbyVoice, _>(nearby_voice)
            .add_console_command::<VoicePanning, _>(voice_panning)
            .add_debug_section(DebugSection {
                title: "Voice chat",
                query: "/microphone_info",
                fields: &[
                    ("Microphone available", "available"),
                    ("Microphone enabled", "enabled"),
                    ("Room connected", "connected"),
                    ("Permission", "permission"),
                    ("Local microphone track active", "transmitting"),
                    ("Active remote speakers (up to 32)", "speakers"),
                    #[cfg(not(target_arch = "wasm32"))]
                    ("Available input devices", "devices"),
                    #[cfg(not(target_arch = "wasm32"))]
                    ("Current input device", "device"),
                    #[cfg(not(target_arch = "wasm32"))]
                    ("Capture sample rate (Hz)", "rate"),
                    #[cfg(not(target_arch = "wasm32"))]
                    ("Capture channels", "channels"),
                ],
                actions: &[],
                inputs: &[],
            })
            .add_debug_section(DebugSection {
                title: "Nearby voice chat",
                query: "/nearby_voice",
                fields: &[
                    ("Island connected", "connected"),
                    ("Local speaking", "speaking"),
                    ("Remote participants", "participants"),
                    ("Active speakers", "speakerCount"),
                    ("Speakers (up to 32)", "speakers"),
                    ("Subscribed microphone tracks (all rooms)", "tracks"),
                    ("Player audio sources (all transports)", "sources"),
                    (
                        "Sources with a selected transport (all transports)",
                        "assigned",
                    ),
                ],
                actions: &[],
                inputs: &[
                    DebugInput {
                        label: "Spatialize",
                        field: "spatialize",
                        command: "/voice_panning spatialize {value}",
                        kind: DebugInputKind::Toggle,
                    },
                    DebugInput {
                        label: "Smooth panning",
                        field: "smooth",
                        command: "/voice_panning smooth {value}",
                        kind: DebugInputKind::Toggle,
                    },
                    DebugInput {
                        label: "Stereo panning strength",
                        field: "strength",
                        command: "/voice_panning strength {value}",
                        kind: DebugInputKind::Number {
                            min: 0.0,
                            max: 1.0,
                            step: 0.05,
                        },
                    },
                ],
            });
    }
}

/// Report the microphone device and capture state.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/microphone_info")]
struct MicrophoneInfo;

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn microphone_info(
    mut input: ConsoleCommand<MicrophoneInfo>,
    mic: Res<MicState>,
    connected: Query<
        (),
        (
            With<crate::livekit::LivekitTransport>,
            With<Transport>,
            With<Connected>,
        ),
    >,
    permission: Res<State<MicrophonePermission>>,
    transmission: Res<State<MicrophoneTransmissionState>>,
    speakers: Query<&LivekitParticipant, (With<ActiveSpeaker>, Without<Local>)>,
    #[cfg(not(target_arch = "wasm32"))] device: Option<Res<MicrophoneDevice>>,
    #[cfg(not(target_arch = "wasm32"))] format: Option<Res<CaptureFormat>>,
) {
    let Some(Ok(_)) = input.take() else { return };
    let speakers: BTreeSet<_> = speakers.iter().map(|p| p.identity().to_string()).collect();
    #[allow(unused_mut)]
    let mut report = json!({"available": mic.available, "enabled": mic.enabled, "connected": !connected.is_empty(), "permission": format!("{:?}", permission.get()),
        "transmitting": *transmission.get() == MicrophoneTransmissionState::Enabled, "speakers": speakers.into_iter().take(32).collect::<Vec<_>>()});
    #[cfg(not(target_arch = "wasm32"))]
    {
        report["device"] = json!(device.and_then(|d| d.name().ok()));
        report["devices"] = json!(cpal::default_host()
            .input_devices()
            .ok()
            .map(|devices| devices.count()));
        report["rate"] = json!(format.as_ref().map(|f| f.sample_rate));
        report["channels"] = json!(format.as_ref().map(|f| f.channels));
        report["unavailable"] = json!({
            "rate": "Microphone capture is inactive",
            "channels": "Microphone capture is inactive",
            "device": "No current input device",
            "devices": "Input device enumeration failed",
        });
    }
    input.reply_ok(report.to_string());
}

/// Inspect nearby voice membership and the current spatial playback settings.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/nearby_voice")]
struct NearbyVoice;

#[allow(clippy::type_complexity)]
fn nearby_voice(
    mut input: ConsoleCommand<NearbyVoice>,
    settings: Res<NearbyPanning>,
    islands: Query<
        Has<Connected>,
        (
            With<super::super::LivekitTransport>,
            With<Transport>,
            Without<SceneRoom>,
        ),
    >,
    participants: Query<(
        &HostedBy,
        &LivekitParticipant,
        Has<Local>,
        Has<ActiveSpeaker>,
    )>,
    sources: Query<&ForeignAudioSource>,
    tracks: Query<(), (With<Subscribed>, With<Microphone>)>,
) {
    if let Some(Ok(_)) = input.take() {
        let (mut speaking, mut remotes) = (false, 0);
        let mut speakers = BTreeSet::new();
        for (room, participant, local, active) in &participants {
            if !islands.contains(room.get()) {
                continue;
            }
            if local {
                speaking |= active;
            } else {
                remotes += 1;
            }
            if active {
                speakers.insert(participant.identity().to_string());
            }
        }
        input.reply_ok(json!({"connected": islands.iter().any(|connected| connected), "speaking": speaking, "participants": remotes,
            "speakerCount": speakers.len(), "speakers": speakers.into_iter().take(32).collect::<Vec<_>>(), "tracks": tracks.iter().count(),
            "sources": sources.iter().count(), "assigned": sources.iter().filter(|s| s.current_transport.is_some()).count(),
            "spatialize": settings.spatialize, "smooth": settings.smooth, "strength": settings.strength}).to_string());
    }
}

#[derive(Clone, clap::ValueEnum)]
enum PanSetting {
    Spatialize,
    Smooth,
    Strength,
}

/// Set spatialization, panning smoothing, or stereo strength for remote voices.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/voice_panning")]
struct VoicePanning {
    setting: PanSetting,
    value: String,
}

fn voice_panning(mut input: ConsoleCommand<VoicePanning>, mut settings: ResMut<NearbyPanning>) {
    let Some(Ok(command)) = input.take() else {
        return;
    };
    match command.setting {
        PanSetting::Strength => {
            let Some(value) = command
                .value
                .parse::<f32>()
                .ok()
                .filter(|x| (0.0..=1.0).contains(x))
            else {
                input.reply_failed("Strength must be between 0 and 1");
                return;
            };
            settings.strength = value;
        }
        PanSetting::Spatialize | PanSetting::Smooth => {
            let Ok(value) = command.value.parse::<bool>() else {
                input.reply_failed("Expected true or false");
                return;
            };
            if matches!(command.setting, PanSetting::Spatialize) {
                settings.spatialize = value;
            } else {
                settings.smooth = value;
            }
        }
    }
    input.reply_ok("Voice playback settings updated");
}
