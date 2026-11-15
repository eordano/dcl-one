'use strict'

const { test } = require('node:test')
const assert = require('node:assert/strict')
const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')
const { spawnSync } = require('node:child_process')

function run(command, args, cwd, env = {}) {
  return spawnSync(command, args, { cwd, env: { ...process.env, ...env }, encoding: 'utf8' })
}
function fixture(t) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'protocol-tooling-test-'))
  t.after(() => fs.rmSync(root, { recursive: true, force: true }))
  const protocol = path.join(root, 'library/protocol')
  const scripts = path.join(protocol, 'scripts')
  const components = path.join(protocol, 'proto/decentraland/sdk/components')
  fs.mkdirSync(scripts, { recursive: true })
  fs.mkdirSync(components, { recursive: true })
  for (const name of ['check-proto-compabitility.sh', 'component-ids.cjs', 'list-components-ids.sh', 'check-component-id.sh']) {
    fs.copyFileSync(path.join(__dirname, name), path.join(scripts, name))
  }
  fs.writeFileSync(path.join(components, 'one.proto'), 'syntax = "proto3";\noption (common.ecs_component_id) = 1017;\n')
  return { root, protocol, scripts, components }
}
function repository(t) {
  const fx = fixture(t)
  for (const args of [['init', '-q'], ['add', '.'], ['-c', 'user.name=Protocol Test', '-c', 'user.email=test@example.invalid', 'commit', '-qm', 'baseline']]) {
    const result = run('git', args, fx.root)
    assert.equal(result.status, 0, result.stderr)
  }
  const buf = path.join(fx.root, 'buf')
  // The stand-in verifies that a real archived source reaches Buf, then can reject it.
  fs.writeFileSync(buf, '#!/usr/bin/env bash\nset -eu\ntest "$1" = breaking\ntest "$3" = --against\ntest -f "$4/decentraland/sdk/components/one.proto"\nprintf checked > "$BUF_MARKER"\nexit "${BUF_RESULT:-0}"\n', { mode: 0o755 })
  return { ...fx, env: { BUF_AGAINST_REF: 'HEAD', BUF_BIN: buf, BUF_MARKER: path.join(fx.root, 'buf-called') } }
}

test('compatibility checks the archived baseline and propagates a rejected schema', (t) => {
  const fx = repository(t)
  const ok = run('bash', ['scripts/check-proto-compabitility.sh'], fx.protocol, fx.env)
  assert.equal(ok.status, 0, ok.stderr)
  assert.equal(fs.readFileSync(fx.env.BUF_MARKER, 'utf8'), 'checked')
  const refused = run('bash', ['scripts/check-proto-compabitility.sh'], fx.protocol, { ...fx.env, BUF_RESULT: '42' })
  assert.equal(refused.status, 42, refused.stderr)
})

test('npm test installs the Buf executable through make on a fresh dependency tree', (t) => {
  const fx = repository(t)
  fs.copyFileSync(path.join(__dirname, '../package.json'), path.join(fx.protocol, 'package.json'))
  fs.copyFileSync(path.join(__dirname, '../Makefile'), path.join(fx.protocol, 'Makefile'))
  fs.mkdirSync(path.join(fx.protocol, 'node_modules/.bin'), { recursive: true })
  const bin = path.join(fx.root, 'download-bin')
  fs.mkdirSync(bin)
  fs.writeFileSync(path.join(bin, 'curl'), '#!/usr/bin/env bash\nset -eu\nwhile [[ $# -gt 0 ]]; do if [[ "$1" = -o ]]; then cp "$BUF_DOWNLOAD_FIXTURE" "$2"; exit; fi; shift; done\nexit 99\n', { mode: 0o755 })
  const env = { ...fx.env, BUF_BIN: '', BUF_DOWNLOAD_FIXTURE: fx.env.BUF_BIN, PATH: `${bin}${path.delimiter}${process.env.PATH}` }
  const result = run('npm', ['test'], fx.protocol, env)
  assert.equal(result.status, 0, result.stdout + result.stderr)
  assert.equal(fs.readFileSync(fx.env.BUF_MARKER, 'utf8'), 'checked')
})

test('missing revisions and revisions without protocol fail before Buf', (t) => {
  const fx = repository(t)
  const absent = run('bash', ['scripts/check-proto-compabitility.sh'], fx.protocol, { ...fx.env, BUF_AGAINST_REF: 'refs/heads/absent' })
  assert.notEqual(absent.status, 0)
  assert.equal(fs.existsSync(fx.env.BUF_MARKER), false)
  run('git', ['rm', '-qr', 'library/protocol/proto'], fx.root)
  const commit = run('git', ['-c', 'user.name=Protocol Test', '-c', 'user.email=test@example.invalid', 'commit', '-qm', 'without protocol'], fx.root)
  assert.equal(commit.status, 0, commit.stderr)
  const noTree = run('bash', ['scripts/check-proto-compabitility.sh'], fx.protocol, fx.env)
  assert.notEqual(noTree.status, 0)
  assert.equal(fs.existsSync(fx.env.BUF_MARKER), false)
})

for (const failure of ['archive', 'extract']) {
  test(`a failed ${failure} cannot become a passing compatibility check`, (t) => {
    const fx = repository(t)
    const bin = path.join(fx.root, 'bin')
    fs.mkdirSync(bin)
    const realGit = run('which', ['git'], fx.root).stdout.trim()
    const name = failure === 'archive' ? 'git' : 'tar'
    const content = failure === 'archive'
      ? '#!/usr/bin/env bash\nfor arg in "$@"; do if [[ "$arg" == archive ]]; then exit 73; fi; done\nexec "$REAL_GIT" "$@"\n'
      : '#!/usr/bin/env bash\nexit 74\n'
    fs.writeFileSync(path.join(bin, name), content, { mode: 0o755 })
    const result = run('bash', ['scripts/check-proto-compabitility.sh'], fx.protocol, { ...fx.env, REAL_GIT: realGit, PATH: `${bin}${path.delimiter}${process.env.PATH}` })
    assert.equal(result.status, failure === 'archive' ? 73 : 74, result.stderr)
    assert.equal(fs.existsSync(fx.env.BUF_MARKER), false)
  })
}

test('component commands distinguish free and used IDs with deterministic numeric order', (t) => {
  const fx = fixture(t)
  fs.writeFileSync(path.join(fx.components, 'two.proto'), 'option (decentraland.sdk.components.common.ecs_component_id) = 99;\n// option (common.ecs_component_id) = 1017;\n/* option (common.ecs_component_id) = 99; */\n')
  const listed = run('bash', [path.join(fx.scripts, 'list-components-ids.sh')], fx.root)
  assert.equal(listed.status, 0, listed.stderr)
  assert.equal(listed.stdout, 'List of ids (2 total):\n1. 99\n2. 1017\n')
  for (const [id, status, text] of [['99', 1, 'taken'], ['100', 0, 'free']]) {
    const checked = run('bash', [path.join(fx.scripts, 'check-component-id.sh'), id], fx.root)
    assert.equal(checked.status, status, checked.stderr)
    assert.equal(checked.stdout, `id ${id} is ${text}\n`)
  }
})

test('component commands reject malformed candidate IDs and duplicate declarations', (t) => {
  const fx = fixture(t)
  for (const args of [[], ['0'], ['-1'], ['1.5'], ['x'], ['2147483648'], ['99', '100']]) {
    const result = run('bash', [path.join(fx.scripts, 'check-component-id.sh'), ...args], fx.root)
    assert.equal(result.status, 2, `${args}: ${result.stderr}`)
  }
  fs.writeFileSync(path.join(fx.components, 'duplicate.proto'), 'option (common.ecs_component_id) = 0x3f9;\n')
  for (const args of [['list-components-ids.sh'], ['check-component-id.sh', '100']]) {
    const result = run('bash', [path.join(fx.scripts, args[0]), ...args.slice(1)], fx.root)
    assert.equal(result.status, 2, result.stderr)
    assert.match(result.stderr, /duplicate component ID 1017/)
    assert.match(result.stderr, /duplicate\.proto/)
    assert.match(result.stderr, /one\.proto/)
  }
})

test('make passes component IDs as literal data without command execution', (t) => {
  const fx = fixture(t)
  fs.copyFileSync(path.join(__dirname, '../Makefile'), path.join(fx.protocol, 'Makefile'))
  const before = fs.readdirSync(fx.protocol).sort()
  const payloads = [
    '`touch injected-backtick`2147483647',
    '$(touch injected-dollar)2147483647',
    '$(shell touch injected-make)2147483647',
    '"; touch injected-quote; #',
    '$$(touch injected-escaped-dollar)2147483647'
  ]
  for (const id of payloads) {
    const result = run('make', ['--no-print-directory', 'check-component-id', `ID=${id}`], fx.protocol)
    assert.equal(result.status, 2, `${id}: ${result.stdout}${result.stderr}`)
    assert.match(result.stderr, /positive int32 ID/)
    assert.deepEqual(fs.readdirSync(fx.protocol).sort(), before, id)
  }
  for (const [id, status, text] of [['1017', 2, 'taken'], ['2147483647', 0, 'free']]) {
    const result = run('make', ['--no-print-directory', 'check-component-id', `ID=${id}`], fx.protocol)
    assert.equal(result.status, status, result.stderr)
    assert.equal(result.stdout, `id ${id} is ${text}\n`)
  }
})


test('invalid declarations cannot silently disappear from the ID catalogue', (t) => {
  const fx = fixture(t)
  for (const literal of ['-1', '2147483648', 'invalid', '0']) {
    fs.writeFileSync(path.join(fx.components, 'invalid.proto'), `option (common.ecs_component_id) = ${literal};\n`)
    const result = run('bash', [path.join(fx.scripts, 'list-components-ids.sh')], fx.root)
    assert.equal(result.status, 2, result.stderr)
    assert.match(result.stderr, /invalid component ID/)
  }
})
