use bevy::prelude::*;
use dcl_component::{
    component_name_registry::{derive_component_name, make_proto_closures},
    proto_components::sdk::components::{
        PbAudioAnalysis, PbAudioEvent, PbAvatarBase, PbAvatarEmoteCommand, PbAvatarEquippedData,
        PbAvatarMovementInfo, PbAvatarNametag, PbCameraMode, PbEngineInfo,
        PbExplorerItemPurchaseResult, PbExplorerUiEventsResult, PbGltfContainerLoadingState,
        PbGltfNodeState, PbPlayerIdentityData, PbPointerEventsResult, PbPointerLock,
        PbPrimaryPointerInfo, PbRaycastResult, PbRealmInfo, PbTouchScreenControls,
        PbTriggerAreaResult, PbTweenSequence, PbTweenState, PbUiCanvasInformation,
        PbUiDropdownResult, PbUiInputBinding, PbUiInputResult, PbUiLayoutFeedback,
        PbUiScrollResult, PbVideoEvent,
    },
    ComponentNameRegistry, CrdtType, SceneComponentId,
};

/// Register components that aren't covered by add_crdt_lww_component /
/// add_crdt_go_component auto-registration: engine→scene-only ones, and scene→engine ones this
/// engine has no handler for but the editor still reads from the snapshot.
pub struct EngineComponentsPlugin;

impl Plugin for EngineComponentsPlugin {
    fn build(&self, app: &mut App) {
        let mut registry = app.world_mut().resource_mut::<ComponentNameRegistry>();

        macro_rules! reg {
            ($pb:ty, $id:expr, $crdt:expr, rw) => {{
                let (inspect, write, default) = make_proto_closures::<$pb>();
                registry.register(
                    derive_component_name::<$pb>(),
                    $id,
                    $crdt,
                    inspect,
                    Some(write),
                    Some(default),
                );
            }};
            ($pb:ty, $id:expr, $crdt:expr, ro) => {{
                let (inspect, _write, _default) = make_proto_closures::<$pb>();
                registry.register(
                    derive_component_name::<$pb>(),
                    $id,
                    $crdt,
                    inspect,
                    None,
                    None,
                );
            }};
        }

        reg!(
            PbEngineInfo,
            SceneComponentId::ENGINE_INFO,
            CrdtType::LWW_ROOT,
            ro
        );
        reg!(
            PbRaycastResult,
            SceneComponentId::RAYCAST_RESULT,
            CrdtType::LWW_ENT,
            ro
        );
        reg!(
            PbPointerEventsResult,
            SceneComponentId::POINTER_RESULT,
            CrdtType::LWW_ENT,
            ro
        );
        reg!(
            PbPrimaryPointerInfo,
            SceneComponentId::PRIMARY_POINTER_INFO,
            CrdtType::LWW_ENT,
            ro
        );
        reg!(
            PbPointerLock,
            SceneComponentId::POINTER_LOCK,
            CrdtType::LWW_ENT,
            ro
        );
        reg!(
            PbCameraMode,
            SceneComponentId::CAMERA_MODE,
            CrdtType::LWW_ENT,
            ro
        );
        reg!(
            PbTweenState,
            SceneComponentId::TWEEN_STATE,
            CrdtType::LWW_ENT,
            ro
        );
        reg!(
            PbGltfContainerLoadingState,
            SceneComponentId::GLTF_CONTAINER_LOADING_STATE,
            CrdtType::LWW_ANY,
            ro
        );
        reg!(
            PbGltfNodeState,
            SceneComponentId::GLTF_NODE_STATE,
            CrdtType::LWW_ANY,
            ro
        );
        reg!(
            PbVideoEvent,
            SceneComponentId::VIDEO_EVENT,
            CrdtType::GO_ANY,
            ro
        );
        reg!(
            PbAudioEvent,
            SceneComponentId::AUDIO_EVENT,
            CrdtType::GO_ANY,
            ro
        );
        reg!(
            PbUiDropdownResult,
            SceneComponentId::UI_DROPDOWN_RESULT,
            CrdtType::LWW_ENT,
            ro
        );
        reg!(
            PbUiInputResult,
            SceneComponentId::UI_INPUT_RESULT,
            CrdtType::LWW_ENT,
            ro
        );
        reg!(
            PbUiScrollResult,
            SceneComponentId::UI_SCROLL_RESULT,
            CrdtType::LWW_ENT,
            ro
        );
        reg!(
            PbUiLayoutFeedback,
            SceneComponentId::UI_LAYOUT_FEEDBACK,
            CrdtType::LWW_ENT,
            ro
        );
        reg!(
            PbTriggerAreaResult,
            SceneComponentId::TRIGGER_AREA_RESULT,
            CrdtType::GO_ENT,
            ro
        );
        reg!(
            PbExplorerUiEventsResult,
            SceneComponentId::EXPLORER_UI_EVENTS_RESULT,
            CrdtType::GO_ENT,
            ro
        );
        reg!(
            PbPlayerIdentityData,
            SceneComponentId::PLAYER_IDENTITY_DATA,
            CrdtType::LWW_ANY,
            ro
        );
        reg!(
            PbAvatarBase,
            SceneComponentId::AVATAR_BASE,
            CrdtType::LWW_ANY,
            ro
        );
        reg!(
            PbAvatarEquippedData,
            SceneComponentId::AVATAR_EQUIPPED_DATA,
            CrdtType::LWW_ANY,
            ro
        );
        reg!(
            PbAvatarEmoteCommand,
            SceneComponentId::AVATAR_EMOTE_COMMAND,
            CrdtType::GO_ENT,
            ro
        );
        reg!(
            PbAvatarMovementInfo,
            SceneComponentId::AVATAR_MOVEMENT_INFO,
            CrdtType::LWW_ANY,
            ro
        );

        reg!(
            PbUiCanvasInformation,
            SceneComponentId::CANVAS_INFO,
            CrdtType::LWW_ROOT,
            ro
        );
        reg!(
            PbRealmInfo,
            SceneComponentId::REALM_INFO,
            CrdtType::LWW_ROOT,
            ro
        );
        // Scene→engine components this engine does not implement (TouchScreenControls is a no-op
        // without on-screen controls). Registered so the snapshot names them and the editor can
        // author and save them; AudioAnalysis and ExplorerItemPurchaseResult carry engine output, so
        // they stay read-only.
        reg!(
            PbTouchScreenControls,
            SceneComponentId::TOUCH_SCREEN_CONTROLS,
            CrdtType::LWW_ROOT,
            rw
        );
        reg!(
            PbTweenSequence,
            SceneComponentId::TWEEN_SEQUENCE,
            CrdtType::LWW_ENT,
            rw
        );
        reg!(
            PbAudioAnalysis,
            SceneComponentId::AUDIO_ANALYSIS,
            CrdtType::LWW_ENT,
            ro
        );
        reg!(
            PbUiInputBinding,
            SceneComponentId::UI_INPUT_BINDING,
            CrdtType::LWW_ENT,
            rw
        );
        reg!(
            PbAvatarNametag,
            SceneComponentId::AVATAR_NAMETAG,
            CrdtType::LWW_ANY,
            rw
        );
        reg!(
            PbExplorerItemPurchaseResult,
            SceneComponentId::EXPLORER_ITEM_PURCHASE_RESULT,
            CrdtType::GO_ANY,
            ro
        );

        // Transform: special-cased via DclTransformAndParent (DCL binary format, not prost)
        register_transform(&mut registry);
    }
}

fn register_transform(registry: &mut ComponentNameRegistry) {
    use dcl_component::{
        transform_and_parent::DclTransformAndParent, DclReader, DclWriter, FromDclReader,
        ToDclWriter,
    };

    let inspect = std::sync::Arc::new(|bytes: &[u8]| {
        let mut reader = DclReader::new(bytes);
        let t = DclTransformAndParent::from_reader(&mut reader)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        serde_json::to_string_pretty(&t).map_err(|e| anyhow::anyhow!("{e}"))
    });

    let write = std::sync::Arc::new(|json: &str| {
        let t: DclTransformAndParent =
            serde_json::from_str(json).map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut buf = Vec::new();
        let mut writer = DclWriter::new(&mut buf);
        t.to_writer(&mut writer);
        Ok(buf)
    });

    let default = std::sync::Arc::new(|| {
        serde_json::to_string_pretty(&DclTransformAndParent::default())
            .map_err(|e| anyhow::anyhow!("{e}"))
    });

    registry.register(
        "Transform".to_string(),
        SceneComponentId::TRANSFORM,
        CrdtType::LWW_ANY,
        inspect,
        Some(write),
        Some(default),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use dcl_component::proto_components::sdk::components::{
        common::InputAction, pb_touch_screen_controls::TouchInput,
    };
    use prost::Message;

    #[test]
    fn touch_screen_controls_reads_by_name_as_json() {
        let mut app = App::new();
        app.init_resource::<ComponentNameRegistry>();
        app.add_plugins(EngineComponentsPlugin);
        let registry = app.world().resource::<ComponentNameRegistry>();

        assert_eq!(
            registry.name_for_id(SceneComponentId::TOUCH_SCREEN_CONTROLS),
            Some("TouchScreenControls")
        );
        let bytes = PbTouchScreenControls {
            touch_inputs: vec![TouchInput {
                input_action: InputAction::IaAction3 as i32,
                hide: true,
                icon: None,
            }],
            main_action: Some(InputAction::IaPrimary as i32),
            hide_joystick: true,
            hide_crosshair: false,
        }
        .encode_to_vec();
        let entry = registry.get_by_name("TouchScreenControls").unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&(entry.inspect)(&bytes).unwrap()).unwrap();
        assert_eq!(
            json["touchInputs"][0]["inputAction"],
            InputAction::IaAction3 as i32
        );
        assert_eq!(json["touchInputs"][0]["hide"], true);
        assert_eq!(json["mainAction"], InputAction::IaPrimary as i32);
        assert_eq!(json["hideJoystick"], true);
    }

    #[test]
    fn unimplemented_sdk_components_have_names() {
        let mut app = App::new();
        app.init_resource::<ComponentNameRegistry>();
        app.add_plugins(EngineComponentsPlugin);
        let registry = app.world().resource::<ComponentNameRegistry>();

        for (id, name, writable) in [
            (
                SceneComponentId::TOUCH_SCREEN_CONTROLS,
                "TouchScreenControls",
                true,
            ),
            (SceneComponentId::TWEEN_SEQUENCE, "TweenSequence", true),
            (SceneComponentId::AUDIO_ANALYSIS, "AudioAnalysis", false),
            (SceneComponentId::UI_INPUT_BINDING, "UiInputBinding", true),
            (SceneComponentId::AVATAR_NAMETAG, "AvatarNametag", true),
            (
                SceneComponentId::EXPLORER_ITEM_PURCHASE_RESULT,
                "ExplorerItemPurchaseResult",
                false,
            ),
        ] {
            assert_eq!(registry.name_for_id(id), Some(name));
            assert_eq!(
                registry.get_by_name(name).unwrap().write.is_some(),
                writable,
                "{name}"
            );
        }
    }
}
