/**
 * fsops 的 DSH Host 插件：把 fs_* 工具注册到当前 profile。
 *
 * 不重复实现文件语义——通过一条长驻的 `fsops mcp` 子进程（换行分隔 JSON-RPC）
 * 把调用交给同一份 Rust 内核，因此错误码、预算、续读提示和输出格式与 fsops 的
 * MCP / CLI 完全一致。
 *
 * 为什么是长驻而不是每次 `fsops call`：实测单次调用约 33ms，其中工具本身 <0.5ms，
 * 其余是 Windows 进程创建（~9ms）、隐藏 console 分配（~17ms）和镜像加载（~6ms）。
 * 复用同一个进程后单次调用降到亚毫秒；`fsops mcp` 自带 4 个工作线程，
 * 并发只读调用也是真并行。会话不可用时仍退回一次性调用，功能不降级。
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
/** 长驻会话按 cwd 缓存；上限避免长命 Host 切换目录时进程无界累积（LRU 回收）。 */
const MAX_SESSIONS = 8
/** 与 fsops mcp 的 initialize 应答对齐（它原样回显请求的版本）。 */
const MCP_PROTOCOL_VERSION = '2025-06-18'

/**
 * 工具定义由 scripts/gen_dsh_specs.mjs 从 `fsops schema` 生成，
 * 保证描述与参数只有 Rust 一份真源（手写三份时漂移过 11 处）。
 */
import { EXCLUDED } from './excluded.js'
import { TOOL_SPECS } from './tool-specs.generated.js'

/** 只读工具：取消时不必杀会话（没有副作用），也允许并发调用。 */
const READ_ONLY_TOOLS = new Set(TOOL_SPECS.filter((spec) => spec.readOnly === true).map((spec) => spec.name))

/**
 * 插件形态不接受的参数：与生成器共用 dsh/excluded.js，避免两处清单各写一份。
 * DSH 的顶层参数校验放行 schema 外的键，如果不在传输层显式拒绝，就会出现
 * 「schema 说没有、文档说没有、运行时却能穿透」的第三种行为。
 */
const EXCLUDED_ARGS = EXCLUDED

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

/** 执行一次性的 `fsops call` 调用（长驻会话不可用时的退路）。 */
async function invokeOneShot(ctx, executable, cwd, toolName, args, exec) {
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

/** 会话级失败：上层据此退回一次性调用；工具自身的 E_ 错误不能走这条路。 */
function sessionError(message) {
  const error = new Error(message)
  error.fsopsSession = true
  return error
}

function messageOf(value) {
  if (value instanceof Error) return value.message
  if (value !== null && typeof value === 'object' && typeof value.message === 'string') return value.message
  return String(value)
}

/** 取消错误打标记：传输层据此决定是否结束内核会话，避免取消后继续写。 */
function abortedError(signal) {
  const reason = signal === undefined ? undefined : signal.reason
  const detail = reason instanceof Error ? reason.message : ''
  const error = new Error(detail.length > 0 && detail !== '已取消' ? '已取消：' + detail : '已取消')
  error.aborted = true
  return error
}

/**
 * 开一条长驻 `fsops mcp` 会话，并把 stdout 的行流按 id 分发到各自的 Promise。
 *
 * 行帧格式由 src/mcp.rs 固定：一行一个 JSON-RPC 响应，工具调用可由
 * 4 个工作线程乱序应答，所以必须按 id 匹配而不是按顺序取。
 */
function startSession(ctx, executable, cwd) {
  const handle = ctx.subprocess.spawn({
    argv: [executable, 'mcp'],
    cwd,
    stdio: { stdin: 'pipe', stdout: 'pipe', stderr: 'pipe' },
    graceMs: GRACE_MS,
  })
  const stdin = handle.stdin
  const stdout = handle.stdout
  if (stdin === undefined || stdout === undefined) {
    handle.terminate?.()
    throw sessionError('subprocess 未提供 stdin/stdout 管道，无法复用长驻会话')
  }
  let buffer = ''
  let stderrTail = ''
  let exited = false
  let nextId = 0
  const pending = new Map()

  const failPending = (error) => {
    for (const waiter of pending.values()) waiter.reject(error)
    pending.clear()
  }

  stdout.setEncoding?.('utf8')
  stdout.on('data', (chunk) => {
    buffer += chunk
    let index = buffer.indexOf('\n')
    while (index >= 0) {
      const line = buffer.slice(0, index)
      buffer = buffer.slice(index + 1)
      index = buffer.indexOf('\n')
      if (line.trim().length === 0) continue
      let message
      try {
        message = JSON.parse(line)
      } catch {
        continue
      }
      const waiter = pending.get(message.id)
      if (waiter === undefined) continue
      pending.delete(message.id)
      waiter.resolve(message)
    }
  })
  if (handle.stderr !== undefined) {
    handle.stderr.setEncoding?.('utf8')
    // stderr 必须持续排空：管道写满会把子进程卡死。
    handle.stderr.on('data', (chunk) => {
      stderrTail = (stderrTail + chunk).slice(-4096)
    })
  }
  handle.done.then(
    (outcome) => {
      exited = true
      const tail = stderrTail.trim()
      failPending(
        sessionError(
          'fsops mcp 已退出（exit=' + String(outcome.exitCode) + '）' + (tail.length > 0 ? '：' + tail : ''),
        ),
      )
    },
    (error) => {
      exited = true
      failPending(sessionError('fsops mcp 启动失败：' + messageOf(error)))
    },
  )

  const request = (method, params, signal) =>
    new Promise((resolve, reject) => {
      if (exited) {
        reject(sessionError('fsops mcp 会话已退出'))
        return
      }
      if (signal !== undefined && signal.aborted) {
        reject(abortedError(signal))
        return
      }
      const id = ++nextId
      const onAbort = () => {
        pending.delete(id)
        reject(abortedError(signal))
      }
      pending.set(id, {
        resolve: (message) => {
          signal?.removeEventListener?.('abort', onAbort)
          resolve(message)
        },
        reject: (error) => {
          signal?.removeEventListener?.('abort', onAbort)
          reject(error)
        },
      })
      signal?.addEventListener?.('abort', onAbort, { once: true })
      try {
        stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n')
      } catch (error) {
        pending.delete(id)
        reject(sessionError('向 fsops mcp 写入失败：' + messageOf(error)))
      }
    })

  const ready = request('initialize', {
    protocolVersion: MCP_PROTOCOL_VERSION,
    capabilities: {},
    clientInfo: { name: 'dsh-fsops-plugin', version: PLUGIN_VERSION },
  }).then((message) => {
    if (message.error !== undefined) {
      throw sessionError('fsops mcp 初始化失败：' + messageOf(message.error))
    }
  })

  return {
    ready,
    request,
    isExited: () => exited,
    /** 未完成请求数：取消时只有在没有其他在途调用时才敢结束会话。 */
    pendingCount: () => pending.size,
    /**
     * 会话里是否还有后台任务在跑。
     * 后台任务已是插件形态的公开能力，回收会话前必须先问一句，
     * 否则会静默杀掉用户正在等的构建/测试。
     */
    async hasRunningJobs() {
      if (exited) return false
      try {
        const response = await request('tools/call', {
          name: 'fs_exec',
          arguments: { list: true },
        })
        const text = response?.result?.content?.[0]?.text ?? ''
        return /\brunning\b/.test(text)
      } catch {
        // 查不出来时不阻止回收，避免会话泄漏。
        return false
      }
    },
    dispose() {
      exited = true
      failPending(sessionError('fsops mcp 会话已关闭'))
      handle.terminate?.()
    },
  }
}

/** 一次插件实例的传输层：缓存可执行文件、复用长驻会话、提供一次性退路。 */
function createTransport(ctx, options) {
  const sessions = new Map()
  let executablePromise

  // 解析一次就够：每次调用重走候选路径探测纯属白花时间。
  const executableOnce = () => {
    if (executablePromise === undefined) {
      const version =
        typeof options.version === 'string' && options.version.length > 0
          ? options.version
          : PLUGIN_VERSION
      executablePromise = resolveFsops({
        pkgRoot: PKG_ROOT,
        version,
        config: options,
        subprocess: ctx.subprocess,
      }).catch((error) => {
        executablePromise = undefined
        throw error
      })
    }
    return executablePromise
  }

  const sessionFor = async (executable, cwd) => {
    const existing = sessions.get(cwd)
    if (existing !== undefined && !existing.isExited()) {
      await existing.ready
      // Map 的插入顺序即 LRU 顺序：命中的会话挪到末尾，回收时淘汰最久未用。
      sessions.delete(cwd)
      sessions.set(cwd, existing)
      return existing
    }
    sessions.delete(cwd)
    if (sessions.size >= MAX_SESSIONS) {
      // 淘汰最久未用的会话，但跳过仍有后台任务在跑的：后台任务已经是对外能力，
      // dispose 会连带杀掉进程里的所有 job，表现为「任务莫名消失」。
      for (const key of sessions.keys()) {
        const candidate = sessions.get(key)
        if (candidate === undefined) continue
        if (await candidate.hasRunningJobs()) continue
        candidate.dispose()
        sessions.delete(key)
        break
      }
    }
    const fresh = startSession(ctx, executable, cwd)
    sessions.set(cwd, fresh)
    try {
      await fresh.ready
    } catch (error) {
      sessions.delete(cwd)
      fresh.dispose()
      throw error
    }
    return fresh
  }

  const callOnce = async (toolName, args, exec) => {
    const executable = await executableOnce()
    const cwd = await agentCwd(ctx, exec)
    return invokeOneShot(ctx, executable, cwd, toolName, args, exec)
  }

  const callRpc = async (toolName, args, exec) => {
    const executable = await executableOnce()
    const cwd = await agentCwd(ctx, exec)
    const session = await sessionFor(executable, cwd)
    let response
    try {
      response = await session.request(
        'tools/call',
        { name: toolName, arguments: args ?? {} },
        exec.signal,
      )
    } catch (error) {
      // 用户取消：如果这是唯一在途调用且属于写/执行类工具，结束内核会话，
      // 避免“取消后 fsops 继续写盘/跑命令”。下一次调用会自动重建会话。
      // 有并发在途调用时不杀会话：避免它们的失败触发一次性 spawn 重放（可能重复写）。
      const aborted = error !== null && typeof error === 'object' && error.aborted === true
      if (aborted && !session.isExited() && session.pendingCount() === 0 && !READ_ONLY_TOOLS.has(toolName)) {
        sessions.delete(cwd)
        session.dispose()
      }
      throw error
    }
    if (response.error !== undefined) {
      // 协议层错误（未知方法/参数）不是会话故障，直接抛给模型。
      throw new Error('fsops ' + toolName + ' 失败：' + messageOf(response.error))
    }
    const content = response.result === undefined ? undefined : response.result.content
    const first = Array.isArray(content) && content.length > 0 ? content[0] : undefined
    const text = first !== undefined && typeof first.text === 'string' ? first.text : ''
    // isError 与 CLI 退出码 2 同源（首行 E_ 或正文里夹着 E_ 行），行为保持一致。
    if (response.result !== undefined && response.result.isError === true) {
      throw new Error(text.trimEnd().length > 0 ? text.trimEnd() : 'fsops ' + toolName + ' 失败')
    }
    return text.trimEnd()
  }

  return {
    /** 优先长驻会话；会话级故障退回一次性 spawn，保证工具始终可用。 */
    async call(toolName, args, exec) {
      if (options.mcp === false) return callOnce(toolName, args, exec)
      try {
        return await callRpc(toolName, args, exec)
      } catch (error) {
        if (error === null || typeof error !== 'object' || error.fsopsSession !== true) throw error
        return callOnce(toolName, args, exec)
      }
    },
    dispose() {
      for (const session of sessions.values()) session.dispose()
      sessions.clear()
    },
  }
}

/**
 * 注册全部 fs_* 工具。
 * @param ctx Cordis 上下文。
 * @param config 行内配置：binary（绝对路径）、version（覆盖下载版本）、autoDownload（false 关闭下载）。
 */
export function apply(ctx, config) {
  const options = config ?? {}
  const transport = createTransport(ctx, options)
  // 服务释放会杀子进程；这里显式关会话，让 pending 请求立即失败而不是干等。
  if (typeof ctx.on === 'function') ctx.on('dispose', () => transport.dispose())
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
        const excluded = EXCLUDED_ARGS[spec.name]
        if (excluded !== undefined) {
          for (const key of excluded) {
            if (args !== null && typeof args === 'object' && Object.hasOwn(args, key)) {
              throw new Error(
                'E_BAD_ARGS ' + spec.name + ' 在 DSH 插件形态不支持 ' + key +
                  '（后台任务生命周期绑在长驻会话，插件重载会丢）；需要后台任务请用 MCP 形态 `fsops mcp`。',
              )
            }
          }
        }
        return transport.call(spec.name, args, exec)
      },
    }
    if (spec.readOnly === true) definition.isConcurrencySafe = () => true
    ctx.tools.register(defineTool(definition))
  }
}
