//! `GET /ws/{scene}`: the client socket. Handshake, then one signed auth
//! frame within the deadline, then admission by the permission core, then
//! (for an unhosted scene that opts in) the load, then the `Init` frame (the
//! entity range and the state snapshot) and the CRDT relay until either side
//! closes or the client is removed.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{OriginalUri, Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use chrono::NaiveDateTime;
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;

use crate::policy::{Access, AccessGrant, Ban};
use crate::shared::identity::{Address, AuthorityNotEstablished};

use super::auth::{candidate_paths, verify_auth_frame};
use super::protocol::{MessageType, decode_message, encode_init_message, encode_message};
use super::scene::{Scene, SceneIdentity};
use super::{SceneHostState, SceneKey, SharedState, loader};

/// What a socket joins: a hosted scene, or one loaded once the client is admitted.
enum Target {
    Hosted(Arc<Scene>),
    Autoload(SceneIdentity),
}

impl Target {
    fn identity(&self) -> &SceneIdentity {
        match self {
            Self::Hosted(scene) => &scene.identity,
            Self::Autoload(identity) => identity,
        }
    }
}

/// Why a verified identity is not let into a scene.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionRefused {
    Banned(Ban),
    InvalidSecret,
    NotAllowed,
    /// The policy could not answer (a store fault): refused, fail closed.
    Undetermined(String),
}

impl std::fmt::Display for AdmissionRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Banned(b) => write!(f, "banned ({b:?})"),
            Self::InvalidSecret => f.write_str("shared secret missing or invalid"),
            Self::NotAllowed => f.write_str("not allowed by the world's access setting"),
            Self::Undetermined(why) => write!(f, "permission undetermined: {why}"),
        }
    }
}

fn undetermined(e: AuthorityNotEstablished) -> AdmissionRefused {
    AdmissionRefused::Undetermined(e.to_string())
}

/// May `address` be in `scene` at `now`? A world scene asks
/// `Policy::readmit` (bans, owner, the access setting; `secret` from the
/// signed metadata), a Genesis scene `Policy::is_banned`, the local
/// development scene nobody. `prior` is how the client was admitted before
/// (a re-check without the secret): a shared-secret world keeps a client it
/// admitted by secret until a ban or an access-type change refuses it, and
/// an authority fault leaves a client admitted as owner undetermined.
pub async fn admit(
    state: &SceneHostState,
    scene: &SceneIdentity,
    address: &Address,
    secret: Option<&str>,
    prior: Option<AccessGrant>,
    now: NaiveDateTime,
) -> Result<Option<AccessGrant>, AdmissionRefused> {
    let scene_id = scene.scene_id();
    match &scene.key {
        SceneKey::Local => Ok(None),
        SceneKey::World(world) => match state
            .policy
            .readmit(world, address, secret, scene_id, None, prior, now)
            .await
            .map_err(undetermined)?
        {
            Access::Allowed(grant) => Ok(Some(grant)),
            Access::Banned(ban) => Err(AdmissionRefused::Banned(ban)),
            Access::InvalidSecret
                if secret.is_none() && prior == Some(AccessGrant::SharedSecret) =>
            {
                Ok(prior)
            }
            Access::InvalidSecret => Err(AdmissionRefused::InvalidSecret),
            Access::NotAllowed => Err(AdmissionRefused::NotAllowed),
        },
        SceneKey::Genesis(_) | SceneKey::GenesisParcel(_) => match state
            .policy
            .is_banned(address, scene_id, None, now)
            .await
            .map_err(undetermined)?
        {
            Some(ban) => Err(AdmissionRefused::Banned(ban)),
            None => Ok(None),
        },
    }
}

pub async fn ws_upgrade(
    ws: WebSocketUpgrade,
    Path(scene_name): Path<String>,
    OriginalUri(uri): OriginalUri,
    headers: axum::http::HeaderMap,
    State(state): State<SharedState>,
) -> axum::response::Response {
    let Ok(key) = SceneKey::parse(&scene_name) else {
        return not_loaded(&scene_name, None);
    };
    let key = state.canonical_key(&key).await;
    // Nothing is loaded before the client is admitted.
    let target = match state.scenes.get(&key.room_key()) {
        Some(scene) => Target::Hosted(scene),
        None if state.settings.autoload && key != SceneKey::Local => {
            if state.scenes.len() >= state.settings.max_scenes {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!(
                        "{scene_name} is not currently loaded in the server: scene host is full"
                    ),
                )
                    .into_response();
            }
            match loader::resolve(&state, &key).await {
                Ok(Some(resolved)) if resolved.authoritative => Target::Autoload(SceneIdentity {
                    key: key.clone(),
                    deployment: Some(resolved.deployment),
                }),
                Ok(Some(_)) => {
                    return not_loaded(
                        &scene_name,
                        Some("the scene does not enable authoritativeMultiplayer"),
                    );
                }
                Ok(None) => return not_loaded(&scene_name, Some("no local active deployment")),
                Err(e) => {
                    tracing::info!(scene = %key, error = %e, "autoload refused");
                    return not_loaded(&scene_name, Some(&e.to_string()));
                }
            }
        }
        None => return not_loaded(&scene_name, None),
    };

    let route = format!("/ws/{scene_name}");
    let paths = candidate_paths(&headers, uri.path(), &route);
    let ws = ws
        .max_frame_size(state.settings.ws_max_frame_bytes)
        .max_message_size(state.settings.ws_max_frame_bytes);
    let caller_ip = crate::shared::client_ip::client_ip(&headers);
    let mgr = Arc::clone(&state);
    ws.on_upgrade(move |socket| async move {
        mgr.scenes.on_ws_connected();
        handle_socket(&mgr, socket, target, paths, caller_ip).await;
        mgr.scenes.on_ws_closed();
    })
    .into_response()
}

fn not_loaded(scene_name: &str, why: Option<&str>) -> axum::response::Response {
    let body = match why {
        Some(why) => format!("{scene_name} is not currently loaded in the server: {why}"),
        None => format!("{scene_name} is not currently loaded in the server"),
    };
    (StatusCode::NOT_FOUND, body).into_response()
}

async fn handle_socket(
    state: &SceneHostState,
    socket: WebSocket,
    target: Target,
    paths: Vec<String>,
    caller_ip: String,
) {
    let (mut sink, mut stream) = socket.split();
    let identity = target.identity().clone();
    drop(target);
    let name = identity.key.room_key();

    let auth = tokio::time::timeout(state.settings.auth_timeout, async {
        while let Some(Ok(msg)) = stream.next().await {
            if let Message::Binary(bytes) = msg
                && let Some((MessageType::Auth, body)) = decode_message(&bytes)
            {
                let now = chrono::Utc::now().timestamp();
                return Some(verify_auth_frame(body, "GET", &paths, now, state.eip1654()).await);
            }
        }
        None
    })
    .await;

    let authed = match auth {
        Ok(Some(Ok(a))) => a,
        Ok(Some(Err(e))) => {
            tracing::debug!(scene = %name, error = %e, "ws auth failed");
            let _ = sink.close().await;
            return;
        }
        _ => {
            tracing::debug!(scene = %name, "ws auth missing or timed out");
            let _ = sink.close().await;
            return;
        }
    };

    let caller = if caller_ip.is_empty() {
        authed.signer.as_str()
    } else {
        &caller_ip
    };
    let now = chrono::Utc::now().naive_utc();
    // Resolve authority without a secret first. Store failures close below, and a
    // world that changes to shared-secret access cannot skip its attempt budget.
    let mut admission = admit(state, &identity, &authed.signer, None, None, now).await;
    if matches!(admission, Err(AdmissionRefused::InvalidSecret))
        || (matches!(admission, Err(AdmissionRefused::Undetermined(_)))
            && authed.presented_secret().is_some())
    {
        let SceneKey::World(world) = &identity.key else {
            let _ = sink.close().await;
            return;
        };
        let throttle = crate::comms::gatekeeper::admission::world_mint::SecretThrottle {
            pool: &state.pool,
            max_attempts: state.secret_max_attempts,
            world: world.key(),
            subject: caller,
        };
        let reservation = match throttle.reserve(now).await {
            Ok(Some(reservation)) => reservation,
            _ => {
                let _ = sink.close().await;
                return;
            }
        };
        admission = admit(
            state,
            &identity,
            &authed.signer,
            authed.presented_secret(),
            None,
            now,
        )
        .await;
        throttle
            .settle(
                matches!(admission, Err(AdmissionRefused::InvalidSecret)),
                reservation,
            )
            .await;
    }
    let grant = match admission {
        Ok(grant) => grant,
        Err(reason) => {
            tracing::info!(scene = %name, address = %authed.signer, %reason, "ws admission refused");
            let _ = sink.close().await;
            return;
        }
    };

    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(state.settings.limits.client_outbound_max.max(1));
    let address = authed.signer.as_str().to_string();
    let joined = state
        .join(&identity, |scene| {
            scene.add_client_admitted(address, tx, grant)
        })
        .await;
    let (scene, (client, init)) = match joined {
        Ok(joined) => joined,
        Err(e) => {
            tracing::info!(scene = %name, error = %e, "join refused");
            let _ = sink.close().await;
            return;
        }
    };
    let index = client.index;
    let meter = init.outbound_meter.clone();

    if init.size == 0 {
        tracing::error!(scene = %scene.name, index, address = %authed.signer, "no entity range available; refusing the connection");
        scene.remove_connection(index, client.id);
        let _ = sink.close().await;
        return;
    }

    let init_frame = encode_init_message(
        &init.crdt_state,
        init.start,
        init.size,
        init.reserved_local_entities,
    );
    if sink.send(Message::Binary(init_frame.into())).await.is_err() {
        scene.remove_connection(index, client.id);
        return;
    }
    tracing::info!(scene = %scene.name, index, address = %authed.signer, ?grant, "ws authenticated");

    loop {
        tokio::select! {
            _ = client.cancel.cancelled() => {
                tracing::debug!(scene = %scene.name, index, "ws closed by the host");
                let _ = sink.close().await;
                return;
            }

            queued = rx.recv() => {
                let Some(frame) = queued else { break };
                let len = frame.len();
                let sent = sink.send(Message::Binary(frame.into())).await;
                if let Some(meter) = &meter {
                    let _ = meter.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| Some(n.saturating_sub(len)));
                }
                if sent.is_err() {
                    break;
                }
            }

            incoming = stream.next() => {
                let Some(Ok(msg)) = incoming else { break };
                match msg {
                    Message::Binary(bytes) => {
                        if let Some((MessageType::Crdt, body)) = decode_message(&bytes) {
                            if body.is_empty() { continue; }
                            let Ok(outbound) = scene.runtime.on_client_crdt(index, body) else {
                                tracing::info!(scene = %scene.name, index, "client over its inbound byte budget; closing");
                                break;
                            };
                            for out in outbound {
                                let frame = encode_message(MessageType::Crdt, &out);
                                scene.broadcast(&frame, index);
                            }
                        }
                    }
                    Message::Ping(p) => { let _ = sink.send(Message::Pong(p)).await; }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        }
    }

    // A kicked connection's index may already belong to someone else.
    scene.remove_connection(index, client.id);
    tracing::debug!(scene = %scene.name, index, "ws closed");
}
