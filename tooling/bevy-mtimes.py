#!/usr/bin/env python3
"""Keep unchanged engine, vendored and protocol inputs fresh across CI checkouts.

Cargo decides whether a path dependency is fresh by file times, and a fresh checkout gives
every file the current time, so path dependencies in a CI cache would be rebuilt by every job
of every run, changed or not. Crates from git or crates.io are judged by version instead.

  record <target-dir>   after the build: each build input's git blob id and mtime, and each
                        directory's mtime with the id of the input names it holds, into every
                        profile's .fingerprint directory, so the manifest travels in the same
                        cache archive as the artifacts it vouches for
  restore <target-dir>  after the checkout and the cache restore, before Cargo runs: each file
                        whose blob id still matches gets its recorded mtime back, and so does each
                        directory that holds the same input names; a changed or new file keeps
                        the checkout's time, so Cargo rebuilds what depends on it

A tracked file's blob id comes from the index. The trees `tooling/patched-sources.py` writes are not
tracked, so their blob ids are those of the bytes on disk, stamp included. The Explorer's build guard
names each patch series as a directory, and Cargo then reads the directory's own time: a series is
an input too, and its id is that of the names it holds.

To Cargo this is a checkout that never moved. Times are kept to the nanosecond: for a package
whose build script names no input, Cargo stores the time of the newest file as it reads it, so a
rounded time is a change, and it makes many files equally new, where the one Cargo names
depends on the order of a directory listing. A build script that names a directory makes Cargo
read the time of every directory below it, and a checkout writes those too. In CI the tree is
only ever written at checkout, before any build. Without a manifest (a cold cache) restore
changes nothing.
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
# tooling/bevy-target-cache.py keeps every .fingerprint directory, so the manifest stays in the cache.
MANIFEST = 'bevy-mtimes.json'


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


def directories(paths):
    """{directory/: id of the input names it holds} for every directory of TREES above an input in `paths`.
    A patch series is in `paths` already, with the id of every name on disk."""
    names = {}
    for path in paths:
        parts = path.rstrip('/').split('/')
        for depth in range(1, len(parts)):
            names.setdefault('/'.join(parts[:depth]) + '/', set()).add(parts[depth])
    return {directory: hashlib.sha1('\0'.join(sorted(held)).encode('utf-8')).hexdigest()
            for directory, held in names.items()
            if directory not in paths and any(directory.startswith(tree + '/') for tree in TREES)}


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
    inputs = blobs(root)
    files, folders = ({path: [name, (Path(root) / path).stat().st_mtime_ns] for path, name in group.items()}
                      for group in (inputs, directories(inputs)))
    text = json.dumps({'trees': TREES, 'files': files, 'directories': folders}, sort_keys=True)
    profiles = fingerprint_dirs(target)
    for profile in profiles:
        (profile / MANIFEST).write_text(text)
    return len(profiles)


def restore(root, target):
    """Give unchanged files, and directories that hold the same names, their recorded mtime; returns the files
    (restored, left with the checkout's time)."""
    manifests = [d / MANIFEST for d in fingerprint_dirs(target) if (d / MANIFEST).is_file()]
    manifest = json.loads(manifests[0].read_text()) if manifests else {}
    inputs = blobs(root)
    restored = left = 0
    for path, blob in inputs.items():
        entry = manifest.get('files', {}).get(path)
        if entry and entry[0] == blob:
            os.utime(Path(root) / path, ns=(entry[1], entry[1]))
            restored += 1
        else:
            left += 1
    # A manifest from before directories were recorded has none: they keep the checkout's time.
    for path, names in directories(inputs).items():
        entry = manifest.get('directories', {}).get(path)
        if entry and entry[0] == names:
            os.utime(Path(root) / path, ns=(entry[1], entry[1]))
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
        print(f'{", ".join(TREES)}: {restored} unchanged input(s) and the directories with the same names keep their cached mtime; {left} changed or unrecorded')
    return 0


if __name__ == '__main__':
    sys.exit(main())
