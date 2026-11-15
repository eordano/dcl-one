# Web driver

A runner for the web client's tests that needs nothing installed but Node 24 and Chrome. It
launches Chrome over the DevTools protocol and gives a test what the standalone's `dcl/console`
and CEF input give it:

- **Console commands.** `console(line)` runs `await window.engine_console_command(line)` in the
  app frame and returns what it resolves with, or rejects with the page's message.
- **Trusted input.** `/hud_click`, `/hud_fill`, `/press_key` and `/click_at` (page side:
  `library/explorer-core/lib/hudAutomation.ts`) ask the runner for real input instead of dispatching DOM
  events, so the page sees `isTrusted` events.

Chrome is started with `--remote-debugging-pipe`, never a port (the benchmark runner forbids
`--remote-debugging-port`).

## Use

```sh
node local-harness/web-driver/driver.mjs --url http://localhost:5173/ '/hud_click Save'
```

```
node driver.mjs --url <url> [--chrome <path>] [--headed] [--arg <flag>]... [command ...]
```

One JSON line per command, `{"command", "ok", "output"}` or `{"command", "ok": false, "error"}`,
with the credentials of a `/login_identity` or `/hud_login` line (an identity valid for days, found
by its shape wherever the line puts it: base64 of its JSON, every one on the line, or the JSON
itself) replaced by `<redacted>` wherever they appear;
without commands it reads them from stdin, one per line. Exit code 1 if a command failed, 2 for
a bad command line. On Git Bash for Windows, pass commands on stdin: it rewrites an argument that
starts with `/`.

From a script:

```js
import { launch } from './driver.mjs'
const driver = await launch({ url, args: ['--enable-unsafe-webgpu'] })
await driver.ready()
await driver.console('/hud_click Save')
await driver.close()
```

`findChrome()` takes `CHROME_PATH`, else the places `local-harness/benchmark` looks. `launch`
takes `headless` (default true), `args` (more Chrome flags) and `windowSize`. `ready()` waits for
a frame of the page's origin with both `window.engine_console_command` and
`window.__dclTrustedInput`, and for the page to draw it (see "Wire protocol"); it rejects when its
`timeout` (default 120 s) passes first. A page that never draws, such as a minimised headed window,
makes `ready()` reject at that timeout. `frameTargets()` lists the page and the out-of-process
iframes.
`DCL_WEB_DRIVER_DEBUG=1` logs what the driver ignores or cannot answer.

`consoleMessages()` is what the page said so far, every frame's: `console.*` calls, uncaught
exceptions, the browser's own log entries (a request that failed, a worker that would not
register) and a target that crashed, oldest first, the last 300, each cut at 2000 characters once
the credentials of the lines run were replaced. `screenshot()` is the page as a
JPEG. Both are for a boot that does not become ready: what `ready()`'s timeout cannot say. An
error `launch` throws carries the same messages as its `console`. A first navigation the browser
aborts (`net::ERR_ABORTED`, seen on a browser's first start on a machine) is made once more.

## Wire protocol

The page's side of this is a stable contract, documented in `docs/guidelines/running.md` ("Test
commands"): any DevTools client (a Playwright or Puppeteer script through `exposeBinding` and its
own mouse and keyboard) can install the hook and give the HUD commands real input. This runner is
one implementation of it.

The page calls `window.__dclTrustedInput(json)`, a binding the runner installs in every frame
with `Runtime.addBinding`, before the page's scripts run: the page registers `/hud_login` only if
the binding is there when it installs its commands (`library/explorer-core/lib/hudAutomation.ts`). The
JSON is `{"id", "op", ...}`:

| `op` | fields |
| --- | --- |
| `click` | `x`, `y` (CSS pixels in the calling frame's viewport), `button`: `left`, `right` or `middle` |
| `key` | `code`, `key`, `keyCode` (Windows virtual key code), `text` (optional), `location` (0, 1 left, 2 right), `state` (optional, `down` or `up`; absent presses and releases) |
| `insertText` | `text` |

The runner does the input on the top-level page and then evaluates
`window.__dclTrustedInputDone(id, error)` in the frame that asked, `error` a string or `null`.
It serves a call only from a default (not isolated-world) context of the origin it opened.
`Alt*`, `Control*`, `Meta*` and `Shift*` keys sent with `state` `down` stay held (Alt 1,
Control 2, Meta 4, Shift 8 in CDP's `modifiers`) until sent with `up`, and are on every event
between. A key held with Alt, Control or Meta sends no text.

The runner takes a screenshot of the page once for each document, which waits for the document's
first frame. A frame that has only just appeared cannot be hit-tested until its first frame is
composited, and a click sent earlier is lost (it was, one time in five, with several browsers
starting at once). `ready()` waits for that screenshot within its own timeout, because a browser's
first start on a machine can draw its first frame more than 5 s after the page's scripts have run.
A click from a document `ready()` has not seen waits for it up to 5 s, less than the 10 s the page
waits for the answer, and is then sent. A document pays that wait one time: later clicks are sent
at once.

## Why the frame is offset

The app frame is served with `Document-Isolation-Policy`, so it is a same-origin iframe in its own
process, and its events come from the top-level page in that page's viewport pixels. A click's
`x` and `y` are in the frame, so the runner adds where the frame's content box is: the frame
owner's box (`DOM.getFrameOwner`, `DOM.getBoxModel`) in the session that holds it, and so up to
the page. A frame in the page's own process is found the same way in the page's session.

## Tests

```sh
node --test local-harness/web-driver/driver.test.mjs
```

From `rendering/bevy-explorer/source`. Node 24 reads a directory argument as a file, so name the
file, or use the glob `local-harness/web-driver/*.test.mjs`.

The tests serve `fixture/` (a shell with the app in an iframe away from the corner, with a
border) and run Chrome headless against it, once with the app frame out of process and once in
process. Without Chrome they skip, unless `DCL_WEB_DRIVER_REQUIRE_CHROME=1`; the cases that load
`hudAutomation.ts` need `module.stripTypeScriptTypes`. Each browser keeps its profile in a
temporary folder that `close()` removes.
