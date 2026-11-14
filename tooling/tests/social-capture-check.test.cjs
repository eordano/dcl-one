'use strict'

const assert = require('node:assert/strict')
const { createHash } = require('node:crypto')
const fs = require('node:fs')
const os = require('node:os')
const path = require('node:path')
const { test } = require('node:test')
const { validateCatalog, checkSyntax, containedFile, requireNode26 } = require('../social-capture-check.cjs')

const ROOT = path.resolve(__dirname, '../..')
const catalog = JSON.parse(fs.readFileSync(path.join(ROOT, 'tooling/social-captures.json'), 'utf8'))
const SOCIAL = 'services/catalyst/tests/fixtures/conformance/social'

// Each case gets real frozen fixture formats and isolated files; no upstream imports or services.
function sandbox(t) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'social-catalog-'))
  t.after(() => fs.rmSync(root, { recursive: true, force: true }))
  const copy = structuredClone(catalog)
  fs.mkdirSync(path.join(root, 'tooling/tests'), { recursive: true })
  for (const entry of copy.captures) {
    for (const file of [entry.script, entry.fixture]) {
      fs.mkdirSync(path.dirname(path.join(root, file)), { recursive: true })
      fs.copyFileSync(path.join(ROOT, file), path.join(root, file))
    }
  }
  return { root, copy }
}

function editFixture(root, entry, edit) {
  const file = path.join(root, entry.fixture)
  const fixture = JSON.parse(fs.readFileSync(file, 'utf8'))
  edit(fixture)
  const bytes = JSON.stringify(fixture)
  fs.writeFileSync(file, bytes)
  // A matching hash must not hide broken provenance or runtime metadata.
  entry.sha256 = createHash('sha256').update(bytes).digest('hex')
}

test('committed catalog consumes exactly 48 scripts and frozen JSON fixtures', () => {
  assert.deepEqual(validateCatalog(), { captures: 48, fixtures: 48 })
  const args = id => catalog.captures.find(capture => capture.id === id).args
  assert.deepEqual(args('community-voice'), ['social', 'gatekeeper', 'fixture'])
  assert.deepEqual(args('community-voice-errors'), ['social', 'gatekeeper', 'fixture'])
  assert.deepEqual(args('private-lifecycle-cases'), ['social', 'fixture', 'gatekeeper'])
  assert.deepEqual(args('community-ranking'), ['social', 'fixture-directory'])
  assert.deepEqual(args('community-compliance'), ['social', 'fixture-directory'])
  assert.deepEqual(args('community-errors'), ['social', 'fixture', 'postgres-url'])
  assert.deepEqual(args('limiter-cases'), ['social', 'fixture', 'redis-server?'])
})

test('Node major guard rejects other runtimes', () => {
  requireNode26('26.12.0')
  for (const version of ['24.0.0', '25.9.0', '27.0.0']) {
    assert.throws(() => requireNode26(version), /require Node 26/)
  }
})

for (const key of ['id', 'script', 'fixture']) {
  test(`rejects duplicate ${key}`, t => {
    const { root, copy } = sandbox(t)
    copy.captures[1][key] = copy.captures[0][key]
    assert.throws(() => validateCatalog(root, copy), new RegExp(`Duplicate capture ${key}`))
  })
}

test('rejects a missing catalog entry', t => {
  const { root, copy } = sandbox(t)
  copy.captures.pop()
  assert.throws(() => validateCatalog(root, copy), /Capture script coverage mismatch; uncatalogued:/)
})

for (const key of ['script', 'fixture']) {
  test(`rejects a missing ${key} file`, t => {
    const { root, copy } = sandbox(t)
    fs.unlinkSync(path.join(root, copy.captures[0][key]))
    assert.throws(() => validateCatalog(root, copy), /coverage mismatch|Missing artifact/)
  })
}

for (const file of ['tooling/social-orphan.cjs', `${SOCIAL}/source-contracts/capture-orphan.cjs`,
  `${SOCIAL}/orphan-source-cases.json`, `${SOCIAL}/source-contracts/orphan.json`]) {
  test(`rejects uncatalogued artifact ${file}`, t => {
    const { root, copy } = sandbox(t)
    fs.writeFileSync(path.join(root, file), '{}')
    assert.throws(() => validateCatalog(root, copy), /coverage mismatch; uncatalogued:/)
  })
}

test('rejects an omitted fixture mapping even when all scripts remain', t => {
  const { root, copy } = sandbox(t)
  copy.captures[0].fixture = `${SOCIAL}/not-a-capture.json`
  fs.writeFileSync(path.join(root, copy.captures[0].fixture), '{}')
  assert.throws(() => validateCatalog(root, copy), /Fixture coverage mismatch/)
})

test('rejects invalid JSON and changed frozen fixture bytes', t => {
  const { root, copy } = sandbox(t)
  const file = path.join(root, copy.captures[0].fixture)
  const bytes = fs.readFileSync(file)
  fs.writeFileSync(file, '{')
  assert.throws(() => validateCatalog(root, copy), /Invalid fixture JSON/)
  fs.writeFileSync(file, Buffer.concat([bytes, Buffer.from('\n')]))
  assert.throws(() => validateCatalog(root, copy), /Frozen fixture SHA-256 changed/)
})

test('rejects changed catalog version, Node requirement, source repository and pin', t => {
  const { root, copy } = sandbox(t)
  for (const edit of [c => { c.version = 2 }, c => { c.nodeMajor = 24 },
    c => { c.sources.social.repository = 'other/social' },
    c => { c.sources.social.revision = 'a'.repeat(40) },
    c => { c.sources.gatekeeper.revision = 'be6a4b9' }]) {
    const bad = structuredClone(copy)
    edit(bad)
    assert.throws(() => validateCatalog(root, bad), /catalog version|require Node 26|revision pins/)
  }
})

for (const id of ['community-analytics', 'community-errors', 'community-voice-errors',
  'community-write-failures', 'community-raw-lifecycle-cases', 'private-lifecycle-cases']) {
  test(`rejects wrong social provenance in frozen format: ${id}`, t => {
    const { root, copy } = sandbox(t)
    const entry = copy.captures.find(capture => capture.id === id)
    editFixture(root, entry, fixture => {
      if (fixture.baseline) fixture.baseline.revision = 'a'.repeat(40)
      else if (fixture.provenance) fixture.provenance.revision = 'a'.repeat(40)
      else if (typeof fixture.source === 'object') fixture.source.revision = 'a'.repeat(40)
      else if (fixture.social) fixture.social = 'a'.repeat(40)
      else fixture.revision = 'a'.repeat(40)
    })
    assert.throws(() => validateCatalog(root, copy), /Wrong fixture social revision/)
  })
}

for (const id of ['community-voice', 'community-voice-errors', 'private-lifecycle-cases']) {
  test(`requires correct companion provenance: ${id}`, t => {
    const { root, copy } = sandbox(t)
    const entry = copy.captures.find(capture => capture.id === id)
    editFixture(root, entry, fixture => {
      if (typeof fixture.companion === 'string') fixture.companion = 'a'.repeat(40)
      else fixture.companion.revision = 'a'.repeat(40)
    })
    assert.throws(() => validateCatalog(root, copy), /Wrong fixture gatekeeper revision/)
    editFixture(root, entry, fixture => { delete fixture.companion })
    assert.throws(() => validateCatalog(root, copy), /Fixture provenance must cite every required source/)
  })
}

test('rejects a missing source repository even with a correct revision and digest', t => {
  const { root, copy } = sandbox(t)
  editFixture(root, copy.captures[0], fixture => { delete fixture.source })
  assert.throws(() => validateCatalog(root, copy), /fixture repository|fixture source repository/)
})

test('rejects a changed fixture Node runtime with a matching digest', t => {
  const { root, copy } = sandbox(t)
  editFixture(root, copy.captures[0], fixture => { fixture.node = 'v24.0.0' })
  assert.throws(() => validateCatalog(root, copy), /Fixture runtime must use Node 26/)
})

test('rejects unknown, duplicate and inconsistent invocation requirements', t => {
  const { root, copy } = sandbox(t)
  const changes = [entry => { entry.args[1] = 'output' }, entry => { entry.args.push('social') },
    entry => { entry.args.reverse() }, entry => { entry.args.push('fixture-directory') },
    entry => { entry.sources.push('gatekeeper') }, entry => { entry.environment = ['DATABASE_URL'] },
    entry => { entry.commands = ['redis-server'] }]
  for (const change of changes) {
    const bad = structuredClone(copy)
    change(bad.captures[0])
    assert.throws(() => validateCatalog(root, bad), /argument|fixture output|source requirements|environment requirement|command requirements/)
  }
})

test('rejects absolute, parent, Windows and external symlink paths', t => {
  const { root } = sandbox(t)
  for (const relative of ['/tmp/outside.json', '../outside.json', 'tooling/../outside.json',
    'C:/outside.json', 'tooling\\outside.json', './tooling/outside.json']) {
    assert.throws(() => containedFile(root, relative), /path must be relative and contained/)
  }
  const outside = fs.mkdtempSync(path.join(os.tmpdir(), 'social-outside-'))
  t.after(() => fs.rmSync(outside, { recursive: true, force: true }))
  fs.writeFileSync(path.join(outside, 'outside.json'), '{}')
  fs.symlinkSync(path.join(outside, 'outside.json'), path.join(root, 'tooling/outside.json'))
  assert.throws(() => containedFile(root, 'tooling/outside.json'), /resolves outside the repository/)
})

test('syntax check covers captures, helpers and tests without executing them', t => {
  const { root, copy } = sandbox(t)
  const sentinel = "throw new Error('must never execute'); require('no-upstream-dependency');\n"
  for (const entry of copy.captures) fs.writeFileSync(path.join(root, entry.script), sentinel)
  fs.writeFileSync(path.join(root, 'tooling/social-capture-common.cjs'), sentinel)
  fs.writeFileSync(path.join(root, 'tooling/social-voice-fixtures.cjs'), sentinel)
  fs.writeFileSync(path.join(root, 'tooling/tests/social-sentinel.test.cjs'), sentinel)
  validateCatalog(root, copy)
  assert.equal(checkSyntax(root), 51)
  for (const relative of [copy.captures[0].script, 'tooling/social-capture-common.cjs',
    'tooling/social-voice-fixtures.cjs', 'tooling/tests/social-sentinel.test.cjs']) {
    fs.writeFileSync(path.join(root, relative), 'const = ;')
    assert.throws(() => checkSyntax(root), /Syntax check failed:/)
    fs.writeFileSync(path.join(root, relative), sentinel)
  }
})

test('the workflow starts for every file the job reads', () => {
  const workflow = fs.readFileSync(path.join(ROOT, '.github/workflows/social-captures.yml'), 'utf8')
  const lists = workflow.split(/^ {4}paths:\n/m).slice(1)
    .map(block => block.match(/^(?: {6}- '[^']+'\n)+/)[0].trim().split('\n').map(line => line.trim().slice(3, -1)))
  assert.equal(lists.length, 2, 'push and pull_request each need a paths list')
  assert.deepEqual(lists[0], lists[1])
  // GitHub path filters: '**' crosses folders, '*' stays inside one.
  const patterns = lists[0].map(glob => new RegExp('^' + glob.split('**').map(part =>
    part.split('*').map(text => text.replace(/[.+?^${}()|[\]\\]/g, '\\$&')).join('[^/]*')).join('.*') + '$'))
  const tooling = directory => fs.readdirSync(path.join(ROOT, directory)).filter(name => name.startsWith('social-'))
    .map(name => `${directory}/${name}`)
  const read = new Set(['.github/CODEOWNERS', '.github/workflows/social-captures.yml', 'ownership.json', 'tooling/owners.py',
    'library/dcl-http/tests/fixtures/social-analytics-source-cases.json', ...tooling('tooling'), ...tooling('tooling/tests')])
  for (const entry of catalog.captures) for (const file of [entry.script, entry.fixture]) read.add(file)
  for (const file of [...read]) {
    if (!file.endsWith('.cjs')) continue
    const source = fs.readFileSync(path.join(ROOT, file), 'utf8')
    for (const [, relative] of source.matchAll(/require\(\s*['"](\.{1,2}\/[^'"]+)['"]\s*\)/g)) {
      read.add(path.posix.normalize(path.posix.join(path.posix.dirname(file), relative)))
    }
  }
  assert.ok(read.size > 100, 'the inventory lost its files')
  const outside = [...read].filter(file => !patterns.some(pattern => pattern.test(file))).sort()
  assert.deepEqual(outside, [], 'add these files to both paths lists in .github/workflows/social-captures.yml')
})
