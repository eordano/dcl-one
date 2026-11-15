//! Renders profile avatars to PNGs, headless: the node's `social::profile_images`
//! runs one per entity when a profile's own snapshot files are not in its store.
//! The payload file is the one the Godot client's `--avatar-renderer` reads:
//!
//! ```text
//! avatar_render --avatars <payload.json> [--realm <catalyst root>] [--content-server <catalyst root>]
//!               [--settle-frames N] [--max-frames N]
//! ```
//!
//! `payload.json` is `{"baseUrl": <content url>, "payload": [{"entity", "destPath",
//! "width", "height", "faceDestPath", "faceWidth", "faceHeight", "avatar"}]}`, where
//! `avatar` is the profile metadata's `avatars[0].avatar`. Each entry writes a body
//! render to `destPath` and a head render to `faceDestPath`, on a transparent
//! background. The realm defaults to `DCL_PEER_BASE`, else `baseUrl` without its
//! `/content`; the content server (where wearables resolve) defaults to that same
//! root, not the realm's advertised public URL, so a node renders from itself even
//! behind a proxy. Exits 0 once every file is written, 1 when the avatars never
//! finish loading within `--max-frames`, 2 on bad arguments.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use avatar::{
    avatar_texture::{BoothInstance, PhotoBooth},
    AvatarProcessed, AvatarShape,
};
use bevy::{
    app::HierarchyPropagatePlugin,
    asset::RenderAssetUsages,
    core_pipeline::tonemapping::Tonemapping,
    diagnostic::FrameCount,
    prelude::*,
    render::{
        camera::RenderTarget,
        render_resource::{Extent3d, TextureDimension, TextureFormat, TextureUsages},
        view::{
            screenshot::{Screenshot, ScreenshotCaptured},
            ColorGrading, RenderLayers,
        },
    },
};
use common::{
    profile::{AvatarWireFormat, SerializedProfile},
    structs::{AppConfig, GraphicsSettings, SceneDrivenAnimationFeedback, SceneImposterBake},
};
use comms::profile::UserProfile;
use dcl_deno_ipc::init_runtime;
use serde::Deserialize;
use webgpu_build::{offline_app::offline_app, serialized_app_config};

/// Booth avatars start on this layer, one layer per payload entry, so each entry's
/// cameras see only its own avatar.
const FIRST_LAYER: usize = 3;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Payload {
    base_url: String,
    payload: Vec<Entry>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    entity: String,
    dest_path: PathBuf,
    width: u32,
    height: u32,
    face_dest_path: PathBuf,
    face_width: u32,
    face_height: u32,
    avatar: serde_json::Value,
}

struct Shot {
    path: PathBuf,
    target: Handle<Image>,
    transform: Transform,
}

struct Job {
    entity: String,
    shape: AvatarShape,
    layer: RenderLayers,
    shots: [Shot; 2],
}

#[derive(Resource)]
struct Jobs {
    jobs: Vec<Job>,
    settle_frames: u32,
    max_frames: u32,
}

#[derive(Resource)]
struct Booths(#[allow(dead_code)] Vec<BoothInstance>);

#[derive(Resource, Default)]
struct RenderState {
    ready_at: Option<u32>,
    captured: bool,
    written: Arc<Mutex<Vec<Result<PathBuf, String>>>>,
}

/// The profile's avatar as the explorer reads it, with the explorer's defaults for
/// absent colours and body shape. `snapshots` is dropped: it is what this renders,
/// and a deployed profile may carry keys the wire format does not model.
fn shape_of(entity: &str, avatar: &serde_json::Value) -> Result<AvatarShape, String> {
    let mut avatar = avatar.clone();
    if let Some(obj) = avatar.as_object_mut() {
        obj.remove("snapshots");
    }
    let avatar: AvatarWireFormat =
        serde_json::from_value(avatar).map_err(|e| format!("{entity}: avatar: {e}"))?;
    let profile = UserProfile {
        content: SerializedProfile {
            eth_address: entity.to_owned(),
            avatar,
            ..Default::default()
        },
        ..Default::default()
    };
    Ok(AvatarShape::from(&profile))
}

fn target_image(images: &mut Assets<Image>, width: u32, height: u32) -> Handle<Image> {
    let mut image = Image::new_fill(
        Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        &[0, 0, 0, 0],
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::all(),
    );
    image.texture_descriptor.usage |= TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC;
    images.add(image)
}

fn usage(message: &str) -> AppExit {
    eprintln!("avatar_render: {message}");
    AppExit::from_code(2)
}

fn main() -> AppExit {
    let mut args = pico_args::Arguments::from_env();
    let Ok(payload_path) = args.value_from_str::<_, PathBuf>("--avatars") else {
        return usage("--avatars <payload.json> is required");
    };
    let realm: Option<String> = args.opt_value_from_str("--realm").ok().flatten();
    let content_server: Option<String> = args.opt_value_from_str("--content-server").ok().flatten();
    let settle_frames: u32 = args.value_from_str("--settle-frames").unwrap_or(30);
    let max_frames: u32 = args.value_from_str("--max-frames").unwrap_or(3600);
    let rest = args.finish();
    if !rest.is_empty() {
        return usage(&format!("unexpected arguments {rest:?}"));
    }

    let payload: Payload = match std::fs::read(&payload_path)
        .map_err(|e| e.to_string())
        .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|e| e.to_string()))
    {
        Ok(payload) => payload,
        Err(e) => return usage(&format!("{}: {e}", payload_path.display())),
    };
    if payload.payload.is_empty() {
        return usage("the payload lists no avatars");
    }
    let content_base = payload.base_url.trim_end_matches('/');
    let root = content_base
        .strip_suffix("/content")
        .unwrap_or(content_base)
        .to_owned();
    let realm = realm
        .or_else(|| std::env::var("DCL_PEER_BASE").ok())
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| root.clone());

    init_runtime().unwrap();

    let base_config: AppConfig = serialized_app_config();
    let final_config = AppConfig {
        graphics: GraphicsSettings {
            vsync: false,
            log_fps: false,
            fps_target: 999,
            ..base_config.graphics
        },
        scene_imposter_bake: SceneImposterBake::Off,
        ..base_config
    };
    let audio_settings = final_config.audio.clone();

    let mut app = offline_app(final_config, &realm, content_server.unwrap_or(root));

    app.add_plugins((
        bevy_kira_audio::AudioPlugin,
        livestream_manager::plugin::LivestreamManagerPlugin,
        media::plugin::MediaPlugin,
    ));
    app.add_plugins((
        collectibles::CollectiblesPlugin,
        av::plugin::AVPlayerPlugin,
        avatar::AvatarPlugin,
    ));
    app.init_resource::<SceneDrivenAnimationFeedback>();
    app.insert_resource(audio_settings);
    // the booth's layer reaches every wearable mesh under the avatar
    app.add_plugins(HierarchyPropagatePlugin::<RenderLayers>::default());

    let mut jobs = Vec::new();
    for (ix, entry) in payload.payload.into_iter().enumerate() {
        let shape = match shape_of(&entry.entity, &entry.avatar) {
            Ok(shape) => shape,
            Err(e) => return usage(&e),
        };
        for path in [&entry.dest_path, &entry.face_dest_path] {
            if let Some(dir) = path.parent() {
                if let Err(e) = std::fs::create_dir_all(dir) {
                    return usage(&format!("{}: {e}", dir.display()));
                }
            }
        }
        let mut images = app.world_mut().resource_mut::<Assets<Image>>();
        let body = Shot {
            path: entry.dest_path,
            target: target_image(&mut images, entry.width, entry.height),
            transform: Transform::from_xyz(0.0, 0.95, -3.0).looking_at(Vec3::Y * 0.95, Vec3::Y),
        };
        let face = Shot {
            path: entry.face_dest_path,
            target: target_image(&mut images, entry.face_width, entry.face_height),
            transform: Transform::from_xyz(0.0, 1.62, -0.8).looking_at(Vec3::Y * 1.62, Vec3::Y),
        };
        jobs.push(Job {
            entity: entry.entity,
            shape,
            layer: RenderLayers::layer(FIRST_LAYER + ix),
            shots: [body, face],
        });
    }

    app.insert_resource(Jobs {
        jobs,
        settle_frames,
        max_frames,
    });
    app.init_resource::<RenderState>();
    app.add_systems(Startup, setup);
    log_panics::init();
    app.add_systems(Update, drive_capture);
    app.run()
}

fn setup(mut booth: PhotoBooth, jobs: Res<Jobs>, mut commands: Commands) {
    // the booth's own camera renders a placeholder; the shots have cameras of their own
    let placeholder = Extent3d {
        width: 16,
        height: 16,
        depth_or_array_layers: 1,
    };
    let mut booths = Vec::new();
    for job in jobs.jobs.iter() {
        booths.push(booth.spawn_booth(job.layer.clone(), job.shape.clone(), placeholder));
        for shot in job.shots.iter() {
            commands.spawn((
                Camera3d::default(),
                Camera {
                    clear_color: ClearColorConfig::Custom(Color::NONE),
                    target: RenderTarget::Image(shot.target.clone().into()),
                    ..default()
                },
                // profile colours as picked: the booth's own light alone lights the render
                Tonemapping::None,
                ColorGrading::default(),
                AmbientLight {
                    brightness: 0.0,
                    ..default()
                },
                job.layer.clone(),
                shot.transform,
            ));
        }
    }
    commands.insert_resource(Booths(booths));
}

fn drive_capture(
    mut commands: Commands,
    mut state: ResMut<RenderState>,
    jobs: Res<Jobs>,
    frame: Res<FrameCount>,
    processed: Query<(), With<AvatarProcessed>>,
    mut exit: EventWriter<AppExit>,
) {
    let expected = jobs.jobs.len() * 2;
    if state.captured {
        let written = state.written.lock().unwrap();
        if written.len() < expected {
            return;
        }
        let mut failed = false;
        for result in written.iter() {
            match result {
                Ok(path) => println!("avatar_render: wrote {}", path.display()),
                Err(e) => {
                    eprintln!("avatar_render: {e}");
                    failed = true;
                }
            }
        }
        exit.write(if failed {
            AppExit::from_code(1)
        } else {
            AppExit::Success
        });
        return;
    }

    if frame.0 >= jobs.max_frames {
        let entities: Vec<&str> = jobs.jobs.iter().map(|j| j.entity.as_str()).collect();
        eprintln!(
            "avatar_render: {entities:?} still loading after {} frames",
            frame.0
        );
        exit.write(AppExit::from_code(1));
        return;
    }

    if processed.iter().count() < jobs.jobs.len() {
        state.ready_at = None;
        return;
    }
    let start = *state.ready_at.get_or_insert(frame.0);
    if frame.0 - start < jobs.settle_frames {
        return;
    }
    state.captured = true;

    for job in jobs.jobs.iter() {
        for shot in job.shots.iter() {
            let path = shot.path.clone();
            let written = state.written.clone();
            commands
                .spawn(Screenshot::image(shot.target.clone()))
                .observe(move |mut trigger: Trigger<ScreenshotCaptured>| {
                    let img = std::mem::take(&mut trigger.0);
                    let (w, h) = (
                        img.texture_descriptor.size.width,
                        img.texture_descriptor.size.height,
                    );
                    let result = img
                        .data
                        .and_then(|data| image::RgbaImage::from_raw(w, h, data))
                        .ok_or_else(|| format!("{}: no image data", path.display()))
                        .and_then(|buf| {
                            buf.save(&path)
                                .map(|()| path.clone())
                                .map_err(|e| format!("{}: {e}", path.display()))
                        });
                    written.lock().unwrap().push(result);
                });
        }
    }
}
