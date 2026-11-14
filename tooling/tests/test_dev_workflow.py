"""Development CI keeps platform coverage and reusable, isolated build snapshots."""
from pathlib import Path
import json
import os
import re
import subprocess
import tempfile
import textwrap
from types import SimpleNamespace
import unittest
from unittest.mock import patch


WORKFLOW = Path(__file__).parents[2] / '.github/workflows/development.yml'
CLEANUP = WORKFLOW.with_name('closed-pr-cache.yml')
CREATOR_HUB = WORKFLOW.with_name('creator-hub-ci.yml')


def jobs():
    source = WORKFLOW.read_text().split('jobs:\n', 1)[1]
    return dict(re.findall(r'(?ms)^  (\w+):\n(.*?)(?=^  \w+:|\Z)', source))


class DevelopmentWorkflowTest(unittest.TestCase):
    def test_rust_caches_dependencies_only_and_smoke_keeps_a_snapshot_on_both_platforms(self):
        rust, smoke = jobs()['rust'], jobs()['smoke']
        for job in (rust, smoke):
            # Both platforms on main; PRs add macOS with the ci:macos label.
            self.assertRegex(job, r'os: (\[ubuntu-26\.04, macos-26\]|.*"ubuntu-26\.04", "macos-26")')
        # The shards cover every test executable: the matrix lists 1 to n and the script gets i/n.
        self.assertIn('        shard: [1, 2, 3, 4, 5]\n', rust)
        self.assertIn('      CATALYST_TEST_SHARD: ${{ matrix.shard }}/5\n', rust)
        # The dependency cache: the action prunes workspace output before saving, main writes, PRs read.
        self.assertNotIn('actions/cache', rust)
        cache = re.search(r'(?ms)^      - name: Restore the Rust dependency cache\n(.*?)(?=^      - |\Z)', rust).group(1)
        self.assertIn('uses: Swatinem/rust-cache@', cache)
        self.assertIn('workspaces: services/catalyst', cache)
        self.assertIn("save-if: ${{ github.ref == 'refs/heads/main' && github.run_attempt == '1' && matrix.shard == 1 }}", cache)
        self.assertIn('cache-on-failure: true', cache)
        self.assertLess(rust.index('Put the Nix Rust toolchain on PATH'), rust.index('Swatinem/rust-cache@'))
        self.assertLess(rust.index('Swatinem/rust-cache@'), rust.index('bash tooling/dev-test.sh rust'))
        keys = re.findall(r'^          key: (foundation-dev-v2-[^\n]+)', smoke, re.M)
        self.assertEqual(len(keys), 1)
        self.assertTrue(keys[0].startswith('foundation-dev-v2-smoke-'))
        self.assertIn('steps.rust-inputs.outputs.source', keys[0])
        self.assertIn("hashFiles('services/catalyst/Cargo.lock')", keys[0])
        saves = re.findall(r'(?ms)^      - name: Save successful[^\n]*\n(.*?)(?=^      - |\Z)', smoke)
        self.assertEqual(len(saves), 1)
        self.assertIn('if: success()', saves[0])
        self.assertIn("github.event_name == 'push'", saves[0])
        self.assertIn("github.ref == 'refs/heads/main'", saves[0])
        self.assertIn('steps.rust-cache.outputs.cache-primary-key', saves[0])
        self.assertIn('bash tooling/dev-smoke.sh', smoke)
        self.assertIn('bash tooling/dev-test.sh e2e', smoke)

    def test_every_nix_job_restores_the_store_snapshot_before_its_first_shell(self):
        policy = WORKFLOW.parents[2] / 'tooling/repo-actions-policy.sh'
        self.assertIn("patterns_allowed[]=nix-community/cache-nix-action@*", policy.read_text())
        installs = 0
        for name in ('development.yml', 'social-voice-release.yml'):
            jobs = WORKFLOW.with_name(name).read_text().split('\njobs:\n', 1)[1]
            for job in re.split(r'(?m)^  (?=[\w-]+:\n)', jobs):
                steps = re.split(r'(?m)^      - ', job)
                for i, step in enumerate(steps):
                    if not step.startswith('uses: cachix/install-nix-action@'):
                        continue
                    installs += 1
                    snapshot, roots = steps[i + 1], steps[i + 2]
                    shell = 'ci' if 'dev-test.sh rust' in job or 'social-voice-release-test.sh' in job else 'default'
                    key = 'nix-store-${{ runner.os }}-${{ runner.arch }}-' + shell + '-'
                    self.assertTrue(snapshot.startswith('uses: nix-community/cache-nix-action@'), name)
                    self.assertIn('primary-key: ' + key + "${{ hashFiles('flake.nix', 'flake.lock', ", snapshot)
                    self.assertIn('restore-prefixes-first-match: ' + key + '\n', snapshot)
                    # One Rust test shard saves; the other jobs save from every main run.
                    self.assertIn("save: ${{ github.ref == 'refs/heads/main' && github.run_attempt == '1'"
                                  + (' && matrix.shard == 1 }}' if 'shard: [' in job else ' }}'), snapshot)
                    self.assertIn('gc-max-store-size: 0', snapshot)
                    self.assertIn('mkdir -p .cache/nix-roots\n', roots)
                    self.assertIn('nix develop ' + ('.#ci ' if shell == 'ci' else '') + '--profile .cache/nix-roots/env --command true', roots)
                    self.assertIn('nix flake archive --json', roots)
                    for command in re.findall(r'nix develop(?: \.#\w+)?', job):
                        self.assertEqual(command, 'nix develop .#ci' if shell == 'ci' else 'nix develop', name)
        self.assertEqual(installs, 4)

    def test_smoke_snapshots_are_saved_only_from_passing_main_pushes(self):
        def allowed(event, ref, platform, passed=True, hit=False):
            save = re.search(r'(?ms)^      - name: Save successful[^\n]*\n(.*?)(?=^      - |\Z)',
                             jobs()['smoke']).group(1)
            expression = re.search(r'^        if: (.+)$', save, re.M).group(1)
            expression = expression.replace('&&', 'and').replace('||', 'or')
            expression = expression.replace('steps.rust-cache.outputs.cache-hit', 'cache_hit')
            github = SimpleNamespace(event_name=event, ref=ref, repository='decentraland/dcl-one')
            return eval(expression, {'__builtins__': {}}, {
                'github': github, 'runner': SimpleNamespace(os=platform),
                'success': lambda: passed, 'cache_hit': 'true' if hit else 'false',
            })

        for platform in ('Linux', 'macOS'):
            with self.subTest(platform=platform):
                self.assertTrue(allowed('push', 'refs/heads/main', platform))
                self.assertFalse(allowed('push', 'refs/heads/feature', platform))
                self.assertFalse(allowed('workflow_dispatch', 'refs/heads/main', platform))
                self.assertFalse(allowed('push', 'refs/heads/main', platform, passed=False))
                self.assertFalse(allowed('push', 'refs/heads/main', platform, hit=True))
                self.assertFalse(allowed('pull_request', 'refs/pull/150/merge', platform))

    def test_closed_pr_cleanup_deletes_only_exact_merge_ref_from_every_page(self):
        source = CLEANUP.read_text()
        self.assertIn('pull_request:\n    types: [closed]', source)
        self.assertIn('permissions: {}', source)
        self.assertNotIn('checkout', source.split('steps:', 1)[1].replace('checks out', ''))
        self.assertNotIn('uses:', source)
        script = textwrap.dedent(source.split("python3 - <<'PY'\n", 1)[1].rsplit('\n          PY', 1)[0])
        self.assertNotIn('${{', script)
        pages = [{'actions_caches': [
            {'id': 1, 'ref': 'refs/heads/main'},
            {'id': 2, 'ref': 'refs/pull/151/merge'},
            {'id': 3, 'ref': 'refs/pull/150/merge'},
        ]}, {'actions_caches': [
            {'id': 4, 'ref': 'refs/pull/150/head'},
            {'id': 5, 'ref': 'refs/pull/150/merge'},
            {'id': 'unsafe', 'ref': 'refs/pull/150/merge'},
        ]}]
        with patch.dict(os.environ, {'PR_NUMBER': '150', 'GITHUB_REPOSITORY': 'decentraland/dcl-one',
                                     'GITHUB_EVENT_NAME': 'pull_request'}), \
                patch('subprocess.check_output', return_value=json.dumps(pages)) as listing, \
                patch('subprocess.run') as deletion, patch('builtins.print'):
            exec(compile(script, str(CLEANUP), 'exec'), {})
            request = listing.call_args.args[0]
            self.assertIn('ref=refs/pull/150/merge', request)
            self.assertIn('--paginate', request)
            self.assertEqual([call.args[0][-1] for call in deletion.call_args_list], [
                'repos/decentraland/dcl-one/actions/caches/3',
                'repos/decentraland/dcl-one/actions/caches/5',
            ])
            for number in ('', '0', '-1', '../main', '150; echo unsafe'):
                with self.subTest(number=number), patch.dict(os.environ, {'PR_NUMBER': number}):
                    listing.reset_mock()
                    deletion.reset_mock()
                    with self.assertRaises(ValueError):
                        exec(compile(script, str(CLEANUP), 'exec'), {})
                    listing.assert_not_called()
                    deletion.assert_not_called()

    def test_scheduled_sweep_catches_late_uploads_and_bounds_dev_and_rust_caches(self):
        source = CLEANUP.read_text()
        self.assertIn("cron: '17 * * * *'", source)
        script = textwrap.dedent(source.split("python3 - <<'PY'\n", 1)[1].rsplit('\n          PY', 1)[0])
        prefix = 'foundation-dev-v2-smoke-decentraland/dcl-one-Linux-X64-'
        deps = 'v0-rust-dev-tests-'
        bevy = 'v0-rust-bevy-explorer-native-ci-test-test-Linux-x64-'
        caches = [
            {'id': 1, 'ref': 'refs/heads/main', 'key': prefix + 'old'},
            {'id': 2, 'ref': 'refs/heads/main', 'key': prefix + 'new'},
            {'id': 3, 'ref': 'refs/heads/main', 'key': 'images-v2-main-exact'},
            {'id': 4, 'ref': 'refs/pull/10/merge', 'key': prefix + 'late'},
            {'id': 5, 'ref': 'refs/pull/10/merge', 'key': 'images-v2-pr-10-late'},
            {'id': 6, 'ref': 'refs/pull/11/merge', 'key': prefix + 'old'},
            {'id': 7, 'ref': 'refs/pull/11/merge', 'key': prefix + 'new'},
            {'id': 8, 'ref': 'refs/pull/11/merge', 'key': 'images-v2-pr-11-exact'},
            {'id': 9, 'ref': 'refs/pull/10/head', 'key': prefix + 'ignored'},
            {'id': 10, 'ref': 'refs/heads/other', 'key': prefix + 'ignored'},
            {'id': 11, 'ref': 'refs/heads/main', 'key': 'bevy-cache'},
            {'id': 12, 'ref': 'refs/heads/main', 'key': deps + '0' * 64 + '-Linux-x64-previous-flake'},
            {'id': 13, 'ref': 'refs/heads/main', 'key': deps + 'f' * 64 + '-Linux-x64-new'},
            {'id': 14, 'ref': 'refs/heads/main', 'key': deps + 'f' * 64 + '-Darwin-arm64-only'},
            {'id': 15, 'ref': 'refs/heads/main', 'key': bevy + 'old'},
            {'id': 16, 'ref': 'refs/heads/main', 'key': bevy + 'new'},
            {'id': 17, 'ref': 'refs/pull/11/merge', 'key': bevy + 'old'},
            {'id': 18, 'ref': 'refs/pull/11/merge', 'key': bevy + 'new'},
            {'id': 19, 'ref': 'refs/heads/main', 'key': 'v0-rust-e2e-scene-cli-scene-cli-Windows_NT-x64-only'},
        ]
        def api(args, **kwargs):
            if any(arg.endswith('/actions/caches') for arg in args):
                return json.dumps([{'actions_caches': caches}])
            if args[-1].endswith('/pulls/10'):
                return json.dumps({'state': 'closed'})
            if args[-1].endswith('/pulls/11'):
                return json.dumps({'state': 'open'})
            self.fail(f'Unexpected API call: {args}')
        with patch.dict(os.environ, {'GITHUB_EVENT_NAME': 'schedule', 'PR_NUMBER': '',
                                     'GITHUB_REPOSITORY': 'decentraland/dcl-one'}), \
                patch('subprocess.check_output', side_effect=api) as listing, \
                patch('subprocess.run') as deletion, patch('builtins.print'):
            exec(compile(script, str(CLEANUP), 'exec'), {})
        self.assertEqual([call.args[0][-1] for call in deletion.call_args_list], [
            'repos/decentraland/dcl-one/actions/caches/1',
            'repos/decentraland/dcl-one/actions/caches/4',
            'repos/decentraland/dcl-one/actions/caches/5',
            'repos/decentraland/dcl-one/actions/caches/6',
            'repos/decentraland/dcl-one/actions/caches/12',
            'repos/decentraland/dcl-one/actions/caches/15',
            'repos/decentraland/dcl-one/actions/caches/17',
        ])
        self.assertEqual(listing.call_count, 3, 'one state lookup per PR, regardless of cache count')

    def test_source_key_tracks_embedded_inputs_but_not_unrelated_commits(self):
        command = re.search(r'^          source=(.*)$', jobs()['smoke'], re.M).group(1)
        inputs = ('services/catalyst/worlds/src/storage/export.rs',
                  'library/protocol/proto/message.proto', 'schemas/20_worlds.sql',
                  'apps/dcl-scene-cli/src/vendor/node_modules.zip', '.cargo/config.toml')
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)

            def git(*args):
                return subprocess.run(['git', *args], cwd=root, check=True, capture_output=True)

            def commit():
                git('add', '.')
                git('-c', 'user.name=CI test', '-c', 'user.email=ci@example.invalid',
                    '-c', 'commit.gpgsign=false', 'commit', '-qm', 'fixture')

            def fingerprint():
                result = subprocess.run(['bash', '-eo', 'pipefail', '-c',
                                         'source=' + command + '\nprintf "%s" "$source"'],
                                        cwd=root, check=True, capture_output=True, text=True)
                return result.stdout

            git('init', '-q')
            for name in (*inputs, 'docs/unrelated.md'):
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text('original')
            commit()
            previous = fingerprint()
            (root / 'docs/unrelated.md').write_text('changed')
            commit()
            self.assertEqual(previous, fingerprint())
            for name in inputs:
                with self.subTest(name=name):
                    (root / name).write_text('changed')
                    commit()
                    current = fingerprint()
                    self.assertNotEqual(previous, current)
                    previous = current

    def test_creator_hub_desktop_builds_need_the_label_on_pull_requests(self):
        source = CREATOR_HUB.read_text().split('jobs:\n', 1)[1]
        found = dict(re.findall(r'(?ms)^  ([\w-]+):\n(.*?)(?=^  [\w-]+:|\Z)', source))
        label = "contains(github.event.pull_request.labels.*.name, 'ci:desktop')"
        # main always builds; a skipped build is not a failure for the required check.
        self.assertIn(f"    if: github.event_name != 'pull_request' || {label}\n", found['creator-hub'])
        self.assertIn('    needs: [ creator-hub ]\n', found['prune_pre_releases'])
        self.assertIn('creator-hub, prune_pre_releases ]', found['result'])
        self.assertIn('any(.value.result == "failure" or .value.result == "cancelled")', found['result'])
        # No other job reads the label: the checks every pull request needs stay unconditional.
        conditions = {name: re.findall(r'(?m)^    if: (.*)$', job) for name, job in found.items()}
        self.assertEqual([name for name, lines in conditions.items() if any('ci:desktop' in line for line in lines)],
                         ['creator-hub'])
        self.assertIn(f'          DESKTOP: ${{{{ {label} }}}}\n', found['changes'])
        self.assertIn('Add the ci:desktop label', found['changes'])

    def test_only_unit_suites_are_gated_by_web_selection(self):
        web = jobs()['web']
        self.assertIn('name: Web tests and contracts', web)
        self.assertIn('run: python3 tooling/dev-ci.py', web)
        self.assertIn('BASE_SHA: ${{ github.event.pull_request.base.sha }}', web)
        steps = re.split(r'^      - ', web, flags=re.M)[1:]
        selected = [step for step in steps if 'if: steps.web-coverage.outputs' in step]
        self.assertEqual(len(selected), 3)
        for app in ('ui3', 'sites', 'ui3/editor-scene'):
            self.assertTrue(any(f'pnpm --dir apps/{app} test\n' in step for step in selected))
        contracts = next(step for step in steps if 'name: Check web types' in step)
        self.assertNotIn('\n        if:', contracts)
        self.assertIn('typecheck:scripts', contracts)
        self.assertIn('gen:schemas:check', contracts)


if __name__ == '__main__':
    unittest.main()
