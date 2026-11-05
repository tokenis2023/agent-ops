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
    /// 替换成什么；空串表示删除
    #[serde(default)]
    pub new: String,
    /// 是否替换所有出现位置（默认只允许唯一匹配）
    #[serde(default)]
    pub all: bool,
}

#[derive(Debug, Default)]
pub struct EditResult {
    pub applied: usize,
    pub added_lines: i64,
    pub removed_lines: i64,
    /// 每处修改的落点，如 `12`、`40-52`
    pub loci: Vec<String>,
    pub encoding: String,
    pub eol: String,
}

/// 在已解码文本上按原换行风格匹配、修改，返回新文本与摘要。
pub fn apply(original: &str, eol: Eol, edits: &[EditSpec]) -> Result<(String, EditResult), String> {
    let mut work = text::to_lf(original);
    let base_encoding = "utf-8";
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

        let positions: Vec<usize> = find_all(&work, &needle);
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
        // 从后往前改，避免偏移失效
        for pos in targets.iter().rev() {
            let line_no = line_of(&work, *pos);
            work.replace_range(*pos..*pos + needle.len(), &repl);
            res.removed_lines += count_lines(&needle);
            res.added_lines += count_lines(&repl);
            res.applied += 1;
            push_locus(&mut res.loci, line_no, &repl);
        }
    }

    let out = text::from_lf(&work, eol);
    Ok((out, res))
}

fn count_lines(s: &str) -> i64 {
    s.matches('\n').count() as i64
}

fn push_locus(loci: &mut Vec<String>, line_no: usize, repl: &str) {
    let span = count_lines(repl);
    let entry = if span > 0 {
        format!("{line_no}-{}", line_no + span as usize)
    } else {
        format!("{line_no}")
    };
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
        }
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
}
