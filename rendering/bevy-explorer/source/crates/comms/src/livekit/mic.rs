#[cfg(not(target_arch = "wasm32"))]
use std::borrow::Cow;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};

use bevy::{ecs::relationship::Relationship, prelude::*};
use common::structs::{MicState, SystemAudio};
use tokio::{sync::oneshot, task::JoinHandle};
use web_time::Instant;
#[cfg(target_arch = "wasm32")]
use {
    bevy::render::view::RenderLayers,
    common::{structs::AudioSettings, util::AsH160, util::VolumePanning},
    livestream_manager::AudioTransmitterVolume,
};
#[cfg(not(target_arch = "wasm32"))]
use {
    cpal::{
        traits::{DeviceTrait, HostTrait, StreamTrait},
        Device,
    },
    livekit::{
        options::TrackPublishOptions,
        participant::Participant,
        track::{LocalAudioTrack, LocalTrack, TrackSource},
        webrtc::{
            audio_source::native::NativeAudioSource,
            prelude::{AudioFrame, AudioSourceOptions, RtcAudioSource},
        },
    },
    tokio::sync::broadcast,
};

#[cfg(not(target_arch = "wasm32"))]
use crate::global_crdt::{LocalAudioFrame, LocalAudioSource};
#[cfg(target_arch = "wasm32")]
use crate::{
    global_crdt::{ForeignAudioSource, ForeignPlayer, NearbyPanning, VoicePan},
    livekit::{
        track::{plugin::ProximityMicrophone, LivekitTrack, Publishing, Subscribed},
        web::{
            prompt_microphone_permission, setup_microphone_permission, AudioCaptureOptions,
            LocalAudioTrack, LocalTrack, MicrophoneStatus, Participant, TrackPublishOptions,
            TrackSource,
        },
    },
};
use crate::{
    livekit::{
        participant::{HostedBy, LivekitParticipant, Local as LivekitLocalParticipant},
        LivekitRuntimeRes,
    },
    private_chat::PrivateChatRoom,
    recovery::RetryState,
    voice_chat::{CallMic, VoiceRoom},
};

#[path = "mic_debug.rs"]
mod debug;

pub struct MicPlugin;

impl Plugin for MicPlugin {
    fn build(&self, app: &mut App) {
        #[cfg(not(target_arch = "wasm32"))]
        app.init_non_send_resource::<MicStream>();

        app.add_plugins(debug::MicDebugPlugin);
        app.init_state::<MicrophoneAvailability>();
        app.init_state::<MicrophonePermission>();
        app.init_state::<MicrophoneState>();
        app.init_state::<MicrophoneTransmissionState>();
        app.init_resource::<MicrophoneDrain>();

        #[cfg(target_arch = "wasm32")]
        app.add_systems(Startup, setup_microphone_permission);
        app.add_systems(
            Update,
            (
                #[cfg(target_arch = "wasm32")]
                poll_microphone_permission,
                verify_availability.run_if(in_state(MicrophoneAvailability::Unavailable)),
                verify_microphone_device_health.run_if(in_state(MicrophoneAvailability::Available)),
                verify_enabled.run_if(
                    in_state(MicrophoneAvailability::Available)
                        .and(in_state(MicrophoneState::Disabled)),
                ),
                verify_disabled.run_if(in_state(MicrophoneState::Enabled)),
                // Revocation runs even when permission or device availability was lost.
                reconcile_publications,
            )
                .chain(),
        );
        #[cfg(not(target_arch = "wasm32"))]
        app.add_systems(OnEnter(MicrophoneState::Enabled), build_cpal_stream);
        #[cfg(not(target_arch = "wasm32"))]
        app.add_systems(OnEnter(MicrophoneState::Disabled), drop_cpal_stream);
        #[cfg(target_arch = "wasm32")]
        app.add_systems(
            OnEnter(MicrophoneState::Enabled),
            prompt_microphone_permission.run_if(in_state(MicrophonePermission::Prompt)),
        );
        #[cfg(target_arch = "wasm32")]
        app.add_systems(
            OnEnter(MicrophonePermission::Prompt),
            prompt_microphone_permission.run_if(in_state(MicrophoneState::Enabled)),
        );
        #[cfg(not(target_arch = "wasm32"))]
        app.add_systems(OnEnter(MicrophoneAvailability::Available), setup_microphone);
        #[cfg(not(target_arch = "wasm32"))]
        app.add_systems(
            OnExit(MicrophoneAvailability::Available),
            tear_down_microphone,
        );

        app.add_systems(Startup, add_ping_systems);
        app.add_systems(Update, transmission_state);

        #[cfg(not(target_arch = "wasm32"))]
        app.insert_state(MicrophonePermission::Granted);

        #[cfg(target_arch = "wasm32")]
        app.add_systems(Update, locate_foreign_streams);
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, States)]
enum MicrophoneAvailability {
    #[default]
    Unavailable,
    Available,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, States)]
enum MicrophonePermission {
    #[default]
    Denied,
    #[cfg(target_arch = "wasm32")]
    Prompt,
    Granted,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, States)]
enum MicrophoneState {
    #[default]
    Disabled,
    Enabled,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, States)]
enum MicrophoneTransmissionState {
    #[default]
    Disabled,
    Enabled,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Resource, Deref, DerefMut)]
struct MicrophoneDevice(Device);

/// New routes wait until every revoked route has acknowledged stopping its source.
#[derive(Resource, Default, Clone)]
pub(crate) struct MicrophoneDrain(Arc<AtomicUsize>);

impl MicrophoneDrain {
    pub(crate) fn pending(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Default)]
struct PublicationControl {
    track: Option<LocalAudioTrack>,
    revoked: bool,
    stopped_at: Option<Instant>,
    draining: bool,
    transmitting: bool,
    retry: RetryState,
}

/// Dropping a participant or changing its route revokes its async work, including work which
/// has not created a track yet. The task continues only to stop/unpublish its own track.
#[derive(Component)]
pub(crate) struct MicrophonePublication {
    state: Arc<Mutex<PublicationControl>>,
    revoked: Arc<AtomicBool>,
    cancel: tokio::sync::watch::Sender<bool>,
    drain: MicrophoneDrain,
    task: JoinHandle<()>,
}

impl MicrophonePublication {
    fn queue_insert(self, commands: &mut Commands, entity: Entity, start: oneshot::Sender<()>) {
        // A linked participant can disappear before the command applies. In that case dropping
        // the command revokes the publication and leaves the task without permission to start.
        commands
            .entity(entity)
            .queue_silenced(move |mut entity: EntityWorldMut| {
                // A previous room may have been despawned earlier in this command flush.
                if self.drain.pending() == 0 {
                    entity.insert(self);
                    let _ = start.send(());
                }
            });
    }

    fn should_remove(&self, eligible: bool) -> bool {
        // A task exit without an acknowledged stop still needs the drain barrier.
        !eligible
            || self.revoked.load(Ordering::SeqCst)
            || (self.task.is_finished()
                && self
                    .state
                    .lock()
                    .unwrap_or_else(|err| err.into_inner())
                    .stopped_at
                    .is_none())
    }

    fn retry_state(&self, now: Instant) -> Option<RetryState> {
        if !self.task.is_finished() {
            return None;
        }
        let state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        let elapsed = now
            .saturating_duration_since(state.stopped_at?)
            .as_secs_f64();
        (!state.revoked && state.retry.ready(elapsed)).then(|| state.retry.clone())
    }

    pub(crate) fn revoke(&self) {
        self.revoked.store(true, Ordering::SeqCst);
        let mut state = self.state.lock().unwrap_or_else(|err| err.into_inner());
        state.revoked = true;
        state.transmitting = false;
        if let Some(track) = &state.track {
            mute_local_track(track, true);
        }
        if state.stopped_at.is_none() && !state.draining {
            state.draining = true;
            self.drain.0.fetch_add(1, Ordering::SeqCst);
        }
        let _ = self.cancel.send(true);
    }
}

impl Drop for MicrophonePublication {
    fn drop(&mut self) {
        self.revoke();
    }
}

fn complete_publication(state: &Mutex<PublicationControl>, drain: &MicrophoneDrain, stopped: bool) {
    let mut state = state.lock().unwrap_or_else(|err| err.into_inner());
    state.transmitting = false;
    if stopped {
        state.track = None;
        state.stopped_at = Some(Instant::now());
        if !state.revoked {
            state.retry.failed(0.0, None);
        }
        if state.draining {
            state.draining = false;
            drain.0.fetch_sub(1, Ordering::SeqCst);
        }
    } else if !state.draining {
        // Missing acknowledgment fails closed even if cleanup failed before revocation.
        state.draining = true;
        drain.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn mute_local_track(track: &LocalAudioTrack, muted: bool) {
    #[cfg(not(target_arch = "wasm32"))]
    if muted {
        track.mute();
        // The native SDK enables the RTC track at the end of publish even if it is logically muted.
        track.disable();
    } else {
        track.unmute();
    }
    #[cfg(target_arch = "wasm32")]
    track.set_enabled(!muted);
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Default, Deref, DerefMut)]
struct MicStream(Option<cpal::Stream>);

#[cfg(not(target_arch = "wasm32"))]
#[derive(Resource)]
struct CaptureFormat {
    sample_rate: u32,
    channels: u32,
}

fn add_ping_systems(world: &mut World) {
    // This stops the ping being played during startup
    world.resource_scope::<Schedules, ()>(|_, mut schedules| {
        schedules.add_systems(OnEnter(MicrophoneTransmissionState::Enabled), ping_enabled);
        schedules.add_systems(
            OnEnter(MicrophoneTransmissionState::Disabled),
            ping_disabled,
        );
    });
}

fn transmission_state(
    tracks: Query<&MicrophonePublication>,
    microphone_transmission_state: Res<State<MicrophoneTransmissionState>>,
    mut next_state: ResMut<NextState<MicrophoneTransmissionState>>,
) {
    let transmitting = tracks.iter().any(|publication| {
        let state = publication
            .state
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        state.transmitting
    });
    if !transmitting && microphone_transmission_state.get() == &MicrophoneTransmissionState::Enabled
    {
        next_state.set(MicrophoneTransmissionState::Disabled);
    } else if transmitting
        && microphone_transmission_state.get() == &MicrophoneTransmissionState::Disabled
    {
        next_state.set(MicrophoneTransmissionState::Enabled);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn verify_availability(mut commands: Commands, mut mic_state: ResMut<MicState>) {
    let default_host = cpal::default_host();
    let maybe_device = default_host.default_input_device();

    if let Some(device) = maybe_device {
        debug!(
            "Default microphone '{}' set as input device.",
            device
                .name()
                .expect("Shouldn't became unavailable in such a short span.")
        );
        mic_state.available = true;
        commands.set_state(MicrophoneAvailability::Available);
    }
}

#[cfg(target_arch = "wasm32")]
fn verify_availability(
    mut commands: Commands,
    mut mic_state: ResMut<MicState>,
    microphone_status: Res<MicrophoneStatus>,
) {
    // Check if microphone is available in the browser
    let current_available = microphone_status.available;

    // Only update availability if it changed
    if current_available {
        debug!("Microphone became available.");
        mic_state.available = true;
        commands.set_state(MicrophoneAvailability::Available);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn verify_microphone_device_health(
    mut commands: Commands,
    microphone_device: Res<MicrophoneDevice>,
    mut mic_state: ResMut<MicState>,
) {
    if let Err(err) = microphone_device.name() {
        debug!("Microphone device became unavailable due to '{err}'.");
        mic_state.available = false;
        commands.set_state(MicrophoneAvailability::Unavailable);
    }
}

#[cfg(target_arch = "wasm32")]
fn verify_microphone_device_health(
    mut commands: Commands,
    mut mic_state: ResMut<MicState>,
    microphone_status: Res<MicrophoneStatus>,
) {
    // Check if microphone is available in the browser
    let current_available = microphone_status.available;

    if !current_available {
        debug!("Microphone became unavailable.");
        mic_state.available = false;
        commands.set_state(MicrophoneAvailability::Unavailable);
    }
}

#[cfg(target_arch = "wasm32")]
fn poll_microphone_permission(
    mut commands: Commands,
    microphone_permission: Res<State<MicrophonePermission>>,
    microphone_status: Res<MicrophoneStatus>,
) {
    match microphone_status.permission.as_str() {
        "granted" => {
            if *microphone_permission.get() != MicrophonePermission::Granted {
                debug!("Granted microphone permission.");
                commands.set_state(MicrophonePermission::Granted);
            }
        }
        "prompt" => {
            if *microphone_permission.get() != MicrophonePermission::Prompt {
                debug!("Microphone permission needs to be prompted.");
                commands.set_state(MicrophonePermission::Prompt);
            }
        }
        "denied" => {
            if *microphone_permission.get() != MicrophonePermission::Denied {
                debug!("Denied microphone permission.");
                commands.set_state(MicrophonePermission::Denied);
            }
        }
        other => panic!("Unknown microphone permission '{}'.", other),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn setup_microphone(mut commands: Commands) {
    let default_host = cpal::default_host();
    let maybe_device = default_host.default_input_device();

    if let Some(device) = maybe_device {
        commands.insert_resource(MicrophoneDevice(device));
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn tear_down_microphone(mut commands: Commands) {
    commands.remove_resource::<MicrophoneDevice>();
}

/// A call holds the nearby mic off without changing the user's setting.
fn nearby_mic_on(mic_state: &MicState, call_mic: &CallMic) -> bool {
    mic_state.enabled && !call_mic.suppress_nearby
}

fn verify_enabled(mut commands: Commands, mic_state: Res<MicState>, call_mic: Res<CallMic>) {
    if nearby_mic_on(&mic_state, &call_mic) || call_mic.enabled {
        debug!("Microphone is now enabled.");
        commands.set_state(MicrophoneState::Enabled);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn build_cpal_stream(
    mut commands: Commands,
    microphone_device: Res<MicrophoneDevice>,
    local_audio_source: Res<LocalAudioSource>,
    mut mic_stream: NonSendMut<MicStream>,
) {
    let Ok(config) = microphone_device
        .default_input_config()
        .inspect_err(|err| error!("{err}"))
    else {
        // Do not change state until `MicState::enabled` changes to prevent
        // log spam
        return;
    };
    let sender = local_audio_source.sender.clone();

    let num_channels = config.channels() as u32;
    let sample_rate = config.sample_rate().0;

    let new_stream = microphone_device
        .build_input_stream(
            &config.into(),
            move |data_f32: &[f32], _: &cpal::InputCallbackInfo| {
                let mut data_uninit = Arc::new_uninit_slice(data_f32.len());
                let data_slice = Arc::get_mut(&mut data_uninit).unwrap();
                for (src, dest) in data_f32.iter().zip(data_slice.iter_mut()) {
                    dest.write((*src * i16::MAX as f32).round() as i16);
                }
                // SAFETY: we have initialized all 'len' elements
                let data = unsafe { data_uninit.assume_init() };
                if sender
                    .send(LocalAudioFrame {
                        data: data.to_owned(),
                        sample_rate,
                        num_channels,
                        samples_per_channel: data.len() as u32 / num_channels,
                    })
                    .is_err()
                {
                    warn!("mic channel closed?");
                }
            },
            |err: cpal::StreamError| {
                warn!("mic error: {err}");
            },
            None,
        )
        .unwrap();

    if let Err(err) = new_stream.play() {
        error!("{err}");
    } else {
        debug!("Cpal stream built.");
        commands.set_state(MicrophoneState::Enabled);
        **mic_stream = Some(new_stream);
        commands.insert_resource(CaptureFormat {
            sample_rate,
            channels: num_channels,
        });
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn drop_cpal_stream(mut commands: Commands, mut mic_stream: NonSendMut<MicStream>) {
    **mic_stream = None;
    commands.remove_resource::<CaptureFormat>();
    commands.set_state(MicrophoneState::Disabled);
}

fn verify_disabled(mut commands: Commands, mic_state: Res<MicState>, call_mic: Res<CallMic>) {
    if !nearby_mic_on(&mic_state, &call_mic) && !call_mic.enabled {
        debug!("Microphone is now disabled.");
        commands.set_state(MicrophoneState::Disabled);
    }
}

fn can_publish_microphone(participant: &Participant) -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    if let Participant::Local(local) = participant {
        return local
            .permission()
            .is_none_or(|permission| permission.can_publish);
    }
    matches!(participant, Participant::Local(_))
}

fn microphone_route_enabled(
    (proximity, private, voice, connected): (bool, bool, bool, bool),
    mic: &MicState,
    call_mic: &CallMic,
) -> bool {
    connected
        && !private
        && if voice {
            call_mic.enabled
        } else {
            proximity && nearby_mic_on(mic, call_mic)
        }
}

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn reconcile_publications(
    mut commands: Commands,
    participants: Query<
        (
            Entity,
            &LivekitParticipant,
            &HostedBy,
            Option<&MicrophonePublication>,
        ),
        With<LivekitLocalParticipant>,
    >,
    rooms: Query<(
        Has<crate::Transport>,
        Has<PrivateChatRoom>,
        Has<VoiceRoom>,
        Has<super::room::Connected>,
    )>,
    mic: Res<MicState>,
    call_mic: Res<CallMic>,
    permission: Res<State<MicrophonePermission>>,
    availability: Res<State<MicrophoneAvailability>>,
    drain: Res<MicrophoneDrain>,
    livekit_runtime: LivekitRuntimeRes,
    #[cfg(not(target_arch = "wasm32"))] source: Res<LocalAudioSource>,
    #[cfg(not(target_arch = "wasm32"))] format: Option<Res<CaptureFormat>>,
) {
    let enabled = mic.available
        && *permission.get() == MicrophonePermission::Granted
        && *availability.get() == MicrophoneAvailability::Available;
    let eligible = |room: Entity, participant: &LivekitParticipant| {
        rooms.get(room).is_ok_and(|room| {
            enabled
                && microphone_route_enabled(room, &mic, &call_mic)
                && can_publish_microphone(participant)
        })
    };

    // Revoke all old routes before considering any new one. Removing the component runs Drop
    // at the next command flush; avoid starting a replacement in this same update.
    let mut revoking = false;
    for (entity, participant, room, publication) in &participants {
        if let Some(publication) = publication {
            if publication.should_remove(eligible(room.get(), participant)) {
                commands
                    .entity(entity)
                    .try_remove::<MicrophonePublication>();
                revoking = true;
            }
        }
    }
    if revoking || drain.pending() != 0 {
        return;
    }

    #[cfg(not(target_arch = "wasm32"))]
    let Some(format) = format
    else {
        return;
    };

    let now = Instant::now();
    for (entity, participant, room, publication) in &participants {
        if !eligible(room.get(), participant) {
            continue;
        }
        let retry = if let Some(publication) = publication {
            let Some(retry) = publication.retry_state(now) else {
                continue;
            };
            retry
        } else {
            RetryState::default()
        };
        let Participant::Local(participant) = &**participant else {
            continue;
        };
        let participant = participant.clone();
        let state = Arc::new(Mutex::new(PublicationControl {
            retry,
            ..Default::default()
        }));
        let task_state = state.clone();
        let revoked = Arc::new(AtomicBool::new(false));
        let task_revoked = revoked.clone();
        let task_drain = drain.clone();
        let (cancel, mut cancellation) = tokio::sync::watch::channel(false);
        let (start, installed) = oneshot::channel();
        #[cfg(not(target_arch = "wasm32"))]
        let receiver = source.subscribe();
        #[cfg(not(target_arch = "wasm32"))]
        let (sample_rate, channels) = (format.sample_rate, format.channels);

        let task = livekit_runtime.spawn(async move {
            if installed.await.is_err() || task_revoked.load(Ordering::SeqCst) {
                complete_publication(&task_state, &task_drain, true);
                return;
            }
            #[cfg(not(target_arch = "wasm32"))]
            let (track, capture) = build_audio_local_track(
                receiver,
                sample_rate,
                channels,
                task_revoked.clone(),
                task_state.clone(),
            )
            .await;
            #[cfg(target_arch = "wasm32")]
            let track = match build_audio_local_track().await {
                Ok(Ok(track)) => track,
                Ok(Err(err)) => {
                    warn!("microphone creation failed: {err}");
                    complete_publication(&task_state, &task_drain, true);
                    return;
                }
                Err(err) => {
                    warn!("microphone creation was not acknowledged: {err}");
                    complete_publication(&task_state, &task_drain, false);
                    return;
                }
            };
            // Both backends create tracks muted. Publishing itself may outlive cancellation;
            // a canceled publication is never unmuted.
            {
                let mut state = task_state.lock().unwrap_or_else(|err| err.into_inner());
                state.track = Some(track.clone());
            }
            let mut published = false;
            if !task_revoked.load(Ordering::SeqCst) {
                published = participant
                    .publish_track(
                        LocalTrack::Audio(track.clone()),
                        TrackPublishOptions {
                            source: TrackSource::Microphone,
                            ..Default::default()
                        },
                    )
                    .await
                    .is_ok();
                if !published {
                    warn!("microphone publication failed");
                }
            }
            if published {
                {
                    let mut state = task_state.lock().unwrap_or_else(|err| err.into_inner());
                    if !state.revoked {
                        mute_local_track(&track, false);
                        state.transmitting = true;
                    }
                }
                if !task_revoked.load(Ordering::SeqCst) {
                    let _ = cancellation.changed().await;
                }
            }

            // Stop the source before releasing the drain barrier. On web this is an acknowledged
            // page operation, so no later route races an asynchronous old media operation.
            mute_local_track(&track, true);
            #[cfg(not(target_arch = "wasm32"))]
            {
                capture.abort();
                let _ = capture.await;
            }
            #[cfg(target_arch = "wasm32")]
            let stopped = track.stop(!published).await.is_ok();
            #[cfg(not(target_arch = "wasm32"))]
            let stopped = true;
            if published {
                #[cfg(not(target_arch = "wasm32"))]
                let unpublished = participant.unpublish_track(&track.sid()).await;
                #[cfg(target_arch = "wasm32")]
                let unpublished = participant.unpublish_track(&LocalTrack::Audio(track)).await;
                if unpublished.is_err() {
                    warn!("microphone unpublish failed after stopping capture");
                }
            }
            complete_publication(&task_state, &task_drain, stopped);
        });
        MicrophonePublication {
            state,
            revoked,
            cancel,
            drain: drain.clone(),
            task,
        }
        .queue_insert(&mut commands, entity, start);
    }
}

#[cfg(target_arch = "wasm32")]
fn locate_foreign_streams(
    mut streams: Query<(
        &GlobalTransform,
        Option<&RenderLayers>,
        &ForeignAudioSource,
        &ForeignPlayer,
        &mut VoicePan,
    )>,
    participants: Query<(&LivekitParticipant, &Publishing, &HostedBy)>,
    proximity_rooms: Query<(), (With<crate::Transport>, Without<VoiceRoom>)>,
    tracks: Query<
        (&LivekitTrack, Option<&AudioTransmitterVolume>),
        (With<Subscribed>, With<ProximityMicrophone>),
    >,
    pan: VolumePanning,
    settings: Res<AudioSettings>,
    panning_settings: Res<NearbyPanning>,
    mutes: Res<crate::nearby_mutes::NearbyMutes>,
    time: Res<Time>,
) {
    for (emitter_transform, render_layers, source, player, mut voice_pan) in streams.iter_mut() {
        if source.current_transport.is_some() {
            let Some(publishing) = participants
                .iter()
                .filter_map(|(participant, publishing, room)| {
                    if proximity_rooms.contains(room.get())
                        && participant
                            .identity()
                            .as_str()
                            .as_h160()
                            .filter(|h160| *h160 == player.address)
                            .is_some()
                    {
                        Some(publishing.collection().to_vec())
                    } else {
                        None
                    }
                })
                .reduce(|mut accumulator, mut next| {
                    accumulator.append(&mut next);
                    accumulator
                })
            else {
                error!("No participant with address {}.", player.address);
                continue;
            };

            let (volume, panning) =
                pan.volume_and_panning(emitter_transform.translation(), render_layers);
            let volume = if mutes.is_muted(player.address) {
                0.0
            } else {
                volume * settings.voice()
            };
            let panning = voice_pan.update(panning, &panning_settings, time.delta_secs());
            for (livekit_track, multiplier) in tracks.iter_many(publishing) {
                if let Some(track) = livekit_track.track() {
                    track.pan_and_volume(panning, volume * multiplier.map_or(1.0, |value| **value));
                }
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
async fn build_audio_local_track(
    mic_receiver: broadcast::Receiver<LocalAudioFrame>,
    sample_rate: u32,
    num_channels: u32,
    revoked: Arc<AtomicBool>,
    state: Arc<Mutex<PublicationControl>>,
) -> (LocalAudioTrack, JoinHandle<()>) {
    let new_source = NativeAudioSource::new(
        AudioSourceOptions {
            echo_cancellation: true,
            noise_suppression: true,
            auto_gain_control: true,
        },
        sample_rate,
        num_channels,
        50,
    );
    let local_audio_track =
        LocalAudioTrack::create_audio_track("mic", RtcAudioSource::Native(new_source.clone()));
    local_audio_track.mute();

    let local_audio_track_clone = local_audio_track.clone();
    let capture = tokio::task::spawn(async move {
        let mut mic_receiver = mic_receiver;
        while let Ok(frame) = mic_receiver.recv().await {
            if revoked.load(Ordering::SeqCst) || (frame.sample_rate == 0 && frame.num_channels == 0)
            {
                // Termination frame
                break;
            }
            // Keep real samples out of the source until this route has activated transmission.
            if !state
                .lock()
                .unwrap_or_else(|err| err.into_inner())
                .transmitting
            {
                continue;
            }
            if let Err(e) = new_source
                .capture_frame(&AudioFrame {
                    data: Cow::Borrowed(&frame.data),
                    sample_rate: frame.sample_rate,
                    num_channels: frame.num_channels,
                    samples_per_channel: frame.samples_per_channel,
                })
                .await
            {
                warn!("failed to capture from mic: {e}");
            };
        }
        debug!(
            "Mic worker for local audio track {} closed.",
            local_audio_track_clone.sid()
        );
    });

    (local_audio_track, capture)
}

#[cfg(target_arch = "wasm32")]
async fn build_audio_local_track() -> super::web::RoomResult<Result<LocalAudioTrack, String>> {
    LocalAudioTrack::new(AudioCaptureOptions {
        echo_cancellation: Some(true),
        noise_suppression: Some(true),
        auto_gain_control: Some(true),
        ..Default::default()
    })
    .await
}

fn ping_enabled(mut commands: Commands) {
    commands.send_event(SystemAudio(
        "embedded://sounds/ui/voice_chat_mic_on.wav".to_owned(),
    ));
}

fn ping_disabled(mut commands: Commands) {
    commands.send_event(SystemAudio(
        "embedded://sounds/ui/voice_chat_mic_off.wav".to_owned(),
    ));
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod routing_tests {
    use bevy::ecs::world::CommandQueue;
    use web_time::Duration;

    use super::*;

    fn pending(
        runtime: &tokio::runtime::Runtime,
        drain: &MicrophoneDrain,
    ) -> MicrophonePublication {
        let (cancel, _) = tokio::sync::watch::channel(false);
        MicrophonePublication {
            state: Arc::new(Mutex::new(PublicationControl::default())),
            revoked: Arc::new(AtomicBool::new(false)),
            cancel,
            drain: drain.clone(),
            task: runtime.spawn(std::future::pending()),
        }
    }

    #[test]
    fn call_routing_suppresses_nearby_and_waits_for_both_publications_to_stop() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let drain = MicrophoneDrain::default();
        let mic = MicState {
            available: true,
            enabled: true,
        };
        let mut call_mic = CallMic::default();
        let nearby_room = (true, false, false, true);
        let voice_room = (false, false, true, true);
        let nearby = pending(&runtime, &drain);
        let nearby_state = nearby.state.clone();
        assert!(microphone_route_enabled(nearby_room, &mic, &call_mic));
        assert!(!microphone_route_enabled(voice_room, &mic, &call_mic));

        call_mic.enabled = true;
        call_mic.in_call = true;
        call_mic.suppress_nearby = true;
        assert!(nearby.should_remove(microphone_route_enabled(nearby_room, &mic, &call_mic,)));
        assert!(microphone_route_enabled(voice_room, &mic, &call_mic));
        drop(nearby);
        assert_eq!(drain.pending(), 1);
        complete_publication(&nearby_state, &drain, true);
        assert_eq!(drain.pending(), 0);

        let call = pending(&runtime, &drain);
        let call_state = call.state.clone();
        call_mic = CallMic::default();
        assert!(call.should_remove(microphone_route_enabled(voice_room, &mic, &call_mic,)));
        assert!(microphone_route_enabled(nearby_room, &mic, &call_mic));
        drop(call);
        assert_eq!(drain.pending(), 1);
        complete_publication(&call_state, &drain, true);
        assert_eq!(drain.pending(), 0);
        assert!(mic.enabled);
    }

    #[test]
    fn call_mute_and_reconnect_never_restore_nearby_publication() {
        let mic = MicState {
            available: true,
            enabled: true,
        };
        let mut call_mic = CallMic {
            enabled: false,
            in_call: true,
            suppress_nearby: true,
        };
        let nearby_room = (true, false, false, true);
        let voice_room = (false, false, true, true);
        assert!(!microphone_route_enabled(nearby_room, &mic, &call_mic));
        assert!(!microphone_route_enabled(voice_room, &mic, &call_mic));

        call_mic.enabled = true;
        assert!(microphone_route_enabled(voice_room, &mic, &call_mic));
        call_mic.in_call = false;
        assert!(!microphone_route_enabled(
            (false, false, true, false),
            &mic,
            &call_mic,
        ));
        assert!(!microphone_route_enabled(nearby_room, &mic, &call_mic));
        assert!(mic.enabled);
    }

    #[test]
    fn call_publication_uses_its_own_mute_and_never_enables_private_chat() {
        let mic = MicState {
            available: true,
            enabled: false,
        };
        let call_mic = CallMic {
            enabled: true,
            in_call: true,
            suppress_nearby: true,
        };
        assert!(microphone_route_enabled(
            (false, false, true, true),
            &mic,
            &call_mic,
        ));
        assert!(!microphone_route_enabled(
            (false, true, false, true),
            &mic,
            &call_mic,
        ));
        assert!(!microphone_route_enabled(
            (true, false, false, true),
            &mic,
            &CallMic::default(),
        ));
        assert!(!microphone_route_enabled(
            (false, false, false, true),
            &mic,
            &call_mic,
        ));
    }

    #[test]
    fn cancellation_before_creation_blocks_new_route_until_stop_acknowledged() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let drain = MicrophoneDrain::default();
        let publication = pending(&runtime, &drain);
        let state = publication.state.clone();
        let revoked = publication.revoked.clone();
        publication.revoke();
        publication.revoke();
        assert_eq!(drain.pending(), 1);
        drop(publication);
        assert_eq!(drain.pending(), 1);
        assert!(revoked.load(Ordering::SeqCst));
        // A late completion retains revocation; it can never unmute the old route.
        assert!(state.lock().unwrap().revoked);
        complete_publication(&state, &drain, true);
        assert_eq!(drain.pending(), 0);
    }

    #[test]
    fn failed_stop_acknowledgment_keeps_routing_closed() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let drain = MicrophoneDrain::default();
        let publication = pending(&runtime, &drain);
        let state = publication.state.clone();
        drop(publication);
        complete_publication(&state, &drain, false);
        assert_eq!(drain.pending(), 1);
    }

    #[test]
    fn failed_stop_before_revoke_keeps_routing_closed() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let drain = MicrophoneDrain::default();
        let publication = pending(&runtime, &drain);
        complete_publication(&publication.state, &drain, false);
        assert_eq!(drain.pending(), 1);
        drop(publication);
        assert_eq!(drain.pending(), 1);
    }

    #[test]
    fn failed_attempt_retries_after_cooldown_without_a_route_change() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let drain = MicrophoneDrain::default();
        let mut publication = pending(&runtime, &drain);
        let state = publication.state.clone();
        let task_drain = drain.clone();
        publication.task.abort();
        publication.task =
            runtime.spawn(async move { complete_publication(&state, &task_drain, true) });
        runtime.block_on(&mut publication.task).unwrap();
        let stopped_at = publication.state.lock().unwrap().stopped_at.unwrap();
        assert!(publication.retry_state(stopped_at).is_none());
        assert!(publication.should_remove(false));
        assert!(!publication.should_remove(true));
        assert!(publication
            .retry_state(stopped_at + Duration::from_secs(60))
            .is_some());
    }

    #[test]
    fn pending_publication_is_revoked_if_its_host_despawns_before_insertion() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let drain = MicrophoneDrain::default();
        let publication = pending(&runtime, &drain);
        let state = publication.state.clone();
        let cancelled = publication.cancel.subscribe();
        let mut world = World::new();
        let room = world.spawn_empty().id();
        let participant = world.spawn(<HostedBy as Relationship>::from(room)).id();
        let mut queue = CommandQueue::default();
        let mut commands = Commands::new(&mut queue, &world);
        let (start, mut installed) = oneshot::channel();
        commands.entity(room).despawn();
        publication.queue_insert(&mut commands, participant, start);
        queue.apply(&mut world);

        assert!(world.get_entity(participant).is_err());
        assert_eq!(
            installed.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        );
        assert!(*cancelled.borrow());
        assert_eq!(drain.pending(), 1);
        complete_publication(&state, &drain, true);
        assert_eq!(drain.pending(), 0);
    }

    #[test]
    fn publication_starts_after_insertion() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let publication = pending(&runtime, &MicrophoneDrain::default());
        let mut world = World::new();
        let participant = world.spawn_empty().id();
        let mut queue = CommandQueue::default();
        let (start, mut installed) = oneshot::channel();
        publication.queue_insert(&mut Commands::new(&mut queue, &world), participant, start);
        assert_eq!(
            installed.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        );

        queue.apply(&mut world);

        assert!(world.get::<MicrophonePublication>(participant).is_some());
        assert_eq!(installed.try_recv(), Ok(()));
    }

    #[test]
    fn publication_does_not_start_if_a_previous_route_begins_draining_at_flush() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let drain = MicrophoneDrain::default();
        let mut world = World::new();
        let old = world.spawn(pending(&runtime, &drain)).id();
        let new = world.spawn_empty().id();
        let mut queue = CommandQueue::default();
        let mut commands = Commands::new(&mut queue, &world);
        let (start, mut installed) = oneshot::channel();
        commands.entity(old).despawn();
        pending(&runtime, &drain).queue_insert(&mut commands, new, start);
        assert_eq!(drain.pending(), 0);

        queue.apply(&mut world);

        assert!(world.get::<MicrophonePublication>(new).is_none());
        assert_eq!(
            installed.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        );
    }

    #[test]
    fn revocation_disables_a_muted_native_track_reenabled_by_publish() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let drain = MicrophoneDrain::default();
        let publication = pending(&runtime, &drain);
        let (source, receiver) = broadcast::channel::<LocalAudioFrame>(1);
        let (track, capture) = runtime.block_on(build_audio_local_track(
            receiver,
            48_000,
            1,
            publication.revoked.clone(),
            publication.state.clone(),
        ));
        publication.state.lock().unwrap().track = Some(track.clone());
        // Native LiveKit finishes publish by enabling the RTC track while its mute bit is set.
        track.enable();
        assert!(track.is_muted() && track.is_enabled());

        publication.revoke();

        assert!(!track.is_enabled());
        drop(source);
        runtime.block_on(capture).unwrap();
    }

    #[test]
    fn completed_publication_does_not_leave_a_phantom_drain() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let drain = MicrophoneDrain::default();
        let publication = pending(&runtime, &drain);
        complete_publication(&publication.state, &drain, true);
        drop(publication);
        assert_eq!(drain.pending(), 0);
    }
}
