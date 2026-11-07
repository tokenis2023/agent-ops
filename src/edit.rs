//! 编辑引擎：精确替换、批量修改、失败时给"最像的几行"。
//!
//! 关键点在于错误路径——模型改错代码时最贵的是"猜为什么没匹配上"。
//! 所以这里失败时直接返回候选行号和实际内容，让它一次改对。

use crate::text::{self, Eol};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct EditSpec {
    /// 要替换的原文（与 `pattern` 二选一）
    #[serde(default)]
    pub old: Option<String>,
    /// 替换成什么；空串 `""` 表示删除（必须显式写出，避免漏参数被当成删除）。
    #[serde(default)]
    pub new: Option<String>,
    /// 正则模式：替换范围内所有匹配，可用 `$1` 引用捕获组。
    /// 用于“不知道确切原文、只知道形状”的场景，省掉为抄原文而做的一次读取。
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default)]
    pub replacement: Option<String>,
    /// 是否替换所有出现位置（默认只允许唯一匹配）
    #[serde(default)]
    pub all: bool,
    /// 限定替换发生在第 start..=end 行内（1 基，含端点）。
    /// 用途：同一字面量在文件里出现多处时，只改某个函数内的那些，
    /// 不必为每一处构造不同的上下文片段。
    #[serde(default)]
    pub start: Option<usize>,
    #[serde(default)]
    pub end: Option<usize>,
}

/// 一处改动的原文与新文，供 `diff=true` 的紧凑展示。
#[derive(Debug, Default)]
pub struct Hunk {
    pub line: usize,
    pub old: String,
    pub new: String,
}

#[derive(Debug, Default)]
pub struct EditResult {
    pub applied: usize,
    pub added_lines: i64,
    pub removed_lines: i64,
    /// 每处修改的落点，如 `12`、`40-52`
    pub loci: Vec<String>,
    /// 每处改动的原文与新文，按 MAX_HUNKS 截断
    pub hunks: Vec<Hunk>,
    pub encoding: String,
    pub eol: String,
}

/// loci 只用于展示摘要，保留前若干个即可；全量保存会让大文件批量替换变成 O(n²)。
const MAX_LOCI: usize = 64;

/// diff 展示的改动块上限，避免大文件批量替换把响应撑爆。
const MAX_HUNKS: usize = 32;

/// 在已解码文本上按原换行风格匹配、修改，返回新文本与摘要。
pub fn apply(original: &str, eol: Eol, edits: &[EditSpec]) -> Result<(String, EditResult), String> {
    let mut work = text::to_lf(original);
    let base_encoding = "utf-8";
    // 已计数的行号：同一行多处修改只应算一次行数变化。
    // 必须用 HashSet：80 万处替换时线性 contains 会让整趟变成 O(n²)。
    let mut touched_lines: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut res = EditResult {
        encoding: base_encoding.into(),
        eol: eol.as_str().into(),
        ..Default::default()
    };

    for (i, e) in edits.iter().enumerate() {
        // 正则模式：不知道确切原文也能改，省掉为抄原文而做的一次读取。
        if let Some(pat) = &e.pattern {
            apply_regex(&mut work, e, pat, i + 1, &mut res, &mut touched_lines)?;
            continue;
        }
        let old = e.old.as_deref().ok_or_else(|| {
            format!(
                "E_BAD_EDIT 第{}处缺少 old（或用 pattern + replacement）",
                i + 1
            )
        })?;
        let new = e.new.as_deref().ok_or_else(|| {
            format!(
                "E_BAD_EDIT 第{}处缺少 new；删除内容请显式写 \"new\": \"\"",
                i + 1
            )
        })?;
        if old.is_empty() {
            return Err(format!("E_BAD_EDIT 第{}处 old 为空", i + 1));
        }
        let needle = text::to_lf(old);
        let repl = text::to_lf(new);

        let mut positions: Vec<usize> = find_all(&work, &needle);
        // 行范围限定：只保留落在 [start, end] 内的匹配。
        // 同一字面量在文件里出现多处时，用它只改某个函数内的那些，
        // 不必为每一处构造不同的上下文片段。
        if let Some(from) = e.start {
            let to = e.end.unwrap_or(usize::MAX);
            positions.retain(|p| {
                let line = line_of(&work, *p);
                line >= from && line <= to
            });
            if positions.is_empty() {
                return Err(format!(
                    "E_NO_MATCH 第{}处：在 {}..={} 行内未找到目标文本（文件其他位置有 {} 处匹配）",
                    i + 1,
                    from,
                    e.end.map(|v| v.to_string()).unwrap_or("末".into()),
                    find_all(&work, &needle).len()
                ));
            }
        }
        if positions.is_empty() {
            return Err(no_match_error(&work, &needle, i + 1));
        }
        if positions.len() > 1 && !e.all {
            let line_list: Vec<String> = positions
                .iter()
                .take(8)
                .map(|p| line_of(&work, *p).to_string())
                .collect();
            // 同一行多处时行号重复，区分列号才能区分出是第几处。
            let col_list: Vec<String> = positions
                .iter()
                .take(8)
                .map(|p| col_of(&work, *p).to_string())
                .collect();
            // 前几处附上下文片段：模型不必再 fs_read 就能构造出唯一的 old。
            let rows: Vec<&str> = work.split('\n').collect();
            let mut snippets = String::new();
            for p in positions.iter().take(3) {
                let hit = line_of(&work, *p);
                let col = col_of(&work, *p);
                let from = hit.saturating_sub(2).max(1);
                let to = (hit + 2).min(rows.len());
                for n in from..=to {
                    let text = rows.get(n - 1).copied().unwrap_or("");
                    let text = if n == hit {
                        mark_column(text, col)
                    } else {
                        snippet_line(text)
                    };
                    snippets.push_str(&format!("\n  {n}|{}", text));
                }
                snippets.push('\n');
            }
            return Err(format!(
                "E_AMBIGUOUS 第{}处匹配到 {} 个位置（行 {} 列 {}）；加更多上下文，或设 all=true 全部替换，或用 start/end 限定行范围{}",
                i + 1,
                positions.len(),
                line_list.join(","),
                col_list.join(","),
                snippets
            ));
        }

        let targets = if e.all {
            positions.clone()
        } else {
            vec![positions[0]]
        };
        // 单趟线性构建：一次扫描同时算行号并拼接结果。
        // 旧实现逐次 replace_range + 每次 line_of 从头扫，在大文件上是 O(n*m)。
        let (next, lines) = replace_at(&work, &needle, &repl, &targets);
        // 同一行多处匹配时行号重复，给列号才能区分出是第几处。
        for (line_no, pos) in lines.iter().zip(targets.iter()) {
            // 同一行上的多处修改只算一次：`+N -M` 表达的是"改动了几行"，
            // 而不是"执行了几次替换"。否则同一行改三处会报 +3 -3。
            if touched_lines.insert(*line_no) {
                res.removed_lines += count_lines(&needle);
                res.added_lines += count_lines(&repl);
            }
            res.applied += 1;
            push_locus(&mut res.loci, *line_no, col_of(&work, *pos), &repl);
            if res.hunks.len() < MAX_HUNKS {
                res.hunks.push(Hunk {
                    line: *line_no,
                    old: needle.clone(),
                    new: repl.clone(),
                });
            }
        }
        work = next;
    }

    let out = text::from_lf(&work, eol);
    Ok((out, res))
}

/// 正则替换：在 [start,end] 行范围内替换所有匹配，支持 `$1` 捕获组。
///
/// 模型常常只知道“要改的形状”（空格、大小写、数字具体值不确定），
/// 为此先读一遍文件再抄原文是最常见的浪费。
fn apply_regex(
    work: &mut String,
    e: &EditSpec,
    pat: &str,
    ordinal: usize,
    res: &mut EditResult,
    touched_lines: &mut std::collections::HashSet<usize>,
) -> Result<(), String> {
    let replacement = e
        .replacement
        .as_deref()
        .ok_or_else(|| format!("E_BAD_EDIT 第{ordinal}处：pattern 模式需要同时给出 replacement"))?;
    let re = regex::Regex::new(pat)
        .map_err(|err| format!("E_BAD_REGEX 第{ordinal}处 pattern 无效: {err}"))?;
    // 可匹配空位置的正则会命中每一处，等于把文件改烂。守卫与 fs_grep 共用：
    // 只看 `is_match("")` 会放过 `\b`（空串没有词边界，但每个词边界都命中）。
    if crate::search::matches_everywhere(&re) {
        return Err(format!(
            "E_BAD_REGEX 第{ordinal}处 pattern 可匹配空位置（如 ^ $ a* .* `\\b`）；请写出具体字符"
        ));
    }
    // regex 对未知引用不报错，而是展开成空串（`$1_x` 直接变成空）：
    // 静默丢内容比报错危险，写盘前先校验。
    validate_replacement(replacement, &re, ordinal)?;

    let (from, to) = line_span(work, e.start, e.end);
    let scope = &work[from..to];
    let base_line = line_of(work, from);
    let mut out = String::with_capacity(work.len());
    out.push_str(&work[..from]);
    // 单趟游标：`line_cursor` 之前已数过换行，`line_no` 只向前累加。
    // 旧实现每个命中都从范围起点重数换行，批量替换是 O(n²)。
    let mut line_cursor = 0usize;
    let mut line_no = base_line;
    let mut last = 0usize;
    let mut hits = 0usize;
    // 复用同一个展开缓冲，避免每个命中都新建 String。
    let mut expanded = String::new();
    for caps in re.captures_iter(scope) {
        let m = caps.get(0).unwrap();
        line_no += newline_count(&scope[line_cursor..m.start()]);
        line_cursor = m.start();
        out.push_str(&scope[last..m.start()]);
        expanded.clear();
        caps.expand(replacement, &mut expanded);
        out.push_str(&expanded);
        last = m.end();
        hits += 1;
        if touched_lines.insert(line_no) {
            res.removed_lines += count_lines(m.as_str());
            res.added_lines += count_lines(&expanded);
        }
        res.applied += 1;
        push_locus(&mut res.loci, line_no, col_of(scope, m.start()), &expanded);
        if res.hunks.len() < MAX_HUNKS {
            res.hunks.push(Hunk {
                line: line_no,
                old: m.as_str().to_string(),
                new: expanded.clone(),
            });
        }
    }
    if hits == 0 {
        return Err(if e.start.is_some() {
            format!(
                "E_NO_MATCH 第{ordinal}处：在 {}..={} 行内没有匹配；去掉 start/end 可搜全文",
                e.start.unwrap_or(1),
                e.end.map(|v| v.to_string()).unwrap_or("末".into())
            )
        } else {
            // 正则模式的建议不能是“先 fs_read”：用正则就是为了不读。
            format!(
                "E_NO_MATCH 第{ordinal}处：正则未匹配任何内容（共 {} 行）；请检查转义，或先用 fs_grep 确认形状",
                work.split('\n').count().saturating_sub(1)
            )
        });
    }

    out.push_str(&scope[last..]);
    out.push_str(&work[to..]);
    *work = out;
    Ok(())
}

/// 替换文本里的一个 `$` 引用。
#[derive(Debug, PartialEq, Eq)]
enum ReplRef {
    /// `$1` 或 `${1}`
    Number(usize),
    /// `$name` 或 `${name}`
    Name(String),
}

/// 校验替换文本里的 `$` 引用都能展开成本次匹配里存在的捕获组。
///
/// `regex::Captures::expand` 对未定义的引用不会失败，而是展开成空串：
/// `(a)` 配 `$1_x` 会被当成组名 `1_x`，匹配到的 `a` 整段消失。
/// 用户以为在写“组号 + 后缀”，实际得到静默丢内容，所以必须提前报错。
fn validate_replacement(repl: &str, re: &regex::Regex, ordinal: usize) -> Result<(), String> {
    let refs = parse_replacement_refs(repl)
        .map_err(|detail| format!("E_BAD_REPLACEMENT 第{ordinal}处：{detail}"))?;
    let names: Vec<&str> = re.capture_names().flatten().collect();
    for reference in refs {
        match reference {
            ReplRef::Number(index) if index >= re.captures_len() => {
                return Err(format!(
                    "E_BAD_REPLACEMENT 第{ordinal}处：替换文本引用了 ${index}，\
                     但模式只有 {} 个捕获组（含 $0，可用 $0-${}）",
                    re.captures_len(),
                    re.captures_len().saturating_sub(1)
                ));
            }
            ReplRef::Name(name) => {
                if names.iter().any(|known| *known == name) {
                    continue;
                }
                // `$1_x` 是最常见的写法错误：regex 把整串当成组名 `1_x`。
                // 指向正确写法，模型下一次就能改对。
                let message = if name.starts_with(|c: char| c.is_ascii_digit()) {
                    let digits_len = name.chars().take_while(|c| c.is_ascii_digit()).count();
                    let (digits, rest) = name.split_at(digits_len);
                    let suggestion = format!("${{{digits}}}{rest}");
                    format!(
                        "E_BAD_REPLACEMENT 第{ordinal}处：替换文本里的 ${name} 不是捕获组；\
                         组号后紧跟字母/数字/下划线会被当成组名，请写 `{suggestion}`"
                    )
                } else {
                    format!(
                        "E_BAD_REPLACEMENT 第{ordinal}处：替换文本里的 ${name} 不是捕获组\
                         （模式里没有名为 `{name}` 的组）"
                    )
                };
                return Err(message);
            }
            _ => {}
        }
    }
    Ok(())
}

/// 解析替换文本里的 `$` 引用，规则与 regex 的展开保持一致：
/// `$1`/`${1}`/`$name`/`${name}` 是引用，`$$` 是字面 `$`，其余 `$` 按字面保留。
/// 区别只在遇到畸形引用（`${1`、`${}`）时直接报错，而不是当成字面量。
fn parse_replacement_refs(repl: &str) -> Result<Vec<ReplRef>, String> {
    fn is_cap_letter(c: char) -> bool {
        c.is_ascii_alphanumeric() || c == '_'
    }
    let chars: Vec<char> = repl.chars().collect();
    let mut refs = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] != '$' {
            i += 1;
            continue;
        }
        if i + 1 >= chars.len() {
            break; // 末尾单独的 `$` 按字面保留
        }
        match chars[i + 1] {
            '$' => i += 2, // `$$` → 字面 `$`
            '{' => {
                let mut j = i + 2;
                let mut name = String::new();
                while j < chars.len() && chars[j] != '}' {
                    name.push(chars[j]);
                    j += 1;
                }
                if j >= chars.len() {
                    return Err(format!(
                        "替换文本里的 `${{{name}` 缺少右花括号；\
                         命名组写 `${{name}}`，组号写 `${{1}}`"
                    ));
                }
                if name.is_empty() || !name.chars().all(is_cap_letter) {
                    return Err(format!(
                        "替换文本里的 `${{{name}}}` 不是合法捕获组名；\
                         只允许字母、数字、下划线"
                    ));
                }
                refs.push(classify_ref(&name));
                i = j + 1;
            }
            c if is_cap_letter(c) => {
                let mut j = i + 1;
                let mut name = String::new();
                while j < chars.len() && is_cap_letter(chars[j]) {
                    name.push(chars[j]);
                    j += 1;
                }
                refs.push(classify_ref(&name));
                i = j;
            }
            _ => i += 1, // `$` 后不是引用（如 "US$ 5"）：按字面保留
        }
    }
    Ok(refs)
}

fn classify_ref(name: &str) -> ReplRef {
    match name.parse::<usize>() {
        Ok(index) => ReplRef::Number(index),
        Err(_) => ReplRef::Name(name.to_string()),
    }
}

/// 取 [start,end] 行范围对应的字节区间（1 基，含端点）。
fn line_span(text: &str, start: Option<usize>, end: Option<usize>) -> (usize, usize) {
    let first = start.unwrap_or(1).max(1);
    let last = end.unwrap_or(usize::MAX);
    let mut starts = vec![0usize];
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' && i + 1 < text.len() {
            starts.push(i + 1);
        }
    }
    let from = starts.get(first - 1).copied().unwrap_or(text.len());
    let to = if last == usize::MAX {
        text.len()
    } else {
        starts.get(last).copied().unwrap_or(text.len())
    };
    (from.min(text.len()), to.max(from).min(text.len()))
}

/// 片段覆盖的行数：换行数 + 1（单行片段是 1 行，不是 0 行）。
/// 旧实现只数 `\n`，导致所有单行替换都报 `+0 -0`，差异统计完全失去意义。
fn count_lines(s: &str) -> i64 {
    if s.is_empty() {
        return 0;
    }
    s.matches('\n').count() as i64 + 1
}

/// 片段换行数（= 相对起点跨过的行数），用于把命中位置映射到行号区间。
fn newline_count(s: &str) -> usize {
    s.matches('\n').count()
}

/// 在升序位置列表上一次性替换，并在同一趟扫描中算出每个命中的行号。
fn replace_at(work: &str, needle: &str, repl: &str, positions: &[usize]) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(work.len() + repl.len().saturating_mul(positions.len()));
    let mut lines = Vec::with_capacity(positions.len());
    let mut last = 0usize;
    let mut scanned = 0usize;
    let mut line = 1usize;
    for &pos in positions {
        if pos < last || pos + needle.len() > work.len() {
            continue;
        }
        line += work[scanned..pos].matches('\n').count();
        scanned = pos;
        lines.push(line);
        out.push_str(&work[last..pos]);
        out.push_str(repl);
        last = pos + needle.len();
    }
    out.push_str(&work[last..]);
    (out, lines)
}

fn push_locus(loci: &mut Vec<String>, line_no: usize, col_no: usize, repl: &str) {
    if loci.len() >= MAX_LOCI {
        return;
    }
    // 行号区间只取决于替换文本跨了几行（单行替换就只标一个行号）。
    let span = newline_count(repl);
    let base = if span > 0 {
        format!("{line_no}-{}", line_no + span)
    } else {
        format!("{line_no}")
    };
    // 只有同一行已经出现过其它替换时才带列号；常规单处修改保持简洁的 "12"。
    let entry = if loci.iter().any(|l| l == &base) {
        format!("{base}:{col_no}")
    } else {
        base
    };
    // 只在前 MAX_LOCI 条内去重，规模可控，避免 O(n²) 的 contains 扫描。
    if !loci.contains(&entry) {
        loci.push(entry);
    }
}

fn col_of(s: &str, pos: usize) -> usize {
    s[..pos].rfind('\n').map(|i| pos - i).unwrap_or(pos + 1)
}

/// 在命中行行尾标出列号；绝不改动正文。
///
/// 早期实现把 `^` 插在命中列，会直接吞掉那一列的字符
/// （`x();` 显示成 `^();`），模型也会把标记当成文件内容。
fn mark_column(text: &str, col: usize) -> String {
    format!("{} ← 列 {col}", snippet_line(text))
}

/// 歧义片段里的单行截断：定位用，不需要完整长行。
fn snippet_line(text: &str) -> String {
    const MAX_CHARS: usize = 160;
    let trimmed = text.trim_end();
    let mut out: String = trimmed.chars().take(MAX_CHARS).collect();
    if trimmed.chars().count() > MAX_CHARS {
        out.push('…');
    }
    out
}

fn find_all(hay: &str, needle: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(p) = hay[from..].find(needle) {
        let abs = from + p;
        out.push(abs);
        from = abs + needle.len().max(1);
    }
    out
}

fn line_of(s: &str, pos: usize) -> usize {
    s.as_bytes()[..pos.min(s.len())]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
        + 1
}

/// 匹配失败时，找出原文里最像目标片段的行，减少模型瞎猜。
fn no_match_error(work: &str, needle: &str, idx: usize) -> String {
    let first = needle.lines().next().unwrap_or("").trim();
    let mut candidates: Vec<(usize, &str)> = Vec::new();
    if !first.is_empty() {
        let key: String = first.chars().take(24).collect();
        for (i, line) in work.lines().enumerate() {
            let t = line.trim();
            if t.contains(&key) || (key.len() > 6 && key.contains(t) && t.len() > 6) {
                candidates.push((i + 1, line));
                if candidates.len() >= 5 {
                    break;
                }
            }
        }
    }
    if candidates.is_empty() && !first.is_empty() {
        // 退一步：按首行前 8 个非空白字符找（缩进/空白差异）
        let short: String = first
            .chars()
            .filter(|c| !c.is_whitespace())
            .take(8)
            .collect();
        if short.len() >= 4 {
            for (i, line) in work.lines().enumerate() {
                if line.replace(' ', "").contains(&short) {
                    candidates.push((i + 1, line));
                    if candidates.len() >= 5 {
                        break;
                    }
                }
            }
        }
    }
    if candidates.is_empty() && !first.is_empty() {
        // 再退一步：拼错一两个字母时前缀全落空，按编辑距离找最相似的行。
        // 让模型直接看到正确原文，比让它再读一次文件便宜得多。
        candidates = closest_lines(work, first, 3);
    }
    let mut msg = format!("E_NO_MATCH 第{}处未找到目标文本", idx);
    if candidates.is_empty() {
        msg.push_str("；请先 fs_read 确认当前内容");
    } else {
        msg.push_str("；最接近的行：");
        for (n, l) in candidates {
            let t = if l.chars().count() > 80 {
                format!("{}…", l.chars().take(80).collect::<String>())
            } else {
                l.to_string()
            };
            msg.push_str(&format!("\n{n}|{t}"));
        }
        // E_NO_MATCH 是 old 抄错，不是并发冲突；并发冲突由 E_STALE 报。
        // 把模型引向 hash 操作只会白跑一轮。
        msg.push_str("\n以上是实际内容；核对缩进与空白后直接用其中一行作为 old 重试");
    }
    msg
}

/// 按编辑距离找最相似的行；只看长度接近的行，避免大文件里做无谓的 DP。
fn closest_lines<'a>(work: &'a str, needle: &str, limit: usize) -> Vec<(usize, &'a str)> {
    let width = needle.chars().count();
    if width == 0 {
        return Vec::new();
    }
    let mut scored: Vec<(usize, usize, &str)> = Vec::new();
    for (i, line) in work.lines().enumerate() {
        let text = line.trim();
        let n = text.chars().count();
        // 长度差超过一倍的不可能是有用的近似行。
        if n * 2 < width || n > width * 2 {
            continue;
        }
        let d = crate::text::levenshtein(needle, text);
        // 只保留「确实很像」的行：短行差 2 个字符、长行按比例放宽。
        // 否则整个文件的行都成了候选，提示本身变成噪声。
        if d <= 2 || d * 4 <= width {
            scored.push((d, i + 1, line));
        }
    }
    scored.sort_by_key(|(d, n, _)| (*d, *n));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, n, line)| (n, line))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::Eol;

    fn spec(old: &str, new: &str) -> EditSpec {
        EditSpec {
            old: Some(old.into()),
            new: Some(new.into()),
            pattern: None,
            replacement: None,
            all: false,
            start: None,
            end: None,
        }
    }

    fn regex_spec(pattern: &str, replacement: &str) -> EditSpec {
        EditSpec {
            old: None,
            new: None,
            pattern: Some(pattern.into()),
            replacement: Some(replacement.into()),
            all: false,
            start: None,
            end: None,
        }
    }

    #[test]
    fn 正则替换支持捕获组() {
        let (out, res) = apply(
            "let a = 30;\nlet b = 30;\n",
            Eol::Lf,
            &[regex_spec(r"let (\w+) = 30", "let $1 = 60")],
        )
        .unwrap();
        assert_eq!(res.applied, 2);
        assert!(
            out.contains("let a = 60;") && out.contains("let b = 60;"),
            "{out}"
        );
    }

    #[test]
    fn 正则替换可限定行范围() {
        let mut e = regex_spec(r"= 1;", "= 2;");
        e.start = Some(2);
        e.end = Some(2);
        let (out, res) = apply("let a = 1;\nlet b = 1;\nlet c = 1;\n", Eol::Lf, &[e]).unwrap();
        assert_eq!(res.applied, 1);
        assert_eq!(out, "let a = 1;\nlet b = 2;\nlet c = 1;\n");
    }

    #[test]
    fn 拼错一个字母时按编辑距离给出最接近的行() {
        // 前缀匹配对「少一个字母」无效（alfa 不含于 alpha）；
        // 模型拼错时应该直接看到正确原文，而不是再读一次文件。
        let src = "let alpha = 1;\nlet beta = 2;\n";
        let err = apply(src, Eol::Lf, &[spec("let alfa = 1;", "x")]).unwrap_err();
        assert!(err.contains("1|let alpha = 1;"), "{err}");
        assert!(!err.contains("expect_hash"), "{err}");
    }

    #[test]
    fn 正则零匹配与空匹配都被拒绝() {
        let err = apply("abc\n", Eol::Lf, &[regex_spec("zzz", "y")]).unwrap_err();
        assert!(err.starts_with("E_NO_MATCH"), "{err}");
        let err = apply("abc\n", Eol::Lf, &[regex_spec("a*", "y")]).unwrap_err();
        assert!(err.starts_with("E_BAD_REGEX"), "{err}");
    }

    #[test]
    fn 记录改动块用于diff() {
        let mut e = spec("b", "BB");
        e.all = true;
        let (_out, res) = apply("a\nb\nc\n", Eol::Lf, &[e]).unwrap();
        assert_eq!(res.hunks.len(), 1);
        assert_eq!(res.hunks[0].line, 2);
        assert_eq!(res.hunks[0].old, "b");
        assert_eq!(res.hunks[0].new, "BB");
    }

    #[test]
    fn 行范围限定只改区间内匹配() {
        // 同一字面量在文件里多处出现时，只需改某个函数内的那些，
        // 不必为每一处构造不同的上下文片段。
        let src = "fn a() {\n    let x = 42;\n    let y = 42;\n}\nfn b() {\n    let z = 42;\n}\n";
        let mut e = spec("42", "MAX");
        e.all = true;
        e.start = Some(1);
        e.end = Some(4);
        let out = apply(src, Eol::Lf, std::slice::from_ref(&e)).unwrap();
        assert_eq!(out.1.applied, 2, "{}", out.0);
        assert!(out.0.contains("fn b() {\n    let z = 42;"), "{}", out.0);
        assert_eq!(out.0.matches("MAX").count(), 2);

        // 范围内无匹配时给出其他位置的数量，便于判断是不是范围选错了
        let mut narrow = spec("42", "MAX");
        narrow.all = true;
        narrow.start = Some(1);
        narrow.end = Some(1);
        let err = apply(src, Eol::Lf, &[narrow]).unwrap_err();
        assert!(err.contains("E_NO_MATCH"), "{err}");
        assert!(err.contains("3 处匹配"), "应告知文件其他位置的数量: {err}");
    }

    #[test]
    fn 单处替换保留crlf() {
        let src = "fn a() {\r\n    1\r\n}\r\n";
        let (out, res) = apply(src, Eol::CrLf, &[spec("    1", "    2")]).unwrap();
        assert_eq!(out, "fn a() {\r\n    2\r\n}\r\n");
        assert_eq!(res.applied, 1);
        assert_eq!(res.loci, vec!["2"]);
    }

    #[test]
    fn 多处匹配未开all时报歧义并给行号() {
        let src = "x();\nx();\n";
        let err = apply(src, Eol::Lf, &[spec("x();", "y();")]).unwrap_err();
        assert!(err.starts_with("E_AMBIGUOUS"));
        assert!(err.contains("1,2"));
        assert!(err.contains("1,1"), "同行多处应带列号: {err}");
        // 附上下文片段：模型不必再 fs_read 就能构造出唯一的 old。
        assert!(err.contains("1|x();"), "应含命中行片段: {err}");
        assert!(err.contains("2|x();"), "应含命中行片段: {err}");
    }

    #[test]
    fn 歧义片段不把标记插进正文() {
        // 往 `行号|原文` 里插 `^` 会吞掉命中列字符（alpha → ^lpha），
        // 模型重试时会抄到错的原文。
        let src = "alpha one\nbeta two\nalpha three\n";
        let err = apply(src, Eol::Lf, &[spec("alpha", "ALPHA")]).unwrap_err();
        assert!(err.contains("1|alpha one ← 列 1"), "正文必须完整: {err}");
        assert!(!err.contains("|^"), "标记不能占位正文: {err}");
    }

    #[test]
    fn 同行多处匹配给列号() {
        let src = "a foo b foo c\n";
        let mut s = spec("foo", "bar");
        s.all = true;
        let (out, res) = apply(src, Eol::Lf, &[s]).unwrap();
        assert_eq!(out, "a bar b bar c\n");
        assert_eq!(res.applied, 2);
        // 第一处不带列号（还没有重复行号），第二处起同一线路号带上列号以区分。
        assert_eq!(res.loci, vec!["1", "1:9"]);
    }

    #[test]
    fn 全部替换() {
        let src = "x();\nx();\n";
        let mut s = spec("x();", "y();");
        s.all = true;
        let (out, res) = apply(src, Eol::Lf, &[s]).unwrap();
        assert_eq!(out, "y();\ny();\n");
        assert_eq!(res.applied, 2);
    }

    #[test]
    fn 找不到时给出最接近的行() {
        let src = "let total = 0;\nlet count = 0;\n";
        let err = apply(src, Eol::Lf, &[spec("let cnt = 0;", "let count = 1;")]).unwrap_err();
        assert!(err.starts_with("E_NO_MATCH"));
    }

    #[test]
    fn 批量替换行号正确且内容一致() {
        let src = "a x\ny x\nz x\n";
        let mut s = spec("x", "YY");
        s.all = true;
        let (out, res) = apply(src, Eol::Lf, &[s]).unwrap();
        assert_eq!(out, "a YY\ny YY\nz YY\n");
        assert_eq!(res.applied, 3);
        assert_eq!(res.loci, vec!["1", "2", "3"]);
    }

    #[test]
    fn 跨行正则匹配后行号仍正确() {
        // 单趟游标必须把命中消耗掉的换行计入下一个命中的行号；
        // 跨行命中后若重新从命中末尾计数，后续行号会整体前移。
        let src = "l1\nl2\nfoo\nbar\nbaz\nl6\n";
        let (out, res) = apply(src, Eol::Lf, &[regex_spec("foo\nbar|baz", "OK")]).unwrap();
        assert_eq!(out, "l1\nl2\nOK\nOK\nl6\n");
        assert_eq!(res.applied, 2);
        assert_eq!(
            res.hunks.iter().map(|h| h.line).collect::<Vec<_>>(),
            vec![3, 5]
        );
        assert_eq!(res.loci, vec!["3", "5"]);
    }

    #[test]
    fn 命中数超过hunk上限时计数仍完整() {
        let src = "hit\n".repeat(150);
        let (_out, res) = apply(src.as_str(), Eol::Lf, &[regex_spec("hit", "HIT")]).unwrap();
        assert_eq!(res.applied, 150);
        assert_eq!(res.hunks.len(), MAX_HUNKS);
        assert_eq!(res.loci.len(), MAX_LOCI);
        assert_eq!(res.removed_lines, 150);
        assert_eq!(res.added_lines, 150);
    }

    #[test]
    fn 正则大文件批量替换是线性的() {
        // 旧实现每个命中都从范围起点重数换行（O(n²)）；这个规模旧代码需要数十秒。
        // 单趟游标版本应在 5 秒内完成。
        let n = 60_000usize;
        let src = "key=value\n".repeat(n);
        let started = std::time::Instant::now();
        let (out, res) = apply(
            src.as_str(),
            Eol::Lf,
            &[regex_spec(r"key=(\w+)", "key=${1}_x")],
        )
        .unwrap();
        let elapsed = started.elapsed();
        assert_eq!(res.applied, n);
        assert_eq!(res.hunks.len(), MAX_HUNKS);
        assert!(out.starts_with("key=value_x\n"), "{}", &out[..16]);
        assert_eq!(out.matches("key=value_x").count(), n);
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "正则批量替换耗时 {elapsed:?}，应线性"
        );
    }

    #[test]
    fn 正则空位置守卫覆盖词边界() {
        // `\b` 在空串上不匹配，只查 `is_match("")` 会放过它：
        // 4 个词边界都会命中，等于把文件改烂。必须与 fs_grep 共用同一守卫。
        let err = apply("a b\nc d\n", Eol::Lf, &[regex_spec(r"\b", "X")]).unwrap_err();
        assert!(err.starts_with("E_BAD_REGEX"), "{err}");
        assert!(err.contains("空位置"), "{err}");
    }

    #[test]
    fn 未闭合替换引用错误保留原始片段() {
        // 报错文案不能把用户没写的右花括号补进去，否则前后对不上。
        let err = apply("a\n", Eol::Lf, &[regex_spec("(a)", "${1")]).unwrap_err();
        assert!(err.starts_with("E_BAD_REPLACEMENT"), "{err}");
        assert!(err.contains("`${1` 缺少"), "应回显原始片段: {err}");
        assert!(
            !err.contains("`${1}` 缺少"),
            "不应在原始片段后补出右花括号: {err}"
        );
    }

    #[test]
    fn 替换文本未知引用报错而不是静默丢内容() {
        // regex 把 `$1_x` 当成组名 `1_x`，展开成空串：匹配内容无声消失。
        let err = apply("a\n", Eol::Lf, &[regex_spec("(a)", "$1_x")]).unwrap_err();
        assert!(err.starts_with("E_BAD_REPLACEMENT"), "{err}");
        assert!(err.contains("${1}_x"), "应提示正确写法: {err}");

        // 组号超界、未闭合的 `${` 同样必须报错，而不是变成空串。
        let err = apply("a\n", Eol::Lf, &[regex_spec("(a)", "$2")]).unwrap_err();
        assert!(err.starts_with("E_BAD_REPLACEMENT"), "{err}");
        let err = apply("a\n", Eol::Lf, &[regex_spec("(a)", "${1")]).unwrap_err();
        assert!(err.starts_with("E_BAD_REPLACEMENT"), "{err}");
    }

    #[test]
    fn 替换文本合法引用保持不变() {
        let (out, res) = apply("a\n", Eol::Lf, &[regex_spec("(?P<x>a)", "$x/${1}_x/$$")]).unwrap();
        assert_eq!(out, "a/a_x/$\n");
        assert_eq!(res.applied, 1);
    }

    #[test]
    fn 大文件批量替换是线性的() {
        let src = "abcdefghij\n".repeat(200_000);
        let mut s = spec("abcdefghij", "ABCDEFGHIJ");
        s.all = true;
        let started = std::time::Instant::now();
        let (out, res) = apply(&src, Eol::Lf, &[s]).unwrap();
        assert_eq!(res.applied, 200_000);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "耗时 {:?}",
            started.elapsed()
        );
        assert_eq!(out.lines().count(), 200_000);
        assert!(!out.contains("abcdefghij"));
    }
}
