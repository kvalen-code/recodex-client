//! 1.3.8 审计:删除兜底不能覆盖撤销 token / 谎报成功(R1)、撤销要能重试且侧边栏
//! 恢复是尽力而为(R3)、thread-descriptions-v1 随删除清理并可撤销(S6)。
//! 全部在临时目录里跑。

use codex_plus_core::models::{DeleteStatus, SessionRef};
use codex_plus_data::{BackupStore, SQLiteStorageAdapter, delete_local_from_paths};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

fn session(id: &str) -> SessionRef {
    SessionRef::new(id, "Codex Thread").unwrap()
}

/// 一个带 t1 的 Codex home:state_5.sqlite(threads 行)+ rollout + 索引 + 全局状态
/// (含 thread-descriptions-v1)+ 目录缓存库。
struct Home {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    db: PathBuf,
    rollout: PathBuf,
    backups: PathBuf,
}

fn home_with_thread() -> Home {
    let tmp = tempdir().unwrap();
    let home = tmp.path().join(".codex");
    fs::create_dir_all(home.join("sqlite")).unwrap();
    let rollout = home.join("sessions").join("rollout-t1.jsonl");
    fs::create_dir_all(rollout.parent().unwrap()).unwrap();
    fs::write(&rollout, "{\"type\":\"message\"}\n").unwrap();
    let db = home.join("state_5.sqlite");
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, title TEXT, cwd TEXT, \
         archived INTEGER, archived_at INTEGER, updated_at INTEGER, updated_at_ms INTEGER);",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO threads VALUES ('t1', ?1, 'Codex Thread', '/p', 0, NULL, 100, 100000)",
        [rollout.to_string_lossy().to_string()],
    )
    .unwrap();
    drop(conn);
    fs::write(
        home.join("session_index.jsonl"),
        "{\"id\":\"t1\",\"thread_name\":\"T1\",\"updated_at\":\"2026-09-20T00:00:00Z\"}\n\
{\"id\":\"keep\",\"thread_name\":\"K\",\"updated_at\":\"2026-09-20T00:00:00Z\"}\n",
    )
    .unwrap();
    fs::write(
        home.join(".codex-global-state.json"),
        serde_json::to_vec(&json!({
            "projectless-thread-ids": ["t1", "keep"],
            "electron-persisted-atom-state": {
                "thread-descriptions-v1": { "t1": "描述 t1", "keep": "描述 keep" },
                "thread-client-id-v1:t1": "client-t1",
                "sidebar-width": 296
            }
        }))
        .unwrap(),
    )
    .unwrap();
    let catalog = Connection::open(home.join("sqlite").join("codex-dev.db")).unwrap();
    catalog
        .execute_batch(
            "CREATE TABLE local_thread_catalog (thread_id TEXT PRIMARY KEY, payload TEXT);
             INSERT INTO local_thread_catalog VALUES ('t1', 'p1'), ('keep', 'pk');
             CREATE TABLE local_thread_catalog_metadata (catalog_revision INTEGER);
             INSERT INTO local_thread_catalog_metadata VALUES (1);",
        )
        .unwrap();
    drop(catalog);
    let backups = tmp.path().join("backups");
    Home {
        _tmp: tmp,
        home,
        db,
        rollout,
        backups,
    }
}

fn index_has(home: &Path, id: &str) -> bool {
    fs::read_to_string(home.join("session_index.jsonl"))
        .unwrap()
        .contains(&format!("\"id\":\"{id}\""))
}

fn thread_rows(db: &Path, id: &str) -> i64 {
    Connection::open(db)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM threads WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .unwrap()
}

fn global_state(home: &Path) -> Value {
    serde_json::from_slice(&fs::read(home.join(".codex-global-state.json")).unwrap()).unwrap()
}

fn backup_tables(backups: &Path, token: &str) -> Value {
    let backup: Value =
        serde_json::from_slice(&fs::read(backups.join(format!("{token}.json"))).unwrap()).unwrap();
    backup["tables"].clone()
}

/// R1 场景 a:rollout 被占删不掉 —— 数据库行已删、手里已有带整行 + rollout 的
/// token。第二个候选库「查无此会话」也不能让兜底写只含索引的备份把它换掉。
#[cfg(windows)]
#[test]
fn partially_deleted_thread_keeps_its_full_undo_token() {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 0x1;

    let h = home_with_thread();
    let empty_db = h.home.join("other.sqlite");
    Connection::open(&empty_db)
        .unwrap()
        .execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, title TEXT);")
        .unwrap();
    // 只允许别人读:备份能读到 rollout,删除会撞上共享冲突。
    let lock = fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(&h.rollout)
        .unwrap();

    let result = delete_local_from_paths(
        vec![h.db.clone(), empty_db],
        BackupStore::new(&h.backups),
        &session("local:t1"),
        Some(h.home.as_path()),
    );
    drop(lock);

    // 状态是 Partial 而不是 Failed:索引/侧边栏已经清掉,界面必须给撤销按钮。
    assert_eq!(result.status, DeleteStatus::Partial, "{}", result.message);
    let token = result.undo_token.clone().expect("必须保留原撤销 token");
    let tables = backup_tables(&h.backups, &token);
    assert!(tables.get("threads").is_some(), "token 必须是带整行的那份: {tables}");
    assert!(tables.get("__files").is_some(), "token 必须带 rollout");
    assert_eq!(thread_rows(&h.db, "t1"), 0);
    // 行已经没了:索引/侧边栏条目一并清掉,且都在同一份备份里。
    assert!(!index_has(&h.home, "t1"));
    assert!(index_has(&h.home, "keep"));
    assert!(tables.get("__session_index").is_some());

    // 用这把 token 撤销:库行回来;rollout 还在且内容一致,不算冲突。
    let undone = SQLiteStorageAdapter::new(&h.db, BackupStore::new(&h.backups))
        .with_codex_home(&h.home)
        .undo(&token);
    assert_eq!(undone.status, DeleteStatus::Undone, "{}", undone.message);
    assert_eq!(thread_rows(&h.db, "t1"), 1);
    assert!(index_has(&h.home, "t1"));
}

/// R1 场景 b:库查不了(这里是坏库;锁住同理)—— 会话可能还在,不能走「只清索引」
/// 的兜底把侧边栏清掉并报成功。
#[test]
fn unreadable_database_does_not_fall_back_to_index_cleanup() {
    let h = home_with_thread();
    fs::write(&h.db, b"this is not a sqlite database at all, just garbage bytes....").unwrap();

    let result = delete_local_from_paths(
        vec![h.db.clone()],
        BackupStore::new(&h.backups),
        &session("local:t1"),
        Some(h.home.as_path()),
    );

    assert_eq!(result.status, DeleteStatus::Failed, "{}", result.message);
    assert!(result.undo_token.is_none());
    assert!(index_has(&h.home, "t1"), "索引不能被兜底清掉");
    assert_eq!(
        global_state(&h.home)["projectless-thread-ids"],
        json!(["t1", "keep"])
    );
    assert!(!h.backups.exists() || fs::read_dir(&h.backups).unwrap().next().is_none());
}

/// R1:删除连接有 busy_timeout —— Codex 短暂持有写锁时等一下就能删,而不是
/// 立刻 SQLITE_BUSY。
#[test]
fn delete_waits_for_a_briefly_locked_database() {
    let h = home_with_thread();
    let locker = Connection::open(&h.db).unwrap();
    locker.execute_batch("BEGIN EXCLUSIVE;").unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        locker.execute_batch("COMMIT;").unwrap();
    });

    let result = SQLiteStorageAdapter::new(&h.db, BackupStore::new(&h.backups))
        .with_codex_home(&h.home)
        .delete_local(&session("local:t1"));
    release.join().unwrap();

    assert_eq!(result.status, DeleteStatus::LocalDeleted, "{}", result.message);
    assert_eq!(thread_rows(&h.db, "t1"), 0);
}

/// R3:撤销重试 —— 行/文件已经被上一次撤销恢复且内容相同,视为已恢复,不是冲突。
#[test]
fn undo_is_idempotent_when_rows_and_files_were_already_restored() {
    let h = home_with_thread();
    let adapter = SQLiteStorageAdapter::new(&h.db, BackupStore::new(&h.backups))
        .with_codex_home(&h.home);
    let deleted = adapter.delete_local(&session("local:t1"));
    assert_eq!(deleted.status, DeleteStatus::LocalDeleted, "{}", deleted.message);
    let token = deleted.undo_token.unwrap();

    let first = adapter.undo(&token);
    assert_eq!(first.status, DeleteStatus::Undone, "{}", first.message);
    let second = adapter.undo(&token);
    assert_eq!(second.status, DeleteStatus::Undone, "{}", second.message);
    assert_eq!(thread_rows(&h.db, "t1"), 1);
    assert_eq!(
        fs::read_to_string(h.home.join("session_index.jsonl"))
            .unwrap()
            .matches("\"id\":\"t1\"")
            .count(),
        1
    );
}

/// R3:同键的行被改过(不是上一次撤销放回来的)仍然是冲突,不覆盖。
#[test]
fn undo_still_refuses_a_row_that_changed_after_deletion() {
    let h = home_with_thread();
    let adapter = SQLiteStorageAdapter::new(&h.db, BackupStore::new(&h.backups))
        .with_codex_home(&h.home);
    let token = adapter.delete_local(&session("local:t1")).undo_token.unwrap();
    Connection::open(&h.db)
        .unwrap()
        .execute(
            "INSERT INTO threads (id, rollout_path, title) VALUES ('t1', '', 'Brand new')",
            [],
        )
        .unwrap();

    let result = adapter.undo(&token);
    assert_eq!(result.status, DeleteStatus::Failed);
    assert!(result.message.contains("restore conflict"), "{}", result.message);
}

/// R3:目录缓存库被锁住时,撤销照样成功(侧边栏缓存恢复是尽力而为),
/// 不会出现「库行回来了、界面却报撤销失败、再撤又冲突」。
#[test]
fn undo_succeeds_even_if_the_catalog_cache_is_locked() {
    let h = home_with_thread();
    let adapter = SQLiteStorageAdapter::new(&h.db, BackupStore::new(&h.backups))
        .with_codex_home(&h.home);
    let token = adapter.delete_local(&session("local:t1")).undo_token.unwrap();
    // Codex 的目录缓存库是 WAL:读不受影响,写被 Codex 的写事务挡住。
    let locker = Connection::open(h.home.join("sqlite").join("codex-dev.db")).unwrap();
    locker
        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get::<_, String>(0))
        .unwrap();
    locker.execute_batch("BEGIN IMMEDIATE;").unwrap();

    let result = adapter.undo(&token);
    locker.execute_batch("ROLLBACK;").unwrap();

    assert_eq!(result.status, DeleteStatus::Undone, "{}", result.message);
    assert_eq!(thread_rows(&h.db, "t1"), 1);
    assert!(index_has(&h.home, "t1"));
}

/// S6:thread-descriptions-v1 里这条会话的摘要随删除清掉、进备份、撤销放回。
#[test]
fn thread_description_is_removed_with_the_thread_and_restored_by_undo() {
    let h = home_with_thread();
    let adapter = SQLiteStorageAdapter::new(&h.db, BackupStore::new(&h.backups))
        .with_codex_home(&h.home);
    let deleted = adapter.delete_local(&session("local:t1"));
    assert_eq!(deleted.status, DeleteStatus::LocalDeleted, "{}", deleted.message);

    let descriptions =
        &global_state(&h.home)["electron-persisted-atom-state"]["thread-descriptions-v1"];
    assert!(descriptions.get("t1").is_none());
    assert_eq!(descriptions["keep"], "描述 keep");
    let token = deleted.undo_token.unwrap();
    assert_eq!(
        backup_tables(&h.backups, &token)["__sidebar"]["global_state"]["atom_maps"]
            ["thread-descriptions-v1"]["t1"],
        "描述 t1"
    );

    let undone = adapter.undo(&token);
    assert_eq!(undone.status, DeleteStatus::Undone, "{}", undone.message);
    let state = global_state(&h.home);
    let descriptions = &state["electron-persisted-atom-state"]["thread-descriptions-v1"];
    assert_eq!(descriptions["t1"], "描述 t1");
    assert_eq!(descriptions["keep"], "描述 keep");
    assert_eq!(state["electron-persisted-atom-state"]["sidebar-width"], 296);
}

/// 应修 3:纯 API 会话(备份里没有数据库行)撤销时,索引写不进去(这里把
/// session_index.jsonl 换成目录,模拟被占/权限问题)——撤销必须报失败,
/// 而不是静默吞掉再报 Undone。
#[test]
fn index_only_undo_fails_loudly_when_the_index_cannot_be_written() {
    let h = home_with_thread();
    let mut index = fs::read_to_string(h.home.join("session_index.jsonl")).unwrap();
    index.push_str("{\"id\":\"api1\",\"thread_name\":\"API\",\"updated_at\":\"2026-09-20T00:00:00Z\"}
");
    fs::write(h.home.join("session_index.jsonl"), index).unwrap();
    let deleted = delete_local_from_paths(
        vec![h.db.clone()],
        BackupStore::new(&h.backups),
        &session("api1"),
        Some(h.home.as_path()),
    );
    assert_eq!(deleted.status, DeleteStatus::LocalDeleted, "{}", deleted.message);
    let token = deleted.undo_token.unwrap();
    // 索引文件读不了 → 恢复这一步必然失败。
    fs::remove_file(h.home.join("session_index.jsonl")).unwrap();
    fs::create_dir(h.home.join("session_index.jsonl")).unwrap();

    let undone = SQLiteStorageAdapter::new(&h.db, BackupStore::new(&h.backups))
        .with_codex_home(&h.home)
        .undo(&token);

    assert_eq!(undone.status, DeleteStatus::Failed, "{}", undone.message);
    assert_eq!(undone.undo_token.as_deref(), Some(token.as_str()), "还能再撤一次");
    // 没恢复成功就不能记 undone 标记,否则启动清扫会跳过它。
    let recorded = fs::read_to_string(h.backups.join(".leftover-sweep.json")).unwrap_or_default();
    assert!(!recorded.contains("undone"), "{recorded}");
}

/// 应修 3 的兜底检查:撤销做完之后这条会话必须真的回到索引/侧边栏里。
/// 这里用一份**对不上号**的备份(索引行属于别的会话)模拟「写了但没恢复到它」。
#[test]
fn index_only_undo_verifies_the_thread_is_actually_back() {
    let h = home_with_thread();
    let token = BackupStore::new(&h.backups)
        .write_backup(
            "ghost",
            &h.db,
            json!({
                "__session_index": ["{\"id\":\"someone-else\",\"thread_name\":\"X\",\"updated_at\":\"2026-09-20T00:00:00Z\"}"],
                "__sidebar": { "thread_id": "ghost", "global_state": {}, "catalog": [] }
            }),
        )
        .unwrap();

    let undone = SQLiteStorageAdapter::new(&h.db, BackupStore::new(&h.backups))
        .with_codex_home(&h.home)
        .undo(&token);

    assert_eq!(undone.status, DeleteStatus::Failed, "{}", undone.message);
    assert!(undone.message.contains("索引/侧边栏"), "{}", undone.message);
}

/// 反面:索引没被动过时,纯 API 会话的撤销照常成功,重试也成功(幂等)。
#[test]
fn index_only_undo_succeeds_and_can_be_retried() {
    let h = home_with_thread();
    let mut index = fs::read_to_string(h.home.join("session_index.jsonl")).unwrap();
    index.push_str("{\"id\":\"api1\",\"thread_name\":\"API\",\"updated_at\":\"2026-09-20T00:00:00Z\"}\n");
    fs::write(h.home.join("session_index.jsonl"), index).unwrap();
    let token = delete_local_from_paths(
        vec![h.db.clone()],
        BackupStore::new(&h.backups),
        &session("api1"),
        Some(h.home.as_path()),
    )
    .undo_token
    .unwrap();
    let adapter = SQLiteStorageAdapter::new(&h.db, BackupStore::new(&h.backups))
        .with_codex_home(&h.home);

    let first = adapter.undo(&token);
    assert_eq!(first.status, DeleteStatus::Undone, "{}", first.message);
    assert!(index_has(&h.home, "api1"));
    let second = adapter.undo(&token);
    assert_eq!(second.status, DeleteStatus::Undone, "{}", second.message);
}

/// E:表结构认不出来(Codex 改了列)时报失败,不能当成「查无此会话」去清索引。
#[test]
fn unsupported_schema_does_not_fall_back_to_index_cleanup() {
    let h = home_with_thread();
    // 把 threads 表换成一个缺列的版本:schema_kind 认不出来。
    let conn = Connection::open(&h.db).unwrap();
    conn.execute_batch("DROP TABLE threads; CREATE TABLE threads (id TEXT PRIMARY KEY, blob TEXT);")
        .unwrap();
    conn.execute("INSERT INTO threads VALUES ('t1', 'x')", []).unwrap();
    drop(conn);

    let result = delete_local_from_paths(
        vec![h.db.clone()],
        BackupStore::new(&h.backups),
        &session("local:t1"),
        Some(h.home.as_path()),
    );

    assert_eq!(result.status, DeleteStatus::Failed, "{}", result.message);
    assert!(result.undo_token.is_none());
    assert!(index_has(&h.home, "t1"), "索引不能被兜底清掉");
}

/// 但完全不相干的库(候选里按文件名扫进来的)仍算「这里没有」,不挡住纯 API 兜底。
#[test]
fn unrelated_database_still_allows_the_index_fallback() {
    let h = home_with_thread();
    let unrelated = h.home.join("sqlite").join("unrelated.sqlite");
    Connection::open(&unrelated)
        .unwrap()
        .execute_batch("CREATE TABLE notes (id TEXT PRIMARY KEY, body TEXT);")
        .unwrap();
    let mut index = fs::read_to_string(h.home.join("session_index.jsonl")).unwrap();
    index.push_str("{\"id\":\"api2\",\"thread_name\":\"API\",\"updated_at\":\"2026-09-20T00:00:00Z\"}\n");
    fs::write(h.home.join("session_index.jsonl"), index).unwrap();

    let result = delete_local_from_paths(
        vec![unrelated, h.db.clone()],
        BackupStore::new(&h.backups),
        &session("api2"),
        Some(h.home.as_path()),
    );

    assert_eq!(result.status, DeleteStatus::LocalDeleted, "{}", result.message);
    assert!(!index_has(&h.home, "api2"));
}
