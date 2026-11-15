#!/usr/bin/env python3
"""Content and provenance contract for the locally built Explorer consumed by Sites."""
import argparse
import hashlib
import io
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
SOURCE = 'rendering/bevy-explorer/source/'
# The upstream references and patch series of Bevy and the forked crates the engine compiles.
# They decide the rebuilt trees, which are not tracked.
PATCHES = 'rendering/bevy-explorer/patches/'
# The crates the engine compiles from tracked bytes.
VENDOR = 'rendering/bevy-explorer/vendor/'
PROTO = 'library/protocol/proto/'
HUD = SOURCE + 'react-web/src/'
# The ts-rs bindings both HUD hosts compile (scripts/gen-ts-bindings.sh writes them; tracked).
BINDINGS = 'library/explorer-core/engine/generated'
# Shared inputs come from the checked compiler/resolver graph, never a directory allowlist.
_SPEC = importlib.util.spec_from_file_location('hud_shared_inputs', Path(__file__).with_name('hud_shared_inputs.py'))
shared_inputs = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(shared_inputs)


MANIFEST = 'sites-play.json'
PARTS = ('engine', 'bridge-scene', 'service_worker.js', 'preview_realm.js')
# Source is maintained here. Hash its tracked files directly, alongside the build tools and
# configuration (the flake lock and package set pin the protoc and wasm-pack the web job
# builds with); generated outputs and local overrides are excluded.
INPUTS = ('apps/bevy-explorer/build-web.py',
          'apps/bevy-explorer/install-web-tools.py',
          'apps/bevy-explorer/install-linux-deps.py', 'apps/bevy-explorer/install-tools.sh',
          '.github/actions/bevy-explorer-tools/action.yml', 'flake.lock', 'infra/packages/default.nix', 'infra/packages/tools.nix',
          'tooling/play-artifact.py', 'tooling/hud_shared_inputs.py', 'tooling/patched-sources.py',
          '.github/workflows/bevy-explorer-ci.yml')
TOOLCHAINS = ('rust-toolchain', 'rust-toolchain.toml')
# Compiled into the engine (option_env!): a value change must miss the engine cache like a source
# change, while the Sites import, which runs without the variables, still verifies the build.
BUILD_ENV = ('BEVY_SEGMENT_WRITE_KEY', 'BEVY_ANALYTICS_DEV')
REQUIRED = ('engine/pkg/webgpu_build_bg.wasm', 'engine/pkg/webgpu_build.js',
            'engine/pkg/manifest.json', 'engine/pkg-scene/dcl_scene_wasm_bg.wasm',
            'engine/headless.html', 'bridge-scene/static/BevyExplorerUI/about',
            'service_worker.js', 'preview_realm.js')
ENGINE_PARTS = {
    'pkg': SOURCE + 'deploy/web/engine/pkg',
    'pkg-scene': SOURCE + 'deploy/web/engine/pkg-scene',
    'bindings': BINDINGS,
}
ENGINE_REQUIRED = ('pkg/webgpu_build_bg.wasm', 'pkg/webgpu_build.js',
                   'pkg/manifest.json', 'pkg-scene/dcl_scene_wasm_bg.wasm',
                   'pkg-scene/dcl_scene_wasm.js')
ENGINE_MANIFEST = 'engine-cache.json'
# Written by fetch-play.py beside an imported main build that a non-engine change takes:
# the producer commit it was validated against and the tree inputs it was validated for.
PRODUCER_VALIDATION = 'producer-validated.json'


def digest(data):
    return hashlib.sha256(data).hexdigest()


def read_json(path):
    return json.loads(path.read_text())


def input_files(root=ROOT, revision=None, engine_only=False):
    shared = shared_inputs.input_hashes(root, revision) if not engine_only else {}
    def selected(path):
        # Native npm packaging is not consumed by the web builder or WASM. The tracked bindings are not
        # either: the build regenerates them from crates/system_api_types, and verify() compares the
        # tracked copy with the ones it built.
        if path.startswith((SOURCE + 'deploy/headless/', BINDINGS + '/')):
            return False
        if engine_only and (path.startswith(SOURCE + 'react-web/') or path in (
                SOURCE + 'deploy/web/package.json', SOURCE + 'deploy/web/package-lock.json')):
            # Web npm metadata still invalidates the assembled artifact, not WASM.
            return False
        return (path.startswith((SOURCE, PATCHES, VENDOR, PROTO, '.cargo/')) or path in shared or path in INPUTS
                # bevy-ci.py selects the web lane for these paths and drops markdown.
                or (not engine_only and ((path.startswith('library/hashing/') and not path.endswith('.md'))
                                        or path == 'tooling/check-local-hashing.mjs'))
                or path in TOOLCHAINS)
    if revision:
        tree = subprocess.check_output(['git', 'ls-tree', '-rz', revision], cwd=root, text=True)
        entries = []
        for row in tree.split('\0'):
            if not row:
                continue
            attributes, path = row.split('\t', 1)
            if selected(path):
                mode, kind, oid = attributes.split()
                if kind != 'blob' or mode not in ('100644', '100755'):
                    raise ValueError(f'Unsupported Explorer source input: {path}')
                entries.append((path, oid))
        names = {path for path, _ in entries}
        if not set(INPUTS).issubset(names):
            # Historical runs before this contract cannot supply an artifact;
            # reject them before loading the full Explorer source.
            raise ValueError('Incomplete Explorer source inputs')
        # One batch avoids spawning a process for every source file in each
        # candidate producer. Read actual blobs, without archive substitutions.
        data = subprocess.check_output(['git', 'cat-file', '--batch'], cwd=root,
                                       input=''.join(oid + '\n' for _, oid in entries).encode())
        stream = io.BytesIO(data)
        hashes = {}
        for path, oid in entries:
            actual, kind, size = stream.readline().decode().split()
            if actual != oid or kind != 'blob':
                raise ValueError(f'Invalid Explorer source blob: {path}')
            hashes[path] = digest(stream.read(int(size)))
            if stream.read(1) != b'\n':
                raise ValueError(f'Invalid Explorer source blob framing: {path}')
    else:
        if (root / '.git').exists():
            tracked = subprocess.check_output(['git', 'ls-files', '-z'], cwd=root, text=True).split('\0')
        else:
            # Nix's filtered contract has no .git and contains only tracked inputs.
            tracked = [p.relative_to(root).as_posix() for base in (SOURCE, PATCHES, VENDOR, PROTO, '.cargo')
                       for p in ([root / base] if (root / base).is_file() else (root / base).rglob('*'))
                       if p.is_file()]
            tracked += list(INPUTS) + list(shared)
            if not engine_only:
                tracked += [p.relative_to(root).as_posix() for p in (root / 'library/hashing').rglob('*')
                            if p.is_file()]
                if (root / 'tooling/check-local-hashing.mjs').is_file():
                    tracked.append('tooling/check-local-hashing.mjs')
            tracked += [name for name in TOOLCHAINS if (root / name).is_file()]
        hashes = {}
        for path in sorted(set(filter(selected, tracked))):
            source = root / path
            if source.is_symlink() or not source.is_file():
                raise ValueError(f'Missing or linked Explorer source input: {path}')
            hashes[path] = digest(source.read_bytes())
    if not any(path.startswith(SOURCE) for path in hashes) or any(
            path not in hashes for path in INPUTS):
        raise ValueError('Incomplete Explorer source inputs')
    hashes.update(shared)
    return hashes


def inputs(root=ROOT, revision=None):
    """Identical files give identical keys across commits; no timestamp or branch key."""
    return digest(json.dumps(input_files(root, revision), sort_keys=True).encode())


def engine_inputs(root=ROOT, revision=None):
    # Web runtime assets stay included: Rust embeds gpu_cache.js via wasm_bindgen.
    return digest(json.dumps(input_files(root, revision, engine_only=True), sort_keys=True).encode())


def build_env():
    return digest(json.dumps({name: digest(os.environ.get(name, '').encode()) for name in BUILD_ENV},
                             sort_keys=True).encode())


def engine_cache_key(root=ROOT):
    return digest(json.dumps({'inputs': engine_inputs(root), 'env': build_env()}).encode())


def files(directory, parts=(*PARTS, 'bindings'), required=REQUIRED):
    result = {}
    for part in parts:
        base = directory / part
        if not base.exists() or base.is_symlink():
            raise ValueError(f'Missing or linked Explorer build input: {part}')
        for path in ([base] if base.is_file() else sorted(base.rglob('*'))):
            if path.is_symlink() or not (path.is_file() or path.is_dir()):
                raise ValueError(f'Unsupported Explorer artifact member: {path}')
            if path.is_file():
                result[path.relative_to(directory).as_posix()] = digest(path.read_bytes())
    for name in required:
        if name not in result or not (directory / name).stat().st_size:
            raise ValueError(f'Explorer build is missing {name}')
    for name in (name for name in required if name.endswith('.wasm')):
        with (directory / name).open('rb') as stream:
            if stream.read(8) != b'\0asm\x01\0\0\0':
                raise ValueError(f'Invalid WASM module: {name}')
    return result


def verify_engine_info(info, root=ROOT, allow_unoptimized=True, producer=None, trust_inputs=False):
    if not isinstance(info, dict) or not (
            trust_inputs or info.get('inputs') == engine_inputs(root, producer)):
        raise ValueError('Compiled engine inputs differ; rebuild the WASM')
    if not re.fullmatch(r'(?:[0-9a-f]{40}|local)', str(info.get('revision', ''))):
        raise ValueError('Missing or invalid compiled engine revision')
    if info.get('optimized') is not True and not (allow_unoptimized and info.get('optimized') is False):
        raise ValueError('Production requires the optimized compiled engine')
    return info


def verify_engine(directory, root=ROOT, allow_unoptimized=True):
    if directory.is_symlink() or (directory / ENGINE_MANIFEST).is_symlink():
        raise ValueError('Linked compiled engine cache or manifest')
    meta = read_json(directory / ENGINE_MANIFEST)
    if meta.get('format') != 1:
        raise ValueError('Unsupported compiled engine cache format')
    if meta.get('build_env') != build_env():
        raise ValueError('Compiled engine build variables differ; rebuild the WASM')
    verify_engine_info(meta, root, allow_unoptimized)
    inventory = files(directory, ENGINE_PARTS, ENGINE_REQUIRED)
    if meta.get('files') != inventory or not any(p.startswith('bindings/') for p in inventory):
        raise ValueError('Compiled engine checksum or file inventory mismatch')
    return {k: meta[k] for k in ('inputs', 'revision', 'optimized')}


def pack_engine(directory, root=ROOT, optimized=True, revision='local'):
    """One disposable output slot; partial writes cannot become valid cache hits."""
    directory.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=directory.parent) as temporary:
        staged = Path(temporary) / 'engine'
        for part, source in ENGINE_PARTS.items():
            shutil.copytree(root / source, staged / part, symlinks=True)
        meta = {'format': 1, 'inputs': engine_inputs(root), 'build_env': build_env(), 'revision': revision,
                'optimized': optimized, 'files': files(staged, ENGINE_PARTS, ENGINE_REQUIRED)}
        (staged / ENGINE_MANIFEST).write_text(json.dumps(meta, sort_keys=True, indent=2) + '\n')
        info = verify_engine(staged, root)
        if directory.exists():
            shutil.rmtree(directory)
        staged.rename(directory)
    return info


def restore_engine(directory, root=ROOT, allow_unoptimized=True):
    info = verify_engine(directory, root, allow_unoptimized)
    for part, target in ENGINE_PARTS.items():
        target = root / target
        if target.exists():
            shutil.rmtree(target)
        shutil.copytree(directory / part, target)
    return info


def verify_build(directory, root=ROOT, allow_unoptimized=True, producer=None, trust_inputs=False):
    """Validate producer bytes independently of the Sites bindings check.

    The build's inputs must be the tree's, or, for a change that touches no engine
    input, those of `producer` (the main commit that built it). `trust_inputs` also
    accepts the comparison fetch-play.py recorded in PRODUCER_VALIDATION for a tree with
    these very inputs and this build, where no git history is available (the Nix image
    build of a PR); without a valid record the build is checked as usual. Production
    never passes either."""
    meta = read_json(directory / MANIFEST)
    validated = False
    if trust_inputs:
        try:
            record = read_json(directory / PRODUCER_VALIDATION)
        except (OSError, ValueError):
            record = {}
        validated = (record.get('tree_inputs') == inputs(root) and record.get('producer') == meta.get('build_revision')
                     and re.fullmatch('[0-9a-f]{40}', str(record.get('producer', ''))) is not None)
    if meta.get('format') != 3 or not (validated or meta.get('inputs') == inputs(root, producer)):
        raise ValueError('Explorer build inputs differ; rebuild Bevy or import a matching CI artifact')
    if meta.get('optimized') is not True and not (allow_unoptimized and meta.get('optimized') is False):
        raise ValueError('Production requires the optimized Explorer build')
    engine = verify_engine_info(meta.get('engine'), root, allow_unoptimized, producer, validated)
    if engine['optimized'] is not meta['optimized']:
        raise ValueError('Explorer optimization flag differs from the compiled engine')
    if meta.get('files') != files(directory):
        raise ValueError('Explorer artifact checksum or file inventory mismatch')
    return meta


def record_producer_validation(directory, producer, root=ROOT):
    """fetch-play.py validated this build against `producer` for a tree with the current inputs."""
    path = directory / PRODUCER_VALIDATION
    path.write_text(json.dumps({'producer': producer, 'tree_inputs': inputs(root)}, sort_keys=True) + '\n')


def verify(directory, root=ROOT, allow_unoptimized=True, producer=None, trust_inputs=False):
    """Sites additionally requires the build's bindings to match the tracked ones, whatever built the engine."""
    meta = verify_build(directory, root, allow_unoptimized, producer, trust_inputs)
    bindings = root / BINDINGS
    expected = {p.relative_to(bindings).as_posix(): digest(p.read_bytes())
                for p in bindings.rglob('*.ts')}
    actual = {p.removeprefix('bindings/'): h for p, h in meta['files'].items() if p.startswith('bindings/')}
    if not expected or expected != actual:
        raise ValueError('Sites engine bindings differ from this build; regenerate with rendering/bevy-explorer/source/scripts/gen-ts-bindings.sh')
    return meta


def pack(directory, root=ROOT, optimized=True):
    info = read_json(directory / 'build-info.json')
    source = root / BINDINGS
    target = directory / 'bindings'
    if target.exists():
        shutil.rmtree(target)
    shutil.copytree(source, target)
    meta = {'format': 3, 'inputs': inputs(root), 'optimized': optimized,
            'engine': info.get('engine'),
            'build_revision': info['revision'],
            'files': files(directory)}
    (directory / MANIFEST).write_text(json.dumps(meta, sort_keys=True, indent=2) + '\n')
    verify_build(directory, root)
    return meta


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=('pack', 'verify', 'inputs',
                                           'engine-inputs', 'engine-cache-key', 'verify-engine'))
    parser.add_argument('directory', type=Path, nargs='?')
    parser.add_argument('--root', type=Path, default=ROOT)
    parser.add_argument('--unoptimized', action='store_true')
    parser.add_argument('--production', action='store_true')
    parser.add_argument('--trust-inputs', action='store_true',
                        help='verify: accept the build inputs the manifest records (a PR image whose engine fetch-play.py already validated)')
    args = parser.parse_args()
    if args.trust_inputs and args.production:
        parser.error('--trust-inputs is never allowed with --production')
    if args.command == 'inputs':
        print(inputs(args.root))
    elif args.command == 'engine-inputs':
        print(engine_inputs(args.root))
    elif args.command == 'engine-cache-key':
        print(engine_cache_key(args.root))
    elif args.command == 'verify-engine':
        print(json.dumps(verify_engine(args.directory, args.root, not args.production)))
    elif args.command == 'pack':
        print(json.dumps(pack(args.directory, args.root, not args.unoptimized)))
    else:
        meta = verify(args.directory, args.root, not args.production, trust_inputs=args.trust_inputs)
        print(f"Verified Explorer built by dcl-one {meta['build_revision']}")


if __name__ == '__main__':
    main()
