//! `--autopilot-walk-to`: a benchmark run that walks in instead of standing at spawn. Once
//! measuring starts, the player moves in a straight line from where it arrived to the centre of
//! the given parcel at [`WALK_SPEED`], facing it, then stands there for the rest of the run. The
//! scenes on the way load during play: walking never raises the loading backdrop (only a teleport
//! or a realm change does), so their instantiation lands in the measured frames.
//!
//! The walk sets the player's position on the ground plane each frame, after the movement systems
//! and before the camera follows; the physics keeps the height. It needs no input and cannot be
//! stopped by a wall, so every run takes the same path at the same pace.

use bevy::{prelude::*, time::Real};
use common::{
    autopilot::{AutopilotPhase, LOG_TARGET},
    sets::PostUpdateSets,
    structs::{PrimaryCamera, PrimaryUser},
};
use scene_runner::parcel_to_vec3;

/// Metres a second, a jog.
pub const WALK_SPEED: f32 = 6.0;

/// The parcel `--autopilot-walk-to` names; present only with the flag.
#[derive(Resource, Clone, Copy, Debug)]
pub struct AutopilotWalk {
    pub to: IVec2,
}

pub(super) struct AutopilotWalkPlugin;

impl Plugin for AutopilotWalkPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            PostUpdate,
            walk.run_if(resource_exists::<AutopilotWalk>)
                .after(PostUpdateSets::PlayerUpdate)
                .before(PostUpdateSets::CameraUpdate),
        );
    }
}

/// Where the walk is `elapsed` seconds after it began: on the line from `from` to `to` at `speed`,
/// stopping at `to`.
fn walk_position(from: Vec2, to: Vec2, speed: f32, elapsed: f32) -> Vec2 {
    let offset = to - from;
    let distance = offset.length();
    let walked = speed * elapsed;
    // exactly `to` once there: `from + offset` need not equal it in f32
    if walked >= distance {
        return to;
    }
    from + offset * (walked / distance)
}

/// The camera yaw that looks along `direction` (world x, world z): yaw 0 looks along −z.
fn facing_yaw(direction: Vec2) -> f32 {
    (-direction.x).atan2(-direction.y)
}

#[derive(Default)]
struct Walk {
    /// Where the walk began and when (`Time<Real>` seconds).
    began: Option<(Vec2, f64)>,
    arrived: bool,
}

fn walk(
    phase: Res<AutopilotPhase>,
    target: Res<AutopilotWalk>,
    time: Res<Time<Real>>,
    mut player: Query<&mut Transform, With<PrimaryUser>>,
    mut camera: Query<&mut PrimaryCamera>,
    mut state: Local<Walk>,
) {
    if *phase != AutopilotPhase::Measuring || state.arrived {
        return;
    }
    let Ok(mut transform) = player.single_mut() else {
        return;
    };
    // the parcel's centre on the ground plane (world x, world z), where a teleport lands
    let to = parcel_to_vec3(target.to).xz();
    let now = time.elapsed_secs_f64();
    let (from, began) = *state.began.get_or_insert_with(|| {
        let from = transform.translation.xz();
        if let Ok(mut camera) = camera.single_mut() {
            camera.yaw = facing_yaw(to - from);
        }
        info!(
            target: LOG_TARGET,
            "walking to parcel {},{}: {:.0} m at {WALK_SPEED} m/s",
            target.to.x,
            target.to.y,
            from.distance(to)
        );
        (from, now)
    });

    let position = walk_position(from, to, WALK_SPEED, (now - began) as f32);
    transform.translation.x = position.x;
    transform.translation.z = position.y;
    if position == to {
        state.arrived = true;
        info!(
            target: LOG_TARGET,
            "walked to parcel {},{} at {:.1} s",
            target.to.x,
            target.to.y,
            now
        );
    }
}

#[cfg(test)]
mod tests;
