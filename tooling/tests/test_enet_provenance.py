"""Offline provenance gate negative fixtures live here, never in ignored agent work."""
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

SOURCE = Path(__file__).resolve().parents[2] / "library/vendor/rusty_enet"


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def reseal(root, relative):
    manifest = root / "checksums.sha256"
    lines = manifest.read_text().splitlines()
    manifest.write_text("\n".join(f"{sha(root / relative)}  {relative}" if line.endswith("  " + relative) else line for line in lines) + "\n", newline="\n")
    pin_path = root / "upstream.lock.json"
    pin = json.loads(pin_path.read_text())
    pin["manifests"]["checksums.sha256"] = sha(manifest)
    pin_path.write_text(json.dumps(pin), newline="\n")


class ProvenanceTests(unittest.TestCase):
    def test_known_bad_sources_manifests_and_patches(self):
        for scenario, diagnostic in [("missing", "file set mismatch"), ("extra", "file set mismatch"),
                                     ("replay-drift", "patch replay mismatch"),
                                     ("manifest", "manifest digest mismatch"),
                                     ("patch", "patch digest mismatch")]:
            with self.subTest(scenario=scenario), tempfile.TemporaryDirectory(prefix="enet-negative-") as tmp:
                root = Path(tmp) / "enet"
                shutil.copytree(SOURCE, root, ignore=shutil.ignore_patterns("target", "__pycache__"))
                if scenario == "missing":
                    (root / "src/packet.rs").unlink()
                elif scenario == "extra":
                    (root / "src/unexpected.rs").write_text("// unexpected\n")
                elif scenario == "manifest":
                    with (root / "checksums.sha256").open("a") as stream:
                        stream.write("invalid\n")
                elif scenario == "replay-drift":
                    path = root / "src/host.rs"
                    path.write_bytes(path.read_bytes() + b"\n// self-consistent checksum drift\n")
                    reseal(root, "src/host.rs")
                else:
                    pin = json.loads((root / "upstream.lock.json").read_text())
                    relative = pin["patches"][0]["path"]
                    path = root / relative
                    path.write_bytes(path.read_bytes() + b"\n# tamper\n")
                    reseal(root, relative)
                result = subprocess.run([sys.executable, str(root / "verify.py")], capture_output=True, text=True)
                self.assertNotEqual(result.returncode, 0, result.stdout)
                self.assertIn(diagnostic, result.stdout + result.stderr)

# A bare local repository exercises the actual fetch/tree/blob boundary. No fetched
# file is checked out or executed; these fixtures do not need a public network.
class OnlineProvenanceTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        import importlib.util
        spec = importlib.util.spec_from_file_location("enet_online", SOURCE.parents[2] / "tooling/enet-provenance.py")
        cls.guard = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(cls.guard)

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="enet-online-fixture-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.repo = self.root / "upstream"
        self.repo.mkdir()
        self.git("init", "--template=", str(self.repo))
        self.files = {"LICENSE": b"fixture license\r\n", "src/payload.py": b"raise RuntimeError('must never execute')\x00\xff\n"}
        for name, content in self.files.items():
            path = self.repo / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(content)
        self.git("-C", str(self.repo), "add", ".")
        self.git("-C", str(self.repo), "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                 "-c", "commit.gpgsign=false", "commit", "-qm", "fixture")
        self.revision = self.git("-C", str(self.repo), "rev-parse", "HEAD").decode().strip()
        self.manifest = {name: hashlib.sha256(content).hexdigest() for name, content in self.files.items()}

    def git(self, *arguments):
        return subprocess.run(["git", *arguments], stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                              check=True, env=self.guard.git_environment()).stdout

    def test_exact_complete_binary_blobs_pass_without_checkout(self):
        self.assertEqual(self.guard.verify_tree(str(self.repo), self.revision, self.manifest), 2)
        self.assertFalse((self.root / "payload.py").exists())

    def test_altered_hash_fails_even_with_self_consistent_local_manifest(self):
        changed = dict(self.manifest, LICENSE="0" * 64)
        with self.assertRaisesRegex(ValueError, "upstream blob mismatch: LICENSE"):
            self.guard.verify_tree(str(self.repo), self.revision, changed)

    def test_missing_and_extra_pristine_paths_fail(self):
        for manifest in [{"LICENSE": self.manifest["LICENSE"]}, dict(self.manifest, **{"missing.rs": "0" * 64})]:
            with self.subTest(manifest=manifest), self.assertRaisesRegex(ValueError, "upstream file set mismatch"):
                self.guard.verify_tree(str(self.repo), self.revision, manifest)

    def test_wrong_public_pin_and_repository_fail_before_fetch(self):
        pin = json.loads((SOURCE / "upstream.lock.json").read_text())
        for key, bad in [("revision", "f" * 40), ("repository", "https://example.invalid/wrong")]:
            with self.subTest(key=key), self.assertRaisesRegex(ValueError, "reviewed public upstream pin"):
                self.guard.validate_pin(dict(pin, **{key: bad}), (SOURCE / "upstream-files.sha256").read_bytes())

    def test_resealed_wrong_pristine_manifest_fails_independent_anchor(self):
        pin = json.loads((SOURCE / "upstream.lock.json").read_text())
        content = (SOURCE / "upstream-files.sha256").read_bytes() + b"0" * 64 + b"  injected.rs\n"
        pin["manifests"]["upstream-files.sha256"] = hashlib.sha256(content).hexdigest()
        with self.assertRaisesRegex(ValueError, "reviewed pristine manifest"):
            self.guard.validate_pin(pin, content)

    def test_fetch_failure_is_explicit_and_never_success(self):
        with self.assertRaisesRegex(ValueError, "cannot fetch exact upstream commit"):
            self.guard.verify_tree(str(self.root / "absent-repository"), self.revision, self.manifest)

    def test_nonexistent_commit_is_explicit_failure(self):
        with self.assertRaisesRegex(ValueError, "cannot fetch exact upstream commit"):
            self.guard.verify_tree(str(self.repo), "f" * 40, self.manifest)


    def test_required_online_ci_step_and_relevant_selection(self):
        workflow = (SOURCE.parents[2] / ".github/workflows/development.yml").read_text()
        self.guard.validate_workflow(workflow)
        mutations = [workflow.replace("      - 'tooling/enet-provenance.py'\n", ""),
                    workflow.replace("      - 'library/**'\n", ""),
                    workflow.replace("      - 'tooling/tests/**'\n", ""),
                    workflow.replace("      - name: Verify the exact public ENet source pin", "      - if: false\n        name: Verify the exact public ENet source pin"),
                    workflow.replace("        run: nix develop .#ci --command python3 tooling/enet-provenance.py",
                                     "        continue-on-error: true\n        run: nix develop .#ci --command python3 tooling/enet-provenance.py"),
                    workflow.replace("nix develop .#ci --command python3 tooling/enet-provenance.py", "echo skipped"),
                    workflow.replace("  rust:\n", "  native:\n"),
                    workflow.replace("'[\"ubuntu-26.04\"]'", "'[]'"),
                    workflow.replace('"ubuntu-26.04", "macos-26"', '"macos-26"'),
                    workflow.replace(" && '[\"ubuntu-26.04\"]' || ", "\n          && '[\"macos-26\"]' || "),
                    workflow.replace("    if: ${{ !cancelled() && needs.changes.outputs.run != 'false' }}\n", ""),
                    workflow.replace("    if: ${{ !cancelled() && needs.changes.outputs.run != 'false' }}\n", "    if: ${{ success() }}\n"),
                    workflow.replace("    if: ${{ !cancelled() && needs.changes.outputs.run != 'false' }}\n", "    if: ${{ !cancelled() && needs.changes.outputs.run != 'false' }}\n    continue-on-error: true\n"),
                    # The skip must stay tied to the plan's comparison, never an unconditional or looser one.
                    workflow.replace("    if: ${{ !cancelled() && needs.changes.outputs.run != 'false' }}\n", "    if: ${{ !cancelled() && false }}\n")]
        for bad in mutations:
            # Every mutation must actually change the workflow, or the test would prove nothing.
            self.assertNotEqual(bad, workflow)
            with self.subTest(bad=bad[:80]), self.assertRaisesRegex(ValueError, "online provenance"):
                self.guard.validate_workflow(bad)

    def test_upstream_symlink_is_not_read_as_regular_source(self):
        target = subprocess.run(["git", "-C", str(self.repo), "hash-object", "-w", "--stdin"],
                                input=b"outside", stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                check=True, env=self.guard.git_environment()).stdout.decode().strip()
        self.git("-C", str(self.repo), "update-index", "--add", "--cacheinfo", "120000," + target + ",link")
        self.git("-C", str(self.repo), "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                 "-c", "commit.gpgsign=false", "commit", "-qm", "symlink")
        revision = self.git("-C", str(self.repo), "rev-parse", "HEAD").decode().strip()
        with self.assertRaisesRegex(ValueError, "unsupported upstream tree entry"):
            self.guard.verify_tree(str(self.repo), revision, dict(self.manifest, link=hashlib.sha256(b"outside").hexdigest()))


if __name__ == "__main__":
    unittest.main()
