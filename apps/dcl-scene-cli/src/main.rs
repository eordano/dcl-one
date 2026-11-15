use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use dcl_scene_cli::{
    build, build_script, code_to_composite, context_files, deploy, export_static, init, joinblock,
    pack, scene, start, ux, watch, workspace, world, world_access,
};
use std::path::PathBuf;

/// Below this the Explorer ignores the deep link's `mcp-port` and serves on its
/// own default, leaving the scene-log poller waiting on a port nobody opened.
const MIN_MCP_PORT: u16 = 1024;

const CI_HELP: &str = "Non-interactive (also when CI is set in the environment): no colours, spinners, prompts, QR codes or browser windows";

/// The flag, or `CI` in the environment: either turns the whole run
/// non-interactive, so every later print and prompt sees one answer.
/// `--position X,Y`.
fn parse_position(raw: &str) -> Result<(i64, i64), String> {
    let bad = || format!("expected X,Y (two integers), got {raw:?}");
    let (x, y) = raw.split_once(',').ok_or_else(bad)?;
    let num = |s: &str| s.trim().parse::<i64>().map_err(|_| bad());
    Ok((num(x)?, num(y)?))
}

fn ci_mode(flag: bool) -> bool {
    if flag {
        ux::set_ci(true);
    }
    ux::ci()
}

fn resolved_mcp_port(mcp_port: Option<u16>) -> u16 {
    mcp_port.unwrap_or(joinblock::DEFAULT_EXPLORER_MCP_PORT)
}

#[derive(Parser)]
#[command(
    name = "dcl-scene-cli",
    version,
    about = "Binary-compatible Rust replacement for @dcl/sdk-commands (build, start, deploy)"
)]
struct Cli {
    #[arg(
        long,
        global = true,
        help = "Show detailed logs and full error chains (RUST_LOG also enables this)"
    )]
    verbose: bool,
    #[arg(
        long,
        global = true,
        help = "Print the command's result as JSON on stdout and nothing else there (export-static: scenesUrn, destination, entities, entityFiles, contentFiles, aboutFile; deploy: entityId, message; init --template/--github-repo: dir, missingDependencies, missingPackages; init --node-modules-only: dir, dryRun, installed, kept, stoppedExtraction). start prints one JSON event per line instead (ready, client-failed, rebuild, build-error, error) and sends its text to stderr; a deploy that fails prints the error event too; the other commands have no result document"
    )]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
// Parsed once, then destructured: the size of `Start` costs nothing.
#[allow(clippy::large_enum_variant)]
enum Command {
    #[command(
        about = "Scaffold a new scene, smart wearable, portable experience or library project"
    )]
    Init {
        #[arg(long, default_value = ".", help = "Folder to scaffold into")]
        dir: PathBuf,
        #[arg(
            long,
            value_enum,
            conflicts_with_all = ["template", "github_repo"],
            help = "What to scaffold (default: scene)"
        )]
        project: Option<init::ProjectKind>,
        #[arg(short = 'y', long, help = "Scaffold even if the folder is not empty")]
        yes: bool,
        #[arg(
            long,
            help = "Only install the vendored node_modules into an existing project; scaffold nothing"
        )]
        node_modules_only: bool,
        #[arg(
            long,
            requires = "node_modules_only",
            conflicts_with_all = ["template", "github_repo"],
            help = "With --node-modules-only: install only the packages that node_modules does not have; a package that is there stays as it is, whatever its version"
        )]
        missing_only: bool,
        #[arg(
            long,
            requires = "missing_only",
            help = "With --missing-only: write nothing; report the packages that node_modules does not have and the versions of the packages that it has"
        )]
        dry_run: bool,
        #[arg(
            long,
            value_name = "ZIP",
            conflicts_with = "github_repo",
            help = "Scaffold from a zip instead, by http(s) URL or local path: one top-level folder whose contents become the project"
        )]
        template: Option<String>,
        #[arg(
            long,
            value_name = "URL",
            help = "Scaffold from a GitHub repository: https://github.com/<owner>/<repo>, or /tree/<branch>/<path> for one folder of it"
        )]
        github_repo: Option<String>,
        #[arg(long, help = "Do not install the vendored node_modules")]
        skip_install: bool,
    },
    #[command(
        about = "Install the bundled migrate-smart-items-to-code skill into .claude/skills/ and download the official SDK7 AI context files into dclcontext/"
    )]
    GetContextFiles {
        #[arg(long, default_value = ".")]
        dir: PathBuf,
        #[arg(
            long,
            help = "Only write the bundled skill; skip the GitHub ai-sdk-context download"
        )]
        offline: bool,
    },
    #[command(
        about = "Run package.json scripts.build, or type-check and bundle with the SDK",
        long_about = "Run package.json scripts.build from the project directory via the system shell, with node_modules/.bin on PATH and inherited output (under --json the script's stdout goes to stderr, so stdout stays the result document). The script owns type checking, bundling and assets, must exit zero and produce a nonempty scene.json main bundle. No npm lifecycle hooks or SDK fallback. DCL_SCENE_CLI_PRODUCTION=1 for production (0 otherwise); DCL_SCENE_CLI_MAIN names the output. Build, start/watch, deploy and pack share this contract. Use --built-in for an explicit SDK build, including inside a build script. The legacy script dcl-scene-cli build is a built-in alias. See README: Custom build scripts."
    )]
    Build {
        #[arg(
            long,
            help = "Ignore scripts.build and use the SDK type-check/bundle pipeline"
        )]
        built_in: bool,
        #[arg(long, default_value = ".", help = "Project folder to build")]
        dir: PathBuf,
        #[arg(
            short = 'p',
            long,
            help = "Minify and drop dev-only checks, as deploy does"
        )]
        production: bool,
        #[arg(short = 'w', long, help = "Rebuild on every change instead of exiting")]
        watch: bool,
        #[arg(
            long = "ignoreComposite",
            visible_alias = "ignore-composite",
            help = "Leave main.crdt alone instead of regenerating it from composites"
        )]
        ignore_composite: bool,
        #[arg(
            long = "customEntryPoint",
            visible_alias = "custom-entry-point",
            help = "Bundle scene.json's main verbatim instead of generating the loader stub"
        )]
        custom_entry_point: bool,
        #[arg(
            long,
            help = "Do not restore missing node_modules from the vendored SDK"
        )]
        skip_install: bool,
        #[arg(
            long,
            help = "Bundle without type checking (the bundle is written either way)"
        )]
        skip_type_check: bool,
        #[arg(
            long,
            value_name = "GLOB",
            conflicts_with_all = ["watch", "custom_entry_point"],
            help = "Bundle each matching .ts/.tsx under the project to a sibling .js instead of the scene (library files; the SDK stays a require, as in the scene chunk)"
        )]
        single: Option<String>,
        #[arg(
            long = "emitDeclaration",
            visible_alias = "emit-declaration",
            help = "Have the type check also write .d.ts files (tsc --emitDeclarationOnly, into tsconfig's declarationDir or outDir)"
        )]
        emit_declaration: bool,
    },
    #[command(about = "Build the scene and serve a live preview with hot reload")]
    Start {
        #[arg(long, default_value = ".", help = "Project folder to preview")]
        dir: PathBuf,
        #[arg(
            short = 'p',
            long,
            help = "Port to serve on (0: any free port); without it, 8000 or the next free port"
        )]
        port: Option<u16>,
        #[arg(long, help = "Serve the existing bin/ as-is instead of building first")]
        skip_build: bool,
        #[arg(
            long,
            help = "Do not type check; checking runs beside the watch loop and never delays a reload"
        )]
        skip_type_check: bool,
        #[arg(
            long,
            help = "Do not restore missing node_modules from the vendored SDK"
        )]
        skip_install: bool,
        #[arg(short = 'w', long, help = "Serve once and stop watching for changes")]
        no_watch: bool,
        #[arg(
            long,
            help = "Do not announce each preview rebuild that runs package.json scripts.build; its rebuilt line still prints"
        )]
        no_build_note: bool,
        #[arg(
            short = 'b',
            long,
            help = "Accepted for compatibility; this CLI opens a browser only for --talk, and this keeps it from doing so"
        )]
        no_browser: bool,
        #[arg(long, help = CI_HELP)]
        ci: bool,
        #[arg(
            long,
            help = "Expose the Creator Hub data layer so the inspector can edit this scene live"
        )]
        data_layer: bool,
        #[arg(
            long = "ignoreComposite",
            visible_alias = "ignore-composite",
            help = "Leave main.crdt alone instead of regenerating it from composites"
        )]
        ignore_composite: bool,
        #[arg(
            long,
            help = "Serve comms locally so the preview needs no comms service"
        )]
        offline_comms: bool,
        #[arg(
            long,
            value_name = "WS_URL",
            help = "Put comms, and with them voice, on a LiveKit server instead of the built-in ws-room: the SFU's ws:// or wss:// address as the explorer reaches it [env: LIVEKIT_URL]"
        )]
        livekit_url: Option<String>,
        #[arg(
            long,
            value_name = "KEY",
            help = "The LiveKit API key the preview mints room tokens with [env: LIVEKIT_API_KEY]"
        )]
        livekit_api_key: Option<String>,
        #[arg(
            long,
            value_name = "SECRET",
            help = "The LiveKit API secret; prefer --livekit-api-secret-file or LIVEKIT_API_SECRET, an argument shows in ps"
        )]
        livekit_api_secret: Option<String>,
        #[arg(
            long,
            value_name = "FILE",
            help = "Read the LiveKit API secret from this file"
        )]
        livekit_api_secret_file: Option<PathBuf>,
        #[arg(
            long,
            value_name = "NAME",
            default_value = dcl_scene_cli::livekit::DEFAULT_ROOM,
            help = "The LiveKit room of the realm; scene rooms are scene:<NAME>:<sceneId>. Rename it to keep two previews on one server apart"
        )]
        livekit_room: String,
        #[arg(
            long,
            conflicts_with = "livekit_url",
            help = "Do not run the built-in livekit-server: comms stay on the built-in ws-room (positions, chat, scene messages; no voice). Voice is on by default \u{2014} every dcl-scene-cli binary embeds livekit-server and runs it for the preview (LIVEKIT_SERVER_BIN runs a different one)"
        )]
        no_livekit: bool,
        #[arg(
            long = "multi-instance",
            help = "Add multi-instance=true to the desktop deep link. With this flag or --skip-auth-screen, the link has only the parameters of the given flags; with neither, it has both"
        )]
        multi_instance: bool,
        #[arg(long = "no-client", hide = true)]
        no_client: bool,
        #[arg(
            short = 'm',
            long,
            help = "Show a QR code and LAN URL for opening the preview on a phone"
        )]
        mobile: bool,
        #[arg(
            long,
            help = "Launch the desktop Explorer on the printed deep link (install: https://dcl.gg/explorer); --no-client or --ci keep it from launching"
        )]
        explorer_alpha: bool,
        #[arg(
            long = "web",
            visible_alias = "bevy-web",
            help = "Open the hosted web explorer on this preview in a browser; --no-browser, --no-client or --ci keep it closed"
        )]
        web: bool,
        #[arg(
            long,
            value_name = "org|zone",
            default_value = "org",
            value_parser = ["org", "zone"],
            help = "Decentraland environment: the deep link's dclenv and the hosted web explorer's domain"
        )]
        dclenv: String,
        #[arg(
            long,
            value_name = "URL",
            help = "Realm the printed desktop deep links name instead of this preview"
        )]
        realm: Option<String>,
        #[arg(
            long,
            value_name = "X,Y",
            value_parser = parse_position,
            help = "Parcel the join links land on instead of scene.json's base"
        )]
        position: Option<(i64, i64)>,
        #[arg(long, help = "Add hub=true to the desktop deep link")]
        hub: bool,
        #[arg(
            short = 'n',
            long = "open-deeplink-in-new-instance",
            help = "Add open-deeplink-in-new-instance=true to the desktop deep link"
        )]
        new_instance: bool,
        #[arg(
            long,
            help = "Add landscape-terrain-enabled=true to the desktop deep link"
        )]
        landscape_terrain_enabled: bool,
        #[arg(
            long,
            help = "Add skip-auth-screen=true to the desktop deep link. With this flag or --multi-instance, the link has only the parameters of the given flags; with neither, it has both"
        )]
        skip_auth_screen: bool,
        #[arg(long = "local-scene", hide = true)]
        local_scene: bool,
        #[arg(long, hide = true)]
        debug: bool,
        #[arg(short = 'd', long = "no-debug", hide = true)]
        no_debug: bool,
        #[arg(long, hide = true)]
        web3: bool,
        #[arg(
            long,
            help = "Do not run the abgen asset-bundle sidecar, and stop forwarding local-ab=true with it. The sidecar is on by default and needs no install \u{2014} every dcl-scene-cli binary embeds abgen (ABGEN_BIN runs a different one). Upstream sdk-commands has no sidecar, so this is how to get its unoptimized preview"
        )]
        no_asset_bundles: bool,
        #[arg(
            long = "asset-bundles",
            conflicts_with = "no_asset_bundles",
            help = "Accepted for upstream CLI parity and does nothing: forwarding local-ab=true is now the default"
        )]
        asset_bundles: bool,
        #[arg(
            long,
            help = "Do not enable the Explorer's MCP server, and stop reading the running scene's errors out of it. MCP is on by default: the deep link carries mcp=true and mcp-port, and a scene that throws prints here instead of only in the client's log"
        )]
        no_mcp: bool,
        #[arg(
            long,
            help = "Let a machine other than this one press Deploy on the preview's /deploy page. Off by default: publishing signs with this machine's wallet, and the preview port is unauthenticated"
        )]
        allow_remote_deploy: bool,
        #[arg(
            long,
            conflicts_with = "no_mcp",
            help = "Accepted for parity with earlier releases and does nothing: enabling the Explorer's MCP server is now the default"
        )]
        mcp: bool,
        #[arg(
            long = "error-source-lines-context",
            value_name = "N",
            help = "Extra source lines either side of a scene error's line (default 0 \u{2014} just the line itself); --error-source-lines-before/-after override one side"
        )]
        error_lines_context: Option<u32>,
        #[arg(
            long = "error-source-lines-before",
            value_name = "N",
            help = "Source lines to show BEFORE a scene error's line"
        )]
        error_source_lines_before: Option<u32>,
        #[arg(
            long = "error-source-lines-after",
            value_name = "N",
            help = "Source lines to show AFTER a scene error's line"
        )]
        error_source_lines_after: Option<u32>,
        #[arg(
            long = "mcp-port",
            value_name = "PORT",
            conflicts_with = "no_mcp",
            value_parser = clap::value_parser!(u16).range(i64::from(MIN_MCP_PORT)..=i64::from(u16::MAX)),
            help = "Port for the Explorer's MCP server (1024-65535), forwarded into the desktop deep link. Defaults to the Explorer's own default port, so both ends agree without being told"
        )]
        mcp_port: Option<u16>,
        #[arg(
            last = true,
            value_name = "EXPLORER_PARAMS",
            help = "Everything after a standalone -- is forwarded verbatim into the desktop Explorer deep link as query params: --key=value, --key value, and bare --key (becomes key=true)"
        )]
        explorer_params: Vec<String>,
        #[arg(
            long,
            value_name = "URL",
            env = "DCL_SCENE_CLI_PUBLIC_URL",
            help = "The address people open when a reverse proxy or a custom domain serves this preview (https://preview.example.com, a path prefix allowed): its join links are printed, and a request for that host is answered as that origin even when the proxy forwards no scheme"
        )]
        public_url: Option<String>,
        #[arg(
            long,
            value_name = "WSS_URL|help",
            help = "Expose this preview publicly through a tunnel service; pass 'help' for setup"
        )]
        tunnel: Option<String>,
        #[arg(
            long,
            help = "Auth token for the --tunnel service; prefer --tunnel-token-file or DCL_SCENE_CLI_TUNNEL_TOKEN (a flag value is visible in ps and shell history)"
        )]
        tunnel_token: Option<String>,
        #[arg(
            long,
            value_name = "PATH",
            help = "Read the --tunnel auth token from a file (wins over DCL_SCENE_CLI_TUNNEL_TOKEN; --tunnel-token wins over both)"
        )]
        tunnel_token_file: Option<PathBuf>,
        #[arg(
            long,
            visible_alias = "no-server",
            help = "Do not attach the authoritative-server isolate a scene.json authoritativeMultiplayer flag would auto-start (--no-server is upstream's spelling)"
        )]
        no_host: bool,
        #[arg(
            long,
            help = "Hands-free: open the web explorer with the Scene assistant listening, and let that explorer's origin reach this preview's /api/project without DCL_SCENE_CLI_ALLOWED_ORIGINS"
        )]
        talk: bool,
        #[arg(
            long,
            value_name = "URL",
            requires = "talk",
            help = "The web explorer --talk opens; DCL_SCENE_CLI_WEB_EXPLORER otherwise, and the build's site player (<DCL_SCENE_CLI_SITE>/play) when neither is set"
        )]
        player: Option<String>,
    },
    #[command(
        about = "Run the scene's authoritative-server isolate against a running preview's room"
    )]
    Host {
        #[arg(long, default_value = ".", help = "Project folder to host")]
        dir: PathBuf,
        #[arg(
            long,
            default_value = "http://127.0.0.1:8000",
            help = "The preview server whose mini-comms room this host joins"
        )]
        preview: String,
        #[arg(long, default_value = "room-1", help = "Room id on the preview")]
        room: String,
    },
    #[command(about = "Sign and publish the scene to a catalyst or worlds content server")]
    Deploy {
        #[arg(long, default_value = ".", help = "Project folder to deploy")]
        dir: PathBuf,
        #[arg(
            short = 't',
            long = "target-server",
            alias = "target",
            help = "Server to publish to, for this run only: a catalyst domain (its /about names the content server) or a content-server URL with its scheme, used verbatim; a scene that names a world sends it to a worlds server, used verbatim too (a worlds server answers /status, not /about). Default, from the scene: a worldConfiguration.name goes to https://worlds-content-server.decentraland.org, parcels to a healthy catalyst of the public Genesis rotation. DCL_SCENE_CLI_TARGET_SERVER is the same setting from the environment"
        )]
        target: Option<String>,
        #[arg(
            long,
            hide = true,
            help = "Alias of --target-server for a content-server URL"
        )]
        target_content: Option<String>,
        #[arg(
            long,
            help = "Sign headlessly with this private-key file instead of opening a wallet (env: DCL_PRIVATE_KEY; this flag wins)"
        )]
        sign_key: Option<PathBuf>,
        #[arg(long, help = "Publish the existing bin/ as-is instead of rebuilding")]
        skip_build: bool,
        #[arg(
            long,
            help = "Skip the local checks before signing (scene.json media permissions, the 50 MB per-file limit); implied by --target-server, whose limits this CLI does not know"
        )]
        skip_validations: bool,
        #[arg(
            long,
            help = "Accepted for upstream parity: this CLI runs no SDK version check before publishing"
        )]
        skip_version_checks: bool,
        #[arg(
            long,
            help = "Accepted for upstream parity: the signing page is served on loopback, which browsers already treat as a secure context"
        )]
        https: bool,
        #[arg(
            long,
            help = "Upload every file, even those the server already stores (DCL_SCENE_CLI_UPLOAD_ALL=1 does the same)"
        )]
        force_upload: bool,
        #[arg(
            long,
            conflicts_with = "replace_world_scenes",
            help = "Deploy alongside the world's other scenes: the default (see --replace-world-scenes)"
        )]
        multi_scene: bool,
        #[arg(
            long,
            help = "Pack and hash the entity, print what would be published, and touch no network"
        )]
        dry_run: bool,
        #[arg(
            long,
            help = "Pin the entity timestamp (unix ms) so the same input yields the same entity id"
        )]
        timestamp: Option<i64>,
        #[arg(long, help = "Also write the entity JSON to this path")]
        entity_out: Option<PathBuf>,
        #[arg(
            long,
            help = "Replace ALL scenes in the world with this one (default: additive — deploy alongside, keep the others). Destructive, and needs a pre-flight check that a Cloudflare-fronted worlds server challenges"
        )]
        replace_world_scenes: bool,
        #[arg(
            long,
            help = "Ask the worlds server whether the signing wallet may publish before uploading, so a refusal names the owner and the grant command. Off by default: that pre-flight GET gets the upload challenged behind a Cloudflare-fronted worlds server, and the server refuses an unpermitted upload itself"
        )]
        check_permissions: bool,
        #[arg(
            short = 'y',
            long,
            help = "Answer prompts yes, including consent to publish to the public network"
        )]
        yes: bool,
        #[arg(short = 'b', long, help = "Do not open a browser for wallet signing")]
        no_browser: bool,
        #[arg(
            long,
            help = "Non-interactive (also when CI is set): no colours or prompts, never opens a browser, refuses a public deploy unless --yes is given"
        )]
        ci: bool,
        #[arg(
            short = 'p',
            long,
            help = "Port for the local signing page (default: loopback, ephemeral)"
        )]
        port: Option<u16>,
        #[arg(
            long,
            value_name = "ADDRESS",
            help = "Address the local signing server listens on (default: 127.0.0.1, or DCL_SCENE_CLI_LINKER_HOST when it is set); 0.0.0.0 opens the port to the network; one other address that is not loopback needs remote signing (DCL_SCENE_CLI_LINKER_HOST)"
        )]
        host: Option<std::net::IpAddr>,
        #[arg(
            long,
            value_name = "CODE",
            help = "Accepted for upstream parity: the text of this CLI and of its signing page is English only"
        )]
        language: Option<String>,
        #[arg(
            long,
            help = "Machine-driven run: no browser, no narration; stdout carries one JSON line when the signing server is up (its URL, the /api/deploy resource, the deploy token, the entity id to sign) and one with the final deployment state"
        )]
        programmatic: bool,
        #[arg(
            long,
            value_name = "TOKEN",
            help = "Token the X-Deploy-Token header must carry to cancel through DELETE /api/deploy (at least 16 characters); random per run by default"
        )]
        deploy_token: Option<String>,
    },
    #[command(
        about = "Remove a LAND scene published to a dcl-one-style content server (signed request)"
    )]
    Unpublish {
        #[arg(long, value_name = "X,Y")]
        parcel: String,
        #[arg(
            short = 't',
            long = "target-server",
            alias = "target",
            help = "Catalyst domain or content-server URL to act on; DCL_SCENE_CLI_TARGET_SERVER from the environment otherwise"
        )]
        target: Option<String>,
        #[arg(
            long,
            hide = true,
            help = "Alias of --target-server for a content-server URL"
        )]
        target_content: Option<String>,
        #[arg(
            long,
            help = "Sign with this private-key file instead of a wallet (env: DCL_PRIVATE_KEY; this flag wins)"
        )]
        sign_key: Option<PathBuf>,
    },
    #[command(
        alias = "pack-smart-wearable",
        about = "Build and zip a smart wearable for upload to the builder"
    )]
    Pack {
        #[arg(long, default_value = ".", help = "Smart-wearable folder to pack")]
        dir: PathBuf,
        #[arg(long, help = "Zip the existing bin/ as-is instead of rebuilding")]
        skip_build: bool,
    },
    #[command(
        about = "Run the scene code once and keep what it creates as assets/scene/main.composite + main.crdt, commenting the code out"
    )]
    CodeToComposite {
        #[arg(
            long,
            default_value = ".",
            help = "Project or workspace folder to convert"
        )]
        dir: PathBuf,
        #[arg(short = 'y', long, help = "Convert without the confirmation prompt")]
        yes: bool,
    },
    #[command(
        about = "Export the scene as a content server stores it: hash-named files, the entity file and, with --realmName, a realm about"
    )]
    ExportStatic {
        #[arg(
            long,
            default_value = ".",
            help = "Project or workspace folder to export"
        )]
        dir: PathBuf,
        #[arg(
            long,
            default_value = ".",
            value_name = "DIR",
            help = "Folder the hash-named files and the entity file are written to; not the project folder or one above it"
        )]
        destination: PathBuf,
        #[arg(
            long,
            value_name = "MS|RFC3339",
            value_parser = export_static::parse_timestamp,
            help = "Entity timestamp instead of now: unix milliseconds or an RFC 3339 date"
        )]
        timestamp: Option<i64>,
        #[arg(
            long = "realmName",
            visible_alias = "realm-name",
            value_name = "NAME",
            requires = "base_url",
            help = "Also write <destination>/<NAME>/about, exposing the export as a realm (needs --baseUrl)"
        )]
        realm_name: Option<String>,
        #[arg(
            long = "commsAdapter",
            visible_alias = "comms-adapter",
            value_name = "URL",
            help = "The realm's fixed comms adapter (ADR-180); default offline:offline"
        )]
        comms_adapter: Option<String>,
        #[arg(
            long = "baseUrl",
            visible_alias = "base-url",
            value_name = "URL",
            help = "Public URL --destination is served from, appended to every scene URN"
        )]
        base_url: Option<String>,
        #[arg(long, help = "Export the existing bin/ as-is instead of rebuilding")]
        skip_build: bool,
    },
    #[command(about = "Manage a world's settings and permissions on a worlds content server")]
    World {
        #[command(subcommand)]
        command: WorldCommand,
    },
    #[command(
        about = "Read and write the scene's server-side storage (scene, player and env values), locally or on a storage service"
    )]
    Storage {
        #[command(subcommand)]
        command: dcl_scene_cli::storage_cli::StorageCommand,
    },
    #[command(
        about = "Mirror this folder with a project on a node, both ways, by content hash; stops on a path both sides changed"
    )]
    Sync {
        #[command(flatten)]
        opts: dcl_scene_cli::project_sync::SyncOptions,
    },
    /// Generate main.crdt from a scene's composites into an arbitrary file, so
    /// the native generator's bytes can be diffed against a node data-layer dump.
    #[command(hide = true)]
    CrdtGen {
        #[arg(long, default_value = ".")]
        dir: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Build the prebuilt SDK runtime chunks for the vendored blob: a step of
    /// `scripts/build-base-blob.py`, so `--dir` must be a throwaway scene whose
    /// `node_modules` is the full install tree (including `@dcl/asset-packs`
    /// and `@dcl/sdk-commands`, which the blob itself does not ship).
    #[command(hide = true)]
    VendorChunks {
        #[arg(long, default_value = ".")]
        dir: PathBuf,
        #[arg(long)]
        out_core: PathBuf,
        #[arg(long)]
        out_smart: PathBuf,
    },
}

#[derive(Subcommand)]
enum WorldCommand {
    #[command(about = "Remove one reviewed World scene; keep all other scenes")]
    RemoveScene {
        name: String,
        coordinate: String,
        #[arg(
            long = "entity",
            help = "Expected deployed entity ID; removal is refused if it changed"
        )]
        entity_id: String,
        #[command(flatten)]
        signed: SignedWriteArgs,
    },
    #[command(about = "Get or set world metadata (title, spawn, skybox, categories, ...)")]
    Settings {
        #[command(subcommand)]
        command: WorldSettingsCommand,
    },
    #[command(about = "List, grant, or revoke world access permissions")]
    Permissions {
        #[command(subcommand)]
        command: WorldPermissionsCommand,
    },
    #[command(
        about = "Who can enter the world: anyone, a password, or a list of people and communities"
    )]
    Access {
        #[command(subcommand)]
        command: WorldAccessCommand,
    },
}

#[derive(Subcommand)]
enum WorldAccessCommand {
    #[command(about = "Print who can enter, with avatar names and community names resolved")]
    Show {
        name: String,
        #[arg(
            long = "target-server",
            alias = "target-content",
            help = "Worlds server to read from; DCL_SCENE_CLI_TARGET_SERVER from the environment otherwise"
        )]
        target_content: Option<String>,
    },
    #[command(
        about = "Replace the rule (signed request): --public, --password, or a list of --user/--community",
        after_help = "  --user takes a wallet address or an avatar name; --community a community id or its name.
  The list replaces the current one; `add` and `remove` change one entry at a time."
    )]
    Set {
        name: String,
        #[command(flatten)]
        signed: SignedWriteArgs,
        #[arg(long, help = "Anyone can enter", conflicts_with_all = ["password", "users", "communities"])]
        public: bool,
        #[arg(long, help = "Anyone with this password can enter", conflicts_with_all = ["users", "communities"])]
        password: Option<String>,
        #[arg(
            long = "user",
            help = "A wallet address or avatar name allowed in (repeatable)"
        )]
        users: Vec<String>,
        #[arg(
            long = "community",
            help = "A community whose members are allowed in (repeatable)"
        )]
        communities: Vec<String>,
    },
    #[command(about = "Add one person or community to the list (signed request)")]
    Add {
        name: String,
        #[command(flatten)]
        entry: AccessEntry,
        #[command(flatten)]
        signed: SignedWriteArgs,
    },
    #[command(about = "Remove one person or community from the list (signed request)")]
    Remove {
        name: String,
        #[command(flatten)]
        entry: AccessEntry,
        #[command(flatten)]
        signed: SignedWriteArgs,
    },
    #[command(
        about = "Edit the rule in the browser: see who you are, pick from your communities and their members, sign once"
    )]
    Edit {
        name: String,
        #[command(flatten)]
        signed: SignedWriteArgs,
    },
}

#[derive(Args)]
#[group(required = true, multiple = false)]
struct AccessEntry {
    #[arg(long, help = "A wallet address or avatar name")]
    user: Option<String>,
    #[arg(long, help = "A community id or name")]
    community: Option<String>,
}

#[derive(Subcommand)]
enum WorldSettingsCommand {
    #[command(about = "Print the current settings of a world")]
    Get {
        name: String,
        #[arg(
            long = "target-server",
            alias = "target-content",
            help = "Worlds server to read from; DCL_SCENE_CLI_TARGET_SERVER from the environment otherwise"
        )]
        target_content: Option<String>,
    },
    #[command(about = "Update settings fields of a world (signed request)")]
    Set {
        name: String,
        #[command(flatten)]
        signed: SignedWriteArgs,
        #[command(flatten)]
        update: Box<world::SettingsUpdate>,
    },
}

#[derive(Subcommand)]
enum WorldPermissionsCommand {
    #[command(about = "Print who holds each permission on a world")]
    List {
        name: String,
        #[arg(
            long = "target-server",
            alias = "target-content",
            help = "Worlds server to read from; DCL_SCENE_CLI_TARGET_SERVER from the environment otherwise"
        )]
        target_content: Option<String>,
    },
    #[command(about = "Grant a permission on a world to an address (signed request)")]
    Grant {
        name: String,
        permission: String,
        address: String,
        #[command(flatten)]
        signed: SignedWriteArgs,
    },
    #[command(about = "Revoke a permission on a world from an address (signed request)")]
    Revoke {
        name: String,
        permission: String,
        address: String,
        #[command(flatten)]
        signed: SignedWriteArgs,
    },
}

#[derive(Args)]
struct SignedWriteArgs {
    #[arg(
        long = "target-server",
        alias = "target-content",
        help = "Worlds server to publish this signed request to; DCL_SCENE_CLI_TARGET_SERVER from the environment otherwise"
    )]
    target_content: Option<String>,
    #[arg(
        long,
        help = "Sign with this private-key file instead of a wallet (env: DCL_PRIVATE_KEY; this flag wins)"
    )]
    sign_key: Option<PathBuf>,
    #[arg(short = 'b', long, help = "Do not open a browser for wallet signing")]
    no_browser: bool,
    #[arg(long, help = CI_HELP)]
    ci: bool,
    #[arg(short, long, help = "Port for the local signing page")]
    port: Option<u16>,
}

impl SignedWriteArgs {
    async fn run(&self, name: &str, action: world::WorldAction) -> Result<()> {
        world::run_action(
            name,
            action,
            self.target_content.as_deref(),
            self.sign_key.as_deref(),
            world::BrowserOptions {
                port: self.port,
                no_browser: self.no_browser,
                ci: ci_mode(self.ci),
            },
        )
        .await
    }
}

struct PlainFormat;

impl<S, N> tracing_subscriber::fmt::FormatEvent<S, N> for PlainFormat
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &tracing_subscriber::fmt::FmtContext<'_, S, N>,
        mut writer: tracing_subscriber::fmt::format::Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        let prefix = match *event.metadata().level() {
            tracing::Level::ERROR => "error: ",
            tracing::Level::WARN => "warning: ",
            _ => "",
        };
        write!(writer, "{prefix}")?;
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

fn init_tracing(verbose: bool) {
    if verbose {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            // stdout is the document or the event stream of `--json`.
            .with_writer(std::io::stderr)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new("warn"))
            .event_format(PlainFormat)
            .with_writer(std::io::stderr)
            .init();
    }
}

/// Arguments are parsed before the runtime exists, so `--version`, `--help`
/// and a usage error exit without starting a worker thread per core.
fn main() {
    let cli = Cli::parse();
    let verbose = cli.verbose || std::env::var_os("RUST_LOG").is_some();
    ux::set_verbose(verbose);
    ux::set_json(cli.json);
    ux::set_ci(ux::ci_env());
    init_tracing(verbose);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("the tokio runtime starts");
    // deploy --json and deploy --programmatic have no event stream, but a
    // failure of the run is the same `error` event that start --json prints.
    let deploy_events = matches!(
        &cli.command,
        Command::Deploy { programmatic, .. } if cli.json || *programmatic
    );
    let result = runtime.block_on(async {
        dcl_scene_cli::shutdown::install();
        run(cli.command).await
    });
    if let Err(e) = result {
        if ux::usage_error(&e) {
            ux::report(&e, verbose);
            std::process::exit(2);
        }
        if deploy_events && !stopped_on_request(&e) {
            ux::set_events(true);
        }
        ux::error_event(&e);
        ux::report(&e, verbose);
        std::process::exit(1);
    }
    std::process::exit(0);
}

/// A deploy that the caller stopped is not a failure of the run: a cancel
/// through `DELETE /api/deploy`, or a signal.
fn stopped_on_request(e: &anyhow::Error) -> bool {
    e.downcast_ref::<dcl_scene_cli::linker::Cancelled>()
        .is_some()
        || dcl_scene_cli::shutdown::signalled()
}

/// `--json`: the result, as upstream writes it.
fn print_json(value: serde_json::Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(&value).unwrap_or_default()
    );
}

async fn run(command: Command) -> Result<()> {
    match command {
        Command::Init {
            dir,
            project,
            yes,
            node_modules_only,
            missing_only,
            dry_run,
            template,
            github_repo,
            skip_install,
        } => {
            let source = match (template, github_repo) {
                (Some(zip), _) => Some(init::RemoteSource::Zip(zip)),
                (None, Some(url)) => Some(init::RemoteSource::GitHub(url)),
                (None, None) => None,
            };
            match source {
                Some(source) => init::init_remote(&init::RemoteInitOptions {
                    dir,
                    yes,
                    skip_install,
                    source,
                })
                .await
                .map(|made| {
                    if ux::json() {
                        let names = made
                            .missing
                            .as_ref()
                            .map(|missing| Vec::from_iter(missing.iter().map(|m| m.name.as_str())));
                        print_json(serde_json::json!({
                            "dir": made.root.display().to_string(),
                            "missingDependencies": names,
                            "missingPackages": made.missing,
                        }));
                    }
                }),
                None if node_modules_only => {
                    let made = match missing_only {
                        true => init::install_missing_node_modules(&dir, dry_run)?,
                        false => init::install_node_modules(&dir)?,
                    };
                    if ux::json() {
                        print_json(serde_json::to_value(&made)?);
                    }
                    Ok(())
                }
                None => init::init_with(
                    &init::InitOptions {
                        dir,
                        project,
                        yes,
                        node_modules_only,
                    },
                    skip_install,
                ),
            }
        }
        Command::CrdtGen { dir, out } => {
            let generated = dcl_scene_cli::crdt_gen::generate(&dir)?
                .ok_or_else(|| anyhow::anyhow!("no .composite files under {}", dir.display()))?;
            std::fs::write(&out, &generated.bytes)?;
            println!(
                "{} bytes from {} composite(s) -> {}",
                generated.bytes.len(),
                generated.composites,
                out.display()
            );
            Ok(())
        }
        Command::VendorChunks {
            dir,
            out_core,
            out_smart,
        } => dcl_scene_cli::prebuilt::build_chunks(&dir, &out_core, &out_smart).await,
        Command::GetContextFiles { dir, offline } => {
            let api = std::env::var("DCL_SCENE_CLI_CONTEXT_API")
                .unwrap_or_else(|_| context_files::DEFAULT_API.to_string());
            context_files::get_context_files(&dir, &api, offline).await
        }
        Command::Build {
            built_in,
            dir,
            production,
            watch,
            ignore_composite,
            custom_entry_point,
            skip_install,
            skip_type_check,
            single,
            emit_declaration,
        } => {
            dcl_scene_cli::init::set_restore_node_modules(!skip_install);
            let opts = build::BuildOptions {
                built_in,
                dir,
                production,
                ignore_composite,
                custom_entry_point,
                skip_type_check,
                out_root: None,
                quiet: false,
            };
            if emit_declaration && let Some(why) = build::no_declarations(skip_type_check, watch) {
                ux::note_absent(why);
            }
            if let Some(pattern) = single {
                return build::build_single(&opts, &pattern, emit_declaration)
                    .await
                    .map(|_| ());
            }
            let mode = match emit_declaration {
                true => build::BuildMode::EmitDeclaration,
                false => build::BuildMode::Plain,
            };
            if workspace::member_folders(&opts.dir)?.is_some() {
                let ws = workspace::Workspace::load(&opts.dir)?;
                if watch {
                    return watch_workspace(&ws, &opts).await;
                }
                return build::build_workspace(&ws, &opts, mode).await;
            }
            if watch {
                let opts = build_script::preview_options(&opts);
                let project = scene::Project::load(&opts.dir)?;
                let fs = watch::FsWatcher::new(&project.root)?;
                let mut steps = ux::Steps::new(4);
                let session = watch::WatchSession::create(project, &opts, true, &mut steps).await?;
                steps.done("Watching for changes (ctrl-c to stop)");
                tokio::select! {
                    r = session.run(fs, |_| {}) => r,
                    _ = dcl_scene_cli::shutdown::shutdown_signal() => Ok(()),
                }
            } else {
                build::build_as(&opts, mode).await.map(|_| ())
            }
        }
        Command::Start {
            dir,
            port,
            skip_build,
            skip_type_check,
            skip_install,
            no_watch,
            no_build_note,
            no_browser,
            ci,
            data_layer,
            ignore_composite,
            offline_comms,
            livekit_url,
            livekit_api_key,
            livekit_api_secret,
            livekit_api_secret_file,
            livekit_room,
            no_livekit,
            multi_instance,
            no_client,
            mobile,
            explorer_alpha,
            web,
            dclenv,
            realm,
            position,
            hub,
            new_instance,
            landscape_terrain_enabled,
            skip_auth_screen,
            local_scene,
            debug,
            no_debug,
            web3,
            no_asset_bundles,
            asset_bundles: _,
            no_mcp,
            allow_remote_deploy,
            mcp: _,
            mcp_port,
            error_lines_context,
            error_source_lines_before,
            error_source_lines_after,
            explorer_params,
            tunnel,
            public_url,
            tunnel_token,
            tunnel_token_file,
            no_host,
            talk,
            player,
        } => {
            if tunnel.as_deref().map(str::trim) == Some("help") {
                println!("{}", dcl_scene_cli::tunnel::tunnel_help());
                return Ok(());
            }
            // start has no result document: under --json its stdout is a
            // stream of events and its text goes to stderr.
            if ux::json() {
                ux::set_events(true);
            }
            ux::set_custom_build_note(!no_build_note);
            dcl_scene_cli::init::set_restore_node_modules(!skip_install);
            let ci = ci_mode(ci);
            let tunnel_token = if tunnel.is_some() {
                dcl_scene_cli::tunnel::resolve_token(tunnel_token, tunnel_token_file.as_deref())?
            } else {
                tunnel_token
            };
            let livekit = dcl_scene_cli::livekit::resolve(dcl_scene_cli::livekit::CliArgs {
                url: livekit_url,
                api_key: livekit_api_key,
                api_secret: livekit_api_secret,
                api_secret_file: livekit_api_secret_file,
                room: livekit_room.clone(),
                offline_comms,
            })?;
            let launch_desktop = explorer_alpha && !ci && !no_client;
            let launch_web = web && !no_browser && !ci && !no_client;
            for (given, note) in [
                (
                    no_browser && !talk && !web && !ci,
                    "--no-browser has no effect without --talk or --web (dcl-scene-cli opens a browser only for those)",
                ),
                (
                    no_client && !explorer_alpha && !web,
                    "--no-client has no effect without --explorer-alpha or --web (dcl-scene-cli launches a client only when asked)",
                ),
                (
                    explorer_alpha && !launch_desktop,
                    "--explorer-alpha: not launching the desktop Explorer (--ci or --no-client); the deep link still prints",
                ),
                (
                    web && !launch_web && !talk,
                    "--web: not opening a browser (--no-browser, --ci or --no-client); the web link still prints",
                ),
                (
                    local_scene,
                    "--local-scene is already the default: it rides on every desktop deep link",
                ),
                (
                    debug || no_debug || web3,
                    "--debug, --no-debug and --web3 are accepted for upstream parity and do nothing",
                ),
            ] {
                if given {
                    ux::note(note);
                }
            }
            let mut explorer_params = explorer_params;
            for (on, key) in [
                (hub, "hub"),
                (new_instance, "open-deeplink-in-new-instance"),
                (landscape_terrain_enabled, "landscape-terrain-enabled"),
            ] {
                if on {
                    explorer_params.push(format!("--{key}=true"));
                }
            }
            start::start(start::StartOptions {
                dir,
                port,
                skip_build,
                skip_type_check,
                no_watch,
                ignore_composite,
                offline_comms,
                livekit,
                embedded_livekit: !no_livekit,
                livekit_room,
                mobile,
                ab_sidecar: !no_asset_bundles,
                local_ab: !no_asset_bundles,
                mcp: !no_mcp,
                allow_remote_deploy,
                mcp_port: resolved_mcp_port(mcp_port),
                source_context: start::SourceContext::resolve(
                    error_lines_context,
                    error_source_lines_before,
                    error_source_lines_after,
                ),
                explorer_params,
                multi_instance,
                skip_auth_screen,
                data_layer,
                tunnel,
                public_url,
                tunnel_token,
                no_host,
                talk: talk.then_some(start::Talk {
                    player,
                    open_browser: !no_browser && !ci,
                }),
                dclenv,
                realm,
                position,
                launch_desktop,
                launch_web,
            })
            .await
        }
        Command::Host { dir, preview, room } => {
            dcl_scene_cli::host::host(&dcl_scene_cli::host::HostOptions { dir, preview, room })
                .await
        }
        Command::Deploy {
            dir,
            target,
            target_content,
            sign_key,
            skip_build,
            skip_validations,
            skip_version_checks,
            https,
            force_upload,
            multi_scene: _,
            dry_run,
            timestamp,
            entity_out,
            replace_world_scenes,
            check_permissions,
            yes,
            no_browser,
            ci,
            port,
            host,
            language,
            programmatic,
            deploy_token,
        } => {
            let signing = dcl_scene_cli::linker::signing_bind(host)?;
            if programmatic {
                ux::set_quiet(true);
            }
            for (given, note) in [
                (
                    https,
                    "--https is accepted for upstream parity: the signing page is served on loopback, a secure context already",
                ),
                (
                    skip_version_checks,
                    "--skip-version-checks is accepted for upstream parity: this CLI runs no SDK version check",
                ),
                (
                    language.is_some(),
                    "--language is accepted for upstream parity: the text of this CLI and of its signing page is English only",
                ),
            ] {
                if given {
                    ux::note(note);
                }
            }
            deploy::deploy(&deploy::DeployOptions {
                dir,
                target,
                target_content,
                sign_key,
                skip_build,
                skip_validations,
                force_upload,
                dry_run,
                timestamp,
                entity_out,
                multi_scene: !replace_world_scenes,
                check_permissions,
                yes,
                no_browser,
                ci: ci_mode(ci),
                port,
                signing,
                quiet: ux::quiet(),
                host_signer: None,
                identity: None,
                programmatic,
                deploy_token,
            })
            .await
            .map(|published| {
                if ux::json() && !programmatic {
                    print_json(serde_json::json!({
                        "entityId": published.entity_id,
                        "message": published.message,
                    }));
                }
            })
        }
        Command::Unpublish {
            parcel,
            target,
            target_content,
            sign_key,
        } => {
            deploy::unpublish(&deploy::UnpublishOptions {
                parcel,
                target,
                target_content,
                sign_key,
            })
            .await
        }
        Command::Pack { dir, skip_build } => {
            pack::pack(&pack::PackOptions { dir, skip_build }).await
        }
        Command::CodeToComposite { dir, yes } => {
            code_to_composite::code_to_composite(&code_to_composite::CodeToCompositeOptions {
                dir,
                yes,
            })
            .await
        }
        Command::ExportStatic {
            dir,
            destination,
            timestamp,
            realm_name,
            comms_adapter,
            base_url,
            skip_build,
        } => export_static::export_static(&export_static::ExportStaticOptions {
            dir,
            destination,
            timestamp,
            realm_name,
            comms_adapter,
            base_url,
            skip_build,
        })
        .await
        .map(|exported| {
            if ux::json() {
                // A path is written as it is shown: one that is not UTF-8
                // has no JSON form of its own.
                let shown = |path: &std::path::Path| path.display().to_string();
                let entity_files: Vec<_> = exported
                    .entities
                    .iter()
                    .map(|id| shown(&exported.destination.join(id)))
                    .collect();
                print_json(serde_json::json!({
                    "scenesUrn": exported.scenes_urn,
                    "destination": shown(&exported.destination),
                    "entities": exported.entities,
                    "entityFiles": entity_files,
                    "contentFiles": exported.content_files,
                    "aboutFile": exported.about.as_deref().map(shown),
                }));
            }
        }),
        Command::World { command } => run_world(command).await,
        Command::Storage { command } => dcl_scene_cli::storage_cli::run(command).await,
        Command::Sync { opts } => dcl_scene_cli::project_sync::run(opts).await,
    }
}

async fn run_world(command: WorldCommand) -> Result<()> {
    match command {
        WorldCommand::RemoveScene {
            name,
            coordinate,
            entity_id,
            signed,
        } => {
            signed
                .run(
                    &name,
                    world::WorldAction::SceneRemove {
                        coordinate,
                        entity_id,
                    },
                )
                .await
        }
        WorldCommand::Settings { command } => match command {
            WorldSettingsCommand::Get {
                name,
                target_content,
            } => world::settings_get(&name, target_content.as_deref()).await,
            WorldSettingsCommand::Set {
                name,
                signed,
                update,
            } => {
                signed
                    .run(&name, world::WorldAction::SettingsSet(*update))
                    .await
            }
        },
        WorldCommand::Permissions { command } => {
            let revoke = matches!(command, WorldPermissionsCommand::Revoke { .. });
            match command {
                WorldPermissionsCommand::List {
                    name,
                    target_content,
                } => world::permissions_list(&name, target_content.as_deref()).await,
                WorldPermissionsCommand::Grant {
                    name,
                    permission,
                    address,
                    signed,
                }
                | WorldPermissionsCommand::Revoke {
                    name,
                    permission,
                    address,
                    signed,
                } => {
                    signed
                        .run(
                            &name,
                            world::WorldAction::Permission {
                                permission,
                                address,
                                revoke,
                            },
                        )
                        .await
                }
            }
        }
        WorldCommand::Access { command } => run_world_access(command).await,
    }
}

async fn run_world_access(command: WorldAccessCommand) -> Result<()> {
    let revoke = matches!(command, WorldAccessCommand::Remove { .. });
    match command {
        WorldAccessCommand::Show {
            name,
            target_content,
        } => world_access::show(&name, target_content.as_deref()).await,
        WorldAccessCommand::Set {
            name,
            signed,
            public,
            password,
            users,
            communities,
        } => {
            let spec = if public {
                world_access::AccessSpec::Unrestricted
            } else if let Some(secret) = password {
                world_access::AccessSpec::SharedSecret { secret }
            } else if users.is_empty() && communities.is_empty() {
                return Err(ux::UserError::new(
                    "nothing to set \u{2014} no rule given",
                    ux::TrySteps::one(
                        "pass --public, --password <secret>, or --user/--community entries",
                    )
                    .and("or `world access edit <name>` to pick them in the browser"),
                )
                .into());
            } else {
                let dir = world_access::Directory::from_env()?;
                world_access::AccessSpec::allow_list(
                    &world_access::resolve_people(&dir, &users).await?,
                    &world_access::resolve_communities(&dir, &communities).await?,
                )
            };
            signed.run(&name, world::WorldAction::AccessSet(spec)).await
        }
        WorldAccessCommand::Add {
            name,
            entry,
            signed,
        }
        | WorldAccessCommand::Remove {
            name,
            entry,
            signed,
        } => {
            let dir = world_access::Directory::from_env()?;
            let action = match (entry.user, entry.community) {
                (Some(user), _) => world::WorldAction::Permission {
                    permission: "access".to_string(),
                    address: world_access::resolve_person(&dir, &user).await?,
                    revoke,
                },
                (_, Some(community)) => world::WorldAction::AccessCommunity {
                    id: world_access::resolve_community(&dir, &community).await?,
                    revoke,
                },
                (None, None) => unreachable!("clap requires one of --user/--community"),
            };
            signed.run(&name, action).await
        }
        WorldAccessCommand::Edit { name, signed } => {
            world_access::edit(
                &name,
                signed.target_content.as_deref(),
                world::BrowserOptions {
                    port: signed.port,
                    no_browser: signed.no_browser,
                    ci: ci_mode(signed.ci),
                },
            )
            .await
        }
    }
}

async fn watch_workspace(ws: &workspace::Workspace, opts: &build::BuildOptions) -> Result<()> {
    let mut runners = Vec::new();
    for (i, project) in ws.projects.iter().enumerate() {
        if let Some(header) = ws.member_header(i) {
            ux::note(header);
        }
        let member = build_script::preview_options(&build::member_options(opts, project));
        let chunk = 3;
        let tc = if member.skip_type_check { 0 } else { 1 };
        let mut steps = ux::Steps::new(chunk + tc);
        let fs = watch::FsWatcher::new(&project.root)?;
        let session =
            watch::WatchSession::create(project.clone(), &member, true, &mut steps).await?;
        if session.is_custom() {
            // The build script has already performed its own checks.
        } else if member.skip_type_check {
            ux::note("type check skipped (--skip-type-check)");
        } else {
            match build::type_check(session.project(), build::Reloaded::Yes).await {
                Ok(_) => {
                    tracing::info!("type checking completed without errors");
                    steps.done("Type check passed");
                }
                Err(e) => ux::report_watch(&e),
            }
        }
        runners.push((session, fs));
    }
    ux::note("Watching for changes (ctrl-c to stop)");
    let mut set = tokio::task::JoinSet::new();
    for (session, fs) in runners {
        set.spawn(session.run(fs, |_| {}));
    }
    tokio::select! {
        joined = set.join_next() => match joined {
            Some(Ok(r)) => r,
            Some(Err(e)) => Err(e.into()),
            None => Ok(()),
        },
        _ = dcl_scene_cli::shutdown::shutdown_signal() => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    fn try_start(args: &[&str]) -> Result<Cli, clap::Error> {
        let mut argv = vec!["dcl-scene-cli", "start"];
        argv.extend_from_slice(args);
        Cli::try_parse_from(argv)
    }

    fn parse(args: &[&str]) -> Command {
        try_start(args)
            .unwrap_or_else(|e| panic!("expected `start {args:?}` to parse: {e}"))
            .command
    }

    /// (no_mcp, mcp, mcp_port) of a parsed `start`.
    fn mcp_flags(args: &[&str]) -> (bool, bool, Option<u16>) {
        match parse(args) {
            Command::Start {
                no_mcp,
                mcp,
                mcp_port,
                ..
            } => (no_mcp, mcp, mcp_port),
            _ => panic!("not a start command"),
        }
    }

    fn start_err(args: &[&str]) -> clap::Error {
        match try_start(args) {
            Ok(_) => panic!("expected `start {args:?}` to be rejected"),
            Err(e) => e,
        }
    }

    /// (talk, player) of a parsed `start`.
    fn talk_flags(args: &[&str]) -> (bool, Option<String>) {
        match parse(args) {
            Command::Start { talk, player, .. } => (talk, player),
            _ => panic!("not a start command"),
        }
    }

    /// Every flag `@dcl/sdk-commands` documents parses here under the same
    /// spelling, so a script written for one CLI runs on the other.
    #[test]
    fn upstream_flags_parse() {
        let accepted: &[&[&str]] = &[
            &[
                "init",
                "--yes",
                "--dir",
                "d",
                "--skip-install",
                "--project",
                "scene-template",
            ],
            &["init", "-y", "--project", "px-template"],
            &["init", "--project", "library"],
            &["init", "--project", "smart-wearable"],
            &[
                "init",
                "--template",
                "https://example.org/t.zip",
                "--skip-install",
            ],
            &[
                "init",
                "--github-repo",
                "https://github.com/decentraland/sdk7-scene-template",
            ],
            &[
                "build",
                "--watch",
                "--production",
                "--ignoreComposite",
                "--customEntryPoint",
                "--skip-install",
                "--dir",
                "d",
            ],
            &["build", "-w", "-p"],
            &["build", "--single", "src/**/*.ts", "--emitDeclaration"],
            &[
                "start",
                "--dir",
                "d",
                "--port",
                "0",
                "--no-debug",
                "--no-browser",
                "--no-watch",
                "--ci",
                "--skip-install",
                "--web3",
                "--skip-build",
                "--data-layer",
                "--explorer-alpha",
                "--hub",
                "--mobile",
                "--debug",
                "--dclenv",
                "zone",
                "--realm",
                "http://127.0.0.1:1",
                "--local-scene",
                "--position",
                "1,-2",
                "--skip-auth-screen",
                "--landscape-terrain-enabled",
                "-n",
                "--bevy-web",
                "--multi-instance",
                "--asset-bundles",
                "--no-client",
                "--no-server",
                "--mcp",
                "--mcp-port",
                "9000",
            ],
            &["start", "-p", "0", "-d", "-b", "-w", "-m", "--web"],
            &[
                "deploy",
                "--dir",
                "d",
                "--target",
                "peer.decentraland.org",
                "--skip-validations",
                "--skip-version-checks",
                "--skip-build",
                "--https",
                "--force-upload",
                "--yes",
                "--no-browser",
                "--port",
                "1",
                "--multi-scene",
                "--programmatic",
                "--deploy-token",
                "0123456789abcdef",
                "--json",
            ],
            &["deploy", "-t", "x", "-b", "-p", "1"],
            &["deploy", "--target-content", "https://x/content"],
            &[
                "export-static",
                "--dir",
                "d",
                "--destination",
                "o",
                "--timestamp",
                "2023-11-14T22:13:20Z",
                "--realmName",
                "r",
                "--commsAdapter",
                "offline:offline",
                "--baseUrl",
                "https://x/",
            ],
            &[
                "export-static",
                "--realm-name",
                "r",
                "--base-url",
                "https://x",
                "--skip-build",
            ],
            &["code-to-composite", "--dir", "d", "--yes"],
            &["code-to-composite", "-y"],
        ];
        for case in accepted {
            let mut argv = vec!["dcl-scene-cli"];
            argv.extend_from_slice(case);
            Cli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{case:?}: {e}"));
        }
        let rejected: &[&[&str]] = &[
            &["init", "--project", "scene", "--template", "t.zip"],
            &[
                "init",
                "--template",
                "t.zip",
                "--github-repo",
                "https://github.com/a/b",
            ],
            &["build", "--single", "src/a.ts", "--watch"],
            &["deploy", "--multi-scene", "--replace-world-scenes"],
            &["start", "--position", "nope"],
            &["start", "--dclenv", "staging"],
            &["export-static", "--realmName", "r"],
            &["export-static", "--timestamp", "soon"],
            &["code-to-composite", "--destination", "o"],
        ];
        for case in rejected {
            let mut argv = vec!["dcl-scene-cli"];
            argv.extend_from_slice(case);
            assert!(
                Cli::try_parse_from(&argv).is_err(),
                "{case:?} should be rejected"
            );
        }
        match parse(&["--position", " 3 , -4 "]) {
            Command::Start { position, .. } => assert_eq!(position, Some((3, -4))),
            _ => panic!("not a start command"),
        }
    }

    #[test]
    fn talk_is_off_by_default_and_player_rides_on_it() {
        assert_eq!(talk_flags(&[]), (false, None));
        assert_eq!(talk_flags(&["--talk"]), (true, None));
        assert_eq!(
            talk_flags(&["--talk", "--player", "https://example.org/play/"]),
            (true, Some("https://example.org/play/".to_string()))
        );
        assert_eq!(
            start_err(&["--player", "https://example.org/play/"]).kind(),
            ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn mcp_is_on_by_default_and_agrees_with_the_explorers_own_port() {
        assert_eq!(mcp_flags(&[]), (false, false, None));
        assert_eq!(
            resolved_mcp_port(None),
            joinblock::DEFAULT_EXPLORER_MCP_PORT
        );
        assert_eq!(resolved_mcp_port(Some(9111)), 9111);
    }

    #[test]
    fn bare_mcp_is_accepted_and_changes_nothing() {
        assert_eq!(mcp_flags(&["--mcp"]), (false, true, None));
        let (no_mcp, _, mcp_port) = mcp_flags(&["--mcp"]);
        assert_eq!((no_mcp, resolved_mcp_port(mcp_port)), {
            let (d_no_mcp, _, d_port) = mcp_flags(&[]);
            (d_no_mcp, resolved_mcp_port(d_port))
        });
    }

    #[test]
    fn no_mcp_conflicts_with_mcp_and_with_mcp_port() {
        assert_eq!(mcp_flags(&["--no-mcp"]), (true, false, None));
        assert_eq!(
            start_err(&["--mcp", "--no-mcp"]).kind(),
            ErrorKind::ArgumentConflict
        );
        assert_eq!(
            start_err(&["--no-mcp", "--mcp-port", "8123"]).kind(),
            ErrorKind::ArgumentConflict
        );
    }

    #[test]
    fn mcp_port_rejects_values_the_explorer_would_not_serve() {
        for bad in ["0", "1", "1023", "70000", "abc"] {
            let err = start_err(&["--mcp-port", bad]);
            assert_eq!(
                err.kind(),
                ErrorKind::ValueValidation,
                "--mcp-port {bad} should be rejected"
            );
        }
        assert!(try_start(&["--mcp-port", "-1"]).is_err());
        let msg = start_err(&["--mcp-port", "0"]).to_string();
        assert!(
            msg.contains("--mcp-port") && msg.contains("1024") && msg.contains("65535"),
            "unhelpful error: {msg}"
        );
        assert_eq!(mcp_flags(&["--mcp-port", "1024"]).2, Some(1024));
        assert_eq!(mcp_flags(&["--mcp-port", "65535"]).2, Some(65535));
        assert_eq!(
            mcp_flags(&["--mcp-port", "8123"]).2,
            Some(joinblock::DEFAULT_EXPLORER_MCP_PORT)
        );
    }
}
