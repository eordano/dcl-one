//! The camera reel's capture side: photo mode (a free camera around the avatar), the photo
//! itself and its upload to the camera-reel service. The gallery that shows the photos is the
//! HUD's.

pub mod capture;
pub mod diagnostics;
pub mod metadata;
pub mod mode;
pub mod photo_camera;
pub mod relay;
pub mod smooth_damp;
pub mod storage;

use bevy::prelude::*;

pub use capture::{
    CapturingPhoto, PhotoCaptured, PhotoUploadFailed, PhotoUploaded, TakePhotoRequest,
};
pub use mode::{PhotoModeChanged, PhotoModeRequest};
pub use photo_camera::{PhotoCamera, PhotoCameraSettings, PhotoModeSource};
pub use storage::CameraReelStorage;

pub struct CameraReelPlugin;

impl Plugin for CameraReelPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(mode::PhotoModePlugin);
        app.add_plugins(capture::PhotoCapturePlugin);
        app.add_plugins(relay::PhotoModeRelayPlugin);
        app.add_systems(
            Update,
            storage::fetch_storage_status
                .in_set(common::sets::SceneSets::Input)
                .after(mode::apply_photo_mode_requests),
        );
        app.add_plugins(diagnostics::PhotoModeDiagnosticsPlugin);
    }
}
