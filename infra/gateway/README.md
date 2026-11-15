# Gateway

Nginx public entry for APIs and Sites (routing source).

The exact `/places/place?position=...` and `/places/world?name=...` endpoints, including their trailing-slash forms, stay backend social cards; `/places/place/<position>`, `/places/world/<name>` and their `.data` requests go to Sites, and the JSON catalogue stays under `/places/api`.

The exact `/events/assets/event-default.webp` path serves Catalyst's bundled event image. Event image links use the node's `PUBLIC_URL` plus the `/events` mount; `EVENTS_BASE_URL` remains the website URL used by sitemaps.

Referral quotas, the `POST /entities` burst limit and daily quota, the
`/auth-api` pending sign-in caps and the
`/contents` rate limit use the backend TCP peer by default. Configure
`HTTP_TRUSTED_PROXY_CIDRS` on the API process with the direct backend proxy
networks (the gateway's container network), and ensure those proxies overwrite
a single valid `X-Real-IP`. An
empty setting trusts no forwarded header (only a loopback peer may name the
client to the other per-client limits and IP bindings) and therefore buckets
traffic behind a non-loopback gateway under that gateway's peer address: every
deployer then shares one deploy limit. `/auth-api` instead skips its
per-network cap for an untrusted private or loopback peer and keeps only its
global cap. The API logs one warning when an untrusted private peer forwards
`X-Real-IP`, and `/auth-api` one the first time it sees an untrusted private or
loopback peer. Missing or malformed X-Real-IP from a trusted peer
fails closed. This API-side setting is separate from
`GATEWAY_TRUSTED_PROXY_CIDRS`, which controls the gateway's upstream CDN trust.
Verify the deployed trust chain and client bucket separation before cutover:
without it every client behind the gateway shares one `/contents` budget
(1000 requests a minute by default) and gets 429s past it.

Place reports count against the reporter's wallet and, when its address names
one client (a trusted proxy's `X-Real-IP` or a public peer), its network. Behind
an unlisted private gateway only the wallet limit applies, and fresh wallets cost
nothing, so set `HTTP_TRUSTED_PROXY_CIDRS` to keep the per-network report limit.

Set `PUBLIC_URL` to the externally reachable node base URL, including any deployment prefix. Without it, the node uses its bind address (for example `http://0.0.0.0:<port>`), which is only a local fallback and cannot provide public image or notification links. See [event client and deployment guidance](../../services/catalyst/docs/events.md).

```sh
nix build .#image-gateway --out-link .cache/gateway-image
docker load --input .cache/gateway-image
python3 infra/gateway/smoke.py --revision "$(git rev-parse HEAD)" --arch amd64   # or arm64
docker run --rm --network dcl-one --read-only --cap-drop ALL --security-opt no-new-privileges \
  --tmpfs /tmp:rw,nosuid,size=512m -p 8080:8080 -e GATEWAY_DOMAIN=example.org \
  quay.io/decentraland/dcl-one-gateway:main
```

| Setting | Default | Meaning |
| --- | --- | --- |
| `GATEWAY_DOMAIN` | `localhost` | Base domain |
| `GATEWAY_HOST_ALIASES` | empty | Extra apex hosts |
| `GATEWAY_APIS_UPSTREAMS` | `apis:5210` | API destinations |
| `GATEWAY_SITES_UPSTREAMS` | `sites:5212` | Sites destinations |
| `GATEWAY_ADMIN_CIDRS` | empty | Operator networks; empty denies all |
| `GATEWAY_TRUSTED_PROXY_CIDRS` | empty | Proxies trusted for client IP and scheme |
| `GATEWAY_READ_RATE` | `100` | Requests per second per client outside the content, storage, worlds and deploy zones |
| `GATEWAY_READ_BURST` | `300` | Burst allowed above `GATEWAY_READ_RATE` (a web explorer boot is ~150 requests in its first second) |
| `GATEWAY_TRUSTED_SYNC_PEER_IPS` | empty | Sync peers exempt from the `read` and `content` request limits; list the API's `TRUSTED_SYNC_PEER_IPS` |
| `GATEWAY_STORAGE_TRUSTED_CIDRS` | empty | Client networks (such as scene-host egress) with a higher, bounded storage budget |
| `GATEWAY_RESOLVERS` | resolv.conf | DNS servers |
| `GATEWAY_STORAGE_ROOT` | empty | Content mount; API sets `STORAGE_X_ACCEL_BASE=/__protected_storage` |

- Publish only port 8080; keep APIs and Sites private; terminate TLS before it.
- Operator paths (`/admin`, `/server`, `/metabase`, telemetry admin, and an `admin` segment directly under an API mount or its `/v1` or `/api`, such as `/rpc/admin` or `/shop-api/v1/admin`, and the worlds `/worlds-content-server/gc` and `/reprocess-ab`) are gated on the normalized path; an allowed network does not authenticate.
- Never trust arbitrary client networks as proxies; a trusted proxy must set `X-Forwarded-For` and `X-Forwarded-Proto`.
- The gateway overwrites client IP, forwarding, `X-Catalyst-Admin-Gate` and `x-original-path` headers and removes `True-Client-IP`.
- `/internal/pulse` and `/metrics` are not public; internal callers use the APIs' private address.
- Limits: bodies 1 MiB (content deployments 248, worlds deployments 360, world settings 12, world storage 529,408 bytes (the service's largest body plus slack), builder 21, report evidence 5); 100 r/s per client with a burst of 300 (`GATEWAY_READ_RATE`, `GATEWAY_READ_BURST`; content 100, worlds writes and admin routes 30, Segment webhooks under `/events-notifier` 300 with a burst of 600, deployments 5 with a burst of 200 for content and 4 for worlds); 10 deployment uploads in flight per client, then 429; `%3F` in paths returns 400, except in a `/world-storage` path forwarded unrewritten.
- World storage keys: a `storage.<domain>`, `world-storage.<domain>` or `gateway.<domain>/storage` request is forwarded from the path as the client sent it, under `/world-storage`, so `%2F` and `%3F` in a key reach the service; a path with dot or empty segments takes the normalized rewrite instead. The storage body cap answers the service's JSON 413.
- `/world-storage` (after host rewriting): reads 100 r/s per client (burst 400), writes also 30 r/s (burst 60). Each client address in `GATEWAY_STORAGE_TRUSTED_CIDRS` gets a separate 1000 r/s budget (burst 1000); the allowance is per address, not per CIDR, and never disables limiting. Startup rejects overlaps with `GATEWAY_TRUSTED_PROXY_CIDRS`.

A service subdomain `<sub>.<domain>` reaches the APIs under `/<sub>`, except
upstream host names served under another prefix: `abcdn` and `abgen-cdn` →
`/ab-cdn`, `asset-bundle-registry-abgen` → `/asset-bundle-registry`,
`rpc-social-service-ea` → `/social-rpc`, `market` → `/shop`, `storage` →
`/world-storage`, `comms-gatekeeper-local` → `/comms-gatekeeper`; `peer` and
`worlds` are the root. `gateway.<domain>/<sub>/...` uses the same table, and a
path already under the target prefix is not prefixed twice.
