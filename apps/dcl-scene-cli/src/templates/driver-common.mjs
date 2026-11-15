// What the node drivers of dcl-scene-cli share. The CLI writes this file
// beside each driver in .dcl-one/ (scene::write_driver).
import fs from 'node:fs'
import path from 'node:path'

// Whether `p` is `base` or under it, once resolved.
export function inside(base, p) {
  const rel = path.relative(base, path.resolve(p))
  return rel === '' || (!path.isAbsolute(rel) && rel !== '..' && !rel.startsWith('..' + path.sep))
}

// `p` with its symlinks followed, as far as it exists. A link to nothing is
// refused: a write through it would create its target.
export function realpath(p) {
  try {
    return fs.realpathSync(p)
  } catch (e) {
    const parent = path.dirname(p)
    if (e.code !== 'ENOENT' || parent === p || fs.lstatSync(p, { throwIfNoEntry: false })) throw e
    return path.join(realpath(parent), path.basename(p))
  }
}

// A file the scene names through ~system/Runtime.readFile.
export function readSceneFile(root, fileName) {
  const file = path.join(root, fileName)
  if (!inside(root, file)) throw new Error(`${fileName} is outside the scene folder`)
  return fs.readFileSync(file)
}

// Runs a built CommonJS bundle with `sandbox` as its globalThis and
// `sandbox.console` as its `console`, and returns its exports. The sandbox
// hides names only: the code keeps node's privileges.
export function loadCjs(root, rel, sandbox) {
  const code = fs.readFileSync(path.join(root, rel), 'utf8')
  const mod = { exports: {} }
  const wrapper = new Function(
    'globalThis',
    'module',
    'exports',
    'console',
    'const require = globalThis.require;\n' + code
  )
  wrapper.call(sandbox, sandbox, mod, mod.exports, sandbox.console)
  return mod.exports
}

// The CLI holds this process's stdin, and any way the CLI dies closes the
// pipe: exit with it instead of staying behind as an orphan.
export function exitWithParent() {
  process.stdin.resume()
  for (const event of ['end', 'close']) process.stdin.on(event, () => process.exit(0))
}
