"""Cache identity and runner setup must not weaken image publication."""
import importlib.util
import io
import json
import os
import fnmatch
import re
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location('image_cache', Path(__file__).parents[1] / 'image-cache.py')
cache = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(cache)


class CachePlanTest(unittest.TestCase):
    def setUp(self):
        self.derivations = {
            package: f'/nix/store/abc123-{package}.drv'
            for packages in cache.PACKAGES.values() for package in packages
        }

    def plan(self, event='pull_request', ref='refs/pull/93/merge', arch='arm64'):
        return cache.cache_plan(arch, event, ref, self.derivations)

    def test_pull_requests_can_save_but_never_share_the_main_write_key(self):
        pr = self.plan()
        main = self.plan('push', 'refs/heads/main')
        self.assertEqual(pr['save'], 'true')
        self.assertEqual(main['save'], 'true')
        for group in cache.PACKAGES:
            self.assertNotEqual(pr[f'{group}-key'], main[f'{group}-key'])
            self.assertEqual(pr[f'{group}-main-key'], main[f'{group}-key'])
            self.assertEqual(main[f'{group}-main-prefix'], '')
            if group in cache.RUNTIME:
                self.assertEqual(pr[f'{group}-main-prefix'], '')
            else:
                self.assertTrue(main[f'{group}-key'].startswith(pr[f'{group}-main-prefix']))
                self.assertFalse(pr[f'{group}-key'].startswith(pr[f'{group}-main-prefix']))
            self.assertNotEqual(pr[f'{group}-key'], self.plan(ref='refs/pull/94/merge')[f'{group}-key'])

    def test_tags_and_other_branches_only_read_main(self):
        for event, ref in [('push', 'refs/tags/v1'), ('workflow_dispatch', 'refs/heads/feature'),
                           ('workflow_dispatch', 'refs/heads/main'), ('pull_request_target', 'refs/heads/main'),
                           ('pull_request', 'refs/heads/feature')]:
            with self.subTest(event=event, ref=ref):
                plan = self.plan(event, ref)
                self.assertEqual(plan['save'], 'false')
                self.assertEqual(plan['catalyst-key'], plan['catalyst-main-key'])

    def test_only_pr_dependencies_allow_stale_main_entries(self):
        events = [('push', 'refs/heads/main'), ('push', 'refs/tags/v1'),
                  ('workflow_dispatch', 'refs/heads/main'),
                  ('workflow_dispatch', 'refs/heads/feature'),
                  ('workflow_run', 'refs/heads/main'),
                  ('pull_request_target', 'refs/heads/main'),
                  ('pull_request', 'refs/heads/feature'),
                  ('pull_request', 'refs/pull/93/merge')]
        for event, ref in events:
            for arch in ('amd64', 'arm64'):
                with self.subTest(event=event, ref=ref, arch=arch):
                    plan = self.plan(event, ref, arch)
                    for group in cache.PACKAGES:
                        expected = (f'images-v2-main-{group}-{arch}-'
                                    if event == 'pull_request' and ref == 'refs/pull/93/merge'
                                    and group not in cache.RUNTIME else '')
                        self.assertEqual(plan[f'{group}-main-prefix'], expected)

    def test_dependency_groups_change_independently_from_application_source(self):
        before = self.plan()
        self.derivations['catalyst'] = '/nix/store/changed-catalyst.drv'
        application = self.plan()
        self.assertNotEqual(before['apis-key'], application['apis-key'])
        self.assertEqual(before['catalyst-key'], application['catalyst-key'])
        self.assertEqual(before['sites-key'], application['sites-key'])
        self.derivations['catalyst-deps'] = '/nix/store/changed-catalyst-deps.drv'
        dependencies = self.plan()
        self.assertNotEqual(application['catalyst-key'], dependencies['catalyst-key'])
        self.assertEqual(application['sites-key'], dependencies['sites-key'])
        self.derivations['sites-toolchain'] = '/nix/store/changed-sites-toolchain.drv'
        self.assertNotEqual(dependencies['sites-key'], self.plan()['sites-key'])
        self.assertNotEqual(self.plan()['sites-key'], self.plan(arch='amd64')['sites-key'])
        self.assertEqual(before['sites-app-key'], self.plan()['sites-app-key'])
        self.derivations['sites-app'] = '/nix/store/changed-sites-app.drv'
        compiled = self.plan()
        self.assertNotEqual(before['sites-app-key'], compiled['sites-app-key'])
        self.assertEqual(self.plan(ref='refs/heads/main', event='push')['sites-app-main-prefix'], '')
        self.assertEqual(compiled['sites-app-main-prefix'], '')
        self.assertEqual(compiled['sites-key'], self.plan()['sites-key'])
        self.derivations['sites-app-polygon-credits'] = '/nix/store/changed-sites-app-credits.drv'
        self.assertNotEqual(compiled['sites-app-key'], self.plan()['sites-app-key'])

    def test_rejects_malformed_derivation_identity(self):
        self.derivations['catalyst'] = '/nix/store/abc-catalyst.drv\ninjected-key=anything'
        with self.assertRaises(ValueError):
            self.plan()

    def test_command_evaluates_once_and_emits_scoped_outputs(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory) / 'outputs'
            env = {'GITHUB_EVENT_NAME': 'pull_request', 'GITHUB_REF': 'refs/pull/93/merge',
                   'GITHUB_OUTPUT': str(output)}
            with patch.dict(cache.os.environ, env, clear=True), \
                    patch('sys.argv', ['image-cache.py', 'keys', '--arch', 'arm64']), \
                    patch.object(cache.subprocess, 'check_output', return_value=json.dumps(self.derivations)) as run, \
                    patch('sys.stdout', new_callable=io.StringIO):
                cache.main()
            run.assert_called_once()
            self.assertEqual(run.call_args.args[0][:4],
                             ['nix', 'eval', '--json', '.#packages.aarch64-linux'])
            values = dict(line.split('=', 1) for line in output.read_text().splitlines())
            self.assertEqual(values, self.plan())


class CatalystSourceTest(unittest.TestCase):
    def test_every_library_path_the_backend_sources_name_is_in_the_nix_source_set(self):
        # The image build copies only this set, so a path outside it is missing there. This check
        # names the path before a compilation fails on it, or a build script reads an empty folder.
        root = Path(__file__).parents[2]
        nix = (root / 'infra/packages/catalyst.nix').read_text()
        self.assertNotIn('(root + /library)', nix)
        listed = ['library/' + path for path in re.findall(r'\(library \+ /([\w./-]+)\)', nix)]
        self.assertEqual(sorted(listed), ['library/catalyst-api/openapi', 'library/protocol/proto',
                                          'library/vendor/rusty_enet'])
        self.assertIn('file.hasExt "json") (library + /catalyst-api/openapi)', nix)
        self.assertIn('lib.hasPrefix "dcl-" name', nix)
        tracked = subprocess.run(['git', 'ls-files', '-z', '--', 'services/catalyst', 'library/dcl-*',
                                  'library/vendor/rusty_enet'], cwd=root, capture_output=True, text=True, check=True)
        named = set()
        for name in tracked.stdout.split('\0'):
            if name.endswith('.rs'):
                named.update(re.findall(r'"/?(library/[^"]*)"', (root / name).read_text(errors='replace')))
        self.assertIn('library/protocol/proto', named)
        for path in sorted(named):
            with self.subTest(path=path):
                inside = re.match(r'library/dcl-[^/]+/', path) or any(
                    path == entry or path.startswith(entry + '/') for entry in listed)
                self.assertTrue(inside, 'add this path to infra/packages/catalyst.nix')
                if path.startswith('library/catalyst-api/openapi/'):
                    self.assertTrue(path.endswith('.json'), 'the source set takes only the JSON documents')


class CompileWorkflowTest(unittest.TestCase):
    """The compile job saves the entries that the image build restores, so both must name them alike."""

    def setUp(self):
        root = Path(__file__).parents[2] / '.github/workflows'
        images = (root / 'apis-sites-images.yml').read_text()
        self.workflow = (root / 'apis-sites-compile.yml').read_text()
        self.arches = (images.split('  arches:\n', 1)[1].split('  build:\n', 1)[0],
                       self.workflow.split('  arches:\n', 1)[1].split('  packages:\n', 1)[0])
        self.build = self.steps(images.split('  build:\n', 1)[1].split('  production-ready:\n', 1)[0])
        self.packages = self.steps(self.workflow.split('  packages:\n', 1)[1])

    @staticmethod
    def steps(job):
        found = {}
        for step in job.split('\n      - ')[1:]:
            name = re.match(r'name: (.+)', step)
            found[name.group(1) if name else step.splitlines()[0]] = step.rstrip() + '\n'
        return found

    @staticmethod
    def body(step):
        # What a step does, without its name, its condition, the lookup switch and comments.
        return re.sub(r'(?m)^(name: .+|        if: .+|          lookup-only: true| *# .*)\n', '', step)

    def test_it_reads_nothing_from_the_web_build_and_holds_no_write_permission(self):
        self.assertIn('\non:\n  workflow_call:\npermissions:\n  contents: read\njobs:\n', self.workflow)
        self.assertEqual(self.workflow.count('permissions:'), 1)
        for word in ('fetch-play', 'download-artifact', 'upload-artifact', 'docker', 'secrets.', 'environment:'):
            self.assertNotIn(word, self.workflow)

    def test_both_workflows_choose_the_same_architectures_and_prepare_the_same_checkout(self):
        self.assertEqual(*self.arches)
        setup = [name for name in self.packages if name.startswith('uses: ')]
        self.assertEqual([name.split('@')[0] for name in setup],
                         ['uses: actions/checkout', 'uses: cachix/install-nix-action', 'uses: actions/setup-python'])
        for name in ('Preserve disk headroom on small runners', 'Fetch Sites media and shared fonts from Git LFS',
                     'Identify exact Nix cache inputs', *setup):
            with self.subTest(step=name):
                self.assertEqual(self.packages[name], self.build[name])

    def test_the_lookups_use_the_keys_of_the_image_build_and_download_nothing(self):
        for lookup, restore in (('Look for the compiled APIs package', 'Restore the compiled APIs package'),
                                ('Look for the compiled Sites app', 'Restore the compiled Sites app')):
            with self.subTest(step=lookup):
                self.assertIn('          lookup-only: true\n', self.packages[lookup])
                self.assertNotIn('lookup-only', self.build[restore])
                self.assertEqual(self.body(self.packages[lookup]), self.body(self.build[restore]))
        self.assertEqual(self.body(self.packages['Decide which packages already have an exact entry']),
                         self.body(self.build['Decide which dependency caches this build still needs']))

    def test_it_compiles_only_when_this_attempt_can_save_a_missing_entry(self):
        plan = self.packages['Decide what this job compiles']
        self.assertIn('if [ "$SAVE" = true ] && [ "$GITHUB_RUN_ATTEMPT" = 1 ] && '
                      '{ [ "$APIS" != true ] || [ "$SITES_APP" != true ]; }; then\n', plan)
        self.assertIn('SAVE: ${{ steps.cache-keys.outputs.save }}\n', plan)
        names = list(self.packages)
        for name in names[names.index('Decide what this job compiles') + 1:]:
            with self.subTest(step=name):
                condition = re.search(r'(?m)^        if: (.+)$', self.packages[name]).group(1)
                self.assertRegex(condition, r"^steps\.(plan\.outputs\.work|cache-writer\.outputs\.allowed) == 'true'")
        compile_step = self.packages['Compile the APIs and the Sites apps']
        for target in ('catalyst', 'sites-app', 'sites-app-polygon-credits'):
            # The flake reference of the image build: another one gives other derivations and other keys.
            self.assertIn(f"'git+file:.?lfs=1#{target}'", compile_step)
        self.assertIn("'git+file:.?lfs=1#image-apis'", self.build['Build the APIs and Sites images with our Bevy artifact'])
        self.assertNotIn('--impure', compile_step)  # the export steps compare both evaluations

    def test_the_dependency_caches_the_gate_and_the_saves_are_those_of_the_image_build(self):
        same = {name: name for name in ('Restore Catalyst dependency cache', 'Restore Sites dependency cache',
                                        'Decide which caches a successful build saves',
                                        'Build the complete Sites toolchain and dependencies',
                                        'Allow cache uploads only from current first attempts')}
        renamed = {'Export the Catalyst dependencies': 'Export tested Catalyst dependencies',
                   'Save the Catalyst dependencies': 'Save tested Catalyst dependencies',
                   'Export the Sites dependencies': 'Export tested Sites dependencies',
                   'Save the Sites dependencies': 'Save tested Sites dependencies',
                   'Export the compiled APIs package': 'Export the tested APIs package',
                   'Save the compiled APIs package': 'Save the tested APIs package',
                   'Export the compiled Sites app': 'Export the tested Sites app',
                   'Save the compiled Sites app': 'Save the tested Sites app'}
        for ours, theirs in {**same, **renamed}.items():
            with self.subTest(step=ours):
                self.assertEqual(self.body(self.packages[ours]), self.body(self.build[theirs]))
        for ours, theirs in renamed.items():
            # The same guard as the image build: the gate allowed it and this group has no entry.
            condition = r'(?m)^        if: (.+)$'
            self.assertEqual(re.search(condition, self.packages[ours]).group(1),
                             re.search(condition, self.build[theirs]).group(1))
        names = list(self.packages)
        self.assertLess(names.index('Compile the APIs and the Sites apps'),
                        names.index('Allow cache uploads only from current first attempts'))
        self.assertEqual(names[-1], 'Save the compiled Sites app')


class WorkflowSetupTest(unittest.TestCase):
    def setUp(self):
        workflow = (Path(__file__).parents[2] / '.github/workflows/apis-sites-images.yml').read_text()
        self.build = workflow.split('  build:\n', 1)[1].split('  production-ready:\n', 1)[0]
        self.checkout = self.build.index('      - uses: actions/checkout@')

    def test_lfs_checkout_covers_sites_media_and_fonts_after_disk_preparation(self):
        before = self.build[:self.checkout]
        self.assertIn('name: Preserve disk headroom on small runners', before)
        include = re.search(r"lfs.fetchinclude '([^']+)'", before)
        self.assertIsNotNone(include)
        patterns = include.group(1).split(',')
        required = ('apps/sites/content/docs/fixture.webp',
                    'library/ui/apps/sites/assets/hero_desktop.webp',
                    'library/ui/apps/sites/assets/catch_the_vibe/alan.mp4',
                    'library/ui/apps/sites/assets/video_landing.webm',
                    'library/ui/foundations/typography/fonts/inter-latin-wght-normal.woff2')
        for path in required:
            with self.subTest(path=path):
                self.assertTrue(any(fnmatch.fnmatchcase(path, pattern) for pattern in patterns))
        self.assertFalse(any(fnmatch.fnmatchcase('library/uuav/dist/libavcodec.so', pattern)
                             for pattern in patterns))
        self.assertIn('lfs: true', self.build[self.checkout:])
        self.assertNotIn('tooling/', before)

    def test_sites_source_archive_keeps_public_ui_imports_without_design_tooling(self):
        source = (Path(__file__).parents[2] / 'infra/packages/sites.nix').read_text()
        archive = source.split('  src = lib.fileset.toSource {', 1)[1].split('  # A build', 1)[0]
        roots = re.findall(r'root \+ /([^\s)]+)', archive)

        def included(path):
            return any(path == root or path.startswith(root + '/') for root in roots)

        for path in ('library/ui/index.ts',
                     'library/ui/css-modules.d.ts',
                     'library/ui/components/NavBar/NavBar.tsx',
                     'library/ui/components/NavBar/NavBar.module.css',
                     'library/ui/components/Button/Button.tsx',
                     'library/ui/components/Button/Button.module.css',
                     'library/ui/apps/sites/chrome/SitesNotificationsDialog.tsx',
                     'library/ui/foundations/foundations.css'):
            with self.subTest(path=path):
                self.assertTrue(included(path), f'Sites archive omits browser input: {path}')
        for path in ('library/ui/figma/src/cli.ts',
                     'library/ui/figma/review/src/main.tsx',
                     'library/ui/.agent-work/private.txt'):
            with self.subTest(path=path):
                self.assertFalse(included(path), f'Sites archive includes tooling/private input: {path}')

    def test_sites_media_guard_rejects_pointer_text_and_accepts_materialized_binary(self):
        source = (Path(__file__).parents[2] / 'infra/packages/sites.nix').read_text()
        build = source.split("  buildPhase = ''", 1)[1]
        guard = build.split('    if grep -rIl ', 1)[1].split('    pnpm run build', 1)[0]
        script = 'if grep -rIl ' + guard
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            work = root / 'apps/sites'
            assets = root / 'library/ui/apps/sites/assets'
            fonts = root / 'library/ui/foundations/typography/fonts'
            for path in (work, assets, fonts):
                path.mkdir(parents=True)
            media = assets / 'hero.webp'
            font = fonts / 'inter.woff2'
            payload = b'actual\0binary'
            media.write_bytes(payload)
            font.write_bytes(payload)
            result = subprocess.run(['bash', '-e', '-o', 'pipefail', '-c', script],
                                    cwd=work, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            for path in (media, font):
                with self.subTest(path=path.name):
                    path.write_text('version https://git-lfs.github.com/spec/v1\n'
                                    'oid sha256:' + 'a' * 64 + '\nsize 12\n')
                    before = path.read_bytes()
                    result = subprocess.run(['bash', '-e', '-o', 'pipefail', '-c', script],
                                            cwd=work, capture_output=True)
                    self.assertEqual(result.returncode, 1, result.stderr)
                    self.assertIn(b'Git LFS pointers', result.stderr)
                    self.assertEqual(path.read_bytes(), before)
                    path.write_bytes(payload)
            font.unlink()
            fonts.rmdir()
            result = subprocess.run(['bash', '-e', '-o', 'pipefail', '-c', script],
                                    cwd=work, capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(b'Unable to inspect', result.stderr)

    def test_sdk_cleanup_runs_only_below_fifty_gibibytes(self):
        step = self.build.split('      - name: Preserve disk headroom on small runners\n', 1)[1].split('      - name:', 1)[0]
        script = '\n'.join(line[10:] for line in step.split('        run: |\n', 1)[1].splitlines())
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            commands = {
                'df': 'if [[ "$*" == *"--output=avail"* ]]; then printf "Avail\\n%12s\\n" "$AVAILABLE_BYTES"; else echo "disk report"; fi',
                'sudo': 'printf "%s\\n" "$*" >> "$CLEANUP_CALLS"',
                'free': 'echo "memory report"',
            }
            for name, body in commands.items():
                executable = root / name
                executable.write_text('#!/usr/bin/env bash\n' + body + '\n')
                executable.chmod(0o755)
            for available, cleans in ((19 * 1024**3, True), (50 * 1024**3 - 1, True),
                                      (50 * 1024**3, False), (88 * 1024**3, False)):
                with self.subTest(available=available):
                    calls = root / 'calls'
                    calls.unlink(missing_ok=True)
                    env = dict(os.environ, PATH=str(root) + os.pathsep + os.environ['PATH'],
                               AVAILABLE_BYTES=str(available), CLEANUP_CALLS=str(calls))
                    result = subprocess.run(['bash', '-e', '-o', 'pipefail', '-c', script],
                                            env=env, capture_output=True, text=True)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(calls.exists(), cleans)
                    self.assertEqual(result.stdout.count('disk report'), 2)
                    if cleans:
                        self.assertEqual(calls.read_text().splitlines(), [
                            'rm -rf /usr/local/lib/android /usr/share/dotnet /usr/share/swift /opt/ghc /usr/local/.ghcup'])

    def test_one_image_build_follows_the_bevy_import_without_legacy_caches(self):
        self.assertNotIn('images-deps-node-all', self.build)
        self.assertNotIn('image-nix', self.build)
        streams = self.build.index("nix build --impure 'git+file:.?lfs=1#image-apis-stream' 'git+file:.?lfs=1#image-sites-stream'")
        images = self.build.index("nix build --impure 'git+file:.?lfs=1#image-apis' 'git+file:.?lfs=1#image-sites'")
        self.assertLess(self.build.index('tooling/fetch-play.py --wait 0'), streams)
        self.assertLess(streams, images)
        self.assertEqual(self.build.count('nix build --impure'), 2)
        self.assertNotIn('nix build .#image-apis', self.build)
        loads = self.build[self.build.rindex('if [ "$GITHUB_EVENT_NAME" = pull_request ]; then\n', 0, streams):self.build.index('tooling/image-smoke.py')]
        self.assertIn('"$apis" | docker load &\n', loads)
        self.assertIn('"$sites" | docker load\n', loads)
        self.assertIn('docker load --input .cache/images/apis.tar.gz &\n', loads)
        self.assertIn("'git+file:.?lfs=1#image-sites-polygon-credits' --no-link", loads)
        self.assertIn('docker load --input .cache/images/sites.tar.gz\n', loads)
        self.assertIn('"$polygon" | docker load\n', loads)
        self.assertIn("'git+file:.?lfs=1#image-sites-polygon-credits-stream' --no-link", loads)
        self.assertIn('docker load --input .cache/images/sites-polygon-credits.tar.gz\n', loads)
        self.assertEqual(loads.count('wait "$apis_load"'), 2)

    def test_pull_requests_stream_the_images_and_upload_no_archive(self):
        upload = self.build.split('      - name: Upload tested images\n', 1)[1].split('      - name:', 1)[0]
        self.assertIn("if: github.event_name != 'pull_request'", upload)
        receipt = self.build.split('      - name: Upload the production receipt for validation\n', 1)[1].split('      - name:', 1)[0]
        self.assertIn("if: github.event_name != 'pull_request'", receipt)

    def test_exact_compiled_hits_skip_the_dependency_restores_and_builds(self):
        compiled = self.build.index('id: compiled\n')
        for group in ('apis', 'sites-app'):
            self.assertLess(self.build.index(f'id: {group}-cache\n'), compiled)
        for group, hit in (('catalyst', 'apis'), ('sites', 'sites-app')):
            restore = self.build.index(f'id: {group}-cache\n')
            self.assertLess(compiled, restore)
            condition = self.build.rfind('        if: ', 0, restore)
            self.assertEqual(self.build[condition:restore].split('\n', 1)[0],
                             f"        if: steps.compiled.outputs.{hit} != 'true'")
        toolchain = self.build.index('      - name: Build the complete Sites toolchain and dependencies\n')
        self.assertIn("if: steps.compiled.outputs.sites-app != 'true'",
                      self.build[toolchain:].split('      - name:', 2)[1])
        self.assertIn('.cache/catalyst-nix .cache/sites-nix .cache/apis-nix .cache/sites-app-nix', self.build)
        step = self.build.split('        id: compiled\n', 1)[1].split('      - name:', 1)[0]
        script = '\n'.join(line[10:] for line in step.split('        run: |\n', 1)[1].splitlines())
        for matched, expected in [('', 'false'), ('images-v2-pr-93-apis-amd64-abc', 'true'),
                                  ('images-v2-main-apis-amd64-abc', 'true'),
                                  ('images-v2-main-apis-amd64-old', 'false')]:
            with self.subTest(matched=matched), tempfile.TemporaryDirectory() as directory:
                output = Path(directory) / 'outputs'
                env = dict(os.environ, GITHUB_OUTPUT=str(output),
                           APIS=f'images-v2-pr-93-apis-amd64-abc {matched}',
                           SITES_APP=f'images-v2-pr-93-sites-app-amd64-abc {matched}')
                subprocess.run(['bash', '-e', '-c', script], env=env, check=True, capture_output=True)
                self.assertEqual(dict(line.split('=') for line in output.read_text().splitlines()),
                                 {'apis': expected, 'sites-app': expected})

    def test_stale_dependency_hits_fall_back_to_the_newest_main_entry_and_still_save(self):
        for group in ('catalyst', 'sites'):
            restore = self.build.split(f'id: {group}-cache\n', 1)[1].split('      - name:', 1)[0]
            self.assertIn(f'${{{{ steps.cache-keys.outputs.{group}-main-key }}}}\n', restore)
            self.assertIn(f'${{{{ steps.cache-keys.outputs.{group}-main-prefix }}}}\n', restore)
        for group in cache.RUNTIME:
            self.assertNotIn(f'{group}-main-prefix', self.build)
            restore = self.build.split(f'id: {group}-cache\n', 1)[1].split('      - name:', 1)[0]
            self.assertIn(f'restore-keys: ${{{{ steps.cache-keys.outputs.{group}-main-key }}}}\n', restore)
        self.assertNotIn("cache-matched-key == ''", self.build)
        decision = self.build.index('id: cache-saves')
        for group in cache.PACKAGES:
            self.assertLess(self.build.index(f'id: {group}-cache'), decision)
            self.assertEqual(self.build.count(f"steps.cache-saves.outputs.{group} == 'true'"), 2)
        self.assertLess(decision, self.build.index('Import cached store paths'))

    def test_upload_guard_uses_pr_head_and_skips_reruns_superseded_heads_and_failed_queries(self):
        step = self.build.split('      - name: Allow cache uploads only from current first attempts\n', 1)[1]
        script = textwrap.dedent(step.split('        run: |\n', 1)[1].split('      - name:', 1)[0])
        self.assertIn("if: steps.cache-keys.outputs.save == 'true'", step.split('        run:', 1)[0])
        self.assertLess(self.build.index('tooling/image-smoke.py'), self.build.index('id: cache-writer'))
        self.assertEqual(self.build.count("steps.cache-writer.outputs.allowed == 'true'"), 8)
        self.assertIn('PR_HEAD_SHA: ${{ github.event.pull_request.head.sha }}', step.split('        run:', 1)[0])
        cases = [('1', 'head', '0', True, True, 'head'),
                 ('2', 'head', '0', False, False, 'head'),
                 ('1', 'newer', '0', False, True, 'head'),
                 ('1', 'head', '1', False, True, 'head')]
        for ref in ('refs/heads/main', 'refs/pull/93/merge'):
            pr = ref.startswith('refs/pull/')
            variants = cases + ([('1', 'head', '0', False, False, '')] if pr else [])
            for attempt, current, failure, allowed, queried, event_head in variants:
                with self.subTest(ref=ref, attempt=attempt, current=current, failure=failure), tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    gh = root / 'gh'
                    gh.write_text('#!/usr/bin/env bash\nprintf "%s\\n" "$*" > "$API_CALL"\n'
                                  'echo "$CURRENT_SHA"\nexit "$API_FAILURE"\n')
                    gh.chmod(0o755)
                    env = dict(os.environ, PATH=str(root) + os.pathsep + os.environ['PATH'],
                               GITHUB_OUTPUT=str(root / 'output'), API_CALL=str(root / 'called'),
                               GITHUB_REF=ref, GITHUB_SHA='event-merge-before-main-advanced' if pr else 'head',
                               GITHUB_EVENT_NAME='pull_request' if pr else 'push',
                               PR_HEAD_SHA=event_head, GITHUB_RUN_ATTEMPT=attempt,
                               GITHUB_REPOSITORY='decentraland/dcl-one',
                               CURRENT_SHA=current, API_FAILURE=failure)
                    result = subprocess.run(['bash', '-e', '-o', 'pipefail', '-c', script],
                                            env=env, capture_output=True, text=True)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual((root / 'output').read_text(), f'allowed={str(allowed).lower()}\n')
                    self.assertEqual((root / 'called').exists(), queried)
                    if queried:
                        checked_ref = 'pull/93/head' if pr else 'heads/main'
                        self.assertEqual((root / 'called').read_text().strip(),
                                         f'api repos/decentraland/dcl-one/git/ref/{checked_ref} --jq .object.sha')

    def test_compiled_exports_require_matching_derivations_and_successful_evaluations(self):
        exports = {'Export the tested APIs package': 'apis', 'Export the tested Sites app': 'sites-app'}
        for name, group in exports.items():
            package = cache.PACKAGES[group][0]
            step = self.build.split(f'      - name: {name}\n', 1)[1]
            script = textwrap.dedent(step.split('        run: |\n', 1)[1].split('      - name:', 1)[0])
            # The export copies every package its cache key names.
            self.assertIn(' ' + ' '.join(f'.#{p}' for p in cache.PACKAGES[group]) + '\n', script)
            for image_drv, failure, expected in [('same.drv', '', True), ('different.drv', '', False),
                                                  ('same.drv', f'.#{package}.drvPath', False),
                                                  ('same.drv', f'git+file:.?lfs=1#{package}.drvPath', False)]:
                with self.subTest(package=package, image_drv=image_drv, failure=failure), \
                        tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    nix = root / 'nix'
                    nix.write_text('#!/usr/bin/env bash\n'
                                   'echo "$*" >> "$NIX_CALLS"\n'
                                   'if [[ "$1" == eval ]]; then\n'
                                   '  if [[ "${!#}" == "$FAIL_REF" ]]; then exit 1; fi\n'
                                   '  if [[ "${!#}" == .#*.drvPath ]]; then echo same.drv; else echo "$IMAGE_DRV"; fi\n'
                                   'fi\n')
                    nix.chmod(0o755)
                    env = dict(os.environ, PATH=str(root) + os.pathsep + os.environ['PATH'],
                               NIX_CALLS=str(root / 'calls'), FAIL_REF=failure, IMAGE_DRV=image_drv)
                    result = subprocess.run(['bash', '-e', '-o', 'pipefail', '-c', script],
                                            cwd=root, env=env, capture_output=True, text=True)
                    self.assertEqual(result.returncode == 0, expected, result.stderr)
                    calls = (root / 'calls').read_text()
                    self.assertEqual(any(line.startswith('copy ') for line in calls.splitlines()), expected)
                    if not failure:
                        self.assertIn(f'eval --raw --impure git+file:.?lfs=1#{package}.drvPath', calls)

    def test_actual_save_script_handles_miss_exact_stale_and_main_digest_hits(self):
        step = self.build.split('      - name: Decide which caches a successful build saves\n', 1)[1]
        script = step.split('        run: |\n', 1)[1].split('      - name:', 1)[0]
        script = '\n'.join(line[10:] for line in script.splitlines())
        for matched, expected in [('', 'true'), ('images-v2-pr-93-catalyst-amd64-abc', 'false'),
                                  ('images-v2-main-catalyst-amd64-old', 'true'),
                                  ('images-v2-main-catalyst-amd64-abc', 'false')]:
            for skipped in ('', 'CATALYST', 'CATALYST SITES', ' SITES'):
                with self.subTest(matched=matched, skipped=skipped), tempfile.TemporaryDirectory() as directory:
                    output = Path(directory) / 'outputs'
                    env = dict(os.environ, GITHUB_OUTPUT=str(output), SKIPPED=skipped)
                    for group in ('CATALYST', 'SITES', 'APIS', 'SITES_APP'):
                        env[group] = f'images-v2-pr-93-catalyst-amd64-abc {matched}'
                    subprocess.run(['bash', '-e', '-c', script], env=env, check=True, capture_output=True)
                    decided = dict.fromkeys(('catalyst', 'sites', 'apis', 'sites-app'), expected)
                    for group in skipped.split():
                        decided[group.lower()] = 'false'
                    self.assertEqual(dict(line.split('=') for line in output.read_text().splitlines()), decided)


class ImageRetentionTest(unittest.TestCase):
    def sweep(self, entries, closed=(), event='schedule', number=''):
        workflow = Path(__file__).parents[2] / '.github/workflows/closed-pr-cache.yml'
        source = workflow.read_text()
        script = textwrap.dedent(source.split("python3 - <<'PY'\n", 1)[1].rsplit('\n          PY', 1)[0])
        calls = []
        def api(args, **kwargs):
            calls.append(args)
            if any(arg.endswith('/actions/caches') for arg in args):
                return json.dumps([{'actions_caches': entries[:3]}, {'actions_caches': entries[3:]}])
            pr = args[-1].rsplit('/', 1)[-1]
            return json.dumps({'state': 'closed' if pr in closed else 'open'})
        with patch.dict(os.environ, {'GITHUB_EVENT_NAME': event, 'PR_NUMBER': number,
                                     'GITHUB_REPOSITORY': 'decentraland/dcl-one'}), \
                patch('subprocess.check_output', side_effect=api), \
                patch('subprocess.run') as deletion, patch('builtins.print'):
            exec(compile(script, str(workflow), 'exec'), {})
        return {int(call.args[0][-1].rsplit('/', 1)[-1]) for call in deletion.call_args_list}

    def entry(self, number, scope='main', group='catalyst', arch='amd64', **changes):
        ref = 'refs/heads/main' if scope == 'main' else f'refs/pull/{scope[3:]}/merge'
        return dict(id=number, ref=ref,
                    key=f'images-v2-{scope}-{group}-{arch}-{number:064x}',
                    created_at=f'2026-11-08T00:{number:02d}:00Z', **changes)

    def test_two_generations_per_group_architecture_and_ref(self):
        rows, expected = [], set()
        for scope in ('main', 'pr-93', 'pr-94'):
            for group in ('catalyst', 'sites', 'apis', 'sites-app'):
                for arch in ('amd64', 'arm64'):
                    start = len(rows) + 1
                    rows += [self.entry(i, scope, group, arch) for i in range(start, start + 3)]
                    expected.add(start)
        for event in ('schedule', 'workflow_dispatch'):
            with self.subTest(event=event):
                self.assertEqual(self.sweep(rows[::-1], event=event), expected)

    def test_creation_time_then_id_not_listing_order_or_access_time(self):
        rows = [self.entry(i) for i in (1, 2, 3, 4)]
        for row in rows:
            row['created_at'] = '2026-11-08T00:00:00Z'
        rows[0]['created_at'] = '2026-11-09T00:00:00Z'
        rows[1]['last_accessed_at'] = '2026-11-10T00:00:00Z'
        self.assertEqual(self.sweep(rows), {2, 3})

    def test_unrelated_keys_and_scope_mismatches_survive(self):
        rows = [self.entry(i) for i in range(1, 22)]
        for i, row in enumerate(rows):
            variant = i % 7
            if variant == 0:
                row['key'] = row['key'].replace('images-v2-main-', 'images-v2-unrelated-')
            elif variant == 1:
                row['ref'] = 'refs/heads/feature'
            elif variant == 2:
                row['key'] = row['key'].replace('-main-', '-pr-93-')
            elif variant == 3:
                row['ref'] = 'refs/pull/93/merge'
            elif variant == 4:
                row['key'] += '-extra'
            elif variant == 5:
                row['key'] = 'other-cache-' + row['key']
            else:
                row['ref'] = 'refs/pull/93/head'
        self.assertEqual(self.sweep(rows), set())

    def test_closed_pr_removes_all_its_entries_without_touching_main(self):
        rows = [self.entry(1), self.entry(2)] + [self.entry(i, 'pr-93') for i in (3, 4, 5)]
        rows[-1]['key'] = 'unrelated-on-closed-pr'
        self.assertEqual(self.sweep(rows, closed=('93',)), {3, 4, 5})
        self.assertEqual(self.sweep(rows, event='pull_request', number='93'), {3, 4, 5})

    def test_concurrent_upload_outside_snapshot_is_never_deleted(self):
        rows = [self.entry(i) for i in range(1, 5)]
        self.assertEqual(self.sweep(rows), {1, 2})
        # A later sweep sees the new upload and retains it plus its predecessor.
        self.assertEqual(self.sweep(rows[2:] + [self.entry(5)]), {3})


if __name__ == '__main__':
    unittest.main()
