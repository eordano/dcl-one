import { after, afterEach, before, describe, it } from 'node:test'
import assert from 'node:assert/strict'
import { chmod, mkdtemp, readFile, rm, writeFile, stat } from 'node:fs/promises'
import path from 'node:path'
import os from 'node:os'
import { EventEmitter } from 'node:events'
import { execFileSync } from 'node:child_process'
import { createServer, type Server } from 'node:http'
import type { AddressInfo } from 'node:net'
import { browserArgs } from '../scripts/browser-args.mjs'
import { findBrowser, openSession } from '../scripts/capture-browser.mjs'
import { cleanupCapture, installCaptureSignals } from '../scripts/capture-teardown.mjs'

const folders: string[] = []
afterEach(async () => {
  for (const folder of folders.splice(0)) await rm(folder, { recursive: true, force: true })
})
async function script(body: string) {
  const folder = await mkdtemp(path.join(os.tmpdir(), 'capture-browser-test-'))
  folders.push(folder)
  const file = path.join(folder, 'browser')
  await writeFile(file, `#!/bin/sh\nexec '${process.execPath}' -e '${body}' -- "$@"\n`)
  await chmod(file, 0o755)
  return { folder, file }
}

describe('when starting a capture browser', () => {
  it('should skip a non-executable candidate', async () => {
    const { file } = await script('process.exit(0)')
    await chmod(file, 0o644)
    assert.equal(await findBrowser([file, process.execPath]), process.execPath)
  })
  it('should fail promptly if the child exits before publishing its DevTools port', async () => {
    const { file } = await script('process.exit(7)')
    const begin = Date.now()
    await assert.rejects(openSession(file), /exited during startup/)
    assert.ok(Date.now() - begin < 2000)
  })
  it('should cancel startup and remove the browser profile', async () => {
    const { folder, file } = await script('process.exit(0)')
    const marker = path.join(folder, 'profile')
    await writeFile(
      file,
      `#!/bin/sh\nexec '${process.execPath}' -e 'require("node:fs").writeFileSync(${JSON.stringify(marker)}, process.argv.find(x=>x.startsWith("--user-data-dir=")).slice(16)); setInterval(()=>{},1000)' -- "$@"\n`
    )
    const controller = new AbortController()
    const pending = openSession(file, { signal: controller.signal })
    let profile = ''
    for (let i = 0; i < 100; i++) {
      try {
        profile = await readFile(marker, 'utf8')
        break
      } catch {
        await new Promise((resolve) => setTimeout(resolve, 10))
      }
    }
    assert.ok(profile)
    controller.abort(new Error('cancelled startup'))
    await assert.rejects(pending, /cancelled startup/)
    await assert.rejects(stat(profile), { code: 'ENOENT' })
  })
})
describe('when startup diagnostics explain a failed browser launch', () => {
  function report(calls: { arguments: unknown[] }[]) {
    const call = calls.find(
      (entry) => entry.arguments[0] === 'Capture browser startup diagnostics:'
    )
    assert.ok(call, 'a failed launch must report its bounded diagnostic')
    return JSON.parse(String(call.arguments[1]))
  }

  it('should identify the executable, available version and child exit without disclosing its path', async (t) => {
    const log = t.mock.method(console, 'error', () => {})
    const { file } = await script(
      'require("node:fs").writeSync(2,"Chromium 157.0.0.1\\nlaunch failed\\n");process.exit(7)'
    )
    await assert.rejects(openSession(file), /exited during startup/)
    const diagnostic = report(log.mock.calls)
    assert.equal(diagnostic.executable, 'browser')
    assert.equal(diagnostic.version, '157.0.0.1')
    assert.equal(diagnostic.exitCode, 7)
    assert.equal(diagnostic.signal, null)
    assert.equal(diagnostic.phase, 'port-file')
    assert.match(diagnostic.stderr, /launch failed/)
    assert.ok(!JSON.stringify(diagnostic).includes(path.dirname(file)))
  })

  it('should let the caller exit when a failed browser leaves an inherited stderr writer', async () => {
    const { file } = await script(
      'require("node:child_process").spawn(process.execPath,["-e","setTimeout(()=>{},8000)"],{stdio:["ignore","ignore",2]}).unref();process.exit(7)'
    )
    const source = new URL('../scripts/capture-browser.mjs', import.meta.url).href
    const runner = `import {openSession} from ${JSON.stringify(source)};try{await openSession(${JSON.stringify(file)});process.exitCode=1}catch(error){if(!error.message.includes('exited during startup'))throw error}`
    execFileSync(process.execPath, ['--input-type=module', '-e', runner], {
      timeout: 3000,
      stdio: 'pipe'
    })
  })

  it('should report a child signal before cleanup changes its process state', async (t) => {
    const log = t.mock.method(console, 'error', () => {})
    const { file } = await script('process.kill(process.pid,"SIGTERM")')
    await assert.rejects(openSession(file), /exited during startup/)
    const diagnostic = report(log.mock.calls)
    assert.equal(diagnostic.exitCode, null)
    assert.equal(diagnostic.signal, 'SIGTERM')
  })

  it('should preserve an empty port failure and expose the empty-read state', async (t) => {
    const log = t.mock.method(console, 'error', () => {})
    const { file } = await script(
      'const dir=process.argv.find(x=>x.startsWith("--user-data-dir=")).slice(16);require("node:fs").writeFileSync(dir+"/DevToolsActivePort","");setInterval(()=>{},1000)'
    )
    await assert.rejects(openSession(file), /did not start/)
    const diagnostic = report(log.mock.calls)
    assert.equal(diagnostic.port.state, 'empty')
    assert.equal(diagnostic.port.validObserved, false)
  })

  it('should report a partial port read without changing the existing reader behavior', async (t) => {
    const log = t.mock.method(console, 'error', () => {})
    const { file } = await script(
      'const dir=process.argv.find(x=>x.startsWith("--user-data-dir=")).slice(16);require("node:fs").writeFileSync(dir+"/DevToolsActivePort","not-a-port");setInterval(()=>{},1000)'
    )
    await assert.rejects(openSession(file), /URL|fetch/i)
    const diagnostic = report(log.mock.calls)
    assert.equal(diagnostic.port.state, 'partial')
    assert.equal(diagnostic.port.validObserved, false)
    assert.equal(diagnostic.port.lastFsError, null)
    assert.equal(diagnostic.phase, 'endpoint')
  })

  it('should retain the last filesystem error when the start limit expires', async (t) => {
    const log = t.mock.method(console, 'error', () => {})
    const { file } = await script(
      'const dir=process.argv.find(x=>x.startsWith("--user-data-dir=")).slice(16);require("node:fs").mkdirSync(dir+"/DevToolsActivePort");setInterval(()=>{},1000)'
    )
    const started = performance.now()
    await assert.rejects(openSession(file, { startTimeoutMs: 500 }), /did not start/)
    const waited = performance.now() - started
    assert.ok(waited >= 500 && waited < 5000, `waited ${waited} ms`)
    const diagnostic = report(log.mock.calls)
    assert.equal(diagnostic.port.lastFsError, 'EISDIR')
    assert.equal(diagnostic.port.validObserved, false)
    assert.equal(diagnostic.exitCode, null)
    assert.equal(diagnostic.signal, null)
  })

  it('should sanitize split stderr lines and omit overlong lines before bounding the report', async (t) => {
    const log = t.mock.method(console, 'error', () => {})
    const key = 'capture-test-key'
    const previous = process.env.FIGMA_FILE_KEY
    process.env.FIGMA_FILE_KEY = key
    try {
      const url = ['https://', 'www.', 'figma', '.com/design/', key, '/private'].join('')
      const payload =
        'x'.repeat(6000) +
        '\n' +
        url +
        '\n' +
        key.toUpperCase() +
        '\n' +
        os.homedir() +
        '/private-profile\nlaunch failed\n'
      const split = 6000 + 24
      const { file } = await script(
        'require("node:fs").writeSync(2,' +
          JSON.stringify(payload.slice(0, split)) +
          ');setTimeout(()=>{require("node:fs").writeSync(2,' +
          JSON.stringify(payload.slice(split)) +
          ');process.exit(9)},20)'
      )
      await assert.rejects(openSession(file), /exited during startup/)
      const diagnostic = report(log.mock.calls)
      assert.ok(diagnostic.stderr.length <= 4096)
      assert.ok(!diagnostic.stderr.toLowerCase().includes(key))
      assert.ok(!diagnostic.stderr.includes(url))
      assert.ok(!diagnostic.stderr.includes(os.homedir()))
      assert.match(diagnostic.stderr, /omitted|redacted/)
      assert.match(diagnostic.stderr, /launch failed/)
    } finally {
      if (previous === undefined) delete process.env.FIGMA_FILE_KEY
      else process.env.FIGMA_FILE_KEY = previous
    }
  })

  it('should redact a configured Unicode home split between stderr chunks', async (t) => {
    const log = t.mock.method(console, 'error', () => {})
    const home = '/test-home-caf' + String.fromCharCode(233)
    t.mock.method(os, 'homedir', () => home)
    const payload = Buffer.from(home + '/profile\nlaunch failed\n')
    const split = payload.indexOf(Buffer.from(String.fromCharCode(233))) + 1
    const { file } = await script(
      'const data=Buffer.from(' +
        JSON.stringify(payload.toString()) +
        ');require("node:fs").writeSync(2,data.subarray(0,' +
        split +
        '));setTimeout(()=>{require("node:fs").writeSync(2,data.subarray(' +
        split +
        '));process.exit(8)},20)'
    )
    await assert.rejects(openSession(file), /exited during startup/)
    const diagnostic = report(log.mock.calls)
    assert.ok(!diagnostic.stderr.includes('test-home'))
    assert.match(diagnostic.stderr, /redacted/)
  })

  it('should preserve startup failure and cleanup when home lookup fails', async (t) => {
    const log = t.mock.method(console, 'error', () => {})
    const { folder, file } = await script('process.exit(0)')
    const marker = path.join(folder, 'profile')
    await writeFile(
      file,
      `#!/bin/sh\nexec '${process.execPath}' -e 'const fs=require("node:fs");fs.writeFileSync(${JSON.stringify(marker)},process.argv.find(x=>x.startsWith("--user-data-dir=")).slice(16));fs.writeSync(2,"private startup details\\n");process.exit(8)' -- "$@"\n`
    )
    t.mock.method(os, 'homedir', () => {
      throw new Error('home unavailable')
    })
    await assert.rejects(openSession(file), /exited during startup/)
    await assert.rejects(stat(await readFile(marker, 'utf8')), { code: 'ENOENT' })
    assert.ok(!report(log.mock.calls).stderr.includes('private startup details'))
  })

  it('should preserve diagnostic slashes when the home directory is the filesystem root', async (t) => {
    const log = t.mock.method(console, 'error', () => {})
    t.mock.method(os, 'homedir', () => '/')
    const { file } = await script(
      'require("node:fs").writeSync(2,"port /tmp/devtools\\n");process.exit(8)'
    )
    await assert.rejects(openSession(file), /exited during startup/)
    assert.match(report(log.mock.calls).stderr, /port \/tmp\/devtools/)
  })

  it('should snapshot startup output before teardown noise is emitted', async (t) => {
    const log = t.mock.method(console, 'error', () => {})
    const { file } = await script(
      'const fs=require("node:fs");const dir=process.argv.find(x=>x.startsWith("--user-data-dir=")).slice(16);fs.writeSync(2,"startup failure\\n");fs.writeFileSync(dir+"/DevToolsActivePort","not-a-port");process.on("SIGTERM",()=>{fs.writeSync(2,"shutdown noise\\n".repeat(500));process.exit(0)});setInterval(()=>{},1000)'
    )
    await assert.rejects(openSession(file), /URL|fetch/i)
    assert.match(report(log.mock.calls).stderr, /startup failure/)
    assert.ok(!report(log.mock.calls).stderr.includes('shutdown noise'))
  })

  it('should retain the primary failure if diagnostic logging is unavailable', async (t) => {
    t.mock.method(console, 'error', () => {
      throw new Error('logger unavailable')
    })
    const { file } = await script('process.exit(8)')
    await assert.rejects(openSession(file), /exited during startup/)
  })

  it('should preserve an explicit abort reason without emitting a startup failure diagnostic', async (t) => {
    const log = t.mock.method(console, 'error', () => {})
    const controller = new AbortController()
    const reason = new Error('expected test cancellation')
    const { file } = await script('setInterval(()=>{},1000)')
    const pending = openSession(file, { signal: controller.signal })
    controller.abort(reason)
    await assert.rejects(pending, (error) => error === reason)
    assert.equal(log.mock.calls.length, 0)
  })
})

describe('when capture cleanup also fails', () => {
  it('should preserve the primary failure and report the cleanup failure', async () => {
    const primary = new Error('capture failed')
    const cleanup = new Error('profile busy')
    const diagnostics: unknown[][] = []
    await cleanupCapture(
      primary,
      async () => {
        throw cleanup
      },
      (...items: unknown[]) => diagnostics.push(items)
    )
    assert.deepEqual(diagnostics, [['Capture cleanup failed:', cleanup]])
  })
  it('should surface cleanup failures after a successful capture', async () => {
    await assert.rejects(
      cleanupCapture(undefined, async () => {
        throw new Error('profile busy')
      }),
      /profile busy/
    )
  })
})
describe('when a capture receives a termination signal', () => {
  for (const [name, code] of [
    ['SIGINT', 130],
    ['SIGTERM', 143]
  ] as const)
    it(`should abort on ${name}, set its exit code and remove handlers`, () => {
      const target = Object.assign(new EventEmitter(), { exitCode: 0 })
      const controller = new AbortController()
      const dispose = installCaptureSignals(controller, target)
      target.emit(name)
      assert.equal(controller.signal.aborted, true)
      assert.equal(target.exitCode, code)
      dispose()
      assert.equal(target.listenerCount('SIGINT') + target.listenerCount('SIGTERM'), 0)
    })
})

// Ported from figma-design-system f63bb11 (the launch flag only).
describe('when a capture launches its browser', () => {
  it('should turn off Brave canvas farbling, so a screenshot always measures the same', () => {
    assert.ok(browserArgs('/tmp/profile').includes('--disable-features=BraveFarbling'))
  })
})

// A DevTools endpoint stand-in: the module reads fetch and WebSocket from globalThis when it loads, so a fresh
// copy is imported while both are stubbed. Every CDP command gets an empty result at once.
class FakeSocket extends EventTarget {
  static readonly OPEN = 1
  static readonly CLOSED = 3
  static last: FakeSocket | undefined
  readyState = 0
  sent: string[] = []
  constructor() {
    super()
    FakeSocket.last = this
    queueMicrotask(() => {
      this.readyState = FakeSocket.OPEN
      this.dispatchEvent(new Event('open'))
    })
  }
  send(data: string) {
    this.sent.push(data)
    const { id } = JSON.parse(data) as { id: number }
    queueMicrotask(() =>
      this.dispatchEvent(
        Object.assign(new Event('message'), { data: JSON.stringify({ id, result: {} }) })
      )
    )
  }
  close() {
    this.readyState = FakeSocket.CLOSED
  }
}
async function stubbedOpenSession(): Promise<typeof openSession> {
  const { fetch, WebSocket } = globalThis
  Object.assign(globalThis, {
    fetch: async () =>
      new Response(JSON.stringify({ webSocketDebuggerUrl: 'ws://127.0.0.1:1/devtools' })),
    WebSocket: FakeSocket
  })
  try {
    const specifier = new URL('../scripts/capture-browser.mjs?stubbed-cdp', import.meta.url).href
    return ((await import(specifier)) as { openSession: typeof openSession }).openSession
  } finally {
    Object.assign(globalThis, { fetch, WebSocket })
  }
}
// A browser that publishes a DevTools port in its profile and then stays up until it is stopped.
const PORT_BROWSER =
  'const dir = process.argv.find(x=>x.startsWith("--user-data-dir=")).slice(16); require("node:fs").writeFileSync(dir + "/DevToolsActivePort", "1\\n"); setInterval(()=>{},1000)'

describe('when a command is sent on a capture session that can no longer answer', () => {
  it('should reject a send after close at once instead of waiting out the CDP timeout', async () => {
    const open = await stubbedOpenSession()
    const { file } = await script(PORT_BROWSER)
    const session = await open(file)
    await session.close()
    const sentBefore = FakeSocket.last!.sent.length
    const begin = Date.now()
    await assert.rejects(session.send('Runtime.evaluate'), /Capture browser session closed/)
    assert.ok(Date.now() - begin < 1000)
    assert.equal(FakeSocket.last!.sent.length, sentBefore)
  })
  it('should reject a send on a socket the browser already closed, before the session is closed', async () => {
    const open = await stubbedOpenSession()
    const { file } = await script(PORT_BROWSER)
    const session = await open(file)
    try {
      FakeSocket.last!.readyState = FakeSocket.CLOSED
      const begin = Date.now()
      await assert.rejects(session.send('Runtime.evaluate'), /Capture browser session closed/)
      assert.ok(Date.now() - begin < 1000)
    } finally {
      await session.close()
    }
  })
})

// A DevTools stand-in that answers Page.getFrameTree with a top frame, and Runtime.evaluate and Page.navigate with what
// `reply` returns:
// the Page events to deliver first, then the answer.
class ScriptedSocket extends FakeSocket {
  static reply: (method: string) => { events: object[]; answer: object } = () => ({
    events: [],
    answer: { result: {} }
  })
  override send(data: string) {
    this.sent.push(data)
    const { id, method } = JSON.parse(data) as { id: number; method: string }
    const { events, answer } =
      method === 'Page.getFrameTree'
        ? { events: [], answer: { result: { frameTree: { frame: { id: 'top' } } } } }
        : method === 'Runtime.evaluate' || method === 'Page.navigate'
          ? ScriptedSocket.reply(method)
          : { events: [], answer: { result: {} } }
    queueMicrotask(() => {
      for (const event of [...events, { id, ...answer }])
        this.dispatchEvent(Object.assign(new Event('message'), { data: JSON.stringify(event) }))
    })
  }
}
type CaptureBrowser = typeof import('../scripts/capture-browser.mjs')
async function scriptedCaptureBrowser(): Promise<CaptureBrowser> {
  const { fetch, WebSocket } = globalThis
  Object.assign(globalThis, {
    fetch: async () =>
      new Response(JSON.stringify({ webSocketDebuggerUrl: 'ws://127.0.0.1:1/devtools' })),
    WebSocket: ScriptedSocket
  })
  try {
    const specifier = new URL('../scripts/capture-browser.mjs?scripted-cdp', import.meta.url).href
    return (await import(specifier)) as CaptureBrowser
  } finally {
    Object.assign(globalThis, { fetch, WebSocket })
  }
}
const contextLost = { error: { code: -32000, message: 'Inspected target navigated or closed' } }
const started = (frameId: string) => ({
  method: 'Page.frameStartedNavigating',
  params: { frameId }
})

describe('when an evaluation loses its document', () => {
  afterEach(() => {
    ScriptedSocket.reply = () => ({ events: [], answer: { result: {} } })
  })
  // Resolves with the evaluation's error, and the NavigationInterrupted class of the module that raised it.
  const evaluateWith = async (events: object[], answer: object) => {
    const { openSession: open, NavigationInterrupted } = await scriptedCaptureBrowser()
    const { file } = await script(PORT_BROWSER)
    const session = await open(file)
    ScriptedSocket.reply = () => ({ events, answer })
    try {
      await session.evaluate('1')
      throw new Error('the evaluation resolved')
    } catch (error) {
      return { error: error as Error, NavigationInterrupted }
    } finally {
      await session.close()
    }
  }
  it('should report a navigation of the top frame that started while it was pending', async () => {
    const { error, NavigationInterrupted } = await evaluateWith([started('top')], contextLost)
    assert.ok(error instanceof NavigationInterrupted, String(error))
    assert.match(String(error.cause), /Inspected target navigated or closed/)
  })
  it('should keep the CDP error when no navigation was observed, as a closed or crashed target answers', async () => {
    const { error, NavigationInterrupted } = await evaluateWith([], contextLost)
    assert.ok(!(error instanceof NavigationInterrupted))
    assert.match(error.message, /Inspected target navigated or closed/)
  })
  it('should keep the CDP error when only a child frame navigated', async () => {
    const { error, NavigationInterrupted } = await evaluateWith([started('child')], contextLost)
    assert.ok(!(error instanceof NavigationInterrupted))
    assert.match(error.message, /Inspected target navigated or closed/)
  })
  it('should keep any other CDP error during a navigation', async () => {
    const { error, NavigationInterrupted } = await evaluateWith([started('top')], {
      error: { code: -32601, message: 'method not found' }
    })
    assert.ok(!(error instanceof NavigationInterrupted))
    assert.match(error.message, /method not found/)
  })
})

describe('when a navigation is answered before its document commits', () => {
  afterEach(() => {
    ScriptedSocket.reply = () => ({ events: [], answer: { result: {} } })
  })
  const deliver = (...events: object[]) => {
    for (const event of events)
      FakeSocket.last!.dispatchEvent(
        Object.assign(new Event('message'), { data: JSON.stringify(event) })
      )
  }
  const navigateWith = async (answer: object) => {
    const { openSession: open } = await scriptedCaptureBrowser()
    const { file } = await script(PORT_BROWSER)
    const session = await open(file)
    ScriptedSocket.reply = () => ({ events: [], answer })
    let settled = false
    const navigation = session.navigate('http://127.0.0.1:1/next', 1000).then(() => {
      settled = true
    })
    await new Promise((resolve) => setTimeout(resolve, 20))
    return { session, navigation, settled: () => settled }
  }
  it('should wait for the document of the loader it started, not the idle frame it answered from', async () => {
    const { session, navigation, settled } = await navigateWith({
      result: { frameId: 'top', loaderId: 'next' }
    })
    try {
      assert.equal(settled(), false)
      deliver(
        { method: 'Page.frameStartedLoading', params: { frameId: 'top' } },
        { method: 'Page.frameNavigated', params: { frame: { id: 'top', loaderId: 'next' } } },
        { method: 'Page.frameStoppedLoading', params: { frameId: 'top' } }
      )
      await navigation
    } finally {
      await session.close()
    }
  })
  it('should not wait for a document a child frame commits', async () => {
    const { session, navigation, settled } = await navigateWith({
      result: { frameId: 'top', loaderId: 'next' }
    })
    try {
      deliver({
        method: 'Page.frameNavigated',
        params: { frame: { id: 'child', parentId: 'top', loaderId: 'next' } }
      })
      await new Promise((resolve) => setTimeout(resolve, 20))
      assert.equal(settled(), false)
      await assert.rejects(navigation, /Timed out waiting for the page/)
    } finally {
      await session.close()
    }
  })
  it('should count a request for this tab as a navigation, but not one that opens a new tab', async () => {
    const { openSession: open } = await scriptedCaptureBrowser()
    const { file } = await script(PORT_BROWSER)
    const session = await open(file)
    try {
      const request = (disposition: string) => ({
        method: 'Page.frameRequestedNavigation',
        params: { frameId: 'top', reason: 'anchorClick', url: 'http://127.0.0.1:1/', disposition }
      })
      deliver(request('newTab'))
      assert.deepEqual([session.navigations, session.loading], [0, false])
      deliver(request('currentTab'))
      assert.deepEqual([session.navigations, session.loading], [1, true])
    } finally {
      await session.close()
    }
  })
  it('should resolve a failed navigation once the frame is idle, since it commits no document of its own', async () => {
    const { session, navigation, settled } = await navigateWith({
      result: { frameId: 'top', loaderId: 'failed', errorText: 'net::ERR_ABORTED' }
    })
    try {
      await navigation
      assert.equal(settled(), true)
    } finally {
      await session.close()
    }
  })
})

// A real Chromium against a local server: /slow answers after 1.5 s, /hang sends its headers and never finishes the body, /redirect replaces itself with /landed while it
// loads, and every other path is a small page.
describe('when a real page navigates', () => {
  let session: Awaited<ReturnType<typeof openSession>>
  let server: Server
  let origin: string
  before(async () => {
    server = createServer((request, response) => {
      if (request.url === '/slow')
        return void setTimeout(
          () => response.writeHead(200, { 'content-type': 'text/html' }).end('<title>slow</title>'),
          1500
        )
      if (request.url === '/hang')
        return void response
          .writeHead(200, { 'content-type': 'text/html' })
          .write('<!doctype html>')
      const body =
        request.url === '/redirect'
          ? `<script>location.replace('/landed')</script>`
          : `<title>${request.url}</title>`
      response.writeHead(200, { 'content-type': 'text/html' }).end(`<!doctype html>${body}`)
    })
    await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve))
    origin = `http://127.0.0.1:${(server.address() as AddressInfo).port}`
    session = await openSession(
      await findBrowser([
        process.env.BROWSER_PATH,
        process.env.CHROME_PATH,
        '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome',
        '/usr/bin/google-chrome',
        '/usr/bin/chromium',
        '/usr/bin/chromium-browser'
      ])
    )
  })
  after(async () => {
    await session?.close()
    server.closeAllConnections()
    await new Promise((resolve) => server.close(resolve))
  })
  const evaluate = (expression: string) => session.evaluate(expression)

  it('should resolve navigate only after the document a script redirect lands on has loaded', async () => {
    await session.navigate(`${origin}/redirect`)
    assert.equal(session.loading, false)
    assert.equal(
      await evaluate(`document.readyState + ' ' + location.pathname`),
      'complete /landed'
    )
  })
  it('should reject an evaluation pending when the page navigates as NavigationInterrupted, then evaluate in the new document', async () => {
    await session.navigate(`${origin}/first`)
    const { NavigationInterrupted } = await import('../scripts/capture-browser.mjs')
    await assert.rejects(
      evaluate(`setTimeout(() => { location.href = '/second' }), new Promise(() => {})`),
      NavigationInterrupted
    )
    await session.whenLoaded()
    assert.equal(await evaluate('location.pathname'), '/second')
  })
  it('should count a navigation that replaces the document but not one within it', async () => {
    await session.navigate(`${origin}/first`)
    const before = session.navigations
    await session.navigate(`${origin}/first#fragment`)
    assert.equal(session.navigations, before)
    await session.navigate(`${origin}/second`)
    assert.ok(session.navigations > before)
  })
  it('should count a navigation the page requested as loading before the evaluation that requested it answers', async () => {
    await session.navigate(`${origin}/first`)
    const before = session.navigations
    await evaluate(`location.href = '/slow', true`)
    // The request event comes before that answer; the start and loading events of the navigation come after it.
    assert.equal(session.loading, true)
    assert.ok(session.navigations > before)
    await session.whenLoaded()
    assert.equal(await evaluate('location.pathname'), '/slow')
  })
  it('should still report an exception the page throws as an ordinary error', async () => {
    const { NavigationInterrupted } = await import('../scripts/capture-browser.mjs')
    await assert.rejects(
      evaluate(`(() => { throw new Error('page threw') })()`),
      (error) => !(error instanceof NavigationInterrupted) && /page threw/.test(String(error))
    )
  })
  it('should time out waiting for a document that never finishes loading', async () => {
    await assert.rejects(session.navigate(`${origin}/hang`, 300), /Timed out waiting for the page/)
    assert.equal(session.loading, true)
    const waiting = assert.rejects(session.whenLoaded(), /Capture browser session closed/)
    await session.close()
    await waiting
  })
})
