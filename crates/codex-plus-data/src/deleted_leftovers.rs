//! 清理「ReCodex 删过的会话」留在索引与侧边栏缓存里的残骸。
//!
//! 旧版会话删除只删 `state_5.sqlite` 的行和 rollout 文件，却把同一线程留在
//! `session_index.jsonl`、`sqlite/codex-dev.db` 的目录缓存表和
//! `.codex-global-state.json` 里。结果是重启后会话又出现在侧边栏，点开报
//! 「no rollout found」。新版删除已经同步清这些地方（见 `storage.rs`），这里
//! 负责把**已经留下**的残骸扫掉。
//!
//! 安全边界——只动我们能证明是自己删掉的线程：
//! 1. 备份目录里有 ReCodex 删除时写下的备份，且备份里有这条线程的
//!    `threads` 行（或新版删除写下的 `__session_index`/`__sidebar`）；
//! 2. 备份的来源库是当前 Codex home 的会话库；
//! 3. 这条线程**现在**不在任何会话库的 `threads` 表里（撤销过的会被跳过）。
//!
//! 任何一个库查不了（锁住/损坏）就不下结论，留到下次。
//!
//! 清理前先把要删的条目合并进这条线程最新的那份删除备份，所以对原 token
//! 撤销仍能完整恢复（数据库行 + rollout + 索引 + 侧边栏）。
//!
//! 每份备份只处理一次：处理结果记在备份目录的 `.leftover-sweep.json` 里，
//! 重复运行是幂等的。只在 Codex 未运行时执行——Codex 运行中会把内存里的
//! 全局状态整份写回，此时清了也会被盖回去；这种情况整轮顺延到下次启动，
//! 不记任何标记。

use crate::storage::{add_thread_sidebar_backup_tables, sidebar_backup_has_content};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

const MARKER_FILE_NAME: &str = ".leftover-sweep.json";
const MARKER_VERSION: u64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LeftoverSweepStatus {
    Completed,
    /// Codex 正在运行，整轮顺延。
    DeferredCodexRunning,
    /// 找不到任何可查询的会话库，无法证明线程已删除，整轮跳过。
    NoStateDatabase,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LeftoverSweepReport {
    pub status: LeftoverSweepStatus,
    pub backups_scanned: usize,
    pub threads_cleaned: usize,
    pub threads_still_present: usize,
    pub session_index_lines_removed: usize,
    pub global_state_entries_removed: usize,
    pub catalog_rows_removed: usize,
    /// 这一轮没下结论、留到下次的线程数（库查不了/清理中途失败）。
    pub threads_deferred: usize,
    pub errors: Vec<String>,
}

impl LeftoverSweepReport {
    fn new(status: LeftoverSweepStatus) -> Self {
        Self {
            status,
            backups_scanned: 0,
            threads_cleaned: 0,
            threads_still_present: 0,
            session_index_lines_removed: 0,
            global_state_entries_removed: 0,
            catalog_rows_removed: 0,
            threads_deferred: 0,
            errors: Vec::new(),
        }
    }
}

/// 启动器入口：Codex 在运行就顺延；结果写诊断日志，永不失败。
pub fn sweep_deleted_thread_leftovers_at_startup(codex_home: &Path, backup_dir: &Path) {
    let blocking = codex_plus_core::watcher::find_session_index_cleanup_blocking_processes();
    let report = if blocking.is_empty() {
        match sweep_deleted_thread_leftovers(codex_home, backup_dir) {
            Ok(report) => report,
            Err(error) => {
                let mut report = LeftoverSweepReport::new(LeftoverSweepStatus::Completed);
                report.errors.push(error.to_string());
                report
            }
        }
    } else {
        LeftoverSweepReport::new(LeftoverSweepStatus::DeferredCodexRunning)
    };
    let nothing_happened = report.status == LeftoverSweepStatus::Completed
        && report.threads_cleaned == 0
        && report.threads_deferred == 0
        && report.errors.is_empty();
    if !nothing_happened {
        let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
            "launcher.deleted_thread_leftover_sweep",
            json!({ "report": report, "blocking_process_ids": blocking }),
        );
    }
}

/// 扫描并清理。调用方负责保证 Codex 未运行（见 [`sweep_deleted_thread_leftovers_at_startup`]）。
pub fn sweep_deleted_thread_leftovers(
    codex_home: &Path,
    backup_dir: &Path,
) -> anyhow::Result<LeftoverSweepReport> {
    let state_dbs = codex_plus_core::codex_sqlite::codex_session_db_paths_from_home(codex_home)
        .into_iter()
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    if state_dbs.is_empty() {
        return Ok(LeftoverSweepReport::new(
            LeftoverSweepStatus::NoStateDatabase,
        ));
    }
    let allowed_sources = state_dbs
        .iter()
        .filter_map(|path| fs::canonicalize(path).ok())
        .collect::<HashSet<_>>();

    let marker_path = backup_dir.join(MARKER_FILE_NAME);
    let mut marker = load_marker(&marker_path);
    let mut report = LeftoverSweepReport::new(LeftoverSweepStatus::Completed);

    // thread id -> 这条线程的备份 token（按文件名即时间先后排序）
    let mut threads: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for token in list_backup_tokens(backup_dir)? {
        if marker.contains_key(&token) {
            continue;
        }
        report.backups_scanned += 1;
        let path = backup_dir.join(format!("{token}.json"));
        let backup = match fs::read(&path)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| Ok(serde_json::from_slice::<Value>(&bytes)?))
        {
            Ok(backup) => backup,
            Err(_) => {
                marker.insert(token, json!("unreadable"));
                continue;
            }
        };
        match deleted_thread_id(&backup, &allowed_sources) {
            Some(thread_id) => threads.entry(thread_id).or_default().push(token),
            None => {
                marker.insert(token, json!("not_a_deleted_codex_thread"));
            }
        }
    }

    for (thread_id, tokens) in threads {
        match thread_present(&state_dbs, &thread_id) {
            Err(error) => {
                report.threads_deferred += 1;
                report.errors.push(format!("{thread_id}: {error}"));
                continue;
            }
            Ok(true) => {
                // 撤销过（或又被别的途径放回来了）：不是残骸，不碰。
                report.threads_still_present += 1;
                mark_all(&mut marker, &tokens, "thread_present");
                continue;
            }
            Ok(false) => {}
        }
        let newest = tokens.last().expect("每个线程至少一份备份").clone();
        match clean_thread(codex_home, backup_dir, &newest, &thread_id) {
            Ok(None) => mark_all(&mut marker, &tokens, "no_leftovers"),
            Ok(Some(cleaned)) => {
                report.threads_cleaned += 1;
                report.session_index_lines_removed += cleaned.session_index_lines;
                report.global_state_entries_removed += cleaned.global_state_entries;
                report.catalog_rows_removed += cleaned.catalog_rows;
                mark_all(&mut marker, &tokens, "cleaned");
            }
            Err(error) => {
                report.threads_deferred += 1;
                report.errors.push(format!("{thread_id}: {error}"));
            }
        }
    }

    save_marker(&marker_path, &marker)?;
    Ok(report)
}

struct CleanedCounts {
    session_index_lines: usize,
    global_state_entries: usize,
    catalog_rows: usize,
}

fn clean_thread(
    codex_home: &Path,
    backup_dir: &Path,
    newest_token: &str,
    thread_id: &str,
) -> anyhow::Result<Option<CleanedCounts>> {
    let mut leftovers = Map::new();
    add_thread_sidebar_backup_tables(&mut leftovers, codex_home, thread_id)?;
    if !sidebar_backup_has_content(&leftovers) {
        return Ok(None);
    }
    let expected_index_lines = leftovers
        .get("__session_index")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);

    // 先备份：并进原删除备份，撤销原 token 就能连同这些条目一起恢复。
    merge_into_backup(&backup_dir.join(format!("{newest_token}.json")), &leftovers)?;

    let session_index_lines =
        crate::provider_sync::remove_session_index_entry(codex_home, thread_id)?;
    if expected_index_lines > 0 && session_index_lines == 0 {
        anyhow::bail!("session_index.jsonl 在清理时被改动，留到下次");
    }
    let sidebar = crate::provider_sync::remove_thread_sidebar_references(codex_home, thread_id)?;
    Ok(Some(CleanedCounts {
        session_index_lines,
        global_state_entries: sidebar.global_state_entries_removed,
        catalog_rows: sidebar.catalog_rows_removed,
    }))
}

/// 备份能证明「这是 ReCodex 从当前 Codex 会话库删掉的线程」时返回线程 id。
fn deleted_thread_id(backup: &Value, allowed_sources: &HashSet<PathBuf>) -> Option<String> {
    let session_id = backup.get("session_id").and_then(Value::as_str)?;
    let thread_id = session_id.strip_prefix("local:").unwrap_or(session_id);
    if thread_id.trim().is_empty() {
        return None;
    }
    let source = backup.get("source_db").and_then(Value::as_str)?;
    let source = fs::canonicalize(source).ok()?;
    if !allowed_sources.contains(&source) {
        return None;
    }
    let tables = backup.get("tables").and_then(Value::as_object)?;
    let has_thread_row = tables
        .get("threads")
        .and_then(Value::as_array)
        .is_some_and(|rows| {
            rows.iter()
                .any(|row| row.get("id").and_then(Value::as_str) == Some(thread_id))
        });
    let has_index_backup = tables
        .get("__sidebar")
        .and_then(|sidebar| sidebar.get("thread_id"))
        .and_then(Value::as_str)
        == Some(thread_id)
        || tables
            .get("__session_index")
            .and_then(Value::as_array)
            .is_some_and(|lines| !lines.is_empty());
    (has_thread_row || has_index_backup).then(|| thread_id.to_string())
}

fn thread_present(state_dbs: &[PathBuf], thread_id: &str) -> anyhow::Result<bool> {
    for path in state_dbs {
        let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let has_threads = db
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'threads'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !has_threads {
            continue;
        }
        let found = db
            .query_row("SELECT 1 FROM threads WHERE id = ?1", [thread_id], |_| {
                Ok(())
            })
            .optional()?
            .is_some();
        if found {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 把残骸并进已有备份：索引行取并集，侧边栏快照按条目补齐，目录缓存行追加
/// （撤销时是 INSERT OR IGNORE，重复无害）。
fn merge_into_backup(path: &Path, leftovers: &Map<String, Value>) -> anyhow::Result<()> {
    let mut backup: Value = serde_json::from_slice(&fs::read(path)?)?;
    let tables = backup
        .get_mut("tables")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow::anyhow!("备份缺少 tables"))?;

    if let Some(lines) = leftovers.get("__session_index").and_then(Value::as_array) {
        let target = tables
            .entry("__session_index")
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Some(target) = target.as_array_mut() {
            for line in lines {
                if !target.contains(line) {
                    target.push(line.clone());
                }
            }
        }
    }

    if let Some(snapshot) = leftovers.get("__sidebar") {
        match tables.get_mut("__sidebar") {
            None => {
                tables.insert("__sidebar".to_string(), snapshot.clone());
            }
            Some(existing) => merge_sidebar_snapshot(existing, snapshot),
        }
    }

    codex_plus_core::settings::atomic_write(path, serde_json::to_string_pretty(&backup)?.as_bytes())
}

fn merge_sidebar_snapshot(existing: &mut Value, incoming: &Value) {
    let Some(existing) = existing.as_object_mut() else {
        return;
    };
    if let Some(incoming_global) = incoming.get("global_state").and_then(Value::as_object) {
        let global = existing
            .entry("global_state")
            .or_insert_with(|| Value::Object(Map::new()));
        if let Some(global) = global.as_object_mut() {
            merge_missing(global, incoming_global);
        }
    }
    if let Some(incoming_catalog) = incoming.get("catalog").and_then(Value::as_array) {
        let catalog = existing
            .entry("catalog")
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Some(catalog) = catalog.as_array_mut() {
            for entry in incoming_catalog {
                if !catalog.contains(entry) {
                    catalog.push(entry.clone());
                }
            }
        }
    }
}

/// 递归补齐：对象按键补缺，数组取并集，已有的标量不覆盖。
fn merge_missing(target: &mut Map<String, Value>, incoming: &Map<String, Value>) {
    for (key, value) in incoming {
        match (target.get_mut(key), value) {
            (None, _) => {
                target.insert(key.clone(), value.clone());
            }
            (Some(Value::Object(target)), Value::Object(value)) => merge_missing(target, value),
            (Some(Value::Array(target)), Value::Array(values)) => {
                for value in values {
                    if !target.contains(value) {
                        target.push(value.clone());
                    }
                }
            }
            _ => {}
        }
    }
}

fn list_backup_tokens(backup_dir: &Path) -> anyhow::Result<Vec<String>> {
    let entries = match fs::read_dir(backup_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut tokens = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().to_string();
            let token = name.strip_suffix(".json")?.to_string();
            is_backup_token(&token).then_some(token)
        })
        .collect::<Vec<_>>();
    // token = `<秒级时间戳>-<uuid>`：先按时间戳数值排，保证「最后一个」是最新的删除。
    tokens.sort_by_key(|token| {
        let epoch = token
            .split_once('-')
            .and_then(|(epoch, _)| epoch.parse::<u64>().ok())
            .unwrap_or(0);
        (epoch, token.clone())
    });
    Ok(tokens)
}

/// BackupStore 生成的 token：`<十进制秒>-<32 位十六进制>`。
fn is_backup_token(token: &str) -> bool {
    let Some((epoch, uuid)) = token.split_once('-') else {
        return false;
    };
    !epoch.is_empty()
        && epoch.bytes().all(|byte| byte.is_ascii_digit())
        && uuid.len() == 32
        && uuid.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn load_marker(path: &Path) -> Map<String, Value> {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(|value| value.get("version").and_then(Value::as_u64) == Some(MARKER_VERSION))
        .and_then(|value| value.get("processed").and_then(Value::as_object).cloned())
        .unwrap_or_default()
}

fn save_marker(path: &Path, processed: &Map<String, Value>) -> anyhow::Result<()> {
    if processed.is_empty() && !path.exists() {
        return Ok(());
    }
    let marker = json!({ "version": MARKER_VERSION, "processed": processed });
    codex_plus_core::settings::atomic_write(path, serde_json::to_string_pretty(&marker)?.as_bytes())
}

fn mark_all(marker: &mut Map<String, Value>, tokens: &[String], outcome: &str) {
    for token in tokens {
        marker.insert(token.clone(), json!(outcome));
    }
}
