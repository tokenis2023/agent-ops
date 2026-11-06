//! MCP 会话内的读取缓存：同文件同 hash 同区间再读时只回一行摘要。
//!
//! 只在长驻的 MCP 服务进程里有意义；`fsops call` 一次性进程天然无缓存。
//! 命中条件严格到"每个请求区间都被完整返回过"，宁可多读一次也不给错摘要。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

const MAX_FILES: usize = 256;

struct Entry {
    hash: String,
    max_line: usize,
    ranges: Vec<(usize, usize)>,
    last: Instant,
}

fn cache() -> &'static Mutex<HashMap<String, Entry>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Entry>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// `FSOPS_MCP_CACHE=0` 可整体关闭。
pub fn enabled() -> bool {
    !matches!(std::env::var("FSOPS_MCP_CACHE").as_deref(), Ok("0"))
}

/// 已读区间能覆盖全部请求区间时返回覆盖范围。
pub fn lookup(
    path: &str,
    hash: &str,
    max_line: usize,
    ranges: &[(usize, usize)],
) -> Option<Vec<(usize, usize)>> {
    if !enabled() {
        return None;
    }
    let map = cache().lock().ok()?;
    let entry = map.get(path)?;
    if entry.hash != hash || entry.max_line != max_line {
        return None;
    }
    let covered = ranges
        .iter()
        .all(|&(from, to)| entry.ranges.iter().any(|&(cf, ct)| cf <= from && to <= ct));
    covered.then(|| entry.ranges.clone())
}

pub fn record(path: &str, hash: &str, max_line: usize, ranges: &[(usize, usize)]) {
    if !enabled() || ranges.is_empty() {
        return;
    }
    let Ok(mut map) = cache().lock() else {
        return;
    };
    if map.len() >= MAX_FILES && !map.contains_key(path) {
        if let Some(oldest) = map
            .iter()
            .min_by_key(|(_, entry)| entry.last)
            .map(|(key, _)| key.clone())
        {
            map.remove(&oldest);
        }
    }
    let entry = map.entry(path.to_string()).or_insert_with(|| Entry {
        hash: hash.to_string(),
        max_line,
        ranges: Vec::new(),
        last: Instant::now(),
    });
    if entry.hash != hash || entry.max_line != max_line {
        entry.hash = hash.to_string();
        entry.max_line = max_line;
        entry.ranges.clear();
    }
    entry.ranges.extend_from_slice(ranges);
    entry.ranges = merge(entry.ranges.clone());
    entry.last = Instant::now();
}

pub fn invalidate(path: &str) {
    if let Ok(mut map) = cache().lock() {
        map.remove(path);
    }
}

/// 未变更摘要：带 hash/区间/总行数/首行锚点，避免上下文被压缩后模型误以为还记得。
pub fn stub(
    path: &str,
    hash: &str,
    ranges: &[(usize, usize)],
    total_lines: usize,
    first_line: &str,
) -> String {
    let short = &hash[..hash.len().min(12)];
    let mut out = format!(
        "{path} 未变更 hash={short}（已读 {}，共 {total_lines} 行）\n",
        fmt_ranges(ranges)
    );
    let first = first_line.trim_end();
    if !first.is_empty() {
        let shown: String = first.chars().take(120).collect();
        let more = if first.chars().count() > 120 {
            "…"
        } else {
            ""
        };
        out.push_str(&format!("首行: {shown}{more}\n"));
    }
    out.push_str("… 用 force=true 可重读\n");
    out
}

fn fmt_ranges(ranges: &[(usize, usize)]) -> String {
    ranges
        .iter()
        .map(|(from, to)| {
            if from == to {
                format!("{from}")
            } else {
                format!("{from}-{to}")
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn merge(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    ranges.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::new();
    for (from, to) in ranges {
        if let Some(last) = out.last_mut() {
            if from <= last.1.saturating_add(1) {
                last.1 = last.1.max(to);
                continue;
            }
        }
        out.push((from, to));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 记录后命中且合并相邻区间() {
        let p = "test://cache-merge";
        record(p, "h1", 240, &[(1, 10)]);
        record(p, "h1", 240, &[(11, 20)]);
        let got = lookup(p, "h1", 240, &[(5, 15)]).expect("应命中");
        assert_eq!(got, vec![(1, 20)]);
        invalidate(p);
    }

    #[test]
    fn 区间未覆盖或hash或maxline不同不命中() {
        let p = "test://cache-miss";
        record(p, "h1", 240, &[(1, 10)]);
        assert!(lookup(p, "h1", 240, &[(5, 15)]).is_none());
        assert!(lookup(p, "h2", 240, &[(1, 5)]).is_none());
        assert!(lookup(p, "h1", 0, &[(1, 5)]).is_none());
        invalidate(p);
    }

    #[test]
    fn 失效后不命中() {
        let p = "test://cache-invalidate";
        record(p, "h1", 240, &[(1, 10)]);
        assert!(lookup(p, "h1", 240, &[(1, 2)]).is_some());
        invalidate(p);
        assert!(lookup(p, "h1", 240, &[(1, 2)]).is_none());
    }
}
