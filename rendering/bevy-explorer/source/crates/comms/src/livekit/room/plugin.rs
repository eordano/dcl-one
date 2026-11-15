use alloy_core::primitives::Address;
use bevy::prelude::*;
use common::{debug_panic, structs::AudioDecoderError, util::AsH160};
use tokio::sync::{mpsc, oneshot};
#[cfg(not(target_arch = "wasm32"))]
use {
    kira::sound::streaming::StreamingSoundData,
    livekit::{
        id::ParticipantIdentity, participant::Participant, ConnectionState, DataPacket,
        DisconnectReason, Room, RoomEvent, RoomOptions,
    },
};

use crate::{
    global_crdt::ChannelControl,
    livekit::{
        participant::{
            ActiveSpeakersChanged, HostingParticipants, LivekitParticipant, ParticipantConnected,
            ParticipantConnectionQuality, ParticipantDisconnected, ParticipantMetadataChanged,
            ParticipantPayload,
        },
        room::{
            Connected, Connecting, ConnectingLivekitRoom, Disconnected, LivekitRoom, Reconnecting,
            UnannouncedParticipants,
        },
        track, ConnectionAvailability, LivekitChannelControl, LivekitNetworkMessage,
        LivekitRuntimeRes, LivekitTransport,
    },
    room_connection::{
        AdmissionError, CredentialRequest, FailureKind, NeedsAdapter, RoomAccessScope,
        RoomRecovery, RoomSuspended,
    },
    voice_chat::{VoiceRoom, VoiceRoomDisconnected},
    NetworkMessageRecipient,
};
#[cfg(target_arch = "wasm32")]
use crate::{
    global_crdt::StreamingSoundData,
    livekit::web::{
        ConnectionState, DataPacket, DisconnectReason, Participant, ParticipantIdentity, Room,
        RoomEvent, RoomOptions,
    },
};

pub struct LivekitRoomPlugin;

impl Plugin for LivekitRoomPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(super::control::RoomControlPlugin);
        app.add_observer(livekit_transport_added);
        app.add_observer(initiate_room_connection);
        app.add_observer(create_local_participant);

        app.add_systems(
            Update,
            (
                try_reconnect.run_if(not(in_state(ConnectionAvailability::Unavailable))),
                poll_connecting_rooms,
                (
                    process_room_events,
                    process_channel_control,
                    process_network_message,
                ),
                verify_room_tasks,
            )
                .chain(),
        );
        app.add_systems(Last, close_rooms_on_app_exit.run_if(on_event::<AppExit>));
    }
}

fn livekit_transport_added(trigger: Trigger<OnAdd, LivekitTransport>, mut commands: Commands) {
    let entity = trigger.target();
    commands.entity(entity).try_insert(Connecting);
}

fn initiate_room_connection(
    trigger: Trigger<OnAdd, Connecting>,
    mut commands: Commands,
    mut livekit_transports: Query<(&LivekitTransport, &mut RoomRecovery)>,
    paused: Query<(), With<RoomSuspended>>,
    livekit_runtime: LivekitRuntimeRes,
    connection_availability: Res<State<ConnectionAvailability>>,
) {
    let entity = trigger.target();
    if paused.contains(entity) {
        debug!("Can't connect because the room is paused.");
        return;
    }
    let Ok((livekit_transport, mut recovery)) = livekit_transports.get_mut(entity) else {
        error!("{entity} does not have a LivekitRuntime.");
        return;
    };
    if *connection_availability.get() == ConnectionAvailability::Unavailable {
        debug!("Can't connect because new connections are disabled.");
        recovery.failure = Some(FailureKind::IdentityConflict);
        commands.entity(trigger.target()).try_remove::<Connecting>();
        return;
    }

    let remote_address = &livekit_transport.address;

    let Ok((address, token)) = crate::adapter::livekit_credentials(remote_address) else {
        recovery.failure = Some(FailureKind::Invalid);
        commands.entity(entity).try_remove::<Connecting>();
        return;
    };

    recovery.failure = None;
    commands.entity(entity).try_insert(ConnectingLivekitRoom(
        livekit_runtime.spawn(connect_to_room(address, token)),
    ));
}

fn poll_connecting_rooms(
    mut commands: Commands,
    livekit_rooms: Populated<(
        Entity,
        &mut ConnectingLivekitRoom,
        &mut RoomRecovery,
        Has<CredentialRequest>,
        Has<crate::archipelago::IslandOwner>,
    )>,
    livekit_runtime: LivekitRuntimeRes,
    time: Res<Time>,
) {
    for (entity, mut connecting_livekit_room, mut recovery, refreshable, island) in
        livekit_rooms.into_inner()
    {
        if connecting_livekit_room.is_finished() {
            let poll = livekit_runtime
                .block_on(connecting_livekit_room.as_deref_mut().as_mut())
                .unwrap_or_else(|_| Err(AdmissionError::new(FailureKind::Transient)));

            match poll {
                Ok(room) => {
                    recovery.failure = None;
                    commands
                        .entity(entity)
                        .try_insert(room)
                        .try_remove::<ConnectingLivekitRoom>();
                }
                Err(err) => {
                    let err = if err.kind == FailureKind::Denied {
                        AdmissionError::new(if refreshable || island {
                            FailureKind::Transient
                        } else {
                            FailureKind::CredentialsRequired
                        })
                    } else {
                        err
                    };
                    warn!(?entity, kind = ?err.kind, "room connection failed");
                    recovery.fail(time.elapsed_secs_f64(), err);
                    commands
                        .entity(entity)
                        .try_remove::<(ConnectingLivekitRoom, Connecting)>();
                }
            }
        }
    }
}

#[allow(clippy::result_large_err)] // RoomError is livekit's, and connect failures are rare
async fn connect_to_room(address: String, token: String) -> Result<LivekitRoom, AdmissionError> {
    let mut options = RoomOptions::default();
    options.auto_subscribe = false;
    options.adaptive_stream = false;
    options.dynacast = false;
    Room::connect(&address, &token, options)
        .await
        .map(|(room, receiver)| LivekitRoom::new(room, receiver))
        .map_err(classify_connect_error)
}

#[cfg(not(target_arch = "wasm32"))]
fn classify_connect_error(error: livekit::RoomError) -> AdmissionError {
    if let livekit::RoomError::Engine(livekit::rtc_engine::EngineError::Signal(
        livekit_signaling::SignalError::Client(status, _),
    )) = error
    {
        return AdmissionError::status(status.as_u16(), None);
    }
    AdmissionError::new(FailureKind::Transient)
}

#[cfg(target_arch = "wasm32")]
fn classify_connect_error(error: crate::livekit::web::RoomError) -> AdmissionError {
    error
        .status
        .map_or(AdmissionError::new(FailureKind::Transient), |status| {
            AdmissionError::status(status, None)
        })
}

/// Parses the scene room's metadata into its moderation lists. Unreadable metadata keeps the
/// lists the room had, or gives it empty ones.
fn apply_scene_room_metadata(commands: &mut Commands, entity: Entity, metadata: &str) {
    match crate::SceneRoomModeration::from_metadata(metadata) {
        Ok(moderation) => {
            debug!(
                "scene room metadata: {} banned, {} admins",
                moderation.banned_addresses.len(),
                moderation.scene_admins.len()
            );
            commands.entity(entity).try_insert(moderation);
        }
        Err(err) => {
            warn!("scene room metadata unreadable ({err:?}): {metadata}");
            commands
                .entity(entity)
                .try_insert_if_new(crate::SceneRoomModeration::default());
        }
    }
}

/// Marker for a room that hit a terminal disconnect (kicked / identity conflict) while
/// running as an authoritative server: no in-process reconnect is possible (the token is
/// spent); the supervisor must tear the transport down and re-mint.
#[derive(Component)]
pub struct ServerRoomTerminal;

/// How long a sender may stay unannounced after its first data before its presence is withdrawn
/// (LiveKit announces a joiner within a couple of seconds of its data channel opening).
const UNANNOUNCED_GRACE_SECS: f64 = 15.0;

#[expect(clippy::type_complexity)]
fn process_room_events(
    mut commands: Commands,
    livekit_rooms: Query<(
        Entity,
        &mut LivekitRoom,
        &mut UnannouncedParticipants,
        &mut RoomRecovery,
        Option<&RoomAccessScope>,
        Has<crate::archipelago::IslandOwner>,
        Has<crate::SceneRoom>,
        Has<VoiceRoom>,
    )>,
    time: Res<Time>,
) {
    for (
        entity,
        mut livekit_room,
        mut unannounced,
        mut recovery,
        scope,
        island,
        scene_room,
        is_voice_room,
    ) in livekit_rooms
    {
        while let Ok(room_event) = livekit_room.room_event_receiver.try_recv() {
            trace!("in: {:?}", room_event);

            match room_event {
                RoomEvent::Connected {
                    participants_with_tracks,
                } => {
                    if scene_room {
                        apply_scene_room_metadata(&mut commands, entity, &livekit_room.metadata());
                    }
                    for (participant, publications) in participants_with_tracks {
                        unannounced.remove(participant.identity().as_str());
                        commands.trigger(ParticipantConnected {
                            participant: participant.clone().into(),
                            room: entity,
                        });
                        for publication in &publications {
                            commands.trigger(track::TrackPublished {
                                participant: Participant::Remote(participant.clone()),
                                track: publication.clone(),
                            });
                        }
                    }
                }
                RoomEvent::Disconnected { reason } => {
                    if is_voice_room {
                        recovery
                            .retry
                            .observe_connection(false, time.elapsed_secs_f64());
                        commands
                            .entity(entity)
                            .try_insert(VoiceRoomDisconnected::new(&reason));
                    } else {
                        record_disconnect(
                            &mut commands,
                            entity,
                            &mut recovery,
                            scope,
                            reason,
                            island,
                            time.elapsed_secs_f64(),
                        );
                    }
                }
                RoomEvent::ConnectionStateChanged(state) => match state {
                    ConnectionState::Connected => {
                        recovery.failure = None;
                        commands.entity(entity).try_insert(Connected);
                    }
                    ConnectionState::Reconnecting => {
                        recovery
                            .retry
                            .observe_connection(false, time.elapsed_secs_f64());
                        commands.entity(entity).try_insert(Reconnecting);
                    }
                    ConnectionState::Disconnected => {
                        if recovery.failure.is_none() {
                            recovery.fail(
                                time.elapsed_secs_f64(),
                                AdmissionError::new(FailureKind::Transient),
                            );
                        }
                        livekit_room.closed = true;
                        commands.entity(entity).try_insert(Disconnected);
                    }
                },
                RoomEvent::DataReceived {
                    payload,
                    topic,
                    participant,
                    participant_identity,
                    ..
                } => {
                    livekit_room.traffic.record_received(payload.len());
                    let identity = participant_identity
                        .filter(|identity| !identity.as_str().is_empty())
                        .or_else(|| participant.as_ref().map(|p| p.identity()));
                    if let Some(identity) = identity {
                        if participant.is_none() {
                            unannounced
                                .entry(identity.as_str().to_owned())
                                .or_insert(time.elapsed_secs_f64() + UNANNOUNCED_GRACE_SECS);
                        }
                        commands.trigger(ParticipantPayload {
                            room: entity,
                            identity,
                            payload,
                            topic,
                        });
                    } else {
                        debug!("Owner-less payload received.");
                    }
                }
                RoomEvent::ParticipantConnected(participant) => {
                    unannounced.remove(participant.identity().as_str());
                    commands.trigger(ParticipantConnected {
                        participant: participant.clone().into(),
                        room: entity,
                    });
                }
                RoomEvent::ParticipantDisconnected(participant) => {
                    commands.trigger(ParticipantDisconnected {
                        participant: participant.into(),
                        room: entity,
                    });
                }
                RoomEvent::ParticipantMetadataChanged { participant, .. } => {
                    commands.trigger(ParticipantMetadataChanged {
                        room: entity,
                        participant: participant.into(),
                    });
                }
                RoomEvent::RoomMetadataChanged { metadata, .. } => {
                    if scene_room {
                        apply_scene_room_metadata(&mut commands, entity, &metadata);
                    }
                }
                RoomEvent::TrackPublished {
                    publication,
                    participant,
                } => {
                    commands.trigger(track::TrackPublished {
                        participant: Participant::Remote(participant.clone()),
                        track: publication.clone(),
                    });
                }
                RoomEvent::TrackUnpublished {
                    publication,
                    participant,
                } => {
                    commands.trigger(track::TrackUnpublished {
                        participant: Participant::Remote(participant.clone()),
                        track: publication.clone(),
                    });
                }
                RoomEvent::TrackSubscribed { publication, .. } => {
                    commands.trigger(track::TrackSubscribed { track: publication });
                }
                RoomEvent::TrackUnsubscribed { publication, .. } => {
                    commands.trigger(track::TrackUnsubscribed { track: publication });
                }
                RoomEvent::ConnectionQualityChanged {
                    quality,
                    participant,
                } => {
                    commands.trigger(ParticipantConnectionQuality::new(
                        participant.into(),
                        entity,
                        quality,
                    ));
                }
                RoomEvent::ActiveSpeakersChanged { speakers } => {
                    commands.trigger_targets(ActiveSpeakersChanged { speakers }, entity);
                }
                #[cfg(not(target_arch = "wasm32"))]
                _ => {
                    debug!("Event: {:?}", room_event);
                }
            };
        }
    }
}

fn process_channel_control(
    mut commands: Commands,
    rooms: Query<(Entity, &LivekitRoom, &mut LivekitChannelControl)>,
) {
    for (entity, livekit_room, mut channel_control) in rooms {
        loop {
            match channel_control.try_recv() {
                Ok(channel_control) => {
                    match channel_control {
                        ChannelControl::VoiceSubscribe(address, sender) => {
                            commands.run_system_cached_with(
                                subscribe_to_voice,
                                (entity, address, sender),
                            );
                        }
                        ChannelControl::VoiceUnsubscribe(address) => {
                            commands
                                .run_system_cached_with(unsubscribe_to_voice, (entity, address));
                        }
                    };
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    debug_panic!("Channel control of {} was closed.", livekit_room.name());
                }
            }
        }
    }
}

/// Outgoing messages are published only while the room is connected; otherwise they wait in
/// the transport's bounded channel and go out once it is (again).
#[allow(clippy::type_complexity)]
fn process_network_message(
    rooms: Query<
        (&mut LivekitRoom, &mut LivekitNetworkMessage),
        (With<Connected>, Without<RoomSuspended>),
    >,
    livekit_runtime: LivekitRuntimeRes,
) {
    for (mut room, mut network_message) in rooms {
        loop {
            match network_message.try_recv() {
                Ok(outgoing) => {
                    let Some(payload) = outgoing.message.to_rfc4() else {
                        continue;
                    };
                    let destination_identities = match outgoing.recipient {
                        NetworkMessageRecipient::All => Vec::default(),
                        NetworkMessageRecipient::Peer(address) => {
                            vec![ParticipantIdentity(format!("{address:#x}"))]
                        }
                        NetworkMessageRecipient::AuthServer => {
                            vec![ParticipantIdentity("authoritative-server".to_string())]
                        }
                        NetworkMessageRecipient::MessageRouter => {
                            vec![ParticipantIdentity(
                                crate::community_chat::message_router_identity(),
                            )]
                        }
                    };

                    let size = payload.len();
                    let packet = DataPacket {
                        payload,
                        topic: outgoing.topic,
                        reliable: !outgoing.unreliable,
                        destination_identities,
                    };

                    let local_participant = room.local_participant();
                    room.publishes.push(livekit_runtime.spawn(async move {
                        local_participant.publish_data(packet).await.map(|()| size)
                    }));
                }
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    debug_panic!("Network message of {} was closed.", room.name());
                }
            }
        }
    }
}

fn create_local_participant(
    trigger: Trigger<OnAdd, Connected>,
    mut commands: Commands,
    rooms: Query<&LivekitRoom>,
) {
    let entity = trigger.target();
    let Ok(room) = rooms.get(entity) else {
        debug_panic!("Can't create local participant because {entity} is not a LivekitRoom.");
    };

    let local_participant = room.local_participant();
    commands.trigger(ParticipantConnected {
        participant: local_participant.into(),
        room: entity,
    });
}

type SubscribeToAudio = (
    Entity,
    Address,
    oneshot::Sender<StreamingSoundData<AudioDecoderError>>,
);

fn subscribe_to_voice(
    In(input): In<SubscribeToAudio>,
    mut commands: Commands,
    rooms: Query<(&LivekitRoom, Option<&HostingParticipants>)>,
    participants: Query<(&LivekitParticipant, &track::Publishing)>,
    tracks: Query<Entity, With<track::Microphone>>,
) {
    #[cfg(not(target_arch = "wasm32"))]
    let (room_entity, address, sender) = input;
    #[cfg(target_arch = "wasm32")]
    let (room_entity, address, _) = input;

    let Ok((room, maybe_hosting)) = rooms.get(room_entity) else {
        debug_panic!("{} is not an well formed room.", room_entity);
    };

    let Some(hosting) = maybe_hosting else {
        error!(
            "Trying to subscribe to voice in room {}, but there are not participants.",
            room.name()
        );
        return;
    };

    let Some((participant, publishing)) =
        participants
            .iter_many(hosting.collection())
            .find(|(participant, _)| {
                participant
                    .identity()
                    .as_str()
                    .as_h160()
                    .filter(|participant_address| participant_address == &address)
                    .is_some()
            })
    else {
        error!(
            "No participant with address {} in room {}.",
            address,
            room.name()
        );
        return;
    };

    if let Some(track_entity) = tracks.iter_many(publishing.collection()).next() {
        commands.trigger_targets(
            track::SubscribeToAudioTrack {
                #[cfg(not(target_arch = "wasm32"))]
                sender,
            },
            track_entity,
        );
    } else {
        error!(
            "No microphone track for {} ({}).",
            participant.sid(),
            participant.identity()
        );
    }
}

fn unsubscribe_to_voice(
    In((room_entity, address)): In<(Entity, Address)>,
    mut commands: Commands,
    rooms: Query<(&LivekitRoom, Option<&HostingParticipants>)>,
    participants: Query<(&LivekitParticipant, &track::Publishing)>,
    tracks: Query<Entity, With<track::Microphone>>,
) {
    let Ok((room, maybe_hosting)) = rooms.get(room_entity) else {
        debug_panic!("{} is not an well formed room.", room_entity);
    };

    let Some(hosting) = maybe_hosting else {
        error!(
            "Trying to subscribe to voice in room {}, but there are not participants.",
            room.name()
        );
        return;
    };

    let Some((participant, publishing)) =
        participants
            .iter_many(hosting.collection())
            .find(|(participant, _)| {
                participant
                    .identity()
                    .as_str()
                    .as_h160()
                    .filter(|participant_address| participant_address == &address)
                    .is_some()
            })
    else {
        error!(
            "No participant with address {} in room {}.",
            address,
            room.name()
        );
        return;
    };

    if let Some(track_entity) = tracks.iter_many(publishing.collection()).next() {
        commands.trigger_targets(track::UnsubscribeToTrack, track_entity);
    } else {
        error!(
            "No microphone track for {} ({}).",
            participant.sid(),
            participant.identity()
        );
    }
}

fn verify_room_tasks(rooms: Query<&mut LivekitRoom>, livekit_runtime: LivekitRuntimeRes) {
    for mut room in rooms {
        let mut i = 0;
        while i < room.publishes.len() {
            if room.publishes[i].is_finished() {
                let task = room.publishes.swap_remove(i);

                let res = livekit_runtime.block_on(task);
                match res {
                    Ok(Ok(size)) => room.traffic.record_sent(size),
                    Ok(Err(err)) => {
                        error!("Failed to complete room task due to {err}.");
                    }
                    Err(err) => {
                        debug_panic!("Failed to pull RoomTask due to '{err}'.");
                    }
                }
            } else {
                i += 1;
            }
        }
    }
}

fn close_rooms_on_app_exit(rooms: Query<&LivekitRoom>, livekit_runtime: LivekitRuntimeRes) {
    for room in rooms {
        if let Err(err) = livekit_runtime.block_on(room.close()) {
            error!(
                "Failed to close room {} before exiting due to '{err}'.",
                room.name()
            );
        }
    }
}

fn record_disconnect(
    commands: &mut Commands,
    entity: Entity,
    recovery: &mut RoomRecovery,
    scope: Option<&RoomAccessScope>,
    reason: DisconnectReason,
    island: bool,
    now: f64,
) {
    recovery.retry.observe_connection(false, now);
    if matches!(
        reason,
        DisconnectReason::DuplicateIdentity | DisconnectReason::ParticipantRemoved
    ) {
        if common::structs::server_mode() {
            // The headless supervisor reaps the room and obtains a fresh adapter.
            commands.entity(entity).try_insert(ServerRoomTerminal);
        } else if reason == DisconnectReason::DuplicateIdentity {
            record_identity_conflict(commands, recovery, scope);
        } else if island {
            // ConnectionStateChanged can already have started the backoff.
            if recovery.failure != Some(FailureKind::Transient) {
                recovery.fail(now, AdmissionError::new(FailureKind::Transient));
            }
        } else {
            recovery.failure = Some(FailureKind::Denied);
        }
    }
}

fn record_identity_conflict(
    commands: &mut Commands,
    recovery: &mut RoomRecovery,
    scope: Option<&RoomAccessScope>,
) {
    recovery.failure = Some(FailureKind::IdentityConflict);
    if !matches!(
        scope.map(|scope| &scope.0),
        Some(crate::access_status::AccessScope::PrivateChat)
    ) {
        commands.set_state(ConnectionAvailability::Unavailable);
    }
}

#[cfg(test)]
mod disconnect_tests {
    use super::*;
    use crate::access_status::AccessScope;
    use crate::archipelago::{IslandOwner, RefreshIsland};
    use bevy::ecs::system::RunSystemOnce;

    #[test]
    fn participant_removed_from_island_requests_a_fresh_assignment() {
        let mut world = World::new();
        let owner = world.spawn_empty().id();
        let island = world
            .spawn((
                IslandOwner(owner),
                LivekitTransport {
                    address: String::new(),
                },
            ))
            .id();
        let mut recovery = RoomRecovery::default();
        record_disconnect(
            &mut world.commands(),
            island,
            &mut recovery,
            None,
            DisconnectReason::ParticipantRemoved,
            true,
            0.0,
        );
        world.entity_mut(island).insert(recovery);
        world.init_resource::<Time>();
        world.run_system_once(try_reconnect).unwrap();
        assert!(world.get::<RefreshIsland>(owner).is_none());
        world
            .resource_mut::<Time>()
            .advance_by(std::time::Duration::from_secs(5));
        world.run_system_once(try_reconnect).unwrap();
        assert!(world.get::<RefreshIsland>(owner).is_some());
        assert!(world.get::<RoomSuspended>(island).is_some());
    }

    #[test]
    fn participant_removed_from_a_credentialed_room_stays_denied() {
        let mut world = World::new();
        let room = world
            .spawn((
                CredentialRequest {
                    method: "POST",
                    url: "https://rooms.example/voice".into(),
                    metadata: "{}".into(),
                },
                LivekitTransport {
                    address: String::new(),
                },
            ))
            .id();
        let mut recovery = RoomRecovery::default();
        record_disconnect(
            &mut world.commands(),
            room,
            &mut recovery,
            Some(&RoomAccessScope(AccessScope::PrivateChat)),
            DisconnectReason::ParticipantRemoved,
            false,
            0.0,
        );
        assert_eq!(recovery.failure, Some(FailureKind::Denied));
        world.entity_mut(room).insert(recovery);
        world.init_resource::<Time>();
        world
            .resource_mut::<Time>()
            .advance_by(std::time::Duration::from_secs(5));
        world.run_system_once(try_reconnect).unwrap();
        assert!(world.get::<NeedsAdapter>(room).is_none());
    }

    #[test]
    fn a_private_identity_conflict_stops_only_its_room() {
        let mut world = World::new();
        world.init_resource::<NextState<ConnectionAvailability>>();
        let mut recovery = RoomRecovery::default();
        record_identity_conflict(
            &mut world.commands(),
            &mut recovery,
            Some(&RoomAccessScope(AccessScope::PrivateChat)),
        );
        world.flush();
        assert_eq!(recovery.failure, Some(FailureKind::IdentityConflict));
        assert!(!recovery.can_retry(60.0));
        assert!(matches!(
            world.resource::<NextState<ConnectionAvailability>>(),
            NextState::Unchanged
        ));
    }

    #[test]
    fn a_world_identity_conflict_keeps_the_session_gate() {
        for scope in [
            None,
            Some(AccessScope::Realm("realm".into())),
            Some(AccessScope::Scene("scene".into())),
        ] {
            let mut world = World::new();
            world.init_resource::<NextState<ConnectionAvailability>>();
            let mut recovery = RoomRecovery::default();
            record_identity_conflict(
                &mut world.commands(),
                &mut recovery,
                scope.map(RoomAccessScope).as_ref(),
            );
            world.flush();
            assert_eq!(recovery.failure, Some(FailureKind::IdentityConflict));
            assert!(matches!(
                world.resource::<NextState<ConnectionAvailability>>(),
                NextState::Pending(ConnectionAvailability::Unavailable)
            ));
        }
    }
}

fn try_reconnect(
    mut commands: Commands,
    rooms: Query<
        (
            Entity,
            &RoomRecovery,
            Has<Disconnected>,
            Has<CredentialRequest>,
            Option<&crate::archipelago::IslandOwner>,
        ),
        (
            With<LivekitTransport>,
            Without<Connecting>,
            Without<NeedsAdapter>,
            Without<RoomSuspended>,
            Without<VoiceRoom>,
        ),
    >,
    time: Res<Time>,
) {
    if common::structs::multi_tenant() {
        // orchestrated server: the adapter's token is long expired (the gatekeeper mints
        // them with a five-minute TTL), so a re-dial can only be refused. The room stays
        // Disconnected and the headless bin asks the orchestrator for a fresh adapter.
        return;
    }
    for (entity, recovery, disconnected, refreshable, island) in &rooms {
        if (!disconnected && recovery.failure.is_none())
            || !recovery.can_retry(time.elapsed_secs_f64())
        {
            continue;
        }
        restart_room(
            &mut commands,
            entity,
            refreshable,
            island.map(|island| island.0),
        );
    }
}

/// Manual resume and automatic recovery use the same credential source.
pub(super) fn restart_room(
    commands: &mut Commands,
    entity: Entity,
    refreshable: bool,
    island: Option<Entity>,
) {
    if let Some(island) = island {
        if let Ok(mut owner) = commands.get_entity(island) {
            owner.try_insert(crate::archipelago::RefreshIsland);
            commands.entity(entity).try_insert(RoomSuspended);
        }
    } else if refreshable {
        commands.entity(entity).try_insert(NeedsAdapter);
    } else {
        commands.entity(entity).try_insert(Connecting);
    }
}
