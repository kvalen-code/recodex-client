//! 本机信息:机器名(手机上显示「允许电脑 XXX 连接?」)与官方 Codex 命令行的位置
//! (写进运行时 settings.json 的 `codexPath`,§2.1)。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use super::layout::RemoteHome;

/// 机器名。与运行时(`os.hostname()`)、命令行(Go `os.Hostname()`)同源:
/// Windows 取 DNS 主机名(不是被截成 15 位大写的 NetBIOS 名),类 Unix 取 gethostname。
pub fn machine_name() -> String {
    let name = platform_hostname().unwrap_or_default();
    let name = name.trim();
    if name.is_empty() {
        "computer".to_string()
    } else {
        name.to_string()
    }
}

#[cfg(windows)]
fn platform_hostname() -> Option<String> {
    use windows::Win32::System::SystemInformation::{
        ComputerNamePhysicalDnsHostname, GetComputerNameExW,
    };
    use windows::core::PWSTR;
    let mut size: u32 = 0;
    // 第一次调用拿需要的长度(必然失败并回填 size)
    let _ =
        unsafe { GetComputerNameExW(ComputerNamePhysicalDnsHostname, PWSTR::null(), &mut size) };
    if size == 0 {
        return std::env::var("COMPUTERNAME").ok();
    }
    let mut buffer = vec![0u16; size as usize + 1];
    let filled = unsafe {
        GetComputerNameExW(
            ComputerNamePhysicalDnsHostname,
            PWSTR(buffer.as_mut_ptr()),
            &mut size,
        )
    };
    if filled.is_err() {
        return std::env::var("COMPUTERNAME").ok();
    }
    let len = (size as usize).min(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..len]))
}

#[cfg(unix)]
fn platform_hostname() -> Option<String> {
    let mut buffer = [0u8; 256];
    let rc = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if rc != 0 {
        return None;
    }
    let end = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
    Some(String::from_utf8_lossy(&buffer[..end]).into_owned())
}

#[cfg(not(any(windows, unix)))]
fn platform_hostname() -> Option<String> {
    None
}

/// 平台名,与后台 `platform` 字段、命令行(runtime.GOOS)一致。
pub fn platform_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        other => other,
    }
}

/// 给守护进程用的官方 Codex 命令行。只要**能执行**的那份:
/// 商店版 WindowsApps 目录里的 codex.exe 文件在、却跑不了(ACL),写进去等于埋雷,
/// 那种情况宁可不写,让运行时回落 PATH。
pub fn runnable_codex_cli() -> Option<PathBuf> {
    let path = crate::app_paths::find_codex_cli(None)?;
    if in_windows_apps(&path) || !path.is_file() {
        return None;
    }
    Some(path)
}

fn in_windows_apps(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| name.eq_ignore_ascii_case("WindowsApps"))
    })
}

/// `settings.json` 里 `codexPath` 应变成什么。`None` = 不动文件。
///
/// - 找到了能用的 CLI → 写它(已是同值就不动);
/// - 没找到:之前写过的路径还在就留着(可能是用户或命令行写的);已经不存在了就删掉,
///   让运行时回落 PATH(运行时对不存在的路径也会回落,删掉只是免得留一条误导人的旧值)。
pub fn plan_codex_path(
    existing: Option<&str>,
    found: Option<&Path>,
    exists: impl Fn(&str) -> bool,
) -> Option<Option<String>> {
    match (existing, found) {
        (Some(current), Some(found)) if current == found.to_string_lossy() => None,
        (_, Some(found)) => Some(Some(found.to_string_lossy().into_owned())),
        (Some(current), None) if !exists(current) => Some(None),
        _ => None,
    }
}

/// 把 `codexPath` 写进运行时的 settings.json,保留其它所有字段。
///
/// 与运行时的 updateSettings 走**同一把锁**(`settings.json.lock`,O_EXCL 创建,
/// 10 秒算陈旧):守护进程可能同时在改 machineId 之类的字段,不能互相覆盖。
pub fn sync_codex_path(home: &RemoteHome, found: Option<&Path>) -> anyhow::Result<bool> {
    let settings = home.settings_path();
    let existing = read_object(&settings);
    let current = existing
        .as_ref()
        .and_then(|map| map.get("codexPath"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let Some(change) = plan_codex_path(current.as_deref(), found, |p| Path::new(p).exists()) else {
        return Ok(false);
    };
    std::fs::create_dir_all(&home.root)?;
    let _lock = SettingsLock::acquire(&settings)?;
    // 拿到锁之后重读:等锁期间运行时可能刚写过
    let mut map = read_object(&settings).unwrap_or_default();
    match change {
        Some(path) => {
            map.insert("codexPath".into(), Value::String(path));
        }
        None => {
            map.remove("codexPath");
        }
    }
    let mut data = serde_json::to_vec_pretty(&Value::Object(map))?;
    data.push(b'\n');
    super::layout::write_file_replace(&settings, &data)?;
    Ok(true)
}

fn read_object(path: &Path) -> Option<Map<String, Value>> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}')) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

struct SettingsLock {
    path: PathBuf,
}

impl SettingsLock {
    fn acquire(settings: &Path) -> anyhow::Result<Self> {
        let mut name = settings.as_os_str().to_owned();
        name.push(".lock");
        let path = PathBuf::from(name);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&path)
                        .and_then(|meta| meta.modified())
                        .ok()
                        .and_then(|modified| modified.elapsed().ok())
                        .is_some_and(|age| age > Duration::from_secs(10));
                    if stale {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if Instant::now() > deadline {
                        anyhow::bail!("远程组件的设置文件被占用");
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

impl Drop for SettingsLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_path_plan() {
        let found = Path::new("/Applications/Codex.app/Contents/Resources/codex");
        let yes = |_: &str| true;
        let no = |_: &str| false;
        assert_eq!(
            plan_codex_path(None, Some(found), yes),
            Some(Some(found.to_string_lossy().into_owned()))
        );
        assert_eq!(
            plan_codex_path(Some(&found.to_string_lossy()), Some(found), yes),
            None
        );
        assert_eq!(
            plan_codex_path(Some("/old/codex"), Some(found), yes),
            Some(Some(found.to_string_lossy().into_owned()))
        );
        assert_eq!(
            plan_codex_path(Some("/old/codex"), None, yes),
            None,
            "别人写的、还在的路径不动"
        );
        assert_eq!(
            plan_codex_path(Some("/old/codex"), None, no),
            Some(None),
            "已经不存在就删"
        );
        assert_eq!(plan_codex_path(None, None, no), None);
    }

    #[test]
    fn syncing_codex_path_keeps_other_fields() {
        let temp = tempfile::tempdir().unwrap();
        let home = RemoteHome {
            root: temp.path().to_path_buf(),
            custom: true,
        };
        std::fs::write(
            home.settings_path(),
            r#"{"schemaVersion":2,"onboardingCompleted":true,"machineId":"m-1"}"#,
        )
        .unwrap();
        let cli = temp.path().join("codex.exe");
        std::fs::write(&cli, b"x").unwrap();
        assert!(sync_codex_path(&home, Some(&cli)).unwrap());
        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(home.settings_path()).unwrap()).unwrap();
        assert_eq!(value["machineId"], "m-1");
        assert_eq!(value["schemaVersion"], 2);
        assert_eq!(value["codexPath"], cli.to_string_lossy().as_ref());
        // 再来一次:同值不写
        assert!(!sync_codex_path(&home, Some(&cli)).unwrap());
        // 锁文件不残留
        assert!(!temp.path().join("settings.json.lock").exists());
        // CLI 没了、文件也没了:删掉字段
        std::fs::remove_file(&cli).unwrap();
        assert!(sync_codex_path(&home, None).unwrap());
        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(home.settings_path()).unwrap()).unwrap();
        assert!(value.get("codexPath").is_none());
        assert_eq!(value["machineId"], "m-1");
    }

    #[test]
    fn a_stale_lock_does_not_block_forever() {
        let temp = tempfile::tempdir().unwrap();
        let home = RemoteHome {
            root: temp.path().to_path_buf(),
            custom: true,
        };
        let lock = temp.path().join("settings.json.lock");
        std::fs::write(&lock, b"").unwrap();
        let old = std::time::SystemTime::now() - Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&lock)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let cli = temp.path().join("codex");
        std::fs::write(&cli, b"x").unwrap();
        assert!(sync_codex_path(&home, Some(&cli)).unwrap());
    }

    #[test]
    fn windows_apps_paths_are_not_runnable() {
        assert!(in_windows_apps(Path::new(
            r"C:\Program Files\WindowsApps\OpenAI.Codex_1\app\resources\codex.exe"
        )));
        assert!(!in_windows_apps(Path::new(
            r"C:\Users\me\AppData\Local\OpenAI\Codex\bin\abc\codex.exe"
        )));
    }

    #[test]
    fn machine_name_is_never_empty() {
        assert!(!machine_name().is_empty());
        assert!(
            ["windows", "darwin", "linux"].contains(&platform_name())
                || !platform_name().is_empty()
        );
    }
}
