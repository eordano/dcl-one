use super::*;
use crate::archipelago::control_v4::{AssignmentPayload, LaneKey, V4Error, V4Owner};

pub(super) const POLL_INTERVAL: Duration = Duration::from_secs(2);

pub(super) struct LegacyFence {
    pub owner: V4Owner,
    last_revision: u64,
    delivered: bool,
}

fn realm() -> LaneKey {
    LaneKey::parse("realm").expect("static realm lane")
}

pub(super) async fn reserve(state: &AppState) -> Result<Option<u64>, V4Error> {
    if state.control_v4.is_available() || state.gatekeeper.cluster.control_v4_audience.is_some() {
        state.control_v4.next_connection_epoch().await.map(Some)
    } else {
        Ok(None)
    }
}

impl LegacyFence {
    pub async fn claim(
        state: &AppState,
        address: &str,
        session: &str,
        epoch: u64,
    ) -> Result<(Self, PeerLink, SocketEvents), V4Error> {
        let owner = V4Owner {
            address: address.to_owned(),
            session: session.to_owned(),
            epoch,
        };
        state.control_v4.claim_lanes(&owner, &[realm()]).await?;
        let (link, events) = state.registry.on_v4_peer_connected_with_realm_epoch(
            address,
            session,
            &uuid::Uuid::new_v4().to_string(),
            true,
            epoch,
        );
        Ok((
            Self {
                owner,
                last_revision: 0,
                delivered: false,
            },
            link,
            events,
        ))
    }

    pub async fn poll(
        &mut self,
        state: &AppState,
        socket: &mut WebSocket,
        link: &PeerLink,
    ) -> bool {
        let snapshot = match state.control_v4.snapshot(&self.owner, &realm()).await {
            Ok(snapshot) => snapshot,
            Err(V4Error::AuthorityConflict) => {
                let _ = send_packet(socket, kicked_packet(), Some(link)).await;
                return false;
            }
            Err(_) => {
                state
                    .gatekeeper
                    .announce_session(&self.owner.address, &self.owner.session);
                return false;
            }
        };
        if snapshot.assignment_revision <= self.last_revision && !link.needs_assignment() {
            return true;
        }
        let Some(mut assignment) = snapshot.assignment else {
            link.assignment_cleared();
            return !self.delivered;
        };
        let Some(stamp) = assignment.pulse_admission.take() else {
            link.assignment_cleared();
            state
                .gatekeeper
                .announce_session(&self.owner.address, &self.owner.session);
            return true;
        };
        let valid = state.gatekeeper.verify_island_stamp(&stamp).await;
        if !valid
            || stamp.wallet != self.owner.address
            || stamp.session != self.owner.session
            || crate::rooms::island_room_name(&stamp.cluster_id) != assignment.island_id
        {
            link.assignment_cleared();
            state
                .gatekeeper
                .announce_session(&self.owner.address, &self.owner.session);
            return true;
        }
        let Some(audience) = state.gatekeeper.cluster.control_v4_audience.clone() else {
            return false;
        };
        let token_fence = crate::gatekeeper::admission::cluster::fence::TokenFence {
            audience,
            lane: "realm".into(),
            owner_session: snapshot.owner_session.clone(),
            owner_epoch: snapshot.owner_epoch,
            assignment_revision: snapshot.assignment_revision,
            fencing_token: snapshot.fencing_token,
        };
        if !super::io::send_outgoing(
            socket,
            super::io::Outgoing::ConfiguredAssignment {
                message: axum::extract::ws::Message::Binary(
                    super::craft(server_packet::Message::IslandChanged(project(assignment))).into(),
                ),
                stamp,
                token_fence,
                owner: self.owner.clone(),
            },
            Some(link),
        )
        .await
        {
            return false;
        }
        self.last_revision = snapshot.assignment_revision;
        self.delivered = true;
        true
    }
}

fn project(assignment: AssignmentPayload) -> IslandChangedMessage {
    IslandChangedMessage {
        island_id: assignment.island_id,
        conn_str: assignment.connection_string,
        from_island_id: assignment.from_island_id,
        peers: assignment
            .peers
            .into_iter()
            .filter_map(|(address, value)| position_from_value(value).map(|p| (address, p)))
            .collect(),
        assignment_context: None,
    }
}

pub(super) async fn owns_realm(state: &AppState, owner: &V4Owner) -> Result<bool, V4Error> {
    match tokio::time::timeout(
        AUTHORITY_OPERATION_TIMEOUT,
        state.control_v4.snapshot(owner, &realm()),
    )
    .await
    .unwrap_or(Err(V4Error::AuthorityUnavailable))
    {
        Ok(_) => Ok(true),
        Err(V4Error::AuthorityConflict) => Ok(false),
        Err(error) => Err(error),
    }
}

pub(super) fn position_from_value(
    value: serde_json::Value,
) -> Option<crate::protocol::archipelago::Position> {
    if let Some(array) = value.as_array()
        && array.len() == 3
    {
        return Some(crate::protocol::archipelago::Position {
            x: array.first()?.as_f64()? as f32,
            y: array.get(1)?.as_f64()? as f32,
            z: array.get(2)?.as_f64()? as f32,
        });
    }
    Some(crate::protocol::archipelago::Position {
        x: value.get("x")?.as_f64()? as f32,
        y: value.get("y")?.as_f64()? as f32,
        z: value.get("z")?.as_f64()? as f32,
    })
}
