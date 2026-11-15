# ADR: Each Explorer scene owns its Fetch limit

- Status: Accepted
- Date: 2026-11-03

## Context

Scene scripts choose the destinations and lifetimes of their Fetch requests.
Several scenes run in one native sidecar or server process. Browser scenes
each run in a dedicated worker. A scene waiting for a slow endpoint must leave
other scenes free to send requests.

## Decision

Each client or hosted scene may send five Fetch or signedFetch requests at once.
The two APIs share those slots. Additional eligible calls wait in arrival order.
Fetch imposes no cap on outstanding calls. Each native isolate owns its slots
in Deno's `OpState`. Permission requests still use the scene's normal per-tick
RPC queue. Each browser sandbox owns its Fetch queue for the worker's lifetime.

Native scenes acquire a slot after permission is granted and hold it through the
response body. The native authoritative server starts a 15-second deadline at
slot acquisition. Permission and queue waits do not consume it. Native clients
have no overall request deadline. A scene may set its own deadline with
`AbortSignal.timeout()`. Aborting a native call withdraws its pending permission
prompt or queue entry, or releases the slot if the transfer has started.

Browser scenes, including hosted scenes running in a user's browser, have no
engine-imposed deadline. The slot is released at headers, when the caller takes
ownership of the response stream. An open body can still occupy a browser
connection. Other browser facilities, such as module imports, use the browser's
own scheduling.

A browser request to a host that the node's scene proxy forwards meets that
proxy's limits: a 20-second deadline for the whole answer, a 1 MiB request body
and a 32 MiB answer ([ADR](2026-11-14-scene-proxy.md)). A native scene calls the
same host directly and meets none of them.
