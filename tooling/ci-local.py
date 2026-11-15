#!/usr/bin/env python3
"""Run the GitHub workflows' checks locally, on exactly the content being committed or pushed.

  python3 tooling/ci-local.py install      enable the hooks in .githooks (core.hooksPath)
  python3 tooling/ci-local.py pre-commit   fast checks on the staged tree
  python3 tooling/ci-local.py pre-push     every triggered workflow job on each pushed commit
  python3 tooling/ci-local.py run [REV]    the pre-push checks on REV (default HEAD), without pushing

Checks run in a detached worktree under .cache/ci-local/tree, so unstaged edits in the
main checkout never leak in, and its Rust target, node_modules and dev database persist
between runs. The Nix shell environment is cached per flake.lock/toolchain, and a step
that passed on a tree is not repeated for that tree. Workflow path filters are read from
.github/workflows, so a workflow GitHub would skip is skipped here too. Without Nix
(e.g. Windows) the steps whose tools are on PATH run and the rest are reported skipped.
`git commit --no-verify` / `git push --no-verify` bypass the hooks.
"""
import contextlib
import errno
import fnmatch
import hashlib
import importlib.util
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
STATE = ROOT / '.cache/ci-local'
TREE = STATE / 'tree'
ZERO = '0' * 40
NODE_CRATES = ('--workspace --exclude dcl-scene-cli --exclude dcl-scene-authoring --exclude dcl-scene-build '
               '--exclude dcl-preview-tunnel --exclude dcl-preview-tunnel-protocol')
SHELL_INPUTS = ('flake.nix', 'flake.lock', 'services/catalyst/rust-toolchain.toml', 'infra/dev/shell-hook.sh')
LINUX = platform.system() == 'Linux'
WINDOWS = os.name == 'nt'
_paths_spec = importlib.util.spec_from_file_location('ci_paths', Path(__file__).with_name('ci_paths.py'))
paths = importlib.util.module_from_spec(_paths_spec)
_paths_spec.loader.exec_module(paths)
glob_regex = paths.glob_regex
matching_paths = paths.matching_paths
non_documentation = paths.non_documentation


def git(*args, cwd=ROOT, env=None, check=True, input=None):
    result = subprocess.run(['git', *args], cwd=cwd, env=env, input=input, capture_output=True, text=True)
    if check and result.returncode:
        sys.exit(f'git {" ".join(args)} failed:\n{result.stderr}')
    return result.stdout.strip()


def clean_env():
    """The hook's GIT_DIR/GIT_INDEX_FILE must not reach git commands run in the worktree."""
    return {k: v for k, v in os.environ.items() if not k.startswith('GIT_')}


def workflow_paths(name, event='pull_request', source=None):
    """The `paths:` list of a workflow's event (as checked out for the run), or None when it runs on every change."""
    source = source or (TREE if (TREE / '.github/workflows' / name).exists() else ROOT)
    if name in ('apis-sites-images.yml', 'bevy-explorer-headless.yml'):
        spec = importlib.util.spec_from_file_location('bevy_selection', source / 'tooling/bevy-ci.py')
        selection = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(selection)
        if name == 'apis-sites-images.yml':
            return list(selection.IMAGES_INPUTS)
        return ['.cargo/**', 'rust-toolchain*',
                'rendering/bevy-explorer/source/.cargo/**', 'rendering/bevy-explorer/source/build.rs',
                'rendering/bevy-explorer/source/src/**', 'rendering/bevy-explorer/source/crates/**',
                'rendering/bevy-explorer/source/Cargo.*', 'rendering/bevy-explorer/source/deploy/headless/**',
                'rendering/bevy-explorer/patches/**', 'rendering/bevy-explorer/vendor/**',
                'tooling/patched-sources.py', '.github/actions/bevy-explorer-sources/**',
                'apps/bevy-explorer/build-headless.py', 'apps/bevy-explorer/headless-packages.py',
                'apps/bevy-explorer/install-*.py', 'apps/bevy-explorer/prepare-native-cache.py',
                '.github/workflows/bevy-explorer-headless*.yml', 'tooling/ci-publication.py']
    lines = (source / '.github/workflows' / name).read_text().splitlines()
    paths, section = None, None
    for line in lines:
        stripped = line.strip()
        indent = len(line) - len(line.lstrip())
        if indent == 2 and stripped.endswith(':'):
            section = stripped[:-1]
        elif section == event and stripped == 'paths:':
            paths = []
        elif paths is not None and section == event and stripped.startswith('- '):
            paths.append(stripped[2:].strip('\'"'))
        elif stripped and indent == 0:
            section = None
    return paths


def triggered(name, changed, event='pull_request'):
    paths = workflow_paths(name, event)
    if paths is None:
        return True
    matched = matching_paths(changed, paths)
    if name == 'apis-sites-images.yml':
        matched = non_documentation(matched)
    return bool(matched)


def touches(changed, *patterns):
    return any(fnmatch.fnmatch(f, p) for f in changed for p in patterns)


class Step:
    def __init__(self, name, command, nix=True, tools=()):
        self.name, self.command, self.nix, self.tools = name, command, nix, tools


def web_steps(changed_ui3=True):
    install = Step('web install', 'pnpm --dir apps/ui3 install --frozen-lockfile --prefer-offline && '
                   'pnpm --dir apps/sites install --frozen-lockfile --prefer-offline', tools=('pnpm',))
    steps = [install]
    if changed_ui3:
        steps.append(Step('ui3 unit tests', 'pnpm --dir apps/ui3 test', tools=('pnpm',)))
        steps.append(Step('editor scene tests', 'pnpm --dir apps/ui3/editor-scene install --frozen-lockfile --prefer-offline && '
                          'pnpm --dir apps/ui3/editor-scene test', tools=('pnpm',)))
    steps.append(Step('sites unit tests', 'pnpm --dir apps/sites test', tools=('pnpm',)))
    return steps


STRUCTURE = [Step('ownership catalogue', 'python3 tooling/owners.py --check', tools=('python3',)),
             Step('module layers', 'bash services/catalyst/scripts/check-module-dag.sh', tools=('bash', 'rg'))]
TOOLING_TESTS = Step('tooling tests', 'python3 -m unittest discover -s tooling/tests', tools=('python3',))


def pre_commit_steps(changed):
    steps = list(STRUCTURE)
    if touches(changed, 'tooling/*', 'flake.*', 'infra/dev/*'):
        steps.append(TOOLING_TESTS)
    if touches(changed, 'infra/gateway/*', '.github/workflows/gateway-images.yml'):
        steps.append(Step('gateway configuration', 'python3 -m unittest discover -s infra/gateway/tests',
                          tools=('python3',)))
    if touches(changed, 'services/catalyst/*', 'schemas/*', 'apps/dcl-scene-cli/*', 'library/*', '.cargo/*'):
        steps.append(Step('cargo check (node)', f'cd services/catalyst && cargo check --locked {NODE_CRATES} --all-targets',
                          tools=('cargo',)))
    if touches(changed, 'apps/dcl-scene-cli/*', 'library/*', '.cargo/*'):
        steps.append(Step('cargo check (sdk)', 'cd services/catalyst && cargo check --locked -p dcl-scene-cli --all-targets',
                          tools=('cargo',)))
    if touches(changed, 'apps/ui3/*', 'library/catalyst-api/*', 'library/shop/*', 'library/ui/apps/hud/*', 'library/explorer-core/*'):
        steps += web_steps()
    elif touches(changed, 'apps/sites/*'):
        steps += web_steps(changed_ui3=False)
    return steps


def image_system():
    """The Linux system to build production images for: native, or (with CI_LOCAL_IMAGES=1) a remote builder."""
    if LINUX:
        return f'{platform.machine().replace("arm64", "aarch64")}-linux'
    if os.environ.get('CI_LOCAL_IMAGES') != '1':
        return None
    builders = subprocess.run(['nix', 'config', 'show', 'builders'], capture_output=True, text=True).stdout.strip()
    if builders.startswith('@'):
        with contextlib.suppress(OSError):
            builders = Path(builders[1:]).read_text()
    for system in ('aarch64-linux', 'x86_64-linux'):
        if system in builders:
            return system
    return None


def pre_push_steps(changed):
    steps = list(STRUCTURE)
    # Pull requests run it unfiltered (its CI gate always reports); what it tests is its push.paths.
    development = triggered('development.yml', changed, event='push')
    images = triggered('apis-sites-images.yml', changed)
    gateway = triggered('gateway-images.yml', changed)
    if gateway:
        steps.append(Step('gateway configuration', 'python3 -m unittest discover -s infra/gateway/tests',
                          tools=('python3',)))
        system = image_system()
        if system is None:
            steps.append(Step('gateway image', None))
        else:
            steps.append(Step('gateway image', f'nix build .#packages.{system}.image-gateway '
                              '--out-link .cache/gateway-image -L', tools=('nix',)))
            if LINUX and shutil.which('docker'):
                arch = 'arm64' if platform.machine() in ('aarch64', 'arm64') else 'amd64'
                steps.append(Step('gateway smoke', 'docker load --input .cache/gateway-image && '
                                  'python3 infra/gateway/smoke.py --revision "$(git rev-parse HEAD)" '
                                  f'--arch {arch}', tools=('docker',)))
    bevy = triggered('bevy-explorer-ci.yml', changed)
    if bevy:
        steps.append(Step('bevy-explorer formatting', 'cd rendering/bevy-explorer/source && cargo fmt --all -- --check', tools=('cargo',)))
    if development or images or bevy:
        steps.append(TOOLING_TESTS)
    if development:
        steps.append(Step('dev stack smoke', 'bash tooling/dev-smoke.sh', tools=('nix',)))
        steps += web_steps()
        steps.append(Step('rust and sites e2e suites', 'bash tooling/dev-test.sh', tools=('nix',)))
    if images:
        system = image_system()
        if system is None:
            steps.append(Step('production images', None))
        else:
            native = LINUX and shutil.which('docker')
            apis_link, sites_link = (f'--out-link .cache/images/{n}' if native else '--no-link'
                                    for n in ('apis', 'sites'))
            steps.append(Step(f'packaged docs ({system})',
                              f"nix build 'git+file:.?lfs=1#packages.{system}.sites.docsContent' --no-link -L",
                              tools=('nix',)))
            steps.append(Step(f'Sites toolchain ({system})',
                              f'nix build .#packages.{system}.sites-toolchain --no-link -L',
                              tools=('nix',)))
            steps.append(Step('Sites Explorer artifact', 'python3 tooling/fetch-play.py --production', tools=('python3',)))
            steps.append(Step(f'production images ({system})',
                              f'mkdir -p .cache/images && '
                              f'nix build .#packages.{system}.image-apis {apis_link} -L && '
                              f"PLAY_WEB_DIR=\"$(python3 tooling/fetch-play.py --print-directory)\" "
                              f"nix build --impure 'git+file:.?lfs=1#packages.{system}.image-sites' {sites_link} -L",
                              tools=('nix',)))
            if native:
                steps.append(Step('image smoke', 'for i in apis sites; do docker load --input .cache/images/$i; done && '
                                  'python3 tooling/image-smoke.py --revision "$(git rev-parse HEAD)"', tools=('docker',)))
    return steps


@contextlib.contextmanager
def locked():
    STATE.mkdir(parents=True, exist_ok=True)
    with open(STATE / 'lock', 'w') as handle:
        if WINDOWS:
            import msvcrt
            delay = .1
            announced = False
            while True:
                try:
                    msvcrt.locking(handle.fileno(), msvcrt.LK_NBLCK, 1)
                    break
                except OSError as error:
                    if error.errno not in (errno.EACCES, errno.EAGAIN, errno.EDEADLK):
                        raise
                    if not announced:
                        print('Waiting for another local CI run to finish; Ctrl-C cancels this queued run.', file=sys.stderr)
                        announced = True
                    time.sleep(delay)
                    delay = min(delay * 2, 2)
        else:
            import fcntl
            fcntl.flock(handle, fcntl.LOCK_EX)
        yield


def sync_tree(treeish, commit=None):
    """Make the worktree hold exactly `treeish`; ignored build caches survive."""
    env = clean_env()
    if not (TREE / '.git').exists():
        git('worktree', 'prune', env=env)
        git('worktree', 'add', '--detach', '--no-checkout', str(TREE), 'HEAD', env=env)
    if commit:
        git('update-ref', '--no-deref', 'HEAD', commit, cwd=TREE, env=env)
    git('read-tree', '-u', '--reset', treeish, cwd=TREE, env=env)
    git('clean', '-fdq', cwd=TREE, env=env)


def nix_env():
    """A cached `nix print-dev-env` for the worktree's flake inputs, or None without Nix."""
    if not shutil.which('nix'):
        return None
    digest = hashlib.sha256(platform.system().encode())
    for name in SHELL_INPUTS:
        digest.update((TREE / name).read_bytes())
    path = STATE / f'dev-env-{digest.hexdigest()[:16]}.sh'
    if not path.exists():
        print('ci-local: evaluating the development shell (cached until the flake inputs change)')
        result = subprocess.run(['nix', 'print-dev-env'], cwd=TREE, env=clean_env(), capture_output=True, text=True)
        if result.returncode:
            sys.exit(f'nix print-dev-env failed:\n{result.stderr}')
        for old in STATE.glob('dev-env-*.sh'):
            old.unlink()
        path.write_text(result.stdout)
    return path


def run_steps(label, treeish, steps):
    tree = git('rev-parse', f'{treeish}^{{tree}}', cwd=TREE, env=clean_env())
    passed_dir = STATE / 'passed' / tree
    env_file = nix_env()
    env = clean_env()
    env['CI_LOCAL'] = '1'
    failures, skipped, timings = [], [], []
    for step in steps:
        stamp = passed_dir / hashlib.sha256(f'{step.name}\0{step.command}\0{env_file}'.encode()).hexdigest()[:16]
        if step.command is None:
            skipped.append(f'{step.name}: not on Linux; CI_LOCAL_IMAGES=1 builds them on a remote Linux builder')
            continue
        if stamp.exists():
            timings.append((step.name, 'cached'))
            continue
        if env_file is None:
            missing = [t for t in step.tools if not shutil.which(t)]
            if 'nix' in step.tools or missing:
                skipped.append(f'{step.name}: needs {", ".join(missing or ["nix"])}')
                continue
        print(f'\n━━ ci-local {label}: {step.name}', flush=True)
        started = time.monotonic()
        if env_file:
            script = (f'source "{env_file.as_posix()}" >/dev/null; trap \'rm -rf "$NIX_BUILD_TOP"\' EXIT; '
                      f'set -eo pipefail; {step.command}')
            code = subprocess.run(['bash', '-c', script], cwd=TREE, env=env).returncode
        else:
            code = subprocess.run(['bash', '-c', f'set -e; {step.command}'], cwd=TREE, env=env).returncode
        timings.append((step.name, f'{time.monotonic() - started:.0f}s'))
        if code:
            failures.append(step.name)
            break
        passed_dir.mkdir(parents=True, exist_ok=True)
        stamp.touch()
    print(f'\n━━ ci-local {label} summary')
    for name, took in timings:
        print(f'  {"FAIL" if name in failures else "ok  "} {name} ({took})')
    for reason in skipped:
        print(f'  skip {reason}')
    if failures:
        print(f'  Worktree with the failing content: {TREE}\n  Bypass (not recommended): --no-verify')
    return not failures


def changed_files(*diff_args, env=None):
    return set(filter(None, git('diff', '--name-only', '--no-renames', *diff_args, env=env).splitlines()))


def pre_commit():
    # write-tree/diff use the hook's environment so a partial `git commit <paths>` index is honoured.
    tree = git('write-tree')
    has_head = git('rev-parse', '--verify', '-q', 'HEAD', check=False)
    changed = changed_files('--cached', *(['HEAD'] if has_head else ['4b825dc642cb6eb9a060e54bf8d69288fbee4904']))
    with locked():
        sync_tree(tree)
        return run_steps('pre-commit', tree, pre_commit_steps(changed))


def changes_for_push(sha, remote_sha):
    env = clean_env()
    changed = set()
    if remote_sha != ZERO and subprocess.run(['git', 'cat-file', '-e', remote_sha], cwd=ROOT, env=env,
                                             capture_output=True).returncode == 0:
        changed |= changed_files(remote_sha, sha, env=env)
    base = git('merge-base', sha, 'origin/main', check=False, env=env)
    if base:
        changed |= changed_files(base, sha, env=env)
    elif not changed:
        changed = set(git('ls-tree', '-r', '--name-only', sha, env=env).splitlines())
    return changed


def check_commit(sha, changed, label):
    with locked():
        sync_tree(sha, commit=sha)
        return run_steps(label, sha, pre_push_steps(changed))


def pre_push(args):
    updates = sys.stdin.read()
    seen = set()
    for line in updates.splitlines():
        _, sha, _, remote_sha = line.split()
        if sha == ZERO or sha in seen:
            continue
        seen.add(sha)
        if not check_commit(sha, changes_for_push(sha, remote_sha), f'pre-push {sha[:10]}'):
            return False
    if shutil.which('git-lfs'):
        return subprocess.run(['git', 'lfs', 'pre-push', *args], input=updates, text=True).returncode == 0
    return True


def install():
    git('config', 'core.hooksPath', '.githooks')
    print('Hooks enabled from .githooks (pre-commit, pre-push and the Git LFS hooks).')
    return True


def main():
    command = sys.argv[1] if len(sys.argv) > 1 else ''
    if command == 'install':
        ok = install()
    elif command == 'pre-commit':
        ok = pre_commit()
    elif command == 'pre-push':
        ok = pre_push(sys.argv[2:])
    elif command == 'run':
        sha = git('rev-parse', '--verify', (sys.argv[2] if len(sys.argv) > 2 else 'HEAD') + '^{commit}')
        ok = check_commit(sha, changes_for_push(sha, ZERO), f'run {sha[:10]}')
    else:
        print(__doc__, file=sys.stderr)
        return 2
    return 0 if ok else 1


if __name__ == '__main__':
    sys.exit(main())
