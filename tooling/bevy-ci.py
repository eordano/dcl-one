#!/usr/bin/env python3
"""Select Explorer jobs from their inputs and validate image-producing runs."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = '.github/workflows/bevy-explorer-ci.yml'
SOURCE = 'rendering/bevy-explorer/source/'
SCENES_WORKFLOW = '.github/workflows/bevy-explorer-scenes.yml'
SCENE_SUITE = 'apps/bevy-explorer/scene-tests.txt'
SNAPSHOT_SCENES = ('52,-62', '52,-64', '52,-66')
PRODUCERS = {'web': 'Web engine, HUD and bridge scene', 'headless': 'Headless Smoke'}
LANES = ('clippy', 'test', 'headless', 'web')
# These jobs use larger runners; on a draft they wait for ready for review or this label.
HELD_ON_DRAFT = ('test', 'headless', 'web')
ENGINE_LABEL = 'ci:engine'
NATIVE_JOBS = {'Test Suite (ubuntu-24.04)', 'Test Suite (windows-2025)', 'Test Suite (macos-15)'}
GATEWAY_HUD_INPUTS = ('apps/ui3/src/play/lib/baseDomain.ts',
                      'library/explorer-core/engine/generated/serviceTable.ts')
# Sources the web HUD compiles from library/: the ones tooling/play-artifact.py fingerprints with it.
_ARTIFACT = importlib.util.spec_from_file_location('play_artifact', Path(__file__).with_name('play-artifact.py'))
_artifact = importlib.util.module_from_spec(_ARTIFACT)
_ARTIFACT.loader.exec_module(_artifact)
# The flake's tool packages (bevy-tools: protoc, taplo and wasm-pack for the Linux lanes; bevy-ffmpeg
# for the macOS Test Suite) and the action that delivers them; Windows, headless packaging and
# scenes run install-protoc.py instead.
TOOLS_INPUTS = ('flake.nix', 'flake.lock', 'infra/packages/default.nix', 'infra/packages/tools.nix', '.github/actions/bevy-explorer-tools')
INPUTS = ('rendering/bevy-explorer/source', 'rendering/bevy-explorer/bevy', 'library/vendor/rusty_enet', 'tooling/dev-regressions*', 'library/protocol/proto', 'apps/bevy-explorer',
          'infra/gateway', *GATEWAY_HUD_INPUTS, *TOOLS_INPUTS,
          'tooling/bevy-ci.py', 'tooling/hud_shared_inputs.py', 'tooling/ci_paths.py', 'tooling/bevy-mtimes.py', 'tooling/play-artifact.py', 'tooling/tests/test_play_*.py',
          'tooling/tests/test_bevy_*.py', 'tooling/tests/test_hud_shared_inputs.py', 'tooling/tests/hud_manifest_fixture.py', 'tooling/tests/hud_manifest_invalid_cases.json', 'tooling/tests/check_hud_manifest_nix.py', WORKFLOW, SCENES_WORKFLOW,
          '.github/workflows/bevy-explorer-images.yml',
          '.github/workflows/bevy-explorer-headless.yml',
          '.github/workflows/bevy-explorer-headless-build.yml',
          'tooling/ci-publication.py', '.cargo', 'rust-toolchain*')
# Web files that the Test Suite's harness tests read: the benchmark runner's
# (rendering/bevy-explorer/source/local-harness/benchmark/tests) and the web driver's
# (local-harness/web-driver), which loads the HUD's test commands into Chrome. A change to one is
# also an input of the Test Suite.
HARNESS_WEB_READS = ('rendering/bevy-explorer/source/deploy/web/engine/perf_stream.js',
                     'rendering/bevy-explorer/source/react-web/src/shell/localGuest.ts',
                     'library/explorer-core/lib/hudAutomation.ts')
IMAGES_INPUTS = (
    'flake.*',
    'rendering/bevy-explorer/source/**',
    'rendering/bevy-explorer/bevy/**',
    'apps/bevy-explorer/build-web.py',
    'apps/bevy-explorer/install-*',
    '.github/workflows/bevy-explorer-ci.yml',
    '.github/actions/bevy-explorer-tools/**',
    'tooling/fetch-play.py',
    'tooling/bevy-ci.py',
    'tooling/play-artifact.py',
    'tooling/hud_shared_inputs.py',
    'tooling/tests/hud_manifest_fixture.py',
    'tooling/tests/hud_manifest_invalid_cases.json',
    'tooling/tests/check_hud_manifest_nix.py',
    'tooling/tests/test_play_*.py',
    'rust-toolchain*',
    'infra/packages/**',
    'infra/containers/**',
    'services/catalyst/**',
    'schemas/**',
    'library/**',
    '.cargo/**',
    'apps/sites/**',
    'apps/ui3/**',
    'tooling/image-*',
    'tooling/ci-publication.py',
    'tooling/ci_paths.py',
    'tooling/tests/test_ci_publication.py',
    'tooling/tests/test_image_publish.py',
    '.github/workflows/apis-sites-images.yml',
)
# How the images are built and published, and what the fixed dependency hashes in infra/packages
# follow (manifests, lockfiles, toolchain). A PR builds the images only for these; main builds
# them for every input above. The architecture choice in apis-sites-images.yml reads a subset.
IMAGES_PACKAGING = (
    'flake.*', 'rust-toolchain*', 'infra/packages/**', 'infra/containers/**', '.cargo/**',
    'services/catalyst/rust-toolchain*', 'services/catalyst/Cargo.*', 'services/catalyst/**/Cargo.toml',
    'library/**/Cargo.toml', 'apps/dcl-scene-cli/Cargo.toml',
    'apps/sites/package.json', 'apps/sites/pnpm-lock.yaml', 'apps/sites/pnpm-workspace.yaml', 'apps/sites/.npmrc',
    'tooling/image-*', 'tooling/ci-publication.py', 'tooling/ci_paths.py', 'tooling/bevy-ci.py',
    'tooling/fetch-play.py', 'tooling/play-artifact.py', 'tooling/hud_shared_inputs.py',
    'tooling/tests/check_hud_manifest_nix.py',
    'tooling/tests/test_ci_publication.py', 'tooling/tests/test_image_publish.py',
    '.github/workflows/bevy-explorer-ci.yml', '.github/workflows/apis-sites-images.yml',
)
IMAGES_JOBS = {'APIs + Sites / build (amd64, ubuntu-26.04)',
               'APIs + Sites / build (arm64, ubuntu-26.04-arm)'}
HEADLESS_JOBS = {
    'Headless packages / build (macos-arm64, darwin-arm64, macos-15, aarch64-apple-darwin)',
    'Headless packages / build (macos-amd64, darwin-x64, macos-15, x86_64-apple-darwin)',
    'Headless packages / build (windows-amd64, win32-x64, windows-2025, x86_64-pc-windows-msvc)',
}
# Unknown app inputs remain conservative. Only named integration boundaries narrow work.
APP_INPUTS = {
    'README.md': (),
    'Dockerfile.web': ('web',),
    'nginx.conf': ('web',),
    'Dockerfile.headless': ('headless',),
    'image-smoke.py': ('web', 'headless'),
    'Makefile': ('web', 'headless'),
    # Each producer has its own build script; web edits must not rebuild headless.
    'build-web.py': ('web',),
    'install-web-tools.py': ('web',),
    'build-headless.py': ('headless',),
    'install-linux-deps.py': (*LANES, 'scenes'),
    'install-tools.sh': (*LANES, 'native-matrix'),
    'prepare-native-cache.py': ('clippy', 'test', 'headless', 'native-matrix', 'scenes'),
    'scene-tests.txt': ('scenes',),
}


def git(*args):
    return subprocess.check_output(['git', *args], cwd=ROOT, text=True, env={**os.environ, 'GIT_GLOB_PATHSPECS': '1'})


def tested_base(merge, pr_head, fallback):
    """GitHub tests a PR as `Merge <head> into <base tip>`; pull_request.base.sha goes stale when only main moves."""
    try:
        header = subprocess.run(['git', 'cat-file', 'commit', merge], cwd=ROOT, capture_output=True,
                                check=True, timeout=60).stdout.decode(errors='replace').split('\n\n', 1)[0]
    except (OSError, subprocess.SubprocessError):
        return fallback
    parents = [line.split()[1] for line in header.splitlines() if line.startswith('parent ')]
    return parents[0] if len(parents) == 2 and parents[1] == pr_head else fallback


def event_pull_request():
    try:
        pull_request = json.loads(Path(os.environ['GITHUB_EVENT_PATH']).read_text())['pull_request']
        return pull_request if isinstance(pull_request, dict) else {}
    except (KeyError, OSError, TypeError, ValueError):
        return {}


def event_pr_head():
    try:
        return event_pull_request()['head']['sha']
    except (KeyError, TypeError):
        return None


def hold_for_draft(result, pull_request):
    """A draft runs the jobs on standard runners; the jobs on larger runners wait until it is ready or labelled.

    An unreadable event holds nothing. The web job is held too, so the images that read its output wait with it.
    """
    try:
        draft = pull_request.get('draft') is True
        labelled = any(label.get('name') == ENGINE_LABEL for label in pull_request.get('labels') or [])
    except (AttributeError, TypeError):
        return result
    held = [lane for lane in HELD_ON_DRAFT if result.get(lane) is True]
    if not draft or labelled or not held:
        return result
    print(f'::notice::This draft does not run the engine jobs on larger runners ({", ".join(held)}). '
          f'Mark it ready for review, or add the {ENGINE_LABEL} label and push, to run them.')
    return {**result, **dict.fromkeys((*HELD_ON_DRAFT, 'native-matrix'), False), 'held': True,
            'reason': result['reason'] + '; draft: ' + ', '.join(held) + ' wait until it is ready for review'}


def api(path):
    return json.loads(subprocess.check_output(['gh', 'api', path], text=True, timeout=60))


def trusted_run(run, repo):
    return (run['status'] == 'completed' and run['conclusion'] == 'success'
            and run['head_branch'] == 'main' and run['event'] in ('push', 'workflow_dispatch')
            and run['head_repository']['full_name'] == repo and run['path'] == WORKFLOW
            and re.fullmatch(r'[0-9a-f]{40}', run['head_sha']))


def produced_artifacts(jobs):
    """Each product needs its own successful producer; explicit skips preserve its tags."""
    products = []
    for product, name in PRODUCERS.items():
        selected = [job for job in jobs if job['name'] == name]
        if len(selected) != 1:
            raise ValueError(f'Missing or duplicate Explorer artifact producer: {name}')
        job = selected[0]
        if job['status'] != 'completed' or job['conclusion'] not in ('success', 'skipped'):
            raise ValueError(f'Explorer artifact producer must succeed or explicitly skip: {name}')
        if job['conclusion'] == 'success':
            products.append(product)
    return products


def run_jobs(repo, run_id):
    data = api(f'repos/{repo}/actions/runs/{run_id}/jobs?per_page=100')
    if data['total_count'] != len(data['jobs']):
        raise ValueError('Incomplete source job list')
    return data['jobs']


def passed_lane(jobs, lane):
    if lane == 'test':
        selected = [job for job in jobs if job['name'].startswith('Test Suite (')]
        names = {job['name'] for job in selected}
        if names not in ({'Test Suite (ubuntu-24.04)'}, NATIVE_JOBS) or len(names) != len(selected):
            return False
    else:
        name = 'Clippy' if lane == 'clippy' else PRODUCERS[lane]
        selected = [job for job in jobs if job['name'] == name]
        if len(selected) != 1:
            return False
    return all(job['status'] == 'completed' and job['conclusion'] == 'success' for job in selected)


def main_baselines(repo):
    """A web-only pass cannot advance the baseline of unexecuted native jobs."""
    runs = api(f'repos/{repo}/actions/workflows/bevy-explorer-ci.yml/runs'
               '?branch=main&status=success&per_page=20')['workflow_runs']
    baselines = {}
    for run in runs:
        if not trusted_run(run, repo):
            continue
        jobs = run_jobs(repo, run['id'])
        for lane in LANES:
            if lane not in baselines and passed_lane(jobs, lane):
                baselines[lane] = run['head_sha']
        if len(baselines) == len(LANES):
            break
    return baselines


def ensure_commit(sha):
    if not sha or not re.fullmatch(r'[0-9a-f]{40}', sha) or sha == '0' * 40:
        raise ValueError('Missing comparison commit')
    found = subprocess.run(['git', 'cat-file', '-e', f'{sha}^{{commit}}'], cwd=ROOT,
                           capture_output=True).returncode == 0
    if not found:
        subprocess.run(['git', 'fetch', '--no-tags', '--depth=1', 'origin', sha], cwd=ROOT,
                       check=True, timeout=60)


def full_selection(reason):
    return {**dict.fromkeys((*LANES, 'native-matrix', 'scenes'), True), 'reason': reason}


def platform_input(path):
    """A source path whose change can behave differently on Windows or macOS."""
    path = path.removeprefix(SOURCE)
    return (path == 'Cargo.toml' or re.fullmatch(r'build\w*\.rs', path.rsplit('/', 1)[-1]) is not None
            or path.startswith(('.cargo/', 'rust-toolchain', 'local-harness/benchmark/', 'local-harness/web-driver/'))
            or re.search(r'(^|/)(windows|macos)(\.rs$|/)', path) is not None)


def locked_packages(rev):
    """The engine's locked (name, version, source) set at rev, or None when it cannot be read."""
    try:
        import tomllib
        lock = tomllib.loads(git('show', f'{rev}:{SOURCE}Cargo.lock'))
        return {(package['name'], package['version'], package.get('source')) for package in lock['package']}
    except (ImportError, ValueError, KeyError, TypeError, subprocess.CalledProcessError):
        return None


def changed_inputs(base, head, inputs):
    spec = importlib.util.spec_from_file_location('ci_paths', Path(__file__).with_name('ci_paths.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    changed = git('diff', '--name-only', '--no-renames', '-z', base, head, '--', *inputs).split('\0')
    removed = git('diff', '--name-only', '--no-renames', '--diff-filter=D', '-z',
                  base, head, '--', *inputs).split('\0')
    removed = module.matching_paths(removed, ('**/*.md', *('!' + path for path in module.PROSE_TREES)))
    return set(module.non_documentation(filter(None, changed))) | set(removed)


def select(base, head):
    ensure_commit(base)
    ensure_commit(head)
    shared = _artifact.shared_inputs.input_hashes(ROOT, head)
    bootstrap = False
    try:
        previous = _artifact.shared_inputs.input_hashes(ROOT, base)
    except _artifact.shared_inputs.ManifestError:
        previous, bootstrap = {}, True
    shared_paths = set(shared) | set(previous)
    paths = changed_inputs(base, head, (*INPUTS, *(':(literal)' + path for path in sorted(shared_paths | set(HARNESS_WEB_READS)))))
    selected = dict.fromkeys((*LANES, 'native-matrix', 'scenes'), False)
    if bootstrap:
        selected['web'] = True
    for path in paths:
        if path == SCENES_WORKFLOW:
            affected = ('scenes',)
        elif path in ('.github/workflows/bevy-explorer-headless.yml',
                      '.github/workflows/bevy-explorer-headless-build.yml', 'tooling/ci-publication.py'):
            affected = ('headless',)
        elif path == WORKFLOW:
            affected = (*LANES, 'native-matrix')
        elif path == '.github/workflows/bevy-explorer-images.yml':
            affected = ('web', 'headless')
        elif path in ('tooling/play-artifact.py', 'tooling/hud_shared_inputs.py'):
            affected = ('web',)
        elif path in TOOLS_INPUTS or path.startswith('.github/actions/bevy-explorer-tools/'):
            # The Linux lanes compile with the flake's protoc and build the web engine with its
            # wasm-pack; the macOS Test Suite, in the native matrix, links the flake's ffmpeg; scenes
            # install protoc with install-protoc.py.
            affected = (*LANES, 'native-matrix')
        elif path.startswith(('tooling/tests/test_bevy_', 'tooling/tests/test_play_')) or path in (
                'tooling/tests/test_hud_shared_inputs.py', 'tooling/tests/hud_manifest_fixture.py',
                'tooling/tests/hud_manifest_invalid_cases.json', 'tooling/tests/check_hud_manifest_nix.py'):
            # The always-run source job executes these tests; they aren't build inputs.
            affected = ()
        elif path.startswith('infra/gateway/') or path in GATEWAY_HUD_INPUTS:
            # The Linux Test Suite compares Explorer and HUD routing with the nginx map.
            affected = ('test',)
        elif path in HARNESS_WEB_READS:
            # The web lane builds and tests these files; the Test Suite runs the harness tests,
            # which read them (the sender the benchmark runner's receiver must accept, the guest's
            # root-key name its reset page keeps, the HUD commands the web driver gives real input).
            affected = ('web', 'test')
        elif path in shared_paths and not path.startswith(SOURCE):
            # Shared library inputs are web-only; owned source keeps its existing lane rules.
            affected = ('web',)
        elif path.startswith(('rendering/bevy-explorer/source/react-web/',
                              'rendering/bevy-explorer/source/deploy/web/')):
            # These are runtime web assets, not inputs to Cargo's native tests
            # or headless engine. The web producer builds/tests the HUD and bridge.
            affected = ('web',)
        elif path.startswith('rendering/bevy-explorer/source/deploy/headless/'):
            # Launcher/package assembly cannot affect web or graphical/native tests.
            # Keep the headless build/smoke; main also packages all four platforms.
            affected = ('headless',)
        elif path.startswith('apps/bevy-explorer/e2e/'):
            # The end-to-end suite drives a built client; its own workflow (bevy-explorer-e2e.yml)
            # checks it and runs it against the web app this workflow built.
            affected = ()
        elif path.startswith('apps/bevy-explorer/') and path.removeprefix('apps/bevy-explorer/') in APP_INPUTS:
            affected = APP_INPUTS[path.removeprefix('apps/bevy-explorer/')]
        elif path == SOURCE + 'Cargo.lock':
            old, new = locked_packages(base), locked_packages(head)
            affected = (*LANES, 'scenes') if old is not None and old == new else (*LANES, 'native-matrix', 'scenes')
        elif path.startswith('library/protocol/proto/') or (path.startswith(SOURCE) and not platform_input(path)):
            # Linux validates platform-neutral source on PRs; main pushes and ci:macos add Windows and macOS.
            affected = (*LANES, 'scenes')
        else:
            # Source, selector and unknown input changes validate all jobs.
            affected = (*LANES, 'native-matrix', 'scenes')
        for lane in affected:
            selected[lane] = True
    names = [name for name, enabled in selected.items() if enabled]
    return {**selected, 'base': base,
            'reason': 'Selected: ' + ', '.join(names) if names else 'Inputs unchanged; integrity checks only'}


def plan(event, base, head, repo):
    if event == 'workflow_dispatch':
        return full_selection('Manual full run')
    try:
        if event == 'push':
            baselines = main_baselines(repo)
            selections = {}
            result = {}
            for lane in LANES:
                previous = baselines.get(lane)
                if previous not in selections:
                    selections[previous] = select(previous, head) if previous else full_selection('Missing lane baseline')
                result[lane] = selections[previous][lane]
            result['native-matrix'] = result['test']
            result['reason'] = 'Compared each job with its latest successful main execution'
            return result
        if event not in ('pull_request', 'local'):
            raise ValueError('Unknown event')
        return select(base, head)
    except _artifact.shared_inputs.ManifestError as exc:
        print(_artifact.shared_inputs.validation_message(exc), file=sys.stderr)
        raise
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as exc:
        print(f'Conservative full build: {exc}')
        return full_selection('Comparison unavailable; full run')


def scenes_plan(event, base, head):
    if event == 'workflow_dispatch':
        return {'scenes': True, 'sha': head, 'reason': 'Manual or nightly graphical run'}
    try:
        if event not in ('push', 'pull_request', 'local'):
            raise ValueError('Unknown graphical event')
        selected = select(base, head)
        return {'scenes': selected['scenes'], 'sha': head, 'reason': selected['reason']}
    except _artifact.shared_inputs.ManifestError as exc:
        print(_artifact.shared_inputs.validation_message(exc), file=sys.stderr)
        raise
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as exc:
        print(f'Conservative graphical run: {exc}')
        return {'scenes': True, 'sha': head, 'reason': 'Comparison unavailable; run graphical tests'}


def scene_suite(suite):
    """The --test_scenes argument: each scene's parcel, then the failures it allows."""
    scenes = []
    for line in (ROOT / SCENE_SUITE).read_text().splitlines():
        if not line or line.startswith('#'):
            continue
        parcel, *allowed = line.split('/')
        if (not re.fullmatch(r'-?[0-9]+,-?[0-9]+', parcel) or ';' in line
                or any(not name or name != name.strip() for name in allowed)):
            raise ValueError(f'Malformed scene suite line: {line!r}')
        scenes.append(line)
    if suite == 'snapshots':
        scenes = [scene for scene in scenes if scene.split('/')[0] in SNAPSHOT_SCENES]
    if not scenes:
        raise ValueError(f'Empty {suite} scene suite')
    return ';'.join(scenes)


def unescape(text):
    """The text of a Rust `{:?}` string literal body."""
    escapes = {'n': '\n', 'r': '\r', 't': '\t', '0': '\0'}
    return re.sub(r'\\u\{([0-9a-fA-F]+)\}|\\(.)',
                  lambda m: chr(int(m[1], 16)) if m[1] else escapes.get(m[2], m[2]), text)


def scene_summary(suite, outcome, log):
    """One step-summary line: assertion counts, failures the suite does not list, listed ones
    that pass again, and that acceptance stays open while any failure is listed."""
    log = re.sub(r'\x1b\[[0-9;]*[A-Za-z]', '', log)
    verdicts = re.findall(r'automatic_testing: test .*: (true|false) \[[0-9]+ remaining\]', log)
    passed, failed = verdicts.count('true'), verdicts.count('false')
    # The runner prints its failure block only when it reaches the end of the suite. The block
    # omits failures from scenes outside the suite, which the runner discards.
    finished = 'all tests passed!' in log or 'some tests failed:' in log
    failures = {(parcel, unescape(name)): expected == 'true' for parcel, name, expected in
                re.findall(r'"\[\((-?[0-9]+,-?[0-9]+)\) : (.*)\]",\n\s*".*",\n\s*(true|false),', log)}
    listed = {(scene.split('/')[0], name) for scene in scene_suite(suite).split(';')
              for name in scene.split('/')[1:]}
    unexpected = sorted(f'({parcel}) {name}' for (parcel, name), expected in failures.items() if not expected)
    stale = sorted(f'({parcel}) {name}' for parcel, name in listed - failures.keys()) if finished else []
    discarded = failed - len(failures) if finished else 0
    text = (f'Graphical scene tests ({suite}): **{outcome}**. '
            f'{passed} assertion{"s" if passed != 1 else ""} passed, {failed - discarded} failed')
    if not verdicts:
        text += '; no assertion ran, see the scene-test log artifact'
    elif not finished:
        text += f'; the run stopped before its end, so which failures `{SCENE_SUITE}` lists is not determined'
    elif failed - discarded:
        text += f'; {len(failures) - len(unexpected)} of them are listed in `{SCENE_SUITE}`, so graphical acceptance stays open'
    else:
        text += '; graphical acceptance holds for this suite' if not listed else f'; `{SCENE_SUITE}` still lists failures'
    if discarded:
        text += f'; the runner discarded {discarded} failure{"s" if discarded != 1 else ""} outside the suite'
    if unexpected:
        text += '. Not listed: ' + ', '.join(unexpected)
    if stale:
        text += '. Listed but passing, remove them: ' + ', '.join(stale)
    return text + '. Only the full suite establishes graphical acceptance; see the scene-test log artifact.\n'


def image_matrix(products):
    return {'include': [
        {'product': product, 'arch': arch, 'runner': 'ubuntu-24.04' + ('-arm' if arch == 'arm64' else '')}
        for product in products for arch in (('amd64', 'arm64') if product == 'web' else ('amd64',))]}


def consumer_ready(jobs, consumer):
    expected = IMAGES_JOBS if consumer == 'images' else HEADLESS_JOBS
    prefix = 'APIs + Sites / build (' if consumer == 'images' else 'Headless packages / build ('
    selected = [job for job in jobs if job['name'].startswith(prefix)]
    if not selected or all(job['conclusion'] == 'skipped' for job in selected):
        return False
    if {job['name'] for job in selected} != expected or len(selected) != len(expected):
        raise ValueError(f'Incomplete or duplicate {consumer} build matrix')
    if consumer == 'headless':
        selected += [job for job in jobs if job['name'] == PRODUCERS['headless']]
        if len(selected) != len(expected) + 1:
            raise ValueError('Missing Linux headless producer')
    if not all(job['status'] == 'completed' and job['conclusion'] == 'success' for job in selected):
        raise ValueError(f'Every {consumer} platform must pass before publication')
    return True


def consumer_changed(base, head, consumer):
    if consumer == 'headless':
        return select(base, head)['headless']
    if consumer != 'images':
        raise ValueError('Unknown publication consumer')
    ensure_commit(base)
    ensure_commit(head)
    return bool(changed_inputs(base, head, IMAGES_INPUTS))


def consumer_plan(event, base, head, repo):
    if event == 'workflow_dispatch':
        return {'images': False, 'reason': 'Manual Explorer run; consumers have their own manual entrypoints'}
    try:
        if event == 'push':
            # Compare against a successful image build, never a canceled push
            # whose changes have not been validated. Pre-migration runs contain
            # no image jobs, so the first coordinated push builds both images.
            base = None
            runs = api(f'repos/{repo}/actions/workflows/bevy-explorer-ci.yml/runs'
                       '?branch=main&status=success&per_page=20')['workflow_runs']
            for run in runs:
                if trusted_run(run, repo) and consumer_ready(run_jobs(repo, run['id']), 'images'):
                    base = run['head_sha']
                    break
        elif event not in ('pull_request', 'local'):
            raise ValueError('Unknown image selection event')
        ensure_commit(base)
        ensure_commit(head)
        changed = changed_inputs(base, head, IMAGES_INPUTS if event == 'push' else IMAGES_PACKAGING)
        return {'images': bool(changed), 'reason': 'Image consumer inputs changed' if changed else 'Image consumer inputs unchanged'}
    except (OSError, ValueError, KeyError, TypeError, subprocess.SubprocessError) as exc:
        print(f'Conservative image build: {exc}')
        return {'images': True, 'reason': 'Image comparison unavailable; build both architectures'}


def consumer_source(run, repo, consumer):
    ready = consumer_ready(run_jobs(repo, run['id']), consumer)
    if ready:
        if consumer == 'images':
            names = {f'apis-sites-{kind}-{arch}' for kind in ('images', 'producer') for arch in ('amd64', 'arm64')}
        else:
            names = {f'bevy-headless-{platform}-{run["head_sha"]}' for platform in
                     ('linux-x64', 'darwin-x64', 'darwin-arm64', 'win32-x64')}
        data = api(f'repos/{repo}/actions/runs/{run["id"]}/artifacts?per_page=100')
        found = [row['name'] for row in data['artifacts'] if row['name'] in names and not row['expired']]
        if data['total_count'] != len(data['artifacts']) or set(found) != names or len(found) != len(names):
            raise ValueError(f'Missing, expired or duplicate {consumer} artifacts')
    return {'sha': run['head_sha'], 'run_id': str(run['id']), 'attempt': str(run['run_attempt']), 'build': ready,
            'reason': f'{consumer} builds passed' if ready else f'{consumer} builds were not selected'}


def write_outputs(values):
    print(json.dumps(values, indent=2))
    if os.environ.get('GITHUB_OUTPUT'):
        with open(os.environ['GITHUB_OUTPUT'], 'a') as output:
            for key, value in values.items():
                encoded = json.dumps(value, separators=(',', ':')) if not isinstance(value, str) else value
                output.write(f'{key}={encoded}\n')
    if os.environ.get('GITHUB_STEP_SUMMARY'):
        with open(os.environ['GITHUB_STEP_SUMMARY'], 'a') as summary:
            summary.write(f"Explorer CI: {values['reason']}.\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=('plan', 'source', 'scenes-plan', 'consumer-plan',
                                            'scene-suite', 'scene-summary'))
    parser.add_argument('--consumer', choices=('images', 'headless'))
    parser.add_argument('--base', default=os.environ.get('BASE_SHA', ''))
    parser.add_argument('--head', default=os.environ.get('GITHUB_SHA', 'HEAD'))
    parser.add_argument('--suite', choices=('full', 'snapshots'), default='full')
    parser.add_argument('--outcome', default='')
    parser.add_argument('--log', default='scene-tests.log')
    args = parser.parse_args()
    if args.command == 'scene-suite':
        print(scene_suite(args.suite))
        return
    if args.command == 'scene-summary':
        log = Path(args.log).read_text() if Path(args.log).exists() else ''
        print(scene_summary(args.suite, args.outcome, log), end='')
        return
    if args.command != 'source':
        head = git('rev-parse', args.head).strip()
        event = os.environ.get('GITHUB_EVENT_NAME', 'local')
        base = args.base
        if event == 'pull_request':
            base = tested_base(head, event_pr_head(), args.base)
            print(f'Comparing with {base} (BASE_SHA {args.base or "unset"})')
        if args.command == 'consumer-plan':
            write_outputs(consumer_plan(event, base, head, os.environ.get('GH_REPO', '')))
            return
        result = (scenes_plan(event, base, head) if args.command == 'scenes-plan' else
                  plan(event, base, head, os.environ.get('GH_REPO', '')))
        if args.command == 'plan' and event == 'pull_request':
            result = hold_for_draft(result, event_pull_request())
        write_outputs(result)
    else:
        repo, run_id = os.environ['GH_REPO'], os.environ['SOURCE_RUN']
        if not re.fullmatch(r'[0-9]+', run_id):
            raise ValueError('Invalid run ID')
        run = api(f'repos/{repo}/actions/runs/{run_id}')
        if not trusted_run(run, repo):
            raise ValueError('Source must be a successful main Explorer CI run in this repository')
        if args.consumer:
            write_outputs(consumer_source(run, repo, args.consumer))
            return
        products = produced_artifacts(run_jobs(repo, run_id))
        write_outputs({'sha': run['head_sha'], 'run_id': run_id, 'build': bool(products),
                       'products': products, 'build-matrix': image_matrix(products),
                       'reason': 'Build images from successful producers: ' + ', '.join(products) if products else
                       'Both producers explicitly skipped; image tags stay unchanged'})


if __name__ == '__main__':
    main()
