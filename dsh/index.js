/**
 * fsops 的 DSH Host 插件：把 8 个 fs_* 工具注册到当前 profile。
 *
 * 不重复实现文件语义——每次调用都通过 `fsops call <tool> -`（JSON 走 stdin）
 * 交给同一份 Rust 内核，因此错误码、预算、续读提示和输出格式与 fsops 的
 * MCP / CLI 完全一致。
 */
import { readFileSync } from 'node:fs'
import { dirname, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { defineTool } from '@deepseek-ai/dsh-tools'
import { resolveFsops } from './binary.js'

/** Cordis 插件名。 */
export const name = 'fsops'
/** 依赖的 Harness 服务：工具注册表、子进程、Agent 工作目录。 */
export const inject = ['tools', 'subprocess', 'workingDirectory']

const PKG_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..')
/** 与 Cargo.toml 同源的版本号：只从 package.json 读，避免两处硬编码漂移。 */
const PLUGIN_VERSION = JSON.parse(readFileSync(resolve(PKG_ROOT, 'package.json'), 'utf8')).version
const MAX_STDOUT_BYTES = 8 * 1024 * 1024
const MAX_STDERR_BYTES = 256 * 1024
const GRACE_MS = 5000

/** fs_edit 的单条修改：字面量 {old,new} 或正则 {pattern,replacement}。 */
function editItemSchema() {
  return {
    type: 'object',
    additionalProperties: false,
    properties: {
      old: { type: 'string', description: '原文片段；必须唯一，除非 all=true' },
      new: { type: 'string', description: '替换文本；删除必须显式写空串' },
      pattern: { type: 'string', description: '正则模式；不知道确切原文时用它' },
      replacement: { type: 'string', description: '正则替换文本，支持 $1 捕获组' },
      all: { type: 'boolean', description: '替换所有匹配' },
      start: { type: 'integer', description: '只在起始行之后修改（1 基）' },
      end: { type: 'integer', description: '只在结束行之前修改（含）' },
    },
  }
}

const TOOL_SPECS = [
  {
    name: 'fs_read',
    readOnly: true,
    description:
      '读取文件。优先用本工具替代宿主 read_file/cat/head/tail/Get-Content。默认最多返回 32KB 并给出续读方式；支持行范围、多文件、reads 异构区间、focus 按模式取块、summary 结构摘要、block 代码块展开、DOCX/PDF 正文、自动识别 GBK/UTF-16/BOM 与 CRLF。模式四选一：单文件 path、多文件 paths、异构区间 reads、按正则取块 focus；summary 仅单文件且不能与 ranges/block/end 同用；reads 每项可带 focus 与 budget。',
    parameters: {
      path: { type: 'string', description: '文件路径，如 /d/repo/app/src/main.rs' },
      paths: { type: 'array', items: { type: 'string' }, description: '一次读多个文件（最多 20 个）' },
      start: { type: 'integer', description: '起始行（1 基）' },
      end: { type: 'integer', description: '结束行（含）' },
      block: { type: 'boolean', description: '返回包含 start 行的整个代码块；配合 fs_grep 的行号用' },
      ranges: {
        type: 'array',
        items: { type: 'array', items: { type: 'integer' } },
        description: '单文件多段行区间，如 [[1,20],[80,100]]；优先于 start/end',
      },
      budget: { type: 'integer', description: '最多返回字节数，默认 32768（约 8K token）' },
      max_line: { type: 'integer', description: '每行最多字符数，默认 240；0 表示不截断长行' },
      scan: {
        type: 'string',
        enum: ['window', 'full'],
        description: '仅对 >32MB 文件生效：window（默认）只扫请求窗口，不给全文 hash/总行数；full 全扫',
      },
      summary: {
        type: 'boolean',
        description: '只输出结构摘要：声明行 + 行号，不展开函数体；行号可直接接 block=true 读实现。与 ranges/block/end 互斥',
      },
      tail: {
        type: 'integer',
        description: '只读最后 N 行（日志场景）；与 start/end/ranges/block/summary/focus/paths 互斥',
      },
      reads: {
        type: 'array',
        items: {
          type: 'object',
          additionalProperties: false,
          properties: {
            path: { type: 'string' },
            start: { type: 'integer' },
            end: { type: 'integer' },
            ranges: {
              type: 'array',
              items: { type: 'array', items: { type: 'integer' } },
              description: '该项的多段行区间，优先于 start/end',
            },
            block: { type: 'boolean' },
            max_line: { type: 'integer' },
            scan: { type: 'string', enum: ['window', 'full'], description: '仅对 >32MB 文件生效' },
            budget: { type: 'integer', description: '该项独立预算；未给时与其他项均分顶层 budget' },
            focus: { type: 'string', description: '该项只返回命中所在的完整代码块' },
          },
        },
        description: '一次读多个文件的不同区间，每项 {path,start?,end?,ranges?,block?,max_line?,scan?,budget?,focus?}；与 path/paths/ranges/start/end/block/summary/focus 互斥，预算按项均分',
      },
      focus: { type: 'string', description: '正则；只返回命中所在的完整代码块，替代 grep+block 两步；数据文件只给命中行 ±2 行；仅单文件，与 paths/reads/ranges/start/end/block/summary 互斥；JSON 字符串里反斜杠需双写' },
      force: { type: 'boolean', description: '读取缓存默认关闭；仅当 FSOPS_MCP_CACHE=1 开启后，用本参数绕过未变更摘要强制返回正文' },
    },
  },
  {
    name: 'fs_edit',
    description:
      '精确替换文本（old→new）或正则替换（pattern→replacement）。优先替代 patch/Edit/apply_patch。同文件多处用 edits，多文件用 files；全部预检通过才写入。自动处理 CRLF/LF 差异并保留原编码；DOCX 不支持精确替换。',
    parameters: {
      path: { type: 'string', description: '单文件路径；批量时改用 files' },
      edits: { type: 'array', items: editItemSchema(), description: '单文件的修改列表' },
      files: {
        type: 'array',
        description: '多文件批量编辑；全部预检成功后逐文件原子写入，最多 50 个',
        items: {
          type: 'object',
          additionalProperties: false,
          properties: {
            path: { type: 'string' },
            edits: { type: 'array', items: editItemSchema() },
            expect_hash: { type: 'string', description: '该文件的并发保护指纹' },
            normalize_eol: { type: 'boolean', description: '允许把混合换行归一为 LF' },
          },
        },
      },
      dry_run: { type: 'boolean', description: '只校验不落盘' },
      diff: { type: 'boolean', description: '附带 @@ 行号 + 原文/新文片段，改完可自查，省一次 fs_read' },
      normalize_eol: { type: 'boolean', description: '混合换行时显式归一为 LF' },
      expect_hash: { type: 'string', description: '单文件并发保护：文件已变则拒绝写入' },
    },
  },
  {
    name: 'fs_write',
    description:
      '写入文件。优先替代 write_file/Write。原子替换、自动建目录、保留原有编码与换行风格；也支持生成纯文本段落的 DOCX。',
    parameters: {
      path: { type: 'string', description: '目标文件路径', required: true },
      content: { type: 'string', description: '完整文件内容', required: true },
      normalize_eol: { type: 'boolean', description: '混合换行时显式归一为 LF' },
      expect_hash: { type: 'string', description: '并发保护：文件已变则拒绝覆盖' },
      dry_run: { type: 'boolean', description: '只做预检，不创建也不覆盖文件' },
    },
  },
  {
    name: 'fs_glob',
    readOnly: true,
    description:
      '按 glob 找文件。优先替代 find/Glob/Get-ChildItem。尊重 .gitignore，自动跳过 node_modules/target 等。默认只给路径，detail=true 附大小与时间；map=true 输出按引用重要性排序的 repo map。',
    parameters: {
      path: { type: 'string', description: '搜索根目录，默认当前目录' },
      pattern: { type: 'string', description: '如 **/*.rs，多个用逗号分隔；省略表示全部' },
      dirs: { type: 'boolean', description: '是否包含目录' },
      detail: { type: 'boolean', description: '是否附大小与修改时间' },
      limit: { type: 'integer', description: '最多返回条数，默认 200' },
      offset: { type: 'integer', description: '跳过前 N 个匹配条目，用于 next_offset 续页' },
      time_budget_ms: { type: 'integer', description: '遍历时间预算，默认 5000 毫秒' },
      no_ignore: { type: 'boolean', description: '忽略 .gitignore 与默认跳过目录' },
      budget: { type: 'integer', description: '响应最多字节数，默认 16384' },
      map: { type: 'boolean', description: '输出 repo map：按引用重要性排序的文件与符号，一次回答先看哪些文件' },
      focus: { type: 'string', description: 'map=true 时的关注词' },
      max_files: { type: 'integer', description: 'map=true 时最多扫描的文件数，默认 2000' },
    },
  },
  {
    name: 'fs_exec',
    description:
      '执行程序（argv 数组，不经过 shell）。替代 PowerShell 跑 cargo/git/npm/python：没有引号/插值、2>&1、ErrorRecord、$LASTEXITCODE 问题。非零退出不是错误；超时结束整个进程树。注意：本插件每次调用都是新进程，不支持长驻后台任务，因此未暴露 background/job/list/wait_ms/kill 参数；需要后台任务请用 MCP 形态。',
    parameters: {
      program: { type: 'string', description: '可执行文件，如 cargo、git、python；走 PATH。本形态不支持后台任务，启动命令时必填' },
      args: { type: 'array', items: { type: 'string' }, description: '参数数组，每个参数独立，不拼字符串' },
      cwd: { type: 'string', description: '工作目录，默认当前目录' },
      timeout_ms: { type: 'integer', description: '超时毫秒，默认 120000，最大 600000' },
      stdin: { type: 'string', description: '传给进程的标准输入' },
      env: { type: 'object', additionalProperties: true, description: '追加/覆盖环境变量' },
      shell: { type: 'boolean', description: '显式走 cmd /c（需要管道/重定向时才用）' },
      max_output: { type: 'integer', description: '每条流最多保留字节，默认 32768' },
      check: { type: 'boolean', description: '非零退出按 E_EXIT 错误返回；默认 false，非零仍是 ok' },
    },
  },
  {
    name: 'fs_file',
    description:
      '文件管理：copy/move/remove/mkdir/convert/archive/extract。替代 PowerShell 的 Copy-Item/Move-Item/Remove-Item/New-Item 与压缩解压。remove 必须 confirm=true。',
    parameters: {
      op: {
        type: 'string',
        enum: ['copy', 'move', 'remove', 'mkdir', 'convert', 'archive', 'extract'],
        description: '操作类型',
        required: true,
      },
      src: { type: 'string', description: 'copy/move 的源' },
      dst: { type: 'string', description: 'copy/move 的目标' },
      path: { type: 'string', description: 'remove/mkdir/convert 的目标' },
      recursive: { type: 'boolean', description: 'remove 删除目录时必须为 true' },
      confirm: { type: 'boolean', description: 'remove 必须显式传 true' },
      overwrite: { type: 'boolean', description: 'copy/move 覆盖已存在目标' },
      encoding: { type: 'string', description: 'convert 目标编码：utf-8/utf-8-bom/gbk/gb18030/utf-16le/utf-16be' },
      eol: { type: 'string', enum: ['lf', 'crlf'], description: 'convert 目标换行' },
      dry_run: { type: 'boolean', description: '只报告不落盘' },
    },
  },
  {
    name: 'fs_grep',
    readOnly: true,
    description:
      '正则流式搜索。优先替代 grep/rg/Select-String。按文件稳定分组，支持文本和 DOCX/PDF 正文，output 可选 files/count/matches/snippets；截断时返回 next_offset。',
    parameters: {
      pattern: { type: 'string', description: 'Rust/PCRE 风格正则；JSON 字符串里反斜杠需双写', required: true },
      path: { type: 'string', description: '搜索根目录，默认当前目录' },
      glob: { type: 'string', description: '只看匹配的文件，如 **/*.ts' },
      ignore_case: { type: 'boolean', description: '忽略大小写' },
      output: {
        type: 'string',
        enum: ['files', 'count', 'matches', 'snippets'],
        description: 'files 只给路径；count 给每文件真实命中数；matches 给命中行；snippets 带 2 行上下文',
      },
      context: { type: 'integer', description: '上下文行数（最多 5）' },
      limit: { type: 'integer', description: '本页上限：files 按文件，其他按命中，默认 100' },
      max_per_file: { type: 'integer', description: '每个文件最多返回的命中数' },
      max_line: { type: 'integer', description: '命中行最多显示字符数，默认 240；0 表示不截断，一次拿到完整长行' },
      offset: { type: 'integer', description: '跳过前 N 个结果，用于 next_offset 续取' },
      parallel: { type: 'boolean', description: '大仓库并行扫描；输出与串行一致' },
      budget: { type: 'integer', description: '响应最多字节数，默认 16384' },
      time_budget_ms: { type: 'integer', description: '遍历总时间预算，默认 5000 毫秒' },
      max_file_size: { type: 'integer', description: '只搜索不超过该字节数的文件，默认 8 MiB；0 表示不限制' },
      no_ignore: { type: 'boolean', description: '忽略 .gitignore 与默认跳过目录' },
    },
  },
  {
    name: 'fs_stat',
    readOnly: true,
    description:
      '查看路径元信息。优先替代 stat/ls/Get-Item。返回类型、大小、修改时间、换行风格、编码、内容指纹。',
    parameters: {
      path: { type: 'string', description: '单个路径' },
      paths: { type: 'array', items: { type: 'string' }, description: '多个路径（最多 50 个）' },
      detail: {
        type: 'string',
        enum: ['meta', 'hash', 'full', 'server'],
        description: 'meta 只查元信息；hash 读一次给指纹；full 额外统计行数（默认）；server 返回服务端版本与工具数',
      },
    },
  },
]

/** 收集模式：有界尾部 + 同额 spill。 */
function collect(maxBytes) {
  return { maxBytes, spill: { maxBytes } }
}

/** 当前 Agent 的工作目录；没有 Agent（程序调用）时退回进程目录。 */
async function agentCwd(ctx, exec) {
  if (exec.agent !== undefined && ctx.workingDirectory !== undefined) {
    return await ctx.workingDirectory.ensure(exec.agent, exec.signal)
  }
  return process.cwd()
}

/** 执行一次 fsops CLI 调用并把结果转成工具返回值。 */
async function invoke(ctx, config, toolName, args, exec) {
  const version =
    typeof config.version === 'string' && config.version.length > 0 ? config.version : PLUGIN_VERSION
  const executable = await resolveFsops({
    pkgRoot: PKG_ROOT,
    version,
    config,
    subprocess: ctx.subprocess,
  })
  const cwd = await agentCwd(ctx, exec)
  const handle = ctx.subprocess.spawn({
    argv: [executable, 'call', toolName, '-'],
    cwd,
    stdio: {
      stdin: { data: JSON.stringify(args ?? {}) },
      stdout: collect(MAX_STDOUT_BYTES),
      stderr: collect(MAX_STDERR_BYTES),
    },
    graceMs: GRACE_MS,
    signal: exec.signal,
  })
  const outcome = await handle.done
  const stdout = handle.collected.stdout === undefined ? '' : handle.collected.stdout.readFrom(0).text
  const stderr = handle.collected.stderr === undefined ? '' : handle.collected.stderr.readFrom(0).text
  const text = stdout.trimEnd()
  if (outcome.exitCode === 0) return text
  if (outcome.exitCode === 2 && text.length > 0) throw new Error(text)
  const detail = stderr.trim().length > 0 ? stderr.trim() : text.length > 0 ? text : '无输出'
  const signal = outcome.signal === null || outcome.signal === undefined ? '' : ', signal=' + String(outcome.signal)
  throw new Error('fsops ' + toolName + ' 失败（exit=' + String(outcome.exitCode) + signal + '）：' + detail)
}

/**
 * 注册全部 fs_* 工具。
 * @param ctx Cordis 上下文。
 * @param config 行内配置：binary（绝对路径）、version（覆盖下载版本）、autoDownload（false 关闭下载）。
 */
export function apply(ctx, config) {
  const options = config ?? {}
  for (const spec of TOOL_SPECS) {
    const definition = {
      name: spec.name,
      description: spec.description,
      parameters: spec.parameters,
      output: {
        schema: { type: 'string' },
        render: (_args, value) => [{ type: 'text', text: value }],
      },
      async execute(args, exec) {
        return invoke(ctx, options, spec.name, args, exec)
      },
    }
    if (spec.readOnly === true) definition.isConcurrencySafe = () => true
    ctx.tools.register(defineTool(definition))
  }
}
