use alloy_core::primitives::Address;
use bevy::prelude::*;
use bevy_console::ConsoleCommand;
use collectibles::base_wearables;
use common::structs::{AvatarDynamicState, EmoteCommand, EmoteMask, PrimaryUser};
use comms::profile::{ProfileManager, UserProfile};
use console::{AddDebugSection, DebugInput, DebugInputKind, DebugSection, DoAddConsoleCommand};
use dcl_component::proto_components::sdk::components::PbAvatarShape;
use rand::{seq::IteratorRandom, Rng};
use serde_json::json;

use crate::{AvatarSelection, AvatarShape};

const MAX_DEMOS: usize = 128;
const BASE_AVATARS: &str = "urn:decentraland:off-chain:base-avatars:";
const CREATOR_EMOTES: &[(&str, &str)] = &[
    ("None", "none"),
    ("Clap", "clap"),
    ("Cry", "cry"),
    ("Dance", "dance"),
    ("Disco", "disco"),
    ("Hammer", "hammer"),
    ("Kiss", "kiss"),
    ("Money", "money"),
    ("Robot", "robot"),
    ("Tektonik", "tektonik"),
    ("Tik", "tik"),
];

pub(super) struct DebugAvatarsPlugin;

impl Plugin for DebugAvatarsPlugin {
    fn build(&self, app: &mut App) {
        if !app
            .world()
            .get_resource::<common::structs::PreviewMode>()
            .is_some_and(|p| p.is_preview)
        {
            return;
        }
        app.init_resource::<Creator>()
            .add_preview_console_command::<DemoCommand, _>(demo_command)
            .add_preview_console_command::<LookCommand, _>(look_command)
            .add_observer(clear_on_logout)
            .add_systems(
                Update,
                (
                    forget_displayed_profile.after(crate::update_base_avatar_shape),
                    fetch_look
                        .after(look_command)
                        .run_if(any_with_component::<PendingProfile>),
                )
                    .chain(),
            )
            .add_debug_section(DebugSection {
                title: "Avatar debug",
                query: "/avatar_demo",
                fields: &[("Spawned demo avatars", "count")],
                actions: &[
                    ("Destroy demo avatars", "/avatar_demo clear"),
                    ("Destroy random number", "/avatar_demo remove-random"),
                    ("Randomize demo wearables", "/avatar_demo randomize"),
                ],
                inputs: &[
                    DebugInput {
                        label: "Force male for random avatars",
                        field: "maleOnly",
                        command: "/avatar_demo male-only {value}",
                        kind: DebugInputKind::Toggle,
                    },
                    DebugInput {
                        label: "Spawn random avatars",
                        field: "",
                        command: "/avatar_demo spawn {value}",
                        kind: DebugInputKind::Number {
                            min: 1.0,
                            max: MAX_DEMOS as f64,
                            step: 1.0,
                        },
                    },
                ],
            })
            .add_debug_section(DebugSection {
                title: "Avatar creator",
                query: "/avatar_demo",
                fields: &[],
                actions: &[
                    ("Instantiate male", "/avatar_demo male"),
                    ("Instantiate female", "/avatar_demo female"),
                    ("Clear wearables", "/avatar_demo wearables"),
                ],
                inputs: &[
                    DebugInput {
                        label: "Wearable URNs (up to three, comma-separated)",
                        field: "wearables",
                        command: "/avatar_demo wearables {value}",
                        kind: DebugInputKind::Text,
                    },
                    DebugInput {
                        label: "Emote for new avatars",
                        field: "emote",
                        command: "/avatar_demo emote {value}",
                        kind: DebugInputKind::Select(CREATOR_EMOTES),
                    },
                ],
            })
            .add_debug_section(DebugSection {
                title: "Profile avatar shape",
                query: "/avatar_look",
                fields: &[
                    ("Displayed profile", "displayed"),
                    ("Requested profile", "requested"),
                    ("Status", "status"),
                ],
                actions: &[("Restore own appearance", "/avatar_look reset")],
                inputs: &[DebugInput {
                    label: "Preview profile address",
                    field: "",
                    command: "/avatar_look {value}",
                    kind: DebugInputKind::Text,
                }],
            });
    }
}

#[derive(Component)]
struct DemoAvatar;

fn clear_on_logout(
    trigger: Trigger<OnRemove, UserProfile>,
    mut commands: Commands,
    primary: Query<(), With<PrimaryUser>>,
    demos: Query<Entity, With<DemoAvatar>>,
) {
    if primary.contains(trigger.target()) {
        if let Ok(mut player) = commands.get_entity(trigger.target()) {
            player.try_remove::<(PendingProfile, DisplayedProfile, MissingProfile)>();
        }
        for demo in &demos {
            commands.entity(demo).try_despawn();
        }
    }
}

#[derive(Default, Resource)]
struct Creator {
    male_only: bool,
    wearables: Vec<String>,
    emote: Option<String>,
    next: u64,
}

/// Create local rendered avatars, configure their look or remove only the test population.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/avatar_demo")]
struct DemoCommand {
    #[command(subcommand)]
    action: Option<DemoAction>,
}

#[derive(clap::Subcommand)]
enum DemoAction {
    Spawn {
        count: usize,
    },
    MaleOnly {
        #[arg(action = clap::ArgAction::Set)]
        enabled: bool,
    },
    Wearables {
        #[arg(value_delimiter = ',')]
        urns: Vec<String>,
    },
    Emote {
        name: String,
    },
    Male,
    Female,
    Clear,
    RemoveRandom,
    Randomize,
}

fn demo_command(
    mut cmd: ConsoleCommand<DemoCommand>,
    mut commands: Commands,
    mut creator: ResMut<Creator>,
    player: Query<&GlobalTransform, With<PrimaryUser>>,
    mut demos: Query<(Entity, &mut AvatarSelection), With<DemoAvatar>>,
) {
    let Some(Ok(args)) = cmd.take() else { return };
    let mut count = demos.iter().len();
    let mut rng = rand::thread_rng();
    match args.action {
        Some(DemoAction::MaleOnly { enabled }) => creator.male_only = enabled,
        Some(DemoAction::Wearables { urns }) => {
            let wearables: Vec<_> = urns.into_iter().filter(|s| !s.is_empty()).collect();
            if wearables.len() > 3 {
                cmd.reply_failed("supply up to three wearable URNs");
                return;
            }
            creator.wearables = wearables;
        }
        Some(DemoAction::Emote { name }) => {
            if !CREATOR_EMOTES.iter().any(|(_, value)| *value == name) {
                cmd.reply_failed("unknown creator emote");
                return;
            }
            creator.emote = (name != "none").then_some(name);
        }
        Some(DemoAction::Clear | DemoAction::RemoveRandom) => {
            let n = if matches!(args.action, Some(DemoAction::Clear)) {
                count
            } else {
                rng.gen_range(0..=count)
            };
            for entity in demos.iter().map(|(e, _)| e).choose_multiple(&mut rng, n) {
                commands.entity(entity).despawn();
            }
            count -= n;
        }
        Some(DemoAction::Randomize) => {
            let catalog = base_wearables::base_wearable_urns();
            for (_, mut selection) in &mut demos {
                selection.shape.0.wearables = catalog
                    .iter()
                    .skip(2)
                    .choose_multiple(&mut rng, 8)
                    .into_iter()
                    .map(ToString::to_string)
                    .collect();
            }
        }
        Some(action @ (DemoAction::Spawn { .. } | DemoAction::Male | DemoAction::Female)) => {
            let n = match action {
                DemoAction::Spawn { count } => count,
                _ => 1,
            };
            if n == 0 || n > MAX_DEMOS.saturating_sub(count) {
                cmd.reply_failed(format!("choose a positive count that keeps the demo population at or below {MAX_DEMOS}"));
                return;
            }
            let Ok(player) = player.single() else {
                cmd.reply_failed("player unavailable");
                return;
            };
            let catalog = base_wearables::base_wearable_urns();
            for _ in 0..n {
                let random = matches!(action, DemoAction::Spawn { .. });
                let male = if random {
                    creator.male_only || rng.gen_bool(0.5)
                } else {
                    matches!(action, DemoAction::Male)
                };
                let wearables = if random {
                    catalog
                        .iter()
                        .skip(2)
                        .choose_multiple(&mut rng, 8)
                        .into_iter()
                        .map(ToString::to_string)
                        .collect()
                } else {
                    creator.wearables.clone()
                };
                let index = creator.next;
                creator.next += 1;
                let slot = index % MAX_DEMOS as u64;
                let offset = Vec3::new(
                    (slot % 12) as f32 * 1.6 - 3.2,
                    0.0,
                    -((slot / 12) as f32) * 1.6 - 2.0,
                );
                let shape = AvatarShape(PbAvatarShape {
                    id: format!("debug-avatar-{index}"),
                    body_shape: Some(format!(
                        "{BASE_AVATARS}{}",
                        if male { "BaseMale" } else { "BaseFemale" }
                    )),
                    wearables,
                    ..default()
                });
                let mut entity = commands.spawn((
                    DemoAvatar,
                    Transform::from_translation(player.translation() + offset),
                    Visibility::Inherited,
                    AvatarDynamicState::default(),
                    AvatarSelection {
                        scene: None,
                        shape,
                        automatic_delete: false,
                        disable_dither: false,
                    },
                ));
                if let Some(name) = &creator.emote {
                    entity.insert(EmoteCommand {
                        urn: format!("urn:decentraland:off-chain:base-emotes:{name}"),
                        timestamp: 1,
                        r#loop: true,
                        mask: EmoteMask::FullBody,
                    });
                }
            }
            count += n;
        }
        None => {}
    }
    cmd.reply_ok(json!({"count":count, "maleOnly":creator.male_only, "wearables":creator.wearables.join(","), "emote":creator.emote.as_deref().unwrap_or("none")}).to_string());
}

/// Temporarily render a fetched profile's appearance on the local player.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/avatar_look")]
struct LookCommand {
    address: Option<String>,
}

#[derive(Component)]
struct PendingProfile(Address);
#[derive(Component)]
struct DisplayedProfile(Address);

fn forget_displayed_profile(
    mut commands: Commands,
    changed: Query<Entity, (Changed<UserProfile>, With<DisplayedProfile>)>,
) {
    for entity in &changed {
        commands.entity(entity).try_remove::<DisplayedProfile>();
    }
}
#[derive(Component)]
struct MissingProfile(Address);

#[allow(clippy::type_complexity)]
fn look_command(
    mut cmd: ConsoleCommand<LookCommand>,
    mut commands: Commands,
    player: Query<
        (
            Entity,
            &UserProfile,
            Option<&PendingProfile>,
            Option<&DisplayedProfile>,
            Option<&MissingProfile>,
        ),
        With<PrimaryUser>,
    >,
) {
    let Some(Ok(args)) = cmd.take() else { return };
    let Ok((entity, own, pending, displayed, missing)) = player.single() else {
        cmd.reply_failed("local profile unavailable");
        return;
    };
    match args.address.as_deref() {
        Some("reset") => {
            let mut shape = AvatarShape::from(own);
            shape.0.name = None;
            commands
                .entity(entity)
                .insert(shape)
                .remove::<(PendingProfile, DisplayedProfile, MissingProfile)>();
            cmd.reply_ok("restored own appearance");
        }
        Some(raw) => match raw.parse::<Address>() {
            Ok(address) => {
                commands
                    .entity(entity)
                    .insert(PendingProfile(address))
                    .remove::<MissingProfile>();
                cmd.reply_ok(format!("loading appearance for {address:#x}"));
            }
            Err(_) => cmd.reply_failed("expected a wallet address or reset"),
        },
        None => {
            let status = if pending.is_some() {
                "loading"
            } else if missing.is_some() {
                "profile unavailable"
            } else if displayed.is_some() {
                "preview"
            } else {
                "own profile"
            };
            cmd.reply_ok(json!({"status":status, "requested":pending.map(|p| p.0).or(missing.map(|p| p.0)).map(|a| format!("{a:#x}")), "displayed":displayed.map(|p| format!("{:#x}",p.0)).unwrap_or_else(|| own.content.eth_address.clone()),
                "unavailable":{"requested":"No pending profile request."}}).to_string());
        }
    }
}

fn fetch_look(
    mut commands: Commands,
    mut profiles: ProfileManager,
    players: Query<(Entity, &UserProfile, &PendingProfile), With<PrimaryUser>>,
) {
    for (entity, own, pending) in &players {
        match profiles.get_data(pending.0) {
            Ok(Some(profile)) => {
                let mut shape = AvatarShape::from(profile);
                shape.0.id.clone_from(&own.content.eth_address);
                shape.0.name = None;
                commands
                    .entity(entity)
                    .remove::<PendingProfile>()
                    .insert((DisplayedProfile(pending.0), shape));
            }
            Ok(None) => {}
            Err(_) => {
                commands
                    .entity(entity)
                    .remove::<PendingProfile>()
                    .insert(MissingProfile(pending.0));
            }
        }
    }
}
