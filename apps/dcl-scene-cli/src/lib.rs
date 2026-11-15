pub mod abgen_embed;
pub mod asset_bundles;
pub mod build;
pub mod build_script;
pub mod check_server;
pub mod check_stamp;
pub mod code_to_composite;
pub mod comms;
pub mod context_files;
pub mod crdt_gen;
pub mod data_layer;
pub mod deep_link;
pub mod deploy;
pub mod entity_names;
pub mod entrypoint;
pub mod esbuild;
pub mod exec_file;
pub mod export_static;
pub mod host;
pub mod init;
pub mod joinblock;
pub mod linker;
pub mod live_reload;
pub mod livekit;
pub mod livekit_embed;
pub mod livekit_server;
pub mod netinfo;
pub mod pack;
pub mod play_embed;
pub mod prebuilt;
pub mod project_sync;
#[cfg(feature = "rolldown")]
pub mod rolldown_backend;
pub mod scene;
pub mod scene_files;
pub mod sdk_cache;
pub mod shutdown;
pub mod signed_fetch;
pub mod skills;
pub mod split;
pub mod start;
pub mod storage;
pub mod storage_cli;
pub mod storage_remote;
pub mod tunnel;
pub mod typescript;
pub mod ux;
pub mod watch;
pub mod workspace;
pub mod world;
pub mod world_access;
pub mod world_linker;

/// A test's environment write.
#[cfg(test)]
pub(crate) fn set_test_env(key: impl AsRef<std::ffi::OsStr>, value: impl AsRef<std::ffi::OsStr>) {
    // SAFETY: test-only. std orders the write against every `std::env` read;
    // a libc `getenv` on another test thread (DNS resolution, SQLite's temp
    // directory) can still race it, which these tests accept.
    unsafe { std::env::set_var(key, value) }
}

/// A test's environment removal; see [`set_test_env`].
#[cfg(test)]
pub(crate) fn remove_test_env(key: impl AsRef<std::ffi::OsStr>) {
    // SAFETY: as in `set_test_env`.
    unsafe { std::env::remove_var(key) }
}

/// The integration tests' condition wait, shared with the unit tests.
#[cfg(test)]
#[path = "../tests/common/wait.rs"]
pub(crate) mod test_wait;

#[cfg(test)]
pub(crate) fn random_test_wallet() -> dcl_auth::Wallet {
    loop {
        let bytes: [u8; 32] = rand::random();
        if let Ok(w) = dcl_auth::Wallet::from_hex(&hex::encode(bytes)) {
            return w;
        }
    }
}
