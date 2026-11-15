"""APT mirror replacement preserves signed sources and fails on install errors."""
import importlib.util
from pathlib import Path
import socket
import subprocess
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    'bevy_linux_deps', Path(__file__).resolve().parents[2] / 'apps/bevy-explorer/install-linux-deps.py')
deps = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(deps)


class LinuxDependenciesTest(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        (self.root / 'sources.list.d').mkdir()
        (self.root / 'apt.conf.d').mkdir()
        getaddrinfo = patch.object(deps.socket, 'getaddrinfo', return_value=[])
        self.getaddrinfo = getaddrinfo.start()
        self.addCleanup(getaddrinfo.stop)

    def test_legacy_and_deb822_sources_keep_suites_components_and_signing_keys(self):
        fixtures = {
            'sources.list': 'deb [arch=amd64 signed-by=/usr/share/keyrings/ubuntu-archive-keyring.gpg] '
                            'http://azure.archive.ubuntu.com/ubuntu/ jammy-updates main universe\n',
            'sources.list.d/ubuntu.sources': 'Types: deb\nURIs: https://azure.archive.ubuntu.com/ubuntu\n'
                                            'Suites: noble noble-updates\nComponents: main universe\n'
                                            'Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg\n',
        }
        for name, original in fixtures.items():
            (self.root / name).write_text(original)
        deps.configure(self.root)
        for name, original in fixtures.items():
            expected = original.replace('http://azure.archive.ubuntu.com', 'https://archive.ubuntu.com')
            expected = expected.replace('https://azure.archive.ubuntu.com', 'https://archive.ubuntu.com')
            self.assertEqual((self.root / name).read_text(), expected)
        deps.configure(self.root)
        self.assertEqual((self.root / 'sources.list.d/ubuntu.sources').read_text(), expected)

    def test_security_ports_and_third_party_sources_are_unchanged(self):
        original = ('deb http://security.ubuntu.com/ubuntu noble-security main\n'
                    'deb http://ports.ubuntu.com/ubuntu-ports noble main\n'
                    'deb https://example.invalid/azure.archive.ubuntu.com/ubuntu noble main\n'
                    'deb https://azure.archive.ubuntu.com/ubuntu-unrelated noble main\n')
        path = self.root / 'sources.list.d/other.list'
        path.write_text(original)
        deps.configure(self.root)
        self.assertEqual(path.read_text(), original)

    def test_hosted_runner_mirror_indirection_keeps_priorities_and_source_definitions(self):
        # actions/runner-images c87f6f63 configure-apt-sources.sh uses this list
        # from both Jammy's legacy source and Noble's deb822 source.
        mirrors = ('http://azure.archive.ubuntu.com/ubuntu/\tpriority:1\n'
                   'https://archive.ubuntu.com/ubuntu/\tpriority:2\n'
                   'https://security.ubuntu.com/ubuntu/\tpriority:3\n')
        fixtures = {
            'sources.list': 'deb mirror+file:/etc/apt/apt-mirrors.txt jammy main universe\n',
            'sources.list.d/ubuntu.sources': 'Types: deb\nURIs: mirror+file:/etc/apt/apt-mirrors.txt\n'
                                            'Suites: noble noble-updates\nComponents: main universe\n'
                                            'Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg\n',
        }
        for name, content in fixtures.items():
            (self.root / name).write_text(content)
        path = self.root / 'apt-mirrors.txt'
        path.write_text(mirrors)
        for _ in range(2):
            deps.configure(self.root)
            self.assertEqual(path.read_text(),
                             'https://archive.ubuntu.com/ubuntu/\tpriority:1\n'
                             'https://archive.ubuntu.com/ubuntu/\tpriority:2\n'
                             'https://security.ubuntu.com/ubuntu/\tpriority:3\n')
            for name, content in fixtures.items():
                self.assertEqual((self.root / name).read_text(), content)

    def test_unresolvable_archive_tries_the_outside_mirror_first_once(self):
        self.getaddrinfo.side_effect = socket.gaierror(socket.EAI_AGAIN, 'Temporary failure in name resolution')
        path = self.root / 'apt-mirrors.txt'
        path.write_text('http://azure.archive.ubuntu.com/ubuntu/\tpriority:1\n'
                        'https://archive.ubuntu.com/ubuntu/\tpriority:2\n'
                        'https://security.ubuntu.com/ubuntu/\tpriority:3\n')
        for _ in range(2):
            deps.configure(self.root)
            self.assertEqual(path.read_text(),
                             'https://mirrors.edge.kernel.org/ubuntu/\tpriority:0\n'
                             'https://archive.ubuntu.com/ubuntu/\tpriority:1\n'
                             'https://archive.ubuntu.com/ubuntu/\tpriority:2\n'
                             'https://security.ubuntu.com/ubuntu/\tpriority:3\n')

    def test_successful_update_precedes_noninteractive_install(self):
        with patch.object(deps.subprocess, 'run') as run:
            deps.install(['clang', 'libavcodec-dev'], self.root)
        self.assertEqual(run.call_args_list[0].args[0],
                         ['apt-get', '-o', 'APT::Update::Error-Mode=any', 'update'])
        self.assertEqual(run.call_args_list[1].args[0],
                         ['apt-get', 'install', '--yes', '--no-install-recommends', 'clang', 'libavcodec-dev'])
        for call in run.call_args_list:
            self.assertTrue(call.kwargs['check'])
            self.assertEqual(call.kwargs['env']['DEBIAN_FRONTEND'], 'noninteractive')
            self.assertEqual(call.kwargs['env']['NEEDRESTART_SUSPEND'], '1')

    def test_failed_update_never_installs_from_stale_indexes(self):
        with patch.object(deps.subprocess, 'run', side_effect=subprocess.CalledProcessError(100, 'apt-get')) as run:
            with self.assertRaises(subprocess.CalledProcessError):
                deps.install(['clang'], self.root)
        self.assertEqual(run.call_count, 1)

    def test_install_failure_is_not_hidden(self):
        with patch.object(deps.subprocess, 'run', side_effect=[None, subprocess.CalledProcessError(100, 'apt-get')]):
            with self.assertRaises(subprocess.CalledProcessError):
                deps.install(['clang'], self.root)


if __name__ == '__main__':
    unittest.main()
