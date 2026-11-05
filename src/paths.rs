//! 路径层：把模型习惯的 POSIX 写法翻译成 Windows 真实路径。
//!
//! 支持四种输入形式：
//! - `/c/Users/x`、`/mnt/c/Users/x`（git-bash / WSL 风格）
//! - `C:\Users\x`、`C:/Users/x`（原生）
//! - `~/x`（用户目录）
//! - 相对路径（相对当前工作目录）
//!
//! 输出一律用 POSIX 形式（`/c/...`），因为模型对斜杠路径最熟；
//! 真正传给 Windows API 前再转成原生反斜杠 + 长路径前缀。

use std::path::{Component, Path, PathBuf};

/// 规范化后的路径：内部保存原生形式，展示时转 POSIX。
#[derive(Debug, Clone)]
pub struct FsPath {
    /// 原生 Windows 路径（不含长路径前缀）
    native: PathBuf,
}

impl FsPath {
    pub fn parse(input: &str) -> Result<Self, String> {
        let raw = input.trim();
        if raw.is_empty() {
            return Err("路径为空".into());
        }
        let p = if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
            home_dir()?.join(rest)
        } else if raw == "~" {
            home_dir()?
        } else if let Some(rest) = strip_drive_prefix(raw) {
            // /c/... 或 /mnt/c/...
            let mut s = String::new();
            s.push(rest.0);
            s.push(':');
            s.push('\\');
            s.push_str(&rest.1.replace('/', "\\"));
            PathBuf::from(s)
        } else if raw.starts_with("//") || raw.starts_with("\\\\") {
            // UNC：//server/share/x
            PathBuf::from(raw.replace('/', "\\"))
        } else if is_drive_abs(raw) {
            PathBuf::from(raw.replace('/', "\\"))
        } else {
            std::env::current_dir()
                .map_err(|e| format!("无法获取当前目录: {e}"))?
                .join(raw.replace('/', "\\"))
        };
        Ok(Self {
            native: normalize(&p),
        })
    }

    pub fn native(&self) -> &Path {
        &self.native
    }

    /// 展示形式：`/c/code/fsops/src/main.rs`；UNC 保持 `//server/share/...`。
    pub fn display(&self) -> String {
        let s = self.native.to_string_lossy().replace('\\', "/");
        if let Some(rest) = s.strip_prefix("//") {
            return format!("//{rest}");
        }
        let bytes = s.as_bytes();
        if bytes.len() >= 2 && bytes[1] == b':' {
            let drive = (bytes[0] as char).to_ascii_lowercase();
            let tail = &s[2..];
            if tail.is_empty() {
                return format!("/{drive}");
            }
            return format!("/{drive}{tail}");
        }
        s
    }

    /// 交给 Windows API 的形式：超长或含特殊结尾时加 `\\?\` 前缀。
    pub fn wide(&self) -> PathBuf {
        let s = self.native.to_string_lossy();
        if s.starts_with("\\\\?\\") {
            return self.native.clone();
        }
        let needs_prefix = s.len() >= 240 || s.ends_with(' ') || s.ends_with('.');
        if !needs_prefix {
            return self.native.clone();
        }
        if let Some(unc) = s.strip_prefix("\\\\") {
            PathBuf::from(format!("\\\\?\\UNC\\{unc}"))
        } else {
            PathBuf::from(format!("\\\\?\\{s}"))
        }
    }

    pub fn file_name(&self) -> String {
        self.native
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    }

    pub fn exists(&self) -> bool {
        self.wide().exists()
    }

    pub fn is_dir(&self) -> bool {
        self.wide().is_dir()
    }
}

fn is_drive_abs(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 3 && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/') && b[0].is_ascii_alphabetic()
}

/// `/c/...` 或 `/mnt/c/...` → ("c", "rest")
fn strip_drive_prefix(s: &str) -> Option<(char, String)> {
    let t = if let Some(rest) = s.strip_prefix("/mnt/") {
        rest
    } else {
        s.strip_prefix('/')?
    };
    let mut it = t.chars();
    let drive = it.next()?;
    if !drive.is_ascii_alphabetic() {
        return None;
    }
    let rest = it.as_str();
    // `/mnt/c` 后必须是结尾或斜杠，避免把 /mnt/cdrom 当成 C 盘
    if !(rest.is_empty() || rest.starts_with('/')) {
        return None;
    }
    Some((drive, rest.trim_start_matches('/').to_string()))
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub fn home_dir() -> Result<PathBuf, String> {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .or_else(|| {
            let drive = std::env::var_os("HOMEDRIVE")?;
            let path = std::env::var_os("HOMEPATH")?;
            Some(PathBuf::from(drive).join(path))
        })
        .ok_or_else(|| "无法定位用户目录".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解析各种盘符写法() {
        let a = FsPath::parse("/c/repo/x").unwrap();
        assert_eq!(a.display(), "/c/repo/x");
        let b = FsPath::parse("/mnt/d/repo/fsops").unwrap();
        assert_eq!(b.display(), "/d/repo/fsops");
        let c = FsPath::parse("C:\\repo\\x").unwrap();
        assert_eq!(c.display(), "/c/repo/x");
        let d = FsPath::parse("d:/repo/y").unwrap();
        assert_eq!(d.display(), "/d/repo/y");
    }

    #[test]
    fn 不把_mnt_cdrom_当成_c_盘() {
        assert!(strip_drive_prefix("/mnt/cdrom").is_none());
    }

    #[test]
    fn 折叠点点() {
        let p = FsPath::parse("/c/code/a/../b").unwrap();
        assert_eq!(p.display(), "/c/code/b");
    }
}
