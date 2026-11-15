use crate::archipelago::registry::{MAX_ASSIGNMENT_BYTES, PeerLink, SocketEvent, SocketEvents};
use crate::archipelago::session::{is_address, is_session_key, session_key_of};
use crate::archipelago::state::AppState;
use crate::auth::AuthChain;
use crate::protocol::archipelago::archipelago::{
    ChallengeResponseMessage, IslandChangedMessage, KickedMessage, KickedReason, ServerPacket,
    WelcomeMessage, client_packet, server_packet,
};
use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use io::WebSocket;
use prost::Message as _;
use rand::RngExt;
use std::time::Duration;

mod control_position;
mod io;
mod legacy_decode;
mod legacy_fence;

const MAX_CLIENT_MESSAGE_BYTES: usize = 16 * 1024;
const AUTHORITY_OPERATION_TIMEOUT: Duration = Duration::from_secs(2);

pub fn routes() -> Router<AppState> {
    Router::new().route("/ws", get(ws_upgrade))
}

type Peer = Option<axum::Extension<axum::extract::ConnectInfo<std::net::SocketAddr>>>;

pub(super) fn connector_routes(
    http: std::sync::Arc<super::handlers::ConnectorHttp>,
) -> Router<AppState> {
    Router::new().route(
        "/ws",
        axum::routing::any(move |method, uri, headers, peer, ws, state| {
            connector_upgrade(method, uri, headers, peer, ws, state, http.clone())
        }),
    )
}

async fn connector_upgrade(
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
    peer: Peer,
    ws: Result<WebSocketUpgrade, axum::extract::ws::rejection::WebSocketUpgradeRejection>,
    State(state): State<AppState>,
    http: std::sync::Arc<super::handlers::ConnectorHttp>,
) -> axum::response::Response {
    if method != axum::http::Method::GET
        || !headers
            .get("sec-websocket-key")
            .is_some_and(|key| key.as_bytes().len() == 24)
    {
        return http.respond(method, uri, headers).await;
    }
    match ws {
        Ok(ws) => ws_upgrade(ws, State(state), headers, peer)
            .await
            .into_response(),
        Err(error) => error.into_response(),
    }
}

async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    peer: Peer,
) -> impl IntoResponse {
    let start = std::time::Instant::now();
    let client = crate::shared::client_ip::distinct_client_ip(
        &headers,
        peer.map(|axum::Extension(axum::extract::ConnectInfo(peer))| peer),
        &state.settings.server.trusted_proxy_cidrs,
    );
    if client.is_none() && state.sockets_per_ip.enabled() {
        crate::gatekeeper::admission::cluster::metrics::ws_unattributed_upgrade();
    }
    // Optional protocol names never select the authentication or packet decoder.
    let protocols = headers
        .get("sec-websocket-protocol")
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let response = upgrade(ws, state.clone(), protocols, client.as_deref());
    state
        .connector_metrics
        .observe_connector(crate::stats::http::Observation {
            method: "GET".into(),
            handler: "/ws".into(),
            code: response.status().as_u16(),
            request_size: 0.,
            duration: start.elapsed(),
        });
    response
}

fn upgrade(
    ws: WebSocketUpgrade,
    state: AppState,
    protocols: Vec<String>,
    client: Option<&str>,
) -> axum::response::Response {
    let Ok(ip_slot) = state.sockets_per_ip.try_acquire(client) else {
        return (
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            [(axum::http::header::RETRY_AFTER, "5")],
        )
            .into_response();
    };
    let handler = state.handlers.clone().try_acquire_owned();
    let permit = match state.sockets.clone().try_acquire_owned() {
        Ok(permit) if handler.is_ok() && !state.restart.is_cancelled() => permit,
        _ => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                [(axum::http::header::RETRY_AFTER, "5")],
            )
                .into_response();
        }
    };
    let ws = ws.protocols(protocols);
    ws.read_buffer_size(16 * 1024)
        .write_buffer_size(0)
        .max_write_buffer_size(MAX_ASSIGNMENT_BYTES + 1024)
        .max_message_size(MAX_CLIENT_MESSAGE_BYTES)
        .max_frame_size(MAX_CLIENT_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
            let _ip_slot = ip_slot;
            let socket = io::WebSocket::new(socket, state.clone(), permit);
            handle_socket(socket, state).await;
            drop(handler);
        })
        .into_response()
}

#[derive(Clone, Copy, PartialEq)]
enum Stage {
    HandshakeStart,
    ChallengeSent,
    Completed,
}

fn craft(message: server_packet::Message) -> Vec<u8> {
    let packet = ServerPacket {
        message: Some(message),
        request_sequence: 0,
    };
    let mut buf = Vec::with_capacity(packet.encoded_len());
    packet.encode(&mut buf).expect("ServerPacket encodes");
    buf
}

async fn send_packet(
    socket: &mut WebSocket,
    message: server_packet::Message,
    owner: Option<&PeerLink>,
) -> bool {
    let outgoing = match message {
        server_packet::Message::IslandChanged(assignment) => io::Outgoing::Assignment {
            message: Message::Binary(
                craft(server_packet::Message::IslandChanged(assignment)).into(),
            ),
            stamp: None,
        },
        server_packet::Message::Kicked(kick) => io::Outgoing::Kick(Message::Binary(
            craft(server_packet::Message::Kicked(kick)).into(),
        )),
        other => io::Outgoing::Packet(Message::Binary(craft(other).into())),
    };
    io::send_outgoing(socket, outgoing, owner).await
}

fn kicked_packet() -> server_packet::Message {
    server_packet::Message::Kicked(KickedMessage {
        reason: KickedReason::KrNewSession as i32,
    })
}

async fn handle_renewal_result(
    socket: &mut WebSocket,
    result: Result<(), crate::archipelago::control_v4::V4Error>,
    link: &PeerLink,
) -> bool {
    match result {
        Ok(()) => true,
        Err(crate::archipelago::control_v4::V4Error::AuthorityConflict) => {
            let _ = send_packet(socket, kicked_packet(), Some(link)).await;
            false
        }
        Err(_) => false,
    }
}

fn heartbeat_position(
    hb: &crate::protocol::archipelago::archipelago::Heartbeat,
) -> Option<[f32; 3]> {
    hb.position
        .as_ref()
        .map(|p| [p.x, p.y, p.z])
        .filter(|position| position.iter().all(|value| value.is_finite()))
}

/// Suppress only a complete assignment already accepted by this socket's bounded writer queue.
fn is_repeat(last: Option<&(Vec<u8>, i64)>, assignment: &[u8], now: i64, dedup_ms: f64) -> bool {
    match last {
        Some((seen, at)) => {
            dedup_ms > 0.0
                && seen.as_slice() == assignment
                && (now as f64) - (*at as f64) < dedup_ms
        }
        None => false,
    }
}

/// Never resolves before the handshake completes, which keeps a socket with no registration out of
/// the select without a second copy of the loop.
async fn next_event(rx: Option<&mut SocketEvents>) -> Option<SocketEvent> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

struct Registration {
    address: String,
    session: String,
    link: PeerLink,
    fence: Option<legacy_fence::LegacyFence>,
}

/// Keep receiving while authentication/policy is pending so Close reaches the transport coordinator.
async fn handshake_operation<F: std::future::Future>(
    socket: &mut WebSocket,
    state: &AppState,
    deadline: tokio::time::Instant,
    signed_pending: bool,
    operation: F,
) -> Option<F::Output> {
    tokio::pin!(operation);
    loop {
        tokio::select! {
            biased;
            _ = state.restart.cancelled() => return None,
            _ = tokio::time::sleep_until(deadline) => return None,
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Ping(payload))) => {
                    if !io::send(socket, Message::Pong(payload), None).await { return None; }
                }
                Some(Ok(Message::Pong(_))) => {}
                Some(Ok(Message::Binary(bytes))) => match legacy_decode::decode(bytes.as_ref()) {
                    Ok(packet) if signed_pending && matches!(packet.message, Some(client_packet::Message::SignedChallenge(_))) => {}
                    Err(_) => {
                        let _ = io::send(socket, Message::Close(Some(axum::extract::ws::CloseFrame {
                            code: 1007, reason: "Cannot decode ClientPacket".into(),
                        })), None).await;
                        return None;
                    }
                    _ => return None,
                }
                _ => return None,
            },
            result = &mut operation => return Some(result),
        }
    }
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    let mut stage = Stage::HandshakeStart;
    let mut challenge_to_sign = String::new();
    let mut issuance = None;
    let mut authority_epoch = None;
    let mut registration: Option<Registration> = None;
    let mut events: Option<SocketEvents> = None;
    let mut last_island: Option<(Vec<u8>, i64)> = None;
    let mut last_stamp = None;
    let dedup_ms = state.settings.server.island_changed_dedup_ms;
    let handshake_timeout = Duration::from_millis(state.settings.auth.handshake_timeout_ms);
    let mut handshake_deadline = tokio::time::Instant::now() + handshake_timeout;
    let mut ownership_poll = tokio::time::interval(legacy_fence::POLL_INTERVAL);
    ownership_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut renewal = tokio::time::interval(crate::archipelago::control_v4::OWNER_RENEWAL_INTERVAL);
    renewal.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        if let Some(current) = &registration {
            socket.bind_owner(&current.link);
        }
        if registration
            .as_ref()
            .is_some_and(|current| current.link.is_closed())
        {
            if matches!(
                events.as_mut().and_then(|rx| rx.try_recv().ok()),
                Some(SocketEvent::Kicked)
            ) {
                let _ = send_packet(&mut socket, kicked_packet(), None).await;
            }
            break;
        }
        if stage != Stage::Completed && tokio::time::Instant::now() >= handshake_deadline {
            tracing::debug!("closing socket: the handshake stalled past its timeout");
            break;
        }
        tokio::select! {
            incoming = socket.recv() => {
                let Some(msg) = incoming else { break };
                let msg = match msg { Ok(m) => m, Err(_) => break };
                match msg {
                    Message::Binary(bytes) => {
                        let packet = match legacy_decode::decode(bytes.as_ref()) {
                            Ok(p) => p,
                            Err(_) => {
                                let _ = io::send(&mut socket,
                                    Message::Close(Some(axum::extract::ws::CloseFrame {
                                        code: 1007,
                                        reason: "Cannot decode ClientPacket".into(),
                                    })), registration.as_ref().map(|current| &current.link))
                                    .await;
                                break;
                            }
                        };
                        match stage {
                            Stage::HandshakeStart => {
                                let Some(client_packet::Message::ChallengeRequest(req)) = packet.message else {
                                    break;
                                };
                                let addr = req.address.to_ascii_lowercase();
                                if !req.address.starts_with("0x") || !is_address(&addr) {
                                    tracing::debug!("rejecting: challengeRequest carries no valid address");
                                    break;
                                }
                                let Some(denied) = handshake_operation(&mut socket, &state, handshake_deadline, false, state.deny_list.is_denied(&addr)).await else { break };
                                if denied {
                                    tracing::warn!(addr = %addr, "archipelago ws rejected: deny-listed wallet");
                                    break;
                                }
                                authority_epoch = match tokio::time::timeout_at(
                                    handshake_deadline, legacy_fence::reserve(&state),
                                ).await {
                                    Ok(Ok(epoch)) => epoch,
                                    _ => break,
                                };
                                let nonce: [u8; 32] = rand::rng().random();
                                challenge_to_sign = format!("dcl-{}", nonce.iter().map(|b| format!("{b:02x}")).collect::<String>());

                                issuance = Some(state.challenges.issue(&challenge_to_sign));
                                let already_connected = state.registry.has_any_peer(&addr);
                                if !send_packet(
                                    &mut socket,
                                    server_packet::Message::ChallengeResponse(ChallengeResponseMessage {
                                        challenge_to_sign: challenge_to_sign.clone(),
                                        already_connected,
                                    }),
                                    None,
                                )
                                .await
                                {
                                    break;
                                }
                                stage = Stage::ChallengeSent;
                                handshake_deadline = tokio::time::Instant::now() + handshake_timeout;
                            }
                            Stage::ChallengeSent => {
                                let Some(client_packet::Message::SignedChallenge(signed)) = packet.message else {
                                    break;
                                };
                                let chain: AuthChain = match serde_json::from_str(&signed.auth_chain_json) {
                                    Ok(c) => c,
                                    Err(_) => break,
                                };
                                let Some(issued) = issuance.as_ref() else { break };
                                let Some(verified) = handshake_operation(
                                    &mut socket, &state, handshake_deadline, true,
                                    state.challenges.redeem_and_verify_payload(
                                        issued, &challenge_to_sign, &chain, state.gatekeeper.eip1654(),
                                    ),
                                ).await else { break };
                                match verified {
                                    Ok(signer) => {
                                        let session = session_key_of(&chain, &signer);
                                        if !is_session_key(&session) {
                                            tracing::warn!(addr = %signer, "rejecting: auth chain yields no usable session key");
                                            break;
                                        }
                                        let Some(denied) = handshake_operation(&mut socket, &state, handshake_deadline, true, state.deny_list.is_denied(&signer)).await else { break };
                                        if denied {
                                            tracing::warn!(addr = %signer, "archipelago ws rejected: deny-listed wallet (post-auth)");
                                            break;
                                        }
                                        let Some(banned) = handshake_operation(&mut socket, &state, handshake_deadline, true, state.ban_checker.is_banned(&signer)).await else { break };
                                        if banned {
                                            tracing::warn!(addr = %signer, "archipelago ws rejected: platform-banned wallet");
                                            let _ = send_packet(&mut socket, kicked_packet(), None).await;
                                            break;
                                        }
                                        if state.restart.is_cancelled() || tokio::time::Instant::now() >= handshake_deadline {
                                            break;
                                        }
                                        let admitted = if let Some(epoch) = authority_epoch {
                                            match tokio::time::timeout_at(
                                                handshake_deadline,
                                                legacy_fence::LegacyFence::claim(&state, &signer, &session, epoch),
                                            ).await {
                                                Ok(Ok((fence, link, rx))) => Some((link, rx, None, Some(fence))),
                                                Ok(Err(crate::archipelago::control_v4::V4Error::AuthorityConflict)) => None,
                                                _ => {
                                                    tracing::debug!("closing legacy handshake: assignment authority unavailable");
                                                    break;
                                                }
                                            }
                                        } else {
                                            // No await between final policy completion and local registration.
                                            state.registry.begin_admission(&signer).and_then(|ticket| ticket.commit(&session))
                                                .map(|(link, rx, previous)| (link, rx, previous, None))
                                        };
                                        let Some((link, rx, previous, fence)) = admitted else {
                                            tracing::info!(addr = %signer, session = %session, "rejecting superseded handshake admission");
                                            let _ = send_packet(&mut socket, kicked_packet(), None).await;
                                            break;
                                        };
                                        if let Some(previous) = previous {
                                            if !previous.is_closed() {
                                                tracing::debug!(addr = %signer, "replacing this device's previous socket");
                                                previous.send(SocketEvent::Kicked);
                                            }
                                            previous.close();
                                        }
                                        events = Some(rx);
                                        registration = Some(Registration {
                                            address: signer.clone(),
                                            session: session.clone(),
                                            link,
                                            fence,
                                        });
                                        if state.restart.is_cancelled() || tokio::time::Instant::now() >= handshake_deadline { break; }
                                        tracing::info!(addr = %signer, session = %session, "archipelago ws handshake complete (welcome)");
                                        if !send_packet(
                                            &mut socket,
                                            server_packet::Message::Welcome(WelcomeMessage {
                                                peer_id: signer.clone(),
                                            }),
                                            registration.as_ref().map(|current| &current.link),
                                        )
                                        .await
                                        {
                                            break;
                                        }
                                        // The cluster feed is edge-triggered and says nothing while
                                        // a peer's cluster is unchanged: a client that reconnects
                                        // standing still is given a room only because of this.
                                        state.gatekeeper.announce_session(&signer, &session);
                                        stage = Stage::Completed;
                                        handshake_deadline = tokio::time::Instant::now() + handshake_timeout;
                                    }
                                    Err(e) => {
                                        tracing::warn!(reason = e.class(), "archipelago signed-challenge rejected");
                                        break;
                                    }
                                }
                            }
                            Stage::Completed => {
                                let Some(client_packet::Message::Heartbeat(hb)) = packet.message else {
                                    continue;
                                };
                                let Some(current) = registration.as_ref() else { continue };
                                if let Some(position) = hb.position.as_ref() {
                                    state.observations.publish(crate::protocol::bus::statistics::Event::Heartbeat {
                                        address: current.address.clone(),
                                        position: [position.x as f64, position.y as f64, position.z as f64],
                                    });
                                }
                                let Some(position) = heartbeat_position(&hb) else { continue };
                                let realm = hb.desired_room.unwrap_or_else(|| "catalyrst".into());
                                if !crate::protocol::bus::control_position::valid_realm(&realm) { continue; }
                                if let Some(fence) = current.fence.as_ref() {
                                    match tokio::time::timeout(
                                        AUTHORITY_OPERATION_TIMEOUT,
                                    legacy_fence::owns_realm(&state, &fence.owner),
                                    ).await {
                                        Ok(Ok(true)) => {
                                            // Deferred positions recheck this exact owner before application.
                                        }
                                        Ok(Ok(false)) => {
                                            let _ = send_packet(&mut socket, kicked_packet(), None).await;
                                            break;
                                        }
                                        _ => break,
                                    }
                                }
                                socket.position(current.fence.as_ref().map(|f| f.owner.clone()), current.link.clone(), realm, position).await;
                            }
                        }
                    }
                    Message::Ping(p) => {
                        if !io::send(&mut socket, Message::Pong(p), registration.as_ref().map(|current| &current.link)).await {
                            break;
                        }
                    }
                    Message::Close(_) => break,

                    _ => {}
                }
            }
            _ = tokio::time::sleep_until(handshake_deadline), if stage != Stage::Completed => {
                tracing::debug!("closing socket: the handshake stalled past its timeout");
                break;
            }
            event = next_event(events.as_mut()) => {
                let Some(event) = event else { break };
                match event {
                    SocketEvent::IslandChanged(payload, stamp) => {
                        if registration.as_ref().is_some_and(|current| current.fence.is_some()) {
                            continue;
                        }
                        let Ok(assignment) = IslandChangedMessage::decode(payload.as_slice()) else {
                            continue;
                        };
                        let now = chrono::Utc::now().timestamp_millis();
                        if last_stamp == stamp && is_repeat(last_island.as_ref(), &payload, now, dedup_ms) {
                            state.feed.on_deduplicated();
                            continue;
                        }
                        if !io::send_outgoing(
                            &mut socket,
                            io::Outgoing::Assignment {
                                message: Message::Binary(craft(server_packet::Message::IslandChanged(assignment.clone())).into()),
                                stamp: stamp.clone(),
                            },
                            registration.as_ref().map(|current| &current.link),
                        )
                        .await
                        {
                            break;
                        }
                        // Local queue acceptance is not proof of remote consumption.
                        last_island = Some((payload, now));
                        last_stamp = stamp;
                    }
                    SocketEvent::Kicked => {
                        let addr = registration.as_ref().map(|r| r.address.clone()).unwrap_or_default();
                        tracing::info!(addr = %addr, "session superseded or kicked; closing socket");
                        let _ = send_packet(&mut socket, kicked_packet(), None).await;
                        break;
                    }
                }
            }
            _ = ownership_poll.tick(), if registration.as_ref().is_some_and(|current| current.fence.is_some()) => {
                let current = registration.as_mut().expect("registered fenced socket");
                if !matches!(tokio::time::timeout(
                    AUTHORITY_OPERATION_TIMEOUT,
                    current.fence.as_mut().unwrap().poll(&state, &mut socket, &current.link),
                ).await, Ok(true)) {
                    break;
                }
            }
            _ = renewal.tick(), if registration.as_ref().is_some_and(|current| current.fence.is_some()) => {
                let current = registration.as_ref().unwrap();
                let owner = &current.fence.as_ref().unwrap().owner;
                let result = tokio::time::timeout(
                    AUTHORITY_OPERATION_TIMEOUT, state.control_v4.renew(owner),
                ).await.unwrap_or(Err(crate::archipelago::control_v4::V4Error::AuthorityUnavailable));
                if !handle_renewal_result(&mut socket, result, &current.link).await {
                    break;
                }
            }
        }
    }

    drop(socket);
    if let Some(issued) = issuance.as_ref() {
        state.challenges.discard(issued);
    }
    if let Some(current) = registration {
        current.link.close();
        if let Some(fence) = current.fence.as_ref() {
            control_position::publish(&state, &fence.owner, "", None);
            state
                .registry
                .on_v4_peer_disconnected(&current.address, current.link.id);
        } else {
            state.registry.on_peer_disconnected(
                &current.address,
                &current.session,
                current.link.id,
            );
        }
        state.registry.while_wallet_absent(&current.address, || {
            state.enforcement.remove_peer(&current.address)
        });
        // Statistics backpressure retains handler admission credit without
        // delaying signaling revocation, registry cleanup or durable release.
        tokio::join!(
            state.observations.disconnect(current.address.clone()),
            async {
                if let Some(fence) = current.fence.as_ref() {
                    let _ = tokio::time::timeout(
                        AUTHORITY_OPERATION_TIMEOUT,
                        state.control_v4.release(&fence.owner),
                    )
                    .await;
                }
            }
        );
        tracing::info!(addr = %current.address, session = %current.session, "archipelago ws closed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::archipelago::Position;
    use crate::protocol::archipelago::archipelago::Heartbeat;
    use std::time::Duration;

    #[tokio::test]
    async fn renewal_conflict_sends_kicked_before_close_but_unavailable_only_closes() {
        use crate::archipelago::control_v4::V4Error;
        use futures::StreamExt;
        use std::sync::Arc;
        use tokio_tungstenite::tungstenite::Message as ClientMessage;

        for conflict in [true, false] {
            let state = crate::archipelago::test_support::state_over(
                crate::archipelago::Settings::for_tests(),
                Arc::new(crate::gatekeeper::bus::DisabledBus),
            )
            .await;
            let app = Router::new().route(
                "/",
                get(move |upgrade: WebSocketUpgrade| {
                    let state = state.clone();
                    async move {
                        upgrade.on_upgrade(move |transport| async move {
                            let permit = state.sockets.clone().acquire_owned().await.unwrap();
                            let mut socket = WebSocket::new(transport, state.clone(), permit);
                            let (link, _events, _) =
                                state.registry.on_peer_connected("wallet", "session");
                            socket.bind_owner(&link);
                            let result = Err(if conflict {
                                V4Error::AuthorityConflict
                            } else {
                                V4Error::AuthorityUnavailable
                            });
                            assert!(!handle_renewal_result(&mut socket, result, &link).await);
                            drop(socket);
                            link.close();
                        })
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let (mut client, _) = tokio_tungstenite::connect_async(format!("ws://{address}/"))
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                if conflict {
                    let ClientMessage::Binary(bytes) = client.next().await.unwrap().unwrap() else {
                        panic!("renewal conflict must send Kicked before close");
                    };
                    let packet = ServerPacket::decode(bytes.as_ref()).unwrap();
                    assert!(
                        matches!(packet.message, Some(server_packet::Message::Kicked(kicked))
                        if kicked.reason == KickedReason::KrNewSession as i32)
                    );
                }
                assert!(matches!(
                    client.next().await.unwrap().unwrap(),
                    ClientMessage::Close(_)
                ));
            })
            .await
            .expect("renewal failure must close promptly");
            task.abort();
        }
    }

    #[test]
    fn positionless_heartbeat_is_ignored() {
        let hb = Heartbeat {
            position: None,
            desired_room: Some("catalyrst".into()),
            sample_sequence: None,
        };
        assert_eq!(heartbeat_position(&hb), None);
    }

    #[test]
    fn heartbeat_with_position_yields_xyz() {
        let hb = Heartbeat {
            position: Some(Position {
                x: 1.5,
                y: 2.0,
                z: -3.25,
            }),
            desired_room: None,
            sample_sequence: None,
        };
        assert_eq!(heartbeat_position(&hb), Some([1.5, 2.0, -3.25]));
    }

    #[test]
    fn dedup_preserves_fractional_infinite_and_number_subtraction_boundaries() {
        let last = (vec![1], 1_000);
        for (threshold, now, expected) in [
            (0.5, 1_000, true),
            (0.5, 1_001, false),
            (10.0, 1_010, false),
            (10.0, 1_011, false),
            (10.0, 999, true),
            (f64::INFINITY, i64::MAX, true),
            (-1.0, 999, false),
            (-0.0, 999, false),
            (f64::NEG_INFINITY, 999, false),
        ] {
            assert_eq!(is_repeat(Some(&last), &[1], now, threshold), expected);
            assert!(!is_repeat(Some(&last), &[2], now, threshold));
        }
        // The operands round separately before subtraction, as Number timestamps do.
        let last = (vec![1], 9_007_199_254_740_992);
        assert!(is_repeat(Some(&last), &[1], 9_007_199_254_740_993, 0.5));
    }

    #[test]
    fn the_same_room_inside_the_window_is_a_repeat_and_a_different_one_is_not() {
        let now = 1000;
        let assignment = IslandChangedMessage {
            island_id: "island-C1".into(),
            conn_str: "livekit:wss://example.invalid?access_token=first".into(),
            ..Default::default()
        };
        let last = (assignment.encode_to_vec(), now);
        assert!(is_repeat(
            Some(&last),
            &assignment.encode_to_vec(),
            now,
            10_000.0
        ));
        assert!(!is_repeat(
            Some(&last),
            &IslandChangedMessage {
                island_id: "island-C2".into(),
                ..assignment.clone()
            }
            .encode_to_vec(),
            now,
            10_000.0
        ));
        assert!(!is_repeat(None, &assignment.encode_to_vec(), now, 10_000.0));
    }

    #[test]
    fn a_zero_window_repeats_nothing_and_an_elapsed_one_lets_the_room_through() {
        let now = 1000;
        let assignment = IslandChangedMessage {
            island_id: "island-C1".into(),
            conn_str: "livekit:wss://example.invalid?access_token=first".into(),
            ..Default::default()
        };
        let last = (assignment.encode_to_vec(), now);
        assert!(!is_repeat(
            Some(&last),
            &assignment.encode_to_vec(),
            now,
            0.0
        ));
        assert!(!is_repeat(
            Some(&last),
            &assignment.encode_to_vec(),
            now + 10,
            10.0
        ));
        assert!(is_repeat(
            Some(&last),
            &assignment.encode_to_vec(),
            now - 100_000,
            10.0
        ));
        assert!(!is_repeat(
            Some(&last),
            &assignment.encode_to_vec(),
            now + 100_000,
            10.0
        ));
    }
}
