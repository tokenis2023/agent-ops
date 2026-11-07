/**
 * fsops 二进制的定位与按需下载。
 *
 * 插件本身不含 exe（仓库不提交构建产物），按以下顺序取：
 *   1. 行内 config.binary（绝对路径）
 *   2. 环境变量 FSOPS_BIN
 *   3. 包内 bin/fsops.exe（用户自行放入时）
 *   4. %LOCALAPPDATA%\\fsops\\fsops.exe（官方 install.ps1 的默认安装位置）
 *   5. PATH 中的 fsops / fsops.exe
 *   6. %LOCALAPPDATA%\\fsops-dsh\\<version>\\fsops.exe（本插件自己的缓存）
 *   7. 从 GitHub Release 下载并校验 SHA-256；该版本没有 Release 时回退到 latest
 */
import { createHash } from 'node:crypto'
import { existsSync } from 'node:fs'
import { mkdir, rename, rm, writeFile } from 'node:fs/promises'
import { homedir } from 'node:os'
import { dirname, join } from 'node:path'

/** release.py 所在的仓库。 */
export const REPO = 'tokenis2023/agent-ops'

/** Release 资产名，与 scripts/release.py 一致。 */
export function assetName(version) {
  return 'fsops-v' + version + '-windows-x86_64.exe'
}

/** Release 下载地址。 */
export function assetUrl(version) {
  return 'https://github.com/' + REPO + '/releases/download/v' + version + '/' + assetName(version)
}

function localAppData() {
  return process.env.LOCALAPPDATA || join(homedir(), 'AppData', 'Local')
}

/** 插件自己的缓存路径。 */
export function cachePath(version) {
  return join(localAppData(), 'fsops-dsh', version, 'fsops.exe')
}

/** 按优先级列出候选路径；不检查存在性。 */
export function candidatePaths({ pkgRoot, version, config }) {
  const exe = process.platform === 'win32' ? 'fsops.exe' : 'fsops'
  return [
    config?.binary,
    process.env.FSOPS_BIN,
    join(pkgRoot, 'bin', exe),
    join(localAppData(), 'fsops', exe),
    cachePath(version),
  ].filter((value) => typeof value === 'string' && value.length > 0)
}

/** GitHub latest Release 的版本号（去掉前缀 v）；失败时 undefined。 */
async function latestVersion() {
  try {
    const response = await fetch('https://api.github.com/repos/' + REPO + '/releases/latest', {
      headers: { accept: 'application/vnd.github+json', 'user-agent': 'fsops-dsh-plugin' },
      signal: AbortSignal.timeout(30_000),
    })
    if (!response.ok) return undefined
    const body = await response.json()
    const tag = typeof body.tag_name === 'string' ? body.tag_name : ''
    const version = tag.startsWith('v') ? tag.slice(1) : tag
    return version.length > 0 ? version : undefined
  } catch {
    return undefined
  }
}

/** 拉取一个版本的 exe 与 .sha256；404 返回 undefined，其余失败抛错。 */
async function fetchAsset(version) {
  const url = assetUrl(version)
  const response = await fetch(url, { redirect: 'follow', signal: AbortSignal.timeout(180_000) })
  if (response.status === 404) return undefined
  if (!response.ok) {
    throw new Error(
      'E_NETWORK 下载 ' + url + ' 失败：HTTP ' + response.status +
        '。可先运行 fsops 的 install.ps1，或设置 FSOPS_BIN 指向已有的 fsops.exe。',
    )
  }
  const bytes = Buffer.from(await response.arrayBuffer())
  let expected = ''
  try {
    const shaResponse = await fetch(url + '.sha256', {
      redirect: 'follow',
      signal: AbortSignal.timeout(30_000),
    })
    if (shaResponse.ok) expected = (await shaResponse.text()).trim().split(/\s+/)[0].toLowerCase()
  } catch {
    // 拿不到 .sha256 时仍继续，但调用方会知道没有校验过。
  }
  return { bytes, expected }
}

/** 下载并校验一个 Release 资产；返回实际使用的版本。 */
export async function downloadFsops(version) {
  let target = version
  let asset = await fetchAsset(target)
  if (asset === undefined) {
    const latest = await latestVersion()
    if (latest === undefined || latest === version) {
      throw new Error(
        'E_NOT_FOUND 找不到 fsops v' + version + ' 的 Release。请运行 install.ps1，或设置 FSOPS_BIN。',
      )
    }
    target = latest
    asset = await fetchAsset(target)
    if (asset === undefined) {
      throw new Error('E_NOT_FOUND 找不到 fsops 的 Release（v' + version + ' 与 latest 均不可用）')
    }
  }

  const actual = createHash('sha256').update(asset.bytes).digest('hex')
  if (asset.expected !== '' && asset.expected !== actual) {
    throw new Error('E_HASH fsops 下载内容校验失败：期望 ' + asset.expected + '，实际 ' + actual)
  }

  const dest = cachePath(target)
  await mkdir(dirname(dest), { recursive: true })
  const tmp = dest + '.tmp-' + process.pid
  try {
    await writeFile(tmp, asset.bytes)
    await rename(tmp, dest)
  } catch (error) {
    await rm(tmp, { force: true }).catch(() => {})
    throw error
  }
  return { path: dest, version: target, verified: asset.expected !== '' }
}

/** 从 `<exe> version` 的输出里取版本号；格式不符时返回 undefined。 */
export function extractVersion(text) {
  const match = /fsops\s+v?(\d+\.\d+\.\d+)/.exec(String(text))
  return match === null ? undefined : match[1]
}

/** 探测一个已解析二进制的版本；探测不到（无 subprocess、启动失败）返回 undefined。 */
async function probeVersion(subprocess, executable) {
  if (typeof subprocess?.spawn !== 'function') return undefined
  try {
    const handle = subprocess.spawn({
      argv: [executable, 'version'],
      cwd: process.cwd(),
      stdio: { stdin: 'ignore', stdout: { maxBytes: 4096 }, stderr: { maxBytes: 4096 } },
      graceMs: 5000,
    })
    const outcome = await handle.done
    if (outcome.exitCode !== 0) return undefined
    const stdout = handle.collected === undefined ? undefined : handle.collected.stdout
    return extractVersion(stdout === undefined ? '' : stdout.readFrom(0).text)
  } catch {
    return undefined
  }
}

/**
 * 确保拿到一个可执行的 fsops，返回绝对路径。
 * `subprocess` 是 Harness 的 ctx.subprocess，用来校验可执行文件；缺失时只做存在性判断。
 *
 * 版本裁定：
 *   - 显式来源（配置 binary / FSOPS_BIN）就是用户的选择，直接用，不事后推翻；
 *   - 包内 bin/ 与 %LOCALAPPDATA%\fsops-dsh\<version>\ 由版本号决定，不探测；
 *   - %LOCALAPPDATA%\fsops\ 与 PATH 上的可能是任意旧版本，必须核对——
 *     否则会出现「插件 0.6.0 配 0.5.0 内核」这种静默降级：schema 是新插件的，
 *     执行的却是旧二进制，参数差异直接变成 E_BAD_ARGS 循环。
 */
export async function resolveFsops({ pkgRoot, version, config, subprocess }) {
  const exe = process.platform === 'win32' ? 'fsops.exe' : 'fsops'
  const verify = async (path) => {
    if (subprocess?.resolveExecutable) {
      try {
        return await subprocess.resolveExecutable(path)
      } catch {
        return undefined
      }
    }
    return existsSync(path) ? path : undefined
  }

  const explicit = [config?.binary, process.env.FSOPS_BIN].filter(
    (value) => typeof value === 'string' && value.length > 0,
  )
  for (const candidate of explicit) {
    const resolved = await verify(candidate)
    if (resolved !== undefined) return resolved
  }

  const pinned = new Set([join(pkgRoot, 'bin', exe), cachePath(version)])
  const drifted = []
  for (const candidate of candidatePaths({ pkgRoot, version, config })) {
    if (explicit.includes(candidate)) continue
    const resolved = await verify(candidate)
    if (resolved === undefined) continue
    if (pinned.has(candidate)) return resolved
    const found = await probeVersion(subprocess, resolved)
    // 探测不到版本（异常/裁剪过的构建）不据此拒绝，宁可信任也不误伤。
    if (found === undefined || found === version) return resolved
    drifted.push({ path: resolved, found })
  }

  // 裸名称交给平台在 PATH 里查。
  const fromPath = await verify('fsops')
  if (fromPath !== undefined) {
    const found = await probeVersion(subprocess, fromPath)
    if (found === undefined || found === version) return fromPath
    drifted.push({ path: fromPath, found })
  }

  let downloadError
  if (config?.autoDownload !== false) {
    try {
      const downloaded = await downloadFsops(version)
      const resolved = await verify(downloaded.path)
      if (resolved !== undefined) return resolved
    } catch (error) {
      downloadError = error
    }
  }

  const allowMismatch =
    config?.allowVersionMismatch === true || process.env.FSOPS_ALLOW_VERSION_MISMATCH === '1'
  if (drifted.length > 0) {
    if (allowMismatch) return drifted[0].path
    const found = drifted.map((item) => item.path + '（v' + item.found + '）').join('、')
    throw new Error(
      'E_VERSION 找到的 fsops 与插件版本不符：' +
        found +
        '；本插件需要 v' +
        version +
        '。请安装对应版本、用 FSOPS_BIN 指向 v' +
        version +
        '，或保持 autoDownload 开启；确要用旧版本时设 FSOPS_ALLOW_VERSION_MISMATCH=1。' +
        (downloadError === undefined ? '' : '（自动下载也失败：' + downloadError.message + '）'),
    )
  }

  if (config?.autoDownload === false) {
    throw new Error(
      'E_NOT_FOUND 找不到 fsops.exe。请运行 fsops 的 install.ps1，或设置 FSOPS_BIN，' +
        '或在插件配置里用 binary 指定绝对路径。',
    )
  }
  if (downloadError !== undefined) throw downloadError
  throw new Error('E_NOT_FOUND 找不到可用的 fsops.exe')
}
