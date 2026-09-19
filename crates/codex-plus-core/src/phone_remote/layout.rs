//! 远程组件的本地布局(docs/remote-app-plan.md §2.1)。桌面客户端与 `recodex app`
//! **共用**同一份运行时与数据目录 —— 这里的每条规则都与命令行
//! (cmd/recodex/app_runtime.go)逐条对齐,改一边必须改另一边。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const REMOTE_HOME_ENV: &str = "RECODEX_REMOTE_HOME";
const RUNTIME_DIR_NAME: &str = "runtime";
const CURRENT_FILE: &str = "current.json";
pub const ENTRY_REL_PATH: &str = "app/entry.mjs";

/// 远程组件的数据目录(REMOTE_HOME)。`custom` 表示来自环境变量 ——
/// 这时开机自启项也得把它带上,否则开机拉起的守护进程会去默认目录找凭据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteHome {
    pub root: PathBuf,
    pub custom: bool,
}

impl RemoteHome {
    pub fn resolve() -> Option<Self> {
        let user_home = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf());
        Self::resolve_from(
            std::env::var(REMOTE_HOME_ENV).ok().as_deref(),
            user_home.as_deref(),
        )
    }

    pub fn resolve_from(env_value: Option<&str>, user_home: Option<&Path>) -> Option<Self> {
        if let Some(raw) = env_value.map(str::trim).filter(|v| !v.is_empty()) {
            let expanded = if raw == "~" {
                user_home?.to_path_buf()
            } else if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
                user_home?.join(rest)
            } else {
                PathBuf::from(raw)
            };
            let absolute = if expanded.is_absolute() {
                expanded
            } else {
                std::env::current_dir().ok()?.join(expanded)
            };
            return Some(Self {
                root: absolute,
                custom: true,
            });
        }
        Some(Self {
            root: user_home?.join(".recodex").join("remote"),
            custom: false,
        })
    }

    pub fn runtime_root(&self) -> PathBuf {
        self.root.join(RUNTIME_DIR_NAME)
    }

    pub fn current_path(&self) -> PathBuf {
        self.runtime_root().join(CURRENT_FILE)
    }

    pub fn settings_path(&self) -> PathBuf {
        self.root.join("settings.json")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }
}

/// `runtime/current.json`:当前生效的运行时版本与其绝对目录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeCurrent {
    pub version: String,
    pub dir: String,
}

pub fn executable_name(os: &str) -> &'static str {
    if os == "windows" {
        "recodex-remote.exe"
    } else {
        "recodex-remote"
    }
}

impl RuntimeCurrent {
    pub fn dir_path(&self) -> PathBuf {
        PathBuf::from(&self.dir)
    }

    pub fn executable(&self, os: &str) -> PathBuf {
        self.dir_path().join(executable_name(os))
    }

    pub fn entry(&self) -> PathBuf {
        let mut path = self.dir_path();
        for part in ENTRY_REL_PATH.split('/') {
            path.push(part);
        }
        path
    }
}

/// 读 current.json。文件缺失、坏掉、或指向的目录里缺可执行文件/入口都当作「没装」——
/// 装一份新的就好,不该让用户去手动删文件。
pub fn read_runtime_current(home: &RemoteHome, os: &str) -> Option<RuntimeCurrent> {
    let data = std::fs::read(home.current_path()).ok()?;
    let data = data.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&data);
    let current: RuntimeCurrent = serde_json::from_slice(data).ok()?;
    parse_stable_version(&current.version)?;
    if !Path::new(&current.dir).is_absolute() {
        return None;
    }
    for path in [current.executable(os), current.entry()] {
        if !path.is_file() {
            return None;
        }
    }
    Some(current)
}

/// 原子替换 current.json:写临时文件再改名,命令行同时在读也不会读到半截。
pub fn write_runtime_current(home: &RemoteHome, current: &RuntimeCurrent) -> std::io::Result<()> {
    let root = home.runtime_root();
    std::fs::create_dir_all(&root)?;
    let mut data = serde_json::to_vec_pretty(current).map_err(std::io::Error::other)?;
    data.push(b'\n');
    write_file_replace(&home.current_path(), &data)
}

/// 写到同目录临时文件再 rename(Windows 上 std::fs::rename 会替换已有文件)。
pub fn write_file_replace(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent directory"))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.{}.tmp", random_suffix()));
    std::fs::write(&tmp, data)?;
    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    Ok(())
}

pub fn random_suffix() -> String {
    let id = uuid::Uuid::new_v4();
    id.simple().to_string()[..8].to_string()
}

/// 版本号只认 `MAJOR.MINOR.PATCH` 三段纯数字(可带 `v` 前缀,段不许有前导 0),
/// 与命令行 parseStableVersion 相同。
pub fn parse_stable_version(value: &str) -> Option<[u64; 3]> {
    let value = value.strip_prefix('v').unwrap_or(value);
    let mut out = [0u64; 3];
    let mut parts = value.split('.');
    for slot in &mut out {
        let part = parts.next()?;
        if part.is_empty()
            || (part.len() > 1 && part.starts_with('0'))
            || !part.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        *slot = part.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_home_is_under_dot_recodex() {
        let home = RemoteHome::resolve_from(None, Some(Path::new("/u/me"))).unwrap();
        assert_eq!(
            home.root,
            Path::new("/u/me").join(".recodex").join("remote")
        );
        assert!(!home.custom);
        assert_eq!(
            home.current_path(),
            Path::new("/u/me")
                .join(".recodex")
                .join("remote")
                .join("runtime")
                .join("current.json")
        );
    }

    #[test]
    fn env_override_is_custom_and_expands_tilde() {
        // 用本平台的绝对路径当家目录(Windows 上 "/u/me" 不算绝对路径)
        let user = std::env::temp_dir().join("u-me");
        let home = RemoteHome::resolve_from(Some("~/rr"), Some(&user)).unwrap();
        assert_eq!(home.root, user.join("rr"));
        assert!(home.custom);
        let absolute = user.join("elsewhere");
        let home =
            RemoteHome::resolve_from(Some(&absolute.to_string_lossy()), Some(&user)).unwrap();
        assert_eq!(home.root, absolute);
        let blank = RemoteHome::resolve_from(Some("   "), Some(Path::new("/u/me"))).unwrap();
        assert!(!blank.custom);
    }

    #[test]
    fn versions_parse_like_the_cli() {
        assert_eq!(parse_stable_version("1.2.3"), Some([1, 2, 3]));
        assert_eq!(parse_stable_version("v0.1.0"), Some([0, 1, 0]));
        for bad in ["1.2", "1.2.3.4", "01.2.3", "1.2.x", "", "1..3", "1.2.-3"] {
            assert_eq!(parse_stable_version(bad), None, "{bad}");
        }
    }

    #[test]
    fn current_json_round_trips_and_requires_the_files() {
        let temp = tempfile::tempdir().unwrap();
        let home = RemoteHome {
            root: temp.path().join("remote"),
            custom: true,
        };
        let dir = home.runtime_root().join("1.0.0");
        let current = RuntimeCurrent {
            version: "1.0.0".into(),
            dir: dir.to_string_lossy().into_owned(),
        };
        write_runtime_current(&home, &current).unwrap();
        // 文件还没放进去:当作没装
        assert_eq!(read_runtime_current(&home, "windows"), None);
        std::fs::create_dir_all(dir.join("app")).unwrap();
        std::fs::write(dir.join("recodex-remote.exe"), b"x").unwrap();
        std::fs::write(dir.join("app").join("entry.mjs"), b"x").unwrap();
        assert_eq!(
            read_runtime_current(&home, "windows"),
            Some(current.clone())
        );
        // 与命令行 json.MarshalIndent 同形:两空格缩进、结尾换行
        let text = std::fs::read_to_string(home.current_path()).unwrap();
        assert!(
            text.starts_with("{\n  \"version\": \"1.0.0\",\n  \"dir\": "),
            "{text}"
        );
        assert!(text.ends_with("}\n"));
        // 带 BOM 也认(PowerShell 写出来的)
        let mut bom = b"\xEF\xBB\xBF".to_vec();
        bom.extend_from_slice(text.as_bytes());
        std::fs::write(home.current_path(), bom).unwrap();
        assert_eq!(read_runtime_current(&home, "windows"), Some(current));
    }

    #[test]
    fn relative_dir_in_current_json_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let home = RemoteHome {
            root: temp.path().to_path_buf(),
            custom: true,
        };
        std::fs::create_dir_all(home.runtime_root()).unwrap();
        std::fs::write(
            home.current_path(),
            r#"{"version":"1.0.0","dir":"runtime/1.0.0"}"#,
        )
        .unwrap();
        assert_eq!(read_runtime_current(&home, "linux"), None);
    }
}
