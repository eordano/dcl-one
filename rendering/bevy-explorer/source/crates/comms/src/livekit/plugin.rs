use bevy::prelude::*;
use common::{
    debug_panic,
    inputs::SystemAction,
    rpc::{RpcCall, RpcCallEvent, RpcOrigin},
    structs::MicState,
};
use dcl_component::proto_components::kernel::apis::{
    VideoTrackSourceType, VideoTracksActiveStreamsData, VideoTracksActiveStreamsResponse,
};
use input_manager::{InputManager, InputPriority};
use livestream_manager::ActiveTransmitter;
use system_bridge::SystemApi;
use tokio::{sync::mpsc, task::JoinHandle};
#[cfg(not(target_arch = "wasm32"))]
use {
    crate::livekit::LivekitAudioManager,
    common::structs::AudioSettings,
    kira::{
        manager::{AudioManager, AudioManagerSettings, DefaultBackend},
        track::TrackBuilder,
        tween::Tween,
    },
};

#[cfg(feature = "room_debug")]
use crate::livekit::room_debug::RoomDebugPlugin;
use crate::{
    global_crdt::NetworkUpdate,
    livekit::{
        mic::MicPlugin,
        participant::{plugin::LivekitParticipantPlugin, HostingParticipants, LivekitParticipant},
        room::plugin::LivekitRoomPlugin,
        runtime::LivekitRuntimePlugin,
        track::{
            plugin::LivekitTrackPlugin, Camera as LivekitSourceCamera, LivekitTrack, Publishing,
            ScreenshareVideo as LivekitSourceScreenshare, Video,
        },
        ConnectionAvailability, LivekitRuntime, LivekitTransport, StartLivekit,
    },
    voice_chat::{state::Input as VoiceInput, CallMic, VoiceInputs},
    SceneRoom, Transport, TransportType,
};

pub struct LivekitPlugin;

impl Plugin for LivekitPlugin {
    fn build(&self, app: &mut App) {
        #[cfg(not(target_arch = "wasm32"))]
        app.init_resource::<PlayerUpdateTasks>();
        #[cfg(target_arch = "wasm32")]
        app.init_non_send_resource::<PlayerUpdateTasks>();
        app.init_state::<ConnectionAvailability>();

        #[cfg(target_arch = "wasm32")]
        {
            app.insert_resource(crate::livekit::web::take_event_receiver())
                .init_resource::<crate::livekit::web::MicrophoneStatus>()
                .add_systems(PreUpdate, crate::livekit::web::dispatch_page_events);
        }

        // a headless app has no microphone, and plays no voices
        let headless = app
            .world()
            .get_resource::<common::structs::NoRenderApp>()
            .is_some();
        if !headless {
            app.add_plugins(MicPlugin);
        }
        app.add_plugins(LivekitRuntimePlugin);
        app.add_plugins(LivekitRoomPlugin);
        app.add_plugins(super::room::diagnostics::RoomDiagnosticsPlugin);
        app.add_plugins(LivekitParticipantPlugin);
        app.add_plugins(LivekitTrackPlugin);

        app.add_systems(
            Update,
            (
                reset_identity_policy,
                start_livekit,
                verify_player_update_tasks,
            ),
        );
        // on the web the page plays voice (livekit-client), and the engine worker has no audio
        // output; a headless app plays nothing, so opens no audio device
        #[cfg(not(target_arch = "wasm32"))]
        if !headless {
            app.add_systems(Startup, build_kira_audio_manager);
            app.add_systems(
                Update,
                respond_to_audio_settings_change
                    .run_if(resource_exists_and_changed::<AudioSettings>),
            );
        }

        app.add_event::<StartLivekit>();

        app.add_systems(
            Update,
            (handle_input, respond_to_system_api, respond_to_rpc_calls),
        );

        #[cfg(feature = "room_debug")]
        app.add_plugins(RoomDebugPlugin);
    }
}

fn reset_identity_policy(
    wallet: Res<wallet::Wallet>,
    mut identity: Local<Option<alloy_core::primitives::Address>>,
    mut availability: ResMut<NextState<ConnectionAvailability>>,
) {
    if wallet.is_changed() && *identity != wallet.address() {
        *identity = wallet.address();
        availability.set(ConnectionAvailability::Available);
    }
}

/// Holds [`LivekitRuntime`]s, so on the web it is non-send like them (see `runtime.rs`).
#[derive(Default, Deref, DerefMut)]
#[cfg_attr(not(target_arch = "wasm32"), derive(Resource))]
pub(super) struct PlayerUpdateTasks(Vec<PlayerUpdateTask>);

#[cfg(not(target_arch = "wasm32"))]
pub(super) type PlayerUpdateTasksMut<'w> = ResMut<'w, PlayerUpdateTasks>;
#[cfg(target_arch = "wasm32")]
pub(super) type PlayerUpdateTasksMut<'w> = NonSendMut<'w, PlayerUpdateTasks>;

pub(super) struct PlayerUpdateTask {
    pub runtime: LivekitRuntime,
    pub task: JoinHandle<Result<(), mpsc::error::SendError<NetworkUpdate>>>,
}

fn start_livekit(
    mut commands: Commands,
    mut room_events: EventReader<StartLivekit>,
    mut existing: Query<&mut LivekitTransport>,
) {
    for ev in room_events.read() {
        if crate::adapter::livekit_credentials(&ev.address).is_err() {
            warn!("invalid or insecure LiveKit adapter");
            continue;
        }
        let Ok(mut entity) = commands.get_entity(ev.entity) else {
            continue;
        };
        if let Ok(mut transport) = existing.get_mut(ev.entity) {
            transport.address.clone_from(&ev.address);
            entity.try_remove::<super::room::Connecting>();
            entity.try_insert(super::room::Connecting);
            continue;
        }
        info!("starting livekit protocol");
        let (components, sender, control_sender) =
            super::standalone_room_components(ev.address.clone());

        // No connect-time profile announce here — `profile::mod` periodically announces the version
        // over PRIMARY (Pulse + websocket dev server), and the Pulse handshake carries the
        // connect-time version. Peers reach this transport for guest profile request/response by
        // picking a profile-capable transport at request time, not off any announcement of ours.

        entity.try_insert((
            components,
            super::RoomSender {
                sender: sender.clone(),
                control: control_sender.clone(),
            },
        ));
        if let Some(context) = ev.context {
            entity.try_insert(Transport {
                transport_type: TransportType::Livekit,
                sender,
                control: Some(control_sender),
                context,
            });
        }
    }
}

#[cfg(test)]
mod stale_start_tests {
    use super::*;

    #[derive(Resource)]
    struct Departing(Entity);

    #[test]
    fn adapter_start_after_queued_despawn_is_harmless() {
        let mut app = App::new();
        let owner = app.world_mut().spawn_empty().id();
        app.insert_resource(Departing(owner))
            .add_event::<StartLivekit>()
            .add_systems(
                Update,
                (
                    |mut commands: Commands, departing: Res<Departing>| {
                        commands.entity(departing.0).despawn();
                    },
                    start_livekit,
                )
                    .chain_ignore_deferred(),
            );
        app.world_mut().send_event(StartLivekit {
            entity: owner,
            address: "wss://example.org?access_token=test".into(),
            context: None,
        });
        app.update();
        assert!(app.world().get_entity(owner).is_err());
    }
}

fn verify_player_update_tasks(mut player_update_tasks: PlayerUpdateTasksMut) {
    let mut done = vec![];
    for (
        i,
        PlayerUpdateTask {
            runtime,
            ref mut task,
        },
    ) in player_update_tasks.iter_mut().enumerate()
    {
        if task.is_finished() {
            done.push(i);
            let res = runtime.block_on(task);
            match res {
                Ok(res) => {
                    if let Err(err) = res {
                        debug_panic!("Failed to send PlayerUpdate due to {err}.");
                    }
                }
                Err(err) => {
                    debug_panic!("Failed to pull PlayerUpdateTask due to '{err}'.");
                }
            }
        }
    }

    while let Some(i) = done.pop() {
        player_update_tasks.remove(i);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn build_kira_audio_manager(mut commands: Commands, audio_settings: Option<Res<AudioSettings>>) {
    let (scene, voice) = audio_settings
        .map(|s| (s.scene(), s.voice_chat()))
        .unwrap_or((1.0, 1.0));
    let mut manager = match AudioManager::new(AudioManagerSettings::<DefaultBackend>::default()) {
        Ok(manager) => manager,
        Err(err) => {
            debug_panic!("Failed to livekit build AudioManager due to '{err}'.");
        }
    };
    let scene_track = manager.add_sub_track(TrackBuilder::new().volume(scene as f64));
    let voice_track = manager.add_sub_track(TrackBuilder::new().volume(voice as f64));
    match (scene_track, voice_track) {
        (Ok(scene_track), Ok(voice_track)) => {
            debug!("Livekit AudioManager built.");
            commands.insert_resource(LivekitAudioManager {
                manager,
                scene_track,
                voice_track,
            });
        }
        (Err(err), _) | (_, Err(err)) => {
            debug_panic!("Failed to build the livekit audio tracks due to '{err}'.");
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn respond_to_audio_settings_change(
    mut livekit_audio_manager: ResMut<LivekitAudioManager>,
    audio_settings: Res<AudioSettings>,
) {
    livekit_audio_manager
        .scene_track
        .set_volume(audio_settings.scene() as f64, Tween::default());
    livekit_audio_manager
        .voice_track
        .set_volume(audio_settings.voice_chat() as f64, Tween::default());
}

/// The microphone key toggles the nearby mic, or the call mic while a call is connected.
fn handle_input(
    input_manager: InputManager,
    mut mic_state: ResMut<MicState>,
    mut pressed: Local<bool>,
    call_mic: Option<Res<CallMic>>,
    voice_inputs: Option<ResMut<VoiceInputs>>,
) {
    let toggled = if *pressed {
        if input_manager.just_up(SystemAction::Microphone) {
            *pressed = false;
            true
        } else {
            false
        }
    } else if input_manager.just_down(SystemAction::Microphone, InputPriority::None) {
        *pressed = true;
        true
    } else {
        false
    };
    if !toggled {
        return;
    }
    match (call_mic, voice_inputs) {
        (Some(call_mic), Some(mut voice_inputs)) if call_mic.in_call => {
            voice_inputs
                .0
                .push_back(VoiceInput::SetMic(!call_mic.enabled));
        }
        _ => mic_state.enabled = !mic_state.enabled,
    }
}

fn respond_to_system_api(mut events: EventReader<SystemApi>, mut mic: ResMut<MicState>) {
    for ev in events.read() {
        match ev {
            SystemApi::GetMicState(rpc_result_sender) => {
                rpc_result_sender.send(mic.clone());
            }
            SystemApi::SetMicEnabled(enabled) => {
                mic.enabled = mic.available && *enabled;
            }
            _ => (),
        }
    }
}

#[expect(clippy::type_complexity, reason = "Queries are complex")]
fn respond_to_rpc_calls(
    mut rpc_calls: EventReader<RpcCallEvent>,
    participants: Query<(&LivekitParticipant, &Publishing)>,
    tracks: Query<
        (
            &LivekitTrack,
            AnyOf<(&LivekitSourceCamera, &LivekitSourceScreenshare)>,
            Has<ActiveTransmitter>,
        ),
        With<Video>,
    >,
    scene_rooms: Query<(&HostingParticipants, &SceneRoom)>,
) {
    for RpcCallEvent { origin, call } in rpc_calls.read() {
        #[expect(clippy::single_match, reason = "may be expanded later")]
        match call {
            RpcCall::ActiveVideoStreams { response } => {
                let RpcOrigin::Scene { entity: _, hash } = origin else {
                    response.send(VideoTracksActiveStreamsResponse { streams: vec![] });
                    continue;
                };
                let Some((hosting_participants, _)) = scene_rooms
                    .iter()
                    .find(|(_, scene_room)| scene_room.0.as_str() == &**hash)
                else {
                    debug!("No mapping between RpcCall origin and a transport.");
                    continue;
                };

                let mut streams = vec![];
                for (participant, publishing) in
                    participants.iter_many(hosting_participants.collection())
                {
                    for (track, source, has_active_streamer) in
                        tracks.iter_many(publishing.collection())
                    {
                        let identity = participant.identity().0;
                        let track_sid = format!("livekit-video://{}/{}", identity, track.sid());

                        if has_active_streamer {
                            streams.push(VideoTracksActiveStreamsData {
                                identity: participant.identity().0.clone(),
                                track_sid: "livekit-video://current-stream".to_owned(),
                                source_type: match source {
                                    (Some(_), _) => VideoTrackSourceType::VtstCamera as i32,
                                    (_, Some(_)) => VideoTrackSourceType::VtstScreenShare as i32,
                                    _ => VideoTrackSourceType::VtstUnknown as i32,
                                },
                            });
                        }

                        streams.push(VideoTracksActiveStreamsData {
                            identity: participant.identity().0,
                            track_sid,
                            source_type: match source {
                                (Some(_), _) => VideoTrackSourceType::VtstCamera as i32,
                                (_, Some(_)) => VideoTrackSourceType::VtstScreenShare as i32,
                                _ => VideoTrackSourceType::VtstUnknown as i32,
                            },
                        });
                    }
                }
                response.send(VideoTracksActiveStreamsResponse { streams });
            }
            _ => (),
        }
    }
}
