use super::*;
use bevy_console::ConsoleCommand;
use common::structs::PrimaryUser;
use console::{AddDebugSection, DebugSection, DoAddConsoleCommand};
use scene_runner::{renderer_context::RendererSceneContext, ContainingScene, SceneEntity};
use serde_json::json;
#[cfg(any(feature = "ffmpeg", feature = "html"))]
use serde_json::Value;

pub(super) struct MediaDebugPanelPlugin;

impl Plugin for MediaDebugPanelPlugin {
    fn build(&self, app: &mut App) {
        app.add_console_command::<MediaState, _>(media_state)
            .add_debug_section(DebugSection {
                title: "Media player",
                query: "/media_state",
                fields: &[
                    ("Playback backend", "backend"),
                    ("Loaded video players", "video"),
                    ("Loaded audio streams", "audio"),
                    ("Current scene", "scene"),
                    ("Current parcel video players", "parcelVideo"),
                    ("Current parcel audio streams", "parcelAudio"),
                    ("Current parcel HTTP media", "parcelExternal"),
                    ("Players", "players"),
                ],
                actions: &[],
                inputs: &[],
            });
    }
}

/// Inspect scene media players and their requested and scheduled playback states.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/media_state")]
struct MediaState;

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn media_state(
    mut cmd: ConsoleCommand<MediaState>,
    video: Query<(
        Entity,
        &VideoPlayer,
        &SceneEntity,
        Has<InScene>,
        Has<ShouldBePlaying<VideoPlayer>>,
    )>,
    audio: Query<(
        Entity,
        &AudioStream,
        &SceneEntity,
        Has<InScene>,
        Has<ShouldBePlaying<AudioStream>>,
    )>,
    scenes: Query<&RendererSceneContext>,
    player: Query<Entity, With<PrimaryUser>>,
    containing: ContainingScene,
    #[cfg(any(feature = "ffmpeg", feature = "html"))] images: Res<Assets<Image>>,
    #[cfg(any(feature = "ffmpeg", feature = "html"))] video_sinks: Query<&AVSinks<VideoPlayer>>,
    #[cfg(any(feature = "ffmpeg", feature = "html"))] audio_sinks: Query<&AVSinks<AudioStream>>,
) {
    let Some(Ok(_)) = cmd.take() else { return };
    let mut rows = Vec::new();
    let parcel = player.single().ok().and_then(|e| containing.get_parcel(e));
    let scene = parcel.and_then(|e| scenes.get(e).ok()).map(|s| &s.title);
    let is_external =
        |source: &str| source.starts_with("https://") || source.starts_with("http://");
    let mut parcel_video = 0;
    let mut parcel_audio = 0;
    let mut parcel_external = 0;
    let row = |kind: &str,
               entity: &SceneEntity,
               here: bool,
               scheduled: bool,
               config: &dyn AVPlayerConfig| {
        json!({"type":kind, "entity":entity.id.to_string(), "scene":scenes.get(entity.root).ok().map(|ctx| &ctx.title),
               "inScene":here, "requested":config.playing(), "scheduled":scheduled, "volume":config.volume(), "loop":config.r#loop(), "rate":config.playback_rate()})
    };
    for (_ent, player, entity, here, scheduled) in &video {
        if Some(entity.root) == parcel {
            parcel_video += 1;
            parcel_external += usize::from(is_external(player.url()));
        }
        let mut item = row("video", entity, here, scheduled, &player.config());
        item["source"] = json!(source_label(player.url()));
        #[cfg(any(feature = "ffmpeg", feature = "html"))]
        add_sink(&mut item, video_sinks.get(_ent).ok(), &images);
        rows.push(item);
    }
    for (_ent, player, entity, here, scheduled) in &audio {
        if Some(entity.root) == parcel {
            parcel_audio += 1;
            parcel_external += usize::from(is_external(player.url()));
        }
        let mut item = row("audio", entity, here, scheduled, &player.config());
        item["source"] = json!(source_label(player.url()));
        #[cfg(any(feature = "ffmpeg", feature = "html"))]
        add_sink(&mut item, audio_sinks.get(_ent).ok(), &images);
        rows.push(item);
    }
    let backend = if cfg!(feature = "html") {
        "HTML media"
    } else if cfg!(feature = "ffmpeg") {
        "FFmpeg"
    } else {
        "No playback backend"
    };
    cmd.reply_ok(json!({"backend":backend, "scene":scene, "video":video.iter().len(), "audio":audio.iter().len(), "parcelVideo":parcel_video, "parcelAudio":parcel_audio, "parcelExternal":parcel_external, "players":rows,
        "unavailable":{"scene":"No loaded scene at the player's parcel."}}).to_string());
}

// Stream paths and queries can contain credentials. A remote origin or a local content key
// identifies the source without exposing playback credentials in the panel or console logs.
fn source_label(source: &str) -> String {
    let source = source.trim();
    if source.starts_with("//") {
        return String::from("remote media");
    }
    match reqwest::Url::parse(source) {
        Ok(url) if matches!(url.scheme(), "http" | "https") => url.origin().ascii_serialization(),
        Ok(url) => format!("{} stream", url.scheme()),
        Err(_) => {
            let path = source.split(['?', '#']).next().unwrap_or_default();
            if path.starts_with(['/', '\\']) || path.contains([':', '@', '\\']) {
                return String::from("unrecognized media source");
            }
            path.chars().take(128).collect()
        }
    }
}

#[cfg(any(feature = "ffmpeg", feature = "html"))]
fn add_sink<T: AVPlayer>(row: &mut Value, sinks: Option<&AVSinks<T>>, images: &Assets<Image>) {
    let sink = sinks.and_then(|s| s.video.as_ref());
    row["hasSink"] = json!(sinks.is_some());
    row["lastPlaybackState"] = json!(sinks.map(|s| s.sent_state.as_str_name()));
    row["appliedVolume"] = json!(sinks.and_then(|s| s.sent_volume));
    row["frameRate"] = json!(sinks
        .and_then(|s| s.rate)
        .filter(|hz| hz.is_finite() && *hz > 0.0));
    row["textureSize"] = json!(sink
        .filter(|_| T::HAS_VIDEO && sinks.is_some_and(|s| s.rate.is_some()))
        .and_then(|s| images.get(&s.image))
        .map(|image| [image.width(), image.height()]));
    row["timeSeconds"] = json!(sink
        .filter(|s| s.current_time >= 0.0)
        .map(|s| s.current_time));
    row["durationSeconds"] = json!(sink.and_then(|s| s.length));
}

#[cfg(all(test, feature = "ffmpeg"))]
mod tests {
    use super::*;

    #[test]
    fn source_labels_keep_stream_credentials_private_even_for_invalid_urls() {
        for source in [
            "https://private@bad host/path",
            "  //private@host/path",
            r"\\private@host\path",
            "https://user:private@example.com/path?key=private",
        ] {
            assert!(!source_label(source).contains("private"));
        }
        assert_eq!(source_label("videos/intro.mp4"), "videos/intro.mp4");
    }

    #[test]
    fn a_video_without_a_reported_frame_rate_can_still_have_a_texture() {
        let mut images = Assets::<Image>::default();
        let image = images.add(Image::default());
        let mut sinks = crate::video_stream::noop_sinks::<VideoPlayer>(String::new(), Some(image));
        sinks.rate = Some(0.0); // HTML media provides dimensions but no frame rate.
        let mut row = json!({});
        add_sink(&mut row, Some(&sinks), &images);
        assert!(row["frameRate"].is_null());
        assert!(row["textureSize"].is_array());
    }
}
