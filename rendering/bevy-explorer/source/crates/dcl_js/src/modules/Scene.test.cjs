const test = require('node:test');
const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const vm = require('node:vm');

function sceneApi(op) {
    const context = { module: { exports: {} }, Deno: { core: { ops: { op_scene_information: op } } } };
    vm.runInNewContext(readFileSync(require.resolve('./Scene.js'), 'utf8'), context);
    return context.module.exports;
}

for (const content of [[], [{ file: 'scene.json', hash: 'bafk-scene' }]]) {
    test(`scene info returns its four contract fields with ${content.length} content entries`, async () => {
        const scene = {
            urn: 'bafk-entity',
            content,
            metadataJson: '{"scene":{"base":"0,0","parcels":["0,0"]}}',
            baseUrl: 'https://content.example.org/contents/',
        };
        const response = await sceneApi(async () => scene).getSceneInfo({});
        assert.deepEqual(JSON.parse(JSON.stringify(response)), {
            cid: scene.urn,
            contents: content,
            metadata: scene.metadataJson,
            baseUrl: scene.baseUrl,
        });
    });
}

test('scene info waits for the host and reads fresh information on each call', async () => {
    let resolve;
    let calls = 0;
    const api = sceneApi(() => {
        calls++;
        return new Promise(r => { resolve = r; });
    });
    for (const urn of ['first', 'second']) {
        let settled = false;
        const pending = api.getSceneInfo({}).then(result => { settled = true; return result; });
        await Promise.resolve();
        assert.equal(settled, false);
        resolve({ urn, content: [], metadataJson: '{}', baseUrl: 'https://example.org/' });
        assert.equal((await pending).cid, urn);
    }
    assert.equal(calls, 2);
});

test('scene info propagates a host failure', async () => {
    const failure = new Error('scene unavailable');
    const api = sceneApi(async () => { throw failure; });
    await assert.rejects(api.getSceneInfo({}), error => error === failure);
});
