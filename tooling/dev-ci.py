#!/usr/bin/env python3
"""Plan the Development environment and select frontend unit coverage, failing open to full coverage.

Whether a pull request changed anything it tests (the workflow's push.paths, so the jobs skip on the rest
and "CI gate" still reports), and whether it changed a macOS input (build, toolchain, dev stack). PRs run the
Rust tests on Linux; a PR with a macOS input gets a notice to add the ci:macos label, and main runs macOS.
A PR that changes only web source skips the Rust tests, unless a Rust source names a changed file.

  python3 tooling/dev-ci.py plan                     in a pull_request run (compares the merge commit's parents)
  python3 tooling/dev-ci.py plan --base origin/main  locally (compares HEAD with its merge base)
  python3 tooling/dev-ci.py --base SHA --head SHA   frontend unit selection (also reads BASE_SHA/GITHUB_SHA)
"""
import argparse
import importlib.util
import os
import json
import re
import sys
from pathlib import Path, PurePosixPath
import subprocess

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = 'development.yml'
# What the macOS Rust job itself runs (the dev stack, the ENet pin check and dev-test.sh's tests)
# and what changes how it builds (manifests, lockfiles, build scripts, toolchains, cargo config).
MACOS = ('tooling/dev*', 'tooling/catalyst-test*.sh', 'tooling/tests/test_catalyst_test_resources.py',
         'tooling/enet-provenance.py', 'tooling/tests/test_dev_regressions.py',
         'tooling/archipelago-fixtures.py', 'tooling/tests/test_archipelago_fixtures.py',
         'tooling/tests/test_enet_provenance.py', '**/Cargo.toml', '**/Cargo.lock', '**/build.rs',
         '**/rust-toolchain*', 'services/catalyst/.cargo/**', 'library/vendor/rusty_enet/**')
# Inputs whose Rust-job behaviour the Linux leg covers: web apps, other Python tooling and Rust/SQL
# source whose compiled code is the same on both platforms. Main pushes still run them on macOS.
LINUX_ONLY = ('apps/sites/**', 'apps/ui3/**',
              'tooling/*.py', 'tooling/tests/**', 'services/catalyst/**', 'library/**', 'schemas/**')
# Web source that the Rust tests do not compile. A PR that changes nothing else skips the Rust tests,
# unless a changed file is a Rust build file or a Rust source under RUST_ROOTS names it (an include).
WEB_ONLY = ('apps/sites/', 'apps/ui3/', 'library/ui/')
RUST_ROOTS = ('services/catalyst', 'library', 'apps/dcl-scene-cli', 'schemas')
RUST_BUILD_FILE = re.compile(r'(?:^|/)(?:Cargo\.[^/]+|rust-toolchain[^/]*|[^/]+\.rs)$')
WEB_REFERENCE = re.compile(r'''(?:apps/(?:sites|ui3)|library/ui)((?:/[A-Za-z0-9_.@+-]+)*)(/?["'`])?''')


def load_ci_local():
    spec = importlib.util.spec_from_file_location('ci_local', Path(__file__).with_name('ci-local.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def needs_macos(changed, ci):
    macos = [ci.glob_regex(p) for p in MACOS]
    linux = [ci.glob_regex(p) for p in LINUX_ONLY]
    relevant = watched_changes(changed, ci)
    for path in relevant:
        if any(r.match(path) for r in macos) or not any(r.match(path) for r in linux):
            return True, f'macOS input changed: {path}'
    return False, 'Only Linux-covered inputs changed' if relevant else 'No watched inputs changed'


def rust_web_references(head):
    """The web paths that Rust sources name, comments included. A folder covers the files below it.

    A web root that ends a string is a path built from parts: that refuses the selection.
    """
    patterns = [f':(glob){root}/**/*.rs' for root in RUST_ROOTS]
    found = subprocess.run(['git', 'grep', '-I', '-l', '-z', '-E', 'apps/(sites|ui3)|library/ui', head, '--', *patterns],
                           cwd=ROOT, capture_output=True, text=True, timeout=60)
    if found.returncode not in (0, 1):
        raise ValueError('Rust source scan failed')
    references = set()
    for blob in filter(None, found.stdout.split('\0')):
        text = subprocess.check_output(['git', 'show', blob], cwd=ROOT, text=True, stderr=subprocess.PIPE, timeout=60)
        for match in WEB_REFERENCE.finditer(text):
            if not match.group(1):
                if match.group(2):
                    raise ValueError(f'Computed web path in {blob}')
                continue
            path = match.group()[:len(match.group()) - len(match.group(2) or '')]
            references.update((path, path.rstrip('.')))
    return references


def needs_rust(changed, head):
    """(Rust, reason): False only when every changed file is web source that no Rust source names.

    It reads every changed file but documentation, not only what the workflow watches.
    """
    relevant = list(changed)
    if not relevant or not all(path.startswith(WEB_ONLY) for path in relevant):
        return True, 'Not only web source changed'
    try:
        references = rust_web_references(head)
    except Exception as exc:
        return True, f'Rust source scan unavailable: {exc}'
    for path in relevant:
        if RUST_BUILD_FILE.search(path):
            return True, f'Rust build file changed: {path}'
        if any(path == reference or path.startswith(reference + '/') for reference in references):
            return True, f'A Rust source names a changed file: {path}'
    return False, 'Only web source changed that no Rust source names'


def watched_changes(changed, ci):
    """The changed paths the workflow tests: its push.paths, which pull requests no longer filter on."""
    changed = ci.non_documentation(changed)
    triggers = ci.workflow_paths(WORKFLOW, event='push', source=ROOT)
    return ci.matching_paths(changed, triggers)


def plan(event, base, head):
    """(macOS, reason, run, Rust, Rust reason).

    run is False only when a comparison succeeded and found nothing the workflow tests; Rust is False only
    when it found web source alone.
    """
    if event not in ('pull_request', 'local'):
        return True, f'{event} event runs every platform', True, True, f'{event} event runs the Rust tests'
    try:
        if event == 'local':
            base = subprocess.check_output(['git', 'merge-base', base, head], cwd=ROOT, text=True,
                                           stderr=subprocess.PIPE, timeout=60).strip()
        changed = subprocess.check_output(['git', 'diff', '--name-only', '--no-renames', '-z', base, head],
                                          cwd=ROOT, text=True, stderr=subprocess.PIPE, timeout=60).split('\0')
        changed = [path for path in changed if path]
        ci = load_ci_local()
        relevant = watched_changes(changed, ci)
        macos, reason = needs_macos(changed, ci)
        rust, rust_reason = needs_rust(ci.non_documentation(changed), head)
        return macos, reason, bool(relevant), rust, rust_reason
    except Exception as exc:
        return True, f'Comparison unavailable: {exc}', True, True, 'Comparison unavailable'


# Explicit backend implementation boundaries, not every file under catalyst.
# Shared types, configuration, manifests, schemas and unknown crates keep full CI.
BACKEND_CRATES = frozenset({
    'builder', 'camera-reel', 'chain-index', 'comms', 'content', 'db', 'explore',
    'fed', 'jobs', 'land-authz', 'price', 'shop', 'social', 'storage', 'testing', 'worlds',
})
FRONTEND_ROOTS = ('apps/ui3', 'apps/sites', 'library')
SOURCE_EXTENSIONS = ('ts', 'tsx', 'js', 'jsx', 'mts', 'mjs', 'cts', 'cjs', 'json')
BACKEND_REFERENCE = re.compile(r'services/catalyst(?:/[A-Za-z0-9_.@+-]+)*')


def backend_source(path):
    parts = PurePosixPath(path).parts
    if '..' in parts or not path.endswith('.rs'):
        return False
    if parts[:2] != ('services', 'catalyst'):
        return False
    if len(parts) >= 4 and parts[2] in ('src', 'tests'):
        return True
    return (len(parts) >= 5 and parts[2] in BACKEND_CRATES
            and parts[3] in ('src', 'tests'))


def ensure_commit(sha):
    if not re.fullmatch(r'[0-9a-f]{40}', sha or '') or sha == '0' * 40:
        raise ValueError('Missing comparison commit')
    found = subprocess.run(['git', 'cat-file', '-e', f'{sha}^{{commit}}'],
                           cwd=ROOT, capture_output=True, timeout=60).returncode == 0
    if not found:
        subprocess.run(['git', 'fetch', '--no-tags', '--depth=1', 'origin', sha],
                       cwd=ROOT, check=True, timeout=60)


def frontend_backend_references(head):
    """Inspect committed frontend sources and helpers, including source-reading tests.

    This deliberately is not an import/JavaScript parser. Literal paths (even in
    comments) count, directory references cover their subtree, and recognizable
    computed/split paths refuse the optimization rather than guessing a target.
    """
    for root in FRONTEND_ROOTS:
        kind = subprocess.check_output(['git', 'cat-file', '-t', f'{head}:{root}'],
                                       cwd=ROOT, text=True, timeout=60).strip()
        if kind != 'tree':
            raise ValueError(f'Frontend source root unavailable: {root}')
    patterns = [f':(glob){root}/**/*.{extension}'
                for root in FRONTEND_ROOTS for extension in SOURCE_EXTENSIONS]
    matches = subprocess.run(
        ['git', 'grep', '-a', '-l', '-z', '-F', '-e', 'services', head, '--', *patterns],
        cwd=ROOT, capture_output=True, text=True, timeout=60,
    )
    if matches.returncode not in (0, 1):
        raise ValueError('Frontend dependency scan failed')
    references = set()
    for blob in filter(None, matches.stdout.split('\0')):
        text = subprocess.check_output(['git', 'show', blob], cwd=ROOT, text=True, timeout=60)
        # join(root, "services", "catalyst", ...) and string concatenation do
        # not give us a complete literal path. Keep the full frontend suites.
        if (re.search(r'''services/?["'`]''', text)
                and re.search(r'''["'`]catalyst(?:/|["'`])''', text)):
            raise ValueError('Computed frontend backend dependency')
        for match in BACKEND_REFERENCE.finditer(text):
            path = match.group()
            tail = text[match.end():]
            if '..' in PurePosixPath(path).parts or (tail and tail[0] in '/${\\'):
                raise ValueError('Ambiguous frontend backend dependency')
            # A sentence-ending period is not part of a Rust filename. Keep
            # both interpretations conservative for directories with dots.
            references.add(path)
            references.add(path.rstrip('.'))
    return references


def web_plan(event, base, head):
    if event != 'pull_request':
        return {'run-web-unit-tests': True, 'reason': 'Full frontend units outside pull requests'}
    try:
        ensure_commit(base)
        ensure_commit(head)
        # Compare the actual PR base to its tested merge tree. --no-renames
        # exposes both paths of a rename, including a removed frontend input.
        changed = subprocess.check_output(
            ['git', 'diff', '--name-only', '--no-renames', '-z', base, head, '--'],
            cwd=ROOT, text=True, timeout=60,
        )
        changed = set(filter(None, changed.split('\0')))
        paths = set(load_ci_local().non_documentation(changed))
        if not paths:
            return {'run-web-unit-tests': False,
                    'reason': 'Documentation-only changes' if changed else 'No changed files'}
        backend_only = all(backend_source(path) for path in paths)
        if backend_only:
            references = frontend_backend_references(head)
            if any(path == reference or path.startswith(reference + '/')
                   for path in paths for reference in references):
                return {'run-web-unit-tests': True,
                        'reason': 'Frontend sources or helpers reference changed backend inputs'}
        return {
            'run-web-unit-tests': not backend_only,
            'reason': ('Known backend implementation changes only; contracts and smoke still run'
                       if backend_only else 'Shared, frontend, unknown or empty changes keep full frontend units'),
        }
    except (OSError, ValueError, TypeError, subprocess.SubprocessError) as exc:
        print(f'Conservative frontend selection: {exc}')
        return {'run-web-unit-tests': True, 'reason': 'Comparison unavailable; run full frontend units'}


def write_outputs(values):
    print(json.dumps(values, sort_keys=True))
    if os.environ.get('GITHUB_OUTPUT'):
        with open(os.environ['GITHUB_OUTPUT'], 'a') as output:
            for key, value in values.items():
                encoded = json.dumps(value) if isinstance(value, bool) else value
                output.write(f'{key}={encoded}\n')
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with open(os.environ['GITHUB_STEP_SUMMARY'], 'a') as summary:
            summary.write(f"Frontend unit tests: {values['reason']}.\n")


def platform_main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=('plan',))
    parser.add_argument('--base')
    parser.add_argument('--head', default='HEAD')
    args = parser.parse_args()
    event = os.environ.get('GITHUB_EVENT_NAME', 'local')
    if args.base is None and event == 'local':
        parser.error('--base is required outside GitHub Actions')
    macos, reason, run, rust, rust_reason = plan(event, args.base or 'HEAD^1', args.head)
    print(f'run={str(run).lower()} macos={str(macos).lower()}: {reason}')
    print(f'rust={str(rust).lower()}: {rust_reason}')
    # The label already adds macOS, and a pull request without Rust tests has none to add.
    if macos and run and rust and event == 'pull_request' and os.environ.get('CI_MACOS_LABEL') != 'true':
        print(f'::notice::{reason}. Add the ci:macos label to run the Rust tests on macOS before the merge.')
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with open(os.environ['GITHUB_STEP_SUMMARY'], 'a') as summary:
            summary.write(f'Tests run: {str(run).lower()}. macOS input changed: {str(macos).lower()} ({reason}).\n')
            summary.write(f'Rust tests run: {str(run and rust).lower()} ({rust_reason}).\n')
    if os.environ.get('GITHUB_OUTPUT'):
        with open(os.environ['GITHUB_OUTPUT'], 'a') as output:
            output.write(f'run={str(run).lower()}\nrust={str(rust).lower()}\n')


def web_main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--base', default=os.environ.get('BASE_SHA', ''))
    parser.add_argument('--head', default=os.environ.get('GITHUB_SHA', ''))
    args = parser.parse_args()
    write_outputs(web_plan(os.environ.get('GITHUB_EVENT_NAME', ''), args.base, args.head))


def main():
    if len(sys.argv) > 1 and sys.argv[1] == 'plan':
        platform_main()
    else:
        web_main()


if __name__ == '__main__':
    main()
