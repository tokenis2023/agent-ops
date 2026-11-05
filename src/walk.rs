//! 遍历层：glob 与文件发现。
//!
//! 默认尊重 .gitignore、跳过 .git 与常见构建产物目录，
//! 但**不**跳过点文件（.env / .github 这类对排障很重要）。

use crate::io::{fmt_time, human_size, meta};
use crate::paths::FsPath;
use globset::{Glob, GlobSetBuilder};
use ignore::WalkBuilder;

/// 无论 .gitignore 怎么写，这些目录一律不进（否则一次 glob 能扫出几万条）
const HARD_SKIP: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".venv",
    "venv",
    "__pycache__",
    "dist",
    ".next",
    ".turbo",
    ".gradle",
    ".idea",
];

pub struct WalkOpts {
    pub include_dirs: bool,
    pub limit: usize,
    pub follow_links: bool,
}

impl Default for WalkOpts {
    fn default() -> Self {
        Self {
            include_dirs: false,
            limit: 500,
            follow_links: false,
        }
    }
}

pub struct Entry {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: std::time::SystemTime,
}

/// 流式访问的控制信号；Stop 会立刻终止目录迭代。
pub enum VisitControl {
    Continue,
    Stop,
}

pub struct WalkReport {
    pub entries_visited: usize,
    pub stopped: bool,
}

/// 逐项访问匹配路径，不预先收集完整文件列表。
pub fn visit<F>(
    root: &FsPath,
    pattern: &str,
    opts: &WalkOpts,
    mut visitor: F,
) -> Result<WalkReport, String>
where
    F: FnMut(Entry) -> VisitControl,
{
    if !root.exists() {
        return Err(format!("E_NOT_FOUND {}", root.display()));
    }
    let mut builder = GlobSetBuilder::new();
    let has_pattern = !pattern.trim().is_empty();
    if has_pattern {
        for pat in pattern.split(',') {
            let pat = pat.trim();
            if pat.is_empty() {
                continue;
            }
            let g = Glob::new(pat).map_err(|e| format!("E_BAD_GLOB {pat}: {e}"))?;
            builder.add(g);
        }
    }
    let set = builder
        .build()
        .map_err(|e| format!("E_BAD_GLOB 解析失败: {e}"))?;
    let mut report = WalkReport {
        entries_visited: 0,
        stopped: opts.limit == 0,
    };
    if opts.limit == 0 {
        return Ok(report);
    }
    // 单文件根直接访问该文件，不能退到父目录后把相对文件名再拼一次。
    if !root.is_dir() {
        let rel_posix = root.file_name().replace('\\', "/");
        if has_pattern && !set.is_match(&rel_posix) {
            return Ok(report);
        }
        let meta = match meta(root) {
            Ok(meta) => meta,
            Err(_) => return Ok(report),
        };
        report.entries_visited = 1;
        let control = visitor(Entry {
            path: rel_posix,
            is_dir: false,
            size: meta.size,
            mtime: meta.mtime,
        });
        report.stopped =
            matches!(control, VisitControl::Stop) || report.entries_visited >= opts.limit;
        return Ok(report);
    }
    let base = root.native().to_path_buf();
    let mut walker = WalkBuilder::new(&base);
    walker
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .follow_links(opts.follow_links)
        // 串行流只按路径排序当前目录，不预先收集全局列表；输出因此可重放。
        .sort_by_file_path(|a, b| a.cmp(b))
        .filter_entry(|e| {
            !e.file_name()
                .to_str()
                .map(|n| HARD_SKIP.contains(&n))
                .unwrap_or(false)
        });

    for entry in walker.build() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if path == base {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir && !opts.include_dirs {
            continue;
        }
        let rel = path.strip_prefix(&base).unwrap_or(path);
        let rel_posix = rel.to_string_lossy().replace('\\', "/");
        // 目录不参与文件 glob，但必须交给调用方检查遍历 deadline。
        if has_pattern && !is_dir && !set.is_match(&rel_posix) {
            continue;
        }
        let fp = FsPath::parse(&path.to_string_lossy())?;
        let m = match meta(&fp) {
            Ok(m) => m,
            Err(_) => continue,
        };
        report.entries_visited += 1;
        let control = visitor(Entry {
            path: rel_posix,
            is_dir,
            size: m.size,
            mtime: m.mtime,
        });
        if matches!(control, VisitControl::Stop) || report.entries_visited >= opts.limit {
            report.stopped = true;
            break;
        }
    }
    Ok(report)
}

/// 按 glob 收集文件；`pattern` 为空表示全部。
pub fn glob(root: &FsPath, pattern: &str, opts: &WalkOpts) -> Result<Vec<Entry>, String> {
    if !root.exists() {
        return Err(format!("E_NOT_FOUND {}", root.display()));
    }
    if opts.limit == 0 {
        return Ok(Vec::new());
    }

    let mut builder = GlobSetBuilder::new();
    let has_pattern = !pattern.trim().is_empty();
    if has_pattern {
        for pat in pattern.split(',') {
            let pat = pat.trim();
            if pat.is_empty() {
                continue;
            }
            let g = Glob::new(pat).map_err(|e| format!("E_BAD_GLOB {pat}: {e}"))?;
            builder.add(g);
        }
    }
    let set = builder
        .build()
        .map_err(|e| format!("E_BAD_GLOB 解析失败: {e}"))?;

    // 文件根只返回该文件，不能退到父目录后把兄弟文件也扫出来。
    if !root.is_dir() {
        let rel_posix = root.file_name().replace('\\', "/");
        if has_pattern && !set.is_match(&rel_posix) {
            return Ok(Vec::new());
        }
        let m = match meta(root) {
            Ok(m) => m,
            Err(_) => return Ok(Vec::new()),
        };
        return Ok(vec![Entry {
            path: rel_posix,
            is_dir: false,
            size: m.size,
            mtime: m.mtime,
        }]);
    }

    let base = root.native().to_path_buf();

    let mut walker = WalkBuilder::new(&base);
    walker
        .hidden(false) // 保留点文件
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .follow_links(opts.follow_links)
        // 目录内按路径稳定排序，达到 limit 立即停止，不再全量收集后排序。
        .sort_by_file_path(|a, b| a.cmp(b))
        .filter_entry(|e| {
            !e.file_name()
                .to_str()
                .map(|n| HARD_SKIP.contains(&n))
                .unwrap_or(false)
        });

    let mut out = Vec::new();
    for entry in walker.build() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue, // 权限不足等直接跳过，不打断整体结果
        };
        let path = entry.path();
        if path == base {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir && !opts.include_dirs {
            continue;
        }
        let rel = path.strip_prefix(&base).unwrap_or(path);
        let rel_posix = rel.to_string_lossy().replace('\\', "/");
        if has_pattern && !set.is_match(&rel_posix) {
            continue;
        }
        let fp = FsPath::parse(&path.to_string_lossy())?;
        let m = match meta(&fp) {
            Ok(m) => m,
            Err(_) => continue,
        };
        out.push(Entry {
            // 输出相对搜索根的路径：比绝对路径短一半，模型拼接也不易错
            path: rel_posix,
            is_dir,
            size: m.size,
            mtime: m.mtime,
        });
        if out.len() >= opts.limit {
            break;
        }
    }

    if out.len() == opts.limit {
        // 无法在不继续遍历的情况下确认是否还有更多，明确提示而不是静默截断。
        out.push(Entry {
            path: format!("… 达到 limit={}，可能还有更多", opts.limit),
            is_dir: false,
            size: 0,
            mtime: std::time::SystemTime::UNIX_EPOCH,
        });
    }
    Ok(out)
}

/// 紧凑列表：默认只给路径（目录带 `/`），`detail` 时补大小与时间。
pub fn render_entries(entries: &[Entry], detail: bool) -> String {
    let mut out = String::with_capacity(entries.len() * 48);
    for e in entries {
        if e.size == 0 && e.mtime == std::time::SystemTime::UNIX_EPOCH {
            out.push_str(&e.path);
            out.push('\n');
            continue;
        }
        if !detail {
            out.push_str(&e.path);
            if e.is_dir {
                out.push('/');
            }
            out.push('\n');
            continue;
        }
        let kind = if e.is_dir { "d" } else { "f" };
        out.push_str(&format!(
            "{kind} {} {} {}\n",
            human_size(e.size),
            &fmt_time(e.mtime)[5..],
            e.path
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn glob_只返回匹配文件并跳过硬忽略目录() {
        let tmp = std::env::temp_dir().join(format!("fsops-glob-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("src")).unwrap();
        fs::create_dir_all(tmp.join("node_modules")).unwrap();
        fs::write(tmp.join("src/a.rs"), "fn a(){}").unwrap();
        fs::write(tmp.join("src/b.txt"), "b").unwrap();
        fs::write(tmp.join("node_modules/c.rs"), "c").unwrap();

        let root = FsPath::parse(&tmp.to_string_lossy()).unwrap();
        let got = glob(&root, "**/*.rs", &WalkOpts::default()).unwrap();
        let names: Vec<String> = got.iter().map(|e| e.path.clone()).collect();
        assert_eq!(names.len(), 1);
        assert!(names[0].ends_with("src/a.rs"));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn glob_limit提前停止并标注可能有更多() {
        let tmp = std::env::temp_dir().join(format!("fsops-glob-limit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            fs::write(tmp.join(name), "x").unwrap();
        }

        let root = FsPath::parse(&tmp.to_string_lossy()).unwrap();
        let got = glob(
            &root,
            "**/*.txt",
            &WalkOpts {
                limit: 1,
                ..Default::default()
            },
        )
        .unwrap();
        let _ = fs::remove_dir_all(&tmp);

        assert_eq!(got.len(), 2, "返回条目数不符");
        assert!(got[0].path.ends_with(".txt"));
        assert!(got[1].path.contains("还有更多"), "{:?}", got[1].path);
    }

    #[test]
    fn 流式访问达到上限后立即停止遍历() {
        let tmp = std::env::temp_dir().join(format!("fsops-visit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            fs::write(tmp.join(name), "needle\n").unwrap();
        }

        let root = FsPath::parse(&tmp.to_string_lossy()).unwrap();
        let mut seen = Vec::new();
        let report = visit(
            &root,
            "**/*.txt",
            &WalkOpts {
                limit: 1,
                ..Default::default()
            },
            |entry| {
                seen.push(entry.path);
                VisitControl::Continue
            },
        )
        .unwrap();

        assert_eq!(seen.len(), 1);
        assert_eq!(report.entries_visited, 1);
        assert!(report.stopped);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn 流式访问在glob过滤前暴露目录项() {
        let tmp = std::env::temp_dir().join(format!("fsops-visit-dirs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("empty/nested")).unwrap();
        fs::write(tmp.join("a.txt"), "needle\n").unwrap();

        let root = FsPath::parse(&tmp.to_string_lossy()).unwrap();
        let mut saw_directory = false;
        let report = visit(
            &root,
            "**/*.txt",
            &WalkOpts {
                include_dirs: true,
                ..Default::default()
            },
            |entry| {
                saw_directory |= entry.is_dir;
                VisitControl::Continue
            },
        )
        .unwrap();

        assert!(saw_directory, "glob 把目录提前过滤，搜索无法检查遍历超时");
        assert!(report.entries_visited >= 3);
        let _ = fs::remove_dir_all(&tmp);
    }
}
