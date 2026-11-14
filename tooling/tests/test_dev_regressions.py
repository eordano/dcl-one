"""CI inventory fixtures and a dependency-free Cargo build-context regression."""
import importlib.util
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("regressions", ROOT / "tooling/dev-regressions.py")
guard = importlib.util.module_from_spec(spec)
spec.loader.exec_module(guard)


class InventoryTests(unittest.TestCase):
    def test_bot_review_bodies_are_bound_to_their_existing_targets(self):
        bodies = {
            ("catalyst_comms", "lib"): [
                "archipelago::observations::tests::saturated_heartbeats_cannot_drop_or_overtake_a_waiting_disconnect",
                "archipelago::observations::tests::disconnect_waiters_retain_admission_credit_across_repeated_churn",
                "archipelago::observations::tests::disconnect_admission_ignores_link_readiness_but_counts_bus_and_shutdown_loss",
                "archipelago::observations::tests::refused_observations_count_every_loss_without_logging_on_the_socket_path",
                "pulse::statistics::tests::draining_changes_releases_bytes_while_the_backlog_stays_nonempty",
                "stats::tests::indexed_changes_preserve_order_and_rebuild_after_removal_and_recovery",
                "stats::tests::http_tests::method_admission_bounds_labels_without_relabeling_accepted_extensions",
                "stats::tests::content_tests::thumbnail_only_sql_matches_full_projection_for_metadata_and_selection_variants",
                "gatekeeper::bus::broker::tests::terminal_submission_waits_for_capacity_without_admission_readiness",
                "gatekeeper::bus::broker::tests::cancelled_terminal_heads_cannot_publish_after_a_link_rebuild",
                "gatekeeper::bus::broker::tests::retained_head_is_bounded_and_cancellation_before_sdk_start_prevents_publication",
                "gatekeeper::bus::broker::tests::retained_capability_rejects_every_non_disconnect_payload",
                "archipelago::observations::tests::shutdown_deadline_cancels_unsubmitted_disconnect_and_counts_all_forced_loss_once",
                "gatekeeper::bus::broker::tests::retained_sdk_deadline_and_link_changes_leave_submission_unconfirmed",
                "archipelago::handlers::tests::connector_method_flood_preserves_http_without_creating_metric_series",
                "stats::runtime::tests::connector_observer_bounds_method_labels_and_preserves_known_lowercase_labels",
                "archipelago::observations::tests::producer_registration_is_lazy_and_recovers_restart_without_replaying_events",
                "pulse::statistics::tests::producer_capacity_never_forgets_cursors_and_backlog_recovers_completely",
            ],
            ("catalyst_content", "lib"): [
                "write_path_tests::a_profile_with_nul_metadata_preserves_the_write_error_and_active_profile",
            ],
            ("pulse_nats_live", "test"): [
                "cancelled_terminal_disconnect_is_not_replayed_after_real_broker_reconnect",
            ],
        }
        for target, names in bodies.items():
            required = guard.MANDATORY_ARCHIPELAGO[target]
            normal = "".join(f"{name}: test\n" for name in required)
            guard.verify(required, normal, "", str(target))
            for name in names:
                self.assertIn(name, required)
                others = normal.replace(f"{name}: test\n", "")
                for changed, ignored in [
                    (others, ""),
                    (others + f"{name}_renamed: test\n", ""),
                    (normal, f"{name}: test\n"),
                    (normal + f"{name}: test\n", ""),
                    (others + f"{name}: benchmark\n", ""),
                ]:
                    with self.subTest(target=target, name=name, changed=changed, ignored=ignored), self.assertRaises(ValueError):
                        guard.verify(required, changed, ignored, str(target))

    def test_selected_reader_bodies_cannot_disappear_from_the_ordinary_inventory(self):
        for target in [("catalyst_comms", "lib"), ("pulse_cluster_lookup", "test"),
                       ("comms_v4_assignment_fence", "test")]:
            required = guard.MANDATORY_ARCHIPELAGO[target]
            normal = "".join(f"{name}: test\n" for name in required)
            guard.verify(required, normal, "", str(target))
            for name in required:
                others = normal.replace(f"{name}: test\n", "")
                for changed, ignored in [
                    (others, ""),
                    (others + f"{name}_renamed: test\n", ""),
                    (normal, f"{name}: test\n"),
                    (normal + f"{name}: test\n", ""),
                    (others + f"{name}: benchmark\n", ""),
                ]:
                    with self.subTest(target=target, name=name, changed=changed, ignored=ignored), self.assertRaises(ValueError):
                        guard.verify(required, changed, ignored, str(target))

    def test_rpc_diagnostic_body_cannot_disappear_from_the_ordinary_inventory(self):
        name = "rpc_validator::tests::workload_transport_diagnostics_keep_io_causes_without_endpoint_data"
        required = guard.MANDATORY_ARCHIPELAGO[("dcl_auth", "lib")]
        others = "".join(f"{entry}: test\n" for entry in required if entry != name)
        normal = others + f"{name}: test\n"
        guard.verify(required, normal, "", "auth diagnostics")
        for changed, ignored in [
            (others, ""),
            (others + f"{name}_renamed: test\n", ""),
            (normal, f"{name}: test\n"),
            (normal + f"{name}: test\n", ""),
        ]:
            with self.subTest(normal=changed, ignored=ignored), self.assertRaises(ValueError):
                guard.verify(required, changed, ignored, "auth diagnostics")

    def test_exact_engine_inventory_requires_one_unignored_test_and_no_other_entries(self):
        validate = getattr(guard, "verify_exact", None)
        self.assertTrue(callable(validate), "the isolated engine process needs an exact inventory guard")
        validate(["configured"], "configured: test\n", "", "engine")
        for normal, ignored in [
            ("", ""),
            ("renamed: test\n", ""),
            ("configured: test\nextra: test\n", ""),
            ("configured: test\nextra: benchmark\n", ""),
            ("configured: benchmark\n", ""),
            ("configured: test\n", "configured: test\n"),
            ("configured: test\n", "extra: test\n"),
            ("configured: test\n", "extra: benchmark\n"),
            ("configured: test\nconfigured: test\n", ""),
        ]:
            with self.subTest(normal=normal, ignored=ignored), self.assertRaises(ValueError):
                validate(["configured"], normal, ignored, "engine")
        with self.assertRaises(ValueError):
            validate(["configured", "extra"], "configured: test\nextra: test\n", "", "engine")

    def test_exact_engine_guard_reads_both_executable_inventories(self):
        with mock.patch.object(guard, "run", side_effect=["configured: test\n", ""]) as execute:
            guard.check_executable(["configured"], "engine", "isolated engine", exact=True)
        self.assertEqual(execute.call_args_list, [
            mock.call(["engine", "--list", "--format", "terse"]),
            mock.call(["engine", "--list", "--ignored", "--format", "terse"]),
        ])
        with mock.patch.object(guard, "run", side_effect=["configured: test\nextra: test\n", ""]), \
             self.assertRaises(ValueError):
            guard.check_executable(["configured"], "engine", "isolated engine", exact=True)

    def test_external_result_requires_exact_success_without_skips(self):
        validate = getattr(guard, "verify_external_result", None)
        self.assertTrue(callable(validate), "provisioned tests need an exact execution-result guard")
        validate("required", "test required ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored; 9 filtered out; finished in 0.01s\n")
        for output in ["test result: ok. 0 passed; 0 failed; 0 ignored; 9 filtered out; finished in 0.00s\n",
                       "test required ... ignored\ntest result: ok. 0 passed; 0 failed; 1 ignored; 9 filtered out; finished in 0.00s\n",
                       "test other ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored; 9 filtered out; finished in 0.01s\n"]:
            with self.subTest(output=output), self.assertRaises(ValueError):
                validate("required", output)

    def test_external_result_rejects_duplicate_or_failed_summary(self):
        good = "test required ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 9 filtered out; finished in 0.01s\n"
        guard.verify_external_result("required", good)
        for output in [good + good, good.replace("1 passed", "2 passed"),
                       good.replace("0 failed", "1 failed"), good.split("test result:")[0]]:
            with self.subTest(output=output), self.assertRaises(ValueError):
                guard.verify_external_result("required", output)

    def test_external_inventory_selects_the_exact_ignored_body(self):
        output = "test required ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 9 filtered out; finished in 0.01s\n"
        with mock.patch.object(guard, "run", side_effect=["required: test\nother: test\n", "required: test\n"]), \
             mock.patch.object(guard.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, output)) as execute:
            guard.run_external("fixture", "required")
        self.assertEqual(execute.call_args.args[0], ["fixture", "required", "--exact", "--ignored", "--test-threads=1", "--color", "never"])
        for normal, ignored in [("other: test\n", ""), ("required: test\n", ""),
                                ("required: test\nrequired: test\n", "required: test\n")]:
            with self.subTest(normal=normal), mock.patch.object(guard, "run", side_effect=[normal, ignored]), \
                 mock.patch.object(guard.subprocess, "run") as execute, self.assertRaises(ValueError):
                guard.run_external("fixture", "required")
            execute.assert_not_called()

    def test_external_process_failure_is_not_a_pass(self):
        with mock.patch.object(guard, "run", side_effect=["required: test\n", "required: test\n"]), \
             mock.patch.object(guard.subprocess, "run", return_value=subprocess.CompletedProcess([], 7, "failed")), \
             self.assertRaisesRegex(ValueError, "failed \\(7\\)"):
            guard.run_external("fixture", "required")

    def test_external_prerequisites_fail_before_compilation(self):
        for environment in [{}, {"PULSE_NATS_SERVER_BIN": "missing", "CATALYST_TEST_DATABASE_URL": "fixture"}]:
            with mock.patch.dict(guard.os.environ, environment, clear=True), \
                 mock.patch.object(guard, "run") as execute, self.assertRaisesRegex(ValueError, "PULSE_NATS_SERVER_BIN"):
                guard.check_external()
            execute.assert_not_called()
        with mock.patch.object(Path, "is_file", return_value=True), mock.patch.object(guard.os, "access", return_value=True):
            with self.assertRaisesRegex(ValueError, "CATALYST_TEST_DATABASE_URL"):
                guard.external_prerequisites({"PULSE_NATS_SERVER_BIN": "fixture"})
            self.assertEqual(guard.external_prerequisites({"PULSE_NATS_SERVER_BIN": "fixture", "CATALYST_TEST_DATABASE_URL": "fixture"}), "fixture")

    def test_external_target_selection_includes_lib_and_root_integration(self):
        import json
        artifacts = [
            {"reason": "compiler-artifact", "executable": "unit", "profile": {"test": True}, "target": {"name": "catalyst_comms", "kind": ["lib"]}},
            {"reason": "compiler-artifact", "executable": "socket", "profile": {"test": True}, "target": {"name": "archipelago_socket_bounds", "kind": ["test"]}},
            {"reason": "compiler-artifact", "executable": "config", "profile": {"test": True}, "target": {"name": "catalyst_config", "kind": ["lib"]}},
            {"reason": "compiler-artifact", "executable": "auth", "profile": {"test": True}, "target": {"name": "dcl_auth", "kind": ["lib"]}},
            {"reason": "compiler-artifact", "executable": "owner", "profile": {"test": True}, "target": {"name": "archipelago_control_extensions_websocket", "kind": ["test"]}},
            {"reason": "compiler-artifact", "executable": "content", "profile": {"test": True}, "target": {"name": "catalyst_content", "kind": ["lib"]}},
            {"reason": "compiler-artifact", "executable": "root", "profile": {"test": True}, "target": {"name": "catalyst", "kind": ["lib"]}},
            {"reason": "compiler-artifact", "executable": "root-bin", "profile": {"test": True}, "target": {"name": "catalyst", "kind": ["bin"]}},
            {"reason": "compiler-artifact", "executable": "retry", "profile": {"test": True}, "target": {"name": "retry_keeps_the_record_until_written", "kind": ["test"]}},
            {"reason": "compiler-artifact", "executable": "retry-lib", "profile": {"test": True}, "target": {"name": "retry_keeps_the_record_until_written", "kind": ["lib"]}},
            {"reason": "compiler-artifact", "executable": "engine", "profile": {"test": True}, "target": {"name": "stats_metrics_engine", "kind": ["test"]}},
            {"reason": "compiler-artifact", "executable": "engine-lib", "profile": {"test": True}, "target": {"name": "stats_metrics_engine", "kind": ["lib"]}},
        ]
        output = "\n".join(map(json.dumps, artifacts))
        required = {(entry["target"]["name"], entry["target"]["kind"][0]): [f"required_{index}"]
                    for index, entry in enumerate(artifacts) if entry["executable"] not in ("root-bin", "retry-lib", "engine-lib")}
        with mock.patch.object(guard, "MANDATORY_ARCHIPELAGO", required), \
             mock.patch.object(guard, "external_prerequisites", return_value="nats"), \
             mock.patch.object(guard.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, "nats fixture")), \
             mock.patch.object(guard, "run", return_value=output) as build, \
             mock.patch.object(guard, "check_executable") as mandatory, \
             mock.patch.object(guard, "run_external") as execute:
            guard.check_external()
        self.assertEqual([call.args[0] for call in execute.call_args_list], ["unit", "socket", "socket"])
        self.assertEqual([call.args[1] for call in execute.call_args_list], [entry[2] for entry in guard.EXTERNAL])
        self.assertEqual([call.args[0] for call in mandatory.call_args_list], list(required.values()))
        self.assertEqual([call.args[1] for call in mandatory.call_args_list], ["unit", "socket", "config", "auth", "owner", "content", "root", "retry", "engine"])
        self.assertEqual(mandatory.call_args_list[-1].kwargs, {"exact": True})
        self.assertEqual(build.call_args.args[0], ["cargo", "test", "--locked", *guard.SELECTION.split(), "--no-run", "--message-format=json-render-diagnostics"])
        for absent, kind in (("content", "lib"), ("root", "lib"), ("retry", "test"), ("engine", "test")):
            incomplete = "\n".join(json.dumps(entry) for entry in artifacts if entry["executable"] != absent)
            with self.subTest(absent=absent), mock.patch.object(guard, "MANDATORY_ARCHIPELAGO", required), \
                 mock.patch.object(guard, "external_prerequisites", return_value="nats"), \
                 mock.patch.object(guard.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, "nats fixture")), \
                 mock.patch.object(guard, "run", return_value=incomplete), \
                 mock.patch.object(guard, "check_executable"), mock.patch.object(guard, "run_external") as execute, \
                 self.assertRaisesRegex(ValueError, f"missing provisioned {kind} target"):
                guard.check_external()
            execute.assert_not_called()
        with mock.patch.object(guard, "external_prerequisites", return_value="nats"), \
             mock.patch.object(guard.subprocess, "run", return_value=subprocess.CompletedProcess([], 0, "nats fixture")), \
             mock.patch.object(guard, "run", return_value=""), self.assertRaisesRegex(ValueError, "missing provisioned"):
            guard.check_external()

    def test_runtime_rejects_missing_filtered_or_conditional_external_checks(self):
        script = (ROOT / "tooling/dev-test.sh").read_text()
        commands = [guard.EXTERNAL_COMMAND,
                    "python3 tooling/archipelago-fixtures.py check",
                    "python3 -m unittest discover -s tooling/tests -p 'test_archipelago_fixtures.py'"]
        guard.validate_runtime_script(script)
        for command in commands:
            for changed in ["", command + "\n      " + command, command + " || true",
                            "if false; then " + command + "; fi", "if false; then\n      " + command + "\n      fi"]:
                with self.subTest(command=command, changed=changed), self.assertRaises(ValueError):
                    guard.validate_runtime_script(script.replace(command, changed))

    def test_database_generator_tracks_root_across_shared_cargo_target(self):
        # Exercise Cargo's cached build-script dependency contract, not its source text.
        with tempfile.TemporaryDirectory(prefix="db-build-context-") as temporary:
            probe = Path(temporary)
            fixture = probe / "crate"
            (fixture / "src").mkdir(parents=True)
            shutil.copyfile(ROOT / "services/catalyst/db/build.rs", fixture / "build.rs")
            (fixture / "Cargo.toml").write_text(
                '[package]\nname="db-build-context-probe"\nversion="0.0.0"\nedition="2024"\n[workspace]\n',
                encoding="utf-8",
            )
            (fixture / "Cargo.lock").write_text(
                'version = 4\n\n[[package]]\nname = "db-build-context-probe"\nversion = "0.0.0"\n',
                encoding="utf-8",
            )
            (fixture / "src/main.rs").write_text(
                'fn main() { print!("{}\n-- PROBE MIGRATIONS --\n{}", '
                'include_str!(concat!(env!("OUT_DIR"), "/schema.sql")), '
                'include_str!(concat!(env!("OUT_DIR"), "/migrations.sql"))); }\n',
                encoding="utf-8",
            )
            release_definitions = [
                Path("schemas/releases/v3/41_social_voice_notifications.sql"),
                Path("schemas/releases/v3/42_social_private_voice_indexes.sql"),
            ]
            roots = []
            for marker in ("A", "B"):
                source = probe / ("root-" + marker)
                migrations = source / "schemas/migrations"
                migrations.mkdir(parents=True)
                (source / "schemas/00_context.sql").write_text(
                    f"-- SCHEMA_CONTEXT_{marker}\nCREATE TABLE context_{marker.lower()}(id integer);\n",
                    encoding="utf-8",
                )
                (migrations / "0001_context.sql").write_text(
                    f"-- MIGRATION_CONTEXT_{marker}\nALTER TABLE context_{marker.lower()} ADD COLUMN label text;\n",
                    encoding="utf-8",
                )
                (source / "schemas/45_after_release.sql").write_text(
                    f"-- AFTER_RELEASE_CONTEXT_{marker}\n", encoding="utf-8",
                )
                for relative in release_definitions:
                    destination = source / relative
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copyfile(ROOT / relative, destination)
                    with destination.open("ab") as body:
                        body.write(f"\n-- NESTED_CONTEXT_{marker}_{relative.stem}\n".encode())
                # build.rs also renders the release `schema migrate --release` embeds.
                release = Path("schemas/releases/v3-private-voice-pending.sql")
                shutil.copyfile(ROOT / release, source / release)
                roots.append((marker, source))
            source_state = {
                path: (path.read_bytes(), path.stat().st_mtime_ns)
                for directory in [fixture, *(source for _, source in roots)]
                for path in directory.rglob("*") if path.is_file()
            }
            target = probe / "shared-target"
            executable = target / "debug" / ("db-build-context-probe.exe" if os.name == "nt" else "db-build-context-probe")
            previous = None
            generated_paths = None
            for marker, source in roots:
                result = subprocess.run(
                    ["cargo", "build", "--locked", "--offline", "--manifest-path",
                     str(fixture / "Cargo.toml"), "--target-dir", str(target)],
                    cwd=fixture,
                    env={**os.environ, "DCL_ONE_ROOT": str(source)},
                    capture_output=True, text=True, timeout=60,
                )
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                schemas = list((target / "debug/build").glob("db-build-context-probe-*/out/schema.sql"))
                self.assertEqual(len(schemas), 1)
                paths = (schemas[0], schemas[0].with_name("migrations.sql"))
                if generated_paths is not None:
                    self.assertEqual(paths, generated_paths, "one shared Cargo output directory is reused")
                generated_paths = paths
                generated = tuple(path.read_bytes() for path in paths)
                other = "B" if marker == "A" else "A"
                for kind, content in zip(("SCHEMA", "MIGRATION"), generated):
                    self.assertIn(f"{kind}_CONTEXT_{marker}".encode(), content)
                    self.assertNotIn(f"{kind}_CONTEXT_{other}".encode(), content)
                for relative in release_definitions:
                    canonical = (ROOT / relative).read_bytes().rstrip(b"\n")
                    self.assertEqual(generated[0].count(canonical), 1, "canonical release DDL is enrolled once")
                    self.assertIn(f"NESTED_CONTEXT_{marker}_{relative.stem}".encode(), generated[0])
                    self.assertNotIn(f"NESTED_CONTEXT_{other}_{relative.stem}".encode(), generated[0])
                ordered = [Path("schemas/00_context.sql"), *release_definitions, Path("schemas/45_after_release.sql")]
                positions = [generated[0].index(f"-- ════ {path.as_posix()} ════".encode()) for path in ordered]
                self.assertEqual(positions, sorted(positions), "nested release DDL follows filename order")
                self.assertEqual(
                    {path: (path.read_bytes(), path.stat().st_mtime_ns) for path in source_state},
                    source_state, "changing DCL_ONE_ROOT rebuilds without touching source files",
                )
                embedded = subprocess.run([str(executable)], capture_output=True, timeout=10, check=True).stdout
                self.assertEqual(embedded, generated[0] + b"\n-- PROBE MIGRATIONS --\n" + generated[1])
                if previous is not None:
                    self.assertNotEqual(generated[0], previous[0])
                    self.assertNotEqual(generated[1], previous[1])
                previous = generated

    def test_good(self):
        guard.verify(["required"], "required: test\nother: test\n", "other: test\n", "fixture")

    def test_known_bad_inventories(self):
        for normal, ignored in [("", ""), ("renamed: test\n", ""),
                                ("required: test\n", "required: test\n"),
                                ("required: test\nrequired: test\n", ""),
                                ("required: test\nmalformed\n", "")]:
            with self.subTest(normal=normal, ignored=ignored), self.assertRaises(ValueError):
                guard.verify(["required"], normal, ignored, "bad fixture")
        for required in [[], ["required", "required"]]:
            with self.assertRaises(ValueError):
                guard.verify(required, "required: test\n", "", "bad manifest")

    def test_active_broad_invocation(self):
        guard.validate_invocation(guard.BROAD + "\n", {})
        for invocation in [guard.BROAD.replace("--workspace", "-p catalyst-comms"),
                           guard.BROAD + " --exclude catalyst", guard.BROAD + " case_name",
                           guard.BROAD + " -- --skip required", guard.BROAD + " -- --ignored",
                           guard.BROAD.replace(" --features catalyst/test-support", ""),
                           guard.BROAD.replace("catalyst-test.sh", "catalyst-test.sh --test unrelated")]:
            with self.subTest(invocation=invocation), self.assertRaises(ValueError):
                guard.validate_invocation(invocation, {})
        with self.assertRaises(ValueError):
            guard.validate_invocation(guard.BROAD, {"MAKEFLAGS": "NODE_CRATES=-p catalyst-comms"})

    def test_catalyst_inventory_compiles_the_runtime_selection(self):
        runtime = ["cargo", "test", "--locked", *guard.SELECTION.split()]
        manifest = '{"catalyst": {"required": ["case"]}, "unix": {}}'
        with (
            mock.patch.object(guard, "run", side_effect=[guard.BROAD, ""]) as execute,
            mock.patch.object(guard.sys, "argv", ["dev-regressions.py"]),
            mock.patch.dict(guard.os.environ, clear=True),
            mock.patch.object(Path, "read_text", side_effect=[manifest, "", ""]),
            mock.patch.object(guard, "verify_resolution"),
            mock.patch.object(guard, "validate_runtime_script"),
            mock.patch.object(guard, "validate_test_script"),
            self.assertRaisesRegex(ValueError, "missing required target: required"),
        ):
            guard.main()
        self.assertEqual(execute.call_args_list, [
            mock.call(["make", "--no-print-directory", "-n", "catalyst-test"]),
            mock.call([*runtime, "--no-run", "--message-format=json-render-diagnostics"], ROOT / "services/catalyst"),
        ])

    def test_inventory_distinguishes_same_named_targets_by_kind(self):
        import json
        unit = {"reason": "compiler-artifact", "executable": "unit-test", "profile": {"test": True},
                "target": {"name": "required", "kind": ["lib"]}}
        integration = {**unit, "executable": "integration-test", "target": {"name": "required", "kind": ["test"]}}
        output = "\n".join(map(json.dumps, [unit, integration]))
        self.assertEqual(guard.artifact_targets(output, {"required"}, kind="test"), {"required": "integration-test"})
        self.assertEqual(guard.artifact_targets(json.dumps(unit), {"required"}, kind="test"), {})

    def test_existing_bevy_artifact_inventory_requires_native_comms_target(self):
        import json
        artifact = {"reason": "compiler-artifact", "executable": "comms-test", "profile": {"test": True}, "target": {"name": "comms"}}
        output = json.dumps(artifact) + "\n"
        self.assertEqual(guard.artifact_targets(output, {"comms"}), {"comms": "comms-test"})
        self.assertEqual(guard.artifact_targets(output, {"removed"}), {})
        with self.assertRaisesRegex(ValueError, "duplicate executable"):
            guard.artifact_targets(output + output, {"comms"})
        with self.assertRaisesRegex(ValueError, "missing required target"):
            guard.verify_targets({"comms": ["pulse::native::tests::native_frame_mapping_preserves_channels_and_explicit_throttle_bypass"]}, {})

    def test_native_external_vendor_case_stays_ignored_by_default(self):
        with mock.patch.object(guard, "run", side_effect=["required: test\nexternal: test\n", "external: test\n"]):
            guard.check_executable(["required"], "fake", "vendor", ["external"])
        with mock.patch.object(guard, "run", side_effect=["required: test\nexternal: test\n", ""]):
            with self.assertRaisesRegex(ValueError, "remain ignored by default"):
                guard.check_executable(["required"], "fake", "vendor", ["external"])

    def test_actual_runtime_script_rejects_removed_duplicate_and_filtered_bodies(self):
        script = (ROOT / "tooling/dev-test.sh").read_text()
        guard.validate_runtime_script(script)
        for bad in [script.replace(guard.VENDOR, guard.VENDOR + " -- --ignored"),
                    script.replace(guard.VENDOR, ""), script + "\n" + guard.VENDOR,
                    script.replace('catalyst-test ;;', 'catalyst-test NODE_CRATES=-p catalyst-comms ;;'),
                    script.replace('make -C "$FOUNDATION_DEV_ROOT" catalyst-test ;;', ''),
                    script + '\nmake -C "$FOUNDATION_DEV_ROOT" catalyst-test']:
            with self.subTest(bad=bad[-100:]), self.assertRaises(ValueError):
                guard.validate_runtime_script(bad)


    def test_the_shards_of_the_actual_test_script_run_every_binary_once(self):
        script = (ROOT / "tooling/catalyst-test.sh").read_text()
        select = script[script.index('shard="${CATALYST_TEST_SHARD:-1/1}"'):script.index('cores=')]
        names = [f"/target/debug/deps/{name}" for name in "gfedcba"]

        def run(value):
            with tempfile.TemporaryDirectory() as logs:
                Path(logs, "binaries").write_text("".join(name + "\n" for name in names))
                env = {**os.environ, "logs": logs}
                env.pop("CATALYST_TEST_SHARD", None)
                if value is not None:
                    env["CATALYST_TEST_SHARD"] = value
                result = subprocess.run(["bash", "-euo", "pipefail", "-c", select], env=env,
                                        capture_output=True, text=True)
                return result.returncode, Path(logs, "shard").read_text().split() if result.returncode == 0 else []

        self.assertEqual(run(None), (0, sorted(names)))
        for total in (1, 2, 3, 4, 7, 9):
            shards = [run(f"{index}/{total}") for index in range(1, total + 1)]
            self.assertEqual([code for code, _ in shards], [0] * total)
            self.assertEqual(sorted(name for _, shard in shards for name in shard), sorted(names))
        for bad in ("3", "0/3", "4/3", "1/0", "a/b", "1/3/3", "/3", "2/3x", " 2/3", "02/03", "-1/3"):
            with self.subTest(bad=bad):
                self.assertEqual(run(bad)[0], 1)

    def test_actual_test_script_rejects_filters_skipped_binaries_and_early_exit(self):
        script = (ROOT / "tooling/catalyst-test.sh").read_text()
        guard.validate_test_script(script)
        guard.validate_test_script(script.replace("# Run the node", "# Runs the node"))
        for bad in [script.replace("#!/usr/bin/env bash", "#!/usr/bin/env true"),
                    script.replace("set -euo pipefail", "set -uo pipefail"),
                    script.replace('"$@" --no-run', '"$@" --test unrelated --no-run'),
                    script.replace('"$1" > "$log"', '"$1" --skip required > "$log"'),
                    script.replace('run "$jobs" < "$logs/shard"', 'run "$jobs" < <(head -n 1 "$logs/shard")'),
                    script.replace("'NR % n == i % n'", "'NR == 1'"),
                    script.replace('${CATALYST_TEST_SHARD:-1/1}', '${CATALYST_TEST_SHARD:-1/2}'),
                    script.replace('[[ "$shard" =~ ^[1-9][0-9]*/[1-9][0-9]*$ ]] && ', ''),
                    script.replace('< "$logs/others"', '< <(head -n 1 "$logs/others")'),
                    script.replace('run "$socket_jobs" < "$logs/sockets"\n', ''),
                    script.replace('print(artifact["executable"])', 'print(artifact["executable"]) if "comms" not in artifact["executable"] else None'),
                    script.replace('cargo test --locked "$@" --doc\n', ''),
                    script.replace("cores=", "exit 0\ncores=")]:
            with self.subTest(bad=bad[-100:]), self.assertRaisesRegex(ValueError, "catalyst-test.sh"):
                guard.validate_test_script(bad)

    def test_vendor_examples_compile_gate_cannot_be_removed_filtered_or_conditional(self):
        script = (ROOT / "tooling/dev-test.sh").read_text()
        command = "cargo check --locked --manifest-path library/vendor/rusty_enet/Cargo.toml --all-features --all-targets"
        for bad in [script.replace(command, ""), script + "\n" + command,
                    script.replace(command, command.replace("--all-features", "")),
                    script.replace(command, command + " || true"),
                    script.replace(command, "if false; then " + command + "; fi")]:
            with self.subTest(bad=bad[-100:]), self.assertRaisesRegex(ValueError, "vendor compile"):
                guard.validate_runtime_script(bad)

    def test_runtime_make_overrides_cannot_hide_in_continuations(self):
        script = (ROOT / "tooling/dev-test.sh").read_text()
        invocation = 'make -C "$FOUNDATION_DEV_ROOT" catalyst-test ;;'
        for variable in ["MAKEFLAGS", "MFLAGS", "MAKEOVERRIDES"]:
            for prefix in [f'{variable}="NODE_CRATES=-p catalyst-comms" ',
                           f'{variable}="NODE_CRATES=-p catalyst-comms" ' + "\\\n      "]:
                with self.subTest(variable=variable, prefix=prefix), self.assertRaises(ValueError):
                    guard.validate_runtime_script(script.replace(invocation, prefix + invocation))

    def test_bevy_runtime_is_in_the_enabled_expected_job_and_step(self):
        workflow = (ROOT / ".github/workflows/bevy-explorer-ci.yml").read_text()
        guard.validate_bevy_workflow(workflow)
        command = "          " + guard.BEVY + "\n"
        for bad in [workflow.replace("    name: Test Suite", "    if: false\n    name: Test Suite"),
                    workflow.replace("  test:\n", "  renamed-test:\n"),
                    workflow.replace(command, ""), workflow + command,
                    workflow.replace(command, "        if: false\n" + command),
                    workflow.replace(command, "          echo disabled\n")]:
            with self.subTest(bad=bad[-100:]), self.assertRaises(ValueError):
                guard.validate_bevy_workflow(bad)

    def test_shared_vendor_resolution_rejects_fork_and_registry_lock(self):
        guard.verify_resolution()
        with tempfile.TemporaryDirectory(prefix="enet-resolution-") as tmp:
            root = Path(tmp)
            for relative in ["services/catalyst/Cargo.toml", "services/catalyst/Cargo.lock",
                             "services/catalyst/comms/Cargo.toml", "rendering/bevy-explorer/source/Cargo.toml",
                             "rendering/bevy-explorer/source/Cargo.lock", "rendering/bevy-explorer/source/crates/comms/Cargo.toml"]:
                destination = root / relative
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(ROOT / relative, destination)
            guard.verify_resolution(root)
            manifest = root / "rendering/bevy-explorer/source/Cargo.toml"
            original = manifest.read_text()
            manifest.write_text(original.replace('../../../library/vendor/rusty_enet', '../../../library/vendor/client_fork'))
            with self.assertRaisesRegex(ValueError, "single pinned vendor path"):
                guard.verify_resolution(root)
            manifest.write_text(original)
            lock = root / "services/catalyst/Cargo.lock"
            lock.write_text(lock.read_text().replace('name = "rusty_enet"\nversion = "0.4.0"', 'name = "rusty_enet"\nversion = "0.4.0"\nsource = "registry+https://example.invalid/index"'))
            with self.assertRaisesRegex(ValueError, "exactly one local"):
                guard.verify_resolution(root)

    def test_missing_or_renamed_target(self):
        # The executable map, not source attribute scanning, defines available targets.
        with unittest.mock.patch.object(guard, "run", return_value='{"reason":"build-finished","success":true}\n'):
            targets = guard.compiled_targets([], ROOT)
            with self.assertRaisesRegex(ValueError, "missing required target"):
                guard.verify_targets({"removed": ["required"]}, targets)
        with self.assertRaisesRegex(ValueError, "missing required target"):
            guard.verify_targets({"renamed": ["required"]}, {"old": "unused"})


class DevScriptTests(unittest.TestCase):
    def run_fixture(self, scenario="good", ci="true", platform="Darwin", kernel="65536"):
        bash = shutil.which("bash")
        if os.name == "nt":
            bash = str(Path(os.environ.get("ProgramFiles", "C:/Program Files")) / "Git/bin/bash.exe")
        if not bash or not Path(bash).exists():
            self.skipTest("bash unavailable")
        with tempfile.TemporaryDirectory(prefix="dev-regression-") as tmp:
            root = Path(tmp)
            tools = root / "bin"
            tools.mkdir()
            log = root / "calls"
            def executable(name, body):
                path = tools / name
                path.write_text("#!/bin/bash\nset -eu\n" + body + "\n", newline="\n")
                path.chmod(0o755)
            executable("foundation-dev", r'''echo "foundation:$*" >> "$CALLS"
if [ "$1" = env ]; then
  printf 'export FOUNDATION_DEV_ROOT=%q\n' "$FIXTURE_ROOT"
  if [ "$SCENARIO" != unset-pg ]; then
    printf 'export FOUNDATION_DEV_TEST_DATABASE_URL=%q\n' "$FIXTURE_PG"
  fi
fi''')
            executable("nats-server", 'echo "nats:$*" >> "$CALLS"; [ "$SCENARIO" != bad-nats ]; echo "nats-server fixture"')
            # Exercise the macOS resource branch on every host without depending
            # on /usr/sbin or changing the machine's actual resource limits.
            executable("uname", '[ "$*" = -s ]; echo "$FIXTURE_PLATFORM"')
            executable("sysctl", '''[ "$*" = '-n kern.maxfilesperproc' ]
echo "resource:sysctl $*" >> "$CALLS"
echo "$FIXTURE_KERNEL"''')
            executable("python3", '''echo "python:$*" >> "$CALLS"
[ "$SCENARIO" != guard-fail ]
if [ "$SCENARIO" = external-fail ] && [ "$*" = "tooling/dev-regressions.py --archipelago-external" ]; then exit 8; fi''')
            executable("cargo", 'echo "cargo:$*" >> "$CALLS"; if [ "$1" = check ] && [ "$SCENARIO" = check-fail ]; then exit 7; fi')
            executable("make", r'''echo "make:$*" >> "$CALLS"
[ "$PULSE_NATS_SERVER_BIN" = "$ARCHIPELAGO_NATS_SERVER_BIN" ]
[ "$PULSE_NATS_SERVER_BIN" = "$(command -v nats-server)" ]
[ "$CATALYST_TEST_DATABASE_URL" = "$FIXTURE_PG" ]
[ "${GITHUB_ACTIONS-unset}" = "$EXPECTED_CI" ]
echo verified-exports >> "$CALLS"''')
            if scenario == "missing-nats":
                (tools / "nats-server").unlink()
            environment = dict(os.environ, CALLS=log.as_posix(), FIXTURE_ROOT=root.as_posix(),
                               FIXTURE_PG=" " if scenario == "blank-pg" else "postgres://fixture",
                               SCENARIO=scenario, EXPECTED_CI=ci if ci is not None else "unset",
                               FIXTURE_PLATFORM=platform, FIXTURE_KERNEL=kernel)
            environment.pop("FOUNDATION_DEV_TEST_DATABASE_URL", None)
            if scenario == "missing-pg":
                environment["FIXTURE_PG"] = ""
            if ci is None:
                environment.pop("GITHUB_ACTIONS", None)
            else:
                environment["GITHUB_ACTIONS"] = ci
            # Git Bash converts Windows paths passed through PATH differently; set it in bash.
            command = 'export PATH="$1:/usr/bin:/bin"; bash "$2" rust'
            if os.name == "nt":
                command = 'export PATH="$(cygpath -u "$1"):/usr/bin:/bin"; bash "$2" rust'
            command = '''ulimit() {
  case "$*" in -Sn) echo 65536 ;; -Hn) echo unlimited ;; *) return 77 ;; esac
}
export -f ulimit
''' + command
            result = subprocess.run([bash, "-c", command, "fixture", tools.as_posix(),
                                     (ROOT / "tooling/dev-test.sh").as_posix()], env=environment,
                                    text=True, capture_output=True)
            return result, log.read_text() if log.exists() else ""

    def test_macos_resource_probe_uses_controlled_tools_on_the_restricted_path(self):
        result, calls = self.run_fixture()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(calls.count("resource:sysctl -n kern.maxfilesperproc"), 1, calls)
        self.assertIn("descriptors kern.maxfilesperproc=65536 required=65536", result.stdout)
        self.assertIn("verified-exports", calls)
        self.assertEqual(calls.count("foundation:down"), 1, calls)

    def test_macos_inadequate_resource_stops_before_guards_and_cleans_up(self):
        result, calls = self.run_fixture(kernel="10240")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("kernel per-process limit 10240 is below 65536", result.stderr)
        self.assertIn("resource:sysctl -n kern.maxfilesperproc", calls)
        for command in ("python:", "cargo:", "make:"):
            self.assertNotIn(command, calls)
        self.assertEqual(calls.count("foundation:down"), 1, calls)

    def test_good_exports_cleanup_and_one_broad_run(self):
        for ci in ["true", "false", None]:
            result, calls = self.run_fixture(ci=ci)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(calls.count("make:"), 1, calls)
            self.assertIn("verified-exports", calls)
            self.assertEqual(calls.count("foundation:down"), 1, calls)
            self.assertIn("foundation:up --postgres-only", calls)
            self.assertIn("foundation:wait --postgres-only", calls)
            self.assertEqual(calls.count("python:tooling/dev-regressions.py --archipelago-external"), 1, calls)
            self.assertEqual(calls.count("python:tooling/archipelago-fixtures.py check"), 1, calls)

    def test_external_failure_stops_before_broad_runtime(self):
        result, calls = self.run_fixture("external-fail")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("python:tooling/dev-regressions.py --archipelago-external", calls)
        self.assertNotIn("make:", calls)
        self.assertEqual(calls.count("foundation:down"), 1, calls)

    def test_vendor_compile_failure_stops_before_runtime(self):
        result, calls = self.run_fixture("check-fail")
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("cargo:check --locked --manifest-path library/vendor/rusty_enet/Cargo.toml --all-features --all-targets", calls)
        self.assertNotIn("cargo:test", calls)
        self.assertNotIn("make:", calls)
        self.assertEqual(calls.count("foundation:down"), 1, calls)

    def test_missing_bad_prerequisites_and_guard_never_run_bodies(self):
        for scenario in ["missing-nats", "bad-nats", "missing-pg", "unset-pg", "blank-pg", "guard-fail"]:
            with self.subTest(scenario=scenario):
                result, calls = self.run_fixture(scenario)
                self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
                if scenario in ["missing-pg", "unset-pg", "blank-pg"]:
                    self.assertIn("FOUNDATION_DEV_TEST_DATABASE_URL must name disposable PostgreSQL", result.stderr)
                    self.assertNotIn("unbound variable", result.stderr)
                self.assertNotIn("make:", calls)
                self.assertNotIn("cargo:", calls)
                self.assertEqual(calls.count("foundation:down"), 1, calls)


if __name__ == "__main__":
    unittest.main()
