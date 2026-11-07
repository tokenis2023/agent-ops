//! 渲染层：把文件内容压成"模型读得懂、又尽量短"的样子。
//!
//! 三条规则（直接决定 token 开销）：
//! 1. 每行前缀 `行号|`，比 JSON 逐行对象省一半以上字符；
//! 2. 连续空行折叠成一个 `⋯`，长行截断并标注剩余字符数；
//! 3. 每次返回都有硬预算，超出就少给行、并在末尾告诉模型怎么继续取。

/// 默认响应预算。32KB ≈ 8K token：多数源码文件一次读完，
/// 少一轮续读就省一次完整上下文重发，比省这几 KB 划算。
pub const DEFAULT_BUDGET: usize = 32 * 1024;
pub const MAX_LINE_CHARS: usize = 240;

#[derive(Debug, Clone, Copy)]
pub struct RenderOpts {
    pub start: usize,
    pub end: usize,
    pub budget: usize,
    pub max_line: usize,
    pub squeeze_blank: bool,
    pub numbers: bool,
    /// 输出行号偏移；大文件只加载局部片段时用于还原真实行号。
    pub line_offset: usize,
}

impl Default for RenderOpts {
    fn default() -> Self {
        Self {
            start: 1,
            end: usize::MAX,
            budget: DEFAULT_BUDGET,
            max_line: MAX_LINE_CHARS,
            squeeze_blank: true,
            numbers: true,
            line_offset: 0,
        }
    }
}

pub struct Rendered {
    pub body: String,
    /// 已经输出到的最后一行（1 基）
    pub last_line: usize,
    /// 实际输出的第一行（1 基）；尾注写“起始-结束”时用它，不能按固定行数倒推。
    pub first_line: usize,
    /// 是否因预算被截断
    pub truncated: bool,
    pub total_lines: usize,
    /// 是否有长行因 max_line 被截断（只在尾注提示一次，避免每行重复浪费预算）。
    pub line_cut: bool,
    /// 本响应所有截断行的最大原始字节数。
    pub max_cut_line_bytes: usize,
    /// 被 max_line 截断的行号（1 基，含 line_offset）；尾注据此给可重放命令。
    pub cut_lines: Vec<usize>,
    /// 预算截断后仍未输出的区间，可直接作为下一次 ranges 参数。
    pub continuation: Vec<(usize, usize)>,
}

/// 单个文件的行索引：一次 split 后同时服务总行数、block 探测和 ranges 渲染。
///
/// `fs_read` 的预算收敛会重渲染多轮；若每轮都重新 `split('\n')`，
/// 大文件会被反复扫描。索引在进入 converge 前构建一次即可。
pub struct LineIndex<'a> {
    /// 保留 `split('\n')` 的原始末项（末尾换行会产生一个空串），
    /// block 探测依赖这一形态，不能预先剔除。
    lines: Vec<&'a str>,
    total: usize,
}

impl<'a> LineIndex<'a> {
    pub fn new(text: &'a str) -> Self {
        #[cfg(test)]
        TEST_INDEX_BUILDS.with(|c| c.set(c.get() + 1));
        let lines: Vec<&str> = if text.is_empty() {
            Vec::new()
        } else {
            text.split('\n').collect()
        };
        // 末尾换行（含 CRLF）不代表额外一行；否则"共N行"会比头部多 1，自相矛盾。
        let total = if lines.len() > 1 && lines[lines.len() - 1].is_empty() {
            lines.len() - 1
        } else {
            lines.len()
        };
        Self { lines, total }
    }

    pub fn total_lines(&self) -> usize {
        self.total
    }

    /// 按 1 基行号索引的原始行（含行内 `\r` 与末尾空串），供 block 探测复用。
    pub fn lines(&self) -> &[&'a str] {
        &self.lines
    }

    pub fn render_ranges(&self, ranges: &[(usize, usize)], opts: RenderOpts) -> Rendered {
        render_indexed(&self.lines, self.total, ranges, opts)
    }
}

#[cfg(test)]
thread_local! {
    static TEST_INDEX_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static TEST_CONVERGE_ROUNDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub fn test_reset_index_builds() {
    TEST_INDEX_BUILDS.with(|c| c.set(0));
}

#[cfg(test)]
pub fn test_index_builds() -> usize {
    TEST_INDEX_BUILDS.with(|c| c.get())
}

#[cfg(test)]
pub fn test_reset_converge_rounds() {
    TEST_CONVERGE_ROUNDS.with(|c| c.set(0));
}

#[cfg(test)]
pub fn test_converge_rounds() -> usize {
    TEST_CONVERGE_ROUNDS.with(|c| c.get())
}

/// 按行号范围渲染，并遵守字节预算。
pub fn render_window(text: &str, opts: RenderOpts) -> Rendered {
    render_ranges(text, &[(opts.start, opts.end)], opts)
}

/// 多个区间共享同一份正文预算；截断时保留所有尚未输出的区间。
pub fn render_ranges(text: &str, ranges: &[(usize, usize)], opts: RenderOpts) -> Rendered {
    LineIndex::new(text).render_ranges(ranges, opts)
}

fn render_indexed(
    lines: &[&str],
    total: usize,
    ranges: &[(usize, usize)],
    opts: RenderOpts,
) -> Rendered {
    // 空文件没有任何行：直接返回空正文，避免渲染出一个空的 `1|`。
    if lines.is_empty() {
        return Rendered {
            body: String::new(),
            last_line: 0,
            first_line: 0,
            truncated: false,
            total_lines: 0,
            line_cut: false,
            max_cut_line_bytes: 0,
            cut_lines: Vec::new(),
            continuation: Vec::new(),
        };
    }
    let initial_start = ranges.first().map(|r| r.0).unwrap_or(opts.start).max(1);
    let mut out = String::with_capacity(2048);
    let mut first = 0usize;
    let mut last = initial_start.saturating_sub(1);
    let mut truncated = false;
    let mut line_cut = false;
    let mut max_cut_line_bytes = 0usize;
    let mut cut_lines: Vec<usize> = Vec::new();
    let mut continuation = Vec::new();

    'ranges: for (range_index, &(raw_start, raw_end)) in ranges.iter().enumerate() {
        let start = raw_start.max(1);
        let end = raw_end.min(total);
        let mut blank_run = 0usize;
        for idx in start..=end {
            let raw = lines[idx - 1];
            let line = raw.strip_suffix('\r').unwrap_or(raw);

            if opts.squeeze_blank && line.trim().is_empty() {
                blank_run += 1;
                if blank_run > 1 {
                    // 第二个及以后的连续空行直接跳过
                    last = idx;
                    continue;
                }
            } else {
                blank_run = 0;
            }

            let (shown, extra) = truncate_line(line, opts.max_line);
            let piece = if opts.numbers {
                format!("{}|{shown}", idx + opts.line_offset)
            } else {
                shown.to_string()
            };
            let suffix = if extra > 0 {
                line_cut = true;
                max_cut_line_bytes = max_cut_line_bytes.max(line.len());
                cut_lines.push(idx + opts.line_offset);
                format!("…(+{extra}字符)")
            } else {
                String::new()
            };
            // 行内容、长行标注和换行都占用同一份精确字节预算。
            if out.len() + piece.len() + suffix.len() + 1 > opts.budget {
                truncated = true;
                continuation.push((idx, raw_end));
                continuation.extend(ranges[range_index + 1..].iter().copied());
                break 'ranges;
            }
            out.push_str(&piece);
            out.push_str(&suffix);
            out.push('\n');
            if first == 0 {
                first = idx;
            }
            last = idx;
        }
    }

    Rendered {
        body: out,
        last_line: last,
        first_line: first,
        truncated,
        total_lines: total,
        line_cut,
        max_cut_line_bytes,
        cut_lines,
        continuation,
    }
}

fn truncate_line(line: &str, max: usize) -> (String, usize) {
    if max == 0 || line.chars().count() <= max {
        return (line.to_string(), 0);
    }
    let cut: String = line.chars().take(max).collect();
    (cut, line.chars().count() - max)
}

/// 给模型看的续读提示（空串表示读完了）。
pub fn next_hint(r: &Rendered, shown_path: &str) -> String {
    if r.last_line >= r.total_lines {
        return String::new();
    }
    format!(
        "… {}-{}/{} 行；继续读: {} {} {}",
        r.last_line.saturating_sub(9).max(1),
        r.last_line,
        r.total_lines,
        shown_path,
        r.last_line + 1,
        ""
    )
    .trim_end()
    .to_string()
}

/// 对完整响应做字节预算收敛；闭包参数是本轮可用于正文的预算。
///
/// 头部和尾部由闭包每次都带上，因此不会只约束正文而让总响应超限。
pub fn converge_budget<F>(budget: usize, mut render_response: F) -> String
where
    F: FnMut(usize) -> String,
{
    let mut body_budget = budget;
    loop {
        #[cfg(test)]
        TEST_CONVERGE_ROUNDS.with(|c| c.set(c.get() + 1));
        let response = render_response(body_budget).trim_end().to_string();
        if response.len() <= budget {
            return response;
        }
        if body_budget == 0 {
            return truncate_utf8(&response, budget).to_string();
        }
        let overflow = response.len() - budget;
        body_budget = body_budget.saturating_sub(overflow.max(1));
    }
}

/// 在 UTF-8 字符边界截断，绝不返回半个中文或 emoji。
pub fn truncate_utf8(text: &str, budget: usize) -> &str {
    // 错误消息保留最低可解析长度：错误的全部价值在于"可执行"，
    // 被截成 `E_BAD_ARGS` 或 `E_READ /c/` 就完全失去意义。
    // 但仍受 budget 约束（budget 是调用方给的硬上限），
    // 因此这里只是把"太小以至于无意义"的预算抬到能放下错误码+一句话的下限。
    let effective = if text.starts_with("E_") {
        budget.max(ERROR_MIN_BUDGET)
    } else {
        budget
    };
    if text.len() <= effective {
        return text;
    }
    let mut end = effective;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// 错误消息的最低可解析长度：够放下 `E_XXX` + 一句原因。
pub const ERROR_MIN_BUDGET: usize = 240;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 错误消息不被极小预算截成碎片() {
        // 错误的全部价值是"可执行"；截成 `E_BAD_ARGS` 或 `E_READ /c/` 毫无用处。
        let long_err = "E_BAD_ARGS 一次最多读 20 个文件，当前 25 个；请分批读取";
        for budget in [0, 5, 10, 20, 50] {
            let out = truncate_utf8(long_err, budget);
            assert!(
                out.starts_with("E_BAD_ARGS") && out.len() > 20,
                "budget={budget} 截成了碎片: {out}"
            );
        }
        // 非错误内容仍严格受 budget 约束
        let content = "abcdefghij";
        assert_eq!(truncate_utf8(content, 5), "abcde");
        assert_eq!(truncate_utf8(content, 0), "");
    }

    #[test]
    fn 末尾换行不算额外一行() {
        let r = render_window(
            "a\nb\n",
            RenderOpts {
                budget: 4096,
                ..Default::default()
            },
        );
        assert_eq!(r.total_lines, 2, "末尾换行不应多算一行");
        let r2 = render_window(
            "a\nb",
            RenderOpts {
                budget: 4096,
                ..Default::default()
            },
        );
        assert_eq!(r2.total_lines, 2);
    }

    #[test]
    fn 行号前缀与范围正确() {
        let text = "a\nb\nc\nd\n";
        let r = render_window(
            text,
            RenderOpts {
                start: 2,
                end: 3,
                ..Default::default()
            },
        );
        assert_eq!(r.body, "2|b\n3|c\n");
        assert_eq!(r.last_line, 3);
    }

    #[test]
    fn 连续空行被折叠() {
        let text = "a\n\n\n\nb\n";
        let r = render_window(text, RenderOpts::default());
        // 末尾换行不是一行，末尾不再输出幽灵的 `6|`。
        assert_eq!(r.body, "1|a\n2|\n5|b\n");
        assert_eq!(r.total_lines, 5);
    }

    #[test]
    fn 超长行被截断且标注() {
        let long = "x".repeat(300);
        let r = render_window(&long, RenderOpts::default());
        assert!(r.body.contains("…(+60字符)"));
    }

    #[test]
    fn 行索引与直接渲染逐字节一致() {
        // LineIndex 是 fs_read 的复用路径；它产出的总行数、行切片和
        // ranges 渲染必须与旧的 render_ranges 完全一致，否则大文件
        // 与小文件会在同一请求里给出互相矛盾的行号。
        for text in ["", "a", "a\n", "a\nb\n", "a\r\nb\r\n", "\n\n"].iter() {
            let index = LineIndex::new(text);
            let expected_total = if text.is_empty() {
                0
            } else {
                let n = text.split('\n').count();
                if text.ends_with('\n') {
                    n - 1
                } else {
                    n
                }
            };
            assert_eq!(index.total_lines(), expected_total, "{text:?}");
            let expected_pieces = if text.is_empty() {
                0
            } else {
                text.split('\n').count()
            };
            assert_eq!(index.lines().len(), expected_pieces, "{text:?}");
            for ranges in [vec![(1, 10)], vec![(2, 2)], vec![(1, 1), (3, 5)]] {
                let opts = RenderOpts {
                    budget: 4096,
                    ..Default::default()
                };
                let a = render_ranges(text, &ranges, opts);
                let b = index.render_ranges(&ranges, opts);
                assert_eq!(a.body, b.body, "{text:?} {ranges:?}");
                assert_eq!(a.total_lines, b.total_lines, "{text:?} {ranges:?}");
                assert_eq!(a.last_line, b.last_line, "{text:?} {ranges:?}");
                assert_eq!(a.first_line, b.first_line, "{text:?} {ranges:?}");
                assert_eq!(a.continuation, b.continuation, "{text:?} {ranges:?}");
            }
        }
    }

    #[test]
    fn 行索引构建被计数() {
        // 测试专用计数器：证明 fs_read 的多次预算收敛不会重复建索引。
        test_reset_index_builds();
        let _ = LineIndex::new("a\nb\n");
        let _ = LineIndex::new("c");
        assert_eq!(test_index_builds(), 2);
    }

    #[test]
    fn 预算生效时给出续读提示() {
        let text = (1..=200)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let r = render_window(
            text.as_str(),
            RenderOpts {
                budget: 400,
                ..Default::default()
            },
        );
        assert!(r.truncated);
        assert!(r.last_line < 200);
        assert!(!next_hint(&r, "x").is_empty());
    }
}
