"""Patched third-party sources rebuild byte for byte from an upstream commit and a patch series."""
import contextlib
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import threading
import unittest
from unittest import mock
import zlib

SPEC = importlib.util.spec_from_file_location("patched_sources", Path(__file__).parents[1] / "patched-sources.py")
tool = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(tool)

POSIX = os.name != "nt"
CRLF = b"first\r\nsecond\r\nthird\r\n"
BINARY = bytes(range(256)) * 8
OLD = 1_700_000_000 * 10**9
SRI = "sha256-" + "A" * 43 + "="


def git(directory, *arguments, stdin=None):
    environment = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    environment.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull)
    command = ["git", "-c", "user.name=CI", "-c", "user.email=ci@example.invalid", "-c", "core.autocrlf=false",
               "-c", "commit.gpgsign=false", *arguments]
    return subprocess.run(command, cwd=directory, input=stdin, stdout=subprocess.PIPE, check=True, env=environment).stdout


class Upstream:
    """A repository served over file://: an upstream and, as later commits, its fork."""

    def __init__(self, path):
        self.path = path
        path.mkdir(parents=True)
        git(path, "init", "-q")
        self.url = path.as_uri()

    def write(self, name, data, executable=False):
        file = self.path / name
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_bytes(data)
        if executable and POSIX:
            file.chmod(0o755)
        git(self.path, "add", "--", name)
        if executable:
            git(self.path, "update-index", "--chmod=+x", "--", name)

    def remove(self, name):
        git(self.path, "rm", "-q", "--", name)

    def entry(self, mode, oid, name):
        git(self.path, "update-index", "--add", "--cacheinfo", f"{mode},{oid},{name}")

    def blob(self, data):
        return git(self.path, "hash-object", "-w", "--stdin", stdin=data).decode().strip()

    def commit(self, message):
        git(self.path, "commit", "-q", "-m", message)
        return self.rev("HEAD")

    def rev(self, name):
        return git(self.path, "rev-parse", name).decode().strip()

    def tree(self, rev):
        return self.rev(rev + "^{tree}")

    def files(self, rev, prefix=""):
        """{path: (mode, bytes)} of the blobs of a revision."""
        result = {}
        for record in filter(None, git(self.path, "ls-tree", "-r", "-z", rev).split(b"\0")):
            meta, name = record.split(b"\t", 1)
            mode, kind, oid = meta.decode().split()
            if kind == "blob":
                result[prefix + name.decode()] = (mode, git(self.path, "cat-file", "blob", oid))
        return result

    def top_tree(self, rev, keep=None, drop=()):
        """The tree of a revision with only, or without, some top-level entries."""
        lines = [line for line in git(self.path, "ls-tree", "-z", rev).split(b"\0") if line
                 and (keep is None or line.split(b"\t")[1].decode() in keep) and line.split(b"\t")[1].decode() not in drop]
        return git(self.path, "mktree", "-z", stdin=b"".join(line + b"\0" for line in lines)).decode().strip()

    def patch(self, commit, *pathspec):
        """One patch file in the tracked format for one fork commit."""
        subject, date = git(self.path, "log", "-1", "--format=%s%n%aI", commit).decode().splitlines()
        diff = git(self.path, "diff", "--binary", "--full-index", f"{commit}^", commit, "--", *pathspec)
        header = (f"From {commit}\nSubject: {subject}\nOrigin: https://example.invalid/fork/commit/{commit}\n"
                  f"Date: {date}\n\nWhy the fork needs it.\n--- not the end of the header\n---\n")
        return header.encode() + diff

    def export(self, rev, directory):
        """What an archive of a revision holds: plain files, no repository."""
        for name, (mode, data) in self.files(rev).items():
            file = directory / name
            file.parent.mkdir(parents=True, exist_ok=True)
            if mode == "120000":
                os.symlink(os.fsdecode(data), file)
                continue
            file.write_bytes(data)
            if mode == "100755" and POSIX:
                file.chmod(0o755)


class PatchedSourcesCase(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.work = Path(temporary.name).resolve()
        self.root = self.work / "repository"
        self.root.mkdir()
        self.cache = self.work / "cache"
        self.home = self.work / "home"
        self.home.mkdir()
        # No git that a test or the tool starts reads the configuration of the user or of the machine.
        self.patch(mock.patch.dict(os.environ, {
            "DCL_PATCHED_SOURCES_CACHE": str(self.cache), "HOME": str(self.home),
            "XDG_CONFIG_HOME": str(self.home / ".config"), "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}))
        # The upstreams of the tests are local repositories, and no test waits before a second fetch.
        self.patch(mock.patch.object(tool, "URL_SCHEMES", ("https://", "file://")))
        self.patch(mock.patch.object(tool, "FETCH_PAUSE", 0))
        self.up = Upstream(self.work / "upstream")
        self.up.write("Cargo.toml", b"[workspace]\n")
        self.up.write("crates/a/Cargo.toml", b'[package]\nname = "a"\n')
        self.up.write("crates/a/src/lib.rs", b"pub fn one() {}\npub fn two() {}\n")
        self.up.write("crates/a/windows.txt", CRLF)
        self.up.write("assets/logo.bin", BINARY)
        self.up.write("examples/demo.rs", b"fn main() {}\n")
        self.up.write("run.sh", b"#!/bin/sh\n", executable=True)
        self.base = self.up.commit("upstream release")
        self.up.write("crates/a/src/lib.rs", b"pub fn one() {}\npub fn two_patched() {}\n")
        self.up.write("crates/a/windows.txt", CRLF.replace(b"second", b"patched"))
        self.first = self.up.commit("Patch the library: keep CRLF")
        self.up.write("assets/logo.bin", BINARY[::-1])
        self.up.write("crates/a/src/new.rs", b"pub fn added() {}\n")
        self.up.remove("examples/demo.rs")
        self.second = self.up.commit("Replace the logo")
        # The base is then no branch tip: it is fetched by its id alone.
        self.up.write("unrelated.txt", b"later upstream work\n")
        self.up.commit("later upstream work")

    def patch(self, patcher):
        value = patcher.start()
        self.addCleanup(patcher.stop)
        return value

    def run_tool(self, *arguments):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = tool.main([str(argument) for argument in arguments], root=self.root)
        return code, out.getvalue(), err.getvalue()

    def ok(self, *arguments):
        code, out, err = self.run_tool(*arguments)
        self.assertEqual(code, 0, err)
        return out

    def fails(self, *arguments):
        code, _out, err = self.run_tool(*arguments)
        self.assertEqual(code, 1)
        return err

    def materialised(self, *arguments, **options):
        self.add_source(*arguments, **options)
        return self.ok("materialise")

    def assert_status(self, state):
        self.assertEqual(self.ok("status"), f"winit: {state}\n")

    def assert_verifies(self):
        self.assertEqual(self.ok("verify"), "winit: ok\n")

    def directory(self, name):
        return self.root / tool.PATCHES / name

    def dest(self, name="winit", root=None):
        return (root or self.root) / tool.EXPLORER / ("bevy" if name == "bevy" else f"forks/{name}")

    def add_source(self, name="winit", commits=None, base_tree=None, tree=None, pathspec=(), extra="", upstream=None, base=None,
                   binary=None):
        """Write a source. `binary` lists by default the patches that hold a binary hunk."""
        upstream = upstream or self.up
        commits = [self.first, self.second] if commits is None else commits
        base = base or self.base
        series = self.directory(name) / "series"
        series.mkdir(parents=True, exist_ok=True)
        holds_binary = []
        for number, commit in enumerate(commits, 1):
            patch = upstream.patch(commit, *pathspec)
            (series / f"{number:04d}-fork-{number}.patch").write_bytes(patch)
            if b"\nGIT binary patch\n" in patch:
                holds_binary.append(f"{number:04d}-fork-{number}.patch")
        tip = commits[-1] if commits else base
        binary = holds_binary if binary is None else binary
        (self.directory(name) / "source.toml").write_bytes((
            f'name = "{name}"\nupstream = "{upstream.url}"\ncommit = "{base}"\nref = "v1"\n'
            f'origin = "https://example.invalid/fork"\norigin_rev = "{tip}"\nlicense = "MIT OR Apache-2.0"\n'
            + (f"binary = {json.dumps(binary)}\n" if binary else "")
            + f'base_tree = "{base_tree or upstream.tree(base)}"\n'
            f'tree = "{tree or upstream.tree(tip)}"\nnix_hash = "{SRI}"\n{extra}').encode())

    def set_key(self, name, key, value):
        file = self.directory(name) / "source.toml"
        file.write_bytes(re.sub(rf'(?m)^{key} = "[^"]*"'.encode(), f'{key} = "{value}"'.encode(), file.read_bytes()))

    def on_disk(self, dest):
        return {name: path.read_bytes() for name, path in tool.walk(dest, {tool.STAMP})}

    def assert_holds(self, dest, files):
        self.assertEqual(self.on_disk(dest), {name: data for name, (_mode, data) in files.items()})
        if POSIX:
            for name, (mode, _data) in files.items():
                self.assertEqual(bool((dest / name).stat().st_mode & 0o100), mode == "100755", name)

    def age(self, dest):
        for _name, path in tool.walk(dest):
            os.utime(path, ns=(OLD, OLD))

    def times(self, dest):
        return {name: path.stat().st_mtime_ns for name, path in tool.walk(dest)}

    def stamp(self, dest=None):
        return json.loads(((dest or self.dest()) / tool.STAMP).read_text())

    def beside(self, dest=None):
        """The names beside a dest: a lock or a temporary file that a run left would be one of them."""
        return sorted(path.name for path in (dest or self.dest()).parent.iterdir())


class MaterialiseTest(PatchedSourcesCase):
    def test_the_dest_is_the_fork_tree_byte_for_byte(self):
        self.add_source()
        self.assert_verifies()
        out = self.ok("materialise")
        tree = self.up.tree(self.second)
        self.assertEqual(out, f"winit: 7 written, 0 removed, tree {tree}\n")
        self.assert_holds(self.dest(), self.up.files(self.second))
        self.assertFalse((self.dest() / "examples").exists())
        stamp = (self.dest() / tool.STAMP).read_text()
        self.assertRegex(stamp, r'^\{"crc32": "[0-9a-f]{8}", "executable": \["run.sh"\], "files": "[0-9a-f]{64}", '
                                r'"local": \["Cargo.lock"\], "tree": "' + tree + r'", "written": "' + tree + r'"\}\n$')
        self.assertEqual(self.beside(), ["winit"])

    def test_a_lock_file_cargo_writes_is_not_an_edit(self):
        self.materialised()
        (self.dest() / "Cargo.lock").write_text("version = 4\n")
        self.assert_status("current")
        self.assertEqual(self.ok("materialise"), "winit: current\n")
        self.assertEqual(self.ok("materialise", "--quiet"), "")
        self.ok("materialise", "--force")
        self.assertTrue((self.dest() / "Cargo.lock").is_file())

    def test_a_crlf_file_stays_crlf(self):
        self.materialised()
        self.assertEqual((self.dest() / "crates/a/windows.txt").read_bytes(), b"first\r\npatched\r\nthird\r\n")

    def test_a_binary_file_is_exact(self):
        self.materialised()
        self.assertEqual((self.dest() / "assets/logo.bin").read_bytes(), BINARY[::-1])

    def test_a_second_materialise_writes_nothing(self):
        self.materialised()
        self.age(self.dest())
        shutil.rmtree(self.cache)
        self.assertEqual(self.ok("materialise", "--offline"), "winit: current\n")
        self.assertEqual(set(self.times(self.dest()).values()), {OLD})
        self.assertFalse(self.cache.exists())

    def test_a_longer_series_rewrites_only_what_changed(self):
        self.materialised(commits=[self.first])
        self.age(self.dest())
        self.add_source()
        self.assert_status("stale")
        self.assertRegex(self.ok("materialise"), r"^winit: 2 written, 1 removed, ")
        self.assert_holds(self.dest(), self.up.files(self.second))
        fresh = {name for name, time in self.times(self.dest()).items() if time != OLD}
        self.assertEqual(fresh, {"assets/logo.bin", "crates/a/src/new.rs", tool.STAMP})

    def test_paths_keep_only_their_part_of_the_base(self):
        self.add_source(pathspec=("crates",), extra='paths = ["crates"]\n',
                        base_tree=self.up.top_tree(self.base, keep={"crates"}),
                        tree=self.up.top_tree(self.second, keep={"crates"}))
        self.assert_verifies()
        self.ok("materialise")
        self.assert_holds(self.dest(), {name: value for name, value in self.up.files(self.second).items()
                                        if name.startswith("crates/")})

    def test_exclude_drops_paths_before_any_patch_applies(self):
        self.add_source(pathspec=(".", ":(exclude)assets", ":(exclude)examples"), extra='exclude = ["assets", "examples"]\n',
                        base_tree=self.up.top_tree(self.base, drop={"assets", "examples"}),
                        tree=self.up.top_tree(self.second, drop={"assets", "examples"}))
        self.assert_verifies()
        self.ok("materialise")
        self.assertFalse((self.dest() / "assets").exists())
        self.assert_holds(self.dest(), {name: value for name, value in self.up.files(self.second).items()
                                        if not name.startswith("assets/")})

    def test_names_select_sources_and_bevy_has_its_own_dest(self):
        self.add_source("winit")
        self.add_source("bevy")
        self.assertEqual(self.ok("materialise", "bevy"), f"bevy: 7 written, 0 removed, tree {self.up.tree(self.second)}\n")
        self.assert_holds(self.dest("bevy"), self.up.files(self.second))
        self.assertEqual(self.ok("status"), "bevy: current\nwinit: missing\n")
        self.assertEqual(self.fails("status", "absent"), f"patched-sources: absent: no {tool.PATCHES}/absent/source.toml\n")

    def test_an_edited_dest_with_unchanged_inputs_is_kept(self):
        self.materialised()
        edited = self.dest() / "crates/a/src/lib.rs"
        edited.write_bytes(b"pub fn mine() {}\n")
        (self.dest() / "run.sh").unlink()
        (self.dest() / "notes.txt").write_bytes(b"mine\n")
        kept = "patched-sources: winit: edited, kept (1 changed, 1 missing, 1 added)\n"
        self.assertEqual(self.run_tool("materialise", "--quiet"), (0, "", kept))
        self.assertEqual(self.run_tool("materialise"), (0, "", kept))
        self.assertEqual(edited.read_bytes(), b"pub fn mine() {}\n")
        self.assert_status("edited")
        shutil.rmtree(self.cache)
        self.assertEqual(self.run_tool("materialise")[2], "patched-sources: winit: edited, kept (not counted: the cache "
                                                          "does not hold the tree it was written from)\n")

    def test_strict_fails_on_an_edited_dest(self):
        self.materialised()
        self.assertEqual(self.ok("materialise", "--strict"), "winit: current\n")
        edited = self.dest() / "crates/a/src/lib.rs"
        edited.write_bytes(b"pub fn mine() {}\n")
        problem = (f"patched-sources: winit: {tool.EXPLORER}/forks/winit is edited (1 changed, 0 missing, 0 added): "
                   "--strict accepts no edit\n")
        self.assertEqual(self.run_tool("materialise", "--strict", "--quiet"), (1, "", problem))
        self.set_key("winit", "ref", "v2")
        self.assertEqual(self.fails("materialise", "--strict"), problem)
        self.assertEqual(edited.read_bytes(), b"pub fn mine() {}\n")
        self.assertRegex(self.ok("materialise", "--strict", "--force"), r"^winit: 1 written, 0 removed, ")

    def test_a_dest_that_lost_every_file_is_written_again(self):
        self.materialised()
        dest = self.dest()
        (dest / "target/debug").mkdir(parents=True)
        (dest / "target/debug/a.rlib").write_bytes(b"built")
        (dest / "Cargo.lock").write_text("version = 4\n")
        for name in ("Cargo.toml", "run.sh"):
            (dest / name).unlink()
        for name in ("assets", "crates"):
            shutil.rmtree(dest / name)
        self.assert_status("stale")
        self.assertEqual(self.run_tool("materialise", "--quiet"),
                         (0, f"winit: 7 written, 0 removed, tree {self.up.tree(self.second)}\n", ""))
        self.assert_holds(dest, {**self.up.files(self.second), "Cargo.lock": ("100644", b"version = 4\n"),
                                 "target/debug/a.rlib": ("100644", b"built")})

    def test_a_stamp_of_the_version_before_is_read(self):
        self.materialised()
        stamp = self.stamp()
        old = {"crc32": stamp["crc32"], "files": stamp["files"], "inputs": "0" * 64, "local": stamp["local"], "tree": stamp["tree"]}
        tool.write_stamp(self.dest(), **old)
        self.assert_status("current")
        (self.dest() / "notes.txt").write_bytes(b"mine\n")
        self.assertIn("edited, kept (not counted: ", self.run_tool("materialise")[2])
        (self.dest() / "notes.txt").unlink()
        self.set_key("winit", "ref", "v2")
        self.assert_status("stale")
        self.assertRegex(self.ok("materialise"), r"^winit: 0 written, 0 removed, ")
        self.assertEqual(self.stamp()["written"], self.up.tree(self.second))

    def test_empty_directories_of_the_build_are_kept(self):
        self.materialised()
        kept = [self.dest() / "target/debug/incremental", self.dest() / ".git/refs"]
        for directory in kept:
            directory.mkdir(parents=True)
        self.set_key("winit", "ref", "v2")
        self.ok("materialise")
        self.assertTrue(all(directory.is_dir() for directory in kept))

    def test_force_drops_the_edits(self):
        self.materialised()
        edited = self.dest() / "crates/a/src/lib.rs"
        edited.write_bytes(b"pub fn mine() {}\n")
        self.assertRegex(self.ok("materialise", "--force"), r"^winit: 1 written, 0 removed, ")
        self.assert_holds(self.dest(), self.up.files(self.second))
        self.assert_status("current")

    def test_a_dest_with_files_and_no_stamp_is_not_overwritten(self):
        self.add_source()
        stray = self.dest() / "notes.txt"
        stray.parent.mkdir(parents=True)
        stray.write_bytes(b"mine\n")
        self.assertIn("holds files and no stamp", self.fails("materialise"))
        self.assertRegex(self.ok("materialise", "--force"), r"^winit: 7 written, 1 removed, ")
        self.assertFalse(stray.exists())

    def test_what_a_build_left_in_a_dest_without_a_stamp_needs_no_force(self):
        self.add_source()
        artefact = self.dest() / "target/debug/a.rlib"
        artefact.parent.mkdir(parents=True)
        artefact.write_bytes(b"built")
        (self.dest() / "Cargo.lock").write_text("version = 4\n")
        self.assertRegex(self.ok("materialise"), r"^winit: 7 written, 0 removed, ")
        self.assertEqual(artefact.read_bytes(), b"built")
        self.assertEqual((self.dest() / "Cargo.lock").read_text(), "version = 4\n")
        self.assert_status("current")

    def interrupted(self, count):
        """A materialise that stops with an error after it wrote `count` files."""
        real = tool.write_files

        def stop(repo, dest, files, keep=()):
            real(repo, dest, dict(sorted(files.items())[:count]), keep={*keep, *files})
            raise OSError("the disk is full")
        with mock.patch.object(tool, "write_files", stop):
            self.assertEqual(self.fails("materialise"), "patched-sources: the disk is full\n")

    def test_an_interrupted_first_run_is_stale_and_can_be_repeated(self):
        self.add_source()
        self.interrupted(3)
        self.assertEqual(len(self.on_disk(self.dest())), 3)
        self.assert_status("stale")
        self.assertRegex(self.ok("materialise"), r"^winit: 4 written, 0 removed, ")
        self.assert_holds(self.dest(), self.up.files(self.second))
        self.assert_status("current")

    def test_an_interrupted_later_run_is_stale_and_can_be_repeated(self):
        self.materialised(commits=[self.first])
        self.add_source()
        self.interrupted(2)
        partly = {name: data for name, (_mode, data) in self.up.files(self.first).items() if name != "examples/demo.rs"}
        self.assertEqual(self.on_disk(self.dest()), {**partly, "assets/logo.bin": BINARY[::-1]})
        self.assert_status("stale")
        self.assertRegex(self.ok("materialise"), r"^winit: 1 written, 0 removed, ")
        self.assert_holds(self.dest(), self.up.files(self.second))
        self.assert_status("current")

    def test_a_file_that_was_written_with_other_bytes_fails(self):
        self.add_source()
        real = tool.Repo.blobs

        @contextlib.contextmanager
        def altered(repo):
            with real(repo) as read:
                yield lambda oid: read(oid).replace(b"two_patched", b"two_altered")
        with mock.patch.object(tool.Repo, "blobs", altered):
            err = self.fails("materialise")
        blob = tool.blob_id(self.up.files(self.second)["crates/a/src/lib.rs"][1])
        self.assertEqual(err, f"patched-sources: {self.dest() / 'crates/a/src/lib.rs'} does not hold the bytes of blob {blob} "
                              "after it was written\n")
        self.assert_status("stale")
        self.ok("materialise")
        self.assert_holds(self.dest(), self.up.files(self.second))

    def test_build_directories_in_a_dest_are_left_alone(self):
        self.materialised()
        artefact = self.dest() / "target/debug/a.rlib"
        artefact.parent.mkdir(parents=True)
        artefact.write_bytes(b"built")
        self.assert_status("current")
        self.set_key("winit", "ref", "v2")
        self.ok("materialise")
        self.assertEqual(artefact.read_bytes(), b"built")

    def test_offline_fails_when_the_base_was_never_fetched(self):
        self.add_source()
        err = self.fails("materialise", "--offline")
        self.assertIn(f"winit: commit {self.base} is not in", err)
        self.assertIn("--offline forbids a fetch", err)
        self.assertFalse(self.dest().exists())
        self.ok("materialise")
        shutil.rmtree(self.dest())
        self.assertRegex(self.ok("materialise", "--offline"), r"^winit: 7 written")

    def test_base_dir_reads_an_exported_base_without_a_fetch(self):
        self.add_source()
        self.up.export(self.base, self.work / "bases/winit")
        self.set_key("winit", "upstream", "https://example.invalid/never-fetched")
        out_root = self.work / "out"
        self.assertRegex(self.ok("materialise", "--offline", "--base-dir", self.work / "bases", "--dest-root", out_root),
                         r"^winit: 7 written")
        self.assert_holds(self.dest(root=out_root), self.up.files(self.second))
        self.assertEqual(self.stamp(self.dest(root=out_root))["written"], self.up.tree(self.second))
        self.assertFalse(self.cache.exists())
        self.assertFalse(self.dest().exists())
        self.assertIn("no exported base at", self.fails("materialise", "--base-dir", self.work / "nowhere"))

    @unittest.skipUnless(POSIX, "a link and file modes")
    def test_base_dir_reads_a_read_only_export_behind_a_link(self):
        # A Nix build passes each base this way: a link to a fetched tree that nothing can write.
        self.add_source()
        export = self.work / "store/winit-base"
        self.up.export(self.base, export)
        (self.work / "bases").mkdir()
        os.symlink(export, self.work / "bases/winit")
        paths = [export, *(Path(directory) / name for directory, dirs, names in os.walk(export) for name in dirs + names)]
        for path in paths:
            path.chmod(path.stat().st_mode & 0o555)
        self.addCleanup(lambda: [path.chmod(0o755) for path in paths])
        before = self.on_disk(export)
        out_root = self.work / "out"
        self.ok("materialise", "winit", "--offline", "--base-dir", self.work / "bases", "--dest-root", out_root)
        self.assert_holds(self.dest(root=out_root), self.up.files(self.second))
        self.assertEqual(self.stamp(self.dest(root=out_root))["tree"], self.up.tree(self.second))
        self.assertEqual(self.on_disk(export), before)
        self.assertFalse(self.cache.exists())

    def test_base_dir_fails_when_the_export_is_not_the_base(self):
        self.add_source()
        self.up.export(self.first, self.work / "bases/winit")
        self.fails("materialise", "--base-dir", self.work / "bases")
        self.assertFalse(self.dest().exists())

    def test_a_patch_that_does_not_apply_names_itself(self):
        self.add_source(commits=[self.second], binary=["0001-fork-1.patch", "0002-fork-2.patch"])
        self.set_key("winit", "tree", self.up.tree(self.second))
        (self.directory("winit") / "series/0001-fork-1.patch").write_bytes(self.up.patch(self.second).replace(b"added()", b"other()"))
        (self.directory("winit") / "series/0002-fork-2.patch").write_bytes(self.up.patch(self.second))
        self.assertIn("winit: series/0002-fork-2.patch does not apply", self.fails("materialise"))

    def test_git_reads_no_configuration_of_the_user(self):
        (self.home / ".gitconfig").write_text("[core]\n\tautocrlf = true\n\thooksPath = /nowhere\n[commit]\n\tgpgsign = true\n"
                                              "[init]\n\ttemplateDir = /nowhere\n[url \"https://example.invalid/\"]\n\tinsteadOf = file://\n")
        hostile = self.work / "hostile.gitconfig"
        hostile.write_bytes((self.home / ".gitconfig").read_bytes())
        with mock.patch.dict(os.environ, {"GIT_CONFIG_GLOBAL": str(hostile), "GIT_CONFIG_SYSTEM": str(hostile), "GIT_DIR": "nowhere"}):
            self.up.write("crates/a/windows.txt", CRLF + CRLF)
            third = self.up.commit("More lines")
            self.add_source(commits=[self.first, self.second, third], tree=self.up.top_tree(third, drop={"unrelated.txt"}))
            self.ok("materialise")
            (self.dest() / "crates/a/windows.txt").write_bytes(CRLF * 3)
            self.ok("export", "winit", "-m", "Yet more lines")
        self.assertEqual(git(self.up.path, "config", "--list", "--show-scope", "--global").strip(), b"")
        self.assertEqual((self.dest() / "crates/a/windows.txt").read_bytes(), CRLF * 3)
        self.assert_verifies()


class CarryEditsTest(PatchedSourcesCase):
    """A dest that was edited while its series changed keeps the edits, merged into the new tree."""

    def setUp(self):
        super().setUp()
        self.up.write("crates/a/src/long.rs", b"".join(b"line %d\n" % number for number in range(1, 11)))
        self.third = self.up.commit("Add a long file")
        self.up.write("crates/a/src/long.rs", b"line one\n" + b"".join(b"line %d\n" % number for number in range(2, 11)))
        self.up.write("run.sh", b"#!/bin/sh\nexit 0\n", executable=True)
        self.fourth = self.up.commit("Change the first line")
        self.long = self.dest() / "crates/a/src/long.rs"

    def series(self, *commits):
        self.add_source(commits=list(commits), tree=self.up.top_tree(commits[-1], drop={"unrelated.txt"}))

    def expected(self, commit):
        return {name: value for name, value in self.up.files(commit).items() if name != "unrelated.txt"}

    def saved_patch(self, out):
        return Path(re.search(r"(?m)^winit: edits saved as (.+)$", out).group(1))

    def test_edits_are_merged_into_the_tree_of_a_longer_series(self):
        self.series(self.first, self.second, self.third)
        self.ok("materialise")
        dest = self.dest()
        self.long.write_bytes(self.long.read_bytes().replace(b"line 9\n", b"line nine\n"))
        (dest / "crates/a/src/lib.rs").write_bytes(b"pub fn mine() {}\n")
        (dest / "notes.txt").write_bytes(b"mine\n")
        (dest / "Cargo.toml").unlink()
        (dest / "run.sh").write_bytes(b"#!/bin/sh\nexit 0\n")
        self.series(self.first, self.second, self.third, self.fourth)
        code, out, err = self.run_tool("materialise", "--quiet")
        self.assertEqual((code, err), (0, ""))
        patch = self.saved_patch(out)
        self.assertEqual(out, "winit: the series changed: carrying 5 edited files over (3 changed, 1 missing, 1 added)\n"
                              f"winit: edits saved as {patch}\n"
                              "winit: Cargo.toml: edit kept\n"
                              "winit: crates/a/src/lib.rs: edit kept\n"
                              "winit: crates/a/src/long.rs: edit merged with the new series\n"
                              "winit: notes.txt: edit kept\n"
                              "winit: run.sh: edit already in the new series\n"
                              f"winit: 1 written, 0 removed, tree {self.up.top_tree(self.fourth, drop={'unrelated.txt'})}\n"
                              "winit: the edits are in rendering/bevy-explorer/forks/winit: export turns them into a patch\n")
        self.assertEqual(patch.parent, self.cache / "edits")
        self.assertIn(b"+line nine\n", patch.read_bytes())
        expected = self.expected(self.fourth)
        del expected["Cargo.toml"]
        expected["crates/a/src/long.rs"] = ("100644", b"line one\n" + b"".join(b"line %d\n" % n for n in range(2, 9))
                                            + b"line nine\nline 10\n")
        expected["crates/a/src/lib.rs"] = ("100644", b"pub fn mine() {}\n")
        expected["notes.txt"] = ("100644", b"mine\n")
        self.assert_holds(dest, expected)
        self.assertEqual(self.beside(), ["winit"])
        self.assert_status("edited")
        self.assertEqual(self.run_tool("materialise")[2], "patched-sources: winit: edited, kept (2 changed, 1 missing, 1 added)\n")
        self.ok("export", "winit", "-m", "Keep what was carried")
        self.assert_status("current")
        self.assert_verifies()
        self.assert_holds(dest, expected)

    def test_edits_that_the_new_series_holds_leave_a_current_dest(self):
        self.series(self.first, self.second, self.third)
        self.ok("materialise")
        self.long.write_bytes(self.up.files(self.fourth)["crates/a/src/long.rs"][1])
        (self.dest() / "run.sh").write_bytes(b"#!/bin/sh\nexit 0\n")
        self.series(self.first, self.second, self.third, self.fourth)
        out = self.ok("materialise")
        self.assertIn("winit: crates/a/src/long.rs: edit already in the new series\nwinit: run.sh: edit already in the new series\nwinit: 0 written, 0 removed, ", out)
        self.assert_holds(self.dest(), self.expected(self.fourth))
        self.assert_status("current")

    def test_a_conflict_changes_nothing_and_saves_the_edits(self):
        self.series(self.first, self.second, self.third)
        self.ok("materialise")
        dest = self.dest()
        self.long.write_bytes(self.long.read_bytes().replace(b"line 1\n", b"line uno\n"))
        (dest / "run.sh").unlink()
        (dest / "notes.txt").write_bytes(b"mine\n")
        before, stamp = self.on_disk(dest), (dest / tool.STAMP).read_bytes()
        self.series(self.first, self.second, self.third, self.fourth)
        code, out, err = self.run_tool("materialise")
        self.assertEqual(code, 1)
        patch = self.saved_patch(out)
        self.assertEqual(out, "winit: the series changed: carrying 3 edited files over (1 changed, 1 missing, 1 added)\n"
                              f"winit: edits saved as {patch}\n")
        self.assertEqual(err, "patched-sources: winit: the edits conflict with the new series in crates/a/src/long.rs, run.sh: "
                              f"{tool.EXPLORER}/forks/winit is unchanged; --force drops the edits, and the saved patch can be "
                              "applied by hand\n")
        self.assertIn(b"+line uno\n", patch.read_bytes())
        self.assertIn(b"deleted file mode 100755", patch.read_bytes())
        self.assertEqual((self.on_disk(dest), (dest / tool.STAMP).read_bytes()), (before, stamp))
        self.assertEqual(self.beside(), ["winit"])
        self.assertRegex(self.ok("materialise", "--force"), r"^winit: 2 written, 1 removed, ")
        self.assert_holds(dest, self.expected(self.fourth))

    def test_a_binary_file_that_both_sides_changed_is_a_conflict(self):
        self.materialised(commits=[self.first])
        (self.dest() / "assets/logo.bin").write_bytes(BINARY[100:300])
        self.add_source()
        self.assertIn("conflict with the new series in assets/logo.bin:", self.fails("materialise"))
        self.assertEqual((self.dest() / "assets/logo.bin").read_bytes(), BINARY[100:300])

    def test_without_the_tree_it_was_written_from_a_dest_is_not_merged(self):
        self.series(self.first, self.second, self.third)
        self.ok("materialise")
        self.long.write_bytes(b"mine\n")
        before = self.on_disk(self.dest())
        shutil.rmtree(self.cache)
        self.series(self.first, self.second, self.third, self.fourth)
        err = self.fails("materialise")
        self.assertIn("the cache does not hold the tree it was written from", err)
        self.assertIn("export the edits", err)
        self.assertIn("--force", err)
        self.assertEqual(self.on_disk(self.dest()), before)

    def test_the_tree_a_dest_was_written_from_has_a_name_in_the_cache(self):
        self.series(self.first, self.second, self.third)
        self.ok("materialise")
        written = self.stamp()["written"]
        refs = subprocess.run(["git", "--git-dir", str(self.cache / "winit.git"), "for-each-ref", "--format=%(refname) %(objectname)"],
                              stdout=subprocess.PIPE, check=True, text=True).stdout
        self.assertEqual(refs, f"refs/patched-sources/{written} {written}\n")

    def test_a_run_that_stopped_while_it_merged_names_the_saved_edits(self):
        self.series(self.first, self.second, self.third)
        self.ok("materialise")
        self.long.write_bytes(self.long.read_bytes().replace(b"line 9\n", b"line nine\n"))
        self.series(self.first, self.second, self.third, self.fourth)
        with mock.patch.object(tool, "write_files", side_effect=OSError("the disk is full")):
            self.fails("materialise")
        self.assert_status("stale")
        code, out, err = self.run_tool("materialise")
        patch = next((self.cache / "edits").iterdir())
        self.assertEqual((code, err), (0, "patched-sources: winit: a run that did not finish was carrying edits over; "
                                          f"they are saved as {patch}\n"))
        self.assertRegex(out, r"^winit: 2 written, 0 removed, ")
        self.assertIn(b"+line nine\n", patch.read_bytes())
        self.assert_holds(self.dest(), self.expected(self.fourth))


class LockTest(PatchedSourcesCase):
    def setUp(self):
        super().setUp()
        self.patch(mock.patch.object(tool, "LOCK_SECONDS", 0.3))

    def test_a_dest_that_another_run_works_on_stops_with_a_message(self):
        self.add_source()
        lock = self.dest().parent / ".winit.patched-lock"
        with tool.locked(lock, "the other run"):
            self.assertEqual(self.fails("materialise"),
                             f"patched-sources: winit: another run holds the lock {lock}; gave up after 0.3 seconds\n")
            self.assertFalse(self.dest().exists())
        self.assertFalse(lock.exists())
        self.ok("materialise")
        (self.dest() / "run.sh").write_bytes(b"#!/bin/sh\nexit 0\n")
        with tool.locked(lock, "the other run"):
            self.assertIn("another run holds the lock", self.fails("export", "winit", "-m", "Exit cleanly"))
        self.ok("export", "winit", "-m", "Exit cleanly")
        self.assertEqual(self.beside(), ["winit"])

    def test_a_cache_repository_that_another_run_works_on_stops_with_a_message(self):
        self.add_source()
        lock = self.cache / "winit.git.lock"
        with tool.locked(lock, "the other run"):
            for verb in ("materialise", "verify", "divergence"):
                self.assertIn(f"winit: another run holds the lock {lock}; gave up after 0.3 seconds", self.fails(verb), verb)
        self.ok("materialise")
        self.assertEqual(sorted(path.name for path in self.cache.iterdir()), ["winit.git"])

    def test_a_lock_waits_for_the_run_that_holds_it(self):
        lock, order = self.work / "a.lock", []
        self.patch(mock.patch.object(tool, "LOCK_SECONDS", 60))

        def other():
            with tool.locked(lock, "the other run"):
                order.append("other")
        with tool.locked(lock, "this run"):
            thread = threading.Thread(target=other)
            thread.start()
            thread.join(0.1)
            order.append("this")
        thread.join()
        self.assertEqual(order, ["this", "other"])
        self.assertFalse(lock.exists())

    def test_the_stamp_is_replaced_and_never_written_in_place(self):
        self.materialised()
        before = (self.dest() / tool.STAMP).read_bytes()
        self.set_key("winit", "ref", "v2")
        with mock.patch.object(tool.os, "replace", side_effect=OSError("the disk is full")):
            self.assertEqual(self.fails("materialise"), "patched-sources: the disk is full\n")
        self.assertEqual((self.dest() / tool.STAMP).read_bytes(), before)
        self.assertEqual(self.beside(), ["winit"])
        self.assertEqual(sorted(self.on_disk(self.dest())), sorted(self.up.files(self.second)))


class FetchTest(PatchedSourcesCase):
    def fetches(self, failures):
        """Run materialise with a fetch that fails some times first; (result, fetch commands, their time limits, pauses)."""
        real, commands, limits, pauses, sleep = tool.Repo.run, [], [], [], tool.time.sleep

        def run(command, **options):
            if "fetch" in command:
                commands.append(command)
                limits.append(options.get("timeout"))
                if len(commands) <= failures:
                    raise tool.Failure("the connection was reset")
            return real(command, **options)
        # The standard library also sleeps, for a moment each time, while it waits for a process.
        with mock.patch.object(tool.Repo, "run", staticmethod(run)), mock.patch.object(tool, "FETCH_PAUSE", 5), \
                mock.patch.object(tool.time, "sleep", lambda seconds: (pauses.append if seconds >= 1 else sleep)(seconds)):
            result = self.run_tool("materialise")
        return result, commands, limits, pauses

    def test_a_fetch_that_fails_is_repeated_after_a_pause(self):
        self.add_source()
        (code, out, err), commands, limits, pauses = self.fetches(2)
        self.assertEqual(code, 0, err)
        self.assertEqual(err, "patched-sources: winit: fetch attempt 1 of 3 failed (the connection was reset); "
                              "the next one starts in 5 seconds\n"
                              "patched-sources: winit: fetch attempt 2 of 3 failed (the connection was reset); "
                              "the next one starts in 15 seconds\n")
        self.assertEqual((limits, pauses), ([300, 300, 300], [5, 15]))
        self.assertRegex(out, r"^winit: 7 written")

    def test_a_fetch_gives_up_after_three_attempts(self):
        self.add_source()
        (code, _out, err), commands, _limits, pauses = self.fetches(3)
        self.assertEqual((code, len(commands), pauses), (1, 3, [5, 15]))
        self.assertEqual(err.splitlines()[-1], f"patched-sources: winit: cannot fetch {self.base} from {self.up.url} "
                                               "in 3 attempts: the connection was reset")
        self.assertFalse(self.dest().exists())

    def test_git_reads_no_option_from_the_url_or_the_commit(self):
        self.add_source()
        _result, commands, _limits, _pauses = self.fetches(0)
        self.assertEqual(commands[0][-3:], ["--end-of-options", self.up.url, self.base])

    def test_only_https_urls_are_fetched(self):
        self.add_source()
        self.patch(mock.patch.object(tool, "URL_SCHEMES", ("https://",)))
        problem = "patched-sources: winit: source.toml: upstream must be a URL that starts with https://\n"
        self.assertEqual(self.fails("materialise"), problem)
        for url in ("http://example.invalid/x", "ssh://example.invalid/x", "git@example.invalid:x", "ext::sh -c id",
                    "--upload-pack=id", "/srv/x", "https://example.invalid/x y"):
            with self.subTest(url):
                self.set_key("winit", "upstream", url)
                self.assertEqual(self.fails("verify"), problem)
        self.assertFalse(self.cache.exists())
        self.set_key("winit", "upstream", "https://example.invalid/x")
        submodule = f'[[submodule]]\npath = "libs/yuv"\nurl = "{self.up.url}"\ncommit = "{self.base}"\nnix_hash = "{SRI}"\n'
        with open(self.directory("winit") / "source.toml", "a") as file:
            file.write(submodule)
        self.assertEqual(self.fails("status"), "patched-sources: winit: source.toml: [[submodule]] url must be a URL that "
                                               "starts with https://\n")


class BinaryPatchTest(PatchedSourcesCase):
    def test_a_patch_with_a_binary_hunk_must_be_listed(self):
        self.add_source(binary=[])
        problem = ("patched-sources: winit: series/0002-fork-2.patch holds a binary hunk: list it under binary in source.toml\n")
        self.assertEqual(self.run_tool("verify"), (1, "", problem))
        self.assertEqual(self.fails("materialise"), problem)
        self.assertFalse(self.dest().exists())
        self.add_source()
        self.assertIn(b'binary = ["0002-fork-2.patch"]\n', (self.directory("winit") / "source.toml").read_bytes())
        self.assert_verifies()

    def test_the_list_names_only_patches_with_a_binary_hunk(self):
        for listed in ("0001-fork-1.patch", "0009-absent.patch"):
            with self.subTest(listed):
                self.add_source(binary=["0002-fork-2.patch", listed])
                self.assertEqual(self.fails("verify"), f"patched-sources: winit: binary lists {listed}, which is no patch of "
                                                       "the series with a binary hunk\n")

    def test_export_takes_binary_edits_only_when_asked(self):
        self.materialised(commits=[self.first])
        source = self.directory("winit") / "source.toml"
        before = source.read_bytes()
        self.assertNotIn(b"binary", before)
        (self.dest() / "assets/logo.bin").write_bytes(BINARY[100:300])
        self.assertEqual(self.fails("export", "winit", "-m", "New logo"),
                         "patched-sources: winit: the edits change binary files, which no review can read: "
                         "--binary lists 0002-new-logo.patch under binary in source.toml\n")
        self.assertEqual((source.read_bytes(), len(list((self.directory("winit") / "series").iterdir()))), (before, 1))
        self.ok("export", "winit", "-m", "New logo", "--binary")
        self.assertRegex(source.read_text(), r'\nbinary     = \["0002-new-logo.patch"\]\nbase_tree = ')
        (self.dest() / "assets/logo.bin").write_bytes(BINARY[200:400])
        self.ok("export", "winit", "-m", "Newer logo", "--binary")
        self.assertRegex(source.read_text(), r'\nbinary     = \["0002-new-logo.patch", "0003-newer-logo.patch"\]\nbase_tree = ')
        self.assert_status("current")
        self.assert_verifies()


class VerifyAndStatusTest(PatchedSourcesCase):
    def test_verify_fails_on_a_wrong_tree(self):
        self.add_source()
        tree, wrong = self.up.tree(self.second), self.up.tree(self.first)
        self.set_key("winit", "tree", wrong)
        code, out, err = self.run_tool("verify")
        self.assertEqual((code, out, err), (1, "", f"patched-sources: winit: tree is {tree}, source.toml has {wrong}\n"))
        self.set_key("winit", "tree", tree)
        self.set_key("winit", "base_tree", wrong)
        self.assertEqual(self.fails("verify"),
                         f"patched-sources: winit: base_tree is {self.up.tree(self.base)}, source.toml has {wrong}\n")
        self.assertFalse(self.dest().exists())

    def test_materialise_refuses_a_wrong_tree(self):
        self.add_source()
        self.set_key("winit", "tree", self.up.tree(self.first))
        self.assertIn("winit: tree is", self.fails("materialise"))
        self.assertFalse(self.dest().exists())

    def test_verify_checks_every_source(self):
        self.add_source("winit")
        self.add_source("opener")
        self.set_key("opener", "tree", tool.ZERO)
        code, out, err = self.run_tool("verify")
        self.assertEqual((code, out), (1, "winit: ok\n"))
        self.assertRegex(err, r"^patched-sources: opener: tree is [0-9a-f]{40}, source.toml has 0{40}\n$")

    def test_status_names_the_four_states(self):
        self.add_source()
        self.assert_status("missing")
        self.ok("materialise")
        self.assert_status("current")
        file = self.dest() / "crates/a/src/lib.rs"
        original = file.read_bytes()
        file.write_bytes(original + b"// edit\n")
        self.assert_status("edited")
        file.write_bytes(original)
        self.assert_status("current")
        (self.dest() / "extra.rs").write_bytes(b"")
        self.assert_status("edited")
        (self.dest() / "extra.rs").unlink()
        patch = self.directory("winit") / "series/0002-fork-2.patch"
        patch.write_bytes(patch.read_bytes().replace(b"Why the fork", b"Why this fork"))
        self.assert_status("stale")
        shutil.rmtree(self.cache)
        self.assert_status("stale")

    @unittest.skipUnless(POSIX, "file modes")
    def test_a_changed_file_mode_is_an_edit(self):
        self.materialised()
        (self.dest() / "Cargo.toml").chmod(0o755)
        self.assert_status("edited")
        self.assertEqual(self.run_tool("materialise")[2], "patched-sources: winit: edited, kept (1 changed, 0 missing, 0 added)\n")
        (self.dest() / "Cargo.toml").chmod(0o644)
        (self.dest() / "run.sh").chmod(0o644)
        self.assert_status("edited")
        self.ok("export", "winit", "-m", "Run nothing")
        self.assertIn(b"\nold mode 100755\nnew mode 100644\n", (self.directory("winit") / "series/0003-run-nothing.patch").read_bytes())
        self.assert_status("current")

    def test_a_stamp_that_is_no_stamp_reads_as_missing(self):
        self.materialised()
        for text in ("", "[]", '{"local": "Cargo.lock"}', '{"files": 1}'):
            with self.subTest(text):
                (self.dest() / tool.STAMP).write_text(text)
                self.assert_status("missing")
                self.assertIn("holds files and no stamp", self.fails("materialise"))

    def test_the_removed_verbs_are_no_commands(self):
        self.add_source()
        for arguments in (["hash"], ["divergence", "--json"]):
            with self.subTest(arguments), self.assertRaises(SystemExit), contextlib.redirect_stderr(io.StringIO()):
                tool.main(arguments, root=self.root)


class SourceFileTest(PatchedSourcesCase):
    def test_the_dest_is_not_a_key(self):
        self.add_source(extra='dest = "rendering"\n')
        self.assertEqual(self.fails("materialise", "--force"), "patched-sources: winit: source.toml: unknown keys: dest\n")
        self.assertEqual(tool.Source(self.root, "winit").dest if self.add_source() is None else "", f"{tool.EXPLORER}/forks/winit")

    def test_a_key_with_an_unusable_value_names_itself(self):
        self.add_source()
        good = (self.directory("winit") / "source.toml").read_bytes()
        for key, value, problem in (
                ("commit", "v1", "commit must be a full commit id"),
                ("ref", "", "ref is needed"),
                ("ref", "v1 beta", "ref must be a tag or branch name"),
                ("ref", "-v1", "ref must be a tag or branch name"),
                ("ref", "a/../b", "ref must be a tag or branch name"),
                ("origin", "git@example.invalid:fork", "origin must be a URL that starts with https:// or file://"),
                ("origin_rev", "main", "origin_rev must be a full commit id"),
                ("origin_rev", "", "origin_rev is needed"),
                ("license", "", "license is needed"),
                ("license", "MIT / Apache-2.0", "license must be an SPDX licence expression"),
                ("license", "MIT or Apache-2.0", "license must be an SPDX licence expression"),
                ("license", "(MIT OR Apache-2.0", "license must be an SPDX licence expression"),
                ("license", "MIT AND", "license must be an SPDX licence expression"),
                ("nix_hash", "", "nix_hash is needed"),
                ("nix_hash", "sha256-AAAA", "nix_hash must be an SRI hash: sha256-<base64>"),
                ("nix_hash", "0" * 52, "nix_hash must be an SRI hash: sha256-<base64>"),
                ("tree", "main", "tree must be a full tree id")):
            with self.subTest(key=key, value=value):
                self.set_key("winit", key, value)
                self.assertEqual(self.fails("status"), f"patched-sources: winit: source.toml: {problem}\n")
                (self.directory("winit") / "source.toml").write_bytes(good)
        self.assert_status("missing")

    def test_every_needed_key_is_needed(self):
        self.add_source()
        good = (self.directory("winit") / "source.toml").read_text()
        for key in ("upstream", "commit", "ref", "origin", "origin_rev", "license", "nix_hash"):
            with self.subTest(key):
                (self.directory("winit") / "source.toml").write_text(re.sub(rf"(?m)^{key} = .*\n", "", good))
                self.assertEqual(self.fails("status"), f"patched-sources: winit: source.toml: {key} is needed\n")

    def test_a_licence_is_an_spdx_expression(self):
        for text in ("MIT", "MIT OR Apache-2.0", "(MIT OR Apache-2.0) AND Zlib", "Zlib AND (MIT OR Apache-2.0)",
                     "GPL-2.0-only WITH Classpath-exception-2.0", "Apache-2.0+", "((MIT))"):
            self.assertTrue(tool.is_license(text), text)
        for text in ("", "MIT OR", "OR MIT", "MIT Apache-2.0", "MIT WITH", "()", "(MIT", "MIT)", "MIT, Apache-2.0", "MIT/Apache-2.0",
                     "(" * 5000 + "MIT" + ")" * 5000):
            self.assertFalse(tool.is_license(text), text)

    def test_a_submodule_needs_its_keys(self):
        table = f'[[submodule]]\npath = "libs/yuv"\nurl = "https://example.invalid/yuv"\ncommit = "{self.base}"\nnix_hash = "{SRI}"\n'
        for line, problem in (('nix_hash = "' + SRI + '"\n', "[[submodule]] nix_hash is needed"),
                              ('path = "libs/yuv"\n', "[[submodule]] path is needed"),
                              (f'commit = "{self.base}"\n', "[[submodule]] commit is needed")):
            with self.subTest(problem):
                self.add_source(extra=table.replace(line, ""))
                self.assertEqual(self.fails("status"), f"patched-sources: winit: source.toml: {problem}\n")
        self.add_source(extra=table.replace(SRI, "sha256-AAAA"))
        self.assertEqual(self.fails("status"), "patched-sources: winit: source.toml: [[submodule]] nix_hash must be an SRI hash: "
                                               "sha256-<base64>\n")
        self.add_source(extra=table.replace('"libs/yuv"', '"../yuv"'))
        self.assertEqual(self.fails("status"), "patched-sources: winit: source.toml: [[submodule]] path must be a path inside the tree\n")
        self.add_source(extra=table + 'branch = "main"\n')
        self.assertEqual(self.fails("status"), "patched-sources: winit: source.toml: [[submodule]] unknown keys: branch\n")


class TrackedSourcesTest(unittest.TestCase):
    """The source.toml files of this repository."""
    KEYS = ["name", "upstream", "commit", "ref", "origin", "origin_rev", "license", "paths", "exclude", "binary",
            "base_tree", "tree", "nix_hash"]
    SUBMODULE_KEYS = ["path", "url", "commit", "nix_hash"]

    def test_every_source_is_valid_and_in_one_style(self):
        names = tool.names(tool.ROOT)
        self.assertTrue(names)
        for name in names:
            with self.subTest(name):
                source = tool.Source(tool.ROOT, name)
                self.assertEqual(tool.binary_patches(source), source.binary)
                top, *tables = re.split(r"(?m)^\[\[submodule\]\]\n", source.file.read_text())
                self.assertEqual(len(tables), len(source.submodules))
                for text, order in ((top, self.KEYS), *((table, self.SUBMODULE_KEYS) for table in tables)):
                    lines = [line for line in text.splitlines() if re.match(r"[a-z_]+\s*=", line)]
                    keys = [line.split("=")[0].strip() for line in lines]
                    self.assertEqual(keys, [key for key in order if key in keys])
                    # The values start in one column: the longest key of the table, a space, the sign, a space.
                    width = max(map(len, order))
                    for line in lines:
                        self.assertRegex(line, rf"^[a-z_ ]{{{width}}} = \S", line)


class ExportTest(PatchedSourcesCase):
    def edit(self):
        dest = self.dest()
        (dest / "crates/a/src/lib.rs").write_bytes(b"pub fn one() {}\npub fn two_exported() {}\n")
        (dest / "crates/a/windows.txt").write_bytes(b"first\r\npatched\r\nthird\r\nfourth\r\n")
        (dest / "assets/logo.bin").write_bytes(BINARY[100:300])
        (dest / "crates/a/src/blob.bin").write_bytes(b"\0\1\2" * 500)
        (dest / "crates/a/src/new.rs").unlink()
        if POSIX:
            (dest / "Cargo.toml").chmod(0o755)

    def series(self):
        return sorted(path.name for path in (self.directory("winit") / "series").iterdir())

    def test_edits_become_the_next_patch_and_rebuild_elsewhere(self):
        self.materialised()
        self.edit()
        edited = self.on_disk(self.dest())
        with mock.patch.dict(os.environ, {"SOURCE_DATE_EPOCH": "1700000000"}):
            out = self.ok("export", "winit", "-m", "Tune the thing: keep CRLF!", "--binary")
        patch = self.directory("winit") / "series/0003-tune-the-thing-keep-crlf.patch"
        source = (self.directory("winit") / "source.toml").read_text()
        tree = re.search(r'(?m)^tree = "([0-9a-f]{40})"$', source).group(1)
        self.assertEqual(out, f"winit: {tool.PATCHES}/winit/series/{patch.name}, tree {tree}\n")
        self.assertNotEqual(tree, self.up.tree(self.second))
        self.assertIn(f'\nbinary     = ["0002-fork-2.patch", "{patch.name}"]\n', source)
        header, _, diff = patch.read_bytes().partition(b"\n---\n")
        self.assertEqual(header.decode(), f"From {tool.ZERO}\nSubject: Tune the thing: keep CRLF!\n"
                                          "Date: 2023-11-14T22:13:20+00:00\n")
        self.assertTrue(diff.startswith(b"diff --git a/"))
        self.assertIn(b"GIT binary patch", diff)
        self.assertIn(b"+fourth\r\n", diff)
        self.assertEqual(self.on_disk(self.dest()), edited)
        self.assertEqual(self.series(), ["0001-fork-1.patch", "0002-fork-2.patch", patch.name])
        self.assert_status("current")
        self.assert_verifies()
        with mock.patch.dict(os.environ, {"DCL_PATCHED_SOURCES_CACHE": str(self.work / "cache-2")}):
            self.ok("materialise", "--dest-root", self.work / "second")
        second = self.dest(root=self.work / "second")
        self.assertEqual(self.on_disk(second), edited)
        if POSIX:
            self.assertTrue((second / "Cargo.toml").stat().st_mode & 0o100)

    def test_export_needs_edits(self):
        self.materialised()
        self.assertEqual(self.fails("export", "winit", "-m", "Nothing"), "patched-sources: winit: rendering/bevy-explorer/forks/winit has no edits\n")
        self.assertEqual(len(list((self.directory("winit") / "series").iterdir())), 2)

    def test_export_needs_a_dest_from_the_current_series(self):
        self.materialised(commits=[self.first])
        self.add_source()
        (self.dest() / "run.sh").write_bytes(b"#!/bin/sh\nexit 0\n")
        self.assertIn("was not written from the current series", self.fails("export", "winit", "-m", "Late edit"))

    def test_the_stamp_holds_one_checksum_of_the_inputs(self):
        self.materialised()
        self.assertEqual(sorted(self.stamp()), ["crc32", "executable", "files", "local", "tree", "written"])
        (self.dest() / "run.sh").write_bytes(b"#!/bin/sh\nexit 0\n")
        tool.write_stamp(self.dest(), **{**self.stamp(), "crc32": "00000000"})
        self.assertIn("was not written from the current series", self.fails("export", "winit", "-m", "Late edit"))

    def test_the_first_patch_of_a_source_is_number_one(self):
        self.add_source(commits=[])
        shutil.rmtree(self.directory("winit") / "series")
        self.ok("materialise")
        (self.dest() / "run.sh").write_bytes(b"#!/bin/sh\nexit 0\n")
        self.ok("export", "winit", "-m", "  Exit cleanly  ")
        patch = self.directory("winit") / "series/0001-exit-cleanly.patch"
        self.assertIn(b"\nSubject: Exit cleanly\n", patch.read_bytes())
        self.assertNotIn(b"binary", (self.directory("winit") / "source.toml").read_bytes())
        self.assert_verifies()

    def test_the_patch_and_the_new_tree_are_written_together_or_not_at_all(self):
        self.materialised()
        (self.dest() / "run.sh").write_bytes(b"#!/bin/sh\nexit 0\n")
        source = self.directory("winit") / "source.toml"
        before, real = source.read_bytes(), os.replace

        def replace(temporary, path):
            if Path(path) == source:
                raise OSError("the disk is full")
            real(temporary, path)
        with mock.patch.object(tool.os, "replace", replace):
            self.assertEqual(self.fails("export", "winit", "-m", "Exit cleanly"), "patched-sources: the disk is full\n")
        self.assertEqual((source.read_bytes(), self.series()), (before, ["0001-fork-1.patch", "0002-fork-2.patch"]))
        self.assertEqual(sorted(path.name for path in self.directory("winit").iterdir()), ["series", "source.toml"])
        replaced = []
        with mock.patch.object(tool.os, "replace", lambda temporary, path: (replaced.append(Path(path).name), real(temporary, path))):
            self.ok("export", "winit", "-m", "Exit cleanly")
        self.assertEqual(replaced, ["0003-exit-cleanly.patch", "source.toml", tool.STAMP])
        self.assert_verifies()

    def test_export_skips_what_an_editor_or_a_patch_run_left(self):
        self.materialised()
        dest = self.dest()
        junk = [".DS_Store", "crates/.lib.rs.swp", "crates/a/notes~", "crates/a/src/lib.rs.rej", "run.sh.orig"]
        for name in junk:
            (dest / name).write_bytes(b"left over\n")
        self.assertEqual(self.fails("export", "winit", "-m", "Nothing"),
                         "".join(f"patched-sources: winit: skipped {name}: it is left in {tool.EXPLORER}/forks/winit and is "
                                 "not in the patch\n" for name in junk)
                         + "patched-sources: winit: rendering/bevy-explorer/forks/winit has no edits\n")
        (dest / "run.sh").write_bytes(b"#!/bin/sh\nexit 0\n")
        (dest / "crates/a/kept.orig.rs").write_bytes(b"// no junk\n")
        code, _out, err = self.run_tool("export", "winit", "-m", "Exit cleanly")
        self.assertEqual((code, err.count("skipped")), (0, 5))
        patch = (self.directory("winit") / "series/0003-exit-cleanly.patch").read_bytes()
        self.assertEqual(re.findall(rb"(?m)^diff --git a/(\S+)", patch), [b"crates/a/kept.orig.rs", b"run.sh"])
        self.assertTrue(all((dest / name).is_file() for name in junk))
        self.assert_verifies()
        self.assertEqual(self.run_tool("materialise")[2], "patched-sources: winit: edited, kept (0 changed, 0 missing, 5 added)\n")

    def test_bad_input_is_one_line(self):
        self.materialised()
        (self.dest() / "run.sh").write_bytes(b"#!/bin/sh\nexit 0\n")
        for value in ("soon", "1e9", "9" * 30):
            with self.subTest(value), mock.patch.dict(os.environ, {"SOURCE_DATE_EPOCH": value}):
                self.assertEqual(self.fails("export", "winit", "-m", "Exit cleanly"),
                                 f"patched-sources: SOURCE_DATE_EPOCH is no time in seconds: {value!r}\n")
        for subject in ("", "  ", "two\nlines", "a\ttab", "a\0zero"):
            with self.subTest(subject):
                self.assertEqual(self.fails("export", "winit", "-m", subject), "patched-sources: export: the subject is one line of text\n")
        self.assertEqual(self.fails("export", "../winit", "-m", "Exit cleanly"),
                         f"patched-sources: ../winit: no {tool.PATCHES}/../winit/source.toml\n")
        self.assertEqual(self.series(), ["0001-fork-1.patch", "0002-fork-2.patch"])

    def test_the_series_ends_at_patch_9999(self):
        self.add_source()
        series = self.directory("winit") / "series"
        (series / "0002-fork-2.patch").rename(series / "9999-fork-2.patch")
        source = self.directory("winit") / "source.toml"
        source.write_bytes(source.read_bytes().replace(b"0002-fork-2.patch", b"9999-fork-2.patch"))
        self.ok("materialise")
        (self.dest() / "run.sh").write_bytes(b"#!/bin/sh\nexit 0\n")
        self.assertEqual(self.fails("export", "winit", "-m", "Exit cleanly"),
                         "patched-sources: winit: the series is full at patch 9999: fold patches into one before the next export\n")
        self.assertEqual(self.series(), ["0001-fork-1.patch", "9999-fork-2.patch"])


class LinkAndSubmoduleTest(PatchedSourcesCase):
    def setUp(self):
        super().setUp()
        self.sub = Upstream(self.work / "submodule")
        self.sub.write("include/yuv.h", b"#pragma once\r\n")
        self.sub.write("build.sh", b"#!/bin/sh\n", executable=True)
        self.sub_commit = self.sub.commit("library")
        self.up.entry("160000", self.sub_commit, "libs/yuv")
        self.up.entry("160000", self.sub_commit, "tests/suite")
        self.up.entry("120000", self.up.blob(b"../../Cargo.toml"), "crates/a/WORKSPACE.toml")
        self.up.entry("120000", self.up.blob(b"WORKSPACE.toml"), "crates/a/AGAIN.toml")
        self.up.entry("120000", self.up.blob(b"../outside"), "LICENSE")
        self.linked = self.up.commit("links and submodules")
        self.up.write("Cargo.toml", b"[workspace]\nresolver = \"2\"\n")
        self.fork = self.up.commit("Use the new resolver")
        self.submodule = (f'[[submodule]]\npath = "libs/yuv"\nurl = "{self.sub.url}"\ncommit = "{self.sub_commit}"\n'
                          f'nix_hash = "{SRI}"\n')

    def expected(self):
        files = self.up.files(self.fork)
        files["crates/a/WORKSPACE.toml"] = files["crates/a/AGAIN.toml"] = files["Cargo.toml"]
        files["LICENSE"] = ("100644", b"../outside")
        files.update(self.sub.files(self.sub_commit, "libs/yuv/"))
        return files

    def test_a_submodule_path_is_filled_and_links_become_files(self):
        self.add_source(commits=[self.fork], base=self.linked, extra=self.submodule)
        self.ok("materialise")
        dest = self.dest()
        self.assert_holds(dest, self.expected())
        self.assertFalse(any(path.is_symlink() for _name, path in tool.walk(dest)))
        self.assertFalse((dest / "tests").exists())
        self.assert_status("current")
        self.assertNotEqual(self.stamp()["written"], self.stamp()["tree"])
        self.age(dest)
        self.assertEqual(self.ok("materialise"), "winit: current\n")
        (dest / "libs/yuv/include/yuv.h").unlink()
        (dest / "crates/a/AGAIN.toml").write_bytes(b"mine\n")
        self.assertEqual(self.run_tool("materialise")[2], "patched-sources: winit: edited, kept (1 changed, 1 missing, 0 added)\n")

    def test_verify_fails_on_a_gitlink_that_nothing_fills(self):
        self.add_source(commits=[self.fork], base=self.linked, extra=self.submodule)
        self.assertEqual(self.run_tool("verify"), (1, "", "patched-sources: winit: gitlink tests/suite has no [[submodule]]: "
                                                          "add one, or exclude the path\n"))
        self.add_source(commits=[self.fork], base=self.linked, extra='exclude = ["tests"]\n' + self.submodule,
                        base_tree=self.up.top_tree(self.linked, drop={"tests"}), tree=self.up.top_tree(self.fork, drop={"tests"}))
        self.assertEqual(self.run_tool("verify"), (0, "winit: ok\n", ""))

    def test_the_submodule_commit_must_be_the_one_in_the_tree(self):
        self.sub.write("include/yuv.h", b"#pragma once\n")
        other = self.sub.commit("newer")
        self.add_source(commits=[self.fork], base=self.linked, extra=self.submodule.replace(self.sub_commit, other))
        self.assertIn(f"the tree has {self.sub_commit} at libs/yuv", self.fails("materialise"))

    def test_export_keeps_links_and_refuses_submodule_edits(self):
        self.materialised(commits=[self.fork], base=self.linked, extra=self.submodule)
        dest = self.dest()
        (dest / "libs/yuv/include/yuv.h").write_bytes(b"#pragma twice\r\n")
        self.assertIn("libs/yuv/include/yuv.h is inside a submodule", self.fails("export", "winit", "-m", "Edit the library"))
        (dest / "libs/yuv/include/yuv.h").write_bytes(b"#pragma once\r\n")
        (dest / "Cargo.toml").write_bytes(b"[workspace]\nresolver = \"3\"\n")
        self.ok("export", "winit", "-m", "Resolver three")
        patch = (self.directory("winit") / "series/0002-resolver-three.patch").read_bytes()
        self.assertEqual(patch.count(b"diff --git "), 1)
        # The copies of the link follow the file it names, as a new materialise writes them.
        self.assertEqual((dest / "crates/a/AGAIN.toml").read_bytes(), b"[workspace]\nresolver = \"3\"\n")
        self.assert_status("current")

    def test_an_edited_link_becomes_a_file(self):
        self.materialised(commits=[self.fork], base=self.linked, extra=self.submodule)
        (self.dest() / "LICENSE").write_bytes(b"The licence text.\n")
        self.ok("export", "winit", "-m", "Carry the licence")
        patch = (self.directory("winit") / "series/0002-carry-the-licence.patch").read_bytes()
        self.assertIn(b"deleted file mode 120000", patch)
        self.assertIn(b"new file mode 100644", patch)
        self.assert_status("current")

    @unittest.skipUnless(POSIX, "an export holds links")
    def test_base_dir_takes_the_submodule_from_the_export(self):
        self.add_source(commits=[self.fork], base=self.linked, extra='exclude = ["tests"]\n' + self.submodule,
                        base_tree=self.up.top_tree(self.linked, drop={"tests"}),
                        tree=self.up.top_tree(self.fork, drop={"tests"}))
        self.assertEqual(self.run_tool("verify"), (0, "winit: ok\n", ""))
        self.up.export(self.linked, self.work / "bases/winit")
        self.sub.export(self.sub_commit, self.work / "bases/winit/libs/yuv")
        # A checkout without submodules leaves an empty directory at each gitlink.
        (self.work / "bases/winit/tests/suite").mkdir(parents=True)
        self.set_key("winit", "upstream", "https://example.invalid/never-fetched")
        shutil.rmtree(self.cache)
        out_root = self.work / "out"
        self.ok("materialise", "--offline", "--base-dir", self.work / "bases", "--dest-root", out_root)
        self.assert_holds(self.dest(root=out_root), self.expected())
        self.assertFalse(self.cache.exists())


class HashAndDivergenceTest(PatchedSourcesCase):
    def framed(self, files, new):
        digest = new()
        for name, data in sorted(files.items()):
            digest.update(name.encode() + b"\0" + len(data).to_bytes(8, "little") + data)
        return digest.hexdigest()

    def test_the_stamp_holds_the_hashes_of_its_source(self):
        self.materialised()
        stamp = json.loads((self.dest() / tool.STAMP).read_text())
        inputs = {path.relative_to(self.directory("winit")).as_posix(): path.read_bytes()
                  for path in self.directory("winit").rglob("*") if path.is_file()}
        crc = 0
        for name, data in sorted(inputs.items()):
            crc = zlib.crc32(name.encode() + b"\0" + len(data).to_bytes(8, "little") + data, crc)
        self.assertEqual(stamp["crc32"], f"{crc:08x}")
        self.assertEqual(stamp["files"], self.framed(self.on_disk(self.dest()), hashlib.sha256))

    def test_divergence_counts_files_and_lines_by_crate(self):
        self.add_source()
        self.assertEqual(self.ok("divergence"), "winit: 2 patches, 5 files, +3 -3\n"
                                                "  crates/a: 3 files, +3 -2\n  .: 2 files, +0 -1\n")


if __name__ == "__main__":
    unittest.main()
