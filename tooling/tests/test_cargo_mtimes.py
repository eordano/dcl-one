import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / 'cargo-mtimes.py'
spec = importlib.util.spec_from_file_location('cargo_mtimes', SCRIPT)
mtimes = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mtimes)
OLD = 1_600_000_000 * 10**9


def git(root, *args):
    subprocess.run(['git', '-c', 'user.name=t', '-c', 'user.email=t@example.invalid', *args], cwd=root, check=True,
                   capture_output=True)


class CargoMtimesTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name) / 'repo'
        self.target = Path(self.tmp.name) / 'target'
        for name in ('same/a.rs', 'same/deep/b.sql', 'edited/c.rs', 'edited/kept.rs', 'grows/d.rs'):
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(name)
        git(self.root, 'init', '-q')
        git(self.root, 'add', '.')
        git(self.root, 'commit', '-qm', 'one')
        self.set_all(OLD)
        mtimes.record(self.root, self.target)

    def tearDown(self):
        self.tmp.cleanup()

    def set_all(self, ns):
        for path in [self.root, *self.root.rglob('*')]:
            if '.git' not in path.parts:
                os.utime(path, ns=(ns, ns))

    def time(self, name):
        return (self.root / name).stat().st_mtime_ns

    def test_an_unchanged_checkout_gets_every_time_back(self):
        self.set_all(OLD + 500 * 10**9)
        restored, left = mtimes.restore(self.root, self.target)
        self.assertEqual(left, 0)
        self.assertEqual(restored, 9)
        for name in ('same', 'same/a.rs', 'same/deep', 'same/deep/b.sql', 'edited/c.rs', 'grows'):
            self.assertEqual(self.time(name), OLD, name)

    def test_a_changed_file_and_every_folder_above_it_keep_the_checkout_time(self):
        (self.root / 'edited/c.rs').write_text('changed')
        (self.root / 'grows/new.rs').write_text('new')
        (self.root / 'same/deep/b.sql').unlink()
        git(self.root, 'add', '-A')
        git(self.root, 'commit', '-qm', 'two')
        now = OLD + 500 * 10**9
        self.set_all(now)
        mtimes.restore(self.root, self.target)
        for name in ('edited/c.rs', 'edited', 'grows/new.rs', 'grows', 'same/deep', 'same'):
            self.assertEqual(self.time(name), now, name)
        for name in ('edited/kept.rs', 'grows/d.rs', 'same/a.rs'):
            self.assertEqual(self.time(name), OLD, name)

    def test_no_manifest_changes_nothing(self):
        (self.target / mtimes.MANIFEST).unlink()
        now = OLD + 500 * 10**9
        self.set_all(now)
        self.assertEqual(mtimes.restore(self.root, self.target)[0], 0)
        self.assertEqual(self.time('same/a.rs'), now)

    def test_recorded_times_are_whole_seconds(self):
        os.utime(self.root / 'same/a.rs', ns=(OLD + 123456789, OLD + 123456789))
        mtimes.record(self.root, self.target)
        os.utime(self.root / 'same/a.rs', ns=(OLD + 900 * 10**9, OLD + 900 * 10**9))
        mtimes.restore(self.root, self.target)
        self.assertEqual(self.time('same/a.rs'), OLD)


if __name__ == '__main__':
    unittest.main()
