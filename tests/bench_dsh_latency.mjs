/**
 * DSH 插件执行延迟基准。
 *
 * 复现插件的真实调用路径，对比两种传输：
 *   A. 每次 `fsops call <tool> -`（改造前的做法）
 *   B. 复用一条长驻 `fsops mcp` 会话（改造后的做法）
 *
 * 用法：node tests/bench_dsh_latency.mjs [--exe target/release/fsops.exe] [--n 30]
 *
 * 只测工具调用本身，不含模型推理时间；数字为客户端观测到的墙钟时间。
 */
import { spawn } from 'node:child_process'
import { existsSync } from 'node:fs'
import { mkdtemp, writeFile, rm } from 'node:fs/promises'
import { registerHooks } from 'node:module'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { pathToFileURL } from 'node:url'

const ROOT = resolve(dirname(new URL(import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, '$1')), '..')
const argv = process.argv.slice(2)
const flag = (name, fallback) => {
  const index = argv.indexOf('--' + name)
  return index >= 0 ? argv[index + 1] : fallback
}
const EXE = resolve(flag('exe', join(ROOT, 'target', 'release', 'fsops.exe')))
const N = Number(flag('n', '30'))
if (!existsSync(EXE)) {
  console.error('找不到 ' + EXE + '，先 cargo build --release 或传 --exe')
  process.exit(1)
}

const stubDir = await mkdtemp(join(tmpdir(), 'fsops-bench-stub-'))
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

const work = await mkdtemp(join(tmpdir(), 'fsops-bench-'))
const sample = join(work, 'sample.rs')
await writeFile(sample, 'fn main() {\n    let x = 1;\n}\n'.repeat(40), 'utf8')

function makeContext(cwd) {
  const registered = new Map()
  const disposers = []
  return {
    registered,
    disposers,
    tools: { register: (definition) => registered.set(definition.name, definition) },
    workingDirectory: { async ensure() { return cwd } },
    on(event, handler) {
      if (event === 'dispose') disposers.push(handler)
    },
    subprocess: {
      async resolveExecutable(path) {
        if (!existsSync(path)) throw new Error('missing: ' + path)
        return path
      },
      spawn(spec) {
        const child = spawn(spec.argv[0], spec.argv.slice(1), {
          cwd: spec.cwd,
          windowsHide: true,
          stdio: ['pipe', 'pipe', 'pipe'],
        })
        const done = new Promise((settle) => {
          child.on('close', (exitCode, signal) => settle({ exitCode, signal }))
        })
        const stdio = spec.stdio ?? {}
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
        child.stdout.on('data', (chunk) => (out += chunk))
        child.stderr.on('data', (chunk) => (err += chunk))
        const input = stdio.stdin?.data
        child.stdin.end(input ?? '')
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

const plugin = await import(pathToFileURL(join(ROOT, 'dsh', 'index.js')).href)
const ctx = makeContext(work)
plugin.apply(ctx, { binary: EXE })
const execute = (name) => ctx.registered.get(name).execute

/** 改造前的做法：每次调用新起一个进程。 */
function oneShot(toolName, args) {
  return new Promise((resolvePromise, rejectPromise) => {
    const started = performance.now()
    const child = spawn(EXE, ['call', toolName, '-'], {
      cwd: work,
      windowsHide: true,
      stdio: ['pipe', 'pipe', 'pipe'],
    })
    let out = ''
    child.stdout.on('data', (chunk) => (out += chunk))
    child.stderr.on('data', () => {})
    child.on('error', rejectPromise)
    child.on('close', () => resolvePromise({ ms: performance.now() - started, text: out }))
    child.stdin.end(JSON.stringify(args))
  })
}

function stats(times) {
  const sorted = [...times].sort((a, b) => a - b)
  const at = (q) => sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * q))]
  return { min: sorted[0], med: at(0.5), p95: at(0.95), max: sorted[sorted.length - 1] }
}

async function bench(label, fn) {
  // 预热：首调用包含 spawn/握手/缓存，单独算。
  const first = performance.now()
  await fn()
  const coldMs = performance.now() - first
  const times = []
  for (let i = 0; i < N; i++) {
    const started = performance.now()
    await fn()
    times.push(performance.now() - started)
  }
  const s = stats(times)
  console.log(
    label.padEnd(30) +
      'min=' + s.min.toFixed(2).padStart(7) +
      '  med=' + s.med.toFixed(2).padStart(7) +
      '  p95=' + s.p95.toFixed(2).padStart(7) +
      '  max=' + s.max.toFixed(2).padStart(7) +
      '  | 首调用(含冷启动)=' + coldMs.toFixed(1) + 'ms',
  )
  return { ...s, cold: coldMs }
}

console.log('可执行文件: ' + EXE)
console.log('样本: 每组 ' + N + ' 次，单位 ms\n')

const readArgs = { path: sample, start: 1, end: 5 }
const statArgs = { path: sample, detail: 'meta' }
const grepArgs = { pattern: 'let x', path: work, output: 'count' }

console.log('== A. 每次新起进程（改造前）==')
const aRead = await bench('fs_read', () => oneShot('fs_read', readArgs))
const aStat = await bench('fs_stat', () => oneShot('fs_stat', statArgs))

console.log('\n== B. 复用长驻 MCP 会话（改造后）==')
const bRead = await bench('fs_read', () => execute('fs_read')(readArgs, { agent: {}, signal: undefined }))
const bStat = await bench('fs_stat', () => execute('fs_stat')(statArgs, { agent: {}, signal: undefined }))
const bGrep = await bench('fs_grep', () => execute('fs_grep')(grepArgs, { agent: {}, signal: undefined }))

// 并发：4 个只读调用同时发出，长驻会话下应由 4 个工作线程真并行。
const parallel = []
for (let i = 0; i < 10; i++) {
  const started = performance.now()
  await Promise.all(
    [1, 2, 3, 4].map(() => execute('fs_stat')(statArgs, { agent: {}, signal: undefined })),
  )
  parallel.push(performance.now() - started)
}
const p = stats(parallel)
console.log(
  '4 路并发（每轮总耗时）'.padEnd(30) +
    'min=' + p.min.toFixed(2).padStart(7) +
    '  med=' + p.med.toFixed(2).padStart(7) +
    '  p95=' + p.p95.toFixed(2).padStart(7) +
    '  max=' + p.max.toFixed(2).padStart(7),
)

console.log('\n== 对比 ==')
const speed = (a, b) => ((a.med / b.med).toFixed(1) + '×')
console.log('fs_read : ' + aRead.med.toFixed(2) + ' → ' + bRead.med.toFixed(2) + ' ms  ' + speed(aRead, bRead))
console.log('fs_stat : ' + aStat.med.toFixed(2) + ' → ' + bStat.med.toFixed(2) + ' ms  ' + speed(aStat, bStat))
console.log('单次省下: 约 ' + (aRead.med - bRead.med).toFixed(1) + ' ms/次')

// 关掉长驻会话，否则它占着 cwd 导致临时目录删不掉。
for (const dispose of ctx.disposers) dispose()
await new Promise((settle) => setTimeout(settle, 100))
for (let attempt = 0; attempt < 5; attempt++) {
  try {
    await rm(work, { recursive: true, force: true })
    break
  } catch {
    await new Promise((settle) => setTimeout(settle, 100))
  }
}
await rm(stubDir, { recursive: true, force: true })
process.exit(0)
