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

/// `settings.json` 里 `codexPath` 应变成什么。`None` = 不动文件;`Some(None)` = 删掉该字段。
///
/// 与命令行 planCodexPath 同一张表。规则:只在「没有」或「旧值指的文件已不存在」时才写,
/// 旧值还有效就**永远不覆盖** —— 桌面客户端找的是官方 Codex 自带的 CLI,`recodex app` 找的是
/// 终端 PATH 上的 `codex`(常是 npm 装的),两边各写各的就会来回改(flapping)。谁先写谁的算:
///
/// - 旧值有效(文件还在)→ 不动,哪怕这次找到的不一样;
/// - 旧值没有或已失效、这次找到了 → 写找到的;
/// - 旧值已失效、这次也没找到 → 删掉,让运行时回落 PATH;
/// - 都没有 → 不动。
pub fn plan_codex_path(
    existing: Option<&str>,
    found: Option<&Path>,
    exists: impl Fn(&str) -> bool,
) -> Option<Option<String>> {
    let existing = existing.filter(|value| !value.is_empty());
    if existing.is_some_and(|value| exists(value)) {
        return None;
    }
    match (existing, found) {
        (_, Some(found)) => Some(Some(found.to_string_lossy().into_owned())),
        (Some(_), None) => Some(None),
        (None, None) => None,
    }
}

/// 同命令行 fileExists:存在且不是目录。
fn is_existing_file(path: &str) -> bool {
    Path::new(path).is_file()
}

/// 守护进程实际会用的 codexPath:settings.json 里有、且文件还在(同命令行 effectiveRemoteCodexPath)。
/// 开机自启项的 PATH 以它为准(其所在目录通常也有 node,npm 装的 codex 要靠它跑)。
pub fn effective_codex_path(home: &RemoteHome) -> Option<String> {
    read_object(&home.settings_path())?
        .get("codexPath")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty() && is_existing_file(path))
        .map(str::to_string)
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
    if plan_codex_path(current.as_deref(), found, is_existing_file).is_none() {
        return Ok(false);
    }
    std::fs::create_dir_all(&home.root)?;
    let _lock = SettingsLock::acquire(&settings)?;
    // 拿到锁之后重读并重新决定:等锁期间运行时(或 recodex app)可能刚写过
    let mut map = read_object(&settings).unwrap_or_default();
    let current = map.get("codexPath").and_then(Value::as_str).map(str::to_string);
    let Some(change) = plan_codex_path(current.as_deref(), found, is_existing_file) else {
        return Ok(false);
    };
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

    /// 与命令行 TestPlanCodexPath 同一张表。
    #[test]
    fn codex_path_plan_matches_the_cli() {
        let yes = |_: &str| true;
        let no = |_: &str| false;
        let bin = Path::new("/bin/codex");
        let write = |v: &str| Some(Some(v.to_string()));
        assert_eq!(plan_codex_path(None, Some(bin), yes), write("/bin/codex"));
        assert_eq!(plan_codex_path(Some("/bin/codex"), Some(bin), yes), None);
        assert_eq!(
            plan_codex_path(Some("/old/codex"), Some(bin), yes),
            None,
            "旧值还有效:不覆盖(防桌面端与 recodex app 来回改)"
        );
        assert_eq!(
            plan_codex_path(Some("/old/codex"), Some(bin), no),
            write("/bin/codex"),
            "旧值已失效:换成找到的"
        );
        assert_eq!(plan_codex_path(Some("/old/codex"), None, yes), None, "没找到但旧值还在:留着");
        assert_eq!(plan_codex_path(Some("/old/codex"), None, no), Some(None), "旧值已不存在:删掉");
        assert_eq!(plan_codex_path(None, None, no), None);
        assert_eq!(plan_codex_path(Some(""), Some(bin), yes), write("/bin/codex"));
    }

    /// 旧值有效时,即使这次找到的是另一份也不改写文件(字节不变)。
    #[test]
    fn a_valid_codex_path_written_by_the_cli_is_never_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let home = RemoteHome {
            root: temp.path().join("home"),
            custom: true,
        };
        std::fs::create_dir_all(&home.root).unwrap();
        let kept = temp.path().join("npm-codex");
        let found = temp.path().join("official-codex");
        std::fs::write(&kept, b"x").unwrap();
        std::fs::write(&found, b"x").unwrap();
        let original = format!(
            "{{\"machineId\":\"m-1\",\"codexPath\":{}}}",
            serde_json::to_string(&kept.to_string_lossy()).unwrap()
        );
        std::fs::write(home.settings_path(), &original).unwrap();
        assert!(!sync_codex_path(&home, Some(&found)).unwrap());
        assert_eq!(std::fs::read_to_string(home.settings_path()).unwrap(), original);
        assert_eq!(effective_codex_path(&home), Some(kept.to_string_lossy().into_owned()));
        // 旧的那份被卸了:换成这次找到的
        std::fs::remove_file(&kept).unwrap();
        assert_eq!(effective_codex_path(&home), None);
        assert!(sync_codex_path(&home, Some(&found)).unwrap());
        assert_eq!(effective_codex_path(&home), Some(found.to_string_lossy().into_owned()));
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
