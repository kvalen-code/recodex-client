use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

// recodex-overlay: 数据目录去品牌 `.codex-session-delete` → `.recodex`。
// 直接改名会让已有用户的设置/日志「凭空消失」,所以首次访问时把旧目录整个搬过来。
const APP_STATE_DIR: &str = ".recodex";
const LEGACY_APP_STATE_DIR: &str = ".codex-session-delete";
const SETTINGS_FILE: &str = "settings.json";
const LATEST_STATUS_FILE: &str = "latest-status.json";
const DIAGNOSTIC_LOG_FILE: &str = "recodex.log";
// 改名前的日志名。用户会打开 ~/.recodex/ 这个目录(客服让他发日志时),
// 里面躺着一个 codex-plus.log 就白费了整个目录的去品牌。
const LEGACY_DIAGNOSTIC_LOG_FILE: &str = "codex-plus.log";
const PENDING_PROVIDER_IMPORT_FILE: &str = "pending-provider-import.json";
const PENDING_REMOTE_CONTROL_RECOVERY_FILE: &str = "pending-remote-control-recovery.json";

pub fn default_app_state_dir() -> PathBuf {
    if let Some(home_dir) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()) {
        let current = home_dir.join(APP_STATE_DIR);
        migrate_legacy_app_state_dir(&home_dir.join(LEGACY_APP_STATE_DIR), &current);
        return current;
    }

    PathBuf::from(APP_STATE_DIR)
}

/// 把旧数据目录搬到新位置。只在「新目录还不存在且旧目录存在」时动手,
/// 且只做一次(重命名成功后旧目录就没了)。重命名失败不阻断启动 ——
/// 大不了用户回到默认设置,总好过程序起不来。
fn migrate_legacy_app_state_dir(legacy: &std::path::Path, current: &std::path::Path) {
    static DONE: OnceLock<()> = OnceLock::new();
    DONE.get_or_init(|| {
        let _ = try_migrate_app_state_dir(legacy, current);
    });
}

/// 只在「旧目录在、新目录不在」时搬迁。新目录已存在说明用户已经在用新版,
/// 此时搬过去会覆盖更新的数据 —— 宁可留下旧目录不管。
fn try_migrate_app_state_dir(
    legacy: &std::path::Path,
    current: &std::path::Path,
) -> std::io::Result<bool> {
    if current.exists() || !legacy.exists() {
        return Ok(false);
    }
    std::fs::rename(legacy, current)?;
    Ok(true)
}

pub fn default_settings_path() -> PathBuf {
    if let Some(path) = settings_path_for_tests() {
        return path;
    }
    default_app_state_dir().join(SETTINGS_FILE)
}

pub fn default_latest_status_path() -> PathBuf {
    default_app_state_dir().join(LATEST_STATUS_FILE)
}

pub fn default_diagnostic_log_path() -> PathBuf {
    let dir = default_app_state_dir();
    let current = dir.join(DIAGNOSTIC_LOG_FILE);
    // 顺手把旧日志改名过来。不搬的话,待回传的积压事件全留在旧文件里 ——
    // 那些事件会永远传不上来,而且不会有任何报错(上报任务只读新路径)。
    // 与数据目录搬迁同一条规则:新的已存在就不动,失败不阻断。
    migrate_legacy_diagnostic_log(&dir.join(LEGACY_DIAGNOSTIC_LOG_FILE), &current);
    current
}

/// 搬日志时**必须连上报水位一起搬**。
///
/// 上报进度存在一个边车文件 `<日志>.uploaded` 里(diagnostics_flush.rs 的
/// `watermark_path`),只搬日志本体的话水位读不到就归零 —— 于是整个日志从头重传。
/// 实测这台机器:日志 46 MB、旧水位 46,460,055,也就是 **98.9% 是已经传过的**;
/// 按每轮 20 条、每 10 分钟一轮算,单机要不间断重传约 17 小时,而且是全体升级用户
/// **同时**触发。那正是上一版刚修完的「重试风暴把限流额度烧光,真故障挤不进来」,
/// 只是这次没有任何报错。
///
/// 不用 OnceLock:那会把"失败"也记成"做过了"。rename 在 Windows 上会输给
/// ERROR_SHARING_VIOLATION(另一个进程正在写这个文件),而升级后第一次启动的
/// `restart_with_fresh_launcher` **故意**让新旧两个 launcher 并存,这个窗口是
/// 主动造出来的。一旦那一次失败,本进程开始写新日志 → `current.exists()` 从此为真
/// → 下次启动也不再尝试 → 旧文件里的积压永久孤儿。这段每次多两次 stat,
/// 而同一条路径上本来就有 create_dir_all + open,省不出什么。
fn migrate_legacy_diagnostic_log(legacy: &std::path::Path, current: &std::path::Path) {
    if current.exists() || !legacy.exists() {
        return;
    }
    if std::fs::rename(legacy, current).is_err() {
        // 日志没搬成就别动水位:水位指向的是日志里的字节偏移,
        // 两者分开搬会让新日志配上一个属于旧日志的偏移 —— 比不搬更糟。
        return;
    }
    let _ = std::fs::rename(watermark_sidecar(legacy), watermark_sidecar(current));
}

/// 与 diagnostics_flush.rs 的 `watermark_path` 必须一致:`<日志路径>.uploaded`。
/// 那个函数在另一个 crate 里(适配器),这里只能照着拼 —— 守卫见
/// `diagnostic_log_migration_moves_the_upload_watermark_too`。
fn watermark_sidecar(log_path: &std::path::Path) -> PathBuf {
    PathBuf::from(format!("{}.uploaded", log_path.display()))
}

pub fn default_pending_provider_import_path() -> PathBuf {
    default_app_state_dir().join(PENDING_PROVIDER_IMPORT_FILE)
}

pub fn default_pending_remote_control_recovery_path() -> PathBuf {
    default_app_state_dir().join(PENDING_REMOTE_CONTROL_RECOVERY_FILE)
}

fn settings_path_for_tests() -> Option<PathBuf> {
    SETTINGS_PATH_FOR_TESTS
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|path| path.clone())
}

static SETTINGS_PATH_FOR_TESTS: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

#[cfg(test)]
static SETTINGS_PATH_TEST_GUARD: OnceLock<Mutex<()>> = OnceLock::new();

#[cfg(test)]
pub(crate) fn settings_path_test_guard() -> std::sync::MutexGuard<'static, ()> {
    SETTINGS_PATH_TEST_GUARD
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap()
}

pub fn set_settings_path_for_tests(path: Option<PathBuf>) -> Option<PathBuf> {
    SETTINGS_PATH_FOR_TESTS
        .get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|mut current| std::mem::replace(&mut *current, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_path_uses_app_state_directory() {
        let _guard = settings_path_test_guard();
        let path = default_settings_path();

        assert!(path.ends_with(".recodex/settings.json"));
    }

    #[test]
    fn default_latest_status_path_uses_app_state_directory() {
        let path = default_latest_status_path();

        assert!(path.ends_with(".recodex/latest-status.json"));
    }

    #[test]
    fn default_diagnostic_log_path_uses_app_state_directory() {
        let path = default_diagnostic_log_path();

        assert!(path.ends_with(".recodex/recodex.log"));
    }

    #[test]
    fn default_pending_provider_import_path_uses_app_state_directory() {
        let path = default_pending_provider_import_path();

        assert!(path.ends_with(".recodex/pending-provider-import.json"));
    }

    #[test]
    fn default_pending_remote_control_recovery_path_uses_app_state_directory() {
        let path = default_pending_remote_control_recovery_path();

        assert!(path.ends_with(".recodex/pending-remote-control-recovery.json"));
    }

    #[test]
    fn app_state_migration_only_moves_when_target_is_absent() {
        let root = std::env::temp_dir().join(format!("recodex-migrate-{}", std::process::id()));
        let legacy = root.join("legacy");
        let current = root.join("current");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("settings.json"), b"{}").unwrap();

        // 新目录不存在 -> 搬迁
        assert!(try_migrate_app_state_dir(&legacy, &current).unwrap());
        assert!(current.join("settings.json").exists());
        assert!(!legacy.exists());

        // 新目录已存在 -> 不动(避免覆盖更新的数据)
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("settings.json"), b"old").unwrap();
        assert!(!try_migrate_app_state_dir(&legacy, &current).unwrap());
        assert_eq!(std::fs::read(current.join("settings.json")).unwrap(), b"{}");

        let _ = std::fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod log_migration_tests {
    use super::{migrate_legacy_diagnostic_log, watermark_sidecar};

    /// 日志改名必须**连上报水位一起搬**。
    ///
    /// 只搬日志本体的话水位归零,整个日志从头重传 —— 实测现场是 46 MB 日志、
    /// 98.9% 已传过,单机约 17 小时不间断重传,全体升级用户同时触发。
    /// 这段迁移此前零测试覆盖:把整个调用删掉,全仓 256 条测试照样全绿。
    #[test]
    fn diagnostic_log_migration_moves_the_upload_watermark_too() {
        let dir = tempfile::tempdir().expect("tempdir");
        let legacy = dir.path().join("codex-plus.log");
        let current = dir.path().join("recodex.log");
        std::fs::write(&legacy, b"line one\nline two\n").expect("write legacy log");
        std::fs::write(watermark_sidecar(&legacy), b"9").expect("write legacy watermark");

        migrate_legacy_diagnostic_log(&legacy, &current);

        assert!(!legacy.exists(), "旧日志应该已经搬走");
        assert_eq!(
            std::fs::read(&current).expect("read new log"),
            b"line one\nline two\n",
            "日志内容必须原样过来"
        );
        assert_eq!(
            std::fs::read_to_string(watermark_sidecar(&current)).expect("read new watermark"),
            "9",
            "水位没跟着搬 —— 会把已经传过的积压全量重传一遍"
        );
        assert!(
            !watermark_sidecar(&legacy).exists(),
            "旧水位应该已经搬走,留着下次还会被当成孤儿"
        );
    }

    /// 新日志已经存在就不动。搬过去会覆盖更新的数据。
    #[test]
    fn migration_does_not_clobber_an_existing_new_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let legacy = dir.path().join("codex-plus.log");
        let current = dir.path().join("recodex.log");
        std::fs::write(&legacy, b"old").expect("write legacy");
        std::fs::write(&current, b"new").expect("write current");

        migrate_legacy_diagnostic_log(&legacy, &current);

        assert_eq!(std::fs::read(&current).expect("read"), b"new", "不能覆盖新日志");
        assert!(legacy.exists(), "新的已存在时旧文件原地不动");
    }

    /// 没有旧日志时什么都不做,也不能凭空造出一个水位文件。
    #[test]
    fn migration_is_a_noop_on_a_fresh_install() {
        let dir = tempfile::tempdir().expect("tempdir");
        let legacy = dir.path().join("codex-plus.log");
        let current = dir.path().join("recodex.log");

        migrate_legacy_diagnostic_log(&legacy, &current);

        assert!(!current.exists());
        assert!(!watermark_sidecar(&current).exists());
    }

    /// 边车路径的算法必须和 diagnostics_flush.rs 的 watermark_path 一致 ——
    /// 那个函数在另一个 crate 里,只能照着拼,拼错了就是搬了个不存在的文件。
    #[test]
    fn watermark_sidecar_matches_the_flusher_naming() {
        let flusher = include_str!("../../recodex-integration/src/diagnostics_flush.rs");
        assert!(
            flusher.contains(r#"format!("{}.uploaded", log_path.display())"#),
            "上报侧的水位文件名变了 —— 这边的 watermark_sidecar 要跟着改"
        );
        assert_eq!(
            watermark_sidecar(std::path::Path::new("/tmp/recodex.log")),
            std::path::PathBuf::from("/tmp/recodex.log.uploaded")
        );
    }
}
