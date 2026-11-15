#!/usr/bin/env python3
"""Rebuild the patched third-party Rust sources from an upstream reference and a patch series.

Tracked, per source:  rendering/bevy-explorer/patches/<name>/source.toml
                      rendering/bevy-explorer/patches/<name>/series/NNNN-<slug>.patch
Written here, ignored by git: the patched tree, called the dest, at rendering/bevy-explorer/bevy for
`bevy` and rendering/bevy-explorer/forks/<name> for every other source, and <dest>/.patched-stamp.json.

  materialise [NAME...] [--offline] [--base-dir DIR] [--dest-root DIR] [--force] [--strict] [--quiet]
  verify      [NAME...]                    base_tree and tree of source.toml reproduce; exit 1 if not
  status      [NAME...]                    missing | current | stale | edited, one line per source
  export      NAME -m "subject" [--binary] the edits in the dest become the next patch; tree is updated
  divergence  [NAME...]                    files and lines changed against the base, by crate

source.toml: name, upstream and commit (the base), ref, origin and origin_rev (where the series came
from), license (an SPDX expression), paths and exclude (the part of the base that is kept), binary
(the patch files that may hold a binary hunk), base_tree and tree (what must reproduce), nix_hash, and
one [[submodule]] with path, url, commit and nix_hash for each gitlink that is filled.

Trees are built in a scratch git index (read-tree, apply --cached, write-tree) of a bare repository,
so bytes are exact on every platform: no line-ending conversion, no attributes. Bases are fetched
once, by commit, into $DCL_PATCHED_SOURCES_CACHE (default ~/.cache/dcl-one/patched-sources), one bare repository
per source. `--base-dir DIR` reads an exported base from DIR/<name>/ and needs no network;
`--dest-root DIR` writes below DIR instead of the repository root.

A dest holds plain files only. A link in the tree is written as a copy of the file it names, or as
its target text when it names no file of the tree. Each `[[submodule]]` path is filled from its url
and commit. `.git` and `target` at the top of a dest are never read, written or removed.

materialise keeps an edited dest. While the series is the one the dest was written from, it says
`edited, kept` and writes nothing; `--strict` makes that an error. When the series changed, it
rebuilds the new tree and merges the edits into it, file by file against the tree the dest was
written from, which the cache keeps under refs/patched-sources. A conflict changes nothing in the
dest. The edits are saved as a patch below the cache in both cases. `--force` drops the edits.

One run at a time works on a dest and on a cache repository: each has a lock file beside it, held
while the run works and removed after.

The stamp is one JSON line:
  {"crc32": ..., "executable": [...], "files": ..., "local": [...], "tree": ..., "written": ...}
crc32 and files cover the same byte stream: for each file in path order, the path, a zero byte, the
length as 8 little-endian bytes, the bytes. crc32 = that stream over the source's source.toml and
patches, which the Explorer's build script also computes, files = sha256 of it over the dest as
written, executable = the files written with the executable bit, local = names in the dest that are
not its content, tree = the git tree of the series, written = the git tree of the files as written.
"""
import argparse
import contextlib
import datetime
import fnmatch
import hashlib
import json
import os
from pathlib import Path
import posixpath
import re
import subprocess
import sys
import tempfile
import time
import tomllib
import zlib

ROOT = Path(__file__).resolve().parents[1]
EXPLORER = "rendering/bevy-explorer"
PATCHES = EXPLORER + "/patches"
STAMP = ".patched-stamp.json"
LOCAL = frozenset({".git", "target", STAMP})
JUNK = ("*.orig", "*.rej", "*~", "*.swp", ".DS_Store")
KEYS = frozenset({"name", "upstream", "commit", "ref", "origin", "origin_rev", "license", "paths", "exclude",
                  "binary", "base_tree", "tree", "nix_hash", "submodule"})
SUBMODULE_KEYS = frozenset({"path", "url", "commit", "nix_hash"})
ZERO = "0" * 40
OID = re.compile(r"[0-9a-f]{40}")
REF = re.compile(r"[A-Za-z0-9][A-Za-z0-9._+-]*(/[A-Za-z0-9][A-Za-z0-9._+-]*)*")
SRI = re.compile(r"sha256-[A-Za-z0-9+/]{43}=")
LICENSE_ID = re.compile(r"[A-Za-z0-9][A-Za-z0-9.-]*\+?")
BINARY_HUNK = re.compile(rb"(?m)^GIT binary patch$")
URL_SCHEMES = ("https://",)
FETCH_SECONDS = 300
FETCH_ATTEMPTS = 3
FETCH_PAUSE = 5
LOCK_SECONDS = 1200


class Failure(ValueError):
    pass


def require(condition, message):
    if not condition:
        raise Failure(message)


def note(message):
    print(f"patched-sources: {message}", file=sys.stderr)


def chunks(files):
    for name, path in sorted(files):
        data = content(path)
        yield name.encode("utf-8") + b"\0" + len(data).to_bytes(8, "little")
        yield data


def sha256(files):
    digest = hashlib.sha256()
    for chunk in chunks(files):
        digest.update(chunk)
    return digest.hexdigest()


def crc32(files):
    value = 0
    for chunk in chunks(files):
        value = zlib.crc32(chunk, value)
    return f"{value:08x}"


def blob_id(data):
    return hashlib.sha1(b"blob %d\0" % len(data) + data).hexdigest()


def content(path):
    return os.fsencode(os.readlink(path)) if path.is_symlink() else path.read_bytes()


def walk(directory, skip=()):
    """(relative POSIX path, Path) of every file below a directory; links are entries, never followed."""
    found, pending = [], [(Path(directory), "")]
    while pending:
        current, prefix = pending.pop()
        if not current.is_dir():
            continue
        with os.scandir(current) as entries:
            for entry in entries:
                relative = prefix + entry.name
                if relative in skip:
                    continue
                if entry.is_dir(follow_symlinks=False):
                    pending.append((Path(entry.path), relative + "/"))
                else:
                    found.append((relative, Path(entry.path)))
    return sorted(found)


def file_mode(path, known="100644"):
    if path.is_symlink():
        return "120000"
    if os.name == "nt":
        return known if known in ("100644", "100755") else "100644"
    return "100755" if path.stat().st_mode & 0o100 else "100644"


def scan(dest, skip, known=None):
    """{path: (mode, blob id)} of the files in a dest. Windows keeps the mode a file is known by."""
    known = known or {}
    return {name: (file_mode(path, known.get(name, ("",))[0]), blob_id(content(path))) for name, path in walk(dest, skip)}


def compare(rows, disk):
    """(changed, missing, added) paths of a dest against the rows it was written from."""
    return (sorted(path for path in rows if path in disk and disk[path] != rows[path]),
            sorted(path for path in rows if path not in disk),
            sorted(path for path in disk if path not in rows))


def counted(changed, missing, added):
    return f"{len(changed)} changed, {len(missing)} missing, {len(added)} added"


def replace(temporary, path):
    # On Windows the rename fails while a reader holds the file open.
    for attempt in range(5):
        try:
            os.replace(temporary, path)
            return
        except PermissionError:
            if attempt == 4:
                raise
            time.sleep(0.1 * (attempt + 1))


def write_atomic(path, data, temporary=None, sync=False):
    """Readers see the old file or the new one, never a part of it."""
    temporary = temporary or path.with_name(path.name + ".tmp")
    try:
        with open(temporary, "wb") as handle:
            handle.write(data)
            if sync:
                handle.flush()
                os.fsync(handle.fileno())
        replace(temporary, path)
    except BaseException:
        with contextlib.suppress(OSError):
            temporary.unlink()
        raise


def try_lock(handle):
    try:
        if os.name == "nt":
            import msvcrt
            handle.seek(0)
            msvcrt.locking(handle.fileno(), msvcrt.LK_NBLCK, 1)
        else:
            import fcntl
            fcntl.flock(handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
    except OSError:
        return False
    return True


@contextlib.contextmanager
def locked(path, label):
    """Hold the lock file at a path. The system drops the lock of a run that died, so none goes stale."""
    path.parent.mkdir(parents=True, exist_ok=True)
    deadline, pause = time.monotonic() + LOCK_SECONDS, 0.05
    while True:
        handle = open(path, "a+b")
        if try_lock(handle):
            # The run before removed the file it held: the lock counts only on the file the path names now.
            try:
                if os.path.samestat(os.fstat(handle.fileno()), os.stat(path)):
                    break
            except OSError:
                pass
        handle.close()
        require(time.monotonic() < deadline,
                f"{label}: another run holds the lock {path}; gave up after {LOCK_SECONDS} seconds")
        time.sleep(pause)
        pause = min(pause * 2, 1.0)
    try:
        yield
    finally:
        # Windows removes no open file; everywhere else the file goes while the lock still holds.
        if os.name != "nt":
            with contextlib.suppress(OSError):
                path.unlink()
        handle.close()
        if os.name == "nt":
            with contextlib.suppress(OSError):
                path.unlink()


def git_environment(index=None):
    # Inherited repository, index and config redirects must not change what is built.
    environment = {key: value for key, value in os.environ.items()
                   if not key.startswith("GIT_") or key.startswith(("GIT_SSL_", "GIT_HTTP_"))}
    environment.update(GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull,
                       GIT_TERMINAL_PROMPT="0", GCM_INTERACTIVE="never")
    if index is not None:
        environment["GIT_INDEX_FILE"] = str(index)
    return environment


class Repo:
    """A bare repository: the object store of one source. Every tree lives in a scratch index."""
    OPTIONS = ("-c", "protocol.ext.allow=never", "-c", "core.autocrlf=false", "-c", "core.safecrlf=false",
               "-c", "core.longpaths=true", "-c", "gc.auto=0", "-c", "maintenance.auto=false")

    def __init__(self, path):
        self.path = Path(path)
        if not (self.path / "HEAD").is_file():
            self.path.mkdir(parents=True, exist_ok=True)
            self.run(["git", "init", "-q", "--bare", "--template=", str(self.path)])

    def command(self, arguments):
        return ["git", *self.OPTIONS, "--git-dir", str(self.path), *arguments]

    @staticmethod
    def run(command, index=None, stdin=None, cwd=None, timeout=None, ok=(0,)):
        """Run git; `ok` lists the exit codes that are no failure, None takes every code."""
        try:
            result = subprocess.run(command, input=stdin, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                    env=git_environment(index), cwd=cwd, timeout=timeout)
        except FileNotFoundError as error:
            raise Failure("the git executable is not on PATH") from error
        except subprocess.TimeoutExpired as error:
            raise Failure(f"git exceeded {timeout} seconds") from error
        if ok is not None and result.returncode not in ok:
            lines = result.stderr.decode("utf-8", "replace").strip().splitlines()
            raise Failure("; ".join(lines[-3:]) or f"git exit {result.returncode}")
        return result

    def git(self, *arguments, **options):
        return self.run(self.command(arguments), **options).stdout

    def text(self, *arguments, **options):
        return self.git(*arguments, **options).decode("ascii").strip()

    def has(self, oid, kind):
        return self.run(self.command(["cat-file", "-e", f"{oid}^{{{kind}}}"]), ok=(0, 1, 128)).returncode == 0

    def fetch(self, url, commit, offline, label):
        if self.has(commit, "commit"):
            return
        require(not offline, f"{label}: commit {commit} is not in {self.path} and --offline forbids a fetch")
        for attempt in range(1, FETCH_ATTEMPTS + 1):
            try:
                # Nothing after --end-of-options is read as an option, whatever source.toml holds.
                self.git("fetch", "-q", "--no-tags", "--depth", "1", "--end-of-options", url, commit, timeout=FETCH_SECONDS)
                break
            except Failure as error:
                if attempt == FETCH_ATTEMPTS:
                    raise Failure(f"{label}: cannot fetch {commit} from {url} in {attempt} attempts: {error}") from error
                pause = FETCH_PAUSE * 3 ** (attempt - 1)
                note(f"{label}: fetch attempt {attempt} of {FETCH_ATTEMPTS} failed ({error}); "
                     f"the next one starts in {pause} seconds")
                time.sleep(pause)
        require(self.has(commit, "commit"), f"{label}: {url} did not deliver commit {commit}")

    @staticmethod
    def records(listing, oid_field):
        result = []
        for record in filter(None, listing.split(b"\0")):
            meta, path = record.split(b"\t", 1)
            fields = meta.decode("ascii").split()
            result.append((fields[0], fields[oid_field], path.decode("utf-8")))
        return result

    def rows(self, index, *pathspec):
        """(mode, oid, path) of the index entries, all or those a pathspec selects."""
        return self.records(self.git("ls-files", "-s", "-z", "--", *pathspec, index=index), 1)

    def tree_rows(self, tree):
        return self.records(self.git("ls-tree", "-r", "-z", tree), 2)

    def update(self, index, rows):
        """Add, replace or (oid of zeros) remove index entries."""
        lines = b"".join(f"{'0' if oid == ZERO else mode} {oid} 0\t".encode("ascii") + path.encode("utf-8") + b"\0"
                         for mode, oid, path in rows)
        self.git("update-index", "-z", "--index-info", index=index, stdin=lines)

    def load(self, index, rows):
        self.git("read-tree", "--empty", index=index)
        self.update(index, rows)

    def store(self, files, known=None):
        """Write files as blobs, bytes as they are; (mode, oid, path) rows."""
        known = known or {}
        plain = [(name, path) for name, path in files if not path.is_symlink()]
        for _name, path in plain:
            require("\n" not in str(path) and not str(path).startswith('"'), f"unsupported file name: {path}")
        listing = "".join(f"{path}\n" for _name, path in plain).encode("utf-8")
        oids = self.git("hash-object", "-w", "--no-filters", "--stdin-paths", stdin=listing).decode("ascii").split()
        require(len(oids) == len(plain), "git hash-object returned an incomplete list")
        rows = [(file_mode(path, known.get(name)), oid, name) for (name, path), oid in zip(plain, oids)]
        for name, path in files:
            if path.is_symlink():
                rows.append(("120000", self.text("hash-object", "-w", "--stdin", stdin=content(path)), name))
        return sorted(rows, key=lambda row: row[2])

    def write_tree(self, index):
        return self.text("write-tree", index=index)

    def tree_of(self, index, files):
        """The tree of {path: (mode, oid)}, written through an index that is overwritten."""
        self.load(index, [(mode, oid, path) for path, (mode, oid) in files.items()])
        return self.write_tree(index)

    def keep(self, tree):
        """Name a tree, so that no collection of the cache drops what an edited dest is compared with."""
        self.git("update-ref", f"refs/patched-sources/{tree}", tree)

    def diff(self, old, new):
        return self.git("diff", "--binary", "--full-index", "--no-color", "--no-ext-diff", "--no-textconv",
                        "--src-prefix=a/", "--dst-prefix=b/", old, new)

    def merge(self, base, ours, theirs):
        """The blob of a three-way merge of three blobs, or None when the changes overlap or a side is binary."""
        with tempfile.TemporaryDirectory(prefix="patched-sources-") as temporary, self.blobs() as read:
            paths = []
            for label, oid in (("ours", ours), ("base", base), ("theirs", theirs)):
                paths.append(str(Path(temporary) / label))
                Path(paths[-1]).write_bytes(read(oid))
            result = self.run(["git", "merge-file", "-p", *paths], ok=None)
        return self.text("hash-object", "-w", "--stdin", stdin=result.stdout) if result.returncode == 0 else None

    @contextlib.contextmanager
    def blobs(self):
        process = subprocess.Popen(self.command(["cat-file", "--batch"]), stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, env=git_environment())

        def read(oid):
            process.stdin.write(oid.encode("ascii") + b"\n")
            process.stdin.flush()
            header = process.stdout.readline().split()
            require(len(header) == 3 and header[1] == b"blob", f"object {oid} is not a blob in {self.path}")
            data = process.stdout.read(int(header[2]))
            process.stdout.read(1)
            return data
        try:
            yield read
        finally:
            process.stdin.close()
            process.stdout.close()
            process.wait()


def is_license(text):
    """Whether a text is an SPDX licence expression: ids joined by AND, OR and WITH, with parentheses."""
    tokens = re.findall(r"[()]|[^\s()]+", text)

    def identifier(at):
        return at < len(tokens) and tokens[at] not in ("AND", "OR", "WITH") and LICENSE_ID.fullmatch(tokens[at])

    def expression(at):
        """The index after one expression that starts at an index, or None."""
        while True:
            if tokens[at:at + 1] == ["("]:
                at = expression(at + 1)
                if at is None or tokens[at:at + 1] != [")"]:
                    return None
                at += 1
            elif identifier(at):
                at += 1
                if tokens[at:at + 1] == ["WITH"]:
                    if not identifier(at + 1):
                        return None
                    at += 2
            else:
                return None
            if tokens[at:at + 1] not in (["AND"], ["OR"]):
                return at
            at += 1

    return len(tokens) <= 64 and expression(0) == len(tokens)


def is_url(value):
    return value.startswith(URL_SCHEMES) and not re.search(r"[\s\\\x00-\x1f\x7f]", value)


class Source:
    def __init__(self, root, name):
        self.name = name
        self.directory = Path(root) / PATCHES / name
        self.file = self.directory / "source.toml"
        require(self.file.is_file(), f"{name}: no {PATCHES}/{name}/source.toml")
        try:
            data = tomllib.loads(self.file.read_bytes().decode("utf-8"))
        except (UnicodeDecodeError, tomllib.TOMLDecodeError) as error:
            raise Failure(f"{name}: source.toml: {error}") from error

        def check(condition, message):
            require(condition, f"{name}: source.toml: {message}")

        def text(table, key, test, shape, where=""):
            value = table.get(key)
            check(isinstance(value, str) and value, f"{where}{key} is needed")
            check(test(value), f"{where}{key} must be {shape}")
            return value

        def texts(key, default):
            value = data.get(key, default)
            check(isinstance(value, list) and all(isinstance(item, str) and item for item in value),
                  f"{key} must be a list of texts")
            return value

        url = f"a URL that starts with {' or '.join(URL_SCHEMES)}"
        check(not set(data) - KEYS, f"unknown keys: {', '.join(sorted(set(data) - KEYS))}")
        check(data.get("name") == name, f"name is {data.get('name')!r}")
        self.upstream = text(data, "upstream", is_url, url)
        self.commit = text(data, "commit", OID.fullmatch, "a full commit id")
        text(data, "ref", lambda value: REF.fullmatch(value) and ".." not in value, "a tag or branch name")
        text(data, "origin", is_url, url)
        text(data, "origin_rev", OID.fullmatch, "a full commit id")
        text(data, "license", is_license, "an SPDX licence expression")
        text(data, "nix_hash", SRI.fullmatch, "an SRI hash: sha256-<base64>")
        self.paths, self.exclude, self.binary = texts("paths", ["."]), texts("exclude", []), texts("binary", [])
        check(self.paths, "paths must keep something")
        self.base_tree, self.tree = data.get("base_tree", ""), data.get("tree", "")
        for key, value in (("base_tree", self.base_tree), ("tree", self.tree)):
            check(isinstance(value, str) and (not value or OID.fullmatch(value)), f"{key} must be a full tree id")
        self.submodules = data.get("submodule", [])
        check(isinstance(self.submodules, list) and all(isinstance(item, dict) for item in self.submodules),
              "submodule must be [[submodule]] tables")
        for submodule in self.submodules:
            where = "[[submodule]] "
            check(not set(submodule) - SUBMODULE_KEYS, f"{where}unknown keys: {', '.join(sorted(set(submodule) - SUBMODULE_KEYS))}")
            text(submodule, "path", safe_path, "a path inside the tree", where)
            text(submodule, "url", is_url, url, where)
            text(submodule, "commit", OID.fullmatch, "a full commit id", where)
            text(submodule, "nix_hash", SRI.fullmatch, "an SRI hash: sha256-<base64>", where)

    @property
    def dest(self):
        return f"{EXPLORER}/bevy" if self.name == "bevy" else f"{EXPLORER}/forks/{self.name}"

    @property
    def pathspec(self):
        if self.paths == ["."] and not self.exclude:
            return []
        return [*(":(top)" + ("" if path == "." else path) for path in self.paths),
                *(f":(top,exclude){path}" for path in self.exclude)]

    def patches(self):
        series = self.directory / "series"
        return sorted(path for path in series.glob("*.patch") if path.is_file()) if series.is_dir() else []

    def inputs(self):
        return [("source.toml", self.file), *((f"series/{path.name}", path) for path in self.patches())]


def names(root, selected=()):
    directory = Path(root) / PATCHES
    known = sorted(path.name for path in directory.iterdir() if (path / "source.toml").is_file()) if directory.is_dir() else []
    for name in selected:
        require(name in known, f"{name}: no {PATCHES}/{name}/source.toml")
    return list(selected) or known


def safe_path(path):
    parts = path.split("/")
    return (not any(part in ("", ".", "..") or part.lower() == ".git" for part in parts)
            and "\\" not in path and ":" not in parts[0])


def cache_directory():
    return Path(os.environ.get("DCL_PATCHED_SOURCES_CACHE") or Path.home() / ".cache/dcl-one/patched-sources")


@contextlib.contextmanager
def workspace(source, base_dir=None):
    """The source's repository, locked, and a scratch index. An exported base gets a repository that is discarded."""
    with tempfile.TemporaryDirectory(prefix="patched-sources-") as temporary:
        index = Path(temporary) / "index"
        if base_dir:
            yield Repo(Path(temporary) / "objects.git"), index
            return
        path = cache_directory() / f"{source.name}.git"
        with locked(path.with_name(path.name + ".lock"), source.name):
            yield Repo(path), index


def diff_of(patch):
    """The diff of a patch file: what follows the `---` line that ends its header."""
    at = patch.find(b"\n---\ndiff --git ")
    return patch[at + 5:] if at >= 0 else patch


def binary_patches(source):
    """The names of the patch files that hold a binary hunk, which no review can read."""
    return [patch.name for patch in source.patches() if BINARY_HUNK.search(diff_of(patch.read_bytes()))]


def build(source, repo, index, offline=False, base_dir=None):
    """Fill the index with the base under paths and exclude, then the series; (base tree, tree)."""
    binary = binary_patches(source)
    for name in binary:
        require(name in source.binary, f"{source.name}: series/{name} holds a binary hunk: list it under binary in source.toml")
    for name in source.binary:
        require(name in binary, f"{source.name}: binary lists {name}, which is no patch of the series with a binary hunk")
    if base_dir:
        directory = Path(base_dir) / source.name
        require(directory.is_dir(), f"{source.name}: no exported base at {directory}")
        links = [("160000", submodule["commit"], submodule["path"]) for submodule in source.submodules]
        files = walk(directory, skip={".git", *(submodule["path"] for submodule in source.submodules)})
        repo.load(index, [*repo.store(files), *links])
    else:
        repo.fetch(source.upstream, source.commit, offline, source.name)
        repo.git("read-tree", source.commit, index=index)
    if source.pathspec:
        repo.load(index, repo.rows(index, *source.pathspec))
    base_tree = repo.write_tree(index)
    for patch in source.patches():
        try:
            repo.git("apply", "--cached", "--whitespace=nowarn", "-", index=index, stdin=diff_of(patch.read_bytes()))
        except Failure as error:
            raise Failure(f"{source.name}: series/{patch.name} does not apply: {error}") from error
    return base_tree, repo.write_tree(index)


def differences(source, base_tree, tree):
    return [f"{source.name}: {key} is {built}, source.toml has {recorded or 'none'}"
            for key, built, recorded in (("base_tree", base_tree, source.base_tree), ("tree", tree, source.tree))
            if built != recorded]


def built(source, repo, index, offline=False, base_dir=None):
    base_tree, tree = build(source, repo, index, offline, base_dir)
    wrong = differences(source, base_tree, tree)
    require(not wrong, "; ".join(wrong))
    return tree


def submodule_rows(source, repo, submodule, offline, base_dir):
    exported = Path(base_dir) / source.name / submodule["path"] if base_dir else None
    files = walk(exported) if exported else []
    if files:
        return repo.store(files)
    label = f"{source.name}: submodule {submodule['path']}"
    repo.fetch(submodule["url"], submodule["commit"], offline, label)
    return [row for row in repo.tree_rows(submodule["commit"]) if row[0] != "160000"]


def resolve(entries, read, path):
    """What a link is written as: the file it names in the same tree, else its own target text."""
    text = entries[path][1]
    mode, oid = entries[path]
    seen = {path}
    while mode == "120000":
        path = posixpath.normpath(posixpath.join(posixpath.dirname(path), read(oid).decode("utf-8", "replace")))
        if path in seen or entries.get(path, ("160000", ""))[0] == "160000":
            return "100644", text
        seen.add(path)
        mode, oid = entries[path]
    return mode, oid


def layout(source, repo, index, offline=False, base_dir=None):
    """({path: (mode, blob id of the bytes on disk)}, submodule prefixes) of the dest for the tree in the index."""
    rows = {path: (mode, oid) for mode, oid, path in repo.rows(index)}
    groups = [("", rows)]
    for submodule in source.submodules:
        path = submodule["path"]
        require(rows.get(path, ("160000", submodule["commit"])) == ("160000", submodule["commit"])
                and not any(name.startswith(path + "/") for name in rows),
                f"{source.name}: the tree has {rows.get(path, ('', 'files'))[1]} at {path}, "
                f"[[submodule]] has {submodule['commit']}")
        groups.append((path + "/", {name: (mode, oid) for mode, oid, name in
                                    submodule_rows(source, repo, submodule, offline, base_dir)}))
    files = {}
    with repo.blobs() as read:
        for prefix, entries in groups:
            for path, (mode, oid) in entries.items():
                if mode == "160000":
                    continue
                name = prefix + path
                require(safe_path(name) and name.split("/")[0] not in LOCAL,
                        f"{source.name}: the tree has a path this tool cannot write: {name}")
                files[name] = resolve(entries, read, path) if mode == "120000" else (mode, oid)
    return files, tuple(prefix for prefix, _entries in groups[1:])


def read_stamp(dest):
    try:
        stamp = json.loads((dest / STAMP).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return None
    if not isinstance(stamp, dict):
        return None
    for key, value in stamp.items():
        if key in ("executable", "local"):
            if not isinstance(value, list) or not all(isinstance(item, str) for item in value):
                return None
        elif not isinstance(value, str):
            return None
    return stamp


def write_stamp(dest, **values):
    dest.mkdir(parents=True, exist_ok=True)
    write_atomic(dest / STAMP, (json.dumps(values, sort_keys=True) + "\n").encode("ascii"),
                 temporary=dest.with_name(f".{dest.name}.patched-stamp"))


def local_names(files):
    """Names in a dest that are not its content: Cargo writes a lock file where the tree has none."""
    return LOCAL | ({"Cargo.lock"} - set(files))


def record(source, repo, index, files):
    """Keep the tree of the files as written in the repository; the stamp of a dest that holds them."""
    written = repo.tree_of(index, files)
    repo.keep(written)
    digest = hashlib.sha256()
    with repo.blobs() as read:
        for name, (_mode, oid) in sorted(files.items()):
            data = read(oid)
            digest.update(name.encode("utf-8") + b"\0" + len(data).to_bytes(8, "little"))
            digest.update(data)
    return {"crc32": crc32(source.inputs()), "executable": sorted(name for name, (mode, _oid) in files.items() if mode == "100755"),
            "files": digest.hexdigest(), "local": sorted(local_names(files) - LOCAL), "tree": source.tree, "written": written}


def matches_inputs(source, stamp):
    return stamp.get("crc32") == crc32(source.inputs()) and stamp.get("tree") == source.tree


def stamp_local(stamp):
    return LOCAL | frozenset(stamp.get("local", ()))


def inspect(source, dest):
    """(missing | current | stale | edited, the stamp) of a dest."""
    stamp = read_stamp(dest)
    if stamp is None:
        return "missing", None
    files = walk(dest, stamp_local(stamp))
    # A dest that lost every file is written again: nothing in it is an edit to keep.
    if not stamp.get("files") or all(name == "Cargo.lock" for name, _path in files):
        return "stale", stamp
    # A stamp without the list is from a version that compared no modes; Windows has none to compare.
    if os.name != "nt" and stamp.get("executable", None) not in (
            None, [name for name, path in files if file_mode(path) == "100755"]):
        return "edited", stamp
    if stamp["files"] != sha256(files):
        return "edited", stamp
    return ("current" if matches_inputs(source, stamp) else "stale"), stamp


def written_rows(source, stamp):
    """{path: (mode, blob id)} a dest was written from, or None when the cache does not hold that tree."""
    path, tree = cache_directory() / f"{source.name}.git", stamp.get("written")
    if not tree or not (path / "HEAD").is_file():
        return None
    repo = Repo(path)
    return {name: (mode, oid) for mode, oid, name in repo.tree_rows(tree)} if repo.has(tree, "tree") else None


def edits_of(source, dest, stamp):
    """How an edited dest differs from what was written, in words."""
    rows = written_rows(source, stamp)
    if rows is None:
        return "not counted: the cache does not hold the tree it was written from"
    return counted(*compare(rows, scan(dest, stamp_local(stamp), rows)))


def write_files(repo, dest, files, keep=()):
    """Make dest hold exactly `files`, and the names to keep: only changed bytes are written; (written, removed)."""
    removed = 0
    for name, path in walk(dest, local_names(files)):
        if name not in files and name not in keep:
            path.unlink()
            removed += 1
    directories = []
    for directory, inside, _names in os.walk(dest):
        if Path(directory) == dest:
            inside[:] = sorted(set(inside) - LOCAL)
        else:
            directories.append(directory)
    for directory in reversed(directories):
        if not os.listdir(directory):
            os.rmdir(directory)
    written = 0
    with repo.blobs() as read:
        for name, (mode, oid) in sorted(files.items()):
            path = dest / name
            if path.is_symlink() or not path.is_file() or blob_id(path.read_bytes()) != oid:
                if path.is_symlink() or path.is_file():
                    path.unlink()
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(read(oid))
                require(blob_id(path.read_bytes()) == oid, f"{path} does not hold the bytes of blob {oid} after it was written")
                written += 1
            if os.name != "nt" and file_mode(path) != mode:
                path.chmod(0o755 if mode == "100755" else 0o644)
    return written, removed


def carry(source, repo, index, dest, stamp, files):
    """The edits of a dest merged into `files`, the new tree: (the files to write, the patch, lines to print).

    Each edited file is merged against the tree the dest was written from. The dest is only read.
    """
    name, old = source.name, stamp.get("written", "")
    require(old and repo.has(old, "tree"),
            f"{name}: {source.dest} was edited and the series changed, and the cache does not hold the tree it was written "
            f"from: restore the series it was written from and export the edits, or --force to drop them")
    base = {path: (mode, oid) for mode, oid, path in repo.tree_rows(old)}
    disk = scan(dest, stamp_local(stamp), base)
    changed, missing, added = compare(base, disk)
    edited = sorted([*changed, *missing, *added])
    for path in edited:
        require(disk.get(path, ("",))[0] != "120000",
                f"{name}: {path} is a link, which no rebuild carries over: replace it with a file, or --force to drop the edits")
    stored = repo.store([(path, dest / path) for path in (*changed, *added)], {path: base[path][0] for path in changed})
    theirs = {**base, **{path: (mode, oid) for mode, oid, path in stored}}
    for path in missing:
        del theirs[path]
    edits = repo.tree_of(index, theirs)
    patch = cache_directory() / "edits" / f"{name}-{edits[:12]}.patch"
    patch.parent.mkdir(parents=True, exist_ok=True)
    write_atomic(patch, repo.diff(old, edits), sync=True)
    lines = [f"{name}: the series changed: carrying {len(edited)} edited files over ({counted(changed, missing, added)})",
             f"{name}: edits saved as {patch}"]
    merged, conflicts = dict(files), []
    for path in edited:
        was, new, mine = base.get(path), files.get(path), theirs.get(path)
        if new == was:
            result, how = mine, "edit kept"
        elif new == mine:
            result, how = new, "edit already in the new series"
        elif None in (was, new, mine):
            conflicts.append(path)
            continue
        else:
            mode = mine[0] if new[0] == was[0] else new[0] if mine[0] in (was[0], new[0]) else None
            oid = (new[1] if mine[1] in (was[1], new[1]) else mine[1] if new[1] == was[1]
                   else repo.merge(was[1], new[1], mine[1]))
            if mode is None or oid is None:
                conflicts.append(path)
                continue
            result, how = (mode, oid), "edit merged with the new series"
        if result is None:
            merged.pop(path, None)
        else:
            merged[path] = result
        lines.append(f"{name}: {path}: {how}")
    if conflicts:
        print("\n".join(lines[:2]))
        raise Failure(f"{name}: the edits conflict with the new series in {', '.join(conflicts)}: "
                      f"{source.dest} is unchanged; --force drops the edits, and the saved patch can be applied by hand")
    return merged, patch, lines


def refresh(source, dest, options):
    """Write one dest when it is not current. The caller holds its lock."""
    name = source.name
    condition, stamp = inspect(source, dest)
    if condition == "current":
        if not options.quiet:
            print(f"{name}: current")
        return
    carried = condition == "edited" and not options.force
    if carried:
        edits = edits_of(source, dest, stamp)
        require(not options.strict, f"{name}: {source.dest} is edited ({edits}): --strict accepts no edit")
        if matches_inputs(source, stamp):
            note(f"{name}: edited, kept ({edits})")
            return
    if condition == "missing" and not options.force:
        require(not walk(dest, LOCAL | {"Cargo.lock"}), f"{name}: {source.dest} holds files and no stamp: --force replaces them")
    if condition == "stale" and stamp.get("edits"):
        note(f"{name}: a run that did not finish was carrying edits over; they are saved as {stamp['edits']}")
    with workspace(source, options.base_dir) as (repo, index):
        tree = built(source, repo, index, options.offline, options.base_dir)
        files, _inside = layout(source, repo, index, options.offline, options.base_dir)
        final = record(source, repo, index, files)
        target, patch, lines = carry(source, repo, index, dest, stamp, files) if carried else (files, "", [])
        # An interrupted run leaves this stamp: stale to status and to the build, and safe to write over.
        write_stamp(dest, crc32="", files="", tree="", edits=str(patch))
        written, removed = write_files(repo, dest, target)
    write_stamp(dest, **final)
    for line in lines:
        print(line)
    print(f"{name}: {written} written, {removed} removed, tree {tree}")
    if lines:
        print(f"{name}: the edits are in {source.dest}: export turns them into a patch")


def lock_of(dest):
    return dest.with_name(f".{dest.name}.patched-lock")


def materialise(root, options):
    dest_root = Path(options.dest_root) if options.dest_root else Path(root)
    for name in names(root, options.names):
        source = Source(root, name)
        dest = dest_root / source.dest
        # A current dest needs no lock and no write, so one on a read-only file system passes.
        stamp = read_stamp(dest)
        if stamp is not None and matches_inputs(source, stamp) and inspect(source, dest)[0] == "current":
            if not options.quiet:
                print(f"{name}: current")
            continue
        with locked(lock_of(dest), name):
            refresh(source, dest, options)
    return 0


def verify(root, options):
    failed = 0
    for name in names(root, options.names):
        try:
            source = Source(root, name)
            with workspace(source) as (repo, index):
                base_tree, tree = build(source, repo, index)
                filled = {submodule["path"] for submodule in source.submodules}
                # An exported base holds no gitlink, so one that nothing fills must not be in the tree.
                wrong = [*differences(source, base_tree, tree),
                         *(f"{name}: gitlink {path} has no [[submodule]]: add one, or exclude the path"
                           for mode, _oid, path in repo.rows(index) if mode == "160000" and path not in filled)]
        except Failure as error:
            wrong = [str(error)]
        for line in wrong:
            note(line)
        failed += bool(wrong)
        if not wrong:
            print(f"{name}: ok")
    return 1 if failed else 0


def status(root, options):
    for name in names(root, options.names):
        source = Source(root, name)
        print(f"{name}: {inspect(source, Path(root) / source.dest)[0]}")
    return 0


def slug(subject):
    return re.sub(r"[^a-z0-9]+", "-", subject.lower()).strip("-")[:60].rstrip("-") or "patch"


def patch_date():
    seconds = os.environ.get("SOURCE_DATE_EPOCH")
    try:
        moment = (datetime.datetime.fromtimestamp(int(seconds), datetime.timezone.utc) if seconds
                  else datetime.datetime.now(datetime.timezone.utc))
    except (ValueError, OverflowError, OSError) as error:
        raise Failure(f"SOURCE_DATE_EPOCH is no time in seconds: {seconds!r}") from error
    return moment.replace(microsecond=0).isoformat()


def is_junk(path):
    return any(fnmatch.fnmatchcase(posixpath.basename(path), pattern) for pattern in JUNK)


def with_tree(source, raw, tree, binary):
    """The bytes of source.toml with a new tree and, when it grew, the binary list, in the file's aligned style."""
    updated, count = re.subn(rb'(?m)^(tree\s*=\s*")[0-9a-f]*(")', rb"\g<1>" + tree.encode("ascii") + rb"\g<2>", raw)
    require(count == 1, f'{source.name}: source.toml needs exactly one line: tree = "..."')
    if binary == source.binary:
        return updated
    line = f"{'binary':<10} = [{', '.join(json.dumps(name) for name in binary)}]\n".encode("ascii")
    listed, count = re.subn(rb"(?ms)^binary\s*=\s*\[.*?\]\n", lambda _match: line, updated)
    if not count:
        listed, count = re.subn(rb"(?m)^(?=base_tree\s*=)", lambda _match: line, updated)
    require(count == 1, f"{source.name}: source.toml needs one base_tree line, which the binary list goes before")
    return listed


def export(root, options):
    name, subject = options.name, options.message.strip()
    require(subject and not re.search(r"[\x00-\x1f\x7f]", subject), "export: the subject is one line of text")
    names(root, [name])
    source = Source(root, name)
    dest = Path(root) / source.dest
    with locked(lock_of(dest), name):
        stamp = read_stamp(dest)
        require(stamp is not None and matches_inputs(source, stamp),
                f"{name}: {source.dest} was not written from the current series: materialise it before editing")
        number = max([int(path.name[:4]) for path in source.patches() if re.match(r"\d{4}-", path.name)], default=0) + 1
        require(number <= 9999, f"{name}: the series is full at patch 9999: fold patches into one before the next export")
        patch = source.directory / "series" / f"{number:04d}-{slug(subject)}.patch"
        header = f"From {ZERO}\nSubject: {subject}\nDate: {patch_date()}\n\n---\n".encode("utf-8")
        with workspace(source) as (repo, index):
            tree = built(source, repo, index)
            files, inside = layout(source, repo, index)
            changed, missing, added = compare(files, scan(dest, local_names(files), files))
            junk = [path for path in added if is_junk(path)]
            for path in junk:
                note(f"{name}: skipped {path}: it is left in {source.dest} and is not in the patch")
            edited = [path for path in (*changed, *added) if path not in junk]
            for path in (*missing, *edited):
                require(not path.startswith(inside), f"{name}: {path} is inside a submodule; its edits cannot be exported")
            # The index holds a link where the dest holds its copy; an edited copy replaces the link with a file.
            modes = {path: mode for mode, _oid, path in repo.rows(index)}
            repo.update(index, [*repo.store([(path, dest / path) for path in edited], {path: files[path][0] for path in changed}),
                                *((modes[path], ZERO, path) for path in missing)])
            new_tree = repo.write_tree(index)
            require(new_tree != tree, f"{name}: {source.dest} has no edits")
            diff = repo.diff(tree, new_tree)
            binary = [*source.binary, patch.name] if BINARY_HUNK.search(diff) else source.binary
            require(binary == source.binary or options.binary,
                    f"{name}: the edits change binary files, which no review can read: "
                    f"--binary lists {patch.name} under binary in source.toml")
            repo.git("read-tree", tree, index=index)
            repo.git("apply", "--cached", "--whitespace=nowarn", "-", index=index, stdin=diff)
            require(repo.write_tree(index) == new_tree, f"{name}: the new patch does not reproduce the dest")
            updated = with_tree(source, source.file.read_bytes(), new_tree, binary)
            patch.parent.mkdir(exist_ok=True)
            write_atomic(patch, header + diff, sync=True)
            try:
                write_atomic(source.file, updated, sync=True)
            except BaseException:
                patch.unlink()
                raise
            source = Source(root, name)
            # Copies of links follow the files they name, so the dest is again what materialise writes.
            files = layout(source, repo, index)[0]
            final = record(source, repo, index, files)
            write_files(repo, dest, files, keep=junk)
        write_stamp(dest, **final)
    print(f"{name}: {patch.relative_to(Path(root)).as_posix()}, tree {new_tree}")
    return 0


def crate_of(path, crates):
    directory = posixpath.dirname(path)
    while directory and directory not in crates:
        directory = posixpath.dirname(directory)
    return directory or ("." if "" in crates else "(no crate)")


def divergence(root, options):
    for name in names(root, options.names):
        source = Source(root, name)
        with workspace(source) as (repo, index):
            base_tree, tree = build(source, repo, index)
            crates = {posixpath.dirname(path) for _mode, _oid, path in repo.rows(index)
                      if posixpath.basename(path) == "Cargo.toml"}
            numstat = repo.git("diff", "--numstat", "--no-renames", "-z", base_tree, tree)
        total, by_crate = [0, 0, 0], {}
        for entry in filter(None, numstat.split(b"\0")):
            added, deleted, path = entry.decode("utf-8").split("\t", 2)
            for counts in (total, by_crate.setdefault(crate_of(path, crates), [0, 0, 0])):
                counts[0] += 1
                counts[1] += 0 if added == "-" else int(added)
                counts[2] += 0 if deleted == "-" else int(deleted)
        print(f"{name}: {len(source.patches())} patches, {total[0]} files, +{total[1]} -{total[2]}")
        for crate, (files, added, deleted) in sorted(by_crate.items(), key=lambda item: (-item[1][0], item[0])):
            print(f"  {crate}: {files} files, +{added} -{deleted}")
    return 0


def main(arguments=None, root=ROOT):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    command = commands.add_parser("materialise", help="write each dest from its base and series")
    command.add_argument("names", nargs="*", metavar="NAME")
    command.add_argument("--offline", action="store_true", help="never fetch")
    command.add_argument("--base-dir", metavar="DIR", help="read exported bases from DIR/<name>/")
    command.add_argument("--dest-root", metavar="DIR", help="write below DIR instead of the repository root")
    command.add_argument("--force", action="store_true", help="replace a dest that was edited")
    command.add_argument("--strict", action="store_true", help="fail on a dest that was edited")
    command.add_argument("--quiet", action="store_true", help="print only the sources that are written")
    command.set_defaults(run=materialise)
    for name, run, text in (("verify", verify, "check that base_tree and tree reproduce"),
                            ("status", status, "missing, current, stale or edited"),
                            ("divergence", divergence, "files and lines changed against the base, by crate")):
        command = commands.add_parser(name, help=text)
        command.add_argument("names", nargs="*", metavar="NAME")
        command.set_defaults(run=run)
    command = commands.add_parser("export", help="turn the edits in a dest into the next patch")
    command.add_argument("name", metavar="NAME")
    command.add_argument("-m", "--message", required=True, metavar="SUBJECT")
    command.add_argument("--binary", action="store_true", help="accept edits to binary files and list the patch under binary")
    command.set_defaults(run=export)
    options = parser.parse_args(arguments)
    try:
        return options.run(root, options)
    except (Failure, OSError, UnicodeError) as error:
        note(error)
        return 1
    except KeyboardInterrupt:
        note("interrupted")
        return 130


if __name__ == "__main__":
    sys.exit(main())
