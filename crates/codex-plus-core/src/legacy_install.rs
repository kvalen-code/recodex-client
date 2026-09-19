//! recodex-overlay: 改名前的安装(`codex-plus-plus.exe` 等)的一次性迁移与残留清理。
//!
//! 背景:自更新只替换 `current_exe()` 的**内容**,文件名不变(selfupdate.rs)。
//! 于是 1.3.4 之前装的机器,一路自更新上来,磁盘上跑的仍是
//! `%LOCALAPPDATA%\Programs\ReCodex\codex-plus-plus.exe` —— 任务管理器里的进程名、
//! 桌面/开始菜单/任务栏快捷方式、卸载项的图标都还指着它。只有重跑一遍安装包才会变。
//!
//! 这里在启动时替用户做安装包会做的那几件事,**任何一步失败都只记日志、照常运行**:
//!
//! 1. 以旧名启动时([`handoff_from_legacy_binary`]):把自己复制成同目录的 `recodex.exe`,
//!    改写指向旧 exe 的快捷方式与注册表项,然后从新名重新拉起、本进程退出;
//! 2. 以新名启动时([`spawn_startup_housekeeping`],后台线程):同目录里还躺着旧 exe 就
//!    再改写一遍引用(幂等)并删掉它;任务栏固定项里的旧 AppUserModelID 换成新的;
//!    卸载项的 DisplayVersion 跟上当前版本;清理确认已迁移完的旧数据残留。
//!
//! 旧名识别(`LEGACY_SILENT_BINARY`、watcher 里的进程匹配)一律保留 —— 迁移失败的
//! 机器还会以旧名跑下去,新代码必须认得自己。
//!
//! 纯逻辑(路径判断、快捷方式改写决策、注册表字符串替换、目录是否已被取代)放在
//! cfg 门控之外,任何平台都编译、都有单测;cfg(windows) 里只留直白的 fs / COM / 注册表调用。

use std::path::{Path, PathBuf};

use serde_json::json;

use crate::install::{LEGACY_SILENT_BINARY, SILENT_BINARY};

/// 我们给 Codex 窗口打的 AppUserModelID(任务栏按它分组、「固定到任务栏」的快捷方式
/// 里也记着它)。原先是上游的 `com.bigpizzav3.codexplusplus.codex`。
///
/// 只改窗口上的 ID 的话,用户已固定的任务栏图标与运行中的窗口会分成两组 ——
/// 所以启动时会把**指向我们 exe 的**固定项里的旧 ID 一并改成新的。
pub const CODEX_WINDOW_APP_USER_MODEL_ID: &str = "com.recodex.desktop.codex";
/// 改名前的 AppUserModelID。只用于识别并改写老的固定项,**不再写到窗口上**。
/// 上游 Codex++ 用的也是它 —— 所以改写只针对目标是我们 exe 的快捷方式。
pub const LEGACY_CODEX_WINDOW_APP_USER_MODEL_ID: &str = "com.bigpizzav3.codexplusplus.codex";

/// 旧数据目录。与 paths.rs 的 LEGACY_APP_STATE_DIR 同值(那边负责首次整目录搬迁)。
const LEGACY_APP_STATE_DIR: &str = ".codex-session-delete";
const APP_STATE_DIR: &str = ".recodex";
/// 以前出货过的 Tauri 管理工具的 WebView 数据目录(`%LOCALAPPDATA%\<identifier>`)。
/// 管理工具早已不出货,目录里只剩 WebView2 缓存。上游 Codex++ 的管理工具也用这个名字。
const LEGACY_MANAGER_DATA_DIR: &str = "com.bigpizzav3.codexplusplus.manager";

/// 一个 .lnk 里我们关心的字段。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShortcutInfo {
    pub target: String,
    pub icon: String,
    pub app_user_model_id: String,
}

/// 对一个 .lnk 要做的改动;全是 None 表示不动它。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShortcutChanges {
    pub target: Option<PathBuf>,
    pub icon: Option<PathBuf>,
    pub app_user_model_id: Option<String>,
}

impl ShortcutChanges {
    pub fn is_empty(&self) -> bool {
        self.target.is_none() && self.icon.is_none() && self.app_user_model_id.is_none()
    }
}

fn exe_file_name(stem: &str) -> String {
    format!("{stem}.exe")
}

fn file_name_is(path: &Path, expected: &str) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(expected))
}

/// 当前 exe 是旧名 `codex-plus-plus.exe` 时,返回同目录下新名 `recodex.exe` 的路径。
pub fn legacy_handoff_target(current_exe: &Path) -> Option<PathBuf> {
    if !file_name_is(current_exe, &exe_file_name(LEGACY_SILENT_BINARY)) {
        return None;
    }
    Some(current_exe.parent()?.join(exe_file_name(SILENT_BINARY)))
}

/// 当前 exe 是新名 `recodex.exe` 时,返回同目录下可能残留的旧名 exe 路径。
pub fn legacy_sibling(current_exe: &Path) -> Option<PathBuf> {
    if !file_name_is(current_exe, &exe_file_name(SILENT_BINARY)) {
        return None;
    }
    Some(current_exe.parent()?.join(exe_file_name(LEGACY_SILENT_BINARY)))
}

/// 路径比较用的规范形:去空白与引号、去 `\\?\` 前缀、斜杠统一成反斜杠、ASCII 小写。
///
/// 只做 ASCII 小写,保证字节长度不变 —— [`replace_exe_path`] 依赖这一点把
/// 规范形里找到的位置原样映射回原串。
fn normalize_path_text(value: &str) -> String {
    let trimmed = value.trim().trim_matches('"');
    let trimmed = trimmed.strip_prefix(r"\\?\").unwrap_or(trimmed);
    trimmed.replace('/', "\\").to_ascii_lowercase()
}

/// `text`(注册表值、快捷方式字段)指的是不是 `path` 这个文件。
pub fn same_file_path(text: &str, path: &Path) -> bool {
    !text.trim().is_empty() && normalize_path_text(text) == normalize_path_text(&path.to_string_lossy())
}

/// 决定一个 .lnk 要怎么改。
///
/// - 目标是旧 exe → 改指新 exe;
/// - 图标取自旧 exe → 改取新 exe(老安装包的「卸载 ReCodex」快捷方式就是这样:
///   目标是 uninstall.exe,图标借的是 codex-plus-plus.exe);
/// - 目标是我们的 exe(新旧都算)且带着旧 AppUserModelID → 换成新 ID。
///   目标不是我们的就不碰:上游 Codex++ 的固定项用的是同一个旧 ID。
pub fn plan_shortcut_changes(info: &ShortcutInfo, legacy: &Path, current: &Path) -> ShortcutChanges {
    let targets_legacy = same_file_path(&info.target, legacy);
    let targets_ours = targets_legacy || same_file_path(&info.target, current);
    ShortcutChanges {
        target: targets_legacy.then(|| current.to_path_buf()),
        icon: same_file_path(&info.icon, legacy).then(|| current.to_path_buf()),
        app_user_model_id: (targets_ours
            && info.app_user_model_id == LEGACY_CODEX_WINDOW_APP_USER_MODEL_ID)
            .then(|| CODEX_WINDOW_APP_USER_MODEL_ID.to_string()),
    }
}

/// 把一段注册表字符串/命令行里出现的旧 exe 路径换成新路径。没出现就返回 None。
///
/// 大小写不敏感、正反斜杠都认;引号、参数、`,0` 图标序号这些周边内容原样保留。
/// 例:`"C:\...\codex-plus-plus.exe" --debug-port 9229` → `"C:\...\recodex.exe" --debug-port 9229`。
pub fn replace_exe_path(value: &str, legacy: &Path, current: &Path) -> Option<String> {
    let needle = normalize_path_text(&legacy.to_string_lossy());
    if needle.is_empty() {
        return None;
    }
    // 与 normalize_path_text 不同:这里不去引号/前缀,只做等长变换,位置才能对回原串
    let haystack = value.replace('/', "\\").to_ascii_lowercase();
    let replacement = current.to_string_lossy();
    let mut out = String::with_capacity(value.len());
    let mut cursor = 0;
    let mut changed = false;
    while let Some(offset) = haystack[cursor..].find(&needle) {
        let start = cursor + offset;
        out.push_str(&value[cursor..start]);
        out.push_str(&replacement);
        cursor = start + needle.len();
        changed = true;
    }
    if !changed {
        return None;
    }
    out.push_str(&value[cursor..]);
    Some(out)
}

/// 卸载项是不是**这个安装目录**的(而不是别处的另一份 ReCodex)。
///
/// 只有属于当前 exe 所在目录的卸载项才去改 DisplayVersion —— 否则一个从
/// `target\release\` 跑起来的开发版会把用户正式安装的版本号改掉。
pub fn uninstall_entry_belongs_to(values: &[(String, Option<String>)], install_dir: &Path) -> bool {
    let dir = normalize_path_text(&install_dir.to_string_lossy());
    let dir = dir.trim_end_matches('\\');
    if dir.is_empty() {
        return false;
    }
    let prefix = format!("{dir}\\");
    values.iter().any(|(name, value)| {
        let Some(value) = value else { return false };
        let value = normalize_path_text(value);
        match name.as_str() {
            "InstallLocation" => value.trim_end_matches('\\') == dir,
            "DisplayIcon" | "UninstallString" | "QuietUninstallString" => value.starts_with(&prefix),
            _ => false,
        }
    })
}

/// 启动器用来接管「老卸载程序」的命令行参数。
pub const LEGACY_UNINSTALL_FLAG: &str = "--legacy-uninstall";

/// 1.3.4 之前的安装包写的卸载项,要不要改指我们自己的 `--legacy-uninstall`。
///
/// 背景:老安装包生成的 `uninstall.exe` 只认得 `codex-plus-plus.exe` —— 它删旧名 exe、
/// 快捷方式和注册表,却不知道迁移后多出来的 `recodex.exe`,于是「程序和功能」里卸载完,
/// 新名 exe 和安装目录留在磁盘上。那个卸载程序是 NSIS 编出来的,改不了它本身,
/// 只能把卸载项的 UninstallString 换成 `"<目录>\recodex.exe" --legacy-uninstall`:
/// 由我们原地跑一遍老卸载程序(界面、确认页照旧),它确认卸完后再删掉 recodex.exe 与目录。
///
/// 只在三件事同时成立时才改(任何一条不满足都返回 None,什么都不动):
///   1. 卸载项属于**这个**安装目录(别处还有一份 ReCodex 的不碰);
///   2. UninstallString 就是本目录的 `uninstall.exe`(已经改过的、或别的写法都不碰);
///   3. DisplayIcon 还指着本目录的旧名 exe —— 只有老安装包会这么写,新安装包写的是
///      recodex.exe。这是「卸载程序是老的」唯一可靠的信号,所以必须在迁移改写 DisplayIcon
///      **之前**判断(retarget_references 里就是这个顺序)。
pub fn legacy_uninstall_redirect(
    values: &[(String, Option<String>)],
    install_dir: &Path,
    current_exe: &Path,
) -> Option<String> {
    if !uninstall_entry_belongs_to(values, install_dir) {
        return None;
    }
    let value_of = |key: &str| {
        values
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(key))
            .and_then(|(_, value)| value.clone())
    };
    let uninstall = value_of("UninstallString")?;
    if !same_file_path(&uninstall, &install_dir.join("uninstall.exe")) {
        return None;
    }
    let icon = value_of("DisplayIcon")?;
    // DisplayIcon 可能带 ",0" 之类的图标序号
    let icon = match icon.rsplit_once(',') {
        Some((path, index)) if index.trim().parse::<i32>().is_ok() => path.to_string(),
        _ => icon,
    };
    if !same_file_path(&icon, &install_dir.join(exe_file_name(LEGACY_SILENT_BINARY))) {
        return None;
    }
    Some(format!("\"{}\" {LEGACY_UNINSTALL_FLAG}", current_exe.display()))
}

/// 看起来像不像一个完整的 Windows exe。迁移时同目录已有 `recodex.exe` 就用它,
/// 但一个半截文件(上次复制到一半断电)不能拿来接班。
pub fn plausible_windows_exe(head: &[u8], len: u64) -> bool {
    const MIN_PLAUSIBLE_SIZE: u64 = 64 * 1024;
    head.starts_with(b"MZ") && len >= MIN_PLAUSIBLE_SIZE
}

/// 旧数据目录里这些东西都是可再生的(状态快照、日志、锁),不算「有没迁移的数据」。
fn is_regenerable_state_file(relative: &Path) -> bool {
    if relative
        .components()
        .next()
        .is_some_and(|first| first.as_os_str() == "locks")
    {
        return true;
    }
    let name = relative
        .file_name()
        .map(|name| name.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    name == "latest-status.json"
        || name.ends_with(".log")
        || name.ends_with(".log.uploaded")
        || name.ends_with(".log.uploaded.tmp")
}

/// 旧数据目录是否已经被新目录**完全取代**,可以安全删除。
///
/// 旧目录里每个非可再生文件,都必须在新目录的同一相对路径上存在,并且内容相同
/// 或新目录那份更新(mtime 不早于旧的)。有一个不满足就整个保留 ——
/// 比如用户同时还装着上游 Codex++,它会一直往这个目录里写更新的数据。
/// 遇到符号链接、读不了的条目、文件多得离谱,一律当作「不确定」保留。
pub fn legacy_dir_superseded(legacy: &Path, current: &Path) -> bool {
    const MAX_ENTRIES: usize = 20_000;
    if !legacy.is_dir() || !current.is_dir() {
        return false;
    }
    let mut stack = vec![legacy.to_path_buf()];
    let mut seen = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return false;
        };
        for entry in entries {
            seen += 1;
            if seen > MAX_ENTRIES {
                return false;
            }
            let Ok(entry) = entry else { return false };
            let Ok(file_type) = entry.file_type() else {
                return false;
            };
            let path = entry.path();
            if file_type.is_symlink() {
                return false;
            }
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            let Ok(relative) = path.strip_prefix(legacy) else {
                return false;
            };
            if is_regenerable_state_file(relative) {
                continue;
            }
            if !file_superseded(&path, &current.join(relative)) {
                return false;
            }
        }
    }
    true
}

fn file_superseded(legacy_file: &Path, current_file: &Path) -> bool {
    let (Ok(legacy_meta), Ok(current_meta)) =
        (std::fs::metadata(legacy_file), std::fs::metadata(current_file))
    else {
        return false;
    };
    if !current_meta.is_file() {
        return false;
    }
    if let (Ok(legacy_time), Ok(current_time)) = (legacy_meta.modified(), current_meta.modified()) {
        if current_time >= legacy_time {
            return true;
        }
    }
    legacy_meta.len() == current_meta.len()
        && matches!(
            (std::fs::read(legacy_file), std::fs::read(current_file)),
            (Ok(a), Ok(b)) if a == b
        )
}

/// 管理工具的旧 WebView 数据目录里只有 WebView2 缓存时才可以删。
/// 多出任何别的东西(说明有别的程序在用这个目录)就保留。
pub fn legacy_manager_dir_is_webview_cache_only(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let mut any = false;
    for entry in entries {
        let Ok(entry) = entry else { return false };
        any = true;
        if !entry.file_name().to_string_lossy().eq_ignore_ascii_case("EBWebView") {
            return false;
        }
    }
    any
}

/// 日志改名时没搬成(那一刻旧日志被另一个进程占着),之后新日志已经建起来了 ——
/// paths.rs 的搬迁从此不再尝试,旧日志成了孤儿。
///
/// 这里把旧日志里**还没上报的部分**(上报水位 `<日志>.uploaded` 之后的字节)接到新日志
/// 末尾,再删掉旧日志和它的水位。只接最后 256 KiB:没有水位文件的旧日志多半来自
/// 还没有上报功能的版本,几十 MB 全量接过去会触发一次上报洪峰
/// (paths.rs 里记过那次 46 MB / 17 小时重传的教训),而那么老的事件已经没有价值。
///
/// 返回接过去的字节数;新旧不是同时存在时什么都不做,返回 None。
pub fn merge_leftover_legacy_log(legacy: &Path, current: &Path) -> std::io::Result<Option<usize>> {
    use std::io::Write;
    const MAX_TAIL: usize = 256 * 1024;
    // 先把旧日志改名「认领」下来再处理:还有进程开着它时 Windows 上改名会失败,
    // 这次就整个跳过、下次再来 —— 而不是先接过去、删的时候才失败,下次再接一遍(重复)。
    // 上次认领后中途退出留下的 .merging 也在这里续上。
    let claimed = PathBuf::from(format!("{}.merging", legacy.display()));
    if !current.is_file() {
        return Ok(None);
    }
    if !claimed.is_file() {
        if !legacy.is_file() {
            return Ok(None);
        }
        std::fs::rename(legacy, &claimed)?;
    }
    let bytes = std::fs::read(&claimed)?;
    let sidecar = PathBuf::from(format!("{}.uploaded", legacy.display()));
    let watermark = std::fs::read_to_string(&sidecar)
        .ok()
        .and_then(|text| text.trim().parse::<usize>().ok())
        .filter(|value| *value <= bytes.len())
        .unwrap_or(0);
    let mut tail = &bytes[watermark..];
    if tail.len() > MAX_TAIL {
        tail = &tail[tail.len() - MAX_TAIL..];
        // 从截断点之后的第一个完整行开始,别接半行
        match tail.iter().position(|byte| *byte == b'\n') {
            Some(newline) => tail = &tail[newline + 1..],
            None => tail = &[],
        }
    }
    let mut chunk = tail.to_vec();
    if !chunk.is_empty() && !chunk.ends_with(b"\n") {
        chunk.push(b'\n');
    }
    if !chunk.is_empty() {
        let mut file = std::fs::OpenOptions::new().append(true).open(current)?;
        // 一次 write_all:追加模式下单次写入是原子的,不会和别的进程的行交错
        file.write_all(&chunk)?;
        file.sync_all()?;
    }
    std::fs::remove_file(&claimed)?;
    match std::fs::remove_file(&sidecar) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    Ok(Some(chunk.len()))
}

/// 机器上是不是(还)装着上游 Codex++。
///
/// 旧数据目录 `~/.codex-session-delete` 与管理工具的 WebView 目录都是和上游**共用**的名字。
/// 用户同时装着上游时,那是别人的数据 —— 整个清理跳过,宁可留着不删。
pub fn upstream_codexplusplus_present() -> bool {
    #[cfg(windows)]
    {
        windows_impl::upstream_present()
    }
    #[cfg(target_os = "macos")]
    {
        let home = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf());
        [Some(PathBuf::from("/Applications")), home.map(|home| home.join("Applications"))]
            .into_iter()
            .flatten()
            .any(|dir| dir.join("Codex++.app").exists())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        false
    }
}

/// 以旧名启动时迁移到新名,并从新名重新拉起。
///
/// 返回 true 表示新进程已经拉起,调用方应**立即退出**(不要再去抢单实例锁、启动 Codex);
/// 返回 false 表示照常以当前身份继续运行 —— 不是旧名、不在 Windows、或者迁移中途失败。
/// 失败一律只写诊断日志,下次启动会再试。
pub fn handoff_from_legacy_binary(args: &[String]) -> bool {
    #[cfg(windows)]
    {
        windows_impl::handoff(args)
    }
    #[cfg(not(windows))]
    {
        let _ = args;
        false
    }
}

/// 启动后的后台杂务:清理旧 exe、改写旧引用、同步卸载项版本、清理旧数据残留。
/// 全部在后台线程里做,不拖慢启动;失败只写诊断日志。
pub fn spawn_startup_housekeeping() {
    let _ = std::thread::Builder::new()
        .name("recodex-legacy-housekeeping".to_string())
        .spawn(|| {
            #[cfg(windows)]
            windows_impl::housekeeping();
            cleanup_legacy_data_leftovers();
        });
}

/// 自更新就位后调用:卸载项里的版本号跟上。只在卸载项属于当前安装目录时才写。
pub fn record_installed_version(version: &str) {
    #[cfg(windows)]
    {
        if let Err(error) = windows_impl::sync_uninstall_display_version(version) {
            let _ = crate::diagnostic_log::append_diagnostic_log(
                "launcher.uninstall_entry_sync",
                json!({ "error": error.to_string() }),
            );
        }
    }
    #[cfg(not(windows))]
    {
        let _ = version;
    }
}

/// 清理旧品牌留下的数据残留。只删**确认已被新位置取代**的东西。
fn cleanup_legacy_data_leftovers() {
    let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()) else {
        return;
    };
    let current_dir = home.join(APP_STATE_DIR);
    if !current_dir.is_dir() {
        // 新数据目录都还没有,说明迁移都没发生过,什么都别动
        return;
    }
    let mut removed: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    // 1) ~/.recodex/codex-plus.log(日志改名时没搬成的孤儿)。这个文件只可能是我们写的,
    //    不受上游是否存在影响。
    match merge_leftover_legacy_log(
        &current_dir.join("codex-plus.log"),
        &current_dir.join("recodex.log"),
    ) {
        Ok(Some(bytes)) => removed.push(format!("~/.recodex/codex-plus.log(接入 {bytes} 字节未上报日志)")),
        Ok(None) => {}
        Err(error) => errors.push(format!("合并旧日志失败:{error}")),
    }

    let upstream = upstream_codexplusplus_present();
    if !upstream {
        // 2) ~/.codex-session-delete:首次启动时已整体搬到 ~/.recodex;两者并存说明之后
        //    又有东西写了旧目录(多半是升级窗口里还在跑的老版本)。
        let legacy_dir = home.join(LEGACY_APP_STATE_DIR);
        if legacy_dir.is_dir() {
            if legacy_dir_superseded(&legacy_dir, &current_dir) {
                match std::fs::remove_dir_all(&legacy_dir) {
                    Ok(()) => removed.push(format!("~/{LEGACY_APP_STATE_DIR}")),
                    Err(error) => errors.push(format!("删除 ~/{LEGACY_APP_STATE_DIR} 失败:{error}")),
                }
            }
        }
        // 3) 管理工具的旧 WebView 数据目录
        #[cfg(windows)]
        if let Some(dir) = legacy_manager_data_dir() {
            if dir.is_dir() && legacy_manager_dir_is_webview_cache_only(&dir) {
                match std::fs::remove_dir_all(&dir) {
                    Ok(()) => removed.push(dir.display().to_string()),
                    Err(error) => errors.push(format!("删除 {} 失败:{error}", dir.display())),
                }
            }
        }
    }

    if removed.is_empty() && errors.is_empty() {
        return;
    }
    let mut detail = json!({ "removed": removed, "upstream_present": upstream });
    if !errors.is_empty() {
        detail["error"] = json!(errors.join("; "));
    }
    let _ = crate::diagnostic_log::append_diagnostic_log("launcher.legacy_data_cleanup", detail);
}

/// `%LOCALAPPDATA%\com.bigpizzav3.codexplusplus.manager`
pub fn legacy_manager_data_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join(LEGACY_MANAGER_DATA_DIR))
}

/// 卸载时清理管理工具的旧 WebView 数据目录。与启动时的清理同一套守卫:
/// 上游 Codex++ 还在就不碰,目录里有 WebView 缓存以外的东西也不碰。
pub fn remove_legacy_manager_data_for_uninstall() -> Option<PathBuf> {
    if upstream_codexplusplus_present() {
        return None;
    }
    let dir = legacy_manager_data_dir()?;
    if !dir.is_dir() || !legacy_manager_dir_is_webview_cache_only(&dir) {
        return None;
    }
    std::fs::remove_dir_all(&dir).ok()?;
    Some(dir)
}

#[cfg(windows)]
mod windows_impl {
    use std::path::{Path, PathBuf};

    use serde_json::json;

    use super::*;

    const UNINSTALL_SUBKEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\ReCodex";
    /// 可能引用了旧 exe 路径的注册表项:(子键, 值名)。都只在值确实提到旧 exe 时才改。
    const EXE_REFERENCES: [(&str, &str); 6] = [
        (UNINSTALL_SUBKEY, "DisplayIcon"),
        (UNINSTALL_SUBKEY, "UninstallString"),
        (UNINSTALL_SUBKEY, "QuietUninstallString"),
        // 从 Codex++ 迁移过来的机器上,开机自启项可能指着这个 exe
        (crate::watcher::WATCHER_RUN_KEY, crate::watcher::WATCHER_RUN_NAME),
        (r"Software\Classes\codexplusplus\shell\open\command", ""),
        (r"Software\Classes\dreamskin\shell\open\command", ""),
    ];

    pub(super) fn upstream_present() -> bool {
        use crate::windows_integration::read_current_user_string_values;
        // 上游安装器写的两个键(我们的安装器从来只写 Software\ReCodex)
        let registry = [r"Software\Codex++", r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Codex++"]
            .iter()
            .any(|key| read_current_user_string_values(key).is_ok_and(|values| !values.is_empty()));
        // 上游默认安装位置
        let default_dir = std::env::var_os("LOCALAPPDATA")
            .map(|base| PathBuf::from(base).join("Programs").join("Codex++"))
            .is_some_and(|dir| dir.exists());
        // 正在运行、但不在我们安装目录里的上游进程
        let our_dir = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf));
        let running = crate::windows_integration::enumerate_processes()
            .into_iter()
            .any(|process| {
                let upstream_name = ["codex-plus-plus.exe", "codex-plus-plus-manager.exe"]
                    .iter()
                    .any(|name| process.exe_file.eq_ignore_ascii_case(name));
                upstream_name
                    && match (&process.executable_path, &our_dir) {
                        (Some(path), Some(ours)) => path
                            .parent()
                            .is_none_or(|dir| !same_file_path(&dir.to_string_lossy(), ours)),
                        // 拿不到路径就当它是上游的 —— 宁可少删
                        _ => true,
                    }
            });
        registry || default_dir || running
    }

    /// 以旧名启动时:备好新名 exe → 改写引用 → 从新名拉起。
    pub(super) fn handoff(args: &[String]) -> bool {
        let Ok(exe) = std::env::current_exe() else {
            return false;
        };
        let Some(target) = legacy_handoff_target(&exe) else {
            return false;
        };
        let mut detail = json!({ "from": exe, "to": target });
        let outcome = (|| -> anyhow::Result<()> {
            let copied = ensure_target_binary(&exe, &target)?;
            detail["copied"] = json!(copied);
            let report = retarget_references(&exe, &target);
            detail["shortcuts_updated"] = json!(report.shortcuts_updated);
            detail["registry_updated"] = json!(report.registry_updated);
            if !report.errors.is_empty() {
                // 引用没改全不阻止接班:新 exe 接班后还会再改一遍(幂等),
                // 旧 exe 在引用改全之前不会被删。
                detail["reference_errors"] = json!(report.errors);
            }
            spawn_detached(&target, args)?;
            Ok(())
        })();
        let handed_off = outcome.is_ok();
        if let Err(error) = outcome {
            detail["error"] = json!(format!("{error:#}"));
        }
        detail["handed_off"] = json!(handed_off);
        let _ = crate::diagnostic_log::append_diagnostic_log("launcher.legacy_binary_handoff", detail);
        handed_off
    }

    /// 确保 `target` 是一个可用的 exe。已有且像样就用现成的(返回 false);
    /// 否则把自己复制过去(先写临时文件再改名,不留半截文件)。
    fn ensure_target_binary(exe: &Path, target: &Path) -> anyhow::Result<bool> {
        if let Ok(meta) = std::fs::metadata(target) {
            let mut head = [0u8; 2];
            let readable = std::fs::File::open(target)
                .and_then(|mut file| std::io::Read::read_exact(&mut file, &mut head))
                .is_ok();
            if readable && plausible_windows_exe(&head, meta.len()) {
                // 同目录已有新名 exe(之前迁移过、或装过新安装包):用它,不拿自己覆盖 ——
                // 它可能比我们新(旧名这份没被删掉、又被某个没改到的入口拉起来的情况)。
                return Ok(false);
            }
        }
        let staging = PathBuf::from(format!("{}.migrating", target.display()));
        let _ = std::fs::remove_file(&staging);
        std::fs::copy(exe, &staging)?;
        std::fs::File::open(&staging)?.sync_all()?;
        let _ = std::fs::remove_file(target);
        if let Err(error) = std::fs::rename(&staging, target) {
            let _ = std::fs::remove_file(&staging);
            return Err(error.into());
        }
        Ok(true)
    }

    fn spawn_detached(target: &Path, args: &[String]) -> std::io::Result<()> {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        // 与 watcher::restart_with_fresh_launcher 相同的起法。参数原样转交
        // (包括 --await-guard:由自更新重启拉起时,旧 launcher 可能还没退干净)。
        let mut command = std::process::Command::new(target);
        command.args(args).creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
        if let Some(dir) = target.parent() {
            command.current_dir(dir);
        }
        command.spawn().map(|_| ())
    }

    #[derive(Default)]
    struct RetargetReport {
        shortcuts_updated: Vec<String>,
        registry_updated: Vec<String>,
        errors: Vec<String>,
    }

    /// 桌面、开始菜单(含「启动」)、任务栏/开始屏幕固定项里的 .lnk。
    /// 深度与数量都设上限:这些目录里什么都可能有,不能让一次启动去扫半个硬盘。
    fn candidate_shortcuts() -> Vec<PathBuf> {
        const MAX_DEPTH: usize = 3;
        const MAX_FILES: usize = 3000;
        let mut roots: Vec<PathBuf> = Vec::new();
        roots.extend(crate::windows_integration::desktop_dir());
        roots.extend(crate::windows_integration::start_menu_programs_dir());
        if let Some(appdata) = std::env::var_os("APPDATA") {
            roots.push(
                PathBuf::from(appdata)
                    .join("Microsoft")
                    .join("Internet Explorer")
                    .join("Quick Launch"),
            );
        }
        let mut found = Vec::new();
        let mut stack: Vec<(PathBuf, usize)> = roots.into_iter().map(|root| (root, 0)).collect();
        while let Some((dir, depth)) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(file_type) = entry.file_type() else { continue };
                if file_type.is_dir() {
                    if depth < MAX_DEPTH {
                        stack.push((path, depth + 1));
                    }
                } else if path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("lnk"))
                {
                    found.push(path);
                    if found.len() >= MAX_FILES {
                        return found;
                    }
                }
            }
        }
        found
    }

    /// 把指向旧 exe 的快捷方式与注册表项改指新 exe;顺带把我们固定项里的旧
    /// AppUserModelID 换成新的。幂等:已经改过的不会再动。
    fn retarget_references(legacy: &Path, current: &Path) -> RetargetReport {
        let mut report = RetargetReport::default();
        let (updated, errors) = crate::windows_integration::update_shortcuts(
            &candidate_shortcuts(),
            |info| plan_shortcut_changes(info, legacy, current),
        );
        report.shortcuts_updated = updated.iter().map(|path| path.display().to_string()).collect();
        report.errors.extend(errors);
        // 必须在下面改写 DisplayIcon **之前**:它是判断「卸载程序是老的」的唯一信号。
        if let (Ok(values), Some(dir)) = (
            crate::windows_integration::read_current_user_string_values(UNINSTALL_SUBKEY),
            current.parent(),
        ) {
            if let Some(command) = legacy_uninstall_redirect(&values, dir, current) {
                match crate::windows_integration::set_current_user_string_value(
                    UNINSTALL_SUBKEY,
                    "UninstallString",
                    &command,
                ) {
                    Ok(()) => report
                        .registry_updated
                        .push(format!(r"{UNINSTALL_SUBKEY}\UninstallString")),
                    Err(error) => report.errors.push(format!("{error:#}")),
                }
            }
        }
        for (subkey, name) in EXE_REFERENCES {
            let Ok(values) = crate::windows_integration::read_current_user_string_values(subkey) else {
                continue;
            };
            let Some(Some(value)) = values
                .iter()
                .find(|(value_name, _)| value_name.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.clone())
            else {
                continue;
            };
            let Some(rewritten) = replace_exe_path(&value, legacy, current) else {
                continue;
            };
            match crate::windows_integration::set_current_user_string_value(subkey, name, &rewritten) {
                Ok(()) => report.registry_updated.push(format!(r"{subkey}\{name}")),
                Err(error) => report.errors.push(format!("{error:#}")),
            }
        }
        report
    }

    /// 卸载项属于当前安装目录时,把 DisplayVersion 写成 `version`。
    pub(super) fn sync_uninstall_display_version(version: &str) -> anyhow::Result<bool> {
        let exe = std::env::current_exe()?;
        let Some(dir) = exe.parent() else { return Ok(false) };
        let values = crate::windows_integration::read_current_user_string_values(UNINSTALL_SUBKEY)?;
        if values.is_empty() || !uninstall_entry_belongs_to(&values, dir) {
            return Ok(false);
        }
        let current = values
            .iter()
            .find(|(name, _)| name == "DisplayVersion")
            .and_then(|(_, value)| value.clone());
        if current.as_deref() == Some(version) {
            return Ok(false);
        }
        crate::windows_integration::set_current_user_string_value(
            UNINSTALL_SUBKEY,
            "DisplayVersion",
            version,
        )?;
        Ok(true)
    }

    /// 与路径无关的每台机器只需一次的活(改任务栏固定项的 AppUserModelID)做完后记一笔,
    /// 免得每次启动都扫一遍开始菜单。按 exe 路径区分:开发机上 target\release 下跑过一次,
    /// 不能挡住正式安装那份去改它自己的固定项。
    fn shortcut_scan_marker(exe: &Path) -> Option<PathBuf> {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in normalize_path_text(&exe.to_string_lossy()).bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        Some(
            crate::paths::default_app_state_dir()
                .join("locks")
                .join(format!("shortcut-migration-v1-{hash:016x}.done")),
        )
    }

    /// 以新名启动后的后台杂务。
    pub(super) fn housekeeping() {
        let Ok(exe) = std::env::current_exe() else { return };
        let mut detail = json!({});
        let mut worth_logging = false;

        // 只有以新名 recodex.exe 运行时才清理旧 exe、改写引用。以旧名运行说明接班失败了
        // (handoff 那边已记过日志),此时旧 exe 正是我们自己,什么都不能删。
        let legacy = legacy_sibling(&exe);
        let legacy_exists = legacy.as_ref().is_some_and(|legacy| legacy.exists());
        let marker = shortcut_scan_marker(&exe);
        let marker_done = marker.as_ref().is_some_and(|marker| marker.exists());
        if let Some(legacy) = legacy.as_ref().filter(|_| legacy_exists || !marker_done) {
            let report = retarget_references(legacy, &exe);
            if !report.shortcuts_updated.is_empty() || !report.registry_updated.is_empty() {
                worth_logging = true;
                detail["shortcuts_updated"] = json!(report.shortcuts_updated);
                detail["registry_updated"] = json!(report.registry_updated);
            }
            if report.errors.is_empty() {
                if let Some(marker) = &marker {
                    if let Some(parent) = marker.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    let _ = std::fs::write(marker, b"1");
                }
                if legacy_exists {
                    // 引用全改完了才删旧 exe;改不全就留着,免得哪个入口点了没反应
                    worth_logging = true;
                    detail["legacy_removed"] = json!(delete_legacy_binary(legacy));
                }
            } else {
                worth_logging = true;
                detail["error"] = json!(report.errors.join("; "));
            }
        }

        match sync_uninstall_display_version(crate::version::VERSION) {
            Ok(true) => {
                worth_logging = true;
                detail["display_version"] = json!(crate::version::VERSION);
            }
            Ok(false) => {}
            Err(error) => {
                worth_logging = true;
                detail["display_version_error"] = json!(format!("{error:#}"));
            }
        }

        if worth_logging {
            let _ = crate::diagnostic_log::append_diagnostic_log("launcher.legacy_binary_cleanup", detail);
        }
    }

    /// 删旧 exe(连同自更新留下的 .old/.new)。刚交接完时旧进程可能还没退干净、
    /// 映像还锁着,所以重试一会儿;还删不掉就留给下次启动。
    fn delete_legacy_binary(legacy: &Path) -> bool {
        for suffix in [".old", ".new"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", legacy.display()));
        }
        for _ in 0..30 {
            match std::fs::remove_file(legacy) {
                Ok(()) => return true,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return true,
                Err(_) => std::thread::sleep(std::time::Duration::from_secs(1)),
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(value: &str) -> PathBuf {
        PathBuf::from(value)
    }

    /// 用本平台的路径形态造安装目录,测试在 mac/linux 上也成立(那边 `\` 不是分隔符)。
    fn install_dir() -> PathBuf {
        if cfg!(windows) {
            p(r"C:\Users\u\AppData\Local\Programs\ReCodex")
        } else {
            p("/home/u/.local/share/ReCodex")
        }
    }

    fn in_install(name: &str) -> String {
        install_dir().join(name).to_string_lossy().into_owned()
    }

    fn legacy_exe() -> PathBuf {
        install_dir().join("codex-plus-plus.exe")
    }

    fn new_exe() -> PathBuf {
        install_dir().join("recodex.exe")
    }

    // ---------- 旧名/新名判断 ----------

    #[test]
    fn only_the_legacy_name_hands_off_and_it_hands_off_to_the_same_directory() {
        let target = legacy_handoff_target(&legacy_exe()).expect("旧名要迁移");
        assert_eq!(target.file_name().unwrap(), "recodex.exe");
        assert_eq!(target.parent(), legacy_exe().parent(), "必须迁到同一目录");
        // 大小写不敏感:Windows 文件名本来就不分
        assert!(legacy_handoff_target(&install_dir().join("Codex-Plus-Plus.EXE")).is_some());
        // 新名、别的程序、没有扩展名的都不迁 —— 否则就是无限自我接班
        assert!(legacy_handoff_target(&new_exe()).is_none());
        assert!(legacy_handoff_target(&install_dir().join("codex-plus-plus-manager.exe")).is_none());
        assert!(legacy_handoff_target(&install_dir().join("codex-plus-plus")).is_none());
    }

    #[test]
    fn only_the_new_name_looks_for_a_legacy_sibling() {
        assert_eq!(legacy_sibling(&new_exe()), Some(legacy_exe()));
        assert!(legacy_sibling(&legacy_exe()).is_none());
        // 测试进程、开发工具之类的别的 exe 不做任何清理
        assert!(legacy_sibling(&install_dir().join("legacy_install-1234.exe")).is_none());
    }

    #[test]
    fn path_comparison_ignores_case_quotes_slashes_and_verbatim_prefix() {
        let exe = legacy_exe();
        assert!(same_file_path(&exe.to_string_lossy().to_uppercase(), &exe));
        assert!(same_file_path(&format!("\"{}\"", exe.display()), &exe));
        assert!(same_file_path(&exe.to_string_lossy().replace('\\', "/"), &exe));
        assert!(same_file_path(&format!(r"\\?\{}", exe.display()), &exe));
        assert!(!same_file_path("", &exe), "空目标(比如指向文件夹的快捷方式)不算");
        assert!(!same_file_path(&new_exe().to_string_lossy(), &exe));
    }

    // ---------- 快捷方式决策 ----------

    #[test]
    fn a_shortcut_to_the_legacy_exe_is_retargeted_with_its_icon() {
        let info = ShortcutInfo {
            target: legacy_exe().to_string_lossy().into(),
            icon: legacy_exe().to_string_lossy().into(),
            app_user_model_id: String::new(),
        };
        let changes = plan_shortcut_changes(&info, &legacy_exe(), &new_exe());
        assert_eq!(changes.target, Some(new_exe()));
        assert_eq!(changes.icon, Some(new_exe()));
        assert_eq!(changes.app_user_model_id, None, "没有旧 ID 就不写 ID");
    }

    /// 老安装包建的「卸载 ReCodex」:目标是 uninstall.exe,图标借旧 exe。
    /// 旧 exe 删掉之后图标会变白 —— 只换图标,目标不动。
    #[test]
    fn the_uninstall_shortcut_only_gets_its_icon_fixed() {
        let info = ShortcutInfo {
            target: in_install("uninstall.exe"),
            icon: legacy_exe().to_string_lossy().into(),
            app_user_model_id: String::new(),
        };
        let changes = plan_shortcut_changes(&info, &legacy_exe(), &new_exe());
        assert_eq!(changes.target, None);
        assert_eq!(changes.icon, Some(new_exe()));
    }

    /// 从运行中的 Codex 窗口「固定到任务栏」时,固定项里记的是窗口的 AppUserModelID。
    /// 窗口换了新 ID 而固定项还是旧的,两者就分成两个图标。
    #[test]
    fn our_pinned_items_get_the_new_app_user_model_id() {
        for target in [legacy_exe(), new_exe()] {
            let info = ShortcutInfo {
                target: target.to_string_lossy().into(),
                icon: String::new(),
                app_user_model_id: LEGACY_CODEX_WINDOW_APP_USER_MODEL_ID.into(),
            };
            let changes = plan_shortcut_changes(&info, &legacy_exe(), &new_exe());
            assert_eq!(
                changes.app_user_model_id.as_deref(),
                Some(CODEX_WINDOW_APP_USER_MODEL_ID),
                "目标 {} 的固定项要换 ID",
                target.display()
            );
        }
    }

    /// 上游 Codex++ 用的是同一个旧 ID。别人家的固定项一个都不能碰。
    #[test]
    fn upstream_pins_with_the_same_old_id_are_left_alone() {
        let info = ShortcutInfo {
            target: r"C:\Users\u\AppData\Local\Programs\Codex++\codex-plus-plus.exe".into(),
            icon: r"C:\Users\u\AppData\Local\Programs\Codex++\codex-plus-plus.exe".into(),
            app_user_model_id: LEGACY_CODEX_WINDOW_APP_USER_MODEL_ID.into(),
        };
        assert!(plan_shortcut_changes(&info, &legacy_exe(), &new_exe()).is_empty());
    }

    #[test]
    fn already_migrated_shortcuts_are_not_rewritten_again() {
        let info = ShortcutInfo {
            target: new_exe().to_string_lossy().into(),
            icon: new_exe().to_string_lossy().into(),
            app_user_model_id: CODEX_WINDOW_APP_USER_MODEL_ID.into(),
        };
        assert!(plan_shortcut_changes(&info, &legacy_exe(), &new_exe()).is_empty());
    }

    #[test]
    fn app_user_model_ids_are_rebranded() {
        assert!(!CODEX_WINDOW_APP_USER_MODEL_ID.contains("bigpizza"));
        assert!(!CODEX_WINDOW_APP_USER_MODEL_ID.contains("codexplusplus"));
        assert_ne!(CODEX_WINDOW_APP_USER_MODEL_ID, LEGACY_CODEX_WINDOW_APP_USER_MODEL_ID);
    }

    // ---------- 注册表字符串 ----------

    #[test]
    fn registry_values_keep_quotes_arguments_and_icon_index() {
        let legacy = legacy_exe();
        let current = new_exe();
        assert_eq!(
            replace_exe_path(
                &format!("\"{}\" --debug-port 9229", legacy.display()),
                &legacy,
                &current
            ),
            Some(format!("\"{}\" --debug-port 9229", current.display()))
        );
        assert_eq!(
            replace_exe_path(&format!("{},0", legacy.display()), &legacy, &current),
            Some(format!("{},0", current.display()))
        );
        // 大小写、斜杠不同也认
        let shouted = legacy.to_string_lossy().to_uppercase().replace('\\', "/");
        assert_eq!(
            replace_exe_path(&format!("\"{shouted}\" \"%1\""), &legacy, &current),
            Some(format!("\"{}\" \"%1\"", current.display()))
        );
    }

    #[test]
    fn registry_values_not_mentioning_the_legacy_exe_are_untouched() {
        let legacy = legacy_exe();
        assert_eq!(replace_exe_path(&in_install("uninstall.exe"), &legacy, &new_exe()), None);
        // 上游 Codex++ 的自启项:同名 exe,不同目录 —— 不是我们的
        assert_eq!(
            replace_exe_path(
                r#""C:\Users\u\AppData\Local\Programs\Codex++\codex-plus-plus.exe" --debug-port 9229"#,
                &legacy,
                &new_exe()
            ),
            None
        );
        // 已经是新名:幂等
        assert_eq!(replace_exe_path(&new_exe().to_string_lossy(), &legacy, &new_exe()), None);
    }

    fn legacy_entry() -> Vec<(String, Option<String>)> {
        // 1.3.4 之前的安装包写的样子(见 ReCodex.nsi 的历史版本)
        vec![
            ("DisplayName".to_string(), Some("ReCodex".to_string())),
            ("DisplayVersion".to_string(), Some("1.2.60".to_string())),
            ("DisplayIcon".to_string(), Some(in_install("codex-plus-plus.exe"))),
            ("UninstallString".to_string(), Some(in_install("uninstall.exe"))),
            ("Publisher".to_string(), Some("ReCodex".to_string())),
        ]
    }

    #[test]
    fn legacy_uninstaller_is_redirected_to_our_own_flag() {
        let command = legacy_uninstall_redirect(&legacy_entry(), &install_dir(), &new_exe())
            .expect("老安装包写的卸载项应当被接管");
        assert_eq!(command, format!("\"{}\" --legacy-uninstall", new_exe().display()));
        // 带引号的 UninstallString、带图标序号的 DisplayIcon 也认
        let mut quoted = legacy_entry();
        quoted[2].1 = Some(format!("{},0", in_install("codex-plus-plus.exe")));
        quoted[3].1 = Some(format!("\"{}\"", in_install("uninstall.exe")));
        assert!(legacy_uninstall_redirect(&quoted, &install_dir(), &new_exe()).is_some());
    }

    #[test]
    fn new_installer_entries_are_left_alone() {
        // 1.3.4+ 的安装包:DisplayIcon 就是 recodex.exe,卸载程序本身认得新名 —— 不动
        let mut modern = legacy_entry();
        modern[2].1 = Some(in_install("recodex.exe"));
        assert_eq!(legacy_uninstall_redirect(&modern, &install_dir(), &new_exe()), None);
    }

    #[test]
    fn already_redirected_or_foreign_entries_are_left_alone() {
        // 已经改过:幂等
        let mut done = legacy_entry();
        done[3].1 = Some(format!("\"{}\" --legacy-uninstall", new_exe().display()));
        assert_eq!(legacy_uninstall_redirect(&done, &install_dir(), &new_exe()), None);
        // 别的目录里的另一份 ReCodex(比如开发机 target\release 跑起来的):不碰正式安装的卸载项
        let elsewhere = if cfg!(windows) { p(r"D:\dev\target\release") } else { p("/dev/target/release") };
        assert_eq!(
            legacy_uninstall_redirect(&legacy_entry(), &elsewhere, &elsewhere.join("recodex.exe")),
            None
        );
        // 卸载项里缺字段:不猜
        let partial: Vec<_> = legacy_entry().into_iter().filter(|(name, _)| name != "DisplayIcon").collect();
        assert_eq!(legacy_uninstall_redirect(&partial, &install_dir(), &new_exe()), None);
        // UninstallString 指向别的程序:不碰
        let mut other = legacy_entry();
        other[3].1 = Some(in_install("something-else.exe"));
        assert_eq!(legacy_uninstall_redirect(&other, &install_dir(), &new_exe()), None);
    }

    #[test]
    fn uninstall_entry_must_belong_to_this_install_directory() {
        let dir = install_dir();
        let ours = vec![
            ("DisplayName".to_string(), Some("ReCodex".to_string())),
            ("DisplayIcon".to_string(), Some(in_install("recodex.exe"))),
            ("UninstallString".to_string(), Some(in_install("uninstall.exe"))),
        ];
        assert!(uninstall_entry_belongs_to(&ours, &dir));
        let quoted = vec![(
            "UninstallString".to_string(),
            Some(format!("\"{}\" /S", in_install("uninstall.exe"))),
        )];
        assert!(uninstall_entry_belongs_to(&quoted, &dir));
        let location = vec![(
            "InstallLocation".to_string(),
            Some(format!("{}{}", dir.display(), std::path::MAIN_SEPARATOR)),
        )];
        assert!(uninstall_entry_belongs_to(&location, &dir));

        // 从 target\release 跑的开发版不能去改正式安装的版本号
        assert!(!uninstall_entry_belongs_to(&ours, &dir.join("target").join("release")));
        // 前缀相同的兄弟目录不算
        let sibling = dir.parent().unwrap().join("ReCod");
        assert!(!uninstall_entry_belongs_to(&ours, &sibling));
        assert!(!uninstall_entry_belongs_to(&[], &dir));
    }

    #[test]
    fn half_written_binaries_are_not_used_for_handoff() {
        assert!(plausible_windows_exe(b"MZ", 17 * 1024 * 1024));
        assert!(!plausible_windows_exe(b"MZ", 1024), "复制到一半的文件");
        assert!(!plausible_windows_exe(b"\0\0", 17 * 1024 * 1024), "被截断/清零的文件");
        assert!(!plausible_windows_exe(b"", 0));
    }

    // ---------- 旧数据目录 ----------

    fn write(path: &Path, bytes: &[u8]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn set_mtime(path: &Path, seconds_ago: u64) {
        let time = std::time::SystemTime::now() - std::time::Duration::from_secs(seconds_ago);
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    #[test]
    fn a_legacy_dir_holding_only_regenerable_files_is_superseded() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(".codex-session-delete");
        let current = root.path().join(".recodex");
        std::fs::create_dir_all(&current).unwrap();
        write(&legacy.join("latest-status.json"), b"{}");
        write(&legacy.join("codex-plus.log"), b"line\n");
        write(&legacy.join("codex-plus.log.uploaded"), b"5");
        write(&legacy.join("locks").join("loopback-port-1.lock"), b"");
        assert!(legacy_dir_superseded(&legacy, &current));
    }

    #[test]
    fn a_legacy_dir_with_data_missing_from_the_new_dir_is_kept() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(".codex-session-delete");
        let current = root.path().join(".recodex");
        std::fs::create_dir_all(&current).unwrap();
        write(&legacy.join("dream-skin").join("theme.json"), b"{}");
        assert!(!legacy_dir_superseded(&legacy, &current), "新目录里没有的数据不能删");
    }

    /// 同时装着上游 Codex++ 时,它会一直往旧目录里写**更新**的设置。
    #[test]
    fn a_legacy_file_newer_than_its_copy_is_kept() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(".codex-session-delete");
        let current = root.path().join(".recodex");
        write(&legacy.join("settings.json"), b"{\"newer\":true}");
        write(&current.join("settings.json"), b"{}");
        set_mtime(&current.join("settings.json"), 3600);
        set_mtime(&legacy.join("settings.json"), 60);
        assert!(!legacy_dir_superseded(&legacy, &current));

        // 反过来:新目录那份更新 → 旧的是过期副本,可以删
        set_mtime(&legacy.join("settings.json"), 7200);
        assert!(legacy_dir_superseded(&legacy, &current));
    }

    #[test]
    fn identical_copies_are_superseded_regardless_of_mtime() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(".codex-session-delete");
        let current = root.path().join(".recodex");
        write(&legacy.join("backups").join("a.json"), b"same");
        write(&current.join("backups").join("a.json"), b"same");
        set_mtime(&current.join("backups").join("a.json"), 3600);
        assert!(legacy_dir_superseded(&legacy, &current));
    }

    #[test]
    fn nothing_is_superseded_when_the_new_dir_is_missing() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join(".codex-session-delete");
        write(&legacy.join("latest-status.json"), b"{}");
        assert!(!legacy_dir_superseded(&legacy, &root.path().join(".recodex")));
    }

    #[test]
    fn the_manager_dir_is_removable_only_when_it_is_pure_webview_cache() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("com.bigpizzav3.codexplusplus.manager");
        std::fs::create_dir_all(dir.join("EBWebView").join("Default")).unwrap();
        assert!(legacy_manager_dir_is_webview_cache_only(&dir));
        write(&dir.join("settings.json"), b"{}");
        assert!(!legacy_manager_dir_is_webview_cache_only(&dir), "多出别的东西就保留");
        assert!(!legacy_manager_dir_is_webview_cache_only(&root.path().join("missing")));
        let empty = root.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(!legacy_manager_dir_is_webview_cache_only(&empty), "空目录不是我们要删的东西");
    }

    // ---------- 旧日志合并 ----------

    #[test]
    fn only_the_unuploaded_tail_of_the_orphan_log_is_carried_over() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("codex-plus.log");
        let current = dir.path().join("recodex.log");
        write(&legacy, b"uploaded-1\nuploaded-2\npending-1\npending-2\n");
        write(&PathBuf::from(format!("{}.uploaded", legacy.display())), b"22");
        write(&current, b"new-1\n");

        let carried = merge_leftover_legacy_log(&legacy, &current).unwrap();

        assert_eq!(carried, Some("pending-1\npending-2\n".len()));
        assert_eq!(std::fs::read(&current).unwrap(), b"new-1\npending-1\npending-2\n");
        assert!(!legacy.exists(), "合并完旧日志要删掉");
        assert!(!PathBuf::from(format!("{}.uploaded", legacy.display())).exists(), "水位也要删");
    }

    #[test]
    fn a_fully_uploaded_orphan_log_is_just_removed() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("codex-plus.log");
        let current = dir.path().join("recodex.log");
        write(&legacy, b"a\nb\n");
        write(&PathBuf::from(format!("{}.uploaded", legacy.display())), b"4");
        write(&current, b"new\n");
        assert_eq!(merge_leftover_legacy_log(&legacy, &current).unwrap(), Some(0));
        assert_eq!(std::fs::read(&current).unwrap(), b"new\n");
        assert!(!legacy.exists());
    }

    /// 没有水位的旧日志多半来自还没有上报功能的版本,可能几十 MB。
    /// 全量接过去会造成一次上报洪峰 —— 只接最后一截、且从完整的行开始。
    #[test]
    fn a_huge_orphan_log_without_watermark_is_capped_to_whole_lines() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("codex-plus.log");
        let current = dir.path().join("recodex.log");
        let mut big = Vec::new();
        for index in 0..40_000 {
            big.extend_from_slice(format!("event-{index:08}\n").as_bytes());
        }
        write(&legacy, &big);
        write(&current, b"");

        let carried = merge_leftover_legacy_log(&legacy, &current).unwrap().unwrap();

        assert!(carried <= 256 * 1024, "接过去 {carried} 字节,超过上限");
        let merged = std::fs::read_to_string(&current).unwrap();
        assert!(merged.starts_with("event-"), "必须从完整的行开始:{:?}", &merged[..20]);
        assert!(merged.ends_with("event-00039999\n"), "最新的事件必须在");
    }

    #[test]
    fn log_merge_needs_both_files() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("codex-plus.log");
        let current = dir.path().join("recodex.log");
        write(&legacy, b"x\n");
        assert_eq!(merge_leftover_legacy_log(&legacy, &current).unwrap(), None);
        assert!(legacy.exists(), "新日志还不存在时由 paths.rs 负责整份搬迁,这里不能删");
    }

    /// 真的走一遍 COM:建一个指向旧 exe 的 .lnk(带旧 AppUserModelID),改写后读回来。
    /// 纯决策逻辑上面测过了,这条钉的是「读写 .lnk 那层真的能用」。
    #[cfg(windows)]
    #[test]
    fn shortcuts_are_really_rewritten_on_disk() {
        use crate::windows_integration::{ShortcutSpec, create_shortcut, update_shortcuts};
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("codex-plus-plus.exe");
        let current = dir.path().join("recodex.exe");
        std::fs::write(&legacy, b"MZ").unwrap();
        std::fs::write(&current, b"MZ").unwrap();
        let link = dir.path().join("ReCodex.lnk");
        create_shortcut(&ShortcutSpec {
            path: link.clone(),
            target: legacy.clone(),
            arguments: String::new(),
            working_directory: Some(dir.path().to_path_buf()),
            description: "test".into(),
            icon: Some(legacy.clone()),
            show_minimized: false,
        })
        .unwrap();
        // 先把旧 ID 写进去(模拟从窗口固定到任务栏的那种快捷方式)
        let (_, errors) = update_shortcuts(std::slice::from_ref(&link), |_| ShortcutChanges {
            app_user_model_id: Some(LEGACY_CODEX_WINDOW_APP_USER_MODEL_ID.into()),
            ..Default::default()
        });
        assert!(errors.is_empty(), "{errors:?}");

        let (updated, errors) =
            update_shortcuts(std::slice::from_ref(&link), |info| plan_shortcut_changes(info, &legacy, &current));
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(updated, vec![link.clone()]);

        // 读回来:目标、图标、ID 都换了;再跑一遍什么都不改(幂等)
        let seen = std::cell::RefCell::new(ShortcutInfo::default());
        let (updated, _) = update_shortcuts(std::slice::from_ref(&link), |info| {
            *seen.borrow_mut() = info.clone();
            plan_shortcut_changes(info, &legacy, &current)
        });
        assert!(updated.is_empty(), "改过的不该再改");
        let seen = seen.into_inner();
        assert!(same_file_path(&seen.target, &current), "{seen:?}");
        assert!(same_file_path(&seen.icon, &current), "{seen:?}");
        assert_eq!(seen.app_user_model_id, CODEX_WINDOW_APP_USER_MODEL_ID);
    }

    /// 上次认领(改名成 .merging)之后中途退出:下次要接着合并,不能把它丢成孤儿。
    #[test]
    fn an_interrupted_merge_is_resumed_from_the_claimed_file() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("codex-plus.log");
        let current = dir.path().join("recodex.log");
        let claimed = PathBuf::from(format!("{}.merging", legacy.display()));
        write(&claimed, b"pending\n");
        write(&current, b"new\n");

        assert_eq!(merge_leftover_legacy_log(&legacy, &current).unwrap(), Some(8));
        assert_eq!(std::fs::read(&current).unwrap(), b"new\npending\n");
        assert!(!claimed.exists());
    }
}
