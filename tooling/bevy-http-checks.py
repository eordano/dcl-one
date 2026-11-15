#!/usr/bin/env python3
"""Run the Explorer HTTP admission review checks from any working directory."""
import argparse
from pathlib import Path
import subprocess
import sys

WORKSPACE = Path(__file__).resolve().parents[1] / "rendering/bevy-explorer/source"
REQUIRED = {
    "ipfs": {
        "a_content_fetch_uses_exactly_one_ticket_on_its_own_gate",
        "retry_uses_live_priorities_after_another_download_completes_in_backoff",
        "immediate_timeout_retry_yields_to_an_older_unowned_request",
        "shared_downloads_keep_arrival_order_when_workers_poll_in_reverse",
        "dropping_shared_readers_withdraws_only_the_last_claim_of_each_owner",
        "last_shared_reader_cancels_a_queued_attempt_and_a_retry_timer",
        "concurrent_writes_to_one_cache_key_publish_complete_files",
        "cache_writer_skips_a_stale_staging_file",
        "failed_cache_publish_removes_its_staging_file",
        "engine_request_keeps_its_slot_until_the_body_finishes",
        "readiness_poll_respects_retry_after_and_remaining_deadline",
        "keys_are_read_while_a_slot_is_in_use",
        "a_departed_hash_ranks_last_for_a_retry_after_its_queue_drained",
        "a_poisoned_gate_keeps_granting_and_releasing",
        "a_queue_entry_without_its_ticket_is_skipped_inside_a_release",
    },
    "platform": {
        "grant_is_held_through_body_read_and_released_on_cancellation",
        "cancellation_before_headers_drops_the_grant",
        "failed_sends_and_stalled_bodies_return_their_grants",
        "retry_after_obeys_server_delay_and_limit",
    },
}


def run(command, capture=False):
    print("+ " + " ".join(command), flush=True)
    result = subprocess.run(command, cwd=WORKSPACE, text=True,
                            stdout=subprocess.PIPE if capture else None)
    if result.returncode:
        if capture:
            print(result.stdout, end="", flush=True)
        raise RuntimeError(f"command exited {result.returncode}")
    return result.stdout


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--filter", default="", help="Run a subset; an empty match fails")
    parser.add_argument("--tests-only", action="store_true", help="Skip consumer compilation")
    args = parser.parse_args()
    # Cargo reads Bevy and the forked crates from trees that are not tracked.
    run([sys.executable, str(Path(__file__).with_name("patched-sources.py")), "materialise", "--quiet"])
    matched = 0
    for package, required in REQUIRED.items():
        command = ["cargo", "test", "--locked", "-p", package, "--lib"]
        listing = run(command + ["--", "--list"], capture=True)
        names = {line[:-6] for line in listing.splitlines() if line.endswith(": test")}
        missing = required - {name.rsplit("::", 1)[-1] for name in names}
        if missing:
            raise RuntimeError(f"{package}: missing review checks: {sorted(missing)}")
        selected = {name for name in names if args.filter in name}
        if not selected:
            continue
        matched += len(selected)
        print(f"{package}: running {len(selected)} tests with the default parallel runner", flush=True)
        output = run(command + ([args.filter] if args.filter else []) + ["--", "--nocapture"], capture=True)
        print(output, end="", flush=True)
        # Stable libtest has no stable JSON output; inspect its named results.
        # Other suite members may be intentionally ignored benchmarks.
        passed = {line[5:-7] for line in output.splitlines()
                  if line.startswith("test ") and line.endswith(" ... ok")}
        expected = {name for name in selected if name.rsplit("::", 1)[-1] in required}
        if not passed or not expected.issubset(passed):
            raise RuntimeError(f"{package}: selected review checks did not run successfully")
    if not matched:
        raise RuntimeError(f"filter {args.filter!r} matches no tests")
    if not args.tests_only:
        run(["cargo", "check", "--locked", "-p", "scene_inspector", "-p", "imposters", "-p", "scene_runner"])
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, RuntimeError) as error:
        print(f"HTTP admission checks failed: {error}", file=sys.stderr)
        sys.exit(1)
