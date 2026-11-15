#!/usr/bin/env python3
"""Install CI packages from Ubuntu's archive without the slow Azure mirror."""
import argparse
import os
from pathlib import Path
import re
import socket
import subprocess
import time

FALLBACK_MIRROR = 'https://mirrors.edge.kernel.org/ubuntu/\tpriority:0\n'


def resolves(host):
    try:
        socket.getaddrinfo(host, 443)
    except OSError:
        return False
    return True


def configure(apt_root):
    # Hosted runners point both source formats at mirror+file:.../apt-mirrors.txt.
    # Replace the Azure entry there as well as direct URLs in older images.
    # Keep suites, components, signing keys, security and third-party sources.
    sources = [apt_root / 'apt-mirrors.txt', apt_root / 'sources.list',
               *sorted((apt_root / 'sources.list.d').glob('*.list')),
               *sorted((apt_root / 'sources.list.d').glob('*.sources'))]
    for path in sources:
        if not path.is_file():
            continue
        original = path.read_text()
        updated = re.sub(r'https?://azure\.archive\.ubuntu\.com/ubuntu(?=[/\s]|$)',
                         'https://archive.ubuntu.com/ubuntu', original)
        if updated != original:
            path.write_text(updated)
            print(f'Using the Ubuntu archive in {path.name}', flush=True)
    mirrors = apt_root / 'apt-mirrors.txt'
    if mirrors.is_file() and not resolves('archive.ubuntu.com'):
        listed = mirrors.read_text()
        if FALLBACK_MIRROR not in listed:
            mirrors.write_text(FALLBACK_MIRROR + listed)
            print('archive.ubuntu.com does not resolve; trying mirrors.edge.kernel.org first', flush=True)
    config = apt_root / 'apt.conf.d/80-bevy-ci-network'
    config.write_text('Acquire::Retries "3";\n'
                      'Acquire::http::Timeout "30";\n'
                      'Acquire::https::Timeout "30";\n')


def install(packages, apt_root=Path('/etc/apt')):
    configure(apt_root)
    # The runner is disposable: needrestart's scan and service restarts after the install only use time.
    env = {**os.environ, 'DEBIAN_FRONTEND': 'noninteractive', 'NEEDRESTART_SUSPEND': '1'}
    started = time.monotonic()
    # Do not proceed with stale indexes when any repository update fails.
    subprocess.run(['apt-get', '-o', 'APT::Update::Error-Mode=any', 'update'],
                   env=env, check=True)
    subprocess.run(['apt-get', 'install', '--yes', '--no-install-recommends', *packages],
                   env=env, check=True)
    print(f'Linux dependency installation: {time.monotonic() - started:.1f}s', flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('packages', nargs='+')
    args = parser.parse_args()
    if os.geteuid() != 0:
        parser.error('Run with sudo on the disposable Ubuntu CI runner')
    install(args.packages)


if __name__ == '__main__':
    main()
