/**
 * 从 `fsops schema` 生成文档里的工具清单与数量。
 *
 * 为什么要有这个脚本：工具清单曾散落在 6 处（README 两处、使用指南、工具参考、
 * package.json、CONTRIBUTING），加一个工具就要手改 6 个地方——v0.9.0 加 fs_inspect
 * 时就有 4 处漏改，文档说「8 个工具」而实际是 9 个。这类漂移人肉审不出来，
 * 只能靠生成 + CI 红灯。
 *
 * 分工：
 *   - 机械事实（工具名、参数名、数量）→ 从 `fsops schema` 生成，不可能说谎；
 *   - 散文（每个工具的「用途」一句话）→ 在 TOOL_SUMMARY 里手写，缺一个就报错，
 *     强制新增工具时补上（而不是留给读者发现）。
 *
 * 用法：
 *   node scripts/gen_docs_tools.mjs                # 生成/更新
 *   node scripts/gen_docs_tools.mjs --check        # 只校验（CI 用）
 *   node scripts/gen_docs_tools.mjs --exe target/release/fsops.exe
 */
import { execFileSync } from 'node:child_process'
import { existsSync, readFileSync, writeFileSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..')

/** 每个工具的一句话用途。新增工具必须在这里补一行，否则脚本报错。 */
const TOOL_SUMMARY = {
  fs_read: '读文件、行区间、多文件、多文件异构区间、按模式取块、大文件窗口、结构摘要、DOCX/PDF 正文',
  fs_edit: '精确替换、正则替换、diff 自查、多文件批量、并发保护',
  fs_write: '新建或整体覆盖，原子写入，可生成 DOCX；多文件用 files 一次提交',
  fs_glob: '按 glob 找文件，尊重 .gitignore，带遍历时间预算，可输出 repo map',
  fs_inspect: '一次看清目录：文件规模分布 + 可选正则的命中分布与样例，替代「glob → stat → grep」多轮探索',
  fs_grep: '正则搜索、四种输出、分页、并行，支持 DOCX/PDF 正文；首行带命中分布摘要',
  fs_stat: '元信息、编码、换行、hash；detail=server 一条调用自检服务端版本与工具数',
  fs_exec: '执行程序（argv 数组，不经过 shell），支持后台任务；替代 PowerShell 跑 cargo/git/npm',
  fs_file: '文件管理：copy/move/remove/mkdir/convert/archive/extract',
}

const MARK_BEGIN = '<!-- tools:begin -->'
const MARK_END = '<!-- tools:end -->'

/** 生成 Markdown 工具表（含数量行）。 */
export function renderToolSection(defs) {
  const lines = []
  lines.push(MARK_BEGIN)
  lines.push('')
  lines.push(defs.length + ' 个工具，故意做窄。')
  lines.push('')
  lines.push('| 工具 | 用途 | 参数（MCP 形态；DSH 插件形态不含 fs_exec 的后台任务参数） |')
  lines.push('|---|---|---|')
  for (const def of defs) {
    const name = def.name
    const summary = TOOL_SUMMARY[name]
    if (summary === undefined) {
      throw new Error(
        'TOOL_SUMMARY 缺少 ' + name + ' 的一句话用途；新增工具必须补上，' +
          '否则文档会漏掉它（这正是本脚本要防的漂移）',
      )
    }
    const params = Object.keys(def.inputSchema?.properties ?? {}).map((k) => '`' + k + '`')
    lines.push('| `' + name + '` | ' + summary + ' | ' + (params.join('、') || '—') + ' |')
  }
  lines.push('')
  lines.push(MARK_END)
  return lines.join('\n')
}

/** 把文件里两处标记之间替换成生成内容。 */
function replaceSection(text, section, file) {
  const begin = text.indexOf(MARK_BEGIN)
  const end = text.indexOf(MARK_END)
  if (begin < 0 || end < 0) {
    throw new Error(file + ' 缺少 ' + MARK_BEGIN + ' / ' + MARK_END + ' 标记')
  }
  return text.slice(0, begin) + section + text.slice(end + MARK_END.length)
}

/** 把「N 个工具」这类数量声明同步成真实数量。 */
function syncCounts(text, count) {
  return text
    .replace(/(暴露)\s+\d+(\s+个工具)/g, (_, a, b) => a + ' ' + count + b)
    .replace(/(提供)\s+\d+(\s+个\s*`fs_\*`\s*工具)/g, (_, a, b) => a + ' ' + count + b)
    // 文档里的自检示例也带工具数。release.py 的 prepare 只同步版本号，
    // 不同步这里——曾出现 `tools=8` 与实际 9 个不符。
    // 三种写法都覆盖：具体版本、x.y.z 占位、<版本> 占位。
    .replace(/(`fsops )(\d+\.\d+\.\d+|x\.y\.z|<版本>)( tools=)\d+(`)/g,
      (_, a, version, b, c) => a + version + b + count + c)
}

/** package.json 的 description 里那句工具清单。 */
function syncPackageDescription(text, defs) {
  const names = defs.map((d) => d.name).join(' / ')
  return text.replace(
    /(以 DSH 插件形态提供\s+)[\w\s/]+?(。)/,
    (_, head, tail) => head + names + tail,
  )
}

function parseArgs(argv) {
  const options = { exe: undefined, check: false }
  for (let i = 0; i < argv.length; i += 1) {
    if (argv[i] === '--check') options.check = true
    else if (argv[i] === '--exe') {
      options.exe = argv[i + 1]
      i += 1
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
  ].filter((v) => typeof v === 'string' && v.length > 0)
  for (const candidate of candidates) if (existsSync(candidate)) return candidate
  throw new Error('找不到 fsops.exe，先 cargo build --release 或用 --exe 指定')
}

function main() {
  const options = parseArgs(process.argv.slice(2))
  const exe = resolveExe(options.exe)
  const out = execFileSync(exe, ['schema'], { encoding: 'utf8', maxBuffer: 32 * 1024 * 1024 })
  const defs = JSON.parse(out)
  const section = renderToolSection(defs)
  const count = defs.length

  const targets = [
    { path: join(ROOT, 'README.md'), section: true, counts: true, pkg: false },
    { path: join(ROOT, 'docs', '工具参考.md'), section: false, counts: true, pkg: false },
    { path: join(ROOT, 'docs', '使用指南.md'), section: false, counts: true, pkg: false },
    { path: join(ROOT, 'package.json'), section: false, counts: false, pkg: true },
  ]

  const drift = []
  for (const target of targets) {
    const before = readFileSync(target.path, 'utf8')
    let after = before
    if (target.section) after = replaceSection(after, section, target.path)
    if (target.counts) after = syncCounts(after, count)
    if (target.pkg) after = syncPackageDescription(after, defs)
    if (after !== before) {
      drift.push(target.path)
      if (!options.check) writeFileSync(target.path, after, 'utf8')
    }
  }

  if (options.check) {
    if (drift.length > 0) {
      console.error('E_DRIFT 以下文件与 fsops schema 不一致，请运行 node scripts/gen_docs_tools.mjs：')
      for (const p of drift) console.error('  - ' + p)
      process.exit(2)
    }
    console.log('ok 文档工具清单与 fsops schema 一致（' + count + ' 个工具）')
    return
  }
  if (drift.length > 0) {
    for (const p of drift) console.log('已更新 ' + p)
  } else {
    console.log('无需改动')
  }
  console.log('ok 文档工具清单与 fsops schema 一致（' + count + ' 个工具）')
}

if (process.argv[1] !== undefined && import.meta.url === new URL('file://' + process.argv[1].replace(/\\/g, '/')).href) {
  main()
}
