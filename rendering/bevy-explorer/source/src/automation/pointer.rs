//! `/hover_entity` and `/click_entity`: hover and click a scene entity by id through the
//! production pointer pipeline, on native and web alike. A command sets a synthetic aim
//! (`SyntheticPointer`) at the entity, so the pointer ray goes from the camera to it whatever the
//! cursor does; a click presses the bound key or mouse button through the input script, so the
//! press reaches the scene exactly as a person's would. `drive_pointer_job` follows the job frame
//! by frame with [`ClickMachine`] and answers with what the pipeline did:
//!
//! `{"hit": {"entity", "container", "scene", "playerDistance", "cameraDistance"} | null,
//! "entries": [HoverEntryReport], "events": [{"event", "button", "tick"}], "diagnosis": string |
//! null}`
//!
//! A diagnosis (the entity is blocked, out of reach of the ray, gone, the HUD holds the input, a
//! stage timed out) still answers with that JSON; a refusal (a press is already held, a gamepad
//! binding, nothing to release) fails the command.

use std::collections::HashSet;

use bevy::{
    diagnostic::FrameCount,
    ecs::system::SystemParam,
    input::{
        keyboard::{Key, NativeKey},
        ButtonState,
    },
    prelude::*,
    render::primitives::Aabb,
};
use bevy_console::ConsoleCommand;
use common::{
    inputs::{CommonInputAction, InputIdentifier, InputMap},
    rpc::{RpcResultReceiver, RpcResultSender},
    sets::SceneSets,
};
use console::{DoAddConsoleCommand, PendingConsoleResponses};
use dcl_component::{proto_components::sdk::components::common::PointerEventType, SceneEntityId};
use input_manager::{InputManager, InputPriority};
use scene_inspector::{parse_entity_id, SceneResolver};
use scene_runner::{
    renderer_context::RendererSceneContext,
    update_scene::{
        pointer_results::{PointerTarget, PointerTargetInfo},
        synthetic_pointer::{
            anchor_point, hover_entries, Anchor, Hold, HoverEntryReport, PointerEventLog,
            SyntheticAim, SyntheticPointer,
        },
    },
    update_world::pointer_events::PointerEvents,
    ContainerEntity, SceneEntity,
};
use serde::Serialize;

use super::input::{key_from_code, InputScript, Step};

/// Frames a hover lasts without `--frames`.
const DEFAULT_HOVER_FRAMES: u32 = 30;
/// Frames a job may take without `--timeout-frames`.
const DEFAULT_TIMEOUT_FRAMES: u32 = 600;
/// The id a hit reports when it is no entity of the job's scene (an avatar, another scene).
const FOREIGN_HIT: u32 = u32::MAX;

mod machine;
pub use machine::{
    binding_error, check_hover_frames, refuse_while_held, ClickKind, ClickMachine, Effect,
    Observed, TargetSeen,
};
use machine::{ENTITY_NOT_LIVE, HUD_HOLDS_INPUT};

pub(crate) struct AutomationPointerPlugin;

impl Plugin for AutomationPointerPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PointerJob>()
            .add_console_command::<HoverEntityCommand, _>(hover_entity_cmd)
            .add_console_command::<ClickEntityCommand, _>(click_entity_cmd)
            .add_systems(
                Update,
                (settle_release, drive_pointer_job)
                    .chain()
                    .after(SceneSets::Input),
            );
    }
}

/// The input script step pressing or releasing `binding`; `None` for a gamepad binding.
fn binding_step(binding: &InputIdentifier, state: ButtonState) -> Option<Step> {
    match *binding {
        InputIdentifier::Key(key_code) => {
            // bevy names key codes as the DOM does, so the press carries the key and text a
            // keyboard would; a key without a DOM name carries none
            let (logical_key, text) = key_from_code(&format!("{key_code:?}"))
                .filter(|(code, ..)| *code == key_code)
                .map(|(_, key, text)| (key, text))
                .unwrap_or((Key::Unidentified(NativeKey::Unidentified), None));
            Some(Step::Key {
                key_code,
                logical_key,
                text,
                state,
            })
        }
        InputIdentifier::Mouse(button) => Some(Step::Button(button, state)),
        InputIdentifier::Gamepad(_) | InputIdentifier::Analog(..) => None,
    }
}

// --- the reply ---

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct HitReport {
    entity: u32,
    container: u32,
    scene: String,
    player_distance: f32,
    camera_distance: f32,
}

#[derive(Debug, Clone, Serialize)]
struct EventReport {
    event: String,
    button: String,
    tick: u32,
}

#[derive(Serialize)]
struct Reply<'a> {
    hit: Option<&'a HitReport>,
    entries: &'a [HoverEntryReport],
    events: &'a [EventReport],
    diagnosis: Option<String>,
}

fn reply_json(
    hit: Option<&HitReport>,
    entries: &[HoverEntryReport],
    events: &[EventReport],
    diagnosis: Option<String>,
) -> String {
    serde_json::to_string(&Reply {
        hit,
        entries,
        events,
        diagnosis,
    })
    .unwrap_or_default()
}

// --- the job ---

/// The running job, the press a finished `--event down` left held, and a release still to land
/// before the aim ends.
#[derive(Resource, Default)]
pub struct PointerJob {
    pub job: Option<Job>,
    held: Option<Held>,
    releasing: Option<Releasing>,
}

impl PointerJob {
    /// The refusal while a job runs or its release has not landed yet.
    fn busy(&self) -> Option<String> {
        let entity = self
            .job
            .as_ref()
            .map(Job::requested)
            .or(self.releasing.as_ref().map(|releasing| releasing.entity))?;
        Some(format!(
            "a pointer command is still running on entity {entity}; wait for its reply or send /hover_entity --clear"
        ))
    }
}

type PendingAnswer = (
    RpcResultSender<Result<String, String>>,
    Result<String, String>,
);

/// A queued release: the aim stays on its entity until the release has been played and the
/// pointer pipeline has run after it, then the aim ends and the replies go out.
struct Releasing {
    /// the proto id the refusal names
    entity: u32,
    /// the aimed entity whose aim ends; `None` ends any aim
    target: Option<Entity>,
    /// the input script step number of the release
    step: u64,
    replies: Vec<PendingAnswer>,
}

/// A press left held by `--event down`, with the aim kept open on its entity.
struct Held {
    scene: Entity,
    entity: SceneEntityId,
    target: Entity,
    binding: InputIdentifier,
}

pub struct Job {
    machine: ClickMachine,
    reply: RpcResultSender<Result<String, String>>,
    /// the pressed binding; `None` for a hover
    binding: Option<InputIdentifier>,
    scene: Entity,
    entity: SceneEntityId,
    target: Entity,
    started: u32,
    // what the frames since the start saw, so an aim that expires loses nothing
    hit: Option<HitReport>,
    entries: Vec<HoverEntryReport>,
    events: Vec<EventReport>,
    seen_events: HashSet<(i32, i32, u32)>,
    /// the hit entity of the latest target, whose events count with the asked entity's
    hit_entity: Option<u32>,
    /// the input script step number of the release the machine asked for
    release_step: Option<u64>,
}

impl Job {
    fn requested(&self) -> u32 {
        self.entity.as_proto_u32().unwrap_or_default()
    }

    fn release_binding(&self) -> Option<InputIdentifier> {
        self.binding.filter(|_| self.machine.holds_press())
    }
}

/// Ends the aim on `target` (any aim for `None`) and sends `answer`: at once, or, with a press to
/// release, once the release has been played and the pointer pipeline has run after it
/// ([`settle_release`]), so the release lands on the aimed entity and no new command takes the aim
/// before it does.
#[allow(clippy::too_many_arguments)]
fn wind_down(
    jobs: &mut PointerJob,
    synthetic: &mut SyntheticPointer,
    log: &mut PointerEventLog,
    script: &mut InputScript,
    release: Option<InputIdentifier>,
    entity: u32,
    target: Option<Entity>,
    answer: PendingAnswer,
) {
    let step = release
        .and_then(|binding| binding_step(&binding, ButtonState::Released))
        .map(|step| script.push(step));
    match (step, jobs.releasing.as_mut()) {
        (step, Some(releasing)) => {
            // a release is already pending: this answer waits for it too
            if let Some(step) = step {
                releasing.step = releasing.step.max(step);
            }
            releasing.replies.push(answer);
        }
        (Some(step), None) => {
            jobs.releasing = Some(Releasing {
                entity,
                target,
                step,
                replies: vec![answer],
            });
        }
        (None, None) => {
            end_aim(synthetic, log, target);
            answer.0.send(answer.1);
        }
    }
}

/// Ends the aim if it is on `target` (any aim for `None`), and clears the log with it.
fn end_aim(synthetic: &mut SyntheticPointer, log: &mut PointerEventLog, target: Option<Entity>) {
    let ours = synthetic
        .0
        .as_ref()
        .is_some_and(|aim| target.is_none_or(|target| aim.target == target));
    if ours {
        synthetic.0 = None;
        log.0.clear();
    }
}

/// Ends the job, the held press and the aim, answering `reply` once any release has landed.
fn clear_pointer(
    jobs: &mut PointerJob,
    synthetic: &mut SyntheticPointer,
    log: &mut PointerEventLog,
    script: &mut InputScript,
    reply: RpcResultSender<Result<String, String>>,
) {
    let (release, entity) = if let Some(job) = jobs.job.take() {
        let release = job.release_binding();
        let entity = job.requested();
        job.reply
            .send(Err("cleared by /hover_entity --clear".to_owned()));
        (release, entity)
    } else {
        let held = jobs.held.take();
        let entity = held
            .as_ref()
            .and_then(|held| held.entity.as_proto_u32())
            .unwrap_or_default();
        (held.map(|held| held.binding), entity)
    };
    jobs.held = None;
    wind_down(
        jobs,
        synthetic,
        log,
        script,
        release,
        entity,
        None,
        (reply, Ok("cleared".to_owned())),
    );
}

/// Once the queued release has been played (in this frame's `PreUpdate`, so this `Update`'s
/// pointer pipeline has already run after it), ends the aim and sends the replies that waited.
fn settle_release(
    mut jobs: ResMut<PointerJob>,
    script: Res<InputScript>,
    mut synthetic: ResMut<SyntheticPointer>,
    mut log: ResMut<PointerEventLog>,
) {
    let landed = jobs
        .releasing
        .as_ref()
        .is_some_and(|releasing| script.played() >= releasing.step);
    let Some(releasing) = jobs.releasing.take_if(|_| landed) else {
        return;
    };
    end_aim(&mut synthetic, &mut log, releasing.target);
    for (reply, answer) in releasing.replies {
        reply.send(answer);
    }
}

// --- the commands ---

/// Hover a scene entity of the active scene through the pointer pipeline, for --frames frames,
/// and answer, as JSON, with the hit, the hover entries, the pointer events the scene got and a
/// diagnosis when the entity was not hovered. --frames is at most 1800, and a hover may always
/// take its frames plus 3 whatever --timeout-frames says. --clear ends any aim and releases a held
/// press
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/hover_entity")]
pub struct HoverEntityCommand {
    /// Entity id (u32, as /scene_entities lists it) or alias: root, player, camera
    entity: Option<String>,
    /// Aim at the centre of the bounds of the visible meshes of the entity and its descendants,
    /// rather than at its origin (the origin when it has none)
    #[arg(long)]
    bounds: bool,
    /// Frames to hover for, at most 1800
    #[arg(long, default_value_t = DEFAULT_HOVER_FRAMES)]
    frames: u32,
    /// Frames the hover may take in all; never fewer than --frames plus 3
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_FRAMES)]
    timeout_frames: u32,
    /// End any aim, releasing a held press, instead of hovering
    #[arg(long)]
    clear: bool,
}

/// Click a scene entity of the active scene through the pointer pipeline, pressing the key or
/// mouse button bound to the action, and answer, as JSON, with the hit, the hover entries, the
/// pointer events the scene got and a diagnosis when the click did not land. `--event down` keeps
/// the press held until `--event up` or /hover_entity --clear
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/click_entity")]
pub struct ClickEntityCommand {
    /// Entity id (u32, as /scene_entities lists it) or alias: root, player, camera
    entity: String,
    /// Aim at the centre of the bounds of the visible meshes of the entity and its descendants,
    /// rather than at its origin (the origin when it has none)
    #[arg(long)]
    bounds: bool,
    /// The action to press: pointer, primary or secondary
    #[arg(long, default_value = "pointer")]
    button: String,
    /// click (down then up), down or up
    #[arg(long, default_value = "click")]
    event: String,
    /// Frames the click may take in all
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_FRAMES)]
    timeout_frames: u32,
}

/// How a command answers.
enum Answer {
    Failed(String),
    Json(String),
    Pending(RpcResultReceiver<Result<String, String>>),
}

fn answer<T: bevy_console::Command>(
    input: &mut ConsoleCommand<T>,
    pending: &mut PendingConsoleResponses,
    answer: Answer,
) {
    match answer {
        Answer::Failed(message) => input.reply_failed(message),
        Answer::Json(json) => input.reply_ok(json),
        Answer::Pending(rx) => pending.push_receiver(rx, |r| r, input.take_responder()),
    }
}

fn diagnosed(diagnosis: &str) -> Answer {
    Answer::Json(reply_json(None, &[], &[], Some(diagnosis.to_owned())))
}

/// What both commands share: the scene, the aim, the job and the input script.
#[derive(SystemParam)]
pub(crate) struct PointerControl<'w, 's> {
    resolver: SceneResolver<'w, 's>,
    synthetic: ResMut<'w, SyntheticPointer>,
    log: ResMut<'w, PointerEventLog>,
    jobs: ResMut<'w, PointerJob>,
    script: ResMut<'w, InputScript>,
    frame: Res<'w, FrameCount>,
    pending: ResMut<'w, PendingConsoleResponses>,
    transforms: Query<'w, 's, &'static GlobalTransform>,
    meshes: Query<
        'w,
        's,
        (
            &'static Aabb,
            &'static GlobalTransform,
            &'static InheritedVisibility,
        ),
    >,
    children: Query<'w, 's, &'static Children>,
}

/// A job a command asks for.
struct Request<'a> {
    command: &'a str,
    entity: &'a str,
    kind: ClickKind,
    bounds: bool,
    timeout: u32,
    /// the binding to press, `None` for a hover and an up (which releases the held one)
    binding: Option<InputIdentifier>,
    /// the HUD holds the binding's input
    hud_holds: bool,
}

impl PointerControl<'_, '_> {
    fn start(&mut self, request: Request) -> Answer {
        let id = match parse_entity_id(request.entity) {
            Ok(id) => id,
            Err(e) => return Answer::Failed(e),
        };
        let requested = id.as_proto_u32().unwrap_or_default();
        if let Some(refusal) = self.jobs.busy() {
            return Answer::Failed(refusal);
        }
        let held_on = self
            .jobs
            .held
            .as_ref()
            .map(|held| held.entity.as_proto_u32().unwrap_or_default());
        if request.kind == ClickKind::Up {
            if held_on.is_none() {
                return Answer::Failed("no press is held".to_owned());
            }
            if held_on != Some(requested) {
                return Answer::Failed(
                    refuse_while_held(held_on, request.command).unwrap_or_default(),
                );
            }
        } else if let Some(refusal) = refuse_while_held(held_on, request.command) {
            return Answer::Failed(refusal);
        }

        let frame = self.frame.0;
        let (scene, target, binding) = if let Some(held) = self.jobs.held.take() {
            // an up releases the held press on the aim the down left open
            (held.scene, held.target, Some(held.binding))
        } else {
            let (scene, ctx) = match self.resolver.resolve() {
                Ok(resolved) => resolved,
                Err(e) => return Answer::Failed(e),
            };
            let Some(target) = ctx.bevy_entity(id) else {
                return diagnosed(ENTITY_NOT_LIVE);
            };
            if request.hud_holds {
                return diagnosed(HUD_HOLDS_INPUT);
            }
            let bounds = request.bounds
                && anchor_point(
                    target,
                    Anchor::Bounds,
                    &self.transforms,
                    &self.meshes,
                    &self.children,
                )
                .is_some();
            let anchor = if bounds {
                Anchor::Bounds
            } else {
                Anchor::Origin
            };
            let hold = match request.kind {
                ClickKind::Hover(frames) => Hold::Frames(frames.saturating_add(2)),
                _ => Hold::Open,
            };
            self.synthetic.0 = Some(SyntheticAim {
                scene,
                entity: id,
                target,
                anchor,
                hold,
                started_frame: frame,
            });
            (scene, target, request.binding)
        };
        self.log.0.clear();

        let (reply, rx) = RpcResultSender::channel();
        self.jobs.job = Some(Job {
            machine: ClickMachine::new(request.kind, requested, request.timeout, frame),
            reply,
            binding,
            scene,
            entity: id,
            target,
            started: frame,
            hit: None,
            entries: Vec::new(),
            events: Vec::new(),
            seen_events: HashSet::new(),
            hit_entity: None,
            release_step: None,
        });
        Answer::Pending(rx)
    }

    /// Ends a running job, the held press and the aim; answers once any release has landed.
    fn clear(&mut self) -> Answer {
        let (reply, rx) = RpcResultSender::channel();
        clear_pointer(
            &mut self.jobs,
            &mut self.synthetic,
            &mut self.log,
            &mut self.script,
            reply,
        );
        Answer::Pending(rx)
    }
}

fn hover_entity_cmd(mut input: ConsoleCommand<HoverEntityCommand>, mut control: PointerControl) {
    let Some(Ok(command)) = input.take() else {
        return;
    };
    let result = if command.clear {
        control.clear()
    } else if let Err(refusal) = check_hover_frames(command.frames) {
        Answer::Failed(refusal)
    } else if let Some(entity) = command.entity.as_deref() {
        control.start(Request {
            command: "/hover_entity",
            entity,
            kind: ClickKind::Hover(command.frames),
            bounds: command.bounds,
            timeout: command.timeout_frames,
            binding: None,
            hud_holds: false,
        })
    } else {
        Answer::Failed("expected an entity id, or --clear".to_owned())
    };
    answer(&mut input, &mut control.pending, result);
}

fn click_entity_cmd(
    mut input: ConsoleCommand<ClickEntityCommand>,
    mut control: PointerControl,
    input_map: Res<InputMap>,
    input_manager: InputManager,
) {
    let Some(Ok(command)) = input.take() else {
        return;
    };
    let result = click_request(&command, &mut control, &input_map, &input_manager);
    answer(&mut input, &mut control.pending, result);
}

fn click_request(
    command: &ClickEntityCommand,
    control: &mut PointerControl,
    input_map: &InputMap,
    input_manager: &InputManager,
) -> Answer {
    let kind = match command.event.as_str() {
        "click" => ClickKind::Click,
        "down" => ClickKind::Down,
        "up" => ClickKind::Up,
        other => return Answer::Failed(format!("unknown event `{other}`: use click, down or up")),
    };
    let action = match command.button.as_str() {
        "pointer" => CommonInputAction::IaPointer,
        "primary" => CommonInputAction::IaPrimary,
        "secondary" => CommonInputAction::IaSecondary,
        other => {
            return Answer::Failed(format!(
                "unknown button `{other}`: use pointer, primary or secondary"
            ));
        }
    };
    let (binding, hud_holds) = if kind == ClickKind::Up {
        (None, false)
    } else {
        let Some(binding) = input_map.get_input(action) else {
            return Answer::Failed(format!("{} has no binding", command.button));
        };
        if let Some(refusal) = binding_error(&command.button, &binding) {
            return Answer::Failed(refusal);
        }
        let hud_holds = !input_manager.check_priority(&binding, InputPriority::Scene);
        (Some(binding), hud_holds)
    };
    control.start(Request {
        command: "/click_entity",
        entity: &command.entity,
        kind,
        bounds: command.bounds,
        timeout: command.timeout_frames,
        binding,
        hud_holds,
    })
}

// --- the driver ---

/// The scene ids of a hit container: (entity, container, the scene it belongs to).
fn hit_ids(
    container: Entity,
    ids: &Query<(Option<&SceneEntity>, Option<&ContainerEntity>)>,
) -> Option<(SceneEntityId, SceneEntityId, Entity)> {
    let (scene_entity, container_entity) = ids.get(container).ok()?;
    let (entity, root) = match (scene_entity, container_entity) {
        (Some(se), _) => (se.id, se.root),
        (None, Some(ce)) => (ce.container_id, ce.root),
        (None, None) => return None,
    };
    let container_id = container_entity.map_or(entity, |ce| ce.container_id);
    Some((entity, container_id, root))
}

/// The target as the machine compares it; a hit outside `scene` matches no entity of it.
fn target_seen(
    info: &PointerTargetInfo,
    scene: Entity,
    ids: &Query<(Option<&SceneEntity>, Option<&ContainerEntity>)>,
    parents: &Query<&ChildOf>,
) -> TargetSeen {
    let proto = |id: SceneEntityId| id.as_proto_u32().unwrap_or_default();
    let Some((entity, container, root)) = hit_ids(info.container, ids).filter(|ids| ids.2 == scene)
    else {
        return TargetSeen {
            entity: FOREIGN_HIT,
            container: FOREIGN_HIT,
            ancestors_hit: Vec::new(),
        };
    };
    let mut ancestors_hit = Vec::new();
    let mut next = parents.get(info.container).ok().map(ChildOf::parent);
    while let Some(ancestor) = next.filter(|&e| e != root) {
        if let Ok((Some(se), _)) = ids.get(ancestor) {
            if se.id == SceneEntityId::ROOT {
                break;
            }
            ancestors_hit.push(proto(se.id));
        }
        next = parents.get(ancestor).ok().map(ChildOf::parent);
    }
    TargetSeen {
        entity: proto(entity),
        container: proto(container),
        ancestors_hit,
    }
}

/// Follows the running job each frame after the pointer pipeline: snapshots what the pipeline
/// saw, steps the machine, plays its presses and releases, and answers when it finishes.
#[allow(clippy::too_many_arguments)]
fn drive_pointer_job(
    mut jobs: ResMut<PointerJob>,
    frame: Res<FrameCount>,
    pointer: Res<PointerTarget>,
    mut synthetic: ResMut<SyntheticPointer>,
    mut log: ResMut<PointerEventLog>,
    mut script: ResMut<InputScript>,
    scenes: Query<&RendererSceneContext>,
    ids: Query<(Option<&SceneEntity>, Option<&ContainerEntity>)>,
    parents: Query<&ChildOf>,
    pointer_events: Query<&PointerEvents>,
) {
    let frame = frame.0;
    let Some(job) = jobs.job.as_mut() else {
        return;
    };

    // the target counts only while this job's aim drives the pointer
    let aimed = frame > job.started
        && synthetic
            .0
            .as_ref()
            .is_some_and(|aim| aim.target == job.target);
    let seen = pointer
        .0
        .as_ref()
        .filter(|_| aimed)
        .map(|info| (info, target_seen(info, job.scene, &ids, &parents)));
    if aimed && seen.is_none() {
        // the target is gone: what an earlier frame saw no longer stands
        job.hit = None;
        job.entries.clear();
    }
    if let Some((info, seen)) = &seen {
        let scene_title = hit_ids(info.container, &ids)
            .and_then(|(_, _, root)| scenes.get(root).ok())
            .map(|ctx| ctx.title.clone())
            .unwrap_or_default();
        job.hit = Some(HitReport {
            entity: seen.entity,
            container: seen.container,
            scene: scene_title,
            player_distance: info.distance.0,
            camera_distance: info.camera_distance.0,
        });
        job.entries = pointer_events
            .get(info.container)
            .map(|pe| hover_entries(info, pe))
            .unwrap_or_default();
        job.hit_entity = Some(seen.entity);
    }

    let requested = job.requested();
    for logged in &log.0 {
        let id = logged.entity.as_proto_u32().unwrap_or_default();
        if id != requested && Some(id) != job.hit_entity {
            continue;
        }
        if job
            .seen_events
            .insert((logged.event as i32, logged.button as i32, logged.tick))
        {
            job.events.push(EventReport {
                event: format!("{:?}", logged.event),
                button: format!("{:?}", logged.button),
                tick: logged.tick,
            });
        }
    }
    let logged = |event: PointerEventType| {
        let name = format!("{event:?}");
        job.events.iter().any(|e| e.event == name)
    };
    let ctx = scenes.get(job.scene).ok();
    let target = seen.map(|(_, seen)| seen);
    let observed = Observed {
        target,
        scene_tick: ctx.map(|ctx| ctx.tick_number),
        down_logged: logged(PointerEventType::PetDown),
        up_logged: logged(PointerEventType::PetUp),
        release_played: job.release_step.is_some_and(|step| script.played() >= step),
        entity_live: ctx.and_then(|ctx| ctx.bevy_entity(job.entity)) == Some(job.target),
    };

    let mut finished = None;
    for effect in job.machine.step(frame, observed) {
        match effect {
            Effect::Press | Effect::Release => {
                let state = if effect == Effect::Press {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                };
                if let Some(step) = job
                    .binding
                    .and_then(|binding| binding_step(&binding, state))
                {
                    let step = script.push(step);
                    if effect == Effect::Release {
                        job.release_step = Some(step);
                    }
                }
            }
            Effect::Finish(result) => finished = Some(result),
        }
    }
    let Some(result) = finished else {
        return;
    };

    let Some(job) = jobs.job.take() else {
        return;
    };
    let diagnosis = result.err();
    let reply = Ok(reply_json(
        job.hit.as_ref(),
        &job.entries,
        &job.events,
        diagnosis.clone(),
    ));
    match job.binding {
        // a down that landed keeps its press and its open aim
        Some(binding)
            if diagnosis.is_none()
                && job.machine.kind() == ClickKind::Down
                && job.machine.holds_press() =>
        {
            jobs.held = Some(Held {
                scene: job.scene,
                entity: job.entity,
                target: job.target,
                binding,
            });
            job.reply.send(reply);
        }
        // anything else ends its aim, releasing a press it still holds first
        _ => {
            let release = job.release_binding();
            let entity = job.requested();
            wind_down(
                &mut jobs,
                &mut synthetic,
                &mut log,
                &mut script,
                release,
                entity,
                Some(job.target),
                (job.reply, reply),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use bevy::{
        ecs::system::SystemState,
        input::{keyboard::KeyboardInput, mouse::MouseButtonInput},
    };

    use super::super::input::play_input_script;
    use super::*;

    type Control<'w> = (
        ResMut<'w, PointerJob>,
        ResMut<'w, SyntheticPointer>,
        ResMut<'w, PointerEventLog>,
        ResMut<'w, InputScript>,
    );

    /// The input script, the aim, the log, the job and the driver; no window and no scene.
    fn app() -> App {
        let mut app = App::new();
        app.add_event::<KeyboardInput>()
            .add_event::<MouseButtonInput>()
            .init_resource::<InputScript>()
            .init_resource::<SyntheticPointer>()
            .init_resource::<PointerEventLog>()
            .init_resource::<PointerJob>()
            .init_resource::<PointerTarget>()
            .insert_resource(FrameCount(0))
            .add_systems(PreUpdate, play_input_script)
            .add_systems(Update, (settle_release, drive_pointer_job).chain());
        app
    }

    #[test]
    fn clear_answers_and_frees_the_aim_only_once_the_release_has_played() {
        let mut app = app();
        let scene = app.world_mut().spawn_empty().id();
        let target = app.world_mut().spawn_empty().id();
        let entity = SceneEntityId::new(7, 0);
        app.world_mut().resource_mut::<SyntheticPointer>().0 = Some(SyntheticAim {
            scene,
            entity,
            target,
            anchor: Anchor::Origin,
            hold: Hold::Open,
            started_frame: 0,
        });
        app.world_mut().resource_mut::<PointerJob>().held = Some(Held {
            scene,
            entity,
            target,
            binding: InputIdentifier::Mouse(MouseButton::Left),
        });
        // a step queued ahead (another command's), so the release plays a frame later
        app.world_mut()
            .resource_mut::<InputScript>()
            .push(Step::Cursor(Vec2::ZERO));

        let (reply, mut rx) = RpcResultSender::channel();
        let mut state = SystemState::<Control>::new(app.world_mut());
        let (mut jobs, mut synthetic, mut log, mut script) = state.get_mut(app.world_mut());
        clear_pointer(&mut jobs, &mut synthetic, &mut log, &mut script, reply);
        let refusal = Some(
            "a pointer command is still running on entity 7; wait for its reply or send /hover_entity --clear"
                .to_owned(),
        );

        // before anything played, and after the step ahead of the release: refused, aim kept
        for frame in 0..2 {
            assert_eq!(app.world().resource::<PointerJob>().busy(), refusal);
            assert!(
                app.world().resource::<SyntheticPointer>().0.is_some(),
                "frame {frame}"
            );
            assert!(rx.try_recv().is_err(), "frame {frame}");
            app.update();
        }

        // the release played in the last update, and the pipeline ran after it
        assert_eq!(app.world().resource::<InputScript>().played(), 2);
        assert_eq!(app.world().resource::<PointerJob>().busy(), None);
        assert!(app.world().resource::<SyntheticPointer>().0.is_none());
        assert_eq!(rx.try_recv().ok(), Some(Ok("cleared".to_owned())));
    }

    #[test]
    fn clear_with_nothing_held_answers_at_once() {
        let mut app = app();
        let (reply, mut rx) = RpcResultSender::channel();
        let mut state = SystemState::<Control>::new(app.world_mut());
        let (mut jobs, mut synthetic, mut log, mut script) = state.get_mut(app.world_mut());
        clear_pointer(&mut jobs, &mut synthetic, &mut log, &mut script, reply);
        assert_eq!(app.world().resource::<PointerJob>().busy(), None);
        assert_eq!(rx.try_recv().ok(), Some(Ok("cleared".to_owned())));
    }
}
