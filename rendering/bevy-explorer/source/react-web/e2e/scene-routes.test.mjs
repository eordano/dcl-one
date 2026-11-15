// The scene workers' routing of third-party requests through the node (deploy/web/engine/scene_fetch.js).
import test from 'node:test'
import assert from 'node:assert/strict'
import { createSceneFetch, createSceneRoutes, routeSceneRequest } from '../../deploy/web/engine/scene_fetch.js'

const ORIGIN = 'https://node.example'
const ROUTES = new Map([['rooms.example', '/_scene/rooms.example']])
const map = () => Response.json({ routes: Object.fromEntries(ROUTES) })

test('a request to a listed host is re-addressed to the node as the scene sent it', async () => {
  const request = new Request('https://rooms.example/matchmake/a%2Fb?x=%26y', {
    method: 'POST',
    headers: { 'content-type': 'application/json', 'x-identity-timestamp': '1' },
    body: '{"hello":"room"}',
  })
  const routed = await routeSceneRequest(request, ROUTES, ORIGIN)
  assert.equal(routed.url, `${ORIGIN}/_scene/rooms.example/matchmake/a%2Fb?x=%26y`)
  assert.equal(routed.method, 'POST')
  assert.equal(routed.credentials, 'omit')
  assert.equal(routed.headers.get('content-type'), 'application/json')
  assert.equal(routed.headers.get('x-identity-timestamp'), '1')
  assert.equal(await routed.text(), '{"hello":"room"}')
})

test('another host, another method and a missing map leave the request as it is', async () => {
  assert.equal(await routeSceneRequest(new Request('https://other.example/x'), ROUTES, ORIGIN), null)
  const put = new Request('https://rooms.example/x', { method: 'PUT', body: 'a' })
  assert.equal(await routeSceneRequest(put, ROUTES, ORIGIN), null)
  assert.equal(await routeSceneRequest(new Request('https://rooms.example/x'), null, ORIGIN), null)
})

test('the map is read once, without credentials, for every caller', async () => {
  const asked = []
  const sceneRoutes = createSceneRoutes(async (url, init) => { asked.push([url, init.credentials]); return map() }, ORIGIN)
  const [first, second] = await Promise.all([sceneRoutes(), sceneRoutes()])
  assert.equal(first, second)
  assert.equal((await sceneRoutes()).get('rooms.example'), '/_scene/rooms.example')
  assert.deepEqual(asked, [[`${ORIGIN}/_scene`, 'omit']])
})

test('a node that answers without a map is not asked again', async () => {
  for (const answer of [() => new Response('no', { status: 404 }), () => new Response('<html>')]) {
    let asked = 0
    const sceneRoutes = createSceneRoutes(async () => { asked++; return answer() }, ORIGIN)
    assert.equal((await sceneRoutes()).size, 0)
    assert.equal((await sceneRoutes()).size, 0)
    assert.equal(asked, 1)
  }
})

test('an answer that is not a map has its body dropped', async () => {
  for (const status of [404, 503]) {
    const answer = new Response('no map here', { status })
    await createSceneRoutes(async () => answer, ORIGIN)()
    assert.equal(answer.bodyUsed, true)
  }
})

test('a node that fails or cannot be reached is asked again after the pause, not before', async t => {
  t.mock.timers.enable({ apis: ['Date'], now: 1_000 })
  const answers = [() => { throw new TypeError('unreachable') }, () => new Response('', { status: 503 }), map]
  let asked = 0
  const sceneRoutes = createSceneRoutes(async () => answers[asked++](), ORIGIN)
  assert.equal(await sceneRoutes(), null)
  assert.equal(await sceneRoutes(), null)
  assert.equal(asked, 1)
  t.mock.timers.tick(60_000)
  assert.equal(await sceneRoutes(), null)
  assert.equal(asked, 2)
  t.mock.timers.tick(60_000)
  assert.equal((await sceneRoutes()).size, 1)
  assert.equal(asked, 3)
})

test('a scene fetch sends a listed host through the node and any other host direct', async () => {
  globalThis.self = { location: { origin: ORIGIN } }
  const sent = []
  const sceneFetch = createSceneFetch(async request => { sent.push(request.url); return new Response('ok') }, () => ROUTES)
  await sceneFetch('https://rooms.example/a')
  await sceneFetch('https://other.example/b')
  assert.deepEqual(sent, [`${ORIGIN}/_scene/rooms.example/a`, 'https://other.example/b'])
})
