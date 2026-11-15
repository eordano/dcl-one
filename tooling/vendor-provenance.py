#!/usr/bin/env python3
"""Prove the Explorer's in-tree crates: each is the exact bytes of one public revision.

Each directory below rendering/bevy-explorer/vendor holds the files of one upstream revision, an
upstream.lock.json that names the repository and the revision, and upstream-files.sha256 with the
digest of every file of that revision. For each directory this checks, in order:

  - the lock file names the repository this tool records for the directory (ANCHORS);
  - the shipped files are the files of the manifest, byte for byte, with no file added;
  - the public repository has that revision, with the same file set and the same bytes.

The last step fetches Git objects and never checks out or runs fetched code; --offline skips it.
With no directory, every recorded directory is proved, and a directory without a record fails.
"""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path, PurePosixPath
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[1]
VENDOR = ROOT / "rendering/bevy-explorer/vendor"
# Independent review anchors: the repository each directory comes from. A lock file that names a
# different repository fails; changing a line here is the review of the new repository.
ANCHORS = {
    "bevy_dui": "https://github.com/robtfm/bevy_dui",
    "boimp": "https://github.com/robtfm/boimp",
    "rpc-rust": "https://github.com/decentraland/rpc-rust",
}

# The fetch and the manifest reader are the ones the ENet import is proved with.
_SPEC = importlib.util.spec_from_file_location("enet_provenance", Path(__file__).with_name("enet-provenance.py"))
_public = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(_public)
require, git_environment, parse_manifest, verify_tree = (
    _public.require, _public.git_environment, _public.parse_manifest, _public.verify_tree)


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def anchor_of(directory):
    """The recorded repository of a directory below rendering/bevy-explorer/vendor."""
    directory = Path(directory).resolve()
    require(directory.parent == VENDOR.resolve() and directory.name in ANCHORS,
            f"no recorded repository for this directory; ANCHORS in tooling/vendor-provenance.py names: {', '.join(sorted(ANCHORS))}")
    return ANCHORS[directory.name]


def verify_directory(source, anchor, repository=None, fetch=True):
    """Every shipped file is a file of the pinned revision of the anchored repository, byte for byte.

    `repository` is where the revision is fetched from when it is not the anchor itself."""
    pin = json.loads((source / "upstream.lock.json").read_text(encoding="utf-8"))
    require(pin.get("repository") == anchor,
            f"the lock file names the repository {pin.get('repository')!r}, the tool records {anchor!r}; "
            "review the new repository, then change ANCHORS in tooling/vendor-provenance.py")
    require(not pin.get("patches") and pin.get("manifests", {}).keys() == {"upstream-files.sha256"},
            "not an exact-bytes crate; a patched import is checked by its own verify.py")
    manifest_bytes = (source / "upstream-files.sha256").read_bytes()
    require(hashlib.sha256(manifest_bytes).hexdigest() == pin["manifests"]["upstream-files.sha256"],
            "manifest digest mismatch: upstream-files.sha256")
    manifest = parse_manifest(manifest_bytes)
    require(list(manifest) == sorted(manifest), "upstream-files.sha256 is not sorted")
    omitted = set(pin.get("omitted", {}).get("paths", []))
    require(omitted <= manifest.keys(), "omitted paths must be files of the pinned revision")
    expected = (manifest.keys() - omitted) | {"upstream.lock.json", "upstream-files.sha256"}
    package = PurePosixPath(pin.get("path_in_vcs", "."))
    # A standalone build of the package would write target/ beside its manifest.
    actual = {path.relative_to(source).as_posix() for path in source.rglob("*")
              if (path.is_file() or path.is_symlink())
              and (package / "target") not in PurePosixPath(path.relative_to(source).as_posix()).parents}
    require(actual == expected,
            f"file set mismatch; missing={sorted(expected - actual)}, extra={sorted(actual - expected)}")
    size = 0
    for relative in sorted(manifest.keys() - omitted):
        path = source / relative
        require(not path.is_symlink() and sha256(path) == manifest[relative], f"source digest mismatch: {relative}")
        size += path.stat().st_size
    license = pin["license"]
    files = license.get("files") or ([license] if license.get("path") else [])
    for entry in files:
        require(entry["path"] in manifest.keys() - omitted and entry["sha256"] == manifest[entry["path"]],
                f"license digest mismatch: {entry['path']}")
    cargo = tomllib.loads((source / package / "Cargo.toml").read_text(encoding="utf-8"))["package"]
    require((cargo.get("name"), cargo.get("version"), cargo.get("license"))
            == (pin["package"], pin["version"], license["spdx"]),
            "package name, version or license differs from the package manifest")
    if fetch:
        require(repository is not None or anchor.startswith("https://"),
                "upstream repository must be a public https URL")
        verify_tree(repository or anchor, pin["revision"], manifest)
    return len(manifest) - len(omitted), size, len(manifest)


def unrecorded(vendor=None, anchors=None):
    """Directories below the vendor directory with no anchor, and anchors with no directory."""
    vendor, anchors = vendor or VENDOR, ANCHORS if anchors is None else anchors
    present = {path.name for path in vendor.iterdir() if path.is_dir()}
    return sorted(present ^ anchors.keys())


def main(arguments=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("directories", nargs="*", metavar="DIRECTORY", type=Path,
                        help="a directory below rendering/bevy-explorer/vendor (default: every recorded one)")
    parser.add_argument("--offline", action="store_true",
                        help="check the lock file, the manifest and the shipped bytes; fetch nothing")
    options = parser.parse_args(arguments)
    directories = options.directories
    if not directories:
        require(not unrecorded(),
                f"rendering/bevy-explorer/vendor and ANCHORS in tooling/vendor-provenance.py differ: {', '.join(unrecorded())}")
        directories = [VENDOR / name for name in sorted(ANCHORS)]
    for directory in directories:
        try:
            anchor = anchor_of(directory)
            shipped, size, total = verify_directory(directory.resolve(), anchor, fetch=not options.offline)
            revision = json.loads((directory / "upstream.lock.json").read_text(encoding="utf-8"))["revision"]
        except (OSError, ValueError, KeyError) as error:
            raise ValueError(f"{directory}: {error}") from error
        proof = ("nothing fetched" if options.offline
                 else f"all {total} paths of the public revision fetched and hashed, no fetched code executed")
        print(f"Verified {directory.resolve().relative_to(ROOT).as_posix()} against its lock file: "
              f"{shipped} files, {size} bytes of {anchor}@{revision}; {proof}.")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, KeyError) as error:
        sys.exit(f"In-tree crate provenance: {error}")
