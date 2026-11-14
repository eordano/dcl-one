"""Engine selection uses committed inputs; publication requires explicit producers."""
import importlib.util
import fnmatch
import shlex
import re
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch
from hud_manifest_fixture import seal, SHARED

SPEC = importlib.util.spec_from_file_location('bevy_ci', Path(__file__).parents[1] / 'bevy-ci.py')
ci = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ci)


def source_run(**changes):
    return {'id': 42, 'status': 'completed', 'conclusion': 'success', 'head_branch': 'main',
            'event': 'push', 'head_repository': {'full_name': 'example/project'},
            'path': ci.WORKFLOW, 'head_sha': 'a' * 40, **changes}


def producers(outcome='success'):
    return [{'name': name, 'status': 'completed', 'conclusion': outcome} for name in ci.PRODUCERS.values()]


class WorkflowTest(unittest.TestCase):
    def test_nix_play_tooling_dependencies_are_watched_by_image_selection(self):
        contract = (ci.ROOT / 'infra/packages/play.nix').read_text()
        paths = set(re.findall(r'/tooling/[a-zA-Z0-9_-]+\.py', contract))
        self.assertTrue(paths)
        for path in paths:
            with self.subTest(path=path):
                self.assertTrue(any(fnmatch.fnmatchcase(path.lstrip('/'), pattern) for pattern in ci.IMAGES_INPUTS))

    def test_shared_reader_and_fixture_changes_schedule_both_workflow_events(self):
        workflow = (ci.ROOT / ci.WORKFLOW).read_text()
        for event, end in (('push', 'pull_request'), ('pull_request', 'workflow_dispatch')):
            block = workflow.split('  ' + event + ':', 1)[1].split('  ' + end + ':', 1)[0]
            patterns = [line.strip()[2:] for line in block.splitlines() if line.strip().startswith('- ')]
            for name in ('tooling/hud_shared_inputs.py', 'tooling/tests/test_hud_shared_inputs.py',
                         'tooling/tests/hud_manifest_fixture.py', 'tooling/tests/hud_manifest_invalid_cases.json', 'tooling/tests/check_hud_manifest_nix.py'):
                with self.subTest(event=event, path=name):
                    self.assertTrue(any(fnmatch.fnmatchcase(name, pattern) for pattern in patterns))
        self.assertIn("-p 'test_hud_shared_inputs.py' -v", workflow)

    def test_pure_nix_hud_corpus_runs_after_nix_install_and_before_heavy_builds(self):
        workflow = (ci.ROOT / '.github/workflows/apis-sites-images.yml').read_text()
        install = workflow.index('cachix/install-nix-action@')
        corpus = workflow.index('run: python3 tooling/tests/check_hud_manifest_nix.py')
        build = workflow.index('run: nix build')
        self.assertLess(install, corpus)
        self.assertLess(corpus, build)
        for name in ('tooling/tests/check_hud_manifest_nix.py', 'tooling/tests/hud_manifest_invalid_cases.json',
                     'tooling/tests/hud_manifest_fixture.py', 'infra/packages/hud-shared-inputs.nix',
                     'infra/packages/hud-shared-inputs-corpus.nix'):
            self.assertTrue(any(fnmatch.fnmatchcase(name, pattern) for pattern in ci.IMAGES_INPUTS), name)

    def test_generator_suite_is_a_ci_gate_after_build_and_before_artifact_upload(self):
        workflow = (ci.ROOT / ci.WORKFLOW).read_text()
        job = workflow.split('  build-deploy-web:', 1)[1].split('  headless-packages:', 1)[0]
        build = job.index('run: python3 ../../../apps/bevy-explorer/build-web.py')
        tests = job.index('run: npm run test:shared-inputs --prefix react-web')
        upload = job.index('- name: Upload the complete web app')
        self.assertLess(build, tests)
        self.assertLess(tests, upload)
        test_step = job[job.rfind('- name:', 0, tests):job.index('- name:', tests)]
        self.assertNotIn('continue-on-error', test_step)
        self.assertNotIn('if:', test_step)

    def test_main_runs_are_not_cancelled_by_the_next_push(self):
        workflow = (ci.ROOT / ci.WORKFLOW).read_text(encoding='utf-8')
        lines = workflow.split('\nconcurrency:\n', 1)[1].splitlines(True)
        # The workflow-level block only: it ends at the next top-level key.
        block = ''.join(lines[:next(i for i, line in enumerate(lines) if line[:1] not in (' ', '#'))])
        self.assertIn("  group: bevy-explorer-ci-${{ github.event.pull_request.number && format('pr-{0}', "
                      "github.event.pull_request.number) || github.ref }}\n", block)
        self.assertIn("  cancel-in-progress: ${{ github.event_name == 'pull_request' || "
                      "github.ref != 'refs/heads/main' }}\n", block)

    def test_a_pull_request_publishes_a_preview_only_with_the_label(self):
        workflow = (ci.ROOT / ci.WORKFLOW).read_text(encoding='utf-8')
        job = workflow.split('\n  build-deploy-web:\n', 1)[1].split('\n  headless-packages:\n', 1)[0]
        self.assertIn("      BEVY_WEB_PUBLISH: ${{ (github.event_name == 'push' && github.ref == 'refs/heads/main') || "
                      "(github.event_name == 'workflow_dispatch' && inputs.publish-tag != '') || "
                      "(github.event_name == 'pull_request' && "
                      "github.event.pull_request.head.repo.full_name == github.repository && "
                      "github.head_ref != 'main' && github.head_ref != 'master' && "
                      "contains(github.event.pull_request.labels.*.name, 'ci:preview')) }}\n", job)
        self.assertIn('      publish: ${{ env.BEVY_WEB_PUBLISH }}\n', job)
        # The package upload and the publish job read only that answer.
        self.assertIn("      - name: Upload the stamped package for publication\n"
                      "        if: env.BEVY_WEB_PUBLISH == 'true'\n", job)
        self.assertIn("\n  publish-web:\n    if: needs.build-deploy-web.outputs.publish == 'true'\n", workflow)
        notice = job[job.index('      - name: Say how to get a preview\n'):
                     job.index('      - name: Upload the stamped package for publication\n')]
        self.assertIn("github.event.pull_request.head.repo.full_name == github.repository && "
                      "env.BEVY_WEB_PUBLISH != 'true'\n", notice)
        self.assertIn('Add the ci:preview label, then push or reopen', notice)
        # A closed pull request removes its tag with or without the label.
        cleanup = workflow.split('\n  preview-cleanup:\n', 1)[1].split('\n    name:', 1)[0]
        self.assertNotIn('labels', cleanup)

    def test_pr_web_builds_skip_wasm_opt_only_while_the_benchmark_is_off(self):
        workflow = (ci.ROOT / ci.WORKFLOW).read_text(encoding='utf-8')
        job = workflow.split('\n  build-deploy-web:\n', 1)[1].split('\n  headless-packages:\n', 1)[0]
        self.assertIn('          key=$(python3 tooling/play-artifact.py engine-cache-key)\n'
                      '          echo "key=$key" >> "$GITHUB_OUTPUT"\n', job)
        self.assertIn("      - name: Save tested compiled engine\n"
                      "        if: steps.engine-cache.outputs.cache-matched-key == ''\n", job)
        self.assertIn("      BEVY_WEB_OPT: ${{ github.event_name == 'pull_request' && "
                      "vars.BENCHMARK_ENABLED_RUNNERS == 'none' && 'unoptimized' || 'optimized' }}\n", job)
        self.assertIn('          key: bevy-engine-v1-${{ env.BEVY_WEB_OPT }}-${{ steps.engine-inputs.outputs.key }}\n'
                      '          restore-keys: bevy-engine-v1-optimized-${{ steps.engine-inputs.outputs.key }}\n',
                      job)
        self.assertIn("verify-engine .cache/bevy-engine ${{ env.BEVY_WEB_OPT == 'optimized' && '--production' || '' }}\n",
                      job)
        self.assertIn("build-web.py --engine-cache ../../../.cache/bevy-engine "
                      "${{ env.BEVY_WEB_OPT == 'unoptimized' && '--no-opt' || '' }} --shared-build\n", job)
        self.assertIn('          path: .cache/bevy-engine\n'
                      '          key: ${{ steps.engine-cache.outputs.cache-primary-key }}\n', job)

    def test_gateway_route_step_requires_the_ignored_fixture_to_pass(self):
        workflow = (ci.ROOT / ci.WORKFLOW).read_text(encoding='utf-8')
        _, _, after = workflow.partition('      - name: Test Explorer routes against the gateway\n')
        step = after.split('      - name:', 1)[0]
        self.assertIn("if: runner.os == 'Linux'", step)
        self.assertIn('set -o pipefail', step)
        self.assertIn('gateway::gateway_tests::gateway_routes_match_deployment -- --ignored --exact', step)
        self.assertIn('| tee "$RUNNER_TEMP/explorer-gateway-routes.log"', step)
        self.assertIn(
            "grep -Fx 'test gateway::gateway_tests::gateway_routes_match_deployment ... ok'"
            ' "$RUNNER_TEMP/explorer-gateway-routes.log"',
            step,
        )


class SelectionTest(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.root_patch = patch.object(ci, 'ROOT', self.root)
        self.root_patch.start()
        self.addCleanup(self.root_patch.stop)
        self.git('init', '-q')
        # A detached Git maintenance process can recreate .git entries while
        # TemporaryDirectory removes this fixture after the last commit.
        self.git('config', 'maintenance.auto', 'false')
        self.git('config', 'gc.auto', '0')
        self.write('rendering/bevy-explorer/source/src/lib.rs', 'fn main() {}\n')
        self.base = self.commit()

    def git(self, *args):
        return subprocess.check_output(['git', '-c', 'user.name=CI', '-c', 'user.email=ci@example.invalid',
                                        *args], cwd=self.root, text=True).strip()

    def write(self, name, value):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(value)

    def commit(self):
        seal(self.root)
        self.git('add', '.')
        self.git('commit', '-qm', 'fixture', '--allow-empty')
        return self.git('rev-parse', 'HEAD')

    def test_reader_changes_select_web_but_test_only_changes_do_not_build(self):
        for name in ('tooling/hud_shared_inputs.py', 'tooling/tests/test_hud_shared_inputs.py',
                     'tooling/tests/hud_manifest_fixture.py', 'tooling/tests/hud_manifest_invalid_cases.json', 'tooling/tests/check_hud_manifest_nix.py'):
            with self.subTest(path=name):
                before = self.git('rev-parse', 'HEAD')
                self.write(name, 'reader input')
                result = ci.select(before, self.commit())
                self.assertEqual(result['web'], name == 'tooling/hud_shared_inputs.py')
                self.assertFalse(any(result[lane] for lane in ('clippy', 'test', 'headless', 'native-matrix', 'scenes')))

    def test_shared_reader_only_change_selects_images_without_engine_lanes(self):
        self.write('tooling/hud_shared_inputs.py', 'changed image verification dependency')
        head = self.commit()
        self.assertTrue(ci.consumer_plan('pull_request', self.base, head, '')['images'])
        selected = ci.select(self.base, head)
        self.assertTrue(selected['web'])
        self.assertFalse(any(selected[lane] for lane in ('clippy', 'test', 'headless', 'native-matrix', 'scenes')))

    def test_source_and_integration_inputs_select_builds(self):
        for name in ('rendering/bevy-explorer/source/src/lib.rs',
                     'rendering/bevy-explorer/source/react-web/package-lock.json',
                     'apps/bevy-explorer/build-web.py', 'apps/bevy-explorer/Dockerfile.web',
                     'tooling/bevy-ci.py', 'tooling/ci_paths.py', ci.WORKFLOW,
                     '.github/workflows/bevy-explorer-images.yml'):
            with self.subTest(path=name):
                before = self.git('rev-parse', 'HEAD')
                self.write(name, 'changed')
                result = ci.select(before, self.commit())
                self.assertTrue(any(result[lane] for lane in ci.LANES))

    def test_dependency_toolchain_and_workflow_changes_select_native_matrix(self):
        for name in ('rendering/bevy-explorer/source/Cargo.lock',
                     'rendering/bevy-explorer/source/Cargo.toml',
                     'rendering/bevy-explorer/source/.cargo/config.toml',
                     'rendering/bevy-explorer/source/rust-toolchain.toml',
                     '.cargo/config.toml', 'rust-toolchain.toml', ci.WORKFLOW,
                     'library/vendor/rusty_enet/src/packet.rs',
                     'library/vendor/rusty_enet/checksums.sha256',
                     'tooling/dev-regressions.py', 'tooling/dev-regressions.json'):
            with self.subTest(path=name):
                before = self.git('rev-parse', 'HEAD')
                self.write(name, 'changed')
                self.assertTrue(ci.select(before, self.commit())['native-matrix'])

    def test_platform_inputs_under_source_select_native_matrix(self):
        for name in ('build.rs', 'crates/assets/build.rs', 'crates/dcl_component/build_quant.rs',
                     'local-harness/benchmark/src/main.rs', 'local-harness/web-driver/driver.mjs',
                     'crates/platform/src/native/deeplink/windows.rs', 'crates/platform/src/native/deeplink/macos.rs',
                     'deploy/macos/Info.plist'):
            with self.subTest(path=name):
                before = self.git('rev-parse', 'HEAD')
                self.write('rendering/bevy-explorer/source/' + name, 'changed')
                self.assertTrue(ci.select(before, self.commit())['native-matrix'])

    def test_platform_neutral_source_and_protos_skip_only_the_native_matrix(self):
        for name in ('rendering/bevy-explorer/source/src/lib.rs', 'rendering/bevy-explorer/source/assets/image.png',
                     'rendering/bevy-explorer/source/crates/example/Cargo.toml',
                     'library/protocol/proto/decentraland/common/vectors.proto'):
            with self.subTest(path=name):
                before = self.git('rev-parse', 'HEAD')
                self.write(name, 'changed')
                selected = ci.select(before, self.commit())
                self.assertEqual({lane for lane in (*ci.LANES, 'native-matrix', 'scenes')
                                  if selected[lane]}, {*ci.LANES, 'scenes'})

    def test_cargo_lock_selects_native_matrix_only_when_the_locked_packages_change(self):
        def lock(*packages, dependencies=()):
            entries = ''.join(f'[[package]]\nname = "{name}"\nversion = "{version}"\n' for name, version in packages)
            return (f'version = 4\n\n{entries}[[package]]\nname = "dcl"\nversion = "0.1.0"\n'
                    f'dependencies = {json.dumps(list(dependencies))}\n')
        name = 'rendering/bevy-explorer/source/Cargo.lock'
        self.write(name, lock(('tokio', '1.0.0')))
        before = self.commit()
        cases = {'a new edge between locked packages': (lock(('tokio', '1.0.0'), dependencies=['tokio']), False),
                 'a new package': (lock(('tokio', '1.0.0'), ('url', '2.5.0')), True),
                 'a version bump': (lock(('tokio', '1.1.0')), True),
                 'an unreadable lockfile': ('not toml [', True)}
        for case, (content, native) in cases.items():
            with self.subTest(case=case):
                self.write(name, content)
                selected = ci.select(before, self.commit())
                self.assertEqual(selected['native-matrix'], native)
                self.assertTrue(all(selected[lane] for lane in (*ci.LANES, 'scenes')))

    def test_main_push_keeps_native_matrix_for_platform_neutral_source(self):
        self.write('rendering/bevy-explorer/source/src/lib.rs', 'native change')
        head = self.commit()
        with patch.object(ci, 'main_baselines', return_value=dict.fromkeys(ci.LANES, self.base)):
            result = ci.plan('push', self.base, head, 'example/project')
        self.assertTrue(result['test'])
        self.assertTrue(result['native-matrix'])

    def test_renaming_or_deleting_source_selects_builds(self):
        path = self.root / 'rendering/bevy-explorer/source/src/lib.rs'
        path.rename(self.root / 'moved.rs')
        result = ci.select(self.base, self.commit())
        self.assertTrue(all(result[lane] for lane in ci.LANES))

    def test_web_assets_validate_web_without_recompiling_native_targets(self):
        for name in ('react-web/src/App.tsx', 'react-web/package-lock.json',
                     'react-web/bridge-scene/src/index.ts', 'deploy/web/engine/boot.js',
                     'deploy/web/service_worker.js'):
            with self.subTest(path=name):
                before = self.git('rev-parse', 'HEAD')
                self.write('rendering/bevy-explorer/source/' + name, 'changed')
                selected = ci.select(before, self.commit())
                self.assertEqual({lane for lane in (*ci.LANES, 'native-matrix', 'scenes')
                                  if selected[lane]}, {'web'})

    def test_shared_hud_sources_validate_web_only(self):
        for name in SHARED:
            with self.subTest(path=name):
                before = self.git('rev-parse', 'HEAD')
                self.write(name, 'changed')
                selected = ci.select(before, self.commit())
                self.assertEqual({lane for lane in (*ci.LANES, 'native-matrix', 'scenes')
                                  if selected[lane]}, {'web'})

    def test_library_ui_outside_the_hud_entries_selects_nothing(self):
        for name in ('library/ui/figma/src/cli.ts', 'library/ui/index.ts', 'library/ui/index.tsx',
                     'library/ui/components/NavBar/NavBar.tsx',
                     'library/ui/components/NavBar/NavBar.module.css', 'library/ui/upstream.lock.json'):
            with self.subTest(path=name):
                before = self.git('rev-parse', 'HEAD')
                self.write(name, 'changed')
                selected = ci.select(before, self.commit())
                self.assertEqual({lane for lane in (*ci.LANES, 'native-matrix', 'scenes') if selected[lane]}, set())

    def test_web_files_the_harness_tests_read_also_select_the_test_suite(self):
        for name in ci.HARNESS_WEB_READS:
            with self.subTest(path=name):
                before = self.git('rev-parse', 'HEAD')
                self.write(name, 'changed')
                selected = ci.select(before, self.commit())
                self.assertEqual({lane for lane in (*ci.LANES, 'native-matrix', 'scenes')
                                  if selected[lane]}, {'web', 'test'})

    def test_the_end_to_end_suite_selects_no_engine_lane(self):
        for name in ('driver/explorer.ts', 'specs/chat.test.ts', 'scenes/core/scene.json', 'README.md'):
            with self.subTest(path=name):
                before = self.git('rev-parse', 'HEAD')
                self.write('apps/bevy-explorer/e2e/' + name, 'changed')
                selected = ci.select(before, self.commit())
                self.assertEqual({lane for lane in (*ci.LANES, 'native-matrix', 'scenes')
                                  if selected[lane]}, set())

    def test_web_change_cannot_hide_a_native_or_shared_input_change(self):
        self.write('rendering/bevy-explorer/source/react-web/src/App.tsx', 'web edit')
        self.write('rendering/bevy-explorer/source/crates/system_api_types/build.rs', 'build script edit')
        selected = ci.select(self.base, self.commit())
        self.assertTrue(all(selected[lane] for lane in (*ci.LANES, 'native-matrix', 'scenes')))

    def test_headless_packages_select_headless_without_web_native_tests_or_scenes(self):
        for name in ('launcher/package.json', 'launcher/bin/cli.js',
                     'launcher/lib/resolve-binary.js', 'build-platform-package.js'):
            with self.subTest(path=name):
                before = self.git('rev-parse', 'HEAD')
                self.write('rendering/bevy-explorer/source/deploy/headless/' + name, 'changed')
                selected = ci.select(before, self.commit())
                self.assertEqual({lane for lane in (*ci.LANES, 'native-matrix', 'scenes')
                                  if selected[lane]}, {'headless'})

    def test_mixed_packages_keep_both_producers_and_rust_changes_widen_coverage(self):
        self.write('rendering/bevy-explorer/source/deploy/headless/launcher/package.json', 'headless')
        self.write('rendering/bevy-explorer/source/deploy/web/package.json', 'web')
        selected = ci.select(self.base, self.commit())
        self.assertEqual({lane for lane in (*ci.LANES, 'native-matrix', 'scenes')
                          if selected[lane]}, {'web', 'headless'})
        self.write('rendering/bevy-explorer/source/crates/dcl/src/lib.rs', 'changed Rust')
        selected = ci.select(self.base, self.commit())
        self.assertEqual({lane for lane in (*ci.LANES, 'native-matrix', 'scenes')
                          if selected[lane]}, {*ci.LANES, 'scenes'})

    def test_deleted_headless_launcher_still_selects_headless(self):
        name = 'rendering/bevy-explorer/source/deploy/headless/launcher/bin/cli.js'
        self.write(name, 'launcher')
        before = self.commit()
        (self.root / name).unlink()
        selected = ci.select(before, self.commit())
        self.assertTrue(selected['headless'])
        self.assertFalse(selected['test'])
        self.assertFalse(selected['web'])

    def test_deleted_web_assets_still_select_their_producer(self):
        name = 'rendering/bevy-explorer/source/react-web/src/App.tsx'
        self.write(name, 'web asset')
        before = self.commit()
        (self.root / name).unlink()
        selected = ci.select(before, self.commit())
        self.assertTrue(selected['web'])
        self.assertFalse(selected['test'])

    def test_pr_uses_supplied_base_and_merge_tree_without_api(self):
        self.write('apps/sites/only-sites.ts', 'changed')
        with patch.object(ci, 'api', side_effect=AssertionError('PR must not consult main runs')):
            self.assertFalse(ci.plan('pull_request', self.base, self.commit(), '')['test'])

    def pr_merge(self, main_file, pr_file):
        self.write(pr_file, 'pr')
        pr = self.commit()
        self.git('checkout', '-q', '--detach', self.base)
        self.write(main_file, 'main moved')
        tip = self.commit()
        self.git('merge', '-q', '--no-ff', '-m', f'Merge {pr} into {tip}', pr)
        return tip, pr, self.git('rev-parse', 'HEAD')

    def run_main(self, command, head, base, pr_head):
        with tempfile.TemporaryDirectory() as temporary:
            output, event = Path(temporary) / 'output', Path(temporary) / 'event.json'
            event.write_text(json.dumps({'pull_request': {'head': {'sha': pr_head}}}))
            env = {'GITHUB_EVENT_NAME': 'pull_request', 'GITHUB_EVENT_PATH': str(event), 'BASE_SHA': base,
                   'GH_REPO': '', 'GITHUB_OUTPUT': str(output), 'GITHUB_STEP_SUMMARY': ''}
            with patch.dict(os.environ, env), patch('sys.argv', ['bevy-ci.py', command, '--head', head]), \
                    patch.object(ci, 'api', side_effect=AssertionError('PR must not consult main runs')):
                ci.main()
            return dict(line.split('=', 1) for line in output.read_text().splitlines())

    def test_pr_compares_with_the_main_tip_it_was_merged_onto(self):
        tip, pr, merge = self.pr_merge('rendering/bevy-explorer/source/Cargo.toml', 'apps/sites/only-sites.ts')
        self.assertTrue(ci.plan('pull_request', self.base, merge, '')['native-matrix'])
        self.assertEqual(ci.tested_base(merge, pr, self.base), tip)
        values = self.run_main('plan', merge, self.base, pr)
        for lane in ('clippy', 'test', 'headless', 'web', 'native-matrix', 'scenes'):
            self.assertEqual(values[lane], 'false', lane)

    def test_pr_images_ignore_main_changes_since_the_payload_base(self):
        _, pr, merge = self.pr_merge('services/catalyst/src/main.rs', 'docs/only-docs.md')
        self.assertEqual(self.run_main('consumer-plan', merge, self.base, pr)['images'], 'false')

    def test_pr_merged_onto_main_keeps_its_own_engine_change(self):
        _, pr, merge = self.pr_merge('apps/sites/main-only.ts', 'rendering/bevy-explorer/source/src/lib.rs')
        values = self.run_main('plan', merge, self.base, pr)
        self.assertEqual((values['test'], values['web']), ('true', 'true'))

    def test_pr_base_falls_back_unless_github_merged_the_pr_head(self):
        self.write('rendering/bevy-explorer/source/src/lib.rs', 'changed')
        head = self.commit()
        self.assertEqual(ci.tested_base(head, head, self.base), self.base)
        self.assertEqual(self.run_main('plan', head, self.base, head)['test'], 'true')
        # A branch's own merge of main has two parents, but its second is not the PR head.
        self.git('checkout', '-q', '--detach', self.base)
        self.write('apps/sites/main-only.ts', 'main')
        main_tip = self.commit()
        self.git('checkout', '-q', '--detach', head)
        self.git('merge', '-q', '--no-ff', '-m', 'Merge main into feature', main_tip)
        branch_merge = self.git('rev-parse', 'HEAD')
        self.assertEqual(ci.tested_base(branch_merge, branch_merge, self.base), self.base)
        self.assertEqual(ci.tested_base(branch_merge, None, self.base), self.base)
        self.git('commit', '-q', '--allow-empty', '-m', f'notes\n\nparent {head}')
        self.assertEqual(ci.tested_base(self.git('rev-parse', 'HEAD'), head, 'fallback'), 'fallback')
        self.assertEqual(ci.tested_base('0' * 39 + '1', head, 'fallback'), 'fallback')

    def test_pr_base_is_read_from_a_shallow_checkout_of_the_merge_commit(self):
        tip, pr, merge = self.pr_merge('rendering/bevy-explorer/source/Cargo.toml', 'apps/sites/only-sites.ts')
        self.git('config', 'uploadpack.allowAnySHA1InWant', 'true')
        with tempfile.TemporaryDirectory() as temporary:
            clone = Path(temporary)
            run = lambda *args: subprocess.check_output(['git', *args], cwd=clone, text=True).strip()
            run('init', '-q')
            run('config', 'maintenance.auto', 'false')
            run('config', 'gc.auto', '0')
            # What actions/checkout does for a pull_request: the merge commit at depth 1.
            run('remote', 'add', 'origin', self.root.as_uri())
            run('fetch', '-q', '--no-tags', '--depth=1', 'origin', f'+{merge}:refs/remotes/pull/1/merge')
            run('checkout', '-q', '--detach', merge)
            self.assertNotEqual(subprocess.run(['git', 'rev-parse', '--verify', '-q', 'HEAD^1'], cwd=clone).returncode, 0)
            with patch.object(ci, 'ROOT', clone):
                self.assertEqual(ci.tested_base(merge, pr, self.base), tip)
                self.assertEqual(self.run_main('plan', merge, self.base, pr)['native-matrix'], 'false')

    def test_gateway_contract_inputs_select_only_the_linux_test_lane_on_prs(self):
        for name in ('infra/gateway/gateway.py', 'infra/gateway/routes.nix',
                     'apps/ui3/src/play/lib/baseDomain.ts',
                     'library/explorer-core/engine/generated/serviceTable.ts'):
            with self.subTest(path=name):
                before = self.git('rev-parse', 'HEAD')
                self.write(name, 'changed')
                selected = ci.plan('pull_request', before, self.commit(), '')
                self.assertEqual({lane for lane in (*ci.LANES, 'native-matrix', 'scenes')
                                  if selected[lane]}, {'test'})

    def test_upstream_watch_and_manual_packaging_are_not_ci_build_inputs(self):
        for name in ('upstream', 'desktop', 'rollout-manual'):
            self.write(f'.github/workflows/bevy-explorer-{name}.yml', 'changed')
        result = ci.select(self.base, self.commit())
        self.assertFalse(any(result[lane] for lane in ci.LANES))

    def test_platform_workflow_changes_select_the_shared_linux_producer(self):
        for name in ('bevy-explorer-headless.yml', 'bevy-explorer-headless-build.yml'):
            before = self.git('rev-parse', 'HEAD')
            self.write('.github/workflows/' + name, 'changed')
            selected = ci.select(before, self.commit())
            self.assertTrue(selected['headless'])
            self.assertFalse(selected['web'])
            self.assertFalse(selected['test'])

    def test_image_only_change_selects_consumer_without_engine(self):
        self.write('apps/sites/server.ts', 'changed')
        head = self.commit()
        self.assertTrue(ci.consumer_changed(self.base, head, 'images'))
        self.assertFalse(ci.consumer_plan('pull_request', self.base, head, '')['images'])
        self.assertFalse(any(ci.select(self.base, head)[lane] for lane in ci.LANES))

    def test_docs_only_change_skips_image_consumer(self):
        for name in ('DATABASE.md', 'docs/example.md', 'services/catalyst/docs/events.md',
                     'services/catalyst/docs/example.json', 'rendering/bevy-explorer/docs/example.json',
                     'rendering/bevy-explorer/source/README.md',
                     'apps/bevy-explorer/README.md', 'library/vendor/rusty_enet/README.md'):
            self.write(name, 'changed')
        head = self.commit()
        self.assertFalse(ci.consumer_plan('pull_request', self.base, head, '')['images'])
        self.assertFalse(ci.consumer_changed(self.base, head, 'images'))
        result = ci.select(self.base, head)
        self.assertFalse(any(result[lane] for lane in (*ci.LANES, 'native-matrix', 'scenes')))
        self.write('services/catalyst/explore/src/lib.rs', 'source')
        head = self.commit()
        self.assertTrue(ci.consumer_changed(self.base, head, 'images'))
        self.assertFalse(ci.consumer_plan('pull_request', self.base, head, '')['images'])

    def test_docs_source_embedded_data_and_published_content_rebuild_images(self):
        for name in ('apps/sites/src/features/docs/DocsPage.tsx',
                     'apps/sites/src/data/lib/docs/render.ts',
                     'library/dcl-scene-authoring/docs/composite-component-schemas.json',
                     'library/dcl-scene-authoring/docs/composite-tojson-edge-cases.json',
                     'apps/sites/content/docs/example.md', 'apps/sites/content/docs/page.mdx',
                     'apps/sites/content/docs/manifest.json', 'apps/sites/content/docs/search-index.json',
                     'apps/sites/content/docs/_media.json'):
            with self.subTest(path=name):
                before = self.git('rev-parse', 'HEAD')
                self.write(name, 'changed')
                head = self.commit()
                self.assertTrue(ci.consumer_changed(before, head, 'images'))
                self.assertEqual(ci.consumer_plan('pull_request', before, head, '')['images'],
                                 any(fnmatch.fnmatchcase(name, pattern) for pattern in ci.IMAGES_PACKAGING))
                with patch.object(ci, 'api', return_value={'workflow_runs': [{'head_sha': before, 'id': 1}]}), \
                        patch.object(ci, 'trusted_run', return_value=True), \
                        patch.object(ci, 'run_jobs', return_value=[]), \
                        patch.object(ci, 'consumer_ready', return_value=True):
                    result = ci.consumer_plan('push', before, head, 'example/project')
                self.assertTrue(result['images'])
                self.assertEqual(result['reason'], 'Image consumer inputs changed')

    def test_a_pull_request_builds_images_for_packaging_only(self):
        for name, built in (('services/catalyst/src/new.rs', False), ('apps/sites/src/new.ts', False),
                            ('infra/containers/new.nix', True), ('tooling/image-new.py', True),
                            ('services/catalyst/Cargo.lock', True), ('services/catalyst/new/Cargo.toml', True),
                            ('services/catalyst/rust-toolchain.toml', True), ('.cargo/config.toml', True),
                            ('library/new/Cargo.toml', True), ('apps/sites/pnpm-lock.yaml', True),
                            ('apps/sites/package.json', True), ('apps/sites/.npmrc', True)):
            with self.subTest(path=name):
                before = self.git('rev-parse', 'HEAD')
                self.write(name, 'changed')
                head = self.commit()
                self.assertEqual(ci.consumer_plan('pull_request', before, head, '')['images'], built)
                self.assertTrue(ci.consumer_changed(before, head, 'images'))

    def test_the_architecture_choice_reads_packaging_inputs_only(self):
        workflow = (Path(__file__).parents[2] / '.github/workflows/apis-sites-images.yml').read_text()
        listed = workflow.split('git diff --quiet HEAD^1 HEAD -- \\\n', 1)[1].split('; then', 1)[0]
        specs = shlex.split(listed.replace('\\\n', ' '))
        self.assertIn('apps/sites/pnpm-lock.yaml', specs)
        for spec in specs:
            sample = spec.replace('*', 'a')
            with self.subTest(spec=spec):
                self.assertTrue(any(fnmatch.fnmatchcase(path, pattern) for pattern in ci.IMAGES_PACKAGING
                                    for path in (sample, sample + '/a')))

    def test_source_renamed_to_documentation_still_selects_builds(self):
        (self.root / 'rendering/bevy-explorer/source/src/lib.rs').rename(
            self.root / 'rendering/bevy-explorer/source/source.md')
        head = self.commit()
        self.assertTrue(ci.consumer_changed(self.base, head, 'images'))
        self.assertTrue(ci.select(self.base, head)['test'])

    def test_markdown_golden_changes_select_tests_and_images(self):
        self.write('rendering/bevy-explorer/source/local-harness/benchmark/tests/fixtures/report/ok.md',
                   'broken report golden')
        head = self.commit()
        self.assertTrue(ci.select(self.base, head)['test'])
        self.assertTrue(ci.consumer_changed(self.base, head, 'images'))

    def test_unwatched_fixtures_do_not_select_images(self):
        self.write('apps/creator-hub/tests/fixtures/example.md', 'fixture')
        self.write('apps/creator-hub/tests/fixtures/example.json', '{}')
        head = self.commit()
        self.assertFalse(ci.consumer_changed(self.base, head, 'images'))
        self.assertFalse(ci.consumer_plan('pull_request', self.base, head, '')['images'])

    def test_deleted_prose_documentation_does_not_select_images(self):
        for path in ('services/catalyst/docs/example.md', 'rendering/bevy-explorer/docs/example.md',
                     'docs/example.md'):
            with self.subTest(path=path):
                self.write(path, 'prose')
                before = self.commit()
                (self.root / path).unlink()
                head = self.commit()
                self.assertFalse(ci.consumer_changed(before, head, 'images'))
                self.assertFalse(ci.consumer_plan('pull_request', before, head, '')['images'])

    def test_removed_or_renamed_embedded_markdown_selects_builds(self):
        for path in ('rendering/bevy-explorer/bevy/crates/example/README.md',
                     'rendering/bevy-explorer/bevy/crates/bevy_color/docs/conversion.md',
                     'rendering/bevy-explorer/bevy/errors/B0001.md'):
            for rename in (False, True):
                with self.subTest(path=path, rename=rename):
                    self.write(path, 'original')
                    before = self.commit()
                    self.write(path, 'edited rustdoc')
                    edited = self.commit()
                    self.assertFalse(ci.select(before, edited)['test'])
                    self.assertFalse(ci.consumer_changed(before, edited, 'images'))
                    source = self.root / path
                    if rename:
                        source.rename(source.with_name('renamed.md'))
                    else:
                        source.unlink()
                    removed = self.commit()
                    self.assertTrue(ci.select(edited, removed)['test'])
                    self.assertTrue(ci.consumer_changed(edited, removed, 'images'))

    def test_local_hook_changes_do_not_select_engine_or_images(self):
        self.write('tooling/ci-local.py', 'unrelated local hook change')
        head = self.commit()
        result = ci.select(self.base, head)
        self.assertFalse(any(result[lane] for lane in (*ci.LANES, 'native-matrix', 'scenes')))
        self.assertFalse(ci.consumer_changed(self.base, head, 'images'))

    def test_wildcard_inputs_stay_in_their_directory(self):
        self.write('tooling/dev-regressions-x/y.json', '{}')
        result = ci.select(self.base, self.commit())
        self.assertFalse(any(result[lane] for lane in ci.LANES) or result['scenes'])

    def test_schema_files_rebuild_images_and_independent_crates_or_docs_do_not(self):
        base = self.base
        for name, expected in (('schemas/10_explore.sql', True), ('apps/dcl-scene-cli/src/main.rs', False),
                               ('rendering/bevy-explorer/README.md', False)):
            self.write(name, 'changed')
            head = self.commit()
            self.assertEqual(ci.consumer_changed(base, head, 'images'), expected, name)
            base = head

    def test_consumer_failure_to_find_a_successful_baseline_builds_images(self):
        with patch.object(ci, 'api', return_value={'workflow_runs': []}):
            self.assertTrue(ci.consumer_plan('push', self.base, self.base, 'example/project')['images'])

    def test_push_uses_successful_build_not_previous_canceled_commit(self):
        self.write('rendering/bevy-explorer/source/src/lib.rs', 'engine edit')
        canceled = self.commit()
        self.write('apps/bevy-explorer/README.md', 'docs only')
        with patch.object(ci, 'main_baselines', return_value=dict.fromkeys(ci.LANES, self.base)):
            self.assertTrue(ci.plan('push', canceled, self.commit(), 'example/project')['test'])

    def test_manual_missing_history_and_api_errors_run_full(self):
        with patch.object(ci, 'api', side_effect=OSError('unavailable')):
            for event, base in (('workflow_dispatch', self.base), ('push', self.base),
                                ('pull_request', '0' * 40), ('unknown', self.base)):
                with self.subTest(event=event):
                    result = ci.plan(event, base, self.base, 'example/project')
                    self.assertTrue(all(result[lane] for lane in ci.LANES))
                    self.assertTrue(result['native-matrix'])

    def test_known_integration_inputs_select_only_their_consumers(self):
        cases = {
            'apps/bevy-explorer/Dockerfile.web': {'web'},
            'apps/bevy-explorer/nginx.conf': {'web'},
            'apps/bevy-explorer/build-web.py': {'web'},
            'apps/bevy-explorer/build-headless.py': {'headless'},
            'apps/bevy-explorer/Dockerfile.headless': {'headless'},
            'apps/bevy-explorer/image-smoke.py': {'web', 'headless'},
            '.github/workflows/bevy-explorer-images.yml': {'web', 'headless'},
            'apps/bevy-explorer/prepare-native-cache.py': {'clippy', 'test', 'headless', 'native-matrix', 'scenes'},
            'apps/bevy-explorer/install-linux-deps.py': {*ci.LANES, 'scenes'},
            'apps/bevy-explorer/install-web-tools.py': {'web'},
            ci.SCENES_WORKFLOW: {'scenes'},
            ci.WORKFLOW: {*ci.LANES, 'native-matrix'},
            'apps/bevy-explorer/README.md': set(),
            'tooling/tests/test_bevy_linux_deps.py': set(),
        }
        for path, expected in cases.items():
            with self.subTest(path=path):
                before = self.git('rev-parse', 'HEAD')
                self.write(path, 'changed')
                result = ci.select(before, self.commit())
                self.assertEqual({lane for lane in (*ci.LANES, 'native-matrix', 'scenes') if result[lane]}, expected)

    def test_unknown_inputs_and_shared_protoc_changes_run_all_platforms_and_scenes(self):
        for path in ('apps/bevy-explorer/future-build-input', 'apps/bevy-explorer/install-protoc.py',
                     'tooling/bevy-ci.py', 'tooling/bevy-mtimes.py'):
            before = self.git('rev-parse', 'HEAD')
            self.write(path, 'changed')
            result = ci.select(before, self.commit())
            self.assertTrue(all(result[lane] for lane in (*ci.LANES, 'native-matrix', 'scenes')))

    def test_flake_tool_changes_select_the_linux_lanes_and_the_native_matrix_without_scenes(self):
        # The Linux lanes take protoc, taplo and wasm-pack from the flake's bevy-tools package and
        # the macOS Test Suite its ffmpeg; the graphical scenes still run install-protoc.py.
        for path in ('flake.lock', 'flake.nix', 'infra/packages/default.nix', 'infra/packages/tools.nix',
                     '.github/actions/bevy-explorer-tools/action.yml', 'apps/bevy-explorer/install-tools.sh'):
            with self.subTest(path=path):
                before = self.git('rev-parse', 'HEAD')
                self.write(path, 'changed')
                result = ci.select(before, self.commit())
                self.assertEqual({lane for lane in (*ci.LANES, 'native-matrix', 'scenes') if result[lane]}, {*ci.LANES, 'native-matrix'})
        # The other package definitions feed the images, not the engine lanes.
        before = self.git('rev-parse', 'HEAD')
        self.write('infra/packages/play.nix', 'changed')
        result = ci.select(before, self.commit())
        self.assertFalse(any(result[lane] for lane in (*ci.LANES, 'native-matrix', 'scenes')))

    def test_vendored_bevy_selects_every_lane_and_the_native_matrix(self):
        # The Explorer builds Bevy from rendering/bevy-explorer/bevy, platform code included.
        self.write('rendering/bevy-explorer/bevy/crates/bevy_scene/src/scene_spawner.rs', 'changed')
        result = ci.select(self.base, self.commit())
        self.assertTrue(all(result[lane] for lane in (*ci.LANES, 'native-matrix', 'scenes')))

    def test_vendored_bevy_rebuilds_the_images(self):
        self.write('rendering/bevy-explorer/bevy/crates/bevy_scene/src/scene_spawner.rs', 'changed')
        self.assertTrue(ci.consumer_changed(self.base, self.commit(), 'images'))

    def test_newer_web_pass_cannot_hide_an_unvalidated_native_change_on_main(self):
        self.write('rendering/bevy-explorer/source/src/lib.rs', 'native change')
        later_web = self.commit()
        self.write('apps/bevy-explorer/nginx.conf', 'web change')
        head = self.commit()
        baselines = {**dict.fromkeys(ci.LANES, self.base), 'web': head}
        with patch.object(ci, 'main_baselines', return_value=baselines):
            result = ci.plan('push', later_web, head, 'example/project')
        self.assertFalse(result['web'])
        self.assertTrue(result['test'])
        self.assertTrue(result['headless'])
        self.assertTrue(result['native-matrix'])

    def test_missing_main_lane_baseline_runs_only_that_unproven_lane(self):
        baselines = dict.fromkeys(('clippy', 'test', 'web'), self.base)
        with patch.object(ci, 'main_baselines', return_value=baselines):
            result = ci.plan('push', self.base, self.base, 'example/project')
        self.assertTrue(result['headless'])
        self.assertFalse(result['web'])
        self.assertFalse(result['test'])
        self.assertFalse(result['native-matrix'])

    def test_graphical_selection_uses_event_diff_and_manual_run_is_full(self):
        self.write('apps/bevy-explorer/Dockerfile.web', 'web image change')
        head = self.commit()
        self.assertFalse(ci.scenes_plan('pull_request', self.base, head)['scenes'])
        self.assertFalse(ci.scenes_plan('push', self.base, head)['scenes'])
        self.assertTrue(ci.scenes_plan('workflow_dispatch', self.base, head)['scenes'])
        self.write('rendering/bevy-explorer/source/src/lib.rs', 'engine change')
        head = self.commit()
        self.assertTrue(ci.scenes_plan('pull_request', self.base, head)['scenes'])
        self.assertEqual(ci.scenes_plan('push', self.base, head)['sha'], head)
        self.assertTrue(ci.scenes_plan('push', '0' * 40, head)['scenes'])

    def test_allowed_failure_list_changes_select_only_the_graphical_suite(self):
        self.write('apps/bevy-explorer/scene-tests.txt', '52,-52\n')
        selected = ci.select(self.base, self.commit())
        self.assertTrue(selected['scenes'])
        self.assertFalse(any(selected[lane] for lane in (*ci.LANES, 'native-matrix')))


class SceneSuiteTest(unittest.TestCase):
    SCENES = ['52,-52', '52,-54', '52,-56', '52,-58', '52,-60', '52,-62', '52,-64', '52,-66', '52,-68',
              '54,-52', '54,-54', '54,-56', '54,-58', '54,-60']

    def test_suite_file_lists_the_fourteen_scenes_and_the_snapshot_subset(self):
        full = ci.scene_suite('full').split(';')
        self.assertEqual([scene.split('/')[0] for scene in full], self.SCENES)
        self.assertEqual([scene.split('/')[0] for scene in ci.scene_suite('snapshots').split(';')],
                         list(ci.SNAPSHOT_SCENES))
        names = [name for scene in full for name in scene.split('/')[1:]]
        self.assertTrue(all(name == name.strip() and ';' not in name for name in names))
        self.assertEqual(len(names), len(set(names)))

    def test_malformed_suite_lines_are_rejected(self):
        with tempfile.TemporaryDirectory() as temporary, patch.object(ci, 'ROOT', Path(temporary)):
            suite = Path(temporary) / ci.SCENE_SUITE
            suite.parent.mkdir(parents=True)
            for bad in ('52,-52/ trailing space ', '52,-52//', 'a,b', '52,-52/x;y'):
                suite.write_text(f'# comment\n{bad}\n')
                with self.assertRaises(ValueError, msg=bad):
                    ci.scene_suite('full')
            suite.write_text('52,-52/a: b\n52,-62\n')
            self.assertEqual(ci.scene_suite('full'), '52,-52/a: b;52,-62')
            self.assertEqual(ci.scene_suite('snapshots'), '52,-62')

    def test_summary_reports_listed_unlisted_and_stale_failures(self):
        log = ('INFO scene_runner::automatic_testing: test a: b: true [1 remaining]\n'
               'INFO scene_runner::automatic_testing: test c: d: false [0 remaining]\n'
               'INFO scene_runner::automatic_testing: test e: f: false [0 remaining]\n'
               'INFO scene_runner::automatic_testing: some tests failed:\n [\n'
               '    (\n        "[(52,-52) : c: d]",\n        "Error: image not found (it has been created)",\n'
               '        true,\n    ),\n    (\n        "[(52,-62) : e: f]",\n'
               '        "Error: method grey-diff was specified and greyPixelDiff similarity (0.97) is lower than threshold 0.9995",\n'
               '        false,\n    ),\n]\n')
        with tempfile.TemporaryDirectory() as temporary, patch.object(ci, 'ROOT', Path(temporary)):
            suite = Path(temporary) / ci.SCENE_SUITE
            suite.parent.mkdir(parents=True)
            suite.write_text('52,-52/c: d/g: h\n52,-62\n')
            text = ci.scene_summary('full', 'failure', log)
            self.assertIn('1 assertion passed, 2 failed; 1 of them are listed', text)
            self.assertIn('Not listed: (52,-62) e: f', text)
            self.assertIn('Listed but passing, remove them: (52,-52) g: h', text)
            self.assertNotIn('discarded', text)
            self.assertIn('no assertion ran', ci.scene_summary('full', 'failure', ''))
            self.assertIn('acceptance holds', ci.scene_summary('snapshots', 'success', 'INFO scene_runner::automatic_testing: test a: b: true [0 remaining]\nall tests passed!'))

    def test_summary_of_a_run_that_stopped_early_determines_no_list(self):
        log = ('INFO scene_runner::automatic_testing: test a: b: true [1 remaining]\n'
               'INFO scene_runner::automatic_testing: test c: d: false [0 remaining]\n'
               'INFO scene_runner::automatic_testing: test e: f: false [0 remaining]\n')
        with tempfile.TemporaryDirectory() as temporary, patch.object(ci, 'ROOT', Path(temporary)):
            suite = Path(temporary) / ci.SCENE_SUITE
            suite.parent.mkdir(parents=True)
            suite.write_text('52,-52/c: d/g: h\n52,-62\n')
            text = ci.scene_summary('full', 'failure', log)
            self.assertIn('1 assertion passed, 2 failed; the run stopped before its end', text)
            self.assertIn('not determined', text)
            for absent in ('of them are listed', 'Not listed', 'remove them'):
                self.assertNotIn(absent, text)

    def test_summary_matches_escaped_names_and_skips_discarded_failures(self):
        log = ('INFO scene_runner::automatic_testing: test say "hi"\\now: false [0 remaining]\n'
               'INFO scene_runner::automatic_testing: test outside: false [0 remaining]\n'
               'WARN scene_runner::automatic_testing: location (52,-60) wasn\'t part of the required set, ignoring this failure\n'
               'INFO scene_runner::automatic_testing: some tests failed:\n [\n'
               '    (\n        "[(52,-52) : say \\"hi\\"\\\\now]",\n        "Error: image not found (it has been created)",\n'
               '        true,\n    ),\n]\n'
               'INFO scene_runner::automatic_testing: all failures were allowed\n')
        with tempfile.TemporaryDirectory() as temporary, patch.object(ci, 'ROOT', Path(temporary)):
            suite = Path(temporary) / ci.SCENE_SUITE
            suite.parent.mkdir(parents=True)
            suite.write_text('52,-52/say "hi"\\now\n')
            text = ci.scene_summary('full', 'success', log)
            self.assertIn('0 assertions passed, 1 failed; 1 of them are listed', text)
            self.assertIn('the runner discarded 1 failure outside the suite', text)
            for absent in ('Not listed', 'remove them'):
                self.assertNotIn(absent, text)
            suite.write_text('52,-52/say "hi"\\now/other\n')
            self.assertIn('remove them: (52,-52) other', ci.scene_summary('full', 'success', log))
            unlisted = log.replace('        true,', '        false,')
            self.assertIn('Not listed: (52,-52) say "hi"\\now', ci.scene_summary('full', 'failure', unlisted))
            suite.write_text('# nothing\n')
            with self.assertRaisesRegex(ValueError, 'Empty full scene suite'):
                ci.scene_suite('full')

    def test_summary_reads_the_runner_log_with_its_colour_codes(self):
        line = ('\x1b[2m2026-10-31T23:51:03.032696Z\x1b[0m \x1b[32m INFO\x1b[0m '
                '\x1b[2mscene_runner::automatic_testing\x1b[0m\x1b[2m:\x1b[0m ')
        log = line + 'test a: b: true [0 remaining]\n' + line + 'all tests passed!\n'
        with tempfile.TemporaryDirectory() as temporary, patch.object(ci, 'ROOT', Path(temporary)):
            suite = Path(temporary) / ci.SCENE_SUITE
            suite.parent.mkdir(parents=True)
            suite.write_text('52,-52\n')
            self.assertIn('1 assertion passed, 0 failed; graphical acceptance holds',
                          ci.scene_summary('full', 'success', log))

    def test_scenes_workflow_builds_its_suite_from_the_listed_file(self):
        text = (Path(__file__).parents[2] / ci.SCENES_WORKFLOW).read_text()
        self.assertIn('bevy-ci.py scene-suite --suite "$SCENE_SUITE"', text)
        self.assertIn('bevy-ci.py scene-summary --suite "$SCENE_SUITE" --outcome "$SCENE_OUTCOME"', text)
        self.assertIn('--log scene-tests.log | tee -a "$GITHUB_STEP_SUMMARY"', text)
        self.assertNotIn('52,-52;', text)


class PublicationTest(unittest.TestCase):
    def test_only_successful_same_repository_main_ci_is_trusted(self):
        self.assertTrue(ci.trusted_run(source_run(), 'example/project'))
        for changed in ({'status': 'in_progress'}, {'conclusion': 'failure'}, {'event': 'pull_request'},
                        {'head_branch': 'dev'}, {'head_repository': {'full_name': 'fork/project'}},
                        {'path': '.github/workflows/unrelated.yml'}, {'head_sha': 'not-a-sha'}):
            with self.subTest(changed=changed):
                self.assertFalse(ci.trusted_run(source_run(**changed), 'example/project'))

    def test_products_are_selected_independently_from_successful_producers(self):
        self.assertEqual(ci.produced_artifacts(producers('skipped')), [])
        self.assertEqual(ci.produced_artifacts(producers()), ['web', 'headless'])
        for product in ci.PRODUCERS:
            jobs = producers('skipped')
            next(job for job in jobs if job['name'] == ci.PRODUCERS[product])['conclusion'] = 'success'
            self.assertEqual(ci.produced_artifacts(jobs), [product])
            entries = ci.image_matrix([product])['include']
            self.assertEqual({row['product'] for row in entries}, {product})
            self.assertEqual({row['arch'] for row in entries},
                             {'amd64', 'arm64'} if product == 'web' else {'amd64'})

    def test_missing_failed_duplicate_or_running_producer_never_allows_publication(self):
        invalid = [[], producers()[:1], producers() * 2, producers('failure'), producers('cancelled')]
        for outcome in ('failure', 'cancelled', 'timed_out'):
            mixed = producers()
            mixed[0]['conclusion'] = outcome
            invalid.append(mixed)
        running = producers()
        running[0]['status'] = 'in_progress'
        invalid.append(running)
        for jobs in invalid:
            with self.subTest(jobs=jobs), self.assertRaises(ValueError):
                ci.produced_artifacts(jobs)

    def test_baselines_are_per_lane_and_ignore_untrusted_or_skipped_jobs(self):
        runs = [source_run(id=44, event='pull_request'), source_run(id=43),
                source_run(id=42, head_sha='b' * 40), source_run(id=41, head_sha='c' * 40)]
        web_only = producers('skipped')
        web_only[0]['conclusion'] = 'success'
        native = producers() + [
            {'name': name, 'status': 'completed', 'conclusion': 'success'}
            for name in ('Clippy', *ci.NATIVE_JOBS)]
        with patch.object(ci, 'api', return_value={'workflow_runs': runs}), \
                patch.object(ci, 'run_jobs', side_effect=[producers('skipped'), web_only, native]) as jobs:
            expected = {**dict.fromkeys(ci.LANES, 'c' * 40), 'web': 'b' * 40}
            self.assertEqual(ci.main_baselines('example/project'), expected)
            self.assertEqual([call.args[1] for call in jobs.call_args_list], [43, 42, 41])

    def test_partial_or_duplicate_native_matrix_is_not_a_passing_baseline(self):
        jobs = [{'name': name, 'status': 'completed', 'conclusion': 'success'} for name in sorted(ci.NATIVE_JOBS)]
        self.assertTrue(ci.passed_lane(jobs, 'test'))
        linux = next(job for job in jobs if job['name'] == 'Test Suite (ubuntu-24.04)')
        self.assertTrue(ci.passed_lane([linux], 'test'))
        for invalid in ([], jobs[:2], jobs + [linux], [{**linux, 'conclusion': 'skipped'}]):
            self.assertFalse(ci.passed_lane(invalid, 'test'))

    def test_missing_baseline_or_incomplete_job_list_cannot_prove_a_pass(self):
        with patch.object(ci, 'api', return_value={'workflow_runs': []}):
            self.assertEqual(ci.main_baselines('example/project'), {})
        with patch.object(ci, 'api', return_value={'total_count': 3, 'jobs': producers()}), \
                self.assertRaises(ValueError):
            ci.run_jobs('example/project', 42)

    def test_source_command_reports_skip_without_relabeling_another_commit(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / 'output'
            env = {'GH_REPO': 'example/project', 'SOURCE_RUN': '42', 'GITHUB_REF': 'refs/heads/dev',
                   'GITHUB_SHA': 'b' * 40, 'GITHUB_OUTPUT': str(output), 'GITHUB_STEP_SUMMARY': ''}
            with patch.dict(os.environ, env), patch('sys.argv', ['bevy-ci.py', 'source']), \
                    patch.object(ci, 'api', return_value=source_run()), \
                    patch.object(ci, 'run_jobs', return_value=producers('skipped')):
                ci.main()
            text = output.read_text()
            self.assertIn('build=false\n', text)
            self.assertIn(f"sha={'a' * 40}\n", text)
            self.assertNotIn('b' * 40, text)

    def test_source_command_exports_only_the_successful_product_and_exact_source_sha(self):
        jobs = producers('skipped')
        jobs[0]['conclusion'] = 'success'
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / 'output'
            env = {'GH_REPO': 'example/project', 'SOURCE_RUN': '42',
                   'GITHUB_OUTPUT': str(output), 'GITHUB_STEP_SUMMARY': ''}
            with patch.dict(os.environ, env), patch('sys.argv', ['bevy-ci.py', 'source']), \
                    patch.object(ci, 'api', return_value=source_run()), \
                    patch.object(ci, 'run_jobs', return_value=jobs):
                ci.main()
            values = dict(line.split('=', 1) for line in output.read_text().splitlines())
            self.assertEqual(values['sha'], 'a' * 40)
            self.assertEqual(values['build'], 'true')
            self.assertEqual(json.loads(values['products']), ['web'])
            self.assertEqual(json.loads(values['build-matrix']), ci.image_matrix(['web']))


if __name__ == '__main__':
    unittest.main()
