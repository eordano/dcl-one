use bevy::{
    ecs::system::{lifetimeless::SRes, SystemParamItem},
    prelude::*,
};
use common::structs::AppConfig;

use super::{sync_prepasses, AppSetting, EnumAppSetting};

#[derive(Debug, PartialEq, Eq)]
pub enum AvatarOutlineSetting {
    Off,
    On,
}

impl EnumAppSetting for AvatarOutlineSetting {
    fn variants() -> Vec<Self> {
        vec![Self::Off, Self::On]
    }

    fn name(&self) -> String {
        match self {
            AvatarOutlineSetting::Off => "Off",
            AvatarOutlineSetting::On => "On",
        }
        .to_owned()
    }
}

impl AppSetting for AvatarOutlineSetting {
    type Param = SRes<AppConfig>;

    fn title() -> String {
        "Avatar Outline".to_owned()
    }

    fn description(&self) -> String {
        "Dark edge outline around avatars.\n\nOn: avatars are drawn with an edge outline so they stand out from the world. Off: no outline is drawn, and the outline's per-pixel shading cost is skipped.".to_owned()
    }

    fn category() -> super::SettingCategory {
        super::SettingCategory::Graphics
    }

    fn save(&self, config: &mut AppConfig) {
        config.graphics.avatar_outline = matches!(self, AvatarOutlineSetting::On);
    }

    fn load(config: &AppConfig) -> Self {
        if config.graphics.avatar_outline {
            Self::On
        } else {
            Self::Off
        }
    }

    fn apply_to_camera(
        &self,
        config: &SystemParamItem<Self::Param>,
        mut commands: Commands,
        camera_entity: Entity,
    ) {
        if let Ok(mut cmds) = commands.get_entity(camera_entity) {
            sync_prepasses(&mut cmds, &config.graphics);
        }
    }
}
