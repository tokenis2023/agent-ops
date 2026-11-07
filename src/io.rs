//! IO 层：带重试的读取、原子写入、内容指纹。
//!
//! Windows 上两个坑必须处理：
//! 1. 编辑器/杀软短暂独占文件 → 读写失败要退避重试，而不是直接报错给模型。
//! 2. 直接覆盖写会在写入中途崩溃时留下半截文件 → 走 临时文件 + fsync + 改名。

use crate::paths::FsPath;
use crate::text::{self, Doc};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const RETRY_DELAYS_MS: [u64; 4] = [15, 40, 120, 300];

#[cfg(test)]
thread_local! {
    /// 仅测试编译启用的线程本地读取计数器。
    static TEST_READ_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// 大文件流式读取实际消费的字节数，用于验证 window 模式没有全扫。
    static TEST_LARGE_BYTES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub fn reset_test_read_count() {
    TEST_READ_COUNT.with(|count| count.set(0));
}

#[cfg(test)]
pub fn test_read_count() -> usize {
    TEST_READ_COUNT.with(std::cell::Cell::get)
}

#[cfg(test)]
pub fn reset_test_large_bytes() {
    TEST_LARGE_BYTES.with(|count| count.set(0));
}

#[cfg(test)]
pub fn test_large_bytes() -> u64 {
    TEST_LARGE_BYTES.with(std::cell::Cell::get)
}

/// 跨进程写锁：Windows 用命名互斥量，进程崩溃时由内核自动释放。
#[cfg(windows)]
#[derive(Debug)]
pub struct PathLock {
    handle: usize,
}

#[cfg(windows)]
pub fn lock_path(fp: &FsPath, timeout_ms: u32) -> Result<PathLock, String> {
    use std::ffi::c_void;

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateMutexW(
            attributes: *const c_void,
            initial_owner: i32,
            name: *const u16,
        ) -> *mut c_void;
        fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    const WAIT_OBJECT_0: u32 = 0;
    const WAIT_ABANDONED: u32 = 0x80;
    const WAIT_TIMEOUT: u32 = 0x102;

    let key = fp.native().to_string_lossy().to_lowercase();
    let digest = blake3::hash(key.as_bytes()).to_hex();
    let name: Vec<u16> = format!("Local\\fsops-write-{digest}")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if handle.is_null() {
        return Err(format!(
            "E_LOCK 无法创建文件写锁 {}: {}",
            fp.display(),
            std::io::Error::last_os_error()
        ));
    }
    let wait = unsafe { WaitForSingleObject(handle, timeout_ms) };
    if wait == WAIT_OBJECT_0 || wait == WAIT_ABANDONED {
        return Ok(PathLock {
            handle: handle as usize,
        });
    }
    unsafe {
        CloseHandle(handle);
    }
    if wait == WAIT_TIMEOUT {
        Err(locked_error(&fp.display().to_string(), timeout_ms))
    } else {
        Err(format!("E_LOCK 获取文件写锁失败（wait={wait}）"))
    }
}

#[cfg(windows)]
impl Drop for PathLock {
    fn drop(&mut self) {
        use std::ffi::c_void;
        #[link(name = "kernel32")]
        extern "system" {
            fn ReleaseMutex(handle: *mut c_void) -> i32;
            fn CloseHandle(handle: *mut c_void) -> i32;
        }
        let handle = self.handle as *mut c_void;
        unsafe {
            ReleaseMutex(handle);
            CloseHandle(handle);
        }
    }
}

/// 非 Windows 回退：临时目录锁文件；项目主要面向 Windows。
#[cfg(not(windows))]
#[derive(Debug)]
pub struct PathLock {
    path: std::path::PathBuf,
}

#[cfg(not(windows))]
pub fn lock_path(fp: &FsPath, timeout_ms: u32) -> Result<PathLock, String> {
    let dir = std::env::temp_dir().join("fsops-locks");
    fs::create_dir_all(&dir)
        .map_err(|e| format!("E_LOCK 无法创建锁目录 {}: {e}", dir.display()))?;
    let key = fp.native().to_string_lossy().to_lowercase();
    let digest = blake3::hash(key.as_bytes()).to_hex();
    let path = dir.join(format!("{digest}.lock"));
    let deadline = Instant::now() + Duration::from_millis(timeout_ms as u64);
    loop {
        match OpenOptions::new().create_new(true).write(true).open(&path) {
            Ok(_) => return Ok(PathLock { path }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if Instant::now() >= deadline {
                    return Err(locked_error(&fp.display().to_string(), timeout_ms));
                }
                sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(format!("E_LOCK 获取文件写锁失败: {e}")),
        }
    }
}

#[cfg(not(windows))]
impl Drop for PathLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn retry<T, F>(mut f: F) -> Result<T, String>
where
    F: FnMut() -> std::io::Result<T>,
{
    let mut last: Option<std::io::Error> = None;
    for (i, delay) in RETRY_DELAYS_MS.iter().enumerate() {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) => {
                // 只对"被占用/共享冲突"类错误重试
                let transient = matches!(
                    e.kind(),
                    std::io::ErrorKind::PermissionDenied
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::Interrupted
                ) || e.raw_os_error() == Some(32) // ERROR_SHARING_VIOLATION
                    || e.raw_os_error() == Some(33);
                last = Some(e);
                if !transient || i + 1 == RETRY_DELAYS_MS.len() {
                    break;
                }
                sleep(Duration::from_millis(*delay));
            }
        }
    }
    Err(format!(
        "E_READ 读取失败: {}",
        last.map(|e| e.to_string()).unwrap_or_default()
    ))
}

/// 在错误信息里补上路径：多文件读取时只报系统错误无法定位是哪个文件。
/// 复用 FsPath::display()，保证与其它输出用同一种路径写法。
pub fn with_path(err: String, fp: &FsPath) -> String {
    match err.strip_prefix("E_READ 读取失败: ") {
        Some(rest) => format!("E_READ {} 读取失败: {}", fp.display(), with_hint(rest, fp)),
        None => err,
    }
}

/// 路径不存在时检查是否写错了格式（双重盘符、多余斜杠），
/// 或文件名拼错——同目录里最相似的几个条目直接列出来，省掉一次 fs_glob。
fn with_hint(reason: &str, fp: &FsPath) -> String {
    // 环境变量不会被展开：模型常写 $env:TEMP 或 %TEMP%，结果被当成相对路径拼到
    // cwd 下，报一个和真实原因无关的 OS 错误（os error 123）。这里点破它。
    // 必须排在双重盘符判断之前：`/d/x/$env:TEMP` 会被误判成多了一层盘符。
    if let Some(name) = env_placeholder(&fp.display()) {
        return format!(
            "{reason}；路径里的 {name} 不会被展开（原样当字符），请先 fs_exec 取到绝对路径再传，或直接用绝对路径"
        );
    }
    if let Some(fixed) = crate::paths::double_prefix_hint(&fp.display()) {
        return format!("{reason}；路径疑似多了一层盘符，应为 {fixed}");
    }
    let missing = is_missing_reason(reason);
    if !missing {
        return reason.to_string();
    }
    match similar_siblings(fp) {
        Some(names) => format!("{reason}；同目录下有：{}", names.join(", ")),
        // 没有相近候选时也要给下一步，否则模型只能盲猜或干等。
        // 用文件名片段拼一条可直接跑的 fs_glob，比「请确认路径」有用。
        None => {
            let name = fp
                .native()
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let pattern = if name.is_empty() {
                "*".to_string()
            } else {
                format!("*{}*", name)
            };
            format!("{reason}；用 fs_glob pattern=\"{pattern}\" 定位，或 fs_stat 确认路径")
        }
    }
}

/// 判断一个 OS 错误是不是「路径不存在」。
///
/// 旧实现只比「找不到指定的文件 / 找不到指定路径」：而 Windows 实际报的是
/// 「找不到指定**的**路径」，少一个「的」就让整个提示分支从未生效（os error 3 的目录不存在）。
fn is_missing_reason(reason: &str) -> bool {
    const MARKERS: [&str; 8] = [
        "os error 2",
        "os error 3",
        "找不到指定",
        "cannot find",
        "could not find",
        "not find the",
        "not found",
        "NotFound",
    ];
    MARKERS.iter().any(|marker| reason.contains(marker))
}

/// 路径里出现未展开的环境变量占位符（`$env:NAME` 或 `%NAME%`）时返回该片段。
fn env_placeholder(display: &str) -> Option<String> {
    if let Some(index) = display.find("$env:") {
        let name: String = display[index + 5..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() {
            return Some(format!("$env:{name}"));
        }
    }
    for (start, c) in display.char_indices() {
        if c != '%' {
            continue;
        }
        let rest = &display[start + 1..];
        let Some(end) = rest.find('%') else {
            continue;
        };
        let name = &rest[..end];
        let looks_like_var = !name.is_empty()
            && name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if looks_like_var {
            return Some(format!("%{name}%"));
        }
    }
    None
}

/// 同目录里名字最相似的 3 个条目；没有相近候选时返回 None。
fn similar_siblings(fp: &FsPath) -> Option<Vec<String>> {
    let path = fp.native();
    let dir = path.parent()?;
    let name = path.file_name()?.to_string_lossy().to_lowercase();
    let width = name.chars().count();
    let mut scored: Vec<(usize, String)> = Vec::new();
    for entry in std::fs::read_dir(dir).ok()?.flatten().take(2000) {
        let candidate = entry.file_name().to_string_lossy().to_string();
        let d = crate::text::levenshtein(&name, &candidate.to_lowercase());
        if d <= 2 || d * 4 <= width {
            scored.push((d, candidate));
        }
    }
    scored.sort_by_key(|(d, name)| (*d, name.clone()));
    let out: Vec<String> = scored.into_iter().take(3).map(|(_, n)| n).collect();
    (!out.is_empty()).then_some(out)
}

pub fn read_bytes(p: &Path) -> Result<Vec<u8>, String> {
    let wide = p.to_path_buf();
    // 计数器按“一次逻辑读取”计，放在 retry 外层：
    // 共享冲突重试不应被误当成多次读取。
    #[cfg(test)]
    TEST_READ_COUNT.with(|count| count.set(count.get() + 1));
    retry(|| {
        let mut f = File::open(&wide)?;
        let mut buf = Vec::new();
        f.read_to_end(&mut buf)?;
        Ok(buf)
    })
}

pub fn read_doc(fp: &FsPath) -> Result<Doc, String> {
    Ok(read_snapshot(fp)?.doc)
}

/// 单次读取后的不可变快照，避免同一次工具调用重复读取文件。
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub bytes: Vec<u8>,
    pub doc: Doc,
    pub hash: String,
}

pub fn read_snapshot(fp: &FsPath) -> Result<Snapshot, String> {
    if fp.is_dir() {
        return Err(format!("E_IS_DIR {} 是目录", fp.display()));
    }
    let bytes = read_bytes(&fp.wide()).map_err(|e| with_path(e, fp))?;
    snapshot_from_bytes(fp, bytes)
}

/// 从已读取的字节构造快照，供 stat 等调用方复用同一次 I/O 结果。
pub fn snapshot_from_bytes(fp: &FsPath, bytes: Vec<u8>) -> Result<Snapshot, String> {
    if text::is_binary(&bytes) {
        return Err(format!(
            "E_BINARY {} 是二进制文件（{}），fsops 只处理文本和 DOCX/PDF",
            fp.display(),
            human_size(bytes.len() as u64)
        ));
    }
    let doc = text::decode(&bytes);
    let hash = hash_hex(&bytes);
    Ok(Snapshot { bytes, doc, hash })
}

/// 大文件局部读取结果。
///
/// `scan=full` 保留旧契约：逐行全扫，给精确总行数与全文指纹。
/// `scan=window` 只读到请求区间末尾再探一行，因此 `total_lines`/`hash` 为 `None`，
/// 模型不能把局部信息当成全文件信息。
pub struct LargeWindow {
    pub encoding: &'static str,
    pub eol: crate::text::Eol,
    pub size: u64,
    pub total_lines: Option<usize>,
    pub hash: Option<String>,
    pub text: String,
    pub first_line: usize,
    pub lines: usize,
    /// 请求区间之后文件是否还有内容；window 模式靠多读一行判断。
    pub more: bool,
}

/// 流式读取大文件的一个连续行区间，不把整个文件载入内存。
///
/// `full_scan=false`（window）：只扫描到 `end`，随后多读一行判断 `more` 即停；
/// 换行风格与编码取首 64KB 样本，hash/总行数省略。
/// `full_scan=true`（full）：保持旧的全文件扫描契约。
pub fn read_large_window(
    fp: &FsPath,
    start: usize,
    end: usize,
    full_scan: bool,
) -> Result<LargeWindow, String> {
    let mut file = File::open(fp.wide()).map_err(|e| format!("E_READ 读取失败: {e}"))?;
    let size = file
        .metadata()
        .map_err(|e| format!("E_READ 读取失败: {e}"))?
        .len();
    let mut sample = vec![0u8; 64 * 1024];
    let sample_len = file
        .read(&mut sample)
        .map_err(|e| format!("E_READ 读取失败: {e}"))?;
    sample.truncate(sample_len);
    let sample_doc = text::decode(&sample);
    if sample_doc.encoding.starts_with("utf-16") {
        return Err(format!(
            "E_TOO_LARGE_UTF16 {} 是 {} 编码的超大文件；请先转换为 UTF-8 再读",
            fp.display(),
            sample_doc.encoding
        ));
    }
    if sample[..sample.len().min(8192)].contains(&0) {
        return Err(format!("E_BINARY {} 看起来是二进制文件", fp.display()));
    }
    // 样本是否被 64KB 截断：决定末尾孤立的 `\r` 可不可信（可能只是 CRLF 被切开）。
    let sample_truncated = (sample_len as u64) < size;
    file.seek(SeekFrom::Start(0))
        .map_err(|e| format!("E_READ 读取失败: {e}"))?;
    let mut reader = BufReader::new(file);
    let mut hasher = blake3::Hasher::new();
    let mut line = Vec::new();
    let mut number = 1usize;
    let mut selected = Vec::new();
    let mut crlf = 0usize;
    let mut lf = 0usize;
    let mut cr = 0usize;
    let mut newlines = 0usize;
    // 末尾字节决定最后一行是否已经由换行结束；流式读取顺手记下，不额外 seek。
    let mut last_byte: Option<u8> = None;
    // window 模式：读到 end 后多读一行即可判断 more，不再继续扫。
    let mut more = false;
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|e| format!("E_READ 读取失败: {e}"))?;
        if read == 0 {
            break;
        }
        #[cfg(test)]
        TEST_LARGE_BYTES.with(|count| count.set(count.get() + read as u64));
        if !full_scan && number > end {
            more = true;
            break;
        }
        last_byte = line.last().copied();
        if full_scan {
            hasher.update(&line);
            if line.last() == Some(&b'\n') {
                newlines += 1;
                if line.len() >= 2 && line[line.len() - 2] == b'\r' {
                    crlf += 1;
                } else {
                    lf += 1;
                }
            } else if line.last() == Some(&b'\r') {
                cr += 1;
            }
        }
        if number >= start && (end == usize::MAX || number <= end) {
            let body = line.strip_suffix(b"\n").unwrap_or(&line);
            let body = body.strip_suffix(b"\r").unwrap_or(body);
            let body = if number == 1 {
                body.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(body)
            } else {
                body
            };
            selected.push(decode_large_line(body, sample_doc.encoding));
        }
        number += 1;
    }
    let (eol, total_lines, hash) = if full_scan {
        let kinds = [crlf > 0, lf > 0, cr > 0]
            .into_iter()
            .filter(|present| *present)
            .count();
        let eol = if kinds > 1 {
            crate::text::Eol::Mixed
        } else if crlf > 0 {
            crate::text::Eol::CrLf
        } else if cr > 0 {
            crate::text::Eol::Cr
        } else {
            crate::text::Eol::Lf
        };
        let hash = hasher.finalize().to_hex().chars().take(16).collect();
        // 总行数 = 换行数 + （末尾没有换行时的最后一行）。旧实现恒为 newlines+1，
        // 末尾带换行时多报一行（"a\n" 报 2 行），空文件也报成 1 行。
        let total_lines = if size == 0 {
            0
        } else {
            newlines + usize::from(!last_byte_newline(last_byte))
        };
        more = total_lines > 0 && start <= total_lines && end < total_lines;
        (eol, Some(total_lines), Some(hash))
    } else {
        (sample_eol(&sample, sample_truncated), None, None)
    };
    Ok(LargeWindow {
        encoding: sample_doc.encoding,
        eol,
        size,
        total_lines,
        hash,
        text: selected.join("\n"),
        first_line: start,
        lines: selected.len(),
        more,
    })
}

/// 从首 64KB 样本判断换行风格。
///
/// 样本在 64KB 处可能把 CRLF 切开：末尾孤立的 `\r` 不可信，先丢弃再统计。
fn sample_eol(sample: &[u8], truncated: bool) -> crate::text::Eol {
    let mut end = sample.len();
    if truncated && end > 0 && sample[end - 1] == b'\r' {
        end -= 1;
    }
    let sample = &sample[..end];
    let mut crlf = 0usize;
    let mut lf = 0usize;
    let mut cr = 0usize;
    for (index, byte) in sample.iter().enumerate() {
        match byte {
            b'\n' => {
                if index > 0 && sample[index - 1] == b'\r' {
                    crlf += 1;
                } else {
                    lf += 1;
                }
            }
            b'\r' if sample.get(index + 1) != Some(&b'\n') => {
                cr += 1;
            }
            _ => {}
        }
    }
    let kinds = [crlf > 0, lf > 0, cr > 0]
        .into_iter()
        .filter(|present| *present)
        .count();
    if kinds > 1 {
        crate::text::Eol::Mixed
    } else if crlf > 0 {
        crate::text::Eol::CrLf
    } else if cr > 0 {
        crate::text::Eol::Cr
    } else {
        crate::text::Eol::Lf
    }
}

/// 文件末尾字节是否为换行。
///
/// 空文件没有末尾换行，调用方把行数按 0 处理；`"a\n"` 是 1 行，`"a"` 也是 1 行。
fn last_byte_newline(last: Option<u8>) -> bool {
    last == Some(b'\n')
}

fn decode_large_line(bytes: &[u8], encoding: &str) -> String {
    match encoding {
        "gbk" => encoding_rs::GBK.decode(bytes).0.to_string(),
        "gb18030" => encoding_rs::GB18030.decode(bytes).0.to_string(),
        _ => String::from_utf8_lossy(bytes).to_string(),
    }
}

/// 解析符号链接/junction 的真实目标；不是链接则原样返回。
///
/// 不这样做的话，原子写入的 rename 会把链接本身替换成普通文件：
/// 目标文件没被修改，链接却被静默破坏（Windows 上 node_modules、junction 常见）。
fn resolve_link(path: &std::path::Path) -> std::path::PathBuf {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
        }
        _ => path.to_path_buf(),
    }
}

/// 目标是否带只读属性（Windows 上 rename 覆盖会因此失败）。
fn is_readonly(path: &std::path::Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.permissions().readonly())
        .unwrap_or(false)
}

/// 原子写入：同目录临时文件 → 落盘 → 改名覆盖。
pub fn atomic_write(dp: &FsPath, bytes: &[u8]) -> Result<(), String> {
    // 目标是符号链接时写到真实文件，避免把链接本身换成普通文件。
    let resolved = resolve_link(dp.native());
    let native = resolved.as_path();
    // 目录不能被当文件写入：不加这个检查会在 rename 阶段报
    // `E_RENAME 拒绝访问`，模型看不出真正原因。
    if native.is_dir() {
        return Err(format!(
            "E_IS_DIR {} 是目录，不能用 fs_write 写入",
            dp.display()
        ));
    }
    if let Some(parent) = native.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("E_MKDIR 无法创建目录 {}: {e}", parent.display()))?;
        }
    }
    let tmp = native.with_file_name(format!(
        ".{}.fsops-tmp-{}-{}",
        native
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "f".into()),
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    let result = (|| -> Result<(), String> {
        {
            let mut f = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&tmp)
                .map_err(|e| format!("E_OPEN 无法创建临时文件: {e}"))?;
            f.write_all(bytes)
                .map_err(|e| format!("E_WRITE 写入失败: {e}"))?;
            f.sync_all().map_err(|e| format!("E_SYNC 落盘失败: {e}"))?;
        }
        // Windows 的 rename 覆盖已存在文件是原子替换
        for (i, delay) in RETRY_DELAYS_MS.iter().enumerate() {
            match fs::rename(&tmp, native) {
                Ok(()) => return Ok(()),
                Err(e) if i + 1 < RETRY_DELAYS_MS.len() => {
                    let _ = e;
                    sleep(Duration::from_millis(*delay));
                }
                Err(e) => {
                    let _ = fs::remove_file(&tmp);
                    // 只读属性是 Windows 上最常见的写失败原因，直说并给解法。
                    if is_readonly(native) {
                        return Err(format!(
                            "E_READONLY {} 是只读文件；先去掉只读属性（PowerShell: \
                             Set-ItemProperty -Path '{}' -Name IsReadOnly -Value $false）再写入",
                            dp.display(),
                            native.display()
                        ));
                    }
                    return Err(format!("E_RENAME 替换目标失败: {e}"));
                }
            }
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// 内容指纹：模型可用它做"读-改-写"的并发保护。
pub fn hash_hex(bytes: &[u8]) -> String {
    let h = blake3::hash(bytes);
    h.to_hex().chars().take(16).collect()
}

#[derive(Debug, Clone)]
pub struct Meta {
    pub kind: &'static str,
    pub size: u64,
    pub mtime: SystemTime,
    pub readonly: bool,
    /// 符号链接的真实目标及其大小：链接自身的 size 是 0，
    /// 只报 0B 会让模型以为目标文件是空的。
    pub link: Option<(String, u64)>,
}

pub fn meta(fp: &FsPath) -> Result<Meta, String> {
    let md = fs::symlink_metadata(fp.wide())
        .map_err(|e| format!("E_NOT_FOUND {}: {e}", fp.display()))?;
    let ft = md.file_type();
    let kind = if ft.is_symlink() {
        "link"
    } else if ft.is_dir() {
        "dir"
    } else if ft.is_file() {
        "file"
    } else {
        "other"
    };
    let link = if ft.is_symlink() {
        fs::canonicalize(fp.wide()).ok().and_then(|target| {
            fs::metadata(&target).ok().map(|m| {
                // canonicalize 会带 `\\?\` 长路径前缀，对模型是噪声。
                let shown = target
                    .to_string_lossy()
                    .replace('\\', "/")
                    .trim_start_matches("//?/")
                    .to_string();
                (shown, m.len())
            })
        })
    } else {
        None
    };
    Ok(Meta {
        kind,
        size: md.len(),
        mtime: md.modified().unwrap_or(UNIX_EPOCH),
        readonly: md.permissions().readonly(),
        link,
    })
}

/// 人类可读时间：`2026-11-05 14:32`
pub fn fmt_time(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // 不引 chrono：用 civil-from-days 算法换算本地近似时间（UTC+8）
    let secs = secs + 8 * 3600;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60
    )
}

/// Howard Hinnant 的 civil_from_days 算法
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 把文件大小压缩成 `1.2K` / `3.4M`
pub fn human_size(n: u64) -> String {
    const K: f64 = 1024.0;
    let f = n as f64;
    if f < K {
        format!("{n}B")
    } else if f < K * K {
        format!("{:.1}K", f / K)
    } else if f < K * K * K {
        format!("{:.1}M", f / K / K)
    } else {
        format!("{:.1}G", f / K / K / K)
    }
}

/// 锁超时时把「已经等过」和「参数不变直接重试」写清，避免模型整轮重发后还白等。
pub fn locked_error(path: &str, waited_ms: u32) -> String {
    format!(
        "E_LOCKED {path} 正被其他进程修改；已自动等待 {waited_ms}ms，本次未写入任何内容，参数不变直接重试即可"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 锁错误给出无需重读的重试指令() {
        let msg = locked_error("a.txt", 3000);
        assert!(msg.starts_with("E_LOCKED"), "{msg}");
        assert!(msg.contains("3000ms"), "应说明已等多久: {msg}");
        assert!(
            msg.contains("参数不变直接重试"),
            "应给可直接照做的下一步: {msg}"
        );
    }

    #[test]
    fn 文件不存在时提示同目录相似文件() {
        let dir = std::env::temp_dir().join(format!("fsops-hint-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.yaml"), "a").unwrap();
        std::fs::write(dir.join("other.txt"), "b").unwrap();
        let fp = FsPath::parse(&dir.join("confg.yaml").to_string_lossy()).unwrap();
        let msg = with_hint("系统找不到指定的文件。 (os error 2)", &fp);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(msg.contains("config.yaml"), "应给出候选文件名: {msg}");
        assert!(!msg.contains("other.txt"), "不相似的文件不应出现: {msg}");
    }

    #[test]
    fn 目录不存在时也给定位提示() {
        // 回归：旧判定比的是「找不到指定路径」，而 Windows 报「找不到指定**的**路径」，
        // 少一个「的」就导致 os error 3 这个分支从未生效。
        let fp = FsPath::parse("/d/no-such-dir-xyz/a.txt").unwrap();
        let msg = with_path(
            "E_READ 读取失败: 系统找不到指定的路径。 (os error 3)".to_string(),
            &fp,
        );
        assert!(
            msg.contains("fs_glob pattern="),
            "目录不存在也要给下一步: {msg}"
        );
    }

    #[test]
    fn 缺文件且无相近候选时仍给出下一步() {
        let dir = std::env::temp_dir().join(format!("fsops-hint-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("aaa.txt"), "a").unwrap();
        std::fs::write(dir.join("bbb.txt"), "b").unwrap();
        let fp = FsPath::parse(&dir.join("完全不同名字.md").to_string_lossy()).unwrap();
        let msg = with_hint("系统找不到指定的文件。 (os error 2)", &fp);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            msg.contains("fs_glob pattern="),
            "缺文件要给定位手段: {msg}"
        );
        assert!(
            msg.contains("完全不同名字.md"),
            "pattern 应带上文件名片段: {msg}"
        );
    }

    #[test]
    fn 路径含未展开环境变量时点破而不是报无关错误() {
        for (input, want) in [
            ("/d/x/$env:TEMP/a.txt", "$env:TEMP"),
            ("/d/x/%TEMP%/a.txt", "%TEMP%"),
        ] {
            let fp = FsPath::parse(input).unwrap();
            let msg = with_hint("文件名、目录名或卷标语法不正确。 (os error 123)", &fp);
            assert!(msg.contains(want), "应点名占位符 {want}: {msg}");
            assert!(msg.contains("不会被展开"), "应说明不会展开: {msg}");
        }
    }

    #[test]
    fn 普通文件名里的百分号不误判为环境变量() {
        let fp = FsPath::parse("/d/x/100%.txt").unwrap();
        assert!(env_placeholder(&fp.display()).is_none());
    }

    #[test]
    fn 时间格式化正常() {
        let t = UNIX_EPOCH + Duration::from_secs(1_767_225_600);
        assert_eq!(fmt_time(t), "2026-01-01 08:00");
    }

    #[test]
    fn 大小格式化正常() {
        assert_eq!(human_size(999), "999B");
        assert_eq!(human_size(2048), "2.0K");
    }

    #[test]
    fn 快照一次提供原始字节文档和指纹() {
        let path = std::env::temp_dir().join(format!(
            "fsops-snapshot-{}-{}.txt",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let bytes = "中文\r\nabc\r\n".as_bytes().to_vec();
        fs::write(&path, &bytes).unwrap();
        let fp = FsPath::parse(path.to_str().unwrap()).unwrap();

        let result = read_snapshot(&fp);
        fs::remove_file(path).unwrap();
        let snapshot = result.unwrap();
        assert_eq!(snapshot.bytes, bytes);
        assert_eq!(snapshot.doc.text, "中文\r\nabc\r\n");
        assert_eq!(snapshot.hash, hash_hex(&bytes));
    }

    #[test]
    fn 大文件窗口只保留请求区间() {
        let path = std::env::temp_dir().join(format!("fsops-window-{}.txt", std::process::id()));
        fs::write(&path, b"one\r\ntwo\r\nthree\r\n").unwrap();
        let fp = FsPath::parse(path.to_str().unwrap()).unwrap();

        let window = read_large_window(&fp, 2, 3, true).unwrap();
        let _ = fs::remove_file(path);

        assert_eq!(window.text, "two\nthree");
        assert_eq!(window.total_lines, Some(3));
        assert_eq!(window.lines, 2);
        assert_eq!(window.first_line, 2);
        assert_eq!(window.eol, crate::text::Eol::CrLf);
        assert!(!window.more);
        assert!(window.hash.is_some());
    }

    /// 窗口模式：只给请求区间，hash/总行数必须省略，并探测区间后是否还有内容。
    #[test]
    fn 大文件窗口模式省略全量指纹与总行数() {
        let path = std::env::temp_dir().join(format!(
            "fsops-window-fast-{}-{}.txt",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, b"one\ntwo\nthree\n").unwrap();
        let fp = FsPath::parse(path.to_str().unwrap()).unwrap();

        let window = read_large_window(&fp, 2, 2, false).unwrap();
        let _ = fs::remove_file(path);

        assert_eq!(window.text, "two");
        assert_eq!(window.total_lines, None);
        assert_eq!(window.hash, None);
        assert_eq!(window.lines, 1);
        assert_eq!(window.first_line, 2);
        assert!(window.more, "第 3 行存在时 more 必须为 true");
        assert_eq!(window.eol, crate::text::Eol::Lf);
        assert_eq!(window.size, 14);
    }

    /// 空文件、末尾换行、无末尾换行、请求到末行、请求超出末尾都要给出确定的 more。
    #[test]
    fn 大文件窗口模式区间末尾的more() {
        for (start, end, want_more, want_lines) in [
            (1usize, 3usize, false, 3usize),
            (1, 2, true, 2),
            (2, 2, true, 1),
            (3, 3, false, 1),
            (4, 5, false, 0),
            (1, 10, false, 3),
        ] {
            let path = std::env::temp_dir().join(format!(
                "fsops-window-more-{}-{}-{}.txt",
                std::process::id(),
                start,
                end
            ));
            fs::write(&path, b"a\nb\nc\n").unwrap();
            let fp = FsPath::parse(path.to_str().unwrap()).unwrap();
            let window = read_large_window(&fp, start, end, false).unwrap();
            let _ = fs::remove_file(&path);
            assert_eq!(window.lines, want_lines, "start={start} end={end}");
            assert_eq!(window.more, want_more, "start={start} end={end}");
        }

        // 无末尾换行：最后一行的 more 仍必须是 false。
        let path =
            std::env::temp_dir().join(format!("fsops-window-more-tail-{}.txt", std::process::id()));
        fs::write(&path, b"a\nb\nc").unwrap();
        let fp = FsPath::parse(path.to_str().unwrap()).unwrap();
        let window = read_large_window(&fp, 3, 3, false).unwrap();
        let _ = fs::remove_file(&path);
        assert_eq!(window.lines, 1);
        assert!(!window.more);
        assert_eq!(window.text, "c");
    }

    #[test]
    fn 大文件窗口模式空文件() {
        let path =
            std::env::temp_dir().join(format!("fsops-window-empty-{}.txt", std::process::id()));
        fs::write(&path, b"").unwrap();
        let fp = FsPath::parse(path.to_str().unwrap()).unwrap();

        let window = read_large_window(&fp, 1, 1, false).unwrap();
        let full = read_large_window(&fp, 1, 1, true).unwrap();
        let _ = fs::remove_file(path);

        assert_eq!(window.lines, 0);
        assert!(!window.more);
        assert_eq!(window.total_lines, None);
        assert_eq!(window.hash, None);
        assert_eq!(window.size, 0);
        assert_eq!(full.lines, 0);
        assert_eq!(full.total_lines, Some(0));
        assert!(full.hash.is_some());
    }

    /// 窗口不再扫全文件，换行风格只能来自首 64KB 样本。
    #[test]
    fn 大文件窗口模式换行风格取样本() {
        let path =
            std::env::temp_dir().join(format!("fsops-window-eol-{}.txt", std::process::id()));
        // 第 2 行没有换行：只按扫描到的行判 EOL 会退回 LF。
        fs::write(&path, b"one\r\ntwo").unwrap();
        let fp = FsPath::parse(path.to_str().unwrap()).unwrap();
        let window = read_large_window(&fp, 2, 2, false).unwrap();
        let _ = fs::remove_file(path);
        assert_eq!(window.text, "two");
        assert_eq!(window.eol, crate::text::Eol::CrLf);
    }

    /// 1480 行 × 1KB：窗口模式只读请求区间（外加一行 more 探测），不能全扫。
    #[test]
    fn 大文件窗口模式不扫全文件() {
        use std::io::Write;
        let path =
            std::env::temp_dir().join(format!("fsops-window-fast-io-{}.txt", std::process::id()));
        let line = format!("{}\n", "x".repeat(1023));
        let mut file = File::create(&path).unwrap();
        for _ in 0..1480 {
            file.write_all(line.as_bytes()).unwrap();
        }
        file.sync_all().unwrap();
        drop(file);
        let size = fs::metadata(&path).unwrap().len();
        let fp = FsPath::parse(path.to_str().unwrap()).unwrap();

        reset_test_large_bytes();
        let window = read_large_window(&fp, 1, 5, false).unwrap();
        let window_bytes = test_large_bytes();
        reset_test_large_bytes();
        let full = read_large_window(&fp, 1, 5, true).unwrap();
        let full_bytes = test_large_bytes();
        let _ = fs::remove_file(path);

        assert_eq!(window.lines, 5);
        assert!(window.more);
        assert!(
            window_bytes * 8 < size,
            "窗口模式读了 {window_bytes} 字节，文件共 {size} 字节"
        );
        assert_eq!(full_bytes, size, "full 模式必须全扫");
        assert_eq!(full.total_lines, Some(1480));
    }

    #[test]
    fn 大文件窗口总行数贴合末尾换行() {
        // 旧实现恒为 newlines+1：末尾有换行时多报一行，空文件报 1 行。
        for (body, want) in [
            ("", 0usize),
            ("a\n", 1),
            ("a", 1),
            ("a\nb", 2),
            ("a\nb\n", 2),
        ] {
            let path = std::env::temp_dir().join(format!(
                "fsops-window-lines-{}-{}.txt",
                std::process::id(),
                body.len()
            ));
            fs::write(&path, body).unwrap();
            let fp = FsPath::parse(path.to_str().unwrap()).unwrap();
            let window = read_large_window(&fp, 1, 1, true).unwrap();
            let _ = fs::remove_file(path);
            assert_eq!(window.total_lines, Some(want), "{body:?}");
        }
    }

    #[test]
    fn 路径锁阻止第二个写入者() {
        let path = std::env::temp_dir().join(format!("fsops-lock-{}.txt", std::process::id()));
        let fp = FsPath::parse(path.to_str().unwrap()).unwrap();
        let _ = fs::write(&path, "x");

        let held = lock_path(&fp, 100).expect("首个锁应成功");
        let other = fp.clone();
        let blocked = std::thread::spawn(move || lock_path(&other, 50))
            .join()
            .unwrap();
        assert!(
            blocked
                .as_ref()
                .err()
                .is_some_and(|error| error.starts_with("E_LOCKED")),
            "第二个写入者未被拒绝: {blocked:?}"
        );
        drop(held);
        assert!(lock_path(&fp, 100).is_ok(), "释放后应能重新加锁");
        let _ = fs::remove_file(path);
    }

    /// 共享冲突重试属于同一次逻辑读取，计数只能是 1。
    #[cfg(windows)]
    #[test]
    fn 读取重试只计一次逻辑读取() {
        use std::os::windows::fs::OpenOptionsExt;

        let path = std::env::temp_dir().join(format!("fsops-retry-{}.txt", std::process::id()));
        fs::write(&path, "x").unwrap();
        // share_mode(0) 拒绝后续读取，read_bytes 会走共享冲突重试。
        let held = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();

        reset_test_read_count();
        let err = read_bytes(&path).unwrap_err();
        let count = test_read_count();
        drop(held);
        let _ = fs::remove_file(path);

        assert!(err.starts_with("E_READ"), "应报读取失败: {err}");
        assert_eq!(count, 1, "重试被误计成 {count} 次逻辑读取");
    }
}
