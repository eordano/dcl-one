#!/usr/bin/env python3
"""Install the pinned CEF distribution on the Explorer CI runners.

It downloads the archive by name and checks it against a pinned SHA-256, then lays it out as the
export-cef-dir tool does: the Release files at the root of the directory with the resources, the
CMake files and the headers beside them, and an archive.json that the CEF build script reads.
The tool itself first reads the index of every CEF build, which is larger than its read limit.
"""
import hashlib
import http.client
import json
from pathlib import Path
import platform
import shutil
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request

# The CEF build that the `cef` crates in rendering/bevy-explorer/source/Cargo.lock bind (139.8.0+139.0.40).
BUILD = '139.0.40+g465474a+chromium-139.0.7258.139'
HOST = 'https://cef-builds.spotifycdn.com'
RETRY_BUDGET = 300
RETRYABLE_HTTP = {408, 429, 500, 502, 503, 504}
# The minimal distribution for each runner host, with the SHA-256 of the archive.
ARCHIVES = {
    ('Linux', 'x86_64'): ('linux64', '35eb6d36704e9483111d91471a243e6ad45f4a6c0f7e5d4fa586eefa718039ee'),
    ('Darwin', 'arm64'): ('macosarm64', '589c73886c1bcf6bdd24a1ee82d58afdb3575983a3431bf327fc6f5fc5a3dda1'),
    ('Windows', 'amd64'): ('windows64', 'e6fee513c478e023bc943008fbd2b7e470fb89d6a7d7687bc48831c01a08d951'),
}
# What export-cef-dir moves beside the Release files.
BUILD_FILES = ('CMakeLists.txt', 'cmake', 'include', 'libcef_dll', 'CREDITS.html')


def download(url, target):
    deadline = time.monotonic() + RETRY_BUDGET
    delay = 1
    # Opened once, outside the retries: a target that cannot be written fails at once.
    with open(target, 'wb') as archive:
        while True:
            archive.seek(0)
            archive.truncate()
            try:
                with urllib.request.urlopen(url, timeout=60) as response:
                    length = response.headers.get('Content-Length')
                    shutil.copyfileobj(response, archive, 1 << 20)
                # A read in pieces ends without an error when the connection closes early.
                if length is not None and archive.tell() != int(length):
                    raise http.client.IncompleteRead(b'', int(length) - archive.tell())
                return
            # URLError and TimeoutError are OSErrors; a body cut short is an HTTPException.
            except (OSError, http.client.HTTPException) as error:
                if isinstance(error, urllib.error.HTTPError) and error.code not in RETRYABLE_HTTP:
                    raise
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise
                print(f'CEF download failed ({error!r}); the next try starts in {min(delay, remaining):.0f} s', flush=True)
                time.sleep(min(delay, remaining))
                delay = min(delay * 2, 30)


def digests(path):
    sha256, sha1 = hashlib.sha256(), hashlib.sha1()
    with open(path, 'rb') as archive:
        while chunk := archive.read(1 << 20):
            sha256.update(chunk)
            sha1.update(chunk)
    return sha256.hexdigest(), sha1.hexdigest()


def install(system, machine, destination):
    target, digest = ARCHIVES[(system, machine.lower())]
    if not hasattr(tarfile, 'tar_filter'):
        raise RuntimeError('this Python has no extraction filter (tarfile.tar_filter): use Python 3.12 or newer, '
                           'or a release of an older version that has the filter')
    name = f'cef_binary_{BUILD}_{target}_minimal.tar.bz2'
    destination = Path(destination).absolute()
    destination.parent.mkdir(parents=True, exist_ok=True)
    # Beside the destination, so the last step is a rename and a failed run leaves the old directory.
    work = Path(tempfile.mkdtemp(prefix='cef-install-', dir=destination.parent))
    try:
        archive = work / name
        download(f'{HOST}/{urllib.parse.quote(name)}', archive)
        sha256, sha1 = digests(archive)
        if sha256 != digest:
            raise ValueError(f'CEF archive SHA-256 does not match the pin: {archive.stat().st_size} bytes, '
                             f'{sha256}, pinned {digest}')
        with tarfile.open(archive, 'r:bz2') as bundle:
            bundle.extractall(work, filter='tar')
        extracted = work / name.removesuffix('.tar.bz2')
        export = work / 'export'
        (extracted / 'Release').rename(export)
        # The macOS framework holds its own resources.
        if not target.startswith('macos'):
            for entry in (extracted / 'Resources').iterdir():
                entry.rename(export / entry.name)
        for entry in BUILD_FILES:
            (extracted / entry).rename(export / entry)
        (export / 'archive.json').write_text(json.dumps({'type': 'minimal', 'name': name, 'sha1': sha1}, indent=2))
        if destination.exists():
            shutil.rmtree(destination)
        export.rename(destination)
    finally:
        shutil.rmtree(work, ignore_errors=True)
    print(f'CEF {BUILD} ({target}) in {destination}')
    return destination


if __name__ == '__main__':
    install(platform.system(), platform.machine(), sys.argv[1])
