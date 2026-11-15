# ADR: The node forwards browser scene requests to listed third-party hosts

- Status: Proposed
- Date: 2026-11-14

## Context

A scene in the browser Explorer runs in a worker on the page's origin. A scene's
`fetch` or `signedFetch` to another origin is a cross-origin browser request,
and a service that answers no CORS preflight is refused by the browser. A native
Explorer or the scene host sends the same request with no such check. Some
services that scenes call, a game room matchmaker and a files bucket among
them, are third-party and outside this tree.

## Decision

- **One route.** `catalyst serve` answers `GET /_scene` with a map of host to a
  path prefix on the node, and forwards `/_scene/<host>/<path>` to
  `https://<host>/<path>` (`services/catalyst/src/scene_proxy.rs`). The page
  reads the map once and hands it to each worker it spawns; the worker's fetch
  (`rendering/bevy-explorer/source/deploy/web/engine/scene_fetch.js`)
  re-addresses a request whose host the map names. The node lets a browser keep
  the map for a minute. A node that answers without a map has none, and the
  page does not ask again; while the node fails or cannot be reached, requests
  go out direct and the page asks once a minute.
- **The operator owns the list.** `SCENE_PROXY_HOSTS` names the forwarded hosts.
  With no host the node serves the map alone. The node's own `/places` and
  `/marketplace-api` routes stand in for the services of those names, with no
  forward.
- **A narrow forward.** GET, HEAD and POST. Of the request: the content type,
  `accept`, `authorization`, ranges, validators and the `x-identity-*`
  signed-fetch headers. Cookies, the origin and forwarding headers stay on the
  node. Of the answer: the status, the body's type, length, range, cache and
  validator headers, and the body. A `set-cookie`, a CORS grant and a `Location`
  never pass. The answer carries `Content-Security-Policy: sandbox` and
  `X-Content-Type-Options: nosniff`, because it is another origin's content on
  the node's origin.
- **The list bounds every hop.** The upstream is dialled through the guarded
  client (public hosts, pinned DNS) and no redirect is followed. A 3xx goes back
  as it is.
- **Limits.** Each is a bounded policy, not a measurement.

  | Limit | Value | Reason |
  | --- | --- | --- |
  | Request body | 1 MiB | the scene host's fetch body cap and the gateway's default body cap |
  | Answer body | 32 MiB | one forward's share of bandwidth; the answer is a stream |
  | Deadline for the whole answer | 20 seconds | above the 15 seconds of the native scene server |
  | Forwards per client address | 300 a minute | five fetch slots of one scene turning over once a second |
  | Forwards in flight | 64 | at most 64 MiB of request bodies in memory |

  The client address is the one the server pins: the socket peer, or the address
  a trusted proxy names. An IPv6 client is its /64. Over its budget an address
  gets `429`; past the in-flight bound a request gets `503`. The budget is held
  in each `serve` process's memory.

## Consequences

- A browser scene's request to a listed host meets the limits above. A native
  scene calls the same host directly and meets only the limits of
  [the scene fetch ADR](2026-11-03-explorer-scene-fetch-limits.md).
- A listed service sees the node's address, not the player's. A signed fetch
  still names the player through its identity headers.
- A service that answers a listed host with a redirect to another host is not
  reachable through the proxy.
- Players behind one address share one budget of forwards.
- Any internet client can reach a listed host through the node, within the
  limits. The list is the control: a host goes on it only when scenes need it.

## Open points

- No dated capture of the listed hosts' preflight answers is in the tree. The
  claim that they refuse a browser scene is unverified here.
- The answer body cap, the deadline and the two rate bounds have no load
  measurement.
- A client that stops reading an answer keeps its in-flight place until its
  connection closes.
