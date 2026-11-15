//! Camera Reel response shapes. Image URLs contain the photo UUID and retained
//! upload basename; rows without a basename keep hash URLs. The database renders
//! both forms under the configured public API URL.

use serde::{Deserialize, Serialize};

use super::db::DbImage;

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "camera-reel/")
)]
pub struct Image {
    pub id: String,
    pub url: String,
    pub thumbnail_url: String,
    pub is_public: bool,
    pub metadata: Metadata,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "camera-reel/")
)]
pub struct GalleryImage {
    pub id: String,
    pub url: String,
    pub thumbnail_url: String,
    pub is_public: bool,
    pub date_time: String,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "camera-reel/")
)]
pub struct GalleryImageWithPlace {
    pub id: String,
    pub url: String,
    pub thumbnail_url: String,
    pub is_public: bool,
    pub date_time: String,
    pub place_id: String,
}

#[derive(Deserialize, Serialize, Debug, Default, Clone)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "camera-reel/")
)]
pub struct Metadata {
    pub user_name: String,
    pub user_address: String,
    pub date_time: String,
    pub realm: String,
    pub scene: Scene,
    pub visible_people: Vec<User>,
    pub place_id: String,
}

#[derive(Deserialize, Serialize, Debug, Default, Clone)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "camera-reel/")
)]
pub struct Scene {
    pub name: String,
    pub location: Location,
}

#[derive(Deserialize, Serialize, Debug, Default, Clone)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "camera-reel/")
)]
pub struct Location {
    pub x: String,
    pub y: String,
}

#[derive(Deserialize, Serialize, Debug, Default, Clone)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "camera-reel/")
)]
pub struct User {
    pub user_name: String,
    pub user_address: String,
    pub wearables: Vec<String>,
    #[serde(default)]
    pub is_guest: bool,
    #[serde(default)]
    pub is_emoting: Option<bool>,
    /// Omitted rather than written as `null`, so a photo whose client does
    /// not report it keeps the shape it always had.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub screen_rect: Option<ScreenRect>,
}

/// Where a visible person stands in the photo, normalised to the image with
/// the origin at its top-left corner, as the explorer measures it when the
/// shot is taken. Every field is in `0..=1`; `x + width` and `y + height`
/// may pass 1 by up to [`SCREEN_RECT_EDGE_TOLERANCE`].
#[derive(Deserialize, Serialize, Debug, Default, Clone, Copy, PartialEq)]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "camera-reel/")
)]
pub struct ScreenRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Slack for `x + width` and `y + height`: the explorer clamps the rect to
/// the image, and the sum of two clamped floats can land a hair past 1.
pub const SCREEN_RECT_EDGE_TOLERANCE: f32 = 1e-4;

impl ScreenRect {
    /// Whether the rect is a rectangle of the image. An upload with any
    /// other rect is refused: a number too large for an `f32` parses as
    /// infinity and would be stored as `null`.
    pub fn is_valid(&self) -> bool {
        [self.x, self.y, self.width, self.height]
            .iter()
            .all(|v| v.is_finite() && (0.0..=1.0).contains(v))
            && self.x + self.width <= 1.0 + SCREEN_RECT_EDGE_TOLERANCE
            && self.y + self.height <= 1.0 + SCREEN_RECT_EDGE_TOLERANCE
    }
}

impl Metadata {
    /// Every visible person's `screenRect`, when present, is valid.
    pub fn screen_rects_are_valid(&self) -> bool {
        self.visible_people
            .iter()
            .filter_map(|person| person.screen_rect)
            .all(|rect| rect.is_valid())
    }
}

impl From<DbImage> for Image {
    fn from(value: DbImage) -> Self {
        Self {
            id: value.id.to_string(),
            url: value.url,
            thumbnail_url: value.thumbnail_url,
            is_public: value.is_public,
            metadata: value.metadata.0,
        }
    }
}

impl From<DbImage> for GalleryImage {
    fn from(value: DbImage) -> Self {
        Self {
            id: value.id.to_string(),
            url: value.url,
            thumbnail_url: value.thumbnail_url,
            is_public: value.is_public,
            date_time: value.metadata.0.date_time,
        }
    }
}

impl From<DbImage> for GalleryImageWithPlace {
    fn from(value: DbImage) -> Self {
        Self {
            id: value.id.to_string(),
            url: value.url,
            thumbnail_url: value.thumbnail_url,
            is_public: value.is_public,
            date_time: value.metadata.0.date_time,
            place_id: value.metadata.0.place_id,
        }
    }
}

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "camera-reel/")
)]
pub struct UserDataResponse {
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub current_images: u64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub max_images: u64,
}

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "camera-reel/")
)]
pub struct PlaceDataResponse {
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub max_images: u64,
}

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "camera-reel/")
)]
pub struct UploadResponse {
    pub image: Image,
    #[serde(flatten)]
    pub user_data: UserDataResponse,
}

/// The gallery pages. `nextCursor` is present only on a keyset request
/// (`?cursor=`), so the offset shape stays what the old service answered.
#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "camera-reel/")
)]
pub struct GetImagesResponse {
    pub images: Vec<Image>,
    #[serde(flatten)]
    pub user_data: UserDataResponse,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub next_cursor: Option<Option<String>>,
}

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "camera-reel/")
)]
pub struct GetGalleryImagesResponse {
    pub images: Vec<GalleryImage>,
    #[serde(flatten)]
    pub user_data: UserDataResponse,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub next_cursor: Option<Option<String>>,
}

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "camera-reel/")
)]
pub struct GetPlaceImagesResponse {
    pub images: Vec<GalleryImage>,
    #[serde(flatten)]
    pub place_data: PlaceDataResponse,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub next_cursor: Option<Option<String>>,
}

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(
    feature = "ts",
    derive(ts_rs::TS),
    ts(export, export_to = "camera-reel/")
)]
pub struct GetMultiplePlacesImagesResponse {
    pub images: Vec<GalleryImageWithPlace>,
    #[serde(flatten)]
    pub place_data: PlaceDataResponse,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub next_cursor: Option<Option<String>>,
}

/// `GET /api/wearables/{item}/images`: full images (who is in the shot and
/// where it was taken, without one metadata request per photo) and the
/// matching set's size; offset pages only, as upstream answers them.
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct GetWearableImagesResponse {
    pub images: Vec<Image>,
    pub max_images: u64,
}

#[derive(Deserialize, Debug)]
pub struct UpdateVisibility {
    pub is_public: bool,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct UpdateReview {
    pub review_status: String,
}

impl UpdateReview {
    pub fn is_valid(&self) -> bool {
        matches!(self.review_status.as_str(), "ok" | "flagged" | "rejected")
    }
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct GetMultiplePlacesImagesBody {
    pub places_ids: Vec<String>,
}

#[cfg(test)]
mod parity_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn visible_person_preserves_screen_rect_and_omits_it_for_older_photos() {
        let mut person = json!({
            "userName": "photographer", "userAddress": "0x1", "wearables": [],
            "isGuest": false, "isEmoting": null
        });
        let without_rect: User = serde_json::from_value(person.clone()).unwrap();
        assert_eq!(serde_json::to_value(without_rect).unwrap(), person);

        person["screenRect"] = json!({"x": 0.25, "y": 0.125, "width": 0.5, "height": 0.75});
        let with_rect: User = serde_json::from_value(person.clone()).unwrap();
        assert_eq!(serde_json::to_value(with_rect).unwrap(), person);

        person["screenRect"] = serde_json::Value::Null;
        let null_rect: User = serde_json::from_value(person).unwrap();
        assert!(
            serde_json::to_value(null_rect)
                .unwrap()
                .get("screenRect")
                .is_none()
        );
    }

    #[test]
    fn screen_rect_accepts_edges_and_rejects_invalid_or_overflowing_coordinates() {
        let rect = |x, y, width, height| ScreenRect {
            x,
            y,
            width,
            height,
        };
        for value in [
            rect(0.0, 0.0, 0.0, 0.0),
            rect(0.0, 0.0, 1.0, 1.0),
            rect(0.3, 0.5, 0.70005, 0.5),
        ] {
            assert!(value.is_valid(), "{value:?}");
        }
        for value in [
            rect(-0.1, 0.0, 0.1, 0.1),
            rect(0.0, -0.1, 0.1, 0.1),
            rect(0.0, 0.0, -0.1, 0.1),
            rect(0.0, 0.0, 0.1, -0.1),
            rect(0.9, 0.0, 0.2, 0.1),
            rect(0.0, 0.9, 0.1, 0.2),
            rect(f32::INFINITY, 0.0, 0.1, 0.1),
            rect(0.0, 0.0, f32::NAN, 0.1),
        ] {
            assert!(!value.is_valid(), "{value:?}");
        }
        let overflow =
            serde_json::from_str::<ScreenRect>(r#"{"x":1e39,"y":0,"width":0,"height":0}"#);
        assert!(match overflow {
            Ok(rect) => !rect.is_valid(),
            Err(_) => true,
        });
    }
}
