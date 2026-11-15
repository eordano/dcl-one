#!/usr/bin/env python3
"""Keep unchanged engine, vendored and protocol inputs fresh across CI checkouts.

Cargo decides whether a path dependency is fresh by file times, and a fresh checkout gives
every file the current time, so path dependencies in a CI cache would be rebuilt by every job
of every run, changed or not. Crates from git or crates.io are judged by version instead.

  record <target-dir>   after the build: each build input's git blob id and mtime, floored
                        to whole seconds, into every profile's .fingerprint directory, so the
                        manifest travels in the same cache archive as the artifacts it vouches for
  restore <target-dir>  after the checkout and the cache restore, before Cargo runs: each file
                        whose blob id still matches gets its recorded mtime back; a changed or
                        new file keeps the checkout's time, so Cargo rebuilds what depends on it

A tracked file's blob id comes from the index. The trees `tooling/patched-sources.py` writes are not
tracked, so their blob ids are those of the bytes on disk, stamp included. The Explorer's build guard
names each patch series as a directory, and Cargo then reads the directory's own time: a series is
an input too, and its id is that of the names it holds.

To Cargo this is a checkout that never moved. Floored times stay valid when a cache archive
keeps whole seconds only; in CI the tree is only ever written at checkout, before any build.
Without a manifest (a cold cache) restore changes nothing.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
TREE = 'rendering/bevy-explorer/bevy'
FORKS = 'rendering/bevy-explorer/forks'
PATCHES = 'rendering/bevy-explorer/patches'
TREES = (TREE, FORKS, PATCHES, 'rendering/bevy-explorer/vendor', 'library/vendor/rusty_enet',
         'rendering/bevy-explorer/source', 'library/protocol/proto')
# Written by tooling/patched-sources.py: the repository does not track them.
MATERIALISED = (TREE, FORKS)
# tooling/bevy-target-cache.py prunes the .fingerprint entries of workspace crates, <crate>-<hash>;
# `bevy` is no workspace crate, so the manifest survives its pruning.
MANIFEST = 'bevy-mtimes.json'
SECOND = 10**9


def blobs(root):
    """{path: id} for every input under TREES: a file's blob id from the index, or from the bytes of a tree git does
    not track, and the id of each patch series."""
    out = subprocess.check_output(['git', 'ls-files', '--stage', '-z', '--', *TREES], cwd=root)
    result = {}
    for record in filter(None, out.split(b'\0')):
        meta, path = record.split(b'\t', 1)
        result[path.decode('utf-8')] = meta.split()[1].decode('ascii')
    for tree in MATERIALISED:
        result.update(hashed(root, tree))
    result.update(listed(root))
    return result


def listed(root):
    """{directory/: id of the names it holds} for every patch series: its time changes when a name does."""
    result = {}
    for series in sorted((Path(root) / PATCHES).glob('*/series')):
        if series.is_dir():
            names = '\0'.join(sorted(os.listdir(series)))
            result[series.relative_to(root).as_posix() + '/'] = hashlib.sha1(names.encode('utf-8')).hexdigest()
    return result


def hashed(root, tree):
    """{path: blob id} of the bytes of every file under an untracked tree; no filter, so no line-ending conversion."""
    paths = sorted(Path(directory, name).relative_to(root).as_posix()
                   for directory, _dirs, names in os.walk(Path(root) / tree) for name in names)
    # git reads one path per line and unquotes a line that starts with a quote.
    paths = [path for path in paths if '\n' not in path and not path.startswith('"') and not (Path(root) / path).is_symlink()]
    if not paths:
        return {}
    out = subprocess.check_output(['git', 'hash-object', '--no-filters', '--stdin-paths'], cwd=root,
                                  input=''.join(path + '\n' for path in paths).encode('utf-8'))
    return dict(zip(paths, out.decode('ascii').split(), strict=True))


def fingerprint_dirs(target):
    """The .fingerprint directory of every profile: target/<profile> and target/<triple>/<profile>."""
    target = Path(target)
    if not target.is_dir():
        return []
    return sorted(p for p in [*target.glob('*/.fingerprint'), *target.glob('*/*/.fingerprint')] if p.is_dir())


def record(root, target):
    """Write the manifest into every profile; returns how many profiles got it."""
    files = {path: [blob, (Path(root) / path).stat().st_mtime_ns // SECOND * SECOND]
             for path, blob in blobs(root).items()}
    text = json.dumps({'trees': TREES, 'files': files}, sort_keys=True)
    directories = fingerprint_dirs(target)
    for directory in directories:
        (directory / MANIFEST).write_text(text)
    return len(directories)


def restore(root, target):
    """Give unchanged files their recorded mtime; returns (restored, left with the checkout's time)."""
    manifests = [d / MANIFEST for d in fingerprint_dirs(target) if (d / MANIFEST).is_file()]
    recorded = json.loads(manifests[0].read_text())['files'] if manifests else {}
    restored = left = 0
    for path, blob in blobs(root).items():
        entry = recorded.get(path)
        if entry and entry[0] == blob:
            os.utime(Path(root) / path, ns=(entry[1], entry[1]))
            restored += 1
        else:
            left += 1
    return restored, left


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('command', choices=('record', 'restore'))
    parser.add_argument('target', help="the explorer's Cargo target directory")
    args = parser.parse_args()
    if args.command == 'record':
        print(f'{", ".join(TREES)}: mtimes recorded in {record(ROOT, args.target)} profile(s) of {args.target}')
    else:
        restored, left = restore(ROOT, args.target)
        print(f'{", ".join(TREES)}: {restored} unchanged input(s) keep their cached mtime; {left} changed or unrecorded')
    return 0


if __name__ == '__main__':
    sys.exit(main())
