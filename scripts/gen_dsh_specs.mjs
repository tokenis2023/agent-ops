/**
 * 从 `fsops schema` 生成 DSH 插件的工具定义。
 *
 * 为什么要有这个脚本：工具描述与参数曾经手写三份（src/tools.rs、dsh/index.js、
 * docs/工具参考.md），实测漂移过 11 处、评审时仍有 5/8 个工具的描述不一致。
 * 模型靠 description 拼参数，漂移就是直接让模型拼错。现在 Rust 侧是唯一真源，
 * 本脚本把它投影成 DSH 的 value-schema DSL。
 *
 * 投影规则（DSL 的硬限制，见 tests/dsh_plugin_smoke.mjs 的 validateNode）：
 *   - 允许：description / title / default / examples / required(true) / type / enum /
 *     const / properties / additionalProperties / items / oneOf；
 *   - 因此丢弃 minimum / maxItems / minItems 等不在白名单里的约束；
 *   - MCP 的对象级 `required: ["a","b"]` 改写成各属性自己的 `required: true`。
 *
 * 用法：
 *   node scripts/gen_dsh_specs.mjs                 # 用 %LOCALAPPDATA% 或 PATH 上的 fsops
 *   node scripts/gen_dsh_specs.mjs --exe target/release/fsops.exe
 *   node scripts/gen_dsh_specs.mjs --check         # 只校验生成的产物是否最新（CI 用）
 */
import { execFileSync } from 'node:child_process'
import { existsSync, readFileSync, writeFileSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

import { pathToFileURL } from 'node:url'

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const OUT = join(ROOT, 'dsh', 'tool-specs.generated.js')

/** 只读工具可以并行调用；与 Rust 侧语义无关，是 DSH 的并发提示。 */
const READ_ONLY = new Set(['fs_read', 'fs_glob', 'fs_grep', 'fs_stat'])

/**
 * 插件形态刻意不暴露的参数：后台任务的生命周期绑在长驻会话上，
 * 插件重载/宿主重启后就丢了，模型排障会踩空。需要后台任务请用 MCP 形态。
 * 这里集中声明，避免"手写少了几个参数"这种又一种漂移。
 */
const EXCLUDED = {
  fs_exec: ['background', 'job', 'list', 'wait_ms', 'kill'],
}

/**
 * 插件形态需要改写描述的条目：Rust 侧描述是 MCP/CLI 的完整能力，其中后台任务
 * 用法在插件形态不存在（schema 不声明，运行时也会显式拒绝，见 dsh/index.js 的
 * EXCLUDED_ARGS）。不在这里覆写，模型会按描述拼出拿不到的参数（评测 P2 漂移）。
 */
const DESCRIPTION_PATCHES = {
  fs_exec: (description) => {
    const cut = description.indexOf('启动任务必须给 program')
    const head = cut >= 0 ? description.slice(0, cut) : description
    return (
      head +
      '启动任务必须给 program。DSH 插件形态不提供后台任务参数（background/job/list/wait_ms/kill）；' +
      '需要后台任务请用 MCP 形态 `fsops mcp`。'
    )
  },
}

/** 把一个 JSON-Schema 节点投影成 DSH DSL 节点。 */
function projectNode(node, inPropertyMap) {
  const out = {}
  if (typeof node.description === 'string') out.description = node.description
  if (Array.isArray(node.enum)) {
    out.type = node.type ?? 'string'
    out.enum = [...node.enum]
    return out
  }
  if (node.type === 'array') {
    out.type = 'array'
    if (node.items !== undefined) out.items = projectNode(node.items, false)
    return out
  }
  if (node.type === 'object') {
    // 自由映射（MCP 只声明 type:"object"，例如 fs_exec.env 的任意环境变量）必须保留
    // “可以带任意键”。DSH 的 value-schema DSL 没有开放对象写法，用 json 表达最接近；
    // 若投影成空对象 + additionalProperties:false，宿主会拒绝一切键（评测 P1）。
    if (Object.keys(node.properties ?? {}).length === 0 && node.additionalProperties !== false) {
      out.type = 'json'
      return out
    }
    out.type = 'object'
    out.additionalProperties = node.additionalProperties === true
    out.properties = projectProperties(node, inPropertyMap)
    return out
  }
  out.type = node.type ?? 'json'
  return out
}

/** 投影一层属性表，并把对象级 required 数组下推到各属性。 */
function projectProperties(schema, allowRequired, toolName) {
  const required = new Set(Array.isArray(schema.required) ? schema.required : [])
  const skip = new Set(EXCLUDED[toolName] ?? [])
  const properties = {}
  for (const key of Object.keys(schema.properties ?? {}).sort()) {
    if (skip.has(key)) continue
    const child = projectNode(schema.properties[key], true)
    if (allowRequired && required.has(key)) child.required = true
    properties[key] = child
  }
  return properties
}

/** 把 MCP tools/list 的定义投影成插件用的 TOOL_SPECS。 */
export function projectSpecs(defs) {
  return defs.map((def) => {
    const schema = def.inputSchema ?? {}
    const spec = { name: def.name }
    if (READ_ONLY.has(def.name)) spec.readOnly = true
    if (typeof def.description === 'string') {
      const patch = DESCRIPTION_PATCHES[def.name]
      spec.description = patch === undefined ? def.description : patch(def.description)
    }
    spec.parameters = projectProperties(schema, true, def.name)
    return spec
  })
}

/** 生成文件内容（稳定输出：属性已排序）。 */
export function renderSpecs(specs) {
  return (
    '/**\n' +
    ' * 本文件由 scripts/gen_dsh_specs.mjs 从 `fsops schema` 生成，请勿手工编辑。\n' +
    ' * 改了 src/tools.rs 的工具定义后，跑一次该脚本重新生成。\n' +
    ' */\n' +
    'export const TOOL_SPECS = ' +
    JSON.stringify(specs, null, 2) +
    '\n'
  )
}

function parseArgs(argv) {
  const options = { exe: undefined, check: false }
  for (let index = 0; index < argv.length; index += 1) {
    const arg = argv[index]
    if (arg === '--check') options.check = true
    else if (arg === '--exe') {
      options.exe = argv[index + 1]
      index += 1
    }
  }
  return options
}

function resolveExe(explicit) {
  const candidates = [
    explicit,
    process.env.FSOPS_BIN,
    join(ROOT, 'target', 'release', 'fsops.exe'),
    join(ROOT, 'target', 'debug', 'fsops.exe'),
    join(process.env.LOCALAPPDATA ?? '', 'fsops', 'fsops.exe'),
  ].filter((value) => typeof value === 'string' && value.length > 0)
  for (const candidate of candidates) {
    if (existsSync(candidate)) return candidate
  }
  throw new Error('找不到 fsops.exe，先 cargo build --release 或用 --exe 指定')
}

function readSchema(exe) {
  const stdout = execFileSync(exe, ['schema'], { encoding: 'utf8', maxBuffer: 32 * 1024 * 1024 })
  return JSON.parse(stdout)
}

function main() {
  const options = parseArgs(process.argv.slice(2))
  const exe = resolveExe(options.exe)
  const expected = renderSpecs(projectSpecs(readSchema(exe)))
  if (options.check) {
    const actual = existsSync(OUT) ? readFileSync(OUT, 'utf8') : ''
    if (actual !== expected) {
      console.error('E_DRIFT ' + OUT + ' 与 fsops schema 不一致，请重新生成')
      process.exit(2)
    }
    console.log('ok ' + OUT + ' 与 fsops schema 一致')
    return
  }
  writeFileSync(OUT, expected, 'utf8')
  console.log('ok 已生成 ' + OUT + '（来源 ' + exe + '）')
}

if (process.argv[1] !== undefined && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main()
}
