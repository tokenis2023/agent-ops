//! fs_file：文件管理（copy/move/remove/mkdir/convert）。
//!
//! 目的：让 Agent 不必用 PowerShell 的 Copy-Item/Move-Item/Remove-Item
//! 去处理路径、覆盖与编码问题。

use crate::paths::{display_native, FsPath};
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn run(args: &Value) -> String {
    match crate::tools::arg_str(args, "op").unwrap_or("") {
        "copy" => transfer(args, false),
        "move" => transfer(args, true),
        "remove" => remove(args),
        "mkdir" => mkdir(args),
        "convert" => convert(args),
        "archive" => archive(args),
        "extract" => extract(args),
        "" => "E_BAD_ARGS fs_file 需要 op：copy/move/remove/mkdir/convert/archive/extract".into(),
        other => {
            format!(
                "E_BAD_ARGS fs_file.op 未知：{other}；可用 copy/move/remove/mkdir/convert/archive/extract"
            )
        }
    }
}

fn path_arg(args: &Value, key: &str) -> Result<FsPath, String> {
    let Some(raw) = crate::tools::arg_str(args, key) else {
        return Err(format!("E_BAD_ARGS fs_file 需要 {key}"));
    };
    FsPath::parse(raw).map_err(|e| format!("E_BAD_PATH {key}: {e}"))
}

const MAX_ARCHIVE_ENTRIES: usize = 10_000;
const MAX_EXTRACT_BYTES: u64 = 512 * 1024 * 1024;

fn archive(args: &Value) -> String {
    let src = match path_arg(args, "src") {
        Ok(path) => path,
        Err(e) => return e,
    };
    let dst = match path_arg(args, "dst") {
        Ok(path) => path,
        Err(e) => return e,
    };
    if !src.native().exists() {
        return format!("E_NOT_FOUND src={} 不存在", src.display());
    }
    if dst.native().exists() && !crate::tools::arg_bool(args, "overwrite", false) {
        return format!(
            "E_EXISTS dst={} 已存在；覆盖请传 overwrite=true",
            dst.display()
        );
    }
    let files = match collect_files(src.native()) {
        Ok(files) => files,
        Err(e) => return e,
    };
    if files.is_empty() {
        return format!("E_BAD_ARGS src={} 没有可打包的文件", src.display());
    }
    if crate::tools::arg_bool(args, "dry_run", false) {
        return format!(
            "ok dry_run: 将打包 {} 个文件 -> {}",
            files.len(),
            dst.display()
        );
    }
    if let Some(parent) = dst.native().parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return format!("E_IO 创建目录 {} 失败: {e}", display_native(parent));
        }
    }
    let file = match std::fs::File::create(dst.native()) {
        Ok(file) => file,
        Err(e) => return format!("E_IO 创建压缩包 {} 失败: {e}", dst.display()),
    };
    let mut writer = zip::ZipWriter::new(file);
    let base = if src.native().is_dir() {
        src.native()
    } else {
        src.native().parent().unwrap_or(Path::new("."))
    };
    let mut total = 0u64;
    for path in &files {
        let rel = path.strip_prefix(base).unwrap_or(path);
        let name = rel.to_string_lossy().replace('\\', "/");
        if name.is_empty() {
            continue;
        }
        let options =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        if let Err(e) = writer.start_file(name, options) {
            return format!("E_ZIP 写入条目失败: {e}");
        }
        match fs::read(path) {
            Ok(bytes) => {
                total += bytes.len() as u64;
                if let Err(e) = writer.write_all(&bytes) {
                    return format!("E_ZIP 写入 {} 失败: {e}", path.display());
                }
            }
            Err(e) => return format!("E_IO 读取 {} 失败: {e}", path.display()),
        }
    }
    if let Err(e) = writer.finish() {
        return format!("E_ZIP 收尾失败: {e}");
    }
    let size = fs::metadata(dst.native())
        .map(|meta| meta.len())
        .unwrap_or(0);
    format!(
        "ok archive {} -> {}（{} 个文件，{}B -> {}B）",
        src.display(),
        dst.display(),
        files.len(),
        total,
        size
    )
}

fn extract(args: &Value) -> String {
    let src = match path_arg(args, "src") {
        Ok(path) => path,
        Err(e) => return e,
    };
    let dst = match path_arg(args, "dst") {
        Ok(path) => path,
        Err(e) => return e,
    };
    if !src.native().is_file() {
        return format!("E_NOT_FOUND src={} 不是文件", src.display());
    }
    let file = match std::fs::File::open(src.native()) {
        Ok(file) => file,
        Err(e) => return format!("E_IO 打开 {} 失败: {e}", src.display()),
    };
    let mut reader = match zip::ZipArchive::new(file) {
        Ok(reader) => reader,
        Err(e) => return format!("E_ZIP 读取压缩包失败: {e}"),
    };
    if reader.len() > MAX_ARCHIVE_ENTRIES {
        return format!("E_ZIP 条目过多：{} > {MAX_ARCHIVE_ENTRIES}", reader.len());
    }
    if crate::tools::arg_bool(args, "dry_run", false) {
        return format!(
            "ok dry_run: 将解压 {} 个条目 -> {}",
            reader.len(),
            dst.display()
        );
    }
    if let Err(e) = fs::create_dir_all(dst.native()) {
        return format!("E_IO 创建目录 {} 失败: {e}", dst.display());
    }
    let overwrite = crate::tools::arg_bool(args, "overwrite", false);
    let mut files = 0usize;
    let mut total = 0u64;
    for index in 0..reader.len() {
        let mut entry = match reader.by_index(index) {
            Ok(entry) => entry,
            Err(e) => return format!("E_ZIP 读取条目失败: {e}"),
        };
        let Some(rel) = entry.enclosed_name() else {
            return format!("E_ZIP 非法路径（zip slip）：{}", entry.name());
        };
        let out_path = dst.native().join(rel);
        if entry.is_dir() {
            let _ = fs::create_dir_all(&out_path);
            continue;
        }
        if let Some(parent) = out_path.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                return format!("E_IO 创建目录 {} 失败: {e}", display_native(parent));
            }
        }
        if out_path.exists() && !overwrite {
            return format!(
                "E_EXISTS {} 已存在；覆盖请传 overwrite=true",
                display_native(&out_path)
            );
        }
        let mut output = match std::fs::File::create(&out_path) {
            Ok(output) => output,
            Err(e) => return format!("E_IO 创建 {} 失败: {e}", display_native(&out_path)),
        };
        match std::io::copy(&mut entry, &mut output) {
            Ok(bytes) => {
                total += bytes;
                files += 1;
            }
            Err(e) => return format!("E_ZIP 解压 {} 失败: {e}", display_native(&out_path)),
        }
        if total > MAX_EXTRACT_BYTES {
            return format!("E_ZIP 解压体积超过上限 {MAX_EXTRACT_BYTES}B");
        }
    }
    format!(
        "ok extract {} -> {}（{files} 个文件，{total}B）",
        src.display(),
        dst.display()
    )
}

fn collect_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    if root.is_file() {
        return Ok(vec![root.to_path_buf()]);
    }
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
        let entries = fs::read_dir(dir)
            .map_err(|e| format!("E_IO 读取目录 {} 失败: {e}", display_native(dir)))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("E_IO 读取目录项失败: {e}"))?;
            let path = entry.path();
            let meta = match fs::symlink_metadata(&path) {
                Ok(meta) => meta,
                Err(_) => continue,
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                walk(&path, out)?;
            } else {
                out.push(path);
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(root, &mut out)?;
    Ok(out)
}

fn transfer(args: &Value, is_move: bool) -> String {
    let src = match path_arg(args, "src") {
        Ok(path) => path,
        Err(e) => return e,
    };
    let dst = match path_arg(args, "dst") {
        Ok(path) => path,
        Err(e) => return e,
    };
    let overwrite = crate::tools::arg_bool(args, "overwrite", false);
    let dry = crate::tools::arg_bool(args, "dry_run", false);
    let from = src.native();
    let to = dst.native();
    if !from.exists() {
        return format!("E_NOT_FOUND src={} 不存在", src.display());
    }
    let target: PathBuf = if to.is_dir() {
        to.join(from.file_name().unwrap_or_default())
    } else {
        to.to_path_buf()
    };
    if target.exists() && !overwrite {
        return format!(
            "E_EXISTS dst={} 已存在；覆盖请传 overwrite=true",
            display_native(&target)
        );
    }
    let verb = if is_move { "move" } else { "copy" };
    if dry {
        return format!(
            "ok dry_run: {verb} {} -> {}",
            src.display(),
            display_native(&target)
        );
    }
    let result = if is_move {
        match fs::rename(from, &target) {
            Ok(()) => Ok(stats_of(&target)),
            Err(_) => match copy_tree(from, &target, overwrite) {
                Ok(stats) => if from.is_dir() {
                    fs::remove_dir_all(from)
                } else {
                    fs::remove_file(from)
                }
                .map(|()| stats)
                .map_err(|e| format!("E_IO 移动后删除源失败: {e}")),
                Err(e) => Err(e),
            },
        }
    } else {
        copy_tree(from, &target, overwrite)
    };
    match result {
        Ok((bytes, files)) => format!(
            "ok {verb} {} -> {}（{files} 个文件，{bytes}B）",
            src.display(),
            display_native(&target)
        ),
        Err(e) => e,
    }
}

/// 同盘 rename 不产生拷贝，但输出仍要报实际大小与文件数。
/// 目录按递归统计，跳过符号链接，口径与 copy_tree/collect_files 一致。
fn stats_of(path: &Path) -> (u64, usize) {
    if path.is_dir() {
        let files = collect_files(path).unwrap_or_default();
        let bytes = files
            .iter()
            .filter_map(|file| fs::metadata(file).ok())
            .map(|meta| meta.len())
            .sum();
        (bytes, files.len())
    } else {
        (fs::metadata(path).map(|meta| meta.len()).unwrap_or(0), 1)
    }
}

fn copy_tree(src: &Path, dst: &Path, overwrite: bool) -> Result<(u64, usize), String> {
    if src.is_dir() {
        fs::create_dir_all(dst)
            .map_err(|e| format!("E_IO 创建目录 {} 失败: {e}", display_native(dst)))?;
        let mut bytes = 0u64;
        let mut files = 0usize;
        for entry in fs::read_dir(src)
            .map_err(|e| format!("E_IO 读取目录 {} 失败: {e}", display_native(src)))?
        {
            let entry = entry.map_err(|e| format!("E_IO 读取目录项失败: {e}"))?;
            let (child_bytes, child_files) =
                copy_tree(&entry.path(), &dst.join(entry.file_name()), overwrite)?;
            bytes += child_bytes;
            files += child_files;
        }
        Ok((bytes, files))
    } else {
        if dst.exists() && !overwrite {
            return Err(format!("E_EXISTS {} 已存在", display_native(dst)));
        }
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("E_IO 创建目录 {} 失败: {e}", display_native(parent)))?;
        }
        fs::copy(src, dst).map_err(|e| {
            format!(
                "E_IO 复制 {} -> {} 失败: {e}",
                display_native(src),
                display_native(dst)
            )
        })?;
        let size = fs::metadata(dst).map(|meta| meta.len()).unwrap_or(0);
        Ok((size, 1))
    }
}

fn remove(args: &Value) -> String {
    let path = match path_arg(args, "path") {
        Ok(path) => path,
        Err(e) => return e,
    };
    if !crate::tools::arg_bool(args, "confirm", false) {
        return "E_CONFIRM_REQUIRED fs_file op=remove 需要 confirm=true；删除不可恢复".into();
    }
    let native = path.native();
    if !native.exists() {
        return format!("ok 不存在（无需删除）{}", path.display());
    }
    if native.parent().is_none() {
        return "E_BAD_ARGS 拒绝删除盘符根目录".into();
    }
    let recursive = crate::tools::arg_bool(args, "recursive", false);
    if native.is_dir() && !recursive {
        return "E_BAD_ARGS 删除目录需要 recursive=true".into();
    }
    if crate::tools::arg_bool(args, "dry_run", false) {
        return format!("ok dry_run: 将删除 {}", path.display());
    }
    let result = if native.is_dir() {
        fs::remove_dir_all(native)
    } else {
        fs::remove_file(native)
    };
    match result {
        Ok(()) => format!("ok remove {}", path.display()),
        Err(e) => format!("E_IO 删除 {} 失败: {e}", path.display()),
    }
}

fn mkdir(args: &Value) -> String {
    let path = match path_arg(args, "path") {
        Ok(path) => path,
        Err(e) => return e,
    };
    let native = path.native();
    if native.is_file() {
        return format!("E_EXISTS {} 是文件", path.display());
    }
    if native.is_dir() {
        return format!("ok mkdir {}（已存在）", path.display());
    }
    match fs::create_dir_all(native) {
        Ok(()) => format!("ok mkdir {}", path.display()),
        Err(e) => format!("E_IO 创建目录 {} 失败: {e}", path.display()),
    }
}

fn encoding_label(raw: &str) -> Option<&'static str> {
    match raw.to_ascii_lowercase().as_str() {
        "utf-8" | "utf8" => Some("utf-8"),
        "utf-8-bom" | "utf8-bom" => Some("utf-8-bom"),
        "gbk" => Some("gbk"),
        "gb18030" => Some("gb18030"),
        "utf-16le" | "utf-16" => Some("utf-16le"),
        "utf-16be" => Some("utf-16be"),
        _ => None,
    }
}

fn convert(args: &Value) -> String {
    let path = match path_arg(args, "path") {
        Ok(path) => path,
        Err(e) => return e,
    };
    let Some(raw_encoding) = crate::tools::arg_str(args, "encoding") else {
        return "E_BAD_ARGS fs_file op=convert 需要 encoding（utf-8/gbk/utf-16le/utf-16be/gb18030）"
            .into();
    };
    let Some(encoding) = encoding_label(raw_encoding) else {
        return format!(
            "E_BAD_ARGS encoding 未知：{raw_encoding}；可用 utf-8/utf-8-bom/gbk/gb18030/utf-16le/utf-16be"
        );
    };
    let eol_arg = crate::tools::arg_str(args, "eol").unwrap_or("");
    let eol = match eol_arg {
        "" => None,
        "lf" => Some(crate::text::Eol::Lf),
        "crlf" => Some(crate::text::Eol::CrLf),
        other => return format!("E_BAD_ARGS eol 未知：{other}；可用 lf/crlf"),
    };
    let bytes = match crate::io::read_bytes(&path.wide()) {
        Ok(bytes) => bytes,
        Err(e) => return e,
    };
    let doc = crate::text::decode(&bytes);
    let mut text = doc.text.clone();
    if let Some(target_eol) = eol {
        text = crate::text::from_lf(&crate::text::to_lf(&text), target_eol);
    }
    let final_eol = eol.unwrap_or(doc.eol);
    // utf-8-bom / utf-16 必须写 BOM：否则 Windows 编辑器会把 UTF-16 当二进制，
    // 而 utf-8-bom 这个目标编码本身就意味着「要 BOM」。
    let bom = matches!(encoding, "utf-8-bom" | "utf-16le" | "utf-16be");
    let new_doc = crate::text::Doc {
        text: text.clone(),
        encoding,
        eol: final_eol,
        bom,
        size: bytes.len() as u64,
    };
    let encoded = match crate::text::encode(&new_doc, &text) {
        Ok(encoded) => encoded,
        Err(e) => return e,
    };
    if crate::tools::arg_bool(args, "dry_run", false) {
        return format!(
            "ok dry_run: convert {} {} -> {encoding} {}（{}B）",
            path.display(),
            doc.encoding,
            final_eol.as_str(),
            encoded.len()
        );
    }
    match crate::io::atomic_write(&path, &encoded) {
        Ok(()) => format!(
            "ok convert {} {} -> {encoding} {}（{}B）",
            path.display(),
            doc.encoding,
            final_eol.as_str(),
            encoded.len()
        ),
        Err(e) => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn 目录(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fsops-file-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn copy文件并拒绝重复覆盖() {
        let dir = 目录("copy");
        let src = dir.join("a.txt");
        fs::write(&src, "hello").unwrap();
        let posix = |p: &Path| p.to_string_lossy().replace('\\', "/");
        let out = run(&json!({"op": "copy", "src": posix(&src), "dst": posix(&dir.join("b.txt"))}));
        assert!(out.starts_with("ok copy"), "{out}");
        assert_eq!(fs::read_to_string(dir.join("b.txt")).unwrap(), "hello");
        let again =
            run(&json!({"op": "copy", "src": posix(&src), "dst": posix(&dir.join("b.txt"))}));
        assert!(again.starts_with("E_EXISTS"), "{again}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy目录递归() {
        let dir = 目录("copydir");
        fs::create_dir_all(dir.join("src/sub")).unwrap();
        fs::write(dir.join("src/sub/a.txt"), "x").unwrap();
        let posix = |p: &Path| p.to_string_lossy().replace('\\', "/");
        let out = run(
            &json!({"op": "copy", "src": posix(&dir.join("src")), "dst": posix(&dir.join("dst"))}),
        );
        assert!(out.starts_with("ok copy"), "{out}");
        assert!(dir.join("dst/sub/a.txt").exists(), "{out}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn move文件() {
        let dir = 目录("move");
        let src = dir.join("a.txt");
        fs::write(&src, "x").unwrap();
        let posix = |p: &Path| p.to_string_lossy().replace('\\', "/");
        let out = run(&json!({"op": "move", "src": posix(&src), "dst": posix(&dir.join("b.txt"))}));
        assert!(out.starts_with("ok move"), "{out}");
        assert!(!src.exists() && dir.join("b.txt").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn remove必须confirm() {
        let dir = 目录("remove");
        let file = dir.join("a.txt");
        fs::write(&file, "x").unwrap();
        let posix = |p: &Path| p.to_string_lossy().replace('\\', "/");
        let out = run(&json!({"op": "remove", "path": posix(&file)}));
        assert!(out.starts_with("E_CONFIRM_REQUIRED"), "{out}");
        assert!(file.exists());
        let out = run(&json!({"op": "remove", "path": posix(&file), "confirm": true}));
        assert!(out.starts_with("ok remove"), "{out}");
        assert!(!file.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mkdir递归创建() {
        let dir = 目录("mkdir");
        let target = dir.join("a/b/c");
        let posix = |p: &Path| p.to_string_lossy().replace('\\', "/");
        let out = run(&json!({"op": "mkdir", "path": posix(&target)}));
        assert!(out.starts_with("ok mkdir"), "{out}");
        assert!(target.is_dir());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn archive目录并解压回来() {
        let dir = 目录("archive");
        fs::create_dir_all(dir.join("src/sub")).unwrap();
        fs::write(dir.join("src/a.txt"), "A").unwrap();
        fs::write(dir.join("src/sub/b.txt"), "B").unwrap();
        let posix = |p: &Path| p.to_string_lossy().replace('\\', "/");
        let zip_path = dir.join("out.zip");
        let out =
            run(&json!({"op": "archive", "src": posix(&dir.join("src")), "dst": posix(&zip_path)}));
        assert!(out.starts_with("ok archive"), "{out}");
        let outdir = dir.join("out");
        let out = run(&json!({"op": "extract", "src": posix(&zip_path), "dst": posix(&outdir)}));
        assert!(out.starts_with("ok extract"), "{out}");
        assert_eq!(fs::read_to_string(outdir.join("a.txt")).unwrap(), "A");
        assert_eq!(fs::read_to_string(outdir.join("sub/b.txt")).unwrap(), "B");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn archive拒绝覆盖已存在目标() {
        let dir = 目录("archive-overwrite");
        fs::write(dir.join("a.txt"), "A").unwrap();
        let zip_path = dir.join("out.zip");
        fs::write(&zip_path, "old").unwrap();
        let posix = |p: &Path| p.to_string_lossy().replace('\\', "/");
        let out = run(
            &json!({"op": "archive", "src": posix(&dir.join("a.txt")), "dst": posix(&zip_path)}),
        );
        assert!(out.starts_with("E_EXISTS"), "{out}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract拒绝路径穿越() {
        let dir = 目录("zipslip");
        let zip_path = dir.join("evil.zip");
        {
            let file = fs::File::create(&zip_path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            writer
                .start_file("../evil.txt", zip::write::FileOptions::default())
                .unwrap();
            writer.write_all(b"x").unwrap();
            writer.finish().unwrap();
        }
        let outdir = dir.join("out");
        let posix = |p: &Path| p.to_string_lossy().replace('\\', "/");
        let out = run(&json!({"op": "extract", "src": posix(&zip_path), "dst": posix(&outdir)}));
        assert!(out.starts_with("E_ZIP"), "{out}");
        assert!(!dir.join("evil.txt").exists(), "不得写到目标目录之外");
        let _ = fs::remove_dir_all(&dir);
    }
    #[test]
    fn copy输出统一_posix_路径() {
        let dir = 目录("copy-posix");
        let src = dir.join("a.txt");
        fs::write(&src, "hello").unwrap();
        let posix = |p: &Path| p.to_string_lossy().replace('\\', "/");
        let out = run(&json!({"op": "copy", "src": posix(&src), "dst": posix(&dir.join("b.txt"))}));
        assert!(out.starts_with("ok copy"), "{out}");
        assert!(
            !out.contains('\\'),
            "输出应统一 POSIX，不得出现反斜杠：{out}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn move报告实际字节数() {
        let dir = 目录("move-bytes");
        let src = dir.join("a.txt");
        fs::write(&src, "hello").unwrap();
        let posix = |p: &Path| p.to_string_lossy().replace('\\', "/");
        let out = run(&json!({"op": "move", "src": posix(&src), "dst": posix(&dir.join("b.txt"))}));
        assert!(out.starts_with("ok move"), "{out}");
        assert!(out.contains("1 个文件，5B"), "move 应报实际大小：{out}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn move目录报告文件数与字节() {
        let dir = 目录("move-dir");
        fs::create_dir_all(dir.join("src/sub")).unwrap();
        fs::write(dir.join("src/a.txt"), "A").unwrap();
        fs::write(dir.join("src/sub/b.txt"), "BB").unwrap();
        let posix = |p: &Path| p.to_string_lossy().replace('\\', "/");
        let out = run(
            &json!({"op": "move", "src": posix(&dir.join("src")), "dst": posix(&dir.join("dst"))}),
        );
        assert!(out.starts_with("ok move"), "{out}");
        assert!(
            out.contains("2 个文件，3B"),
            "目录 move 应统计文件数与字节：{out}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract输出统一_posix_路径() {
        let dir = 目录("extract-posix");
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/a.txt"), "A").unwrap();
        let posix = |p: &Path| p.to_string_lossy().replace('\\', "/");
        let zip_path = dir.join("out.zip");
        let archive =
            run(&json!({"op": "archive", "src": posix(&dir.join("src")), "dst": posix(&zip_path)}));
        assert!(archive.starts_with("ok archive"), "{archive}");
        let outdir = dir.join("out");
        let out = run(&json!({"op": "extract", "src": posix(&zip_path), "dst": posix(&outdir)}));
        assert!(out.starts_with("ok extract"), "{out}");
        assert!(
            !out.contains('\\'),
            "输出应统一 POSIX，不得出现反斜杠：{out}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn convert到utf8_bom会写bom() {
        let dir = 目录("convert-bom");
        let file = dir.join("a.txt");
        fs::write(&file, "hi\n").unwrap();
        let posix = |p: &Path| p.to_string_lossy().replace('\\', "/");
        let out = run(&json!({"op": "convert", "path": posix(&file), "encoding": "utf-8-bom"}));
        assert!(out.starts_with("ok convert"), "{out}");
        let bytes = fs::read(&file).unwrap();
        assert_eq!(&bytes[..3], &[0xEF, 0xBB, 0xBF], "应写入 UTF-8 BOM");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn convert_gbk转utf8() {
        let dir = 目录("convert");
        let file = dir.join("a.txt");
        // "中文\r\n" 的 GBK 字节
        fs::write(&file, [0xd6, 0xd0, 0xce, 0xc4, 0x0d, 0x0a]).unwrap();
        let posix = |p: &Path| p.to_string_lossy().replace('\\', "/");
        let out =
            run(&json!({"op": "convert", "path": posix(&file), "encoding": "utf-8", "eol": "lf"}));
        assert!(out.starts_with("ok convert"), "{out}");
        assert_eq!(fs::read(&file).unwrap(), "中文\n".as_bytes());
        let _ = fs::remove_dir_all(&dir);
    }
}
