"""Unused runner compilers must not change the native Rust cache identity; vendored sources must."""
import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    'bevy_native_cache', Path(__file__).resolve().parents[2] / 'apps/bevy-explorer/prepare-native-cache.py')
cache = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(cache)
ROOT = Path(__file__).resolve().parents[2]
WORKSPACE = 'workspaces: rendering/bevy-explorer/source -> target'
# The key file works because this version hashes each .cargo/config.toml below the workspace into
# the last part of its key. Check that again before a change of this pin.
RUST_CACHE = 'Swatinem/rust-cache@d1527c8f67ef9af357290475d4af4b0fab8f12e0'


class NativeCacheTest(unittest.TestCase):
    def setUp(self):
        self.environment = patch.dict(cache.os.environ, {'RUNNER_ENVIRONMENT': 'github-hosted'})
        self.environment.start()
        self.addCleanup(self.environment.stop)
        folder = tempfile.TemporaryDirectory()
        self.addCleanup(folder.cleanup)
        self.key_file = Path(folder.name) / 'cache-key/.cargo/config.toml'
        for name, value in (('KEY_FILE', self.key_file), ('vendored_trees', lambda: ['a' * 40, 'b' * 40])):
            patcher = patch.object(cache, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)

    def test_retains_active_compiler_with_targets_and_components_on_every_native_platform(self):
        for host in ('x86_64-unknown-linux-gnu', 'x86_64-pc-windows-msvc',
                     'aarch64-apple-darwin', 'x86_64-apple-darwin'):
            with self.subTest(host=host):
                active = f'1.99.1-{host}'
                stable, nightly = f'stable-{host}', f'nightly-{host}'
                compiler = f'rustc 1.99.1\nrelease: 1.99.1\nhost: {host}'
                installed = {active: {'rustc', 'clippy', 'rustfmt', 'x86_64-apple-darwin'},
                             stable: {'rustc'}, nightly: {'rustc'}}
                expected = installed[active].copy()

                def run(args, **kwargs):
                    self.assertTrue(kwargs['check'])
                    self.assertEqual(args[:3], ['rustup', 'toolchain', 'uninstall'])
                    del installed[args[3]]

                with patch.object(cache, 'output', side_effect=[
                        active + ' (default)', compiler, '\n'.join(installed)]), \
                        patch.object(cache.subprocess, 'run', side_effect=run):
                    cache.prepare()
                self.assertEqual(installed, {active: expected})

    def test_already_prepared_runner_does_not_mutate_toolchains(self):
        active = '1.99.1-x86_64-unknown-linux-gnu'
        with patch.object(cache, 'output', side_effect=[active, 'release: 1.99.1\nhost: x86_64-unknown-linux-gnu', active]), \
                patch.object(cache.subprocess, 'run') as run:
            cache.prepare()
        run.assert_not_called()
        self.assertEqual(self.key_file.read_text(), ''.join(
            f'# {path} {tree}\n' for path, tree in zip(cache.VENDORED, ('a' * 40, 'b' * 40))))

    def test_developer_and_self_hosted_environments_are_never_modified(self):
        for environment in ('', 'self-hosted'):
            with self.subTest(environment=environment), \
                    patch.dict(cache.os.environ, {'RUNNER_ENVIRONMENT': environment}), \
                    patch.object(cache, 'output') as output, patch.object(cache.subprocess, 'run') as run:
                with self.assertRaisesRegex(RuntimeError, 'disposable GitHub-hosted'):
                    cache.prepare()
                output.assert_not_called()
                run.assert_not_called()
                self.assertFalse(self.key_file.exists())

    def test_rolling_or_mismatched_compiler_is_rejected_before_removing_anything(self):
        for active, release in [('stable-x86_64-unknown-linux-gnu', '1.99.1'),
                                ('1.99.1-x86_64-unknown-linux-gnu', '1.99.0'),
                                ('nightly-x86_64-unknown-linux-gnu', '1.100.0-nightly')]:
            with self.subTest(active=active, release=release), \
                    patch.object(cache, 'output', side_effect=[active, f'release: {release}\nhost: x86_64-unknown-linux-gnu']), \
                    patch.object(cache.subprocess, 'run') as run:
                with self.assertRaisesRegex(RuntimeError, 'numbered native Rust toolchain'):
                    cache.prepare()
                run.assert_not_called()
                self.assertFalse(self.key_file.exists())

    def test_active_compiler_must_be_in_the_inventory_before_removing_anything(self):
        with patch.object(cache, 'output', side_effect=[
                '1.99.1-x86_64-unknown-linux-gnu', 'release: 1.99.1\nhost: x86_64-unknown-linux-gnu',
                'stable-x86_64-unknown-linux-gnu']), patch.object(cache.subprocess, 'run') as run:
            with self.assertRaisesRegex(RuntimeError, 'absent'):
                cache.prepare()
            run.assert_not_called()

    def test_failed_cleanup_stops_before_cache_restoration(self):
        active = '1.99.1-x86_64-unknown-linux-gnu'
        with patch.object(cache, 'output', side_effect=[
                active, 'release: 1.99.1\nhost: x86_64-unknown-linux-gnu', active + '\nstable-x86_64-unknown-linux-gnu']), \
                patch.object(cache.subprocess, 'run', side_effect=subprocess.CalledProcessError(1, 'rustup')):
            with self.assertRaises(subprocess.CalledProcessError):
                cache.prepare()


class NativeCacheKeyTest(unittest.TestCase):
    def test_the_key_file_changes_with_each_vendored_tree_and_only_then(self):
        with tempfile.TemporaryDirectory() as folder:
            key_file = Path(folder) / 'cache-key/.cargo/config.toml'
            texts = []
            for trees in (['a' * 40, 'b' * 40], ['a' * 40, 'b' * 40], ['c' * 40, 'b' * 40], ['a' * 40, 'c' * 40]):
                with patch.object(cache, 'KEY_FILE', key_file), patch.object(cache, 'vendored_trees', lambda: trees):
                    cache.write_cache_key()
                texts.append(key_file.read_bytes())
        self.assertEqual(texts[0], texts[1])
        self.assertEqual(len(set(texts)), 3)
        # A comment only: Cargo would get no setting from it.
        self.assertTrue(all(line.startswith(b'# ') for line in texts[0].splitlines()))

    def test_the_trees_are_the_path_dependencies_outside_the_engine_workspace(self):
        source = ROOT / 'rendering/bevy-explorer/source'
        outside = set()
        for manifest in [source / 'Cargo.toml', *source.glob('crates/*/Cargo.toml')]:
            for line in manifest.read_text().splitlines():
                if 'path = "' not in line or line.lstrip().startswith('#'):
                    continue
                target = (manifest.parent / line.split('path = "', 1)[1].split('"', 1)[0]).resolve()
                if source.resolve() not in target.parents:
                    outside.add(target.relative_to(ROOT.resolve()).as_posix())
        self.assertTrue(outside)
        for path in outside:
            self.assertTrue(any(path == tree or path.startswith(tree + '/') for tree in cache.VENDORED), path)
        for tree in cache.VENDORED:
            self.assertTrue((ROOT / tree).is_dir(), tree)

    def test_identity_comes_from_the_commit_and_rejects_anything_else(self):
        with patch.object(cache, 'output', side_effect=['a' * 40, 'b' * 40]) as output:
            self.assertEqual(cache.vendored_trees(), ['a' * 40, 'b' * 40])
        self.assertEqual([call.args[-1] for call in output.call_args_list],
                         [f'HEAD:{path}' for path in cache.VENDORED])
        with patch.object(cache, 'output', side_effect=['a' * 40, 'HEAD:missing']):
            with self.assertRaisesRegex(RuntimeError, 'no Git identity'):
                cache.vendored_trees()

    def test_every_cache_of_the_engine_workspace_follows_the_key_file(self):
        self.assertEqual(cache.KEY_FILE, ROOT / 'rendering/bevy-explorer/source/cache-key/.cargo/config.toml')
        ignored = (ROOT / 'rendering/bevy-explorer/source/.gitignore').read_text().splitlines()
        self.assertIn('/cache-key', ignored)
        uses = 0
        for workflow in sorted((ROOT / '.github/workflows').glob('*.yml')):
            before, *steps = workflow.read_text().split('- uses: Swatinem/rust-cache@')
            for step in steps:
                if WORKSPACE in step.split('\n      - ', 1)[0]:
                    uses += 1
                    self.assertTrue(('Swatinem/rust-cache@' + step).startswith(RUST_CACHE), workflow.name)
                    self.assertIn('prepare-native-cache.py', before.rsplit('actions/checkout@', 1)[-1], workflow.name)
                before = step
        self.assertGreaterEqual(uses, 6)


if __name__ == '__main__':
    unittest.main()
