//! Taking a photo: the frame is read back from the window, cut to the photo frame, encoded as
//! JPEG and uploaded with its metadata. One photo is in flight at a time; its stage is the
//! component on the primary camera.

use analytics::{
    data_definition::{SegmentEvent, SegmentEventOpenCamera, SegmentEventTakePhoto},
    segment_system::{SegmentConfig, SegmentMetricsEvents},
};
use avatar::animate::{is_emoting, ActiveEmote, MaskedEmote};
use base64::{prelude::BASE64_STANDARD, Engine};
use bevy::{
    prelude::*,
    render::view::screenshot::{Screenshot, ScreenshotCaptured},
    tasks::{IoTaskPool, Task},
    window::PrimaryWindow,
};
use common::{
    inputs::SystemAction,
    sets::SceneSets,
    structs::{CurrentRealm, PrimaryCamera, PrimaryUser, SystemAudio},
    util::{TaskCompat, TaskExt},
};
use comms::{
    global_crdt::ForeignPlayer,
    profile::{CurrentUserProfile, UserProfile},
};
use image::{codecs::jpeg::JpegEncoder, imageops::FilterType, DynamicImage};
use input_manager::{InputManager, InputPriority};
use ipfs::IpfsAssetServer;
use scene_runner::vec3_to_parcel;
use wallet::Wallet;

use crate::{
    metadata::{
        avatar_aabb, filter_non_base_wearables, in_frame, lowercase_address, normalized_frame_size,
        photo_frustum, screen_rect, ScreenshotMetadata, VisiblePerson, PHOTO_HEIGHT, PHOTO_WIDTH,
        UNKNOWN_PLACE, UNKNOWN_USER, UNKNOWN_USER_WALLET,
    },
    mode::{PhotoModeChanged, PhotoModeRequest},
    photo_camera::{PhotoCamera, PhotoModeSource},
    storage::{lookup_place, upload_photo, CameraReelStorage, UploadError, UploadResponse},
};

pub const JPEG_QUALITY: u8 = 75;
/// The HUD's capture animation flies a small copy of the photo to the gallery icon.
const PREVIEW_WIDTH: u32 = 320;
const PREVIEW_HEIGHT: u32 = 180;
const PREVIEW_JPEG_QUALITY: u8 = 60;
/// The capture effect's length, during which no new photo starts: the flash, a pause, the fly.
pub const CAPTURE_EFFECT_SECS: f64 = 0.5 + 0.1 + 0.5;
pub const SHUTTER_SOUND: &str = "embedded://sounds/ui/take_screenshot.mp3";

/// Asks for a photo. Sent by the key, the HUD and the console.
#[derive(Event, Clone, Copy, Debug)]
pub struct TakePhotoRequest {
    pub source: PhotoModeSource,
}

/// The frame being read back is the photo: the native HUD hides while it is on the camera.
#[derive(Component)]
pub struct CapturingPhoto;

/// A photo's frame is on its way back from the GPU.
#[derive(Component)]
pub(crate) struct PendingPhoto {
    frame: tokio::sync::oneshot::Receiver<Image>,
    shot: Option<Shot>,
}

/// A photo's frame is being cut and encoded while its place is looked up.
#[derive(Component)]
pub(crate) struct EncodingPhoto {
    encode: Task<Result<EncodedPhoto, String>>,
    encoded: Option<EncodedPhoto>,
    shot: Option<Shot>,
}

/// A photo is on its way to the service.
#[derive(Component)]
pub(crate) struct UploadingPhoto {
    upload: Task<Result<UploadResponse, UploadError>>,
    source: PhotoModeSource,
    session: u32,
    people: Vec<(String, bool)>,
}

/// What was true when the shutter fired.
struct Shot {
    metadata: ScreenshotMetadata,
    frame: Vec2,
    source: PhotoModeSource,
    /// The photo-mode session the shutter fired in.
    session: u32,
    place: Task<(String, String)>,
    place_found: Option<(String, String)>,
}

#[derive(Clone)]
pub struct EncodedPhoto {
    pub jpeg: Vec<u8>,
    pub preview_jpeg: Vec<u8>,
}

/// The photo's frame is back and encoded: play the capture effect with this preview.
#[derive(Event, Clone, Debug)]
pub struct PhotoCaptured {
    pub preview_jpeg_base64: String,
}

#[derive(Event, Clone, Debug)]
pub struct PhotoUploaded {
    pub id: String,
    pub current: u32,
    pub max: u32,
}

#[derive(Event, Clone, Debug)]
pub struct PhotoUploadFailed {
    pub message: String,
}

/// The stage of the photo in flight, for the console and the HUD.
pub(crate) fn phase(pending: bool, encoding: bool, uploading: bool) -> &'static str {
    if pending {
        "capturing"
    } else if encoding {
        "encoding"
    } else if uploading {
        "uploading"
    } else {
        "idle"
    }
}

/// Counters for the console and the debug panel.
#[derive(Resource, Default, Debug)]
pub struct PhotoStats {
    pub captures: u32,
    pub uploaded: u32,
    pub failed: u32,
    pub last_photo_id: Option<String>,
    pub last_error: Option<String>,
    /// Seconds (engine time) until which the capture effect blocks the next photo.
    pub effect_until: f64,
}

pub(crate) struct PhotoCapturePlugin;

impl Plugin for PhotoCapturePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PhotoStats>()
            .init_resource::<CameraReelStorage>()
            .add_event::<TakePhotoRequest>()
            .add_event::<PhotoCaptured>()
            .add_event::<PhotoUploaded>()
            .add_event::<PhotoUploadFailed>()
            .add_systems(
                Update,
                (
                    request_photo_on_key,
                    start_capture,
                    finish_capture,
                    finish_encode,
                    finish_upload,
                    report_photo_mode_opened,
                )
                    .chain()
                    .in_set(SceneSets::Input)
                    .after(crate::mode::apply_photo_mode_requests),
            );
    }
}

fn request_photo_on_key(
    input_manager: InputManager,
    camera: Query<(), (With<PrimaryCamera>, With<PhotoCamera>)>,
    mut requests: EventWriter<TakePhotoRequest>,
) {
    if !camera.is_empty() && input_manager.just_down(SystemAction::TakePhoto, InputPriority::Ui) {
        requests.write(TakePhotoRequest {
            source: PhotoModeSource::Shortcut,
        });
    }
}

/// Whether a photo may start now, else why not.
pub fn capture_blocker(
    in_photo_mode: bool,
    busy: bool,
    storage: &CameraReelStorage,
    effect_until: f64,
    now: f64,
) -> Option<&'static str> {
    if !in_photo_mode {
        Some("photo mode is off")
    } else if busy {
        Some("a photo is in flight")
    } else if now < effect_until {
        Some("the capture effect is playing")
    } else if !storage.loaded {
        Some("the storage status is not known yet")
    } else if !storage.has_free_space() {
        Some("the storage is full")
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn start_capture(
    mut commands: Commands,
    mut requests: EventReader<TakePhotoRequest>,
    time: Res<Time>,
    storage: Res<CameraReelStorage>,
    mut stats: ResMut<PhotoStats>,
    windows: Query<(Entity, &Window), With<PrimaryWindow>>,
    camera: Query<
        (
            Entity,
            &Camera,
            &GlobalTransform,
            &Projection,
            Option<&PhotoCamera>,
            Has<PendingPhoto>,
            Has<EncodingPhoto>,
            Has<UploadingPhoto>,
        ),
        With<PrimaryCamera>,
    >,
    player: Query<&GlobalTransform, With<PrimaryUser>>,
    people: Query<
        (
            &GlobalTransform,
            &UserProfile,
            Option<&Visibility>,
            Option<(&ActiveEmote, &MaskedEmote)>,
        ),
        Or<(With<ForeignPlayer>, With<PrimaryUser>)>,
    >,
    current_profile: Res<CurrentUserProfile>,
    wallet: Res<Wallet>,
    realm: Res<CurrentRealm>,
    ipfs: IpfsAssetServer,
) {
    let Some(request) = requests.read().last().copied() else {
        return;
    };
    let Ok((camera_entity, camera, camera_transform, projection, photo, p, e, u)) = camera.single()
    else {
        return;
    };
    let now = time.elapsed_secs_f64();
    if let Some(reason) = capture_blocker(
        photo.is_some(),
        p || e || u,
        &storage,
        stats.effect_until,
        now,
    ) {
        debug!("photo not taken: {reason}");
        return;
    }
    let Ok((window_entity, window)) = windows.single() else {
        warn!("photo not taken: no primary window");
        return;
    };
    let Ok(player_transform) = player.single() else {
        return;
    };
    let Projection::Perspective(perspective) = projection else {
        warn!("photo not taken: the camera is not perspective");
        return;
    };

    let aspect = window.width() / window.height().max(1.0);
    let frame = normalized_frame_size(aspect);
    let frustum = photo_frustum(camera_transform, perspective.fov, perspective.near, frame);
    let clip_from_view = camera.clip_from_view();

    // a player a scene hides (`Visibility::Hidden` on the player entity) is not in the photo
    let visible_people = people
        .iter()
        .filter_map(|(transform, profile, visibility, emote)| {
            if visibility == Some(&Visibility::Hidden) {
                return None;
            }
            let aabb = avatar_aabb(transform.translation());
            if !in_frame(&frustum, &aabb) {
                return None;
            }
            Some(VisiblePerson {
                user_name: profile.content.name.clone(),
                user_address: lowercase_address(&profile.content.eth_address),
                is_guest: profile.content.has_connected_web3 == Some(false),
                is_emoting: emote.is_some_and(|(active, masked)| is_emoting(active, masked)),
                screen_rect: screen_rect(
                    &aabb,
                    camera_transform,
                    clip_from_view,
                    perspective.near,
                    frame,
                ),
                wearables: filter_non_base_wearables(&profile.content.avatar.wearables),
            })
        })
        .collect();

    let parcel = vec3_to_parcel(player_transform.translation());
    let realm_name = realm
        .config
        .realm_name
        .clone()
        .unwrap_or_else(|| realm.address.clone());
    let is_world = realm
        .config
        .scenes_urn
        .as_ref()
        .is_some_and(|urns| !urns.is_empty());
    let user_name = current_profile
        .profile
        .as_ref()
        .map(|profile| profile.content.name.clone())
        .unwrap_or_else(|| UNKNOWN_USER.to_owned());
    // the service accepts a photo only from its `userAddress`, so this is the signer's, not the
    // profile's (which may not have loaded yet)
    let user_address = wallet
        .address()
        .map(|address| lowercase_address(&format!("{address:#x}")))
        .unwrap_or_else(|| UNKNOWN_USER_WALLET.to_owned());
    let date_time = web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
        .to_string();
    let metadata = ScreenshotMetadata {
        user_name,
        user_address,
        date_time,
        realm: realm_name.clone(),
        scene: crate::metadata::Scene {
            name: UNKNOWN_PLACE.to_owned(),
            location: parcel.into(),
        },
        visible_people,
        place_id: UNKNOWN_PLACE.to_owned(),
    };

    let client = ipfs.ipfs().client();
    let place = IoTaskPool::get().spawn_compat(lookup_place(
        client,
        is_world.then_some(realm_name),
        parcel,
    ));

    let (sender, receiver) = tokio::sync::oneshot::channel();
    let mut sender = Some(sender);
    commands.spawn(Screenshot::window(window_entity)).observe(
        move |mut trigger: Trigger<ScreenshotCaptured>| {
            if let Some(sender) = sender.take() {
                let _ = sender.send(std::mem::take(&mut trigger.0));
            }
        },
    );
    commands.entity(camera_entity).insert((
        CapturingPhoto,
        PendingPhoto {
            frame: receiver,
            shot: Some(Shot {
                metadata,
                frame,
                source: request.source,
                session: photo.map(|p| p.session).unwrap_or_default(),
                place,
                place_found: None,
            }),
        },
    ));
    stats.captures += 1;
    info!("photo capture started");
}

fn finish_capture(
    mut commands: Commands,
    mut camera: Query<(Entity, &mut PendingPhoto), With<PrimaryCamera>>,
    mut stats: ResMut<PhotoStats>,
) {
    let Ok((entity, mut pending)) = camera.single_mut() else {
        return;
    };
    let image = match pending.frame.try_recv() {
        Ok(image) => image,
        Err(tokio::sync::oneshot::error::TryRecvError::Empty) => return,
        Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
            stats.failed += 1;
            stats.last_error = Some("the frame was not captured".to_owned());
            commands
                .entity(entity)
                .remove::<(CapturingPhoto, PendingPhoto)>();
            return;
        }
    };
    let Some(shot) = pending.shot.take() else {
        return;
    };
    commands
        .entity(entity)
        .remove::<(CapturingPhoto, PendingPhoto)>();
    let frame = shot.frame;
    let encode = IoTaskPool::get().spawn_compat(async move {
        let dynamic = image
            .try_into_dynamic()
            .map_err(|e| format!("the frame cannot be read: {e:?}"))?;
        encode_photo(&dynamic, frame)
    });
    commands.entity(entity).insert(EncodingPhoto {
        encode,
        encoded: None,
        shot: Some(shot),
    });
}

/// The saved photo: the centred `frame` share of the screen, scaled to the photo size.
pub fn encode_photo(screen: &DynamicImage, frame: Vec2) -> Result<EncodedPhoto, String> {
    let (width, height) = (screen.width(), screen.height());
    let crop_w = ((width as f32 * frame.x).round() as u32).clamp(1, width);
    let crop_h = ((height as f32 * frame.y).round() as u32).clamp(1, height);
    let x = (width - crop_w) / 2;
    let y = (height - crop_h) / 2;
    let cropped = screen.crop_imm(x, y, crop_w, crop_h).to_rgb8();
    let photo = image::imageops::resize(&cropped, PHOTO_WIDTH, PHOTO_HEIGHT, FilterType::Triangle);
    let preview =
        image::imageops::resize(&photo, PREVIEW_WIDTH, PREVIEW_HEIGHT, FilterType::Triangle);
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, JPEG_QUALITY)
        .encode_image(&photo)
        .map_err(|e| format!("JPEG encode failed: {e}"))?;
    let mut preview_jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut preview_jpeg, PREVIEW_JPEG_QUALITY)
        .encode_image(&preview)
        .map_err(|e| format!("preview encode failed: {e}"))?;
    Ok(EncodedPhoto { jpeg, preview_jpeg })
}

#[allow(clippy::too_many_arguments)]
fn finish_encode(
    mut commands: Commands,
    mut camera: Query<(Entity, &mut EncodingPhoto), With<PrimaryCamera>>,
    time: Res<Time>,
    mut stats: ResMut<PhotoStats>,
    mut captured: EventWriter<PhotoCaptured>,
    mut audio: EventWriter<SystemAudio>,
    wallet: Res<Wallet>,
    ipfs: IpfsAssetServer,
) {
    let Ok((entity, mut encoding)) = camera.single_mut() else {
        return;
    };
    if encoding.encoded.is_none() {
        match encoding.encode.complete() {
            None => {}
            Some(Ok(encoded)) => {
                captured.write(PhotoCaptured {
                    preview_jpeg_base64: BASE64_STANDARD.encode(&encoded.preview_jpeg),
                });
                audio.write(SystemAudio(SHUTTER_SOUND.to_owned()));
                stats.effect_until = time.elapsed_secs_f64() + CAPTURE_EFFECT_SECS;
                encoding.encoded = Some(encoded);
            }
            Some(Err(message)) => {
                warn!("photo dropped: {message}");
                stats.failed += 1;
                stats.last_error = Some(message);
                commands.entity(entity).remove::<EncodingPhoto>();
                return;
            }
        }
    }
    let place_found = match encoding.shot.as_mut() {
        Some(shot) => {
            if shot.place_found.is_none() {
                shot.place_found = shot.place.complete();
            }
            shot.place_found.is_some()
        }
        None => return,
    };
    if encoding.encoded.is_none() || !place_found {
        return;
    }
    let (Some(encoded), Some(shot)) = (encoding.encoded.take(), encoding.shot.take()) else {
        return;
    };
    commands.entity(entity).remove::<EncodingPhoto>();
    let (name, place_id) = shot.place_found.expect("checked above");
    let mut metadata = shot.metadata;
    metadata.scene.name = name;
    metadata.place_id = place_id;
    let people = metadata
        .visible_people
        .iter()
        .map(|p| (p.user_address.clone(), p.is_emoting))
        .collect();
    let jpeg = encoded.jpeg;
    let upload = IoTaskPool::get().spawn_compat(upload_photo(
        ipfs.ipfs().client(),
        wallet.clone(),
        jpeg,
        metadata,
    ));
    commands.entity(entity).insert(UploadingPhoto {
        upload,
        source: shot.source,
        session: shot.session,
        people,
    });
}

#[allow(clippy::too_many_arguments)]
fn finish_upload(
    mut commands: Commands,
    mut camera: Query<(Entity, &mut UploadingPhoto, Option<&PhotoCamera>), With<PrimaryCamera>>,
    mut storage: ResMut<CameraReelStorage>,
    mut stats: ResMut<PhotoStats>,
    mut uploaded: EventWriter<PhotoUploaded>,
    mut failed: EventWriter<PhotoUploadFailed>,
    mut mode_requests: EventWriter<PhotoModeRequest>,
    mut metrics: ResMut<SegmentMetricsEvents>,
    segment_config: Res<SegmentConfig>,
) {
    let Ok((entity, mut uploading, photo)) = camera.single_mut() else {
        return;
    };
    let Some(result) = uploading.upload.complete() else {
        return;
    };
    commands.entity(entity).remove::<UploadingPhoto>();
    match result {
        Ok(response) => {
            storage.set(response.current_images, response.max_images);
            stats.uploaded += 1;
            stats.last_photo_id = Some(response.image.id.clone());
            info!("photo uploaded: {}", response.image.id);
            metrics.add_event(
                SegmentEvent::TakePhoto(SegmentEventTakePhoto::new(
                    response.image.id.clone(),
                    &uploading.people,
                    uploading.source,
                )),
                segment_config.common(),
            );
            uploaded.write(PhotoUploaded {
                id: response.image.id,
                current: response.current_images,
                max: response.max_images,
            });
        }
        Err(UploadError::LimitReached) => {
            // the HUD already shows the full storage; the count catches up on the next fetch
            storage.current = storage.max;
            stats.failed += 1;
            stats.last_error = Some(UploadError::LimitReached.to_string());
        }
        Err(error) => {
            let message = error.to_string();
            warn!("photo upload failed: {message}");
            stats.failed += 1;
            stats.last_error = Some(message.clone());
            failed.write(PhotoUploadFailed { message });
            // a failure may land a minute later, in a session that is not the shot's
            if photo.is_some_and(|p| p.session == uploading.session) {
                mode_requests.write(PhotoModeRequest::Disable);
            }
        }
    }
}

fn report_photo_mode_opened(
    mut changes: EventReader<PhotoModeChanged>,
    mut metrics: ResMut<SegmentMetricsEvents>,
    segment_config: Res<SegmentConfig>,
) {
    for change in changes.read().filter(|c| c.active) {
        metrics.add_event(
            SegmentEvent::OpenCamera(SegmentEventOpenCamera::new(change.source)),
            segment_config.common(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_photo_is_the_centred_frame_of_the_screen_at_photo_size() {
        // a 1000x1000 screen: the frame is 0.87 wide and 0.87*9/16 tall, the centre is grey and
        // the edges are white, so the photo is grey everywhere
        let mut screen = image::RgbImage::from_pixel(1000, 1000, image::Rgb([255, 255, 255]));
        let frame = normalized_frame_size(1.0);
        let (w, h) = ((1000.0 * frame.x) as u32, (1000.0 * frame.y) as u32);
        for y in 0..h {
            for x in 0..w {
                screen.put_pixel(
                    (1000 - w) / 2 + x,
                    (1000 - h) / 2 + y,
                    image::Rgb([128, 128, 128]),
                );
            }
        }
        let encoded = encode_photo(&DynamicImage::ImageRgb8(screen), frame).unwrap();
        let photo = image::load_from_memory(&encoded.jpeg).unwrap().to_rgb8();
        assert_eq!((photo.width(), photo.height()), (PHOTO_WIDTH, PHOTO_HEIGHT));
        for (x, y) in [(5, 5), (PHOTO_WIDTH - 6, PHOTO_HEIGHT - 6), (960, 540)] {
            let px = photo.get_pixel(x, y).0;
            assert!(
                px.iter().all(|c| (120..=136).contains(c)),
                "{x},{y}: {px:?}"
            );
        }
        let preview = image::load_from_memory(&encoded.preview_jpeg).unwrap();
        assert_eq!(
            (preview.width(), preview.height()),
            (PREVIEW_WIDTH, PREVIEW_HEIGHT)
        );
    }

    #[test]
    fn capture_waits_for_storage_and_the_effect() {
        let mut storage = CameraReelStorage::default();
        assert_eq!(
            capture_blocker(false, false, &storage, 0.0, 1.0),
            Some("photo mode is off")
        );
        assert_eq!(
            capture_blocker(true, false, &storage, 0.0, 1.0),
            Some("the storage status is not known yet")
        );
        storage.set(500, 500);
        assert_eq!(
            capture_blocker(true, false, &storage, 0.0, 1.0),
            Some("the storage is full")
        );
        storage.set(1, 500);
        assert_eq!(
            capture_blocker(true, false, &storage, 2.0, 1.0),
            Some("the capture effect is playing")
        );
        assert_eq!(
            capture_blocker(true, true, &storage, 0.0, 1.0),
            Some("a photo is in flight")
        );
        assert_eq!(capture_blocker(true, false, &storage, 0.0, 1.0), None);
    }
}
