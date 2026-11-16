"""The Explorer's native Cargo cache: keyed by what Cargo's fingerprints depend on, with the workspace crates in it."""
import importlib.util
import io
import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location('bevy_target_cache', ROOT / 'tooling/bevy-target-cache.py')
cache = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(cache)

ENV = {'RUNNER_OS': 'Linux', 'RUNNER_ARCH': 'X64', 'CC': '/opt/hostedtoolcache/llvm-21.1.8/bin/clang',
       'CXX': '/opt/hostedtoolcache/llvm-21.1.8/bin/clang++', 'CEF_PATH': '/opt/cef',
       'LD_LIBRARY_PATH': '/opt/cef', 'PATH': os.environ['PATH']}
RUSTC = 'rustc 1.99.1 (abcdef012 2026-10-01)\nhost: x86_64-unknown-linux-gnu\n'


class KeyTest(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name).resolve()
        self.source = self.root / 'rendering/bevy-explorer/source'
        self.git('init', '-q')
        self.git('config', 'maintenance.auto', 'false')
        self.git('config', 'gc.auto', '0')
        for name, text in {'rendering/bevy-explorer/patches/bevy/series/0001-world.patch': 'world\n',
                           'rendering/bevy-explorer/vendor/boimp/src/lib.rs': 'pub fn import() {}\n',
                           'library/vendor/rusty_enet/src/lib.rs': 'pub fn host() {}\n',
                           'rendering/bevy-explorer/source/vendor/basisu/src/lib.rs': 'pub fn transcode() {}\n',
                           'rendering/bevy-explorer/source/Cargo.lock': 'version = 4\n',
                           'rendering/bevy-explorer/source/Cargo.toml': '[workspace]\nmembers = ["crates/*"]\n',
                           'rendering/bevy-explorer/source/.cargo/config.toml': '[env]\nRUST_BACKTRACE = "1"\n',
                           '.cargo/config.toml': '[env]\nDCL_ONE_ROOT = { value = "", relative = true }\n',
                           'rendering/bevy-explorer/source/crates/common/Cargo.toml': '[package]\nname = "common"\n',
                           'rendering/bevy-explorer/source/src/lib.rs': 'fn main() {}\n',
                           'library/protocol/proto/decentraland/common/vectors.proto': 'syntax = "proto3";\n'}.items():
            self.write(name, text)
        self.commit()
        manifests = [self.source / 'Cargo.toml', self.source / 'crates/common/Cargo.toml']
        for target, value in (('workspace', manifests), ('rustc', RUSTC)):
            patcher = patch.object(cache, target, return_value=value)
            patcher.start()
            self.addCleanup(patcher.stop)

    def git(self, *args):
        return subprocess.check_output(['git', '-c', 'user.name=CI', '-c', 'user.email=ci@example.invalid', *args],
                                       cwd=self.root, text=True).strip()

    def write(self, name, text):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def commit(self):
        self.git('add', '.')
        self.git('commit', '-qm', 'fixture')

    def key(self, lane='test', env=ENV):
        return cache.key(lane, env, self.root, self.source)

    def test_key_names_lane_platform_compiler_tools_sources_and_members(self):
        exact, fallbacks = self.key()
        self.assertRegex(exact, r'^bevy-native-v1-test-Linux-X64-[0-9a-f]{16}-[0-9a-f]{16}-[0-9a-f]{16}-[0-9a-f]{16}$')
        self.assertEqual(fallbacks, [exact[:-17], exact[:-33], exact[:-50]])
        self.assertNotEqual(self.key('clippy')[0], exact)
        self.assertNotEqual(self.key(env={**ENV, 'RUNNER_OS': 'macOS', 'RUNNER_ARCH': 'ARM64'})[0], exact)

    def test_the_tool_environment_changes_the_tools_digest_only(self):
        exact, fallbacks = self.key()
        moved, moved_fallbacks = self.key(env={**ENV, 'CC': '/opt/hostedtoolcache/llvm-22.1.0/bin/clang'})
        self.assertNotEqual(moved, exact)
        self.assertEqual(moved[-16:], exact[-16:])
        self.assertEqual(moved[-33:], exact[-33:])
        self.assertNotEqual(moved_fallbacks[0], fallbacks[0])
        self.assertNotEqual(moved_fallbacks[1], fallbacks[1])
        self.assertEqual(moved_fallbacks[2], fallbacks[2])
        self.assertEqual(self.key(env={**ENV, 'LD_LIBRARY_PATH': '/elsewhere', 'PATH': '/opt/bin'})[0], exact)

    def test_cargo_inputs_and_vendored_trees_change_the_sources_digest_only(self):
        exact, fallbacks = self.key()
        for name, text in (('rendering/bevy-explorer/source/Cargo.lock', 'version = 4\n[[package]]\nname = "cc"\n'),
                           ('rendering/bevy-explorer/source/crates/common/Cargo.toml', '[package]\nname = "common"\nversion = "0.2.0"\n'),
                           ('rendering/bevy-explorer/source/.cargo/config.toml', '[env]\nRUST_BACKTRACE = "full"\n'),
                           ('.cargo/config.toml', '[build]\nrustflags = ["-Cdebuginfo=0"]\n'),
                           ('rendering/bevy-explorer/patches/bevy/series/0001-world.patch', 'the world\n'),
                           ('rendering/bevy-explorer/patches/bevy/source.toml', 'commit = "1"\n'),
                           ('rendering/bevy-explorer/vendor/boimp/src/lib.rs', 'pub fn import() { }\n'),
                           ('library/vendor/rusty_enet/src/lib.rs', 'pub fn host() { }\n'),
                           ('rendering/bevy-explorer/source/vendor/basisu/src/lib.rs', 'pub fn transcode() { }\n')):
            with self.subTest(name=name):
                self.write(name, text)
                self.commit()
                changed, changed_fallbacks = self.key()
                self.assertNotEqual(changed, exact)
                self.assertNotEqual(changed_fallbacks[0], fallbacks[0])
                self.assertEqual(changed_fallbacks[1:], fallbacks[1:])
                exact, fallbacks = changed, changed_fallbacks

    def test_workspace_sources_and_protocol_definitions_change_the_members_digest_only(self):
        # The entry holds the workspace crates: main saves one for each state of what they compile from,
        # and a pull request finds the newest of them under the unchanged first fallback.
        exact, fallbacks = self.key()
        for name, text in (('rendering/bevy-explorer/source/src/lib.rs', 'fn main() { }\n'),
                           ('library/protocol/proto/decentraland/common/vectors.proto', 'syntax = "proto3";\npackage common;\n')):
            with self.subTest(name=name):
                self.write(name, text)
                self.commit()
                changed, changed_fallbacks = self.key()
                self.assertNotEqual(changed, exact)
                self.assertEqual(changed[:-16], exact[:-16])
                self.assertEqual(changed_fallbacks, fallbacks)
                exact = changed

    def test_a_file_outside_the_cargo_inputs_changes_no_key(self):
        exact = self.key()[0]
        self.write('docs/development.md', 'notes\n')
        self.commit()
        self.assertEqual(self.key()[0], exact)

    def test_a_compiler_change_shares_no_fallback(self):
        exact, fallbacks = self.key()
        with patch.object(cache, 'rustc', return_value='rustc 1.100.0 (123456789 2026-11-20)\n'):
            bumped, bumped_fallbacks = self.key()
        self.assertNotEqual(bumped, exact)
        self.assertFalse(set(bumped_fallbacks) & set(fallbacks))

    def test_the_key_command_writes_github_output_in_its_multiline_form(self):
        exact, fallbacks = self.key()
        output = self.root / 'output'
        with patch('sys.argv', ['bevy-target-cache.py', 'key', '--lane', 'test']), \
                patch.dict(os.environ, {**ENV, 'GITHUB_OUTPUT': str(output)}, clear=True), \
                patch.object(cache, 'ROOT', self.root), patch.object(cache, 'SOURCE', self.source), \
                patch('sys.stdout', new_callable=io.StringIO) as stdout:
            cache.main()
        self.assertEqual(output.read_text(), f'key={exact}\ndependencies={fallbacks[0]}\nrestore-keys<<KEYS\n'
                                             f'{fallbacks[0]}\n{fallbacks[1]}\n{fallbacks[2]}\nKEYS\n')
        self.assertTrue(exact.startswith(fallbacks[0] + '-'))
        self.assertIn(exact, stdout.getvalue())


class PruneTest(unittest.TestCase):
    KEPT = ('release/deps/libbevy_ecs-1a.rlib', 'release/deps/libbevy_ecs-1a.rmeta', 'release/deps/bevy_ecs-1a.d',
            'release/deps/libbasisu-2b.rlib', 'release/deps/liblibsqlite3_sys-3c.rlib',
            'release/build/openssl-sys-4d/output', 'release/build/openssl-sys-4d/out/lib/libssl.a',
            'release/build/basisu-2b/output', 'release/.fingerprint/bevy_ecs-1a/lib-bevy_ecs',
            'release/.fingerprint/openssl-sys-4d/run-build-script-build-script-build', 'release/.fingerprint/bevy-mtimes.json',
            'ci-test/deps/libbevy_app-5e.rlib', 'ci-test/.fingerprint/bevy_app-5e/lib-bevy_app',
            'x86_64-unknown-linux-gnu/release/deps/libcc-6f.rlib', 'x86_64-unknown-linux-gnu/release/.fingerprint/cc-6f/lib-cc',
            'CACHEDIR.TAG', '.rustc_info.json')
    # The workspace crates' artifacts, build script outputs and fingerprints stay: a later run reuses them.
    WORKSPACE = ('release/deps/libwebgpu_build-7a.rlib', 'release/deps/webgpu_build-7a.d', 'release/deps/benchmark-8b',
                 'release/deps/benchmark-8b.exe', 'release/deps/benchmark-8b.pdb', 'release/deps/libharness_util-9c.rmeta',
                 'release/build/decentra-bevy-7a/output', 'release/build/decentra-bevy-7a/build_script_build-7a',
                 'release/.fingerprint/decentra-bevy-7a/lib-webgpu_build', 'release/.fingerprint/benchmark-8b/bin-benchmark',
                 'ci-test/deps/webgpu_build-7b', 'ci-test/.fingerprint/decentra-bevy-7b/test-lib-webgpu_build',
                 'x86_64-unknown-linux-gnu/release/deps/libdcl_scene_wasm-0d.rlib')
    PRUNED = ('release/incremental/webgpu_build-xyz/s-abc.lock', 'release/examples/demo-1', 'release/avatar_render',
              'release/.cargo-lock', 'ci-test/webgpu_build', 'x86_64-unknown-linux-gnu/release/dcl_scene_wasm.wasm',
              'cargo-timings/cargo-timing.html', 'native-test-artifacts.jsonl', 'sccache-stats.json', 'tmp/scratch')
    # The Cargo home is the runner's own: the prune removes nothing in it, saved path or not.
    HOME_KEPT = ('registry/index/index.crates.io-1cd66030c949c28d/config.json',
                 'registry/cache/index.crates.io-1cd66030c949c28d/cc-1.2.57.crate', 'git/db/livekit-abc/HEAD', 'bin/export-cef-dir',
                 'registry/src/index.crates.io-1cd66030c949c28d/cc-1.2.57/src/lib.rs',
                 'git/checkouts/livekit-abc/def/Cargo.toml', '.package-cache', '.package-cache-mutate', 'credentials.toml')

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        # Resolved, as the prune command resolves its target: the macOS temporary directory is behind a link.
        root = Path(temporary.name).resolve()
        self.target = root / 'target'
        self.home = root / 'cargo-home'
        for name in self.KEPT + self.WORKSPACE + self.PRUNED:
            self.write(self.target / name)
        for name in self.HOME_KEPT:
            self.write(self.home / name)

    def write(self, path):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text('x')

    def assert_pruned(self):
        for name in self.KEPT + self.WORKSPACE:
            self.assertTrue((self.target / name).exists(), name)
        for name in self.PRUNED:
            self.assertFalse((self.target / name).exists(), name)
        for name in self.HOME_KEPT:
            self.assertTrue((self.home / name).exists(), name)
        for directory in ('release/incremental', 'release/examples', 'tmp', 'cargo-timings'):
            self.assertFalse((self.target / directory).exists(), directory)

    def test_prune_keeps_every_crates_artifacts_and_the_cargo_home_and_drops_final_binaries_and_reports(self):
        removed = []
        cache.prune_target(self.target, removed)
        self.assert_pruned()
        for name in self.PRUNED:
            path = (self.target / name).resolve()
            self.assertTrue(any(path == gone or gone in path.parents for gone in removed), name)
        for name in self.KEPT + self.WORKSPACE + tuple(f'../cargo-home/{name}' for name in self.HOME_KEPT):
            path = (self.target / name).resolve()
            self.assertFalse(any(path == gone or gone in path.parents for gone in removed), name)

    def test_the_prune_command_reports_what_it_removed(self):
        with patch('sys.argv', ['bevy-target-cache.py', 'prune', str(self.target)]), \
                patch.dict(os.environ, {'CARGO_HOME': str(self.home)}), \
                patch('sys.stdout', new_callable=io.StringIO) as stdout:
            cache.main()
        self.assert_pruned()
        self.assertRegex(stdout.getvalue(), r'^\d+ entries pruned; \d+ MB left in \S+\n$')


class WorkflowTest(unittest.TestCase):
    """The native jobs key after their tool steps, build after the restore and save from main's first attempts;
    a pull request saves only when the restored entry was not built for its dependencies."""
    WORKFLOW = (ROOT / '.github/workflows/bevy-explorer-ci.yml').read_text()
    GUARD = ("        if: >-\n"
             "          github.run_attempt == '1' && steps.target-cache.outputs.cache-hit != 'true' &&\n"
             "          (github.ref == 'refs/heads/main' || (github.event_name == 'pull_request' &&\n"
             "          !startsWith(steps.target-cache.outputs.cache-matched-key, steps.target-cache-key.outputs.dependencies)))\n")
    PATHS = ('          path: |\n            ~/.cargo/registry/index/\n            ~/.cargo/registry/cache/\n'
             '            ~/.cargo/git/db/\n            rendering/bevy-explorer/source/target/\n')

    def job(self, name):
        return re.search(rf'^  {name}:\n(.*?)(?=^  [\w-]+:|\Z)', self.WORKFLOW, re.M | re.S).group(1)

    def test_no_explorer_job_uses_rust_cache(self):
        self.assertNotIn('Swatinem/rust-cache', self.WORKFLOW)
        self.assertNotIn('prepare-native-cache.py', self.WORKFLOW)

    def test_native_jobs_restore_after_their_tool_steps_and_save_after_their_uploads(self):
        for name, lane, last_tool, build in (('test', 'test', 'Export CEF distribution', 'cargo test --manifest-path vendor/basisu'),
                                             ('headless-smoke', 'headless', 'Install clang for webrtc-sys', 'build-headless.py'),
                                             ('clippy', 'clippy', 'Install clang for webrtc-sys', 'cargo clippy')):
            with self.subTest(job=name):
                job = self.job(name)
                self.assertIn("CARGO_INCREMENTAL: '0'", job)
                key = job.index(f'bevy-target-cache.py" key --lane {lane}')
                self.assertLess(job.index('dtolnay/rust-toolchain'), key)
                self.assertLess(job.index(f'name: {last_tool}'), key)
                restore = job.index('actions/cache/restore@', key)
                self.assertEqual(job.count('key: ${{ steps.target-cache-key.outputs.key }}'), 2)
                self.assertIn('restore-keys: ${{ steps.target-cache-key.outputs.restore-keys }}', job[restore:])
                mtimes = job.index('bevy-mtimes.py" restore target', restore)
                self.assertLess(mtimes, job.index(build))
                record = job.index('bevy-mtimes.py" record target')
                prune = job.index('bevy-target-cache.py" prune target')
                save = job.index('actions/cache/save@', prune)
                self.assertLess(job.index(build), record)
                self.assertLess(record, prune)
                self.assertLess(prune, save)
                if 'actions/upload-artifact@' in job:
                    self.assertLess(job.rindex('actions/upload-artifact@'), prune)
                self.assertEqual(job.count(self.GUARD), 2)
                self.assertEqual(job.count(self.PATHS), 2)


if __name__ == '__main__':
    unittest.main()
