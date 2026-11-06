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

/**
 * 确保拿到一个可执行的 fsops，返回绝对路径。
 * `subprocess` 是 Harness 的 ctx.subprocess，用来校验可执行文件；缺失时只做存在性判断。
 */
export async function resolveFsops({ pkgRoot, version, config, subprocess }) {
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

  for (const candidate of candidatePaths({ pkgRoot, version, config })) {
    const resolved = await verify(candidate)
    if (resolved !== undefined) return resolved
  }

  // 裸名称交给平台在 PATH 里查。
  const fromPath = await verify('fsops')
  if (fromPath !== undefined) return fromPath

  if (config?.autoDownload === false) {
    throw new Error(
      'E_NOT_FOUND 找不到 fsops.exe。请运行 fsops 的 install.ps1，或设置 FSOPS_BIN，' +
        '或在插件配置里用 binary 指定绝对路径。',
    )
  }
  const downloaded = await downloadFsops(version)
  const resolved = await verify(downloaded.path)
  if (resolved === undefined) {
    throw new Error('E_NOT_FOUND 已下载 fsops 到 ' + downloaded.path + '，但无法作为可执行文件解析')
  }
  return resolved
}
