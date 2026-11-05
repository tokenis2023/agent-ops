//! 搜索层：正则 grep，输出 `路径:行号:内容` 的紧凑格式。
//!
//! 和系统 findstr / Select-String 的差别：
//! - 会先按 .gitignore 过滤，不把 node_modules 里的几万行翻出来；
//! - 自动跳过二进制文件与大文件；
//! - 有硬预算，命中太多时只给前 N 条并说明总数。

use crate::paths::FsPath;
use crate::text;
use crate::walk::{self, VisitControl, WalkOpts};
use regex::Regex;

pub struct GrepOpts {
    pub pattern: String,
    pub glob: String,
    pub ignore_case: bool,
    pub context: usize,
    pub limit: usize,
    /// 跳过过滤后的前 N 个命中，用于稳定分页
    pub offset: usize,
    /// 每个文件最多返回的命中数；只过滤展示流，不影响全局 offset
    pub max_per_file: usize,
    pub budget: usize,
    pub max_file_size: u64,
    /// 并行扫描文件；保持与串行相同的稳定输出
    pub parallel: bool,
    /// 时间预算：超时后返回已找到的结果并标注，避免在 C:\ 这种根目录上卡死
    pub time_budget_ms: u128,
}

impl Default for GrepOpts {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            glob: String::new(),
            ignore_case: false,
            context: 0,
            limit: 100,
            offset: 0,
            max_per_file: usize::MAX,
            budget: 8 * 1024,
            max_file_size: 8 * 1024 * 1024,
            parallel: false,
            time_budget_ms: 5000,
        }
    }
}

pub struct Hit {
    pub path: String,
    pub line: usize,
    pub text: String,
    pub is_context: bool,
}

pub struct GrepResult {
    pub hits: Vec<Hit>,
    pub match_count: usize,
    pub files_scanned: usize,
    pub files_matched: usize,
    pub entries_visited: usize,
    /// 被 offset 跳过的命中数；用于区分“本页为空”和“完全无匹配”
    pub offset_skipped: usize,
    /// 被 max_per_file 主动省略的命中数
    pub omitted_per_file: usize,
    pub truncated: bool,
    pub timed_out: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrepOutput {
    Files,
    Count,
    Matches,
    Snippets,
}

pub struct GrepRendered {
    pub text: String,
    pub matches_rendered: usize,
    pub files_rendered: usize,
    pub truncated: bool,
}

pub fn grep(root: &FsPath, opts: &GrepOpts) -> Result<GrepResult, String> {
    let mut pat = opts.pattern.clone();
    if opts.ignore_case {
        pat = format!("(?i){pat}");
    }
    let re = Regex::new(&pat).map_err(|e| format!("E_BAD_REGEX {e}"))?;
    if opts.parallel && root.is_dir() {
        return grep_parallel(root, opts, &re);
    }

    let mut hits = Vec::new();
    let mut match_count = 0usize;
    let mut skipped = 0usize;
    let mut omitted_per_file = 0usize;
    let mut scanned = 0usize;
    let mut matched_files = 0usize;
    let mut truncated = false;
    let mut timed_out = false;
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_millis(opts.time_budget_ms.min(u64::MAX as u128) as u64);
    if std::time::Instant::now() > deadline {
        return Ok(GrepResult {
            hits,
            match_count: 0,
            files_scanned: 0,
            files_matched: 0,
            entries_visited: 0,
            offset_skipped: 0,
            omitted_per_file: 0,
            truncated: true,
            timed_out: true,
        });
    }

    // 目录项也进入回调，因此纯目录遍历同样受时间预算约束。
    let root_is_dir = root.is_dir();
    let report = walk::visit(
        root,
        &opts.glob,
        &WalkOpts {
            include_dirs: true,
            limit: usize::MAX,
            follow_links: false,
        },
        |e| {
            if std::time::Instant::now() > deadline {
                timed_out = true;
                truncated = true;
                return VisitControl::Stop;
            }
            if e.is_dir || e.size == 0 || e.size > opts.max_file_size {
                return VisitControl::Continue;
            }
            // e.path 是相对搜索根的路径；根为单文件时直接使用根路径。
            let full = if root_is_dir {
                format!("{}/{}", root.display().trim_end_matches('/'), e.path)
            } else {
                root.display()
            };
            let fp = match FsPath::parse(&full) {
                Ok(p) => p,
                Err(_) => return VisitControl::Continue,
            };
            let bytes = match crate::io::read_bytes(&fp.wide()) {
                Ok(b) => b,
                Err(_) => return VisitControl::Continue,
            };
            if std::time::Instant::now() > deadline {
                timed_out = true;
                truncated = true;
                return VisitControl::Stop;
            }
            // 二进制判定：前 8KB 里有 NUL
            if bytes[..bytes.len().min(8192)].contains(&0) {
                return VisitControl::Continue;
            }
            scanned += 1;
            let doc = text::decode(&bytes);
            let lines: Vec<&str> = doc.text.split('\n').collect();
            let mut eligible_in_file = 0usize;
            let mut selected = Vec::new();

            for (i, raw) in lines.iter().enumerate() {
                // 大文件逐行扫描内部也检查 deadline，避免单文件绕过时间预算。
                if i % 256 == 0 && std::time::Instant::now() > deadline {
                    timed_out = true;
                    truncated = true;
                    break;
                }
                let line = raw.strip_suffix('\r').unwrap_or(raw);
                if !re.is_match(line) {
                    continue;
                }
                eligible_in_file += 1;
                if eligible_in_file > opts.max_per_file {
                    omitted_per_file += 1;
                    continue;
                }
                if skipped < opts.offset {
                    skipped += 1;
                    continue;
                }
                if match_count >= opts.limit {
                    truncated = true;
                    break;
                }
                selected.push(i);
                match_count += 1;
            }

            if !selected.is_empty() {
                matched_files += 1;
                // 先标记所有命中及其上下文，再按行号输出；相邻命中不会重复上下文。
                let mut in_snippet = vec![false; lines.len()];
                let mut is_match = vec![false; lines.len()];
                for &index in &selected {
                    is_match[index] = true;
                    let from = index.saturating_sub(opts.context);
                    let to = (index + opts.context).min(lines.len() - 1);
                    for mark in &mut in_snippet[from..=to] {
                        *mark = true;
                    }
                }
                let first = selected[0].saturating_sub(opts.context);
                let last = (selected[selected.len() - 1] + opts.context).min(lines.len() - 1);
                for index in first..=last {
                    if !in_snippet[index] {
                        continue;
                    }
                    hits.push(Hit {
                        path: e.path.clone(),
                        line: index + 1,
                        text: trim_line(lines[index]),
                        is_context: !is_match[index],
                    });
                }
            }
            if truncated || timed_out {
                VisitControl::Stop
            } else {
                VisitControl::Continue
            }
        },
    )?;

    Ok(GrepResult {
        hits,
        match_count,
        files_scanned: scanned,
        files_matched: matched_files,
        entries_visited: report.entries_visited,
        offset_skipped: skipped,
        omitted_per_file,
        truncated,
        timed_out,
    })
}

struct ParallelFileScan {
    path: String,
    /// (行号 1 基, 内容, 是否为命中行)
    entries: Vec<(usize, String, bool)>,
    omitted: usize,
    has_more: bool,
    scanned: bool,
    timed_out: bool,
}

fn scan_parallel_file(
    root: &FsPath,
    entry: &walk::Entry,
    re: &Regex,
    opts: &GrepOpts,
    materialize_cap: usize,
    deadline: std::time::Instant,
) -> ParallelFileScan {
    let mut out = ParallelFileScan {
        path: entry.path.clone(),
        entries: Vec::new(),
        omitted: 0,
        has_more: false,
        scanned: false,
        timed_out: false,
    };
    if std::time::Instant::now() > deadline {
        out.timed_out = true;
        return out;
    }
    let full = format!("{}/{}", root.display().trim_end_matches('/'), entry.path);
    let fp = match FsPath::parse(&full) {
        Ok(fp) => fp,
        Err(_) => return out,
    };
    let bytes = match crate::io::read_bytes(&fp.wide()) {
        Ok(bytes) => bytes,
        Err(_) => return out,
    };
    if std::time::Instant::now() > deadline {
        out.timed_out = true;
        return out;
    }
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return out;
    }
    out.scanned = true;
    let doc = text::decode(&bytes);
    let lines: Vec<&str> = doc.text.split('\n').collect();
    let mut eligible = 0usize;
    let mut selected = Vec::new();
    for (index, raw) in lines.iter().enumerate() {
        if index % 256 == 0 && std::time::Instant::now() > deadline {
            out.timed_out = true;
            break;
        }
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if !re.is_match(line) {
            continue;
        }
        eligible += 1;
        if eligible > opts.max_per_file {
            out.omitted += 1;
            continue;
        }
        if selected.len() < materialize_cap {
            selected.push(index);
        } else {
            out.has_more = true;
        }
    }

    if !selected.is_empty() {
        let mut in_snippet = vec![false; lines.len()];
        let mut is_match = vec![false; lines.len()];
        for &index in &selected {
            is_match[index] = true;
            let from = index.saturating_sub(opts.context);
            let to = (index + opts.context).min(lines.len() - 1);
            for mark in &mut in_snippet[from..=to] {
                *mark = true;
            }
        }
        let first = selected[0].saturating_sub(opts.context);
        let last = (selected[selected.len() - 1] + opts.context).min(lines.len() - 1);
        for index in first..=last {
            if in_snippet[index] {
                out.entries
                    .push((index + 1, trim_line(lines[index]), is_match[index]));
            }
        }
    }
    out
}

/// 按路径顺序分块并行扫描；先并行算一批，再按原顺序合并，保证分页稳定。
fn grep_parallel(root: &FsPath, opts: &GrepOpts, re: &Regex) -> Result<GrepResult, String> {
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_millis(opts.time_budget_ms.min(u64::MAX as u128) as u64);
    if std::time::Instant::now() > deadline {
        return Ok(GrepResult {
            hits: Vec::new(),
            match_count: 0,
            files_scanned: 0,
            files_matched: 0,
            entries_visited: 0,
            offset_skipped: 0,
            omitted_per_file: 0,
            truncated: true,
            timed_out: true,
        });
    }

    let mut entries = Vec::new();
    let mut timed_out = false;
    let report = walk::visit(
        root,
        &opts.glob,
        &WalkOpts {
            include_dirs: false,
            limit: 100_000,
            follow_links: false,
        },
        |entry| {
            if std::time::Instant::now() > deadline {
                timed_out = true;
                return VisitControl::Stop;
            }
            if entry.size > 0 && entry.size <= opts.max_file_size {
                entries.push(entry);
            }
            VisitControl::Continue
        },
    )?;

    let threads = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(4)
        .clamp(1, 8);
    let chunk_size = 16usize;
    let wave_files = threads * chunk_size;
    let materialize_cap = opts
        .offset
        .saturating_add(opts.limit)
        .saturating_add(1)
        .min(opts.max_per_file);

    let mut hits = Vec::new();
    let mut match_count = 0usize;
    let mut skipped = 0usize;
    let mut omitted_per_file = 0usize;
    let mut scanned = 0usize;
    let mut matched_files = 0usize;
    let mut truncated = false;
    let mut position = 0usize;

    while position < entries.len() {
        let wave_end = (position + wave_files).min(entries.len());
        let scans = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for start in (position..wave_end).step_by(chunk_size) {
                let end = (start + chunk_size).min(wave_end);
                let chunk = &entries[start..end];
                handles.push(scope.spawn(move || {
                    chunk
                        .iter()
                        .map(|entry| {
                            scan_parallel_file(root, entry, re, opts, materialize_cap, deadline)
                        })
                        .collect::<Vec<_>>()
                }));
            }
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });

        for chunk in scans {
            for scan in chunk {
                scanned += usize::from(scan.scanned);
                omitted_per_file += scan.omitted;
                timed_out |= scan.timed_out;
                if scan.scanned && !scan.entries.is_empty() {
                    let mut selected_lines = Vec::new();
                    for (line, _, is_match) in &scan.entries {
                        if !is_match {
                            continue;
                        }
                        if skipped < opts.offset {
                            skipped += 1;
                            continue;
                        }
                        if match_count >= opts.limit {
                            truncated = true;
                            break;
                        }
                        selected_lines.push(*line);
                        match_count += 1;
                    }
                    if !selected_lines.is_empty() {
                        matched_files += 1;
                        for (line, body, _) in &scan.entries {
                            let is_selected = selected_lines.contains(line);
                            let is_context = !is_selected
                                && selected_lines
                                    .iter()
                                    .any(|selected| line.abs_diff(*selected) <= opts.context);
                            if is_selected || is_context {
                                hits.push(Hit {
                                    path: scan.path.clone(),
                                    line: *line,
                                    text: body.clone(),
                                    is_context,
                                });
                            }
                        }
                    }
                    if scan.has_more && match_count >= opts.limit {
                        truncated = true;
                    }
                }
                if truncated || timed_out {
                    break;
                }
            }
            if truncated || timed_out {
                break;
            }
        }
        if truncated || timed_out {
            break;
        }
        if match_count >= opts.limit && wave_end < entries.len() {
            truncated = true;
            break;
        }
        position = wave_end;
    }

    Ok(GrepResult {
        hits,
        match_count,
        files_scanned: scanned,
        files_matched: matched_files,
        entries_visited: report.entries_visited,
        offset_skipped: skipped,
        omitted_per_file,
        truncated,
        timed_out,
    })
}

fn trim_line(s: &str) -> String {
    let t = s.trim_end();
    if t.chars().count() > 200 {
        format!("{}…", t.chars().take(200).collect::<String>())
    } else {
        t.to_string()
    }
}

/// 按文件分组渲染：路径只出现一次。
pub fn render(r: &GrepResult) -> String {
    let mut out = String::with_capacity(r.hits.len() * 48);
    let mut last_path = String::new();
    for h in &r.hits {
        if h.path != last_path {
            if !last_path.is_empty() {
                out.push('\n');
            }
            out.push_str(&h.path);
            out.push('\n');
            last_path = h.path.clone();
        }
        let sep = if h.is_context { "-" } else { ":" };
        out.push_str(&format!("{}{}{}\n", h.line, sep, h.text));
    }
    out
}

/// 在给定字节预算内按输出模式渲染；预算包含路径分组头与换行。
pub fn render_mode(r: &GrepResult, output: GrepOutput, budget: usize) -> GrepRendered {
    let mut text = String::new();
    let mut matches_rendered = 0usize;
    let mut files_rendered = 0usize;
    let mut truncated = false;
    match output {
        GrepOutput::Files => {
            let mut i = 0usize;
            while i < r.hits.len() {
                let path = &r.hits[i].path;
                let mut j = i;
                while j < r.hits.len() && &r.hits[j].path == path {
                    j += 1;
                }
                let line = format!("{path}\n");
                if text.len() + line.len() > budget {
                    truncated = true;
                    break;
                }
                text.push_str(&line);
                files_rendered += 1;
                matches_rendered += r.hits[i..j].iter().filter(|hit| !hit.is_context).count();
                i = j;
            }
        }
        GrepOutput::Count => {
            let mut i = 0usize;
            while i < r.hits.len() {
                let path = &r.hits[i].path;
                let mut j = i;
                let mut count = 0usize;
                while j < r.hits.len() && &r.hits[j].path == path {
                    if !r.hits[j].is_context {
                        count += 1;
                    }
                    j += 1;
                }
                let line = format!("{path}:{count}\n");
                if text.len() + line.len() > budget {
                    truncated = true;
                    break;
                }
                text.push_str(&line);
                files_rendered += 1;
                matches_rendered += count;
                i = j;
            }
        }
        GrepOutput::Matches => {
            let (body, matches, files, cut) = render_line_hits(r, budget);
            text = body;
            matches_rendered = matches;
            files_rendered = files;
            truncated = cut;
        }
        GrepOutput::Snippets => {
            let (body, matches, files, cut) = render_line_hits(r, budget);
            text = body;
            matches_rendered = matches;
            files_rendered = files;
            truncated = cut;
        }
    }
    GrepRendered {
        text,
        matches_rendered,
        files_rendered,
        truncated,
    }
}

/// 渲染逐行命中；路径分组头、命中行、上下文行与换行共享同一字节预算。
fn render_line_hits(r: &GrepResult, budget: usize) -> (String, usize, usize, bool) {
    let mut text = String::new();
    let mut matches_rendered = 0usize;
    let mut files_rendered = 0usize;
    let mut truncated = false;
    let mut last_path = "";
    for hit in &r.hits {
        if hit.path != last_path {
            let prefix = if text.is_empty() { "" } else { "\n" };
            let header = format!("{prefix}{}\n", hit.path);
            if text.len() + header.len() > budget {
                truncated = true;
                break;
            }
            text.push_str(&header);
            files_rendered += 1;
            last_path = &hit.path;
        }
        let sep = if hit.is_context { "-" } else { ":" };
        let line = format!("{}{}{}\n", hit.line, sep, hit.text);
        if text.len() + line.len() > budget {
            truncated = true;
            break;
        }
        text.push_str(&line);
        if !hit.is_context {
            matches_rendered += 1;
        }
    }
    (text, matches_rendered, files_rendered, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn 搜索命中并按文件分组() {
        let tmp = std::env::temp_dir().join(format!("fsops-grep-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("a.txt"), "fn main() {\n    let x = 1;\n}\n").unwrap();
        fs::write(tmp.join("b.txt"), "nothing here\n").unwrap();

        let root = FsPath::parse(&tmp.to_string_lossy()).unwrap();
        let r = grep(
            &root,
            &GrepOpts {
                pattern: "let x".into(),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(r.hits.len(), 1);
        assert_eq!(r.hits[0].line, 2);
        let text = render(&r);
        assert!(text.contains("2:    let x = 1;"));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn 并行搜索与串行结果一致() {
        let tmp = std::env::temp_dir().join(format!("fsops-grep-parallel-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("src")).unwrap();
        for index in 0..20 {
            let dir = if index % 2 == 0 {
                &tmp
            } else {
                &tmp.join("src")
            };
            fs::write(
                dir.join(format!("f{index:02}.txt")),
                format!("before{index}\nneedle-{index}\nctx-a\nctx-b\nneedle-{index}\n"),
            )
            .unwrap();
        }
        let root = FsPath::parse(&tmp.to_string_lossy()).unwrap();
        let opts = GrepOpts {
            pattern: "needle".into(),
            context: 1,
            limit: 8,
            offset: 2,
            max_per_file: 2,
            ..Default::default()
        };

        let serial = grep(&root, &opts).unwrap();
        let parallel = grep(
            &root,
            &GrepOpts {
                parallel: true,
                ..opts
            },
        )
        .unwrap();
        let _ = fs::remove_dir_all(&tmp);

        assert_eq!(parallel.match_count, serial.match_count);
        assert_eq!(parallel.offset_skipped, serial.offset_skipped);
        assert_eq!(parallel.omitted_per_file, serial.omitted_per_file);
        assert_eq!(parallel.truncated, serial.truncated);
        let serial_hits: Vec<_> = serial
            .hits
            .iter()
            .map(|hit| (&hit.path, hit.line, &hit.text, hit.is_context))
            .collect();
        let parallel_hits: Vec<_> = parallel
            .hits
            .iter()
            .map(|hit| (&hit.path, hit.line, &hit.text, hit.is_context))
            .collect();
        assert_eq!(parallel_hits, serial_hits, "并行与串行输出不一致");
    }

    #[test]
    fn 搜索达到命中上限后不再遍历后续文件() {
        let tmp = std::env::temp_dir().join(format!("fsops-grep-stream-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("a.txt"), "needle\nneedle\n").unwrap();
        fs::write(tmp.join("b.txt"), "needle\n").unwrap();
        fs::write(tmp.join("c.txt"), "needle\n").unwrap();

        let root = FsPath::parse(&tmp.to_string_lossy()).unwrap();
        let r = grep(
            &root,
            &GrepOpts {
                pattern: "needle".into(),
                limit: 1,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(r.files_scanned, 1);
        assert_eq!(r.entries_visited, 1, "达到 limit 后仍访问了后续目录项");
        assert!(r.truncated);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn 命中计数独立于上下文行() {
        let tmp = std::env::temp_dir().join(format!("fsops-grep-count-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("a.txt"), "needle\nbefore\nneedle\nafter\nneedle\n").unwrap();

        let root = FsPath::parse(&tmp.to_string_lossy()).unwrap();
        let r = grep(
            &root,
            &GrepOpts {
                pattern: "needle".into(),
                context: 1,
                limit: 10,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(r.match_count, 3);
        assert!(r.hits.len() > r.match_count);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn 相邻命中的snippets上下文不重复且保持行序() {
        let tmp = std::env::temp_dir().join(format!("fsops-grep-ctx-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("a.txt"), "alpha\nneedle1\nneedle2\nomega\n").unwrap();

        let root = FsPath::parse(&tmp.to_string_lossy()).unwrap();
        let r = grep(
            &root,
            &GrepOpts {
                pattern: "needle".into(),
                context: 1,
                limit: 10,
                ..Default::default()
            },
        )
        .unwrap();

        let lines: Vec<usize> = r.hits.iter().map(|hit| hit.line).collect();
        assert_eq!(lines, vec![1, 2, 3, 4], "上下文重复或乱序");
        assert_eq!(
            r.hits.iter().map(|hit| hit.is_context).collect::<Vec<_>>(),
            vec![true, false, false, true]
        );
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn files_模式每个文件只输出路径() {
        let r = GrepResult {
            hits: vec![
                Hit {
                    path: "a.txt".into(),
                    line: 1,
                    text: "needle".into(),
                    is_context: false,
                },
                Hit {
                    path: "a.txt".into(),
                    line: 3,
                    text: "needle".into(),
                    is_context: false,
                },
                Hit {
                    path: "b.txt".into(),
                    line: 2,
                    text: "needle".into(),
                    is_context: false,
                },
            ],
            match_count: 3,
            files_scanned: 2,
            files_matched: 2,
            entries_visited: 2,
            offset_skipped: 0,
            omitted_per_file: 0,
            truncated: false,
            timed_out: false,
        };

        let rendered = render_mode(&r, GrepOutput::Files, 4096);
        assert_eq!(rendered.text, "a.txt\nb.txt\n");
        assert_eq!(rendered.matches_rendered, 3);
    }

    #[test]
    fn count_模式按文件给出命中数() {
        let r = GrepResult {
            hits: vec![
                Hit {
                    path: "a.txt".into(),
                    line: 1,
                    text: "needle".into(),
                    is_context: false,
                },
                Hit {
                    path: "a.txt".into(),
                    line: 3,
                    text: "needle".into(),
                    is_context: false,
                },
                Hit {
                    path: "b.txt".into(),
                    line: 2,
                    text: "needle".into(),
                    is_context: false,
                },
            ],
            match_count: 3,
            files_scanned: 2,
            files_matched: 2,
            entries_visited: 2,
            offset_skipped: 0,
            omitted_per_file: 0,
            truncated: false,
            timed_out: false,
        };

        let rendered = render_mode(&r, GrepOutput::Count, 4096);
        assert_eq!(rendered.text, "a.txt:2\nb.txt:1\n");
        assert_eq!(rendered.files_rendered, 2);
        assert_eq!(rendered.matches_rendered, 3);
    }

    #[test]
    fn matches_模式保留命中与上下文() {
        let r = 带上下文结果();

        let rendered = render_mode(&r, GrepOutput::Matches, 4096);
        assert_eq!(
            rendered.text,
            "a.txt\n1-before\n2:needle\n3-after\n\nb.txt\n2:needle\n"
        );
        assert_eq!(rendered.matches_rendered, 2);
        assert_eq!(rendered.files_rendered, 2);
    }

    #[test]
    fn snippets_模式输出上下文行() {
        let r = 带上下文结果();

        let rendered = render_mode(&r, GrepOutput::Snippets, 4096);
        assert_eq!(
            rendered.text,
            "a.txt\n1-before\n2:needle\n3-after\n\nb.txt\n2:needle\n"
        );
        assert_eq!(rendered.matches_rendered, 2);
    }

    #[test]
    fn 每文件命中数受max_per_file限制() {
        let tmp = std::env::temp_dir().join(format!("fsops-grep-max-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("a.txt"), "needle\nneedle\nneedle\nneedle\n").unwrap();
        fs::write(tmp.join("b.txt"), "needle\nneedle\nneedle\n").unwrap();

        let root = FsPath::parse(&tmp.to_string_lossy()).unwrap();
        let r = grep(
            &root,
            &GrepOpts {
                pattern: "needle".into(),
                limit: 10,
                max_per_file: 2,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(r.match_count, 4);
        let by_file: Vec<(&str, usize)> = ["a.txt", "b.txt"]
            .into_iter()
            .map(|path| {
                (
                    path,
                    r.hits
                        .iter()
                        .filter(|hit| !hit.is_context && hit.path == path)
                        .count(),
                )
            })
            .collect();
        assert_eq!(by_file, vec![("a.txt", 2), ("b.txt", 2)]);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn offset_跳过单文件上限过滤后的全局命中() {
        let tmp = std::env::temp_dir().join(format!("fsops-grep-offset-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("a.txt"), "needle\nneedle\nneedle\nneedle\n").unwrap();
        fs::write(tmp.join("b.txt"), "needle\nneedle\nneedle\n").unwrap();

        let root = FsPath::parse(&tmp.to_string_lossy()).unwrap();
        let r = grep(
            &root,
            &GrepOpts {
                pattern: "needle".into(),
                limit: 10,
                max_per_file: 2,
                offset: 1,
                ..Default::default()
            },
        )
        .unwrap();

        let matches: Vec<(&str, usize)> = r
            .hits
            .iter()
            .filter(|hit| !hit.is_context)
            .map(|hit| (hit.path.as_str(), hit.line))
            .collect();
        assert_eq!(matches, vec![("a.txt", 2), ("b.txt", 1), ("b.txt", 2)]);
        assert_eq!(r.match_count, 3);
        let _ = fs::remove_dir_all(&tmp);
    }

    fn 带上下文结果() -> GrepResult {
        GrepResult {
            hits: vec![
                Hit {
                    path: "a.txt".into(),
                    line: 1,
                    text: "before".into(),
                    is_context: true,
                },
                Hit {
                    path: "a.txt".into(),
                    line: 2,
                    text: "needle".into(),
                    is_context: false,
                },
                Hit {
                    path: "a.txt".into(),
                    line: 3,
                    text: "after".into(),
                    is_context: true,
                },
                Hit {
                    path: "b.txt".into(),
                    line: 2,
                    text: "needle".into(),
                    is_context: false,
                },
            ],
            match_count: 2,
            files_scanned: 2,
            files_matched: 2,
            entries_visited: 2,
            offset_skipped: 0,
            omitted_per_file: 0,
            truncated: false,
            timed_out: false,
        }
    }
}
