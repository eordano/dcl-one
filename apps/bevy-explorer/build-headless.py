#!/usr/bin/env python3
"""Build and collect the Explorer headless engine and scene sidecar."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys

APP = Path(__file__).resolve().parent
ROOT = APP.parents[1]
SOURCE = ROOT / 'rendering/bevy-explorer/source'


def run(*args):
    subprocess.run(args, cwd=SOURCE, check=True)


def headless(target=None):
    flags = ['--target', target] if target else []
    run('cargo', 'build', '--release', '--locked', *flags, '-p', 'dcl_deno_ipc')
    run('cargo', 'build', '--release', '--locked', *flags, '--bin', 'headless',
        '--no-default-features', '--features', 'headless,livekit')
    directory = SOURCE / 'target' / (target or '') / 'release'
    output = APP / 'dist/headless' / (target or 'native')
    output.mkdir(parents=True, exist_ok=True)
    suffix = '.exe' if 'windows' in (target or '') or os.name == 'nt' else ''
    for name in ('headless', 'dcl_deno_ipc'):
        shutil.copy2(directory / (name + suffix), output / (name + suffix))
    shutil.copy2(SOURCE / 'LICENSE', output / 'LICENSE')
    print(output)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--target', help='Rust target triple')
    args = parser.parse_args()
    # Cargo reads Bevy and the forked crates from trees that are not tracked: write the ones that
    # are missing or stale.
    subprocess.run((sys.executable, str(ROOT / 'tooling/patched-sources.py'), 'materialise', '--quiet'), check=True)
    headless(args.target)


if __name__ == '__main__':
    main()
