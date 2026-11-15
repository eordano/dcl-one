//! Photo mode for the HUD: one `PhotoModeState` per change on the `GetPhotoModeStream`.

use bevy::prelude::*;
use common::rpc::RpcStreamSender;
use common::{sets::SceneSets, structs::CursorLocks};
use system_api_types::PhotoModeState;
use system_bridge::SystemApi;

use crate::{
    capture::{
        phase, EncodingPhoto, PendingPhoto, PhotoCaptured, PhotoUploadFailed, PhotoUploaded,
        UploadingPhoto,
    },
    photo_camera::PhotoCamera,
    storage::CameraReelStorage,
};

#[derive(Default)]
struct Relay {
    senders: Vec<RpcStreamSender<PhotoModeState>>,
    last: Option<PhotoModeState>,
    event_seq: u32,
}

pub(crate) struct PhotoModeRelayPlugin;

impl Plugin for PhotoModeRelayPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            relay_photo_mode
                .in_set(SceneSets::Input)
                .after(crate::mode::apply_photo_mode_requests),
        );
    }
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn relay_photo_mode(
    mut events: EventReader<SystemApi>,
    mut relay: Local<Relay>,
    camera: Query<
        (
            Option<&PhotoCamera>,
            Has<PendingPhoto>,
            Has<EncodingPhoto>,
            Has<UploadingPhoto>,
        ),
        With<common::structs::PrimaryCamera>,
    >,
    locks: Res<CursorLocks>,
    storage: Res<CameraReelStorage>,
    mut captured: EventReader<PhotoCaptured>,
    mut uploaded: EventReader<PhotoUploaded>,
    mut failed: EventReader<PhotoUploadFailed>,
) {
    let new_senders: Vec<_> = events
        .read()
        .filter_map(|ev| match ev {
            SystemApi::GetPhotoModeStream(sender) => Some(sender.clone()),
            _ => None,
        })
        .collect();
    let has_new = !new_senders.is_empty();
    relay.senders.extend(new_senders);
    relay.senders.retain(|s| !s.is_closed());

    // an event is stamped once, whether or not anyone listens, so a late listener sees the seq move
    let mut event = ("none", String::new(), String::new());
    if let Some(ev) = captured.read().last() {
        event = ("captured", ev.preview_jpeg_base64.clone(), String::new());
    }
    if let Some(ev) = uploaded.read().last() {
        event = ("uploaded", String::new(), ev.id.clone());
    }
    if let Some(ev) = failed.read().last() {
        event = ("uploadFailed", String::new(), ev.message.clone());
    }
    if event.0 != "none" {
        relay.event_seq = relay.event_seq.wrapping_add(1);
    }

    if relay.senders.is_empty() {
        relay.last = None;
        return;
    }

    let Ok((photo, pending, encoding, uploading)) = camera.single() else {
        return;
    };
    let state = PhotoModeState {
        active: photo.is_some(),
        hud_hidden: photo.is_some_and(|p| p.hud_hidden),
        cursor_locked: locks.0.contains("photo"),
        phase: phase(pending, encoding, uploading).to_owned(),
        storage_current: storage.current,
        storage_max: storage.max,
        storage_loaded: storage.loaded,
        event: event.0.to_owned(),
        event_seq: relay.event_seq,
        preview_jpeg_base64: event.1,
        message: event.2,
    };
    let changed = relay.last.as_ref() != Some(&state);
    if changed || has_new {
        for sender in &relay.senders {
            let _ = sender.send(state.clone());
        }
        relay.last = Some(state);
    }
}
