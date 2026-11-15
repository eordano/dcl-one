//! `export-static`: the scene as a content server would store it. A folder
//! of hash-named files plus the entity file, every scene's URN, and with
//! `--realmName` a realm `about` that points an explorer at the export.

use crate::build;
use crate::deploy;
use crate::ux::{self, TrySteps, UserError};
use crate::workspace::Workspace;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub struct ExportStaticOptions {
    pub dir: PathBuf,
    pub destination: PathBuf,
    /// Entity timestamp in unix milliseconds; now when absent.
    pub timestamp: Option<i64>,
    /// Write `<destination>/<name>/about`; needs `base_url`.
    pub realm_name: Option<String>,
    /// The realm's fixed comms adapter (ADR-180); `offline:offline` when absent.
    pub comms_adapter: Option<String>,
    /// Public URL the destination is served from, appended to every URN.
    pub base_url: Option<String>,
    pub skip_build: bool,
}

/// One content file of an exported scene.
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
pub struct ContentFile {
    /// The path in the scene, as the entity names it.
    pub file: String,
    /// The content hash: the name of the file in the destination.
    pub hash: String,
}

#[derive(Debug)]
pub struct Exported {
    pub scenes_urn: Vec<String>,
    pub entities: Vec<String>,
    /// The content files of each entity, in the order of `entities`.
    pub content_files: Vec<Vec<ContentFile>>,
    pub destination: PathBuf,
    /// The realm `about` file, when `--realmName` wrote one.
    pub about: Option<PathBuf>,
}

const DEFAULT_ADAPTER: &str = "offline:offline";

pub async fn export_static(opts: &ExportStaticOptions) -> Result<Exported> {
    let realm_name = opts
        .realm_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if let Some(name) = realm_name {
        if opts.base_url.is_none() {
            return Err(UserError::new(
                "--realmName needs --baseUrl: the about file names where the exported files are served",
                TrySteps::one("pass --baseUrl https://host/path/ (the public URL of --destination)"),
            )
            .into());
        }
        if !valid_realm_name(name) {
            return Err(UserError::new(
                format!("{name:?} is not a realm name"),
                TrySteps::one("use letters, digits, - and /, starting with a letter"),
            )
            .into());
        }
    }
    std::fs::create_dir_all(&opts.destination)
        .map_err(|e| ux::write_error(&opts.destination, e))?;
    let destination = dunce::canonicalize(&opts.destination)
        .with_context(|| format!("resolving {}", opts.destination.display()))?;
    let ws = Workspace::load(&opts.dir)?;
    for project in &ws.projects {
        let root = dunce::canonicalize(&project.root).unwrap_or_else(|_| project.root.clone());
        if root.starts_with(&destination) {
            return Err(UserError::new(
                format!(
                    "the destination {} holds the project {}: every file of the scene would be left out of the export",
                    destination.display(),
                    root.display()
                ),
                TrySteps::one("pass --destination with a folder for the export, for example --destination out"),
            )
            .into());
        }
    }
    let timestamp = opts.timestamp.unwrap_or_else(deploy::now_ms);
    let mut steps = ux::Steps::new(ws.projects.len() + usize::from(realm_name.is_some()));
    let mut scenes_urn = Vec::new();
    let mut entities = Vec::new();
    let mut content_files = Vec::new();
    for (i, project) in ws.projects.iter().enumerate() {
        if let Some(header) = ws.member_header(i) {
            ux::note(header);
        }
        if !opts.skip_build {
            build::build(&build::BuildOptions {
                built_in: false,
                dir: project.root.clone(),
                production: true,
                ignore_composite: false,
                custom_entry_point: false,
                skip_type_check: false,
                out_root: Some(project.root.join(build::RELEASE_OUT)),
                quiet: false,
            })
            .await?;
        }
        let mut prepared = deploy::prepare_checked(project, false)?;
        // A static export is addressed by URN, never by parcel: the entity
        // carries no pointers, as upstream's does.
        prepared.pointers.clear();
        let (entity_id, written) =
            export_scene(&mut prepared, &project.root, &destination, timestamp)?;
        let urn = scene_urn(&entity_id, opts.base_url.as_deref());
        steps.done(format!(
            "Exported {} \u{2014} {} files ({written} new) and the entity {}",
            deploy::scene_title(&prepared.metadata),
            prepared.files.len(),
            short(&entity_id)
        ));
        scenes_urn.push(urn);
        entities.push(entity_id);
        content_files.push(Vec::from_iter(prepared.files.iter().map(
            |(file, hash, _)| ContentFile {
                file: file.clone(),
                hash: hash.clone(),
            },
        )));
    }
    let mut about_file = None;
    if let Some(name) = realm_name {
        let adapter = opts
            .comms_adapter
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_ADAPTER);
        let about = static_about(name, &scenes_urn, adapter);
        let realm_dir = destination.join(name);
        std::fs::create_dir_all(&realm_dir).map_err(|e| ux::write_error(&realm_dir, e))?;
        let path = realm_dir.join("about");
        std::fs::write(&path, serde_json::to_vec_pretty(&about)?)
            .map_err(|e| ux::write_error(&path, e))?;
        steps.done(format!("Realm written {name}/about (comms {adapter})"));
        about_file = Some(path);
    }
    ux::note(format!("Destination: {}", destination.display()));
    for urn in &scenes_urn {
        ux::note_arrow(urn);
    }
    Ok(Exported {
        scenes_urn,
        entities,
        content_files,
        destination,
        about: about_file,
    })
}

/// Writes each file under its hash and the entity under its id; files
/// already there are kept. Returns the entity id and how many files landed.
///
/// A destination inside the project holds the previous export: those files
/// are dropped from `prepared` before anything is written or listed, so the
/// entity names the scene's files alone and a second run mints the same id.
pub fn export_scene(
    prepared: &mut deploy::Prepared,
    root: &Path,
    destination: &Path,
    timestamp: i64,
) -> Result<(String, usize)> {
    prepared.files.retain(|(rel, _, _)| {
        let inside = deploy::payload_path(root, rel).starts_with(destination);
        if inside {
            ux::note(format!("{rel} skipped: it lives inside the destination"));
        }
        !inside
    });
    let mut written = 0;
    for (_, hash, bytes) in &prepared.files {
        let dst = destination.join(hash);
        if dst.exists() {
            continue;
        }
        std::fs::write(&dst, bytes).map_err(|e| ux::write_error(&dst, e))?;
        written += 1;
    }
    let (entity_id, entity_bytes) = deploy::build_entity(prepared, timestamp)?;
    let dst = destination.join(&entity_id);
    std::fs::write(&dst, &entity_bytes).map_err(|e| ux::write_error(&dst, e))?;
    Ok((entity_id, written))
}

/// `urn:decentraland:entity:<id>`, plus `?=&baseUrl=<url>/` when the export
/// has a public address (the explorer fetches the files there).
pub fn scene_urn(entity_id: &str, base_url: Option<&str>) -> String {
    match base_url.map(str::trim).filter(|s| !s.is_empty()) {
        Some(base) => format!(
            "urn:decentraland:entity:{entity_id}?=&baseUrl={}/",
            base.trim_end_matches('/')
        ),
        None => format!("urn:decentraland:entity:{entity_id}"),
    }
}

/// The realm `about` an explorer reads: the export's scenes by URN, a fixed
/// comms adapter, and the configured catalyst for everything else.
pub fn static_about(realm_name: &str, scenes_urn: &[String], adapter: &str) -> serde_json::Value {
    let catalyst = crate::start::proxy::catalyst_base();
    serde_json::json!({
        "acceptingUsers": true,
        "bff": { "healthy": false, "publicUrl": format!("{catalyst}/bff") },
        "comms": { "healthy": true, "protocol": "v3", "fixedAdapter": adapter },
        "configurations": {
            "networkId": 0,
            "globalScenesUrn": [],
            "scenesUrn": scenes_urn,
            "realmName": realm_name
        },
        "content": { "healthy": true, "publicUrl": format!("{catalyst}/content") },
        "lambdas": { "healthy": true, "publicUrl": format!("{catalyst}/lambdas") },
        "healthy": true
    })
}

/// Letters, digits, `-` and `/`, starting with a letter: a path segment an
/// explorer can append `/about` to.
pub fn valid_realm_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '/')
}

/// `--timestamp`: unix milliseconds, or an RFC 3339 date.
pub fn parse_timestamp(raw: &str) -> std::result::Result<i64, String> {
    let raw = raw.trim();
    if let Ok(ms) = raw.parse::<i64>() {
        return Ok(ms);
    }
    chrono::DateTime::parse_from_rfc3339(raw)
        .map(|d| d.timestamp_millis())
        .map_err(|_| format!("expected unix milliseconds or an RFC 3339 date, got {raw:?}"))
}

fn short(hash: &str) -> String {
    match hash.char_indices().nth(12) {
        Some((i, _)) => format!("{}\u{2026}", &hash[..i]),
        None => hash.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deploy::TempTree;

    const SCENE_JSON: &str = r#"{"runtimeVersion":"7","main":"bin/index.js","display":{"title":"Static"},"scene":{"parcels":["0,0","1,0"],"base":"0,0"}}"#;

    fn scene(tag: &str) -> TempTree {
        let t = TempTree::new(tag);
        t.write("scene.json", SCENE_JSON);
        t.write("bin/index.js", "console.log('hi')");
        t.write("models/a.glb", "glb-bytes");
        t.write("src/index.ts", "not published");
        t
    }

    #[tokio::test]
    async fn writes_hash_named_files_the_entity_and_the_realm_about() {
        let t = scene("export-static");
        let out = TempTree::new("export-static-out");
        let exported = export_static(&ExportStaticOptions {
            dir: t.0.clone(),
            destination: out.0.clone(),
            timestamp: Some(1_700_000_000_000),
            realm_name: Some("my-realm".to_string()),
            comms_adapter: None,
            base_url: Some("https://cdn.example/static".to_string()),
            skip_build: true,
        })
        .await
        .unwrap();
        assert_eq!(exported.entities.len(), 1);
        let id = &exported.entities[0];
        assert_eq!(
            exported.scenes_urn,
            vec![format!(
                "urn:decentraland:entity:{id}?=&baseUrl=https://cdn.example/static/"
            )]
        );
        let entity_bytes = std::fs::read(out.0.join(id)).unwrap();
        assert_eq!(
            &dcl_auth::hash_bytes_v1(&entity_bytes),
            id,
            "the entity file is its own id"
        );
        let entity: serde_json::Value = serde_json::from_slice(&entity_bytes).unwrap();
        assert_eq!(entity["type"], "scene");
        assert_eq!(entity["version"], "v3");
        assert_eq!(entity["timestamp"], 1_700_000_000_000i64);
        assert_eq!(
            entity["pointers"],
            serde_json::json!([]),
            "a static export has no pointers"
        );
        assert_eq!(entity["metadata"]["display"]["title"], "Static");
        let content = entity["content"].as_array().unwrap();
        let files: Vec<&str> = content
            .iter()
            .map(|c| c["file"].as_str().unwrap())
            .collect();
        assert!(
            files.contains(&"bin/index.js") && files.contains(&"models/a.glb"),
            "{files:?}"
        );
        assert!(!files.contains(&"src/index.ts"), "{files:?}");
        for c in content {
            let hash = c["hash"].as_str().unwrap();
            let bytes = std::fs::read(out.0.join(hash)).unwrap();
            assert_eq!(
                dcl_auth::hash_bytes_v1(&bytes),
                hash,
                "{} is stored under its hash",
                c["file"]
            );
        }
        let about: serde_json::Value =
            serde_json::from_slice(&std::fs::read(out.0.join("my-realm/about")).unwrap()).unwrap();
        assert_eq!(about["configurations"]["realmName"], "my-realm");
        assert_eq!(
            about["configurations"]["scenesUrn"],
            serde_json::json!(exported.scenes_urn)
        );
        assert_eq!(about["comms"]["fixedAdapter"], "offline:offline");
        assert_eq!(about["healthy"], true);

        // A second run finds every file in place and rewrites only the entity.
        let before = std::fs::read_dir(&out.0).unwrap().count();
        let (again, written) = export_scene(
            &mut {
                let mut p =
                    deploy::prepare_checked(&crate::scene::Project::load(&t.0).unwrap(), false)
                        .unwrap();
                p.pointers.clear();
                p
            },
            &t.0,
            &out.0,
            1_700_000_000_000,
        )
        .unwrap();
        assert_eq!(&again, id);
        assert_eq!(written, 0);
        assert_eq!(std::fs::read_dir(&out.0).unwrap().count(), before);
    }

    /// `--destination` inside the project: the previous export's files are
    /// not the scene's. They stay out of the entity, so a second run mints
    /// the same id and writes nothing new.
    #[tokio::test]
    async fn a_destination_inside_the_project_exports_the_same_entity_twice() {
        let t = scene("export-static-inside");
        let opts = ExportStaticOptions {
            dir: t.0.clone(),
            destination: t.0.join("out"),
            timestamp: Some(1_700_000_000_000),
            realm_name: None,
            comms_adapter: None,
            base_url: None,
            skip_build: true,
        };
        let first = export_static(&opts).await.unwrap();
        let entries = std::fs::read_dir(t.0.join("out")).unwrap().count();
        let second = export_static(&opts).await.unwrap();
        assert_eq!(first.entities, second.entities);
        assert_eq!(
            std::fs::read_dir(t.0.join("out")).unwrap().count(),
            entries,
            "nothing new lands on a second run"
        );
        let entity: serde_json::Value = serde_json::from_slice(
            &std::fs::read(t.0.join("out").join(&second.entities[0])).unwrap(),
        )
        .unwrap();
        let files: Vec<&str> = entity["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["file"].as_str().unwrap())
            .collect();
        assert!(
            files.iter().all(|f| !f.starts_with("out/")),
            "the export never lists itself: {files:?}"
        );
        assert!(files.contains(&"bin/index.js"), "{files:?}");
    }

    /// The default flags: `--dir .` and `--destination .` in a project.
    #[tokio::test]
    async fn a_destination_that_holds_the_project_is_refused() {
        let t = scene("export-static-root");
        let before = std::fs::read_dir(&t.0).unwrap().count();
        for destination in [t.0.clone(), t.0.parent().unwrap().to_path_buf()] {
            let err = export_static(&ExportStaticOptions {
                dir: t.0.clone(),
                destination,
                timestamp: None,
                realm_name: None,
                comms_adapter: None,
                base_url: None,
                skip_build: true,
            })
            .await
            .unwrap_err();
            let shown = ux::render(&err, false, false);
            assert!(shown.contains("holds the project"), "{shown}");
            assert!(shown.contains("--destination out"), "{shown}");
        }
        assert_eq!(
            std::fs::read_dir(&t.0).unwrap().count(),
            before,
            "nothing is written"
        );
    }

    #[tokio::test]
    async fn realm_name_needs_a_base_url_and_a_sane_name() {
        let t = scene("export-static-realm");
        let out = TempTree::new("export-static-realm-out");
        let run = |realm: &str, base: Option<&str>| ExportStaticOptions {
            dir: t.0.clone(),
            destination: out.0.clone(),
            timestamp: None,
            realm_name: Some(realm.to_string()),
            comms_adapter: Some("ws-room:rooms.example/room".to_string()),
            base_url: base.map(str::to_string),
            skip_build: true,
        };
        let err = ux::render(
            &export_static(&run("ok", None)).await.unwrap_err(),
            false,
            false,
        );
        assert!(err.contains("--realmName needs --baseUrl"), "{err}");
        let err = ux::render(
            &export_static(&run("1bad name", Some("https://x/")))
                .await
                .unwrap_err(),
            false,
            false,
        );
        assert!(err.contains("is not a realm name"), "{err}");
        export_static(&run("nested/realm-1", Some("https://x")))
            .await
            .unwrap();
        let about: serde_json::Value =
            serde_json::from_slice(&std::fs::read(out.0.join("nested/realm-1/about")).unwrap())
                .unwrap();
        assert_eq!(about["comms"]["fixedAdapter"], "ws-room:rooms.example/room");
        assert!(
            about["configurations"]["scenesUrn"][0]
                .as_str()
                .unwrap()
                .ends_with("?=&baseUrl=https://x/"),
            "{about}"
        );
    }

    #[test]
    fn urn_realm_name_and_timestamp_rules() {
        assert_eq!(scene_urn("bafy", None), "urn:decentraland:entity:bafy");
        assert_eq!(scene_urn("bafy", Some("")), "urn:decentraland:entity:bafy");
        assert_eq!(
            scene_urn("bafy", Some("https://h/p/")),
            "urn:decentraland:entity:bafy?=&baseUrl=https://h/p/"
        );
        assert!(valid_realm_name("SdkStaticExport"));
        assert!(valid_realm_name("a/b-c1"));
        assert!(!valid_realm_name(""));
        assert!(!valid_realm_name("1a"));
        assert!(!valid_realm_name("a b"));
        assert!(!valid_realm_name("a.b"));
        assert_eq!(
            parse_timestamp(" 1700000000000 ").unwrap(),
            1_700_000_000_000
        );
        assert_eq!(
            parse_timestamp("2023-11-14T22:13:20Z").unwrap(),
            1_700_000_000_000
        );
        assert_eq!(
            parse_timestamp("2023-11-14T23:13:20+01:00").unwrap(),
            1_700_000_000_000
        );
        assert!(parse_timestamp("yesterday").is_err());
    }
}
