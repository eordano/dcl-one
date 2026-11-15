use std::str::FromStr;

use alloy_core::primitives::{Address, B256};
use alloy_signer_local::PrivateKeySigner;
use analytics::segment_system::SegmentConfig;
use bevy::{
    app::AppExit,
    prelude::*,
    tasks::{IoTaskPool, Task},
    window::PrimaryWindow,
};
use bevy_dui::{DuiCommandsExt, DuiEntityCommandsExt, DuiProps, DuiRegistry};
use common::{
    autopilot::Autopilot,
    profile::SerializedProfile,
    rpc::{RpcResultReceiver, RpcResultSender},
    sets::SceneSets,
    structs::{
        ActiveDialog, AppConfig, ChainLink, CurrentRealm, PreviewMode, PreviousLogin, SystemAudio,
        WorldHold, ZOrder, TERMS_VERSION,
    },
    util::{TaskCompat, TaskExt},
};
use comms::profile::{
    get_default_look, get_remote_profile, CurrentUserProfile, UserProfile, DEFAULT_LOOKS,
};
use ipfs::{IpfsAssetServer, IpfsIo};
use scene_runner::Toaster;
use system_bridge::{NativeUi, NewAccountPick, SystemApi, WelcomeState, PROFILE_FETCH_FAILED};
use tokio::sync::oneshot::error::TryRecvError;
use ui_core::{
    button::DuiButton,
    ui_actions::{close_ui_happy, Click, EventCloneExt, On},
};
#[cfg(not(target_arch = "wasm32"))]
use wallet::local_guest;
use wallet::{
    browser_auth::{finish_remote_ephemeral_request, init_remote_ephemeral_request, EphemeralKey},
    Wallet,
};

#[path = "client_login.rs"]
mod client_login;

pub struct LoginPlugin;

impl Plugin for LoginPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(client_login::ClientLoginPlugin);
        app.add_event::<LoginType>().add_systems(
            Update,
            (
                (login, update_profile_for_realm).run_if(in_state(ui_core::State::Ready)),
                process_login_bridge.in_set(SceneSets::PostLoop), // use post loop here so that the PlayerIdentityData can be picked up in RestrictedActions
            ),
        );
    }
}

#[derive(Event, Clone)]
enum LoginType {
    ExistingRemote,
    NewRemote,
    Guest,
    Cancel,
}

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn login(
    mut commands: Commands,
    wallet: Res<Wallet>,
    mut req_code: Local<Option<RpcResultReceiver<Result<Option<i32>, String>>>>,
    mut req_done: Local<Option<RpcResultReceiver<Result<(), String>>>>,
    mut logins: EventReader<LoginType>,
    mut dialog: Local<Option<Entity>>,
    mut toaster: Toaster,
    dui: Res<DuiRegistry>,
    active_dialog: Res<ActiveDialog>,
    mut motd_shown: Local<bool>,
    mut bridge: EventWriter<SystemApi>,
    native_active: Res<NativeUi>,
    config: Res<AppConfig>,
) {
    if !native_active.login {
        return;
    }

    if !*motd_shown {
        let Some(permit) = active_dialog.try_acquire() else {
            return;
        };
        let components = commands
            .spawn_template(
                &dui,
                "motd",
                DuiProps::default().with_prop(
                    "buttons",
                    vec![DuiButton::new_enabled("Ok", close_ui_happy)],
                ),
            )
            .unwrap();
        commands
            .entity(components.root)
            .try_insert((permit, ZOrder::Login.default()));
        *motd_shown = true;
        return;
    }

    // cleanup if we're done
    if wallet.address().is_some() {
        if let Some(mut commands) = dialog.and_then(|d| commands.get_entity(d).ok()) {
            commands.despawn();
        }
        *dialog = None;
        *req_code = None;
        *req_done = None;
        return;
    }

    // create dialog
    if dialog.is_none() && req_done.is_none() {
        let Some(permit) = active_dialog.try_acquire() else {
            return;
        };

        let previous_login = get_previous_login(&config);

        let mut dlg = commands.spawn(permit);
        *dialog = Some(dlg.id());
        dlg.apply_template(
            &dui,
            "login",
            DuiProps::new()
                .with_prop("allow-reuse", previous_login.is_some())
                .with_prop("reuse", LoginType::ExistingRemote.send_value_on::<Click>())
                .with_prop("connect", LoginType::NewRemote.send_value_on::<Click>())
                .with_prop("guest", LoginType::Guest.send_value_on::<Click>())
                .with_prop(
                    "quit",
                    On::<Click>::new(|mut e: EventWriter<AppExit>| {
                        e.write_default();
                    }),
                ),
        )
        .unwrap();

        return;
    }

    // handle task results
    if let Some(mut t) = req_code.take() {
        match t.try_recv() {
            Ok(Ok(_)) => {
                // no code to show: the waiting dialog spawned on click stays up
            }
            Ok(Err(e)) => {
                toaster.add_toast("login profile", format!("Login failed: {e}"));
                if let Some(mut commands) = dialog.and_then(|d| commands.get_entity(d).ok()) {
                    commands.despawn();
                    *dialog = None;
                }
            }
            Err(TryRecvError::Empty) => {
                *req_code = Some(t);
            }
            Err(e) => {
                warn!("unexpected {e}");
            }
        }
    }

    if let Some(mut t) = req_done.take() {
        match t.try_recv() {
            Ok(Ok(())) => {
                *dialog = None;
            }
            Ok(Err(e)) => {
                error!("{e}");
                toaster.add_toast("login profile", format!("Login failed: {e}"));
                if let Some(mut commands) = dialog.and_then(|d| commands.get_entity(d).ok()) {
                    commands.despawn();
                }
                *dialog = None;
            }
            Err(TryRecvError::Empty) => {
                *req_done = Some(t);
            }
            Err(e) => {
                warn!("unexpected {e}");
            }
        }
    }

    // handle click
    if let Some(login) = logins.read().last() {
        if let Some(mut commands) = dialog.and_then(|d| commands.get_entity(d).ok()) {
            commands.despawn();
            *dialog = None;
        }

        match login {
            LoginType::ExistingRemote => {
                info!("existing remote");
                commands.send_event(SystemAudio(
                    "embedded://sounds/ui/toggle_enable.wav".to_owned(),
                ));
                let (sx, rx) = RpcResultSender::<Result<(), String>>::channel();
                bridge.write(SystemApi::LoginPrevious(false, sx));
                *req_done = Some(rx);
            }
            LoginType::NewRemote => {
                info!("new remote");

                commands.send_event(SystemAudio(
                    "embedded://sounds/ui/toggle_enable.wav".to_owned(),
                ));
                let (scode, rcode) = RpcResultSender::<Result<Option<i32>, String>>::channel();
                let (sx, rx) = RpcResultSender::<Result<(), String>>::channel();
                bridge.write(SystemApi::LoginNew(false, scode, sx));
                *req_code = Some(rcode);
                *req_done = Some(rx);

                let components = commands
                    .spawn(ZOrder::Login.default())
                    .apply_template(
                        &dui,
                        "cancel-login",
                        DuiProps::new().with_prop(
                            "buttons",
                            vec![DuiButton::new_enabled(
                                "Cancel",
                                |mut e: EventWriter<LoginType>| {
                                    e.write(LoginType::Cancel);
                                },
                            )],
                        ),
                    )
                    .unwrap();

                *dialog = Some(components.root);
            }
            LoginType::Guest => {
                info!("guest");
                toaster.add_toast(
                    "login profile",
                    "Warning: Guest profile will not persist beyond the current session",
                );
                commands.send_event(SystemAudio(
                    "embedded://sounds/ui/toggle_enable.wav".to_owned(),
                ));
                bridge.write(SystemApi::LoginGuest);
            }
            LoginType::Cancel => {
                *req_code = None;
                *req_done = None;
                *dialog = None;
                commands.send_event(SystemAudio(
                    "embedded://sounds/ui/toggle_disable.wav".to_owned(),
                ));
                bridge.write(SystemApi::LoginCancel);
            }
        }
    }
}

#[allow(clippy::type_complexity)]
fn update_profile_for_realm(
    realm: Res<CurrentRealm>,
    wallet: Res<Wallet>,
    mut current_profile: ResMut<CurrentUserProfile>,
    mut task: Local<Option<Task<Result<Option<UserProfile>, anyhow::Error>>>>,
    ipfas: IpfsAssetServer,
) {
    if realm.is_changed() && !wallet.is_guest() {
        if let Some(address) = wallet.address() {
            *task = Some(
                IoTaskPool::get().spawn_compat(get_remote_profile(address, ipfas.ipfs().clone())),
            );
        }
    }

    if let Some(mut t) = task.take() {
        match t.complete() {
            Some(Ok(Some(profile))) => current_profile.adopt_server_profile(profile),
            Some(Ok(None)) | Some(Err(_)) => {
                // keep existing profile
            }
            None => *task = Some(t),
        }
    }
}

fn get_previous_login(config: &AppConfig) -> Option<PreviousLogin> {
    let previous_login = config.previous_login.clone();

    let mut expired = false;
    if let Some(prev) = previous_login.as_ref() {
        for link in &prev.auth {
            if link.ty == "ECDSA_EPHEMERAL" {
                for line in link.payload.lines() {
                    if line.starts_with("Expiration:") {
                        let exp = line.split_once(':').unwrap().1;
                        if let Ok(exp) = chrono::DateTime::<chrono::Utc>::from_str(exp.trim()) {
                            let now: chrono::DateTime<chrono::Utc> =
                                chrono::DateTime::from_timestamp_millis(
                                    web_time::SystemTime::now()
                                        .duration_since(web_time::UNIX_EPOCH)
                                        .unwrap()
                                        .as_millis() as i64,
                                )
                                .unwrap();
                            if now > exp {
                                warn!("previous login expired, removing");
                                expired = true;
                            }
                        }
                    }
                }
            }
        }
    }

    if expired {
        None
    } else {
        previous_login
    }
}

/// Decode a base64(JSON) AuthIdentity into the pieces the wallet needs: the root (signer)
/// address, the ephemeral key (just its address when the page holds the key), and the delegate
/// auth chain (the ECDSA_EPHEMERAL link(s), i.e. the chain WITHOUT the SIGNER entry — matching
/// what `finish_remote_ephemeral_request` / `get_previous_login` use).
///
/// This is the standard Decentraland AuthIdentity, so it is identical regardless of how the
/// user signed in (wallet/MetaMask, social, OTP, magic). The web page just reads it from
/// localStorage and forwards it — there is nothing login-method-specific here.
fn parse_auth_identity(payload: &str) -> Result<(Address, EphemeralKey, Vec<ChainLink>), String> {
    let parts = wallet::browser_auth::parse_auth_identity(payload)
        .and_then(wallet::browser_auth::auth_identity_key_parts)
        .map_err(|e| e.to_string())?;
    if matches!(parts.1, EphemeralKey::Remote(_)) && !wallet::remote_signer::remote_sign_available()
    {
        return Err("identity has no ephemeral private key".to_owned());
    }
    Ok(parts)
}

/// Fetch the profile, retrying on failure. Ok(None) means the user has no profile (or
/// `default_on_error` was set); a persistent failure must otherwise fail the login rather
/// than fall back to a default profile, or we would deploy the default over the user's
/// existing server-side profile. Retries are patient because the realm may still be
/// resolving when login runs (on web the page can fire `/login_identity` at boot), which
/// reads as a fetch failure. Errors carry the PROFILE_FETCH_FAILED prefix so UIs can
/// offer retrying with `default_on_error`.
async fn get_profile_with_retry(
    address: Address,
    ipfs: std::sync::Arc<IpfsIo>,
    default_on_error: bool,
) -> Result<Option<UserProfile>, String> {
    const ATTEMPTS: u32 = 5;
    let mut last_error = String::default();
    for attempt in 0..ATTEMPTS {
        if attempt > 0 {
            async_std::task::sleep(std::time::Duration::from_secs(2)).await;
        }
        match get_remote_profile(address, ipfs.clone()).await {
            Ok(maybe_profile) => return Ok(maybe_profile),
            Err(e) => last_error = e.to_string(),
        }
    }
    if default_on_error {
        warn!("continuing with default profile after fetch failure: {last_error}");
        return Ok(None);
    }
    Err(format!("{PROFILE_FETCH_FAILED}: {last_error}"))
}

/// The profile a login lands with.
enum LoginProfile {
    /// On the server.
    Deployed(UserProfile),
    /// Built here for an account with none; deployed on entry, or by the welcome page.
    New(UserProfile),
}

async fn login_profile(
    root_address: Address,
    ipfs: std::sync::Arc<IpfsIo>,
    default_on_error: bool,
    pick: Option<NewAccountPick>,
) -> Result<LoginProfile, String> {
    match get_profile_with_retry(root_address, ipfs.clone(), default_on_error).await? {
        Some(profile) => Ok(LoginProfile::Deployed(profile)),
        None => Ok(LoginProfile::New(
            new_account_profile(root_address, ipfs, pick).await,
        )),
    }
}

fn new_profile(address: Address, ipfs: &IpfsIo) -> UserProfile {
    UserProfile {
        version: 0,
        content: SerializedProfile {
            has_connected_web3: Some(true),
            eth_address: format!("{address:#x}"),
            user_id: Some(format!("{address:#x}")),
            ..Default::default()
        },
        base_url: ipfs.contents_endpoint().unwrap_or_default(),
    }
}

/// Name parts for a new account. Every first + last pair must be alphanumeric and at most 15
/// characters, the unclaimed-name rule shared by the HUD, the account site and the other
/// clients (and their `@mention` patterns); see the test below.
const GUEST_FIRST_NAMES: &[&str] = &[
    "Astra", "Bolt", "Cinder", "Dusk", "Ember", "Flint", "Gale", "Haze", "Indigo", "Jade", "Kite",
    "Lumen", "Mica", "Nimbus", "Onyx", "Pixel", "Quill", "Rune", "Sable", "Talon", "Umber",
    "Vesper", "Wren", "Xenon", "Yarrow", "Zephyr", "Comet", "Delta", "Echo", "Nova",
];
const GUEST_LAST_NAMES: &[&str] = &[
    "Ashgrove",
    "Deepwater",
    "Fernbrook",
    "Frostvale",
    "Glassmoor",
    "Greywood",
    "Highmoor",
    "Ironwood",
    "Mistgrove",
    "Moonvale",
    "Northwind",
    "Oakhollow",
    "Rainfield",
    "Riverbend",
    "Starfall",
    "Stonehill",
    "Sunridge",
    "Thornwood",
    "Wildmarsh",
    "Windward",
];

/// The name and look are derived from the address so a new account comes back the same if the
/// first deploy didn't land; an EOA address is a keccak digest, so the bytes are uniform.
fn guest_name(address: &Address) -> String {
    let bytes = address.as_slice();
    format!(
        "{}{}",
        GUEST_FIRST_NAMES[bytes[4] as usize % GUEST_FIRST_NAMES.len()],
        GUEST_LAST_NAMES[bytes[5] as usize % GUEST_LAST_NAMES.len()]
    )
}

fn guest_look_index(address: &Address) -> u32 {
    let bytes = address.as_slice();
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) % DEFAULT_LOOKS + 1
}

/// Whether a name follows the unclaimed-name rule (see GUEST_FIRST_NAMES).
fn valid_unclaimed_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 15 && name.chars().all(|c| c.is_ascii_alphanumeric())
}

/// An account without a profile gets a name and one of the curated default looks, so it
/// doesn't start as every other new account: the pick's when it carries them, else derived
/// from the address. Falls back to the engine's default look if the catalyst can't provide one.
async fn new_account_profile(
    address: Address,
    ipfs: std::sync::Arc<IpfsIo>,
    pick: Option<NewAccountPick>,
) -> UserProfile {
    let pick = pick.unwrap_or_default();
    let index = pick
        .look
        .filter(|look| (1..=DEFAULT_LOOKS).contains(look))
        .unwrap_or_else(|| guest_look_index(&address));
    let look = match get_default_look(ipfs.clone(), index).await {
        Ok(Some(look)) => Some(look.content.avatar),
        Ok(None) => {
            warn!("default look {index} not found");
            None
        }
        Err(e) => {
            warn!("default look {index}: {e}");
            None
        }
    };
    let mut profile = new_profile(address, &ipfs);
    profile.content.name = pick
        .name
        .filter(|name| valid_unclaimed_name(name))
        .unwrap_or_else(|| guest_name(&address));
    if let Some(look) = look {
        let own = &mut profile.content.avatar;
        own.body_shape = look.body_shape;
        own.wearables = look.wearables;
        own.force_render = look.force_render;
        own.eyes = look.eyes;
        own.hair = look.hair;
        own.skin = look.skin;
        // snapshots are the look's own; ours are empty until something renders them
    }
    profile
}

const ACCOUNT_LOCKED: &str = "the account cannot change after entering a realm";

#[cfg(not(target_arch = "wasm32"))]
const LOCAL_GUEST_AUTOPILOT_ONLY: &str = "only the benchmark autopilot signs in as the local guest";

#[cfg(not(target_arch = "wasm32"))]
const LOCAL_GUEST_KEY_UNREADABLE: &str =
    "the local guest's root key in the config is not a key; it is left as it is";

/// The root key of the local guest: the one in the config, or a new one when the config has none
/// (the caller saves the config, so the account is the same from then on). Whether it is new
/// comes with it. A kept key that is not a key is an error and stays in the config: a new key
/// would sign the run in as another account. Desktop only: the web build has no code that makes
/// or reads a root key, the shell page keeps its own.
#[cfg(not(target_arch = "wasm32"))]
fn local_guest_root(config: &mut AppConfig) -> Result<(PrivateKeySigner, bool), &'static str> {
    if let Some(key) = config.guest_root_key.as_deref() {
        return PrivateKeySigner::from_slice(key)
            .map(|root| (root, false))
            .map_err(|_| LOCAL_GUEST_KEY_UNREADABLE);
    }
    let root = local_guest::new_root_key();
    config.guest_root_key = Some(root.to_bytes().to_vec());
    Ok((root, true))
}

/// What a finished login leaves in the config, and whether that changed it. A benchmark run
/// leaves nothing: its guest signs a new identity every run from the root key the config keeps,
/// so a stored login (a person's account) is neither replaced nor removed by it.
fn remember_login(
    config: &mut AppConfig,
    autopilot: bool,
    root_address: Address,
    ephemeral_key: &EphemeralKey,
    auth: &[ChainLink],
    guest_account: bool,
) -> bool {
    if autopilot {
        return false;
    }
    match ephemeral_key {
        EphemeralKey::Local(local_wallet) => {
            config.previous_login = Some(PreviousLogin {
                root_address,
                ephemeral_key: local_wallet.to_bytes().to_vec(),
                auth: auth.to_vec(),
                guest_account,
            });
            true
        }
        // the page holds the key, so there is none to store, and none from an earlier login
        // should stay behind
        EphemeralKey::Remote(_) => config.previous_login.take().is_some(),
    }
}

fn account_locked(wallet: &Wallet, world_hold: &Option<Res<WorldHold>>) -> bool {
    world_hold.is_none() && wallet.address().is_some()
}

#[allow(clippy::type_complexity, clippy::too_many_arguments)]
fn process_login_bridge(
    mut e: EventReader<SystemApi>,
    ipfas: IpfsAssetServer,
    mut login_task: Local<
        Option<
            Task<
                Result<
                    (
                        Address,
                        EphemeralKey,
                        Vec<ChainLink>,
                        bool,
                        LoginProfile,
                        RpcResultSender<Result<(), String>>,
                    ),
                    (),
                >,
            >,
        >,
    >,
    mut wallet: ResMut<Wallet>,
    mut segment_config: ResMut<SegmentConfig>,
    mut login_events: ResMut<client_login::ClientLoginDelivery>,
    mut current_profile: ResMut<CurrentUserProfile>,
    mut window: Query<&mut Window, With<PrimaryWindow>>,
    mut config: ResMut<AppConfig>,
    world_hold: Option<Res<WorldHold>>,
    preview: Res<PreviewMode>,
    autopilot: Option<Res<Autopilot>>,
) {
    for ev in e.read().cloned() {
        match ev {
            // Desktop packages have no published update feed.
            SystemApi::CheckForUpdate(sender) => sender.send(None),
            SystemApi::MOTD(rpc_result_sender) => {
                rpc_result_sender.send("".to_owned());
            }
            SystemApi::GetPreviousLogin(rpc_result_sender) => {
                rpc_result_sender
                    .send(get_previous_login(&config).map(|pl| format!("{:#x}", pl.root_address)));
            }
            SystemApi::GetWelcome(sender) => sender.send(WelcomeState {
                terms: !config.terms_accepted() && !preview.is_preview,
                new_profile: current_profile.deploy_held,
            }),
            // the welcome page awaits this before its save, which releases a held new profile
            SystemApi::AcceptTerms(sender) => {
                if !config.terms_accepted() {
                    config.accepted_terms = Some(TERMS_VERSION);
                    platform::write_config_file(&*config);
                }
                sender.send(());
            }
            // the account can only change while the world is held (the lobby): once in
            // world, comms and scenes are bound to the wallet that entered
            SystemApi::LoginPrevious(_, sender)
            | SystemApi::LoginWithIdentity(_, _, _, _, sender)
                if account_locked(&wallet, &world_hold) =>
            {
                sender.send(Err(ACCOUNT_LOCKED.to_owned()));
            }
            SystemApi::LoginNew(_, code_sender, result_sender)
                if account_locked(&wallet, &world_hold) =>
            {
                code_sender.send(Err(ACCOUNT_LOCKED.to_owned()));
                result_sender.send(Err(ACCOUNT_LOCKED.to_owned()));
            }
            SystemApi::LoginLocalGuest(sender) if account_locked(&wallet, &world_hold) => {
                sender.send(Err(ACCOUNT_LOCKED.to_owned()));
            }
            #[cfg(target_arch = "wasm32")]
            SystemApi::LoginLocalGuest(sender) => {
                sender.send(Err("the web shell keeps the guest's key".to_owned()));
            }
            // the guest belongs to the benchmark autopilot: any other launch signs in as an
            // account of its own
            #[cfg(not(target_arch = "wasm32"))]
            SystemApi::LoginLocalGuest(sender) if autopilot.is_none() => {
                sender.send(Err(LOCAL_GUEST_AUTOPILOT_ONLY.to_owned()));
            }
            SystemApi::LoginGuest | SystemApi::Logout if account_locked(&wallet, &world_hold) => {
                warn!("{ACCOUNT_LOCKED}");
            }
            SystemApi::LoginPrevious(default_on_error, rpc_result_sender) => {
                let ipfs = ipfas.ipfs().clone();
                let maybe_previous_login = get_previous_login(&config);
                *login_task = Some(IoTaskPool::get().spawn_compat(async move {
                    let Some(previous_login) = maybe_previous_login else {
                        rpc_result_sender.send(Err("No Previous Login Available".to_string()));
                        return Err(());
                    };

                    let PreviousLogin {
                        root_address,
                        ephemeral_key,
                        auth,
                        guest_account,
                    } = previous_login;

                    let profile =
                        match login_profile(root_address, ipfs, default_on_error, None).await {
                            Ok(profile) => profile,
                            Err(e) => {
                                rpc_result_sender.send(Err(e));
                                return Err(());
                            }
                        };

                    let local_wallet =
                        PrivateKeySigner::from_bytes(&B256::from_slice(&ephemeral_key)).unwrap();

                    Ok((
                        previous_login.root_address,
                        EphemeralKey::Local(local_wallet),
                        auth,
                        guest_account,
                        profile,
                        rpc_result_sender,
                    ))
                }));
            }
            SystemApi::LoginNew(default_on_error, code_sender, result_sender) => {
                let ipfs = ipfas.ipfs().clone();
                *login_task = Some(IoTaskPool::get().spawn_compat(async move {
                    let req = init_remote_ephemeral_request().await;
                    let req = match req {
                        Err(e) => {
                            code_sender.send(Err(e.to_string()));
                            result_sender.send(Err(e.to_string()));
                            return Err(());
                        }
                        Ok(res) => res,
                    };

                    // the deep-link flow has no verification code
                    code_sender.send(Ok(None));

                    let (root_address, local_wallet, auth, _) =
                        match finish_remote_ephemeral_request(req).await {
                            Ok(res) => res,
                            Err(e) => {
                                code_sender.send(Err(e.to_string()));
                                result_sender.send(Err(e.to_string()));
                                return Err(());
                            }
                        };

                    let profile =
                        match login_profile(root_address, ipfs, default_on_error, None).await {
                            Ok(profile) => profile,
                            Err(e) => {
                                result_sender.send(Err(e));
                                return Err(());
                            }
                        };

                    Ok((
                        root_address,
                        EphemeralKey::Local(local_wallet),
                        auth,
                        false,
                        profile,
                        result_sender,
                    ))
                }));
            }
            SystemApi::LoginWithIdentity(
                payload,
                default_on_error,
                guest_account,
                pick,
                rpc_result_sender,
            ) => {
                // The web page already holds a signed AuthIdentity (read from localStorage,
                // produced by whatever sign-in method the user used) — finalize the wallet
                // from it directly, no auth-server request/poll. Mirrors LoginPrevious.
                let ipfs = ipfas.ipfs().clone();
                *login_task = Some(IoTaskPool::get().spawn_compat(async move {
                    let (root_address, ephemeral_key, auth) = match parse_auth_identity(&payload) {
                        Ok(parts) => parts,
                        Err(e) => {
                            rpc_result_sender.send(Err(e));
                            return Err(());
                        }
                    };

                    let profile =
                        match login_profile(root_address, ipfs, default_on_error, pick).await {
                            Ok(profile) => profile,
                            Err(e) => {
                                rpc_result_sender.send(Err(e));
                                return Err(());
                            }
                        };

                    Ok((
                        root_address,
                        ephemeral_key,
                        auth,
                        guest_account,
                        profile,
                        rpc_result_sender,
                    ))
                }));
            }
            #[cfg(not(target_arch = "wasm32"))]
            SystemApi::LoginLocalGuest(rpc_result_sender) => {
                let ipfs = ipfas.ipfs().clone();
                // the HUD fails the run on this answer
                let (root, created) = match local_guest_root(&mut config) {
                    Ok(guest) => guest,
                    Err(e) => {
                        rpc_result_sender.send(Err(e.to_owned()));
                        continue;
                    }
                };
                if created {
                    platform::write_config_file(&*config);
                }
                let root_address = root.address();
                let now_millis = web_time::SystemTime::now()
                    .duration_since(web_time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as i64;
                *login_task = Some(IoTaskPool::get().spawn_compat(async move {
                    let (ephemeral, delegates) = match local_guest::sign_identity(&root, now_millis)
                    {
                        Ok(identity) => identity,
                        Err(e) => {
                            rpc_result_sender.send(Err(e.to_string()));
                            return Err(());
                        }
                    };
                    let profile = match login_profile(root_address, ipfs, false, None).await {
                        Ok(profile) => profile,
                        Err(e) => {
                            rpc_result_sender.send(Err(e));
                            return Err(());
                        }
                    };
                    Ok((
                        root_address,
                        EphemeralKey::Local(ephemeral),
                        delegates,
                        true,
                        profile,
                        rpc_result_sender,
                    ))
                }));
            }
            SystemApi::LoginGuest => {
                *login_task = None;
                login_events.pause();
                wallet.finalize_as_guest();
                segment_config.update_identity(format!("{:#x}", wallet.address().unwrap()), true);
                current_profile.profile = Some(UserProfile {
                    version: 0,
                    content: SerializedProfile {
                        eth_address: format!("{:#x}", wallet.address().unwrap()),
                        user_id: Some(format!("{:#x}", wallet.address().unwrap())),
                        ..Default::default()
                    },
                    base_url: ipfas.ipfs().contents_endpoint().unwrap_or_default(),
                });
                current_profile.is_deployed = true;
                current_profile.deploy_held = false;
                current_profile.revert_to = None;
            }
            SystemApi::LoginCancel => {
                *login_task = None;
            }
            SystemApi::Logout => {
                *login_task = None;
                login_events.pause();
                wallet.disconnect();
                segment_config.reset_identity();
                current_profile.profile = None;
                current_profile.deploy_held = false;
                current_profile.revert_to = None;
            }
            _ => (),
        }
    }

    if let Some(mut task) = login_task.take() {
        match task.complete() {
            // started in the lobby over another account, finished after the world was entered
            Some(Ok((.., sender))) if account_locked(&wallet, &world_hold) => {
                sender.send(Err(ACCOUNT_LOCKED.to_owned()));
            }
            Some(Ok((root_address, ephemeral_key, auth, guest_account, profile, sender))) => {
                if let Ok(mut window) = window.single_mut() {
                    window.focused = true;
                }

                if remember_login(
                    &mut config,
                    autopilot.is_some(),
                    root_address,
                    &ephemeral_key,
                    &auth,
                    guest_account,
                ) {
                    platform::write_config_file(&*config);
                }
                match ephemeral_key {
                    EphemeralKey::Local(local_wallet) => {
                        wallet.finalize(root_address, local_wallet, auth, guest_account);
                    }
                    EphemeralKey::Remote(ephemeral_address) => {
                        wallet.finalize_remote(
                            root_address,
                            ephemeral_address,
                            auth,
                            guest_account,
                        );
                    }
                }
                login_events.completed(&wallet);
                segment_config
                    .update_identity(format!("{:#x}", wallet.address().unwrap()), guest_account);
                let (profile, deployed) = match profile {
                    LoginProfile::Deployed(profile) => (profile, true),
                    LoginProfile::New(profile) => (profile, false),
                };
                current_profile.profile = Some(profile);
                current_profile.is_deployed = deployed;
                // held, the host shows its welcome page, which deploys the new profile
                current_profile.deploy_held = !deployed && world_hold.is_some();
                current_profile.revert_to = None;

                sender.send(Ok(()));
            }
            Some(Err(())) => (),
            None => *login_task = Some(task),
        }
    }
    login_events.drive(&wallet);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_names_are_valid_unclaimed_names() {
        for first in GUEST_FIRST_NAMES {
            for last in GUEST_LAST_NAMES {
                let name = format!("{first}{last}");
                assert!(
                    name.len() <= 15 && name.chars().all(|c| c.is_ascii_alphanumeric()),
                    "{name}"
                );
            }
        }
    }

    fn stored(root: &PrivateKeySigner, signed_at_millis: i64) -> PreviousLogin {
        let (ephemeral, auth) = local_guest::sign_identity(root, signed_at_millis).unwrap();
        PreviousLogin {
            root_address: root.address(),
            ephemeral_key: ephemeral.to_bytes().to_vec(),
            auth,
            guest_account: true,
        }
    }

    // The autopilot's guest is one account for good: an identity that has expired is signed again
    // with the root key the config keeps, which is what lets the run sign in without a screen.
    #[test]
    fn the_local_guest_is_the_same_account_after_its_identity_expires() {
        let now = chrono::Utc::now().timestamp_millis();
        let day = 24 * 3600 * 1000;
        let mut config = AppConfig::default();

        let (root, created) = local_guest_root(&mut config).unwrap();
        assert!(created);
        let (again, created_again) = local_guest_root(&mut config).unwrap();
        assert!(!created_again);
        assert_eq!(again.address(), root.address());

        config.previous_login = Some(stored(&root, now));
        assert_eq!(
            get_previous_login(&config).map(|login| login.root_address),
            Some(root.address())
        );

        // 31 days on, the stored identity is no longer usable ...
        config.previous_login = Some(stored(&root, now - 31 * day));
        assert!(get_previous_login(&config).is_none());

        // ... and signing again as the same account makes a usable one
        let (same_root, created) = local_guest_root(&mut config).unwrap();
        assert!(!created);
        assert_eq!(same_root.address(), root.address());
        config.previous_login = Some(stored(&same_root, now));
        assert_eq!(
            get_previous_login(&config).map(|login| login.root_address),
            Some(root.address())
        );
    }

    /// A config that holds a person's stored account and the local guest's root key.
    fn config_with_an_account_and_a_guest_key() -> (AppConfig, Address) {
        let account = PrivateKeySigner::random();
        let now = chrono::Utc::now().timestamp_millis();
        let mut stored = stored(&account, now);
        stored.guest_account = false;
        let config = AppConfig {
            previous_login: Some(stored),
            guest_root_key: Some(local_guest::new_root_key().to_bytes().to_vec()),
            ..Default::default()
        };
        (config, account.address())
    }

    fn a_guest_identity() -> (Address, EphemeralKey, Vec<ChainLink>) {
        let root = local_guest::new_root_key();
        let (ephemeral, auth) = local_guest::sign_identity(&root, 0).unwrap();
        (root.address(), EphemeralKey::Local(ephemeral), auth)
    }

    #[test]
    fn a_benchmark_run_leaves_the_stored_account_and_the_guest_key_as_they_are() {
        let (mut config, account) = config_with_an_account_and_a_guest_key();
        let guest_key = config.guest_root_key.clone();
        let (guest, ephemeral, auth) = a_guest_identity();

        // the guest signs in with a new identity: nothing is stored, nothing is dropped
        assert!(!remember_login(
            &mut config,
            true,
            guest,
            &ephemeral,
            &auth,
            true
        ));
        // so does a key the page holds
        assert!(!remember_login(
            &mut config,
            true,
            guest,
            &EphemeralKey::Remote(Address::ZERO),
            &auth,
            true
        ));
        assert_eq!(
            config
                .previous_login
                .as_ref()
                .map(|login| login.root_address),
            Some(account)
        );
        assert!(!config.previous_login.as_ref().unwrap().guest_account);
        assert_eq!(config.guest_root_key, guest_key);
    }

    #[test]
    fn outside_the_autopilot_a_login_is_stored_and_a_real_account_keeps_the_guests_key() {
        let (mut config, _) = config_with_an_account_and_a_guest_key();
        let guest_key = config.guest_root_key.clone();
        let (new_account, ephemeral, auth) = a_guest_identity();

        // an account signing in replaces the stored login only; the benchmark guest is separate
        assert!(remember_login(
            &mut config,
            false,
            new_account,
            &ephemeral,
            &auth,
            false
        ));
        let stored = config.previous_login.as_ref().unwrap();
        assert_eq!(stored.root_address, new_account);
        assert!(!stored.guest_account);
        assert_eq!(stored.auth.len(), auth.len());
        assert_eq!(
            config.guest_root_key, guest_key,
            "the guest keeps its address"
        );

        // a key the page holds is stored nowhere, and the earlier login goes
        assert!(remember_login(
            &mut config,
            false,
            new_account,
            &EphemeralKey::Remote(Address::ZERO),
            &auth,
            false
        ));
        assert!(config.previous_login.is_none());
        assert!(!remember_login(
            &mut config,
            false,
            new_account,
            &EphemeralKey::Remote(Address::ZERO),
            &auth,
            false
        ));
        assert_eq!(config.guest_root_key, guest_key);
    }

    // The web shell fails the run on a kept key that is not a key; so does the desktop, rather
    // than becoming a new account whose address differs from every earlier run's.
    #[test]
    fn a_root_key_the_config_cannot_read_is_an_error_and_stays_as_it_is() {
        let mut config = AppConfig {
            guest_root_key: Some(vec![1, 2, 3]),
            ..Default::default()
        };
        assert_eq!(
            local_guest_root(&mut config).err(),
            Some(LOCAL_GUEST_KEY_UNREADABLE)
        );
        assert_eq!(config.guest_root_key, Some(vec![1, 2, 3]));
    }

    #[test]
    fn guest_look_is_a_default_profile_pointer() {
        for address in [
            Address::ZERO,
            Address::repeat_byte(0xff),
            Address::repeat_byte(0x7a),
        ] {
            let index = guest_look_index(&address);
            assert!((1..=DEFAULT_LOOKS).contains(&index), "{index}");
        }
    }
}
