//! DOCX/PDF 文本抽取：只读正文，不解析样式和图片。
//!
//! 目标是让 `fs_read` 能直接看到 Word/PDF 正文，而不是把压缩包或 PDF 当二进制乱码。
//! 复杂版式、页眉页脚、批注和图片仍不在范围内。

use crate::io;
use crate::paths::FsPath;
use quick_xml::events::Event;
use quick_xml::Reader;
use std::io::{Cursor, Read, Write};
use std::path::Path;
use zip::ZipArchive;

pub struct Extracted {
    pub text: String,
    pub size: u64,
    pub hash: String,
}

/// 抽取格式：由扩展名决定，便于 `fs_read`/`fs_grep`/`fs_stat` 统一分派。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Docx,
    Pdf,
}

pub fn format_of(path: &Path) -> Option<Format> {
    let ext = path.extension().and_then(|e| e.to_str())?;
    if ext.eq_ignore_ascii_case("docx") {
        Some(Format::Docx)
    } else if ext.eq_ignore_ascii_case("pdf") {
        Some(Format::Pdf)
    } else {
        None
    }
}

pub fn is_docx(path: &Path) -> bool {
    format_of(path) == Some(Format::Docx)
}

pub fn is_pdf(path: &Path) -> bool {
    format_of(path) == Some(Format::Pdf)
}

pub fn is_document(path: &Path) -> bool {
    format_of(path).is_some()
}

pub fn extract_bytes(format: Format, bytes: &[u8]) -> Result<String, String> {
    match format {
        Format::Docx => extract_docx_bytes(bytes),
        Format::Pdf => extract_pdf_bytes(bytes),
    }
}

fn extract_pdf_bytes(bytes: &[u8]) -> Result<String, String> {
    // 第三方 PDF 解析器在畸形文件上可能 panic；MCP 进程不能因此退出。
    let result = std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(bytes));
    match result {
        Ok(Ok(text)) => Ok(normalize_pdf_text(&text)),
        Ok(Err(e)) => Err(format!("E_PDF 无法抽取正文: {e}")),
        Err(_) => Err("E_PDF PDF 解析失败（文件可能损坏或加密）".to_string()),
    }
}

/// pdf-extract 会在页与页之间留下空行和零宽字符；压成紧凑行，避免浪费 token。
fn normalize_pdf_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0usize;
    for raw in text.split('\n') {
        let line: String = raw
            .chars()
            .filter(|c| !matches!(*c, '\u{0}' | '\u{200b}' | '\u{feff}'))
            .collect();
        let line = line.trim_end();
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

pub fn write_docx(fp: &FsPath, content: &str) -> Result<String, String> {
    let lines = docx_lines(content);
    let body = docx_body(&lines);
    let document = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\">\
<w:body>{body}<w:sectPr/></w:body></w:document>"
    );
    let content_types = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
<Default Extension=\"xml\" ContentType=\"application/xml\"/>\
<Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>\
</Types>";
    let rels = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/>\
</Relationships>";

    let cursor = Cursor::new(Vec::new());
    let mut zip = zip::ZipWriter::new(cursor);
    let options =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, text) in [
        ("[Content_Types].xml", content_types),
        ("_rels/.rels", rels),
        ("word/document.xml", document.as_str()),
    ] {
        zip.start_file(name, options)
            .map_err(|e| format!("E_DOCX 写入压缩包失败: {e}"))?;
        zip.write_all(text.as_bytes())
            .map_err(|e| format!("E_DOCX 写入 {name} 失败: {e}"))?;
    }
    let cursor = zip
        .finish()
        .map_err(|e| format!("E_DOCX 完成压缩包失败: {e}"))?;
    let bytes = cursor.into_inner();
    io::atomic_write(fp, &bytes)?;
    Ok(format!(
        "ok 写入 {} {} {}行 hash={} docx",
        fp.display(),
        io::human_size(bytes.len() as u64),
        lines.len(),
        io::hash_hex(&bytes)
    ))
}

fn docx_lines(content: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') {
        lines.pop();
    }
    if lines.is_empty() {
        lines.push("");
    }
    lines
}

fn docx_body(lines: &[&str]) -> String {
    let mut body = String::new();
    for line in lines {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            body.push_str("<w:p/>");
            continue;
        }
        body.push_str("<w:p><w:r><w:t xml:space=\"preserve\">");
        body.push_str(&xml_escape(line));
        body.push_str("</w:t></w:r></w:p>");
    }
    body
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub fn read_docx(fp: &FsPath) -> Result<Extracted, String> {
    let bytes = io::read_bytes(&fp.wide())?;
    let size = bytes.len() as u64;
    let hash = io::hash_hex(&bytes);
    let text = extract_docx_bytes(&bytes)?;
    Ok(Extracted { text, size, hash })
}

pub fn extract_docx_bytes(bytes: &[u8]) -> Result<String, String> {
    let cursor = Cursor::new(bytes);
    let mut archive =
        ZipArchive::new(cursor).map_err(|e| format!("E_DOCX 不是有效的 DOCX: {e}"))?;
    let mut document = archive
        .by_name("word/document.xml")
        .map_err(|_| "E_DOCX 缺少 word/document.xml".to_string())?;
    let mut xml = String::new();
    document
        .read_to_string(&mut xml)
        .map_err(|e| format!("E_DOCX 读取正文失败: {e}"))?;
    extract_word_xml(&xml)
}

fn extract_word_xml(xml: &str) -> Result<String, String> {
    let mut reader = Reader::from_str(xml);
    reader.trim_text(false);
    let mut out = String::new();
    let mut in_text = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(event)) => match event.local_name().as_ref() {
                b"t" => in_text = true,
                b"tab" => push_tab(&mut out),
                b"br" => push_newline(&mut out),
                _ => {}
            },
            Ok(Event::Empty(event)) => match event.local_name().as_ref() {
                b"tab" => push_tab(&mut out),
                b"br" => push_newline(&mut out),
                _ => {}
            },
            Ok(Event::Text(text)) if in_text => {
                if let Ok(value) = text.unescape() {
                    out.push_str(&value);
                }
            }
            Ok(Event::End(event)) => match event.local_name().as_ref() {
                b"t" => in_text = false,
                b"p" => push_newline(&mut out),
                b"tc" => push_tab(&mut out),
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(e) => return Err(format!("E_DOCX 正文 XML 解析失败: {e}")),
            _ => {}
        }
    }
    Ok(out)
}

fn push_newline(out: &mut String) {
    if !out.ends_with('\n') {
        out.push('\n');
    }
}

fn push_tab(out: &mut String) {
    if !out.ends_with('\t') {
        out.push('\t');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 抽取段落文本和实体() {
        let xml = r#"<?xml version="1.0"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body><w:p><w:r><w:t>第一段 &amp; 内容</w:t></w:r></w:p>
<w:p><w:r><w:t>第二段</w:t></w:r><w:r><w:tab/><w:t>列</w:t></w:r></w:p></w:body></w:document>"#;
        let text = extract_word_xml(xml).unwrap();
        assert!(text.contains("第一段 & 内容"));
        assert!(text.contains("第二段"));
        assert!(text.contains("\t列"));
        assert_eq!(text.lines().filter(|l| !l.is_empty()).count(), 2);
    }

    #[test]
    fn 表格单元格用制表符分隔() {
        let xml = r#"<w:document xmlns:w="x"><w:body><w:tbl>
<w:tr><w:tc><w:p><w:r><w:t>A</w:t></w:r></w:p></w:tc>
<w:tc><w:p><w:r><w:t>B</w:t></w:r></w:p></w:tc></w:tr>
</w:tbl></w:body></w:document>"#;
        let text = extract_word_xml(xml).unwrap();
        assert!(text.contains("A"));
        assert!(text.contains("B"));
        assert!(text.contains('\t'));
    }

    #[test]
    fn pdf_抽取英文正文() {
        let text = extract_pdf_bytes(&minimal_pdf("(hello) Tj")).unwrap();
        assert!(text.contains("hello"), "实际: {text}");
    }

    #[test]
    fn 损坏_pdf_返回错误而不是崩溃() {
        let err = extract_pdf_bytes(b"%PDF-1.4\nbroken\n").unwrap_err();
        assert!(err.starts_with("E_PDF"), "{err}");
    }

    fn minimal_pdf(show: &str) -> Vec<u8> {
        let content = format!("BT /F1 12 Tf 72 720 Td {show} ET");
        let objs = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
             /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
                .to_string(),
            format!(
                "<< /Length {} >>\nstream\n{}\nendstream",
                content.len(),
                content
            ),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
        ];
        let mut out = String::from("%PDF-1.4\n");
        let mut offsets = Vec::new();
        for (i, body) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.push_str(&format!("{} 0 obj\n{}\nendobj\n", i + 1, body));
        }
        let xref = out.len();
        out.push_str(&format!(
            "xref\n0 {}\n0000000000 65535 f \n",
            objs.len() + 1
        ));
        for off in offsets {
            out.push_str(&format!("{off:010} 00000 n \n"));
        }
        out.push_str(&format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objs.len() + 1
        ));
        out.into_bytes()
    }
}
