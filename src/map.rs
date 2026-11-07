//! Repo map lite：一次调用回答「先看哪些文件」。
//!
//! 不引 tree-sitter：复用 summary 的声明提取，用「符号被多少文件引用」
//! 做粗粒度中心性排序。目标是让 Agent 少跑几轮 glob → grep → read。

use crate::paths::FsPath;
use crate::summary::{self, Symbol};
use crate::walk::{self, WalkOpts};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_SYMBOLS_PER_FILE: usize = 5;

struct FileInfo {
    path: String,
    symbols: Vec<Symbol>,
    score: usize,
    refs: usize,
}

pub fn build(
    root: &FsPath,
    pattern: &str,
    focus: &str,
    budget: usize,
    max_files: usize,
    time_budget_ms: u128,
) -> String {
    let opts = WalkOpts {
        include_dirs: false,
        limit: max_files,
        follow_links: false,
        offset: 0,
        time_budget_ms,
        no_ignore: false,
    };
    let (entries, report) = match walk::glob_with_report(root, pattern, &opts) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let deadline =
        Instant::now() + Duration::from_millis(time_budget_ms.min(u64::MAX as u128) as u64);
    let mut files: Vec<FileInfo> = Vec::new();
    let mut skipped = 0usize;
    let mut timed_out = false;
    for entry in &entries {
        if entry.is_dir || entry.size > MAX_FILE_BYTES {
            continue;
        }
        let lang = summary::language_of(&entry.path);
        if summary::is_data(lang) || lang == summary::Language::Unknown {
            continue;
        }
        if Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        let full = resolve(root, &entry.path);
        let Ok(fp) = FsPath::parse(&full) else {
            skipped += 1;
            continue;
        };
        let Ok(snapshot) = crate::io::read_snapshot(&fp) else {
            skipped += 1;
            continue;
        };
        let symbols = summary::extract(&snapshot.doc.text, lang);
        if symbols.is_empty() {
            continue;
        }
        files.push(FileInfo {
            path: full,
            symbols,
            score: 0,
            refs: 0,
        });
    }
    if files.is_empty() {
        return format!("ok 无匹配（{} pattern={pattern}）", root.display());
    }

    // 全局符号表：只统计真正声明出来的名字（impl/use 不算声明）。
    let mut names: HashSet<String> = HashSet::new();
    for file in &files {
        for symbol in &file.symbols {
            if let Some(name) = symbol_name(&symbol.text) {
                // 模块声明（pub mod x;）只是结构，不是可引用条目；
                // TEST_ 开头的常量是测试桩，避免把 io.rs 顶到最前。
                if !name.starts_with("TEST_") {
                    names.insert(name);
                }
            }
        }
    }
    // 第二遍统计引用：同一文件对同一符号只计一次。
    let mut refs: HashMap<String, usize> = HashMap::new();
    for file in &files {
        if Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        let Ok(fp) = FsPath::parse(&file.path) else {
            continue;
        };
        let Ok(snapshot) = crate::io::read_snapshot(&fp) else {
            continue;
        };
        let mut seen: HashSet<&str> = HashSet::new();
        for_each_ident(&snapshot.doc.text, |ident| {
            if names.contains(ident) {
                seen.insert(ident);
            }
        });
        for name in seen {
            *refs.entry(name.to_string()).or_default() += 1;
        }
    }

    let focus_terms: Vec<String> = focus
        .split_whitespace()
        .map(|term| term.to_lowercase())
        .collect();
    for file in &mut files {
        let mut score = 0usize;
        let mut ref_total = 0usize;
        for symbol in &file.symbols {
            if let Some(name) = symbol_name(&symbol.text) {
                let count = refs.get(&name).copied().unwrap_or(0);
                let external = count.saturating_sub(1);
                score += external;
                ref_total += external;
            }
        }
        if is_entry_point(&file.path) {
            score += 5;
        }
        if !focus_terms.is_empty() {
            let lower_path = file.path.to_lowercase();
            for term in &focus_terms {
                if lower_path.contains(term) {
                    score += 5;
                }
                for symbol in &file.symbols {
                    if symbol.text.to_lowercase().contains(term) {
                        score += 5;
                    }
                }
            }
        }
        file.score = score;
        file.refs = ref_total;
    }
    files.sort_by(|a, b| {
        // 测试/基准文件统一排到实现文件之后：它们引用大量符号，
        // 只按中心性会把 tests/ 顶到最前，而 map 要回答的是「先看哪些实现」。
        is_test_path(&a.path)
            .cmp(&is_test_path(&b.path))
            .then_with(|| b.score.cmp(&a.score))
            .then_with(|| a.path.cmp(&b.path))
    });

    let blocks: Vec<String> = files
        .iter()
        .map(|file| render_block(file, &focus_terms))
        .collect();
    let mut shown = 0usize;
    let body = crate::render::converge_budget(budget, |body_budget| {
        let mut text = String::new();
        shown = 0;
        for block in &blocks {
            if text.len() + block.len() > body_budget {
                break;
            }
            text.push_str(block);
            shown += 1;
        }
        text
    });
    if shown == 0 {
        return format!(
            "E_BUDGET repo map 有 {} 个文件但预算 {budget} 不够输出一个条目；请增大 budget",
            files.len()
        );
    }
    let mut out = body;
    out.push_str(&format!(
        "\n… 扫描 {} 个文件 / {} 个符号；显示前 {} 个",
        files.len(),
        files.iter().map(|f| f.symbols.len()).sum::<usize>(),
        shown
    ));
    if shown < files.len() {
        out.push_str(" | 想看其余文件: 调大 budget 或 fs_glob focus=<词> 缩小");
    }
    if report.stopped {
        out.push_str("（达到 max_files 上限）");
    }
    if timed_out {
        out.push_str("；时间预算用尽");
    }
    if skipped > 0 {
        out.push_str(&format!("；跳过 {skipped} 个不可读文件"));
    }
    out.push('\n');
    out
}

/// walk 返回的是相对 root 的路径；读取和展示都要先补成完整路径。
fn resolve(root: &FsPath, rel: &str) -> String {
    if rel.starts_with('/') || rel.contains(":/") || rel.contains(":\\") {
        return rel.to_string();
    }
    let base = root.display().to_string();
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        rel.trim_start_matches("./")
    )
}

/// 测试/基准路径：它们不是「先看哪些实现」的答案。
fn is_test_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    if lower.contains("/tests/")
        || lower.contains("/test/")
        || lower.contains("/benches/")
        || lower.contains("/__tests__/")
    {
        return true;
    }
    let name = lower.rsplit('/').next().unwrap_or("");
    name.starts_with("test_")
        || name.contains("_test.")
        || name.contains("_tests.")
        || name.contains(".spec.")
        || name.contains(".test.")
}

fn render_block(file: &FileInfo, focus_terms: &[String]) -> String {
    let mut block = format!(
        "\n{}  符号 {} · 被引用 {} 处\n",
        file.path,
        file.symbols.len(),
        file.refs
    );
    let mut ordered: Vec<&Symbol> = file.symbols.iter().collect();
    if !focus_terms.is_empty() {
        // 命中 focus 的符号排前面；稳定排序保持原有行号顺序。
        ordered.sort_by_key(|symbol| {
            let lower = symbol.text.to_lowercase();
            !focus_terms.iter().any(|term| lower.contains(term))
        });
    }
    for symbol in ordered.iter().take(MAX_SYMBOLS_PER_FILE) {
        block.push_str(&format!("  {}|{}\n", symbol.line, symbol.text.trim_start()));
    }
    if file.symbols.len() > MAX_SYMBOLS_PER_FILE {
        block.push_str(&format!(
            "  … 还有 {} 个符号（完整结构用 fs_read summary=true）\n",
            file.symbols.len() - MAX_SYMBOLS_PER_FILE
        ));
    }
    block
}

/// 从声明行里解析出符号名；impl/use 等不算声明。
fn symbol_name(text: &str) -> Option<String> {
    let trimmed = text.trim_start();
    // 模块声明不是可引用条目：pub mod x; 的名字会污染引用统计。
    if trimmed.starts_with("mod ")
        || trimmed.starts_with("pub mod ")
        || trimmed.starts_with("pub(crate) mod ")
    {
        return None;
    }
    let mut rest = text.trim_start();
    for _ in 0..10 {
        let (word, after) = take_word(rest)?;
        let after_trim = after.trim_start();
        // Shell：name() { ... }
        if after_trim.starts_with("()") && !is_modifier(word) && !is_decl_keyword(word) {
            return Some(word.to_string());
        }
        if word == "pub" && after_trim.starts_with('(') {
            if let Some(close) = after_trim.find(')') {
                rest = &after_trim[close + 1..];
                continue;
            }
        }
        if word == "extern" {
            if let Some(stripped) = after_trim.strip_prefix('"') {
                if let Some(close) = stripped.find('"') {
                    rest = &stripped[close + 1..];
                    continue;
                }
            }
            rest = after;
            continue;
        }
        if is_modifier(word) {
            rest = after;
            continue;
        }
        if is_decl_keyword(word) {
            let (name, _) = take_word(after_trim)?;
            return Some(name.to_string());
        }
        rest = after;
    }
    None
}

fn take_word(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    let mut end = 0usize;
    for (index, ch) in s.char_indices() {
        if ch.is_alphanumeric() || ch == '_' || ch == '!' {
            end = index + ch.len_utf8();
        } else {
            break;
        }
    }
    if end == 0 {
        None
    } else {
        Some((&s[..end], &s[end..]))
    }
}

fn is_modifier(word: &str) -> bool {
    matches!(
        word,
        "pub"
            | "public"
            | "private"
            | "protected"
            | "internal"
            | "async"
            | "unsafe"
            | "export"
            | "default"
            | "abstract"
            | "final"
            | "sealed"
            | "override"
            | "readonly"
            | "extern"
    )
}

fn is_decl_keyword(word: &str) -> bool {
    matches!(
        word,
        "fn" | "fun"
            | "func"
            | "function"
            | "def"
            | "struct"
            | "enum"
            | "trait"
            | "class"
            | "interface"
            | "type"
            | "const"
            | "static"
            | "mod"
            | "union"
            | "namespace"
            | "object"
            | "record"
            | "protocol"
            | "extension"
    )
}

fn is_entry_point(path: &str) -> bool {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    matches!(
        name,
        "main.rs"
            | "lib.rs"
            | "mod.rs"
            | "main.go"
            | "main.py"
            | "app.py"
            | "__init__.py"
            | "index.ts"
            | "index.tsx"
            | "index.js"
            | "index.jsx"
    )
}

fn for_each_ident<'a>(text: &'a str, mut f: impl FnMut(&'a str)) {
    let bytes = text.as_bytes();
    let mut start: Option<usize> = None;
    for (index, byte) in bytes.iter().enumerate() {
        let is_ident = byte.is_ascii_alphanumeric() || *byte == b'_';
        match (is_ident, start) {
            (true, None) => start = Some(index),
            (false, Some(begin)) => {
                f(&text[begin..index]);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(begin) = start {
        f(&text[begin..]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn 目录(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fsops-map-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn 建仓库(name: &str) -> std::path::PathBuf {
        let dir = 目录(name);
        fs::write(
            dir.join("core.rs"),
            "pub struct Token {}\npub fn verify() {}\n",
        )
        .unwrap();
        fs::write(
            dir.join("user.rs"),
            "fn use_it() {\n    let t = Token;\n    verify();\n}\n",
        )
        .unwrap();
        fs::write(dir.join("main.rs"), "fn main() {}\n").unwrap();
        fs::write(dir.join("notes.md"), "# 标题\n").unwrap();
        fs::write(dir.join("tiny.txt"), "hello\n").unwrap();
        dir
    }

    fn 输出(dir: &std::path::Path, focus: &str, max_files: usize) -> String {
        let root = FsPath::parse(&dir.to_string_lossy().replace('\\', "/")).unwrap();
        build(&root, "**/*", focus, 8192, max_files, 5000)
    }

    #[test]
    fn 被引用多的文件排在前面且数据文件被跳过() {
        let dir = 建仓库("rank");
        let out = 输出(&dir, "", 200);
        let _ = fs::remove_dir_all(&dir);
        let core = out.find("core.rs").expect("应包含 core.rs");
        let user = out.find("user.rs").expect("应包含 user.rs");
        assert!(core < user, "被引用的 core.rs 应排在 user.rs 前: {out}");
        assert!(!out.contains("notes.md"), "数据文件应跳过: {out}");
        assert!(!out.contains("tiny.txt"), "未知语言应跳过: {out}");
    }

    #[test]
    fn 测试目录的文件排在实现文件之后() {
        let dir = 建仓库("tests-order");
        fs::create_dir_all(dir.join("tests")).unwrap();
        // 测试文件引用所有实现符号，只按引用数会让它排到最前，
        // 而 repo map 要回答的是「先看哪些实现文件」。
        fs::write(
            dir.join("tests").join("integration.rs"),
            "fn case() {\n    Token;\n    verify();\n    use_it();\n    main();\n}\n",
        )
        .unwrap();
        let out = 输出(&dir, "", 200);
        let _ = fs::remove_dir_all(&dir);
        let tests_at = out.find("/tests/integration.rs").expect("应包含测试文件");
        let impl_at = out.find("/core.rs").expect("应包含 core.rs");
        assert!(impl_at < tests_at, "实现文件应排在测试文件之前: {out}");
    }

    #[test]
    fn 入口点获得加分() {
        let dir = 建仓库("entry");
        let out = 输出(&dir, "", 200);
        let _ = fs::remove_dir_all(&dir);
        let main = out.find("main.rs").expect("应包含 main.rs");
        let user = out.find("user.rs").expect("应包含 user.rs");
        assert!(main < user, "入口点 main.rs 应排在 user.rs 前: {out}");
    }

    #[test]
    fn focus命中把目标文件提前() {
        let dir = 目录("focus");
        fs::write(dir.join("alpha.rs"), "fn alpha_one() {}\n").unwrap();
        fs::write(dir.join("beta.rs"), "fn beta_two() {}\n").unwrap();
        let out = 输出(&dir, "beta", 200);
        let _ = fs::remove_dir_all(&dir);
        let beta = out.find("beta.rs").expect("应包含 beta.rs");
        let alpha = out.find("alpha.rs").expect("应包含 alpha.rs");
        assert!(beta < alpha, "focus=beta 应把 beta.rs 提前: {out}");
    }

    #[test]
    fn 达到文件上限时明确标注() {
        let dir = 目录("limit");
        for i in 0..5 {
            fs::write(dir.join(format!("f{i}.rs")), format!("fn f{i}() {{}}\n")).unwrap();
        }
        let out = 输出(&dir, "", 2);
        let _ = fs::remove_dir_all(&dir);
        assert!(out.contains("达到 max_files 上限"), "{out}");
    }

    #[test]
    fn 无匹配时给出空结果() {
        let dir = 目录("empty");
        let out = 输出(&dir, "", 200);
        let _ = fs::remove_dir_all(&dir);
        assert!(out.starts_with("ok 无匹配"), "{out}");
    }
}
