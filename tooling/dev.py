#!/usr/bin/env python3
"""Local Foundation services; invoked by the Nix shell's foundation-dev command."""
import argparse
import base64
import errno
import fcntl
import gzip
import hashlib
import json
import os
from pathlib import Path
import platform
import plistlib
import re
import secrets
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import textwrap
import time
import urllib.error
import urllib.parse
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
BACKEND = ROOT / 'services/catalyst'
STATE = ROOT / '.cache/dev'
SETTINGS = STATE / 'settings.json'
# mirror: mainnet catalogues; testnet: the same mirrors over Decentraland's testnet (.zone, Amoy and
# Sepolia), for trading with testnet wallets; offline: empty, for local deployments.
PROFILES = ('mirror', 'testnet', 'offline')
DARWIN = platform.system() == 'Darwin'
PROGRESS_SECONDS = 30
ANSI = re.compile(r'\x1b(?:\[[0-?]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\))')
# The web Explorer dials Pulse's WebTransport at the page's host and port; a page served without a port
# (https://host) goes to the engine's default (bevy-explorer crates/comms/src/pulse/plugin.rs DEFAULT_PULSE_PORT).
WEB_PULSE_PORT = 7743


def public_origin(value):
    """`FOUNDATION_DEV_PUBLIC_ORIGIN` checked: an http(s) origin and nothing more, or None for the loopback stack."""
    if value is None or value.strip() == '' or value.strip() == 'local':
        return None
    url = urllib.parse.urlsplit(value.strip())
    if url.scheme not in ('http', 'https') or not url.hostname or url.path not in ('', '/') or url.query \
            or url.fragment or url.username or url.password:
        raise ValueError('FOUNDATION_DEV_PUBLIC_ORIGIN must be an origin, https://host or https://host:port, '
                         f'got {value!r}')
    return f'{url.scheme}://{url.netloc}'


def public(config):
    """The origin the stack is served at, split: (scheme, host, netloc, explicit port or None); None when loopback."""
    origin = config.get('public_origin')
    if not origin:
        return None
    url = urllib.parse.urlsplit(origin)
    return url.scheme, url.hostname, url.netloc, url.port


def ip_literal(host):
    try:
        socket.inet_pton(socket.AF_INET6 if ':' in host else socket.AF_INET, host)
        return True
    except OSError:
        return False


def tls_files(config):
    """The developer's certificate and key for the public host (`FOUNDATION_DEV_TLS_CERT`/`_KEY`), or None."""
    if not config.get('tls_cert'):
        return None
    certificate, key = Path(config['tls_cert']), Path(config['tls_key'])
    for path in (certificate, key):
        if not path.is_file():
            raise RuntimeError(f'TLS file {path} is missing; run foundation-dev origin again with --cert and --key.')
    return certificate, key


def backoff_delays(deadline, first=.1, maximum=2):
    delay = first
    while (remaining := deadline - time.monotonic()) > 0:
        yield min(delay, remaining)
        delay = min(delay * 2, maximum)


def run(argv, **kwargs):
    return subprocess.run([str(a) for a in argv], check=True, **kwargs)


def save_json(path, value):
    temporary = path.with_suffix('.tmp')
    temporary.write_text(json.dumps(value, indent=2) + '\n')
    temporary.replace(path)


def ports_free(base, webtransport=None):
    sockets = []
    try:
        # TCP (the API's metrics on base+1, NATS on base+8 and +9), then the UDP ports: Pulse's WebTransport
        # (the Sites port, or the public origin's port on every interface) and ENet ports, LiveKit's media.
        for kind, port, address in [*((socket.SOCK_STREAM, port, '127.0.0.1') for port in (
                                        base, base + 1, base + 2, base + 3, base + 4, base + 5, base + 7,
                                        base + 8, base + 9)),
                                    *((socket.SOCK_DGRAM, port, '127.0.0.1') for port in (base + 2, base + 6, base + 7)),
                                    *([(socket.SOCK_DGRAM, webtransport, '0.0.0.0')] if webtransport else [])]:
            sock = socket.socket(socket.AF_INET, kind)
            sockets.append(sock)
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            sock.bind((address, port))
        return True
    except OSError as error:
        if error.errno == errno.EADDRINUSE:
            return False
        raise
    finally:
        for sock in sockets:
            sock.close()


def livekit_credentials():
    # The stack's own LiveKit server holds no data, so a stack created before it gets a pair on its next start.
    return {'LIVEKIT_API_KEY': 'API' + secrets.token_hex(6), 'LIVEKIT_API_SECRET': secrets.token_hex(32)}


def missing_secrets(config):
    """The generated secrets `config` lacks: the LiveKit pair, and the moderator token the API checks and Pulse
    polls its ban snapshots with (`catalyst secrets init` derives the same pair for a deployment)."""
    added = {}
    if 'LIVEKIT_API_SECRET' not in config:
        added.update(livekit_credentials())
    if 'MODERATOR_TOKEN' not in config:
        added['MODERATOR_TOKEN'] = secrets.token_hex(32)
    return added


def settings(create=False):
    if SETTINGS.exists():
        config = json.loads(SETTINGS.read_text())
        requested = os.environ.get('FOUNDATION_DEV_PORT')
        if create and requested is not None and requested != str(config['port']):
            print(f"Using saved port {config['port']}; FOUNDATION_DEV_PORT={requested} applies only on first startup. "
                  'Stop services before editing .cache/dev/settings.json.', file=sys.stderr)
        profile = os.environ.get('FOUNDATION_DEV_PROFILE')
        if create and profile is not None and profile != config['profile']:
            print(f"Using saved profile {config['profile']}; FOUNDATION_DEV_PROFILE={profile} applies only on first "
                  'startup. Change it with foundation-dev profile.', file=sys.stderr)
        if missing_secrets(config):
            # A stack from before a generated secret, on whichever command reads it first; the lock
            # keeps tasks that start together on one value.
            with SETTINGS.with_suffix('.lock').open('w') as lock:
                fcntl.flock(lock, fcntl.LOCK_EX)
                config = json.loads(SETTINGS.read_text())
                if added := missing_secrets(config):
                    config.update(added)
                    save_json(SETTINGS, config)
        return config
    data = STATE / 'postgres'
    if data.exists() and any(data.iterdir()):
        raise RuntimeError('settings.json is missing but PostgreSQL data exists. Restore the original '
                           '.cache/dev/settings.json with its database password and encryption/session keys. '
                           'Data is preserved; no new credentials were generated.')
    if not create:
        raise RuntimeError('Run foundation-dev up first.')
    base = int(os.environ.get('FOUNDATION_DEV_PORT', '5210'))
    if not 1024 <= base <= 65000:
        raise ValueError('FOUNDATION_DEV_PORT must be between 1024 and 65000')
    if 'FOUNDATION_DEV_PORT' not in os.environ:
        while base <= 65000 and not ports_free(base):
            base += 10
    if base > 65000 or not ports_free(base):
        raise RuntimeError('Development ports are occupied; choose FOUNDATION_DEV_PORT.')
    profile = os.environ.get('FOUNDATION_DEV_PROFILE', 'mirror')
    if profile not in PROFILES:
        raise ValueError('FOUNDATION_DEV_PROFILE must be mirror, testnet or offline')
    config = {'port': base, 'profile': profile, 'password': secrets.token_hex(24),
              'ENCRYPTION_KEY': secrets.token_hex(32), 'SESSION_SECRET': secrets.token_hex(32),
              **missing_secrets({}), **origin_settings(os.environ)}
    save_json(SETTINGS, config)
    return config


def origin_settings(env, origin=None, certificate=None, key=None):
    """The public-origin keys of settings.json from `FOUNDATION_DEV_PUBLIC_ORIGIN`, `_TLS_CERT` and `_TLS_KEY`
    (or the `origin` command's arguments): the origin the stack is served at behind a developer's TLS front, and
    the certificate Pulse's WebTransport presents for that host. Loopback by default."""
    origin = public_origin(origin if origin is not None else env.get('FOUNDATION_DEV_PUBLIC_ORIGIN'))
    certificate = certificate if certificate is not None else env.get('FOUNDATION_DEV_TLS_CERT')
    key = key if key is not None else env.get('FOUNDATION_DEV_TLS_KEY')
    if bool(certificate) != bool(key):
        raise ValueError('FOUNDATION_DEV_TLS_CERT and FOUNDATION_DEV_TLS_KEY go together.')
    if certificate and not origin:
        raise ValueError('A TLS certificate needs FOUNDATION_DEV_PUBLIC_ORIGIN, the host it is for.')
    settings = {'public_origin': origin}
    if certificate:
        settings.update(tls_cert=str(Path(certificate).expanduser().resolve()),
                        tls_key=str(Path(key).expanduser().resolve()))
        tls_files(settings)
    return settings


def database_url(config, database='catalyst'):
    return f"postgresql://foundation:{config['password']}@127.0.0.1:{config['port'] + 3}/{database}"


# Third-party services the scenes on this realm call from their workers: Genesis Plaza's fishing game's Colyseus
# room and the marketing files bucket. The browser explorer's scene workers reach them through the node's
# `/_scene/<host>` proxy (services/catalyst/src/scene_proxy.rs), as these hosts answer no CORS preflight.
# This is the default: a stack names its own list under `scene_proxy_hosts` in settings.json (an empty list forwards
# to no host).
SCENE_PROXY_HOSTS = ('us-mia-43272b05.colyseus.cloud', 'marketing-files.decentraland.org')


def scene_proxy_hosts(config):
    hosts = config.get('scene_proxy_hosts', SCENE_PROXY_HOSTS)
    if isinstance(hosts, str) or not all(isinstance(host, str) and host for host in hosts):
        raise SystemExit('settings.json: scene_proxy_hosts is a list of host names')
    return tuple(hosts)

# The engine's headless avatar renderer, as `make bevy-avatar-render` leaves it (apps/bevy-explorer/build-avatar-render.py
# writes the wrapper that carries its library path). Profile face and body images render here once it is built;
# FOUNDATION_DEV_AVATAR_RENDER_BIN names one built elsewhere.
AVATAR_RENDER_BIN = ROOT / 'apps/bevy-explorer/dist/avatar_render'


def avatar_render_bin():
    given = os.environ.get('FOUNDATION_DEV_AVATAR_RENDER_BIN')
    path = Path(given).expanduser() if given else AVATAR_RENDER_BIN
    return path if path.is_file() and os.access(path, os.X_OK) else None


def livekit_ports(config):
    # LiveKit's signaling (HTTP and WebSocket), then its WebRTC TCP and UDP ports, in the stack's block.
    return config['port'] + 4, config['port'] + 5, config['port'] + 6


def livekit_yaml(config):
    signaling, tcp, udp = livekit_ports(config)
    key = config['LIVEKIT_API_KEY']
    # Signaling listens on loopback, as the rest of the stack. Media cannot: browsers gather no loopback
    # ICE candidates, so the server offers this machine's own interface addresses (no STUN); a session
    # still needs a token signed with the stack's secret. The webhook drives comms presence in the API.
    return textwrap.dedent(f'''\
        port: {signaling}
        bind_addresses:
          - 127.0.0.1
        rtc:
          tcp_port: {tcp}
          udp_port: {udp}
          use_external_ip: false
        keys:
          {key}: {config['LIVEKIT_API_SECRET']}
        webhook:
          api_key: {key}
          urls:
            - http://127.0.0.1:{config['port']}/comms-gatekeeper/livekit-webhook
        logging:
          level: info
        ''')


def pulse_ports(config):
    # WebTransport (UDP) on the Sites port, the host:port the /play HUD hands the engine as its Pulse
    # server (apps/ui3/src/play/lib/baseDomain.ts); ENet (UDP) and /metrics (TCP) share base+7.
    # Behind a public origin the page's port is the origin's (the TLS front holds it over TCP only, so
    # UDP is free there), or the engine's default when the origin names none.
    origin = public(config)
    webtransport = config['port'] + 2 if origin is None else origin[3] or WEB_PULSE_PORT
    return webtransport, config['port'] + 7, config['port'] + 7


def nats_ports(config):
    # The broker's client port, then its HTTP monitor (/healthz is the readiness probe).
    return config['port'] + 8, config['port'] + 9


def nats_url(config):
    return f'nats://127.0.0.1:{nats_ports(config)[0]}'


def nats_argv(config):
    client, monitor = nats_ports(config)
    # Core NATS only: Pulse and archipelago use no JetStream, so the broker keeps nothing on disk.
    return ['nats-server', '--name', 'foundation-dev', '--addr', '127.0.0.1', '--port', str(client),
            '--http_port', str(monitor)]


def openssl(*args, **kwargs):
    return run(['openssl', *args], capture_output=True, **kwargs).stdout


def pulse_spki():
    """The base64 SHA-256 of the stack's Pulse key, the value Chrome's --ignore-certificate-errors-spki-list takes."""
    der = openssl('pkey', '-in', STATE / 'pulse/key.pem', '-pubout', '-outform', 'DER')
    return base64.b64encode(hashlib.sha256(der).digest()).decode()


def pulse_certificate(config=None):
    """Pulse's WebTransport certificate: the developer's pair for the public host when given, else a key kept
    per stack, so its SPKI hash stays stable, and a fresh self-signed certificate on every start, valid for
    13 days, naming the public host beside localhost."""
    if config is not None and (given := tls_files(config)):
        return given
    names = 'DNS:localhost,IP:127.0.0.1'
    if config is not None and (origin := public(config)) and origin[1] not in ('localhost', '127.0.0.1'):
        names = f"{'IP' if ip_literal(origin[1]) else 'DNS'}:{origin[1]},{names}"
    directory = STATE / 'pulse'
    directory.mkdir(parents=True, exist_ok=True, mode=0o700)
    key = directory / 'key.pem'
    if not key.exists():
        temporary = directory / 'key.tmp'
        temporary.unlink(missing_ok=True)
        os.close(os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600))
        openssl('genpkey', '-algorithm', 'EC', '-pkeyopt', 'ec_paramgen_curve:P-256', '-out', temporary)
        temporary.replace(key)
    certificate = directory / 'cert.pem'
    openssl('req', '-x509', '-new', '-key', key, '-out', certificate, '-days', '13', '-sha256',
            '-subj', '/CN=localhost', '-addext', f'subjectAltName={names}')
    return certificate, key


def pulse_about_env(config):
    """The /about comms block that tells the /play HUD Pulse's WebTransport endpoint. The realm's adapter
    stays the archipelago one `runtime_env` sets."""
    origin = public(config)
    host, audience = ('127.0.0.1', f'http://127.0.0.1:{config["port"] + 2}') if origin is None else (origin[1], origin[2])
    return {'COMMS_V4_PULSE_WEBTRANSPORT_URL': f'https://{host}:{pulse_ports(config)[0]}', 'COMMS_V4_PULSE_AUDIENCE': audience}


def pulse_env(config, certificate, key):
    webtransport, enet, metrics = pulse_ports(config)
    env = {name: os.environ[name] for name in ('PATH', 'HOME', 'TMPDIR') if name in os.environ}
    # WebTransport is UDP, which the TLS front cannot carry: behind a public origin the browser dials this
    # machine directly, on every interface.
    bind = '127.0.0.1' if public(config) is None else '0.0.0.0'
    return {**env, 'PULSE_BIND': f'127.0.0.1:{enet}', 'PULSE_METRICS_BIND': f'127.0.0.1:{metrics}',
            'PULSE_WT_ENABLED': 'true', 'PULSE_WT_BIND': f'{bind}:{webtransport}',
            'PULSE_WT_CERT_PATH': str(certificate), 'PULSE_WT_KEY_PATH': str(key),
            'PULSE_REPLAY_STATE_PATH': str(STATE / 'pulse/replay.tsv'), 'NO_COLOR': '1',
            # The cluster feed to the API's archipelago, and its engine.discovery heartbeat.
            'PULSE_NATS_URL': nats_url(config),
            # Pulse polls the API's moderator ban snapshot with the token the API checks.
            'COMMS_MODERATOR_TOKEN': config['MODERATOR_TOKEN'],
            'PULSE_BANS_URL': f'http://127.0.0.1:{config["port"]}/comms-gatekeeper/bans',
            # Scene listeners are admitted only from the stack's authoritative server, when it has one.
            **({'AUTHORITATIVE_SERVER_ADDRESS': config['AUTHORITATIVE_SERVER_ADDRESS']}
               if config.get('AUTHORITATIVE_SERVER_ADDRESS') else {})}


# Sign-in identifiers the browser receives (public by design, the ones decentraland.org ships): taken from the shell
# when set, so email codes, social sign-in and WalletConnect work locally. Nothing secret belongs in this list.
PUBLIC_SIGN_IN_IDS = ('THIRDWEB_CLIENT_ID', 'MAGIC_API_KEY', 'WALLETCONNECT_PROJECT_ID')


def runtime_env(config):
    # Do not inherit a shell's production database, upstreams or credentials.
    env = {key: os.environ[key] for key in (
        'PATH', 'HOME', 'USER', 'TMPDIR', 'LANG', 'LC_ALL', 'LOCALE_ARCHIVE',
        'SSL_CERT_FILE', 'NIX_SSL_CERT_FILE', *PUBLIC_SIGN_IN_IDS,
        # Sites' blog CMS base, taken from the shell when set (unset: the blog answers 503).
        # TODO(cms-migration): point it at the in-tree area once cms-server is migrated.
        'BLOG_CMS_URL',
    ) if key in os.environ}
    base = config['port']
    env.update({
        'DATABASE_URL': database_url(config), 'CATALYST_DATABASE_URL': database_url(config),
        'HTTP_SERVER_HOST': '127.0.0.1',
        'HTTP_SERVER_PORT': str(base), 'PUBLIC_URL': f'http://127.0.0.1:{base + 2}',
        # In the stack's block, not the binary's default 5006: a second stack on the machine (a testnet
        # worktree) would otherwise fail to start its API.
        'SERVE_METRICS_BIND': f'127.0.0.1:{base + 1}',
        'PG_POOL_SIZE': '12', 'SERVE_UNSYNCED': 'true',
        # The stack's own livekit-server (the `livekit` task) and its generated pair.
        'LIVEKIT_HOST': f'ws://127.0.0.1:{livekit_ports(config)[0]}',
        'LIVEKIT_API_KEY': config['LIVEKIT_API_KEY'], 'LIVEKIT_API_SECRET': config['LIVEKIT_API_SECRET'],
        'MODERATOR_TOKEN': config['MODERATOR_TOKEN'],
        'SCENE_PROXY_HOSTS': ','.join(scene_proxy_hosts(config)),
        # The stack's own nats-server (the `nats` task). The API is the stack's one primary serve (FLEET_JOBS_ENABLED
        # stays on), so it serves /ws and its island subscriber turns Pulse's cluster feed into island credentials;
        # Pulse's heartbeat makes /about's comms block healthy; the realm advertises the connector, as deployed.
        'NATS_URL': nats_url(config),
        'COMMS_FIXED_ADAPTER': f'archipelago:ws://127.0.0.1:{base}/ws',
        'COMMS_ADAPTER': f'fixed-adapter:archipelago:ws://127.0.0.1:{base}/ws',
        'REALM_NAME': 'Foundation local development',
        'SYNC_ENABLED': 'false', 'SNAPSHOT_GENERATION_INTERVAL_HOURS': '0',
        'CONTENT_HISTORY_PRUNE_ENABLED': 'false', 'WORLDS_CONTENT_MIRROR_ENABLED': 'false',
        'MARKETPLACE_MIRRORS_ENABLED': 'false', 'MARKETPLACE_NFT_INDEX_ENABLED': 'false',
        'MARKETPLACE_TRADE_EVENTS_INDEX_ENABLED': 'false', 'MARKETPLACE_MANA_TRANSFERS_INDEX_ENABLED': 'false',
        'MARKETPLACE_SALES_INDEX_ENABLED': 'false',
        'LAND_AUTHZ_INDEX_ENABLED': 'false', 'CREDITS_USAGE_INDEX_ENABLED': 'false',
        'SHOP_RENTALS_INDEX_ENABLED': 'false', 'PRICE_POLL_ENABLED': 'false',
        'COMMUNITIES_MIRROR_UPSTREAM': 'false', 'SCENE_HOST_ENABLED': 'false',
        'PRESENCE_COLLECTOR_ENABLED': 'false', 'JOBS_FIRST_PASS_NOW': 'true',
        'CONTENT_FORWARD_UPSTREAM_URL': '', 'WORLDS_FORWARD_UPSTREAM_URL': '',
        'STORAGE_ROOT_FOLDER': str(STATE / 'content'),
        'AB_CDN_CACHE_DIR': str(STATE / 'ab-cdn'), 'AB_CDN_CACHE_MAX_BYTES': '536870912',
        'PROFILE_IMAGES_CACHE_DIR': str(STATE / 'profile-images'),
        'PROFILE_IMAGES_CACHE_MAX_BYTES': '134217728',
        'BVIMPOSTERS_STORE_ROOT': str(STATE / 'imposters'),
        'BVIMPOSTERS_STORE_MAX_BYTES': '134217728',
        'SESSION_SECRET': config['SESSION_SECRET'], 'ENCRYPTION_KEY': config['ENCRYPTION_KEY'],
        'HOST': '127.0.0.1', 'PORT': str(base + 2),
        'CATALYST_URL': f'http://127.0.0.1:{base}',
        'DCL_CATALYST_PROXY_TARGET': f'http://127.0.0.1:{base}',
    })
    for domain in ('CONTENT', 'CREATE', 'DATA', 'SOCIAL', 'WORLDS', 'OPS'):
        env[f'{domain}_WRITES_ENABLED'] = 'true'
    if renderer := avatar_render_bin():
        env.update({'PROFILE_IMAGES_RENDER_ENABLED': 'true', 'PROFILE_IMAGES_RENDERER': 'bevy',
                    'PROFILE_IMAGES_AVATAR_RENDER_BIN': str(renderer)})
    if origin := public(config):
        # Served behind a developer's TLS front at this origin: the realm advertises it, not loopback. The
        # front terminates TLS for Sites (which proxies the API routes), `/ws` to the API and `/rtc` to
        # LiveKit's signaling; the API still reaches LiveKit and PostgreSQL over loopback.
        scheme, host, netloc, _ = origin
        ws = 'wss' if scheme == 'https' else 'ws'
        env.update({
            'PUBLIC_URL': f'{scheme}://{netloc}',
            'COMMS_FIXED_ADAPTER': f'archipelago:{ws}://{netloc}/ws',
            'COMMS_ADAPTER': f'fixed-adapter:archipelago:{ws}://{netloc}/ws',
            'LIVEKIT_WS_URL': f'{ws}://{netloc}',
            # Vite refuses an unknown Host header; the front forwards the origin's, and a front that also
            # serves the node under subdomains (a leading dot allows them all).
            'DEV_ALLOWED_HOSTS': host if ip_literal(host) else f'.{host}',
            # The API binds loopback, so its only peer is the front on this host: trusting it gives the
            # `/contents` rate limit and the deploy and referral quotas each client's own address again.
            'HTTP_TRUSTED_PROXY_CIDRS': '127.0.0.1/32,::1/128',
        })
    # Explicit development settings, without a production posture/restore gate.
    # Authentication and mirrored-row ownership checks still apply.
    if config['profile'] == 'offline':
        env['NODE_POSTURE'] = 'journey'
    else:
        env.update({
            'READ_THROUGH_ENABLED': 'true',
            'SYNC_SOURCE': 'https://peer.decentraland.org/content,https://peer-eu1.decentraland.org/content',
            # This unsynced copy of the peers still takes the user's own signed profile deploys
            # (/play saves the avatar); every other type answers the bootstrapping 503.
            'SERVE_UNSYNCED_DEPLOY_TYPES': 'profile',
            'PLACES_MIRROR_UPSTREAM': 'true', 'PLACES_UPSTREAM_URL': 'https://places.decentraland.org',
            'WORLDS_MIRROR_UPSTREAM': 'true', 'WORLDS_UPSTREAM_URL': 'https://places.decentraland.org',
            # No worlds content pass (WORLDS_CONTENT_MIRROR_ENABLED=false): a world is read
            # through from this source, verified, the first time it is asked for.
            'WORLDS_CONTENT_MIRROR_SOURCE': 'https://worlds-content-server.decentraland.org',
            # A scene signed by a deployer the owner granted upstream: on the upstream's word.
            'WORLDS_READ_THROUGH_TRUST_UPSTREAM_GRANTS': 'true',
            'EVENTS_MIRROR_UPSTREAM': 'true', 'EVENTS_UPSTREAM_URL': 'https://events.decentraland.org',
            'EVENTS_POSTER_UPSTREAM_URL': 'https://events-assets-099ac00.decentraland.org',
            'EXPLORER_MIRRORS_ENABLED': 'true',
            'EXPLORER_LISTS_SOURCE_URL': 'https://dcl-lists.decentraland.org',
            'EXPLORER_DENYLIST_SOURCE_URL': 'https://config.decentraland.org/denylist.json',
            'EXPLORER_FEATURE_FLAGS_SOURCE_URL': 'https://feature-flags.decentraland.org/explorer.json',
            'EXPLORER_REGISTRY_SOURCE_URL': 'https://asset-bundle-registry.decentraland.org',
            'ABGEN_UPSTREAM_AB_CDN': 'https://ab-cdn.decentraland.org',
            # Mirrored place and event images come from the upstream peers' content URLs.
            'MEDIA_CONVERT_REMOTE_SOURCES': 'true',
            # Avatar faces of profiles this node holds no snapshot files of come from the upstream
            # service into the blob store: the only source until `make bevy-avatar-render` builds the
            # renderer (avatar_render_bin), the last resort once it has.
            'PROFILE_IMAGES_ORIGIN_URL': 'https://profile-images.decentraland.org',
            # The Shop's catalogue: the marketplace mirrors and the NFT and LAND
            # indexes `catalyst posture show mirror` names, run by the index process.
            'MARKETPLACE_MIRRORS_ENABLED': 'true',
            'TRADES_SYNC_UPSTREAM_URL': 'https://marketplace-api.decentraland.org/v1/trades',
            # The Shop's feeds wait for one complete trades pass; take it in one go rather than
            # 5,000 trades per 15-minute tick (about an hour instead of four on a fresh stack).
            'TRADES_SYNC_PAGES_PER_RUN': '0',
            # Development only: the Shop reads the order book while the first pass is still running.
            'TRADES_SERVE_PARTIAL': 'true',
            'ETH_COLLECTIONS_SUBGRAPH_URL': 'https://subgraph.decentraland.org/collections-ethereum-mainnet',
            'MATIC_COLLECTIONS_SUBGRAPH_URL': 'https://subgraph.decentraland.org/collections-matic-mainnet',
            'MARKETPLACE_SUBGRAPH_URL': 'https://subgraph.decentraland.org/marketplace',
            'LAND_SUBGRAPH_URL': 'https://subgraph.decentraland.org/land-manager',
            'MARKETPLACE_NFT_INDEX_ENABLED': 'true', 'LAND_AUTHZ_INDEX_ENABLED': 'true',
            'SHOP_RENTALS_INDEX_ENABLED': 'true',
            'ETH_RPC_URL': 'https://rpc.decentraland.org/mainnet',
            'BUILDER_CONTENT_BUCKET_URL': 'https://builder-items.decentraland.org',
            # Scene-template models (Qm... CIDs) live in the asset-packs bucket, which this
            # route redirects to; the builder pulls from it when the item bucket misses.
            'BUILDER_ASSET_PACKS_BUCKET_URL': 'https://builder-api.decentraland.org/v1/storage',
            # Credit prices need a MANA spot; the map (Places, the HUD's map, estate images) its grid.
            'PRICE_POLL_ENABLED': 'true', 'COINGECKO_URL': 'https://api.coingecko.com/api/v3',
            'PRICE_FALLBACK_SOURCES': 'coinbase=https://api.coinbase.com,binance=https://api.binance.com,'
                                      'kraken=https://api.kraken.com',
            'EXPLORER_MAP_SOURCE_URL': 'https://api.decentraland.org',
            'RPC_ENDPOINT_POLYGON': 'https://rpc.decentraland.org/polygon',
        })
    if config['profile'] == 'testnet':
        env.update(testnet_env())
    return env


def testnet_env():
    """The mirror profile's upstreams on Decentraland's testnet: .zone services, Amoy and Sepolia.

    The Shop settles on Amoy (VITE_SHOP_CHAIN_ID, read by the Sites dev server), its order book is the
    .zone marketplace API's, and POST /v1/trades checks ownership over the testnet RPCs. Places, events,
    worlds and the map stay on their production sources, but ETH_NETWORK and SYNC_SOURCE move content
    read-through, LAND authorization and the node's network id to Sepolia and the .zone peer, so /play
    does not resolve mainnet scenes on this profile.

    The trade-events index walks Amoy and Sepolia's marketplaces (MARKETPLACE_INDEX_CHAIN_ID_*), so a
    trade signed into this stack leaves `open` once it is cancelled, replaced or executed on chain."""
    return {
        'ETH_NETWORK': 'sepolia',
        'SYNC_SOURCE': 'https://peer.decentraland.zone/content',
        'TRADES_SYNC_UPSTREAM_URL': 'https://marketplace-api.decentraland.zone/v1/trades',
        'ETH_COLLECTIONS_SUBGRAPH_URL': 'https://subgraph.decentraland.org/collections-ethereum-sepolia',
        'MATIC_COLLECTIONS_SUBGRAPH_URL': 'https://subgraph.decentraland.org/collections-matic-amoy',
        'MARKETPLACE_SUBGRAPH_URL': 'https://subgraph.decentraland.org/marketplace-sepolia',
        'LAND_SUBGRAPH_URL': 'https://subgraph.decentraland.org/land-manager-sepolia',
        'ETH_RPC_URL': 'https://rpc.decentraland.org/sepolia',
        'RPC_ENDPOINT_POLYGON': 'https://rpc.decentraland.org/amoy',
        'TRADE_RPC_URLS': '80002=https://rpc.decentraland.org/amoy,11155111=https://rpc.decentraland.org/sepolia',
        # Gasless approvals and cancels: the local relay admits Amoy's Decentraland contracts (the bundled
        # contract-addresses list under `amoy`) and forwards to the .zone transactions API, which pays testnet
        # gas. COLLECTIONS_CHAIN_ID also keys the chain the relay verifies signatures and collections on.
        'COLLECTIONS_CHAIN_ID': '80002',
        'CONTRACT_ADDRESSES_CHAIN_KEY': 'amoy',
        # The published list: the bundled copy predates MarketplaceV5, the off-chain V3 marketplace whose
        # cancelSignature the Shop relays.
        'CONTRACT_ADDRESSES_URL': 'https://contracts.decentraland.org/addresses.json',
        'TRANSACTIONS_UPSTREAM_URL': 'https://transactions-api.decentraland.zone',
        # Cancels and executions on chain close local trades: the trade-events index on the testnets,
        # through TRADE_RPC_URLS' 80002 and 11155111 entries (it checks each endpoint's eth_chainId).
        'MARKETPLACE_TRADE_EVENTS_INDEX_ENABLED': 'true',
        'MARKETPLACE_INDEX_CHAIN_ID_MATIC': '80002',
        'MARKETPLACE_INDEX_CHAIN_ID_ETHEREUM': '11155111',
        # The NFT index walks chains 1/137 only, with no chain-id check: Matic has no endpoint here, and Ethereum
        # would read Sepolia through ETH_RPC_URL and store its logs as mainnet rows.
        'MARKETPLACE_NFT_INDEX_ENABLED': 'false',
        # The Rentals contract is mainnet's; there is no chain-1 endpoint here.
        'SHOP_RENTALS_INDEX_ENABLED': 'false',
        'VITE_SHOP_CHAIN_ID': '80002',
        # The catalogue's chainId for MATIC items, and the chain the Shop's coupons pair with: the pages ask the
        # node to settle on it, and the node refuses a catalogue chain that differs from COLLECTIONS_CHAIN_ID.
        'POLYGON_CHAIN_ID': '80002',
        # A USD-priced trade settles at the MANA/USD feed of the marketplace it runs on. Amoy's off-chain
        # marketplaces read this one (manaUsdAggregator()) and refuse a round older than 27 s
        # (manaUsdAggregatorTolerance()), so the node checks the same age before it converts.
        'MANA_USD_AGGREGATOR_ADDRESS': '0xdcf00f5f60b62b07e668a84c0cedaf6f453d416e',
        'USD_PEGGED_ORACLE_MAX_AGE_SECS': '27',
        # Credits settle through dcl-one's own CreditsManagerPolygon on Amoy. The settlement gate refuses the
        # shared upstream manager (0x8052…2fb3 in contracts.decentraland.org's `amoy` list), so this one is
        # ours: not paused, primary sales on, secondary sales off, no NAME executor allowlisted, funded with
        # test MANA, and CREDITS_SIGNER_ROLE held by the testnet stack's credits signer (a stack whose key lacks
        # it is refused at authorization). The usage index reads its CreditUsed events over RPC_ENDPOINT_POLYGON
        # on COLLECTIONS_CHAIN_ID; POST /credits/authorize signs only while that index is ready. Mock card
        # top-ups grant test credits with no charge.
        # The relay doesn't admit this manager yet: it takes the contracts CONTRACT_ADDRESSES_URL lists (Amoy's
        # entry is the shared one) and verified collections. So a self-custody wallet buys with Credits through
        # its own transaction (the checkout falls back to it, paying its own gas); an in-app (managed) wallet,
        # which can only relay, can't buy with Credits on this profile until the relay admits the manager.
        'CREDITS_MANAGER_CONTRACT': '0xb65fd692d15c30c3adcacdd5ccf80e1facecfc7c',
        # The same manager, for the Sites build: the in-world Shop's shell refuses a credit signed for any other.
        'VITE_SHOP_CREDITS_MANAGER': '0xb65fd692d15c30c3adcacdd5ccf80e1facecfc7c',
        'CREDITS_USAGE_INDEX_ENABLED': 'true',
        # The index replays from the block that deployed the manager. Unset, it starts where the shared one was
        # deployed (block 20612932), over 31 million blocks before this manager existed.
        'CREDITS_USAGE_START_BLOCK': '52152027',
        # Free test credits. With the signer key below they are spent through that manager, which pays testnet
        # MANA only. This profile sets the mock card on Amoy alone, and the stack listens on 127.0.0.1.
        'CREDITS_MOCK_CARD': 'true',
        **credits_signer_env(),
        **name_simulation_env(),
    }


def name_simulation_env():
    """The in-world Shop's NAME purchase rehearsal, off unless FOUNDATION_DEV_NAME_SIMULATION=1 asks for it.

    It turns on VITE_SHOP_NAME_SIMULATION for the Sites build, whose /play shell then rehearses BUY NAME on
    Amoy with the real price, balance and availability, and buys nothing (apps/ui3/src/play/shell/shop/
    namePurchase.ts). Only this profile sets it, with VITE_SHOP_CHAIN_ID=80002, which the shell needs too."""
    return {'VITE_SHOP_NAME_SIMULATION': 'true'} if os.environ.get('FOUNDATION_DEV_NAME_SIMULATION') == '1' else {}


def credits_signer_env():
    """On-chain credit authorization, when the stack's state holds a signer key.

    The operator places the key in the gitignored stack state (credits-signer.key). It has to hold
    CREDITS_SIGNER_ROLE on the profile's CreditsManager (CREDITS_MANAGER_CONTRACT), or the contract rejects
    every credit it signs; the manager's admin grants it."""
    key = STATE / 'credits-signer.key'
    # An empty file reads as no key on the server, so it must not turn authorization on.
    if not key.is_file() or not key.read_text().strip():
        return {'CREDITS_ONCHAIN_AUTHORIZE_ENABLED': 'false'}
    if key.stat().st_mode & 0o077:
        print(f'Warning: {key} is readable by other users; chmod 600 it.', file=sys.stderr)
    return {'CREDITS_ONCHAIN_AUTHORIZE_ENABLED': 'true', 'CREDITS_SIGNER_KEY_FILE': str(key)}


def socket_path():
    current = STATE / 'pc.sock'
    # Allow an already running supervisor to be stopped after updating tooling.
    digest = hashlib.sha256(str(ROOT).encode()).hexdigest()[:16]
    legacy = Path('/tmp') / f'foundation-dev-{os.getuid()}-{digest}' / 'pc.sock'
    if not current.exists() and legacy.exists():
        if legacy.parent.is_symlink() or legacy.parent.stat().st_uid != os.getuid():
            raise RuntimeError('Development socket directory is not owned by this user')
        return legacy
    return current


def pc_argv(*args):
    path = STATE / 'pc.sock' if args and args[0] == 'up' else socket_path()
    # A relative Unix socket address avoids Darwin's path length limit while
    # keeping the socket in persistent checkout state, outside temporary dirs.
    address = path.name if path.parent == STATE else str(path)
    logfile = 'supervisor.log' if args and args[0] == 'up' else 'supervisor-client.log'
    return ['process-compose', '--use-uds', '--unix-socket', address, '--log-file', str(STATE / logfile), *args]


def pc_env(env=None):
    xdg = STATE / 'xdg'
    # This fallback directory quiets missing-config noise without shadowing host settings.
    (xdg / 'process-compose').mkdir(parents=True, exist_ok=True)
    env = dict(os.environ if env is None else env)
    env['XDG_CONFIG_DIRS'] = f"{env.get('XDG_CONFIG_DIRS') or '/etc/xdg'}:{xdg}"
    return env


def pc(*args, **kwargs):
    STATE.mkdir(parents=True, exist_ok=True, mode=0o700)
    return subprocess.run(pc_argv(*args), cwd=STATE, env=pc_env(kwargs.pop('env', None)), **kwargs)


def supervisor_running():
    return pc('process', 'list', stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode == 0


def project(config, postgres_only=False):
    def command(task):
        return 'exec ' + shlex.join([sys.executable, str(ROOT / 'tooling/dev.py'), '_task', task])

    def process(task, dependencies=None):
        result = {'command': command(task), 'working_dir': str(ROOT),
                  'log_location': str(STATE / f'{task}.log'),
                  'log_configuration': {'disable_json': True, 'no_color': True,
                                        'no_metadata': True, 'fields_order': ['message'],
                                        'flush_each_line': True,
                                        'rotation': {'max_size_mb': 10, 'max_backups': 3}},
                  'shutdown': {'timeout_seconds': 30}}
        if dependencies:
            result['depends_on'] = {name: {'condition': condition} for name, condition in dependencies.items()}
        return result

    postgres = process('postgres')
    postgres['shutdown']['signal'] = 2  # PostgreSQL fast shutdown; no orphaned sessions.
    # The probe starts Python and psql; on a loaded machine that takes longer than the 1 s default, and
    # 60 timed-out probes in a row make process-compose stop a healthy PostgreSQL.
    postgres['readiness_probe'] = {'exec': {'command': command('database-ready')},
                                   'period_seconds': 1, 'timeout_seconds': 15, 'failure_threshold': 60}
    if postgres_only:
        return {'version': '0.5', 'processes': {'postgres': postgres}}
    api = process('api', {'postgres': 'process_healthy', 'api-build': 'process_completed_successfully',
                          'web-install': 'process_completed_successfully', 'nats': 'process_healthy'})
    api['readiness_probe'] = {'http_get': {'host': '127.0.0.1', 'port': config['port'], 'path': '/about'},
                             'period_seconds': 2, 'failure_threshold': 1800}
    pulse = process('pulse', {'api-build': 'process_completed_successfully', 'nats': 'process_healthy'})
    pulse['readiness_probe'] = {'http_get': {'host': '127.0.0.1', 'port': pulse_ports(config)[2], 'path': '/metrics'},
                                'period_seconds': 1, 'failure_threshold': 60}
    livekit = process('livekit')
    livekit['readiness_probe'] = {'http_get': {'host': '127.0.0.1', 'port': livekit_ports(config)[0], 'path': '/'},
                                  'period_seconds': 1, 'failure_threshold': 60}
    nats = process('nats')
    nats['readiness_probe'] = {'http_get': {'host': '127.0.0.1', 'port': nats_ports(config)[1], 'path': '/healthz'},
                               'period_seconds': 1, 'failure_threshold': 60}
    sites = process('sites', {'web-install': 'process_completed_successfully'})
    sites['readiness_probe'] = {'http_get': {'host': '127.0.0.1', 'port': config['port'] + 2, 'path': '/'},
                               'period_seconds': 2, 'failure_threshold': 1800}
    processes = {
        'postgres': postgres,
        'api-build': process('api-build', {'postgres': 'process_healthy'}), 'api': api,
        'web-install': process('web-install'), 'sites': sites, 'livekit': livekit, 'pulse': pulse,
        'nats': nats,
    }
    if config['profile'] in ('mirror', 'testnet'):
        # The marketplace mirrors and chain indexes run in `catalyst index`, beside `serve`. The CreditUsed index
        # runs in `serve`: its fold debits the credits ledger.
        processes['index'] = process('index', {'postgres': 'process_healthy',
                                               'api-build': 'process_completed_successfully'})
    return {'version': '0.5', 'processes': processes}


def sql(config, query, database='catalyst'):
    env = runtime_env(config)
    env.update(PGHOST='127.0.0.1', PGPORT=str(config['port'] + 3), PGUSER='foundation',
               PGPASSWORD=config['password'], PGDATABASE=database)
    return run(['psql', '-X', '-v', 'ON_ERROR_STOP=1', '-Atq'], input=query, text=True,
               capture_output=True, env=env).stdout.strip()


def postgres(config):
    env = runtime_env(config)
    data = STATE / 'postgres'
    if not (data / 'PG_VERSION').exists():
        if data.exists() and any(data.iterdir()):
            raise RuntimeError('Incomplete PostgreSQL initialization; data preserved. Inspect .cache/dev/postgres.')
        password_file = STATE / 'pg-password'
        password_file.write_text(config['password'])
        try:
            run(['initdb', '-D', data, '-U', 'foundation', '--auth-local=trust',
                 '--auth-host=scram-sha-256', '--pwfile', password_file, '--encoding=UTF8', '--locale=en_US.UTF-8'], env=env)
        finally:
            password_file.unlink(missing_ok=True)
    version = run(['postgres', '--version'], capture_output=True, text=True, env=env).stdout.split()[2].split('.')[0]
    if (data / 'PG_VERSION').read_text().strip() != version:
        raise RuntimeError('PostgreSQL major version changed. Data preserved; migrate it before restarting.')
    os.execvpe('postgres', ['postgres', '-D', str(data), '-h', '127.0.0.1',
                           '-p', str(config['port'] + 3), '-k', '',
                           '-c', 'max_connections=400', '-c', 'shared_buffers=64MB',
                           # Concurrent test binaries apply the schema at once; an apply holds thousands of locks.
                           '-c', 'max_locks_per_transaction=256'], env)


def ensure_collation(config, database='catalyst'):
    # Linux imports this name from glibc at initdb. Darwin calls its native
    # UTF-8 locale en_US.UTF-8; expose the name used by the existing queries in
    # pg_catalog, the one schema every search_path resolves (the API pins
    # search_path=content,meta, so a public collation is invisible to it).
    # Never replace a collation in an existing database.
    exists = sql(config, "SELECT EXISTS (SELECT 1 FROM pg_collation WHERE collname='en_US.utf8' "
                         "AND collnamespace='pg_catalog'::regnamespace)", database)
    if exists != 't':
        sql(config, 'CREATE COLLATION pg_catalog."en_US.utf8" (provider=libc, locale=\'en_US.UTF-8\')', database)


def apply_schema(config, binary):
    # Source saves can change schema, so every API start checks it. Serialize
    # the entire bootstrap/migrate sequence against an explicit api-build retry.
    with (STATE / 'schema.lock').open('w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if sql(config, "SELECT 1 FROM pg_database WHERE datname='catalyst'", 'postgres') != '1':
            sql(config, 'CREATE DATABASE catalyst', 'postgres')
        ensure_collation(config)
        # Test scratch databases are created from template1 and need it too.
        ensure_collation(config, 'template1')
        stamped = sql(config, "SELECT to_regclass('meta.schema') IS NOT NULL") == 't'
        run([binary, 'schema', 'migrate' if stamped else 'apply'], env=runtime_env(config))


# The trades mirror's tables, in foreign-key order. A snapshot of them lets a fresh stack skip most
# of the first trades pass (77,580 detail fetches): the mirror then only fetches what is newer.
TRADES_TABLES = ('shop.trades', 'shop.trade_assets', 'shop.trade_assets_erc20',
                 'shop.trade_assets_erc721', 'shop.trade_assets_item')


def psql_env(config):
    env = runtime_env(config)
    env.update(PGHOST='127.0.0.1', PGPORT=str(config['port'] + 3), PGUSER='foundation',
               PGPASSWORD=config['password'], PGDATABASE='catalyst')
    return env


def export_trades_snapshot(config, out):
    # Mirrored rows only; a trade signed into this stack stays local.
    tables = [arg for table in TRADES_TABLES for arg in ('--table', table)]
    dump = subprocess.run(['pg_dump', '--data-only', '--no-owner', '--no-privileges', *tables],
                          env=psql_env(config), check=True, capture_output=True).stdout
    with gzip.open(out, 'wb') as file:
        file.write(dump)
    print(f'Wrote {out} ({Path(out).stat().st_size} bytes); its sha256 is {hashlib.sha256(Path(out).read_bytes()).hexdigest()}')


def trades_snapshot_file():
    # FOUNDATION_DEV_TRADES_SNAPSHOT names a local file (`foundation-dev trades-snapshot` writes one).
    local = os.environ.get('FOUNDATION_DEV_TRADES_SNAPSHOT')
    return Path(local) if local else None


def seed_trades(config):
    if config['profile'] != 'mirror' or sql(config, 'SELECT count(*) FROM shop.trades') != '0':
        return
    snapshot = trades_snapshot_file()
    if snapshot is None:
        return
    with gzip.open(snapshot, 'rb') as file:
        subprocess.run(['psql', '-X', '-v', 'ON_ERROR_STOP=1', '-q', '--single-transaction'],
                       input=file.read(), env=psql_env(config), check=True, stdout=subprocess.DEVNULL)
    print(f"Seeded {sql(config, 'SELECT count(*) FROM shop.trades')} trades from {snapshot.name}.")


def seed_dev_flags(config):
    # The node refuses the Shop's Marketplace writes (resales, bids, lists, rental listings) while the
    # dapps-shop-marketplace-features rollout is off, and a development stack reads no flags service: it
    # counts only this node's overrides. A development stack and the trade journey exercise those writes,
    # so they start with the override on; an override a developer set (either way) is kept.
    sql(config, "INSERT INTO explore_api.feature_flags_local (app, name, value) "
                "VALUES ('dapps', 'dapps-shop-marketplace-features', true) ON CONFLICT (app, name) DO NOTHING")


# Creator Hub's asset catalogue, from the @dcl/asset-packs package Sites installs; the files it
# names are pulled from BUILDER_CONTENT_BUCKET_URL (then BUILDER_ASSET_PACKS_BUCKET_URL) on first read.
ASSET_PACKS_CATALOG = ROOT / 'apps/sites/node_modules/@dcl/asset-packs/catalog.json'


def import_asset_catalog(config, binary):
    if not ASSET_PACKS_CATALOG.exists():
        return
    if sql(config, 'SELECT count(*) FROM builder.asset_catalog') != '0':
        return
    run([binary, 'builder', 'catalog', ASSET_PACKS_CATALOG], env=runtime_env(config))


# Long-running tasks and the signal that stops each one cleanly.
SERVICES = {'postgres': signal.SIGINT, 'api': signal.SIGTERM, 'sites': signal.SIGTERM, 'index': signal.SIGTERM,
            'livekit': signal.SIGTERM, 'pulse': signal.SIGTERM, 'nats': signal.SIGTERM}


def started(pid):
    # A process's start time, or '' once it has exited (zombies included).
    state, _, start = subprocess.run(['ps', '-o', 'stat=,lstart=', '-p', str(pid)],
                                     capture_output=True, text=True).stdout.strip().partition(' ')
    return '' if state.startswith('Z') else start.strip()


def reap_orphans():
    # process-compose starts tasks in their own process groups, so a supervisor
    # killed outright leaves them running; launchd has no cgroup to clean up.
    # Only a process with the recorded PID and start time is signalled.
    for name, stop in SERVICES.items():
        path = STATE / 'tasks' / f'{name}.json'
        if not path.exists():
            continue
        record = json.loads(path.read_text())
        pid = record['pid']
        if record['started'] and started(pid) == record['started']:
            print(f'Stopping {name} (PID {pid}) left by a supervisor that exited abnormally.', file=sys.stderr)
            group = name != 'postgres' and os.getpgid(pid) == pid
            for signum in (stop, signal.SIGKILL):
                (os.killpg if group else os.kill)(pid, signum)
                deadline = time.monotonic() + 30
                for delay in backoff_delays(deadline):
                    if started(pid) != record['started']:
                        break
                    time.sleep(min(delay, max(0, deadline - time.monotonic())))
                if started(pid) != record['started']:
                    break
        path.unlink()


def watch(*paths):
    return [arg for path in paths for arg in ('--watch', path)]


def task(name):
    config = settings()
    if name in SERVICES:
        # The PID survives the exec into the service itself.
        (STATE / 'tasks').mkdir(exist_ok=True, mode=0o700)
        save_json(STATE / 'tasks' / f'{name}.json', {'pid': os.getpid(), 'started': started(os.getpid())})
    if name == 'postgres':
        postgres(config)
    elif name == 'database-ready':
        sql(config, 'SELECT 1', 'postgres')
    elif name == 'web-install':
        for app in ('ui3', 'sites'):
            run(['pnpm', '--dir', ROOT / 'apps' / app, 'install', '--frozen-lockfile'])
        # Our built Explorer, verified against the source/toolchain/packaging inputs.
        run([sys.executable, ROOT / 'tooling/fetch-play.py'])
    elif name == 'sites':
        os.chdir(ROOT / 'apps/sites')
        os.execvpe('pnpm', ['pnpm', 'dev'], runtime_env(config))
    elif name == 'api':
        os.chdir(BACKEND)
        # Watch source inputs only. Failed edits remain visible in api.log and
        # the next save retries; dependencies and Cargo artifacts stay cached.
        os.execvp('watchexec', ['watchexec', '--restart', '--debounce', '500ms',
                  *watch('src', *sorted(p.parent.name for p in BACKEND.glob('*/Cargo.toml')),
                         '../../schemas', '../../library', 'Cargo.toml', 'Cargo.lock'),
                  '--exts', 'rs,toml,sql,lock', '--', sys.executable, str(ROOT / 'tooling/dev.py'), '_task', 'api-run'])
    elif name == 'livekit':
        # The config travels in the environment, so the secret never shows in ps.
        env = {key: os.environ[key] for key in ('PATH', 'HOME', 'TMPDIR') if key in os.environ}
        os.execvpe('livekit-server', ['livekit-server'], {**env, 'LIVEKIT_CONFIG': livekit_yaml(config)})
    elif name == 'nats':
        env = {key: os.environ[key] for key in ('PATH', 'HOME', 'TMPDIR') if key in os.environ}
        os.execvpe('nats-server', nats_argv(config), env)
    elif name == 'pulse':
        certificate, key = pulse_certificate(config)
        binary = BACKEND / 'target/debug/catalyst'
        os.execve(binary, [str(binary), 'pulse'], pulse_env(config, certificate, key))
    elif name == 'index':
        binary = BACKEND / 'target/debug/catalyst'
        os.execve(binary, [str(binary), 'index'], runtime_env(config))
    elif name in ('api-build', 'api-run'):
        target = BACKEND / 'target'
        run(['cargo', 'build', '--locked', '--bin', 'catalyst', '--target-dir', target], cwd=BACKEND)
        binary = target / 'debug/catalyst'
        apply_schema(config, binary)
        if name == 'api-build':
            # Before the index process starts mirroring, so the snapshot's rows are not fetched again.
            seed_trades(config)
            seed_dev_flags(config)
            return
        import_asset_catalog(config, binary)
        os.execve(binary, [str(binary), 'serve'], {**runtime_env(config), **pulse_about_env(config)})


def status():
    config = settings()
    print(f"Sites: http://127.0.0.1:{config['port'] + 2}   API: http://127.0.0.1:{config['port']}   "
          f"LiveKit: ws://127.0.0.1:{livekit_ports(config)[0]}   Pulse: https://127.0.0.1:{pulse_ports(config)[0]} (UDP)   "
          f"NATS: {nats_url(config)}")
    if origin := public(config):
        tls = tls_files(config)
        print(f"Public: {config['public_origin']}   /ws and /rtc through the front; Pulse UDP {origin[1]}:{pulse_ports(config)[0]}"
              + (f'   TLS: {tls[0]}' if tls else ''))
    if (STATE / 'pulse/key.pem').exists() and not tls_files(config):
        # The generated certificate is self-signed: Chrome honours an SPKI exception for WebTransport only with
        # --webtransport-developer-mode, and the exception list only beside --user-data-dir.
        print(f'Pulse (avatars) needs Chrome run with --user-data-dir=<dir> --webtransport-developer-mode --ignore-certificate-errors-spki-list={pulse_spki()}')
    print(f"Data: {STATE}   Profile: {config['profile']}", flush=True)
    return pc('process', 'list', '--output', 'wide').returncode


def prepare_supervisor(postgres_only=False):
    """Write the process graph; returns the supervisor's `up` arguments and environment, or None if it runs."""
    config = settings(create=True)
    if supervisor_running():
        running = json.loads((STATE / 'process-compose.json').read_text())['processes']
        if not postgres_only and set(running) == {'postgres'}:
            raise RuntimeError('PostgreSQL-only stack is running. Run foundation-dev down before starting the full stack.')
        return None
    # Behind a public origin without a port Pulse's WebTransport sits outside the block; it must be free too.
    webtransport = pulse_ports(config)[0]
    if not ports_free(config['port'], webtransport if webtransport != config['port'] + 2 else None):
        raise RuntimeError('Configured ports are occupied. Stop the conflicting service or edit .cache/dev/settings.json.')
    temporary = STATE / 'tmp'
    temporary.mkdir(exist_ok=True, mode=0o700)
    save_json(STATE / 'process-compose.json', project(config, postgres_only))
    return (['up', '--config', str(STATE / 'process-compose.json'), '--disable-dotenv', '--ordered-shutdown', '--tui=false'],
            {**os.environ, 'TMPDIR': str(temporary), 'TMP': str(temporary), 'TEMP': str(temporary)})


def supervise():
    # The service manager's main process: the supervisor in the foreground, so
    # SIGTERM from systemd or launchd stops the stack in dependency order.
    STATE.mkdir(parents=True, exist_ok=True, mode=0o700)
    with (STATE / 'manager.lock').open('w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if supervisor_running():
            raise RuntimeError('A detached development stack is running; stop it with foundation-dev down.')
        reap_orphans()
        args, env = prepare_supervisor()
    os.chdir(STATE)
    # Task output is already in each task's log; keep it out of service.log and the journal.
    os.dup2(os.open(os.devnull, os.O_WRONLY), 1)
    os.execvpe('process-compose', pc_argv(*args), pc_env(env))


def service_name():
    return 'one.dcl.foundation-dev.' + hashlib.sha256(str(ROOT).encode()).hexdigest()[:8]


def service_file():
    if DARWIN:
        return Path.home() / 'Library/LaunchAgents' / f'{service_name()}.plist'
    return Path.home() / '.config/systemd/user' / f'{service_name()}.service'


def systemctl(*args, **kwargs):
    if not Path('/run/systemd/system').is_dir():
        raise RuntimeError('systemd is not running. On WSL add "[boot]" and "systemd=true" to /etc/wsl.conf, '
                           'then run wsl.exe --shutdown from Windows.')
    return run(['systemctl', '--user', *args], **kwargs)


def launchctl(*args, **kwargs):
    return subprocess.run(['launchctl', *args], **kwargs)


def service_start():
    if not DARWIN:
        systemctl('start', service_name())
    elif launchctl('print', f'gui/{os.getuid()}/{service_name()}', capture_output=True).returncode:
        launchctl('bootstrap', f'gui/{os.getuid()}', str(service_file()), check=True)
    else:
        launchctl('kickstart', f'gui/{os.getuid()}/{service_name()}', check=True)


def service_stop():
    if DARWIN:
        # bootout waits for ExitTimeOut; the next login loads the agent again.
        launchctl('bootout', f'gui/{os.getuid()}/{service_name()}', capture_output=True)
    else:
        systemctl('stop', service_name())


def service_install():
    # The service environment has no FOUNDATION_DEV_PORT or _PROFILE; apply them now.
    STATE.mkdir(parents=True, exist_ok=True, mode=0o700)
    settings(create=True)
    path = service_file()
    path.parent.mkdir(parents=True, exist_ok=True)
    nix = shutil.which('nix')
    if nix is None:
        raise RuntimeError('nix is not on PATH.')
    # Re-entering the flake on every start picks up toolchain changes.
    command = [nix, 'develop', str(ROOT), '--command', 'foundation-dev', '_supervise']
    if DARWIN:
        path.write_bytes(plistlib.dumps({
            'Label': service_name(), 'ProgramArguments': command, 'WorkingDirectory': str(ROOT),
            'EnvironmentVariables': {'PATH': '/usr/bin:/bin:/usr/sbin:/sbin', 'FOUNDATION_DEV_AUTOSTART': '0'},
            # Restart after a crash, not after foundation-dev down.
            'RunAtLoad': True, 'KeepAlive': {'SuccessfulExit': False}, 'ThrottleInterval': 10,
            'ExitTimeOut': 150, 'ProcessType': 'Interactive',
            'StandardOutPath': str(STATE / 'service.log'), 'StandardErrorPath': str(STATE / 'service.log'),
        }))
    else:
        path.write_text(textwrap.dedent(f'''\
            [Unit]
            Description=Foundation development stack ({ROOT})

            [Service]
            WorkingDirectory={ROOT}
            Environment=FOUNDATION_DEV_AUTOSTART=0
            ExecStart={shlex.join(command)}
            Restart=on-failure
            RestartSec=10
            TimeoutStopSec=150

            [Install]
            WantedBy=default.target
            '''))
        systemctl('daemon-reload')
        systemctl('enable', service_name())
    down()
    service_start()
    print(f'Installed {path}; the stack now starts at login and restarts after a crash.')
    if not DARWIN:
        print(f'To start it at boot without a login session: loginctl enable-linger {os.environ.get("USER", "")}'.rstrip())
    print('Startup output: foundation-dev logs service. Remove with foundation-dev service uninstall.')


def service_uninstall():
    path = service_file()
    if not path.exists():
        print('No service installed for this checkout.')
        return
    if DARWIN:
        service_stop()
    else:
        systemctl('disable', '--now', service_name())
    path.unlink()
    if not DARWIN:
        systemctl('daemon-reload')
    print(f'Removed {path}. PostgreSQL data and settings are preserved; foundation-dev up starts a detached stack.')


def desktop():
    # Rust edits rebuild and relaunch the shell; the /play page it loads is the
    # Sites dev server, so HUD edits arrive through Vite HMR without a relaunch.
    if not os.environ.get('FOUNDATION_DEV_DESKTOP_SHELL'):
        os.execvp('nix', ['nix', 'develop', f'{ROOT}#desktop', '--command', 'foundation-dev', 'desktop'])
    config = settings()
    os.chdir(ROOT / 'apps/desktop')
    env = {**os.environ, 'DCL_PLAY_URL': f"http://127.0.0.1:{config['port'] + 2}/play"}
    os.execvpe('watchexec', ['watchexec', '--restart', '--debounce', '500ms',
                             *watch('src', 'capabilities', 'build.rs', 'Cargo.toml', 'tauri.conf.json'),
                             '--exts', 'rs,toml,json', '--', 'cargo', 'run'], env)


def up(postgres_only=False):
    STATE.mkdir(parents=True, exist_ok=True, mode=0o700)
    if service_file().exists():
        if postgres_only:
            raise RuntimeError('PostgreSQL-only runs require a checkout without an installed development service.')
        service_start()
        print('The installed service runs the stack; use foundation-dev wait or foundation-dev status.')
        return 0
    with (STATE / 'manager.lock').open('w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        prepared = prepare_supervisor(postgres_only)
        if prepared is None:
            print('The development stack is already running.')
            return status()
        args, env = prepared
        pc(*args, '--detached', check=True, env=env)
        deadline = time.monotonic() + 10
        delays = backoff_delays(deadline)
        while not supervisor_running():
            delay = next(delays, None)
            if delay is None:
                raise RuntimeError('Supervisor startup timed out. Inspect .cache/dev/supervisor.log.')
            time.sleep(min(delay, max(0, deadline - time.monotonic())))
    print('Starting PostgreSQL only.' if postgres_only else
          'Starting PostgreSQL, the API and Sites. The first run downloads dependencies and compiles Rust.')
    print('Use foundation-dev logs api-build, foundation-dev status or foundation-dev wait.')
    print('Services remain running after this shell exits. Stop them with foundation-dev down.')
    return status()


def log_position(path):
    try:
        stat = path.stat()
        return stat.st_dev, stat.st_ino, stat.st_size
    except OSError:
        return None


def last_line(path, limit=4096, after=None):
    try:
        with path.open('rb') as log:
            stat = os.fstat(log.fileno())
            offset = after[2] if after and after[:2] == (stat.st_dev, stat.st_ino) and after[2] <= stat.st_size else 0
            log.seek(max(offset, stat.st_size - limit))
            tail = ANSI.sub('', log.read().decode(errors='replace'))
            tail = ''.join(c for c in tail if c.isprintable() or c in '\n\r').rstrip()
            return re.split(r'[\r\n]', tail)[-1].strip()
    except OSError:
        return ''


def progress(processes, elapsed, log_positions=None):
    running = [p['name'] for p in processes if p.get('is_running')]
    pending = [p['name'] for p in processes if not p.get('is_running') and p.get('status') == 'Pending']
    line = f"{int(elapsed) // 60}m{int(elapsed) % 60:02d}s running: {', '.join(running) or 'nothing'}"
    if pending:
        line += f"; waiting to start: {', '.join(pending)}"
    for name in ('web-install', 'api-build'):
        after = (log_positions or {}).get(name)
        if name in running and (last := last_line(STATE / f'{name}.log', after=after)):
            line += f'; {name}: {last[:160]}'
    return line


def wait_ready(timeout, postgres_only=False):
    config = settings()
    urls = [f"http://127.0.0.1:{config['port']}/about", f"http://127.0.0.1:{config['port'] + 2}/"]
    started = reported = time.monotonic()
    deadline = started + timeout
    log_positions = {name: log_position(STATE / f'{name}.log') for name in ('web-install', 'api-build')}
    print(f"Waiting up to {timeout}s for {'PostgreSQL' if postgres_only else 'the API and Sites'}.", flush=True)
    delays = backoff_delays(deadline)
    while True:
        processes = pc('process', 'list', '--output', 'json', capture_output=True, text=True)
        if processes.returncode:
            raise RuntimeError('The development supervisor is not running. Run foundation-dev up.')
        listing = json.loads(processes.stdout)
        for process in listing:
            if process.get('exit_code', 0) != 0 and not process.get('is_running'):
                name = process['name']
                raise RuntimeError(f'{name} failed. Inspect foundation-dev logs {name}.')
        if time.monotonic() - reported >= PROGRESS_SECONDS:
            reported = time.monotonic()
            print(progress(listing, reported - started, log_positions), flush=True)
        try:
            if postgres_only:
                sql(config, 'SELECT 1', 'postgres')
                # Scratch databases clone template1. Match the collation that
                # full-stack startup installs, without compiling the API.
                with (STATE / 'schema.lock').open('w') as lock:
                    fcntl.flock(lock, fcntl.LOCK_EX)
                    ensure_collation(config, 'template1')
                print('PostgreSQL is ready for scratch test databases.')
                return 0
            for url in urls:
                with urllib.request.urlopen(url, timeout=3) as response:
                    if response.status != 200:
                        raise RuntimeError('Service not ready')
            print('API and Sites are ready.')
            return wait_first_mirror_pass(config, deadline)
        except (urllib.error.URLError, TimeoutError, OSError, RuntimeError, subprocess.CalledProcessError):
            delay = next(delays, None)
            if delay is None:
                break
            time.sleep(min(delay, max(0, deadline - time.monotonic())))
    raise RuntimeError('Startup timed out. Check foundation-dev status and foundation-dev logs api/sites.')


def first_pass_pending(status):
    """The configured mirrors with no complete pass yet, and the errors of those that failed."""
    data = status['data']
    done = {m['mirror'] for m in data['mirrors'] if m.get('last_complete_pass_at')}
    pending = [name for name, source in data['configured_mirrors'].items() if source and name not in done]
    failed = {m['mirror']: m['last_error'] for m in data['mirrors'] if m['mirror'] in pending and m.get('last_error')}
    return pending, failed


def wait_first_mirror_pass(config, deadline):
    # Catalogue routes answer 503 until their mirror's first pass. An upstream is never a
    # prerequisite for readiness: an error, an unreadable status or the deadline only warns.
    url = f"http://127.0.0.1:{config['port']}/api/status"
    announced = None
    delays = backoff_delays(deadline, maximum=5)
    while True:
        try:
            with urllib.request.urlopen(url, timeout=3) as response:
                pending, failed = first_pass_pending(json.load(response))
        except (urllib.error.URLError, TimeoutError, OSError, ValueError, KeyError):
            print(f'Could not read {url}; catalogue routes answer 503 until their first mirror pass.')
            return 0
        if not pending:
            if announced:
                print('Mirrors completed their first pass.')
            return 0
        if failed:
            for name, error in failed.items():
                print(f'The {name} mirror failed before its first pass ({error}); its routes answer 503. See {url}.')
            return 0
        if time.monotonic() >= deadline:
            print(f"The first {', '.join(pending)} mirror pass is still running; see {url}.")
            return 0
        if pending != announced:
            print(f"Waiting for the first {', '.join(pending)} mirror pass; its routes answer 503 until then.")
            announced = pending
        time.sleep(min(next(delays, 0), max(0, deadline - time.monotonic())))


def live_postgres_pid():
    try:
        with (STATE / 'postgres/postmaster.pid').open() as pidfile:
            pid = int(pidfile.readline())
        if pid <= 0:
            raise ValueError('Expected a positive PID')
    except FileNotFoundError:
        return None
    except ValueError as error:
        raise RuntimeError('Invalid PostgreSQL PID file. Inspect .cache/dev/postgres/postmaster.pid; '
                           'no processes were stopped.') from error
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        # PostgreSQL checks stale PID files itself on the next startup.
        return None
    except PermissionError:
        pass  # The process exists even though this user cannot signal it.
    return pid


def down():
    if service_file().exists():
        service_stop()
    if not STATE.exists():
        return 0
    with (STATE / 'manager.lock').open('w') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        config = settings() if SETTINGS.exists() else None
        control_socket = socket_path()
        if not supervisor_running():
            pid = live_postgres_pid()
            if pid is not None:
                raise RuntimeError(f'Supervisor is unavailable while PID {pid} recorded by PostgreSQL is alive. '
                                   'Inspect .cache/dev/supervisor.log and verify the process identity; '
                                   'no processes were stopped.')
            if config and not ports_free(config['port']):
                raise RuntimeError('Supervisor is unavailable while development ports are occupied. '
                                   'Inspect .cache/dev/supervisor.log; no processes were stopped.')
            return 0
        # process-compose can close the connection before it answers the stop
        # request ("failed to stop project ... EOF"); the socket and the ports
        # say whether it stopped, not the exit code.
        pc('down')
        deadline = time.monotonic() + 120
        delays = backoff_delays(deadline)
        while control_socket.exists() or (config is not None and not ports_free(config['port'])):
            delay = next(delays, None)
            if delay is None:
                raise RuntimeError('Shutdown timed out. Inspect .cache/dev/supervisor.log before restarting.')
            time.sleep(min(delay, max(0, deadline - time.monotonic())))
        return 0


def main():
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest='command', required=True)
    for command in ('down', 'status', 'ui', 'psql', 'env', 'desktop', '_supervise'):
        commands.add_parser(command)
    commands.add_parser('up').add_argument('--postgres-only', action='store_true',
                                          help='Start only PostgreSQL for backend tests')
    commands.add_parser('logs').add_argument('service', choices=('api', 'api-build', 'sites', 'postgres', 'web-install', 'index', 'livekit', 'pulse', 'nats', 'service'))
    commands.add_parser('restart').add_argument('service', choices=('api', 'api-build', 'sites', 'postgres', 'web-install', 'index', 'livekit', 'pulse', 'nats'))
    commands.add_parser('service', help='Run the stack as a per-user systemd unit or LaunchAgent').add_argument(
        'action', choices=('install', 'uninstall'))
    wait_parser = commands.add_parser('wait')
    wait_parser.add_argument('--timeout', type=int, default=1800)
    wait_parser.add_argument('--postgres-only', action='store_true')
    commands.add_parser('profile').add_argument('profile', choices=PROFILES)
    origin_parser = commands.add_parser('origin', help='Serve behind a TLS front at this origin (local: loopback)')
    origin_parser.add_argument('origin', help='https://host[:port], or local')
    origin_parser.add_argument('--cert', default='', help="PEM certificate for the origin's host, for Pulse's WebTransport")
    origin_parser.add_argument('--key', default='', help='Its PEM private key')
    commands.add_parser('trades-snapshot', help="Write the mirrored trades to a gzipped SQL file").add_argument('out')
    commands.add_parser('_task', help='Internal supervised process').add_argument('task', choices=(
        'postgres', 'database-ready', 'api-build', 'api', 'api-run', 'web-install', 'sites', 'index', 'livekit', 'pulse', 'nats'))
    args = parser.parse_args()
    if args.command == '_task':
        task(args.task)
    elif args.command == 'up':
        return up(args.postgres_only)
    elif args.command == 'status':
        return status()
    elif args.command == 'wait':
        return wait_ready(args.timeout, args.postgres_only)
    elif args.command == 'down':
        return down()
    elif args.command == '_supervise':
        supervise()
    elif args.command == 'service':
        service_install() if args.action == 'install' else service_uninstall()
    elif args.command == 'desktop':
        desktop()
    elif args.command == 'ui':
        return pc('attach').returncode
    elif args.command == 'logs':
        if args.service == 'service' and not DARWIN:
            os.execvp('journalctl', ['journalctl', '--user', '-f', '-n', '80', '-u', service_name()])
        os.execvp('tail', ['tail', '-n', '80', '-F', str(STATE / f'{args.service}.log')])
    elif args.command == 'restart':
        return pc('process', 'restart', args.service).returncode
    elif args.command == 'profile':
        if supervisor_running():
            raise RuntimeError('Run foundation-dev down before changing profile.')
        config = settings()
        data = STATE / 'postgres'
        if (config['profile'] == 'testnet') != (args.profile == 'testnet') and data.exists() and any(data.iterdir()):
            # Mainnet and testnet rows (trades, collections, owners) must never share a database.
            raise RuntimeError(f"This stack already holds data of the {config['profile']} profile, and testnet data "
                               'never shares a database with the others. Run the testnet profile in its own worktree '
                               '(FOUNDATION_DEV_PROFILE=testnet on its first foundation-dev up).')
        config['profile'] = args.profile
        save_json(SETTINGS, config)
        print(f'Profile set to {args.profile}; run foundation-dev up. Existing data is preserved.')
    elif args.command == 'origin':
        if supervisor_running():
            raise RuntimeError('Run foundation-dev down before changing the public origin.')
        config = settings()
        for key in ('public_origin', 'tls_cert', 'tls_key'):
            config.pop(key, None)
        config.update(origin_settings({}, args.origin, args.cert, args.key))
        save_json(SETTINGS, config)
        print(f"Public origin set to {config['public_origin'] or 'loopback'}; run foundation-dev up.")
    elif args.command == 'env':
        # eval "$(foundation-dev env)" names the running stack for scripts and test suites.
        config = settings()
        for key, value in (('FOUNDATION_DEV_API_URL', f"http://127.0.0.1:{config['port']}"),
                           ('FOUNDATION_DEV_SITES_URL', f"http://127.0.0.1:{config['port'] + 2}"),
                           ('FOUNDATION_DEV_DATABASE_URL', database_url(config)),
                           ('FOUNDATION_DEV_TEST_DATABASE_URL', database_url(config, 'postgres')),
                           ('FOUNDATION_DEV_STACK_PROFILE', config['profile']),
                           ('FOUNDATION_DEV_PUBLIC_URL', runtime_env(config)['PUBLIC_URL'])):
            print(f'{key}={shlex.quote(value)}')
    elif args.command == 'psql':
        os.execvpe('psql', ['psql', '-X'], psql_env(settings()))
    elif args.command == 'trades-snapshot':
        config = settings()
        if config['profile'] == 'testnet':
            # seed_trades loads a snapshot into a mirror stack; testnet trades must not reach one.
            raise RuntimeError('This stack runs the testnet profile; its trades are not a snapshot for mainnet stacks.')
        export_trades_snapshot(config, args.out)
    return 0


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (RuntimeError, ValueError, subprocess.CalledProcessError) as error:
        print(f'foundation-dev: {error}', file=sys.stderr)
        sys.exit(1)
