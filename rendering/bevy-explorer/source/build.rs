use std::{
    collections::HashSet,
    fs,
    io::Result,
    path::{Path, PathBuf},
    process::Command,
};

fn main() -> Result<()> {
    inputs(Path::new("src"));
    for path in ["build.rs", "Cargo.toml", "Cargo.lock"] {
        println!("cargo:rerun-if-changed={path}");
    }
    revision_inputs();

    let output = Command::new("git").args(["rev-parse", "HEAD"]).output()?;
    let git_hash = String::from_utf8(output.stdout).unwrap();
    println!("cargo:rustc-env=BEVY_EXPLORER_VERSION={git_hash}");

    modified()?;

    Ok(())
}

/// Names what holds the commit of `HEAD` as inputs: `HEAD` and, on a branch, the file of its ref.
/// Git keeps a ref in `packed-refs` until the next commit writes its file, so for a ref with no
/// file the inputs are `packed-refs` and the nearest directory above the file, which changes when
/// the file appears. Cargo reads every file below that directory, so until then a commit on a
/// branch beside this one runs the script too. `--git-path` answers for a linked worktree, whose
/// `HEAD` and refs are in two directories. Only paths that exist: Cargo runs the script every time
/// for one that does not.
fn revision_inputs() {
    let mut paths = vec![git_path("HEAD")];
    if let Some(file) = git(&["symbolic-ref", "-q", "HEAD"]).and_then(|name| git_path(&name)) {
        if !file.is_file() {
            paths.push(git_path("packed-refs"));
            paths.push(
                file.ancestors()
                    .find(|path| path.is_dir())
                    .map(Path::to_path_buf),
            );
        }
        paths.push(Some(file));
    }
    for path in paths.into_iter().flatten().filter(|path| path.exists()) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn git_path(name: &str) -> Option<PathBuf> {
    git(&["rev-parse", "--git-path", name]).map(PathBuf::from)
}

/// The first line a git command prints; none when it fails or prints nothing.
fn git(arguments: &[&str]) -> Option<String> {
    let output = Command::new("git").args(arguments).output().ok()?;
    let line = String::from_utf8(output.stdout)
        .ok()?
        .lines()
        .next()?
        .to_owned();
    (output.status.success() && !line.is_empty()).then_some(line)
}

fn modified() -> Result<()> {
    let diff = Command::new("git")
        .args(["diff", "--name-only", "-z"])
        .output()?;
    if !diff.status.success() {
        return Err(std::io::Error::other(format!("{}", diff.status)));
    }
    let mut modifications = String::from_utf8(diff.stdout)
        .unwrap()
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(ToOwned::to_owned)
        .collect::<HashSet<_>>();

    let diff = Command::new("git")
        .args(["diff", "--staged", "--name-only", "-z"])
        .output()?;
    if !diff.status.success() {
        return Err(std::io::Error::other(format!("{}", diff.status)));
    }
    modifications.extend(
        String::from_utf8(diff.stdout)
            .unwrap()
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(ToOwned::to_owned),
    );

    let change_to_build_rs = modifications.contains("build.rs");
    let change_to_workspace =
        modifications.contains("Cargo.toml") || modifications.contains("Cargo.lock");
    let changes_to_source = modifications
        .iter()
        .any(|path| path.starts_with("src/") || path.starts_with("crates/"));
    let modified = change_to_build_rs || change_to_workspace || changes_to_source;

    println!(
        "cargo:rustc-env=BEVY_EXPLORER_LOCAL_MODIFICATION={}",
        if modified { "true" } else { "false" }
    );
    Ok(())
}

/// Names every file below `directory` as an input. Files, not directories: a fresh checkout gives a
/// directory a new time, and under `src` CI restores the times of unchanged files only.
fn inputs(directory: &Path) {
    for path in entries(directory) {
        if path.is_dir() {
            inputs(&path);
        } else {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
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
