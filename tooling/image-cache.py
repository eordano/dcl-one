#!/usr/bin/env python3
"""Plan isolated image caches from exact Nix identities."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess


PACKAGES = {
    'catalyst': ('catalyst-deps', 'catalyst-vendor'),
    'sites': ('sites-deps', 'sites-runtime-deps', 'sites-toolchain'),
    'apis': ('catalyst',),
    'sites-app': ('sites-app', 'sites-app-polygon-credits'),
}
# Compiled runtime closures restore under exact identities only; a stale entry
# would not spare a single build step.
RUNTIME = ('apis', 'sites-app')


def cache_plan(arch, event, ref, derivations):
    if arch not in ('amd64', 'arm64'):
        raise ValueError('Unsupported image architecture')
    pr = re.fullmatch(r'refs/pull/([1-9][0-9]*)/merge', ref)
    pr_scope = event == 'pull_request' and pr is not None
    scope = f'pr-{pr[1]}' if pr_scope else 'main'
    save = (event == 'push' and ref == 'refs/heads/main') or pr_scope
    result = {'save': str(save).lower()}
    for group, packages in PACKAGES.items():
        inputs = [(package, derivations[package]) for package in packages]
        if any(not re.fullmatch(r'/nix/store/[a-z0-9]+-[^/\n]+\.drv', drv) for _, drv in inputs):
            raise ValueError('Invalid Nix derivation identity')
        digest = hashlib.sha256(json.dumps(inputs, separators=(',', ':')).encode()).hexdigest()
        suffix = f'{group}-{arch}-{digest}'
        result[f'{group}-key'] = f'images-v2-{scope}-{suffix}'
        result[f'{group}-main-key'] = f'images-v2-main-{suffix}'
        result[f'{group}-main-prefix'] = (
            f'images-v2-main-{group}-{arch}-' if pr_scope and group not in RUNTIME else '')
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=('keys',))
    parser.add_argument('--arch', choices=('amd64', 'arm64'), required=True)
    args = parser.parse_args()
    system = {'amd64': 'x86_64-linux', 'arm64': 'aarch64-linux'}[args.arch]
    # Evaluate the flake once, not once per package. These are fixed package
    # names; no event or branch text is interpolated into the Nix expression.
    fields = ' '.join(f'"{package}" = packages."{package}".drvPath;'
                      for packages in PACKAGES.values() for package in packages)
    derivations = json.loads(subprocess.check_output(
        ['nix', 'eval', '--json', f'.#packages.{system}', '--apply', f'packages: {{ {fields} }}'],
        text=True))
    plan = cache_plan(args.arch, os.environ['GITHUB_EVENT_NAME'], os.environ['GITHUB_REF'], derivations)
    with Path(os.environ['GITHUB_OUTPUT']).open('a') as output:
        for key, value in plan.items():
            print(f'{key}={value}', file=output)
    print(json.dumps(plan, indent=2))


if __name__ == '__main__':
    main()
