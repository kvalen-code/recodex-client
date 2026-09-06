use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
#[cfg(any(windows, target_os = "macos"))]
use std::process::{Command, Stdio};
use std::time::Duration;

#[cfg(windows)]
pub use crate::windows_integration::WindowsProcessInfo;

pub const WATCHER_INTERVAL_SECONDS: f64 = 3.0;
pub const CDP_PROBE_TIMEOUT_SECONDS: f64 = 0.5;
pub const TAKEOVER_FAILURE_BACKOFF_SECONDS: f64 = 30.0;
pub const RESTART_STOP_WAIT_TIMEOUT_MS: u64 = 5_000;
const RESTART_STOP_WAIT_INTERVAL_MS: u64 = 100;
pub const WATCHER_RUN_NAME: &str = "CodexPlusPlusWatcher";
pub const WATCHER_RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub const WATCHER_STARTUP_SHORTCUT_NAME: &str = "CodexPlusPlusWatcher.lnk";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatcherInstallPlan {
    pub run_value_name: String,
    pub run_value: String,
    pub shortcut_name: String,
    pub shortcut_target: String,
    pub shortcut_arguments: String,
}

pub fn watcher_disabled_flag(root: &Path) -> PathBuf {
    root.join("watcher.disabled")
}

pub fn default_watcher_disabled_flag() -> PathBuf {
    watcher_disabled_flag(&crate::paths::default_app_state_dir())
}

pub fn enable_watcher_at(root: &Path) -> std::io::Result<()> {
    let flag = watcher_disabled_flag(root);
    if flag.exists() {
        std::fs::remove_file(flag)?;
    }
    Ok(())
}

pub fn disable_watcher_at(root: &Path) -> std::io::Result<()> {
    let flag = watcher_disabled_flag(root);
    if let Some(parent) = flag.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(flag, b"disabled")
}

pub fn enable_watcher() -> std::io::Result<()> {
    enable_watcher_at(&crate::paths::default_app_state_dir())
}

pub fn disable_watcher() -> std::io::Result<()> {
    disable_watcher_at(&crate::paths::default_app_state_dir())
}

pub fn cdp_listening(port: u16) -> bool {
    [
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
    ]
    .into_iter()
    .any(|addr| TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok())
}

pub fn build_spawn_launcher_command(launcher_path: &str, debug_port: u16) -> Vec<String> {
    vec![
        launcher_path.to_string(),
        "--debug-port".to_string(),
        debug_port.to_string(),
    ]
}

pub fn build_watcher_install_plan(launcher_path: PathBuf, debug_port: u16) -> WatcherInstallPlan {
    let launcher = launcher_path.to_string_lossy().to_string();
    let arguments = format!("--debug-port {debug_port}");
    WatcherInstallPlan {
        run_value_name: WATCHER_RUN_NAME.to_string(),
        run_value: format!("\"{launcher}\" {arguments}"),
        shortcut_name: WATCHER_STARTUP_SHORTCUT_NAME.to_string(),
        shortcut_target: launcher,
        shortcut_arguments: arguments,
    }
}

pub fn codex_process_ids<'a>(processes: impl IntoIterator<Item = (u32, &'a str)>) -> Vec<u32> {
    processes
        .into_iter()
        .filter_map(|(process_id, executable)| {
            is_windowsapps_codex_app_process(executable).then_some(process_id)
        })
        .collect()
}

fn is_windowsapps_codex_app_process(executable: &str) -> bool {
    let executable = executable.replace('/', "\\").to_ascii_lowercase();
    let Some((_, after_windows_apps)) = executable.split_once("\\windowsapps\\") else {
        return false;
    };
    let Some((package_name, after_package)) = after_windows_apps.split_once('\\') else {
        return false;
    };
    let supported_package = crate::app_paths::is_supported_windows_app_package_name(package_name)
        || package_name.starts_with("openai.chatgpt-desktop_");
    supported_package
        && after_package.starts_with("app\\")
        && !after_package.starts_with("app\\resources\\")
        && after_package
            .rsplit('\\')
            .next()
            .is_some_and(crate::app_paths::is_supported_app_executable_name)
}

/// 这个 exe 文件名是不是我们自己的启动器。
///
/// 必须连改名前的 `codex-plus-plus.exe` 一起认:自更新是「用新内容盖掉自己那个 exe」
/// (selfupdate.rs 的 stage_replacement 走 `current_exe()`),**文件名不会跟着变**。
/// 所以老用户升级到新版之后,磁盘上那个 exe 仍然叫旧名字,里面跑的是新代码 ——
/// 只认新名字的话,新代码找不到自己的残留实例,而且不会有任何报错。
fn is_launcher_exe_file(exe_file: &str) -> bool {
    [
        crate::install::SILENT_BINARY,
        crate::install::LEGACY_SILENT_BINARY,
    ]
    .iter()
    .any(|name| exe_file.eq_ignore_ascii_case(&format!("{name}.exe")))
}

pub fn filter_killable_launcher_processes<'a>(
    processes: impl IntoIterator<Item = (u32, u32, &'a str)>,
    current_process_id: u32,
) -> Vec<u32> {
    let processes = processes.into_iter().collect::<Vec<_>>();
    let parents = processes
        .iter()
        .map(|(process_id, parent_process_id, _)| (*process_id, *parent_process_id))
        .collect::<HashMap<_, _>>();
    let mut protected = HashSet::new();
    let mut cursor = current_process_id;
    while cursor != 0 && protected.insert(cursor) {
        cursor = parents.get(&cursor).copied().unwrap_or(0);
    }
    processes
        .into_iter()
        .filter(|(process_id, _, exe_file)| {
            !protected.contains(process_id) && is_launcher_exe_file(exe_file)
        })
        .map(|(process_id, _, _)| process_id)
        .collect()
}

pub fn should_recover_stale_launcher(has_codex_process: bool, cdp_listening: bool) -> bool {
    !has_codex_process && !cdp_listening
}

pub fn process_ids_still_running(
    expected: &[u32],
    running: impl IntoIterator<Item = u32>,
) -> Vec<u32> {
    let expected = expected.iter().copied().collect::<HashSet<_>>();
    running
        .into_iter()
        .filter(|process_id| expected.contains(process_id))
        .collect()
}

#[cfg(windows)]
pub fn process_id_is_running(process_id: u32) -> Option<bool> {
    if process_id == 0 {
        return Some(false);
    }
    let processes = crate::windows_integration::enumerate_processes();
    if processes.is_empty() {
        return None;
    }
    Some(
        processes
            .iter()
            .any(|process| process.process_id == process_id),
    )
}

#[cfg(target_os = "linux")]
pub fn process_id_is_running(process_id: u32) -> Option<bool> {
    if process_id == 0 {
        return Some(false);
    }
    match std::fs::metadata(Path::new("/proc").join(process_id.to_string())) {
        Ok(_) => Some(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(false),
        Err(_) => None,
    }
}

#[cfg(target_os = "macos")]
pub fn process_id_is_running(process_id: u32) -> Option<bool> {
    if process_id == 0 {
        return Some(false);
    }
    let process_id_arg = process_id.to_string();
    let output = Command::new("ps")
        .args(["-p", process_id_arg.as_str(), "-o", "pid="])
        .output()
        .ok()?;
    if !output.status.success() {
        return match output.status.code() {
            Some(1) => Some(false),
            _ => None,
        };
    }
    let process_ids = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.trim().parse::<u32>())
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    Some(process_ids.contains(&process_id))
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
pub fn process_id_is_running(_process_id: u32) -> Option<bool> {
    None
}

#[cfg(windows)]
pub fn install_watcher(launcher_path: &Path, debug_port: u16) -> anyhow::Result<()> {
    let plan = build_watcher_install_plan(launcher_path.to_path_buf(), debug_port);
    crate::windows_integration::set_current_user_string_value(
        WATCHER_RUN_KEY,
        &plan.run_value_name,
        &plan.run_value,
    )?;
    create_startup_shortcut(launcher_path, &plan.shortcut_arguments)?;
    spawn_launcher(launcher_path, debug_port);
    Ok(())
}

#[cfg(not(windows))]
pub fn install_watcher(_launcher_path: &Path, _debug_port: u16) -> anyhow::Result<()> {
    anyhow::bail!("watcher install is only supported on Windows")
}

#[cfg(windows)]
pub fn uninstall_watcher() -> anyhow::Result<()> {
    let _ =
        crate::windows_integration::delete_current_user_value(WATCHER_RUN_KEY, WATCHER_RUN_NAME);
    if let Some(shortcut) = startup_shortcut_path() {
        let _ = std::fs::remove_file(shortcut);
    }
    stop_launcher_processes();
    Ok(())
}

/// 卸载时清理**指向本 exe** 的开机自启项。
///
/// 背景:瘦身版 ReCodex 自己从不调用 `install_watcher`(那是已下线的 manager 干的),
/// 但**从 Codex++ 迁移过来的用户**,注册表 Run 里留着 `CodexPlusPlusWatcher`。
/// 我们的卸载原先完全不碰它 —— 卸完之后每次开机都会去拉一个已经被删掉的 exe。
///
/// 只在 Run 值里确实提到**我们即将删除的这个 exe** 时才动手:
/// 用户可能还单独装着 Codex++,那是别人的自启项,不该由我们越界清理。
///
/// 返回是否真的清掉了(供卸载结果里给用户一句交代)。
#[cfg(windows)]
pub fn uninstall_watcher_pointing_at(exe: &Path) -> bool {
    let Ok(values) = crate::windows_integration::read_current_user_string_values(WATCHER_RUN_KEY)
    else {
        return false;
    };
    let exe_key = exe.to_string_lossy().to_ascii_lowercase().replace('/', "\\");
    let points_at_us = values.iter().any(|(name, value)| {
        name == WATCHER_RUN_NAME
            && value.as_deref().is_some_and(|value| {
                value.to_ascii_lowercase().replace('/', "\\").contains(&exe_key)
            })
    });
    if !points_at_us {
        return false;
    }
    let _ =
        crate::windows_integration::delete_current_user_value(WATCHER_RUN_KEY, WATCHER_RUN_NAME);
    if let Some(shortcut) = startup_shortcut_path() {
        let _ = std::fs::remove_file(shortcut);
    }
    true
}

#[cfg(not(windows))]
pub fn uninstall_watcher_pointing_at(_exe: &Path) -> bool {
    false
}

#[cfg(not(windows))]
pub fn uninstall_watcher() -> anyhow::Result<()> {
    Ok(())
}

#[cfg(windows)]
pub fn find_codex_processes() -> Vec<u32> {
    let processes: Vec<_> = crate::windows_integration::enumerate_processes()
        .into_iter()
        .filter(|process| crate::app_paths::is_supported_app_executable_name(&process.exe_file))
        .collect();
    find_codex_processes_from_snapshot(&processes)
}

/// Filter the list of already enumerated Windows processes for Codex processes.
/// Exposed so the Windows-specific logic can be unit-tested without scanning the live system.
#[cfg(windows)]
pub fn find_codex_processes_from_snapshot(
    processes: &[crate::windows_integration::WindowsProcessInfo],
) -> Vec<u32> {
    let mut ids = codex_process_ids(
        processes
            .iter()
            .filter_map(|process| {
                process
                    .executable_path
                    .as_deref()
                    .map(|path| (process.process_id, path.to_string_lossy().to_string()))
            })
            .collect::<Vec<_>>()
            .iter()
            .map(|(pid, path)| (*pid, path.as_str())),
    );

    // Local/portable installs use Codex.exe as the Electron main process. Do not match
    // lowercase codex.exe here; that is commonly the CLI binary. ChatGPT.exe is accepted
    // only for packaged Store apps above, because the standalone ChatGPT app can be a
    // normal ChatGPT session rather than Codex.
    for process in processes {
        if process.exe_file == "Codex.exe" {
            ids.push(process.process_id);
        }
    }

    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Return desktop processes that can write Codex task state while a destructive
/// session-index cleanup is running. This is intentionally stricter than the
/// watcher filter: any supported ChatGPT desktop process blocks deletion,
/// including portable installs outside WindowsApps.
#[cfg(windows)]
pub fn find_session_index_cleanup_blocking_processes() -> Vec<u32> {
    find_session_index_cleanup_blocking_processes_from_snapshot(
        &crate::windows_integration::enumerate_processes(),
    )
}

#[cfg(windows)]
pub fn find_session_index_cleanup_blocking_processes_from_snapshot(
    processes: &[crate::windows_integration::WindowsProcessInfo],
) -> Vec<u32> {
    let mut ids = processes
        .iter()
        .filter(|process| process.exe_file == "Codex.exe" || process.exe_file == "ChatGPT.exe")
        .map(|process| process.process_id)
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    ids
}

#[cfg(target_os = "macos")]
pub fn find_codex_processes() -> Vec<u32> {
    let mut ids = ["Codex", "ChatGPT"]
        .into_iter()
        .flat_map(|name| {
            std::process::Command::new("pgrep")
                .args(["-x", name])
                .output()
                .ok()
                .into_iter()
                .flat_map(|output| {
                    String::from_utf8_lossy(&output.stdout)
                        .lines()
                        .map(str::to_string)
                        .collect::<Vec<_>>()
                })
        })
        .filter_map(|value| value.trim().parse::<u32>().ok())
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    ids
}

#[cfg(target_os = "macos")]
pub fn find_session_index_cleanup_blocking_processes() -> Vec<u32> {
    find_codex_processes()
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn find_codex_processes() -> Vec<u32> {
    Vec::new()
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn find_session_index_cleanup_blocking_processes() -> Vec<u32> {
    Vec::new()
}

#[cfg(windows)]
pub fn stop_launcher_processes() {
    let processes = crate::windows_integration::enumerate_processes();
    let killable = filter_killable_launcher_processes(
        processes.iter().map(|process| {
            (
                process.process_id,
                process.parent_process_id,
                process.exe_file.as_str(),
            )
        }),
        std::process::id(),
    );
    for process_id in killable {
        let _ = crate::windows_integration::terminate_process(process_id);
    }
}

#[cfg(target_os = "macos")]
pub fn stop_launcher_processes() {
    for process_id in find_launcher_processes() {
        let _ = terminate_macos_process(process_id);
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn stop_launcher_processes() {}

#[cfg(windows)]
pub fn stop_launcher_processes_and_wait() {
    let processes = crate::windows_integration::enumerate_processes();
    let killable = filter_killable_launcher_processes(
        processes.iter().map(|process| {
            (
                process.process_id,
                process.parent_process_id,
                process.exe_file.as_str(),
            )
        }),
        std::process::id(),
    );
    terminate_and_wait_for_exit(
        killable,
        RESTART_STOP_WAIT_TIMEOUT_MS,
        RESTART_STOP_WAIT_INTERVAL_MS,
    );
}

#[cfg(target_os = "macos")]
pub fn stop_launcher_processes_and_wait() {
    terminate_macos_processes_and_wait(
        find_launcher_processes(),
        || find_launcher_processes(),
        RESTART_STOP_WAIT_TIMEOUT_MS,
        RESTART_STOP_WAIT_INTERVAL_MS,
    );
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn stop_launcher_processes_and_wait() {}

#[cfg(windows)]
pub fn stop_codex_processes() {
    for process_id in find_codex_processes() {
        let _ = crate::windows_integration::terminate_process(process_id);
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn stop_codex_processes() {}

#[cfg(target_os = "macos")]
pub fn stop_codex_processes() {
    for process_id in find_codex_processes() {
        let _ = terminate_macos_process(process_id);
    }
}

#[cfg(windows)]
pub fn stop_codex_processes_and_wait() {
    terminate_and_wait_for_exit(
        find_codex_processes(),
        RESTART_STOP_WAIT_TIMEOUT_MS,
        RESTART_STOP_WAIT_INTERVAL_MS,
    );
}

#[cfg(target_os = "macos")]
pub fn stop_codex_processes_and_wait() {
    terminate_macos_processes_and_wait(
        find_codex_processes(),
        || find_codex_processes(),
        RESTART_STOP_WAIT_TIMEOUT_MS,
        RESTART_STOP_WAIT_INTERVAL_MS,
    );
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn stop_codex_processes_and_wait() {}

#[cfg(target_os = "macos")]
pub fn stop_codex_processes_for_debug_port_and_wait(debug_port: u16) {
    terminate_macos_processes_and_wait(
        find_macos_codex_processes_for_debug_port(debug_port),
        || find_macos_codex_processes_for_debug_port(debug_port),
        RESTART_STOP_WAIT_TIMEOUT_MS,
        RESTART_STOP_WAIT_INTERVAL_MS,
    );
}

#[cfg(not(target_os = "macos"))]
pub fn stop_codex_processes_for_debug_port_and_wait(_debug_port: u16) {
    stop_codex_processes_and_wait();
}

#[cfg(target_os = "macos")]
fn terminate_macos_processes_and_wait<F>(
    process_ids: Vec<u32>,
    mut find_processes: F,
    timeout_ms: u64,
    interval_ms: u64,
) where
    F: FnMut() -> Vec<u32>,
{
    if process_ids.is_empty() {
        return;
    }
    for process_id in &process_ids {
        let _ = terminate_macos_process(*process_id);
    }
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let remaining = process_ids_still_running(&process_ids, find_processes());
        if remaining.is_empty() || std::time::Instant::now() >= deadline {
            if !remaining.is_empty() {
                let _ = crate::diagnostic_log::append_diagnostic_log(
                    "watcher.stop_wait_timeout",
                    serde_json::json!({
                        "remaining_process_ids": remaining,
                        "timeout_ms": timeout_ms,
                        "platform": "macos"
                    }),
                );
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(interval_ms));
    }
}

#[cfg(target_os = "macos")]
fn terminate_macos_process(process_id: u32) -> std::io::Result<()> {
    Command::new("kill")
        .arg(process_id.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|_| ())
}

/// macOS 上启动器可能以三个名字出现在进程表里。
///
/// 直接跑二进制时是 SILENT_BINARY;从 .app 启动时,`Contents/MacOS/` 下那个
/// 启动脚本的名字来自 Info.plist 的 CFBundleExecutable,是另一个字符串。
/// `pgrep -x` 精确匹配可执行名,只查前者就漏掉后者 —— 而用户几乎都是点图标启动的。
///
/// 第三个是改名前的旧二进制名:自更新只换内容不换文件名,所以老安装升上来之后
/// 磁盘上仍然是旧名字,里面跑的却是新代码。不认它就等于新代码看不见自己。
#[cfg(target_os = "macos")]
pub fn macos_launcher_process_names() -> [&'static str; 3] {
    [
        crate::install::SILENT_BINARY,
        crate::install::LEGACY_SILENT_BINARY,
        crate::install::MACOS_SILENT_EXECUTABLE,
    ]
}

#[cfg(target_os = "macos")]
fn find_launcher_processes() -> Vec<u32> {
    // 漏掉 .app 那个名字的后果是**看不见还活着的旧实例**:于是新实例以为端口没人用,
    // 去绑已被占住的 helper 端口,退化成 helper.port_fallback(线上 4 台设备见过)。
    // 排掉自己的 pid —— 否则当前进程会把自己当成"旧实例"。
    let current_process_id = std::process::id();
    macos_launcher_process_names()
        .into_iter()
        .flat_map(|process_name| {
            std::process::Command::new("pgrep")
                .args(["-x", process_name])
                .output()
                .ok()
                .into_iter()
                .flat_map(|output| {
                    String::from_utf8_lossy(&output.stdout)
                        .lines()
                        .filter_map(|value| value.trim().parse::<u32>().ok())
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        })
        .filter(|pid| *pid != current_process_id)
        .collect()
}

#[cfg(target_os = "macos")]
fn find_macos_codex_processes_for_debug_port(debug_port: u16) -> Vec<u32> {
    let Ok(output) = std::process::Command::new("ps")
        .args(["-axo", "pid=,args="])
        .output()
    else {
        return Vec::new();
    };
    macos_codex_process_ids_for_debug_port(
        String::from_utf8_lossy(&output.stdout).lines(),
        debug_port,
    )
}

#[cfg(target_os = "macos")]
fn macos_codex_process_ids_for_debug_port<'a>(
    process_lines: impl IntoIterator<Item = &'a str>,
    debug_port: u16,
) -> Vec<u32> {
    let debug_flag = format!("remote-debugging-port={debug_port}");
    let mut ids = process_lines
        .into_iter()
        .filter_map(|line| {
            let trimmed = line.trim_start();
            let (pid, args) = trimmed.split_once(char::is_whitespace)?;
            let process_id = pid.parse::<u32>().ok()?;
            let is_desktop_main = (args.contains(".app/Contents/MacOS/ChatGPT")
                || args.contains(".app/Contents/MacOS/Codex"))
                && !args.contains("/Helpers/");
            (is_desktop_main && args.contains(&debug_flag)).then_some(process_id)
        })
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// 先礼后兵:等优雅关闭的时间。超过就硬杀,别把用户晾在那儿。
///
/// 3 秒是按「Electron 应用收到 WM_CLOSE 后跑完 beforeunload 并退出」估的。
/// 剩下的预算留给硬杀之后的等待,所以两段加起来仍在 RESTART_STOP_WAIT_TIMEOUT_MS
/// 的量级上,不会让"重启 Codex"这件事明显变慢。
#[cfg(windows)]
const GRACEFUL_CLOSE_WAIT_MS: u64 = 3_000;

/// 等这批进程退出,返回仍然活着的。到点就返回,不保证空。
#[cfg(windows)]
fn wait_for_process_exit(process_ids: &[u32], timeout_ms: u64, interval_ms: u64) -> Vec<u32> {
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let running_process_ids = crate::windows_integration::enumerate_processes()
            .into_iter()
            .map(|process| process.process_id);
        let remaining = process_ids_still_running(process_ids, running_process_ids);
        if remaining.is_empty() || std::time::Instant::now() >= deadline {
            return remaining;
        }
        std::thread::sleep(Duration::from_millis(interval_ms));
    }
}

/// 停掉这批进程:**先请求它们自己关**,只对赖着不走的用 TerminateProcess。
///
/// 从前这里是直接 TerminateProcess ——进程当场消失,没有 beforeunload、没有保存。
/// 一直没人碰到是因为唯一的调用点写在 MSIX 分支的 return 之后,所有 Windows
/// 用户都执行不到(线上诊断 0 次 0 设备)。2026-09-07 把它挪到分支之前之后,
/// 它对**每一个** Windows 用户生效了 —— 只要 Codex 在跑且当前调试端口上没有 CDP,
/// 从面板启动就会走到这里。同样的场景 macOS 走的是 osascript quit(优雅),
/// Windows 没理由更粗暴。
#[cfg(windows)]
fn terminate_and_wait_for_exit(process_ids: Vec<u32>, timeout_ms: u64, interval_ms: u64) {
    if process_ids.is_empty() {
        return;
    }

    // 第一步:请求自己关。没有任何窗口可发(无头进程)就没必要白等,直接进第二步。
    let posted: usize = process_ids
        .iter()
        .map(|process_id| crate::windows_integration::request_process_close(*process_id))
        .sum();
    let mut remaining = if posted > 0 {
        wait_for_process_exit(
            &process_ids,
            GRACEFUL_CLOSE_WAIT_MS.min(timeout_ms),
            interval_ms,
        )
    } else {
        process_ids.clone()
    };
    if remaining.is_empty() {
        let _ = crate::diagnostic_log::append_diagnostic_log(
            "watcher.graceful_close_succeeded",
            serde_json::json!({ "process_ids": process_ids, "windows_asked": posted }),
        );
        return;
    }

    // 第二步:赖着不走的才硬杀。
    // 名字用 `timeout` 不是 `timed_out`:上报规则按**子串**认关键词
    // (diagnostics_flush.rs 的 ERROR_MARKERS 里是 `timeout`),`timed_out` 中间那个
    // 下划线让它一个词都不命中 —— 这条会被静默丢弃,而它是"优雅关闭没生效"的
    // 唯一信号。同一个函数里的 watcher.stop_wait_timeout 就是对的写法。
    let _ = crate::diagnostic_log::append_diagnostic_log(
        "watcher.graceful_close_timeout",
        serde_json::json!({
            "remaining_process_ids": remaining,
            "windows_asked": posted,
            "waited_ms": if posted > 0 { GRACEFUL_CLOSE_WAIT_MS.min(timeout_ms) } else { 0 },
        }),
    );
    for process_id in &remaining {
        let _ = crate::windows_integration::terminate_process(*process_id);
    }
    // 用**剩余**预算,不是完整的 timeout_ms —— 否则两段加起来最坏 3+5=8 秒,
    // 而这段是 std::thread::sleep,从 async 的 launch_codex 里直接调,
    // 阻塞的是 tokio 的工作线程。整段总时长必须守住 timeout_ms。
    let graceful_spent = if posted > 0 {
        GRACEFUL_CLOSE_WAIT_MS.min(timeout_ms)
    } else {
        0
    };
    remaining = wait_for_process_exit(
        &remaining,
        timeout_ms.saturating_sub(graceful_spent),
        interval_ms,
    );
    if !remaining.is_empty() {
        let _ = crate::diagnostic_log::append_diagnostic_log(
            "watcher.stop_wait_timeout",
            serde_json::json!({
                "remaining_process_ids": remaining,
                "timeout_ms": timeout_ms
            }),
        );
    }
}

#[cfg(windows)]
fn create_startup_shortcut(launcher_path: &Path, arguments: &str) -> anyhow::Result<()> {
    let Some(shortcut_path) = startup_shortcut_path() else {
        anyhow::bail!("无法定位 Windows 启动目录")
    };
    crate::windows_integration::create_shortcut(&crate::windows_integration::ShortcutSpec {
        path: shortcut_path,
        target: launcher_path.to_path_buf(),
        arguments: arguments.to_string(),
        working_directory: launcher_path.parent().map(Path::to_path_buf),
        description: "ReCodex watcher".to_string(),
        icon: None,
        show_minimized: true,
    })
}

#[cfg(windows)]
fn spawn_launcher(launcher_path: &Path, debug_port: u16) {
    let command = build_spawn_launcher_command(&launcher_path.to_string_lossy(), debug_port);
    if let Some((exe, args)) = command.split_first() {
        let mut command = Command::new(exe);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        use std::os::windows::process::CommandExt;
        command.creation_flags(crate::windows_integration::CREATE_NO_WINDOW);
        let _ = command.spawn();
    }
}

#[cfg(windows)]
fn startup_shortcut_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|appdata| {
        PathBuf::from(appdata)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("Startup")
            .join(WATCHER_STARTUP_SHORTCUT_NAME)
    })
}

// recodex-overlay: 关掉当前 Codex,并拉起一个全新的 launcher 进程接管。
//
// 为什么要整个 launcher 重启,而不是只重启 Codex:
//   1) `config.toml` 与 `RECODEX_KEY` 都是 Codex **进程启动时读一次**,切换官方模式后
//      必须让 Codex 重新起来才生效;
//   2) 当前 launcher 正阻塞在 `wait_for_codex_exit()`,Codex 一退它自己也会退出,
//      所以必须先把接班人拉起来;
//   3) Windows 上 launcher 遇到"已在运行的 Codex"只会激活、不会重新注入
//      (macOS 有 RestartRunningApp 分支,Windows 没有)—— 先杀干净再交接,
//      正好绕开这个老坑。
//
// 调用方在本函数返回 Ok 后应尽快退出当前进程。
#[cfg(windows)]
pub fn restart_with_fresh_launcher() -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const DETACHED_PROCESS: u32 = 0x0000_0008;

    let exe = std::env::current_exe()?;
    for process_id in find_codex_processes() {
        let _ = crate::windows_integration::terminate_process(process_id);
    }
    // 给 Codex 一点时间真正退出,否则接班的 launcher 会看到"已在运行"
    std::thread::sleep(std::time::Duration::from_millis(1500));
    // 注:这里 DETACHED_PROCESS 是安全的 —— launcher 是 GUI 子系统程序,不需要控制台。
    // 但**不要把同样的标志用在 cmd 上**:cmd 拿到 DETACHED_PROCESS 会直接退出
    // (卸载的自删脚本就栽在这上面,见 uninstall.rs 的注释)。
    //
    // --await-guard:告诉接班的 launcher「旧实例正在退出,请等锁释放再判断」。
    // 否则它会看到锁还被占,以为已有实例在跑,走「激活已存在」分支后立刻退出 ——
    // 结果页面里的 CDP binding 没人应答,面板永远停在「加载中」。
    std::process::Command::new(exe)
        .arg("--await-guard")
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .spawn()?;
    Ok(())
}

#[cfg(not(windows))]
pub fn restart_with_fresh_launcher() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    for process_id in find_codex_processes() {
        let _ = terminate_macos_process(process_id);
    }
    std::thread::sleep(std::time::Duration::from_millis(1500));
    std::process::Command::new(exe).arg("--await-guard").spawn()?;
    Ok(())
}

// recodex-overlay: 卸载专用的收尾 —— 杀掉 Codex 但**绝不接班**。
//
// 这里刻意不复用 `restart_with_fresh_launcher()`。卸载时它会要命:
// 自删脚本靠「反复重试删 exe」等待映像锁释放,而接班的 launcher 会**立刻把同一个
// exe 重新锁上**,清理脚本重试到超时放弃 —— 用户点了卸载,配置还了、设备吊销了、
// 快捷方式删了,程序却还在跑、exe 还躺在磁盘上。
//
// 调用方在本函数返回后应尽快退出当前进程,让映像锁释放。
pub fn shutdown_for_uninstall() {
    for process_id in find_codex_processes() {
        #[cfg(windows)]
        let _ = crate::windows_integration::terminate_process(process_id);
        #[cfg(not(windows))]
        let _ = terminate_macos_process(process_id);
    }
}
