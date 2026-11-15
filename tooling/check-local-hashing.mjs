import assert from 'node:assert/strict'
import { realpathSync, readFileSync } from 'node:fs'
import { createRequire } from 'node:module'
import { resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const local = realpathSync(fileURLToPath(new URL('../library/hashing/dist/index.js', import.meta.url)))
assert.ok(process.argv.length > 2, 'Provide consumer package paths')
for (const path of process.argv.slice(2)) {
  const manifest = resolve(path, 'package.json')
  const consumer = createRequire(manifest)
  const sdk = JSON.parse(readFileSync(manifest, 'utf8')).name === '@dcl/sdk-commands'
    ? consumer : createRequire(consumer.resolve('@dcl/sdk-commands/package.json'))
  const catalyst = createRequire(sdk.resolve('dcl-catalyst-client'))
  for (const [name, caller] of [['SDK commands', sdk], ['catalyst client', catalyst]]) {
    assert.equal(realpathSync(caller.resolve('@dcl/hashing')), local, `${path}: ${name} must use library/hashing`)
    assert.equal(typeof caller('@dcl/hashing').hashV1, 'function', `${path}: ${name} must load library/hashing`)
  }
  console.log(`${path}: SDK commands and catalyst client use library/hashing`)
}
