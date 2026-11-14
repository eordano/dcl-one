#!/usr/bin/env python3
"""Check mandatory inventory and explicitly provisioned external libtests."""
import argparse
from collections import Counter
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tomllib

ROOT = Path(__file__).resolve().parent.parent
SELECTION = "--workspace --exclude dcl-scene-cli --exclude dcl-scene-authoring --exclude dcl-scene-build --exclude dcl-preview-tunnel --exclude dcl-preview-tunnel-protocol --features catalyst/test-support"
BROAD = "tooling/catalyst-test.sh " + SELECTION
VENDOR = "cargo test --locked --manifest-path library/vendor/rusty_enet/Cargo.toml --lib"
VENDOR_CHECK = "cargo check --locked --manifest-path library/vendor/rusty_enet/Cargo.toml --all-features --all-targets"
BEVY = "cargo test --all --profile ci-test --locked -- --test-threads=1"
EXTERNAL_COMMAND = "python3 tooling/dev-regressions.py --archipelago-external"
EXTERNAL = [
    ("catalyst_comms", "lib", "pulse::cluster::nats::statistics_tests::statistics_live_broker_late_join_and_reconnect_recover_complete_state"),
    ("archipelago_socket_bounds", "test", "orderly_shutdown_drains_authenticated_disconnect_to_live_pulse"),
    ("archipelago_socket_bounds", "test", "idle_transport_retires_no_pong_but_preserves_pong_and_data_activity"),
]
# The isolated V8 process must keep exactly one initializer. Ordinary target
# inventories remain subsets so unrelated tests can grow without a manifest edit.
EXACT_ARCHIPELAGO = {("stats_metrics_engine", "test")}
MANDATORY_ARCHIPELAGO = {
    ("catalyst_comms", "lib"): [
        "archipelago::ws::legacy_decode::tests::locked_buffer_decoder_differential_corpus",
        "stats::tests::http_tests::source_handler_fixtures_match_pulse_state_and_all_http_aliases",
        "stats::tests::every_contiguous_change_is_visible_at_callback_completion_and_gaps_require_recovery",
        "archipelago::observations::tests::queue_saturation_and_unavailable_shutdown_count_every_lost_observation",
        "archipelago::tests::shutdown_waits_for_handler_observations_after_writers_are_gone",
        "pulse::cluster::nats::statistics_tests::statistics_publisher_recovers_during_single_item_refill",
        "pulse::cluster::nats::statistics_tests::statistics_publisher_preserves_checkpoint_prefix_during_continuous_backlog",
        "pulse::statistics::tests::overflow_and_failed_transfer_discard_pending_checkpoint_for_complete_recovery",
        "archipelago::ws::io::tests::legacy_heartbeat_encodings_receive_identical_rate_accounting",
        "stats::tests::http_tests::root_bypasses_non_candidates_and_records_exact_selection_paths",
        "stats::tests::http_tests::root_collision_suppresses_claim_and_matcher_fault_is_not_a_nonmatch",
        "stats::tests::http_tests::configured_metrics_auth_scrapes_and_actual_lifecycle_values_are_used",
        "stats::tests::http_tests::metrics_observe_route_templates_skip_probes_and_keep_request_semantics",
        "stats::tests::http_tests::standalone_normalizes_dot_segments_without_collapsing_slashes",
        "stats::tests::http_tests::standalone_selection_duration_includes_match_and_nonmatch",
        "stats::tests::http_tests::claimed_root_duration_includes_raw_and_normalized_selection",
        "stats::tests::http_tests::normalized_selection_failure_keeps_raw_selection_duration",
        "archipelago::auth::tests::contract_challenge_is_verified_by_configured_validator",
        "archipelago::auth::tests::pending_contract_verifications_release_lock_and_commit_only_current_issuance_once",
        "archipelago::auth::tests::discarded_or_cancelled_contract_verification_cannot_consume_a_challenge",
        "archipelago::auth::tests::challenge_expiring_during_rpc_is_not_redeemed",
        "archipelago::auth::tests::contract_refusal_missing_provider_and_peer_trust_fail_closed",
        "archipelago::auth::tests::delegated_and_mixed_contract_chains_preserve_binding_expiry_and_ten_link_limit",
        "archipelago::auth::tests::a_cached_delegation_cannot_authorize_a_new_challenge_after_revocation",
        "archipelago::ws::tests::dedup_preserves_fractional_infinite_and_number_subtraction_boundaries",
        "archipelago::handlers::tests::connector_metrics_native_segments_preserve_separators_and_bytes",
        "archipelago::handlers::tests::connector_metrics_native_segment_limit_terminates_registration_and_traversal",
        "archipelago::handlers::tests::connector_metrics_http_selection_preserves_native_precedence_and_methods",
        "archipelago::handlers::tests::connector_metrics_observes_selected_handlers_and_skips_rejected_scrapes",
        "archipelago::handlers::tests::connector_metrics_socket_yield_uses_http_selection_and_one_observation",
        "archipelago::handlers::tests::connector_metrics_socket_claim_keeps_non_key_header_rejections",
        "archipelago::handlers::tests::connector_metrics_socket_upgrade_bypasses_colliding_scrapes_and_challenges",
        "stats::runtime::tests::midnight_reset_resolves_gaps_with_pre_gap_offset_and_ambiguity_with_earlier_instant",
        "stats::runtime::tests::midnight_reset_bounds_unresolved_timezone_lookups_without_claiming_a_gap",
        "stats::runtime::tests::scrape_reads_clocks_after_render_and_uses_fresh_reset_date",
        "stats::runtime::tests::scrape_clock_history_keeps_strict_boundary_and_feed_baseline_with_one_delayed_reset",
        "stats::runtime::tests::scrape_without_rotation_does_not_read_reset_clocks",
        "stats::runtime::tests::rejected_scrape_keeps_reset_boundary_and_success_is_observed_after_old_body",
        "stats::tests::http_tests::selected_sql_metadata_order_reaches_hot_scenes_http",
        "stats::tests::content_tests::selected_sql_content_applies_shared_denylist_before_top_100",
        "archipelago::tests::mounted_hot_scenes_share_local_content_policy_for_get_and_head",
        "archipelago::ws::io::bounds_tests::peer_close_cancels_production_readiness_slot_and_partial_frame_without_drain",
        "archipelago::ws::io::bounds_tests::clean_peer_close_discards_queue_and_preempts_kick_restart_without_start_send",
        "archipelago::ws::io::bounds_tests::stalled_clean_control_flush_releases_permit_once_at_existing_deadline",
        "archipelago::ws::io::bounds_tests::peer_close_wakes_idle_production_writer_without_a_lost_notification",
        "archipelago::ws::io::bounds_tests::peer_close_during_production_authority_wait_never_begins_the_held_credential",
        "archipelago::ws::io::bounds_tests::a_completed_send_can_echo_but_failed_or_timed_out_sends_never_salvage",
        "archipelago::auth::tests::opaque_issuance_isolates_equal_nonces_and_stale_cleanup",
        "archipelago::auth::tests::same_wallet_challenges_survive_mismatch_and_invalid_signature",
        "archipelago::auth::tests::historical_lookup_and_call_preserve_opaque_issuance_consumption_reissue_and_expiry",
        "protocol::bus::control_position::tests::selected_intent_vectors_use_exact_closed_bytes_and_preserve_signed_zero",
        "protocol::bus::cluster::tests::complete_selected_reply_boundaries_match_canonical_bytes_without_truncating_pending",
        "pulse::cluster::control::tests::versions_share_wallet_capacity_and_unproved_mixed_heads_never_select",
        "pulse::cluster::control::tests::duplicate_does_not_refresh_live_expiry_for_either_version",
        "pulse::cluster::control::tests::explicit_withdrawal_is_terminal_but_live_expiry_can_resume",
        "pulse::cluster::control::tests::same_epoch_session_is_fixed",
        "pulse::cluster::control::tests::native_aba_preserves_cleanup_across_terminal_and_expired_control_heads",
        "pulse::cluster::feed::current_tests::native_arrival_departure_without_pass_or_lookup_invalidates_all_legacy_variants",
        "pulse::cluster::feed::current_tests::native_current_peer_and_exhausted_legacy_revalidation_never_reauthorize_old_stamps",
        "pulse::cluster::tests::native_reservation_survives_unusable_position_and_different_legacy_session",
        "pulse::cluster::tests::native_reservation_outweighs_legacy_owner_and_terminal_tombstone",
        "gatekeeper::admission::cluster::fence::tests::selected_wire_vectors_preserve_exact_u64_json_and_refuse_unknown_authority",
        "gatekeeper::admission::cluster::fence::tests::old_unknown_and_cleanup_only_snapshots_cannot_create_admission_stamps",
        "gatekeeper::admission::island_refresh::tests::selected_u64_domains_refresh_through_real_nats_and_persisted_assignment_documents",
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
        "entity::tests::deployment_metadata_retains_order_and_exact_scalars",
        "validator::jsjson::tests::strict_serialization_retains_exact_integer_boundaries",
        "sync_deployer::tests::ordered_projection_keeps_local_and_sync_structural_parsing_distinct",
        "sync_deployer::tests::flushed_batches_preserve_metadata_order_and_scalars",
        "sync_deployer::tests::metadata_guard_rejects_nul_even_on_conflict_and_rolls_back_mixed_batches",
        "sync_deployer::tests::metadata_unicode_refusals_distinguish_json_input_from_scene_index",
        "sync_deployer::tests::metadata_and_retry_cleanup_roll_back_when_content_insert_fails",
        "sync_deployer::tests::cancelling_a_flush_after_insert_keeps_retry_and_ordered_replay",
        "sync_deployer::tests::stalled_flush_backlog_retains_order_with_bounded_batches",
        "write_path_tests::a_signed_scene_deploys_serves_and_unpublishes_end_to_end",
        "write_path_tests::a_signed_positional_scene_keeps_metadata_and_immutable_bytes",
        "partial_deployments::tests::a_scene_uploaded_in_batches_is_published_by_its_last_batch",
        "validator::jsjson::tests::strict_deserialization_preserves_literal_number_marker",
        "validator::jsjson::tests::strict_nested_markers_duplicates_and_index_keys_keep_json_semantics",
        "validator::jsjson::tests::strict_parser_retains_existing_overflow_and_surrogate_refusals",
        "validator::jsjson::tests::strict_wide_objects_keep_first_position_with_last_decoded_value",
        "validator::item_hash::tests::item_hash_explicitly_retains_legacy_number_marker_interpretation",
        "validator::third_party::tests::third_party_hash_explicitly_retains_legacy_number_marker_interpretation",
        "tests::ordinary_content_routes_keep_value_metadata_projection",
        "write_path_tests::a_profile_with_nul_metadata_preserves_the_write_error_and_active_profile",
    ],
    ("catalyst_storage", "lib"): [
        "content::tests::store_file_links_a_spooled_file_and_leaves_the_source",
    ],
    ("catalyst_worlds", "lib"): [
        "core::tests::partial_uploads::two_replicas_completing_one_entity_publish_it_once",
        "core::tests::partial_uploads::a_completion_consumed_after_upsert_replays_before_reservation",
        "core::tests::partial_uploads::a_reservation_disappearance_without_a_receipt_is_refused",
        "core::tests::partial_uploads::a_reservation_disappearance_with_an_expired_receipt_is_refused",
        "core::tests::partial_uploads::a_reservation_disappearance_with_another_signers_receipt_is_refused",
    ],
    ("catalyst", "lib"): [
        "presence::tests::module_sources::validated_historical_sync_order_reaches_hot_scenes_http",
        "presence::tests::module_sources::archive_reads_selected_sql_metadata_and_omits_non_string_fields",
        "metrics_matcher::tests::canceled_queued_and_running_matchers_keep_single_submission",
        "metrics_matcher::tests::panicked_work_returns_error_and_releases_admission",
        "presence::tests::module_sources::selected_sql_archive_observes_shared_content_policy",
    ],
    ("stats_metrics_engine", "test"): [
        "configured_stats_engine_and_http_contract",
    ],
    ("stats_metrics_startup", "test"): [
        "stats_compilation_is_serve_only_and_follows_cli_refusals",
    ],
    ("archipelago_hot_scenes", "test"): [
        "active_content_updates_are_visible_without_a_tile_cache",
    ],
    ("retry_keeps_the_record_until_written", "test"): [
        "a_retried_deployment_keeps_its_record_until_its_write_commits",
    ],
    ("archipelago_socket_bounds", "test"): [
        "repeated_and_unknown_wire_fields_reach_the_socket_statistics_boundary",
        "unknown_auth_field_does_not_replace_a_valid_legacy_signature",
        "sustained_excess_heartbeats_close_the_real_socket_with_1008",
        "contract_socket_auth_uses_configured_fresh_validator_and_admits_derived_session_once",
        "pending_contract_auth_is_cancelled_by_deadline_restart_close_and_invalid_stage_data",
        "clean_peer_close_echoes_exact_reason_and_ends_tcp_at_each_application_stage",
        "server_first_close_does_not_send_a_second_close_for_the_peer_reply",
        "verified_principal_overrides_claim_for_direct_and_delegated_eoa",
        "verified_principal_overrides_claim_for_direct_and_delegated_contract",
        "already_connected_observes_claim_without_authorizing_it",
        "claimed_deny_and_verified_deny_or_ban_fail_before_admission",
        "local_registration_follows_successful_completion_and_preserves_other_devices",
        "local_reconnect_replaces_only_the_verified_device_and_moves_it_newest",
        "another_sockets_nonce_cannot_redeem_or_discard_the_live_issuance",
        "verified_policy_waits_cancel_on_close_restart_and_deadline",
        "contract_fallback_accepts_verified_principal_through_configured_rpc",
        "every_fallback_rpc_phase_cancels_without_admission_or_a_following_request",
    ],
    ("catalyst_config", "lib"): [
        "worlds::tests::island_dedup_uses_ecmascript_numeric_prefixes",
    ],
    ("dcl_auth", "lib"): [
        "validation_cache::tests::fresh_validation_bypasses_nested_caches_after_revocation_without_changing_cached_calls",
        "rpc_validator::tests::verification_selector_preserves_order_shortcuts_equality_and_fractional_time",
        "rpc_validator::tests::complete_latest_pair_precedes_lookup_and_complete_history_pair",
        "rpc_validator::tests::selector_errors_stop_before_historical_signature_and_ordinary_calls_stay_latest",
        "rpc_validator::tests::every_contract_link_uses_same_start_time_and_its_own_fresh_lookup",
        "rpc_validator::tests::fresh_fallback_bypasses_nested_success_cache_without_changing_ordinary_callers",
        "rpc_validator::tests::late_bad_link_wrong_payload_and_malformed_signature_never_accept_partial_history",
        "rpc_validator::tests::repeated_nan_candidate_rejects_the_pure_recursive_failure",
        "rpc_validator::tests::successful_cached_refinement_yields_and_can_be_dropped",
        "rpc_validator::tests::refinement_read_cancellation_drops_work_without_a_following_rpc",
        "rpc_validator::tests::controlled_rpc_workload_measures_latest_and_fallback_at_the_verification_envelope",
        "rpc_validator::tests::workload_transport_diagnostics_keep_io_causes_without_endpoint_data",
        "rpc_validator::tests::selector_degenerate_and_provider_number_cases_fail_closed_or_preserve_selection",
    ],
    ("archipelago_control_extensions_websocket", "test"): [
        "contract_authentication_preserves_configured_owner_fences",
        "fenced_completed_assignment_peer_close_echoes_then_releases_exact_owner",
        "configured_verified_principal_owns_sql_and_preserves_exact_release",
        "configured_claim_and_principal_policy_denials_never_claim_sql",
        "configured_distinct_sessions_share_the_realm_owner_row",
        "configured_close_during_claim_can_commit_before_receive",
        "historical_contract_authentication_keeps_verified_principal_and_configured_fences",
    ],
    ("archipelago_ws_island_dedup", "test"): [
        "a_later_authentication_completion_replaces_the_same_sessions_socket",
    ],
    ("pulse_cluster_lookup", "test"): [
        "real_versioned_control_readers_preserve_single_class_service_and_refuse_unproved_mixture",
        "complete_lookup_cap_keeps_pending_recovery_and_refuses_one_byte_over",
        "real_versioned_control_ordering_expiry_and_reconnect_keep_exact_authority",
        "real_versioned_control_heads_share_one_wallet_capacity",
    ],
    ("comms_v4_assignment_fence", "test"): [
        "selected_identity_domains_survive_production_assignment_json_postgres_roundtrip",
    ],
    ("pulse_nats_live", "test"): [
        "cancelled_terminal_disconnect_is_not_replayed_after_real_broker_reconnect",
    ],
}
TEST_SCRIPT = [
    'cargo test --locked "$@" --no-run --message-format=json-render-diagnostics | python3 -c \'',
    'if artifact.get("executable") and artifact.get("profile", {}).get("test"):',
    'print(artifact["executable"])',
    '\' > "$logs/binaries"',
    '[ -s "$logs/binaries" ] || { echo "catalyst-test: no test binaries found" >&2; exit 1; }',
    'shard="${CATALYST_TEST_SHARD:-1/1}"',
    'index="${shard%/*}" total="${shard#*/}"',
    '[[ "$shard" =~ ^[1-9][0-9]*/[1-9][0-9]*$ ]] && [ "$index" -le "$total" ] || { echo "catalyst-test: CATALYST_TEST_SHARD must be i/n with i from 1 to n" >&2; exit 1; }',
    'sort "$logs/binaries" | awk -v i="$index" -v n="$total" \'NR % n == i % n\' > "$logs/shard"',
    'grep -E \'/(archipelago|pulse)_[^/]*$\' "$logs/shard" > "$logs/sockets" || [ $? -eq 1 ]',
    'grep -v -E \'/(archipelago|pulse)_[^/]*$\' "$logs/shard" > "$logs/others" || [ $? -eq 1 ]',
    'xargs -P "$1" -I{} sh -c \'',
    '"$1" > "$log" 2>&1 || { cat "$log"; exit 1; }',
    'run "$jobs" < "$logs/shard"',
    'run "$jobs" < "$logs/others"',
    'run "$socket_jobs" < "$logs/sockets"',
    'cargo test --locked "$@" --doc',
]


def require(condition, message):
    if not condition:
        raise ValueError(message)


def inventory_entries(output):
    entries = []
    for line in output.splitlines():
        if not line.strip():
            continue
        match = re.fullmatch(r"([^\s]+): (test|benchmark)", line)
        require(match is not None, f"malformed libtest inventory: {line!r}")
        entries.append((match[1], match[2]))
    return entries


def inventory(output):
    names = [name for name, kind in inventory_entries(output) if kind == "test"]
    require(len(names) == len(set(names)), "duplicate libtest inventory")
    return Counter(names)


def verify(required, normal, ignored, label):
    require(required and len(required) == len(set(required)), f"{label}: empty/duplicate manifest")
    all_names, ignored_names = inventory(normal), inventory(ignored)
    for name in required:
        require(all_names[name] == 1, f"{label}: required test absent/renamed: {name}")
        require(ignored_names[name] == 0, f"{label}: required test ignored: {name}")
    print(f"{label}: {len(required)} mandatory tests present once, zero ignored", flush=True)


def verify_exact(required, normal, ignored, label):
    require(len(required) == 1, f"{label}: isolated engine manifest must require exactly one test")
    verify(required, normal, ignored, label)
    require(Counter(inventory_entries(normal)) == Counter((name, "test") for name in required),
            f"{label}: isolated engine inventory must contain only its required tests; remove extra tests/benchmarks")
    require(not inventory_entries(ignored), f"{label}: isolated engine inventory must have no ignored entries")


def validate_invocation(output, environ):
    # Deliberately a fixed contract, not a Makefile or shell parser. make -n sees
    # command-line/environment overrides exactly as the following real make does.
    require(output.strip() == BROAD, "catalyst-test must run the unfiltered locked workspace including catalyst; remove target/name/skip/ignored overrides")
    for variable in ("MAKEFLAGS", "MFLAGS", "MAKEOVERRIDES"):
        require(not environ.get(variable, "").strip(), f"unexpected {variable} override")


def validate_runtime_script(script):
    # Shell removes escaped newlines before recognizing assignments/commands.
    logical = re.sub(r"\\\r?\n", "", script)
    make_lines = [line.strip() for line in logical.splitlines() if re.search(r"\bmake\s", line)]
    require(make_lines == ['make -C "$FOUNDATION_DEV_ROOT" catalyst-test ;;'], "dev-test Rust runtime must invoke broad catalyst-test exactly once without filters")
    vendor_lines = [line.strip() for line in logical.splitlines() if line.strip().startswith("cargo test")]
    require(vendor_lines == [VENDOR], "vendor runtime must be the unfiltered locked default lib suite exactly once")

    compile_lines = [line.strip() for line in logical.splitlines() if line.strip().startswith("cargo check")]
    require(compile_lines == [VENDOR_CHECK], "vendor compile gate must check locked all-features/all-targets exactly once")
    sequence = "      python3 tooling/dev-regressions.py\n      " + VENDOR_CHECK + "\n      " + VENDOR + "\n"
    require(logical.count(sequence) == 1, "vendor compile gate must run unconditionally before the default runtime")
    external = "      " + VENDOR + "\n      " + EXTERNAL_COMMAND + '\n      make -C "$FOUNDATION_DEV_ROOT" catalyst-test ;;'
    require(logical.count(external) == 1 and logical.count(EXTERNAL_COMMAND) == 1,
            "provisioned Archipelago tests must run exactly once after vendor tests and before the broad runtime")
    fixture_commands = [
        "python3 -m unittest discover -s tooling/tests -p 'test_archipelago_fixtures.py'",
        "python3 tooling/archipelago-fixtures.py check",
    ]
    for command in fixture_commands:
        require(logical.count("      " + command + "\n") == 1 and logical.count(command) == 1,
                "fixture integrity check and selftests must run exactly once without conditions or masking")
    fixture_sequence = "      python3 -m unittest discover -s tooling/tests -p 'test_dev_regressions.py'\n" + "".join(
        "      " + command + "\n" for command in fixture_commands
    ) + "      python3 -m unittest discover -s tooling/tests -p 'test_enet_provenance.py'\n"
    require(logical.count(fixture_sequence) == 1,
            "fixture integrity check and selftests must remain unconditional between regression and ENet selftests")


def validate_test_script(script):
    # Pins the lines that keep the run complete: bash under set -e, the selection compiled
    # unfiltered, every listed binary of the shard (all of them without CATALYST_TEST_SHARD) run
    # without arguments in both phases, the doc-tests, and no exit before the end. Comment lines are inert.
    require(script.splitlines()[:1] == ["#!/usr/bin/env bash"], "catalyst-test.sh must run under bash")
    lines = [line.strip() for line in script.splitlines() if line.strip() and not line.lstrip().startswith("#")]
    require(lines[:1] == ["set -euo pipefail"], "catalyst-test.sh must stop at the first failing step")
    positions = [[index for index, line in enumerate(lines) if line == pinned] for pinned in TEST_SCRIPT]
    require(all(len(found) == 1 for found in positions) and positions == sorted(positions),
            "catalyst-test.sh must compile the selection unfiltered, run every listed binary without arguments and the doc-tests exactly once; update the guard's pinned lines with the script")
    require(not any(line.startswith("exit") for line in lines), "catalyst-test.sh must not exit before the doc-tests")


def validate_bevy_workflow(workflow):
    # Fixed indentation/shape contract for this checked-in workflow, not a YAML
    # evaluator. Fail closed if the job, selection condition or step changes.
    jobs = re.findall(r"^  test:\n(.*?)(?=^  [\w-]+:|\Z)", workflow, re.M | re.S)
    require(len(jobs) == 1, "native Bevy test job missing/duplicated")
    job = jobs[0]
    condition = "    if: ${{ needs.changes.outputs.test == 'true' && (needs.changes.outputs.native-matrix == 'true' || github.actor != 'dependabot[bot]') }}"
    require([line for line in job.splitlines() if line.startswith("    if:")] == [condition], "native Bevy test job selection condition changed/disabled")
    steps = re.findall(r"^      - name: Run the native test suite\n(.*?)(?=^      - |^    [\w-]+:|\Z)", job, re.M | re.S)
    require(len(steps) == 1, "native Bevy runtime step missing/duplicated")
    step = steps[0]
    require(not re.search(r"^        if:", step, re.M), "native Bevy runtime step must not be conditional")
    require("        run: |\n" in step, "native Bevy runtime step must run its script")
    command = "          " + BEVY + "\n"
    require(workflow.count(command) == 1 and step.endswith(command), "native Bevy runtime invocation moved/changed; retain the unfiltered lane")
    require("        working-directory: rendering/bevy-explorer/source\n" in job, "native Bevy workspace default changed")


def run(command, cwd=ROOT):
    result = subprocess.run(command, cwd=cwd, text=True, stdout=subprocess.PIPE, check=True)
    return result.stdout


def compiled_targets(arguments, cwd, *, selected=None, kind=None):
    output = run(["cargo", "test", "--locked", *arguments, "--no-run", "--message-format=json-render-diagnostics"], cwd)
    return artifact_targets(output, selected, kind=kind)


def artifact_targets(output, selected=None, *, kind=None):
    targets = {}
    for line in output.splitlines():
        message = json.loads(line)
        if message.get("reason") == "compiler-artifact" and message.get("executable") and message.get("profile", {}).get("test"):
            target = message["target"]
            if kind is not None and kind not in target["kind"]:
                continue
            name = target["name"]
            if selected is not None and name not in selected:
                continue
            require(name not in targets, f"duplicate executable target: {name}")
            targets[name] = message["executable"]
    return targets


def verify_external_result(name, output):
    require(output.splitlines().count(f"test {name} ... ok") == 1,
            f"provisioned test absent, skipped or unsuccessful: {name}")
    summaries = [line for line in output.splitlines() if line.startswith("test result:")]
    require(len(summaries) == 1 and re.fullmatch(
        r"test result: ok\. 1 passed; 0 failed; 0 ignored; (?:0 measured; )?\d+ filtered out; finished in .+", summaries[0]),
        f"provisioned test must execute exactly once with zero ignored/failed: {name}")


def run_external(executable, name):
    normal = inventory(run([executable, "--list", "--format", "terse"]))
    ignored = inventory(run([executable, "--list", "--ignored", "--format", "terse"]))
    require(normal[name] == 1 and ignored[name] == 1,
            f"provisioned ignored test absent/renamed or annotation changed: {name}")
    command = [executable, name, "--exact", "--ignored", "--test-threads=1", "--color", "never"]
    print("provisioned Archipelago test: " + " ".join(command), flush=True)
    result = subprocess.run(command, cwd=ROOT, text=True, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, timeout=210)
    print(result.stdout, flush=True)
    require(result.returncode == 0, f"provisioned test failed ({result.returncode}): {name}")
    verify_external_result(name, result.stdout)


def external_prerequisites(environ):
    binary = environ.get("PULSE_NATS_SERVER_BIN", "")
    require(bool(binary.strip()) and Path(binary).is_file() and os.access(binary, os.X_OK),
            "provisioned Archipelago tests require executable PULSE_NATS_SERVER_BIN")
    require(bool(environ.get("CATALYST_TEST_DATABASE_URL", "").strip()),
            "provisioned Archipelago tests require disposable CATALYST_TEST_DATABASE_URL")
    return binary


def check_external():
    binary = external_prerequisites(os.environ)
    version = subprocess.run([binary, "--version"], text=True, stdout=subprocess.PIPE,
                             stderr=subprocess.STDOUT, check=True, timeout=10)
    print(version.stdout, flush=True)
    # Preserve the broad runner's feature graph; choose targets from this exact build.
    output = run(["cargo", "test", "--locked", *SELECTION.split(), "--no-run",
                  "--message-format=json-render-diagnostics"], ROOT / "services/catalyst")
    selected = set(MANDATORY_ARCHIPELAGO) | {(target, kind) for target, kind, _ in EXTERNAL}
    targets = {kind: artifact_targets(output, {target for target, selected_kind in selected
                                              if selected_kind == kind}, kind=kind)
               for kind in {kind for _, kind in selected}}
    for (target, kind), names in MANDATORY_ARCHIPELAGO.items():
        require(target in targets[kind], f"missing provisioned {kind} target: {target}")
        # The broad runner executes these normally; guard their presence without a second run.
        options = {"exact": True} if (target, kind) in EXACT_ARCHIPELAGO else {}
        check_executable(names, targets[kind][target], f"mandatory {target} corpus/transitions", **options)
    for target, kind, name in EXTERNAL:
        require(target in targets[kind], f"missing provisioned {kind} target: {target}")
        run_external(targets[kind][target], name)


def verify_resolution(root=ROOT):
    vendor = (root / "library/vendor/rusty_enet").resolve()
    for workspace, relative in [("services/catalyst", "../../library/vendor/rusty_enet"),
                                ("rendering/bevy-explorer/source", "../../../library/vendor/rusty_enet")]:
        directory = root / workspace
        manifest = tomllib.loads((directory / "Cargo.toml").read_text())
        dependency = manifest["workspace"]["dependencies"]["rusty_enet"]
        require(dependency == {"path": relative, "version": "0.4.0"}, f"{workspace}: ENet must use the single pinned vendor path")
        require((directory / dependency["path"]).resolve() == vendor, f"{workspace}: ENet vendor path diverged")
        lock = tomllib.loads((directory / "Cargo.lock").read_text())
        entries = [entry for entry in lock["package"] if entry["name"] == "rusty_enet"]
        require(len(entries) == 1 and entries[0]["version"] == "0.4.0" and "source" not in entries[0] and "checksum" not in entries[0], f"{workspace}: ENet lock must resolve exactly one local 0.4.0")
    for consumer in ["services/catalyst/comms", "rendering/bevy-explorer/source/crates/comms"]:
        manifest = tomllib.loads((root / consumer / "Cargo.toml").read_text())
        dependencies = manifest["dependencies"] if consumer.startswith("services/") else manifest["target"]['cfg(not(target_arch = "wasm32"))']["dependencies"]
        require(dependencies.get("rusty_enet") == {"workspace": True}, f"{consumer}: ENet must inherit the shared workspace dependency")


def verify_targets(required, targets):
    for target, names in required.items():
        require(target in targets, f"missing required target: {target}")
        check_executable(names, targets[target], target)


def check_executable(required, executable, label, required_ignored=(), *, exact=False):
    normal = run([executable, "--list", "--format", "terse"])
    ignored = run([executable, "--list", "--ignored", "--format", "terse"])
    (verify_exact if exact else verify)(required, normal, ignored, label)
    for name in required_ignored:
        require(inventory(normal)[name] == 1 and inventory(ignored)[name] == 1, f"{label}: provisioned external test must remain ignored by default: {name}")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--bevy-artifacts", type=Path)
    parser.add_argument("--archipelago-external", action="store_true")
    args = parser.parse_args()
    require(not (args.bevy_artifacts and args.archipelago_external), "select only one regression mode")
    if args.archipelago_external:
        check_external()
        return
    manifest = json.loads((ROOT / "tooling/dev-regressions.json").read_text())
    verify_resolution()
    if args.bevy_artifacts:
        workflow = (ROOT / ".github/workflows/bevy-explorer-ci.yml").read_text()
        validate_bevy_workflow(workflow)
        targets = artifact_targets(args.bevy_artifacts.read_text(encoding="utf-8-sig"), {"comms"})
        require("comms" in targets, "native Bevy comms lib target missing")
        verify_targets(manifest["bevy"], targets)
        return
    validate_runtime_script((ROOT / "tooling/dev-test.sh").read_text())
    validate_test_script((ROOT / "tooling/catalyst-test.sh").read_text())
    validate_invocation(run(["make", "--no-print-directory", "-n", "catalyst-test"]), os.environ)
    required = manifest["catalyst"]
    if os.name != "nt":
        for target, names in manifest["unix"].items():
            required[target] += names
    # Match the runtime selection so Cargo reuses the same feature graph and binaries.
    targets = compiled_targets(SELECTION.split(), ROOT / "services/catalyst", selected=required, kind="test")
    verify_targets(required, targets)
    vendor_targets = compiled_targets(["--manifest-path", "library/vendor/rusty_enet/Cargo.toml", "--lib"], ROOT)
    require("rusty_enet" in vendor_targets, "missing vendor lib target")
    check_executable(manifest["vendor"], vendor_targets["rusty_enet"], "vendored ENet deterministic suite", manifest["vendor_ignored"])


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
        sys.exit(f"mandatory regression guard: {error}")
