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
import { readdirSync } from 'node:fs'
import { registerHooks } from 'node:module'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { pathToFileURL } from 'node:url'

const ROOT = resolve(dirname(new URL(import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, '$1')), '..')
const args = process.argv.slice(2)
const exeIndex = args.indexOf('--exe')
const EXE = exeIndex >= 0 ? resolve(args[exeIndex + 1]) : join(ROOT, 'target', 'debug', 'fsops.exe')

let failures = 0
/** Windows 上被杀内核的子进程可能短暂占住目录，清理时重试几次。 */
async function rmWithRetry(target) {
  for (let attempt = 0; attempt < 10; attempt += 1) {
    try {
      await rm(target, { recursive: true, force: true })
      return
    } catch (error) {
      const code = error?.code
      if (code !== 'EBUSY' && code !== 'EPERM' && code !== 'ENOTEMPTY') throw error
      await new Promise((resolve) => setTimeout(resolve, 200))
    }
  }
  await rm(target, { recursive: true, force: true })
}

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
// dsh/ 下的每个模块都必须是产物的一部分：漏一个就会让 npm pack 出来的包在运行时
// import 失败（Excluded 曾差点只留在仓库里）。
{
  const modules = readdirSync(join(ROOT, 'dsh')).filter((name) => name.endsWith('.js'))
  const declared = JSON.stringify(pkg.files ?? [])
  check(
    declared.includes('"dsh"') || pkg.files.some((entry) => entry === 'dsh' || entry.startsWith('dsh/')),
    'package.json files 收录 dsh/ 目录',
    '当前 files=' + declared,
  )
  for (const module of modules) {
    check(existsSync(join(ROOT, 'dsh', module)), 'dsh/' + module + ' 存在')
  }
}

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

// 工具清单是契约的一部分：新增工具必须同时出现在这里，避免「悄悄多/少一个」。
const EXPECTED_TOOLS = [
  'fs_edit', 'fs_exec', 'fs_file', 'fs_glob', 'fs_grep', 'fs_inspect',
  'fs_read', 'fs_stat', 'fs_write',
]
const names = [...ctx.registered.keys()].sort()
check(
  JSON.stringify(names) === JSON.stringify(EXPECTED_TOOLS),
  '注册全部 ' + EXPECTED_TOOLS.length + ' 个 fs_* 工具',
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
  // 生成物必须与 `fsops schema` 逐字一致：手写三份 schema 曾漂移 11 处、
  // 评审时仍有 5/8 个工具描述不一致。现在漂移在这里变红灯。
  {
    const generator = await import(
      pathToFileURL(join(ROOT, 'scripts', 'gen_dsh_specs.mjs')).href
    )
    const generated = await import(
      pathToFileURL(join(ROOT, 'dsh', 'tool-specs.generated.js')).href
    )
    const expected = JSON.stringify(generator.projectSpecs(defs))
    const actual = JSON.stringify(generated.TOOL_SPECS)
    const drifted = expected !== actual
    check(
      !drifted,
      'dsh/tool-specs.generated.js 与 fsops schema 无漂移',
      drifted ? '跑 node scripts/gen_dsh_specs.mjs 重新生成' : '',
    )
    const rendered = generator.renderSpecs(generator.projectSpecs(defs))
    check(
      rendered === (await readFile(join(ROOT, 'dsh', 'tool-specs.generated.js'), 'utf8')),
      '生成物渲染结果稳定（可直接提交）',
      '渲染不一致：检查生成器输出格式',
    )
    // 插件形态的描述必须与 schema 一致：不能再教模型用被剔除的后台任务参数。
    const generatedExec = generated.TOOL_SPECS.find((spec) => spec.name === 'fs_exec')
    const execDesc = generatedExec?.description ?? ''
    check(
      !execDesc.includes('job=') && !execDesc.includes('list=true'),
      'fs_exec 描述不再宣传后台任务参数',
      execDesc.slice(0, 160),
    )
    check(execDesc.includes('DSH 插件形态'), 'fs_exec 描述点明插件形态边界', execDesc.slice(-120))
    // 自由映射对象（fs_exec.env）必须投影成 json：空对象 + additionalProperties:false 会把任意键拒掉。
    check(
      generatedExec?.parameters?.env?.type === 'json',
      'fs_exec.env 投影为开放 json',
      JSON.stringify(generatedExec?.parameters?.env),
    )
  }

  // 关键能力词必须在两端 description 里都出现：MCP 端曾漏写「正则替换」，
  // 害模型先 fs_read 抄原文再 old/new，白多一轮。
  for (const [tool, words] of Object.entries({
    fs_edit: ['正则替换'],
    fs_read: ['模式表'],
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

// ---- 4.6 插件形态的参数裁剪：生成器、schema、运行时三方必须对齐 ----------
{
  const excluded = await import(pathToFileURL(join(ROOT, 'dsh', 'excluded.js')).href)
  const execSpec = ctx.registered.get('fs_exec')
  const declared = Object.keys(execSpec?.parameters ?? {})
  for (const [tool, names] of Object.entries(excluded.EXCLUDED)) {
    const params = Object.keys(ctx.registered.get(tool)?.parameters ?? {})
    for (const name of names) {
      check(
        !params.includes(name),
        '插件 schema 不声明 ' + tool + '.' + name,
        params.join(','),
      )
    }
    // 描述里也不能再教模型用这些参数（否则模型按描述拼参数一定撞墙）
    const description = ctx.registered.get(tool)?.description ?? ''
    const taught = names.filter((name) => description.includes(name + '='))
    check(
      taught.length === 0,
      tool + ' 描述不教已剔除的参数',
      taught.join(','),
    )
  }
  // 运行时必须真的拒绝：DSH 顶层校验不拦 schema 外字段，只靠 schema 是拦不住的
  let rejected = ''
  try {
    await ctx.registered.get('fs_exec').execute({ program: 'cmd', args: ['/c', 'echo', 'x'], background: true }, { agent: {}, signal: undefined })
  } catch (error) {
    rejected = String(error.message)
  }
  check(
    rejected.includes('不支持 background'),
    '运行时拒绝 schema 外参数 background',
    rejected.slice(0, 160),
  )
  check(declared.length > 0, 'fs_exec 仍有可见参数')
}

// ---- 5. 真实调用 -----------------------------------------------------------

async function call(name, args, exec = { agent: {}, signal: undefined }) {
  const definition = ctx.registered.get(name)
  return await definition.execute(args ?? {}, exec)
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

  // env 曾是宿主 schema 的 P1：自由映射被投影成空封闭对象，任何键都被拒。
  const envOut = await call('fs_exec', {
    program: 'node',
    args: ['-e', 'console.log(process.env.SMOKE_ENV||"MISSING")'],
    env: { SMOKE_ENV: 'ok-from-env' },
  })
  check(envOut.includes('ok-from-env'), 'fs_exec env 注入生效（插件形态）', envOut.slice(0, 200))

  // 后台任务参数：schema 不声明，传输层显式拒绝，避免"文档说没有、运行时却穿透"。
  let backgroundRejected = ''
  try {
    await call('fs_exec', { program: 'cmd', args: ['/c', 'echo', 'x'], background: true })
  } catch (error) {
    backgroundRejected = String(error.message)
  }
  check(
    backgroundRejected.includes('后台任务') && backgroundRejected.includes('MCP'),
    'fs_exec 后台任务参数被显式拒绝',
    backgroundRejected.slice(0, 200),
  )

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

  // 取消语义：唯一在途的写/执行请求被 abort 时结束内核会话，下次调用重建；
  // 否则用户取消后 fsops 可能继续写盘/跑命令。
  {
    const controller = new AbortController()
    const pendingExec = call(
      'fs_exec',
      { program: 'node', args: ['-e', 'setTimeout(() => {}, 5000)'], cwd: tmpdir() },
      { agent: {}, signal: controller.signal },
    )
    setTimeout(() => controller.abort(), 150)
    let abortMessage = ''
    try {
      await pendingExec
    } catch (error) {
      abortMessage = String(error?.message ?? error)
    }
    check(abortMessage.includes('取消'), 'abort 后抛出取消错误', abortMessage.slice(0, 120))
    const afterAbortSpawns = ctx.spawns.filter((argv) => argv[1] === 'mcp').length
    check(afterAbortSpawns === 1, 'abort 结束旧会话（不重放）', 'mcp spawn 次数=' + afterAbortSpawns)
    const oneshotAfterAbort = ctx.spawns.filter((argv) => argv[1] === 'call').length
    check(oneshotAfterAbort === 0, '取消不触发一次性 spawn 重放', 'call spawn 次数=' + oneshotAfterAbort)
    const afterAbort = await call('fs_read', { path: parallelFiles[0] })
    check(afterAbort.includes('marker-0'), 'abort 后自动重建会话')
    const rebuilt = ctx.spawns.filter((argv) => argv[1] === 'mcp').length
    check(rebuilt === 2, 'abort 重建只再 spawn 一次', 'mcp spawn 次数=' + rebuilt)
  }

  // 会话释放后重建：插件重载 / 会话崩溃都要能自愈。
  for (const dispose of ctx.disposers) dispose()
  const revived = await call('fs_read', { path: parallelFiles[0] })
  check(revived.includes('marker-0'), '会话释放后自动重建', revived.slice(0, 120))
  const respawns = ctx.spawns.filter((argv) => argv[1] === 'mcp').length
  check(respawns === 3, '重建会话只再 spawn 一次', 'mcp spawn 次数=' + respawns)
} finally {
  for (const dispose of ctx.disposers) dispose()
  await rmWithRetry(work)
  await rmWithRetry(stubDir)
}

console.log('')
if (failures === 0) {
  console.log('全部通过')
  process.exit(0)
}
console.log(failures + ' 项失败')
process.exit(1)
