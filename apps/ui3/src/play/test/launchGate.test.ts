// DOMAIN: the launch gate's per-service rule behind the dcl-one gateway (lib/launchGate.ts +
// lib/baseDomain.ts derivedServiceUrl). The engine writes its effective options back into the
// address bar, so a reload carries every service url explicitly; the deployment's own urls are
// not overrides and must not stop the boot.

import { afterEach, describe, expect, it, vi } from 'vitest'

const load = async (onSites: boolean, search: string) => {
  vi.resetModules()
  if (onSites) (window as { __DCL_PUBLIC__?: unknown }).__DCL_PUBLIC__ = {}
  else delete (window as { __DCL_PUBLIC__?: unknown }).__DCL_PUBLIC__
  window.history.replaceState(null, '', search)
  const base = await import('../lib/baseDomain')
  const gate = await import('../lib/launchGate')
  return { ...base, ...gate }
}
afterEach(() => {
  delete (window as { __DCL_PUBLIC__?: unknown }).__DCL_PUBLIC__
  window.history.replaceState(null, '', '/')
})

describe('the launch gate behind the gateway', () => {
  it("passes the deployment's own service urls, as the engine writes them back into the address bar", async () => {
    const probe = await load(true, '/')
    const { SERVICES } = await import('@explorer-core/engine/generated')
    expect(window.location.hostname).not.toMatch(/decentraland\.(org|zone)$/)
    const own = Object.fromEntries(SERVICES.map((s) => [s.name, probe.derivedServiceUrl(s.name)]))
    const m = await load(true, '/?' + new URLSearchParams(own).toString())
    expect(m.untrustedLaunchParams({ native: false })).toEqual([])
    for (const s of SERVICES) expect(m.serviceUrl(s.name), s.name).toBe(own[s.name])
  })

  it('still gates a service pointed at a foreign host', async () => {
    const probe = await load(true, '/')
    const places = encodeURIComponent(probe.derivedServiceUrl('places'))
    const m = await load(true, `/?places=${places}&catalyst=${encodeURIComponent('https://evil.example')}`)
    const [p, ...rest] = m.untrustedLaunchParams({ native: false })
    expect(rest).toEqual([])
    expect(p.name).toBe('catalyst')
    expect(p.value).toBe('https://evil.example')
  })

  it("upstream: derivation and trust agree on decentraland.org's own hosts", async () => {
    const m = await load(false, '/?places=https://places.decentraland.org')
    expect(m.derivedServiceUrl('places')).toBe('https://places.decentraland.org')
    expect(m.untrustedLaunchParams({ native: false })).toEqual([])
  })
})
