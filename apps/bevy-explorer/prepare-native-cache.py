#!/usr/bin/env python3
"""Keep only the pinned native toolchain on a disposable GitHub-hosted runner, and key the cache."""
import os
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parents[2]
# The path dependencies outside the engine workspace. The cache keeps their compiled crates.
VENDORED = ('rendering/bevy-explorer/bevy', 'library/vendor/rusty_enet')
KEY_FILE = ROOT / 'rendering/bevy-explorer/source/cache-key/.cargo/config.toml'


def output(*args):
    return subprocess.check_output(args, text=True).strip()


def vendored_trees():
    trees = [output('git', '-C', str(ROOT), 'rev-parse', f'HEAD:{path}') for path in VENDORED]
    if not all(re.fullmatch(r'[0-9a-f]{40,64}', tree) for tree in trees):
        raise RuntimeError('The vendored trees have no Git identity')
    return trees


def write_cache_key():
    # rust-cache saves a cache only when its key changes. The last part of the key is a hash of
    # Cargo.lock, the manifests and each .cargo/config.toml below the workspace: not the vendored
    # sources. This file puts them in that part, so a restore still finds the previous cache.
    # Cargo does not read the file, because no build runs in its folder.
    lines = [f'# {path} {tree}\n' for path, tree in zip(VENDORED, vendored_trees())]
    KEY_FILE.parent.mkdir(parents=True, exist_ok=True)
    KEY_FILE.write_text(''.join(lines), newline='\n')
    print(f'Native cache key input:\n{"".join(lines)}', end='', flush=True)


def prepare():
    # rust-cache hashes every installed compiler, including unused runner defaults.
    # Never remove a developer's toolchains or mutate a shared self-hosted runner.
    if os.environ.get('RUNNER_ENVIRONMENT') != 'github-hosted':
        raise RuntimeError('Toolchain cleanup requires a disposable GitHub-hosted runner')
    active = output('rustup', 'show', 'active-toolchain').split()[0]
    compiler = output('rustc', '-vV')
    fields = dict(line.split(': ', 1) for line in compiler.splitlines() if ': ' in line)
    release = fields.get('release', '')
    if not re.fullmatch(r'\d+\.\d+\.\d+', release) or active != f"{release}-{fields.get('host', '')}":
        raise RuntimeError('Install and select a numbered native Rust toolchain before cache preparation')
    installed = output('rustup', 'toolchain', 'list', '--quiet').splitlines()
    if active not in installed:
        raise RuntimeError('Active toolchain is absent from the installed toolchain list')
    for toolchain in installed:
        if toolchain != active:
            print(f'Removing unused toolchain: {toolchain}', flush=True)
            subprocess.run(['rustup', 'toolchain', 'uninstall', toolchain], check=True)
    # The active compiler, its components and cross-compilation targets stay intact.
    print(f'Native cache toolchain: {active}\n{compiler}', flush=True)
    write_cache_key()


if __name__ == '__main__':
    prepare()
