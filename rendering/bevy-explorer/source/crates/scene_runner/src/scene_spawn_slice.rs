//! Instantiation each frame stops a share of the target frame after bevy's scene spawner starts:
//! the spawner writes queued scene instances (glTF models, avatar parts) an entity at a time, then
//! `update_ready_gltfs` processes spawned glTFs a node at a time, within one slice; the rest waits
//! for the next frame. The spawner stops a little before the slice ends, so processing gets time
//! even while spawns are queued. Earlier in the frame, in `PostLoop`, the mesh renderer and
//! materials charge each entity's first mesh and first material to the same slice
//! ([`InstantiationBudget`]), which then ends that much earlier. Behind the loading backdrop and
//! on headless everything runs whole.
//!
//! [`SceneSpawnSlicePlugin`] owns the slice: its deadlines, the charges, how long the spawner and
//! processing take each frame (`spawn_timing`) and the debug panel's Instantiation section.

use std::time::Duration;

use bevy::{
    ecs::system::SystemParam,
    platform::time::Instant,
    prelude::*,
    scene::{scene_spawner_system, SceneSpawner},
};
use common::structs::{NoRenderApp, OutOfWorld, PrimaryUser};

use crate::{update_scene::raycast_budget::UNCAPPED_FRAME, SceneUpdates};

/// Instantiation (the primitives' and materials' charges, then spawning and glTF processing) may
/// spend a quarter of the target frame.
pub const SLICE_DIVISOR: u32 = 4;

/// The last quarter of the slice is kept for glTF processing.
pub const PROCESSING_DIVISOR: u32 = 4;

/// How long instantiation runs each frame: a quarter of the target frame
/// (of 60 fps when the frame rate is uncapped), 4.2 ms at 60 fps. The target, not the measured
/// frame, so a slow frame does not raise the next frame's slice.
pub fn slice(frame_target: Option<Duration>) -> Duration {
    frame_target.unwrap_or(UNCAPPED_FRAME) / SLICE_DIVISOR
}

/// The end of `slice` kept for glTF processing.
pub fn processing_share(slice: Duration) -> Duration {
    slice / PROCESSING_DIVISOR
}

/// When glTF processing stops this frame. `None`: it processes whole.
#[derive(Resource, Default, Clone, Copy, Debug, PartialEq)]
pub struct ProcessingDeadline(pub Option<Instant>);

pub struct SceneSpawnSlicePlugin;

impl Plugin for SceneSpawnSlicePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ProcessingDeadline>()
            .init_resource::<InstantiationCharges>()
            .add_systems(
                SpawnScene,
                slice_scene_spawning.before(scene_spawner_system),
            )
            .add_plugins(spawn_timing::SpawnTimingPlugin);
        if app.world().get_resource::<NoRenderApp>().is_none() {
            app.add_plugins(diagnostics::InstantiationDebugPanelPlugin);
        }
    }
}

/// The estimated cost of what primitives and materials instantiated this frame, in `PostLoop`:
/// the work lands later in the frame (the command flush, `PostUpdate`, the render world), where
/// no clock in those systems sees it. The slice ends that much earlier for spawning and
/// processing.
#[derive(Resource, Default, Clone, Copy, Debug, PartialEq)]
pub struct InstantiationCharges {
    /// Charged so far this frame.
    pub this_frame: Duration,
    /// What the last frame charged in all, for the debug panel.
    pub last_frame: Duration,
}

/// The frame's instantiation slice as `PostLoop` systems see it.
#[derive(SystemParam)]
pub struct InstantiationBudget<'w, 's> {
    charges: ResMut<'w, InstantiationCharges>,
    updates: Res<'w, SceneUpdates>,
    no_render: Option<Res<'w, NoRenderApp>>,
    out_of_world: Query<'w, 's, (), (With<PrimaryUser>, With<OutOfWorld>)>,
}

impl InstantiationBudget<'_, '_> {
    /// This frame's slice; `None` behind the loading backdrop and on headless, where everything
    /// instantiates whole.
    pub fn slice(&self) -> Option<Duration> {
        let whole = self.no_render.is_some() || !self.out_of_world.is_empty();
        (!whole).then(|| slice(self.updates.frame_target))
    }

    /// A system run's admissions.
    pub fn gate(&mut self) -> Gate<'_> {
        Gate {
            slice: self.slice(),
            charges: &mut self.charges.this_frame,
            admitted: 0,
            deferred: 0,
        }
    }

    fn take_charges(&mut self) -> Duration {
        let charged = std::mem::take(&mut self.charges.this_frame);
        self.charges.last_frame = charged;
        charged
    }
}

/// Admits a system run's items against the slice: the run's first item whatever was spent, so
/// every consumer moves each frame; then each item while it fits in what is left.
pub struct Gate<'a> {
    slice: Option<Duration>,
    charges: &'a mut Duration,
    admitted: usize,
    deferred: usize,
}

impl Gate<'_> {
    pub fn admit(&mut self, cost: Duration) -> bool {
        let Some(slice) = self.slice else {
            return true;
        };
        if self.admitted > 0 && *self.charges + cost > slice {
            self.deferred += 1;
            return false;
        }
        *self.charges += cost;
        self.admitted += 1;
        true
    }

    /// Once it has refused an item this run: the rest wait as they are, untouched.
    pub fn is_spent(&self) -> bool {
        self.slice.is_some() && self.deferred > 0
    }

    /// Logs the run under target `instantiation_budget` when it deferred anything.
    pub fn report(&self, consumer: &'static str) {
        if self.deferred > 0 {
            debug!(
                target: "instantiation_budget",
                "{consumer}: {} admitted, {} deferred, {:.2} ms charged this frame",
                self.admitted,
                self.deferred,
                self.charges.as_secs_f64() * 1000.0
            );
        }
    }
}

/// Marks `entity` as waiting for the slice with `marker`, unless it already is (`marked`).
pub fn mark_waiting<M: Component>(
    commands: &mut Commands,
    entity: Entity,
    marker: M,
    marked: bool,
) {
    if !marked {
        commands.entity(entity).try_insert(marker);
    }
}

/// Runs in `SpawnScene`, just before bevy's `scene_spawner_system`.
pub fn slice_scene_spawning(
    mut spawner: ResMut<SceneSpawner>,
    mut processing: ResMut<ProcessingDeadline>,
    mut budget: InstantiationBudget,
) {
    let charged = budget.take_charges();
    let slice = budget.slice();
    let end = slice.map(|slice| Instant::now() + slice.saturating_sub(charged));
    spawner.set_spawn_deadline(
        end.zip(slice)
            .map(|(end, slice)| end - processing_share(slice)),
    );
    processing.0 = end;
}

mod diagnostics;
pub mod spawn_timing;

#[cfg(test)]
pub(crate) mod tests;
