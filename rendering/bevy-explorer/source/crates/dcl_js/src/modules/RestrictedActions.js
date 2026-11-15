function makeEnum(entries) {
    const result = {};
    for (const [name, value] of entries) {
        result[name] = value;
        result[value] = name;
    }
    return result;
}

module.exports.ExplorerUi = makeEnum([
    ['EU_SETTINGS', 0],
    ['EU_MAP', 1],
    ['EU_BACKPACK', 2],
    ['EU_CAMERA_REEL', 3],
    ['EU_COMMUNITIES', 4],
    ['EU_PLACES', 5],
    ['EU_EVENTS', 6],
    ['EU_ITEM_PURCHASE', 7],
    ['UNRECOGNIZED', -1],
]);

module.exports.OpenExplorerUiResult = makeEnum([
    ['UNSPECIFIED', 0],
    ['OPENED', 1],
    ['WAS_ALREADY_OPEN', 2],
    ['REJECTED_NOT_CURRENT_SCENE', 3],
    ['REJECTED_FEATURE_DISABLED', 4],
    ['REJECTED_NO_USER_GESTURE', 5],
    ['UNRECOGNIZED', -1],
]);

module.exports.movePlayerTo = async function (body) {
    const success = await Deno.core.ops.op_move_player_to(
        body.newRelativePosition,
        body.cameraTarget ?? null,
        body.avatarTarget ?? null,
        body.duration ?? null,
    );
    return { success }
}

module.exports.walkPlayerTo = async function (body) {
    const success = await Deno.core.ops.op_walk_player_to(
        body.newRelativePosition,
        body.stopThreshold,
        body.timeout ?? null,
    );
    return { success }
}

module.exports.teleportTo = async function (body) {
    const parcel = body.worldCoordinates;
    await Deno.core.ops.op_teleport_to(parcel ? Number(parcel.x) : null, parcel ? Number(parcel.y) : null, body.realm ?? null);
    return {} 
}

// `mask` is the sdk's `AvatarMask`, whose only value `AM_UPPER_BODY` is 0: presence, not value, is
// the signal.
function upperBody(body) {
    return body.mask !== undefined && body.mask !== null
}

module.exports.triggerEmote = async function (body) {
    Deno.core.ops.op_emote(body.predefinedEmote, upperBody(body))
    return {}
}

module.exports.triggerSceneEmote = async function (body) {
    Deno.core.ops.op_scene_emote(body.src, body.loop, upperBody(body))
    return { success: true }
}

module.exports.stopEmote = async function (body) {
    Deno.core.ops.op_stop_emote()
    return { success: true }
}

module.exports.changeRealm = async function (body) {
    const success = await Deno.core.ops.op_change_realm(body.realm, body.message);
    return { success }
}

module.exports.openExternalUrl = async function (body) {
    const success = await Deno.core.ops.op_external_url(body.url);
    return { success }
}

module.exports.openNftDialog = async function (body) {
    await Deno.core.ops.op_open_nft_dialog(body.urn)
    return { success: true }
}
module.exports.openExplorerUi = async function (body) {
    const openResult = await Deno.core.ops.op_open_explorer_ui(Number(body.ui ?? 0))
    return { openResult }
}
module.exports.setCommunicationsAdapter = async function (body) { 
    console.error("RestrictedActions::setCommunicationsAdapter not implemented");
    return { success: false }
}
module.exports.setUiFocus = async function(body) {
    return await Deno.core.ops.op_ui_focus(true, body.elementId);
}
module.exports.clearUiFocus = async function(body) {
    return await Deno.core.ops.op_ui_focus(true);
}
module.exports.getUiFocus = async function() {
    return await Deno.core.ops.op_ui_focus(false);
}
module.exports.copyToClipboard = async function(body) {
    await Deno.core.ops.op_copy_to_clipboard(body.text);
    return {}
}
