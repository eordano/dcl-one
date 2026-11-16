import { expectConsoleOutput } from '@explorer-core/test/console'
import { describe, it, expect, vi, beforeEach } from 'vitest'
import type { Ctx } from '../../bridge-scene/src/bridge'
import type { PageToScene, SceneToPage } from '@explorer-core/engine/protocol'

// A frame loop shaped like @dcl/ecs: systems run from the live array, which removal splices.
const systems: Array<(dt: number) => void> = []
vi.mock('../../bridge-scene/node_modules/@dcl/sdk/ecs.js', () => ({
  engine: {
    addSystem: (fn: (dt: number) => void) => systems.push(fn),
    removeSystem: (fn: (dt: number) => void) => {
      const i = systems.indexOf(fn)
      if (i >= 0) systems.splice(i, 1)
    }
  }
}))
vi.mock('../../bridge-scene/src/domains/profile', () => ({fetchIdentities: vi.fn(async () => () => ({}))}))
let player: { userId: string } | null = null
const { openExternalUrl } = vi.hoisted(() => ({ openExternalUrl: vi.fn() }))
vi.mock('~system/RestrictedActions', () => ({ openExternalUrl }))
vi.mock('../../bridge-scene/node_modules/@dcl/sdk/players/index.js', () => ({ getPlayer: () => player }))
vi.mock('../../bridge-scene/src/bevy-api', () => ({
  BevyApi: {
    kernelFetch: vi.fn(async () => ({ ok: true, body: '{"data":{"notifications":[]}}' })),
    getServiceUrls: vi.fn(async () => ({
      notifications: 'https://gateway.example.org/notifications',
      events: 'https://example.org/mounted/events',
      socialApi: 'https://custom.example/social',
      cameraReel: 'https://example.org/mounted/camera-reel-service',
      reels: 'https://share.example/reels',
      catalyst: 'https://custom.example/peer'
    })),
    loginPrevious: vi.fn(async () => ({ success: true, error: '' })),
    logout: vi.fn(),
    getSceneLoadingUIStream: vi.fn(() => new Promise(() => {})),
    social: { getCommunityConnectivityStream: vi.fn(() => new Promise(() => {})) }
  }
}))

const { registerSession } = await import('../../bridge-scene/src/domains/session')
const { waitMs } = await import('../../bridge-scene/src/system-helpers')

function fakeCtx(): { ctx: Ctx; sent: SceneToPage[]; req: (msg: PageToScene) => Promise<void>; frame: () => void } {
  const sent: SceneToPage[] = []
  const handlers = new Map<string, Array<(m: PageToScene) => unknown>>()
  const pushed: Array<(dt: number) => void> = []
  const ctx: Ctx = {
    send: (m) => sent.push(m),
    on: (kind, h) => handlers.set(kind, [...(handlers.get(kind) ?? []), h as (m: PageToScene) => unknown]),
    push: (s) => pushed.push(s)
  }
  const req = async (msg: PageToScene): Promise<void> => {
    await Promise.all((handlers.get(msg.kind) ?? []).map((h) => h(msg)))
  }
  return { ctx, sent, req, frame: () => pushed.forEach((s) => s(1 / 60)) }
}

it('opens only the fixed support URL through the scene permission boundary', async () => {
  const { ctx, req } = fakeCtx()
  registerSession(ctx)
  await req({ kind: 'navAction', action: 'support' })
  expect(openExternalUrl).toHaveBeenCalledWith({ url: 'https://decentraland.org/help/' })
})

describe('openExternalUrl', () => {
  beforeEach(() => {
    openExternalUrl.mockReset()
  })

  it('opens an https url and answers under the request id', async () => {
    const { ctx, sent, req } = fakeCtx()
    registerSession(ctx)
    await req({ kind: 'openExternalUrl', requestId: 7, url: 'https://x.com/intent/post?text=hi' })
    expect(openExternalUrl).toHaveBeenCalledWith({ url: 'https://x.com/intent/post?text=hi' })
    expect(sent).toContainEqual({ kind: 'externalUrlOpened', requestId: 7, ok: true })
  })

  it.each(['file:///etc/passwd', 'http://example.org/', 'javascript:alert(1)', 'steam://run/1', ''])('refuses %s', async (url) => {
    const { ctx, sent, req } = fakeCtx()
    registerSession(ctx)
    expectConsoleOutput('error', [['[session] refused to open', url]])
    await req({ kind: 'openExternalUrl', requestId: 8, url })
    expect(openExternalUrl).not.toHaveBeenCalled()
    expect(sent).toContainEqual({ kind: 'externalUrlOpened', requestId: 8, ok: false })
  })

  it('answers a failed open instead of leaving the page waiting', async () => {
    openExternalUrl.mockRejectedValueOnce(new Error('denied'))
    const { ctx, sent, req } = fakeCtx()
    registerSession(ctx)
    expectConsoleOutput('error', [['[session] open', 'https://example.org/', new Error('denied')]])
    await req({ kind: 'openExternalUrl', requestId: 9, url: 'https://example.org/' })
    expect(sent).toContainEqual({ kind: 'externalUrlOpened', requestId: 9, ok: false })
  })
})

const readies = (sent: SceneToPage[]): number => sent.filter((m) => m.kind === 'event' && m.name === 'playerReady').length

beforeEach(() => {
  systems.length = 0
  player = null
})

describe('bridge session', () => {
  it('logging back into the same account announces playerReady again', async () => {
    const { ctx, sent, req, frame } = fakeCtx()
    registerSession(ctx)
    player = { userId: '0xa' }
    frame()
    expect(readies(sent)).toBe(1)
    await req({ kind: 'rpc:req', id: '1', method: 'logout' })
    frame() // no frame has seen the player gone yet
    expect(readies(sent)).toBe(1)
    await req({ kind: 'rpc:req', id: '2', method: 'loginPrevious' })
    frame()
    expect(readies(sent)).toBe(2)
  })

  it('announces playerReady again once the engine has cleared the player and set the next one', () => {
    const { ctx, sent, frame } = fakeCtx()
    registerSession(ctx)
    player = { userId: '0xa' }
    frame()
    player = null // logout
    frame()
    expect(readies(sent)).toBe(1)
    player = { userId: '0xb' } // a login the bridge did not relay (the web console command)
    frame()
    expect(readies(sent)).toBe(2)
  })
})

describe('bridge service URLs', () => {
  it('uses the engine-resolved APIs for notifications, community detail and profiles', async () => {
    const { registerCommunities } = await import('../../bridge-scene/src/domains/communities')
    const { registerNotifications } = await import('../../bridge-scene/src/domains/notifications')
    const { catalystBase } = await import('../../bridge-scene/src/http')
    const { BevyApi } = await import('../../bridge-scene/src/bevy-api')
    const { ctx, req } = fakeCtx()
    registerNotifications(ctx)
    registerCommunities(ctx)

    await req({ kind: 'getNotifications' })
    await req({ kind: 'getCommunityDetail', id: 'some/id' })
    for (const url of [
      'https://gateway.example.org/notifications/notifications?limit=50&from=0',
      'https://example.org/mounted/events/api/events?community_id=some%2Fid&list=upcoming',
      'https://custom.example/social/v2/communities/some%2Fid/members?limit=100',
      'https://example.org/mounted/camera-reel-service/api/communities/some%2Fid/images?limit=30'
    ]) {
      expect(BevyApi.kernelFetch).toHaveBeenCalledWith(expect.objectContaining({
        url, init: expect.objectContaining({ method: 'GET' })
      }))
    }
    expect(await catalystBase()).toBe('https://custom.example/peer')
    expect(BevyApi.getServiceUrls).toHaveBeenCalledTimes(1)
  })
})

describe('communities list', () => {
  it('is every community the user is a member of, and nothing when that read fails', async () => {
    const { registerCommunities } = await import('../../bridge-scene/src/domains/communities')
    const { BevyApi } = await import('../../bridge-scene/src/bevy-api')
    const { ctx, req, sent } = fakeCtx()
    registerCommunities(ctx)
    const raw = (id: string, role: string) => ({ id, name: `Community ${id}`, description: '', membersCount: 1, role, privacy: 'public', thumbnailUrl: `https://img.example/${id}.png` })
    let memberOfAnswers = true
    const urls: string[] = []
    vi.mocked(BevyApi.kernelFetch).mockImplementation(async ({ url }: { url: string }) => {
      urls.push(url)
      return memberOfAnswers
        ? ({ ok: true, status: 200, body: JSON.stringify({ results: [raw('a', 'member'), raw('z', 'owner')], total: 2 }) } as never)
        : ({ ok: false, status: 500, body: 'down' } as never)
    })
    try {
      await req({ kind: 'getCommunities' })
      memberOfAnswers = false
      // the page's request fails (requestFailed), so it asks again the next time it opens
      await expect(req({ kind: 'getCommunities' })).rejects.toThrow()
    } finally {
      vi.mocked(BevyApi.kernelFetch).mockReset()
    }
    expect(urls.every((u) => u.includes('onlyMemberOf=true'))).toBe(true)
    const lists = sent.flatMap((m) => (m.kind === 'communities' ? [m.communities.map((c) => `${c.id}:${c.role}`)] : []))
    expect(lists).toEqual([['a:member', 'z:owner']])
  })
})

describe('member-of list', () => {
  it('sends nothing for an answer without a results list, and an empty list only for an empty one', async () => {
    expectConsoleOutput('error', Array.from({ length: 3 }, () => ['[communities] member-of list failed', new Error('member-of answer has no results list')]))
    const { registerCommunities } = await import('../../bridge-scene/src/domains/communities')
    const { BevyApi } = await import('../../bridge-scene/src/bevy-api')
    const { ctx, req, sent } = fakeCtx()
    registerCommunities(ctx)
    const answer = (body: string): void => {
      vi.mocked(BevyApi.kernelFetch).mockResolvedValueOnce({ ok: true, status: 200, body } as never)
    }
    for (const body of ['', '{}', '{"data":{}}']) {
      answer(body)
      await req({ kind: 'getCommunityChannels' })
    }
    expect(sent.filter((m) => m.kind === 'communityChannels')).toEqual([])
    answer('{"results":[]}')
    await req({ kind: 'getCommunityChannels' })
    expect(sent.filter((m) => m.kind === 'communityChannels')).toEqual([{ kind: 'communityChannels', communities: [] }])
  })
})

describe('waitMs', () => {
  it('does not make other systems skip a frame when it finishes', async () => {
    const other = vi.fn()
    const done = waitMs(10)
    systems.push(other)
    for (const s of systems) s(1)
    await done
    expect(other).toHaveBeenCalledTimes(1)
  })
})
