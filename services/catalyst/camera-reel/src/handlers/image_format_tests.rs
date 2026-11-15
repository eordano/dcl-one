//! Source image pipeline at camera-reel-service
//! 3e8bd0b099400e7c15b95477c1f003c63042d461: guess, decode, thumbnail, encode.
//! Fixtures include formats whose bytes disagree with the accepted multipart
//! MIME, unsupported encoders, alpha/16-bit/float images and malformed headers.

use base64::Engine;
use serde_json::Value;

use super::thumbnail_source_compat;

// The pinned JPEG decoder selects SSSE3 at runtime, with different rounding
// from its scalar implementation. Both goldens come from the source pipeline.
fn source_pixel_hash(expected: &Value) -> &Value {
    let scalar = &expected["pixelHash"];
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    if std::is_x86_feature_detected!("ssse3") {
        return expected.get("pixelHashSsse3").unwrap_or(scalar);
    }
    scalar
}

#[test]
fn source_image_format_and_thumbnail_contract() {
    let cases: Vec<Value> =
        serde_json::from_str(include_str!("image_format_fixtures.json")).unwrap();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(case["input"].as_str().unwrap())
            .unwrap();
        let expected = &case["expected"];
        let result = thumbnail_source_compat(&bytes);
        if let Some(error) = expected["error"].as_str() {
            assert_eq!(result.unwrap_err().to_string(), error, "{name}");
            continue;
        }
        let thumbnail = result.unwrap_or_else(|error| panic!("{name}: {error}"));
        let format = image::guess_format(&thumbnail).unwrap();
        let decoded = image::load_from_memory_with_format(&thumbnail, format).unwrap();
        assert_eq!(format!("{format:?}"), expected["format"], "{name}");
        assert_eq!(
            decoded.width(),
            expected["width"].as_u64().unwrap() as u32,
            "{name}"
        );
        assert_eq!(
            decoded.height(),
            expected["height"].as_u64().unwrap() as u32,
            "{name}"
        );
        assert_eq!(
            format!("{:?}", decoded.color()),
            expected["color"],
            "{name}"
        );
        assert_eq!(
            dcl_auth::hash_bytes(decoded.as_bytes()),
            *source_pixel_hash(expected),
            "{name}"
        );
    }
}
