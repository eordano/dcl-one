#!/usr/bin/env python3
"""Authenticate the reviewed ENet import against public Git blobs; never check out code."""
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
SOURCE = ROOT / "library/vendor/rusty_enet"
# Independent review anchors. Updating the import requires reviewing these too.
REPOSITORY = "https://github.com/robtfm/rusty_enet"
REVISION = "6fa651d50554f6db8f8a4bd98aaf95ef3238b408"
MANIFEST_SHA256 = "7bce19bf15feb754a84a36ffd7e17f1d2a540cda764a2f7179b69021c3394fca"


def require(condition, message):
    if not condition:
        raise ValueError(message)


def git_environment():
    # Inherited repository/config redirects must not change the independent source.
    environment = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    environment.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull,
                       GIT_TERMINAL_PROMPT="0", GCM_INTERACTIVE="never")
    return environment


def git(*arguments):
    try:
        result = subprocess.run(["git", "-c", "protocol.ext.allow=never", *arguments],
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                env=git_environment(), check=True, timeout=60)
    except subprocess.TimeoutExpired as error:
        raise ValueError("Git operation exceeded 60 seconds; check network and Git availability") from error
    except subprocess.CalledProcessError as error:
        raise ValueError(f"Git operation failed (exit {error.returncode}); check network, Git availability and reviewed pin") from error
    return result.stdout


def validate_pin(pin, manifest_bytes):
    require(pin.get("repository") == REPOSITORY and pin.get("revision") == REVISION,
            "reviewed public upstream pin mismatch; review repository/revision before updating the guard")
    require(hashlib.sha256(manifest_bytes).hexdigest() == MANIFEST_SHA256
            and pin.get("manifests", {}).get("upstream-files.sha256") == MANIFEST_SHA256,
            "reviewed pristine manifest mismatch; review pristine bytes before updating the guard")


def parse_manifest(content):
    entries = {}
    for line in content.decode("utf-8").splitlines():
        checksum, relative = line.split("  ", 1)
        path = PurePosixPath(relative)
        require(re.fullmatch(r"[0-9a-f]{64}", checksum) is not None and relative
                and not path.is_absolute() and ".." not in path.parts
                and path.as_posix() == relative and "\\" not in relative and relative not in entries,
                f"invalid pristine manifest entry: {relative!r}")
        entries[relative] = checksum
    require(entries, "empty pristine manifest")
    return entries


def verify_tree(repository, revision, manifest):
    require(re.fullmatch(r"[0-9a-f]{40}", revision) is not None,
            "upstream revision must be a full commit SHA")
    with tempfile.TemporaryDirectory(prefix="enet-public-pin-") as temporary:
        # No checkout, submodules, worktree filters, template hooks or fetched executables.
        git("init", "--bare", "--template=", temporary)
        try:
            git("-C", temporary, "fetch", "--no-tags", "--depth=1", repository, revision)
        except (OSError, ValueError) as error:
            raise ValueError(f"cannot fetch exact upstream commit {revision}; network/commit failure is not a skip: {error}") from error
        actual = git("-C", temporary, "rev-parse", "--verify", "FETCH_HEAD^{commit}").decode().strip()
        require(actual == revision, f"fetched upstream commit mismatch: expected {revision}, got {actual}")
        tree = {}
        for record in git("-C", temporary, "ls-tree", "-rz", "--full-tree", revision).split(b"\0"):
            if not record:
                continue
            metadata, path_bytes = record.split(b"\t", 1)
            mode, kind, oid = metadata.decode("ascii").split()
            path = path_bytes.decode("utf-8")
            require(mode in {"100644", "100755"} and kind == "blob" and path not in tree,
                    f"unsupported upstream tree entry: {path}")
            tree[path] = oid
        require(tree.keys() == manifest.keys(),
                f"upstream file set mismatch; missing={sorted(manifest.keys() - tree.keys())}, extra={sorted(tree.keys() - manifest.keys())}")
        for path, oid in tree.items():
            blob = git("-C", temporary, "cat-file", "blob", oid)
            require(hashlib.sha256(blob).hexdigest() == manifest[path],
                    f"upstream blob mismatch: {path}")
    return len(tree)


def validate_workflow(workflow):
    # Fixed contract for this root workflow, alongside the existing runtime guards. The path list is push.paths
    # only: pull requests run the workflow unfiltered and its `changes` plan (tooling/dev-ci.py) reads that list.
    for path in ("library/**", "tooling/enet-provenance.py", "tooling/tests/**"):
        require(workflow.count(f"      - '{path}'\n") == 1,
                f"online provenance selection missing/duplicated: {path}")
    jobs = re.findall(r"^  rust:\n(.*?)(?=^  [\w-]+:|\Z)", workflow, re.M | re.S)
    require(len(jobs) == 1, "online provenance Rust job missing/conditional")
    # The only condition allowed keeps it running whatever happens to the jobs it needs, skipping it only when the
    # plan compared the change and found none of push.paths (library/** among them) touched, or web source alone
    # (apps/sites, apps/ui3, library/ui: never the ENet source).
    conditions = re.findall(r"^    (?:if|continue-on-error):.*$", jobs[0], re.M)
    needs = re.search(r"^    needs:", jobs[0], re.M)
    require(conditions == (["    if: ${{ !cancelled() && needs.changes.outputs.run != 'false' && needs.changes.outputs.rust != 'false' }}"] if needs else []),
            "online provenance Rust job missing/conditional")
    matrix = re.findall(r"^        os:(.*?)(?=^ {0,8}\S|\Z)", jobs[0], re.M | re.S)
    platforms = re.findall(r"\[[^\]]*\]", matrix[0]) if len(matrix) == 1 else []
    require(platforms and all("ubuntu-26.04" in p for p in platforms),
            "online provenance Rust job can drop its Linux leg")
    steps = re.findall(r"^      - name: Verify the exact public ENet source pin\n(.*?)(?=^      - |\Z)",
                       jobs[0], re.M | re.S)
    require(steps == ["        run: nix develop .#ci --command python3 tooling/enet-provenance.py\n"],
            "online provenance step missing/conditional/changed; retain explicit failure")


def main():
    validate_workflow((ROOT / ".github/workflows/development.yml").read_text())
    pin = json.loads((SOURCE / "upstream.lock.json").read_text())
    manifest_bytes = (SOURCE / "upstream-files.sha256").read_bytes()
    validate_pin(pin, manifest_bytes)
    count = verify_tree(REPOSITORY, REVISION, parse_manifest(manifest_bytes))
    print(f"Verified exact public upstream {REPOSITORY}@{REVISION}: {count} pristine paths and byte hashes; no fetched code executed.")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError) as error:
        sys.exit(f"ENet public provenance: {error}")
