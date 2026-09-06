use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

pub mod macos;
pub mod windows;

pub const SILENT_NAME: &str = "ReCodex";
// manager 已弃用(方案A),保留常量仅为兼容旧快捷方式清理
pub const MANAGER_NAME: &str = "ReCodex";
/// 出货二进制的文件名。
///
/// 用户看得见它:Windows 上装在 `%LOCALAPPDATA%\Programs\ReCodex\` 下,
/// 任务管理器里也是这个名字。原先叫 `codex-plus-plus`,一眼就能看出上游是谁。
pub const SILENT_BINARY: &str = "recodex";

/// 改名之前的二进制名。**匹配自己的进程时必须连它一起认**。
///
/// 自更新是「用新内容盖掉自己那个 exe」(selfupdate.rs 的 stage_replacement 走
/// `current_exe()`),文件名不会跟着变 —— 也就是说**老用户升级到新版之后,
/// 磁盘上那个 exe 仍然叫 codex-plus-plus.exe**,而里面跑的是新代码。
/// 只认新名字的话,新代码会找不到自己的进程:清理不了残留实例、重启逻辑失灵,
/// 而且不会有任何报错。
///
/// 只有重新跑一遍安装包才会变成新名字(安装包会把旧的那个删掉)。
/// 在还有老安装存活之前,这个常量不能删。
pub const LEGACY_SILENT_BINARY: &str = "codex-plus-plus";
/// macOS 上**出货包**里 `Contents/MacOS/` 下那个可执行文件名。
///
/// 取自 scripts/installer/macos/package-recodex-dmg.sh:它把二进制直接
/// `cp "$BINARY" "$APP_DIR/Contents/MacOS/ReCodex"`,所以点图标启动后
/// `pgrep -x` 看到的进程名是 **ReCodex**,不是二进制名 codex-plus-plus。
///
/// 注意还有第二条布局:应用内安装器(install/macos.rs)在同一位置写的是一个
/// **启动脚本**,它 `exec` 真二进制 —— exec 会替换进程映像,进程名随即变回
/// SILENT_BINARY。那条路径旧代码本来就覆盖得到,不需要新名字。
pub const MACOS_SILENT_EXECUTABLE: &str = SILENT_NAME;
/// 同上,manager 那个包里的可执行文件名。manager 已弃用不出货,
/// 留着只为清理旧安装 —— 但它的名字照样会被编进二进制,所以一并去品牌。
///
/// **不能直接用 MANAGER_NAME**:它和 SILENT_NAME 都是 "ReCodex",两个包的
/// 可执行文件就会重名,`macos_companion_binary_from_exe`(从一个包去找另一个包)
/// 立刻退化成"找到自己"。改名时踩到过一次,由 installers.rs 那两条
/// companion_binary_path_resolves_* 抓出来的。
pub const MACOS_MANAGER_EXECUTABLE: &str = "ReCodexManager";
pub const MANAGER_BINARY: &str = "recodex-manager";
pub const SILENT_BUNDLE_ID: &str = "com.recodex.app";
pub const MANAGER_BUNDLE_ID: &str = "com.recodex.app.manager";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct InstallOptions {
    #[serde(default)]
    pub install_root: Option<PathBuf>,
    #[serde(default)]
    pub launcher_path: Option<PathBuf>,
    #[serde(default)]
    pub manager_path: Option<PathBuf>,
    #[serde(default)]
    pub remove_owned_data: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShortcutState {
    pub installed: bool,
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EntryPointState {
    pub silent_shortcut: ShortcutState,
    pub management_shortcut: ShortcutState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstallActionResult {
    pub status: String,
    pub message: String,
    pub silent_shortcut: ShortcutState,
    pub management_shortcut: ShortcutState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacosAppBundle {
    pub app_path: PathBuf,
    pub info_plist: String,
    pub launch_script: String,
    pub binary_source: Option<PathBuf>,
    pub binary_target_name: Option<String>,
}

impl ShortcutState {
    pub fn missing(path: Option<PathBuf>) -> Self {
        Self {
            installed: false,
            path: path.map(|path| path.to_string_lossy().to_string()),
        }
    }

    pub fn from_candidates(candidates: Vec<PathBuf>) -> Self {
        if let Some(path) = candidates.iter().find(|path| path.exists()) {
            return Self {
                installed: true,
                path: Some(path.to_string_lossy().to_string()),
            };
        }
        Self::missing(candidates.into_iter().next())
    }
}

pub fn shortcut_names() -> (&'static str, &'static str) {
    ("ReCodex.lnk", "ReCodex.lnk")
}

pub fn app_bundle_names() -> (&'static str, &'static str) {
    ("ReCodex.app", "ReCodex.app")
}

pub fn inspect_entrypoints() -> EntryPointState {
    let root = default_install_root();
    EntryPointState {
        silent_shortcut: ShortcutState::from_candidates(entrypoint_candidates(&root, false)),
        management_shortcut: ShortcutState::from_candidates(entrypoint_candidates(&root, true)),
    }
}

pub fn install_entrypoints(options: &InstallOptions) -> InstallActionResult {
    let result = platform_install(options);
    action_result(result, "入口已安装。")
}

pub fn uninstall_entrypoints(options: &InstallOptions) -> InstallActionResult {
    let result = platform_uninstall(options);
    if result.is_ok() && options.remove_owned_data {
        let _ = remove_owned_data();
    }
    action_result(result, "入口已卸载。")
}

pub fn repair_entrypoints(options: &InstallOptions) -> InstallActionResult {
    let result = platform_install(options);
    action_result(result, "入口已修复。")
}

pub fn build_windows_entrypoint_plan(options: &InstallOptions) -> windows::WindowsEntrypointPlan {
    windows::build_windows_entrypoint_plan(options)
}

pub fn build_macos_app_bundle(options: &InstallOptions, manager: bool) -> MacosAppBundle {
    macos::build_app_bundle(options, manager)
}

pub fn remove_owned_data() -> std::io::Result<()> {
    let dir = crate::paths::default_app_state_dir();
    if dir.exists() {
        std::fs::remove_dir_all(dir)?;
    }
    Ok(())
}

pub fn default_install_root() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        return crate::windows_integration::desktop_dir().or_else(|| {
            directories::UserDirs::new().and_then(|dirs| dirs.desktop_dir().map(PathBuf::from))
        });
    }

    #[cfg(target_os = "macos")]
    {
        let sys_apps = PathBuf::from("/Applications");
        if sys_apps.join(format!("{SILENT_NAME}.app")).exists()
            || sys_apps.join(format!("{MANAGER_NAME}.app")).exists()
        {
            return Some(sys_apps);
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = macos_applications_dir_from_exe(&exe) {
                if is_macos_applications_dir(&dir) {
                    return Some(dir);
                }
            }
        }
        return Some(sys_apps);
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    {
        directories::UserDirs::new().and_then(|dirs| dirs.desktop_dir().map(PathBuf::from))
    }
}

pub fn default_install_root_strategy() -> &'static str {
    if cfg!(windows) {
        "windows-known-folder"
    } else if cfg!(target_os = "macos") {
        "macos-applications"
    } else {
        "user-dirs-desktop"
    }
}

fn platform_install(options: &InstallOptions) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        windows::install_shortcuts(options)
    }

    #[cfg(target_os = "macos")]
    {
        macos::install_app_bundles(options)
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = options;
        anyhow::bail!("当前平台暂不支持安装 ReCodex 入口")
    }
}

fn platform_uninstall(options: &InstallOptions) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        windows::uninstall_shortcuts(options)
    }

    #[cfg(target_os = "macos")]
    {
        macos::uninstall_app_bundles(options)
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = options;
        anyhow::bail!("当前平台暂不支持卸载 ReCodex 入口")
    }
}

fn action_result(result: anyhow::Result<()>, success_message: &str) -> InstallActionResult {
    let state = inspect_entrypoints();
    match result {
        Ok(()) => InstallActionResult {
            status: "ok".to_string(),
            message: success_message.to_string(),
            silent_shortcut: state.silent_shortcut,
            management_shortcut: state.management_shortcut,
        },
        Err(error) => InstallActionResult {
            status: "failed".to_string(),
            message: error.to_string(),
            silent_shortcut: state.silent_shortcut,
            management_shortcut: state.management_shortcut,
        },
    }
}

fn entrypoint_candidates(root: &Option<PathBuf>, manager: bool) -> Vec<PathBuf> {
    let Some(root) = root else {
        return Vec::new();
    };
    let name = if manager { MANAGER_NAME } else { SILENT_NAME };
    if cfg!(windows) {
        vec![root.join(format!("{name}.lnk"))]
    } else if cfg!(target_os = "macos") {
        vec![root.join(format!("{name}.app"))]
    } else {
        vec![root.join(format!("{name}.desktop"))]
    }
}

pub fn option_or_current_exe(value: &Option<PathBuf>, binary: &str) -> PathBuf {
    if let Some(value) = value {
        return value.clone();
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
    companion_binary_path_from_exe(&exe, binary)
}

pub fn companion_binary_path(binary: &str) -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
    companion_binary_path_from_exe(&exe, binary)
}

pub fn spawn_companion<I, S>(binary: &str, args: I) -> anyhow::Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args = args
        .into_iter()
        .map(|arg| arg.as_ref().to_os_string())
        .collect::<Vec<OsString>>();

    #[cfg(target_os = "macos")]
    {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
        if let Some(bundle_id) = macos_companion_bundle_identifier_from_exe(&exe, binary) {
            let launch_result = Command::new("/usr/bin/open")
                .args(["-n", "-b", bundle_id, "--args"])
                .args(&args)
                .status();
            if launch_result.as_ref().is_ok_and(|status| status.success()) {
                return Ok(format!("bundle:{bundle_id}"));
            }
            let fallback = companion_binary_path_from_exe(&exe, binary);
            if !fallback.exists() {
                let detail = launch_result
                    .map(|status| status.to_string())
                    .unwrap_or_else(|error| error.to_string());
                anyhow::bail!("macOS Launch Services 无法启动 bundle {bundle_id}：{detail}");
            }
        }
    }

    let path = companion_binary_path(binary);
    let mut command = Command::new(&path);
    command.args(&args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(crate::windows_create_no_window());
    }
    command
        .spawn()
        .map_err(|error| anyhow::anyhow!("无法启动 {}：{error}", path.to_string_lossy()))?;
    Ok(path.to_string_lossy().to_string())
}

pub fn macos_companion_bundle_identifier_from_exe(
    exe: &Path,
    binary: &str,
) -> Option<&'static str> {
    let (_, app_name) = macos_applications_dir_and_app_name_from_exe(exe)?;
    let known_bundle =
        app_name == format!("{SILENT_NAME}.app") || app_name == format!("{MANAGER_NAME}.app");
    if !known_bundle {
        return None;
    }
    match binary {
        SILENT_BINARY => Some(SILENT_BUNDLE_ID),
        MANAGER_BINARY => Some(MANAGER_BUNDLE_ID),
        _ => None,
    }
}

pub fn companion_binary_path_from_exe(exe: &Path, binary: &str) -> PathBuf {
    let dir = exe.parent().unwrap_or_else(|| Path::new("."));
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    if let Some(bundle_binary) = macos_companion_binary_from_exe(exe, binary) {
        return bundle_binary;
    }
    let same_bundle = dir.join(binary);
    if same_bundle.exists() {
        return same_bundle;
    }
    dir.join(format!("{binary}{suffix}"))
}

fn macos_companion_binary_from_exe(exe: &Path, binary: &str) -> Option<PathBuf> {
    let (applications_dir, app_name) = macos_applications_dir_and_app_name_from_exe(exe)?;
    if binary == SILENT_BINARY {
        if app_name == format!("{SILENT_NAME}.app") {
            return Some(macos_preferred_bundle_binary(
                exe,
                SILENT_BINARY,
                MACOS_SILENT_EXECUTABLE,
            ));
        }
        let macos = applications_dir
            .join(format!("{SILENT_NAME}.app"))
            .join("Contents")
            .join("MacOS");
        return Some(
            macos
                .join(SILENT_BINARY)
                .exists()
                .then(|| macos.join(SILENT_BINARY))
                .unwrap_or_else(|| macos.join(MACOS_SILENT_EXECUTABLE)),
        );
    }
    if binary == MANAGER_BINARY {
        if app_name == format!("{MANAGER_NAME}.app") {
            return Some(macos_preferred_bundle_binary(
                exe,
                MANAGER_BINARY,
                MACOS_MANAGER_EXECUTABLE,
            ));
        }
        let macos = applications_dir
            .join(format!("{MANAGER_NAME}.app"))
            .join("Contents")
            .join("MacOS");
        return Some(
            macos
                .join(MANAGER_BINARY)
                .exists()
                .then(|| macos.join(MANAGER_BINARY))
                .unwrap_or_else(|| macos.join(MACOS_MANAGER_EXECUTABLE)),
        );
    }
    None
}

fn macos_preferred_bundle_binary(
    exe: &Path,
    sidecar_name: &str,
    bundle_executable_name: &str,
) -> PathBuf {
    let macos = exe.parent().unwrap_or_else(|| Path::new("."));
    let sidecar = macos.join(sidecar_name);
    if sidecar.exists() {
        return sidecar;
    }
    let bundle_executable = macos.join(bundle_executable_name);
    if bundle_executable.exists() {
        return bundle_executable;
    }
    exe.to_path_buf()
}

#[cfg(target_os = "macos")]
fn macos_applications_dir_from_exe(exe: &Path) -> Option<PathBuf> {
    macos_applications_dir_and_app_name_from_exe(exe).map(|(dir, _)| dir)
}

fn macos_applications_dir_and_app_name_from_exe(exe: &Path) -> Option<(PathBuf, String)> {
    let mut path = exe;
    while let Some(parent) = path.parent() {
        if path.extension().and_then(|extension| extension.to_str()) == Some("app") {
            let app_name = path.file_name()?.to_string_lossy().to_string();
            return Some((parent.to_path_buf(), app_name));
        }
        path = parent;
    }
    None
}

#[cfg(target_os = "macos")]
fn is_macos_applications_dir(path: &Path) -> bool {
    if path == Path::new("/Applications") {
        return true;
    }
    directories::BaseDirs::new()
        .map(|dirs| path == dirs.home_dir().join("Applications"))
        .unwrap_or(false)
}

pub(crate) fn install_root_or_default(options: &InstallOptions) -> PathBuf {
    options
        .install_root
        .clone()
        .or_else(default_install_root)
        .unwrap_or_else(|| PathBuf::from("."))
}
