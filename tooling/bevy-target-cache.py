#!/usr/bin/env python3
"""Key and prune the Explorer's native Cargo cache for the Actions cache.

Cargo rebuilds a crate when its fingerprint changes: the compiler, an environment variable a
build script read (the CC and PKG_CONFIG_PATH the tool steps export), or an input file. A cache
saved under a key blind to one of those keeps stale fingerprints for good: every run restores
it, rebuilds the same crates and never saves, because the key still matches. The key here is
what the fingerprints depend on; a change misses it, restores the newest cache of the same
compiler (the fallback keys drop the sources, then the tools) and, on main, saves the rebuilt
tree.

  key --lane <name>   key and restore-keys for actions/cache/restore, into $GITHUB_OUTPUT:
                      bevy-native-v1-<lane>-<os>-<arch>-<rustc>-<tools>-<sources>
                        rustc    `rustc -vV`
                        tools    the environment the tool steps export for build scripts
                        sources  Cargo.lock, the workspace manifests, the Cargo configs (the
                                 workspace's and the repository root's) and the tracked sources
                                 of the path dependencies Cargo judges by file time: the patch
                                 series, the in-tree crates and the vendored trees
  prune <target-dir>  before the save: what the next run rebuilds anyway (the workspace crates'
                      artifacts and fingerprints, incremental state, final binaries, reports)
                      goes; dependency artifacts and the bevy-mtimes manifest stay. It removes
                      nothing outside <target-dir>: the save step names the Cargo home paths it
                      stores, and the rest of the Cargo home is not in the cache

Run `key` after the tool steps, so it sees their environment, and before the first Cargo
invocation; run `prune` after the last upload that reads the target directory.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / 'rendering/bevy-explorer/source'
# The tracked sources of the path dependencies Cargo judges by file time (tooling/bevy-mtimes.py).
# The patch series stand for the trees that tooling/patched-sources.py rebuilds.
VENDORED = ('rendering/bevy-explorer/patches', 'rendering/bevy-explorer/vendor', 'library/vendor/rusty_enet',
            'rendering/bevy-explorer/source/vendor/basisu')
SOURCE_FILES = ('Cargo.lock', 'Cargo.toml', '.cargo/config.toml')
# What build scripts record with rerun-if-env-changed and the tool steps export (CC and CXX by
# install-clang.sh, PKG_CONFIG_PATH by install-tools.sh, CEF_PATH by the CEF export, LIBCLANG_PATH
# and FFMPEG_DIR on Windows), plus the flags Cargo hashes into every unit. A tool step that
# exports a new variable for build scripts adds it here.
TRACKED_ENV = ('CC', 'CXX', 'AR', 'CFLAGS', 'CXXFLAGS', 'CMAKE', 'PKG_CONFIG_PATH', 'LIBCLANG_PATH', 'FFMPEG_DIR',
               'CEF_PATH', 'RUSTFLAGS', 'CARGO_BUILD_RUSTFLAGS')
KEEP_IN_PROFILE = ('build', 'deps', '.fingerprint')
KEEP_IN_TARGET = ('CACHEDIR.TAG', '.rustc_info.json')


def digest(*parts):
    hasher = hashlib.sha256()
    for part in parts:
        hasher.update(part.encode('utf-8') + b'\0')
    return hasher.hexdigest()[:16]


def rustc(env):
    return subprocess.check_output([env.get('RUSTC', 'rustc'), '-vV'], text=True)


def tools(env):
    return json.dumps({name: env[name] for name in TRACKED_ENV if name in env}, sort_keys=True)


def workspace(source=SOURCE):
    """The workspace members' package and target names, with dashes as underscores too, and their manifests.

    A name that a registry or git package of Cargo.lock also has is left out: an entry of that
    name can be the dependency's, and the prune removes only what is the workspace's.
    """
    metadata = json.loads(subprocess.check_output(
        ['cargo', 'metadata', '--no-deps', '--format-version', '1'], cwd=source, text=True))
    names, manifests = set(), []
    for package in metadata['packages']:
        names.add(package['name'])
        names.update(target['name'] for target in package['targets'])
        manifests.append(Path(package['manifest_path']))
    lock = (Path(source) / 'Cargo.lock').read_text(encoding='utf-8')
    shared = {name.replace('-', '_') for name in re.findall(r'^name = "([^"]+)"\nversion = "[^"]+"\nsource = ', lock, re.M)}
    names = {name for name in names if name.replace('-', '_') not in shared}
    return names | {name.replace('-', '_') for name in names}, sorted(manifests)


def sources(manifests, root, source):
    parts = [subprocess.check_output(['git', 'rev-parse', f'HEAD:{tree}'], cwd=root, text=True).strip()
             for tree in VENDORED]
    files = {source / name for name in SOURCE_FILES} | set(manifests)
    files |= set(source.glob('rust-toolchain*')) | set(root.glob('rust-toolchain*')) | {root / '.cargo/config.toml'}
    for path in sorted(files):
        if path.is_file():
            parts.append(f'{Path(os.path.relpath(path, root)).as_posix()}:{hashlib.sha256(path.read_bytes()).hexdigest()}')
    return digest(*parts)


def key(lane, env, root, source):
    """The exact key and its fallbacks: any sources, then any tools, of the same compiler on the same lane."""
    _, manifests = workspace(source)
    platform_name = f'{env.get("RUNNER_OS") or platform.system()}-{env.get("RUNNER_ARCH") or platform.machine()}'
    compiler = f'bevy-native-v1-{lane}-{platform_name}-{digest(rustc(env))}-'
    with_tools = f'{compiler}{digest(tools(env))}-'
    return f'{with_tools}{sources(manifests, root, source)}', [with_tools, compiler]


def owned(name, names):
    """A deps, build or .fingerprint entry of a workspace crate: <crate>-<hash>, with Cargo's lib prefix and extensions."""
    crate = name.split('.', 1)[0].rsplit('-', 1)[0]
    return crate in names or (crate.startswith('lib') and crate[3:] in names)


def remove(path, removed):
    if path.is_dir() and not path.is_symlink():
        shutil.rmtree(path)
    else:
        path.unlink()
    removed.append(path)


def is_profile(directory):
    return directory.is_dir() and any((directory / name).is_dir() for name in KEEP_IN_PROFILE)


def prune_profile(profile, names, removed):
    for entry in profile.iterdir():
        if entry.name not in KEEP_IN_PROFILE:
            remove(entry, removed)
    for name in KEEP_IN_PROFILE:
        if (profile / name).is_dir():
            for entry in (profile / name).iterdir():
                if owned(entry.name, names):
                    remove(entry, removed)


def prune_target(target, names, removed):
    """Profiles lie in target/<profile> and target/<triple>/<profile>; the rest is reports and scratch."""
    for entry in target.iterdir():
        if is_profile(entry):
            prune_profile(entry, names, removed)
        elif entry.is_dir() and any(is_profile(child) for child in entry.iterdir()):
            prune_target(entry, names, removed)
        elif entry.name not in KEEP_IN_TARGET:
            remove(entry, removed)


def size(directory):
    return sum(path.stat().st_size for path in directory.rglob('*') if path.is_file()) if directory.is_dir() else 0


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest='command', required=True)
    commands.add_parser('key').add_argument('--lane', required=True, help='the job: test, headless or clippy')
    commands.add_parser('prune').add_argument('target', type=Path, help="the explorer's Cargo target directory")
    args = parser.parse_args()
    if args.command == 'key':
        exact, fallbacks = key(args.lane, os.environ, ROOT, SOURCE)
        lines = [f'key={exact}', 'restore-keys<<KEYS', *fallbacks, 'KEYS']
        if os.environ.get('GITHUB_OUTPUT'):
            with open(os.environ['GITHUB_OUTPUT'], 'a', encoding='utf-8') as output:
                output.write('\n'.join(lines) + '\n')
            print(f'{exact}, falling back to {" then ".join(fallbacks)}')
        else:
            print('\n'.join(lines))
    else:
        names, _ = workspace()
        removed = []
        prune_target(args.target.resolve(), names, removed)
        print(f'{len(removed)} entries pruned; {size(args.target) / 2**20:.0f} MB left in {args.target}')
    return 0


if __name__ == '__main__':
    sys.exit(main())
