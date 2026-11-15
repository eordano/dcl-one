#!/usr/bin/env python3
"""Install the pinned protoc compiler on the Explorer CI runners."""
import hashlib
import http.client
import io
import os
from pathlib import Path
import platform
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
import zipfile

VERSION = '36.2'
RETRY_BUDGET = 120
RETRYABLE_HTTP = {408, 429, 500, 502, 503, 504}
# Official v36.2 release archives, the version the flake's bevy-tools package gives the Linux CI
# lanes (install-tools.sh checks the major); choose the runner host, including cross builds.
ARCHIVES = {
    ('Linux', 'x86_64'): ('linux-x86_64', '121f6c7afe1d4d0e3ea6aab9432038599250134cbf4474cb1167d2c7decd4278'),
    ('Darwin', 'arm64'): ('osx-aarch_64', '9cd98a532c5c5e0c4161314de0225de27e4c8a323917b6ea7b1b714d3ae23466'),
    ('Windows', 'amd64'): ('win64', 'f0c128dc0d8492eceece83bb459a4c0e316764b929ffbf1aa416357fd644edd3'),
}


def download(url):
    deadline = time.monotonic() + RETRY_BUDGET
    delay = 1
    while True:
        try:
            with urllib.request.urlopen(url, timeout=30) as response:
                return response.read()
        # URLError and TimeoutError are OSErrors; a body cut short while reading is an HTTPException.
        except (OSError, http.client.HTTPException) as error:
            if isinstance(error, urllib.error.HTTPError) and error.code not in RETRYABLE_HTTP:
                raise
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise
            time.sleep(min(delay, remaining))
            delay = min(delay * 2, 30)


def install(system, machine, destination, cache_dir=None):
    archive, digest = ARCHIVES[(system, machine.lower())]
    cached = Path(cache_dir) / f'protoc-{VERSION}-{archive}-{digest}.zip' if cache_dir else None
    data = cached.read_bytes() if cached and cached.is_file() else None
    from_cache = data is not None and hashlib.sha256(data).hexdigest() == digest
    if not from_cache:
        data = download(f'https://github.com/protocolbuffers/protobuf/releases/download/v{VERSION}/protoc-{VERSION}-{archive}.zip')
    if hashlib.sha256(data).hexdigest() != digest:
        raise ValueError('protoc archive SHA-256 does not match the pin')
    with zipfile.ZipFile(io.BytesIO(data)) as bundle:
        bundle.extractall(destination)
    executable = destination / 'bin' / ('protoc.exe' if system == 'Windows' else 'protoc')
    executable.chmod(0o755)
    actual = subprocess.check_output([str(executable), '--version'], text=True).strip()
    if actual != f'libprotoc {VERSION}':
        raise ValueError(f'Unexpected compiler version: {actual}')
    if cached and not from_cache:
        cached.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.NamedTemporaryFile(dir=cached.parent, delete=False) as partial:
            partial.write(data)
        Path(partial.name).replace(cached)
    print(actual + (' from the cached archive' if from_cache else ''))
    return executable.parent


def main():
    destination = Path(tempfile.mkdtemp(prefix=f'bevy-protoc-{VERSION}-', dir=os.environ['RUNNER_TEMP']))
    binary_dir = install(platform.system(), platform.machine(), destination, os.environ.get('PROTOC_ARCHIVE_CACHE'))
    with open(os.environ['GITHUB_PATH'], 'a', encoding='utf-8') as path:
        path.write(f'{binary_dir}\n')


if __name__ == '__main__':
    main()
