"""Reuse compiled WASM only for identical engine inputs; always rebuild/test the HUD."""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
from hud_manifest_fixture import seal, MANIFEST

REPO = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location('bevy_build', REPO / 'apps/bevy-explorer/build-web.py')
build = importlib.util.module_from_spec(spec)
spec.loader.exec_module(build)
artifact = build.artifact
SHA = 'a' * 40


class EngineCacheTests(unittest.TestCase):
    def test_hashing_changes_invalidate_web_artifacts_but_reuse_the_engine(self):
        path = 'library/hashing/src/index.ts'
        self.write(path, 'first hashing implementation')
        subprocess.run(['git', 'add', path], cwd=self.root, check=True)
        web_before = artifact.inputs(self.root)
        engine_before = artifact.engine_inputs(self.root)
        self.write(path, 'second hashing implementation')
        web_after = artifact.inputs(self.root)
        self.assertNotEqual(web_after, web_before)
        self.assertEqual(artifact.engine_inputs(self.root), engine_before)
        # bevy-ci.py selects no lane for markdown, so markdown is not part of the fingerprint.
        self.write('library/hashing/README.md', 'prose')
        subprocess.run(['git', 'add', 'library/hashing/README.md'], cwd=self.root, check=True)
        self.assertEqual(artifact.inputs(self.root), web_after)

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.cache = self.root / '.cache/bevy-engine'
        self.source = self.root / artifact.SOURCE
        for name in (*artifact.INPUTS, 'rust-toolchain.toml', 'apps/bevy-explorer/build-headless.py',
                     artifact.SOURCE + 'src/lib.rs', artifact.SOURCE + 'Cargo.lock',
                     artifact.PROTO + 'decentraland/example.proto',
                     artifact.SOURCE + 'deploy/web/engine/gpu_cache.js',
                     artifact.SOURCE + 'deploy/web/package.json',
                     artifact.SOURCE + 'deploy/web/package-lock.json',
                     artifact.SOURCE + 'deploy/headless/launcher/package.json',
                     artifact.SOURCE + 'deploy/headless/launcher/bin/cli.js',
                     artifact.SOURCE + 'deploy/headless/build-platform-package.js',
                     artifact.HUD + 'main.tsx', artifact.SOURCE + 'react-web/bridge-scene/package.json'):
            self.write(name, 'input')
        subprocess.run(['git', 'init', '-q'], cwd=self.root, check=True)
        seal(self.root)
        subprocess.run(['git', 'add', '.'], cwd=self.root, check=True)
        self.outputs()

    def write(self, name, value):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(value if isinstance(value, bytes) else value.encode())
        if (self.root / MANIFEST).exists():
            seal(self.root)
        return path

    def outputs(self):
        for name in artifact.ENGINE_REQUIRED:
            self.write(artifact.SOURCE + 'deploy/web/engine/' + name,
                       b'\0asm\x01\0\0\0fixture' if name.endswith('.wasm') else 'fixture')
        self.write(artifact.ENGINE_PARTS['bindings'] + '/Example.ts', 'export type Example = string;')

    def pack(self, optimized=True):
        return artifact.pack_engine(self.cache, self.root, optimized=optimized, revision=SHA)

    def test_missing_manifest_refuses_assembled_identity_without_invalidating_engine_cache(self):
        self.pack()
        engine_key = artifact.engine_inputs(self.root)
        (self.root / MANIFEST).unlink()
        with self.assertRaisesRegex(artifact.shared_inputs.ManifestError, 'Missing'):
            artifact.inputs(self.root)
        self.assertEqual(engine_key, artifact.engine_inputs(self.root))
        self.assertEqual(artifact.verify_engine(self.cache, self.root, False)['revision'], SHA)

    def test_stale_manifest_refuses_assembled_identity_until_the_fixture_is_resealed(self):
        before = (self.root / MANIFEST).read_bytes()
        engine_key = artifact.engine_inputs(self.root)
        (self.root / artifact.HUD / 'main.tsx').write_text('changed HUD without regeneration')
        with self.assertRaisesRegex(artifact.shared_inputs.ManifestError, 'Stale HUD input'):
            artifact.inputs(self.root)
        self.assertEqual(before, (self.root / MANIFEST).read_bytes())
        self.assertEqual(engine_key, artifact.engine_inputs(self.root))
        seal(self.root)
        self.assertTrue(artifact.inputs(self.root))

    def test_hud_and_bridge_edits_reuse_engine_but_compiler_inputs_invalidate_it(self):
        engine_key = artifact.engine_inputs(self.root)
        full_key = artifact.inputs(self.root)
        for path in (artifact.HUD + 'main.tsx', artifact.SOURCE + 'react-web/bridge-scene/package.json'):
            self.write(path, 'changed HUD')
        self.assertEqual(engine_key, artifact.engine_inputs(self.root))
        self.assertNotEqual(full_key, artifact.inputs(self.root))
        for path in ('rust-toolchain.toml', *artifact.INPUTS,
                     artifact.SOURCE + 'src/lib.rs', artifact.SOURCE + 'Cargo.lock',
                     artifact.PROTO + 'decentraland/example.proto',
                     artifact.SOURCE + 'deploy/web/engine/gpu_cache.js'):
            with self.subTest(path=path):
                self.write(path, 'changed compiler input')
                self.assertNotEqual(engine_key, artifact.engine_inputs(self.root))
                self.write(path, 'input')

    # The write keys are compiled in: a changed value misses the engine cache, but the Sites
    # import, which runs without the variables, still verifies the build CI made with them.
    def test_segment_write_keys_miss_the_engine_cache_but_not_the_import(self):
        with patch.dict(os.environ, {name: '' for name in artifact.BUILD_ENV}):
            cache_key = artifact.engine_cache_key(self.root)
            keys = artifact.engine_inputs(self.root), artifact.inputs(self.root)
            with patch.dict(os.environ, {'BEVY_SEGMENT_WRITE_KEY': 'key'}):
                self.assertNotEqual(cache_key, artifact.engine_cache_key(self.root))
                self.assertEqual(keys, (artifact.engine_inputs(self.root), artifact.inputs(self.root)))
                engine = self.pack()
            artifact.verify_engine_info(engine, self.root)
            with self.assertRaisesRegex(ValueError, 'build variables'):
                artifact.restore_engine(self.cache, self.root)

    def test_headless_packaging_changes_do_not_invalidate_web_artifacts(self):
        engine_key, full_key = artifact.engine_inputs(self.root), artifact.inputs(self.root)
        self.pack()
        for name in ('apps/bevy-explorer/build-headless.py',
                     artifact.SOURCE + 'deploy/headless/launcher/package.json',
                     artifact.SOURCE + 'deploy/headless/launcher/bin/cli.js',
                     artifact.SOURCE + 'deploy/headless/build-platform-package.js'):
            with self.subTest(path=name):
                self.write(name, 'changed native packaging')
                self.assertEqual(engine_key, artifact.engine_inputs(self.root))
                self.assertEqual(full_key, artifact.inputs(self.root))
                self.assertEqual(artifact.verify_engine(self.cache, self.root, False)['revision'], SHA)

    def test_web_npm_metadata_reuses_engine_but_invalidates_assembled_artifact(self):
        engine_key, full_key = artifact.engine_inputs(self.root), artifact.inputs(self.root)
        self.pack()
        for name in ('package.json', 'package-lock.json'):
            with self.subTest(path=name):
                self.write(artifact.SOURCE + 'deploy/web/' + name, 'changed npm metadata')
                self.assertEqual(engine_key, artifact.engine_inputs(self.root))
                self.assertNotEqual(full_key, artifact.inputs(self.root))
                artifact.verify_engine(self.cache, self.root, False)

    def test_new_web_runtime_assets_and_unknown_source_inputs_invalidate_engine(self):
        engine_key = artifact.engine_inputs(self.root)
        for name in ('deploy/web/engine/new-runtime.js', 'new-build-input'):
            with self.subTest(path=name):
                path = self.write(artifact.SOURCE + name, 'new input')
                subprocess.run(['git', 'add', '.'], cwd=self.root, check=True)
                self.assertNotEqual(engine_key, artifact.engine_inputs(self.root))
                path.unlink()
                subprocess.run(['git', 'add', '-u'], cwd=self.root, check=True)

    def test_round_trip_restores_bindings_and_preserves_compiler_revision(self):
        info = self.pack()
        for name in artifact.ENGINE_PARTS.values():
            shutil.rmtree(self.root / name)
        self.assertEqual(artifact.restore_engine(self.cache, self.root, False), info)
        self.assertEqual(info['revision'], SHA)
        self.assertTrue((self.root / artifact.ENGINE_PARTS['bindings'] / 'Example.ts').is_file())

    def test_optimization_and_source_mismatch_fail_closed(self):
        self.pack(optimized=False)
        artifact.verify_engine(self.cache, self.root)
        with self.assertRaisesRegex(ValueError, 'optimized'):
            artifact.restore_engine(self.cache, self.root, allow_unoptimized=False)
        self.write(artifact.SOURCE + 'src/lib.rs', 'changed Rust')
        with self.assertRaisesRegex(ValueError, 'inputs differ'):
            artifact.restore_engine(self.cache, self.root)

    def test_corruption_missing_files_extra_files_and_symlinks_are_rejected(self):
        for change in ('corrupt', 'missing', 'extra', 'symlink', 'empty-bindings'):
            with self.subTest(change=change):
                self.pack()
                wasm = self.cache / artifact.ENGINE_REQUIRED[0]
                if change == 'corrupt':
                    wasm.write_bytes(wasm.read_bytes() + b'changed')
                elif change == 'missing':
                    wasm.unlink()
                elif change == 'extra':
                    (self.cache / 'pkg/extra.js').write_text('unexpected')
                elif change == 'symlink':
                    wasm.unlink()
                    wasm.symlink_to(self.root / artifact.SOURCE / 'src/lib.rs')
                else:
                    (self.cache / 'bindings/Example.ts').unlink()
                with self.assertRaises(ValueError):
                    artifact.restore_engine(self.cache, self.root)

    def test_warm_build_skips_rust_but_runs_current_web_tests_and_preserves_provenance(self):
        app = self.root / 'apps/bevy-explorer'
        calls = []
        for name in ('index.html', 'app.html', 'engine/headless.html',
                     'bridge-scene/static/BevyExplorerUI/about', 'service_worker.js', 'preview_realm.js'):
            self.write(artifact.SOURCE + 'deploy/web/' + name, 'fixture')
        self.write(artifact.SOURCE + 'LICENSE', 'fixture')
        self.write(artifact.BINDINGS + '/Example.ts', 'export type Example = string;')

        def command(*args, **kwargs):
            calls.append(args)
            if args[0] == 'wasm-pack' or args[:2] == ('bash', './scripts/gen-ts-bindings.sh'):
                self.outputs()
            elif args[0] == 'node':
                self.write(artifact.SOURCE + 'target/sandbox-worker.meta.json',
                           json.dumps({'outputs': {'worker': {'entryPoint': 'worker.js', 'exports': []}}}))
                self.write(artifact.SOURCE + 'deploy/web/engine/pkg/sandbox_worker.bundle.js', 'worker')
            elif len(args) > 2 and args[2] == 'pack':
                artifact.pack(Path(args[3]), self.root, optimized='--unoptimized' not in args)

        with patch.object(build, 'ROOT', self.root), patch.object(build, 'SOURCE', self.source), \
                patch.object(build, 'APP', app), patch.object(build, 'run', side_effect=command), \
                patch.dict(os.environ, {'GITHUB_SHA': SHA}):
            build.web(engine_cache=self.cache)
            self.assertEqual(sum(c[0] == 'wasm-pack' for c in calls), 2)
            # The trees Cargo reads are written before the first compile, and only for a compile.
            rebuild = (sys.executable, str(self.root / 'tooling/patched-sources.py'), 'materialise', '--quiet')
            self.assertLess(calls.index(rebuild), next(i for i, c in enumerate(calls) if c[0] == 'wasm-pack'))
            self.assertTrue(any(c[0] == 'bash' for c in calls))
            self.assertFalse((self.cache / 'pkg/sandbox_worker.bundle.js').exists())
            calls.clear()
            with patch.dict(os.environ, {'GITHUB_SHA': 'b' * 40}):
                build.web(no_opt=True, engine_cache=self.cache)
        self.assertFalse(any(c[0] in ('wasm-pack', 'bash') for c in calls))
        self.assertNotIn(rebuild, calls)
        self.assertNotIn(('npm', 'run', 'test:shared-inputs', '--prefix', 'react-web'), calls)
        self.assertIn(('node', './react-web/scripts/shared-inputs.mjs', '--check'), calls)
        self.assertIn(('npm', 'run', 'test', '--prefix', 'react-web'), calls)
        self.assertIn(('npm', 'run', 'build', '--prefix', 'react-web'), calls)
        self.assertIn(('corepack', 'pnpm@11.27.0', '--dir', 'react-web/bridge-scene', 'run', 'bundle'), calls)
        self.assertIn(('corepack', 'pnpm@11.27.0', '--dir', 'react-web/bridge-scene',
                       'install', '--frozen-lockfile', '--ignore-scripts'), calls)
        self.assertIn(('node', str(self.root / 'tooling/check-local-hashing.mjs'),
                       str(self.source / 'react-web/bridge-scene')), calls)
        self.assertTrue(any(c[0] == 'node' for c in calls))
        meta = artifact.verify(app / 'dist/web', self.root, allow_unoptimized=False)
        self.assertEqual(meta['engine']['revision'], SHA)
        self.assertEqual(meta['build_revision'], 'b' * 40)
        self.assertTrue(meta['optimized'])


if __name__ == '__main__':
    unittest.main()
