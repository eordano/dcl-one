use std::collections::{BTreeSet, HashSet};

use bevy::prelude::*;
use bevy_console::ConsoleCommand;
use common::util::AsH160;
use console::{AddDebugSection, DebugInput, DebugInputKind, DebugSection, DoAddConsoleCommand};
use serde_json::json;

use super::{
    control::{client_can_control, room_control_block, RoomConnections, RoomKind},
    LivekitRoom,
};
use crate::{
    global_crdt::ForeignPlayer,
    livekit::{
        participant::{ConnectionQuality, HostedBy, LivekitParticipant, Local, ParticipantIndex},
        ConnectionAvailability, LivekitTransport,
    },
    private_chat::PrivateChatRoom,
    room_connection::{RoomState, RoomStatus},
    SceneRoom, Transport, TransportType,
};
use bevy::ecs::relationship::Relationship;

pub(in crate::livekit) struct RoomDiagnosticsPlugin;

impl Plugin for RoomDiagnosticsPlugin {
    fn build(&self, app: &mut App) {
        app.add_console_command::<RoomDiagnostics, _>(room_diagnostics);
        let client = client_can_control(
            app.world()
                .contains_resource::<common::structs::NoRenderApp>(),
        );
        if client {
            app.add_console_command::<RoomConnectionCommand, _>(room_connection);
        }
        for (title, scope) in [
            ("Island room", "island"),
            ("Scene room", "scene"),
            ("Private chat room", "chat"),
        ] {
            let mut fields = vec![
                ("Connection policy", "policy"),
                ("Room", "room.name"),
                ("Connection state", "room.state"),
                #[cfg(not(target_arch = "wasm32"))]
                ("Room SID", "room.sid"),
                ("Dial attempts", "room.attempts"),
                ("Self SID", "room.selfSid"),
                ("Local connection quality", "room.quality"),
                ("Remote participants", "room.remoteCount"),
            ];
            if scope == "scene" {
                fields.extend([
                    ("Scene ID", "room.scene"),
                    ("Associated base parcel (X / Y)", "room.baseParcelPosition"),
                ]);
            }
            let command = format!("/room_connection {scope} {{value}}");
            let inputs = [DebugInput {
                label: "Connection",
                field: "room.connectionEnabled",
                command: &command,
                kind: DebugInputKind::ActionButton {
                    enable: ("Connect", "connect"),
                    disable: ("Disconnect", "disconnect"),
                },
            }];
            app.add_debug_section(DebugSection {
                title,
                query: &format!("/room_diagnostics {scope}"),
                fields: &fields,
                actions: &[],
                inputs: if client { &inputs } else { &[] },
            });
        }
        let indicators = app.is_plugin_added::<bevy::gizmos::GizmoPlugin>()
            && !app
                .world()
                .contains_resource::<common::structs::NoRenderApp>();
        if indicators {
            app.init_resource::<RoomIndicators>()
                .add_console_command::<RoomIndicatorsCommand, _>(room_indicators)
                .add_systems(
                    PostUpdate,
                    draw_room_indicators.run_if(|enabled: Res<RoomIndicators>| enabled.0),
                );
        }
        app.add_debug_section(DebugSection {
            title: "Room info",
            query: "/room_diagnostics info",
            fields: &[
                ("Active remote players", "players"),
                ("Players on LiveKit", "onLivekit"),
                ("Players off LiveKit", "offLivekit"),
                ("Participants without players", "unmatched"),
            ],
            actions: &[],
            inputs: if indicators {
                &[DebugInput {
                    label: "Show room markers (gold Pulse, green island, blue scene)",
                    field: "indicators",
                    command: "/room_indicators {value}",
                    kind: DebugInputKind::Toggle,
                }]
            } else {
                &[]
            },
        })
        .add_debug_section(DebugSection {
            title: "Room throughput",
            query: "/room_diagnostics throughput",
            fields: &[
                ("Island received (payload B/s)", "island.received"),
                ("Island published (payload B/s)", "island.sent"),
                ("Island participant quality (first 32)", "island.peers"),
                ("Scene received (payload B/s)", "scene.received"),
                ("Scene published (payload B/s)", "scene.sent"),
                ("Scene participant quality (first 32)", "scene.peers"),
            ],
            actions: &[],
            inputs: &[],
        });
    }
}

#[derive(clap::ValueEnum, Clone, Copy, PartialEq, Eq)]
enum Scope {
    Island,
    Scene,
    Chat,
    Info,
    Throughput,
}

impl Scope {
    fn room_kind(self) -> Option<RoomKind> {
        match self {
            Self::Island => Some(RoomKind::Island),
            Self::Scene => Some(RoomKind::Scene),
            Self::Chat => Some(RoomKind::Chat),
            Self::Info | Self::Throughput => None,
        }
    }
}

/// Inspect LiveKit room lifecycle, participant membership and data-channel throughput.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/room_diagnostics")]
struct RoomDiagnostics {
    scope: Scope,
}

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn room_diagnostics(
    mut input: ConsoleCommand<RoomDiagnostics>,
    policy: Res<State<ConnectionAvailability>>,
    no_render: Option<Res<common::structs::NoRenderApp>>,
    rooms: Query<
        (
            Entity,
            Option<&SceneRoom>,
            Has<PrivateChatRoom>,
            Has<Transport>,
            Option<&LivekitRoom>,
            RoomStatus,
        ),
        With<LivekitTransport>,
    >,
    participants: Query<(
        &LivekitParticipant,
        &HostedBy,
        Has<Local>,
        Option<&ConnectionQuality>,
    )>,
    players: Query<&ForeignPlayer>,
    indicators: Option<Res<RoomIndicators>>,
    definitions: Option<Res<Assets<ipfs::EntityDefinition>>>,
) {
    let Some(Ok(command)) = input.take() else {
        return;
    };
    if command.scope == Scope::Info {
        let players: HashSet<_> = players.iter().map(|player| player.address).collect();
        let members: HashSet<_> = participants
            .iter()
            .filter(|(_, hosted, local, _)| {
                !*local
                    && rooms
                        .get(hosted.get())
                        .is_ok_and(|(_, _, _, transport, ..)| transport)
            })
            .filter_map(|(p, ..)| p.identity().as_str().as_h160())
            .collect();
        input.reply_ok(json!({"players": players.len(), "onLivekit": players.intersection(&members).count(),
            "offLivekit": players.difference(&members).count(), "unmatched": members.difference(&players).count(),
            "indicators": indicators.map(|s| s.0)}).to_string());
        return;
    }

    let unavailable = match command.scope {
        Scope::Island => json!({"room": "No LiveKit island room assigned"}),
        Scope::Scene => json!({"room": "No LiveKit scene room assigned"}),
        Scope::Chat => json!({"room": "No private chat room assigned"}),
        Scope::Throughput => json!({
            "island": "No LiveKit island room assigned",
            "scene": "No LiveKit scene room assigned",
        }),
        Scope::Info => unreachable!(),
    };
    let mut result =
        json!({"policy": format!("{:?}", policy.get()), "room": null, "unavailable": unavailable});
    for (entity, scene, private, transport, room, status) in &rooms {
        let state = status.state();
        let connected = state == RoomState::Connected;
        let paused = status.suspended;
        let failed = status.recovery.failure.is_some();
        let Some(kind) = RoomKind::for_room(scene.is_some(), private, transport) else {
            continue;
        };
        if Some(kind) != command.scope.room_kind()
            && (command.scope != Scope::Throughput || private)
        {
            continue;
        }
        if command.scope != Scope::Throughput && !result["room"].is_null() {
            result["room"]["connectionEnabled"] = serde_json::Value::Null;
            result["unavailable"]["room.connectionEnabled"] = json!("More than one room matches");
            break;
        }
        let mut remote_count = 0;
        let mut peer_quality = BTreeSet::new();
        let mut local_sid = None;
        let mut local_quality = None;
        for (participant, _, local, _quality) in participants
            .iter()
            .filter(|(_, hosted, ..)| hosted.get() == entity)
        {
            // The native SDK retains current quality even if it hasn't changed since joining.
            #[cfg(not(target_arch = "wasm32"))]
            let quality = Some(format!("{:?}", participant.connection_quality()));
            #[cfg(target_arch = "wasm32")]
            let quality = _quality.map(|q| format!("{q:?}"));
            if local {
                local_sid = Some(participant.sid().to_string());
                local_quality = quality;
            } else {
                remote_count += 1;
                if command.scope == Scope::Throughput {
                    peer_quality.insert(format!(
                        "{} ({})",
                        participant.sid(),
                        quality.as_deref().unwrap_or("unknown")
                    ));
                    if peer_quality.len() > 32 {
                        peer_quality.pop_last();
                    }
                }
            }
        }
        if command.scope == Scope::Throughput {
            let key = if kind == RoomKind::Scene {
                "scene"
            } else {
                "island"
            };
            let traffic = room.map(|room| room.traffic.last_second());
            result[key] = json!({"sent": traffic.map(|t| t.sent.bytes), "received": traffic.map(|t| t.received.bytes), "peers": peer_quality});
            if traffic.is_none() {
                result["unavailable"][key] = json!("Room has not connected");
            } else {
                result["unavailable"].as_object_mut().unwrap().remove(key);
            }
        } else {
            #[cfg(not(target_arch = "wasm32"))]
            let sid = room.and_then(|r| r.maybe_sid()).map(|sid| sid.to_string());
            #[cfg(target_arch = "wasm32")]
            let sid: Option<String> = None;
            let base = scene.and_then(|scene| {
                definitions
                    .as_deref()?
                    .iter()
                    .find(|(_, def)| def.id == scene.0)?
                    .1
                    .metadata
                    .as_ref()?
                    .pointer("/scene/base")?
                    .as_str()
            });
            let base_position = base.and_then(|base| {
                let (x, y) = base.split_once(',')?;
                Some([x.trim().parse::<i32>().ok()?, y.trim().parse::<i32>().ok()?])
            });
            result["room"] = json!({"state": state.name(),
                "name": room.map(|r| r.name().to_string()), "sid": sid, "scene": scene.map(|s| s.0.as_str()),
                "baseParcel": base, "baseParcelPosition": base_position, "attempts": status.recovery.retry.attempts,
                "remoteCount": remote_count, "selfSid": local_sid, "quality": local_quality});
            result["unavailable"] = if connected {
                json!({"room.quality": "Waiting for LiveKit to report connection quality",
                    "room.baseParcelPosition": "Scene metadata has no valid base parcel coordinates",
                    "room.sid": "LiveKit has not reported a room SID",
                    "room.selfSid": "Waiting for the local participant"})
            } else {
                json!({"room": "Room is not connected", "room.baseParcelPosition": "Scene metadata has no valid base parcel coordinates"})
            };
            if let Some(reason) = room_control_block(*policy.get(), no_render.is_some()) {
                result["unavailable"]["room.connectionEnabled"] = json!(reason);
            } else {
                result["room"]["connectionEnabled"] = json!(!paused && !failed);
            }
        }
    }
    input.reply_ok(result.to_string());
}

#[derive(clap::ValueEnum, Clone, Copy)]
enum RoomAction {
    Connect,
    Disconnect,
}

/// Control a current client room without changing account or admission policy.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/room_connection")]
struct RoomConnectionCommand {
    scope: Scope,
    action: RoomAction,
}

fn room_connection(mut input: ConsoleCommand<RoomConnectionCommand>, mut rooms: RoomConnections) {
    if let Some(Ok(command)) = input.take() {
        let enabled = matches!(command.action, RoomAction::Connect);
        match rooms.set_enabled(command.scope.room_kind(), enabled) {
            Ok(()) => input.reply_ok("Room connection request applied"),
            Err(reason) => input.reply_failed(reason),
        }
    }
}

#[derive(Resource, Default)]
struct RoomIndicators(bool);

/// Show the Pulse carrier and the two LiveKit rosters above remote avatars.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/room_indicators")]
struct RoomIndicatorsCommand {
    #[arg(action = clap::ArgAction::Set)]
    show: bool,
}

fn room_indicators(
    mut input: ConsoleCommand<RoomIndicatorsCommand>,
    mut enabled: ResMut<RoomIndicators>,
) {
    if let Some(Ok(command)) = input.take() {
        enabled.0 = command.show;
        input.reply_ok("Room markers updated");
    }
}

fn draw_room_indicators(
    mut gizmos: Gizmos,
    players: Query<(&GlobalTransform, &ForeignPlayer)>,
    transports: Query<&Transport>,
    rooms: Query<(&ParticipantIndex, Has<SceneRoom>), With<Transport>>,
) {
    for (transform, player) in &players {
        let address = format!("{:#x}", player.address);
        let mut island = false;
        let mut scene = false;
        for (roster, is_scene) in &rooms {
            if roster.contains_key(&address) {
                if is_scene {
                    scene = true;
                } else {
                    island = true;
                }
            }
        }
        let pulse = player.transports.iter().any(|t| {
            transports
                .get(*t)
                .is_ok_and(|t| t.transport_type == TransportType::Pulse)
        });
        let top = transform.translation() + Vec3::Y * 2.35;
        for (i, (present, color)) in [
            (pulse, Color::srgb(1.0, 0.75, 0.0)),
            (island, Color::srgb(0.25, 1.0, 0.3)),
            (scene, Color::srgb(0.3, 0.65, 1.0)),
        ]
        .into_iter()
        .enumerate()
        {
            if present {
                gizmos.sphere(
                    Isometry3d::from_translation(top + Vec3::Y * (i as f32 * 0.12)),
                    0.055,
                    color,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        livekit::room::Connecting,
        room_connection::{FailureKind, RoomRecovery, RoomSuspended},
    };
    use bevy_console::ConsoleCommandEntered;
    use std::sync::{Arc, Mutex};

    fn spawn_island(app: &mut App) -> Entity {
        let (components, sender, _) = crate::livekit::standalone_room_components(String::new());
        app.world_mut()
            .spawn((
                components,
                Transport {
                    transport_type: TransportType::Livekit,
                    context: Entity::PLACEHOLDER,
                    sender,
                    control: None,
                },
            ))
            .id()
    }

    fn invoke(app: &mut App, name: &str, args: &[&str]) -> Result<String, String> {
        let reply = Arc::new(Mutex::new(None));
        let writer = reply.clone();
        app.world_mut().send_event(ConsoleCommandEntered {
            command_name: name.into(),
            args: args.iter().map(|arg| arg.to_string()).collect(),
            responder: Some(Arc::new(move |result| {
                *writer.lock().unwrap() = Some(result)
            })),
        });
        app.update();
        let result = reply.lock().unwrap().take().unwrap();
        result
    }

    fn island_diagnostics(app: &mut App) -> serde_json::Value {
        serde_json::from_str(&invoke(app, "/room_diagnostics", &["island"]).unwrap()).unwrap()
    }

    #[test]
    fn room_and_voice_diagnostics_share_admission_pause_and_connection_states() {
        let mut app = App::new();
        app.add_plugins((
            console::ConsolePlugin {
                add_bevy_console: false,
            },
            crate::automation::CommsAutomationPlugin,
        ))
        .init_resource::<common::structs::MicState>()
        .init_resource::<wallet::Wallet>()
        .add_event::<crate::global_crdt::ChatEvent>()
        .insert_resource(State::new(ConnectionAvailability::Available))
        .add_console_command::<RoomDiagnostics, _>(room_diagnostics);
        let room = spawn_island(&mut app);
        let assert_state = |app: &mut App, expected: &str| {
            assert_eq!(island_diagnostics(app)["room"]["state"], expected);
            let voice: serde_json::Value =
                serde_json::from_str(&invoke(app, "/voice_state", &[]).unwrap()).unwrap();
            assert_eq!(voice["rooms"][0]["state"], expected);
        };
        assert_state(&mut app, "connecting");
        app.world_mut().get_mut::<RoomRecovery>(room).unwrap().fail(
            0.0,
            crate::room_connection::AdmissionError::new(FailureKind::Transient),
        );
        assert_state(&mut app, "failed");
        assert_eq!(island_diagnostics(&mut app)["room"]["attempts"], 1);
        for (failure, name) in [
            (FailureKind::Denied, "denied"),
            (FailureKind::Invalid, "invalid"),
            (FailureKind::IdentityConflict, "identity_conflict"),
            (FailureKind::CredentialsRequired, "credentials_required"),
        ] {
            app.world_mut()
                .get_mut::<RoomRecovery>(room)
                .unwrap()
                .failure = Some(failure);
            assert_state(&mut app, name);
        }
        app.world_mut().entity_mut(room).insert(RoomSuspended);
        assert_state(&mut app, "paused");
        app.world_mut().entity_mut(room).remove::<RoomSuspended>();
        app.world_mut()
            .get_mut::<RoomRecovery>(room)
            .unwrap()
            .failure = None;
        assert_state(&mut app, "reconnecting");
        let health = crate::room_connection::ConnectionHealth::default();
        health.connected();
        app.world_mut().entity_mut(room).insert(health);
        assert_state(&mut app, "connected");
    }

    #[test]
    fn room_button_follows_pause_refusal_policy_and_unique_assignment() {
        let mut app = App::new();
        app.add_plugins(console::ConsolePlugin {
            add_bevy_console: false,
        })
        .insert_resource(State::new(ConnectionAvailability::Available))
        .add_console_command::<RoomDiagnostics, _>(room_diagnostics);
        app.update();
        let empty = island_diagnostics(&mut app);
        assert!(empty["room"].is_null());
        assert_eq!(
            empty["unavailable"]["room"],
            "No LiveKit island room assigned"
        );
        let room = spawn_island(&mut app);
        assert_eq!(
            island_diagnostics(&mut app)["room"]["connectionEnabled"],
            true
        );
        app.world_mut().entity_mut(room).insert(RoomSuspended);
        assert_eq!(
            island_diagnostics(&mut app)["room"]["connectionEnabled"],
            false
        );
        app.world_mut()
            .entity_mut(room)
            .remove::<RoomSuspended>()
            .insert(RoomRecovery {
                failure: Some(FailureKind::Transient),
                ..Default::default()
            });
        assert_eq!(
            island_diagnostics(&mut app)["room"]["connectionEnabled"],
            false
        );
        app.insert_resource(State::new(ConnectionAvailability::Unavailable));
        let blocked = island_diagnostics(&mut app);
        assert!(blocked["room"]["connectionEnabled"].is_null());
        assert_eq!(
            blocked["unavailable"]["room.connectionEnabled"],
            "Room connections are controlled by session policy"
        );
        app.insert_resource(State::new(ConnectionAvailability::Available));
        spawn_island(&mut app);
        let ambiguous = island_diagnostics(&mut app);
        assert!(ambiguous["room"]["state"].is_string());
        assert!(ambiguous["room"]["connectionEnabled"].is_null());
        assert_eq!(
            ambiguous["unavailable"]["room.connectionEnabled"],
            "More than one room matches"
        );
    }

    #[test]
    fn pause_discards_queued_messages_and_policy_blocks_resume() {
        let mut app = App::new();
        app.add_plugins((
            console::ConsolePlugin {
                add_bevy_console: false,
            },
            crate::livekit::runtime::LivekitRuntimePlugin,
        ))
        .insert_resource(State::new(ConnectionAvailability::Available))
        .add_console_command::<RoomConnectionCommand, _>(room_connection);
        app.add_plugins(super::super::control::RoomControlPlugin);
        let room = spawn_island(&mut app);
        app.update();
        invoke(&mut app, "/room_connection", &["island", "disconnect"]).unwrap();
        assert!(app.world().get::<RoomSuspended>(room).is_some());

        let sender = app.world().get::<Transport>(room).unwrap().sender.clone();
        let enqueue = || {
            sender
                .try_send(crate::NetworkMessage {
                    message: Box::new(vec![1_u8]),
                    unreliable: true,
                    recipient: crate::NetworkMessageRecipient::All,
                    topic: None,
                })
                .unwrap()
        };
        enqueue();
        app.update();
        assert!(app
            .world()
            .get::<crate::livekit::LivekitNetworkMessage>(room)
            .unwrap()
            .is_empty());

        app.insert_resource(State::new(ConnectionAvailability::Unavailable));
        assert_eq!(
            invoke(&mut app, "/room_connection", &["island", "connect"]).unwrap_err(),
            "Room connections are controlled by session policy",
        );
        assert!(app.world().get::<RoomSuspended>(room).is_some());
        app.insert_resource(State::new(ConnectionAvailability::Available));
        app.insert_resource(common::structs::NoRenderApp);
        assert_eq!(
            invoke(&mut app, "/room_connection", &["island", "connect"]).unwrap_err(),
            "Only a graphical player client can control room connections",
        );
        assert!(app.world().get::<RoomSuspended>(room).is_some());
        // A producer may enqueue after the last Update drain. The marker removal itself
        // must retire that message before a resumed room is allowed to publish.
        enqueue();
        app.world_mut().entity_mut(room).remove::<RoomSuspended>();
        assert!(app
            .world()
            .get::<crate::livekit::LivekitNetworkMessage>(room)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn reconnect_restarts_a_refused_dial_once() {
        #[derive(Resource, Default)]
        struct Attempts(usize);
        let mut app = App::new();
        app.add_plugins(console::ConsolePlugin {
            add_bevy_console: false,
        })
        .insert_resource(State::new(ConnectionAvailability::Available))
        .add_console_command::<RoomConnectionCommand, _>(room_connection)
        .init_resource::<Attempts>()
        .add_observer(
            |_: Trigger<OnAdd, Connecting>, mut attempts: ResMut<Attempts>| {
                attempts.0 += 1;
            },
        );
        let room = spawn_island(&mut app);
        app.world_mut().entity_mut(room).insert(Connecting);
        app.update();
        app.world_mut().entity_mut(room).insert(RoomRecovery {
            failure: Some(FailureKind::Transient),
            ..Default::default()
        });
        invoke(&mut app, "/room_connection", &["island", "connect"]).unwrap();
        assert_eq!(app.world().resource::<Attempts>().0, 2);
    }

    #[test]
    fn reconnect_refreshes_credentials_and_cannot_override_a_denial() {
        use crate::room_connection::{CredentialRequest, FailureKind, NeedsAdapter, RoomRecovery};
        let mut app = App::new();
        app.add_plugins(console::ConsolePlugin {
            add_bevy_console: false,
        })
        .insert_resource(State::new(ConnectionAvailability::Available))
        .add_console_command::<RoomConnectionCommand, _>(room_connection);
        let room = spawn_island(&mut app);
        app.world_mut().entity_mut(room).insert((
            RoomRecovery {
                failure: Some(FailureKind::Transient),
                ..Default::default()
            },
            CredentialRequest {
                method: "POST",
                url: "https://admission.example/".into(),
                metadata: "{}".into(),
            },
        ));
        invoke(&mut app, "/room_connection", &["island", "connect"]).unwrap();
        assert!(app.world().get::<NeedsAdapter>(room).is_some());
        assert!(app.world().get::<Connecting>(room).is_none());
        app.world_mut().entity_mut(room).remove::<NeedsAdapter>();
        app.world_mut()
            .get_mut::<RoomRecovery>(room)
            .unwrap()
            .failure = Some(FailureKind::Denied);
        assert!(invoke(&mut app, "/room_connection", &["island", "connect"]).is_err());
        assert!(app.world().get::<NeedsAdapter>(room).is_none());
        assert!(app.world().get::<Connecting>(room).is_none());
    }
}
