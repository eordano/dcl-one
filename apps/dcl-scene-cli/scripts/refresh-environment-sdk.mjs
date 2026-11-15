#!/usr/bin/env node
// Refresh only the environment bindings in the pinned CLI SDK. The original blob builder is
// absent from this tree; rebuilding its complete core from another SDK would discard patches.
// Keep the original core intact and install generated bindings through its lazy module registry.
import { createRequire } from 'node:module'
import { readFile, writeFile, mkdtemp, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import { execFileSync } from 'node:child_process'
import { createHash } from 'node:crypto'
const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../..')
const require = createRequire(path.join(root, 'library/js-sdk-toolchain/package.json'))
const ts = require('typescript')
const { build } = createRequire(path.join(root, 'rendering/bevy-explorer/source/react-web/package.json'))('esbuild')
const sdk = path.join(root, 'library/js-sdk-toolchain/packages/@dcl/ecs/src')
const archive = path.join(root, 'apps/dcl-scene-cli/src/vendor/node_modules.zip')
const names = ['Skybox', 'Sun', 'EnvironmentFog']
const marker = '\n/* dcl-one generated environment bindings */\n'
const temp = await mkdtemp(path.join(tmpdir(), 'dcl-environment-sdk-'))
const sha = value => createHash('sha256').update(value).digest('hex')
const inputs = {}
async function source(relative) {
  const bytes = await readFile(path.join(sdk, relative), 'utf8')
  inputs[relative] = sha(bytes)
  return bytes
}
const transpile = text => ts.transpileModule(text, {compilerOptions: {
  target: ts.ScriptTarget.ES2020, module: ts.ModuleKind.CommonJS, esModuleInterop: true
}}).outputText
try {
  // The component factory declarations are copied from the generator's AST, not reimplemented.
  const index = ts.createSourceFile('index.ts', await source('components/generated/index.gen.ts'), ts.ScriptTarget.Latest, true)
  const declarations = names.map(name => {
    const statement = index.statements.find(s => ts.isVariableStatement(s) && s.declarationList.declarations.some(d => d.name.getText(index) === name))
    if (!statement) throw new Error(`generated component is missing: ${name}`)
    return statement.getText(index)
  }).join('\n')
  const imports = names.map(n => `import { ${n}Schema } from './${n}.gen'`).join('\n')
  const protoPaths = ['skybox', 'sun', 'environment_fog'].map(n => './pb/decentraland/sdk/components/' + n + '.gen')
  const protoExports = protoPaths.map(p => `export * from '${p}'`).join('\n')
  const factories = transpile(imports + '\n' + declarations + '\n' + protoExports)
  const mappings = await source('components/generated/component-names.gen.ts')
  const selectedMappings = Object.fromEntries(names.map(name => {
    const match = mappings.match(new RegExp('"core::' + name + '": (\\d+)'))
    if (!match) throw new Error(`generated ID is missing: ${name}`)
    return ['core::' + name, Number(match[1])]
  }))
  const files = {}
  const visited = new Set()
  async function collect(relative) {
    if (visited.has(relative)) return
    visited.add(relative)
    const text = await source(relative)
    files[`node_modules/@dcl/ecs/dist-cjs/${relative.replace(/\.ts$/, '.js')}`] = transpile(text)
    // Only generated protobuf dependencies belong to this refresh. Schema type imports disappear.
    const parsed = ts.createSourceFile(relative, text, ts.ScriptTarget.Latest, true)
    for (const statement of parsed.statements) {
      if (!ts.isImportDeclaration(statement) || !ts.isStringLiteral(statement.moduleSpecifier)) continue
      const specifier = statement.moduleSpecifier.text
      if (specifier.startsWith('.') && (relative.includes('/pb/') || specifier.includes('/pb/'))) {
        await collect(path.posix.normalize(path.posix.join(path.posix.dirname(relative), specifier + '.ts')))
      }
    }
  }
  for (const name of names) await collect(`components/generated/${name}.gen.ts`)
  // Common codecs already present in the pinned SDK are deliberately not overwritten below.
  const indexAppend = `\n;(function(exports){\n${factories}\n${names.map(n => `exports.componentDefinitionByName['core::${n}'] = exports.${n};`).join('\n')}\n})(exports);\n`
  const globalAppend = '\n' + names.map(n => `exports.${n} = require('./index.gen').${n}(require('../../runtime/initialization').engine);`).join('\n') + '\n'
  const protoImports = protoPaths.map((p, i) => `import * as proto${i} from '${path.join(sdk, 'components/generated', p)}'`).join('\n') + `\nimport * as gradients from '${path.join(sdk, 'components/generated/pb/decentraland/common/gradient.gen')}'`;
  const entry = protoImports + '\n' + imports.replaceAll("'./", "'" + path.join(sdk, 'components/generated/').replaceAll('\\', '/') ) + '\n' + declarations.replaceAll('export const ', 'const ') + `
const factories = { ${names.join(', ')} };
const codecs = {...proto0, ...proto1, ...proto2};
const instances = new WeakMap();
function addExports(target, values) {
  for (const [name, value] of Object.entries(values)) if (!Object.hasOwn(target, name)) Object.defineProperty(target, name, {enumerable:true, value});
}
function components(target) {
  addExports(target, codecs);
  for (const [name, factory] of Object.entries(factories)) {
    if (!Object.hasOwn(target, name)) Object.defineProperty(target, name, {enumerable:true, configurable:true, value:factory});
    target.componentDefinitionByName['core::' + name] = factory;
  }
}
const registry = __dclEnvironmentRegistry;
for (const key of ['@dcl/ecs', '@dcl/sdk/ecs', '@dcl/ecs/dist/components']) {
  const original = Object.getOwnPropertyDescriptor(registry, key);
  if (!original?.get) throw new Error('Missing SDK registry getter: ' + key);
  Object.defineProperty(registry, key, {...original, get() {
    const target = original.get();
    if (key === '@dcl/ecs/dist/components') { components(target); return target; }
    components(target.components);
    addExports(target, {...codecs, ...gradients});
    let values = instances.get(target.engine);
    if (!values) {
      values = Object.fromEntries(Object.entries(factories).map(([name, factory]) => [name, factory(target.engine)]));
      instances.set(target.engine, values);
    }
    for (const [name, value] of Object.entries(values)) {
      if (!Object.hasOwn(target, name)) Object.defineProperty(target, name, {enumerable:true, value});
    }
    return target;
  }});
}
`
  const result = await build({stdin:{contents:entry, resolveDir:root, loader:'ts'}, bundle:true, format:'iife', platform:'neutral', target:'es2020', write:false, minify:true, nodePaths:[path.join(root,'library/js-sdk-toolchain/node_modules')]})
  // The blob's TypeScript surface is one ambient declaration bundle in @dcl/js-runtime.
  // Emit declarations from the same generated TS, then resolve imports to that bundle's names.
  const ambient = (relative, text) => {
    const options = {target:ts.ScriptTarget.ES2020, module:ts.ModuleKind.ESNext, declaration:true, emitDeclarationOnly:true, noResolve:true}
    const host = ts.createCompilerHost(options)
    const file = path.join(temp, 'declarations.ts')
    const getSourceFile = host.getSourceFile.bind(host)
    host.getSourceFile = (name, ...args) => name === file ? ts.createSourceFile(file, text, ts.ScriptTarget.Latest, true) : getSourceFile(name, ...args)
    let declarationOutput
    const emission = ts.createProgram([file], options, host).emit(undefined, (name, bytes) => { if (name.endsWith('.d.ts')) declarationOutput = bytes })
    if (emission.emitSkipped || declarationOutput === undefined) throw new Error(`could not emit declarations for ${relative}: ${JSON.stringify(emission.diagnostics.map(d => d.messageText))}`)
    const moduleName = '@dcl/ecs/dist/' + relative.replace(/\.ts$/, '')
    const output = declarationOutput.replace(/\bexport declare /g, 'export ').replace(/(['"])(\.[^'"]+)\1/g, (_, quote, specifier) => quote + '@dcl/ecs/dist/' + path.posix.normalize(path.posix.join(path.posix.dirname(relative), specifier)) + quote)
    return `declare module '${moduleName}' {\n${output}\n}\n`
  }
  let declarationsAppend = ''
  for (const relative of visited) {
    if (relative.includes('/pb/') && (!relative.includes('/common/') || relative.endsWith('/gradient.gen.ts'))) {
      declarationsAppend += ambient(relative, await source(relative))
    }
  }
  const pbImports = protoPaths.map((p, i) => `import { PB${names[i]} } from '${p}'`).join('\n')
  const componentType = "import { LastWriteWinElementSetComponentDefinition } from '../../engine/component'\n"
  declarationsAppend += ambient('components/generated/index.gen.ts', "import { LwwComponentGetter } from '@dcl/ecs/dist/components/generated/index.gen'\n" + componentType + pbImports + '\n' + declarations + '\n' + protoExports)
  const globalSource = ts.createSourceFile('global.ts', await source('components/generated/global.gen.ts'), ts.ScriptTarget.Latest, true)
  const globals = names.map(name => globalSource.statements.find(statement => ts.isVariableStatement(statement) && statement.declarationList.declarations.some(d => d.name.getText(globalSource) === name)).getText(globalSource)).join('\n')
  declarationsAppend += ambient('components/generated/global.gen.ts', componentType + pbImports + '\n' + globals)
  declarationsAppend += ambient('components/generated/types.gen.ts', "export * from './pb/decentraland/common/gradient.gen'")
  // Record generated source inputs and bundle bytes. ZIP handling preserves every untouched entry.
  const payload = {files, marker, indexAppend, globalAppend, extension:result.outputFiles[0].text.replaceAll("__dclEnvironmentRegistry", "module.exports"), inputs, selectedMappings, declarationsAppend, typesAppend: transpile("export * from './pb/decentraland/common/gradient.gen'")}
  await writeFile(path.join(temp,'payload.json'), JSON.stringify(payload))
  execFileSync('python3', [path.join(path.dirname(fileURLToPath(import.meta.url)), 'refresh-environment-sdk.py'), archive, path.join(temp,'payload.json')], {stdio:'inherit'})
} finally { await rm(temp,{recursive:true,force:true}) }
