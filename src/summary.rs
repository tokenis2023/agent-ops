//! 文件结构摘要：只列声明行与行号，不展开函数体。
//!
//! 思路借鉴 rtk 的 "signatures and structure over full bodies"：Agent 打开一个陌生
//! 文件时，第一步要的是「这里有什么、在第几行」，而不是全文。fsops 不照搬它的
//! 两行式摘要，而是保持自己的契约——输出「行号|内容」，行号可以直接接
//! fs_read block=true 去读实现。
//!
//! 这里只做纯文本行扫描：不解析语法树、不引入新依赖，因此对格式异常的文件
//! 最坏情况是漏掉几个符号，绝不会误改内容（摘要只用于读取，不参与编辑）。

/// 按扩展名识别的语言；只影响声明关键词，不影响编码与换行处理。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Language {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Go,
    Java,
    CSharp,
    C,
    Cpp,
    Ruby,
    Shell,
    Data,
    Unknown,
}

/// 按路径扩展名判断语言。数据格式单独一类：它们不该被当成代码摘要。
pub fn language_of(path: &str) -> Language {
    let Some((_, ext)) = path.rsplit_once('.') else {
        return Language::Unknown;
    };
    if ext.contains('/') || ext.contains('\\') {
        return Language::Unknown;
    }
    match ext.to_ascii_lowercase().as_str() {
        "rs" => Language::Rust,
        "py" | "pyw" => Language::Python,
        "js" | "mjs" | "cjs" | "jsx" => Language::JavaScript,
        "ts" | "tsx" | "mts" | "cts" => Language::TypeScript,
        "go" => Language::Go,
        "java" => Language::Java,
        "cs" => Language::CSharp,
        "c" | "h" => Language::C,
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => Language::Cpp,
        "rb" => Language::Ruby,
        "sh" | "bash" | "zsh" | "ps1" => Language::Shell,
        "json" | "jsonc" | "json5" | "yaml" | "yml" | "toml" | "xml" | "csv" | "tsv" | "md"
        | "markdown" | "txt" | "lock" | "log" => Language::Data,
        _ => Language::Unknown,
    }
}

/// 数据格式（JSON/YAML/Markdown…）没有「声明」，摘要对它们没有意义。
pub fn is_data(lang: Language) -> bool {
    matches!(lang, Language::Data)
}

/// 一个符号：行号（1 基）+ 原始行（保留缩进，便于看出嵌套）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub line: usize,
    pub text: String,
}

/// 取行首的标识符词；遇到非标识符字符即停。
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

/// 出现在声明关键字前面的修饰词。
fn is_modifier(word: &str) -> bool {
    matches!(
        word,
        "pub"
            | "async"
            | "unsafe"
            | "extern"
            | "default"
            | "export"
            | "declare"
            | "abstract"
            | "final"
            | "public"
            | "private"
            | "protected"
            | "internal"
            | "sealed"
            | "partial"
            | "virtual"
            | "override"
            | "readonly"
            | "inline"
    )
}

fn is_keyword(word: &str, lang: Language) -> bool {
    match lang {
        Language::Rust => matches!(
            word,
            "fn" | "struct"
                | "enum"
                | "trait"
                | "impl"
                | "mod"
                | "type"
                | "union"
                | "static"
                | "const"
                | "macro_rules"
        ),
        Language::Python => matches!(word, "def" | "class"),
        Language::JavaScript | Language::TypeScript => matches!(
            word,
            "function" | "class" | "interface" | "type" | "enum" | "namespace" | "module"
        ),
        Language::Go => matches!(word, "func" | "type"),
        Language::Java | Language::CSharp => {
            matches!(
                word,
                "class" | "interface" | "enum" | "record" | "struct" | "namespace"
            )
        }
        Language::C | Language::Cpp => {
            matches!(word, "struct" | "class" | "enum" | "union" | "namespace")
        }
        Language::Ruby => matches!(word, "def" | "class" | "module"),
        Language::Shell => matches!(word, "function"),
        Language::Unknown | Language::Data => matches!(
            word,
            "fn" | "def"
                | "class"
                | "function"
                | "func"
                | "struct"
                | "enum"
                | "trait"
                | "interface"
                | "type"
                | "module"
                | "namespace"
        ),
    }
}

/// JavaScript/TypeScript 的箭头函数：export const foo = async (…) =>
fn is_js_binding_declaration(word: &str, after: &str) -> bool {
    if !matches!(word, "const" | "let" | "var") {
        return false;
    }
    let Some((_name, rest)) = take_word(after) else {
        return false;
    };
    let Some(rest) = rest.trim_start().strip_prefix('=') else {
        return false;
    };
    let rest = rest.trim_start();
    rest.starts_with('(')
        || rest.starts_with("async (")
        || rest.starts_with("async(")
        || rest.starts_with("function")
        || rest.starts_with("class")
}

/// 判断一行（已 trim）是不是声明行。
fn is_declaration(trimmed: &str, lang: Language) -> bool {
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.starts_with("//")
        || trimmed.starts_with("/*")
        || trimmed.starts_with('*')
        || trimmed.starts_with('@')
        || trimmed.starts_with("\"\"\"")
        || trimmed.starts_with("'''")
    {
        return false;
    }
    if trimmed.starts_with('#') && !trimmed.starts_with("#[") {
        return false;
    }

    let mut rest = trimmed;
    for _ in 0..8 {
        let Some((word, after)) = take_word(rest) else {
            return false;
        };
        if word == "pub" {
            let candidate = after.trim_start();
            if candidate.starts_with('(') {
                if let Some(close) = candidate.find(')') {
                    rest = &candidate[close + 1..];
                    continue;
                }
            }
            rest = after;
            continue;
        }
        if word == "extern" {
            let candidate = after.trim_start();
            if let Some(stripped) = candidate.strip_prefix('"') {
                if let Some(close) = stripped.find('"') {
                    rest = &stripped[close + 1..];
                    continue;
                }
            }
            rest = after;
            continue;
        }
        if lang == Language::Shell && after.trim_start().starts_with("()") {
            return true;
        }
        if is_modifier(word) {
            rest = after;
            continue;
        }
        if matches!(lang, Language::JavaScript | Language::TypeScript)
            && is_js_binding_declaration(word, after)
        {
            return true;
        }
        return is_keyword(word, lang);
    }
    false
}

/// 提取结构摘要。行号 1 基；保留原始缩进。
pub fn extract(text: &str, lang: Language) -> Vec<Symbol> {
    let mut out = Vec::new();
    // Rust 的 mod tests 会贡献上百个测试函数，对「文件结构」是纯噪声。
    // 不能只认「列 0 的 '}'」：测试体里的多行字符串常把 '}' 写在列 0
    // （本文件的测试就是这样），会提前退出并把后面的测试函数当成正式声明。
    // 这里做字符串感知的括号计数：字符串里的花括号不计入深度。
    let mut in_tests = false;
    let mut test_depth = 0i64;
    let mut in_string = false;
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if in_tests {
            test_depth += brace_delta(line, &mut in_string);
            if test_depth <= 0 && !in_string {
                in_tests = false;
            }
            continue;
        }
        if lang == Language::Rust && is_test_module(trimmed) {
            in_tests = true;
            test_depth = brace_delta(line, &mut in_string);
            continue;
        }
        if is_declaration(trimmed, lang) {
            out.push(Symbol {
                line: index + 1,
                text: line.trim_end().to_string(),
            });
        }
    }
    out
}

/// 判断一行是否是 Rust 测试模块声明（mod tests / mod summary_tests / mod io_tests…）。
///
/// 只认 "mod tests" 会漏掉 `mod summary_tests` 这类按模块命名的测试块，
/// 它们后面的测试函数会全被当成正式声明。
fn is_test_module(trimmed: &str) -> bool {
    if trimmed.starts_with("//") || !trimmed.contains('{') {
        return false;
    }
    let Some((_, rest)) = trimmed.split_once("mod ") else {
        return false;
    };
    let name: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    name.ends_with("tests")
}

/// 统计一行里参与语法结构的花括号增量，跳过字符串字面量。
///
/// `in_string` 跨行保持：Rust 的多行字符串（行尾 `\` 续行）里的花括号
/// 不能计入，否则测试模块会在字符串中途被判定结束。
fn brace_delta(line: &str, in_string: &mut bool) -> i64 {
    let bytes = line.as_bytes();
    let mut i = 0usize;
    let mut delta = 0i64;
    let mut string = *in_string;
    while i < bytes.len() {
        let b = bytes[i];
        if string {
            // 行尾反斜杠是字符串续行，保持 string 状态。
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == b'"' {
                string = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'/' if bytes.get(i + 1) == Some(&b'/') => break,
            b'"' => string = true,
            b'{' => delta += 1,
            b'}' => delta -= 1,
            _ => {}
        }
        i += 1;
    }
    *in_string = string;
    delta
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 语言按扩展名识别且数据格式单独归类() {
        assert_eq!(language_of("/d/a/main.rs"), Language::Rust);
        assert_eq!(language_of("C:\\x\\b.py"), Language::Python);
        assert_eq!(language_of("a.tsx"), Language::TypeScript);
        assert_eq!(language_of("a.json"), Language::Data);
        assert_eq!(language_of("a.md"), Language::Data);
        assert_eq!(language_of("Makefile"), Language::Unknown);
        assert!(is_data(language_of("package.json")));
    }

    #[test]
    fn rust_提取声明并保留行号() {
        let text = "use std::x;\n\n#[derive(Debug)]\npub struct Snapshot {\n    a: u8,\n}\n\nimpl Snapshot {\n    pub fn new() -> Self { Self { a: 0 } }\n}\n\npub async fn run() {}\n";
        let symbols = extract(text, Language::Rust);
        let lines: Vec<usize> = symbols.iter().map(|s| s.line).collect();
        assert_eq!(lines, vec![4, 8, 9, 12], "{symbols:?}");
        assert!(symbols[0].text.starts_with("pub struct Snapshot"));
    }

    #[test]
    fn rust_跳过mod_tests整块() {
        let text = "fn keep() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn 用例一() {}\n    fn helper() {}\n}\n\nfn also_keep() {}\n";
        let symbols = extract(text, Language::Rust);
        let names: Vec<&str> = symbols.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names[0].contains("keep"));
        assert!(names[1].contains("also_keep"));
    }

    #[test]
    fn rust_测试体多行字符串里的列首括号不提前结束跳过() {
        // 测试体里写多行字符串时，内容里的 `}` 可能落在列 0；
        // 只认“列 0 的 }”会提前退出，把后面的测试函数当成正式声明。
        let text = "fn keep() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn case_one() {\n        let content = \"\\\nfn fake() {\n}\n\";\n    }\n\n    #[test]\n    fn case_two() {\n        assert!(true);\n    }\n}\n\nfn also_keep() {}\n";
        let symbols = extract(text, Language::Rust);
        let names: Vec<&str> = symbols.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(names.len(), 2, "测试体不应漏出符号: {names:?}");
        assert!(names[1].contains("also_keep"));
    }

    #[test]
    fn rust_命名测试模块也跳过() {
        // tools.rs 用 mod summary_tests 分块；只认 “mod tests” 会漏掉它，
        // 导致后面的测试函数全被当成正式声明。
        let text =
            "fn keep() {}\n\nmod summary_tests {\n    fn case() {}\n}\n\nfn also_keep() {}\n";
        let symbols = extract(text, Language::Rust);
        let names: Vec<&str> = symbols.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(names.len(), 2, "命名测试模块未跳过: {names:?}");
    }

    #[test]
    fn python_识别async_def与class() {
        let text = "# 注释\ndef plain():\n    pass\n\nclass Box:\n    def method(self):\n        pass\n\nasync def later():\n    pass\n";
        let symbols = extract(text, Language::Python);
        let names: Vec<&str> = symbols.iter().map(|s| s.text.trim()).collect();
        assert!(names.iter().any(|n| n.starts_with("def plain")));
        assert!(names.iter().any(|n| n.starts_with("class Box")));
        assert!(names.iter().any(|n| n.starts_with("async def later")));
    }

    #[test]
    fn typescript_识别导出与箭头函数() {
        let text = "export interface Options { a: string }\nexport class Store {}\nexport function boot() {}\nexport const load = async () => {}\nconst notADecl = 1;\n";
        let symbols = extract(text, Language::TypeScript);
        let names: Vec<&str> = symbols.iter().map(|s| s.text.trim()).collect();
        assert_eq!(names.len(), 4, "{names:?}");
        assert!(names.iter().any(|n| n.contains("load")));
        assert!(!names.iter().any(|n| n.contains("notADecl")));
    }

    #[test]
    fn go_识别func与type() {
        let text = "package main\n\nfunc main() {}\n\nfunc (s *Server) Serve() {}\n\ntype Config struct {}\n";
        let symbols = extract(text, Language::Go);
        assert_eq!(symbols.len(), 3, "{symbols:?}");
    }

    #[test]
    fn 注释行与属性行不算声明() {
        let text = "// fn fake()\n/* fn block_fake() */\n#[cfg(test)]\ndef real():\n    pass\n";
        let symbols = extract(text, Language::Python);
        assert_eq!(symbols.len(), 1, "{symbols:?}");
        assert!(symbols[0].text.contains("def real"));
    }
}
