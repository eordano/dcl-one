use std::io::Result;
use std::path::PathBuf;

mod build_quant;
mod build_schema;

/// The one `.proto` source in the tree: `library/protocol/proto` at the repository root
/// (`DCL_ONE_ROOT`, exported by the root `.cargo/config.toml`). `DCL_PROTOCOL_PROTO` names
/// another checkout of it for builds outside the repository.
fn protocol_proto_root() -> PathBuf {
    println!("cargo:rerun-if-env-changed=DCL_PROTOCOL_PROTO");
    println!("cargo:rerun-if-env-changed=DCL_ONE_ROOT");
    if let Ok(path) = std::env::var("DCL_PROTOCOL_PROTO") {
        return PathBuf::from(path);
    }
    match std::env::var("DCL_ONE_ROOT") {
        Ok(root) => PathBuf::from(root).join("library/protocol/proto"),
        Err(_) => panic!(
            "DCL_ONE_ROOT is not set: build from inside the dcl-one repository, or set \
             DCL_PROTOCOL_PROTO to a checkout of library/protocol/proto"
        ),
    }
}

fn gen_sdk_components() -> Result<()> {
    let components = [
        "asset_load",
        "asset_load_loading_state",
        "engine_info",
        "billboard",
        "raycast",
        "raycast_result",
        "mesh_renderer",
        "mesh_collider",
        "material",
        "gltf_container",
        "gltf_container_loading_state",
        "gltf_node",
        "gltf_node_state",
        "animator",
        "pointer_events",
        "pointer_events_result",
        "avatar_base",
        "avatar_emote_command",
        "avatar_equipped_data",
        "player_identity_data",
        "explorer_configuration",
        "avatar_shape",
        "avatar_attach",
        "ui_transform",
        "ui_text",
        "ui_background",
        "ui_input",
        "ui_input_result",
        "ui_canvas_information",
        "ui_dropdown",
        "ui_dropdown_result",
        "ui_scroll_result",
        "ui_layout_feedback",
        "ui_canvas",
        "text_shape",
        "pointer_lock",
        "camera_mode",
        "camera_mode_area",
        "audio_source",
        "video_player",
        "audio_stream",
        "video_event",
        "audio_event",
        "visibility_component",
        "avatar_modifier_area",
        "nft_shape",
        "tween",
        "tween_state",
        "light_source",
        "global_light",
        "texture_camera",
        "camera_layer",
        "camera_layers",
        "primary_pointer_info",
        "realm_info",
        "virtual_camera",
        "main_camera",
        "input_modifier",
        "trigger_area",
        "trigger_area_result",
        "gltf_node_modifiers",
        "skybox_time",
        "skybox",
        "sun",
        "environment_fog",
        "avatar_movement_info",
        "avatar_movement",
        "avatar_locomotion_settings",
        "physics_combined_force",
        "physics_combined_impulse",
        "particle_system",
        "explorer_ui_events_result",
        "touch_screen_controls",
        "tween_sequence",
        "audio_analysis",
        "ui_input_binding",
        "avatar_nametag",
        "explorer_item_purchase_result",
    ];

    let proto_root = protocol_proto_root();
    // A protocol update must not silently disappear at the Explorer generation boundary.
    // MapPin (1097) is deprecated and intentionally has no SDK/Explorer consumer.
    let component_dir = proto_root.join("decentraland/sdk/components");
    for entry in std::fs::read_dir(&component_dir)? {
        let path = entry?.path();
        if path.extension().is_none_or(|ext| ext != "proto") {
            continue;
        }
        let name = path.file_stem().unwrap().to_str().unwrap();
        if name == "map_pin" {
            continue;
        }
        let source = std::fs::read_to_string(&path)?;
        if source.contains("option (common.ecs_component_id)") {
            assert!(components.contains(&name), "active protocol component {name} has no Explorer binding; add its binding and runtime consumer together");
        }
    }
    let readers = include_str!("src/proto_components.rs");
    for component in components {
        let name = component
            .split('_')
            .map(|part| {
                let mut chars = part.chars();
                chars.next().unwrap().to_uppercase().to_string() + chars.as_str()
            })
            .collect::<String>();
        assert!(
            readers.contains(&format!(
                "impl DclProtoComponent for sdk::components::Pb{name} {{}}"
            )),
            "component {component} has no CRDT wire reader"
        );
    }
    let shared = |path: &str| proto_root.join(path);

    let mut sources = components
        .iter()
        .map(|component| shared(&format!("decentraland/sdk/components/{component}.proto")))
        .collect::<Vec<_>>();

    sources.push(shared(
        "decentraland/sdk/development/local_development.proto",
    ));
    sources.push(shared("decentraland/kernel/apis/comms_api.proto"));
    sources.push(shared("decentraland/kernel/comms/rfc5/ws_comms.proto"));
    sources.push(shared("decentraland/kernel/comms/rfc4/comms.proto"));
    sources.push(shared("decentraland/kernel/comms/v3/archipelago.proto"));
    // Pulse: realtime movement transport (server-authoritative). The quantization field
    // options in common/options.proto are parsed by protoc but applied app-side; prost emits
    // plain uint32 fields, so dequantization lives in the pulse transport module.
    sources.push(shared("decentraland/pulse/pulse_shared.proto"));
    sources.push(shared("decentraland/pulse/pulse_client.proto"));
    sources.push(shared("decentraland/pulse/pulse_server.proto"));

    let mut config = prost_build::Config::new();
    config.type_attribute(
        ".decentraland.sdk.components",
        "#[derive(serde::Serialize, serde::Deserialize)]\n#[serde(rename_all = \"camelCase\")]",
    );
    // Per-type serde for types outside decentraland.sdk.components (which gets serde in bulk above).
    // These are in decentraland.common or similar packages.
    let serde_components = [
        "Vector2",
        "Vector3",
        "Color3",
        "Color4",
        "ColorRange",
        "ColorKey",
        "ColorGradient",
        "Quaternion",
        "TextureUnion",
        "TextureUnion.tex",
        "AvatarTexture",
        "VideoTexture",
        "UiCanvasTexture",
        "Texture",
        "BorderRect",
        "FloatRange",
        "WsSceneMessage",
        "WsSceneMessage.message",
        "UpdateScene",
        "UpdateModel",
        "UpdateModelType",
        "VideoTracksActiveStreamsResponse",
        "VideoTracksActiveStreamsData",
    ];

    for component in serde_components {
        config.type_attribute(
            component,
            "#[derive(serde::Serialize, serde::Deserialize)]\n#[serde(rename_all = \"camelCase\")]",
        );
    }

    // ts-rs: inject `#[derive(ts_rs::TS)]` on exactly the proto types the system-api boundary
    // embeds (and their transitive fields), so system_api_types can export TypeScript for them.
    // Proto enum fields are stored as i32 by prost, so the enums themselves need no derive.
    let ts_components = [
        "Color3",
        "Vector2",
        "Vector3",
        "PBAvatarBase",
        "PBAvatarEquippedData",
        "PBPointerEvents.Entry",
        "PBPointerEvents.Info",
    ];
    for component in ts_components {
        config.type_attribute(component, "#[derive(ts_rs::TS)]");
    }

    let hash_components = [
        "PBMaterial",
        "GltfMaterial",
        "TextureUnion",
        "Texture",
        "AvatarTexture",
        "VideoTexture",
        "UiCanvasTexture",
    ];
    for component in hash_components {
        config.type_attribute(component, "#[derive(Hash)]");
    }

    // Emit a FileDescriptorSet alongside the generated code, so the component-schema
    // module can reflect over field/enum/oneof structure at runtime (prost-reflect).
    let descriptor_path =
        std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR not set"))
            .join("sdk_components_descriptor.bin");
    config.file_descriptor_set_path(&descriptor_path);

    config.compile_protos(&sources, &[proto_root.as_path()])?;

    // Generate the structural component-schema JSON from the descriptor (the curated overlay lives
    // in the editor scene now).
    let descriptor_bytes = std::fs::read(&descriptor_path).expect("read descriptor");
    let schemas_path = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR not set"))
        .join("component_schemas.json");
    build_schema::generate(&descriptor_bytes, &schemas_path);

    // Generate Pulse dequantization accessors from the (decentraland.common.quantized) field
    // options carried in the same descriptor — proto stays the single source of truth.
    let quant_path = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR not set"))
        .join("pulse_quant.rs");
    build_quant::generate(&descriptor_bytes, &quant_path);

    // The sources import common protos the list above does not name, so watch the whole tree.
    println!("cargo:rerun-if-changed={}", proto_root.display());
    println!("cargo:rerun-if-changed=build_schema.rs");
    println!("cargo:rerun-if-changed=build_quant.rs");

    Ok(())
}

fn gen_social_service() -> Result<()> {
    let proto_root = protocol_proto_root();
    let mut conf = prost_build::Config::new();
    #[cfg(feature = "social")]
    conf.service_generator(Box::new(dcl_rpc::codegen::RPCServiceGenerator::new()));
    conf.type_attribute("*", "#[derive(Debug)]");
    // Reuse common types already generated by gen_sdk_components() to avoid overwriting
    conf.extern_path(".decentraland.common", "crate::proto_components::common");
    let sources = [proto_root.join("decentraland/social_service/v2/social_service_v2.proto")];
    conf.compile_protos(&sources, &[proto_root.as_path()])?;
    Ok(())
}

fn main() -> Result<()> {
    gen_sdk_components()?;
    gen_social_service()?;
    Ok(())
}
