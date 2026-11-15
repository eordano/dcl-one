use super::*;
use bevy_console::ConsoleCommand;
use common::util::AsH160;
use console::{AddDebugSection, DebugInput, DebugInputKind, DebugSection, DoAddConsoleCommand};
use serde_json::json;

pub(super) struct PulseDebugPlugin;

impl Plugin for PulseDebugPlugin {
    fn build(&self, app: &mut App) {
        let client = !app
            .world()
            .contains_resource::<common::structs::NoRenderApp>()
            && !common::structs::server_mode();
        let mut inputs = Vec::new();
        if client {
            app.add_console_command::<PulseConnection, _>(pulse_connection);
            inputs.push(DebugInput {
                label: "Connection",
                field: "connectionEnabled",
                command: "/pulse_connection {value}",
                kind: DebugInputKind::ActionButton {
                    enable: ("Connect", "connect"),
                    disable: ("Disconnect", "disconnect"),
                },
            });
        }
        inputs.push(DebugInput {
            label: "Look up peer by wallet",
            field: "wallet",
            command: "/pulse_lookup {value}",
            kind: DebugInputKind::Text,
        });
        app.init_resource::<PeerLookup>()
            .add_console_command::<PulseState, _>(pulse_state)
            .add_console_command::<PulseLookup, _>(pulse_lookup)
            .add_debug_section(DebugSection {
                title: "Pulse",
                query: "/pulse_state",
                fields: &[
                    ("Transport state", "state"),
                    ("Authenticated", "authenticated"),
                    ("Outbound payload (B/s)", "sentBytes"),
                    ("Inbound payload (B/s)", "receivedBytes"),
                    ("Outbound packets/s", "sentPackets"),
                    ("Inbound packets/s", "receivedPackets"),
                    ("Resync requests (last 60s)", "resyncs"),
                    ("Emote for selected peer", "emote"),
                ],
                actions: &[],
                inputs: &inputs,
            });
    }
}

#[derive(Resource, Default)]
struct PeerLookup(Option<Address>);

/// Select the remote wallet whose Pulse emote stream to inspect.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/pulse_lookup")]
struct PulseLookup {
    wallet: String,
}

fn pulse_lookup(mut input: ConsoleCommand<PulseLookup>, mut lookup: ResMut<PeerLookup>) {
    if let Some(Ok(command)) = input.take() {
        let Some(wallet) = command.wallet.as_h160() else {
            input.reply_failed("Expected a wallet address");
            return;
        };
        lookup.0 = Some(wallet);
        input.reply_ok(format!("Tracking {wallet:#x}"));
    }
}

/// Read the connection lifecycle, protocol payload rates and decoder state.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/pulse_state")]
struct PulseState;

fn pulse_state(
    mut input: ConsoleCommand<PulseState>,
    session: Option<Res<PulseSession>>,
    no_render: Option<Res<common::structs::NoRenderApp>>,
    lookup: Res<PeerLookup>,
    time: Res<Time>,
) {
    if let Some(Ok(_)) = input.take() {
        let state = match session.as_deref().map(|s| &s.state) {
            None => "unconfigured",
            Some(Connection::Down { .. }) => "waiting",
            Some(Connection::Paused) => "paused",
            Some(Connection::Connecting) => "connecting",
            Some(Connection::Idle) => "connected",
            Some(Connection::Signing(_)) => "signing",
            Some(Connection::AwaitingResponse { .. }) => "authenticating",
            Some(Connection::Established) => "authenticated",
            Some(Connection::Dead) => "terminal disconnect",
        };
        let link = session.as_ref().and_then(|s| s.link.as_ref());
        let traffic = link
            .map(|link| link.traffic.last_second())
            .unwrap_or_default();
        let second = time.elapsed_secs_f64() as u64;
        let resyncs = session
            .as_ref()
            .map(|session| {
                session
                    .resyncs
                    .iter()
                    .filter(|(stamp, _)| second.saturating_sub(*stamp) < 60)
                    .map(|(_, count)| u64::from(*count))
                    .sum::<u64>()
            })
            .unwrap_or_default();
        let emote = lookup.0.map(|wallet| {
            match session.as_ref().and_then(|s| s.decoder.emote_of(wallet)) {
                None => "peer not present",
                Some(None) => "not emoting",
                Some(Some(urn)) => urn,
            }
        });
        let connection = session
            .as_deref()
            .ok_or("Pulse is not configured")
            .and_then(|s| s.connection_enabled(no_render.is_some()));
        let mut unavailable = json!({"emote": "Select a peer wallet to inspect its emote"});
        if let Err(reason) = connection {
            unavailable["connectionEnabled"] = json!(reason);
        }
        input.reply_ok(json!({
            "connectionEnabled": connection.ok(),
            "state": state, "authenticated": matches!(session.as_deref().map(|s| &s.state), Some(Connection::Established)),
            "sentBytes": traffic.sent.bytes, "sentPackets": traffic.sent.packets, "receivedBytes": traffic.received.bytes,
            "receivedPackets": traffic.received.packets, "resyncs": resyncs,
            "wallet": lookup.0.map(|wallet| format!("{wallet:#x}")), "emote": emote,
            "unavailable": unavailable,
        }).to_string());
    }
}

#[derive(clap::ValueEnum, Clone)]
enum Operation {
    Connect,
    Disconnect,
}

/// Pause or resume the player connection. A terminal disconnect holds until the realm changes.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/pulse_connection")]
struct PulseConnection {
    operation: Operation,
}

fn pulse_connection(
    mut input: ConsoleCommand<PulseConnection>,
    session: Option<ResMut<PulseSession>>,
    no_render: Option<Res<common::structs::NoRenderApp>>,
    mut commands: Commands,
) {
    let Some(Ok(command)) = input.take() else {
        return;
    };
    let Some(mut session) = session else {
        input.reply_failed("Pulse is not configured");
        return;
    };
    let enabled = matches!(command.operation, Operation::Connect);
    match session.set_connection_enabled(enabled, no_render.is_some(), &mut commands) {
        Ok(()) if enabled => input.reply_ok("Pulse connection enabled"),
        Ok(()) => input.reply_ok("Pulse paused"),
        Err(reason) => input.reply_failed(reason),
    }
}
