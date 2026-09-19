//! 启动期「删除残骸」清理：只清 ReCodex 自己删掉、且现在确实不在库里的线程。
//! 全部在临时目录里跑，绝不碰真实的 ~/.codex。

use codex_plus_core::models::{DeleteStatus, SessionRef};
use codex_plus_data::{
    BackupStore, LeftoverSweepStatus, SQLiteStorageAdapter, sweep_deleted_thread_leftovers,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::{TempDir, tempdir};

const CATALOG_TABLES: [&str; 3] = [
    "local_thread_catalog",
    "thread_timeline_ledger",
    "local_thread_catalog_scan_entries",
];

struct Fixture {
    _tmp: TempDir,
    home: PathBuf,
    backups: PathBuf,
    state_db: PathBuf,
    rollout: PathBuf,
}

/// 一个 Codex home：
/// - t1：在库里、有 rollout，等下用「旧版删除」（不清索引）删掉 → 留下残骸；
/// - present：在库里，备份目录里也有一份它的删除备份（模拟删了又撤销）；
/// - orphan：不在库里，但索引/侧边栏里有，且**没有**我们的备份（不是我们删的）；
/// - keep：不在库里，只在索引/侧边栏里，无备份。
fn fixture() -> Fixture {
    let tmp = tempdir().unwrap();
    let home = tmp.path().join(".codex");
    let sqlite_dir = home.join("sqlite");
    fs::create_dir_all(&sqlite_dir).unwrap();
    let rollout = home.join("sessions/2026/09/20/rollout-t1.jsonl");
    fs::create_dir_all(rollout.parent().unwrap()).unwrap();
    fs::write(&rollout, "{\"type\":\"message\"}\n").unwrap();

    let state_db = home.join("state_5.sqlite");
    let db = Connection::open(&state_db).unwrap();
    db.execute_batch(
        "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, title TEXT, cwd TEXT, \
         archived INTEGER, archived_at INTEGER, updated_at INTEGER, updated_at_ms INTEGER);",
    )
    .unwrap();
    db.execute(
        "INSERT INTO threads VALUES ('t1', ?1, 'Deleted by us', '/p', 0, NULL, 100, 100000)",
        [rollout.to_string_lossy().to_string()],
    )
    .unwrap();
    db.execute(
        "INSERT INTO threads VALUES ('present', '', 'Undone', '/p', 0, NULL, 100, 100000)",
        [],
    )
    .unwrap();
    drop(db);

    let ids = ["t1", "present", "orphan", "keep"];
    let index = ids
        .iter()
        .map(|id| format!("{{\"id\":\"{id}\",\"thread_name\":\"{id}\",\"updated_at\":\"2026-09-20T00:00:00Z\"}}\n"))
        .collect::<String>();
    fs::write(home.join("session_index.jsonl"), index).unwrap();

    let mut assignments = serde_json::Map::new();
    let mut atoms = serde_json::Map::new();
    for id in ids {
        assignments.insert(
            id.to_string(),
            json!({"projectKind": "local", "projectId": "p"}),
        );
        atoms.insert(
            format!("thread-client-id-v1:{id}"),
            json!(format!("client-{id}")),
        );
        atoms.insert(format!("thread-tab-routes-v1:{id}"), json!({"routes": []}));
    }
    atoms.insert("sidebar-width".to_string(), json!(296));
    fs::write(
        home.join(".codex-global-state.json"),
        serde_json::to_vec(&json!({
            "projectless-thread-ids": ids,
            "pinned-thread-ids": ["t1", "orphan"],
            "thread-project-assignments": assignments,
            "thread-writable-roots": {"t1": ["C:/w"], "orphan": ["C:/o"]},
            "electron-persisted-atom-state": atoms,
            "unrelated-setting": true
        }))
        .unwrap(),
    )
    .unwrap();

    let catalog = Connection::open(sqlite_dir.join("codex-dev.db")).unwrap();
    for table in CATALOG_TABLES {
        catalog
            .execute(
                &format!("CREATE TABLE {table} (thread_id TEXT, payload TEXT)"),
                [],
            )
            .unwrap();
        for id in ids {
            catalog
                .execute(
                    &format!("INSERT INTO {table} VALUES (?1, ?2)"),
                    [id, &format!("{table}-{id}")],
                )
                .unwrap();
        }
    }
    catalog
        .execute_batch(
            "CREATE TABLE local_thread_catalog_metadata (catalog_revision INTEGER);
             INSERT INTO local_thread_catalog_metadata VALUES (1);",
        )
        .unwrap();
    drop(catalog);

    let backups = tmp.path().join("backups");
    Fixture {
        _tmp: tmp,
        home,
        backups,
        state_db,
        rollout,
    }
}

/// 旧版删除：只删数据库行和 rollout，不知道 codex home，索引/侧边栏全留着。
fn legacy_delete(fx: &Fixture, id: &str) -> String {
    let result = SQLiteStorageAdapter::new(&fx.state_db, BackupStore::new(&fx.backups))
        .delete_local(&SessionRef::new(format!("local:{id}"), id).unwrap());
    assert_eq!(
        result.status,
        DeleteStatus::LocalDeleted,
        "{}",
        result.message
    );
    result.undo_token.unwrap()
}

/// 往备份目录放一份「present 被删过」的备份（它后来被撤销，所以还在库里）。
fn write_backup_for_present(fx: &Fixture) {
    BackupStore::new(&fx.backups)
        .write_backup(
            "present",
            &fx.state_db,
            json!({"threads": [{"id": "present", "title": "Undone"}]}),
        )
        .unwrap();
}

fn index_ids(home: &Path) -> Vec<String> {
    fs::read_to_string(home.join("session_index.jsonl"))
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|value| value["id"].as_str().map(ToString::to_string))
        .collect()
}

fn global_state(home: &Path) -> Value {
    serde_json::from_slice(&fs::read(home.join(".codex-global-state.json")).unwrap()).unwrap()
}

fn catalog_count(home: &Path, table: &str, id: &str) -> i64 {
    Connection::open(home.join("sqlite/codex-dev.db"))
        .unwrap()
        .query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE thread_id = ?1"),
            [id],
            |row| row.get(0),
        )
        .unwrap()
}

fn assert_thread_fully_present(home: &Path, id: &str) {
    assert!(index_ids(home).contains(&id.to_string()), "{id} 应仍在索引");
    let state = global_state(home);
    assert!(
        state["projectless-thread-ids"]
            .as_array()
            .unwrap()
            .contains(&json!(id)),
        "{id} 应仍在 projectless-thread-ids"
    );
    assert!(state["thread-project-assignments"].get(id).is_some());
    assert!(
        state["electron-persisted-atom-state"]
            .get(format!("thread-client-id-v1:{id}"))
            .is_some()
    );
    for table in CATALOG_TABLES {
        assert_eq!(catalog_count(home, table, id), 1, "{id} 应仍在 {table}");
    }
}

#[test]
fn sweep_removes_leftovers_only_for_threads_we_deleted_and_undo_still_restores_everything() {
    let fx = fixture();
    let token = legacy_delete(&fx, "t1");
    write_backup_for_present(&fx);
    // 旧版删除留下的残骸
    assert!(index_ids(&fx.home).contains(&"t1".to_string()));

    let report = sweep_deleted_thread_leftovers(&fx.home, &fx.backups).unwrap();

    assert_eq!(report.status, LeftoverSweepStatus::Completed);
    assert_eq!(report.backups_scanned, 2);
    assert_eq!(report.threads_cleaned, 1);
    assert_eq!(report.threads_still_present, 1);
    assert_eq!(report.session_index_lines_removed, 1);
    assert_eq!(report.catalog_rows_removed, 3);
    assert!(report.errors.is_empty(), "{:?}", report.errors);

    // t1 的残骸全没了
    assert!(!index_ids(&fx.home).contains(&"t1".to_string()));
    let state = global_state(&fx.home);
    assert!(
        !state["projectless-thread-ids"]
            .as_array()
            .unwrap()
            .contains(&json!("t1"))
    );
    assert_eq!(state["pinned-thread-ids"], json!(["orphan"]));
    assert!(state["thread-project-assignments"].get("t1").is_none());
    assert!(state["thread-writable-roots"].get("t1").is_none());
    let atoms = &state["electron-persisted-atom-state"];
    assert!(atoms.get("thread-client-id-v1:t1").is_none());
    assert!(atoms.get("thread-tab-routes-v1:t1").is_none());
    assert_eq!(atoms["sidebar-width"], 296);
    assert_eq!(state["unrelated-setting"], true);
    for table in CATALOG_TABLES {
        assert_eq!(catalog_count(&fx.home, table, "t1"), 0, "{table}");
    }

    // 不是我们删的、或撤销过的，一个都不碰
    for id in ["present", "orphan", "keep"] {
        assert_thread_fully_present(&fx.home, id);
    }
    assert!(state["thread-writable-roots"].get("orphan").is_some());

    // 残骸先并进了原删除备份：撤销原 token 能把库行、rollout、索引、侧边栏一起找回
    let backup: Value =
        serde_json::from_slice(&fs::read(fx.backups.join(format!("{token}.json"))).unwrap())
            .unwrap();
    assert_eq!(
        backup["tables"]["__session_index"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(backup["tables"]["__sidebar"]["thread_id"], "t1");

    let undone = SQLiteStorageAdapter::new(&fx.state_db, BackupStore::new(&fx.backups))
        .with_codex_home(&fx.home)
        .undo(&token);
    assert_eq!(undone.status, DeleteStatus::Undone, "{}", undone.message);
    assert!(fx.rollout.exists());
    assert_thread_fully_present(&fx.home, "t1");
    let state = global_state(&fx.home);
    assert!(
        state["pinned-thread-ids"]
            .as_array()
            .unwrap()
            .contains(&json!("t1"))
    );
    assert!(
        state["electron-persisted-atom-state"]
            .get("thread-tab-routes-v1:t1")
            .is_some()
    );
}

#[test]
fn sweep_is_idempotent_and_skips_already_processed_backups() {
    let fx = fixture();
    legacy_delete(&fx, "t1");

    let first = sweep_deleted_thread_leftovers(&fx.home, &fx.backups).unwrap();
    assert_eq!(first.threads_cleaned, 1);
    let index_after_first = fs::read(fx.home.join("session_index.jsonl")).unwrap();
    let state_after_first = fs::read(fx.home.join(".codex-global-state.json")).unwrap();
    assert!(fx.backups.join(".leftover-sweep.json").exists());

    let second = sweep_deleted_thread_leftovers(&fx.home, &fx.backups).unwrap();
    assert_eq!(second.backups_scanned, 0);
    assert_eq!(second.threads_cleaned, 0);
    assert_eq!(
        fs::read(fx.home.join("session_index.jsonl")).unwrap(),
        index_after_first
    );
    assert_eq!(
        fs::read(fx.home.join(".codex-global-state.json")).unwrap(),
        state_after_first
    );
}

#[test]
fn sweep_never_touches_threads_without_our_backup() {
    let fx = fixture();
    // 不删任何东西：orphan/keep 不在库里，但备份目录里没有它们的删除记录。
    // 再放一份来源库不属于这个 Codex home 的备份，也必须被忽略。
    let foreign_db = fx.home.parent().unwrap().join("elsewhere.sqlite");
    Connection::open(&foreign_db)
        .unwrap()
        .execute_batch("CREATE TABLE threads (id TEXT PRIMARY KEY);")
        .unwrap();
    BackupStore::new(&fx.backups)
        .write_backup(
            "orphan",
            &foreign_db,
            json!({"threads": [{"id": "orphan"}]}),
        )
        .unwrap();
    let index_before = fs::read(fx.home.join("session_index.jsonl")).unwrap();
    let state_before = fs::read(fx.home.join(".codex-global-state.json")).unwrap();

    let report = sweep_deleted_thread_leftovers(&fx.home, &fx.backups).unwrap();

    assert_eq!(report.threads_cleaned, 0);
    assert_eq!(
        fs::read(fx.home.join("session_index.jsonl")).unwrap(),
        index_before
    );
    assert_eq!(
        fs::read(fx.home.join(".codex-global-state.json")).unwrap(),
        state_before
    );
    for id in ["t1", "present", "orphan", "keep"] {
        assert_thread_fully_present(&fx.home, id);
    }
}

#[test]
fn sweep_does_nothing_without_a_state_database() {
    let fx = fixture();
    legacy_delete(&fx, "t1");
    fs::remove_file(&fx.state_db).unwrap();
    let index_before = fs::read(fx.home.join("session_index.jsonl")).unwrap();

    let report = sweep_deleted_thread_leftovers(&fx.home, &fx.backups).unwrap();

    assert_eq!(report.status, LeftoverSweepStatus::NoStateDatabase);
    assert_eq!(
        fs::read(fx.home.join("session_index.jsonl")).unwrap(),
        index_before
    );
    assert!(!fx.backups.join(".leftover-sweep.json").exists());
}

#[test]
fn new_delete_path_leaves_nothing_for_the_sweep() {
    let fx = fixture();
    let result = SQLiteStorageAdapter::new(&fx.state_db, BackupStore::new(&fx.backups))
        .with_codex_home(&fx.home)
        .delete_local(&SessionRef::new("local:t1", "t1").unwrap());
    assert_eq!(
        result.status,
        DeleteStatus::LocalDeleted,
        "{}",
        result.message
    );
    assert!(!index_ids(&fx.home).contains(&"t1".to_string()));
    assert!(
        global_state(&fx.home)["thread-project-assignments"]
            .get("t1")
            .is_none()
    );

    let report = sweep_deleted_thread_leftovers(&fx.home, &fx.backups).unwrap();
    assert_eq!(report.threads_cleaned, 0);
    assert_eq!(report.backups_scanned, 1);
}
