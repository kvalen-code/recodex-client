use anyhow::Context;
use serde_json::{Map, Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct BackupStore {
    root: PathBuf,
}

impl BackupStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn write_backup(
        &self,
        session_id: &str,
        source_db: &Path,
        tables: serde_json::Value,
    ) -> anyhow::Result<String> {
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let token = format!("{epoch}-{}", Uuid::new_v4().simple());
        fs::create_dir_all(&self.root).with_context(|| {
            format!(
                "failed to create backup directory {}",
                self.root.to_string_lossy()
            )
        })?;
        let payload = json!({
            "token": token,
            "session_id": session_id,
            "source_db": source_db.to_string_lossy(),
            "tables": tables,
        });
        fs::write(
            self.path_for(&token),
            serde_json::to_string_pretty(&payload)?,
        )?;
        Ok(token)
    }

    /// 读备份,并把同 token 的**残骸补记**(sidecar)并进来。
    ///
    /// 启动清扫清掉索引/侧边栏残骸之前要先把它们存下来,否则原 token 撤销不回来。
    /// 早先是直接并进主备份文件里,可主备份带着整份 rollout 的 base64,几百 MB 的
    /// 「整份读入 + pretty 重写」会把启动卡住好几秒 —— 为此加过 32MB 上限,结果
    /// 超限的线程**永远**清不掉(第三轮审计 应修 1)。改成写一个几 KB 的 sidecar,
    /// 清理这一步不再碰巨型文件;撤销时在这里合回去,上层代码不用改。
    pub fn read_backup(&self, token: &str) -> anyhow::Result<serde_json::Value> {
        let path = self.path_for(token);
        let text = fs::read_to_string(&path)
            .with_context(|| format!("Backup token not found: {token}"))?;
        let mut backup: Value = serde_json::from_str(&text)?;
        // sidecar 读不动时**不能**让整单撤销失败:主备份好好的(可能几百 MB、装着
        // 整份 rollout),而 sidecar 只有几 KB,被杀软/索引器短暂持锁、坏块、被人改坏
        // 都可能读不了。这一步与 restore_backups 对索引/侧边栏的口径一致:尽力而为,
        // 失败只记一条诊断(第四轮审计 A1)。写入侧(append_leftovers)仍是 fail-closed:
        // 存不下残骸就别清。
        match self.read_leftovers(token) {
            Ok(Some(leftovers)) => {
                if let Some(tables) = backup.get_mut("tables").and_then(Value::as_object_mut) {
                    merge_leftover_tables(tables, &leftovers);
                }
            }
            Ok(None) => {}
            Err(error) => {
                let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
                    "backup.leftovers_unreadable",
                    serde_json::json!({
                        "token": token,
                        "error": format!("{error:#}"),
                    }),
                );
            }
        }
        Ok(backup)
    }

    /// 撤销成功后清掉 sidecar:里面的条目已经放回索引/侧边栏了,留着只会在下一次
    /// 读这份备份时又合一遍(第四轮审计 S4)。尽力而为。
    pub fn remove_leftovers(&self, token: &str) -> bool {
        match fs::remove_file(self.leftovers_path(token)) {
            Ok(()) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => false,
        }
    }

    /// 残骸补记文件:`<token>.leftovers.json`,只放 `__session_index` / `__sidebar`。
    pub fn leftovers_path(&self, token: &str) -> PathBuf {
        self.sibling(token, "leftovers.json")
    }

    pub fn read_leftovers(&self, token: &str) -> anyhow::Result<Option<Map<String, Value>>> {
        let path = self.leftovers_path(token);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read leftovers {}", path.display()));
            }
        };
        let value: Value = serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse leftovers {}", path.display()))?;
        Ok(value
            .get("tables")
            .and_then(Value::as_object)
            .cloned()
            .or_else(|| value.as_object().cloned()))
    }

    /// 追加残骸(同一线程可能分几轮清理):与已有 sidecar 取并集后原子写回。
    pub fn append_leftovers(
        &self,
        token: &str,
        leftovers: &Map<String, Value>,
    ) -> anyhow::Result<()> {
        let mut tables = self.read_leftovers(token)?.unwrap_or_default();
        merge_leftover_tables(&mut tables, leftovers);
        let payload = json!({ "version": 1, "token": token, "tables": Value::Object(tables) });
        let path = self.leftovers_path(token);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        codex_plus_core::settings::atomic_write(
            &path,
            serde_json::to_string_pretty(&payload)?.as_bytes(),
        )
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path_for(&self, token: &str) -> PathBuf {
        self.sibling(token, "json")
    }

    fn sibling(&self, token: &str, extension: &str) -> PathBuf {
        let safe: String = token
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')
            .collect();
        self.root.join(format!("{safe}.{extension}"))
    }
}

/// 残骸并进备份表:索引行取并集,侧边栏快照按条目补齐,目录缓存行追加
/// (撤销时是 INSERT OR IGNORE,重复无害)。
pub(crate) fn merge_leftover_tables(tables: &mut Map<String, Value>, leftovers: &Map<String, Value>) {
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
}

fn merge_sidebar_snapshot(existing: &mut Value, incoming: &Value) {
    // 主备份里的 __sidebar 不是对象(null、被改坏)时,整份用 sidecar 的替换 ——
    // 原先直接 return,等于把清扫存下来的残骸悄悄丢掉,撤销就恢复不回侧边栏
    // (第四轮审计 S8)。
    if !existing.is_object() {
        *existing = incoming.clone();
        return;
    }
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
    // thread_id 只在 sidecar 里有、主备份没有时补上(撤销要用它定位线程)。
    if let Some(thread_id) = incoming.get("thread_id") {
        existing
            .entry("thread_id")
            .or_insert_with(|| thread_id.clone());
    }
}

/// 递归补齐:对象按键补缺,数组取并集,已有的标量不覆盖。
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
