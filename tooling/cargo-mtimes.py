#!/usr/bin/env python3
"""Keep unchanged tracked files and folders fresh for Cargo across CI checkouts.

Cargo decides whether a workspace crate is fresh by file times, and a fresh checkout gives
every file the current time, so compiled workspace crates from a cache would be rebuilt by
every run. A build script that watches a folder reads the folder's own time too.

  record <target-dir>   after the build: each tracked file's Git blob id and each tracked
                        folder's Git tree id, with its mtime floored to whole seconds, into
                        <target-dir>/cargo-mtimes.json, so the manifest travels in the same
                        cache archive as the artifacts it vouches for
  restore <target-dir>  after the checkout and the cache restore, before Cargo runs: a file
                        whose blob id still matches, or a folder whose tree id still matches,
                        gets its recorded mtime back; a changed or new one keeps the
                        checkout's time, so Cargo rebuilds what depends on it

A folder's tree id changes when any file below it changes, so a watched folder with one
changed file keeps the checkout's time. Without a manifest restore changes nothing.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
MANIFEST = 'cargo-mtimes.json'
SECOND = 10**9


def entries(root):
    """{path: object id} for every tracked file and folder of the checked-out commit."""
    out = subprocess.check_output(['git', 'ls-tree', '-r', '-t', '-z', 'HEAD'], cwd=root)
    result = {}
    for record in filter(None, out.split(b'\0')):
        meta, path = record.split(b'\t', 1)
        mode, kind, oid = meta.split()
        # A submodule entry is a commit, not content of this checkout; a link's time is its own.
        if kind in (b'blob', b'tree') and mode != b'120000':
            result[path.decode('utf-8')] = oid.decode('ascii')
    return result


def record(root, target):
    """Write the manifest; returns how many entries it holds."""
    recorded = {}
    for path, oid in entries(root).items():
        try:
            recorded[path] = [oid, (Path(root) / path).stat().st_mtime_ns // SECOND * SECOND]
        except FileNotFoundError:
            pass
    Path(target).mkdir(parents=True, exist_ok=True)
    (Path(target) / MANIFEST).write_text(json.dumps({'entries': recorded}, sort_keys=True))
    return len(recorded)


def restore(root, target):
    """Give unchanged entries their recorded mtime; returns (restored, left with the checkout's time)."""
    manifest = Path(target) / MANIFEST
    recorded = json.loads(manifest.read_text())['entries'] if manifest.is_file() else {}
    restored = left = 0
    for path, oid in entries(root).items():
        entry = recorded.get(path)
        if entry and entry[0] == oid and (Path(root) / path).exists():
            os.utime(Path(root) / path, ns=(entry[1], entry[1]))
            restored += 1
        else:
            left += 1
    return restored, left


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('command', choices=('record', 'restore'))
    parser.add_argument('target', help='the Cargo target directory that the cache keeps')
    args = parser.parse_args()
    if args.command == 'record':
        print(f'file times recorded for {record(ROOT, args.target)} tracked file(s) and folder(s) in {args.target}')
    else:
        restored, left = restore(ROOT, args.target)
        print(f'{restored} unchanged file(s) and folder(s) keep their cached time; {left} changed or unrecorded')
    return 0


if __name__ == '__main__':
    sys.exit(main())
