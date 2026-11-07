//! 自更新：查 GitHub 最新 Release → 下载 → SHA-256 校验 → 替换当前二进制。
//!
//! 只做 CLI（`fsops upgrade`），不注册成 MCP 工具：替换过程会与正在运行的
//! 服务端抢同一份文件，从工具调用里触发等于让模型自己拔自己的电源。

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 从重定向后的 Release 地址里取 tag，如 `/releases/tag/v0.6.0` → `v0.6.0`。
pub fn tag_from_url(url: &str) -> Option<String> {
    let rest = url.trim().split("/releases/tag/").nth(1)?;
    let tag = rest.split(['/', '?', '#']).next()?.trim();
    if !tag.starts_with('v') {
        return None;
    }
    version_tuple(tag)?;
    Some(tag.to_string())
}

/// `v0.5.0` → `(0,5,0)`；缺段按 0 补齐，非法返回 None。
pub fn version_tuple(tag: &str) -> Option<(u64, u64, u64)> {
    let text = tag.trim().trim_start_matches('v');
    let mut parts = text.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

pub fn is_newer(latest: &str, current: &str) -> bool {
    match (version_tuple(latest), version_tuple(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

pub fn asset_name(tag: &str) -> String {
    format!("fsops-{tag}-windows-x86_64.exe")
}

pub fn sha256_file(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path)
        .map_err(|e| format!("E_UPGRADE 打开 {} 失败: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buffer)
            .map_err(|e| format!("E_UPGRADE 读取 {} 失败: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// curl 执行器：网络类失败自动重试 3 次；HTTP 状态错误（如 404）不重试。
fn curl(args: &[String]) -> Result<std::process::Output, String> {
    let mut last = String::new();
    for attempt in 1..=3 {
        let out = Command::new("curl").args(args).output().map_err(|e| {
            format!("E_UPGRADE 需要 curl.exe（Windows 10 1803+ 自带）；也可用一行安装命令升级: {e}")
        })?;
        if out.status.success() {
            return Ok(out);
        }
        let code = out.status.code();
        last = format!(
            "curl exit {code:?}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        if code == Some(22) {
            break;
        }
        if attempt < 3 {
            std::thread::sleep(std::time::Duration::from_millis(300 * attempt as u64));
        }
    }
    Err(format!("E_UPGRADE 网络请求失败: {last}"))
}

/// 跟随 `releases/latest` 重定向，从最终 URL 取最新 tag。
pub fn resolve_latest(base: &str) -> Result<String, String> {
    let url = format!("{}/releases/latest", base.trim_end_matches('/'));
    let out = curl(&[
        "-fsSL".into(),
        "-o".into(),
        "NUL".into(),
        "-w".into(),
        "%{url_effective}".into(),
        url.clone(),
    ])?;
    if !out.status.success() {
        return Err(format!(
            "E_UPGRADE 查询最新版本失败: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let final_url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    tag_from_url(&final_url).ok_or_else(|| format!("E_UPGRADE 无法从 {final_url} 解析版本 tag"))
}

fn download(url: &str, dest: &Path) -> Result<(), String> {
    let out = curl(&[
        "-fsSL".into(),
        "--connect-timeout".into(),
        "20".into(),
        "--max-time".into(),
        "300".into(),
        "-o".into(),
        dest.to_string_lossy().into_owned(),
        url.to_string(),
    ])?;
    if !out.status.success() {
        return Err(format!(
            "E_UPGRADE 下载失败: {url}（{}）",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// 清掉历史升级留下的 .old-*；正在被进程占用的会删除失败，忽略即可。
fn cleanup_stale_olds(target: &Path) {
    let Some(parent) = target.parent() else {
        return;
    };
    let Some(name) = target.file_name().map(|n| n.to_string_lossy().to_string()) else {
        return;
    };
    let prefix = format!("{name}.old-");
    if let Ok(entries) = std::fs::read_dir(parent) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// 先改名再落新版：Windows 允许重命名正在运行的自身，替换后下次启动才加载新版。
fn install(new_file: &Path, target: &Path) -> Result<PathBuf, String> {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "fsops.exe".into());
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let old = target.with_file_name(format!("{name}.old-{stamp}"));
    std::fs::rename(target, &old).map_err(|e| {
        format!(
            "E_UPGRADE 无法移走旧二进制 {}（{e}）；确认目录可写",
            target.display()
        )
    })?;
    match std::fs::rename(new_file, target) {
        Ok(()) => Ok(old),
        Err(rename_err) => match std::fs::copy(new_file, target) {
            Ok(_) => {
                let _ = std::fs::remove_file(new_file);
                Ok(old)
            }
            Err(copy_err) => {
                let _ = std::fs::rename(&old, target);
                Err(format!(
                    "E_UPGRADE 写入新二进制失败，已回滚（rename: {rename_err}；copy: {copy_err}）"
                ))
            }
        },
    }
}

/// 下载 + 校验 + 替换 `target`。校验不通过时不改动目标文件。
pub fn upgrade_to(base: &str, tag: &str, target: &Path) -> Result<String, String> {
    let asset = asset_name(tag);
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("fsops-upgrade-{}-{unique}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("E_UPGRADE 创建临时目录失败: {e}"))?;
    let new_exe = dir.join(&asset);
    let sha_file = dir.join(format!("{asset}.sha256"));
    let base = base.trim_end_matches('/');

    let result = (|| -> Result<PathBuf, String> {
        download(&format!("{base}/releases/download/{tag}/{asset}"), &new_exe)?;
        download(
            &format!("{base}/releases/download/{tag}/{asset}.sha256"),
            &sha_file,
        )?;
        let sha_text = std::fs::read_to_string(&sha_file)
            .map_err(|e| format!("E_UPGRADE 读取 sha256 文件失败: {e}"))?;
        let expect = sha_text
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if expect.len() != 64 {
            return Err(format!("E_UPGRADE sha256 文件格式异常: {sha_text:?}"));
        }
        let actual = sha256_file(&new_exe)?;
        if actual != expect {
            return Err(format!(
                "E_UPGRADE SHA-256 不匹配（期望 {expect}，实得 {actual}），已中止，未替换任何文件"
            ));
        }
        cleanup_stale_olds(target);
        install(&new_exe, target)
    })();
    let _ = std::fs::remove_dir_all(&dir);
    let old = result?;
    Ok(format!(
        "ok upgrade → {tag}\n  路径: {}\n  旧文件: {}（本次升级进程仍占用，下次 fsops upgrade 自动清理）\n  生效: MCP 宿主执行 /reload-mcp 或重启；Hermes 执行 hermes gateway restart",
        target.display(),
        old.display()
    ))
}

pub fn run(args: &[String]) -> i32 {
    let check = args.iter().any(|a| a == "--check");
    let force = args.iter().any(|a| a == "--force");
    let unknown: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|a| *a != "--check" && *a != "--force")
        .collect();
    if !unknown.is_empty() {
        println!(
            "E_BAD_ARGS 未知参数 {}（用法: fsops upgrade [--check] [--force]）",
            unknown.join(" ")
        );
        return 2;
    }
    let base = std::env::var("FSOPS_UPGRADE_BASE")
        .unwrap_or_else(|_| env!("CARGO_PKG_REPOSITORY").to_string());
    let current = crate::VERSION;
    let target = match std::env::current_exe() {
        Ok(path) => path,
        Err(e) => {
            println!("E_UPGRADE 无法定位当前二进制: {e}");
            return 2;
        }
    };
    let latest = match resolve_latest(&base) {
        Ok(tag) => tag,
        Err(e) => {
            println!("{e}");
            return 2;
        }
    };
    if check {
        if is_newer(&latest, current) {
            println!("ok current={current} latest={latest} 可升级: fsops upgrade");
        } else {
            println!("ok current={current} latest={latest}（已是最新）");
        }
        return 0;
    }
    if !is_newer(&latest, current) && !force {
        println!("ok current={current} latest={latest}（已是最新；重装用 fsops upgrade --force）");
        return 0;
    }
    match upgrade_to(&base, &latest, &target) {
        Ok(text) => {
            println!("{text}");
            0
        }
        Err(e) => {
            println!("{e}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[test]
    fn 从release地址解析tag() {
        assert_eq!(
            tag_from_url("https://github.com/tokenis2023/agent-ops/releases/tag/v0.6.0").as_deref(),
            Some("v0.6.0")
        );
        assert_eq!(
            tag_from_url("https://x/releases/tag/v1.2.3?expanded=true").as_deref(),
            Some("v1.2.3")
        );
        assert_eq!(tag_from_url("https://x/releases"), None);
        assert_eq!(tag_from_url("https://x/releases/tag/latest"), None);
    }

    #[test]
    fn 版本比较只在更高时升级() {
        assert_eq!(version_tuple("v0.5.0"), Some((0, 5, 0)));
        assert_eq!(version_tuple("1.2"), Some((1, 2, 0)));
        assert_eq!(version_tuple("bad"), None);
        assert!(is_newer("v0.6.0", "0.5.0"));
        assert!(is_newer("v0.5.1", "0.5.0"));
        assert!(!is_newer("v0.5.0", "0.5.0"));
        assert!(!is_newer("v0.4.9", "0.5.0"));
    }

    #[test]
    fn 资产名与命名规则一致() {
        assert_eq!(asset_name("v0.6.0"), "fsops-v0.6.0-windows-x86_64.exe");
    }

    #[test]
    fn sha256与标准向量一致() {
        let dir = std::env::temp_dir().join(format!("fsops-sha-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("abc.txt");
        std::fs::write(&f, b"abc").unwrap();
        let got = sha256_file(&f).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            got,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// 极简 HTTP 服务：latest 重定向到 /tag/v9.9.9，并托管新二进制与 sha256。
    fn spawn_server(
        stop: Arc<AtomicBool>,
        sha_override: Option<String>,
        fail_first: bool,
    ) -> (String, Vec<u8>, String) {
        let new_binary = b"new-fsops-binary".to_vec();
        let sha = sha256_of(&new_binary);
        let served_sha_value = sha_override.unwrap_or_else(|| sha.clone());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let base = format!("http://127.0.0.1:{port}");
        let sha_body = format!("{served_sha_value}  fsops-v9.9.9-windows-x86_64.exe\n");
        let served_base = base.clone();
        let served_sha = sha_body.clone();
        let served_binary = new_binary.clone();
        std::thread::spawn(move || {
            let mut served = 0usize;
            let mut dropped = false;
            while !stop.load(Ordering::Relaxed) && served < 8 {
                let (mut sock, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(_) => {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                        continue;
                    }
                };
                served += 1;
                let mut buf = [0u8; 4096];
                let n = sock.read(&mut buf).unwrap_or(0);
                if fail_first && !dropped {
                    dropped = true;
                    continue;
                }
                let req = String::from_utf8_lossy(&buf[..n]);
                let path = req
                    .lines()
                    .next()
                    .unwrap_or("")
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or("/")
                    .to_string();
                let (head, body): (String, Vec<u8>) = if path == "/releases/latest" {
                    (
                        format!(
                            "HTTP/1.1 302 Found\r\nLocation: {served_base}/releases/tag/v9.9.9\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        ),
                        Vec::new(),
                    )
                } else if path == "/releases/tag/v9.9.9" {
                    (resp_head(200, 2), b"ok".to_vec())
                } else if path == "/releases/download/v9.9.9/fsops-v9.9.9-windows-x86_64.exe" {
                    (resp_head(200, served_binary.len()), served_binary.clone())
                } else if path == "/releases/download/v9.9.9/fsops-v9.9.9-windows-x86_64.exe.sha256"
                {
                    (
                        resp_head(200, served_sha.len()),
                        served_sha.clone().into_bytes(),
                    )
                } else {
                    (resp_head(404, 0), Vec::new())
                };
                let _ = sock.write_all(head.as_bytes());
                let _ = sock.write_all(&body);
                let _ = sock.flush();
            }
        });
        (base, new_binary, sha)
    }

    fn resp_head(code: u16, len: usize) -> String {
        format!("HTTP/1.1 {code} X\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n")
    }

    fn sha256_of(data: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(data);
        format!("{:x}", h.finalize())
    }

    #[test]
    fn 网络瞬时失败自动重试() {
        let stop = Arc::new(AtomicBool::new(false));
        let (base, _new, _sha) = spawn_server(stop.clone(), None, true);
        let tag = resolve_latest(&base).expect("首次空响应后应重试成功");
        stop.store(true, Ordering::Relaxed);
        assert_eq!(tag, "v9.9.9");
    }

    #[test]
    fn 升级端到端_下载校验并替换_旧文件保留() {
        let stop = Arc::new(AtomicBool::new(false));
        let (base, new_binary, _sha) = spawn_server(stop.clone(), None, false);
        let dir = std::env::temp_dir().join(format!("fsops-upgrade-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("fsops.exe");
        std::fs::write(&target, b"old-binary").unwrap();

        let out = upgrade_to(&base, "v9.9.9", &target);
        stop.store(true, Ordering::Relaxed);
        let text = out.unwrap_or_else(|e| panic!("{e}"));
        assert!(text.contains("0.5") || text.contains("9.9.9"), "{text}");
        assert_eq!(std::fs::read(&target).unwrap(), new_binary);
        let olds: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".old-"))
            .collect();
        assert_eq!(olds.len(), 1, "必须保留一个旧文件用于回滚");
        assert_eq!(std::fs::read(olds[0].path()).unwrap(), b"old-binary");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 升级端到端_哈希不匹配则中止且不替换() {
        let stop = Arc::new(AtomicBool::new(false));
        let (base, _new, _sha) = spawn_server(stop.clone(), Some("deadbeef".repeat(8)), false);
        let dir = std::env::temp_dir().join(format!("fsops-upgrade-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("fsops.exe");
        std::fs::write(&target, b"old-binary").unwrap();

        let err = upgrade_to(&base, "v9.9.9", &target).expect_err("哈希不匹配必须中止");
        stop.store(true, Ordering::Relaxed);
        assert!(err.contains("SHA-256"), "{err}");
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"old-binary",
            "不得替换目标文件"
        );
        let olds = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".old-"))
            .count();
        assert_eq!(olds, 0, "校验失败时不应留下 .old 文件");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
