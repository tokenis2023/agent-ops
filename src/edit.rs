//! 编辑引擎：精确替换、批量修改、失败时给"最像的几行"。
//!
//! 关键点在于错误路径——模型改错代码时最贵的是"猜为什么没匹配上"。
//! 所以这里失败时直接返回候选行号和实际内容，让它一次改对。

use crate::text::{self, Eol};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct EditSpec {
    /// 要替换的原文
    pub old: String,
    /// 替换成什么；空串 `""` 表示删除（必须显式写出，避免漏参数被当成删除）。
    pub new: String,
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
        if e.old.is_empty() {
            return Err(format!("E_BAD_EDIT 第{}处 old 为空", i + 1));
        }
        let needle = text::to_lf(&e.old);
        let repl = text::to_lf(&e.new);

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
            let lines: Vec<String> = positions
                .iter()
                .take(8)
                .map(|p| line_of(&work, *p).to_string())
                .collect();
            return Err(format!(
                "E_AMBIGUOUS 第{}处匹配到 {} 个位置（行 {}）；\
                 加更多上下文，或设 all=true 全部替换",
                i + 1,
                positions.len(),
                lines.join(",")
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
        for line_no in lines {
            // 同一行上的多处修改只算一次：`+N -M` 表达的是"改动了几行"，
            // 而不是"执行了几次替换"。否则同一行改三处会报 +3 -3。
            if touched_lines.insert(line_no) {
                res.removed_lines += count_lines(&needle);
                res.added_lines += count_lines(&repl);
            }
            res.applied += 1;
            push_locus(&mut res.loci, line_no, &repl);
            if res.hunks.len() < MAX_HUNKS {
                res.hunks.push(Hunk {
                    line: line_no,
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

fn push_locus(loci: &mut Vec<String>, line_no: usize, repl: &str) {
    if loci.len() >= MAX_LOCI {
        return;
    }
    // 行号区间只取决于替换文本跨了几行（单行替换就只标一个行号）。
    let span = newline_count(repl);
    let entry = if span > 0 {
        format!("{line_no}-{}", line_no + span)
    } else {
        format!("{line_no}")
    };
    // 只在前 MAX_LOCI 条内去重，规模可控，避免 O(n²) 的 contains 扫描。
    if !loci.contains(&entry) {
        loci.push(entry);
    }
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
    if candidates.is_empty() {
        // 退一步：按首行前 8 个非空白字符找
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
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::Eol;

    fn spec(old: &str, new: &str) -> EditSpec {
        EditSpec {
            old: old.into(),
            new: new.into(),
            all: false,
            start: None,
            end: None,
        }
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
