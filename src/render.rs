//! 渲染层：把文件内容压成"模型读得懂、又尽量短"的样子。
//!
//! 三条规则（直接决定 token 开销）：
//! 1. 每行前缀 `行号|`，比 JSON 逐行对象省一半以上字符；
//! 2. 连续空行折叠成一个 `⋯`，长行截断并标注剩余字符数；
//! 3. 每次返回都有硬预算，超出就少给行、并在末尾告诉模型怎么继续取。

pub const DEFAULT_BUDGET: usize = 12 * 1024;
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
    /// 是否因预算被截断
    pub truncated: bool,
    pub total_lines: usize,
    /// 预算截断后仍未输出的区间，可直接作为下一次 ranges 参数。
    pub continuation: Vec<(usize, usize)>,
}

/// 按行号范围渲染，并遵守字节预算。
pub fn render_window(text: &str, opts: RenderOpts) -> Rendered {
    render_ranges(text, &[(opts.start, opts.end)], opts)
}

/// 多个区间共享同一份正文预算；截断时保留所有尚未输出的区间。
pub fn render_ranges(text: &str, ranges: &[(usize, usize)], opts: RenderOpts) -> Rendered {
    let lines: Vec<&str> = text.split('\n').collect();
    let total = lines.len();
    let initial_start = ranges.first().map(|r| r.0).unwrap_or(opts.start).max(1);
    let mut out = String::with_capacity(2048);
    let mut last = initial_start.saturating_sub(1);
    let mut truncated = false;
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
            last = idx;
        }
    }

    Rendered {
        body: out,
        last_line: last,
        truncated,
        total_lines: total,
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
    if text.len() <= budget {
        return text;
    }
    let mut end = budget;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(r.body, "1|a\n2|\n5|b\n6|\n");
    }

    #[test]
    fn 超长行被截断且标注() {
        let long = "x".repeat(300);
        let r = render_window(&long, RenderOpts::default());
        assert!(r.body.contains("…(+60字符)"));
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
