#!/usr/bin/env python3
"""Build the Explorer web engine, HUD and bridge scene."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time

ENVIRONMENT = ('environment: WASM_PACK_MODE is passed to wasm-pack as --mode. The value no-install uses the '
               'wasm-bindgen on PATH instead of a download, for a sandboxed or offline build. '
               'No workflow of this tree sets it; the person who runs the build does.')
APP = Path(__file__).resolve().parent
ROOT = APP.parents[1]
SOURCE = ROOT / 'rendering/bevy-explorer/source'
spec = importlib.util.spec_from_file_location('play_artifact', ROOT / 'tooling/play-artifact.py')
artifact = importlib.util.module_from_spec(spec)
spec.loader.exec_module(artifact)


def git_bash():
    # `bash` on a Windows PATH is often WSL's launcher (System32). Git for Windows' own bash sits at
    # <root>/bin/bash.exe, which puts Git's tools on PATH; `git --exec-path` is under <root>.
    exec_path = subprocess.run(('git', '--exec-path'), capture_output=True, text=True, check=True).stdout
    for parent in Path(exec_path.strip()).parents:
        if (parent / 'bin/bash.exe').is_file():
            return str(parent / 'bin/bash.exe')
    raise SystemExit('Git Bash not found: install Git for Windows (scripts/gen-ts-bindings.sh needs bash)')


def executable(name):
    # Windows starts npm (npm.cmd) only by its full path, not by the bare name.
    if os.name != 'nt':
        return name
    return git_bash() if name == 'bash' else shutil.which(name) or name


def run(*args, cwd=SOURCE, env=None, **options):
    started = time.monotonic()
    subprocess.run((executable(str(args[0])), *args[1:]), cwd=cwd, env=env, check=True, **options)
    print(f'{" ".join(map(str, args))}: {time.monotonic() - started:.1f}s', flush=True)


def build_environment():
    return {**os.environ, 'RUSTUP_TOOLCHAIN': 'nightly-2026-04-15',
            'RAYON_NUM_THREADS': os.environ.get('RAYON_NUM_THREADS', str(min(os.cpu_count() or 1, 16)))}


def materialise_sources(env):
    # Cargo reads Bevy and the forked crates from trees that are not tracked: write the ones
    # that are missing or stale.
    run(sys.executable, str(ROOT / 'tooling/patched-sources.py'), 'materialise', '--quiet', cwd=ROOT, env=env)


def cargo_wasm(no_opt, env):
    """Compile both modules in one feature graph; returns the directory that holds the two .wasm files."""
    if no_opt:
        env = {**env, 'CARGO_PROFILE_RELEASE_CODEGEN_UNITS': '16'}
    run('cargo', 'build', '--locked', '--release', '--target', 'wasm32-unknown-unknown',
        '--lib', '-p', 'decentra-bevy', '-p', 'dcl_scene_wasm', '--no-default-features',
        '--features=decentra-bevy/livekit,decentra-bevy/social', env=env)
    return Path(env.get('CARGO_TARGET_DIR', SOURCE / 'target')) / 'wasm32-unknown-unknown/release'


def compile_wasm(no_opt, env):
    target = cargo_wasm(no_opt, env)
    for name, directory in (('webgpu_build', 'pkg'), ('dcl_scene_wasm', 'pkg-scene')):
        output = SOURCE / 'deploy/web/engine' / directory
        run('wasm-bindgen', str(target / f'{name}.wasm'), '--target', 'web',
            '--typescript', '--out-dir', str(output), env=env)
        if not no_opt:
            wasm = output / f'{name}_bg.wasm'
            optimized = wasm.with_suffix('.opt.wasm')
            run('wasm-opt', str(wasm), '-O', '-o', str(optimized), env=env)
            optimized.replace(wasm)


def prepare_engine(no_opt, engine_cache, shared_build, env):
    flags = ['--no-opt'] if no_opt else []
    # WASM_PACK_MODE=no-install needs the Cargo.lock's wasm-bindgen-cli installed (see ENVIRONMENT).
    if mode := os.environ.get('WASM_PACK_MODE'):
        flags += ['--mode', mode]
    revision = os.environ.get('GITHUB_SHA', 'local')
    if engine_cache and engine_cache.exists():
        engine = artifact.restore_engine(engine_cache, ROOT, allow_unoptimized=no_opt)
        print(f"Reusing compiled WASM from {engine['revision']}", flush=True)
    else:
        # Only generated outputs: stale workers/bindings must not enter a new cache.
        for part in artifact.ENGINE_PARTS.values():
            target = ROOT / part
            if target.exists():
                shutil.rmtree(target)
        materialise_sources(env)
        if shared_build:
            compile_wasm(no_opt, env)
        else:
            run('wasm-pack', 'build', '--target', 'web', *flags, '--out-dir', './deploy/web/engine/pkg',
                '--no-default-features', '--features=livekit,social', env=env)
            run('wasm-pack', 'build', 'crates/dcl_scene_wasm', '--target', 'web', *flags,
                '--out-dir', '../../deploy/web/engine/pkg-scene', '--no-default-features', env=env)
        pkg = SOURCE / 'deploy/web/engine/pkg'
        (pkg / '.gitignore').unlink(missing_ok=True)
        (pkg / 'manifest.json').write_text(json.dumps({'wasmSize': (pkg / 'webgpu_build_bg.wasm').stat().st_size}))
        (SOURCE / 'deploy/web/engine/pkg-scene/.gitignore').unlink(missing_ok=True)
        run('bash', './scripts/gen-ts-bindings.sh', env=env)
        engine = {'inputs': artifact.engine_inputs(ROOT), 'revision': revision, 'optimized': not no_opt}
        if engine_cache:
            artifact.pack_engine(engine_cache, ROOT, optimized=not no_opt, revision=revision)
    return engine


def web(no_opt=False, engine_cache=None, shared_build=False):
    env = build_environment()
    revision = os.environ.get('GITHUB_SHA', 'local')
    with ThreadPoolExecutor(max_workers=2) as pool:
        installations = [
            pool.submit(run, 'npm', 'ci', '--prefix', 'react-web', env=env),
            pool.submit(run, 'corepack', 'pnpm@11.27.0', '--dir', 'react-web/bridge-scene',
                        'install', '--frozen-lockfile', '--ignore-scripts', env=env),
        ]
        engine = prepare_engine(no_opt, engine_cache, shared_build, env)
        for installation in installations:
            installation.result()
    run('node', './react-web/scripts/shared-inputs.mjs', '--check', env=env)
    metadata = SOURCE / 'target/sandbox-worker.meta.json'
    metadata.parent.mkdir(exist_ok=True)
    run('node', './react-web/node_modules/esbuild/bin/esbuild', './deploy/web/engine/sandbox_worker.js',
        '--bundle', '--format=esm', '--log-level=error', f'--metafile={metadata}',
        '--outfile=./deploy/web/engine/pkg/sandbox_worker.bundle.js', env=env)
    entries = [o for o in json.loads(metadata.read_text())['outputs'].values() if o.get('entryPoint')]
    if len(entries) != 1 or entries[0]['exports']:
        raise SystemExit('Sandbox worker must have one entry point and export nothing')
    # Relative URLs allow the same build at / or a gateway subpath. No upstream CDN stamping.
    web_env = {**env, 'PUBLIC_URL': '', 'SCENE_BASE_URL': '../'}
    run('npm', 'run', 'build', '--prefix', 'react-web', env=web_env)
    run('corepack', 'pnpm@11.27.0', '--dir', 'react-web/bridge-scene',
        '--filter', '@dcl/hashing', 'build', env=web_env)
    run('node', str(ROOT / 'tooling/check-local-hashing.mjs'),
        str(SOURCE / 'react-web/bridge-scene'), env=web_env)
    # Node 25+ ships a localStorage global (empty without --localstorage-file) that
    # vitest keeps over jsdom's; CI runs Node 24, so the tests see it switched off.
    node_options = ' '.join(filter(None, [env.get('NODE_OPTIONS'), '--no-experimental-webstorage']))
    run('npm', 'run', 'test', '--prefix', 'react-web', env={**web_env, 'NODE_OPTIONS': node_options})
    run('corepack', 'pnpm@11.27.0', '--dir', 'react-web/bridge-scene', 'run', 'bundle', env=web_env)
    output = APP / 'dist/web'
    if output.exists():
        shutil.rmtree(output)
    shutil.copytree(SOURCE / 'deploy/web', output,
                    ignore=shutil.ignore_patterns('node_modules', '.env', '.gitignore', 'package*.json', 'scripts', '*.md'))
    for name in ('index.html', 'app.html', 'engine/pkg/webgpu_build_bg.wasm',
                 'engine/pkg-scene/dcl_scene_wasm_bg.wasm', 'service_worker.js'):
        if not (output / name).is_file():
            raise SystemExit(f'Missing web build output: {name}')
    shutil.copy2(SOURCE / 'LICENSE', output / 'LICENSE')
    (output / 'build-info.json').write_text(json.dumps({'component': 'bevy-explorer-web',
        'revision': revision, 'engine': engine}) + '\n')
    run(sys.executable, str(ROOT / 'tooling/play-artifact.py'), 'pack', str(output),
        *([] if engine['optimized'] else ['--unoptimized']), cwd=ROOT)
    print(output)


def main():
    parser = argparse.ArgumentParser(description=__doc__, epilog=ENVIRONMENT)
    parser.add_argument('--no-opt', action='store_true', help='Skip wasm-opt for development/PR builds')
    parser.add_argument('--engine-cache', type=Path,
                        help='CI-only compiled WASM cache, restored with an exact input key')
    parser.add_argument('--shared-build', action='store_true',
                        help='Compile both WASM modules in one Cargo feature graph; requires wasm-bindgen and wasm-opt')
    parser.add_argument('--target-only', action='store_true',
                        help='Run only the Cargo step of a shared build and stop; CI fills a target cache with it')
    args = parser.parse_args()
    if args.target_only:
        env = build_environment()
        materialise_sources(env)
        cargo_wasm(args.no_opt, env)
        return
    web(args.no_opt, args.engine_cache.resolve() if args.engine_cache else None, args.shared_build)


if __name__ == '__main__':
    main()
