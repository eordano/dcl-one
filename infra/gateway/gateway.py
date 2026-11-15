#!/usr/bin/env python3
"""Render the container gateway from validated runtime settings and the route contract."""
import argparse
import ipaddress
import json
import os
from pathlib import Path
import re
import subprocess

# The worlds service's wire caps (worlds `MAX_UPLOAD_WIRE_SIZE_BYTES`,
# `MAX_SETTINGS_UPLOAD_WIRE_BYTES`); a worlds test pins them.
WORLDS_DEPLOY_CAP = "360m"
WORLDS_SETTINGS_CAP = "12m"
# The badges backfill body (social badges `MAX_BACKFILL_BODY_BYTES`); a
# gateway test pins it.
BADGES_BACKFILL_CAP = "64m"
# One report evidence file (the gatekeeper reports `MAX_FILE_BYTES`); a
# gateway test pins it.
REPORT_EVIDENCE_CAP = "5m"
# The content server's `POST /entities` route limit at the default field
# count (`max_deployment_body_bytes`: 200 MiB of files and fields plus
# multipart framing), rounded up, so its own 413 is the one a client meets.
CONTENT_DEPLOY_CAP = "248m"
# World storage bodies: the largest storage route maximum at the deployed
# defaults (config/src/worlds.rs: WORLD_STORAGE_MAX_VALUE_SIZE_BYTES, 512 KiB,
# plus the 1 KiB envelope slack; the export body's 256 KiB + 1 KiB is below
# it) and a little headroom, so the service's own 413 is the one a client
# meets and nginx buffers no more than that. Raising a value cap means raising
# this; a gateway test pins it against the defaults. The gateway's own 413 has
# the service's shape without a byte count, which differs per route.
STORAGE_BODY_HEADROOM_BYTES = 4 * 1024
STORAGE_BODY_CAP_BYTES = 512 * 1024 + 1024 + STORAGE_BODY_HEADROOM_BYTES
STORAGE_TOO_LARGE = {"error": "Payload Too Large",
                     "message": "Request body exceeds the maximum allowed size"}
# Storage writes one untrusted address may have in flight, keyed like the write
# budget (reads and trusted callers are not counted): each body is buffered in
# the gateway's temp space before the service answers.
STORAGE_UPLOADS_PER_ADDRESS = 10
# Uploads one address may have in flight on a deploy route: nginx buffers each
# body before proxying it, and the app's limits only answer after that. Above
# upstream catalyst's 4 per source, so it stays a coarse guard.
DEPLOY_CONNECTIONS_PER_ADDRESS = 10
# The gateway's own answers on a worlds deploy: named location, status,
# Retry-After seconds (0 for none) and the worlds service's {error, message} body.
WORLDS_DEPLOY_ERRORS = [
    ("@worlds_deploy_too_large", 413, 0,
     {"error": "Payload Too Large", "message": "The multipart request is too large."}),
    ("@worlds_deploy_rate", 429, 1,
     {"error": "Too Many Requests",
      "message": "Too many deploy requests from this client, please retry shortly."}),
    ("@worlds_deploy_busy", 429, 5,
     {"error": "Too Many Requests",
      "message": "Too many uploads in progress from this client, please retry shortly."}),
]
# Segment delivers each webhook event as its own POST from a few egress
# addresses, and the HMAC in the app is the gate: this guard sits above the
# sink's sizing (DATABASE.md, about 280 inserts/s) so a delivery is never
# answered 429 and retried.
WEBHOOK_RATE = 300
WEBHOOK_BURST = 600


def words(value):
    return [word for word in re.split(r"[\s,]+", value.strip()) if word]


def count(value, name):
    if not re.fullmatch(r"[1-9][0-9]{0,5}", value.strip()):
        raise ValueError(f"{name} must be a positive integer")
    return int(value)


def networks(value):
    result = []
    for word in words(value):
        if not re.fullmatch(r"[0-9a-fA-F:.]+(?:/[0-9]+)?", word):
            raise ValueError("Expected IP addresses or CIDRs without zone IDs")
        result.append(str(ipaddress.ip_network(word, strict=False)))
    return result


def hostname(value):
    if len(value) > 253 or not all(re.fullmatch(r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?", part)
                                  for part in value.split(".")):
        raise ValueError("Expected a lowercase DNS hostname without a scheme, port or path")
    return value


def upstream(value):
    match = re.fullmatch(r"(\[[0-9a-fA-F:]+\]|[a-z0-9.-]+):([0-9]+)", value)
    if not match or not 1 <= int(match[2]) <= 65535:
        raise ValueError("Upstreams must be host:port or [IPv6]:port")
    host, port = match.groups()
    if host.startswith("["):
        ipaddress.IPv6Address(host[1:-1])
    else:
        hostname(host)
    return f"{host}:{int(port)}"


def site_prefixes(files):
    prefixes = set()
    for name in files:
        parts = re.sub(r"\.tsx?$", "", name).split(".")
        if parts[0].startswith(("$", "_")):
            continue
        parts = parts[:2] if parts[0] == "api" else parts[:1]
        parts = [re.split(r"[_$]", part)[0] for part in parts]
        if all(parts):
            prefixes.add("/" + "/".join(parts))
    return sorted(prefixes)


def settings(env, resolv_conf="/etc/resolv.conf"):
    domain = hostname(env.get("GATEWAY_DOMAIN", "localhost"))
    aliases = [hostname(host) for host in words(env.get("GATEWAY_HOST_ALIASES", ""))]
    dns = words(env.get("GATEWAY_RESOLVERS", ""))
    if not dns:
        dns = [parts[1] for line in Path(resolv_conf).read_text().splitlines()
               if len(parts := line.split()) >= 2 and parts[0] == "nameserver"]
    if not dns:
        raise ValueError("No DNS resolver; configure GATEWAY_RESOLVERS")
    resolvers = []
    for value in dns:
        if not re.fullmatch(r"[0-9a-fA-F:.]+", value):
            raise ValueError("Resolvers must be IP addresses without zone IDs")
        address = ipaddress.ip_address(value)
        resolvers.append(f"[{address}]" if address.version == 6 else str(address))
    storage = env.get("GATEWAY_STORAGE_ROOT", "")
    if storage and not re.fullmatch(r"/[a-zA-Z0-9_./-]+", storage):
        raise ValueError("GATEWAY_STORAGE_ROOT must be an absolute path without whitespace")
    trusted = networks(env.get("GATEWAY_TRUSTED_PROXY_CIDRS", ""))
    storage_trusted = networks(env.get("GATEWAY_STORAGE_TRUSTED_CIDRS", ""))
    if any(ipaddress.ip_network(proxy).overlaps(ipaddress.ip_network(caller))
           for proxy in trusted for caller in storage_trusted
           if ipaddress.ip_network(proxy).version == ipaddress.ip_network(caller).version):
        raise ValueError("GATEWAY_STORAGE_TRUSTED_CIDRS must not overlap GATEWAY_TRUSTED_PROXY_CIDRS")
    # Per-client budget for everything outside the content, storage, worlds and deploy zones. A web
    # explorer boot is about 150 same-origin requests in its first second (shell, engine
    # modules for every worker, profile, realm), so the defaults let one boot through and
    # still bound a single address.
    read_rate = count(env.get("GATEWAY_READ_RATE", "100"), "GATEWAY_READ_RATE")
    read_burst = count(env.get("GATEWAY_READ_BURST", "300"), "GATEWAY_READ_BURST")
    return {
        "domain": domain, "hosts": aliases, "resolvers": " ".join(resolvers),
        "read_rate": read_rate, "read_burst": read_burst,
        "apis": [upstream(value) for value in words(env.get("GATEWAY_APIS_UPSTREAMS", "apis:5210"))],
        "sites": [upstream(value) for value in words(env.get("GATEWAY_SITES_UPSTREAMS", "sites:5212"))],
        "admins": networks(env.get("GATEWAY_ADMIN_CIDRS", "")),
        "trusted": trusted,
        "storage_trusted": storage_trusted,
        "sync_peers": networks(env.get("GATEWAY_TRUSTED_SYNC_PEER_IPS", "")),
        "storage": storage.rstrip("/"),
    }


# Worlds routes: reads (GET, HEAD, OPTIONS) are not rate-limited, as upstream; writes and
# the admin routes (reachable from admin networks only) keep the server-wide read budget.
WORLDS_LIMIT = "limit_req zone=worlds burst=60 nodelay;"

# First path segments of social's badges router (services/catalyst/social/src/badges/mod.rs).
BADGES_MOUNTS = ("assets", "badges", "categories", "status", "users")


def render(table, cfg, mime):
    if not cfg["apis"] or not cfg["sites"]:
        raise ValueError("Both API and Sites upstreams are required")
    prefixes = site_prefixes(table["sites"])
    for prefix in prefixes:
        # Only the exact /content page is Sites', on the apex host (the content_root map).
        if prefix in ("/ws", "/hot-scenes", "/content") or any(
                prefix == path or prefix.startswith(path + "/") for path in table["internal"]):
            raise ValueError(f"Sites claims a reserved API path: {prefix}")
    domain = re.escape(cfg["domain"])
    host_aliases = {"peer": "", "worlds": "", "abcdn": "/ab-cdn", "abgen-cdn": "/ab-cdn",
                    "asset-bundle-registry-abgen": "/asset-bundle-registry",
                    "rpc-social-service-ea": "/social-rpc", "market": "/shop",
                    "storage": "/world-storage", "comms-gatekeeper-local": "/comms-gatekeeper"}
    # A host whose path already carries its prefix is not prefixed again, except
    # badges.: upstream's own paths start with /badges (/badges/<id>/tiers), so
    # only a second segment the badges router mounts counts as prefixed. No
    # catalog badge id equals one (/badges/status is the mount, not a badge).
    badges_mounts = "|".join(BADGES_MOUNTS)
    alias_moves, settled = [], []
    for name, target in host_aliases.items():
        alias_moves.extend([f'"~^gateway:/{name}/?$" "{target}/";',
                            f'"~^gateway:/{name}(?<gateway_rest>/.*)$" "{target}$gateway_rest";'])
        settled.append(f'"~^{name}:{re.escape(target)}(/|$)" "";'
                       if target else f'"~^{name}:" "";')
    apex_hosts = " ".join(f'{host} "";' for host in [cfg["domain"], *cfg["hosts"]])
    domain_names = " ".join(dict.fromkeys([cfg["domain"], "~^[a-z0-9-]+\\." + domain + "$", *cfg["hosts"]]))
    read_rate, read_burst = cfg["read_rate"], cfg["read_burst"]
    admin_geo = " ".join(f'{net} "admitted";' for net in cfg["admins"])
    trusted_geo = " ".join(f"{net} 1;" for net in cfg["trusted"])
    sync_peer_geo = " ".join(f"{net} 1;" for net in cfg["sync_peers"])
    storage_geo = " ".join(f"{net} 1;" for net in cfg["storage_trusted"])
    real_ip = "\n".join(f"set_real_ip_from {net};" for net in cfg["trusted"])
    blocked = "|".join(re.escape(path.lstrip("/")) for path in table["internal"])
    upstreams = ""
    for name in ("apis", "sites"):
        servers = "\n".join(f"server {host} resolve max_fails=2 fail_timeout=10s;" for host in cfg[name])
        upstreams += f"upstream {name} {{ zone {name} 64k; {servers} keepalive 32; }}\n"

    def location(key, body):
        return f"location {key} {{\n{body}\n}}\n"

    def proxy(target, extra=""):
        # Headers are inherited from http; never add a partial proxy_set_header
        # list in a location, which would discard that trusted set.
        return f"proxy_pass http://{target};\n{extra}"

    def pair(path, target, final=False, extra=""):
        return (location("= " + path, proxy(target, extra))
                + location(("^~ " if final else "") + path + "/", proxy(target, extra)))

    security_headers = ('add_header X-Content-Type-Options nosniff always;\n'
                        'add_header Referrer-Policy strict-origin-when-cross-origin always;\n'
                        'add_header Cross-Origin-Opener-Policy $coop_fallback always;')
    locations = location("= /_gateway/healthz", 'access_log off; return 200 "ok\\n";')
    # The operator gate is applied in the server rewrite phase, before any
    # location or .data regex; a root alias rewrites with `break`, so the gate
    # lists alias prefixes (market, camera-reel) beside their targets.
    for path in ("/admin", "/server"):
        locations += pair(path, "sites" if path in prefixes else "apis", True)
    for row in table["node"]:
        # The query-based places social cards own only their exact endpoints;
        # /places/place/<position> and /places/world/<name> are Sites pages.
        # A webhook route (Segment's deliveries) takes its own rate zone.
        extra = f"limit_req zone=webhook burst={WEBHOOK_BURST} nodelay;" if row.get("webhook") else ""
        locations += (location("= " + row["path"], proxy("apis", extra))
                      + location("= " + row["path"] + "/", proxy("apis", extra))
                      if row.get("exact") else pair(row["path"], "apis", True, extra))
    for row in table["aliases"]:
        rewrite = row.get("rewrite") or "^(.*)$ " + row["to"].replace("$uri", "$1")
        # These aliases belong to APIs. Re-selecting a location after the
        # rewrite could send /social/... to the Sites /social page instead.
        locations += location(row["key"], f"rewrite {rewrite} break;\n" + proxy("apis"))
    # Worlds deploys reach these matches already rewritten under their host prefix.
    # The content deploy zone is only a coarse abuse guard: 200 at once, then
    # 5 r/s per address, above the content server's own per-client burst limit
    # (upstream's 200 per 60 s, answered as JSON with Retry-After), so a
    # client within that limit never meets nginx's HTML 429. Concurrent
    # uploads per address are capped on both deploy routes, since each body
    # is buffered in the gateway's temp space before the app sees it.
    deploy = "|".join(re.escape(path) for path in table["deploy"])
    uploads = f"limit_conn deploy_uploads {DEPLOY_CONNECTIONS_PER_ADDRESS};"
    # Case-insensitive, as nginx would otherwise pass /content/Entities to the
    # /content/ location and its looser limits.
    locations += location(f'~* "^({deploy})/*$"', proxy("apis",
        f"client_max_body_size {CONTENT_DEPLOY_CAP}; client_body_timeout 300s; "
        f"limit_req zone=deploy burst=200 nodelay; {uploads}"))
    # Upstream caps the POST /entities/active body at 10 MiB and answers its
    # own 413 (MAX_ACTIVE_ENTITIES_BODY_SIZE); the server-wide 1m would answer
    # nginx's HTML 413 first. The content server's case rewrite routes any
    # spelling, so the match ignores case too.
    locations += location('~* "^(/content)?/entities/active/*$"', proxy("apis",
        "client_max_body_size 10m; limit_req zone=content burst=1000 nodelay;"))
    # The gateway's own refusals on a worlds deploy answer the worlds service's JSON
    # envelope instead of nginx's HTML pages; the app's own errors pass through, as the
    # proxy does not intercept them. Both request limits answer 429, but error_page
    # keys on the status alone, so limit_conn is raised here as an internal 503 that
    # only its named location catches and turns back into 429 with its own text (the
    # app's own 503 shed answer is not intercepted). A body that stalls for
    # client_body_timeout gets no answer: nginx logs 408 and closes the connection
    # without a response, which error_page cannot replace.
    locations += location('~ "^/worlds-content-server/entities/*$"', proxy("apis",
        f"client_max_body_size {WORLDS_DEPLOY_CAP}; client_body_timeout 300s; "
        f"limit_req zone=deploy burst=4 nodelay; {uploads} limit_conn_status 503;\n"
        "error_page 413 = @worlds_deploy_too_large; error_page 429 = @worlds_deploy_rate;\n"
        "error_page 503 = @worlds_deploy_busy;"))
    # axum's Json, which the app's deploy answers use, sends application/json without
    # a charset, and nginx adds none to it.
    json_error = ("default_type application/json;\n" + security_headers + "\n"
                  "add_header Access-Control-Allow-Origin $cors_fallback always;\n")
    for name, status, retry, body in WORLDS_DEPLOY_ERRORS:
        retry_after = f"add_header Retry-After {retry} always;\n" if retry else ""
        text = json.dumps(body, separators=(",", ":"))
        locations += location(name, json_error + retry_after + f"return {status} '{text}';")
    # Upstream worlds-content-server limits no route by rate. Every worlds location but the
    # deploy names the worlds zone, which replaces the server-wide read zone and counts only
    # writes and admin calls. Settings uploads are buffered here like deploys, so they also
    # take a per-address connection cap.
    locations += location('~ "^(/worlds-content-server)?/world/[^/]+/settings/*$"', proxy("apis",
        f"client_max_body_size {WORLDS_SETTINGS_CAP}; {WORLDS_LIMIT} "
        f"limit_conn worlds_uploads {DEPLOY_CONNECTIONS_PER_ADDRESS};"))
    locations += location("~ ^/worlds-content-server/(contents|ipfs)/", proxy("apis", WORLDS_LIMIT))
    locations += pair("/worlds-content-server", "apis", extra=WORLDS_LIMIT)
    # The same routes on the root mount (worlds. and peer. hosts). Root /wallet/ is the
    # credits alias and root /admin is the operator's, so neither is listed.
    locations += location('~ "^/world/"', proxy("apis", WORLDS_LIMIT))
    for path in ("/index", "/worlds", "/worlds/status"):
        locations += location("= " + path, proxy("apis", WORLDS_LIMIT))
    # The player report intake: one evidence file for each PUT, and no deeper path. The
    # match also covers the moderator's evidence read, so a key that ends in `.data` stays
    # ahead of the Sites regex below. The app reads the credentials after nginx buffers the
    # body, so concurrent uploads are capped as on the deploy routes. Every other
    # /report-user request takes `location /` and the server-wide cap.
    locations += location('~ "^/report-user/api/reports/(players/)?[^/]+/evidence/[^/]+$"', proxy("apis",
        f"client_max_body_size {REPORT_EVIDENCE_CAP}; {uploads}"))
    # Both badges host forms reach this already rewritten under /badges. The admin
    # check is a bearer token the app reads after nginx buffers the body, so
    # concurrent uploads are capped as on the deploy routes.
    locations += location('~ "^/badges/badges/[^/]+/backfill$"', proxy("apis",
        f"client_max_body_size {BADGES_BACKFILL_CAP}; {uploads}"))
    locations += location("~ ^(/builder-api)?/v1/(projects/[^/]+/(blobs/[^/]+|manifest|files)|items/[^/]+/files)/*$",
                          proxy("apis", "client_max_body_size 21m;"))
    locations += location('~ "^/explorer/0x[0-9a-fA-F]{40}/(wearables|emotes)$"', proxy("apis"))
    # Keep the Sites .data regex after every API regex.
    locations += location(r"~ \.data$", proxy("sites"))
    for prefix in prefixes:
        if prefix in ("/admin", "/server"):
            continue
        # The Cast not-found page is the exact /cast; /cast/ stays the gatekeeper's (root-aliases.nix).
        locations += location("= /cast", proxy("sites")) if prefix == "/cast" else pair(prefix, "sites")
    for path in ("/", "/favicon.ico", "/dcl-sdk-types.json", "/docs/index.md", "/_root.data",
                 "/terms", "/privacy", "/content-policy", "/ethics", "/rewards", "/rewards-terms",
                 "/referral", "/referral-terms", "/credits-terms", "/security", "/brand"):
        locations += location("= " + path, proxy("sites"))
    # The Content Policy page on the apex host; on every other host (peer., worlds., content.)
    # /content stays the content server's.
    locations += location("= /content", proxy("$content_root"))
    for path in ("/assets/", "/fonts/", "/errors/", "/template-bundles/"):
        locations += location("^~ " + path, proxy("sites"))
    locations += location("/content/", proxy("apis", "limit_req zone=content burst=1000 nodelay;"))
    # Matched after host rewrites, so every storage host form shares this budget.
    storage_limits = ("limit_req zone=storage burst=400 nodelay; "
                      "limit_req zone=storage_write burst=60 nodelay; "
                      "limit_req zone=storage_trusted burst=1000 nodelay; "
                      f"limit_conn storage_uploads {STORAGE_UPLOADS_PER_ADDRESS};\n"
                      f"client_max_body_size {STORAGE_BODY_CAP_BYTES}; "
                      "error_page 413 = @storage_too_large;")
    locations += location("= /world-storage", proxy("apis", storage_limits))
    locations += location("^~ /world-storage/", proxy("apis", storage_limits))
    # A storage host request, forwarded as the client spelled it under the
    # /world-storage prefix ($storage_raw_uri), so an encoded `/` or `?` in a
    # key reaches the service; only the server rewrite reaches it.
    locations += location("= /__gateway_storage_raw",
                          "internal;\n" + proxy("apis$storage_raw_uri", storage_limits))
    locations += location("@storage_too_large", json_error + f"return 413 '{json.dumps(STORAGE_TOO_LARGE, separators=(',', ':'))}';")
    storage = 'return 404;'
    if cfg["storage"]:
        # A host rewrite also marks a request internal in nginx. Require an
        # upstream response header as well; client headers cannot supply it.
        # nginx keeps the app's Content-Type, Accept-Ranges and Cache-Control on
        # the redirect and drops its other headers, so ETag and the expose list
        # are copied back. The app answers every Range it supports itself; one it
        # does not (multi-range, `bytes=-`) gets the whole file, as upstream.
        storage = (f'internal; alias {cfg["storage"]}/contents/; sendfile on; etag off;\n'
                   'max_ranges 0;\n'
                   'if ($upstream_http_x_accel_redirect = "") { return 404; }\n'
                   'add_header ETag $upstream_http_etag always;\n'
                   'add_header Access-Control-Expose-Headers $upstream_http_access_control_expose_headers always;\n'
                   'add_header Access-Control-Allow-Origin "*" always;\n'
                   + security_headers)
    locations += location("= /__protected_storage", "return 404;")
    locations += location("^~ /__protected_storage/", storage)
    locations += location("/", proxy("apis"))
    # $uri is normalized by nginx (percent decoding, dot segments and repeated
    # slashes). Case-insensitive gates also cover React Router's matching rules.
    return f"""
worker_processes auto;
pid /tmp/gateway/nginx.pid;
error_log /dev/stderr warn;
events {{ worker_connections 1024; }}
http {{
    include {mime};
    server_tokens off;
    access_log /dev/stdout;
    client_body_temp_path /tmp/gateway/client;
    proxy_temp_path /tmp/gateway/proxy;
    fastcgi_temp_path /tmp/gateway/fastcgi;
    uwsgi_temp_path /tmp/gateway/uwsgi;
    scgi_temp_path /tmp/gateway/scgi;
    resolver {cfg["resolvers"]} valid=10s;
    resolver_timeout 5s;
    {upstreams}
    {real_ip}
    real_ip_header X-Forwarded-For;
    real_ip_recursive on;
    geo $realip_remote_addr $trusted_peer {{ default 0; {trusted_geo} }}
    map "$trusted_peer:$http_x_forwarded_proto" $forwarded_scheme {{
        default $scheme;
        "1:https" https;
        "1:http" http;
    }}
    geo $admin_gate {{ default ""; {admin_geo} }}
    map $http_upgrade $connection_keepalive {{ default upgrade; "" ""; }}
    map $upstream_http_access_control_allow_origin $cors_fallback {{
        "" "*";
        default "";
    }}
    map $upstream_http_cross_origin_opener_policy $coop_fallback {{
        "" "same-origin";
        default "";
    }}
    map $host $gateway_subdomain {{
        default "";
        {apex_hosts}
        "~^(?<gateway_sub>[a-z0-9-]+)\\.{domain}$" $gateway_sub;
    }}
    map $gateway_subdomain $content_root {{
        default apis;
        "" sites;
    }}
    map $gateway_subdomain $gateway_prefix {{
        default /$gateway_subdomain;
        "" "";
        {" ".join(f'{key} "{value}";' for key, value in host_aliases.items())}
    }}
    map "$gateway_subdomain:$uri" $gateway_uri {{
        volatile;
        default "";
        "~^[^:]*:/__protected_storage(/|$)" "";
        {" ".join(alias_moves)}
        "~^gateway:" "";
        {" ".join(settled)}
        "~^badges:/badges/({badges_mounts})(/|$)" "";
        "~^badges:(?<badges_own>/badges(/.*)?)$" /badges$badges_own;
        "~^([^:]+):/\\1(/|$)" "";
        "~^[^:]+:/$" $gateway_prefix/;
        "~^[^:]+:(?<gateway_path>/.*)$" $gateway_prefix$gateway_path;
    }}
    # The storage hosts' rewrite, from the unnormalized $request_uri: the
    # decoded $uri would turn an encoded `/` or `?` in a key into a separator.
    # Dot segments and empty segments keep the normalized rewrite.
    map "$gateway_subdomain:$request_uri" $storage_raw_uri {{
        default "";
        "~*^[^:]*:[^?]*/(\\.|%2e){{1,2}}([/?]|$)" "";
        "~^[^:]*:[^?]*//" "";
        "~^(storage|world-storage):/world-storage([/?]|$)" "";
        "~^(storage|world-storage):(?<storage_raw>/.*)$" /world-storage$storage_raw;
        "~^gateway:/storage(?<storage_rest>/.*)$" /world-storage$storage_rest;
    }}
    # A decoded `?` would split a rewritten path; a /world-storage request
    # that is not rewritten is forwarded as sent.
    map "$gateway_uri:$uri" $ambiguous_path {{
        volatile;
        default 0;
        "~^:/world-storage(/|$)" 0;
        "~[?]" 1;
    }}
    map "$gateway_subdomain:$request_uri" $original_path {{
        default $request_uri;
        "~^gateway:/[^/?]+(?<gateway_signed>/.*)$" $gateway_signed;
    }}
    map $uri $internal_path {{
        volatile;
        default 0;
        "~*^/({blocked})(/|\\\\.|$)" 1;
    }}
    # The telemetry dash entries guard the old serve's routes: they leave with the
    # gateway redeploy once no old serve runs (DATABASE.md, the sink's rollout).
    # An admin segment under an API mount is a bearer-only route, as are the worlds collector and
    # reprocess aliases; /events/admin pages stay public.
    map $uri $operator_path {{
        volatile;
        default 0;
        "~*^/(admin|server)(\\\\W|$)" 1;
        "~*^/((shop-api|marketplace-api|market|transactions-api|signatures-api|credits|rpc|price|badges|builder-api|camera-reel-service|camera-reel|worlds-content-server|world-storage|scene-state|presence|social-api|social-rpc|comms-gatekeeper)/)?((v1|api)/)?admin(\\\\W|$)" 1;
        "~*^/(places|events)/(v1|api)/admin(\\\\W|$)" 1;
        "~*^/worlds-content-server/(gc|reprocess-ab)(\\\\W|$)" 1;
        "~*^/metabase(/|\\\\.|$)" 1;
        "~*^/telemetry/dash/(sql|flag|group/target)(\\\\.data)?/?$" 1;
        "~*^/telemetry/dash/admin(/|\\\\.|$)" 1;
    }}
    map "$operator_path:$admin_gate" $operator_denied {{ volatile; default 0; "1:" 1; }}
    # Listed sync peers (the API's TRUSTED_SYNC_PEER_IPS) get an empty key on
    # GET and HEAD /contents/{{hash}}, which nginx does not count: upstream exempts
    # a peer that syncs from it on that route only.
    geo $sync_peer {{ default 0; {sync_peer_geo} }}
    map "$sync_peer:$request_method:$uri" $content_limit_key {{
        default $binary_remote_addr;
        "~^1:(GET|HEAD):(/content)?/contents/[^/]+/*$" "";
    }}
    geo $storage_trusted {{ default 0; {storage_geo} }}
    map $storage_trusted $storage_limit_key {{ 1 ""; default $binary_remote_addr; }}
    map "$storage_trusted:$request_method" $storage_write_key {{
        default $binary_remote_addr;
        "~^1:" "";
        "0:GET" "";
        "0:HEAD" "";
        "0:OPTIONS" "";
    }}
    map $storage_trusted $storage_trusted_key {{ 1 $binary_remote_addr; default ""; }}
    limit_req_zone $storage_write_key zone=storage_write:10m rate=30r/s;
    limit_req_zone $storage_trusted_key zone=storage_trusted:10m rate=1000r/s;
    # Requests with an empty key are not counted: worlds reads are not limited, while
    # writes and the admin routes (admin networks only) get 30 r/s per address.
    map "$request_method:$uri" $worlds_limit_key {{
        default $binary_remote_addr;
        "~^[^:]+:/worlds-content-server/(admin|gc|reprocess-ab)(/|$)" $binary_remote_addr;
        "~^(GET|HEAD|OPTIONS):" "";
    }}
    limit_req_zone $worlds_limit_key zone=worlds:10m rate=30r/s;
    limit_conn_zone $worlds_limit_key zone=worlds_uploads:10m;
    limit_req_zone $binary_remote_addr zone=read:10m rate={read_rate}r/s;
    limit_req_zone $binary_remote_addr zone=webhook:10m rate={WEBHOOK_RATE}r/s;
    limit_req_zone $content_limit_key zone=content:10m rate=100r/s;
    limit_req_zone $storage_limit_key zone=storage:10m rate=100r/s;
    limit_req_zone $binary_remote_addr zone=deploy:10m rate=5r/s;
    limit_req_status 429;
    limit_conn_zone $binary_remote_addr zone=deploy_uploads:10m;
    limit_conn_zone $storage_write_key zone=storage_uploads:10m;
    limit_conn_status 429;
    proxy_http_version 1.1;
    proxy_read_timeout 600s;
    proxy_buffering off;
    proxy_set_header Host $http_host;
    proxy_set_header X-Forwarded-Host $http_host;
    proxy_set_header X-Forwarded-Proto $forwarded_scheme;
    proxy_set_header X-Forwarded-For $remote_addr;
    proxy_set_header X-Real-IP $remote_addr;
    proxy_set_header CF-Connecting-IP $remote_addr;
    proxy_set_header True-Client-IP "";
    proxy_set_header Forwarded "";
    proxy_set_header X-Catalyst-Admin-Gate $admin_gate;
    proxy_set_header x-original-path $original_path;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection $connection_keepalive;
    {security_headers}
    add_header Access-Control-Allow-Origin $cors_fallback always;
    server {{
        listen 8080 default_server;
        server_name _;
        location = /_gateway/healthz {{ access_log off; return 200 "ok\\n"; }}
        location / {{ return 404; }}
    }}
    server {{
        listen 8080;
        server_name {domain_names};
        if ($storage_raw_uri) {{ rewrite ^ /__gateway_storage_raw last; }}
        if ($ambiguous_path) {{ return 400; }}
        if ($gateway_uri) {{ rewrite ^ $gateway_uri; }}
        if ($internal_path) {{ return 404; }}
        if ($operator_denied) {{ return 403; }}
        client_max_body_size 1m;
        limit_req zone=read burst={read_burst} nodelay;
        {locations}
    }}
}}
"""


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--table", required=True, type=Path)
    parser.add_argument("--nginx", required=True)
    parser.add_argument("--mime", required=True)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    try:
        text = render(json.loads(args.table.read_text()), settings(os.environ), args.mime)
    except ValueError as error:
        parser.error(str(error))
    folder = Path("/tmp/gateway")
    folder.mkdir(mode=0o700, exist_ok=True)
    conf = folder / "nginx.conf"
    conf.write_text(text)
    command = [args.nginx, "-p", str(folder), "-c", str(conf)]
    subprocess.run([*command, "-t"], check=True)
    if not args.check:
        os.execv(args.nginx, [*command, "-g", "daemon off;"])


if __name__ == "__main__":
    main()
