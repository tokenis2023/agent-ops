//! 搜索层：正则 grep，输出 `路径:行号:内容` 的紧凑格式。
//!
//! 和系统 findstr / Select-String 的差别：
//! - 会先按 .gitignore 过滤，不把 node_modules 里的几万行翻出来；
//! - 自动跳过二进制文件与大文件；DOCX 先抽取正文再搜索。
//! - 有硬预算，命中太多时只给前 N 条并说明总数。

use crate::paths::FsPath;
use crate::text;
use crate::walk::{self, VisitControl, WalkOpts};
use regex::Regex;

pub struct GrepOpts {
    pub pattern: String,
    /// 输出模式进入执行层：files/count 不能复用逐命中截断语义。
    pub output: GrepOutput,
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
    /// 忽略 .gitignore 与 node_modules/target 等硬跳过目录；用于查依赖源码或构建产物
    pub no_ignore: bool,
}

impl Default for GrepOpts {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            output: GrepOutput::Matches,
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
            no_ignore: false,
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
    /// count 模式的每文件聚合；不物化逐行命中正文。
    pub file_counts: Vec<(String, usize)>,
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
    /// 因超过 max_file_size 被跳过的文件数；必须回报，否则大文件命中会变成假阴性。
    pub files_skipped_large: usize,
    /// 被跳过的最大文件体积，用于提示调整上限。
    pub largest_skipped: u64,
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

/// 判断正则是否会在"任意文本的每一行"都命中（即能匹配空位置）。
///
/// `fs_grep` 与 `fs_edit pattern` 共用：两边都必须拒绝这类模式，否则
/// 搜索会返回零信息量的满屏命中、编辑会把每个空位置都替换一遍。
/// 只测空串不够：`\b`、`\B` 在空串上不匹配（空串没有词边界），
/// 但在真实文本里每个词边界都命中。所以用多段样本，只要**非空样本全部命中**
/// 且该模式确实能在无内容处匹配，就判定为"无筛选价值"。
pub(crate) fn matches_everywhere(re: &Regex) -> bool {
    // 空串命中 → `^`、`$`、`a*`、`.*`、`\d*` 这类，直接判定
    if re.is_match("") {
        return true;
    }
    // 非空样本：要求每一段都命中（`\b`、`\B` 满足）
    const SAMPLES: [&str; 3] = ["hello world", "中文文本", "123 abc !!!"];
    SAMPLES.iter().all(|s| re.is_match(s))
}

pub fn grep(root: &FsPath, opts: &GrepOpts) -> Result<GrepResult, String> {
    let mut pat = opts.pattern.clone();
    if opts.ignore_case {
        pat = format!("(?i){pat}");
    }
    let re = Regex::new(&pat).map_err(|e| {
        let mut msg = format!("E_BAD_REGEX {e}");
        // 模型常把 glob 当正则写（`*.txt`），直接点明并给出正确的等价写法。
        if opts.pattern.contains('*') && !opts.pattern.contains(".*") {
            let hint = opts.pattern.replace('.', "\\.").replace('*', ".*");
            msg.push_str(&format!(
                "\n提示：pattern 是正则不是 glob。`{}` 的等价写法是 `{}`；\
                 按文件名筛选请用 fs_glob 的 pattern 参数。",
                opts.pattern, hint
            ));
        }
        msg
    })?;
    // 能匹配空串的模式（`^`、`$`、`a*`、`.*`、`\b`、`\d*`）会命中每一行：
    // 返回 100 行"命中"但没任何信息量，还会让模型误以为文件内容真含该串。
    //
    // 判据不能只看 `is_match("")`：`\b` 在空串上不匹配（没有词边界），
    // 但在任意正常文本里每个词边界都命中。用几段代表性样本做实测更可靠。
    if matches_everywhere(&re) {
        return Err(format!(
            "E_BAD_REGEX pattern `{}` 能匹配空位置，会命中每一行而没有信息量；\
             请给出至少一个具体字符，如 `TODO`、`fn main`，或加锚点 `^fn`",
            opts.pattern
        ));
    }
    if opts.parallel && root.is_dir() {
        return grep_parallel(root, opts, &re);
    }

    let mut hits = Vec::new();
    let mut file_counts = Vec::new();
    let mut match_count = 0usize;
    let mut skipped = 0usize;
    let mut omitted_per_file = 0usize;
    let mut scanned = 0usize;
    let mut matched_files = 0usize;
    let mut truncated = false;
    let mut timed_out = false;
    let mut files_skipped_large = 0usize;
    let mut largest_skipped = 0u64;
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_millis(opts.time_budget_ms.min(u64::MAX as u128) as u64);
    if std::time::Instant::now() > deadline {
        return Ok(GrepResult {
            hits,
            file_counts,
            match_count: 0,
            files_scanned: 0,
            files_matched: 0,
            entries_visited: 0,
            offset_skipped: 0,
            omitted_per_file: 0,
            truncated: true,
            timed_out: true,
            files_skipped_large: 0,
            largest_skipped: 0,
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
            offset: 0,
            time_budget_ms: opts.time_budget_ms,
            no_ignore: opts.no_ignore,
        },
        |e| {
            if std::time::Instant::now() > deadline {
                timed_out = true;
                truncated = true;
                return VisitControl::Stop;
            }
            if e.is_dir || e.size == 0 {
                return VisitControl::Continue;
            }
            if e.size > opts.max_file_size {
                files_skipped_large += 1;
                largest_skipped = largest_skipped.max(e.size);
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
            // 二进制判定、文档抽取、末尾换行处理统一走 load_search_text，
            // 保证串行与并行结果一致。
            scanned += 1;
            let body = match load_search_text(&fp) {
                Some(b) => b,
                None => return VisitControl::Continue,
            };
            let lines = split_search_lines(&body);
            if std::time::Instant::now() > deadline {
                timed_out = true;
                truncated = true;
                return VisitControl::Stop;
            }
            let mut eligible_in_file = 0usize;
            let mut file_count = 0usize;
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
                // max_per_file 是“每文件可见命中数”，对 files/count/matches 一律生效。
                // 放在模式分支之前，避免 files/count 绕过它（串行/并行必须一致）。
                eligible_in_file += 1;
                if eligible_in_file > opts.max_per_file {
                    omitted_per_file += 1;
                    continue;
                }
                if opts.output == GrepOutput::Count {
                    if skipped < opts.offset {
                        skipped += 1;
                        continue;
                    }
                    if match_count >= opts.limit {
                        truncated = true;
                        break;
                    }
                    match_count += 1;
                    file_count += 1;
                    continue;
                }
                if opts.output == GrepOutput::Files {
                    if skipped < opts.offset {
                        skipped += 1;
                    } else if match_count >= opts.limit {
                        truncated = true;
                    } else {
                        selected.push(i);
                        match_count += 1;
                    }
                    // files 的分页单位是文件；本文件其余命中不影响下一页。
                    break;
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

            if opts.output == GrepOutput::Count && file_count > 0 {
                matched_files += 1;
                file_counts.push((e.path.clone(), file_count));
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
                        // 命中行按命中位置开窗；上下文行没有命中，从行首截断。
                        text: trim_line_at(lines[index], is_match[index].then_some(&re)),
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

    // walk 在 glob 过滤前就检查 deadline：visitor 可能一次都没被调用。
    // 不折算这个报告，就会把“遍历超时”报成“无命中”（假阴性）。
    if report.timed_out {
        timed_out = true;
        truncated = true;
    }

    Ok(GrepResult {
        hits,
        file_counts,
        match_count,
        files_scanned: scanned,
        files_matched: matched_files,
        entries_visited: report.entries_visited,
        offset_skipped: skipped,
        omitted_per_file,
        truncated,
        timed_out,
        files_skipped_large,
        largest_skipped,
    })
}

struct ParallelFileScan {
    path: String,
    /// (行号 1 基, 内容, 是否为命中行)
    entries: Vec<(usize, String, bool)>,
    match_count: usize,
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
        match_count: 0,
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
    // 二进制判定、文档抽取、末尾换行处理统一走 load_search_text，
    // 与串行路径保持完全一致。
    out.scanned = true;
    let body = match load_search_text(&fp) {
        Some(b) => b,
        None => return out,
    };
    let lines = split_search_lines(&body);
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
        // 与串行路径一致：max_per_file 先于 files/count 分支生效。
        eligible += 1;
        if eligible > opts.max_per_file {
            out.omitted += 1;
            continue;
        }
        if opts.output == GrepOutput::Count {
            out.match_count += 1;
            continue;
        }
        if selected.len() < materialize_cap {
            selected.push(index);
            if opts.output == GrepOutput::Files {
                // 每个文件只需首条命中，避免高密度文件挤占全局 limit。
                break;
            }
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
                out.entries.push((
                    index + 1,
                    trim_line_at(lines[index], is_match[index].then_some(re)),
                    is_match[index],
                ));
            }
        }
    }
    out
}

/// 并行合并时的全局累计状态：命中、分页偏移、截断与超时标记。
struct MergeState {
    hits: Vec<Hit>,
    file_counts: Vec<(String, usize)>,
    match_count: usize,
    skipped: usize,
    omitted_per_file: usize,
    scanned: usize,
    matched_files: usize,
    truncated: bool,
    timed_out: bool,
    files_skipped_large: usize,
    largest_skipped: u64,
}

impl MergeState {
    fn new() -> Self {
        Self {
            hits: Vec::new(),
            file_counts: Vec::new(),
            match_count: 0,
            skipped: 0,
            omitted_per_file: 0,
            scanned: 0,
            matched_files: 0,
            truncated: false,
            timed_out: false,
            files_skipped_large: 0,
            largest_skipped: 0,
        }
    }

    /// 按路径顺序合并一批扫描结果；返回 false 表示应停止遍历（超时或已截断）。
    fn merge(&mut self, scans: Vec<ParallelFileScan>, opts: &GrepOpts) -> bool {
        for scan in scans {
            self.scanned += usize::from(scan.scanned);
            self.timed_out |= scan.timed_out;
            // 单文件在超时处停下：它的 count 数字只是本页可见值，
            // 必须标记 truncated，不能让模型读成文件总数。
            if scan.timed_out {
                self.truncated = true;
            }
            // 本 scan 处理前是否已经截断；用于判定是不是“这个 scan 刚触发满页”。
            let truncated_before = self.truncated;
            if scan.scanned && opts.output == GrepOutput::Count && scan.match_count > 0 {
                let mut visible = 0usize;
                for _ in 0..scan.match_count {
                    if self.skipped < opts.offset {
                        self.skipped += 1;
                        continue;
                    }
                    if self.match_count >= opts.limit {
                        self.truncated = true;
                        break;
                    }
                    self.match_count += 1;
                    visible += 1;
                }
                if visible > 0 {
                    self.matched_files += 1;
                    self.file_counts.push((scan.path.clone(), visible));
                }
            } else if scan.scanned && !scan.entries.is_empty() {
                let mut selected_lines = Vec::new();
                for (line, _, is_match) in &scan.entries {
                    if !is_match {
                        continue;
                    }
                    if self.skipped < opts.offset {
                        self.skipped += 1;
                        continue;
                    }
                    if self.match_count >= opts.limit {
                        self.truncated = true;
                        break;
                    }
                    selected_lines.push(*line);
                    self.match_count += 1;
                }
                if !selected_lines.is_empty() {
                    self.matched_files += 1;
                    for (line, body, _) in &scan.entries {
                        let is_selected = selected_lines.contains(line);
                        let is_context = !is_selected
                            && selected_lines
                                .iter()
                                .any(|selected| line.abs_diff(*selected) <= opts.context);
                        if is_selected || is_context {
                            self.hits.push(Hit {
                                path: scan.path.clone(),
                                line: *line,
                                text: body.clone(),
                                is_context,
                            });
                        }
                    }
                }
                if scan.has_more && self.match_count >= opts.limit {
                    self.truncated = true;
                }
            }
            // 并行侧对整文件累计 omitted，串行侧一旦命中满页就 break：
            // 那个文件的剩余行（被 max_per_file 省略的部分）串行根本没数到。
            // 论证：omitted 只可能出现在第 max_per_file+1 个及之后的命中；
            // 满页截断只能发生在第 <= max_per_file 个命中上，串行执行到该行即退出，
            // 所以“刚触发截断的文件”对串行 omitted 的贡献恒为 0。
            // 尚未触发截断的文件必须照常累计，否则会漏报省略数。
            if !(self.truncated && !truncated_before) {
                self.omitted_per_file += scan.omitted;
            }
            if self.truncated || self.timed_out {
                return false;
            }
        }
        true
    }

    fn into_result(
        self,
        entries_visited: usize,
        report_timed_out: bool,
        report_stopped: bool,
    ) -> GrepResult {
        GrepResult {
            hits: self.hits,
            file_counts: self.file_counts,
            match_count: self.match_count,
            files_scanned: self.scanned,
            files_matched: self.matched_files,
            entries_visited,
            offset_skipped: self.skipped,
            omitted_per_file: self.omitted_per_file,
            // walk 的 glob 过滤可能让 visitor 一次都不被调用；超时必须在结果里体现。
            // 候选上限（WalkOpts.limit=100k）撞到时 walk 会置 stopped，
            // 不合并就会把“没扫完”渲染成完整结果。
            truncated: self.truncated || report_timed_out || report_stopped,
            timed_out: self.timed_out || report_timed_out,
            files_skipped_large: self.files_skipped_large,
            largest_skipped: self.largest_skipped,
        }
    }
}

/// 扫描一个波次的文件：按 chunk 并行、按原顺序返回结果。
fn scan_wave(
    root: &FsPath,
    wave: &[walk::Entry],
    re: &Regex,
    opts: &GrepOpts,
    materialize_cap: usize,
    deadline: std::time::Instant,
    chunk_size: usize,
) -> Vec<ParallelFileScan> {
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for chunk in wave.chunks(chunk_size) {
            handles.push(scope.spawn(move || {
                chunk
                    .iter()
                    .map(|entry| {
                        scan_parallel_file(root, entry, re, opts, materialize_cap, deadline)
                    })
                    .collect::<Vec<_>>()
            }));
        }
        let mut out = Vec::with_capacity(wave.len());
        for handle in handles {
            out.extend(handle.join().unwrap());
        }
        out
    })
}

/// 按路径顺序分波并行扫描：遍历回调攒满一波就立即扫描并按序合并，
/// 满页或超时立刻停止遍历，内存上限是“一个波次”而不是全部候选文件。
fn grep_parallel(root: &FsPath, opts: &GrepOpts, re: &Regex) -> Result<GrepResult, String> {
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_millis(opts.time_budget_ms.min(u64::MAX as u128) as u64);
    if std::time::Instant::now() > deadline {
        return Ok(GrepResult {
            hits: Vec::new(),
            file_counts: Vec::new(),
            match_count: 0,
            files_scanned: 0,
            files_matched: 0,
            entries_visited: 0,
            offset_skipped: 0,
            omitted_per_file: 0,
            truncated: true,
            timed_out: true,
            files_skipped_large: 0,
            largest_skipped: 0,
        });
    }

    let threads = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(4)
        .clamp(1, 8);
    let chunk_size = 16usize;
    let wave_files = threads * chunk_size;
    let materialize_cap = if opts.output == GrepOutput::Files {
        1
    } else {
        opts.offset
            .saturating_add(opts.limit)
            .saturating_add(1)
            .min(opts.max_per_file)
    };

    let mut state = MergeState::new();
    let mut pending: Vec<walk::Entry> = Vec::with_capacity(wave_files);
    // 满页后先不立刻停：还要看是否真有下一个可扫描文件，
    // 才能给出与“还有后续文件”一致的 truncated 标记。
    let mut peek_more = false;
    let report = walk::visit(
        root,
        &opts.glob,
        &WalkOpts {
            include_dirs: false,
            limit: 100_000,
            follow_links: false,
            offset: 0,
            time_budget_ms: opts.time_budget_ms,
            no_ignore: opts.no_ignore,
        },
        |entry| {
            if std::time::Instant::now() > deadline {
                state.timed_out = true;
                state.truncated = true;
                return VisitControl::Stop;
            }
            // 页面已满时只需确认“还有没有下一个可扫描文件”，确认后立即停止。
            // 这个检查必须在空文件 / 超大文件过滤之前：否则满页后遇到空文件会
            // 一个个 continue，1000 个空文件就多访问 1000 个目录项。
            if peek_more {
                state.truncated = true;
                return VisitControl::Stop;
            }
            if entry.size == 0 {
                return VisitControl::Continue;
            }
            if entry.size > opts.max_file_size {
                state.files_skipped_large += 1;
                state.largest_skipped = state.largest_skipped.max(entry.size);
                return VisitControl::Continue;
            }
            pending.push(entry);
            if pending.len() >= wave_files {
                let scans = scan_wave(
                    root,
                    &pending,
                    re,
                    opts,
                    materialize_cap,
                    deadline,
                    chunk_size,
                );
                pending.clear();
                if !state.merge(scans, opts) {
                    if state.timed_out {
                        return VisitControl::Stop;
                    }
                    peek_more = true;
                }
            }
            VisitControl::Continue
        },
    )?;

    // 收尾不足一波的条目。
    if !pending.is_empty() {
        let scans = scan_wave(
            root,
            &pending,
            re,
            opts,
            materialize_cap,
            deadline,
            chunk_size,
        );
        pending.clear();
        state.merge(scans, opts);
    }

    Ok(state.into_result(report.entries_visited, report.timed_out, report.stopped))
}

/// 把过长行裁到 200 字符；命中位置靠后时围绕命中开窗，否则模型看到的全是填充字符，
/// 反而必须再读一次文件才能确认命中内容。
fn trim_line_at(s: &str, needle: Option<&Regex>) -> String {
    let t = s.trim_end();
    let chars: Vec<char> = t.chars().collect();
    if chars.len() <= 200 {
        return t.to_string();
    }
    // 找到命中在行内的字符位置，以它为中心取窗口。
    let center = needle
        .and_then(|re| re.find(t))
        .map(|m| t[..m.start()].chars().count())
        .unwrap_or(0);
    const HALF: usize = 90;
    let start = center
        .saturating_sub(HALF)
        .min(chars.len().saturating_sub(200));
    let end = (start + 200).min(chars.len());
    let body: String = chars[start..end].iter().collect();
    match (start, end < chars.len()) {
        (0, true) => format!("{body}…"),
        (0, false) => body,
        (_, true) => format!("…{body}…"),
        (_, false) => format!("…{body}"),
    }
}

/// 把一个文件读成可搜索正文。
///
/// 串行与并行两条扫描路径共用这个函数：否则任何一处修复（二进制判定、
/// 文档抽取、末尾换行处理）都会只改到一条路径，导致串行/并行结果分叉。
/// 返回 `None` 表示跳过该文件（二进制、抽取失败、读取失败）。
fn load_search_text(fp: &FsPath) -> Option<String> {
    let bytes = crate::io::read_bytes(&fp.wide()).ok()?;
    // 二进制判定：前 8KB 里有 NUL。DOCX/PDF 是压缩包或二进制，改走正文抽取。
    let format = crate::documents::format_of(fp.native());
    if format.is_none() && bytes[..bytes.len().min(8192)].contains(&0) {
        return None;
    }
    match format {
        Some(format) => crate::documents::extract_bytes(format, &bytes).ok(),
        None => Some(text::decode(&bytes).text),
    }
}

/// 正文切行：借用切片，只有最终选中的命中/上下文才生成 `String`。
/// 末尾换行不产生额外行，避免 context 输出幽灵空行。
fn split_search_lines(content: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = content.split('\n').collect();
    if lines.len() > 1 && lines[lines.len() - 1].is_empty() {
        lines.pop();
    }
    lines
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
            for (path, count) in &r.file_counts {
                // 显式 limit 截断或 offset 分页时，数字只是本页可见值，
                // 不标注会被读成“该文件共 N 处命中”。
                let note = if r.truncated {
                    "(本页可见，未扫完)"
                } else if r.offset_skipped > 0 {
                    "(本页可见)"
                } else {
                    ""
                };
                let line = format!("{path}:{count}{note}\n");
                if text.len() + line.len() > budget {
                    truncated = true;
                    break;
                }
                text.push_str(&line);
                files_rendered += 1;
                matches_rendered += count;
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
    fn count模式截断时标注本页可见() {
        // `path:N` 与 rg -c 同形；受 limit 截断时 N 只是本页可见数，
        // 不标注会被读成"该文件共 N 处命中"。
        let dir = std::env::temp_dir().join(format!("fsops-count-trunc-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("many.txt"), "needle\n".repeat(50)).unwrap();
        let root = FsPath::parse(&dir.to_string_lossy()).unwrap();
        let opts = GrepOpts {
            pattern: "needle".to_string(),
            output: GrepOutput::Count,
            limit: 10,
            ..Default::default()
        };
        let truncated = grep(&root, &opts).unwrap();
        let full = grep(&root, &GrepOpts { limit: 100, ..opts }).unwrap();
        let _ = fs::remove_dir_all(&dir);

        let a = render_mode(&truncated, GrepOutput::Count, 8192).text;
        assert!(a.contains("many.txt:10"), "{a}");
        assert!(a.contains("本页可见"), "截断时必须标注: {a}");

        let b = render_mode(&full, GrepOutput::Count, 8192).text;
        assert!(b.contains("many.txt:50"), "{b}");
        assert!(!b.contains("本页可见"), "未截断不应标注: {b}");
    }

    #[test]
    fn count使用独立聚合且串并行一致() {
        let dir =
            std::env::temp_dir().join(format!("fsops-count-aggregate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.txt"), "needle\nneedle\n").unwrap();
        fs::write(dir.join("b.txt"), "needle\n").unwrap();
        let root = FsPath::parse(&dir.to_string_lossy()).unwrap();

        for parallel in [false, true] {
            let result = grep(
                &root,
                &GrepOpts {
                    pattern: "needle".into(),
                    output: GrepOutput::Count,
                    limit: usize::MAX,
                    parallel,
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(
                result.file_counts,
                vec![("a.txt".to_string(), 2), ("b.txt".to_string(), 1)],
                "parallel={parallel}"
            );
            assert!(
                result.hits.is_empty(),
                "count 不保存逐命中正文: {:?}",
                result.hits.iter().map(|h| &h.text).collect::<Vec<_>>()
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn 能匹配空串的模式被拒绝() {
        // `^`、`$`、`a*`、`.*`、`\b` 会命中每一行：返回 100 行"命中"但零信息量，
        // 还会让模型误以为文件内容真的含有该串。
        let dir = std::env::temp_dir().join(format!("fsops-empty-pat-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.txt"), "line1\nline2\n").unwrap();
        let root = FsPath::parse(&dir.to_string_lossy()).unwrap();
        for pat in ["^", "$", "a*", ".*", "(x)?", r"\b", r"\d*"] {
            let result = grep(
                &root,
                &GrepOpts {
                    pattern: pat.to_string(),
                    ..Default::default()
                },
            );
            let err = match result {
                Ok(_) => panic!("空匹配模式 {pat} 应被拒绝"),
                Err(e) => e,
            };
            assert!(err.starts_with("E_BAD_REGEX"), "{pat}: {err}");
            assert!(err.contains("空位置"), "{pat}: {err}");
        }
        // 合法锚点模式不受影响
        let ok = grep(
            &root,
            &GrepOpts {
                pattern: "^line1".to_string(),
                ..Default::default()
            },
        )
        .unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(ok.hits.len(), 1, "合法锚点应正常工作");
    }

    #[test]
    fn 长行命中在中间时围绕命中开窗() {
        // 压缩后的 JS 常见：单行几千字符，命中在中间。
        // 若从行首截断，模型看到的全是填充字符，必须再读一次文件。
        let dir = std::env::temp_dir().join(format!("fsops-window-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let line = format!("var a={};FINDME;{};", "Q".repeat(3000), "R".repeat(3000));
        fs::write(dir.join("bundle.min.js"), format!("{line}\n")).unwrap();
        let root = FsPath::parse(&dir.to_string_lossy()).unwrap();
        let r = grep(
            &root,
            &GrepOpts {
                pattern: "FINDME".to_string(),
                context: 0,
                ..Default::default()
            },
        )
        .unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(r.hits.len(), 1, "应命中一次");
        let text = &r.hits[0].text;
        assert!(text.contains("FINDME"), "命中词必须在可见窗口内: {text}");
        // 窗口两侧都应标出省略
        assert!(text.starts_with('…'), "前方被截断应有省略号: {text}");
        assert!(text.ends_with('…'), "后方被截断应有省略号: {text}");
        // 长度仍受控
        assert!(
            text.chars().count() <= 202,
            "窗口不应超过 200 字符: {}",
            text.chars().count()
        );
    }

    #[test]
    fn 长行命中在开头时不加前导省略号() {
        let dir = std::env::temp_dir().join(format!("fsops-window2-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let line = format!("FINDME{}", "Z".repeat(3000));
        fs::write(dir.join("head.txt"), format!("{line}\n")).unwrap();
        let root = FsPath::parse(&dir.to_string_lossy()).unwrap();
        let r = grep(
            &root,
            &GrepOpts {
                pattern: "FINDME".to_string(),
                ..Default::default()
            },
        )
        .unwrap();
        let _ = fs::remove_dir_all(&dir);
        let text = &r.hits[0].text;
        assert!(text.starts_with("FINDME"), "命中在开头应原样显示: {text}");
        assert!(!text.starts_with('…'), "不应加前导省略号: {text}");
        assert!(text.ends_with('…'), "{text}");
    }

    #[test]
    fn 上下文不输出末尾幽灵空行() {
        let tmp = std::env::temp_dir().join(format!("fsops-ctx-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        // 文件只有一行且以换行结尾；context=1 不应产生第 2 行空上下文
        fs::write(tmp.join("one.txt"), "NEEDLE here\n").unwrap();
        let root = FsPath::parse(&tmp.to_string_lossy()).unwrap();
        let r = grep(
            &root,
            &GrepOpts {
                pattern: "NEEDLE".to_string(),
                context: 1,
                ..Default::default()
            },
        )
        .unwrap();
        let _ = fs::remove_dir_all(&tmp);
        let rendered = render(&r);
        assert!(!rendered.contains("2-"), "不应有幽灵空上下文行: {rendered}");
        assert_eq!(r.hits.len(), 1, "只有一行命中: {rendered}");
    }

    #[test]
    fn glob写法误用正则时给出提示() {
        let dir = std::env::temp_dir().join(format!("fsops-regex-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "x\n").unwrap();
        let root = FsPath::parse(&dir.to_string_lossy()).unwrap();
        let result = grep(
            &root,
            &GrepOpts {
                pattern: "*.txt".to_string(),
                ..Default::default()
            },
        );
        let _ = std::fs::remove_dir_all(&dir);
        let err = match result {
            Ok(_) => panic!("非法 glob 写法应报错"),
            Err(e) => e,
        };
        assert!(err.starts_with("E_BAD_REGEX"), "{err}");
        assert!(err.contains("是正则不是 glob"), "{err}");
        assert!(err.contains(r".*\.txt"), "应给出正确等价写法: {err}");
    }

    #[test]
    fn 默认跳过依赖目录但可显式关闭忽略() {
        let dir = std::env::temp_dir().join(format!("fsops-ni-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("node_modules/pkg/c.js"), "x\n").unwrap();
        fs::write(dir.join("src/a.rs"), "x\n").unwrap();
        fs::write(dir.join(".gitignore"), "node_modules/\n").unwrap();
        let root = FsPath::parse(&dir.to_string_lossy()).unwrap();

        let opts = WalkOpts {
            limit: 100,
            ..Default::default()
        };
        let (entries, _) = walk::glob_with_report(&root, "**/*.js", &opts).unwrap();
        assert!(entries.is_empty(), "默认应跳过 node_modules");

        let opts = WalkOpts {
            limit: 100,
            no_ignore: true,
            ..Default::default()
        };
        let (entries, _) = walk::glob_with_report(&root, "**/*.js", &opts).unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(entries.len(), 1, "no_ignore 后应能看到依赖源码");
        assert!(entries[0].path.contains("node_modules"));
    }

    #[test]
    fn 搜索行切片借用正文不复制() {
        // 逐行 to_string 会让每个文件产生 N 次分配；切片必须借用同一份正文。
        let text = "a\r\nbb\nccc";
        let lines = split_search_lines(text);
        assert_eq!(lines, vec!["a\r", "bb", "ccc"]);
        assert_eq!(lines[0].as_ptr(), text.as_ptr(), "行切片必须借用正文");
        assert_eq!(split_search_lines("").len(), 1);
        assert_eq!(split_search_lines("a\n"), vec!["a"]);
    }

    #[test]
    fn 数万短行大文件搜索命中与上下文稳定() {
        // 借用切片不能改变行号、上下文范围与末尾换行规则。
        let dir = std::env::temp_dir().join(format!("fsops-grep-huge-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mut body = String::new();
        for i in 0..40_000usize {
            if [10, 20_000, 39_999].contains(&i) {
                body.push_str(&format!("needle-{i}\n"));
            } else {
                body.push_str("pad\n");
            }
        }
        fs::write(dir.join("big.txt"), &body).unwrap();
        let root = FsPath::parse(&dir.to_string_lossy()).unwrap();

        let mut rendered = Vec::new();
        for parallel in [false, true] {
            let r = grep(
                &root,
                &GrepOpts {
                    pattern: "needle".into(),
                    context: 1,
                    limit: 20,
                    parallel,
                    ..Default::default()
                },
            )
            .unwrap();
            let hits: Vec<(usize, &str, bool)> = r
                .hits
                .iter()
                .map(|hit| (hit.line, hit.text.as_str(), hit.is_context))
                .collect();
            assert_eq!(
                hits,
                vec![
                    (10, "pad", true),
                    (11, "needle-10", false),
                    (12, "pad", true),
                    (20_000, "pad", true),
                    (20_001, "needle-20000", false),
                    (20_002, "pad", true),
                    (39_999, "pad", true),
                    (40_000, "needle-39999", false),
                ],
                "parallel={parallel}"
            );
            assert_eq!(r.match_count, 3, "parallel={parallel}");
            rendered.push(render(&r));
        }
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(rendered[0], rendered[1], "串行/并行输出必须一致");
    }

    #[test]
    fn 并行满页后不再访问全部候选文件() {
        // 旧实现先把候选文件全收进 Vec 再扫描：500 个文件时 entries_visited 恒为 500。
        // 有界流水线在攒满一波并达到 limit 后应立即停止遍历。
        let dir = std::env::temp_dir().join(format!("fsops-grep-par-stop-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for i in 0..500usize {
            fs::write(dir.join(format!("f{i:04}.txt")), "needle\n").unwrap();
        }
        let root = FsPath::parse(&dir.to_string_lossy()).unwrap();
        let r = grep(
            &root,
            &GrepOpts {
                pattern: "needle".into(),
                limit: 3,
                parallel: true,
                ..Default::default()
            },
        )
        .unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(r.match_count, 3);
        assert!(r.truncated, "满页必须标记截断");
        // 一波最多 threads(≤8) * 16 个文件，满页判定最多再看一个目录项。
        assert!(
            r.entries_visited <= 8 * 16 + 1,
            "满页后仍访问了 {} 个目录项（共 500 个文件）",
            r.entries_visited
        );
        assert!(
            r.files_scanned <= 8 * 16,
            "扫描了 {} 个文件",
            r.files_scanned
        );
    }

    #[test]
    fn 并行候选上限耗尽时不会静默报完整() {
        // walk 的候选上限（WalkOpts.limit=100k）撞到时只会置 stopped；
        // 不合并进 truncated，并行结果会看起来像“已扫完全部候选”。
        let capped = MergeState::new().into_result(100_000, false, true);
        assert!(capped.truncated, "候选上限耗尽必须标记截断");
        assert!(!capped.timed_out, "不是超时不能标成超时");

        let natural = MergeState::new().into_result(3, false, false);
        assert!(!natural.truncated, "自然结束不应标记截断");
    }

    #[test]
    fn 并行与串行在多组分页参数下逐字节一致() {
        let dir = std::env::temp_dir().join(format!("fsops-grep-matrix-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for i in 0..60usize {
            // 多命中文件是重点：limit/max_per_file 中途截断时的 omitted 计数最容易分叉。
            let body = match i % 4 {
                0 => format!("hit-{i}-a\nmid\nhit-{i}-b\ntail\nhit-{i}-c\n"),
                1 => format!("head\nhit-{i}\n"),
                2 => {
                    let mut body = String::new();
                    for k in 0..5 {
                        body.push_str(&format!("hit-{i}-{k}\npad\n"));
                    }
                    body
                }
                _ => "none\n".to_string(),
            };
            fs::write(dir.join(format!("f{i:02}.txt")), body).unwrap();
        }
        let root = FsPath::parse(&dir.to_string_lossy()).unwrap();

        let mut checked = 0usize;
        for output in [
            GrepOutput::Files,
            GrepOutput::Count,
            GrepOutput::Matches,
            GrepOutput::Snippets,
        ] {
            for &(limit, offset, max_per_file) in &[
                (usize::MAX, 0usize, usize::MAX),
                (5, 0, 1),
                (3, 7, 2),
                (1, 0, usize::MAX),
                (4, 2, 3),
                // 满页时并行侧曾把整文件的 omitted 都算进结果：这组参数专门盯住它。
                (2, 0, 2),
                (4, 0, 4),
                (6, 3, 1),
            ] {
                for context in [0usize, 1] {
                    let make = |parallel| GrepOpts {
                        pattern: "hit".into(),
                        output,
                        context,
                        limit,
                        offset,
                        max_per_file,
                        parallel,
                        ..Default::default()
                    };
                    let serial = grep(&root, &make(false)).unwrap();
                    let parallel = grep(&root, &make(true)).unwrap();
                    let case = format!(
                        "{output:?} limit={limit} offset={offset} \
                         max_per_file={max_per_file} context={context}"
                    );
                    let a = render_mode(&serial, output, 8192).text;
                    let b = render_mode(&parallel, output, 8192).text;
                    assert_eq!(a, b, "输出分叉: {case}");
                    assert_eq!(parallel.match_count, serial.match_count, "{case}");
                    assert_eq!(parallel.offset_skipped, serial.offset_skipped, "{case}");
                    assert_eq!(parallel.omitted_per_file, serial.omitted_per_file, "{case}");
                    assert_eq!(parallel.truncated, serial.truncated, "{case}");
                    assert_eq!(parallel.files_matched, serial.files_matched, "{case}");
                    checked += 1;
                }
            }
        }
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(checked, 64, "分页组合应全部比对");
    }

    #[test]
    fn 并行与串行在多命中文件下省略数一致() {
        // 每个文件都有 6 处命中，limit 会在某个文件的中间命中：串行从那里 break，
        // 并行必须丢掉该文件累计的 omitted（否则把“串行没数到的命中”多报出去）。
        let dir = std::env::temp_dir().join(format!("fsops-grep-multihit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            fs::write(dir.join(name), "hit1\nhit2\nhit3\nhit4\nhit5\nhit6\n").unwrap();
        }
        let root = FsPath::parse(&dir.to_string_lossy()).unwrap();

        for output in [GrepOutput::Matches, GrepOutput::Count] {
            for &(limit, offset, max_per_file) in &[
                (2usize, 0usize, 2usize),
                (3, 0, 4),
                (4, 1, 3),
                (usize::MAX, 0, 2),
            ] {
                let make = |parallel| GrepOpts {
                    pattern: "hit".into(),
                    output,
                    limit,
                    offset,
                    max_per_file,
                    parallel,
                    ..Default::default()
                };
                let serial = grep(&root, &make(false)).unwrap();
                let parallel = grep(&root, &make(true)).unwrap();
                let case =
                    format!("{output:?} limit={limit} offset={offset} max_per_file={max_per_file}");
                assert_eq!(
                    render_mode(&parallel, output, 8192).text,
                    render_mode(&serial, output, 8192).text,
                    "输出分叉: {case}"
                );
                assert_eq!(parallel.match_count, serial.match_count, "{case}");
                assert_eq!(parallel.omitted_per_file, serial.omitted_per_file, "{case}");
                assert_eq!(parallel.offset_skipped, serial.offset_skipped, "{case}");
                assert_eq!(parallel.truncated, serial.truncated, "{case}");
            }
        }
        // 不截断时省略数必须是真值：每文件 6 处命中、上限 2 → 每文件省 4。
        let plain = grep(
            &root,
            &GrepOpts {
                pattern: "hit".into(),
                output: GrepOutput::Count,
                limit: usize::MAX,
                max_per_file: 2,
                parallel: true,
                ..Default::default()
            },
        )
        .unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(plain.omitted_per_file, 12);
        assert!(!plain.truncated);
    }

    #[test]
    fn 并行满页后遇空文件和大文件立即停止遍历() {
        // peek_more 是“页面已满，还要确认是否真有下一个可扫描文件”。
        // 这个检查必须在 size==0 / size>max_file_size 过滤之前：否则页面满后
        // 遇到空文件或超大文件会一个个继续遍历，entries_visited 直接失控。
        let dir = std::env::temp_dir().join(format!("fsops-grep-peek-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("z-a.txt"), "needle\nneedle\nneedle\n").unwrap();
        fs::write(dir.join("z-b.txt"), "needle\n").unwrap();
        // 前面的 pad 把波次刚好填满到 128 个非空文件：无论波次是 threads×16
        // 的哪一种（16/32/64/128），命中文件都在最后一个波次里，
        // 紧接着就是 1000 个空文件。
        for i in 0..126usize {
            fs::write(dir.join(format!("p{i:04}.txt")), "pad\n").unwrap();
        }
        for i in 0..1000usize {
            fs::write(dir.join(format!("zz{i:04}.txt")), "").unwrap();
        }
        let root = FsPath::parse(&dir.to_string_lossy()).unwrap();
        let r = grep(
            &root,
            &GrepOpts {
                pattern: "needle".into(),
                output: GrepOutput::Files,
                limit: 1,
                parallel: true,
                ..Default::default()
            },
        )
        .unwrap();
        let _ = fs::remove_dir_all(&dir);

        assert_eq!(r.match_count, 1);
        assert!(r.truncated, "满页必须标记截断");
        assert!(
            r.entries_visited <= 8 * 16 + 2,
            "满页后仍访问了 {} 个目录项（另有 1000 个空文件）",
            r.entries_visited
        );
        assert!(
            r.files_scanned <= 8 * 16,
            "扫描了 {} 个文件",
            r.files_scanned
        );
    }

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
    fn 并行与串行在边界文件上结果一致() {
        // 两条路径现在共用 load_search_text；这个测试防止它们再次分叉。
        // 覆盖：CRLF、空文件、无末尾换行、中文、二进制（应跳过）。
        let tmp = std::env::temp_dir().join(format!("fsops-par-edge-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("plain.txt"), "needle here\n").unwrap();
        fs::write(tmp.join("crlf.txt"), "needle crlf\r\n").unwrap();
        fs::write(tmp.join("empty.txt"), "").unwrap();
        fs::write(tmp.join("nonl.txt"), "needle no trailing").unwrap();
        fs::write(tmp.join("cn.txt"), "中文 needle\n").unwrap();
        fs::write(tmp.join("bin.dat"), b"\x89PNG\r\n\x1a\n\x00\x00needle\x00").unwrap();
        let root = FsPath::parse(&tmp.to_string_lossy()).unwrap();

        let mut mismatches = Vec::new();
        for output in [
            GrepOutput::Files,
            GrepOutput::Count,
            GrepOutput::Matches,
            GrepOutput::Snippets,
        ] {
            let base = GrepOpts {
                pattern: "needle".to_string(),
                context: 2,
                ..Default::default()
            };
            let serial = grep(&root, &base).unwrap();
            let parallel = grep(
                &root,
                &GrepOpts {
                    parallel: true,
                    ..base
                },
            )
            .unwrap();
            let a = render_mode(&serial, output, 8192);
            let b = render_mode(&parallel, output, 8192);
            if a.text != b.text {
                mismatches.push(format!("{output:?}: 串行={:?} 并行={:?}", a.text, b.text));
            }
        }
        let _ = fs::remove_dir_all(&tmp);
        assert!(mismatches.is_empty(), "串行/并行结果分叉: {mismatches:?}");
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
            file_counts: Vec::new(),
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
            files_skipped_large: 0,
            largest_skipped: 0,
        };

        let rendered = render_mode(&r, GrepOutput::Files, 4096);
        assert_eq!(rendered.text, "a.txt\nb.txt\n");
        assert_eq!(rendered.matches_rendered, 3);
    }

    #[test]
    fn count_模式按文件给出命中数() {
        let r = GrepResult {
            file_counts: vec![("a.txt".into(), 2), ("b.txt".into(), 1)],
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
            files_skipped_large: 0,
            largest_skipped: 0,
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

    #[test]
    fn glob过滤导致visitor未被调用时walk超时必须报超时() {
        // walk 在 glob 过滤前就检查 deadline：600 个文件里只有最后一个匹配，
        // 预算 1ms 时遍历会在到达它之前耗尽。此时必须返回“超时”，
        // 不能因为 visitor 一次都没被调用就报“无命中”（假阴性）。
        let dir = std::env::temp_dir().join(format!("fsops-walk-fold-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for i in 0..600 {
            fs::write(dir.join(format!("f{i:04}.txt")), "x\n").unwrap();
        }
        fs::write(dir.join("zzz_last.txt"), "needle\n").unwrap();
        let root = FsPath::parse(&dir.to_string_lossy()).unwrap();

        for parallel in [false, true] {
            let r = grep(
                &root,
                &GrepOpts {
                    pattern: "needle".into(),
                    glob: "**/zzz_last.txt".into(),
                    time_budget_ms: 1,
                    parallel,
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(r.hits.is_empty(), "parallel={parallel}: 不应有命中");
            assert!(
                r.timed_out,
                "parallel={parallel}: walk 超时被吞掉，报成了无命中"
            );
            assert!(
                r.truncated,
                "parallel={parallel}: 遍历不完整必须标记 truncated"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn 并行count单文件超时置位truncated() {
        // count 的单文件数字在文件内超时时只是本页可见值；
        // 并行路径必须把单文件 timed_out 折算成 truncated，否则会被读成文件总数。
        let dir = std::env::temp_dir().join(format!("fsops-par-count-to-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("a.txt"), "needle\n".repeat(600_000)).unwrap();
        fs::write(dir.join("b.txt"), "needle\n").unwrap();
        let root = FsPath::parse(&dir.to_string_lossy()).unwrap();
        let r = grep(
            &root,
            &GrepOpts {
                pattern: "needle".into(),
                output: GrepOutput::Count,
                limit: usize::MAX,
                parallel: true,
                time_budget_ms: 30,
                ..Default::default()
            },
        )
        .unwrap();
        let _ = fs::remove_dir_all(&dir);
        assert!(r.files_scanned >= 1, "应至少开始扫描一个文件");
        assert!(r.timed_out, "大文件扫描应超时");
        assert!(r.truncated, "单文件超时后 truncated 必须置位");
    }

    fn 带上下文结果() -> GrepResult {
        GrepResult {
            file_counts: Vec::new(),
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
            files_skipped_large: 0,
            largest_skipped: 0,
        }
    }
}
