const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

function loadApi(ops = {}) {
  const context = { module: { exports: {} }, Deno: { core: { ops } } };
  vm.runInNewContext(fs.readFileSync(require.resolve('./RestrictedActions.js'), 'utf8'), context);
  return context.module.exports;
}

function protocolEnum(file, name) {
  let root = __dirname;
  while (!fs.existsSync(path.join(root, 'library/protocol/proto'))) {
    const parent = path.dirname(root);
    assert.notEqual(parent, root, 'repository protocol directory must be available');
    root = parent;
  }
  const source = fs.readFileSync(path.join(root, 'library/protocol/proto', file), 'utf8');
  const body = source.match(new RegExp(`enum ${name}\\s*\\{([^}]+)\\}`));
  assert.ok(body, `protocol enum ${name} must exist`);
  const entries = [...body[1].matchAll(/\b(\w+)\s*=\s*(-?\d+)\s*;/g)]
    .map(([, key, value]) => [key, Number(value)]);
  // The protocol TypeScript generator includes this sentinel alongside declared values.
  entries.push(['UNRECOGNIZED', -1]);
  return entries;
}

for (const [name, file] of [
  ['ExplorerUi', 'decentraland/sdk/components/common/explorer_ui.proto'],
  ['OpenExplorerUiResult', 'decentraland/kernel/apis/restricted_actions.proto'],
]) {
  test(`${name} exposes all protocol values and reverse names`, () => {
    const enumeration = loadApi()[name];
    assert.ok(enumeration, `${name} must be available at runtime`);
    const entries = protocolEnum(file, name);
    for (const [key, value] of entries) {
      assert.equal(enumeration[key], value, key);
      assert.equal(enumeration[value], key, String(value));
    }
    assert.equal(Object.keys(enumeration).length, entries.length * 2);
  });
}

test('a scene uses the exported names to request and interpret an explorer panel', async () => {
  const calls = [];
  const api = loadApi({ op_open_explorer_ui: async ui => { calls.push(ui); return 4; } });
  const result = await api.openExplorerUi({ ui: api.ExplorerUi.EU_MAP });
  assert.deepEqual(calls, [1]);
  assert.equal(result.openResult, api.OpenExplorerUiResult.REJECTED_FEATURE_DISABLED);
  assert.equal(api.OpenExplorerUiResult[result.openResult], 'REJECTED_FEATURE_DISABLED');
});
