"""Sites accepts only matching, intact builds from the permitted producer."""
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import tempfile
import unittest
from unittest.mock import ANY, patch
import zipfile
from hud_manifest_fixture import seal, SHARED

spec = importlib.util.spec_from_file_location('fetch_play', Path(__file__).resolve().parents[1] / 'fetch-play.py')
play = importlib.util.module_from_spec(spec)
spec.loader.exec_module(play)
contract = play.artifact
SHA = 'a' * 40


def run(**changes):
    return {'id': 42, 'run_attempt': 1, 'path': play.WORKFLOW, 'head_sha': SHA, 'head_branch': 'main', 'event': 'push',
            'head_repository': {'full_name': play.REPO}, 'status': 'completed', 'conclusion': 'success', **changes}


class GitHubReadTests(unittest.TestCase):
    def error(self, status):
        return subprocess.CalledProcessError(1, ['gh', 'api'], stderr=f'gh: error (HTTP {status})\n')

    def test_transient_failure_retries_the_same_get_then_returns_the_response(self):
        for failure in (self.error(500), self.error(502), self.error(503), self.error(504),
                        subprocess.TimeoutExpired(['gh', 'api'], 60)):
            with self.subTest(failure=failure), \
                    patch.object(play.subprocess, 'check_output', side_effect=[failure, '{"jobs": []}']) as get, \
                    patch.object(play.time, 'sleep') as sleep:
                self.assertEqual(play.api('actions/runs/42/jobs'), {'jobs': []})
                self.assertEqual(get.call_args_list[0], get.call_args_list[1])
                get.assert_called_with(
                    ['gh', 'api', '--method', 'GET', f'repos/{play.REPO}/actions/runs/42/jobs'],
                    text=True, stderr=subprocess.PIPE, timeout=60)
                sleep.assert_called_once_with(2)

    def test_persistent_outage_exhausts_bounded_retries_and_raises(self):
        for failure in (self.error(502), subprocess.TimeoutExpired(['gh', 'api'], 60)):
            with self.subTest(failure=failure), \
                    patch.object(play.subprocess, 'check_output', side_effect=failure) as get, \
                    patch.object(play.time, 'sleep') as sleep:
                with self.assertRaises(type(failure)):
                    play.api('actions/runs/42')
                self.assertEqual(get.call_count, 5)
                self.assertEqual([call.args[0] for call in sleep.call_args_list], [2, 4, 8, 16])

    def test_auth_not_found_and_unknown_errors_do_not_retry(self):
        for status in (400, 401, 403, 404, 418, 422):
            with self.subTest(status=status), \
                    patch.object(play.subprocess, 'check_output', side_effect=self.error(status)) as get, \
                    patch.object(play.time, 'sleep') as sleep:
                with self.assertRaises(subprocess.CalledProcessError):
                    play.api('actions/runs/42')
                self.assertEqual(get.call_count, 1)
                sleep.assert_not_called()

    def test_rate_limit_backs_off_from_a_minute_and_stays_bounded(self):
        limited = [subprocess.CalledProcessError(1, ['gh', 'api'], stderr=text) for text in (
            'gh: API rate limit exceeded for installation ID 7. (HTTP 403)\n',
            'gh: You have exceeded a secondary rate limit. Please wait a few minutes. (HTTP 403)\n',
            'gh: Too Many Requests (HTTP 429)\n')]
        for failure in limited:
            with self.subTest(failure=failure.stderr), \
                    patch.object(play.subprocess, 'check_output', side_effect=[failure, '{}']) as get, \
                    patch.object(play.time, 'sleep') as sleep:
                self.assertEqual(play.api('actions/runs/42'), {})
                self.assertEqual(get.call_count, 2)
                sleep.assert_called_once_with(60)
        # Without a wait budget: five tries. With one: doubled to a cap, never past the deadline.
        with patch.object(play.subprocess, 'check_output', side_effect=limited[0]) as get, \
                patch.object(play.time, 'sleep') as sleep:
            with self.assertRaises(subprocess.CalledProcessError):
                play.api('actions/runs/42')
            self.assertEqual(get.call_count, 5)
            self.assertEqual([call.args[0] for call in sleep.call_args_list], [60, 120, 240, 240])
        clock = [0]
        with patch.object(play.subprocess, 'check_output', side_effect=limited[1]) as get, \
                patch.object(play.time, 'monotonic', side_effect=lambda: clock[0]), \
                patch.object(play.time, 'sleep',
                             side_effect=lambda delay: clock.__setitem__(0, clock[0] + delay)) as sleep:
            with self.assertRaises(subprocess.CalledProcessError):
                play.api('actions/runs/42', deadline=500)
            self.assertEqual([call.args[0] for call in sleep.call_args_list], [60, 120, 240, 80])
            self.assertEqual(clock[0], 500)

    def test_invalid_json_does_not_retry_or_become_a_successful_read(self):
        with patch.object(play.subprocess, 'check_output', return_value='not JSON') as get, \
                patch.object(play.time, 'sleep') as sleep:
            with self.assertRaises(ValueError):
                play.api('actions/runs/42')
            self.assertEqual(get.call_count, 1)
            sleep.assert_not_called()


    def test_long_wait_recovers_after_more_than_four_failures_with_capped_backoff(self):
        clock = [0]
        with patch.object(play.subprocess, 'check_output',
                          side_effect=[self.error(502)] * 6 + ['{}']) as get, \
                patch.object(play.time, 'monotonic', side_effect=lambda: clock[0]), \
                patch.object(play.time, 'sleep',
                             side_effect=lambda delay: clock.__setitem__(0, clock[0] + delay)) as sleep:
            self.assertEqual(play.api('actions/runs/42', deadline=100), {})
        self.assertEqual(get.call_count, 7)
        self.assertEqual([call.args[0] for call in sleep.call_args_list], [2, 4, 8, 16, 30, 30])
        self.assertEqual(clock[0], 90)

    def test_wait_budget_bounds_requests_and_backoff_without_starting_another_read(self):
        clock = [0]
        def failure(*args, **kwargs):
            clock[0] += 2
            raise self.error(502)
        with patch.object(play.subprocess, 'check_output', side_effect=failure) as get, \
                patch.object(play.time, 'monotonic', side_effect=lambda: clock[0]), \
                patch.object(play.time, 'sleep',
                             side_effect=lambda delay: clock.__setitem__(0, clock[0] + delay)) as sleep:
            with self.assertRaises(subprocess.CalledProcessError):
                play.api('actions/runs/42', deadline=10)
            self.assertEqual([call.kwargs['timeout'] for call in get.call_args_list], [10, 6])
            self.assertEqual([call.args[0] for call in sleep.call_args_list], [2, 4])
            self.assertEqual(clock[0], 10)
            get.reset_mock()
            with self.assertRaises(TimeoutError):
                play.api('actions/runs/42', deadline=10)
            get.assert_not_called()

    def test_bare_http_status_lines_are_transient_too(self):
        failure = subprocess.CalledProcessError(1, ['gh', 'api'], stderr='gh: HTTP 503\n')
        with patch.object(play.subprocess, 'check_output', side_effect=[failure, '{"jobs": []}']), \
                patch.object(play.time, 'sleep') as sleep:
            self.assertEqual(play.api('actions/runs/42/jobs'), {'jobs': []})
        sleep.assert_called_once_with(2)

    def test_artifact_download_retries_a_transient_failure_from_a_clean_file(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        archive = Path(temporary.name) / 'web.zip'
        command = ['gh', 'api', f'repos/{play.REPO}/actions/artifacts/9/zip']
        # A 5xx, a dropped connection and a stalled transfer each get a second attempt with a full 300 s.
        for failure in (subprocess.CalledProcessError(1, command, stderr=b'gh: HTTP 503\n'),
                        subprocess.CalledProcessError(1, command, stderr=b'error reading response body: unexpected EOF\n'),
                        subprocess.TimeoutExpired(command, 300)):
            calls = []

            def gh(args, **kwargs):
                calls.append((args, kwargs['timeout']))
                kwargs['stdout'].write(b'partial' if len(calls) == 1 else b'archive')
                if len(calls) == 1:
                    raise failure
                return subprocess.CompletedProcess(args, 0)

            errors = io.StringIO()
            with patch.object(play.subprocess, 'run', side_effect=gh), patch.object(play.time, 'sleep') as sleep, \
                    patch('sys.stderr', errors):
                play.download({'id': 9}, archive)
            self.assertEqual(archive.read_bytes(), b'archive')
            self.assertEqual(calls, [(command, 300)] * 2)
            sleep.assert_called_once_with(2)
            self.assertEqual(errors.getvalue(), (failure.stderr or b'').decode())
        # Any other failure is reported at once.
        missing = subprocess.CalledProcessError(1, ['gh', 'api'], stderr=b'gh: HTTP 404\n')
        with patch.object(play.subprocess, 'run', side_effect=missing), patch.object(play.time, 'sleep') as sleep, \
                self.assertRaises(subprocess.CalledProcessError):
            play.download({'id': 9}, archive)
        sleep.assert_not_called()


class PlayImportTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.build = self.root / 'build'
        for name in contract.INPUTS:
            self.write(name, 'build input')
        self.write(contract.BINDINGS + '/Example.ts', 'export type Example = string;\n')
        self.write(contract.HUD + 'App.tsx', 'export {}\n')
        self.write(contract.SOURCE + 'src/lib.rs', 'fn engine() {}\n')
        seal(self.root)
        for name in contract.REQUIRED:
            self.write('build/' + name, b'\0asm\x01\0\0\0fixture' if name.endswith('.wasm') else b'fixture')
        self.write('build/build-info.json', json.dumps({'revision': SHA, 'engine': {
            'revision': SHA, 'inputs': contract.engine_inputs(self.root), 'optimized': True}}))
        contract.pack(self.build, self.root)
        root_patch = patch.object(play, 'ROOT', self.root)
        root_patch.start()
        self.addCleanup(root_patch.stop)

    def write(self, name, content):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(content.encode() if isinstance(content, str) else content)
        if (self.root / contract.shared_inputs.MANIFEST).exists():
            seal(self.root)
        return path

    def test_import_keeps_only_served_content_and_contract_and_reuses_it(self):
        self.write('build/index.html', 'standalone HUD is not Sites')
        destination = play.install(self.build, production=True)
        self.assertEqual((self.root / '.cache/play/current').resolve(), destination.resolve())
        self.assertFalse((destination / 'index.html').exists())
        self.assertEqual((destination / 'engine/pkg/webgpu_build_bg.wasm').read_bytes(),
                         (self.build / 'engine/pkg/webgpu_build_bg.wasm').read_bytes())
        self.assertEqual(play.install(self.build, production=True), destination)

    def test_tampered_wasm_or_worker_never_replaces_current(self):
        destination = play.install(self.build)
        for name in ('engine/pkg/webgpu_build_bg.wasm', 'service_worker.js'):
            path = self.build / name
            original = path.read_bytes()
            path.write_bytes(original + b'tampered')
            with self.assertRaisesRegex(ValueError, 'checksum'):
                play.install(self.build)
            self.assertEqual((self.root / '.cache/play/current').resolve(), destination.resolve())
            path.write_bytes(original)

    def test_cached_self_consistent_content_must_match_the_validated_import(self):
        destination = play.install(self.build)
        worker = destination / 'service_worker.js'
        worker.write_text('different locally built worker')
        meta = json.loads((destination / contract.MANIFEST).read_text())
        meta['files']['service_worker.js'] = contract.digest(worker.read_bytes())
        (destination / contract.MANIFEST).write_text(json.dumps(meta))
        contract.verify(destination, self.root)  # Internally valid, but not the trusted archive.
        with self.assertRaisesRegex(ValueError, 'differs from the validated import'):
            play.install(self.build, production=True)

    def test_extra_files_missing_files_and_symlinks_are_rejected(self):
        extra = self.write('build/engine/extra.js', 'unexpected')
        with self.assertRaisesRegex(ValueError, 'inventory'):
            play.install(self.build)
        extra.unlink()
        target = self.build / 'service_worker.js'
        target.unlink()
        target.symlink_to(self.root / contract.BINDINGS / 'Example.ts')
        with self.assertRaisesRegex(ValueError, 'linked'):
            play.install(self.build)

    def test_production_refuses_unoptimized_pr_build(self):
        info = contract.read_json(self.build / 'build-info.json')
        info['engine']['optimized'] = False
        self.write('build/build-info.json', json.dumps(info))
        contract.pack(self.build, self.root, optimized=False)
        play.install(self.build)
        with self.assertRaisesRegex(ValueError, 'optimized'):
            play.install(self.build, production=True)

    def test_production_revalidates_remote_even_with_an_optimized_local_cache(self):
        play.install(self.build)
        with patch.object(play.subprocess, 'run'), patch.object(play, 'fetch') as fetch:
            self.assertEqual(play.main(['--production', '--wait', '0']), 0)
        fetch.assert_called_once_with(0, True)
        with patch.object(play.subprocess, 'run'), \
                patch.object(play, 'fetch', side_effect=RuntimeError('No trusted artifact')), \
                self.assertRaisesRegex(RuntimeError, 'No trusted artifact'):
            play.main(['--production'])

    def test_engine_provenance_is_required_and_preserved_separately(self):
        info = contract.read_json(self.build / 'build-info.json')
        info['engine']['revision'] = 'b' * 40
        self.write('build/build-info.json', json.dumps(info))
        meta = contract.pack(self.build, self.root)
        self.assertEqual(meta['build_revision'], SHA)
        self.assertEqual(meta['engine']['revision'], 'b' * 40)
        for value in (None, {}, {**info['engine'], 'inputs': 'bad'},
                      {**info['engine'], 'revision': ''}, {**info['engine'], 'optimized': False}):
            self.write('build/build-info.json', json.dumps({**info, 'engine': value}))
            with self.subTest(engine=value), self.assertRaises(ValueError):
                contract.pack(self.build, self.root)

    def test_production_cannot_import_local_builds_or_only_print_a_path(self):
        for option in (['--from-build', str(self.build)], ['--print-directory']):
            with patch.object(play.subprocess, 'run') as command, \
                    self.assertRaises(SystemExit) as error:
                play.main(['--production', *option])
            self.assertEqual(error.exception.code, 2)
            command.assert_not_called()

    def test_changed_build_script_invalidates_import(self):
        self.write('apps/bevy-explorer/build-web.py', 'different flags')
        with self.assertRaisesRegex(ValueError, 'inputs differ'):
            play.install(self.build)

    def test_headless_launcher_edits_keep_the_matching_web_artifact(self):
        self.write(contract.SOURCE + 'deploy/headless/launcher/package.json', 'native package')
        self.write(contract.SOURCE + 'deploy/headless/launcher/bin/cli.js', 'native launcher')
        play.install(self.build, production=True)

    def test_web_package_edits_require_a_new_assembled_artifact(self):
        self.write(contract.SOURCE + 'deploy/web/package.json', 'changed web package')
        with self.assertRaisesRegex(ValueError, 'inputs differ'):
            play.install(self.build)

    def test_generated_bindings_must_match(self):
        self.write(contract.BINDINGS + '/Example.ts', 'different protocol')
        with self.assertRaisesRegex(ValueError, 'bindings differ'):
            play.install(self.build)

    def test_first_party_engine_edits_invalidate_the_artifact(self):
        self.write(contract.SOURCE + 'src/lib.rs', 'fn changed_engine() {}\n')
        with self.assertRaisesRegex(ValueError, 'inputs differ'):
            play.install(self.build)

    def test_bevy_patch_edits_invalidate_the_artifact(self):
        # The patch series decides the Bevy tree the engine compiles; this fixture has no .git, so
        # this is the walk Nix's filtered contract takes.
        self.write('rendering/bevy-explorer/patches/bevy/series/0001-scene-spawner.patch', 'fn changed() {}\n')
        with self.assertRaisesRegex(ValueError, 'inputs differ'):
            play.install(self.build)

    def test_the_bevy_patch_series_is_an_engine_input_in_git_and_in_the_nix_contract(self):
        bevy = 'rendering/bevy-explorer/patches/bevy/series/0001-scene-spawner.patch'
        self.write(bevy, 'fn spawn() {}\n')
        crate = 'rendering/bevy-explorer/vendor/boimp/src/lib.rs'
        self.write(crate, 'pub fn import() {}\n')
        def git(*args):
            return subprocess.check_output(['git', '-c', 'user.name=CI', '-c', 'user.email=ci@example.invalid',
                                            *args], cwd=self.root, text=True).strip()
        git('init', '-q')
        git('config', 'maintenance.auto', 'false')
        git('config', 'gc.auto', '0')
        git('add', '.')
        git('commit', '-qm', 'fixture')
        self.assertLessEqual({bevy, crate, 'tooling/patched-sources.py'}, set(contract.input_files(self.root, engine_only=True)))
        key = contract.engine_inputs(self.root)
        self.assertEqual(key, contract.engine_inputs(self.root, git('rev-parse', 'HEAD')))
        filtered = self.root / 'nix-contract'
        for name in git('ls-files').splitlines():
            target = filtered / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(self.root / name, target)
        self.assertEqual(key, contract.engine_inputs(filtered))
        self.write(bevy, 'fn spawn_sliced() {}\n')
        self.assertNotEqual(key, contract.engine_inputs(self.root))

    def test_in_tree_crate_and_rebuild_tool_edits_invalidate_the_artifact(self):
        # bevy_dui, boimp and rpc-rust are compiled from tracked bytes; tooling/patched-sources.py writes the
        # Bevy and fork trees. No .git here: the walk Nix's filtered contract takes.
        for name in ('rendering/bevy-explorer/vendor/boimp/src/lib.rs', 'tooling/patched-sources.py'):
            with self.subTest(name=name):
                key = contract.engine_inputs(self.root)
                self.write(name, name + ' changed\n')
                self.assertNotEqual(key, contract.engine_inputs(self.root))
                with self.assertRaisesRegex(ValueError, 'inputs differ'):
                    play.install(self.build)

    def test_shared_hud_sources_rebuild_the_artifact_but_not_the_engine(self):
        # react-web compiles these through its aliases (react-web/vite.shared.ts).
        engine = contract.engine_inputs(self.root)
        for shared in SHARED:
            with self.subTest(shared=shared):
                self.write(shared + 'changed.ts' if shared.endswith('/') else shared, 'export const changed = true;\n')
                with self.assertRaisesRegex(ValueError, 'inputs differ'):
                    play.install(self.build)
                self.assertEqual(engine, contract.engine_inputs(self.root))
                contract.pack(self.build, self.root)
                play.install(self.build)

    def test_root_ui_exports_and_components_are_not_hud_inputs(self):
        def git(*args):
            return subprocess.check_output(['git', '-c', 'user.name=CI', '-c', 'user.email=ci@example.invalid',
                                            *args], cwd=self.root, text=True).strip()
        self.write('library/ui/index.ts', "export * from './foundations'\n")
        git('init', '-q')
        git('config', 'maintenance.auto', 'false')
        git('config', 'gc.auto', '0')
        git('add', '.')
        git('commit', '-qm', 'fixture')
        key = contract.inputs(self.root)
        for neighbour in ('library/ui/index.ts', 'library/ui/index.tsx', 'library/ui/index.ts.orig',
                          'library/ui/components/NavBar/NavBar.tsx',
                          'library/ui/components/NavBar/NavBar.module.css',
                          'library/ui/figma/src/cli.ts'):
            self.write(neighbour, 'export {};\n')
        git('add', '.')
        git('commit', '-qm', 'neighbours')
        self.assertEqual(key, contract.inputs(self.root))
        self.assertEqual(key, contract.inputs(self.root, git('rev-parse', 'HEAD')))

    def test_the_lfs_fonts_under_the_shared_foundations_are_no_input_either_way_they_are_checked_out(self):
        # A checkout without LFS holds a pointer; Nix's `git+file:.?lfs=1` source holds the font itself.
        font = 'library/ui/foundations/typography/fonts/inter-latin-wght-normal.woff2'
        self.write(font, 'version https://git-lfs.github.com/spec/v1\noid sha256:00\nsize 1\n')
        key = contract.inputs(self.root)
        self.write(font, b'wOF2 the font itself')
        self.assertEqual(key, contract.inputs(self.root))
        self.assertNotIn(font, contract.input_files(self.root))
        play.install(self.build)

    def test_shared_hud_sources_are_inputs_in_git_and_in_the_nix_contract(self):
        # Each entry, a tree or one file, the same in a checkout, in a commit and in Nix's filtered copy.
        names = list(SHARED)
        for name in names:
            self.write(name, 'export const zero = 0n;\n')
        def git(*args):
            return subprocess.check_output(['git', '-c', 'user.name=CI', '-c', 'user.email=ci@example.invalid',
                                            *args], cwd=self.root, text=True).strip()
        git('init', '-q')
        git('config', 'maintenance.auto', 'false')
        git('config', 'gc.auto', '0')
        git('add', '.')
        git('commit', '-qm', 'fixture')
        key = contract.inputs(self.root)
        self.assertEqual(key, contract.inputs(self.root, git('rev-parse', 'HEAD')))
        filtered = self.root / 'nix-contract'
        for name in git('ls-files').splitlines():
            target = filtered / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(self.root / name, target)
        for name in names:
            with self.subTest(name=name):
                for files in (contract.input_files(self.root), contract.input_files(self.root, git('rev-parse', 'HEAD')),
                              contract.input_files(filtered)):
                    self.assertIn(name, files)
                self.assertNotIn(name, contract.input_files(self.root, engine_only=True))
        self.assertEqual(key, contract.inputs(filtered))
        self.write(names[0], 'export const zero = 1n;\n')
        self.assertNotEqual(key, contract.inputs(self.root))

    def test_rebuilt_hud_changes_install_once_packed(self):
        self.write(contract.HUD + 'new-screen.ts', 'export const screen = "new";\n')
        contract.pack(self.build, self.root)
        play.install(self.build)

    def test_producer_build_records_the_bindings_sites_compares(self):
        meta = contract.pack(self.build, self.root)
        self.assertEqual(meta['files']['bindings/Example.ts'],
                         contract.digest(b'export type Example = string;\n'))
        self.write(contract.BINDINGS + '/Example.ts', 'different protocol')
        with self.assertRaisesRegex(ValueError, 'bindings differ'):
            play.install(self.build)

    def test_image_smoke_consumes_current_manifest_and_checks_the_dynamic_bridge(self):
        spec = importlib.util.spec_from_file_location(
            'image_smoke', Path(__file__).resolve().parents[1] / 'image-smoke.py')
        smoke = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(smoke)
        entity = 'baf' + 'a' * 56
        self.write(f'build/bridge-scene/static/{entity}', '{"content": []}')
        meta = contract.pack(self.build, self.root)
        self.assertEqual(meta['format'], 3)
        self.assertNotIn('upstream_revision', meta)
        self.write('apps/sites/content/docs/_media.json',
                   json.dumps({'/docs/media/fixture.png': {'bytes': 7}}))
        responses = {f'http://sites:5212/play/{name}': (self.build / name).read_bytes()
                     for name in (*meta['files'], contract.MANIFEST)}
        responses.update({
            'http://api:5210/about': b'{}',
            'http://api:5210/places/api/places?limit=1': b'[]',
            'http://sites:5212/': b'fixture',
            'http://sites:5212/docs/index.md': b'fixture',
            'http://sites:5212/docs/media/fixture.png': b'fixture',
            'http://sites:5212/play/bridge/about': json.dumps({'configurations': {
                'scenesUrn': [f'urn:decentraland:entity:{entity}?=&baseUrl='
                             'http://sites:5212/play/bridge-scene/static/']}}).encode(),
        })
        requested = []
        bootstraps = 0
        def docker(*args, check=True):
            nonlocal bootstraps
            status, stdout = 0, ''
            if args[:2] == ('image', 'inspect'):
                stdout = json.dumps([{'Config': {'User': '1000:1000'}}])
            elif args[0] == 'exec' and args[2] == 'node':
                url, mode = args[-2:]
                requested.append(url)
                data = responses[url]
                stdout = json.dumps({'bytes': len(data), 'sha256': contract.digest(data),
                                     'body': json.loads(data) if mode == 'json' else None})
            elif args[-2:] == ('schema', 'apply'):
                bootstraps += 1
                status = int(bootstraps > 1)
            elif args[-1] == 'SELECT value FROM public.image_smoke':
                stdout = 'preserved\n'
            return subprocess.CompletedProcess(args, status, stdout, '')
        output = io.StringIO()
        with patch.object(smoke, 'ROOT', self.root), patch.object(smoke, 'docker', docker), \
                patch('sys.argv', ['image-smoke', '--play', str(self.build)]), \
                patch('sys.stdout', output):
            smoke.main()
        self.assertIn('http://sites:5212/play/bridge/about', requested)
        self.assertIn(f'http://sites:5212/play/bridge-scene/static/{entity}', requested)
        self.assertIn(f'Sites serves verified Bevy producer {SHA}', output.getvalue())
        self.assertIn('Image smoke passed:', output.getvalue())

    def test_tooling_only_async_edits_do_not_invalidate_shipped_web_inputs(self):
        artifact = contract.inputs(self.root)
        engine = contract.engine_inputs(self.root)
        for source in ('export const retry = 1;', 'export const retry = 2;'):
            self.write('library/async/index.ts', source)
            self.assertEqual(contract.inputs(self.root), artifact)
            self.assertEqual(contract.engine_inputs(self.root), engine)
            play.install(self.build, production=True)

    def test_removed_source_input_invalidates_the_artifact(self):
        (self.root / contract.SOURCE / 'src/lib.rs').unlink()
        with self.assertRaisesRegex(ValueError, 'inputs differ'):
            play.install(self.build)

    def test_wasm_pointer_cannot_be_packed_even_with_updated_checksums(self):
        self.write('build/engine/pkg/webgpu_build_bg.wasm', 'version https://git-lfs.github.com/spec/v1')
        with self.assertRaisesRegex(ValueError, 'Invalid WASM'):
            contract.pack(self.build, self.root)

    def test_input_fingerprint_matches_git_and_ignores_unrelated_edits(self):
        self.write('.cargo/config.toml', '[build]')
        self.write('rust-toolchain.toml', '[toolchain]')
        self.write(contract.SOURCE + 'deploy/headless/launcher/package.json', 'native package')
        self.write(contract.SOURCE + 'deploy/web/package.json', 'web package')
        def git(*args):
            return subprocess.check_output(['git', '-c', 'user.name=CI', '-c', 'user.email=ci@example.invalid',
                                            *args], cwd=self.root, text=True).strip()
        git('init', '-q')
        # Fixture cleanup must not race detached maintenance after a commit.
        git('config', 'maintenance.auto', 'false')
        git('config', 'gc.auto', '0')
        git('add', '.')
        git('commit', '-qm', 'fixture')
        key = contract.inputs(self.root, git('rev-parse', 'HEAD'))
        self.assertEqual(key, contract.inputs(self.root))
        self.write('.cargo/local.toml', 'untracked operator override')
        self.write('rust-toolchain.notes', 'not a Rust toolchain input')
        self.write(contract.SOURCE + 'target/generated.rs', 'untracked build output')
        self.assertEqual(key, contract.inputs(self.root))
        # Nix receives only tracked files, with no repository metadata.
        filtered = self.root / 'nix-contract'
        for name in git('ls-files').splitlines():
            target = filtered / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(self.root / name, target)
        self.assertEqual(key, contract.inputs(filtered))
        self.write(contract.SOURCE + 'deploy/headless/launcher/package.json', 'changed native package')
        git('add', contract.SOURCE + 'deploy/headless')
        git('commit', '-qm', 'native packaging')
        self.assertEqual(key, contract.inputs(self.root))
        self.assertEqual(key, contract.inputs(self.root, git('rev-parse', 'HEAD')))
        self.write('apps/sites/route.tsx', 'unrelated route edit')
        self.assertEqual(key, contract.inputs(self.root))
        self.write(contract.SOURCE + 'src/lib.rs', 'fn changed_engine() {}\n')
        self.assertNotEqual(key, contract.inputs(self.root))
        self.write(contract.SOURCE + 'src/lib.rs', 'fn engine() {}\n')
        self.assertEqual(key, contract.inputs(self.root))
        self.write('rust-toolchain.toml', 'different compiler')
        self.assertNotEqual(key, contract.inputs(self.root))

    def test_untracked_cargo_config_does_not_change_committed_input_identity(self):
        subprocess.run(['git', 'init', '-q'], cwd=self.root, check=True)
        subprocess.run(['git', 'add', '.'], cwd=self.root, check=True)
        key = contract.inputs(self.root)
        self.write('.cargo/config.toml', '[build]\njobs = 2\n')
        self.write('rust-toolchain', 'nightly')
        self.assertEqual(key, contract.inputs(self.root))
        subprocess.run(['git', 'add', '.cargo/config.toml'], cwd=self.root, check=True)
        self.assertNotEqual(key, contract.inputs(self.root))

    def test_zip_rejects_traversal_git_symlinks_and_normalized_duplicates(self):
        cases = [['../escape'], ['/escape'], ['x/.git/config'], ['x\\escape'], ['x/y', 'x//y']]
        for names in cases:
            with self.subTest(names=names):
                archive = self.root / 'bad.zip'
                with zipfile.ZipFile(archive, 'w') as zipped:
                    for name in names:
                        zipped.writestr(name, 'bad')
                with self.assertRaises(ValueError):
                    play.extract(archive, self.root / 'extract')
        with zipfile.ZipFile(self.root / 'link.zip', 'w') as zipped:
            member = zipfile.ZipInfo('link')
            member.external_attr = (stat.S_IFLNK | 0o777) << 16
            zipped.writestr(member, '../outside')
        with self.assertRaises(ValueError):
            play.extract(self.root / 'link.zip', self.root / 'extract')
        self.assertFalse((self.root / 'extract').exists())

    def test_main_trust_excludes_foreign_repos_branches_and_workflows(self):
        self.assertTrue(play.trusted(run()))
        self.assertTrue(play.trusted(run(event='workflow_dispatch')))
        for changes in ({'head_repository': {'full_name': 'other/project'}}, {'head_branch': 'dev'},
                        {'event': 'pull_request'}, {'path': '.github/workflows/other.yml'}, {'head_sha': 'bad'}):
            self.assertFalse(play.trusted(run(**changes)))

    def test_pr_trust_is_limited_to_this_pr_head_and_cannot_supply_production(self):
        pr = {'number': 7, 'head': SHA, 'merge': 'b' * 40}
        candidate = run(event='pull_request', head_branch='feature', pull_requests=[{'number': 7}])
        self.assertTrue(play.trusted(candidate, pr))
        self.assertFalse(play.trusted(candidate))
        self.assertFalse(play.trusted(candidate, {**pr, 'head': 'c' * 40}))
        self.assertFalse(play.trusted(candidate, {**pr, 'number': 8}))

    def test_engine_changed_compares_the_pr_with_the_commit_it_is_based_on(self):
        pr = {'number': 7, 'head': SHA, 'base': 'b' * 40, 'merge': 'c' * 40}
        fingerprints = {None: 'ours', 'b' * 40: 'ours'}
        with patch.object(play.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0)), \
                patch.object(contract, 'inputs', side_effect=lambda root, revision=None: fingerprints[revision]):
            self.assertFalse(play.engine_changed(pr))
            fingerprints['b' * 40] = 'theirs'
            self.assertTrue(play.engine_changed(pr))
        self.assertTrue(play.engine_changed(None))
        self.assertTrue(play.engine_changed({'number': 7, 'head': SHA, 'merge': 'c' * 40}))
        with patch.object(play.subprocess, 'run', side_effect=OSError):
            self.assertTrue(play.engine_changed(pr))

    def test_pr_base_is_the_main_tip_the_merge_commit_was_built_on(self):
        tip, head, stale, merge = 'd' * 40, 'e' * 40, 'f' * 40, 'c' * 40
        header = lambda *parents: subprocess.CompletedProcess(
            [], 0, (''.join(f'parent {sha}\n' for sha in parents) + f'author CI\n\nparent {stale}\n').encode(), b'')
        with tempfile.TemporaryDirectory() as temporary:
            event = Path(temporary) / 'event.json'
            event.write_text(json.dumps({'number': 7, 'pull_request': {'head': {'sha': head}, 'base': {'sha': stale}}}))
            env = {'GITHUB_EVENT_NAME': 'pull_request', 'GITHUB_EVENT_PATH': str(event), 'GITHUB_SHA': merge}
            with patch.dict(os.environ, env):
                with patch.object(play.subprocess, 'run', return_value=header(tip, head)):
                    self.assertEqual(play.current_pr(), {'number': 7, 'head': head, 'base': tip, 'merge': merge})
                for result in (header(head), header(tip, stale)):
                    with patch.object(play.subprocess, 'run', return_value=result):
                        self.assertEqual(play.current_pr()['base'], stale)
                for error in (subprocess.CalledProcessError(128, 'git'), OSError()):
                    with patch.object(play.subprocess, 'run', side_effect=error):
                        self.assertEqual(play.current_pr()['base'], stale)

    def test_pr_without_engine_changes_takes_the_newest_finished_main_build_without_matching(self):
        pr = {'number': 7, 'head': SHA, 'base': 'b' * 40, 'merge': 'c' * 40}
        newest_running = run(id=45, status='in_progress', conclusion=None)
        newest_done = run(id=44, head_sha='d' * 40)
        older = run(id=43, head_sha='e' * 40)
        with patch.object(play, 'api', return_value={'workflow_runs': [newest_running, newest_done, older]}), \
                patch.object(play, 'current_pr', return_value=pr), \
                patch.object(play, 'engine_changed', return_value=False), \
                patch.object(play.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0)) as git, \
                patch.object(contract, 'inputs',
                             side_effect=lambda root, revision=None: 'theirs' if revision else 'ours') as inputs, \
                patch.object(play, 'download_run', side_effect=[None, self.build]) as download:
            self.assertEqual(play.fetch(), self.build)
        # The running newest is tried first (its web job may already have passed), then the newest finished one;
        # each is fingerprinted at its own commit (fetched if absent) and verified against it.
        self.assertEqual([call.args[0]['id'] for call in download.call_args_list], [45, 44])
        self.assertEqual([call.kwargs['producer'] for call in download.call_args_list], [SHA, 'd' * 40])
        self.assertEqual(inputs.call_count, 3)
        self.assertEqual([call.args[0][:2] for call in git.call_args_list], [['git', 'cat-file']] * 2)

    def test_inputs_at_fingerprints_a_present_commit_and_gives_none_for_a_missing_one(self):
        git = ['git', '-c', 'user.name=t', '-c', 'user.email=t@example.com', '-c', 'commit.gpgsign=false']
        subprocess.run([*git, 'init', '-q'], cwd=self.root, check=True)
        subprocess.run([*git, 'add', '-A'], cwd=self.root, check=True)
        subprocess.run([*git, 'commit', '-q', '-m', 'inputs'], cwd=self.root, check=True)
        head = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=self.root, text=True).strip()
        self.assertEqual(play.inputs_at(head), contract.inputs(self.root))
        # Absent from the checkout and not fetchable (no origin): skipped, never a crash.
        self.assertIsNone(play.inputs_at('f' * 40))

    def test_non_engine_pr_skips_a_main_build_the_tree_rejects_and_takes_the_next(self):
        pr = {'number': 7, 'head': SHA, 'base': 'b' * 40, 'merge': 'c' * 40}
        newest, older = run(id=44, head_sha='d' * 40), run(id=43, head_sha='e' * 40)
        rejected = ValueError('Sites engine bindings differ from this build')
        with patch.object(play, 'api', return_value={'workflow_runs': [newest, older]}), \
                patch.object(play, 'current_pr', return_value=pr), \
                patch.object(play, 'engine_changed', return_value=False), \
                patch.object(play.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0)), \
                patch.object(contract, 'inputs', side_effect=lambda root, revision=None: 'theirs' if revision else 'ours'), \
                patch.object(play, 'download_run', side_effect=[rejected, self.build]) as download:
            self.assertEqual(play.fetch(), self.build)
        self.assertEqual([call.args[0]['id'] for call in download.call_args_list], [44, 43])
        # A build of the tree's own inputs that fails verification is corruption, never skipped.
        with patch.object(play, 'api', return_value={'workflow_runs': [newest, older]}), \
                patch.object(play, 'current_pr', return_value=pr), \
                patch.object(play, 'engine_changed', return_value=False), \
                patch.object(play.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0)), \
                patch.object(contract, 'inputs', return_value='ours'), \
                patch.object(play, 'download_run', side_effect=[rejected, self.build]) as download, \
                self.assertRaisesRegex(ValueError, 'bindings differ'):
            play.fetch()
        self.assertEqual(download.call_count, 1)
        self.assertIsNone(download.call_args.kwargs['producer'])

    def test_main_build_of_other_inputs_is_verified_against_the_commit_that_built_it(self):
        # The tree moved on (main merged an engine change since the build); the build's inputs
        # are those of its producer commit, which is what a non-engine PR imports.
        meta = json.loads((self.build / contract.MANIFEST).read_text())
        tree_inputs, tree_engine = contract.inputs, contract.engine_inputs

        def inputs(root=contract.ROOT, revision=None):
            return meta['inputs'] if revision else 'moved-on'

        def engine_inputs(root=contract.ROOT, revision=None):
            return meta['engine']['inputs'] if revision else 'moved-on'

        with patch.object(contract, 'inputs', side_effect=inputs), \
                patch.object(contract, 'engine_inputs', side_effect=engine_inputs):
            with self.assertRaisesRegex(ValueError, 'inputs differ'):
                contract.verify(self.build, self.root)
            self.assertEqual(contract.verify(self.build, self.root, producer=SHA)['build_revision'], SHA)
            # Without fetch-play's validation record, --trust-inputs is the usual check.
            with self.assertRaisesRegex(ValueError, 'inputs differ'):
                contract.verify(self.build, self.root, trust_inputs=True)
            destination = play.install(self.build, producer=SHA)
            self.assertEqual(contract.verify(destination, self.root, trust_inputs=True)['build_revision'], SHA)
            with self.assertRaises(ValueError):
                play.install(self.build)
        tampered = json.loads((destination / contract.PRODUCER_VALIDATION).read_text())
        for record in ({**tampered, 'tree_inputs': 'another-tree'}, {**tampered, 'producer': 'b' * 40}, {}):
            (destination / contract.PRODUCER_VALIDATION).write_text(json.dumps(record))
            with patch.object(contract, 'inputs', side_effect=inputs), \
                    patch.object(contract, 'engine_inputs', side_effect=engine_inputs), \
                    self.assertRaisesRegex(ValueError, 'inputs differ'):
                contract.verify(destination, self.root, trust_inputs=True)
        # The bindings are still the tree's, whatever built the engine.
        self.write(contract.BINDINGS + '/Example.ts', 'different protocol')
        with patch.object(contract, 'inputs', side_effect=inputs), \
                patch.object(contract, 'engine_inputs', side_effect=engine_inputs), \
                self.assertRaisesRegex(ValueError, 'bindings differ'):
            contract.verify(destination, self.root, producer=SHA)

    def test_trust_inputs_passes_a_build_of_the_trees_own_inputs_without_a_record(self):
        # Every PR image build passes --trust-inputs; an engine-changing PR's exact-match import has no record.
        self.assertEqual(contract.verify(self.build, self.root, trust_inputs=True)['build_revision'], SHA)
        destination = play.install(self.build)
        self.assertFalse((destination / contract.PRODUCER_VALIDATION).exists())
        self.assertEqual(contract.verify(destination, self.root, trust_inputs=True)['build_revision'], SHA)
        self.write('apps/bevy-explorer/build-web.py', 'changed')
        with self.assertRaisesRegex(ValueError, 'inputs differ'):
            contract.verify(destination, self.root, trust_inputs=True)

    def test_pr_without_engine_changes_downloads_and_installs_the_main_build(self):
        archive = self.root / 'web.zip'
        with zipfile.ZipFile(archive, 'w') as zipped:
            for path in self.build.rglob('*'):
                if path.is_file():
                    zipped.write(path, path.relative_to(self.build))
        payload = archive.read_bytes()
        meta = json.loads((self.build / contract.MANIFEST).read_text())
        record = {'id': 1, 'name': f'bevy-explorer-web-{SHA}', 'expired': False,
                  'size_in_bytes': len(payload), 'digest': 'sha256:' + hashlib.sha256(payload).hexdigest()}
        passed = {'name': play.PRODUCER, 'status': 'completed', 'conclusion': 'success'}
        pr = {'number': 7, 'head': 'b' * 40, 'base': 'd' * 40, 'merge': 'c' * 40}

        def command(args, **kwargs):
            if args[0] == 'gh':
                kwargs['stdout'].write(payload)
            return subprocess.CompletedProcess(args, 0)

        def inputs(root=contract.ROOT, revision=None):
            return meta['inputs'] if revision == SHA else 'moved-on'

        def engine_inputs(root=contract.ROOT, revision=None):
            return meta['engine']['inputs'] if revision == SHA else 'moved-on'

        with patch.object(play, 'api', side_effect=[{'workflow_runs': [run()]}, {'total_count': 1, 'jobs': [passed]},
                                                    {'total_count': 1, 'artifacts': [record]}]) as api, \
                patch.object(play, 'current_pr', return_value=pr), \
                patch.object(play, 'engine_changed', return_value=False), \
                patch.object(play.subprocess, 'run', side_effect=command), \
                patch.object(contract, 'inputs', side_effect=inputs), \
                patch.object(contract, 'engine_inputs', side_effect=engine_inputs):
            destination = play.fetch()
        self.assertEqual(api.call_count, 3)
        self.assertEqual(json.loads((destination / contract.MANIFEST).read_text())['build_revision'], SHA)

    def test_a_finished_run_of_this_commit_that_cannot_supply_it_is_named_for_a_rerun(self):
        pr = {'number': 7, 'head': SHA, 'base': 'b' * 40, 'merge': 'c' * 40}
        own = run(id=51, event='pull_request', head_branch='feature', pull_requests=[{'number': 7}], conclusion='failure')
        other = run(id=50, head_sha='d' * 40, conclusion='cancelled')
        with patch.object(play, 'api', return_value={'workflow_runs': [own, other]}), \
                patch.object(play, 'current_pr', return_value=pr), \
                patch.object(play, 'engine_changed', return_value=True), \
                patch.object(contract, 'inputs', return_value='ours'), \
                patch.object(play, 'download_run') as download, \
                self.assertRaisesRegex(RuntimeError, r'did not succeed: 51 \(failure\)\. Rerun it, then this job') as failure:
            play.fetch()
        download.assert_not_called()
        self.assertNotIn('50', str(failure.exception))
        # A main push whose own run was replaced by a newer push names that run.
        superseded = run(id=52, conclusion='cancelled')
        with patch.object(play, 'api', return_value={'workflow_runs': [superseded, other]}), \
                patch.object(play, 'current_pr', return_value=None), \
                patch.object(contract, 'inputs', return_value='ours'), \
                patch.dict(play.os.environ, {'GITHUB_SHA': SHA}), \
                self.assertRaisesRegex(RuntimeError, r'did not succeed: 52 \(cancelled\)') as failure:
            play.fetch()
        self.assertNotIn('50', str(failure.exception))

    def test_only_the_newest_run_of_this_commit_decides_the_rerun_hint(self):
        pr = {'number': 7, 'head': SHA, 'base': 'b' * 40, 'merge': 'c' * 40}

        def own(**changes):
            return run(event='pull_request', head_branch='feature', pull_requests=[{'number': 7}], **changes)

        older = own(id=51, conclusion='failure')
        cases = [
            # A newer run under way, as inside the Bevy CI run itself: nothing to rerun.
            ([own(id=53, status='in_progress', conclusion=None), older], None),
            # Two failures, after a close and reopen: the newest is the one to rerun.
            ([own(id=53, conclusion='cancelled'), older], r'did not succeed: 53 \(cancelled\)\. Rerun it'),
            # A success whose web artifact is gone: rerunning it is the fix.
            ([own(id=53), older], r'succeeded, but its web artifact is unavailable: 53\. Rerun it'),
        ]
        for runs, named in cases:
            with self.subTest(runs=[candidate['id'] for candidate in runs]), \
                    patch.object(play, 'api', return_value={'workflow_runs': runs}), \
                    patch.object(play, 'current_pr', return_value=pr), \
                    patch.object(play, 'engine_changed', return_value=True), \
                    patch.object(contract, 'inputs', return_value='ours'), \
                    patch.object(play, 'inputs_at', return_value='ours'), \
                    patch.object(play, 'download_run', return_value=None), \
                    self.assertRaises(RuntimeError) as failure:
                play.fetch()
            message = str(failure.exception)
            self.assertNotIn('51', message)
            if named is None:
                self.assertNotIn('Rerun it', message)
            else:
                self.assertRegex(message, named)

    def test_pr_without_engine_changes_still_requires_a_trusted_successful_run(self):
        pr = {'number': 7, 'head': SHA, 'base': 'b' * 40, 'merge': 'c' * 40}
        candidates = [run(head_repository={'full_name': 'other/project'}), run(head_branch='dev'),
                      run(conclusion='failure'), run(conclusion='cancelled'),
                      run(event='pull_request', head_branch='feature', pull_requests=[{'number': 8}])]
        with patch.object(play, 'api', return_value={'workflow_runs': candidates}), \
                patch.object(play, 'current_pr', return_value=pr), \
                patch.object(play, 'engine_changed', return_value=False), \
                patch.object(contract, 'inputs', return_value='ours'), \
                patch.object(play, 'download_run') as download, \
                self.assertRaisesRegex(RuntimeError, 'No successful matching'):
            play.fetch()
        download.assert_not_called()

    def test_pr_with_engine_changes_and_production_require_the_matching_build(self):
        pr = {'number': 7, 'head': SHA, 'base': 'b' * 40, 'merge': 'c' * 40}
        for production in (False, True):
            with self.subTest(production=production), \
                    patch.object(play, 'api', return_value={'workflow_runs': [run()]}), \
                    patch.object(play, 'current_pr', return_value=pr), \
                    patch.object(play, 'engine_changed', return_value=True) as changed, \
                    patch.object(play.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0)), \
                    patch.object(contract, 'inputs', side_effect=['ours', 'theirs']), \
                    patch.object(play, 'download_run') as download, \
                    self.assertRaisesRegex(RuntimeError, 'No successful matching'):
                play.fetch(production=production)
            download.assert_not_called()
            self.assertEqual(changed.call_count, 0 if production else 1)

    def test_skipped_failed_or_ambiguous_web_job_cannot_supply_artifact(self):
        good = {'name': play.PRODUCER, 'status': 'completed', 'conclusion': 'success'}
        uploaded = {'name': play.UPLOAD_STEP, 'status': 'completed', 'conclusion': 'success'}
        running = {**good, 'status': 'in_progress', 'conclusion': None}
        for jobs in ([], [good, good], [{**good, 'conclusion': 'skipped'}],
                     [{**good, 'conclusion': 'failure'}], [{**good, 'status': 'in_progress'}],
                     [{**running, 'steps': []}],
                     [{**running, 'steps': [{**uploaded, 'status': 'pending', 'conclusion': None}]}],
                     [{**running, 'steps': [{**uploaded, 'status': 'in_progress', 'conclusion': None}]}],
                     [{**running, 'steps': [{**uploaded, 'conclusion': 'failure'}]}],
                     [{**running, 'steps': [{**uploaded, 'conclusion': 'skipped'}]}],
                     [{**running, 'steps': [{**uploaded, 'conclusion': 'cancelled'}]}],
                     [{**running, 'steps': [uploaded, uploaded]}],
                     [{**running, 'status': 'queued', 'steps': [uploaded]}],
                     [{**good, 'conclusion': 'failure', 'steps': [uploaded]}],
                     [{**good, 'conclusion': 'cancelled', 'steps': [uploaded]}]):
            with patch.object(play, 'api', return_value={'total_count': len(jobs), 'jobs': jobs}) as api:
                self.assertIsNone(play.download_run(run()))
                self.assertEqual(api.call_count, 1)

    def test_running_web_job_supplies_its_artifact_once_the_upload_step_succeeded(self):
        uploaded = {'name': play.UPLOAD_STEP, 'status': 'completed', 'conclusion': 'success'}
        publishing = {'name': 'Publish', 'status': 'in_progress', 'conclusion': None}
        job = {'name': play.PRODUCER, 'status': 'in_progress', 'conclusion': None, 'steps': [uploaded, publishing]}
        record = {'id': 1, 'name': f'bevy-explorer-web-{SHA}', 'expired': False,
                  'size_in_bytes': 1, 'digest': 'sha256:' + '0' * 64}
        with patch.object(play, 'api', side_effect=[{'total_count': 1, 'jobs': [job]},
                                                    {'total_count': 1, 'artifacts': [record]}]) as api:
            self.assertEqual(play.producer_record(run(status='in_progress', conclusion=None)), record)
            self.assertEqual(api.call_count, 2)

    def test_running_web_job_waits_while_github_has_not_listed_the_digest(self):
        uploaded = {'name': play.UPLOAD_STEP, 'status': 'completed', 'conclusion': 'success'}
        job = {'name': play.PRODUCER, 'status': 'in_progress', 'conclusion': None, 'steps': [uploaded]}
        record = {'id': 1, 'name': f'bevy-explorer-web-{SHA}', 'expired': False, 'size_in_bytes': 1}
        for digest in ({}, {'digest': None}, {'digest': ''}):
            responses = [{'total_count': 1, 'jobs': [job]}, {'total_count': 1, 'artifacts': [{**record, **digest}]}]
            with patch.object(play, 'api', side_effect=responses):
                self.assertIsNone(play.producer_record(run(status='in_progress', conclusion=None)))
            with patch.object(play, 'api', side_effect=[{'total_count': 1, 'jobs': [{**job, 'status': 'completed',
                    'conclusion': 'success'}]}, responses[1]]), self.assertRaisesRegex(ValueError, 'verifiable artifact digest'):
                play.producer_record(run())

    def test_producer_job_uploads_the_web_app_after_building_and_testing_it(self):
        workflow = (Path(__file__).resolve().parents[2] / play.WORKFLOW).read_text()
        job = workflow[workflow.index('\n  build-deploy-web:\n') + 1:]
        job = re.split(r'\n  [a-z][\w-]*:\n', job)[0]
        self.assertIn(f'\n    name: {play.PRODUCER}\n', job)
        def step_end(start):
            following = job.find('      - name:', start + 1)
            return following if following != -1 else job.index('\n    defaults:\n', start)
        tested = job.index('      - name: Build and test the local web app\n')
        upload = job.index(f'      - name: {play.UPLOAD_STEP}\n')
        end = step_end(upload)
        self.assertLess(tested, upload)
        self.assertIn('          path: apps/bevy-explorer/dist/web/\n', job[upload:end])
        # Upload success must imply every earlier step passed.
        self.assertNotIn('continue-on-error', job[:end])
        for step in (tested, upload):
            self.assertIsNone(re.search(r'\n {8}if:', job[step:step_end(step)]))

    def test_github_digest_and_producer_sha_are_verified_before_install(self):
        archive = self.root / 'good.zip'
        with zipfile.ZipFile(archive, 'w') as zipped:
            for path in self.build.rglob('*'):
                if path.is_file():
                    zipped.write(path, path.relative_to(self.build))
        payload = archive.read_bytes()
        jobs = {'total_count': 1, 'jobs': [{'name': play.PRODUCER, 'status': 'completed', 'conclusion': 'success'}]}
        record = {'id': 1, 'name': f'bevy-explorer-web-{SHA}', 'expired': False,
                  'size_in_bytes': len(payload), 'digest': 'sha256:' + hashlib.sha256(payload).hexdigest()}
        def download(*args, **kwargs):
            kwargs['stdout'].write(payload)
        with patch.object(play, 'api', side_effect=[jobs, {'total_count': 1, 'artifacts': [record]}]), \
                patch.object(play.subprocess, 'run', side_effect=download):
            self.assertTrue(play.download_run(run(), production=True).is_dir())
        with patch.object(play, 'api', side_effect=[jobs, {'total_count': 1, 'artifacts': [{**record, 'digest': 'sha256:' + '0' * 64}]}]), \
                patch.object(play.subprocess, 'run', side_effect=download), self.assertRaisesRegex(ValueError, 'digest mismatch'):
            play.download_run(run(), production=True)
        meta = json.loads((self.build / contract.MANIFEST).read_text())
        meta['build_revision'] = 'b' * 40
        (self.build / contract.MANIFEST).write_text(json.dumps(meta))
        with zipfile.ZipFile(archive, 'w') as zipped:
            for path in self.build.rglob('*'):
                if path.is_file():
                    zipped.write(path, path.relative_to(self.build))
        payload = archive.read_bytes()
        record['digest'] = 'sha256:' + hashlib.sha256(payload).hexdigest()
        with patch.object(play, 'api', side_effect=[jobs, {'total_count': 1, 'artifacts': [record]}]), \
                patch.object(play.subprocess, 'run', side_effect=download), self.assertRaisesRegex(ValueError, 'producer revision'):
            play.download_run(run(), production=True)

    def test_expired_artifact_is_not_downloaded(self):
        jobs = {'total_count': 1, 'jobs': [{'name': play.PRODUCER, 'status': 'completed', 'conclusion': 'success'}]}
        artifacts = {'total_count': 1, 'artifacts': [{'name': f'bevy-explorer-web-{SHA}', 'expired': True}]}
        with patch.object(play, 'api', side_effect=[jobs, artifacts]), \
                patch.object(play.subprocess, 'run') as download:
            self.assertIsNone(play.download_run(run(), production=True))
        download.assert_not_called()

    def production_receipt(self):
        archive = self.root / 'prepared.zip'
        with zipfile.ZipFile(archive, 'w') as zipped:
            for path in self.build.rglob('*'):
                if path.is_file():
                    zipped.write(path, path.relative_to(self.build))
        payload = archive.read_bytes()
        record = {'id': 1, 'name': f'bevy-explorer-web-{SHA}', 'expired': False,
                  'size_in_bytes': len(payload), 'digest': 'sha256:' + hashlib.sha256(payload).hexdigest()}
        jobs = {'total_count': 1, 'jobs': [{'name': play.PRODUCER, 'status': 'completed', 'conclusion': 'success'}]}
        receipt = self.root / 'images/play-producer.json'
        def command(args, **kwargs):
            if args[0] == 'gh':
                kwargs['stdout'].write(payload)
            return subprocess.CompletedProcess(args, 0)
        active = run(status='in_progress', conclusion=None)
        with patch.object(play, 'api', side_effect=[{'workflow_runs': [active]}, jobs,
                {'total_count': 1, 'artifacts': [record]}]), \
                patch.object(play.subprocess, 'run', side_effect=command), \
                patch.object(contract, 'inputs', return_value=contract.inputs(self.root)):
            self.assertEqual(play.main(['--prepare-production', str(receipt)]), 0)
        return receipt, jobs, record

    def test_preparation_overlaps_native_tests_but_publication_waits_for_exact_run(self):
        receipt, jobs, record = self.production_receipt()
        self.assertEqual(json.loads(receipt.read_text())['artifact_id'], record['id'])
        with patch.object(play, 'api', return_value=run(status='in_progress', conclusion=None)), \
                self.assertRaisesRegex(RuntimeError, 'still pending'):
            play.verify_production(receipt)
        with patch.object(play, 'api', side_effect=[run(status='in_progress', conclusion=None), run(), jobs,
                {'total_count': 1, 'artifacts': [record]}]), patch.object(play.time, 'sleep') as sleep:
            play.verify_production(receipt, wait=60)
        sleep.assert_called_once()

    def test_prepared_images_cannot_publish_after_failure_cancel_rerun_or_identity_change(self):
        receipt, _, _ = self.production_receipt()
        for candidate in (run(conclusion='failure'), run(conclusion='cancelled'), run(run_attempt=2),
                          run(head_sha='b' * 40), run(head_branch='feature'), run(id=43),
                          run(head_repository={'full_name': 'other/project'})):
            with self.subTest(candidate=candidate), patch.object(play, 'api', return_value=candidate), \
                    self.assertRaises(ValueError):
                play.verify_production(receipt)

    def test_recovered_github_read_still_rejects_a_failed_production_run(self):
        receipt, _, _ = self.production_receipt()
        failure = subprocess.CalledProcessError(1, ['gh', 'api'], stderr='gh: error (HTTP 502)')
        with patch.object(play.subprocess, 'check_output',
                          side_effect=[failure, json.dumps(run(conclusion='failure'))]) as get, \
                patch.object(play.time, 'monotonic', return_value=0), \
                patch.object(play.time, 'sleep') as sleep, \
                self.assertRaisesRegex(ValueError, 'Complete Bevy CI did not pass'):
            play.verify_production(receipt, wait=30)
        self.assertEqual(get.call_count, 2)
        self.assertEqual(get.call_args.kwargs['timeout'], 30)
        sleep.assert_called_once_with(2)

    def test_prepared_images_cannot_publish_with_replaced_expired_or_missing_artifacts(self):
        receipt, jobs, record = self.production_receipt()
        for records in ([], [{**record, 'expired': True}], [{**record, 'id': 2}],
                        [{**record, 'digest': 'sha256:' + '0' * 64}], [record, record]):
            with self.subTest(records=records), patch.object(play, 'api', side_effect=[run(), jobs,
                    {'total_count': len(records), 'artifacts': records}]), self.assertRaises(ValueError):
                play.verify_production(receipt)

    def test_changed_inputs_or_malformed_receipt_fail_before_remote_lookup(self):
        receipt, _, _ = self.production_receipt()
        proof = json.loads(receipt.read_text())
        for change in ({'version': 2}, {'inputs': 'changed'}, {'run_id': '../another'}, {'run_attempt': 0},
                       {'artifact_id': True}, {'digest': 'unverified'}, {'revision': 'wrong'}):
            receipt.write_text(json.dumps({**proof, **change}))
            with self.subTest(change=change), patch.object(play, 'api') as api, self.assertRaises(ValueError):
                play.verify_production(receipt)
            api.assert_not_called()
        receipt.write_text(json.dumps(proof))
        self.write(contract.SOURCE + 'src/lib.rs', 'changed source')
        with patch.object(play, 'api') as api, self.assertRaises(ValueError):
            play.verify_production(receipt)
        api.assert_not_called()

    def test_production_preparation_rejects_untrusted_and_failed_runs(self):
        for candidate in (run(conclusion='failure'), run(event='pull_request', head_branch='feature'),
                          run(head_repository={'full_name': 'other/project'})):
            with patch.object(play, 'api') as api:
                self.assertIsNone(play.download_run(candidate, production=True, receipt=self.root / 'receipt.json'))
            api.assert_not_called()

    def test_failed_preparation_clears_old_receipt_and_cannot_fall_back_to_local_cache(self):
        receipt, _, _ = self.production_receipt()
        with patch.object(play, 'fetch', side_effect=RuntimeError('no producer')), self.assertRaises(RuntimeError):
            play.main(['--prepare-production', str(receipt)])
        self.assertFalse(receipt.exists())

    def test_development_imports_verified_web_while_native_jobs_are_running(self):
        archive = self.root / 'web.zip'
        with zipfile.ZipFile(archive, 'w') as zipped:
            for path in self.build.rglob('*'):
                if path.is_file():
                    zipped.write(path, path.relative_to(self.build))
        payload = archive.read_bytes()
        record = {'id': 1, 'name': f'bevy-explorer-web-{SHA}', 'expired': False,
                  'size_in_bytes': len(payload), 'digest': 'sha256:' + hashlib.sha256(payload).hexdigest()}
        expected = contract.inputs(self.root)
        slow = {'name': 'Test Suite (windows-2025)', 'status': 'in_progress', 'conclusion': None}
        passed = {'name': play.PRODUCER, 'status': 'completed', 'conclusion': 'success'}
        publishing = {**passed, 'status': 'in_progress', 'conclusion': None,
                      'steps': [{'name': play.UPLOAD_STEP, 'status': 'completed', 'conclusion': 'success'}]}
        active = run(status='in_progress', conclusion=None)
        pr = {'number': 7, 'head': 'b' * 40, 'merge': SHA}

        def command(args, **kwargs):
            if args[0] == 'gh':
                kwargs['stdout'].write(payload)
            else:
                self.assertEqual(args[:2], ['git', 'cat-file'])
            return subprocess.CompletedProcess(args, 0)

        for identity in (None, pr):
            candidate = active if identity is None else run(
                status='in_progress', conclusion=None, event='pull_request', head_branch='feature',
                head_sha=pr['head'], pull_requests=[{'number': pr['number']}])
            for web_pending, producer in ((False, passed), (True, passed), (False, publishing)):
                with self.subTest(pr=identity is not None, web_pending=web_pending, producer=producer['status']):
                    responses = []
                    if web_pending:
                        responses += [{'workflow_runs': [candidate]}, {'total_count': 2, 'jobs': [slow,
                                      {**passed, 'status': 'in_progress', 'conclusion': None}]}]
                    responses += [{'workflow_runs': [candidate]}, {'total_count': 2, 'jobs': [slow, producer]},
                                  {'total_count': 1, 'artifacts': [record]}]
                    with patch.object(play, 'api', side_effect=responses) as api, \
                            patch.object(play, 'current_pr', return_value=identity), \
                            patch.object(play.subprocess, 'run', side_effect=command), \
                            patch.object(contract, 'inputs', return_value=expected), \
                            patch.object(play.time, 'sleep') as sleep:
                        destination = play.fetch(wait=60)
                    self.assertEqual(sleep.call_count, int(web_pending))
                    self.assertEqual(api.call_count, 5 if web_pending else 3)
                    self.assertEqual((destination / 'engine/pkg/webgpu_build_bg.wasm').read_bytes(),
                                     (self.build / 'engine/pkg/webgpu_build_bg.wasm').read_bytes())
                    self.assertEqual(contract.verify(destination, self.root)['build_revision'], pr['merge'])

    def test_wait_ends_when_the_producer_of_a_running_workflow_ends_without_an_upload(self):
        # A consumer job inside the Bevy run keeps that run under way; the wait must not last to the deadline.
        expected = contract.inputs(self.root)
        # A skipped producer is no failure to rerun: the run found the engine inputs unchanged.
        named = {'failure': r'did not upload a web build: 42 \(failure\)\. Rerun it, then this job',
                 'cancelled': r'did not upload a web build: 42 \(cancelled\)\. Rerun it, then this job',
                 'skipped': r'skipped the web build: 42\. Refresh main with:'}
        for conclusion in ('failure', 'cancelled', 'skipped'):
            with self.subTest(conclusion=conclusion):
                ended = {'name': play.PRODUCER, 'status': 'completed', 'conclusion': conclusion}
                responses = [{'workflow_runs': [run(status='in_progress', conclusion=None)]},
                             {'total_count': 1, 'jobs': [ended]}]
                with patch.object(play, 'api', side_effect=responses) as api, \
                        patch.object(play, 'current_pr', return_value=None), \
                        patch.dict(play.os.environ, {'GITHUB_SHA': SHA}), \
                        patch.object(play.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0)), \
                        patch.object(contract, 'inputs', return_value=expected), \
                        patch.object(play.time, 'sleep') as sleep, \
                        self.assertRaisesRegex(RuntimeError, named[conclusion]) as failure:
                    play.fetch(wait=60)
                sleep.assert_not_called()
                self.assertEqual(api.call_count, 2)
                self.assertEqual('Rerun it' in str(failure.exception), conclusion != 'skipped')
        # A second run of the same commit that has no producer job yet keeps the wait open,
        # and the older run is not read before the first poll.
        class Waited(Exception):
            pass
        ended = {'name': play.PRODUCER, 'status': 'completed', 'conclusion': 'failure'}
        responses = [{'workflow_runs': [run(id=43, status='queued', conclusion=None),
                                        run(status='in_progress', conclusion=None)]},
                     {'total_count': 0, 'jobs': []}]
        with patch.object(play, 'api', side_effect=responses) as api, \
                patch.object(play, 'current_pr', return_value=None), \
                patch.dict(play.os.environ, {'GITHUB_SHA': SHA}), \
                patch.object(play.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0)), \
                patch.object(contract, 'inputs', return_value=expected), \
                patch.object(play.time, 'sleep', side_effect=Waited), \
                self.assertRaises(Waited):
            play.fetch(wait=60)
        self.assertEqual(api.call_count, 2)
        # The same job in a completed run keeps the old answer: no record, no exception.
        with patch.object(play, 'api', return_value={'total_count': 1, 'jobs': [
                {'name': play.PRODUCER, 'status': 'completed', 'conclusion': 'skipped'}]}):
            self.assertIsNone(play.producer_record(run()))

    def test_development_rejects_untrusted_and_already_failed_runs_before_job_lookup(self):
        pr = {'number': 7, 'head': SHA, 'merge': 'b' * 40}
        candidates = [run(status='in_progress', conclusion=None, head_repository={'full_name': 'other/project'}),
                      run(status='in_progress', conclusion=None, event='pull_request',
                          head_branch='feature', pull_requests=[{'number': 8}]),
                      run(conclusion='failure'), run(conclusion='cancelled')]
        for candidate in candidates:
            with self.subTest(candidate=candidate), \
                    patch.object(play, 'api', return_value={'workflow_runs': [candidate]}) as api, \
                    patch.object(play, 'current_pr', return_value=pr), \
                    patch.object(play, 'download_run') as download, \
                    self.assertRaisesRegex(RuntimeError, 'No successful matching'):
                play.fetch(wait=60)
            self.assertEqual(api.call_count, 1)
            download.assert_not_called()

    def test_production_download_requires_the_complete_successful_main_run(self):
        for candidate in (run(status='in_progress', conclusion=None), run(conclusion='failure'),
                          run(event='pull_request', head_branch='feature', pull_requests=[{'number': 7}])):
            with self.subTest(candidate=candidate), patch.object(play, 'api') as api:
                self.assertIsNone(play.download_run(candidate, production=True))
            api.assert_not_called()

    def test_search_reaches_retained_main_build_beyond_a_thousand_newer_runs(self):
        # PR traffic and integrity-only main runs must not hide a retained producer.
        pages = [{'workflow_runs': [run(id=page * 100 + n, head_branch='dev') for n in range(100)]}
                 for page in range(10)]
        pages.append({'workflow_runs': [run(id=1001), run(id=1002)]})
        with patch.object(play, 'api', side_effect=pages) as api, \
                patch.object(play.subprocess, 'run'), \
                patch.object(contract, 'inputs', return_value='match'), \
                patch.object(play, 'download_run', side_effect=[None, self.build]) as download:
            self.assertEqual(play.fetch(production=True), self.build)
        self.assertEqual(api.call_count, 11)
        self.assertEqual(api.call_args.args[0],
                         'actions/workflows/bevy-explorer-ci.yml/runs?per_page=100&page=11')
        self.assertEqual([call.args[0]['id'] for call in download.call_args_list], [1001, 1002])

    def test_failed_and_mismatched_runs_do_not_supply_a_build(self):
        candidates = [run(conclusion='failure'), run(id=43, head_sha='b' * 40)]
        with patch.object(play, 'api', return_value={'workflow_runs': candidates}), \
                patch.object(play.subprocess, 'run'), patch.object(play, 'download_run') as download, \
                patch.object(contract, 'inputs', side_effect=['match', 'different']), \
                self.assertRaisesRegex(RuntimeError, 'No successful matching'):
            play.fetch(production=True)
        download.assert_not_called()

    def test_wait_scans_history_once_and_refreshes_old_pending_run_by_id(self):
        pages = [[run(id=100 + page * 100 + n, head_branch='dev') for n in range(100)]
                 for page in range(10)]
        pages.append([run(status='in_progress', conclusion=None)])
        refreshes = iter([run(status='in_progress', conclusion=None),
                          run(status='in_progress', conclusion=None), run()])
        clock = [0]

        def response(path, deadline):
            self.assertEqual(deadline, 180)
            if path == 'actions/runs/42':
                return next(refreshes)
            return {'workflow_runs': pages[int(path.rsplit('=', 1)[1]) - 1]}

        with patch.object(play, 'api', side_effect=response) as api, \
                patch.object(play.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0)) as git, \
                patch.object(contract, 'inputs', return_value='match') as inputs, \
                patch.object(play, 'download_run', return_value=self.build) as download, \
                patch.object(play.time, 'monotonic', side_effect=lambda: clock[0]), \
                patch.object(play.time, 'sleep', side_effect=lambda delay: clock.__setitem__(0, clock[0] + delay)) as sleep:
            self.assertEqual(play.fetch(wait=180, production=True), self.build)
        requests = [call.args[0] for call in api.call_args_list]
        self.assertEqual(len(requests), 17)  # 11 initial pages + 3 * (recent page + pending ID).
        for page in range(2, 12):
            self.assertEqual(requests.count(
                f'actions/workflows/bevy-explorer-ci.yml/runs?per_page=100&page={page}'), 1)
        self.assertEqual(requests.count('actions/runs/42'), 3)
        self.assertEqual(inputs.call_count, 2)  # Current input key and one producer revision.
        git.assert_called_once()
        download.assert_called_once_with(run(), None, True, None, 180, producer=None)
        self.assertEqual([call.args[0] for call in sleep.call_args_list], [2, 4, 8])
        self.assertEqual(clock[0], 14)

    def history(self, first, older):
        # Page one: `first`, then 99 runs of other branches. Page two: `older`.
        return [[first] + [run(id=900 + n, head_branch='dev') for n in range(99)], older]

    def test_wait_polls_a_matching_run_under_way_before_it_reads_older_history(self):
        # The consumer of an engine change: its own run is the newest, and its build is not uploaded yet.
        under_way = run(id=60, status='in_progress', conclusion=None)
        pages = self.history(under_way, [run(id=41, head_sha='b' * 40), run(id=40, head_sha='c' * 40)])
        polls = iter([[under_way], [run(id=60)]])
        first = [True]

        def response(path, deadline):
            page = int(path.rsplit('=', 1)[1])
            if page == 1 and not first[0]:
                return {'workflow_runs': next(polls)}
            first[0] = False
            return {'workflow_runs': pages[page - 1]}

        with patch.object(play, 'api', side_effect=response) as api, \
                patch.object(play, 'inputs_at', return_value='match') as inputs_at, \
                patch.object(contract, 'inputs', return_value='match'), \
                patch.object(play, 'download_run', return_value=self.build) as download, \
                patch.object(play.time, 'sleep') as sleep:
            self.assertEqual(play.fetch(wait=600, production=True), self.build)
        self.assertEqual([call.args[0].rsplit('=', 1)[1] for call in api.call_args_list], ['1', '1', '1'])
        self.assertEqual([call.args[0] for call in inputs_at.call_args_list], [SHA])  # no older commit fetched
        self.assertEqual([call.args[0] for call in sleep.call_args_list], [2, 4])
        download.assert_called_once_with(run(id=60), None, True, None, ANY, producer=None)

    def test_history_goes_on_when_the_run_under_way_gives_no_build(self):
        under_way = run(id=60, status='in_progress', conclusion=None)
        older = run(id=41)
        pages = self.history(under_way, [older])
        requests = []

        def response(path, deadline):
            requests.append(path.rsplit('=', 1)[1])
            if requests == ['1', '1']:
                return {'workflow_runs': [run(id=60, conclusion='cancelled')]}
            return {'workflow_runs': pages[int(requests[-1]) - 1]}

        with patch.object(play, 'api', side_effect=response), \
                patch.object(play, 'inputs_at', return_value='match'), \
                patch.object(contract, 'inputs', return_value='match'), \
                patch.object(play, 'download_run', return_value=self.build) as download, \
                patch.object(play.time, 'sleep') as sleep:
            self.assertEqual(play.fetch(wait=600, production=True), self.build)
        self.assertEqual(requests, ['1', '1', '2'])
        sleep.assert_called_once_with(2)
        download.assert_called_once_with(older, None, True, None, ANY, producer=None)

    def test_an_import_without_a_wait_reads_older_history_past_a_run_under_way(self):
        older = run(id=41)
        pages = self.history(run(id=60, status='in_progress', conclusion=None), [older])
        with patch.object(play, 'api', side_effect=[{'workflow_runs': page} for page in pages]) as api, \
                patch.object(play, 'inputs_at', return_value='match'), \
                patch.object(contract, 'inputs', return_value='match'), \
                patch.object(play, 'download_run', return_value=self.build) as download, \
                patch.object(play.time, 'sleep') as sleep:
            self.assertEqual(play.fetch(production=True), self.build)
        self.assertEqual(api.call_count, 2)
        sleep.assert_not_called()
        download.assert_called_once_with(older, None, True, None, None, producer=None)

    def test_a_change_without_engine_inputs_takes_the_finished_main_build_and_does_not_wait(self):
        pr = {'number': 7, 'head': 'b' * 40, 'base': 'c' * 40, 'merge': 'd' * 40}
        finished = run(id=41, head_sha='e' * 40)
        with patch.object(play, 'api', return_value={'workflow_runs': [
                run(id=60, status='in_progress', conclusion=None), finished]}), \
                patch.object(play, 'current_pr', return_value=pr), \
                patch.object(play, 'engine_changed', return_value=False), \
                patch.object(play, 'inputs_at', return_value='main'), \
                patch.object(contract, 'inputs', return_value='ours'), \
                patch.object(play, 'download_run', side_effect=[None, self.build]) as download, \
                patch.object(play.time, 'sleep') as sleep:
            self.assertEqual(play.fetch(wait=600), self.build)
        sleep.assert_not_called()
        self.assertEqual([call.args[0]['id'] for call in download.call_args_list], [60, 41])

    def test_a_wait_that_ends_inside_a_read_names_the_run_under_way(self):
        responses = [{'workflow_runs': [run(status='in_progress', conclusion=None)]},
                     TimeoutError('GitHub read exceeded the configured wait deadline')]
        with patch.object(play, 'api', side_effect=responses), \
                patch.object(play, 'current_pr', return_value=None), \
                patch.dict(play.os.environ, {'GITHUB_SHA': SHA}), \
                patch.object(play, 'inputs_at', return_value='match'), \
                patch.object(contract, 'inputs', return_value='match'), \
                patch.object(play, 'download_run', return_value=None), \
                patch.object(play.time, 'sleep'), \
                self.assertRaisesRegex(RuntimeError, r'Bevy CI run 42 has not uploaded its web build after the '
                                       r'60 s wait\. Rerun this job when it has'):
            play.fetch(wait=60)

    def test_pending_run_on_recent_page_is_not_fetched_twice(self):
        with patch.object(play, 'api', side_effect=[
                {'workflow_runs': [run(status='queued', conclusion=None)]},
                {'workflow_runs': [run()]}]) as api, \
                patch.object(play.subprocess, 'run'), \
                patch.object(contract, 'inputs', return_value='match'), \
                patch.object(play, 'download_run', return_value=self.build), \
                patch.object(play.time, 'sleep'):
            self.assertEqual(play.fetch(wait=60, production=True), self.build)
        self.assertEqual([call.args[0] for call in api.call_args_list], [
            'actions/workflows/bevy-explorer-ci.yml/runs?per_page=100&page=1'] * 2)

    def test_wait_discovers_a_new_successful_producer_after_cancellation(self):
        foreign = run(id=43, head_repository={'full_name': 'other/project'})
        replacement = run(id=44)
        with patch.object(play, 'api', side_effect=[
                {'workflow_runs': [run(status='in_progress', conclusion=None)]},
                {'workflow_runs': [foreign, replacement, run(conclusion='cancelled')]}]), \
                patch.object(play.subprocess, 'run'), \
                patch.object(contract, 'inputs', return_value='match'), \
                patch.object(play, 'download_run', return_value=self.build) as download, \
                patch.object(play.time, 'monotonic', return_value=0), \
                patch.object(play.time, 'sleep'):
            self.assertEqual(play.fetch(wait=60, production=True), self.build)
        download.assert_called_once_with(replacement, None, True, None, 60, producer=None)

    def test_wait_reconsiders_a_new_attempt_but_deduplicates_the_completed_attempt(self):
        # A failure or a successful run with no web artifact may be rerun under
        # the same ID while another matching run keeps this importer waiting.
        for conclusion in ('failure', 'success'):
            with self.subTest(previous_conclusion=conclusion):
                previous = run(conclusion=conclusion)
                pending = run(id=43, status='in_progress', conclusion=None)
                retry = run(run_attempt=2)
                clock = [0]
                with patch.object(play, 'api', side_effect=[
                        {'workflow_runs': [previous, pending]},
                        {'workflow_runs': [previous, pending]},
                        {'workflow_runs': [run(run_attempt=2, status='queued', conclusion=None),
                                           run(id=43, conclusion='cancelled')]},
                        {'workflow_runs': [retry]}]) as api, \
                        patch.object(play.subprocess, 'run', return_value=subprocess.CompletedProcess([], 0)), \
                        patch.object(contract, 'inputs', return_value='match') as inputs, \
                        patch.object(play, 'download_run',
                                     side_effect=lambda r, *_, **__: self.build if r['run_attempt'] == 2 else None) as download, \
                        patch.object(play.time, 'monotonic', side_effect=lambda: clock[0]), \
                        patch.object(play.time, 'sleep', side_effect=lambda delay: clock.__setitem__(0, clock[0] + delay)) as sleep:
                    self.assertEqual(play.fetch(wait=180, production=True), self.build)
                attempts = [call.args[0]['run_attempt'] for call in download.call_args_list]
                self.assertEqual(attempts, [1, 2] if conclusion == 'success' else [2])
                download.assert_called_with(retry, None, True, None, 180, producer=None)
                self.assertEqual(api.call_count, 4)
                self.assertEqual(inputs.call_count, 2)
                self.assertEqual([call.args[0] for call in sleep.call_args_list], [2, 4, 8])
                self.assertEqual(clock[0], 14)

    def test_a_new_attempt_still_requires_a_trusted_run(self):
        with patch.object(play, 'api', side_effect=[
                {'workflow_runs': [run(conclusion='failure'), run(id=43, status='queued', conclusion=None)]},
                {'workflow_runs': [run(run_attempt=2, head_repository={'full_name': 'other/project'}),
                                   run(id=43, conclusion='cancelled')]}]), \
                patch.object(play.subprocess, 'run'), \
                patch.object(contract, 'inputs', return_value='match'), \
                patch.object(play, 'download_run') as download, \
                patch.object(play.time, 'sleep'), \
                self.assertRaisesRegex(RuntimeError, 'No successful matching'):
            play.fetch(wait=60, production=True)
        download.assert_not_called()

    def test_pending_run_is_revalidated_after_refresh(self):
        # A changed response cannot bypass the trust checks via the pending set.
        with patch.object(play, 'api', side_effect=[
                {'workflow_runs': [run(status='in_progress', conclusion=None)]},
                {'workflow_runs': []},
                run(head_repository={'full_name': 'other/project'})]), \
                patch.object(play.subprocess, 'run'), \
                patch.object(contract, 'inputs', return_value='match'), \
                patch.object(play, 'download_run') as download, \
                patch.object(play.time, 'sleep'), \
                self.assertRaisesRegex(RuntimeError, 'No successful matching'):
            play.fetch(wait=60, production=True)
        download.assert_not_called()

    def test_wait_deadline_still_fails_without_a_completed_producer(self):
        clock = [0]
        with patch.object(play, 'api', return_value={
                'workflow_runs': [run(status='in_progress', conclusion=None)]}) as api, \
                patch.object(play.subprocess, 'run'), \
                patch.object(contract, 'inputs', return_value='match'), \
                patch.object(play, 'download_run') as download, \
                patch.object(play.time, 'monotonic', side_effect=lambda: clock[0]), \
                patch.object(play.time, 'sleep', side_effect=lambda delay: clock.__setitem__(0, clock[0] + delay)) as sleep, \
                self.assertRaisesRegex(RuntimeError, r'Bevy CI run 42 has not passed after the 60 s wait\. '
                                       'Rerun this job when it has') as failure:
            play.fetch(wait=60, production=True)
        self.assertNotIn('Refresh main with:', str(failure.exception))  # a second main run is no help
        self.assertEqual(api.call_count, 6)
        self.assertEqual([call.args[0] for call in sleep.call_args_list], [2, 4, 8, 16, 30])
        self.assertEqual(clock[0], 60)
        download.assert_not_called()


class HudSharedSourcesTests(unittest.TestCase):
    """The checked resolver graph supplies all consumers without a second import parser."""

    def test_checked_graph_contains_only_foundations_for_the_ui_consumer(self):
        root = Path(__file__).resolve().parents[2]
        manifest = contract.shared_inputs.read_manifest(root)
        paths = {row['path'] for row in manifest['files']}
        self.assertTrue(any(p.startswith('library/ui/foundations/') for p in paths))
        self.assertNotIn('library/ui/index.ts', paths)
        self.assertFalse(any(p.startswith('library/ui/components/') for p in paths))

    def test_native_and_nix_consume_the_checked_graph(self):
        root = Path(__file__).resolve().parents[2]
        self.assertIn('hud_shared_inputs.py', (root / contract.SOURCE / 'justfile').read_text())
        nix = (root / 'infra/packages/play.nix').read_text()
        self.assertIn('shared-inputs.json', nix)
        self.assertIn('import ./hud-shared-inputs.nix', nix)
        validator = (root / 'infra/packages/hud-shared-inputs.nix').read_text()
        self.assertIn('hashFile "sha256"', validator)
        self.assertLess(nix.index('checked = assert import'), nix.index('sharedFiles ='))
        self.assertNotIn('/library/ui/foundations', nix)


if __name__ == '__main__':
    unittest.main()
