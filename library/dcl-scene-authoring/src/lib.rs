//! The filesystem-free core of the SDK7 scene toolchain: composite
//! normalization, jsonSchema CRDT encoding, native `main.crdt` and
//! `entity-names.ts` generation, the production form of the loader stub.
//! Everything here reads scene files through [`SceneFiles`] and touches
//! neither `std::fs` nor the network, so the same code serves `dcl-scene-cli`
//! on the desktop and a browser web worker on `wasm32-unknown-unknown`.

pub mod composite_norm;
pub mod crdt_gen;
pub mod entity_names;
pub mod jsjson;
pub mod schema_crdt;

use std::collections::BTreeMap;

/// A composite larger than this is refused rather than parsed; the native
/// walker applies the cap before a path ever reaches [`SceneFiles::list`].
pub const COMPOSITE_FILE_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// A scene's files as the authoring core sees them: relative, forward-slash
/// paths under the project root, and their bytes. Implementations prune what
/// the native walker prunes (see [`is_pruned_dir`]) and may drop files they
/// refuse to load; `find_composites` re-applies the name rules so an
/// in-memory map that ships `node_modules` still behaves like the disk walk.
pub trait SceneFiles {
    /// Every file under the root, in any order.
    fn list(&self) -> Vec<String>;
    /// The bytes of one listed file; None when it is missing or unreadable.
    fn read(&self, rel: &str) -> Option<Vec<u8>>;
}

impl SceneFiles for BTreeMap<String, Vec<u8>> {
    fn list(&self) -> Vec<String> {
        self.keys().cloned().collect()
    }

    fn read(&self, rel: &str) -> Option<Vec<u8>> {
        self.get(rel).cloned()
    }
}

/// Directories the composite walk never enters: dot-entries and the three
/// build/dependency trees.
pub fn is_pruned_dir(name: &str) -> bool {
    name.starts_with('.') || matches!(name, "node_modules" | "bin" | "dist")
}

/// Is `rel` a composite the walk would have found: a `.composite` file, not
/// dot-prefixed, with no pruned directory on its path?
pub fn is_composite_path(rel: &str) -> bool {
    let mut segments = rel.split('/').peekable();
    while let Some(segment) = segments.next() {
        let is_file = segments.peek().is_none();
        if is_file {
            return segment.ends_with(".composite") && !segment.starts_with('.');
        }
        if is_pruned_dir(segment) {
            return false;
        }
    }
    false
}

/// Every composite the scene ships, ordered like the native walk sorted its
/// paths (component by component, so `a/x` sorts before `a-b/x`); later
/// entries override earlier ones wherever the core instances them.
pub fn find_composites(files: &dyn SceneFiles) -> Vec<String> {
    let mut out: Vec<String> = files
        .list()
        .into_iter()
        .filter(|rel| is_composite_path(rel))
        .collect();
    out.sort_by(|a, b| a.split('/').cmp(b.split('/')));
    out
}

/// A composite's text, or why it could not be loaded.
pub(crate) fn read_composite_text(files: &dyn SceneFiles, rel: &str) -> Result<String, String> {
    let bytes = files
        .read(rel)
        .ok_or_else(|| format!("reading {rel}: not readable"))?;
    String::from_utf8(bytes).map_err(|e| format!("reading {rel}: {e}"))
}

/// The loader stub (`main` of a built scene) without its comment lines,
/// blank lines and indentation, for a production build. `dcl-scene-cli` and
/// `dcl-scene-build` both write it, so their outputs agree. Nothing is
/// renamed and no line is joined: the chunks are direct-eval'd inside this
/// scope, so its identifiers are part of the contract, and the statements
/// rely on their line ends.
pub fn compact_loader(stub: &str) -> String {
    let mut out = String::with_capacity(stub.len());
    for line in stub.lines().map(str::trim) {
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scene(paths: &[&str]) -> BTreeMap<String, Vec<u8>> {
        paths.iter().map(|p| (p.to_string(), Vec::new())).collect()
    }

    #[test]
    fn walk_rules_match_the_native_walker() {
        let files = scene(&[
            "main.composite",
            "assets/scene/main.composite",
            "assets/scene/.hidden.composite",
            "node_modules/pkg/x.composite",
            "bin/x.composite",
            "dist/x.composite",
            ".dcl-one/x.composite",
            "assets/other.json",
        ]);
        assert_eq!(
            find_composites(&files),
            vec!["assets/scene/main.composite", "main.composite"]
        );
    }

    #[test]
    fn ordering_is_per_path_component() {
        let files = scene(&["a-b/x.composite", "a/x.composite"]);
        assert_eq!(
            find_composites(&files),
            vec!["a/x.composite", "a-b/x.composite"]
        );
    }
}
