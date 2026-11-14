"""Check the release-proof runner with real PostgreSQL and a recording Cargo stub."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
RUNNER = ROOT / 'tooling/social-voice-release-test.sh'
WORKFLOW = ROOT / '.github/workflows/social-voice-release.yml'


class SocialVoiceReleaseCiTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tools = {}
        for name in ('postgres', 'initdb', 'pg_ctl', 'psql'):
            found = shutil.which(name)
            if not found:
                raise RuntimeError(f'{name} missing; run these infrastructure tests with nix develop')
            cls.tools[name] = found

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='release-proof-test-')
        self.addCleanup(self.temp.cleanup)
        self.files = Path(self.temp.name)
        self.record = self.files / 'record.json'
        self.bin = self.files / 'bin'
        self.bin.mkdir()
        self.env = dict(os.environ, PATH=f'{self.bin}{os.pathsep}{os.environ["PATH"]}',
                        RELEASE_CI_TEST_RECORD=str(self.record),
                        DATABASE_URL='postgresql://must-not-connect.invalid/production',
                        CATALYST_TEST_DATABASE_URL='postgresql://must-not-connect.invalid/production',
                        PGHOST='must-not-connect.invalid', PGHOSTADDR='192.0.2.1',
                        PGSERVICE='must-not-use', PGPASSWORD='ambient-test-password',
                        PSQL='must-not-use-ambient-psql')
        self.env.pop('RUST_TEST_THREADS', None)
        self.write_tool('cargo', '''
import json, os, subprocess, sys
from pathlib import Path
url = os.environ['CATALYST_TEST_DATABASE_URL']
psql = os.environ['PSQL']
def query(sql, database=url):
    return subprocess.check_output([psql, '-X', '--no-password', database, '-Atqc', sql], text=True).strip()
record = {'args': sys.argv[1:], 'url': url, 'psql': psql, 'data': os.environ['PGDATA'],
          'listen': query('SHOW listen_addresses'), 'server': query('SHOW server_version_num'),
          'locks': query('SHOW max_locks_per_transaction'), 'threads': os.environ['RUST_TEST_THREADS'],
          'target': os.environ.get('CARGO_TARGET_DIR'),
          'role': query('SELECT rolsuper AND rolcreatedb FROM pg_roles WHERE rolname=current_user'),
          'collation': query("SELECT EXISTS(SELECT 1 FROM pg_collation WHERE collname='en_US.utf8' AND collnamespace='pg_catalog'::regnamespace)", url.replace('/postgres?', '/template1?'))}
Path(os.environ['RELEASE_CI_TEST_RECORD']).write_text(json.dumps(record))
sys.exit(int(os.environ.get('RELEASE_CI_TEST_CARGO_EXIT', '0')))
''')

    def write_tool(self, name, body):
        script = self.bin / name
        script.write_text('#!/usr/bin/env python3\n' + body)
        script.chmod(0o755)

    def run_runner(self):
        return subprocess.run(['bash', str(RUNNER)], cwd=ROOT, env=self.env,
                              capture_output=True, text=True, timeout=60)

    def assert_owned_cluster_removed(self, record):
        self.assertTrue(record['data'].startswith('/tmp/catalyst-release.'))
        self.assertFalse(Path(record['data']).parent.exists(), 'owned cluster must be removed')
        if 'url' in record:
            result = subprocess.run([self.tools['psql'], '-X', '--no-password', record['url'], '-Atqc', 'SELECT 1'],
                                    env=dict(os.environ, PGCONNECT_TIMEOUT='2'),
                                    capture_output=True, text=True, timeout=5)
            self.assertNotEqual(result.returncode, 0, 'owned PostgreSQL must be stopped')

    def test_always_supplies_owned_database_and_psql_to_the_actual_release_target(self):
        self.env['CARGO_TARGET_DIR'] = str(self.files / 'isolated-target')
        result = self.run_runner()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        record = json.loads(self.record.read_text())
        self.assertEqual(record['args'], ['test', '--manifest-path', 'services/catalyst/Cargo.toml',
                                        '--locked', '-p', 'catalyst-db', '--test', 'social_voice_release'])
        self.assertIn('/postgres?host=/tmp/catalyst-release.', record['url'])
        self.assertNotIn('must-not-connect', record['url'])
        self.assertEqual(record['psql'], self.tools['psql'])
        self.assertEqual(record['listen'], '')
        self.assertEqual(record['server'], '180006')
        self.assertEqual(record['locks'], '1024')
        self.assertEqual(record['threads'], '2')
        self.assertEqual(record['target'], self.env['CARGO_TARGET_DIR'])
        self.assertEqual(record['role'], 't')
        self.assertEqual(record['collation'], 't')
        self.assert_owned_cluster_removed(record)

    def test_cargo_failure_is_not_hidden_and_still_removes_the_owned_cluster(self):
        self.env['RELEASE_CI_TEST_CARGO_EXIT'] = '7'
        result = self.run_runner()
        self.assertEqual(result.returncode, 7, result.stdout + result.stderr)
        self.assert_owned_cluster_removed(json.loads(self.record.read_text()))

    def test_startup_failure_is_fatal_before_cargo_and_removes_owned_files(self):
        self.write_tool('pg_ctl', f'''
import json, os, sys
from pathlib import Path
if 'start' in sys.argv:
    Path(os.environ['RELEASE_CI_TEST_RECORD']).write_text(json.dumps({{'data': os.environ['PGDATA']}}))
    sys.exit(23)
os.execv({self.tools['pg_ctl']!r}, [{self.tools['pg_ctl']!r}] + sys.argv[1:])
''')
        result = self.run_runner()
        self.assertEqual(result.returncode, 23, result.stdout + result.stderr)
        record = json.loads(self.record.read_text())
        self.assertNotIn('args', record, 'Cargo must not run after startup failure')
        self.assert_owned_cluster_removed(record)

    def test_unavailable_database_is_fatal_before_cargo_and_stops_owned_server(self):
        self.write_tool('psql', '''
import json, os, sys
from pathlib import Path
Path(os.environ['RELEASE_CI_TEST_RECORD']).write_text(json.dumps({'data': os.environ['PGDATA'], 'url': os.environ['CATALYST_TEST_DATABASE_URL']}))
sys.exit(29)
''')
        result = self.run_runner()
        self.assertEqual(result.returncode, 29, result.stdout + result.stderr)
        record = json.loads(self.record.read_text())
        self.assertNotIn('args', record, 'Cargo must not run without a working database')
        self.assert_owned_cluster_removed(record)

    def test_wrong_postgresql_toolchain_fails_before_initialization(self):
        self.write_tool('postgres', "print('postgres (PostgreSQL) 17.0')\n")
        result = self.run_runner()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('requires the locked PostgreSQL 18.6 toolchain', result.stderr)
        self.assertFalse(self.record.exists())

    def test_workflow_selects_release_inputs_and_runs_locked_nix_runner(self):
        workflow = WORKFLOW.read_text()
        for pattern in ('schemas/**', 'services/catalyst/db/**',
                        'services/catalyst/tests/fixtures/schema/**', 'services/catalyst/Cargo.*',
                        'services/catalyst/rust-toolchain.toml', 'flake.*',
                        'tooling/social-voice-release-test.sh',
                        'tooling/tests/test_social_voice_release_ci.py',
                        '.github/workflows/social-voice-release.yml'):
            self.assertEqual(workflow.count(f"- '{pattern}'"), 2, pattern)
        self.assertIn('nix develop .#ci --no-update-lock-file --command bash tooling/social-voice-release-test.sh', workflow)
        self.assertNotIn('continue-on-error', workflow)
        self.assertNotIn('secrets.', workflow)
        self.assertIn('permissions:\n  contents: read\n', workflow)

    def test_workflow_isolates_release_cache_and_deletes_no_runner_software(self):
        workflow = WORKFLOW.read_text()
        self.assertIn('CARGO_TARGET_DIR: ${{ github.workspace }}/services/catalyst/target-social-voice-release', workflow)
        # Dependencies only, in the exact directory Cargo receives, under a key family of its own.
        self.assertNotIn('actions/cache', workflow)
        self.assertEqual(workflow.count('uses: Swatinem/rust-cache@'), 1)
        self.assertIn('          workspaces: services/catalyst -> target-social-voice-release\n', workflow)
        self.assertNotIn('            services/catalyst/target\n', workflow)
        self.assertIn("          shared-key: social-voice-${{ hashFiles('flake.lock') }}\n", workflow)
        self.assertIn("          save-if: ${{ github.ref == 'refs/heads/main' && github.run_attempt == '1' }}\n", workflow)
        self.assertIn('          cache-on-failure: true\n', workflow)
        self.assertIn('          RUSTUP_HOME: /dev/null\n', workflow)
        for family in ('foundation-dev-', 'shared-key: dev-tests', 'social-voice-release-nodebug'):
            self.assertNotIn(family, workflow, 'no sharing with another cache family')
        # The hosted runner has room for this one suite; deleting its SDKs took half of the job time.
        self.assertNotIn('Reserve disk space', workflow)
        self.assertNotIn('sudo rm -rf', workflow)
        self.assertLess(workflow.index('Put the Nix Rust toolchain on PATH'), workflow.index('uses: Swatinem/rust-cache@'))
        self.assertLess(workflow.index('uses: Swatinem/rust-cache@'), workflow.index('bash tooling/social-voice-release-test.sh'))


if __name__ == '__main__':
    unittest.main()
