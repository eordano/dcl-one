'use strict'

const fs = require('node:fs')
const path = require('node:path')

function declarations(root) {
  const rows = []
  for (const entry of fs.readdirSync(root, { withFileTypes: true })) {
    const file = path.join(root, entry.name)
    if (entry.isDirectory()) rows.push(...declarations(file))
    else if (entry.isFile() && file.endsWith('.proto')) {
      // Ignore comments without treating a comment marker inside a quoted string as syntax.
      const source = fs.readFileSync(file, 'utf8').replace(
        /"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|\/\*[\s\S]*?\*\/|\/\/[^\r\n]*/g,
        ' '
      )
      for (const match of source.matchAll(/\boption\s*\(\s*(?:\w+\.)*ecs_component_id\s*\)\s*=\s*([^;]+)\s*;/g)) {
        const literal = match[1].trim()
        const valid = /^(?:0[xX][0-9a-fA-F]+|0[0-7]*|[1-9]\d*)$/.test(literal)
        const id = /^0[0-7]+$/.test(literal) ? parseInt(literal, 8) : Number(literal)
        if (!valid || !Number.isSafeInteger(id) || id < 1 || id > 2147483647) {
          throw new Error(`invalid component ID ${match[1]} in ${file}`)
        }
        rows.push({ id, file })
      }
    }
  }
  return rows.sort((a, b) => a.id - b.id || a.file.localeCompare(b.file))
}

function main(args) {
  const [command, candidate, ...extra] = args
  if (!['list', 'check'].includes(command) || extra.length ||
      (command === 'list' && candidate !== undefined) ||
      (command === 'check' && (!/^[1-9]\d*$/.test(candidate || '') || Number(candidate) > 2147483647))) {
    throw new Error('Usage: list-components-ids.sh | check-component-id.sh <positive int32 ID>')
  }
  const root = path.resolve(__dirname, '../proto/decentraland/sdk/components')
  const rows = declarations(root)
  const duplicates = rows.filter((row, i) => i > 0 && row.id === rows[i - 1].id)
  if (duplicates.length) {
    const ids = [...new Set(duplicates.map((row) => row.id))]
    throw new Error(ids.map((id) => `duplicate component ID ${id}: ${rows.filter((row) => row.id === id).map((row) => path.relative(root, row.file)).join(', ')}`).join('\n'))
  }
  if (command === 'list') {
    console.log(`List of ids (${rows.length} total):`)
    rows.forEach((row, i) => console.log(`${i + 1}. ${row.id}`))
    return 0
  }
  const found = rows.filter((row) => row.id === Number(candidate))
  console.log(`id ${candidate} is ${found.length ? 'taken' : 'free'}`)
  return found.length ? 1 : 0
}

if (require.main === module) {
  try { process.exitCode = main(process.argv.slice(2)) }
  catch (error) { console.error(error.message); process.exitCode = 2 }
}
module.exports = { declarations }
