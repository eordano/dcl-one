use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{Parser, Subcommand};

use catalyst::config::Config;
use catalyst::{builder, comms, content, db, explore, http, ops, storage};
use catalyst_config::posture;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(test)]
mod cli_tests;

#[derive(Parser)]
#[command(name = "catalyst", version, about = "Decentraland Catalyst node")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Verify the schema, attach the workers and serve HTTP.
    Serve(Box<Config>),
    /// Run chain indexes and marketplace mirrors without an HTTP listener.
    Index(Box<Config>),
    /// Administrative schema operations.
    Schema {
        #[command(subcommand)]
        command: SchemaCommand,
    },
    /// Builder operator commands (the old `catalyrst-builder catalog|seed`).
    Builder {
        #[command(subcommand)]
        command: BuilderCommand,
    },
    /// Blob store operator commands.
    Storage {
        #[command(subcommand)]
        command: StorageCommand,
    },
    /// Imposter store operator commands (the old `catalyrst-bvimposters
    /// seed|quarantine`), on the store at `BVIMPOSTERS_STORE_ROOT`.
    Bvimposters {
        #[command(subcommand)]
        command: BvimpostersCommand,
    },
    /// Run Pulse (the ENet/UDP + WebTransport realtime layer) in its own
    /// process; reads the `PULSE_*` contract (`comms::pulse::env::ENV_DOCS`)
    /// and installs its own tracing and Prometheus exporter. Deployed as a
    /// second unit beside `serve` (AGENTS.md, "Ownership").
    Pulse,
    /// Community operator commands.
    Communities {
        #[command(subcommand)]
        command: CommunitiesCommand,
    },
    /// Explore (places, events) operator commands.
    Explore {
        #[command(subcommand)]
        command: ExploreCommand,
    },
    /// The secrets a self-contained host mints for itself (deploy/README.md, "Secrets").
    Secrets {
        #[command(subcommand)]
        command: SecretsCommand,
    },
    /// The node postures `NODE_POSTURE` names.
    Posture {
        #[command(subcommand)]
        command: PostureCommand,
    },
}

#[derive(Subcommand)]
enum PostureCommand {
    /// Print every variable `NODE_POSTURE=<name>` sets and its value, by
    /// family (a variable set in the environment wins over it).
    Show {
        #[arg(value_parser = ["mirror", "island", "journey"])]
        name: String,
    },
    /// Print `KEY=value` for the listed keys (every key when none is listed)
    /// as `NODE_POSTURE=<name>` sets them in an empty environment: the
    /// environment file of a process that reads part of the table without
    /// running this binary (the sites server). A key the posture leaves to
    /// the binary's own default prints nothing.
    Env {
        #[arg(value_parser = ["mirror", "island", "journey"])]
        name: String,
        keys: Vec<String>,
    },
}

#[derive(Subcommand)]
enum ExploreCommand {
    /// Import upstream's `user_likes` export as the ground truth of the like
    /// rates: one JSON object a line, as `psql "$OLD_PLACES_DB" -Atc "COPY
    /// (SELECT row_to_json(l) FROM user_likes l) TO STDOUT"` writes it. Each
    /// row becomes a like fact with `source = 'upstream'` and its voting
    /// power at vote time; a wallet that already voted on this node keeps
    /// its local fact. Each entity is settled and recounted in its own
    /// transaction, once: a later export adds nothing to a settled entity, so
    /// export after the old service stops taking votes. A malformed line
    /// imports nothing. Prints the outcome as JSON. Not behind
    /// DATA_WRITES_ENABLED: an import is operator data.
    ImportLikes {
        #[arg(long, env = "DATABASE_URL", hide_env_values = true)]
        database_url: String,
        /// The export; `-` reads standard input.
        #[arg(long)]
        file: String,
    },
}

#[derive(Subcommand)]
enum CommunitiesCommand {
    /// One pass of the upstream communities mirror (the `communities-mirror`
    /// job of `COMMUNITIES_MIRROR_UPSTREAM`, on demand): pages the upstream
    /// public listing into `social.communities` as read-only mirrored rows
    /// (no local roles; writes on them answer 403), resuming an interrupted
    /// pass, and prints the outcome as JSON. A complete pass deactivates the
    /// mirrored communities not listed for `--grace-secs`. Not behind
    /// SOCIAL_WRITES_ENABLED: a mirror is not local authority.
    Mirror {
        #[arg(long, env = "DATABASE_URL", hide_env_values = true)]
        database_url: String,
        #[arg(
            long,
            env = "COMMUNITIES_UPSTREAM_URL",
            default_value = "https://social-api.decentraland.org"
        )]
        upstream_url: String,
        #[arg(
            long,
            env = "COMMUNITIES_MIRROR_THUMBNAIL_URL",
            default_value = "https://assets-cdn.decentraland.org"
        )]
        thumbnail_url: String,
        #[arg(long, default_value_t = 7200)]
        grace_secs: u64,
    },
}

#[derive(Subcommand)]
enum SecretsCommand {
    /// Fill every slot `--serve-file` lacks (the LiveKit API key pair the SFU
    /// on this host shares, `PULSE_ROOM_AUTHORITY_KEY`, `ENCRYPTION_KEY`, the
    /// authoritative scene-host wallet with `AUTHORITATIVE_SERVER_ADDRESS`, the
    /// admin bearers; `AB_REGISTRY_ADMIN_TOKEN` with `--with-abgen`), keep every
    /// present line as it is, and derive `--pulse-file` from the result. Files
    /// land 0400 in a 0700 directory; nothing is rewritten when nothing changed.
    /// Prints the slots it minted. The `catalyst-secrets-init` oneshot runs it
    /// before serve, pulse and the SFU on a host with `[secrets].generated`.
    Init {
        #[arg(long)]
        serve_file: PathBuf,
        #[arg(long)]
        pulse_file: Option<PathBuf>,
        #[arg(long)]
        with_abgen: bool,
    },
}

#[derive(Subcommand)]
enum BuilderCommand {
    /// Walk an `@dcl/asset-packs` tree, store every asset file in the blob
    /// store and upsert the one `builder.asset_catalog` row `/v1/assetPacks`
    /// serves (503 with this command's hint until it runs). Given the npm
    /// package's `catalog.json` instead, upsert the row from it and store
    /// nothing: the files are pulled from the builder bucket on first read.
    Catalog {
        packs_dir: String,
        #[arg(long, env = "DATABASE_URL", hide_env_values = true)]
        database_url: String,
        #[arg(
            long,
            env = "STORAGE_ROOT_FOLDER",
            default_value = "/var/lib/catalyst/content"
        )]
        storage_root_folder: String,
    },
    /// Pull the named asset-pack files from a bucket into the blob store.
    Seed {
        base_url: String,
        hashes: Vec<String>,
        #[arg(
            long,
            env = "STORAGE_ROOT_FOLDER",
            default_value = "/var/lib/catalyst/content"
        )]
        storage_root_folder: String,
    },
}

#[derive(Subcommand)]
enum StorageCommand {
    /// The store-wide collector (`storage::gc`, the worlds `/admin/gc`
    /// without the HTTP switch): reports the aged files no domain names and
    /// deletes them only with `--delete`.
    Gc {
        #[arg(long)]
        delete: bool,
        /// Raise the age floor (one hour at least).
        #[arg(long, default_value_t = 3600)]
        min_age_secs: u64,
        #[arg(long)]
        allow_empty_reference_set: bool,
        #[arg(long, env = "DATABASE_URL", hide_env_values = true)]
        database_url: String,
        #[arg(
            long,
            env = "STORAGE_ROOT_FOLDER",
            default_value = "/var/lib/catalyst/content"
        )]
        storage_root_folder: String,
        #[arg(long, env = "GLOBAL_SCENES_URN")]
        global_scenes_urn: Option<String>,
    },
}

#[derive(Subcommand)]
enum BvimpostersCommand {
    /// Import a renderer's imposter cache (`{level}/{x},{y}-spec.json` +
    /// `.boimp` + `-floor.boimp`) into the store as stored zips.
    Seed {
        realm_cache_dir: String,
        #[arg(
            long,
            env = "BVIMPOSTERS_STORE_ROOT",
            default_value = "/var/lib/catalyst/bvimposters"
        )]
        store_root: String,
    },
    /// Move every stored zip the list names (`{level}/{x},{y}.{crc}[.zip]`
    /// per line) out of the store; the list defaults to
    /// `BVIMPOSTERS_QUARANTINE_LIST`, else `<store root>/readthrough-quarantine.txt`.
    Quarantine {
        list: Option<String>,
        #[arg(
            long,
            env = "BVIMPOSTERS_STORE_ROOT",
            default_value = "/var/lib/catalyst/bvimposters"
        )]
        store_root: String,
        #[arg(long, env = "BVIMPOSTERS_QUARANTINE_LIST")]
        default_list: Option<String>,
    },
}

#[derive(Subcommand)]
enum SchemaCommand {
    /// Apply the schema to an empty database; refuses if meta.schema exists.
    Apply {
        #[arg(long, env = "DATABASE_URL", hide_env_values = true)]
        database_url: String,
    },
    /// Bring a database stamped by an earlier binary of the same schema
    /// identity and version up to this binary's layout, in one transaction:
    /// the additive steps of schemas/migrations/ it has not recorded in
    /// meta.schema_steps (a column an existing table gained, a constraint, a
    /// backfill), then every object of schema.sql it lacks (tables, indexes,
    /// types, functions, triggers, with the seeds and sequence ownership of
    /// the tables it creates). It never alters or drops anything a step does
    /// not spell out, refuses a step that would (drop, rename, retype,
    /// delete), a recorded step whose text changed, another identity or
    /// version, and a column still missing after the steps; a refusal
    /// applies nothing. The administrative role runs it; the schema oneshot
    /// does on every activation.
    Migrate {
        #[arg(long, env = "DATABASE_URL", hide_env_values = true)]
        database_url: String,
        /// On a database one schema version behind, first run the embedded
        /// release (schemas/releases/v3-private-voice-pending.sql) in the
        /// same transaction. Refused while any other session is connected
        /// to the database: stop every writer first. No effect on a database
        /// already at this binary's version.
        #[arg(long)]
        release: bool,
    },
    /// Compare a database with this binary's layout without changing it:
    /// the meta.schema stamp against this binary's identity and version, the
    /// steps of schemas/migrations/ pending or drifted, the objects and
    /// columns missing. Exit 0 when serve can run on it as it is, 3 when
    /// `schema migrate` brings it up to date, 4 when it cannot (no stamp,
    /// another identity or version, a drifted or refused step, a missing
    /// column no step adds), 5 when it is one version behind and
    /// `schema migrate --release` takes it up once every writer is stopped.
    Status {
        #[arg(long, env = "DATABASE_URL", hide_env_values = true)]
        database_url: String,
        /// Write the exit code to this file and exit 0 instead, for a
        /// container without a shell whose next step reads the verdict.
        #[arg(long)]
        exit_code_file: Option<PathBuf>,
    },
    /// Print the schema this binary applies (schema/*.sql rendered in name
    /// order), for `psql -f` on a lab database or a diff against another
    /// build. `--migrations` prints the steps of schemas/migrations/ instead.
    Print {
        #[arg(long)]
        migrations: bool,
    },
    /// Print what this binary migrates a database to, one line each: the
    /// identity and version it stamps, the SHA-256 of its schema.sql and
    /// each step of schemas/migrations/ with its SHA-256. Two builds that
    /// print the same layout migrate a database alike; local.sh compares the
    /// dev build's with the commit build's before the dev build migrates the
    /// database the two share.
    Layout,
    /// Print this binary's content and admission functions as idempotent
    /// `CREATE OR REPLACE FUNCTION` statements, for an administrative role
    /// to apply to a database created before them:
    /// `catalyst schema functions | psql "$ADMIN_DSN"`.
    Functions,
}

/// `catalyst schema layout`.
fn schema_layout() -> String {
    catalyst::schema_migrate::layout(
        db::SCHEMA_IDENTITY,
        db::SCHEMA_VERSION,
        db::SCHEMA_SQL,
        db::MIGRATIONS_SQL,
    )
}

fn main() -> anyhow::Result<()> {
    // The posture writes the environment clap reads, so it expands first,
    // while this is the only thread.
    posture::apply_to_process_env()?;
    // The complete clap tree exceeds the default Windows debug stack. This
    // parser-only thread returns plain values and finishes before V8 or runtime
    // initialization; it never owns engine state or serves matcher work.
    let parsed = std::thread::Builder::new()
        .name("cli-parser".into())
        .stack_size(64 << 20)
        .spawn(|| -> Result<(Cli, clap::ArgMatches), clap::Error> {
            let matches = <Cli as clap::CommandFactory>::command().try_get_matches()?;
            let cli = <Cli as clap::FromArgMatches>::from_arg_matches(&matches)?;
            Ok((cli, matches))
        })
        .context("creating CLI parser thread")?
        .join()
        .map_err(|_| anyhow::anyhow!("CLI parser thread panicked"))?;
    let (mut cli, matches) = parsed.unwrap_or_else(|error| error.exit());
    let prepared = match &mut cli.command {
        Command::Serve(config) => {
            if let Some(arguments) = matches.subcommand_matches("serve") {
                config.refuse_argv_secrets(arguments)?;
            }
            check_posture(config)?;
            catalyst::worlds::scene_state::jsruntime::ensure_v8_initialized();
            // SAFETY: This startup thread performs the first common initialization.
            // All runtime/blocking workers descend from it afterwards. Prepared
            // state never escapes this runtime; its drop joins canceled blocking
            // work, and final handles are destroyed within this permitted subtree.
            Some(unsafe {
                catalyst::metrics_matcher::prepare(
                    catalyst::comms::stats::http::Settings::from_env(),
                )?
            })
        }
        Command::Index(config) => {
            if let Some(arguments) = matches.subcommand_matches("index") {
                config.refuse_argv_secrets(arguments)?;
            }
            check_posture(config)?;
            None
        }
        _ => None,
    };
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("building the tokio runtime")?
        .block_on(run(cli, prepared))
}

/// The refusals of [`posture::check`] against this process's environment.
fn check_posture(config: &Config) -> anyhow::Result<()> {
    posture::check(config, &|key: &str| std::env::var(key).ok())
}

async fn run(
    cli: Cli,
    prepared: Option<catalyst::metrics_matcher::PreparedStats>,
) -> anyhow::Result<()> {
    match cli.command {
        Command::Serve(config) => {
            let prepared =
                prepared.context("serve metrics must be prepared before runtime creation")?;
            catalyst::serve::serve(*config, prepared).await
        }
        Command::Index(config) => catalyst::index::run(*config).await,
        Command::Schema {
            command: SchemaCommand::Apply { database_url },
        } => {
            tracing_subscriber::fmt().with_target(false).init();
            let pool = db::connect_pool(
                &database_url,
                &db::PoolSettings {
                    max_connections: 1,
                    ..Default::default()
                },
            )
            .await
            .context("connecting to DATABASE_URL")?;
            db::apply_schema(&pool).await?;
            Ok(())
        }
        Command::Schema {
            command:
                SchemaCommand::Migrate {
                    database_url,
                    release,
                },
        } => {
            tracing_subscriber::fmt().with_target(false).init();
            let pool = db::connect_pool(
                &database_url,
                &db::PoolSettings {
                    max_connections: 1,
                    ..Default::default()
                },
            )
            .await
            .context("connecting to DATABASE_URL")?;
            let done = if release {
                catalyst::schema_migrate::migrate_with_release(
                    &pool,
                    db::SCHEMA_SQL,
                    db::MIGRATIONS_SQL,
                    db::RELEASE_SQL,
                )
                .await?
            } else {
                catalyst::schema_migrate::migrate(&pool, db::SCHEMA_SQL, db::MIGRATIONS_SQL).await?
            };
            if let Some(version) = done.released {
                println!("schema migrate: released the database to version {version}");
            }
            if done.is_empty() {
                println!("schema migrate: the database has every object of this binary's schema");
            } else {
                if !done.steps.is_empty() {
                    println!("schema migrate: applied {} step(s):", done.steps.len());
                    for s in &done.steps {
                        println!("  {s}");
                    }
                }
                if !done.created.is_empty() {
                    println!("schema migrate: created {} object(s):", done.created.len());
                    for o in &done.created {
                        println!("  {o}");
                    }
                }
            }
            Ok(())
        }
        Command::Schema {
            command:
                SchemaCommand::Status {
                    database_url,
                    exit_code_file,
                },
        } => {
            let pool = db::connect_pool(
                &database_url,
                &db::PoolSettings {
                    max_connections: 1,
                    ..Default::default()
                },
            )
            .await
            .context("connecting to DATABASE_URL")?;
            let status =
                catalyst::schema_migrate::status(&pool, db::SCHEMA_SQL, db::MIGRATIONS_SQL).await?;
            for line in status.lines() {
                println!("schema status: {line}");
            }
            if let Some(path) = exit_code_file {
                std::fs::write(&path, format!("{}\n", status.exit_code()))
                    .with_context(|| format!("writing {}", path.display()))?;
                return Ok(());
            }
            std::process::exit(status.exit_code());
        }
        Command::Schema {
            command: SchemaCommand::Print { migrations },
        } => {
            print!(
                "{}",
                if migrations {
                    db::MIGRATIONS_SQL
                } else {
                    db::SCHEMA_SQL
                }
            );
            Ok(())
        }
        Command::Schema {
            command: SchemaCommand::Layout,
        } => {
            print!("{}", schema_layout());
            Ok(())
        }
        Command::Schema {
            command: SchemaCommand::Functions,
        } => {
            print!("{}", content::land_publish::content_functions_sql(true));
            print!(
                "{}",
                comms::gatekeeper::admission::functions::social_functions_sql(true)
            );
            Ok(())
        }
        Command::Builder {
            command:
                BuilderCommand::Catalog {
                    packs_dir,
                    database_url,
                    storage_root_folder,
                },
        } => {
            tracing_subscriber::fmt().with_target(false).init();
            let pool = db::connect_pool(
                &database_url,
                &db::PoolSettings {
                    max_connections: 2,
                    ..Default::default()
                },
            )
            .await
            .context("connecting to DATABASE_URL")?;
            db::bootstrap(&pool).await?;
            let storage = storage::Storage::from_config(Path::new(&storage_root_folder))
                .await
                .with_context(|| format!("opening STORAGE_ROOT_FOLDER {storage_root_folder}"))?;
            if Path::new(&packs_dir).is_file() {
                builder::catalog::import(&pool, &packs_dir).await
            } else {
                builder::catalog::build(&pool, &storage, &packs_dir).await
            }
        }
        Command::Pulse => catalyst::comms::pulse::run_from_env().await,
        Command::Explore {
            command: ExploreCommand::ImportLikes { database_url, file },
        } => {
            use std::io::BufRead;
            tracing_subscriber::fmt().with_target(false).init();
            let pool = db::connect_pool(
                &database_url,
                &db::PoolSettings {
                    max_connections: 2,
                    ..Default::default()
                },
            )
            .await
            .context("connecting to DATABASE_URL")?;
            db::bootstrap(&pool).await?;
            let reader: Box<dyn BufRead> = if file == "-" {
                Box::new(std::io::BufReader::new(std::io::stdin()))
            } else {
                Box::new(std::io::BufReader::new(
                    std::fs::File::open(&file).with_context(|| format!("opening {file}"))?,
                ))
            };
            let out =
                explore::places::interactions::import_upstream_likes(&pool, reader.lines()).await?;
            println!("{}", serde_json::to_string(&out)?);
            Ok(())
        }
        Command::Communities {
            command:
                CommunitiesCommand::Mirror {
                    database_url,
                    upstream_url,
                    thumbnail_url,
                    grace_secs,
                },
        } => {
            use catalyst::social::communities::mirror;
            tracing_subscriber::fmt().with_target(false).init();
            let cfg = mirror::MirrorConfig {
                source: explore::MirrorSource::checked(&upstream_url, &[])
                    .context("COMMUNITIES_UPSTREAM_URL")?,
                thumbnail_base: explore::MirrorSource::checked(&thumbnail_url, &[])
                    .context("COMMUNITIES_MIRROR_THUMBNAIL_URL")?,
                interval: std::time::Duration::from_secs(grace_secs),
            };
            let pool = db::connect_pool(
                &database_url,
                &db::PoolSettings {
                    max_connections: 2,
                    ..Default::default()
                },
            )
            .await
            .context("connecting to DATABASE_URL")?;
            db::bootstrap(&pool).await?;
            let client = http::Outbound::new(http::HttpClientCfg::default())
                .context("building the HTTP client")?;
            let cancel = tokio_util::sync::CancellationToken::new();
            let on_signal = cancel.clone();
            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    on_signal.cancel();
                }
            });
            let out = mirror::run_pass(
                &client,
                &pool,
                &cfg,
                std::time::Duration::from_secs(grace_secs),
                &cancel,
            )
            .await?;
            println!("{}", serde_json::to_string(&out)?);
            if !out.complete {
                anyhow::bail!("communities mirror pass incomplete (the next run resumes it)");
            }
            Ok(())
        }
        Command::Posture {
            command: PostureCommand::Show { name },
        } => {
            print!("{}", posture::show(posture::Posture::parse(&name)?));
            Ok(())
        }
        Command::Posture {
            command: PostureCommand::Env { name, keys },
        } => {
            print!(
                "{}",
                posture::env_file(posture::Posture::parse(&name)?, &keys)?
            );
            Ok(())
        }
        Command::Secrets {
            command:
                SecretsCommand::Init {
                    serve_file,
                    pulse_file,
                    with_abgen,
                },
        } => {
            let outcome =
                ops::generated_secrets::init(&serve_file, pulse_file.as_deref(), with_abgen)?;
            if outcome.generated.is_empty() {
                println!(
                    "secrets init: every slot of {} present; nothing written",
                    serve_file.display()
                );
            } else {
                println!(
                    "secrets init: minted {} in {}",
                    outcome.generated.join(", "),
                    serve_file.display()
                );
            }
            if outcome.pulse_written {
                println!(
                    "secrets init: wrote {}",
                    pulse_file
                        .as_deref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default()
                );
            }
            Ok(())
        }
        Command::Storage {
            command:
                StorageCommand::Gc {
                    delete,
                    min_age_secs,
                    allow_empty_reference_set,
                    database_url,
                    storage_root_folder,
                    global_scenes_urn,
                },
        } => {
            tracing_subscriber::fmt().with_target(false).init();
            let pool = db::connect_pool(
                &database_url,
                &db::PoolSettings {
                    max_connections: 2,
                    ..Default::default()
                },
            )
            .await
            .context("connecting to DATABASE_URL")?;
            let storage = storage::Storage::from_config(Path::new(&storage_root_folder))
                .await
                .with_context(|| format!("opening STORAGE_ROOT_FOLDER {storage_root_folder}"))?;
            let globals: Vec<String> = global_scenes_urn
                .map(|r| r.split_whitespace().map(str::to_string).collect())
                .unwrap_or_default();
            let options = storage::gc::GcOptions {
                dry_run: !delete,
                min_age: std::time::Duration::from_secs(min_age_secs)
                    .max(storage::gc::MIN_ORPHAN_AGE),
                allow_empty_reference_set,
            };
            let report = storage::gc::collect(&pool, &storage, &globals, options).await?;
            println!("{}", serde_json::to_string(&report)?);
            Ok(())
        }
        Command::Bvimposters {
            command:
                BvimpostersCommand::Seed {
                    realm_cache_dir,
                    store_root,
                },
        } => {
            tracing_subscriber::fmt().with_target(false).init();
            let store =
                explore::filecache::FileCache::new(Path::new(&store_root).to_path_buf(), u64::MAX);
            store
                .init()
                .with_context(|| format!("opening the imposter store at {store_root}"))?;
            let counts = explore::imposters::seed::seed_into(&store, Path::new(&realm_cache_dir))?;
            println!(
                "imported {} skipped {} crc0 {} incomplete {}",
                counts.imported, counts.skipped, counts.crc0, counts.incomplete
            );
            Ok(())
        }
        Command::Bvimposters {
            command:
                BvimpostersCommand::Quarantine {
                    list,
                    store_root,
                    default_list,
                },
        } => {
            tracing_subscriber::fmt().with_target(false).init();
            let store =
                explore::filecache::FileCache::new(Path::new(&store_root).to_path_buf(), u64::MAX);
            store
                .init()
                .with_context(|| format!("opening the imposter store at {store_root}"))?;
            let path = list
                .or(default_list)
                .filter(|p| !p.trim().is_empty())
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| Path::new(&store_root).join("readthrough-quarantine.txt"));
            let quarantine =
                explore::imposters::quarantine_list::QuarantineList::load(path.clone());
            let counts = explore::imposters::quarantine_list::apply(&store, &quarantine);
            println!(
                "list {} keys {} renamed {} absent {} errors {}",
                path.display(),
                quarantine.len(),
                counts.renamed,
                counts.absent,
                counts.errors
            );
            Ok(())
        }
        Command::Builder {
            command:
                BuilderCommand::Seed {
                    base_url,
                    hashes,
                    storage_root_folder,
                },
        } => {
            tracing_subscriber::fmt().with_target(false).init();
            let storage = storage::Storage::from_config(Path::new(&storage_root_folder))
                .await
                .with_context(|| format!("opening STORAGE_ROOT_FOLDER {storage_root_folder}"))?;
            let http = http::Outbound::new(http::HttpClientCfg::default())
                .context("building the HTTP client")?;
            let stored = builder::catalog::seed(&storage, &http, &base_url, &hashes).await?;
            println!("seed: {stored} of {} files stored", hashes.len());
            Ok(())
        }
    }
}
// CI timing probe.
