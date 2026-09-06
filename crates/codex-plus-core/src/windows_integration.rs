#[cfg(windows)]
use std::ffi::{OsStr, OsString};
#[cfg(windows)]
use std::iter::once;
#[cfg(windows)]
use std::os::windows::ffi::{OsStrExt, OsStringExt};
#[cfg(windows)]
use std::path::PathBuf;
#[cfg(windows)]
use std::sync::OnceLock;

#[cfg(windows)]
use anyhow::Context;
#[cfg(windows)]
use windows::Win32::Foundation::{BOOL, CloseHandle, HANDLE, HWND, LPARAM, MAX_PATH, WPARAM};
#[cfg(windows)]
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoTaskMemFree, CoUninitialize, IPersistFile,
};
#[cfg(windows)]
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
#[cfg(windows)]
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE, REG_EXPAND_SZ, REG_SZ,
    RegCloseKey, RegCreateKeyW, RegDeleteKeyW, RegDeleteValueW, RegEnumValueW, RegOpenKeyExW,
    RegSetValueExW,
};
#[cfg(windows)]
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, QueryFullProcessImageNameW,
    TerminateProcess,
};
#[cfg(windows)]
use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow};
#[cfg(windows)]
use windows::Win32::UI::Shell::{
    ExtractIconExW, FOLDERID_Desktop, IShellLinkW, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
    ShellExecuteW, ShellLink,
};
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWMINNOACTIVE;
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GWL_EXSTYLE, GetClassNameW, GetWindowLongPtrW, GetWindowTextLengthW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, SW_RESTORE, SW_SHOW, SetForegroundWindow,
    ShowWindow, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};
#[cfg(windows)]
use windows::Win32::UI::WindowsAndMessaging::{
    HICON, ICON_BIG, ICON_SMALL, PostMessageW, SendMessageW, WM_CLOSE, WM_SETICON,
};
#[cfg(windows)]
use windows::core::{Interface, PCWSTR, PROPVARIANT, PWSTR};

#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x08000000;

#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsProcessInfo {
    pub process_id: u32,
    pub parent_process_id: u32,
    pub exe_file: String,
    pub executable_path: Option<PathBuf>,
}

#[cfg(windows)]
pub struct ComApartment;

#[cfg(windows)]
impl ComApartment {
    pub fn init() -> windows::core::Result<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        }
        Ok(Self)
    }
}

#[cfg(windows)]
impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortcutSpec {
    pub path: PathBuf,
    pub target: PathBuf,
    pub arguments: String,
    pub working_directory: Option<PathBuf>,
    pub description: String,
    pub icon: Option<PathBuf>,
    pub show_minimized: bool,
}

#[cfg(windows)]
pub fn create_shortcut(spec: &ShortcutSpec) -> anyhow::Result<()> {
    if let Some(parent) = spec.path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _com = ComApartment::init().context("初始化 COM 失败")?;
    unsafe {
        let shell_link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
            .context("创建 ShellLink COM 对象失败")?;
        shell_link
            .SetPath(PCWSTR(wide_null(spec.target.as_os_str()).as_ptr()))
            .context("设置快捷方式目标失败")?;
        shell_link
            .SetArguments(PCWSTR(wide_null(spec.arguments.as_str()).as_ptr()))
            .context("设置快捷方式参数失败")?;
        if let Some(working_directory) = &spec.working_directory {
            shell_link
                .SetWorkingDirectory(PCWSTR(wide_null(working_directory.as_os_str()).as_ptr()))
                .context("设置快捷方式工作目录失败")?;
        }
        shell_link
            .SetDescription(PCWSTR(wide_null(spec.description.as_str()).as_ptr()))
            .context("设置快捷方式描述失败")?;
        if let Some(icon) = &spec.icon {
            shell_link
                .SetIconLocation(PCWSTR(wide_null(icon.as_os_str()).as_ptr()), 0)
                .context("设置快捷方式图标失败")?;
        }
        if spec.show_minimized {
            shell_link
                .SetShowCmd(SW_SHOWMINNOACTIVE)
                .context("设置快捷方式窗口模式失败")?;
        }
        let persist_file: IPersistFile = shell_link.cast().context("获取 IPersistFile 失败")?;
        persist_file
            .Save(PCWSTR(wide_null(spec.path.as_os_str()).as_ptr()), true)
            .context("保存快捷方式失败")?;
    }
    Ok(())
}

#[cfg(windows)]
pub fn desktop_dir() -> Option<PathBuf> {
    unsafe {
        let path = SHGetKnownFolderPath(&FOLDERID_Desktop, KF_FLAG_DEFAULT, None).ok()?;
        let value = path.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(path.as_ptr().cast()));
        value
    }
}

#[cfg(windows)]
pub fn open_url(url: &str) -> anyhow::Result<()> {
    let operation = wide_null("open");
    let file = wide_null(url);
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(operation.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWMINNOACTIVE,
        )
    };
    let code = result.0 as isize;
    if code <= 32 {
        anyhow::bail!("ShellExecuteW returned {code}");
    }
    Ok(())
}

#[cfg(windows)]
pub fn set_current_user_string_value(subkey: &str, name: &str, value: &str) -> anyhow::Result<()> {
    with_created_current_user_key(subkey, |key| {
        let value = wide_null(value);
        let bytes = slice_as_u8(&value);
        unsafe {
            RegSetValueExW(
                key,
                PCWSTR(wide_null(name).as_ptr()),
                0,
                REG_SZ,
                Some(bytes),
            )
        }
        .ok()
        .with_context(|| format!("写入注册表值 {subkey}\\{name} 失败"))
    })
}

#[cfg(windows)]
pub fn delete_current_user_value(subkey: &str, name: &str) -> anyhow::Result<()> {
    let subkey = wide_null(subkey);
    let name = wide_null(name);
    let mut key = HKEY::default();
    if unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            0,
            KEY_SET_VALUE,
            &mut key,
        )
    }
    .is_err()
    {
        return Ok(());
    }
    let _guard = RegistryKeyGuard(key);
    unsafe { RegDeleteValueW(key, PCWSTR(name.as_ptr())) }
        .ok()
        .or_else(|_| Ok(()))
}

#[cfg(windows)]
pub fn read_current_user_string_values(
    subkey: &str,
) -> anyhow::Result<Vec<(String, Option<String>)>> {
    read_registry_string_values(HKEY_CURRENT_USER, subkey)
}

#[cfg(windows)]
pub fn read_local_machine_string_values(
    subkey: &str,
) -> anyhow::Result<Vec<(String, Option<String>)>> {
    read_registry_string_values(HKEY_LOCAL_MACHINE, subkey)
}

#[cfg(windows)]
fn read_registry_string_values(
    root: HKEY,
    subkey: &str,
) -> anyhow::Result<Vec<(String, Option<String>)>> {
    let subkey = wide_null(subkey);
    let mut key = HKEY::default();
    if unsafe { RegOpenKeyExW(root, PCWSTR(subkey.as_ptr()), 0, KEY_READ, &mut key) }.is_err() {
        return Ok(Vec::new());
    }
    let _guard = RegistryKeyGuard(key);
    let mut values = Vec::new();
    for index in 0.. {
        let mut name = vec![0u16; 256];
        let mut name_len = name.len() as u32;
        let mut value_type = 0u32;
        let mut data = vec![0u8; 8192];
        let mut data_len = data.len() as u32;
        let result = unsafe {
            RegEnumValueW(
                key,
                index,
                PWSTR(name.as_mut_ptr()),
                &mut name_len,
                None,
                Some(&mut value_type),
                Some(data.as_mut_ptr()),
                Some(&mut data_len),
            )
        };
        if result.is_err() {
            break;
        }
        let name = OsString::from_wide(&name[..name_len as usize])
            .to_string_lossy()
            .to_string();
        let value = if value_type == REG_SZ.0 || value_type == REG_EXPAND_SZ.0 {
            let units = unsafe {
                std::slice::from_raw_parts(
                    data.as_ptr().cast::<u16>(),
                    (data_len as usize).div_ceil(2),
                )
            };
            let len = units.iter().position(|ch| *ch == 0).unwrap_or(units.len());
            Some(
                OsString::from_wide(&units[..len])
                    .to_string_lossy()
                    .to_string(),
            )
        } else {
            None
        };
        values.push((name, value));
    }
    Ok(values)
}

#[cfg(windows)]
pub fn delete_current_user_key(subkey: &str) -> anyhow::Result<()> {
    let subkey = wide_null(subkey);
    unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(subkey.as_ptr())) }
        .ok()
        .or_else(|_| Ok(()))
}

#[cfg(windows)]
pub fn enumerate_processes() -> Vec<WindowsProcessInfo> {
    let Ok(snapshot) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
        return Vec::new();
    };
    if snapshot.is_invalid() {
        return Vec::new();
    }
    let _guard = HandleGuard(snapshot);
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut processes = Vec::new();
    if unsafe { Process32FirstW(snapshot, &mut entry) }.is_err() {
        return Vec::new();
    }
    loop {
        let process_id = entry.th32ProcessID;
        processes.push(WindowsProcessInfo {
            process_id,
            parent_process_id: entry.th32ParentProcessID,
            exe_file: nul_terminated_wide_to_string(&entry.szExeFile),
            executable_path: query_process_image_path(process_id),
        });
        if unsafe { Process32NextW(snapshot, &mut entry) }.is_err() {
            break;
        }
    }
    processes
}

/// 请求这个进程自己关掉:给它每一个顶层窗口发一条 `WM_CLOSE`。
/// 返回发出去的窗口数,0 表示没找到窗口(无头进程,只能硬杀)。
///
/// 为什么要有这一步:重启无 CDP 的 Codex 时,Windows 侧一直是直接 `TerminateProcess`
/// ——进程当场消失,没有 `beforeunload`、没有保存、没有清理。这段逻辑原先写在
/// MSIX 分支的 return 之后,所有 Windows 用户都执行不到,所以一直没人碰到;
/// 2026-09-07 把它挪到分支之前,它就对**每一个** Windows 用户生效了。
/// macOS 那边走的是 osascript quit(优雅),Windows 不该比它粗暴。
///
/// 用 `PostMessageW` 而不是 `SendMessageW`:后者要等对方消息循环处理完才返回,
/// 目标要是弹了个「确定要退出吗」的对话框,我们就跟着一起卡死。Post 只投递不等。
///
/// 不含 `visible_only` 过滤:隐藏的顶层窗口(托盘宿主之类)同样要收到关闭请求,
/// 漏掉它们的话进程不会退,白等一场超时。
#[cfg(windows)]
pub fn request_process_close(process_id: u32) -> usize {
    let mut state = CloseRequestState {
        process_id,
        hwnds: Vec::new(),
    };
    unsafe {
        let _ = EnumWindows(
            Some(collect_process_windows_proc),
            LPARAM((&mut state as *mut CloseRequestState) as isize),
        );
    }
    let mut posted = 0;
    for hwnd in state.hwnds {
        if unsafe { PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0)) }.is_ok() {
            posted += 1;
        }
    }
    posted
}

#[cfg(windows)]
struct CloseRequestState {
    process_id: u32,
    hwnds: Vec<HWND>,
}

/// 这个窗口该不该收到关闭请求。
///
/// 抽成纯函数只为一件事:让那个 pid 判断**可测**。`EnumWindows` 枚举的是
/// 桌面上**每一个**顶层窗口 —— Word、浏览器、全部;这一句是「关掉 Codex」和
/// 「关掉一切」之间唯一的东西,而它此前零覆盖(实测把它去掉,watcher 那 22 条测试
/// 全绿)。unsafe FFI 回调里一次手滑就能碰掉它,所以判断挪出来单测。
///
/// pid 为 0 一律不匹配:`GetWindowThreadProcessId` 失败时不改写出参,
/// 调用方那个初值 0 会原样留着 —— 拿它去比对等于把失败当成"命中 pid 0"。
pub(crate) fn window_belongs_to_process(window_process_id: u32, target_process_id: u32) -> bool {
    window_process_id != 0 && window_process_id == target_process_id
}

#[cfg(windows)]
unsafe extern "system" fn collect_process_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let state = unsafe { &mut *(lparam.0 as *mut CloseRequestState) };
    let mut window_process_id = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut window_process_id));
    }
    if window_belongs_to_process(window_process_id, state.process_id) {
        state.hwnds.push(hwnd);
    }
    BOOL(1)
}

#[cfg(windows)]
pub fn terminate_process(process_id: u32) -> bool {
    let Ok(handle) = (unsafe {
        OpenProcess(
            PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION,
            false,
            process_id,
        )
    }) else {
        return false;
    };
    if handle.is_invalid() {
        return false;
    }
    let _guard = HandleGuard(handle);
    unsafe { TerminateProcess(handle, 0) }.is_ok()
}

#[cfg(windows)]
pub fn activate_process_window(process_id: u32) -> bool {
    let Some(hwnd) = process_window(process_id, false) else {
        return false;
    };
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        } else if !IsWindowVisible(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
        SetForegroundWindow(hwnd).as_bool()
    }
}

/// 图标一直贴不上时,把「卡在哪一步」说清楚。
///
/// 线上 3/6 台设备报 `launcher.window_icon.apply_failed`,而那条日志只有 pid 和
/// 图标路径 —— 三台的安装路径各不相同,于是既排除不了路径,也说不出是窗口没出来
/// 还是 pid 就不对。这三种情况的修法完全不一样:
///   - 有窗口但一直不可见 → 是时序,该等更久;
///   - 进程在、名下没有任何窗口 → pid 指的不是开窗口的那个进程
///     (MSIX 打包应用的窗口常常挂在 ApplicationFrameHost 名下,这是最可疑的一条);
///   - 进程都打不开 → pid 已经失效,和 `packaged_process_wait_failed_nonfatal`
///     是同一个根因(线上确实是同一个 pid 同时报了这两条)。
#[cfg(windows)]
pub fn describe_window_lookup_failure(process_id: u32) -> &'static str {
    if process_window(process_id, false).is_some() {
        "window exists but never became visible"
    } else if query_process_image_path(process_id).is_some() {
        "process is alive but owns no window"
    } else {
        "process handle could not be opened (pid stale or access denied)"
    }
}

#[cfg(windows)]
pub fn apply_codexplusplus_icon_to_process_window(
    process_id: u32,
    icon_resource_path: PathBuf,
) -> bool {
    let Some(hwnd) = visible_window_for_process(process_id) else {
        return false;
    };
    let mut applied = false;
    if apply_window_icons(hwnd, &icon_resource_path) {
        applied = true;
    }
    if apply_taskbar_properties(hwnd, &icon_resource_path).is_ok() {
        applied = true;
    }
    applied
}

#[cfg(windows)]
fn query_process_image_path(process_id: u32) -> Option<PathBuf> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id).ok()? };
    if handle.is_invalid() {
        return None;
    }
    let _guard = HandleGuard(handle);
    let mut buffer = vec![0u16; MAX_PATH as usize * 4];
    let mut len = buffer.len() as u32;
    unsafe {
        QueryFullProcessImageNameW(
            handle,
            Default::default(),
            PWSTR(buffer.as_mut_ptr()),
            &mut len,
        )
        .ok()?;
    }
    Some(PathBuf::from(OsString::from_wide(&buffer[..len as usize])))
}

#[cfg(windows)]
fn visible_window_for_process(process_id: u32) -> Option<HWND> {
    process_window(process_id, true)
}

#[cfg(windows)]
fn process_window(process_id: u32, visible_only: bool) -> Option<HWND> {
    let mut state = ActivateWindowState {
        process_id,
        hwnd: HWND::default(),
        visible_only,
        score: ProcessWindowScore::None,
    };
    unsafe {
        let _ = EnumWindows(
            Some(find_process_window_proc),
            LPARAM((&mut state as *mut ActivateWindowState) as isize),
        );
    }
    if state.hwnd.is_invalid() {
        None
    } else {
        Some(state.hwnd)
    }
}

#[cfg(windows)]
struct ActivateWindowState {
    process_id: u32,
    hwnd: HWND,
    visible_only: bool,
    score: ProcessWindowScore,
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ProcessWindowScore {
    None,
    Fallback,
    Titled,
    AppWindow,
    TauriWindow,
}

#[cfg(windows)]
unsafe extern "system" fn find_process_window_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let state = unsafe { &mut *(lparam.0 as *mut ActivateWindowState) };
    if state.visible_only && !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return BOOL(1);
    }
    let mut window_process_id = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut window_process_id));
    }
    if window_process_id == state.process_id {
        let title_length = unsafe { GetWindowTextLengthW(hwnd) };
        let extended_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
        let mut class_name = [0u16; 256];
        let class_name_length = unsafe { GetClassNameW(hwnd, &mut class_name) }.max(0) as usize;
        let class_name = String::from_utf16_lossy(&class_name[..class_name_length]);
        let score = process_window_score(title_length > 0, extended_style, &class_name);
        if score > state.score {
            state.hwnd = hwnd;
            state.score = score;
        }
        if score == ProcessWindowScore::TauriWindow {
            return BOOL(0);
        }
    }
    BOOL(1)
}

#[cfg(windows)]
fn process_window_score(
    has_title: bool,
    extended_style: u32,
    class_name: &str,
) -> ProcessWindowScore {
    let is_app_window = extended_style & WS_EX_APPWINDOW.0 != 0;
    let is_tool_window = extended_style & WS_EX_TOOLWINDOW.0 != 0;
    if is_tool_window || is_auxiliary_window_class(class_name) {
        ProcessWindowScore::Fallback
    } else if class_name.eq_ignore_ascii_case("Tauri Window") {
        ProcessWindowScore::TauriWindow
    } else if is_app_window && !is_tool_window {
        ProcessWindowScore::AppWindow
    } else if has_title {
        ProcessWindowScore::Titled
    } else {
        ProcessWindowScore::Fallback
    }
}

#[cfg(windows)]
fn is_auxiliary_window_class(class_name: &str) -> bool {
    matches!(
        class_name.to_ascii_lowercase().as_str(),
        "ime" | "msctfime ui" | "tray_icon_app" | "tao thread event target"
    )
}

#[cfg(windows)]
fn apply_window_icons(hwnd: HWND, icon_resource_path: &PathBuf) -> bool {
    let Some((large_icon, small_icon)) = load_cached_icons(icon_resource_path) else {
        return false;
    };
    unsafe {
        SendMessageW(
            hwnd,
            WM_SETICON,
            WPARAM(ICON_BIG as usize),
            LPARAM(large_icon.0 as isize),
        );
        SendMessageW(
            hwnd,
            WM_SETICON,
            WPARAM(ICON_SMALL as usize),
            LPARAM(small_icon.0 as isize),
        );
    }
    true
}

#[cfg(windows)]
fn load_cached_icons(icon_resource_path: &PathBuf) -> Option<(HICON, HICON)> {
    static ICONS: OnceLock<(usize, usize)> = OnceLock::new();
    let icons = ICONS.get_or_init(|| {
        let path = wide_null(icon_resource_path.as_os_str());
        let mut large_icon = HICON::default();
        let mut small_icon = HICON::default();
        let loaded = unsafe {
            ExtractIconExW(
                PCWSTR(path.as_ptr()),
                0,
                Some(&mut large_icon),
                Some(&mut small_icon),
                1,
            )
        };
        if loaded == 0 {
            (0, 0)
        } else {
            (large_icon.0 as usize, small_icon.0 as usize)
        }
    });
    if icons.0 == 0 || icons.1 == 0 {
        None
    } else {
        Some((
            HICON(icons.0 as *mut core::ffi::c_void),
            HICON(icons.1 as *mut core::ffi::c_void),
        ))
    }
}

#[cfg(windows)]
fn apply_taskbar_properties(hwnd: HWND, icon_resource_path: &PathBuf) -> anyhow::Result<()> {
    use windows::Win32::Storage::EnhancedStorage::{
        PKEY_AppUserModel_ID, PKEY_AppUserModel_RelaunchCommand,
        PKEY_AppUserModel_RelaunchDisplayNameResource, PKEY_AppUserModel_RelaunchIconResource,
    };

    let store: IPropertyStore = unsafe { SHGetPropertyStoreForWindow(hwnd)? };
    let icon_resource = format!("{},0", icon_resource_path.to_string_lossy());
    let relaunch_command = std::env::current_exe()
        .ok()
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|| "codex-plus-plus.exe".to_string());
    set_property_string(
        &store,
        &PKEY_AppUserModel_ID,
        "com.bigpizzav3.codexplusplus.codex",
    )?;
    set_property_string(
        &store,
        &PKEY_AppUserModel_RelaunchIconResource,
        &icon_resource,
    )?;
    set_property_string(
        &store,
        &PKEY_AppUserModel_RelaunchDisplayNameResource,
        "ReCodex",
    )?;
    set_property_string(
        &store,
        &PKEY_AppUserModel_RelaunchCommand,
        &relaunch_command,
    )?;
    unsafe {
        store.Commit()?;
    }
    Ok(())
}

#[cfg(windows)]
fn set_property_string(
    store: &IPropertyStore,
    key: &windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY,
    value: &str,
) -> anyhow::Result<()> {
    let variant = PROPVARIANT::from(value);
    unsafe {
        store.SetValue(key, &variant)?;
    }
    Ok(())
}

#[cfg(windows)]
fn with_created_current_user_key<T>(
    subkey: &str,
    f: impl FnOnce(HKEY) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let mut key = HKEY::default();
    unsafe {
        RegCreateKeyW(
            HKEY_CURRENT_USER,
            PCWSTR(wide_null(subkey).as_ptr()),
            &mut key,
        )
    }
    .ok()
    .with_context(|| format!("打开注册表键 HKCU\\{subkey} 失败"))?;
    let _guard = RegistryKeyGuard(key);
    f(key)
}

#[cfg(windows)]
fn slice_as_u8(value: &[u16]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(value.as_ptr().cast::<u8>(), std::mem::size_of_val(value)) }
}

#[cfg(windows)]
fn wide_null(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(once(0)).collect()
}

#[cfg(windows)]
fn nul_terminated_wide_to_string(value: &[u16]) -> String {
    let len = value.iter().position(|ch| *ch == 0).unwrap_or(value.len());
    OsString::from_wide(&value[..len])
        .to_string_lossy()
        .to_string()
}

#[cfg(windows)]
struct HandleGuard(HANDLE);

#[cfg(windows)]
impl Drop for HandleGuard {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

#[cfg(windows)]
struct RegistryKeyGuard(HKEY);

#[cfg(windows)]
impl Drop for RegistryKeyGuard {
    fn drop(&mut self) {
        let _ = unsafe { RegCloseKey(self.0) };
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// 图标贴不上时那句诊断必须真的能分辨情况，否则和原来只报 pid 一样没用。
    ///
    /// 两种能在测试里造出来的极端各测一次：一个不可能存在的 pid，
    /// 和当前这个活着但没有窗口的测试进程 —— 后者正是线上最可疑的那一类
    /// (「进程在、名下没有窗口」指向窗口挂在别的进程名下)。
    #[test]
    fn window_lookup_failure_tells_a_dead_pid_from_a_windowless_one() {
        // pid 0 是系统空闲进程，OpenProcess 一定失败 —— 代表「句柄打不开」那一类。
        assert_eq!(
            describe_window_lookup_failure(0),
            "process handle could not be opened (pid stale or access denied)",
        );

        // 测试进程自己：活着，但没有任何窗口。
        assert_eq!(
            describe_window_lookup_failure(std::process::id()),
            "process is alive but owns no window",
        );
    }

    #[test]
    fn application_window_outranks_titled_ime_and_tool_windows() {
        let ime_score = process_window_score(true, 0, "IME");
        let tool_score = process_window_score(false, WS_EX_TOOLWINDOW.0, "Tao Thread Event Target");
        let app_score = process_window_score(true, WS_EX_APPWINDOW.0, "Chrome_WidgetWin_1");
        let tauri_score = process_window_score(true, 0, "Tauri Window");
        let auxiliary_app_score = process_window_score(true, WS_EX_APPWINDOW.0, "tray_icon_app");

        assert!(tauri_score > app_score);
        assert!(app_score > ime_score);
        assert_eq!(ime_score, tool_score);
        assert_eq!(auxiliary_app_score, ProcessWindowScore::Fallback);
    }
}

#[cfg(test)]
mod close_request_tests {
    use super::window_belongs_to_process;

    /// 只给目标进程的窗口发 WM_CLOSE。
    ///
    /// EnumWindows 枚举的是桌面上**所有**顶层窗口 —— 这条判断一旦失效,
    /// 「重启 Codex」就变成「关掉用户正在用的一切」。它此前没有任何测试覆盖。
    #[test]
    fn only_the_target_process_gets_the_close_request() {
        assert!(window_belongs_to_process(4321, 4321), "目标进程自己的窗口要发");
        assert!(!window_belongs_to_process(9999, 4321), "别人的窗口绝对不能发");
    }

    /// pid 0 一律不匹配。`GetWindowThreadProcessId` 失败时不改写出参,
    /// 调用点那个初值 0 会原样留着 —— 拿它去比对等于把「查询失败」当成命中。
    #[test]
    fn a_failed_pid_lookup_never_matches() {
        assert!(!window_belongs_to_process(0, 0), "查询失败不能当成命中");
        assert!(!window_belongs_to_process(0, 4321));
        assert!(!window_belongs_to_process(4321, 0));
    }
}
