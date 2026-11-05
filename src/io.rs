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
        Err(format!(
            "E_LOCKED {} 正被其他进程修改；请稍后重试",
            fp.display()
        ))
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
                    return Err(format!(
                        "E_LOCKED {} 正被其他进程修改；请稍后重试",
                        fp.display()
                    ));
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

pub fn read_bytes(p: &Path) -> Result<Vec<u8>, String> {
    let wide = p.to_path_buf();
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
    let bytes = read_bytes(&fp.wide())?;
    let doc = text::decode(&bytes);
    let hash = hash_hex(&bytes);
    Ok(Snapshot { bytes, doc, hash })
}

/// 大文件局部读取结果：只保留请求区间内的行，hash/总行数仍通过流式扫描得到。
pub struct LargeWindow {
    pub encoding: &'static str,
    pub eol: crate::text::Eol,
    pub size: u64,
    pub total_lines: usize,
    pub hash: String,
    pub text: String,
    pub first_line: usize,
    pub lines: usize,
}

/// 流式读取大文件的一个连续行区间，不把整个文件载入内存。
pub fn read_large_window(fp: &FsPath, start: usize, end: usize) -> Result<LargeWindow, String> {
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
    loop {
        line.clear();
        let read = reader
            .read_until(b'\n', &mut line)
            .map_err(|e| format!("E_READ 读取失败: {e}"))?;
        if read == 0 {
            break;
        }
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
    Ok(LargeWindow {
        encoding: sample_doc.encoding,
        eol,
        size,
        total_lines: newlines + 1,
        hash,
        text: selected.join("\n"),
        first_line: start,
        lines: selected.len(),
    })
}

fn decode_large_line(bytes: &[u8], encoding: &str) -> String {
    match encoding {
        "gbk" => encoding_rs::GBK.decode(bytes).0.to_string(),
        "gb18030" => encoding_rs::GB18030.decode(bytes).0.to_string(),
        _ => String::from_utf8_lossy(bytes).to_string(),
    }
}

/// 原子写入：同目录临时文件 → 落盘 → 改名覆盖。
pub fn atomic_write(dp: &FsPath, bytes: &[u8]) -> Result<(), String> {
    let native = dp.native();
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
    Ok(Meta {
        kind,
        size: md.len(),
        mtime: md.modified().unwrap_or(UNIX_EPOCH),
        readonly: md.permissions().readonly(),
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

#[cfg(test)]
mod tests {
    use super::*;

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

        let window = read_large_window(&fp, 2, 3).unwrap();
        let _ = fs::remove_file(path);

        assert_eq!(window.text, "two\nthree");
        assert_eq!(window.total_lines, 4);
        assert_eq!(window.lines, 2);
        assert_eq!(window.first_line, 2);
        assert_eq!(window.eol, crate::text::Eol::CrLf);
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
}
