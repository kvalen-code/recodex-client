use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Value, json};

static TEST_LOG_PATH: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

const MAX_DIAGNOSTIC_LOG_BYTES: u64 = 50 * 1024 * 1024;
const COMPACTED_DIAGNOSTIC_LOG_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
struct DiagnosticRecord {
    timestamp_ms: u64,
    pid: u32,
    event: String,
    detail: Value,
}

/// 重试循环里这一次该不该落日志:只留第 1、2、4、8… 次。
///
/// 线上实测(2026-09-05):CDP 不可达时 `ensure_injection` 会一路跑到 attempt 72
/// (上限 120)、菜单汉化跑到 18(上限 20),每一次都写一条诊断并上报 ——
/// **同一件事**能刷出上百条,真正的首因淹在里面,本地日志也跟着膨胀。
/// 按 2 的幂次采样既留住「第一次的原因」,也留住「最后跑到多远」这个量级信息。
pub fn should_log_retry_attempt(attempt: u32) -> bool {
    attempt.is_power_of_two()
}

pub fn append_diagnostic_log(event: &str, detail: impl Serialize) -> std::io::Result<()> {
    let path = diagnostic_log_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let detail = serde_json::to_value(detail).unwrap_or_else(|error| {
        json!({
            "serialization_error": error.to_string()
        })
    });
    let record = DiagnosticRecord {
        timestamp_ms: now_ms(),
        pid: std::process::id(),
        event: event.to_string(),
        detail,
    };
    let line = serde_json::to_string(&record).unwrap_or_else(|error| {
        json!({
            "timestamp_ms": now_ms(),
            "pid": std::process::id(),
            "event": "diagnostic_log.serialization_failed",
            "detail": {
                "message": error.to_string()
            }
        })
        .to_string()
    });

    compact_diagnostic_log_if_needed(&path)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

pub fn clear_diagnostic_log() -> std::io::Result<()> {
    let path = diagnostic_log_path();
    clear_diagnostic_log_path(&path)
}

fn clear_diagnostic_log_path(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub fn diagnostic_log_path() -> PathBuf {
    if let Some(lock) = TEST_LOG_PATH.get() {
        if let Ok(guard) = lock.lock() {
            if let Some(path) = &*guard {
                return path.clone();
            }
        }
    }
    crate::paths::default_diagnostic_log_path()
}

#[doc(hidden)]
pub fn set_diagnostic_log_path_for_tests(path: Option<PathBuf>) {
    let lock = TEST_LOG_PATH.get_or_init(|| Mutex::new(None));
    *lock.lock().expect("test log path lock poisoned") = path;
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn compact_diagnostic_log_if_needed(path: &PathBuf) -> std::io::Result<()> {
    compact_diagnostic_log(
        path,
        MAX_DIAGNOSTIC_LOG_BYTES,
        COMPACTED_DIAGNOSTIC_LOG_BYTES,
    )
}

fn compact_diagnostic_log(
    path: &PathBuf,
    max_bytes: u64,
    compacted_bytes: u64,
) -> std::io::Result<()> {
    let len = match std::fs::metadata(path) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if len <= max_bytes {
        return Ok(());
    }

    let keep = compacted_bytes.min(len);
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(len - keep))?;
    let mut tail = Vec::with_capacity(keep as usize);
    file.read_to_end(&mut tail)?;
    drop(file);
    if len > keep {
        if let Some(pos) = tail.iter().position(|byte| *byte == b'\n') {
            tail.drain(..=pos);
        }
    }

    crate::settings::atomic_write(path, &tail).map_err(std::io::Error::other)?;
    // ⚠️ 压缩会把**文件头部**砍掉,而上传水位记的是字节偏移(sidecar,由
    // recodex-integration 的 diagnostics_flush 维护)。那边只在
    // `watermark > 文件长度` 时重置 —— 可这里压缩后文件仍有 COMPACTED 那么大,
    // 旧水位往往还小于它,于是**不会**重置:下次从旧偏移继续读,而那个位置已经
    // 是压缩后的新内容,夹在中间的那段日志就被永久跳过了。
    //
    // 触发要日志真涨到 50MB,所以没在现场见过;真修得设计一个跨 crate 的契约
    // (压缩时连带清水位,或水位里存内容指纹),不适合顺手改。先把它变成**可见**的:
    // 这条事件带 error 字段,会传回服务端 —— 一旦真有设备压缩过日志,我们立刻知道
    // 它之后的上报可能有缺口。
    let _ = append_diagnostic_log(
        "diagnostics.log_compacted",
        serde_json::json!({
            "was_bytes": len,
            "kept_bytes": keep,
            "error": "log compacted; upload watermark may now point into rewritten content",
        }),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 压缩必须留下痕迹 —— 它会让上传水位指进被改写过的内容,
    /// 那之后的上报可能有缺口,而缺口本身是看不见的。
    #[test]
    fn compaction_leaves_a_reportable_trace() {
        let source = include_str!("diagnostic_log.rs");
        let code: String = source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("
");
        let compact = code
            .split_once("fn compact_diagnostic_log(")
            .expect("找不到压缩函数")
            .1;
        let compact = &compact[..compact.find("#[cfg(test)]").unwrap_or(compact.len())];

        assert!(
            compact.contains("diagnostics.log_compacted"),
            "压缩没有留下诊断事件,水位错位将无从察觉"
        );
        // 事件名里没有 fail/error 关键词,只能靠 error 字段才传得回服务端。
        assert!(
            compact.contains("\"error\":"),
            "diagnostics.log_compacted 缺少 error 字段,传不回来"
        );
    }

    #[test]
    fn compact_diagnostic_log_keeps_tail_and_drops_partial_first_line() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("codex-plus.log");
        std::fs::write(&path, "line-1\nline-2\nline-3\nline-4\n").unwrap();

        compact_diagnostic_log(&path, 12, 16).unwrap();

        let contents = std::fs::read_to_string(path).unwrap();
        assert_eq!(contents, "line-3\nline-4\n");
    }

    #[test]
    fn clear_diagnostic_log_ignores_missing_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("missing.log");

        clear_diagnostic_log_path(&path).unwrap();
    }
}
