import { setTimeout, clearTimeout } from 'node:timers'
const { fetch, WebSocket, AbortSignal } = globalThis
import { spawn } from 'node:child_process'
import { constants } from 'node:fs'
import { access, readFile, mkdtemp } from 'node:fs/promises'
import path from 'node:path'
import os from 'node:os'
import { performance } from 'node:perf_hooks'
import { StringDecoder } from 'node:string_decoder'
import { setTimeout as delay } from 'node:timers/promises'
import { browserArgs } from './browser-args.mjs'
import { cleanupCapture, removeProfile, stopBrowser } from './capture-teardown.mjs'

class CaptureTimeout extends Error {}

/**
 * A command was in flight in a document that a navigation of the top frame replaced, which the session observed
 * starting while the command was pending. Chrome answers such a command with -32000; any other answer, or the same
 * answer with no navigation observed (a crash, a closed target), stays an ordinary error.
 */
class NavigationInterrupted extends Error {}

// What Chrome answers for a command whose document or execution context went away.
const CONTEXT_LOST = [
  'Inspected target navigated or closed',
  'Execution context was destroyed',
  'Cannot find context with specified id'
]
const lostContext = (error) =>
  error?.cdp?.code === -32000 && CONTEXT_LOST.some((text) => error.cdp.message?.startsWith(text))

async function findBrowser(candidates) {
  for (const candidate of candidates.filter(Boolean)) {
    try {
      await access(candidate, constants.X_OK)
      return candidate
    } catch {
      /* Try the next executable. */
    }
  }
  throw new Error('A Chromium browser is required. Set BROWSER_PATH to Chrome or Chromium.')
}

function startupRedactor() {
  let patterns
  try {
    patterns = [process.env.FIGMA_FILE_KEY, os.homedir()]
      .filter((value) => value && value !== path.parse(value).root)
      .map((value) => new RegExp(value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&'), 'gi'))
  } catch {
    // If the private-path boundary is unavailable, suppress stderr instead of guessing it.
    return () => '[startup text unavailable]'
  }
  return (text) => {
    text = text
      .split(String.fromCharCode(27))
      .map((part) => part.replace(/^\[[0-?]*[ -/]*[@-~]/, ''))
      .join('')
    for (const pattern of patterns) text = text.replace(pattern, '[redacted]')
    return text.replace(/\b(?:https?|wss?|file):\/\/[^\s"'<>]+/gi, '[redacted-url]')
  }
}

function startupOutput(redactStartup) {
  const decoder = new StringDecoder('utf8')
  let retained = ''
  let pending = ''
  let omitted = false
  let version
  const append = (line) => {
    const safe = redactStartup(line)
    version ??= safe.match(/\b(?:Chromium|Google Chrome|Brave Browser) (\d+(?:\.\d+){1,3})\b/)?.[1]
    retained = (retained + safe).slice(-4096)
  }
  return {
    write(chunk) {
      for (const part of decoder.write(chunk).split(/(?<=\n)/)) {
        if (!omitted) {
          pending += part
          if (pending.length > 4096) {
            pending = ''
            omitted = true
          }
        }
        if (part.endsWith('\n')) {
          append(omitted ? '[stderr line omitted]\n' : pending)
          pending = ''
          omitted = false
        }
      }
    },
    finish() {
      pending += decoder.end()
      append(omitted ? '[stderr line omitted]' : pending)
      return { stderr: retained, version: version ?? null }
    }
  }
}

// A loaded CI runner needs 7 to 9 seconds before Chrome writes its first log line.
const START_TIMEOUT_MS = 30_000

async function openSession(executable, { signal, startTimeoutMs = START_TIMEOUT_MS } = {}) {
  const profile = await mkdtemp(path.join(os.tmpdir(), 'dcl-visual-'))
  const redactStartup = startupRedactor()
  const output = startupOutput(redactStartup)
  const browser = spawn(executable, browserArgs(profile), { stdio: ['ignore', 'ignore', 'pipe'] })
  browser.stderr?.on('data', output.write)
  let phase = 'port-file'
  let portState = 'not-read'
  let lastFsError = null
  let validPortObserved = false
  let socket
  let closing
  const pending = new Map()
  // Waiters for the top frame to stop loading; a closed session rejects them.
  const loadWaiters = new Set()
  let startupError
  browser.on('error', (error) => {
    startupError = error
  })
  const close = () => {
    closing ??= (async () => {
      for (const reject of pending.values()) reject(new Error('Capture browser session closed'))
      pending.clear()
      for (const waiter of loadWaiters) waiter.reject(new Error('Capture browser session closed'))
      loadWaiters.clear()
      try {
        await stopBrowser({ socket, browser })
      } finally {
        await removeProfile(profile)
      }
    })()
    return closing
  }
  try {
    let port
    const deadline = performance.now() + startTimeoutMs
    while (performance.now() < deadline) {
      signal?.throwIfAborted()
      if (startupError) throw startupError
      if (browser.exitCode !== null || browser.signalCode !== null)
        throw new Error('Capture browser exited during startup')
      try {
        const contents = await readFile(path.join(profile, 'DevToolsActivePort'), 'utf8')
        port = contents.split('\n')[0]
        lastFsError = null
        const complete = contents.includes('\n')
        validPortObserved =
          complete && /^\d+$/.test(port) && Number(port) > 0 && Number(port) <= 65535
        portState = !contents
          ? 'empty'
          : !complete
            ? 'partial'
            : validPortObserved
              ? 'valid'
              : 'invalid'
        break
      } catch (error) {
        lastFsError = error?.code ?? 'UNKNOWN'
        portState = error?.code === 'ENOENT' ? 'missing' : 'unreadable'
        await delay(100)
      }
    }
    if (!port) throw new Error('Capture browser did not start')
    phase = 'endpoint'
    const tab = await fetch(`http://127.0.0.1:${port}/json/new?about:blank`, {
      method: 'PUT',
      signal: signal
        ? AbortSignal.any([signal, AbortSignal.timeout(10_000)])
        : AbortSignal.timeout(10_000)
    }).then((response) => response.json())
    signal?.throwIfAborted()
    phase = 'socket'
    socket = new WebSocket(tab.webSocketDebuggerUrl)
    await new Promise((resolve, reject) => {
      const abort = () => {
        clearTimeout(timer)
        socket.close()
        reject(signal.reason)
      }
      signal?.addEventListener('abort', abort, { once: true })
      const cleanup = () => {
        clearTimeout(timer)
        signal?.removeEventListener('abort', abort)
      }
      const timer = setTimeout(() => {
        cleanup()
        socket.close()
        reject(new Error('Capture DevTools socket timed out'))
      }, 10_000)
      socket.addEventListener(
        'open',
        () => {
          cleanup()
          resolve()
        },
        { once: true }
      )
      socket.addEventListener(
        'error',
        (error) => {
          cleanup()
          reject(error)
        },
        { once: true }
      )
    })
    let serial = 0
    const abort = () => {
      for (const reject of pending.values()) reject(signal.reason)
      for (const waiter of loadWaiters) waiter.reject(signal.reason)
      loadWaiters.clear()
    }
    signal?.addEventListener('abort', abort, { once: true })
    const requestsById = new Map()
    // The top frame's navigation state, from its Page events. `navigations` counts every navigation it requested or
    // started that replaces its document. `loading` holds from the first of those events (or frameStartedLoading, which
    // a same-document navigation also fires) to frameStoppedLoading, so a navigation requested but not yet loading
    // counts as loading. `committed` holds the loader of every document the frame committed. Child frames are left out.
    let mainFrame
    let navigations = 0
    let loading = false
    const committed = new Set()
    const settle = () => {
      for (const waiter of loadWaiters) if (waiter.ready()) waiter.resolve()
    }
    const session = {
      requests: [],
      requestFailures: [],
      runtimeErrors: [],
      consoleErrors: [],
      close: async () => {
        signal?.removeEventListener('abort', abort)
        await close()
      }
    }
    socket.addEventListener('message', (event) => {
      const message = JSON.parse(event.data)
      if (message.id) {
        pending.get(message.id)?.(undefined, message)
        pending.delete(message.id)
      } else if (message.method === 'Network.requestWillBeSent') {
        const request = {
          url: message.params.request.url,
          type: message.params.type,
          initiator: message.params.initiator.type
        }
        session.requests.push(request)
        requestsById.set(`${message.sessionId ?? ''}:${message.params.requestId}`, request)
      } else if (message.method === 'Network.loadingFailed') {
        const request = requestsById.get(`${message.sessionId ?? ''}:${message.params.requestId}`)
        session.requestFailures.push({ ...request, error: message.params.errorText })
      } else if (message.method === 'Target.attachedToTarget') {
        // Workers and out-of-process frames have their own Network domain. Pause them until it is enabled.
        void observeTarget(message.params.sessionId).catch((error) => {
          session.requestFailures.push({ error: error.message })
        })
      } else if (message.method === 'Page.frameNavigated') {
        if (message.sessionId || message.params.frame.id !== mainFrame) return
        committed.add(message.params.frame.loaderId)
        settle()
      } else if (
        message.method === 'Page.frameRequestedNavigation' ||
        message.method === 'Page.frameStartedNavigating' ||
        message.method === 'Page.frameStartedLoading' ||
        message.method === 'Page.frameStoppedLoading'
      ) {
        if (message.sessionId || message.params.frameId !== mainFrame) return
        // A request to open the link elsewhere (a new tab or window) leaves this document in place.
        if ((message.params.disposition ?? 'currentTab') !== 'currentTab') return
        if (message.method === 'Page.frameStartedLoading') loading = true
        else if (message.method === 'Page.frameStoppedLoading') {
          loading = false
          settle()
        }
        // A same-document navigation (history API, fragment) keeps the document and its execution context.
        else if (!/sameDocument/i.test(message.params.navigationType ?? '')) {
          navigations++
          loading = true
        }
      } else if (message.method === 'Runtime.exceptionThrown')
        session.runtimeErrors.push(
          message.params.exceptionDetails.exception?.description ??
            message.params.exceptionDetails.text
        )
      else if (message.method === 'Runtime.consoleAPICalled' && message.params.type === 'error')
        session.consoleErrors.push(
          message.params.args.map((arg) => String(arg.value ?? arg.description ?? '')).join(' ')
        )
    })
    const send = (method, params = {}, sessionId) =>
      new Promise((resolve, reject) => {
        if (signal?.aborted) return reject(signal.reason)
        // A send after close (or on a socket that is no longer open) would only wait out its timeout.
        if (closing || socket.readyState !== WebSocket.OPEN)
          return reject(new Error('Capture browser session closed'))
        const id = ++serial
        const timeout = setTimeout(() => {
          pending.delete(id)
          reject(new CaptureTimeout('CDP timeout: ' + method))
        }, 30000)
        pending.set(id, (error, message) => {
          clearTimeout(timeout)
          if (error) {
            pending.delete(id)
            reject(error)
            return
          }
          if (message.error)
            reject(Object.assign(new Error(JSON.stringify(message.error)), { cdp: message.error }))
          else resolve(message.result)
        })
        socket.send(JSON.stringify({ id, method, params, sessionId }))
      })
    session.send = (method, params = {}) => send(method, params)
    const attachment = { autoAttach: true, waitForDebuggerOnStart: true, flatten: true }
    const observeTarget = async (sessionId) => {
      try {
        await send('Network.enable', {}, sessionId)
        await send('Target.setAutoAttach', attachment, sessionId)
      } finally {
        await send('Runtime.runIfWaitingForDebugger', {}, sessionId)
      }
    }
    Object.defineProperties(session, {
      navigations: { get: () => navigations },
      loading: { get: () => loading }
    })
    // Resolves once the top frame is not loading and, given a loader id, has committed that loader's document.
    const whenLoaded = (timeout = 30_000, loaderId) =>
      new Promise((resolve, reject) => {
        if (signal?.aborted) return reject(signal.reason)
        if (closing) return reject(new Error('Capture browser session closed'))
        const ready = () => !loading && (!loaderId || committed.has(loaderId))
        if (ready()) return resolve()
        const timer = setTimeout(() => {
          loadWaiters.delete(waiter)
          reject(new CaptureTimeout('Timed out waiting for the page to finish loading'))
        }, timeout)
        const waiter = {
          ready,
          resolve: () => {
            loadWaiters.delete(waiter)
            clearTimeout(timer)
            resolve()
          },
          reject: (error) => {
            loadWaiters.delete(waiter)
            clearTimeout(timer)
            reject(error)
          }
        }
        loadWaiters.add(waiter)
      })
    session.whenLoaded = (timeout) => whenLoaded(timeout)
    // Page.navigate answers before the new document commits; this resolves once that document committed and the top
    // frame finished loading, after any redirect the document makes while it loads. A navigation that failed or
    // stayed within the document commits no new loader of its own, so only the loading state is awaited then.
    session.navigate = async (url, timeout = 30_000) => {
      const result = await session.send('Page.navigate', { url })
      await whenLoaded(timeout, result?.errorText ? undefined : result?.loaderId)
      return result
    }
    session.evaluate = async (expression) => {
      const before = navigations
      let result
      try {
        result = await session.send('Runtime.evaluate', {
          expression,
          awaitPromise: true,
          returnByValue: true
        })
      } catch (error) {
        if (navigations !== before && lostContext(error))
          throw new NavigationInterrupted(`A navigation replaced the page during: ${expression}`, {
            cause: error
          })
        throw error
      }
      if (result.exceptionDetails)
        throw new Error(
          result.exceptionDetails.exception?.description ?? result.exceptionDetails.text
        )
      return result.result?.value
    }
    phase = 'initialize'
    await session.send('Page.enable')
    mainFrame = (await session.send('Page.getFrameTree'))?.frameTree?.frame?.id
    await session.send('Runtime.enable')
    await session.send('Network.enable')
    await session.send('Target.setAutoAttach', attachment)
    await session.send('Emulation.setDefaultBackgroundColorOverride', {
      color: { r: 0, g: 0, b: 0, a: 0 }
    })
    browser.stderr?.off('data', output.write)
    browser.stderr?.resume()
    browser.stderr?.unref()
    return session
  } catch (error) {
    const diagnostic = {
      executable: redactStartup(path.basename(executable)),
      phase,
      exitCode: browser.exitCode,
      signal: browser.signalCode,
      port: { state: portState, lastFsError, validObserved: validPortObserved }
    }
    const startup = output.finish()
    browser.stderr?.off('data', output.write)
    browser.stderr?.resume()
    browser.stderr?.unref()
    await cleanupCapture(error, close)
    if (!signal?.aborted) {
      try {
        console.error(
          'Capture browser startup diagnostics:',
          JSON.stringify({ ...diagnostic, ...startup })
        )
      } catch {
        // Diagnostics must not replace the primary startup failure.
      }
    }
    throw error
  }
}

export { NavigationInterrupted, findBrowser, openSession }
