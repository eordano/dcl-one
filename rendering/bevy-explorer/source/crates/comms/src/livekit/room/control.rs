//! Player control of the currently assigned, credentialed LiveKit room.

use bevy::{ecs::system::SystemParam, prelude::*};
use common::structs::NoRenderApp;

use super::{Connecting, LivekitRoom};
use crate::{
    livekit::{ConnectionAvailability, LivekitNetworkMessage, LivekitRuntimeRes, LivekitTransport},
    private_chat::PrivateChatRoom,
    room_connection::RoomSuspended,
    SceneRoom, Transport,
};

impl LivekitNetworkMessage {
    fn discard_queued(&mut self) {
        while self.try_recv().is_ok() {}
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum RoomKind {
    Island,
    Scene,
    Chat,
}

impl RoomKind {
    pub(super) fn for_room(scene: bool, private: bool, transport: bool) -> Option<Self> {
        if private {
            Some(Self::Chat)
        } else if !transport {
            None
        } else if scene {
            Some(Self::Scene)
        } else {
            Some(Self::Island)
        }
    }
}

pub(super) fn client_can_control(no_render: bool) -> bool {
    !no_render && !common::structs::server_mode()
}

pub(super) fn room_control_block(
    policy: ConnectionAvailability,
    no_render: bool,
) -> Option<&'static str> {
    if !client_can_control(no_render) {
        Some("Only a graphical player client can control room connections")
    } else if policy == ConnectionAvailability::Unavailable {
        Some("Room connections are controlled by session policy")
    } else {
        None
    }
}

#[derive(SystemParam)]
pub(super) struct RoomConnections<'w, 's> {
    commands: Commands<'w, 's>,
    policy: Res<'w, State<ConnectionAvailability>>,
    no_render: Option<Res<'w, NoRenderApp>>,
    #[allow(clippy::type_complexity)]
    rooms: Query<
        'w,
        's,
        (
            Entity,
            Has<SceneRoom>,
            Has<PrivateChatRoom>,
            Has<Transport>,
            Has<RoomSuspended>,
            Option<&'static LivekitRoom>,
            &'static crate::room_connection::RoomRecovery,
            Has<crate::room_connection::CredentialRequest>,
            Option<&'static crate::archipelago::IslandOwner>,
        ),
        With<LivekitTransport>,
    >,
}

impl RoomConnections<'_, '_> {
    pub(super) fn set_enabled(
        &mut self,
        kind: Option<RoomKind>,
        enabled: bool,
    ) -> Result<(), &'static str> {
        if let Some(reason) = room_control_block(*self.policy.get(), self.no_render.is_some()) {
            return Err(reason);
        }
        let kind = kind.ok_or("No current room")?;
        let mut matching = self
            .rooms
            .iter()
            .filter(|(_, scene, private, transport, ..)| {
                RoomKind::for_room(*scene, *private, *transport) == Some(kind)
            });
        let (entity, _, _, _, paused, room, recovery, refreshable, island) =
            matching.next().ok_or("No current room")?;
        if matching.next().is_some() {
            return Err("More than one room matches");
        }
        let failed = recovery.failure.is_some();
        if enabled && (paused || failed) {
            if recovery
                .failure
                .is_some_and(|kind| kind != crate::room_connection::FailureKind::Transient)
            {
                return Err("Room requires an access change or replacement credentials");
            }
            self.commands.entity(entity).remove::<RoomSuspended>();
            // While an SDK close is in flight, its Disconnected event will restart the room
            // after retiring participants from the previous connection.
            if failed || room.is_none_or(|room| room.closed) {
                self.commands.entity(entity).remove::<Connecting>();
                super::plugin::restart_room(
                    &mut self.commands,
                    entity,
                    refreshable,
                    island.map(|owner| owner.0),
                );
            }
        } else if !enabled && !paused {
            self.commands
                .entity(entity)
                .insert(RoomSuspended)
                .remove::<Connecting>();
        }
        Ok(())
    }
}

pub(super) struct RoomControlPlugin;

impl Plugin for RoomControlPlugin {
    fn build(&self, app: &mut App) {
        if client_can_control(app.world().contains_resource::<NoRenderApp>()) {
            app.add_observer(close_paused_room)
                .add_systems(Update, discard_paused_messages);
        }
    }
}

// The pause and the completed dial can arrive in either order, including on re-dial
// when the same transport entity already has a (closed) SDK room.
fn close_paused_room(
    trigger: Trigger<OnInsert, (RoomSuspended, LivekitRoom)>,
    rooms: Query<&LivekitRoom, With<RoomSuspended>>,
    runtime: LivekitRuntimeRes,
) {
    if let Ok(room) = rooms.get(trigger.target()) {
        if !room.closed {
            let room = room.room.clone();
            runtime.spawn(async move {
                if let Err(err) = room.close().await {
                    warn!("Unable to close paused room: {err}");
                }
            });
        }
    }
}

fn discard_paused_messages(mut rooms: Query<&mut LivekitNetworkMessage, With<RoomSuspended>>) {
    for mut messages in &mut rooms {
        messages.discard_queued();
    }
}
