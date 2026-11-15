use std::{
    env, fs,
    path::{Path, PathBuf},
};

/// Fails the build of every package that depends on this one while a patched third-party tree is
/// not the one its tracked inputs describe. Cargo compiles the trees as they are on disk.
fn main() {
    println!("cargo:rerun-if-env-changed=DCL_ONE_ROOT");
    // Outside the repository there is no patched tree to compare.
    let Some(root) = env::var_os("DCL_ONE_ROOT") else {
        return;
    };
    let Some(inputs) = patched_sources_current(Path::new(&root)) else {
        // Cargo runs a failed script again, so this one names no input.
        eprintln!("patched sources are stale: run python3 tooling/patched-sources.py materialise");
        std::process::exit(1);
    };
    for path in inputs {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

/// The inputs to watch when every patched third-party tree was written from its tracked
/// `source.toml` and patch series; none when one was not. `tooling/patched-sources.py` writes the
/// checksum of those inputs into `.patched-stamp.json` at the top of the tree: a CRC-32 over, for
/// each file in path order, the path, a zero byte, the length as 8 little-endian bytes and the
/// bytes. Current when the repository tracks no such source.
///
/// A series is named as a directory, so a patch added to it runs this script again. Cargo reads
/// the time of the directory too; `tooling/bevy-mtimes.py` keeps it across CI checkouts.
fn patched_sources_current(root: &Path) -> Option<Vec<PathBuf>> {
    let explorer = root.join("rendering/bevy-explorer");
    let table = crc32_table();
    let mut inputs = Vec::new();
    let mut current = true;
    for source in entries(&explorer.join("patches")) {
        let manifest = source.join("source.toml");
        if !manifest.is_file() {
            continue;
        }
        let series = source.join("series");
        let mut files = Vec::new();
        for path in entries(&series) {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name.ends_with(".patch") && path.is_file() {
                files.push((format!("series/{name}"), path));
            }
        }
        files.push(("source.toml".to_owned(), manifest.clone()));
        let mut crc = !0u32;
        for (name, path) in &files {
            let data = fs::read(path).ok()?;
            for part in [
                name.as_bytes(),
                &[0],
                &(data.len() as u64).to_le_bytes(),
                &data,
            ] {
                for byte in part {
                    crc = table[((crc ^ u32::from(*byte)) & 0xff) as usize] ^ (crc >> 8);
                }
            }
        }
        let tree = match source.file_name() {
            Some(name) if name != "bevy" => explorer.join("forks").join(name),
            _ => explorer.join("bevy"),
        };
        let stamp = tree.join(".patched-stamp.json");
        let expected = format!("\"crc32\": \"{:08x}\"", !crc);
        current &= fs::read_to_string(&stamp).is_ok_and(|stamp| stamp.contains(&expected));
        inputs.push(manifest);
        if series.is_dir() {
            inputs.push(series);
        }
        inputs.push(stamp);
    }
    current.then_some(inputs)
}

/// The paths in a directory, in name order; none when it cannot be read.
fn entries(directory: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<_> = fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .collect();
    entries.sort();
    entries
}

fn crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    for (index, slot) in table.iter_mut().enumerate() {
        let mut value = index as u32;
        for _ in 0..8 {
            value = if value & 1 == 1 {
                0xedb8_8320 ^ (value >> 1)
            } else {
                value >> 1
            };
        }
        *slot = value;
    }
    table
}
