//! 清理「ReCodex 删过的会话」留在索引与侧边栏缓存里的残骸。
//!
//! 旧版会话删除只删 `state_5.sqlite` 的行和 rollout 文件，却把同一线程留在
//! `session_index.jsonl`、`sqlite/codex-dev.db` 的目录缓存表和
//! `.codex-global-state.json` 里。结果是重启后会话又出现在侧边栏，点开报
//! 「no rollout found」。新版删除已经同步清这些地方（见 `storage.rs`），这里
//! 负责把**已经留下**的残骸扫掉。
//!
//! 安全边界——只动我们能证明是自己删掉、而且**现在仍是删除状态**的线程：
//! 1. 备份目录里有 ReCodex 删除时写下的备份；
//! 2. 备份的来源库是当前 Codex home 的会话库；
//! 3. 撤销过的备份不算（撤销成功时记进标记文件，见 [`record_undone_backups`]）；
//! 4. 这条线程**现在**不在任何存在性来源里。来源按备份种类不同：
//!    - 带 `threads` 行的备份：查 `threads` 表（与自动化表）。索引里还有它恰恰
//!      就是要清的残骸，所以不看索引；
//!    - 自动化任务会话（`automation_runs` 行）：查 `automation_runs` / `threads`
//!      表，**再加上** `session_index.jsonl`；
//!    - 纯 API 会话（备份里只有 `__session_index` / `__sidebar`）：数据库里本来
//!      就没有它，只能看 `session_index.jsonl`。撤销会把索引行放回去，所以索引里
//!      有 = 还在；备份里连索引行都没有的，无从区分「撤销过」与「残骸」，不碰。
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
//!
//! 它跑在拉起 Codex 之前(为了不被 Codex 写回),所以**限时**:备份可能有几百 MB
//! (整份 rollout 以 base64 存在里面)。每份备份只做一次轻量分类(不建 JSON 树,
//! 结果缓存在标记文件里),时间到了就把剩下的留到下次启动 —— 一轮只有在全部备份
//! 都分类完之后才开始清理,保证「最新那份备份」是真的最新。

use crate::storage::{add_thread_sidebar_backup_tables, sidebar_backup_has_content};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MARKER_FILE_NAME: &str = ".leftover-sweep.json";
const MARKER_VERSION: u64 = 1;
/// 启动期清扫最多占用的时间。它挡在拉起 Codex 前面,超出部分顺延到下次启动。
pub const STARTUP_SWEEP_BUDGET: Duration = Duration::from_millis(1500);

// 残骸不再并回主备份文件(那是「整份读入 + pretty 重写」,几百 MB 的备份能把启动
// 卡住好几秒),改成写同 token 的 sidecar `<token>.leftovers.json`,撤销时由
// BackupStore::read_backup 合回去。所以这里不需要任何备份大小上限 —— 上一版为此
// 加的 32MB 上限会让超大备份对应的幽灵条目**永远**清不掉(第三轮审计 应修 1)。

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LeftoverSweepStatus {
    Completed,
    /// Codex 正在运行，整轮顺延。
    DeferredCodexRunning,
    /// 找不到任何可查询的会话库，无法证明线程已删除，整轮跳过。
    NoStateDatabase,
    /// 限时内没把备份分类完:已分类的记下来,这一轮不清理,下次启动接着做。
    DeferredBudget,
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
    /// 这一轮没下结论、留到下次的线程数（库查不了/清理中途失败/超时）。
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

/// 启动器入口：Codex 在运行就顺延；限时执行；结果写诊断日志，永不失败。
pub fn sweep_deleted_thread_leftovers_at_startup(codex_home: &Path, backup_dir: &Path) {
    let blocking = codex_plus_core::watcher::find_session_index_cleanup_blocking_processes();
    let report = if blocking.is_empty() {
        match sweep_deleted_thread_leftovers_within(codex_home, backup_dir, STARTUP_SWEEP_BUDGET) {
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
        // 只上报计数与状态;错误信息里可能带本机路径,只报条数。
        let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
            "launcher.deleted_thread_leftover_sweep",
            json!({
                "status": report.status,
                "backups_scanned": report.backups_scanned,
                "threads_cleaned": report.threads_cleaned,
                "threads_still_present": report.threads_still_present,
                "threads_deferred": report.threads_deferred,
                "session_index_lines_removed": report.session_index_lines_removed,
                "global_state_entries_removed": report.global_state_entries_removed,
                "catalog_rows_removed": report.catalog_rows_removed,
                "error_count": report.errors.len(),
                "codex_running": !blocking.is_empty(),
            }),
        );
    }
}

/// 扫描并清理(不限时)。调用方负责保证 Codex 未运行（见 [`sweep_deleted_thread_leftovers_at_startup`]）。
pub fn sweep_deleted_thread_leftovers(
    codex_home: &Path,
    backup_dir: &Path,
) -> anyhow::Result<LeftoverSweepReport> {
    sweep_deleted_thread_leftovers_within(codex_home, backup_dir, Duration::MAX)
}

/// 限时版本。`budget` 用完时:分类阶段 → 本轮不清理(`DeferredBudget`);
/// 清理阶段 → 剩下的线程不打标记,下次再来。
pub fn sweep_deleted_thread_leftovers_within(
    codex_home: &Path,
    backup_dir: &Path,
    budget: Duration,
) -> anyhow::Result<LeftoverSweepReport> {
    let deadline = Instant::now().checked_add(budget);
    let out_of_time = || deadline.is_some_and(|deadline| Instant::now() >= deadline);
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

    // thread id -> 这条线程的备份(按文件名即时间先后排序)
    let mut threads: BTreeMap<String, Vec<(String, BackupKind)>> = BTreeMap::new();
    for token in list_backup_tokens(backup_dir)? {
        if marker.processed.contains_key(&token) {
            continue;
        }
        let classification = match marker.classified.get(&token).cloned() {
            Some(cached) => cached,
            None => {
                if out_of_time() {
                    report.status = LeftoverSweepStatus::DeferredBudget;
                    break;
                }
                report.backups_scanned += 1;
                match classify_backup(&backup_dir.join(format!("{token}.json"))) {
                    Classified::Unreadable => {
                        marker.processed.insert(token, json!("unreadable"));
                        continue;
                    }
                    Classified::NotOurs => {
                        marker
                            .processed
                            .insert(token, json!("not_a_deleted_codex_thread"));
                        continue;
                    }
                    Classified::Deleted(entry) => {
                        marker.classified.insert(token.clone(), entry.clone());
                        entry
                    }
                }
            }
        };
        let source_ok = fs::canonicalize(&classification.source_db)
            .is_ok_and(|source| allowed_sources.contains(&source));
        if !source_ok {
            marker.classified.remove(&token);
            marker
                .processed
                .insert(token, json!("not_a_deleted_codex_thread"));
            continue;
        }
        threads
            .entry(classification.thread_id.clone())
            .or_default()
            .push((token, classification.kind));
    }

    if report.status == LeftoverSweepStatus::DeferredBudget {
        report.threads_deferred = threads.len();
        save_marker(&marker_path, &marker)?;
        return Ok(report);
    }

    let mut session_index_ids = None;
    for (thread_id, tokens) in threads {
        if out_of_time() {
            report.threads_deferred += 1;
            continue;
        }
        let token_ids = tokens.iter().map(|(token, _)| token.clone()).collect::<Vec<_>>();
        let presence = match thread_presence(
            &state_dbs,
            codex_home,
            &mut session_index_ids,
            &thread_id,
            &tokens,
        ) {
            Err(error) => {
                report.threads_deferred += 1;
                report.errors.push(format!("{thread_id}: {error}"));
                continue;
            }
            Ok(presence) => presence,
        };
        match presence {
            Presence::Present => {
                // 撤销过（或又被别的途径放回来了）：不是残骸，不碰。
                report.threads_still_present += 1;
                mark_all(&mut marker, &token_ids, "thread_present");
                continue;
            }
            Presence::Unverifiable => {
                mark_all(&mut marker, &token_ids, "unverifiable_index_only");
                continue;
            }
            Presence::Absent => {}
        }
        let newest = token_ids.last().expect("每个线程至少一份备份").clone();
        match clean_thread(codex_home, backup_dir, &newest, &thread_id) {
            Ok(None) => mark_all(&mut marker, &token_ids, "no_leftovers"),
            Ok(Some(cleaned)) => {
                report.threads_cleaned += 1;
                report.session_index_lines_removed += cleaned.session_index_lines;
                report.global_state_entries_removed += cleaned.global_state_entries;
                report.catalog_rows_removed += cleaned.catalog_rows;
                mark_all(&mut marker, &token_ids, "cleaned");
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

/// 撤销成功后调用:这些备份对应的会话已经回来了,启动清扫不能再按它们清一遍。
/// 尽力而为,失败只意味着清扫退回到按存在性判断。
pub fn record_undone_backups(backup_dir: &Path, tokens: &[String]) {
    if tokens.is_empty() {
        return;
    }
    let marker_path = backup_dir.join(MARKER_FILE_NAME);
    let mut marker = load_marker(&marker_path);
    let store = crate::BackupStore::new(backup_dir.to_path_buf());
    for token in tokens {
        marker.classified.remove(token);
        marker.processed.insert(token.clone(), json!("undone"));
        // 残骸已经随这次撤销放回索引/侧边栏了,sidecar 留着没用(S4)。
        store.remove_leftovers(token);
    }
    let _ = save_marker(&marker_path, &marker);
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

    // 先备份:写进同 token 的 sidecar(几 KB),撤销原 token 时 BackupStore 会把它
    // 合回来 —— 不去动可能有几百 MB 的主备份文件。
    let store = crate::BackupStore::new(backup_dir.to_path_buf());
    anyhow::ensure!(
        store.path_for(newest_token).is_file(),
        "备份不存在:{newest_token}"
    );
    store.append_leftovers(newest_token, &leftovers)?;

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

// ---------- 备份分类(轻量:不为 rollout 内容建 JSON 树) ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum BackupKind {
    /// 备份里有这条线程的 `threads` 行。
    ThreadRow,
    /// 自动化任务会话:`automation_runs` 行。
    AutomationRun,
    /// 纯 API 会话:只有索引/侧边栏,`had_index` = 备份里有 `__session_index` 行。
    IndexOnly { had_index: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ClassifiedBackup {
    thread_id: String,
    source_db: String,
    kind: BackupKind,
}

enum Classified {
    Unreadable,
    NotOurs,
    Deleted(ClassifiedBackup),
}

#[derive(Deserialize)]
struct BackupHead {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    source_db: Option<String>,
    #[serde(default)]
    tables: TablesHead,
}

/// 只挑分类要用的几个表;`__files`(整份 rollout 的 base64)等其余键被 serde
/// 跳过,不分配内存。
#[derive(Default, Deserialize)]
struct TablesHead {
    #[serde(default)]
    threads: Option<Vec<IdRow>>,
    #[serde(default)]
    automation_runs: Option<Vec<ThreadIdRow>>,
    #[serde(default, rename = "__session_index")]
    session_index: Option<Vec<serde::de::IgnoredAny>>,
    #[serde(default, rename = "__sidebar")]
    sidebar: Option<SidebarHead>,
}

#[derive(Deserialize)]
struct IdRow {
    #[serde(default)]
    id: Option<Value>,
}

#[derive(Deserialize)]
struct ThreadIdRow {
    #[serde(default)]
    thread_id: Option<Value>,
}

#[derive(Deserialize)]
struct SidebarHead {
    #[serde(default)]
    thread_id: Option<Value>,
}

fn classify_backup(path: &Path) -> Classified {
    let Ok(file) = fs::File::open(path) else {
        return Classified::Unreadable;
    };
    let Ok(head) = serde_json::from_reader::<_, BackupHead>(std::io::BufReader::new(file)) else {
        return Classified::Unreadable;
    };
    match deleted_thread(&head) {
        Some(entry) => Classified::Deleted(entry),
        None => Classified::NotOurs,
    }
}

/// 备份能证明「这是 ReCodex 删掉的线程」时返回分类。来源库是否属于当前 Codex
/// home 由调用方判断(缓存的分类在下次启动时也要重新判断)。
fn deleted_thread(head: &BackupHead) -> Option<ClassifiedBackup> {
    let session_id = head.session_id.as_deref()?;
    let thread_id = session_id.strip_prefix("local:").unwrap_or(session_id);
    if thread_id.trim().is_empty() {
        return None;
    }
    let source_db = head
        .source_db
        .clone()
        .filter(|path| !path.trim().is_empty())?;
    let tables = &head.tables;
    let matches =
        |value: &Option<Value>| value.as_ref().and_then(Value::as_str) == Some(thread_id);
    let has_thread_row = tables
        .threads
        .as_ref()
        .is_some_and(|rows| rows.iter().any(|row| matches(&row.id)));
    let has_automation_row = tables
        .automation_runs
        .as_ref()
        .is_some_and(|rows| rows.iter().any(|row| matches(&row.thread_id)));
    let had_index = tables
        .session_index
        .as_ref()
        .is_some_and(|lines| !lines.is_empty());
    let has_sidebar = tables
        .sidebar
        .as_ref()
        .is_some_and(|sidebar| matches(&sidebar.thread_id));
    let kind = if has_thread_row {
        BackupKind::ThreadRow
    } else if has_automation_row {
        BackupKind::AutomationRun
    } else if had_index || has_sidebar {
        BackupKind::IndexOnly { had_index }
    } else {
        return None;
    };
    Some(ClassifiedBackup {
        thread_id: thread_id.to_string(),
        source_db,
        kind,
    })
}

// ---------- 存在性判断 ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Presence {
    Present,
    Absent,
    /// 无从判断(只有侧边栏快照的纯 API 备份):不碰。
    Unverifiable,
}

fn thread_presence(
    state_dbs: &[PathBuf],
    codex_home: &Path,
    session_index_ids: &mut Option<HashSet<String>>,
    thread_id: &str,
    tokens: &[(String, BackupKind)],
) -> anyhow::Result<Presence> {
    if thread_in_state_dbs(state_dbs, thread_id)? {
        return Ok(Presence::Present);
    }
    // 只要有一份带 threads 行的备份,索引里剩下的就是残骸本身,不能拿来判断存在。
    if tokens
        .iter()
        .any(|(_, kind)| *kind == BackupKind::ThreadRow)
    {
        return Ok(Presence::Absent);
    }
    // 自动化任务会话 / 纯 API 会话:数据库里本来就可能没有它,撤销放回的是索引行。
    let ids = match session_index_ids {
        Some(ids) => ids,
        None => session_index_ids.insert(session_index_thread_ids(codex_home)?),
    };
    if ids.contains(thread_id) {
        return Ok(Presence::Present);
    }
    let verifiable = tokens.iter().any(|(_, kind)| {
        matches!(
            kind,
            BackupKind::AutomationRun | BackupKind::IndexOnly { had_index: true }
        )
    });
    Ok(if verifiable {
        Presence::Absent
    } else {
        Presence::Unverifiable
    })
}

/// 线程在不在会话库里:`threads` 表,或自动化任务的 `automation_runs` 表。
fn thread_in_state_dbs(state_dbs: &[PathBuf], thread_id: &str) -> anyhow::Result<bool> {
    for path in state_dbs {
        let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        for (table, column) in [("threads", "id"), ("automation_runs", "thread_id")] {
            if !table_has_column(&db, table, column)? {
                continue;
            }
            let found = db
                .query_row(
                    &format!("SELECT 1 FROM \"{table}\" WHERE \"{column}\" = ?1 LIMIT 1"),
                    [thread_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if found {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn table_has_column(db: &Connection, table: &str, column: &str) -> anyhow::Result<bool> {
    let exists = db
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !exists {
        return Ok(false);
    }
    let mut stmt = db.prepare(&format!("PRAGMA table_info(\"{table}\")"))?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(columns.iter().any(|name| name == column))
}

fn session_index_thread_ids(codex_home: &Path) -> anyhow::Result<HashSet<String>> {
    let text = match fs::read_to_string(codex_home.join("session_index.jsonl")) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
        Err(error) => return Err(error.into()),
    };
    Ok(text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|value| value.get("id").and_then(Value::as_str).map(str::to_string))
        .map(|id| id.strip_prefix("local:").map(str::to_string).unwrap_or(id))
        .collect())
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

/// 标记文件:`processed` 是已下结论的备份(不再看);`classified` 是分类缓存
/// (备份只解析一次,限时跑不完时下次直接用)。旧标记文件没有 `classified`。
#[derive(Default)]
struct Marker {
    processed: Map<String, Value>,
    classified: BTreeMap<String, ClassifiedBackup>,
}

fn load_marker(path: &Path) -> Marker {
    let Some(value) = fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter(|value| value.get("version").and_then(Value::as_u64) == Some(MARKER_VERSION))
    else {
        return Marker::default();
    };
    Marker {
        processed: value
            .get("processed")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default(),
        classified: value
            .get("classified")
            .cloned()
            .and_then(|classified| serde_json::from_value(classified).ok())
            .unwrap_or_default(),
    }
}

fn save_marker(path: &Path, marker: &Marker) -> anyhow::Result<()> {
    if marker.processed.is_empty() && marker.classified.is_empty() && !path.exists() {
        return Ok(());
    }
    let mut value = json!({ "version": MARKER_VERSION, "processed": marker.processed });
    if !marker.classified.is_empty() {
        value["classified"] = serde_json::to_value(&marker.classified)?;
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    codex_plus_core::settings::atomic_write(path, serde_json::to_string_pretty(&value)?.as_bytes())
}

fn mark_all(marker: &mut Marker, tokens: &[String], outcome: &str) {
    for token in tokens {
        marker.classified.remove(token);
        marker.processed.insert(token.clone(), json!(outcome));
    }
}
