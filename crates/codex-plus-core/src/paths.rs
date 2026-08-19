use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

// recodex-overlay: 数据目录去品牌 `.codex-session-delete` → `.recodex`。
// 直接改名会让已有用户的设置/日志「凭空消失」,所以首次访问时把旧目录整个搬过来。
const APP_STATE_DIR: &str = ".recodex";
const LEGACY_APP_STATE_DIR: &str = ".codex-session-delete";
const SETTINGS_FILE: &str = "settings.json";
const LATEST_STATUS_FILE: &str = "latest-status.json";
const DIAGNOSTIC_LOG_FILE: &str = "codex-plus.log";
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
    default_app_state_dir().join(DIAGNOSTIC_LOG_FILE)
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

        assert!(path.ends_with(".recodex/codex-plus.log"));
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
