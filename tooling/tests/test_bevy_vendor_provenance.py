"""The Explorer's in-tree crates, tracked as the exact bytes of one upstream revision, and
tooling/vendor-provenance.py, which proves them."""
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
TOOL = ROOT / "tooling/vendor-provenance.py"
VENDOR = "rendering/bevy-explorer/vendor"
DIRECTORIES = [f"{VENDOR}/bevy_dui", f"{VENDOR}/boimp", f"{VENDOR}/rpc-rust"]


def load_guard():
    spec = importlib.util.spec_from_file_location("vendor_provenance", TOOL)
    guard = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(guard)
    return guard


class InTreeCrateTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.guard = load_guard()

    def run_tool(self, *arguments):
        return subprocess.run([sys.executable, str(TOOL), *arguments], cwd=ROOT, capture_output=True, text=True)

    def test_each_crate_matches_its_lock_file(self):
        for directory, shipped in zip(DIRECTORIES, (2, 16, 19)):
            with self.subTest(directory=directory):
                anchor = self.guard.anchor_of(ROOT / directory)
                self.assertEqual(self.guard.verify_directory(ROOT / directory, anchor, fetch=False)[0], shipped)

    def test_every_crate_directory_has_an_anchor_and_every_anchor_is_public(self):
        self.assertEqual([f"{VENDOR}/{name}" for name in sorted(self.guard.ANCHORS)], sorted(DIRECTORIES))
        self.assertEqual(self.guard.unrecorded(), [])
        for name, repository in self.guard.ANCHORS.items():
            self.assertRegex(repository, r"^https://github\.com/[\w.-]+/[\w.-]+$", name)

    def test_one_command_checks_all_three_and_all_is_the_default(self):
        for arguments in (DIRECTORIES, []):
            result = self.run_tool("--offline", *arguments)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual([line.split()[1] for line in result.stdout.splitlines()], DIRECTORIES)

    def test_command_fails_on_a_directory_with_no_anchor(self):
        for directory in ("library/vendor/rusty_enet", "rendering/bevy-explorer/source/vendor/basisu"):
            result = self.run_tool("--offline", directory)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn(f"In-tree crate provenance: {directory}: no recorded repository", result.stderr)

    def test_command_rejects_an_unknown_option(self):
        result = self.run_tool("--ofline", DIRECTORIES[0])
        self.assertEqual(result.returncode, 2)
        self.assertIn("unrecognized arguments: --ofline", result.stderr)

    def test_the_enet_tool_no_longer_takes_directories(self):
        result = subprocess.run([sys.executable, str(ROOT / "tooling/enet-provenance.py"), "--offline", DIRECTORIES[0]],
                                cwd=ROOT, capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("tooling/vendor-provenance.py proves the Explorer's in-tree crates", result.stderr)

    def test_a_directory_added_without_an_anchor_fails_the_default_run(self):
        with tempfile.TemporaryDirectory(prefix="vendor-provenance-anchors-") as temporary:
            for name in ("bevy_dui", "extra"):
                (Path(temporary) / name).mkdir()
            (Path(temporary) / "README.md").write_text("not a crate\n")
            self.assertEqual(self.guard.unrecorded(Path(temporary), {"bevy_dui": "x", "gone": "y"}), ["extra", "gone"])

    def test_newline_conversion_is_off_for_each_directory(self):
        attributes = (ROOT / ".gitattributes").read_text().splitlines()
        for directory in DIRECTORIES:
            self.assertIn(f"{directory}/** -text -whitespace", attributes)


# A local repository stands in for the public one; a CRLF file and a binary file must survive as bytes.
class CrateFixtureTests(unittest.TestCase):
    ANCHOR = "https://example.invalid/one"

    FILES = {"Cargo.toml": b'[workspace]\nmembers = ["one", "examples/*"]\n',
             "LICENSE": b"fixture license\r\n",
             "examples/demo/main.rs": b"fn main() {}\n",
             "one/Cargo.toml": b'[package]\nname = "one"\nversion = "1.2.3"\nlicense = "MIT"\n',
             "one/src/lib.rs": b"pub fn one() {}\r\n",
             "one/src/table.bin": b"\x00\xff\r\n\x00"}
    OMITTED = ["Cargo.toml", "examples/demo/main.rs"]

    @classmethod
    def setUpClass(cls):
        cls.guard = load_guard()

    def git(self, *arguments):
        return subprocess.run(["git", *arguments], stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                              check=True, env=self.guard.git_environment()).stdout

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="vendor-provenance-fixture-")
        self.addCleanup(self.temporary.cleanup)
        self.repo = Path(self.temporary.name) / "upstream"
        self.source = Path(self.temporary.name) / "vendor/one"
        self.git("init", "--template=", str(self.repo))
        for name, content in self.FILES.items():
            for base in [self.repo] + ([] if name in self.OMITTED else [self.source]):
                (base / name).parent.mkdir(parents=True, exist_ok=True)
                (base / name).write_bytes(content)
        self.git("-C", str(self.repo), "-c", "core.autocrlf=false", "add", ".")
        self.git("-C", str(self.repo), "-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
                 "-c", "commit.gpgsign=false", "commit", "-qm", "fixture")
        self.digests = {name: hashlib.sha256(content).hexdigest() for name, content in self.FILES.items()}
        self.pin = {"repository": self.ANCHOR,
                    "revision": self.git("-C", str(self.repo), "rev-parse", "HEAD").decode().strip(),
                    "package": "one", "version": "1.2.3", "path_in_vcs": "one",
                    "license": {"spdx": "MIT", "path": "LICENSE", "sha256": self.digests["LICENSE"]},
                    "omitted": {"reason": "fixture", "paths": self.OMITTED}}
        self.seal(self.digests)

    def seal(self, digests, **changes):
        manifest = "".join(f"{digests[name]}  {name}\n" for name in digests).encode()
        (self.source / "upstream-files.sha256").write_bytes(manifest)
        pin = dict(self.pin, manifests={"upstream-files.sha256": hashlib.sha256(manifest).hexdigest()}, **changes)
        (self.source / "upstream.lock.json").write_text(json.dumps(pin))

    def verify(self, fetch=True):
        return self.guard.verify_directory(self.source, self.ANCHOR, repository=str(self.repo), fetch=fetch)

    def test_exact_member_of_a_workspace_passes(self):
        self.assertEqual(self.verify(), (4, sum(len(self.FILES[n]) for n in self.FILES if n not in self.OMITTED), 6))

    def test_build_output_beside_the_package_manifest_is_not_a_shipped_file(self):
        (self.source / "one/target/debug").mkdir(parents=True)
        (self.source / "one/target/debug/one.d").write_text("built\n")
        self.assertEqual(self.verify(fetch=False)[0], 4)

    def test_changed_missing_extra_and_omitted_files_fail(self):
        def newline(): (self.source / "one/src/lib.rs").write_bytes(b"pub fn one() {}\n")
        def missing(): (self.source / "one/src/table.bin").unlink()
        def extra(): (self.source / "one/src/extra.rs").write_text("// extra\n")
        def omitted(): (self.source / "Cargo.toml").write_bytes(self.FILES["Cargo.toml"])
        def target(): (self.source / "target").mkdir(); (self.source / "target/x").write_text("x")
        def link():
            (self.source / "LICENSE").unlink()
            (self.source / "LICENSE").symlink_to(self.repo / "LICENSE")
        for change, diagnostic in [(newline, "source digest mismatch: one/src/lib.rs"), (missing, "file set mismatch"),
                                   (extra, "file set mismatch"), (omitted, "file set mismatch"),
                                   (target, "file set mismatch"), (link, "source digest mismatch: LICENSE")]:
            with self.subTest(change=change.__name__):
                self.setUp()
                change()
                with self.assertRaisesRegex(ValueError, diagnostic):
                    self.verify(fetch=False)

    def test_lock_file_that_names_another_repository_fails_before_anything_is_fetched(self):
        # A resealed lock file is consistent with itself: only the anchor in the tool can tell.
        for repository in ("https://example.invalid/two", self.ANCHOR + ".git", str(self.repo), None):
            with self.subTest(repository=repository):
                self.seal(self.digests, repository=repository)
                with self.assertRaisesRegex(ValueError, "the tool records 'https://example.invalid/one'"):
                    self.verify()

    def test_lock_file_that_disagrees_fails(self):
        cases = [(dict(manifests={"upstream-files.sha256": "0" * 64}), "manifest digest mismatch"),
                 (dict(patches=[{"path": "patches/0001.patch"}]), "not an exact-bytes crate"),
                 (dict(license={"spdx": "MIT", "path": "LICENSE", "sha256": "0" * 64}), "license digest mismatch"),
                 (dict(license={"spdx": "MIT", "files": [{"path": "Cargo.toml", "sha256": self.digests["Cargo.toml"]}]}),
                  "license digest mismatch"),
                 (dict(license={"spdx": "Apache-2.0", "path": "LICENSE", "sha256": self.digests["LICENSE"]}),
                  "differs from the package manifest"),
                 (dict(version="1.2.4"), "differs from the package manifest"),
                 (dict(omitted={"paths": ["absent.rs"]}), "omitted paths must be files"),
                 (dict(omitted={"paths": self.OMITTED + ["one/src/table.bin"]}), "file set mismatch")]
        for changes, diagnostic in cases:
            with self.subTest(changes=sorted(changes)):
                self.seal(self.digests)
                path = self.source / "upstream.lock.json"
                path.write_text(json.dumps(dict(json.loads(path.read_text()), **changes)))
                with self.assertRaisesRegex(ValueError, diagnostic):
                    self.verify(fetch=False)

    def test_unsorted_manifest_fails(self):
        self.seal(dict(reversed(self.digests.items())))
        with self.assertRaisesRegex(ValueError, "not sorted"):
            self.verify(fetch=False)

    def test_resealed_local_change_fails_against_the_repository(self):
        (self.source / "one/src/lib.rs").write_bytes(b"pub fn two() {}\n")
        self.seal(dict(self.digests, **{"one/src/lib.rs": hashlib.sha256(b"pub fn two() {}\n").hexdigest()}))
        self.verify(fetch=False)
        with self.assertRaisesRegex(ValueError, "upstream blob mismatch: one/src/lib.rs"):
            self.verify()

    def test_manifest_that_hides_an_upstream_file_fails_against_the_repository(self):
        self.seal({name: digest for name, digest in self.digests.items() if name != "examples/demo/main.rs"},
                  omitted={"paths": ["Cargo.toml"]})
        self.verify(fetch=False)
        with self.assertRaisesRegex(ValueError, "upstream file set mismatch"):
            self.verify()

    def test_wrong_revision_and_non_public_repository_fail(self):
        self.seal(self.digests, revision="f" * 40)
        with self.assertRaisesRegex(ValueError, "cannot fetch exact upstream commit"):
            self.verify()
        self.seal(self.digests, repository=str(self.repo))
        with self.assertRaisesRegex(ValueError, "public https URL"):
            self.guard.verify_directory(self.source, str(self.repo))


if __name__ == "__main__":
    unittest.main()
