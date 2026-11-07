/**
 * DSH 插件冒烟测试。
 *
 * 不依赖 Harness 本体：用 `module.registerHooks` 把 `@deepseek-ai/dsh-tools` 换成桩，
 * 再用一个最小 ctx 复刻 `tools.register` / `subprocess.spawn` / `workingDirectory.ensure`。
 *
 * 校验三件事：
 *   1. 8 个工具的 parameters 符合 DSH value-schema DSL（类型、必填、additionalProperties）；
 *   2. 包清单（package.json / cordis.patch.yml / locale / icon）满足 bundle 契约；
 *   3. 用真实 fsops.exe 跑通 fs_write → fs_read → fs_edit → fs_grep → fs_glob → fs_stat → fs_exec → fs_file，
 *      并确认 E_ 错误会变成工具错误。
 *
 * 用法：node tests/dsh_plugin_smoke.mjs [--exe target/debug/fsops.exe]
 */
import { spawn } from 'node:child_process'
import { existsSync } from 'node:fs'
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { registerHooks } from 'node:module'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { pathToFileURL } from 'node:url'

const ROOT = resolve(dirname(new URL(import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, '$1')), '..')
const args = process.argv.slice(2)
const exeIndex = args.indexOf('--exe')
const EXE = exeIndex >= 0 ? resolve(args[exeIndex + 1]) : join(ROOT, 'target', 'debug', 'fsops.exe')

let failures = 0
function check(condition, message, detail) {
  if (condition) {
    console.log('PASS ' + message)
  } else {
    failures += 1
    console.log('FAIL ' + message)
    if (detail !== undefined) console.log('  - ' + String(detail).replace(/\n/g, '\n    '))
  }
}

// ---- 1. 包清单 -------------------------------------------------------------

const pkg = JSON.parse(await readFile(join(ROOT, 'package.json'), 'utf8'))
const cargo = await readFile(join(ROOT, 'Cargo.toml'), 'utf8')
const cargoVersion = /^version\s*=\s*"([^"]+)"/m.exec(cargo)?.[1]

check(pkg.type === 'module', 'package.json type=module')
check(pkg.dsh?.manifestVersion === 1, 'package.json 声明 dsh.manifestVersion=1')
check(pkg.dsh?.bundle?.patch === './cordis.patch.yml', 'package.json 声明 dsh.bundle.patch')
check(existsSync(join(ROOT, 'cordis.patch.yml')), 'cordis.patch.yml 存在')
check(
  Array.isArray(pkg.dsh?.bundle?.platforms) && pkg.dsh.bundle.platforms.includes('win32'),
  'bundle 仅声明 win32 平台',
)
check(pkg.version === cargoVersion, 'package.json 版本与 Cargo.toml 一致', pkg.version + ' vs ' + cargoVersion)
for (const required of ['dsh', 'cordis.patch.yml', 'locale/*.json', 'icon.svg', 'docs']) {
  check(Array.isArray(pkg.files) && pkg.files.includes(required), 'files 包含 ' + required)
}
check(existsSync(join(ROOT, 'dsh', 'index.js')), 'dsh/index.js 存在')
check(existsSync(join(ROOT, 'dsh', 'binary.js')), 'dsh/binary.js 存在')

const patchText = await readFile(join(ROOT, 'cordis.patch.yml'), 'utf8')
check(patchText.includes("name: '@tokenis2023/fsops'"), 'patch 用包名解析 Host 插件')
check(patchText.includes('id: fsops-tools'), 'patch 有稳定行 id：fsops-tools')

for (const lang of ['en', 'zh']) {
  const locale = JSON.parse(await readFile(join(ROOT, 'locale', lang + '.json'), 'utf8'))
  check(
    typeof locale.meta?.title === 'string' && locale.meta.title.length > 0,
    'locale/' + lang + '.json 有 meta.title',
  )
  check(
    typeof locale.meta?.description === 'string' && locale.meta.description.length > 0,
    'locale/' + lang + '.json 有 meta.description',
  )
}

const iconStat = await readFile(join(ROOT, 'icon.svg'))
check(iconStat.length < 256 * 1024, 'icon.svg 小于 256 KiB')
check(iconStat.toString('utf8').trimStart().startsWith('<svg'), 'icon.svg 是 SVG')

// ---- 2. 桩与插件加载 -------------------------------------------------------

const stubDir = await mkdtemp(join(tmpdir(), 'fsops-dsh-stub-'))
const stubPath = join(stubDir, 'dsh-tools.mjs')
await writeFile(stubPath, 'export const defineTool = (options) => options\n', 'utf8')

registerHooks({
  resolve(specifier, context, nextResolve) {
    if (specifier === '@deepseek-ai/dsh-tools') {
      return { url: pathToFileURL(stubPath).href, shortCircuit: true }
    }
    return nextResolve(specifier, context)
  },
})

const plugin = await import(pathToFileURL(join(ROOT, 'dsh', 'index.js')).href)
check(plugin.name === 'fsops', '插件导出 name=fsops')

// 版本裁定：解析与「显式来源不探测」两条边界。
{
  const binary = await import(pathToFileURL(join(ROOT, 'dsh', 'binary.js')).href)
  check(binary.extractVersion('fsops 0.6.0 tools=8') === '0.6.0', '版本解析：正常输出')
  check(binary.extractVersion('fsops v0.6.0') === '0.6.0', '版本解析：允许 v 前缀')
  check(binary.extractVersion('随便什么') === undefined, '版本解析：无版本返回 undefined')
  check(binary.extractVersion('') === undefined, '版本解析：空输出返回 undefined')
}
check(
  Array.isArray(plugin.inject) && ['tools', 'subprocess', 'workingDirectory'].every((s) => plugin.inject.includes(s)),
  '插件声明 tools/subprocess/workingDirectory 依赖',
)

// ---- 3. parameters DSL 校验 -------------------------------------------------

const SCALARS = new Set(['string', 'number', 'integer', 'boolean', 'null'])
function validateNode(node, path, inPropertyMap) {
  if (node === null || typeof node !== 'object' || Array.isArray(node)) {
    throw new Error(path + ' 必须是对象')
  }
  const allowed = new Set(['description', 'title', 'default', 'examples'])
  if (inPropertyMap) allowed.add('required')
  if (Object.hasOwn(node, 'required')) {
    if (node.required !== true) throw new Error(path + '.required 只能是 true')
  }
  if (Object.hasOwn(node, 'oneOf')) {
    allowed.add('oneOf')
    allowed.add('type')
    if (Object.hasOwn(node, 'type')) throw new Error(path + ' 不能同时声明 type 与 oneOf')
    if (!Array.isArray(node.oneOf) || node.oneOf.length < 2) throw new Error(path + '.oneOf 至少两项')
    node.oneOf.forEach((branch, index) => validateNode(branch, path + '.oneOf[' + index + ']', false))
    return
  }
  const type = node.type
  if (typeof type !== 'string') throw new Error(path + '.type 缺失')
  if (type === 'json') {
    allowed.add('type')
  } else if (type === 'object') {
    allowed.add('type')
    allowed.add('properties')
    allowed.add('additionalProperties')
    if (typeof node.additionalProperties !== 'boolean') {
      throw new Error(path + '.additionalProperties 必须显式 true/false')
    }
    for (const [key, value] of Object.entries(node.properties ?? {})) {
      validateNode(value, path + '.properties.' + key, true)
    }
  } else if (type === 'array') {
    allowed.add('type')
    allowed.add('items')
    if (Object.hasOwn(node, 'items')) validateNode(node.items, path + '.items', false)
  } else if (SCALARS.has(type)) {
    allowed.add('type')
    allowed.add('enum')
    allowed.add('const')
  } else {
    throw new Error(path + '.type 不支持：' + type)
  }
  for (const key of Object.keys(node)) {
    if (!allowed.has(key)) throw new Error(path + '.' + key + ' 不被 DSL 支持')
  }
}

// ---- 4. 假 Harness 上下文 ---------------------------------------------------

function makeContext(cwd) {
  const registered = new Map()
  const spawns = []
  const disposers = []
  return {
    registered,
    spawns,
    disposers,
    tools: {
      register(definition) {
        registered.set(definition.name, definition)
      },
    },
    workingDirectory: {
      async ensure() {
        return cwd
      },
    },
    on(event, handler) {
      if (event === 'dispose') disposers.push(handler)
    },
    subprocess: {
      async resolveExecutable(path) {
        if (path === 'fsops') throw new Error('not on PATH')
        if (!existsSync(path)) throw new Error('missing: ' + path)
        return path
      },
      spawn(spec) {
        spawns.push(spec.argv)
        const stdio = spec.stdio ?? {}
        const child = spawn(spec.argv[0], spec.argv.slice(1), {
          cwd: spec.cwd,
          windowsHide: true,
          stdio: ['pipe', 'pipe', 'pipe'],
        })
        const done = new Promise((settle) => {
          child.on('close', (exitCode, signal) => settle({ exitCode, signal }))
        })
        // 管道模式：长驻会话由调用方持有流，不能替它关 stdin。
        if (stdio.stdin === 'pipe' || stdio.stdout === 'pipe') {
          return {
            done,
            stdin: child.stdin,
            stdout: child.stdout,
            stderr: child.stderr,
            collected: {},
            terminate: () => child.kill(),
          }
        }
        let out = ''
        let err = ''
        child.stdout.on('data', (chunk) => {
          out += chunk
        })
        child.stderr.on('data', (chunk) => {
          err += chunk
        })
        const input = spec.stdio?.stdin?.data
        if (input !== undefined) child.stdin.end(input)
        else child.stdin.end()
        return {
          done,
          collected: {
            stdout: { readFrom: () => ({ text: out, lossy: false, nextOffset: out.length }) },
            stderr: { readFrom: () => ({ text: err, lossy: false, nextOffset: err.length }) },
          },
        }
      },
    },
  }
}

const work = await mkdtemp(join(tmpdir(), 'fsops-dsh-'))
const ctx = makeContext(work)
plugin.apply(ctx, { binary: EXE })

const names = [...ctx.registered.keys()].sort()
check(
  JSON.stringify(names) === JSON.stringify([
    'fs_edit', 'fs_exec', 'fs_file', 'fs_glob', 'fs_grep', 'fs_read', 'fs_stat', 'fs_write',
  ]),
  '注册 8 个 fs_* 工具',
  names.join(','),
)
for (const [toolName, definition] of ctx.registered) {
  try {
    for (const [key, value] of Object.entries(definition.parameters ?? {})) {
      validateNode(value, toolName + '.' + key, true)
    }
    check(true, toolName + ' parameters 符合 DSL')
  } catch (error) {
    check(false, toolName + ' parameters 符合 DSL', error.message)
  }
}

// ---- 4.5 fsops schema 与 DSH 参数集合一致 -----------------------------------
// MCP schema（fsops schema 子命令输出）与 DSH 插件的 parameters 必须同名同参数集合。
// 三处手写 schema 已漂移过 11 处（描述、缺字段），这里把漂移变成红灯。
{
  const text = await new Promise((resolve, reject) => {
    const child = spawn(EXE, ['schema'], { stdio: ['ignore', 'pipe', 'inherit'] })
    let out = ''
    child.stdout.on('data', (chunk) => (out += chunk))
    child.on('error', reject)
    child.on('close', (code) => (code === 0 ? resolve(out) : reject(new Error('exit=' + code))))
  })
  const defs = JSON.parse(text)
  check(
    Array.isArray(defs) && defs.length === ctx.registered.size,
    'fsops schema 与插件工具数一致',
    'schema=' + defs.length + ' 插件=' + ctx.registered.size,
  )
  for (const [toolName, definition] of ctx.registered) {
    const def = defs.find((d) => d.name === toolName)
    if (!def) {
      check(false, 'schema 一致：' + toolName, 'fsops schema 缺少该工具')
      continue
    }
    const mcpKeys = Object.keys(def.inputSchema?.properties ?? {}).sort()
    const dshKeys = Object.keys(definition.parameters ?? {}).sort()
    // 插件刻意不暴露这段后台任务参数：任务生命周期绑在长驻会话里，
    // 跨会话无法清理，需要长任务的用户走 MCP 形态。
    // 除此之外两侧参数集合必须完全一致，任何多出/缺失都是契约漂移。
    const dshAbsent = new Set(['background', 'job', 'list', 'wait_ms', 'kill'])
    const extra = dshKeys.filter((k) => !mcpKeys.includes(k))
    const missing = mcpKeys.filter((k) => !dshKeys.includes(k) && !dshAbsent.has(k))
    check(
      extra.length === 0 && missing.length === 0,
      'schema 一致：' + toolName + ' 参数集合',
      'DSH 多出: ' + extra.join(',') + ' / DSH 缺失(非后台参数): ' + missing.join(','),
    )
  }
  // 关键能力词必须在两端 description 里都出现：MCP 端曾漏写「正则替换」，
  // 害模型先 fs_read 抄原文再 old/new，白多一轮。
  for (const [tool, words] of Object.entries({
    fs_edit: ['正则替换'],
    fs_read: ['四选一'],
  })) {
    const dshDesc = ctx.registered.get(tool)?.description ?? ''
    const mcpDesc = defs.find((d) => d.name === tool)?.description ?? ''
    for (const word of words) {
      check(
        dshDesc.includes(word) && mcpDesc.includes(word),
        '描述能力一致：' + tool + ' 含 ' + word,
        'MCP: ' + mcpDesc.slice(0, 80) + ' / DSH: ' + dshDesc.slice(0, 80),
      )
    }
  }
  const execParams = Object.keys(ctx.registered.get('fs_exec')?.parameters ?? {})
  const forbidden = ['background', 'job', 'list', 'wait_ms', 'kill'].filter((k) =>
    execParams.includes(k),
  )
  check(forbidden.length === 0, 'fs_exec 不暴露后台任务参数', forbidden.join(','))
}

// ---- 5. 真实调用 -----------------------------------------------------------

async function call(name, args) {
  const definition = ctx.registered.get(name)
  return await definition.execute(args ?? {}, { agent: {}, signal: undefined })
}

try {
  const sample = 'fn main() {\n    let limit_0 = 100;\n    let limit_1 = 200;\n}\n'
  const file = join(work, 'sample.rs')
  const written = await call('fs_write', { path: file, content: sample })
  check(written.startsWith('ok'), 'fs_write 成功', written.slice(0, 200))

  const read = await call('fs_read', { path: file })
  check(read.includes('1|fn main()'), 'fs_read 返回行号与内容', read.slice(0, 200))

  const edited = await call('fs_edit', {
    path: file,
    edits: [{ pattern: 'let limit_(\\d+) = \\d+;', replacement: 'let cap_$1 = 500;' }],
    diff: true,
  })
  check(edited.includes('ok') && edited.includes('+let cap_0 = 500;'), 'fs_edit 正则替换并给 diff', edited.slice(0, 300))

  const after = await readFile(file, 'utf8')
  check(after.includes('let cap_1 = 500;'), 'fs_edit 落盘内容正确')

  const grep = await call('fs_grep', { pattern: 'cap_', path: work, output: 'count' })
  check(grep.includes('sample.rs:2'), 'fs_grep count 命中数正确', grep.slice(0, 200))

  const glob = await call('fs_glob', { pattern: '*.rs', path: work })
  check(glob.includes('sample.rs'), 'fs_glob 找到文件', glob.slice(0, 200))

  const stat = await call('fs_stat', { path: file, detail: 'meta' })
  check(/\dB/.test(stat) || stat.includes('file'), 'fs_stat 返回元信息', stat.slice(0, 200))

  const execOut = await call('fs_exec', { program: 'cmd', args: ['/c', 'echo', 'hi-from-exec'] })
  check(execOut.includes('exit=0') && execOut.includes('hi-from-exec'), 'fs_exec 返回退出码与输出', execOut.slice(0, 200))

  const copied = await call('fs_file', { op: 'copy', src: file, dst: join(work, 'copy.rs') })
  check(copied.startsWith('ok copy'), 'fs_file copy 成功', copied.slice(0, 200))
  let noConfirm = ''
  try {
    await call('fs_file', { op: 'remove', path: join(work, 'copy.rs') })
  } catch (error) {
    noConfirm = String(error.message)
  }
  check(noConfirm.startsWith('E_CONFIRM_REQUIRED'), 'fs_file remove 需要 confirm', noConfirm.slice(0, 200))

  let missing = ''
  try {
    await call('fs_read', { path: join(work, 'missing.txt') })
  } catch (error) {
    missing = String(error.message)
  }
  check(missing.startsWith('E_'), 'fsops E_ 错误转成工具错误', missing.slice(0, 200))

  // 并发正确性：4 个工作线程会乱序应答，响应靠 id 匹配，不能串号。
  const parallelFiles = []
  for (let index = 0; index < 4; index += 1) {
    const target = join(work, 'par' + index + '.txt')
    await call('fs_write', { path: target, content: 'marker-' + index + '\n' })
    parallelFiles.push(target)
  }
  const parallelTexts = await Promise.all(parallelFiles.map((target) => call('fs_read', { path: target })))
  const parallelOk = parallelTexts.every((text, index) => {
    if (!text.includes('marker-' + index)) return false
    return [0, 1, 2, 3].every((other) => other === index || !text.includes('marker-' + other))
  })
  check(parallelOk, '并发调用的响应按 id 匹配且不串号', parallelTexts.map((t) => t.slice(0, 40)).join(' | '))

  // 长驻会话复用：一次 spawn 服务上面全部调用，才是这次延迟优化的核心。
  const mcpSpawns = ctx.spawns.filter((argv) => argv[1] === 'mcp').length
  check(mcpSpawns === 1, '工具调用复用同一个长驻 MCP 会话', 'mcp spawn 次数=' + mcpSpawns)
  const oneShotSpawns = ctx.spawns.filter((argv) => argv[1] === 'call').length
  check(oneShotSpawns === 0, '正常路径不退回一次性 spawn', 'call spawn 次数=' + oneShotSpawns)
  // 显式 binary 就是用户的选择，不应该再花一次进程去核版本。
  const versionProbes = ctx.spawns.filter((argv) => argv[1] === 'version').length
  check(versionProbes === 0, '显式 binary 不做版本探测', 'version spawn 次数=' + versionProbes)

  // 会话释放后重建：插件重载 / 会话崩溃都要能自愈。
  for (const dispose of ctx.disposers) dispose()
  const revived = await call('fs_read', { path: parallelFiles[0] })
  check(revived.includes('marker-0'), '会话释放后自动重建', revived.slice(0, 120))
  const respawns = ctx.spawns.filter((argv) => argv[1] === 'mcp').length
  check(respawns === 2, '重建会话只再 spawn 一次', 'mcp spawn 次数=' + respawns)
} finally {
  for (const dispose of ctx.disposers) dispose()
  await rm(work, { recursive: true, force: true })
  await rm(stubDir, { recursive: true, force: true })
}

console.log('')
if (failures === 0) {
  console.log('全部通过')
  process.exit(0)
}
console.log(failures + ' 项失败')
process.exit(1)
