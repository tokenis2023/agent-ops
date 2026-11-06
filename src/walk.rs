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
    /// glob 分页时跳过的匹配条目数；visit 不使用该字段。
    pub offset: usize,
    /// 遍历时间预算；无匹配时也要能及时返回，避免全盘扫描把 Agent 卡死。
    pub time_budget_ms: u128,
    /// 忽略 .gitignore 与硬跳过目录。只在明确要查依赖源码/构建产物时打开：
    /// 默认跳过是对的（否则一次 glob 能扫出几万条），但 agent 不能因此失去查 node_modules 的能力。
    pub no_ignore: bool,
}

impl Default for WalkOpts {
    fn default() -> Self {
        Self {
            include_dirs: false,
            limit: usize::MAX,
            follow_links: false,
            offset: 0,
            time_budget_ms: 5000,
            no_ignore: false,
        }
    }
}

impl WalkOpts {
    fn deadline(&self) -> std::time::Instant {
        std::time::Instant::now()
            + std::time::Duration::from_millis(self.time_budget_ms.min(u64::MAX as u128) as u64)
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
    /// 是否因时间预算耗尽而停止；与“没有匹配”区分开。
    pub timed_out: bool,
    /// 因 offset 跳过的匹配条目数；用于区分“本页为空”和“完全无匹配”。
    pub offset_skipped: usize,
    /// 达到 limit 后只探测了有限个后续原始目录项，仍未确认是否存在下一页。
    /// 与 timed_out 分开：这不是时间预算耗尽，而是主动限制探测成本。
    pub lookahead_limited: bool,
}

/// 达到 limit 后，为确认“是否还有下一页”最多再看多少个后续原始目录项。
/// 全量探测会把大仓库里的即时返回拖到时间预算上限。
const LOOKAHEAD_ITEMS: usize = 1024;

/// 拆分逗号分隔的 glob，但忽略花括号/方括号内部的逗号。
///
/// `{a,b}.rs` 是一个带 alternate group 的 glob；简单 `split(',')` 会把它
/// 切成 `{a` 和 `b}.rs`，报出令人困惑的解析错误。
fn split_patterns(pattern: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, ch) in pattern.char_indices() {
        match ch {
            '{' | '[' => depth += 1,
            '}' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                out.push(&pattern[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&pattern[start..]);
    out.into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect()
}

/// 构建 glob 集合；两处遍历入口共用，避免行为分叉。
fn build_globset(pattern: &str) -> Result<globset::GlobSet, String> {
    // 竖线是正则的"或"，在 glob 里是字面量：会静默匹配不到任何文件。
    if pattern.contains('|') {
        return Err(format!(
            "E_BAD_GLOB pattern 不支持 `|`（那是正则写法）；\
             多个模式请用逗号分隔，如 `*.rs,*.ts`（当前: {pattern}）"
        ));
    }
    let mut builder = GlobSetBuilder::new();
    for pat in split_patterns(pattern) {
        let g = Glob::new(pat).map_err(|e| format!("E_BAD_GLOB {pat}: {e}"))?;
        builder.add(g);
    }
    builder
        .build()
        .map_err(|e| format!("E_BAD_GLOB 解析失败: {e}"))
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
    let has_pattern = !pattern.trim().is_empty();
    let set = if has_pattern {
        build_globset(pattern)?
    } else {
        GlobSetBuilder::new()
            .build()
            .map_err(|e| format!("E_BAD_GLOB 解析失败: {e}"))?
    };
    let mut report = WalkReport {
        entries_visited: 0,
        stopped: opts.limit == 0,
        timed_out: false,
        offset_skipped: 0,
        lookahead_limited: false,
    };
    // 入口就检查 deadline：预算为 0 时必须立刻返回，
    // 否则要等第一个 256 项检查点，白扫 256 项（且错误信息会误导成"已访问 256 项"）。
    let deadline = opts.deadline();
    if std::time::Instant::now() > deadline {
        report.timed_out = true;
        report.stopped = true;
        return Ok(report);
    }
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
    let no_ignore = opts.no_ignore;
    let mut walker = WalkBuilder::new(&base);
    walker
        .hidden(false)
        .git_ignore(!opts.no_ignore)
        .git_global(!opts.no_ignore)
        .git_exclude(!opts.no_ignore)
        .follow_links(opts.follow_links)
        // 串行流只按路径排序当前目录，不预先收集全局列表；输出因此可重放。
        .sort_by_file_path(|a, b| a.cmp(b))
        .filter_entry(move |e| {
            if no_ignore {
                return true;
            }
            !e.file_name()
                .to_str()
                .map(|n| HARD_SKIP.contains(&n))
                .unwrap_or(false)
        });

    let mut raw_entries = 0usize;
    for entry in walker.build() {
        raw_entries += 1;
        // deadline 由遍历层自己执行，不依赖匹配后的 visitor 被调用。
        if raw_entries.is_multiple_of(256) && std::time::Instant::now() > deadline {
            report.timed_out = true;
            report.stopped = true;
            return Ok(report);
        }
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
    // 不再在循环结束后按耗时补判 timed_out：遍历已经完整走完，
    // 慢不等于没走完；visitor 自己的 deadline 由调用方检查。
    Ok(report)
}

/// 按 glob 收集文件；`pattern` 为空表示全部。
pub fn glob(root: &FsPath, pattern: &str, opts: &WalkOpts) -> Result<Vec<Entry>, String> {
    glob_with_report(root, pattern, opts).map(|(entries, _)| entries)
}

/// 带遍历状态的 glob：调用方可区分“无匹配”和“时间预算耗尽”。
pub fn glob_with_report(
    root: &FsPath,
    pattern: &str,
    opts: &WalkOpts,
) -> Result<(Vec<Entry>, WalkReport), String> {
    let deadline = opts.deadline();
    let mut report = WalkReport {
        entries_visited: 0,
        stopped: false,
        timed_out: false,
        offset_skipped: 0,
        lookahead_limited: false,
    };
    let (entries, timed_out) = glob_inner(root, pattern, opts, deadline, &mut report)?;
    report.timed_out = timed_out;
    Ok((entries, report))
}

fn glob_inner(
    root: &FsPath,
    pattern: &str,
    opts: &WalkOpts,
    deadline: std::time::Instant,
    report: &mut WalkReport,
) -> Result<(Vec<Entry>, bool), String> {
    if !root.exists() {
        return Err(format!("E_NOT_FOUND {}", root.display()));
    }
    if opts.limit == 0 {
        return Ok((Vec::new(), false));
    }
    // 预算为 0 时立刻返回：否则要等第一个 256 项检查点，
    // 白扫 256 项，且错误信息会误导成"已访问 256 项"。
    if std::time::Instant::now() > deadline {
        return Ok((Vec::new(), true));
    }

    let has_pattern = !pattern.trim().is_empty();
    let set = if has_pattern {
        build_globset(pattern)?
    } else {
        GlobSetBuilder::new()
            .build()
            .map_err(|e| format!("E_BAD_GLOB 解析失败: {e}"))?
    };

    // 文件根只返回该文件，不能退到父目录后把兄弟文件也扫出来。
    if !root.is_dir() {
        let rel_posix = root.file_name().replace('\\', "/");
        if has_pattern && !set.is_match(&rel_posix) {
            return Ok((Vec::new(), false));
        }
        if opts.offset > 0 {
            // 这里确实存在 1 个匹配条目，只是被本页 offset 跳过。
            report.offset_skipped = 1;
            return Ok((Vec::new(), false));
        }
        let m = match meta(root) {
            Ok(m) => m,
            Err(_) => return Ok((Vec::new(), false)),
        };
        return Ok((
            vec![Entry {
                path: rel_posix,
                is_dir: false,
                size: m.size,
                mtime: m.mtime,
            }],
            false,
        ));
    }

    let base = root.native().to_path_buf();

    let no_ignore = opts.no_ignore;
    let mut walker = WalkBuilder::new(&base);
    walker
        .hidden(false) // 保留点文件
        .git_ignore(!opts.no_ignore)
        .git_global(!opts.no_ignore)
        .git_exclude(!opts.no_ignore)
        .follow_links(opts.follow_links)
        // 目录内按路径稳定排序，达到 limit 立即停止，不再全量收集后排序。
        .sort_by_file_path(|a, b| a.cmp(b))
        .filter_entry(move |e| {
            if no_ignore {
                return true;
            }
            !e.file_name()
                .to_str()
                .map(|n| HARD_SKIP.contains(&n))
                .unwrap_or(false)
        });

    let mut out = Vec::new();
    let mut skipped = 0usize;
    // 达到 limit 后用于“是否还有下一页”的有限探测计数。
    let mut probed = 0usize;
    for entry in walker.build() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue, // 权限不足等直接跳过，不打断整体结果
        };
        let path = entry.path();
        if path == base {
            continue;
        }
        if out.len() >= opts.limit {
            // 已满页：只为确认下一页探测有限个后续原始目录项。
            // 全量走到底会把大仓库里本应即时返回的 glob 拖到时间预算上限。
            if probed >= LOOKAHEAD_ITEMS {
                report.lookahead_limited = true;
                report.stopped = true;
                break;
            }
            probed += 1;
        }
        report.entries_visited += 1;
        // 每 256 项检查一次 deadline：全盘无匹配时也能按时返回。
        if report.entries_visited.is_multiple_of(256) && std::time::Instant::now() > deadline {
            return Ok((out, true));
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
        if skipped < opts.offset {
            skipped += 1;
            report.offset_skipped = skipped;
            continue;
        }
        if out.len() >= opts.limit {
            // 真正观察到下一个匹配条目才报告还有下一页。
            report.stopped = true;
            break;
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
    }

    Ok((out, false))
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
    fn 零时间预算立刻返回不白扫() {
        // 预算为 0 时必须入口就返回：否则要等第一个 256 项检查点，
        // 白扫 256 项，且错误信息会误导成"已访问 256 项"。
        let dir = std::env::temp_dir().join(format!("fsops-zero-budget-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for i in 0..300 {
            fs::write(dir.join(format!("f{i}.txt")), "x").unwrap();
        }
        let root = FsPath::parse(&dir.to_string_lossy()).unwrap();
        let opts = WalkOpts {
            time_budget_ms: 0,
            ..Default::default()
        };
        let (entries, report) = glob_with_report(&root, "**/*.zzz", &opts).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert!(report.timed_out, "应标记超时");
        assert!(entries.is_empty(), "不应返回条目");
        assert!(
            report.entries_visited < 256,
            "预算为 0 时不应白扫 256 项，实际 {} 项",
            report.entries_visited
        );
    }

    #[test]
    fn 花括号内逗号不被当作模式分隔符() {
        // `{a,b}.rs` 是一个带 alternate group 的 glob；
        // 简单 split(',') 会切成 `{a` 和 `b}.rs` 并报解析错误。
        let tmp = std::env::temp_dir().join(format!("fsops-brace-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        for n in ["a.rs", "b.rs", "c.rs"] {
            fs::write(tmp.join(n), "x").unwrap();
        }
        let root = FsPath::parse(&tmp.to_string_lossy()).unwrap();
        let (entries, _) = glob_with_report(&root, "{a,b}.rs", &WalkOpts::default()).unwrap();
        let names: Vec<String> = entries.iter().map(|e| e.path.clone()).collect();
        let _ = fs::remove_dir_all(&tmp);
        assert_eq!(names.len(), 2, "花括号分组应匹配 2 个: {names:?}");
        assert!(names.contains(&"a.rs".to_string()), "{names:?}");
        assert!(names.contains(&"b.rs".to_string()), "{names:?}");
        assert!(!names.contains(&"c.rs".to_string()), "{names:?}");
    }

    #[test]
    fn 竖线写法给出可执行错误() {
        // `|` 是正则写法，在 glob 里是字面量：会静默匹配不到任何文件。
        let tmp = std::env::temp_dir().join(format!("fsops-pipe-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("a.rs"), "x").unwrap();
        let root = FsPath::parse(&tmp.to_string_lossy()).unwrap();
        let result = glob_with_report(&root, "*.rs|*.ts", &WalkOpts::default());
        let _ = fs::remove_dir_all(&tmp);
        let err = match result {
            Ok(_) => panic!("竖线写法应报错而不是静默无匹配"),
            Err(e) => e,
        };
        assert!(err.starts_with("E_BAD_GLOB"), "{err}");
        assert!(err.contains("不支持 `|`"), "{err}");
        assert!(err.contains("逗号分隔"), "应给出正确写法: {err}");
    }

    #[test]
    fn 逗号分隔多个模式() {
        let tmp = std::env::temp_dir().join(format!("fsops-multi-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        for n in ["a.rs", "b.ts", "c.py"] {
            fs::write(tmp.join(n), "x").unwrap();
        }
        let root = FsPath::parse(&tmp.to_string_lossy()).unwrap();
        let (entries, _) = glob_with_report(&root, "*.rs, *.ts", &WalkOpts::default()).unwrap();
        let names: Vec<String> = entries.iter().map(|e| e.path.clone()).collect();
        let _ = fs::remove_dir_all(&tmp);
        assert_eq!(names.len(), 2, "应匹配 rs 和 ts: {names:?}");
    }

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
    fn glob_时间预算耗尽时返回超时标记() {
        let dir = std::env::temp_dir().join(format!("fsops-glob-timeout-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..300 {
            std::fs::write(dir.join(format!("f{i}.txt")), "x").unwrap();
        }
        let fp = crate::paths::FsPath::parse(&dir.to_string_lossy()).unwrap();
        let opts = WalkOpts {
            include_dirs: false,
            limit: usize::MAX,
            follow_links: false,
            offset: 0,
            time_budget_ms: 0,
            no_ignore: false,
        };
        let (entries, report) = glob_with_report(&fp, "**/*.zzz", &opts).unwrap();
        assert!(entries.is_empty());
        assert!(report.timed_out, "时间预算为 0 时应标记超时");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn glob_时间预算耗尽时停止遍历() {
        let dir = std::env::temp_dir().join(format!("fsops-glob-budget-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..500 {
            std::fs::write(dir.join(format!("f{i}.txt")), "x").unwrap();
        }
        let fp = crate::paths::FsPath::parse(&dir.to_string_lossy()).unwrap();
        let opts = WalkOpts {
            include_dirs: false,
            limit: usize::MAX,
            follow_links: false,
            offset: 0,
            time_budget_ms: 0,
            no_ignore: false,
        };
        let (entries, report) = glob_with_report(&fp, "", &opts).unwrap();
        assert!(report.timed_out);
        assert!(
            report.entries_visited < 500,
            "预算为 0 不应遍历全部 {} 项",
            report.entries_visited
        );
        let _ = entries;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn glob_limit提前停止并确认真实后续条目() {
        let tmp = std::env::temp_dir().join(format!("fsops-glob-limit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            fs::write(tmp.join(name), "x").unwrap();
        }

        let root = FsPath::parse(&tmp.to_string_lossy()).unwrap();
        let (got, report) = glob_with_report(
            &root,
            "**/*.txt",
            &WalkOpts {
                limit: 1,
                ..Default::default()
            },
        )
        .unwrap();
        let _ = fs::remove_dir_all(&tmp);

        assert_eq!(got.len(), 1, "返回条目数不符");
        assert!(got[0].path.ends_with(".txt"));
        assert!(report.stopped, "已观察到第二个匹配，应标记停止");
    }

    #[test]
    fn visit无匹配且visitor不调用时也能超时() {
        let dir = std::env::temp_dir().join(format!("fsops-visit-timeout-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for i in 0..600 {
            fs::write(dir.join(format!("f{i}.txt")), "x").unwrap();
        }
        let root = FsPath::parse(&dir.to_string_lossy()).unwrap();
        let mut calls = 0usize;
        let report = visit(
            &root,
            "**/*.zzz",
            &WalkOpts {
                include_dirs: false,
                time_budget_ms: 1,
                ..Default::default()
            },
            |_| {
                calls += 1;
                VisitControl::Continue
            },
        )
        .unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(calls, 0, "无匹配时 visitor 不应调用");
        assert!(report.timed_out, "遍历内部未执行 deadline");
        assert!(report.stopped, "超时应报告停止");
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
    fn 完整遍历即使超过预算也不误报超时() {
        // 遍历已经自然结束：即使耗时超过预算，也不能谎报 timed_out。
        // grep 的 visitor 自己会检查 deadline；walk 只对“没走完”负责。
        let tmp = std::env::temp_dir().join(format!("fsops-visit-done-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            fs::write(tmp.join(name), "x").unwrap();
        }
        let root = FsPath::parse(&tmp.to_string_lossy()).unwrap();
        let report = visit(
            &root,
            "**/*.txt",
            &WalkOpts {
                time_budget_ms: 10,
                ..Default::default()
            },
            |_| {
                std::thread::sleep(std::time::Duration::from_millis(15));
                VisitControl::Continue
            },
        )
        .unwrap();
        let _ = fs::remove_dir_all(&tmp);
        assert_eq!(report.entries_visited, 3, "三项都应访问到");
        assert!(!report.timed_out, "遍历已完整结束，不应误报超时");
        assert!(!report.stopped, "自然结束不是被截断");
    }

    #[test]
    fn glob达到limit后只探测有限后续项() {
        // 1 个匹配 + 大量后续项：不能为了确认“有没有下一页”全量走到底。
        // 上限是 1024 个后续原始目录项；超出后必须给出明确状态而不是假装末页。
        let tmp = std::env::temp_dir().join(format!("fsops-lookahead-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("a_match.txt"), "x").unwrap();
        for i in 0..1200 {
            fs::write(tmp.join(format!("zzz{i:04}.txt")), "x").unwrap();
        }
        let root = FsPath::parse(&tmp.to_string_lossy()).unwrap();
        let (entries, report) = glob_with_report(
            &root,
            "**/a_match.txt",
            &WalkOpts {
                limit: 1,
                ..Default::default()
            },
        )
        .unwrap();
        let _ = fs::remove_dir_all(&tmp);
        assert_eq!(entries.len(), 1);
        assert!(
            report.entries_visited <= 1 + 1024,
            "lookahead 只应探测 1024 个后续项，实际访问 {} 项",
            report.entries_visited
        );
        assert!(report.stopped, "探测到上限应停止并报告");
        assert!(
            report.lookahead_limited,
            "未探测完必须用独立字段说明，不能伪装成末页或超时"
        );
        assert!(!report.timed_out, "这不是时间预算耗尽，不应标 timed_out");
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
