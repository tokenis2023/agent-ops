//! 文本层：编码识别、换行风格、BOM 保留。
//!
//! 中文 Windows 上这一步最关键：文件可能是 GBK、UTF-16LE（记事本旧版默认）、
//! UTF-8 带 BOM。模型只认 UTF-8 文本，所以读的时候统一转码，
//! 写回时再按原编码落盘，避免"改一个字符整个文件变乱码"。

use encoding_rs::{Encoding, GBK, UTF_16BE, UTF_16LE, UTF_8};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eol {
    Lf,
    CrLf,
    Cr,
    /// 同一文件里混用多种换行
    Mixed,
}

impl Eol {
    pub fn as_str(self) -> &'static str {
        match self {
            Eol::Lf => "LF",
            Eol::CrLf => "CRLF",
            Eol::Cr => "CR",
            Eol::Mixed => "Mixed",
        }
    }

    pub fn bytes(self) -> &'static str {
        match self {
            Eol::CrLf => "\r\n",
            _ => "\n",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Doc {
    /// 已解码为 UTF-8 的文本
    pub text: String,
    pub encoding: &'static str,
    pub eol: Eol,
    pub bom: bool,
    pub size: u64,
}

/// 判断字节流是否为二进制（而非可安全当文本编辑的内容）。
///
/// 判据：前 8KB 含 NUL，且不属于合法的 UTF-16（UTF-16 文本天然含大量 NUL）。
/// 目的不是“精确识别所有格式”，而是在读/写前拦住会把二进制当文本糟蹋的操作。
pub fn is_binary(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    // BOM 是最权威的信号：带 UTF-16/UTF-8 BOM 的就是文本。
    // 纯中文的 UTF-16 文本几乎没有零字节（高位不是 0），
    // 光靠零字节分布会把它们误判成二进制。
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) {
        return false;
    }
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return false;
    }
    let sample = &bytes[..bytes.len().min(8192)];
    if !sample.contains(&0) {
        return false;
    }
    // 真 UTF-16 文本的零字节几乎只出现在单一奇偶位（ASCII 高位为零）。
    // 但极小样本里偶发 1 个 NUL 也符合这个形态，因此要求至少 2 个零字节
    // 才承认“这是 UTF-16”，否则一律按二进制拒绝——宁可误拒也不毁文件。
    let zeros_even = sample.iter().step_by(2).filter(|&&b| b == 0).count();
    let zeros_odd = sample
        .iter()
        .skip(1)
        .step_by(2)
        .filter(|&&b| b == 0)
        .count();
    let total_zeros = zeros_even + zeros_odd;
    if total_zeros < 2 {
        return true;
    }
    let dominant = zeros_even.max(zeros_odd);
    if dominant * 10 < total_zeros * 9 {
        return true; // 零字节分布均匀，不是 UTF-16
    }
    // 零字节集中在单一奇偶位：再确认解码结果像正常文本。
    let little = zeros_odd >= zeros_even;
    !looks_like_utf16_text(bytes, little)
}

/// 把字节按 UTF-16 解码，判断结果是否像正常文本（而非二进制乱码）。
fn looks_like_utf16_text(bytes: &[u8], little: bool) -> bool {
    if bytes.len() < 4 || !bytes.len().is_multiple_of(2) {
        return false;
    }
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| {
            if little {
                u16::from_le_bytes([c[0], c[1]])
            } else {
                u16::from_be_bytes([c[0], c[1]])
            }
        })
        .collect();
    let mut printable = 0usize;
    for ch in String::from_utf16_lossy(&units).chars() {
        if ch == '\u{FFFD}' {
            return false;
        }
        if !ch.is_control() || matches!(ch, '\t' | '\n' | '\r') {
            printable += 1;
        }
    }
    let total = units.len();
    total > 0 && printable * 10 >= total * 7
}

/// 识别编码并解码。顺序：BOM → 无 BOM 的 UTF-16 特征 → UTF-8 校验 → GBK → 容错 UTF-8。
pub fn decode(bytes: &[u8]) -> Doc {
    let (text, encoding, bom) = if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        (String::from_utf8_lossy(rest).to_string(), "utf-8-bom", true)
    } else if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        (decode_utf16(rest, true), "utf-16le", true)
    } else if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        (decode_utf16(rest, false), "utf-16be", true)
    } else if looks_like_utf16_le(bytes) {
        (decode_utf16(bytes, true), "utf-16le", false)
    } else if looks_like_utf16_be(bytes) {
        (decode_utf16(bytes, false), "utf-16be", false)
    } else if std::str::from_utf8(bytes).is_ok() {
        (String::from_utf8_lossy(bytes).to_string(), "utf-8", false)
    } else {
        // 非法 UTF-8：优先按 GBK（中文环境），失败再容错
        let (cow, _, had_errors) = GBK.decode(bytes);
        if !had_errors && !cow.is_empty() {
            (cow.to_string(), "gbk", false)
        } else {
            // 尝试 GB18030 覆盖面更大
            let (cow, _, had) = encoding_rs::GB18030.decode(bytes);
            if !had {
                (cow.to_string(), "gb18030", false)
            } else {
                (
                    String::from_utf8_lossy(bytes).to_string(),
                    "utf-8-lossy",
                    false,
                )
            }
        }
    };

    let eol = detect_eol(&text);
    Doc {
        text,
        encoding,
        eol,
        bom,
        size: bytes.len() as u64,
    }
}

fn decode_utf16(bytes: &[u8], little: bool) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| {
            if little {
                u16::from_le_bytes([c[0], c[1]])
            } else {
                u16::from_be_bytes([c[0], c[1]])
            }
        })
        .collect();
    String::from_utf16_lossy(&units)
}

/// 无 BOM 的 UTF-16：ASCII 文本里会出现大量 0x00 间隔字节。
fn looks_like_utf16_le(b: &[u8]) -> bool {
    if b.len() < 4 || !b.len().is_multiple_of(2) {
        return false;
    }
    let sample = &b[..b.len().min(200)];
    let zeros_at_odd = sample
        .iter()
        .skip(1)
        .step_by(2)
        .filter(|&&x| x == 0)
        .count();
    zeros_at_odd * 4 > sample.len() / 2 * 3
}

fn looks_like_utf16_be(b: &[u8]) -> bool {
    if b.len() < 4 || !b.len().is_multiple_of(2) {
        return false;
    }
    let sample = &b[..b.len().min(200)];
    let zeros_at_even = sample.iter().step_by(2).filter(|&&x| x == 0).count();
    zeros_at_even * 4 > sample.len() / 2 * 3
}

pub fn detect_eol(text: &str) -> Eol {
    let mut crlf = 0usize;
    let mut lf = 0usize;
    let mut cr = 0usize;
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\r' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                    crlf += 1;
                    i += 2;
                    continue;
                }
                cr += 1;
            }
            b'\n' => lf += 1,
            _ => {}
        }
        i += 1;
    }
    let kinds = [crlf > 0, lf > 0, cr > 0].iter().filter(|&&x| x).count();
    if kinds > 1 {
        Eol::Mixed
    } else if crlf > 0 {
        Eol::CrLf
    } else if cr > 0 {
        Eol::Cr
    } else {
        Eol::Lf
    }
}

/// 编码回字节；保留 BOM。目标编码无法表示文本时拒绝写入。
pub fn encode(doc: &Doc, text: &str) -> Result<Vec<u8>, String> {
    let enc: &'static Encoding = match doc.encoding {
        "utf-8-bom" => UTF_8,
        "utf-16le" => UTF_16LE,
        "utf-16be" => UTF_16BE,
        "gbk" => GBK,
        "gb18030" => encoding_rs::GB18030,
        _ => UTF_8,
    };
    let mut out = Vec::with_capacity(text.len() + 3);
    if doc.bom {
        match doc.encoding {
            "utf-16le" => out.extend_from_slice(&[0xFF, 0xFE]),
            "utf-16be" => out.extend_from_slice(&[0xFE, 0xFF]),
            _ => out.extend_from_slice(&[0xEF, 0xBB, 0xBF]),
        }
    }
    if enc == UTF_16LE || enc == UTF_16BE {
        let little = enc == UTF_16LE;
        for u in text.encode_utf16() {
            let b = if little {
                u.to_le_bytes()
            } else {
                u.to_be_bytes()
            };
            out.extend_from_slice(&b);
        }
        return Ok(out);
    }
    let (cow, _, had_errors) = enc.encode(text);
    if had_errors {
        // 只报"包含无法表示的字符"等于没说：模型不知道是哪个字符，
        // 只能整段重写反复试。这里给出具体字符与位置。
        let mut bad: Vec<(usize, char)> = Vec::new();
        for (idx, ch) in text.char_indices() {
            if bad.len() >= 5 {
                break;
            }
            let mut probe = [0u8; 8];
            let one = ch.encode_utf8(&mut probe);
            let (_, _, err) = enc.encode(one);
            if err {
                let line = text[..idx].matches('\n').count() + 1;
                bad.push((line, ch));
            }
        }
        let detail = bad
            .iter()
            .map(|(line, ch)| format!("U+{:04X} '{ch}'（第 {line} 行）", *ch as u32))
            .collect::<Vec<_>>()
            .join("、");
        return Err(format!(
            "E_ENCODING_LOSS 新文本有 {} 无法表示的字符：{}；请替换这些字符或改用 UTF-8 文件，文件未修改",
            doc.encoding, detail
        ));
    }
    out.extend_from_slice(&cow);
    Ok(out)
}

/// 把文本统一成 LF，便于做跨平台的精确匹配。
pub fn to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// 把 LF 文本转换回目标换行风格。
pub fn from_lf(text: &str, eol: Eol) -> String {
    match eol {
        Eol::CrLf => text.replace('\n', "\r\n"),
        Eol::Cr => text.replace('\n', "\r"),
        _ => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 编码损失时报出具体字符与行号() {
        // 只说"包含无法表示的字符"等于没说：模型不知道是哪个字符，
        // 只能整段重写反复试。
        let doc = decode(b"\xd6\xd0\xce\xc4\n");
        let err = encode(&doc, "第一行\n第二行\n第三行😀\n").unwrap_err();
        assert!(err.contains("E_ENCODING_LOSS"), "{err}");
        assert!(err.contains("U+1F600"), "应给出码位: {err}");
        assert!(err.contains("😀"), "应给出字符本身: {err}");
        assert!(err.contains("第 3 行"), "应给出所在行: {err}");
        assert!(err.contains("UTF-8"), "应给出替代方案: {err}");
    }

    #[test]
    fn gbk_文件能正确解码并写回() {
        let gbk_bytes = b"\xd6\xd0\xce\xc4\r\nabc\r\n"; // "中文\r\nabc\r\n"
        let doc = decode(gbk_bytes);
        assert_eq!(doc.encoding, "gbk");
        assert_eq!(doc.eol, Eol::CrLf);
        assert!(doc.text.contains("中文"));
        assert_eq!(encode(&doc, &doc.text).unwrap(), gbk_bytes.to_vec());
    }

    #[test]
    fn utf16le_带bom往返一致() {
        let mut bytes = vec![0xFF, 0xFE];
        for u in "中文\n".encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        let doc = decode(&bytes);
        assert_eq!(doc.encoding, "utf-16le");
        assert_eq!(encode(&doc, &doc.text).unwrap(), bytes);
    }

    #[test]
    fn 识别混合换行() {
        assert_eq!(detect_eol("a\r\nb\nc"), Eol::Mixed);
        assert_eq!(detect_eol("a\nb\n"), Eol::Lf);
        assert_eq!(detect_eol("a\r\nb\r\n"), Eol::CrLf);
    }
}
