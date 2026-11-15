//! The console surface and debug-panel section of photo mode.

use bevy::{prelude::*, tasks::Task};
use bevy_console::ConsoleCommand;
use common::{
    structs::{CursorLocks, PrimaryCamera},
    util::{TaskCompat, TaskExt},
};
use console::{AddDebugSection, DebugInput, DebugInputKind, DebugSection, DoAddConsoleCommand};
use ipfs::IpfsAssetServer;
use serde_json::json;
use wallet::Wallet;

use crate::{
    capture::{
        capture_blocker, phase, EncodingPhoto, PendingPhoto, PhotoStats, TakePhotoRequest,
        UploadingPhoto,
    },
    mode::PhotoModeRequest,
    photo_camera::{PhotoCamera, PhotoModeSource},
    storage::{delete_photo, CameraReelStorage},
};

/// Photo deletions asked of the console, by photo id.
#[derive(Resource, Default)]
struct PhotoDeletes(Vec<(String, Task<Result<(), String>>)>);

pub(crate) struct PhotoModeDiagnosticsPlugin;

impl Plugin for PhotoModeDiagnosticsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PhotoDeletes>()
            .add_console_command::<PhotoModeCommand, _>(photo_mode_command)
            .add_console_command::<TakePhotoCommand, _>(take_photo_command)
            .add_console_command::<DeletePhotoCommand, _>(delete_photo_command)
            .add_systems(Update, finish_deletes)
            .add_debug_section(DebugSection {
                title: "Photo mode",
                query: "/photo_mode",
                fields: &[
                    ("Position (X / Y / Z)", "position"),
                    ("Yaw (degrees)", "yaw"),
                    ("Pitch (degrees)", "pitch"),
                    ("Roll (degrees)", "roll"),
                    ("Vertical FOV (degrees)", "fov"),
                    ("Cursor locked", "cursorLocked"),
                    ("Camera HUD hidden", "hudHidden"),
                    ("Opened by", "source"),
                    ("Photos kept (current / max)", "storage"),
                    ("Photo in flight", "phase"),
                    ("Captures this session", "captures"),
                    ("Uploads this session", "uploaded"),
                    ("Failures this session", "failed"),
                    ("Last photo id", "lastPhotoId"),
                    ("Last error", "lastError"),
                ],
                actions: &[("Take photo", "/take_photo")],
                inputs: &[DebugInput {
                    label: "Photo mode",
                    field: "active",
                    command: "/photo_mode {value}",
                    kind: DebugInputKind::Toggle,
                }],
            });
    }
}

/// Read photo mode, or open (`on`/`true`) or close (`off`/`false`) it. `source` names what
/// opened it for analytics: `button` (the HUD), `shortcut` or `console` (the default).
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/photo_mode")]
struct PhotoModeCommand {
    state: Option<String>,
    source: Option<String>,
}

/// Take a photo in photo mode. `source` as for `/photo_mode`.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/take_photo")]
struct TakePhotoCommand {
    source: Option<String>,
}

/// Delete one of the player's photos from the camera reel; the outcome lands in `lastError`.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/delete_photo")]
struct DeletePhotoCommand {
    id: String,
}

fn parse_source(source: Option<&str>) -> Result<PhotoModeSource, String> {
    match source.map(str::to_ascii_lowercase).as_deref() {
        None | Some("console") => Ok(PhotoModeSource::Console),
        Some("button") => Ok(PhotoModeSource::Button),
        Some("shortcut") => Ok(PhotoModeSource::Shortcut),
        Some(other) => Err(format!(
            "expected button, shortcut or console as the source, got {other}"
        )),
    }
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn take_photo_command(
    mut cmd: ConsoleCommand<TakePhotoCommand>,
    mut requests: EventWriter<TakePhotoRequest>,
    camera: Query<(
        &Transform,
        &PrimaryCamera,
        Option<&PhotoCamera>,
        Has<PendingPhoto>,
        Has<EncodingPhoto>,
        Has<UploadingPhoto>,
    )>,
    locks: Res<CursorLocks>,
    storage: Res<CameraReelStorage>,
    stats: Res<PhotoStats>,
    time: Res<Time>,
) {
    let Some(Ok(args)) = cmd.take() else {
        return;
    };
    let source = match parse_source(args.source.as_deref()) {
        Ok(source) => source,
        Err(message) => {
            cmd.reply_failed(message);
            return;
        }
    };
    let Ok((transform, primary, photo, pending, encoding, uploading)) = camera.single() else {
        cmd.reply_failed("no primary camera");
        return;
    };
    let busy = pending || encoding || uploading;
    if let Some(reason) = capture_blocker(
        photo.is_some(),
        busy,
        &storage,
        stats.effect_until,
        time.elapsed_secs_f64(),
    ) {
        cmd.reply_failed(format!("photo not taken: {reason}"));
        return;
    }
    requests.write(TakePhotoRequest { source });
    cmd.reply_ok(
        state_json(
            transform,
            primary,
            photo,
            &locks,
            &storage,
            &stats,
            phase(pending, encoding, uploading),
            None,
        )
        .to_string(),
    );
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn photo_mode_command(
    mut cmd: ConsoleCommand<PhotoModeCommand>,
    mut requests: EventWriter<PhotoModeRequest>,
    camera: Query<(
        &Transform,
        &PrimaryCamera,
        Option<&PhotoCamera>,
        Has<PendingPhoto>,
        Has<EncodingPhoto>,
        Has<UploadingPhoto>,
    )>,
    locks: Res<CursorLocks>,
    storage: Res<CameraReelStorage>,
    stats: Res<PhotoStats>,
) {
    let Some(Ok(args)) = cmd.take() else {
        return;
    };
    let source = match parse_source(args.source.as_deref()) {
        Ok(source) => source,
        Err(message) => {
            cmd.reply_failed(message);
            return;
        }
    };
    let requested = match args
        .state
        .as_deref()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        None => None,
        Some("on" | "true") => Some(true),
        Some("off" | "false") => Some(false),
        Some(other) => {
            cmd.reply_failed(format!("expected on or off, got {other}"));
            return;
        }
    };
    let Ok((transform, primary, photo, pending, encoding, uploading)) = camera.single() else {
        cmd.reply_failed("no primary camera");
        return;
    };
    if let Some(enable) = requested {
        requests.write(if enable {
            PhotoModeRequest::Enable(source)
        } else {
            PhotoModeRequest::Disable
        });
    }
    cmd.reply_ok(
        state_json(
            transform,
            primary,
            photo,
            &locks,
            &storage,
            &stats,
            phase(pending, encoding, uploading),
            requested,
        )
        .to_string(),
    );
}

fn delete_photo_command(
    mut cmd: ConsoleCommand<DeletePhotoCommand>,
    mut deletes: ResMut<PhotoDeletes>,
    wallet: Res<Wallet>,
    ipfs: IpfsAssetServer,
) {
    let Some(Ok(args)) = cmd.take() else {
        return;
    };
    if wallet.address().is_none() {
        cmd.reply_failed("not signed in");
        return;
    }
    let task = bevy::tasks::IoTaskPool::get().spawn_compat(delete_photo(
        ipfs.ipfs().client(),
        wallet.clone(),
        args.id.clone(),
    ));
    deletes.0.push((args.id.clone(), task));
    cmd.reply_ok(json!({ "deleting": args.id }).to_string());
}

/// A finished deletion frees a storage slot; a failed one is the last error.
fn finish_deletes(
    mut deletes: ResMut<PhotoDeletes>,
    mut storage: ResMut<CameraReelStorage>,
    mut stats: ResMut<PhotoStats>,
) {
    deletes.0.retain_mut(|(id, task)| {
        let Some(result) = task.complete() else {
            return true;
        };
        match result {
            Ok(()) => {
                info!("photo deleted: {id}");
                storage.current = storage.current.saturating_sub(1);
            }
            Err(message) => {
                warn!("photo {id} not deleted: {message}");
                stats.last_error = Some(format!("delete {id}: {message}"));
            }
        }
        false
    });
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn state_json(
    transform: &Transform,
    primary: &PrimaryCamera,
    photo: Option<&PhotoCamera>,
    locks: &CursorLocks,
    storage: &CameraReelStorage,
    stats: &PhotoStats,
    phase: &str,
    requested: Option<bool>,
) -> serde_json::Value {
    let mut state = json!({
        "active": photo.is_some(),
        "requested": requested,
        "position": [transform.translation.x, transform.translation.y, transform.translation.z],
        "storage": [storage.current, storage.max],
        "storageLoaded": storage.loaded,
        "phase": phase,
        "captures": stats.captures,
        "uploaded": stats.uploaded,
        "failed": stats.failed,
        "lastPhotoId": stats.last_photo_id,
        "lastError": stats.last_error,
    });
    match photo {
        Some(photo) => {
            state["yaw"] = json!(photo.yaw);
            state["pitch"] = json!(photo.pitch);
            state["roll"] = json!(photo.roll);
            state["fov"] = json!(photo.fov.current);
            state["cursorLocked"] = json!(locks.0.contains("photo"));
            state["hudHidden"] = json!(photo.hud_hidden);
            state["source"] = json!(photo.source);
        }
        None => {
            let reason = if primary.scene_override.is_some() {
                "a scene controls the camera"
            } else {
                "photo mode is off"
            };
            state["unavailable"] = json!({ "": reason });
        }
    }
    state
}
