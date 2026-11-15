"""The rebuilt Bevy tree keeps its cached build across CI checkouts, and only unchanged files count."""
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
import unittest

SPEC = importlib.util.spec_from_file_location('bevy_mtimes', Path(__file__).parents[1] / 'bevy-mtimes.py')
mtimes = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(mtimes)

SECOND = 10**9


class BevyMtimesTest(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.git('init', '-q')
        self.git('config', 'maintenance.auto', 'false')
        self.git('config', 'gc.auto', '0')
        self.write('.gitignore', f'target/\n/{mtimes.TREE}/\n/{mtimes.FORKS}/\n')
        self.write(mtimes.TREE + '/crates/bevy_scene/src/lib.rs', 'pub fn spawn() {}\n')
        self.write(mtimes.TREE + '/crates/bevy_ecs/src/lib.rs', 'pub fn world() {}\n')
        self.write('rendering/bevy-explorer/source/src/lib.rs', 'fn main() {}\n')
        self.write('library/vendor/rusty_enet/src/lib.rs', 'pub fn host() {}\n')
        self.commit()
        self.target = self.root / 'rendering/bevy-explorer/source/target'
        self.profiles = [self.target / 'ci-test', self.target / 'x86_64-unknown-linux-gnu' / 'release']
        for profile in self.profiles:
            (profile / '.fingerprint').mkdir(parents=True)

    def git(self, *args):
        return subprocess.check_output(['git', '-c', 'user.name=CI', '-c', 'user.email=ci@example.invalid', *args],
                                       cwd=self.root, text=True).strip()

    def write(self, name, value):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(value)
        return path

    def commit(self):
        self.git('add', '.')
        self.git('commit', '-qm', 'fixture')

    def set_mtime(self, name, ns):
        os.utime(self.root / name, ns=(ns, ns))

    def mtime(self, name):
        return (self.root / name).stat().st_mtime_ns

    def checkout_now(self):
        """What a CI job does: the checkout writes every file and directory again and the patch tool writes its
        trees, so every mtime is now."""
        now = time.time_ns()
        rebuilt = [path.relative_to(self.root).as_posix() for tree in mtimes.MATERIALISED
                   for path in (self.root / tree).rglob('*') if path.is_file()]
        tracked = self.git('ls-files').splitlines()
        for name in {*tracked, *rebuilt, *(parent.as_posix() for name in tracked for parent in Path(name).parents)}:
            self.set_mtime(name, now)
        return now

    def test_unchanged_files_get_their_recorded_time_back(self):
        scene = mtimes.TREE + '/crates/bevy_scene/src/lib.rs'
        self.set_mtime(scene, 1_700_000_000 * SECOND + 750_000_000)
        mtimes.record(self.root, self.target)
        self.checkout_now()
        self.assertEqual(mtimes.restore(self.root, self.target), (4, 0))
        self.assertEqual(self.mtime(scene), 1_700_000_000 * SECOND)

    def test_changed_files_keep_their_checkout_time(self):
        scene, ecs = mtimes.TREE + '/crates/bevy_scene/src/lib.rs', mtimes.TREE + '/crates/bevy_ecs/src/lib.rs'
        self.set_mtime(ecs, 1_700_000_000 * SECOND)
        mtimes.record(self.root, self.target)
        self.write(scene, 'pub fn spawn_sliced() {}\n')
        now = self.checkout_now()
        self.assertEqual(mtimes.restore(self.root, self.target), (3, 1))
        self.assertEqual(self.mtime(scene), now)
        self.assertEqual(self.mtime(ecs), 1_700_000_000 * SECOND)

    def test_recorded_times_are_whole_seconds(self):
        # A cache archive may keep whole seconds only; a source a fraction newer than its
        # truncated build stamp would make Cargo rebuild it.
        self.set_mtime(mtimes.TREE + '/crates/bevy_ecs/src/lib.rs', 1_700_000_000 * SECOND + 1)
        mtimes.record(self.root, self.target)
        manifest = json.loads((self.profiles[0] / '.fingerprint' / mtimes.MANIFEST).read_text())
        self.assertTrue(all(ns % SECOND == 0 for _, ns in manifest['files'].values()))

    def test_every_profile_gets_the_manifest(self):
        self.assertEqual(mtimes.record(self.root, self.target), 2)
        for profile in self.profiles:
            self.assertTrue((profile / '.fingerprint' / mtimes.MANIFEST).is_file())

    def test_restore_without_a_manifest_changes_nothing(self):
        now = self.checkout_now()
        self.assertEqual(mtimes.restore(self.root, self.target), (0, 4))
        self.assertEqual(self.mtime(mtimes.TREE + '/crates/bevy_ecs/src/lib.rs'), now)

    def test_unchanged_engine_sources_keep_their_cached_time(self):
        source = 'rendering/bevy-explorer/source/src/lib.rs'
        self.set_mtime(source, 1_700_000_000 * SECOND)
        mtimes.record(self.root, self.target)
        now = self.checkout_now()
        mtimes.restore(self.root, self.target)
        self.assertEqual(self.mtime(source), 1_700_000_000 * SECOND)

    def test_changed_engine_and_protocol_sources_keep_the_checkout_time(self):
        engine = 'rendering/bevy-explorer/source/src/lib.rs'
        proto = 'library/protocol/proto/example.proto'
        self.write(proto, 'message Example {}\n')
        self.commit()
        mtimes.record(self.root, self.target)
        self.write(engine, 'fn changed() {}\n')
        self.write(proto, 'message Changed {}\n')
        self.commit()
        now = self.checkout_now()
        self.assertEqual(mtimes.restore(self.root, self.target), (3, 2))
        self.assertEqual(self.mtime(engine), now)
        self.assertEqual(self.mtime(proto), now)

    def test_in_tree_crates_keep_their_cached_time_until_they_change(self):
        # Path dependencies tracked as exact bytes: Cargo judges them by file times too.
        kept, changed = (f'rendering/bevy-explorer/vendor/{name}/src/lib.rs' for name in ('boimp', 'bevy_dui'))
        self.write(kept, 'pub fn import() {}\n')
        self.write(changed, 'pub fn layout() {}\n')
        self.commit()
        for name in (kept, changed):
            self.set_mtime(name, 1_700_000_000 * SECOND)
        mtimes.record(self.root, self.target)
        self.write(changed, 'pub fn layout_changed() {}\n')
        self.commit()
        now = self.checkout_now()
        self.assertEqual(mtimes.restore(self.root, self.target), (5, 1))
        self.assertEqual(self.mtime(kept), 1_700_000_000 * SECOND)
        self.assertEqual(self.mtime(changed), now)

    def materialise(self):
        """What tooling/patched-sources.py leaves: ignored trees, a stamp in each, and tracked inputs beside them."""
        self.write('rendering/bevy-explorer/patches/winit/source.toml', 'name = "winit"\n')
        self.commit()
        self.write(mtimes.TREE + '/.patched-stamp.json', '{"tree": "1"}\n')
        (self.root / mtimes.FORKS / 'winit/src').mkdir(parents=True)
        (self.root / mtimes.FORKS / 'winit/src/lib.rs').write_bytes(b'pub fn window() {}\r\n')
        self.write(mtimes.FORKS + '/winit/.patched-stamp.json', '{"tree": "2"}\n')
        return [mtimes.TREE + '/crates/bevy_scene/src/lib.rs', mtimes.TREE + '/crates/bevy_ecs/src/lib.rs',
                mtimes.TREE + '/.patched-stamp.json', mtimes.FORKS + '/winit/src/lib.rs',
                mtimes.FORKS + '/winit/.patched-stamp.json']

    def rewrite(self, names):
        """A new materialise on a fresh checkout: the same bytes, written now."""
        now = self.checkout_now()
        for name in names:
            path = self.root / name
            path.write_bytes(path.read_bytes())
            self.set_mtime(name, now)
        return now

    def test_rebuilt_trees_are_matched_by_their_bytes(self):
        written = self.materialise()
        self.assertEqual(self.git('ls-files', mtimes.TREE, mtimes.FORKS), '')
        for name in written:
            self.set_mtime(name, 1_700_000_000 * SECOND + 5)
        mtimes.record(self.root, self.target)
        manifest = json.loads((self.profiles[0] / '.fingerprint' / mtimes.MANIFEST).read_text())
        fork = mtimes.FORKS + '/winit/src/lib.rs'
        self.assertEqual(manifest['files'][fork], [self.git('hash-object', '--no-filters', fork), 1_700_000_000 * SECOND])
        self.rewrite(written)
        self.assertEqual(mtimes.restore(self.root, self.target), (8, 0))
        for name in written:
            self.assertEqual(self.mtime(name), 1_700_000_000 * SECOND)

    def test_changed_rebuilt_files_keep_the_time_they_were_written(self):
        written = self.materialise()
        for name in written:
            self.set_mtime(name, 1_700_000_000 * SECOND)
        mtimes.record(self.root, self.target)
        now = self.rewrite(written)
        fork, stamp = mtimes.FORKS + '/winit/src/lib.rs', mtimes.FORKS + '/winit/.patched-stamp.json'
        (self.root / fork).write_bytes(b'pub fn window() {}\n')
        self.write(stamp, '{"tree": "3"}\n')
        self.write(mtimes.FORKS + '/winit/src/new.rs', '')
        for name in (fork, stamp, mtimes.FORKS + '/winit/src/new.rs'):
            self.set_mtime(name, now)
        self.assertEqual(mtimes.restore(self.root, self.target), (6, 3))
        self.assertEqual(self.mtime(fork), now)
        self.assertEqual(self.mtime(stamp), now)
        self.assertEqual(self.mtime(mtimes.TREE + '/.patched-stamp.json'), 1_700_000_000 * SECOND)

    def test_an_unchanged_patch_series_keeps_its_cached_time(self):
        # Cargo reads the time of a directory named as an input, and of every file in it.
        kept, grown = (f'{mtimes.PATCHES}/{name}/series' for name in ('winit', 'opener'))
        for series in (kept, grown):
            self.write(series + '/0001-first.patch', 'first\n')
        self.commit()
        for name in (kept, grown, kept + '/0001-first.patch', grown + '/0001-first.patch'):
            self.set_mtime(name, 1_700_000_000 * SECOND + 5)
        mtimes.record(self.root, self.target)
        self.write(grown + '/0002-second.patch', 'second\n')
        self.commit()
        now = self.checkout_now()
        self.assertEqual(mtimes.restore(self.root, self.target), (7, 2))
        for name in (kept, kept + '/0001-first.patch', grown + '/0001-first.patch'):
            self.assertEqual(self.mtime(name), 1_700_000_000 * SECOND)
        for name in (grown, grown + '/0002-second.patch'):
            self.assertEqual(self.mtime(name), now)

    def test_unchanged_vendored_enet_sources_keep_their_cached_time(self):
        source = 'library/vendor/rusty_enet/src/lib.rs'
        self.set_mtime(source, 1_700_000_000 * SECOND)
        mtimes.record(self.root, self.target)
        self.checkout_now()
        mtimes.restore(self.root, self.target)
        self.assertEqual(self.mtime(source), 1_700_000_000 * SECOND)

    def test_the_manifest_survives_the_native_cache_pruning(self):
        # tooling/bevy-target-cache.py prunes the .fingerprint entries of workspace crates, <crate>-<hash>.
        spec = importlib.util.spec_from_file_location('bevy_target_cache', Path(__file__).parents[1] / 'bevy-target-cache.py')
        cache = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cache)
        if shutil.which('cargo') is None:
            self.skipTest('cargo reads the workspace members')
        names, _ = cache.workspace()
        self.assertFalse(cache.owned(mtimes.MANIFEST, names))


if __name__ == '__main__':
    unittest.main()
