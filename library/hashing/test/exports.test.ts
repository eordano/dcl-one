import assert from 'node:assert/strict'
import { execFileSync } from 'node:child_process'
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, symlinkSync, rmSync } from 'node:fs'
import { createRequire } from 'node:module'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { it } from 'node:test'
import { compareStrings, sortKeys } from '../src/index.ts'

it('compares strings and content references by hash then file without mutation', () => {
  const names = ['', '\u0000', '01', '1', '10', '2', 'A', 'a', 'é', '😀', '\uffff']
  for (const a of names) for (const b of names) {
    assert.equal(compareStrings(a, b), a < b ? -1 : a > b ? 1 : 0)
    assert.equal(sortKeys({ hash: a, file: 'z' }, { hash: b, file: 'a' }), compareStrings(a, b) || 1)
    assert.equal(sortKeys({ hash: 'same', file: a }, { hash: 'same', file: b }), compareStrings(a, b))
  }
  const frozen = Object.freeze({ hash: 'a', file: 'b' })
  assert.equal(sortKeys(frozen, frozen), 0)
})

it('resolves the built root and subpaths through require, import and TypeScript', () => {
  const root = fileURLToPath(new URL('..', import.meta.url))
  const temporary = mkdtempSync(join(tmpdir(), 'hashing-exports-'))
  try {
    const packageRoot = join(temporary, 'node_modules/@dcl/hashing')
    mkdirSync(packageRoot, { recursive: true })
    const manifest = readFileSync(join(root, 'package.json'), 'utf8')
    writeFileSync(join(packageRoot, 'package.json'), manifest)
    const tsc = join(root, 'node_modules/typescript/bin/tsc')
    execFileSync(process.execPath, [tsc, '-p', join(root, 'tsconfig.json'), '--incremental', 'false', '--outDir', join(packageRoot, 'dist')])
    mkdirSync(join(temporary, 'node_modules/@noble'), { recursive: true })
    symlinkSync(join(root, 'node_modules/@noble/hashes'), join(temporary, 'node_modules/@noble/hashes'))
    const require = createRequire(join(temporary, 'consumer.cjs'))
    const expected = {
      '@dcl/hashing': ['hashV0', 'hashV1', 'prepareADR32Data', 'calculateMultipleHashesADR32', 'calculateMultipleHashesADR32LegacyQmHash', 'keccak256Hash', 'sortKeys', 'compareStrings'],
      '@dcl/hashing/node': ['hashV0', 'hashV1'],
      '@dcl/hashing/ADR32': ['calculateMultipleHashesADR32', 'calculateMultipleHashesADR32LegacyQmHash', 'prepareADR32Data', 'sortKeys', 'compareStrings'],
      '@dcl/hashing/ADR62': ['keccak256Hash']
    }
    for (const [entry, names] of Object.entries(expected)) {
      assert.deepEqual(Object.keys(require(entry)).sort(), names.slice().sort(), entry)
      for (const name of names) assert.equal(require(entry)[name], require('@dcl/hashing')[name])
    }
    const imports = Object.entries(expected).map(([entry, names], index) =>
      `import * as m${index} from '${entry}';\n` + names.map(name =>
        `if (typeof m${index}.${name} !== 'function') throw new Error('${entry}: ${name}');`).join('\n')).join('\n')
    writeFileSync(join(temporary, 'consumer.mjs'), imports)
    execFileSync(process.execPath, [join(temporary, 'consumer.mjs')])
    writeFileSync(join(temporary, 'consumer.mts'), imports + `
import type { HashableContent } from '@dcl/hashing/node';
import type { EntityContentItemReference, CalculateMultipleHashesResult } from '@dcl/hashing/ADR32';
const content: HashableContent = new Uint8Array();
const entry: EntityContentItemReference = { hash: 'h', file: 'f' };
const result: Promise<CalculateMultipleHashesResult> = m2.calculateMultipleHashesADR32([entry]);
void m1.hashV1(content); void result;
`)
    execFileSync(process.execPath, [tsc, '--noEmit', '--strict', '--module', 'nodenext', '--target', 'ES2022', '--skipLibCheck', join(temporary, 'consumer.mts')])
  } finally { rmSync(temporary, { recursive: true, force: true }) }
})
