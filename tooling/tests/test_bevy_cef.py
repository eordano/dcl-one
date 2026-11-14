"""The CI CEF install reads no build index, verifies its archive and keeps the export layout."""
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import re
import tarfile
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location('bevy_cef', ROOT / 'apps/bevy-explorer/install-cef.py')
cef = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(cef)

WORKFLOWS = ('bevy-explorer-ci.yml', 'bevy-explorer-scenes.yml', 'bevy-explorer-desktop.yml')


def archive(target, files):
    """The bytes of a distribution archive with these files under its one top folder."""
    top = f'cef_binary_{cef.BUILD}_{target}_minimal'
    data = io.BytesIO()
    with tarfile.open(fileobj=data, mode='w:bz2') as bundle:
        for path, content in files.items():
            entry = tarfile.TarInfo(f'{top}/{path}')
            entry.size = len(content)
            entry.mode = 0o755 if path.endswith('.so') else 0o644
            bundle.addfile(entry, io.BytesIO(content))
    return data.getvalue()


BUILD_FILES = {'CMakeLists.txt': b'cmake', 'cmake/cef_variables.cmake': b'variables', 'include/cef_app.h': b'header',
               'libcef_dll/wrapper.cc': b'wrapper', 'CREDITS.html': b'credits', 'README.txt': b'left behind'}
LINUX = {'Release/libcef.so': b'library', 'Release/v8_context_snapshot.bin': b'snapshot',
         'Resources/icudtl.dat': b'icu', 'Resources/locales/en-US.pak': b'locale', **BUILD_FILES}
MACOS = {'Release/Chromium Embedded Framework.framework/Resources/icudtl.dat': b'icu', **BUILD_FILES}


class CefTest(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.destination = self.root / 'share/cef'

    def install(self, system, machine, data, digest=None):
        target, _ = cef.ARCHIVES[(system, machine.lower())]
        pins = {**cef.ARCHIVES, (system, machine.lower()): (target, digest or hashlib.sha256(data).hexdigest())}
        requested = []

        def download(url, path):
            requested.append(url)
            Path(path).write_bytes(data)

        with patch.object(cef, 'ARCHIVES', pins), patch.object(cef, 'download', side_effect=download), \
                patch('builtins.print'):
            cef.install(system, machine, self.destination)
        return requested

    def files(self):
        return {path.relative_to(self.destination).as_posix() for path in self.destination.rglob('*') if path.is_file()}

    def test_the_export_holds_the_release_files_with_the_resources_beside_them(self):
        data = archive('linux64', LINUX)
        # A directory from an earlier install is replaced.
        self.destination.mkdir(parents=True)
        (self.destination / 'stale.pak').write_text('old')
        requested = self.install('Linux', 'x86_64', data)
        name = f'cef_binary_{cef.BUILD}_linux64_minimal.tar.bz2'
        # The archive is asked for by name: no index of builds is read.
        self.assertEqual(requested, [f'{cef.HOST}/{name.replace("+", "%2B")}'])
        self.assertEqual(self.files(), {
            'libcef.so', 'v8_context_snapshot.bin', 'icudtl.dat', 'locales/en-US.pak', 'CMakeLists.txt',
            'cmake/cef_variables.cmake', 'include/cef_app.h', 'libcef_dll/wrapper.cc', 'CREDITS.html', 'archive.json'})
        self.assertTrue((self.destination / 'libcef.so').stat().st_mode & 0o100)
        # The record the CEF build script reads to check the distribution against the crate version.
        self.assertEqual(json.loads((self.destination / 'archive.json').read_text()),
                         {'type': 'minimal', 'name': name, 'sha1': hashlib.sha1(data).hexdigest()})
        self.assertEqual([path.name for path in self.destination.parent.iterdir()], ['cef'])

    def test_the_host_selects_the_archive(self):
        for system, machine, target in (('Linux', 'x86_64', 'linux64'), ('Windows', 'AMD64', 'windows64'),
                                        ('Darwin', 'arm64', 'macosarm64')):
            with self.subTest(system=system):
                files = MACOS if system == 'Darwin' else LINUX
                requested = self.install(system, machine, archive(target, files))
                self.assertTrue(requested[0].endswith(f'_{target}_minimal.tar.bz2'))
                # The macOS framework keeps its resources; the other hosts get them at the root.
                self.assertEqual((self.destination / 'icudtl.dat').is_file(), system != 'Darwin')
        with self.assertRaises(KeyError):
            cef.install('Linux', 'riscv64', self.destination)

    def test_a_changed_archive_fails_and_keeps_the_installed_directory(self):
        self.install('Linux', 'x86_64', archive('linux64', LINUX))
        before = self.files()
        with self.assertRaisesRegex(ValueError, 'SHA-256 does not match the pin'):
            self.install('Linux', 'x86_64', archive('linux64', {**LINUX, 'Release/libcef.so': b'changed'}),
                         digest=cef.ARCHIVES[('Linux', 'x86_64')][1])
        self.assertEqual(self.files(), before)
        self.assertEqual((self.destination / 'libcef.so').read_bytes(), b'library')
        self.assertEqual([path.name for path in self.destination.parent.iterdir()], ['cef'])

    def test_the_pins_are_whole_digests_of_the_build_the_cef_crates_bind(self):
        for target, digest in cef.ARCHIVES.values():
            with self.subTest(target=target):
                self.assertRegex(digest, r'^[0-9a-f]{64}$')
        lock = (ROOT / 'rendering/bevy-explorer/source/Cargo.lock').read_text()
        versions = set(re.findall(r'name = "cef(?:-dll-sys)?"\nversion = "([^"]+)"', lock))
        self.assertEqual(len(versions), 1)
        # The crate version carries the CEF build after its plus sign.
        self.assertTrue(cef.BUILD.startswith(versions.pop().split('+', 1)[1] + '+'))

    def test_every_workflow_installs_cef_with_the_script(self):
        for name in WORKFLOWS:
            with self.subTest(workflow=name):
                workflow = (ROOT / '.github/workflows' / name).read_text()
                self.assertEqual(workflow.count('/apps/bevy-explorer/install-cef.py" "$HOME/.local/share/cef"\n'), 1)
                # The tool that reads the build index is not installed.
                self.assertNotIn('export-cef-dir', workflow)
        ci = (ROOT / '.github/workflows/bevy-explorer-ci.yml').read_text()
        # A changed pin or layout gets a new cache entry.
        self.assertIn("key: bevy-cef-${{ runner.os }}-${{ runner.arch }}-${{ hashFiles('apps/bevy-explorer/install-cef.py') }}\n", ci)


if __name__ == '__main__':
    unittest.main()
