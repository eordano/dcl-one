//! `/ban_block_state` and the "Bans and blocks" debug section.

use alloy_core::primitives::Address;
use bevy::{
    app::{App, Plugin},
    ecs::system::Res,
    time::Time,
};
use bevy_console::ConsoleCommand;
use common::util::AsH160;
use console::{AddDebugSection, DebugInput, DebugInputKind, DebugSection, DoAddConsoleCommand};
use serde_json::{json, Map, Value};
use wallet::Wallet;

use crate::{
    AdmissionView, BanBlock, BlockDirection, Blocks, PlatformBanState, SceneAdmission,
    SceneRoomView,
};

pub(crate) struct BanBlockDebugPlugin;

impl Plugin for BanBlockDebugPlugin {
    fn build(&self, app: &mut App) {
        app.add_console_command::<BanBlockStateCommand, _>(ban_block_state)
            .add_debug_section(DebugSection {
                title: "Bans and blocks",
                query: "/ban_block_state",
                fields: &[
                    ("Blocks", "blocks.state"),
                    ("Blocked", "blocks.blocked"),
                    ("Blocked by", "blocks.blockedBy"),
                    ("Platform ban", "platformBan.isBanned"),
                    ("Ban reason", "platformBan.reason"),
                    ("Ban matched on", "platformBan.matchedOn"),
                    ("Ban source", "platformBan.source"),
                    ("Checked (s ago)", "platformBan.checkedAgoSecs"),
                    ("Scene admission", "sceneAdmission.state"),
                    ("Refusal scope", "sceneAdmission.scope"),
                    ("Scene room", "sceneRoom.state"),
                    ("Scene banned addresses", "sceneRoom.bannedAddresses"),
                    ("Scene admins", "sceneRoom.sceneAdmins"),
                    ("Lookup: blocked either way", "lookup.isBlockedEitherWay"),
                    ("Lookup: direction", "lookup.direction"),
                    ("Lookup: banned in scene", "lookup.bannedInScene"),
                    ("Lookup: admin", "lookup.adminPermissions"),
                ],
                actions: &[("Re-check platform ban", "/ban_block_refresh")],
                inputs: &[DebugInput {
                    label: "Look up address",
                    field: "lookup.address",
                    command: "/ban_block_state {value}",
                    kind: DebugInputKind::Text,
                }],
            });
    }
}

/// The ban and block status, as JSON; with an address, also that address's standing.
#[derive(clap::Parser, ConsoleCommand)]
#[command(name = "/ban_block_state")]
struct BanBlockStateCommand {
    address: Option<String>,
}

fn ban_block_state(
    mut input: ConsoleCommand<BanBlockStateCommand>,
    ban_block: BanBlock,
    wallet: Res<Wallet>,
    time: Res<Time>,
) {
    let Some(Ok(command)) = input.take() else {
        return;
    };
    let lookup = match command.address.as_deref() {
        None => None,
        Some(raw) => match raw.as_h160() {
            Some(address) => Some(address),
            None => {
                input.reply_failed("invalid address");
                return;
            }
        },
    };
    input.reply_ok(
        snapshot(
            &ban_block,
            wallet.address(),
            lookup,
            time.elapsed_secs_f64(),
        )
        .to_string(),
    );
}

fn hex(address: Address) -> String {
    format!("{address:#x}")
}

fn addresses(set: &bevy::platform::collections::HashSet<Address>) -> Value {
    let mut list: Vec<String> = set.iter().copied().map(hex).collect();
    list.sort();
    json!(list)
}

/// Every `null` reading has its reason in `unavailable`, keyed by the field path.
pub(crate) fn snapshot(
    ban_block: &BanBlock,
    own: Option<Address>,
    lookup: Option<Address>,
    now: f64,
) -> Value {
    let mut unavailable = Map::new();
    let platform_ban = platform_ban_json(ban_block, now, &mut unavailable);
    let blocks = ban_block.blocks();
    let blocks_json = match blocks {
        Blocks::Live {
            blocked,
            blocked_by,
        } => json!({
            "state": "live",
            "reason": null,
            "blocked": addresses(blocked),
            "blockedBy": addresses(blocked_by),
        }),
        Blocks::Unavailable { reason } => {
            let why = match reason {
                crate::BlocksUnavailableReason::SocialFeatureOff => "social feature off",
                crate::BlocksUnavailableReason::NotSignedIn => "not signed in",
                crate::BlocksUnavailableReason::SocialNotConnected => "social not connected",
            };
            unavailable.insert("blocks.blocked".into(), json!(why));
            unavailable.insert("blocks.blockedBy".into(), json!(why));
            json!({
                "state": "unavailable",
                "reason": reason,
                "blocked": [],
                "blockedBy": [],
            })
        }
    };
    let room = ban_block.scene_room();
    let lookup_json = match lookup {
        Some(address) => {
            let direction = blocks.block_direction(address);
            json!({
                "address": hex(address),
                "isBlockedEitherWay": direction != BlockDirection::None,
                "direction": direction,
                "bannedInScene": room.is_addr_banned_in_scene(address),
                "adminPermissions": room.admin_permissions_for_addr(address),
            })
        }
        None => {
            unavailable.insert("lookup".into(), json!("no address given"));
            Value::Null
        }
    };
    json!({
        "address": own.map(hex),
        "blocks": blocks_json,
        "platformBan": platform_ban,
        "sceneAdmission": scene_admission_json(ban_block.scene_admission(), &mut unavailable),
        "sceneRoom": scene_room_json(room, &mut unavailable),
        "lookup": lookup_json,
        "unavailable": unavailable,
    })
}

fn platform_ban_json(
    ban_block: &BanBlock,
    now: f64,
    unavailable: &mut Map<String, Value>,
) -> Value {
    let ago = |checked_at: f64| (now - checked_at).max(0.0).round();
    match ban_block.platform_ban() {
        PlatformBanState::Unknown => {
            unavailable.insert("platformBan.isBanned".into(), json!("not checked yet"));
            unavailable.insert("platformBan.reason".into(), json!("not checked yet"));
            unavailable.insert("platformBan.matchedOn".into(), json!("not checked yet"));
            unavailable.insert("platformBan.source".into(), json!("not checked yet"));
            unavailable.insert(
                "platformBan.checkedAgoSecs".into(),
                json!("not checked yet"),
            );
            json!({
                "state": "unknown",
                "isBanned": null, "source": null, "reason": null, "matchedOn": null,
                "customMessage": null, "expiresAt": null, "checkedAgoSecs": null, "error": null,
            })
        }
        PlatformBanState::Failed { error, checked_at } => {
            let why = format!("check failed: {error}");
            unavailable.insert("platformBan.isBanned".into(), json!(why));
            unavailable.insert("platformBan.reason".into(), json!(why));
            unavailable.insert("platformBan.matchedOn".into(), json!(why));
            unavailable.insert("platformBan.source".into(), json!(why));
            json!({
                "state": "failed",
                "isBanned": null, "source": null, "reason": null, "matchedOn": null,
                "customMessage": null, "expiresAt": null, "checkedAgoSecs": ago(*checked_at),
                "error": error,
            })
        }
        PlatformBanState::Checked { ban, checked_at } => {
            match ban {
                None => {
                    unavailable.insert("platformBan.reason".into(), json!("not banned"));
                    unavailable.insert("platformBan.matchedOn".into(), json!("not banned"));
                    unavailable.insert("platformBan.source".into(), json!("not banned"));
                }
                Some(ban) if ban.matched_on.is_some() => {
                    unavailable.insert("platformBan.reason".into(), json!("record withheld"));
                }
                Some(_) => {
                    unavailable.insert("platformBan.matchedOn".into(), json!("record served"));
                }
            }
            json!({
                "state": "checked",
                "isBanned": ban.is_some(),
                "source": ban_block.platform_ban_source(),
                "reason": ban.as_ref().map(|b| b.reason.clone()).filter(|r| !r.is_empty()),
                "matchedOn": ban.as_ref().and_then(|b| b.matched_on.clone()),
                "customMessage": ban.as_ref().and_then(|b| b.custom_message.clone()),
                "expiresAt": ban.as_ref().and_then(|b| b.expires_at.clone()),
                "checkedAgoSecs": ago(*checked_at),
                "error": null,
            })
        }
    }
}

fn scene_admission_json(admission: &SceneAdmission, unavailable: &mut Map<String, Value>) -> Value {
    match admission.view() {
        AdmissionView::Unknown => {
            unavailable.insert(
                "sceneAdmission.scope".into(),
                json!("no scene admission yet"),
            );
            json!({ "state": "unknown", "scope": null, "message": null, "sceneId": null })
        }
        AdmissionView::Allowed { scene_id } => {
            unavailable.insert("sceneAdmission.scope".into(), json!("admitted"));
            json!({ "state": "allowed", "scope": null, "message": null, "sceneId": scene_id })
        }
        AdmissionView::Refused {
            scope,
            message,
            scene_id,
        } => json!({ "state": "refused", "scope": scope, "message": message, "sceneId": scene_id }),
    }
}

fn scene_room_json(room: SceneRoomView, unavailable: &mut Map<String, Value>) -> Value {
    match room {
        SceneRoomView::NotConnected => {
            unavailable.insert("sceneRoom.sceneId".into(), json!("no scene room connected"));
            unavailable.insert(
                "sceneRoom.bannedAddresses".into(),
                json!("no scene room connected"),
            );
            unavailable.insert(
                "sceneRoom.sceneAdmins".into(),
                json!("no scene room connected"),
            );
            json!({ "state": "not_connected", "sceneId": null, "bannedAddresses": [], "sceneAdmins": [] })
        }
        SceneRoomView::Connected {
            scene_id,
            banned_addresses,
            scene_admins,
        } => json!({
            "state": "connected",
            "sceneId": scene_id,
            "bannedAddresses": addresses(banned_addresses),
            "sceneAdmins": addresses(scene_admins),
        }),
    }
}
