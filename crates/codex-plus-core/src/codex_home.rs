use std::path::PathBuf;

pub fn default_codex_home_dir() -> PathBuf {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .filter(|path| codex_home_env_dir_is_valid(path))
        .unwrap_or_else(default_user_codex_home_dir)
}

/// 全仓一共有**三份** CODEX_HOME 解析,规则必须心里有数:
///
///   1. 这一份 —— 启动器用,决定读哪个 session db / relay 配置 / 模型目录;
///   2. `crates/recodex-integration/src/codexcfg.rs` —— 桌面端写托管块;
///   3. `internal/clientcfg/codexhome.go` —— CLI 写托管块。
///
/// 三份都拒绝空值与相对路径。相对路径尤其不能收:它跟着**进程的工作目录**跑,
/// 启动器从哪起来就指到哪,而 Codex 自己的工作目录又是另一个 —— 两边必然读不同
/// 的地方,且没有任何一处会报错。
///
/// 仍存在的一处已知分歧:**目录不存在时**这一份回落 `~/.codex`,另外两份照用。
/// 那是刻意的(见下面 `default_codex_home_dir_ignores_empty_or_missing_codex_home_env`):
/// 这一份全是读路径,读一个不存在的目录不如去默认位置试试;另外两份是写路径,
/// Codex 会自己把目录建出来。改任何一侧之前先回来读这段。
fn codex_home_env_dir_is_valid(path: &PathBuf) -> bool {
    !path.as_os_str().is_empty()
        && !path.to_string_lossy().trim().is_empty()
        && path.is_absolute()
        && path.is_dir()
}

fn default_user_codex_home_dir() -> PathBuf {
    directories::BaseDirs::new()
        .map(|dirs| dirs.home_dir().join(".codex"))
        .unwrap_or_else(|| PathBuf::from(".codex"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::path::Path;
    use std::sync::Mutex;

    static CODEX_HOME_ENV_LOCK: Mutex<()> = Mutex::new(());

    struct CodexHomeEnvGuard {
        previous: Option<OsString>,
    }

    impl CodexHomeEnvGuard {
        fn set(path: &Path) -> Self {
            let previous = std::env::var_os("CODEX_HOME");
            unsafe {
                std::env::set_var("CODEX_HOME", path);
            }
            Self { previous }
        }

        fn set_raw(value: &str) -> Self {
            let previous = std::env::var_os("CODEX_HOME");
            unsafe {
                std::env::set_var("CODEX_HOME", value);
            }
            Self { previous }
        }
    }

    impl Drop for CodexHomeEnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.previous {
                    Some(value) => std::env::set_var("CODEX_HOME", value),
                    None => std::env::remove_var("CODEX_HOME"),
                }
            }
        }
    }

    #[test]
    fn default_codex_home_dir_uses_existing_codex_home_env_dir() {
        let _lock = CODEX_HOME_ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let codex_home = temp.path().join("custom-codex-home");
        std::fs::create_dir_all(&codex_home).unwrap();
        let _guard = CodexHomeEnvGuard::set(&codex_home);

        assert_eq!(default_codex_home_dir(), codex_home);
        assert_eq!(crate::relay_config::default_codex_home_dir(), codex_home);
        assert_eq!(crate::codex_sqlite::default_codex_home_dir(), codex_home);
    }

    #[test]
    fn default_codex_home_dir_ignores_empty_or_missing_codex_home_env() {
        let _lock = CODEX_HOME_ENV_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing-codex-home");
        let expected = default_user_codex_home_dir();

        {
            let _guard = CodexHomeEnvGuard::set_raw("   ");
            assert_eq!(default_codex_home_dir(), expected);
            assert_eq!(crate::relay_config::default_codex_home_dir(), expected);
            assert_eq!(crate::codex_sqlite::default_codex_home_dir(), expected);
        }

        {
            let _guard = CodexHomeEnvGuard::set(&missing);
            assert_eq!(default_codex_home_dir(), expected);
            assert_eq!(crate::relay_config::default_codex_home_dir(), expected);
            assert_eq!(crate::codex_sqlite::default_codex_home_dir(), expected);
        }
    }

    /// 相对路径必须拒绝:它跟着**进程的工作目录**走,启动器从哪起来就指到哪。
    /// 这一份原先是收的,而写托管块的另外两份(Go / recodex-integration)都拒绝 ——
    /// 于是同一个 CODEX_HOME 下,我们写配置去一个地方、读会话去另一个地方,
    /// 两边都不报错。2026-09-07 那次工单就是这一类"每一步看着都对"的分裂。
    ///
    /// 用 `.` 和 `./` 而不是 `codex-data`:后者在 cwd 里本来就不存在,
    /// `is_dir()` 先把它挡掉了,`is_absolute()` 那一条压根不参与判断 —— 那样写出来
    /// 是个测不到东西的假守卫(第一版就是这么写的)。`.` 必然存在且必然是相对路径,
    /// 只有 `is_absolute()` 能拒绝它。
    #[test]
    fn default_codex_home_dir_ignores_relative_codex_home_env() {
        let _lock = CODEX_HOME_ENV_LOCK.lock().unwrap();
        let expected = default_user_codex_home_dir();
        for raw in [".", "./"] {
            let _guard = CodexHomeEnvGuard::set_raw(raw);
            assert!(
                Path::new(raw).is_dir() && !Path::new(raw).is_absolute(),
                "{raw} 必须是一个存在的相对目录,否则这条守卫测不到 is_absolute"
            );
            assert_eq!(
                default_codex_home_dir(),
                expected,
                "相对路径 {raw} 不该被采用"
            );
        }
    }
}
