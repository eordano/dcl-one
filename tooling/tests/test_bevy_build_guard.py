"""The Explorer's build scripts: the guard on the patched trees, and the inputs of the version string."""
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest import mock

import test_bevy_patched_sources as shared

tool = shared.tool
SOURCE = Path(__file__).parents[2] / "rendering/bevy-explorer/source"
GUARD = SOURCE / "crates/common/build.rs"
VERSION = SOURCE / "build.rs"
STALE = "patched sources are stale: run python3 tooling/patched-sources.py materialise"
INPUT = "cargo:rerun-if-changed="


def rustc():
    """The compiler. Without one a developer's run skips these tests; a CI run fails."""
    compiler = shutil.which("rustc")
    if compiler is None and os.environ.get("CI"):
        raise AssertionError("rustc is not on PATH: CI must run the build script tests")
    if compiler is None:
        raise unittest.SkipTest("needs rustc")
    return compiler


class BuildScriptCase:
    """Compiles one build script once for the class, with no Cargo and no crate."""

    SCRIPT = None

    @classmethod
    def setUpClass(cls):
        super().setUpClass()
        compiler = rustc()
        temporary = tempfile.TemporaryDirectory()
        cls.addClassCleanup(temporary.cleanup)
        cls.binary = Path(temporary.name) / ("build-script" + (".exe" if os.name == "nt" else ""))
        subprocess.run([compiler, "--edition", "2021", "-o", str(cls.binary), str(cls.SCRIPT)], check=True,
                       cwd=temporary.name, stdout=subprocess.PIPE, stderr=subprocess.PIPE)

    def script(self, directory, **variables):
        """(exit code, stderr, the paths the script names as inputs, its other lines)."""
        environment = {key: value for key, value in os.environ.items()
                       if key != "DCL_ONE_ROOT" and not key.startswith("GIT_")}
        environment.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull, **variables)
        result = subprocess.run([str(self.binary)], cwd=directory, env=environment,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        lines = result.stdout.splitlines()
        return (result.returncode, result.stderr, [line[len(INPUT):] for line in lines if line.startswith(INPUT)],
                [line for line in lines if not line.startswith(INPUT)])


class CompilerTest(unittest.TestCase):
    def test_ci_fails_without_a_compiler_and_a_developer_skips(self):
        with mock.patch.object(shutil, "which", return_value=None):
            with mock.patch.dict(os.environ, {"CI": "true"}), self.assertRaises(AssertionError):
                rustc()
            with mock.patch.dict(os.environ):
                os.environ.pop("CI", None)
                with self.assertRaises(unittest.SkipTest):
                    rustc()


class GuardTest(BuildScriptCase, shared.PatchedSourcesCase):
    """The guard reads the stamps with no Python and no crate."""

    SCRIPT = GUARD

    def guard(self, root=True):
        code, err, watched, _lines = self.script(self.work, **({"DCL_ONE_ROOT": str(self.root)} if root else {}))
        return code, err, [Path(path) for path in watched]

    def inputs(self, *names):
        return [path for name in names
                for path in (self.directory(name) / "source.toml", self.directory(name) / "series", self.dest(name) / tool.STAMP)]

    def test_a_tree_without_patches_passes(self):
        self.assertEqual(self.guard(), (0, "", []))
        self.assertEqual(self.guard(root=False), (0, "", []))

    def test_a_current_stamp_passes_and_every_input_is_named(self):
        self.add_source("winit")
        self.materialised("bevy", commits=[self.first])
        self.assertEqual(self.guard(), (0, "", self.inputs("bevy", "winit")))
        # A directory with no source.toml is no source, and a source needs no series.
        (self.directory("notes")).mkdir()
        (self.directory("bevy") / "series/0001-fork-1.patch").unlink()
        (self.directory("bevy") / "series").rmdir()
        tool.write_stamp(self.dest("bevy"), crc32=tool.crc32(tool.Source(self.root, "bevy").inputs()))
        self.assertEqual(self.guard(), (0, "", [path for path in self.inputs("bevy", "winit")
                                                 if path != self.directory("bevy") / "series"]))

    def test_a_stale_stamp_fails_with_one_line(self):
        self.materialised("winit")
        self.assertEqual(self.guard()[0], 0)
        patch = self.directory("winit") / "series/0002-fork-2.patch"
        original = patch.read_bytes()
        patch.write_bytes(original.replace(b"Why the fork", b"Why this fork"))
        # A failed script names no input, so the one line is all Cargo has to show.
        self.assertEqual(self.guard(), (1, STALE + "\n", []))
        patch.write_bytes(original)
        self.assertEqual(self.guard()[0], 0)
        (self.directory("winit") / "series/0003-more.patch").write_bytes(b"")
        self.assertEqual(self.guard(), (1, STALE + "\n", []))
        (self.directory("winit") / "series/0003-more.patch").unlink()
        self.set_key("winit", "ref", "v2")
        self.assertEqual(self.guard(), (1, STALE + "\n", []))

    def test_one_stale_tree_among_current_ones_fails(self):
        self.add_source("winit")
        self.materialised("bevy", commits=[self.first])
        self.set_key("bevy", "ref", "v2")
        self.assertEqual(self.guard(), (1, STALE + "\n", []))

    def test_a_missing_stamp_fails(self):
        self.add_source("winit")
        self.assertEqual(self.guard(), (1, STALE + "\n", []))
        self.ok("materialise")
        self.assertEqual(self.guard()[0], 0)
        (self.dest() / tool.STAMP).unlink()
        self.assertEqual(self.guard(), (1, STALE + "\n", []))

    def test_an_interrupted_materialise_fails(self):
        self.materialised("winit")
        tool.write_stamp(self.dest(), crc32="", files="", inputs="", tree="")
        self.assertEqual(self.guard(), (1, STALE + "\n", []))

    def test_export_leaves_a_tree_the_build_accepts(self):
        self.materialised("winit")
        (self.dest() / "run.sh").write_bytes(b"#!/bin/sh\nexit 0\n")
        self.ok("export", "winit", "-m", "Exit cleanly")
        self.assertEqual(self.guard()[:2], (0, ""))

    def test_the_checksum_is_the_one_the_tool_writes(self):
        # Every byte value, an empty file, names whose order is not the order of their numbers, and a
        # file that is no patch: the script and the tool frame and order the same inputs.
        self.add_source("winit")
        series = self.directory("winit") / "series"
        (series / "0010-bytes.patch").write_bytes(bytes(range(256)) * 300)
        (series / "0003-empty.patch").write_bytes(b"")
        (series / "notes.md").write_bytes(b"not an input\n")
        checksum = tool.crc32(tool.Source(self.root, "winit").inputs())
        tool.write_stamp(self.dest(), crc32=checksum)
        self.assertEqual(self.guard()[:2], (0, ""))
        (series / "notes.md").write_bytes(b"still not an input\n")
        self.assertEqual(self.guard()[:2], (0, ""))
        (series / "0003-empty.patch").write_bytes(b"\0")
        self.assertEqual(self.guard()[:2], (1, STALE + "\n"))
        (series / "0003-empty.patch").write_bytes(b"")
        other = f"{int(checksum, 16) ^ 1:08x}"
        tool.write_stamp(self.dest(), crc32=other)
        self.assertEqual(self.guard()[:2], (1, STALE + "\n"))


class VersionInputsTest(BuildScriptCase, unittest.TestCase):
    """The root build script names what holds the commit it reads, so the version string follows `HEAD`."""

    SCRIPT = VERSION

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.work = Path(temporary.name).resolve()
        self.repository = self.work / "repository"
        (self.repository / "package/src").mkdir(parents=True)
        (self.repository / "package/src/lib.rs").write_bytes(b"")
        shared.git(self.repository, "init", "-q", "-b", "main")
        self.commit(self.repository)

    def commit(self, repository):
        shared.git(repository, "add", ".")
        shared.git(repository, "commit", "-q", "--allow-empty", "-m", "a commit")
        return shared.git(repository, "rev-parse", "HEAD").decode().strip()

    def run_script(self, repository):
        """(the version the script reads, the git files it names as inputs)."""
        package = repository / "package"
        code, err, watched, lines = self.script(package)
        self.assertEqual((code, err), (0, ""))
        self.assertEqual(watched[:4], [os.path.join("src", "lib.rs"), "build.rs", "Cargo.toml", "Cargo.lock"])
        paths = [(package / path).resolve() for path in watched[4:]]
        for path in paths:
            self.assertTrue(path.exists(), path)
        version = next(line.split("=", 2)[2] for line in lines if line.startswith("cargo:rustc-env=BEVY_EXPLORER_VERSION="))
        return version, paths

    def test_a_branch_names_head_and_its_ref(self):
        head = self.commit(self.repository)
        git = self.repository / ".git"
        self.assertEqual(self.run_script(self.repository), (head, [git / "HEAD", git / "refs/heads/main"]))

    def test_a_detached_head_names_head_alone(self):
        shared.git(self.repository, "checkout", "-q", "--detach")
        head = self.commit(self.repository)
        self.assertEqual(self.run_script(self.repository), (head, [self.repository / ".git/HEAD"]))

    def test_a_packed_ref_names_what_the_next_commit_changes(self):
        git = self.repository / ".git"
        shared.git(self.repository, "pack-refs", "--all")
        self.assertFalse((git / "refs/heads/main").exists())
        _version, paths = self.run_script(self.repository)
        self.assertEqual(paths, [git / "HEAD", git / "packed-refs", git / "refs/heads"])
        head = self.commit(self.repository)
        self.assertEqual(self.run_script(self.repository), (head, [git / "HEAD", git / "refs/heads/main"]))

    def test_a_packed_ref_below_a_missing_directory_names_the_nearest_one(self):
        git = self.repository / ".git"
        shared.git(self.repository, "checkout", "-q", "-b", "topic/one")
        shared.git(self.repository, "pack-refs", "--all")
        self.assertFalse((git / "refs/heads/topic").exists())
        self.assertEqual(self.run_script(self.repository)[1], [git / "HEAD", git / "packed-refs", git / "refs/heads"])

    def test_a_linked_worktree_names_its_own_head_and_the_shared_ref(self):
        linked = self.work / "linked"
        shared.git(self.repository, "worktree", "add", "-q", "-b", "topic", str(linked))
        head = self.commit(linked)
        git = self.repository / ".git"
        self.assertNotEqual(head, shared.git(self.repository, "rev-parse", "HEAD").decode().strip())
        self.assertEqual(self.run_script(linked), (head, [git / "worktrees/linked/HEAD", git / "refs/heads/topic"]))
        shared.git(self.repository, "pack-refs", "--all")
        self.assertEqual(self.run_script(linked),
                         (head, [git / "worktrees/linked/HEAD", git / "packed-refs", git / "refs/heads"]))


if __name__ == "__main__":
    unittest.main()
