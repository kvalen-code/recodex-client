use std::ffi::OsStr;
use std::path::{Path, PathBuf};

#[cfg(windows)]
use anyhow::{Context, bail};

#[derive(Debug, Clone, Copy)]
struct AppPackageSpec {
    identity: &'static str,
    /// manifest 读不到时的兜底 Application Id(见 packaged_app_user_model_id)。
    app_id: &'static str,
    executable_names: &'static [&'static str],
    /// 同机并存多个宿主时的优先级,**数值大者优先**,同优先级再比版本。
    /// ChatGPT-Desktop 的实际优先级是动态的,见 `package_priority`。
    priority: u8,
}

const CODEX_PACKAGE_EXECUTABLES: &[&str] = &["ChatGPT.exe", "Codex.exe", "codex.exe"];
const STANDALONE_CODEX_EXECUTABLES: &[&str] = &["ChatGPT.exe", "Codex.exe", "codex.exe"];

#[cfg(windows)]
const OPENAI_PACKAGE_FAMILY_NAMES: &[&str] = &[
    "OpenAI.Codex_2p2nqsd0c76g0",
    "OpenAI.CodexBeta_2p2nqsd0c76g0",
    "OpenAI.ChatGPT-Desktop_2p2nqsd0c76g0",
];

#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegisteredWindowsPackage {
    pub full_name: String,
    pub install_location: PathBuf,
}

/// Codex 已迁到新版 ChatGPT Desktop 宿主(上游 f4f9bae):两者并存时优先新宿主。
/// 但 OpenAI.ChatGPT-Desktop 这个包名**也是老的纯聊天版 ChatGPT**,老版里没有
/// Codex —— 只有包里真带着 Codex 运行时(`app/resources/codex.exe`)才算宿主,
/// 否则降到最低,绝不能压过真正的 OpenAI.Codex。
const CHATGPT_DESKTOP_CODEX_HOST_PRIORITY: u8 = 2;
const NON_CODEX_HOST_PRIORITY: u8 = 0;

const APP_PACKAGE_SPECS: &[AppPackageSpec] = &[
    AppPackageSpec {
        identity: "OpenAI.Codex",
        app_id: "App",
        executable_names: CODEX_PACKAGE_EXECUTABLES,
        priority: 1,
    },
    AppPackageSpec {
        identity: "OpenAI.CodexBeta",
        app_id: "App",
        executable_names: CODEX_PACKAGE_EXECUTABLES,
        priority: 1,
    },
    AppPackageSpec {
        identity: "OpenAI.ChatGPT-Desktop",
        app_id: "App",
        executable_names: CODEX_PACKAGE_EXECUTABLES,
        priority: CHATGPT_DESKTOP_CODEX_HOST_PRIORITY,
    },
];

pub fn find_latest_codex_app_dir(root: &Path) -> Option<PathBuf> {
    let mut matches = std::fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter_map(|path| {
            let spec = package_spec_from_path(&path)?;
            let version = version_tuple(&path)?;
            let app_dir = package_entry_dir(&path, spec)?;
            Some((package_priority(spec, &app_dir), version, app_dir))
        })
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    let (_, _, latest) = matches.pop()?;
    Some(latest)
}

pub fn find_latest_codex_app_dir_from_roots(roots: &[PathBuf]) -> Option<PathBuf> {
    roots
        .iter()
        .filter_map(|root| find_latest_codex_app_dir(root))
        .max_by(compare_app_dir_candidates)
}

pub fn find_latest_codex_app_dir_default() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        // 系统注册信息是 Store 当前状态的权威来源(上游 f4f9bae / be67b30):
        // WindowsApps 下常有**未注册的残留目录**(商店更新中途/回滚后留下的
        // 更高版本号目录),按目录名版本号挑会挑中它 —— 实测 26.915.4065 残留
        // 目录压过了真正注册的 26.915.3509。注册查询失败(或一个都没有)才退回
        // 目录扫描;何况 WindowsApps 普通用户通常根本列不出来。
        if let Ok(Some(registered)) = find_latest_codex_app_dir_from_appx_package() {
            return Some(registered);
        }
        find_latest_codex_app_dir_from_roots(&windows_app_package_roots())
    }

    #[cfg(not(windows))]
    {
        None
    }
}

#[cfg(windows)]
fn find_latest_codex_app_dir_from_appx_package() -> anyhow::Result<Option<PathBuf>> {
    Ok(latest_registered_app_dir(
        registered_windows_packages()?,
        |_| true,
    ))
}

/// 已注册包里挑最合适的 app 目录;`accept` 用来把范围收窄到某个包身份。
#[cfg(windows)]
fn latest_registered_app_dir(
    packages: Vec<RegisteredWindowsPackage>,
    accept: impl Fn(&AppPackageSpec) -> bool,
) -> Option<PathBuf> {
    packages
        .into_iter()
        .filter(|package| {
            codex_package_parts(&package.full_name).is_some_and(|(spec, _, _)| accept(&spec))
        })
        .filter_map(|package| normalize_codex_app_path(&package.install_location))
        .max_by(compare_app_dir_candidates)
}

/// **不缓存**:原先是进程级 OnceLock,而微信连接 / 手机远程会在启动很久之后
/// 才调 `find_codex_cli` —— 期间商店把 Codex 更新了,拿到的还是旧目录(旧目录
/// 随后被系统删掉,子进程直接起不来)。几次 Win32 调用的开销可以忽略。
#[cfg(windows)]
pub(crate) fn registered_windows_packages() -> anyhow::Result<Vec<RegisteredWindowsPackage>> {
    query_registered_windows_packages()
}

#[cfg(windows)]
fn query_registered_windows_packages() -> anyhow::Result<Vec<RegisteredWindowsPackage>> {
    let mut packages = Vec::new();
    for family_name in OPENAI_PACKAGE_FAMILY_NAMES {
        for full_name in package_full_names_for_family(family_name)? {
            // 单个包查不到路径(比如正在更新/注册中)就跳过它,别连累同族或别的族里
            // 好好的包 —— 原先一个 `?` 整批作废,退回到会挑中残留目录的目录扫描。
            match package_path_by_full_name(&full_name) {
                Ok(install_location) => packages.push(RegisteredWindowsPackage {
                    full_name,
                    install_location,
                }),
                Err(error) => {
                    let _ = crate::diagnostic_log::append_diagnostic_log(
                        "app_paths.registered_package_path_failed",
                        serde_json::json!({
                            "full_name": full_name,
                            "error": format!("{error:#}"),
                        }),
                    );
                }
            }
        }
    }
    Ok(packages)
}

#[cfg(windows)]
fn package_full_names_for_family(family_name: &str) -> anyhow::Result<Vec<String>> {
    use windows::Win32::Foundation::{
        APPMODEL_ERROR_NO_PACKAGE, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS,
    };
    use windows::Win32::Storage::Packaging::Appx::GetPackagesByPackageFamily;
    use windows::core::{PCWSTR, PWSTR};

    let family = family_name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut count = 0u32;
    let mut buffer_length = 0u32;
    let first = unsafe {
        GetPackagesByPackageFamily(
            PCWSTR(family.as_ptr()),
            &mut count,
            None,
            &mut buffer_length,
            PWSTR(std::ptr::null_mut()),
        )
    };
    if first == APPMODEL_ERROR_NO_PACKAGE || (first == ERROR_SUCCESS && count == 0) {
        return Ok(Vec::new());
    }
    if first != ERROR_INSUFFICIENT_BUFFER {
        bail!("GetPackagesByPackageFamily failed with {}", first.0);
    }

    let mut pointers = vec![PWSTR(std::ptr::null_mut()); count as usize];
    let mut buffer = vec![0u16; buffer_length as usize];
    let status = unsafe {
        GetPackagesByPackageFamily(
            PCWSTR(family.as_ptr()),
            &mut count,
            Some(pointers.as_mut_ptr()),
            &mut buffer_length,
            PWSTR(buffer.as_mut_ptr()),
        )
    };
    if status != ERROR_SUCCESS {
        bail!("GetPackagesByPackageFamily failed with {}", status.0);
    }
    buffer.truncate(buffer_length as usize);
    buffer
        .split(|value| *value == 0)
        .filter(|value| !value.is_empty())
        .map(|value| String::from_utf16(value).context("invalid package full name"))
        .collect()
}

#[cfg(windows)]
fn package_path_by_full_name(full_name: &str) -> anyhow::Result<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS};
    use windows::Win32::Storage::Packaging::Appx::GetPackagePathByFullName;
    use windows::core::{PCWSTR, PWSTR};

    let full_name = full_name
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut path_length = 0u32;
    let first = unsafe {
        GetPackagePathByFullName(
            PCWSTR(full_name.as_ptr()),
            &mut path_length,
            PWSTR(std::ptr::null_mut()),
        )
    };
    if first != ERROR_INSUFFICIENT_BUFFER {
        bail!("GetPackagePathByFullName failed with {}", first.0);
    }
    let mut path = vec![0u16; path_length as usize];
    let status = unsafe {
        GetPackagePathByFullName(
            PCWSTR(full_name.as_ptr()),
            &mut path_length,
            PWSTR(path.as_mut_ptr()),
        )
    };
    if status != ERROR_SUCCESS {
        bail!("GetPackagePathByFullName failed with {}", status.0);
    }
    let end = path
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(path.len());
    Ok(PathBuf::from(OsString::from_wide(&path[..end])))
}

#[cfg(windows)]
fn windows_app_package_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        roots.push(PathBuf::from(program_files).join("WindowsApps"));
    }
    if let Some(program_files) = std::env::var_os("ProgramW6432") {
        roots.push(PathBuf::from(program_files).join("WindowsApps"));
    }
    roots.push(PathBuf::from(r"C:\Program Files\WindowsApps"));
    roots.sort();
    roots.dedup();
    roots
}

pub fn user_data_candidates() -> Vec<PathBuf> {
    user_data_candidates_from(
        std::env::var_os("LOCALAPPDATA").as_deref().map(Path::new),
        std::env::var_os("APPDATA").as_deref().map(Path::new),
    )
}

pub fn user_data_candidates_from(local: Option<&Path>, roaming: Option<&Path>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(local) = local {
        append_user_data_variants(&mut candidates, local);
    }
    if let Some(roaming) = roaming {
        append_user_data_variants(&mut candidates, roaming);
    }
    candidates
}

pub fn find_macos_codex_app(search_roots: &[PathBuf]) -> Option<PathBuf> {
    for root in search_roots {
        for candidate in macos_app_candidates(root) {
            if candidate.is_dir() {
                return Some(candidate);
            }
        }
    }
    None
}

pub fn find_macos_codex_app_default() -> Option<PathBuf> {
    let mut roots = vec![PathBuf::from("/Applications")];
    if let Some(home) = directories::BaseDirs::new().map(|dirs| dirs.home_dir().to_path_buf()) {
        roots.push(home.join("Applications"));
    }
    find_macos_codex_app(&roots)
}

pub fn resolve_codex_app_dir(app_dir: Option<&Path>) -> Option<PathBuf> {
    if let Some(app_dir) = app_dir {
        return normalize_codex_app_path(app_dir);
    }
    if cfg!(target_os = "macos") {
        return find_macos_codex_app_default();
    }
    // Windows: try MS Store version first, then standalone install
    find_latest_codex_app_dir_default().or_else(|| find_standalone_codex_app_dir())
}

/// Search for standalone Codex installations (non-MS Store).
///
/// Common paths:
/// - %LOCALAPPDATA%\OpenAI\Codex\bin\  (standalone installer)
/// - %LOCALAPPDATA%\OpenAI\Codex\      (user data root)
/// - %LOCALAPPDATA%\Programs\OpenAI\Codex\ (alternative)
pub fn find_standalone_codex_app_dir() -> Option<PathBuf> {
    let local_appdata = std::env::var_os("LOCALAPPDATA")?;

    let candidates: &[PathBuf] = &[
        PathBuf::from(&local_appdata)
            .join("OpenAI")
            .join("Codex")
            .join("bin"),
        PathBuf::from(&local_appdata).join("OpenAI").join("Codex"),
        PathBuf::from(&local_appdata)
            .join("Programs")
            .join("OpenAI")
            .join("Codex"),
    ];

    for candidate in candidates {
        if let Some(path) = normalize_codex_app_path(candidate) {
            if build_codex_executable(&path).exists() {
                return Some(path);
            }
        }
    }
    None
}

pub fn resolve_codex_app_dir_with_saved(
    app_dir: Option<&Path>,
    saved_app_path: Option<&str>,
) -> Option<PathBuf> {
    if let Some(app_dir) = app_dir {
        // 显式 --app-path 仅接受有效 Codex 应用；无效时不回退，避免静默启动错误目录
        return normalize_codex_app_path(app_dir);
    }
    if let Some(saved) = saved_app_path
        .map(str::trim)
        .filter(|saved| !saved.is_empty())
    {
        // 已保存路径无效（例如误选 Codex++）时回退自动探测
        if let Some(path) = normalize_codex_app_path(Path::new(saved)) {
            #[cfg(windows)]
            if let Some(spec) = package_spec_from_path(&path) {
                // Store 更新会生成新的版本目录:保存的旧目录即使还在,也不该压过
                // 当前注册的版本(上游 be67b30)。但只在**同一个包身份**里换新 ——
                // 用户点名要 OpenAI.Codex,不能因为装了 ChatGPT 就被换走。
                // 注册查询失败/查不到时保留已保存路径,兼容离线或受限环境。
                let current = registered_windows_packages().map(|packages| {
                    latest_registered_app_dir(packages, |candidate| {
                        candidate.identity == spec.identity
                    })
                });
                return Some(resolve_saved_store_path(path, current));
            }
            return Some(path);
        }
    }
    resolve_codex_app_dir(None)
}

/// 激活重试时重新解析包目录:同一包身份下**当前注册**的版本(商店更新期间会变)。
/// 查不到或非 Store 包返回 None,调用方沿用原目录。
pub fn reresolve_packaged_app_dir(app_dir: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let spec = package_spec_from_path(app_dir)?;
        latest_registered_app_dir(registered_windows_packages().ok()?, |candidate| {
            candidate.identity == spec.identity
        })
    }
    #[cfg(not(windows))]
    {
        let _ = app_dir;
        None
    }
}

/// 已保存的 Store 路径 vs 当前注册的同身份包:查到就用当前的,查不到保留原值。
pub fn resolve_saved_store_path(
    saved_path: PathBuf,
    current: anyhow::Result<Option<PathBuf>>,
) -> PathBuf {
    match current {
        Ok(Some(current)) => current,
        Ok(None) | Err(_) => saved_path,
    }
}

pub fn normalize_codex_app_path(path: &Path) -> Option<PathBuf> {
    if path.as_os_str().is_empty() {
        return None;
    }

    // 拒绝把 ReCodex安装目录误当成 Codex 桌面应用
    if is_codex_plus_plus_path(path) {
        return None;
    }

    let file_name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
    if is_supported_app_executable_name(file_name) {
        return path.parent().map(Path::to_path_buf);
    }

    if path.extension() == Some(OsStr::new("app")) {
        return Some(path.to_path_buf());
    }

    if path.is_file() {
        // 任意普通文件不再视为应用根；仅当父目录已是合法 Codex 目录时取父路径
        let parent = path.parent()?;
        return normalize_codex_app_path(parent);
    }

    if executable_in_dir(path).is_some() {
        return Some(path.to_path_buf());
    }

    let nested_app = path.join("app");
    if nested_app.is_dir() {
        if executable_in_dir(&nested_app).is_some() {
            return Some(nested_app);
        }
        // WindowsApps 常因 ACL 无法枚举 exe；只要包名像 OpenAI.Codex_* 仍接受 app\
        if is_codex_store_package_dir(path) {
            return Some(nested_app);
        }
    }

    // 接受 Store 包目录本身（含 …\OpenAI.Codex_*\app）
    if path.is_dir() && is_codex_store_package_dir(path) {
        return Some(path.to_path_buf());
    }

    None
}

/// Codex++ 管理控制台/安装根，绝不能当作 OpenAI Codex 桌面应用。
fn is_codex_plus_plus_path(path: &Path) -> bool {
    for component in path.components() {
        let std::path::Component::Normal(name) = component else {
            continue;
        };
        let Some(name) = name.to_str() else {
            continue;
        };
        let lower = name.to_ascii_lowercase();
        if lower == "codex++"
            || lower == "codexplusplus"
            || lower == "codex-plus-plus"
            || lower.contains("codex-plus-manager")
        {
            return true;
        }
    }
    let normalized = path
        .to_string_lossy()
        .replace('/', "\\")
        .to_ascii_lowercase();
    normalized.contains("\\programs\\codex++")
        || normalized.contains("\\codex++\\")
        || normalized.ends_with("\\codex++")
}

fn is_codex_store_package_dir(path: &Path) -> bool {
    package_spec_from_path(path).is_some()
}

pub fn build_codex_executable(app_dir: &Path) -> PathBuf {
    if app_dir.extension() == Some(OsStr::new("app")) {
        let macos_dir = app_dir.join("Contents").join("MacOS");
        if let Some(executable) = macos_app_plist_value(app_dir, "CFBundleExecutable")
            .filter(|value| !value.contains('/') && !value.contains('\\'))
        {
            return macos_dir.join(executable);
        }
        return macos_dir.join("Codex");
    }
    if let Some(executable) = executable_in_dir(app_dir) {
        return executable;
    }
    if let Some(spec) = package_spec_from_path(app_dir) {
        return app_dir.join(spec.executable_names[0]);
    }
    app_dir.join("Codex.exe")
}

pub fn codex_app_version(app_dir: &Path) -> Option<String> {
    if app_dir.extension() == Some(OsStr::new("app")) {
        return macos_app_version(app_dir);
    }
    let package_dir = if app_dir
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.eq_ignore_ascii_case("app"))
    {
        app_dir.parent()?
    } else {
        app_dir
    };
    codex_package_version(package_dir)
        .or_else(|| codex_directory_version(package_dir))
        .or_else(|| codex_directory_version(app_dir))
        .or_else(|| codex_version_file(package_dir))
        .or_else(|| codex_version_file(app_dir))
}

pub fn packaged_app_user_model_id(app_dir: &Path) -> Option<String> {
    let package_name = package_name_from_app_dir(app_dir)?;
    let (spec, _, publisher_id) = codex_package_parts(&package_name)?;
    if publisher_id.is_empty() {
        return None;
    }
    // 应用段原先写死 `App`。新版 ChatGPT Desktop(1.2026.190.0)改了 manifest 里的
    // Application Id,拿 `App` 去激活直接 0x80270254(上游 2a41afb,#2148;上游
    // main 后来的合并把这段弄丢了,这里按 2a41afb 的意图重写)。
    // 优先读包里真实的 AppxManifest.xml,读不到再退回历史默认值。
    let app_id = packaged_manifest_app_id(app_dir).unwrap_or_else(|| spec.app_id.to_string());
    Some(format!("{}_{publisher_id}!{app_id}", spec.identity))
}

fn packaged_manifest_app_id(app_dir: &Path) -> Option<String> {
    let package_dir = if app_dir
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.eq_ignore_ascii_case("app"))
    {
        app_dir.parent()?
    } else {
        app_dir
    };
    let manifest = std::fs::read_to_string(package_dir.join("AppxManifest.xml")).ok()?;
    manifest_application_id(&manifest)
}

/// AppxManifest.xml 里主程序那个 `<Application>` 的 Id,即 AUMID `!` 之后的部分。
///
/// 一个包可以声明多个 Application —— 实测 OpenAI.Codex 26.915 就有两个:
/// `App`(app/ChatGPT.exe)和 `CodexCoreCommandRunner`(命令执行器)。所以先找
/// Executable 指向主程序(精确大小写的 ChatGPT.exe / Codex.exe、不在 resources 下)
/// 的那个;找不到时取第一个**不是**辅助程序(命令执行器、小写 CLI codex.exe)的,
/// 全是辅助程序才退回第一个。
pub fn manifest_application_id(manifest: &str) -> Option<String> {
    let mut first = None;
    let mut first_plausible = None;
    let mut rest = manifest;
    while let Some(pos) = rest.find("<Application") {
        rest = &rest[pos + "<Application".len()..];
        // 跳过 <Applications> 这类容器节点,只看 <Application ...>。
        if !rest.chars().next().is_some_and(char::is_whitespace) {
            continue;
        }
        let tag_end = rest.find('>')?;
        let tag = &rest[..tag_end];
        rest = &rest[tag_end..];
        let Some(id) = xml_attribute_value(tag, "Id").filter(|id| !id.is_empty()) else {
            continue;
        };
        let executable = xml_attribute_value(tag, "Executable").unwrap_or_default();
        match manifest_application_role(&id, &executable) {
            ManifestApplicationRole::Main => return Some(id),
            ManifestApplicationRole::Other => {
                first_plausible.get_or_insert_with(|| id.clone());
            }
            ManifestApplicationRole::Helper => {}
        }
        first.get_or_insert(id);
    }
    first_plausible.or(first)
}

enum ManifestApplicationRole {
    /// 桌面主程序:`ChatGPT.exe` / `Codex.exe`,**精确大小写**,不在 resources 下。
    Main,
    /// 命令执行器、CLI 之类的辅助程序:宁可退到第一个也不选它们。
    Helper,
    Other,
}

/// 大小写要精确:包里的 `app/resources/codex.exe`(小写)是 CLI,不能被
/// `eq_ignore_ascii_case("Codex.exe")` 当成主程序;命令执行器
/// (CodexCoreCommandRunner / *command-runner*)同理。
fn manifest_application_role(id: &str, executable: &str) -> ManifestApplicationRole {
    let normalized = executable.replace('\\', "/");
    let file_name = normalized.rsplit('/').next().unwrap_or_default();
    let lower_path = normalized.to_ascii_lowercase();
    let under_resources = lower_path.split('/').any(|segment| segment == "resources");
    let lower_id = id.to_ascii_lowercase();
    let helper = under_resources
        || lower_id.contains("commandrunner")
        || lower_path.contains("command-runner")
        || file_name == "codex.exe";
    if helper {
        ManifestApplicationRole::Helper
    } else if file_name == "ChatGPT.exe" || file_name == "Codex.exe" {
        ManifestApplicationRole::Main
    } else {
        ManifestApplicationRole::Other
    }
}

fn xml_attribute_value(tag: &str, name: &str) -> Option<String> {
    let mut rest = tag;
    while let Some(pos) = rest.find(name) {
        let before = rest[..pos].chars().next_back();
        let after = &rest[pos + name.len()..];
        rest = after;
        // 必须是完整属性名:前面是空白、后面(可隔空白)紧跟 `=`。
        if !before.is_some_and(char::is_whitespace) {
            continue;
        }
        let Some(value) = after.trim_start().strip_prefix('=') else {
            continue;
        };
        let value = value.trim_start();
        let quote = value.chars().next().filter(|ch| *ch == '"' || *ch == '\'')?;
        let value = &value[1..];
        let end = value.find(quote)?;
        return Some(value[..end].to_string());
    }
    None
}

fn package_name_from_app_dir(app_dir: &Path) -> Option<String> {
    let path = app_dir.to_string_lossy().replace('\\', "/");
    let mut parts = path.split('/').filter(|part| !part.is_empty());
    let mut package_name = parts.next_back()?;
    if package_name.eq_ignore_ascii_case("app") {
        package_name = parts.next_back()?;
    }
    Some(package_name.to_string())
}

fn codex_package_version(package_dir: &Path) -> Option<String> {
    let path = package_dir.to_string_lossy().replace('\\', "/");
    let name = path
        .split('/')
        .rev()
        .find(|part| codex_package_parts(part).is_some())?;
    let (_, version, _) = codex_package_parts(name)?;
    if version.is_empty() {
        None
    } else {
        Some(version.to_string())
    }
}

fn codex_directory_version(app_dir: &Path) -> Option<String> {
    directory_version(app_dir).or_else(|| {
        app_dir
            .canonicalize()
            .ok()
            .and_then(|path| directory_version(&path))
    })
}

fn directory_version(path: &Path) -> Option<String> {
    let version = path.file_name()?.to_str()?;
    if is_version_like(version) {
        Some(version.to_string())
    } else {
        None
    }
}

fn is_version_like(version: &str) -> bool {
    let mut parts = version.split('.');
    let Some(first) = parts.next() else {
        return false;
    };
    if first.is_empty() || !first.chars().all(|ch| ch.is_ascii_digit()) {
        return false;
    }
    let mut count = 1;
    for part in parts {
        if part.is_empty() || !part.chars().all(|ch| ch.is_ascii_digit()) {
            return false;
        }
        count += 1;
    }
    count >= 2
}

fn codex_version_file(app_dir: &Path) -> Option<String> {
    let version = std::fs::read_to_string(app_dir.join("version")).ok()?;
    let version = version.trim();
    if version.is_empty() {
        None
    } else {
        Some(version.to_string())
    }
}

fn macos_app_version(app_dir: &Path) -> Option<String> {
    macos_app_plist_value(app_dir, "CFBundleShortVersionString")
        .or_else(|| macos_app_plist_value(app_dir, "CFBundleVersion"))
}

fn macos_app_plist_value(app_dir: &Path, key: &str) -> Option<String> {
    let plist = std::fs::read_to_string(app_dir.join("Contents").join("Info.plist")).ok()?;
    plist_string_value(&plist, key)
}

fn plist_string_value(plist: &str, key: &str) -> Option<String> {
    let (_, after_key) = plist.split_once(&format!("<key>{key}</key>"))?;
    let (_, after_string_open) = after_key.split_once("<string>")?;
    let (value, _) = after_string_open.split_once("</string>")?;
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn append_user_data_variants(candidates: &mut Vec<PathBuf>, base: &Path) {
    candidates.push(base.join("OpenAI").join("ChatGPT"));
    candidates.push(base.join("OpenAI.ChatGPT-Desktop"));
    candidates.push(base.join("ChatGPT"));
    candidates.push(base.join("OpenAI").join("Codex"));
    candidates.push(base.join("OpenAI.Codex"));
    candidates.push(base.join("Codex"));
}

fn macos_app_candidates(root: &Path) -> Vec<PathBuf> {
    if root.extension() == Some(OsStr::new("app")) {
        return vec![root.to_path_buf()];
    }
    [
        "Codex.app",
        "OpenAI Codex.app",
        "OpenAI.Codex.app",
        "ChatGPT.app",
    ]
    .into_iter()
    .map(|name| root.join(name))
    .collect()
}

fn version_tuple(path: &Path) -> Option<Vec<u32>> {
    let name = path.file_name()?.to_str()?;
    let (_, version, _) = codex_package_parts(name)?;
    let parts = version
        .split('.')
        .map(str::parse::<u32>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    if parts.is_empty() { None } else { Some(parts) }
}

pub(crate) fn is_supported_windows_app_package_name(package_name: &str) -> bool {
    codex_package_parts(package_name).is_some()
}

pub(crate) fn is_supported_app_executable_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("Codex.exe") || name.eq_ignore_ascii_case("ChatGPT.exe")
}

fn package_spec_from_path(path: &Path) -> Option<AppPackageSpec> {
    let package_name = package_name_from_app_dir(path)?;
    let (spec, _, _) = codex_package_parts(&package_name)?;
    Some(spec)
}

fn compare_app_dir_candidates(left: &PathBuf, right: &PathBuf) -> std::cmp::Ordering {
    app_dir_sort_key(left).cmp(&app_dir_sort_key(right))
}

fn app_dir_sort_key(app_dir: &Path) -> Option<(u8, Vec<u32>)> {
    let spec = package_spec_from_path(app_dir)?;
    let package_dir = if app_dir
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.eq_ignore_ascii_case("app"))
    {
        app_dir.parent().unwrap_or(app_dir)
    } else {
        app_dir
    };
    Some((package_priority(spec, app_dir), version_tuple(package_dir)?))
}

/// 见 `CHATGPT_DESKTOP_CODEX_HOST_PRIORITY`:ChatGPT-Desktop 只有真带 Codex
/// 运行时才算宿主。`app_dir` 可以是包目录或其下的 `app`。
fn package_priority(spec: AppPackageSpec, app_dir: &Path) -> u8 {
    if spec.priority != CHATGPT_DESKTOP_CODEX_HOST_PRIORITY {
        return spec.priority;
    }
    let entry = if app_dir
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| name.eq_ignore_ascii_case("app"))
    {
        app_dir.to_path_buf()
    } else {
        app_dir.join("app")
    };
    if app_entry_hosts_codex_runtime(&entry) {
        spec.priority
    } else {
        NON_CODEX_HOST_PRIORITY
    }
}

/// 包的 `app` 目录下有没有 Codex 运行时(`resources/codex.exe`)。
pub(crate) fn app_entry_hosts_codex_runtime(app_entry: &Path) -> bool {
    let resources = app_entry.join("resources");
    resources.join("codex.exe").is_file() || resources.join("codex").is_file()
}

/// 这个包名是不是「可能只是纯聊天版」的 OpenAI.ChatGPT-Desktop:它的进程只有在包里
/// 真带着 Codex 运行时才算 Codex(见 `CHATGPT_DESKTOP_CODEX_HOST_PRIORITY`)。
pub(crate) fn package_needs_codex_runtime_check(package_name: &str) -> bool {
    codex_package_parts(package_name)
        .is_some_and(|(spec, _, _)| spec.priority == CHATGPT_DESKTOP_CODEX_HOST_PRIORITY)
}

fn package_entry_dir(package_dir: &Path, spec: AppPackageSpec) -> Option<PathBuf> {
    let app = package_dir.join("app");
    if app.is_dir() {
        return Some(app);
    }
    for name in spec.executable_names {
        if package_dir.join(name).is_file() {
            return Some(package_dir.to_path_buf());
        }
    }
    None
}

fn executable_in_dir(dir: &Path) -> Option<PathBuf> {
    let names = package_spec_from_path(dir)
        .map(|spec| spec.executable_names)
        .unwrap_or(STANDALONE_CODEX_EXECUTABLES);
    for name in names {
        let candidate = dir.join(name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn codex_package_parts(package_name: &str) -> Option<(AppPackageSpec, &str, &str)> {
    for spec in APP_PACKAGE_SPECS {
        let Some(rest) = strip_prefix_ignore_ascii_case(package_name, spec.identity) else {
            continue;
        };
        let Some(rest) = rest.strip_prefix('_') else {
            continue;
        };
        let Some((version, rest)) = rest.split_once('_') else {
            continue;
        };
        // full name 形如 `Name_Version_Arch_ResourceId_PublisherId`:ResourceId 通常
        // 为空(`x64__pub`),也可能是 `~`(`neutral_~_pub`)。取最后一段即可。
        let Some((_, publisher_id)) = rest.rsplit_once('_') else {
            continue;
        };
        if publisher_id.is_empty() {
            continue;
        }
        return Some((*spec, version, publisher_id));
    }
    None
}

fn strip_prefix_ignore_ascii_case<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    if value.len() < prefix.len() {
        return None;
    }
    let (head, rest) = value.split_at(prefix.len());
    head.eq_ignore_ascii_case(prefix).then_some(rest)
}

/// 找到官方 Codex **命令行**二进制(`codex`),而不是 GUI 应用。
///
/// 微信连接跑的是 `codex app-server`,原先直接 `Command::new("codex")` 靠 PATH 找 ——
/// 而这条路在两个最常见的装法上都走不通:
///   - macOS:客户端装在 `/Applications/*.app` 里,codex 在应用包内部,从来不在 PATH;
///     而且启动器是 GUI 进程,继承的 PATH 只有 `/usr/bin:/bin:/usr/sbin:/sbin`,
///     用户在 shell 里配的路径它一概看不到。
///   - Windows:从商店(MSIX)装的在 `WindowsApps\...\app\resources\codex.exe`,同样不在 PATH。
/// 结果就是微信一发消息就报「无法启动 Codex app-server」。
///
/// 我们本来就知道 Codex 客户端装在哪 —— 直接去它旁边取,取不到再回落 PATH。
/// Windows 上 Codex 自己解出来的那份**可执行**的 CLI。
///
/// 商店(MSIX)装的 codex.exe 在 `C:\Program Files\WindowsApps\...` 下:
/// 文件确实在,`is_file()` 为真 —— 但那个目录的 ACL 不允许普通进程执行它,
/// CreateProcess 直接 Access is denied。于是「找到了」和「能跑」是两回事,
/// 微信一发消息就报「无法启动 Codex app-server」,而报错里印的路径明明存在,
/// 让人以为是路径配错了。
///
/// 应用启动时会把一份可执行的 CLI 解到 LOCALAPPDATA,优先用它。
#[cfg(windows)]
fn windows_unpacked_codex_cli() -> Option<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    let bin = Path::new(&base).join("OpenAI").join("Codex").join("bin");
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in std::fs::read_dir(&bin).ok()?.flatten() {
        let candidate = entry.path().join("codex.exe");
        if !candidate.is_file() {
            continue;
        }
        // 目录名是版本哈希,字典序认不出新旧 —— 按修改时间取最新的那份。
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else {
            continue;
        };
        if newest.as_ref().is_none_or(|(seen, _)| modified > *seen) {
            newest = Some((modified, candidate));
        }
    }
    newest.map(|(_, path)| path)
}

#[cfg(not(windows))]
fn windows_unpacked_codex_cli() -> Option<PathBuf> {
    None
}

pub fn find_codex_cli(app_dir: Option<&Path>) -> Option<PathBuf> {
    // 调用方点名了目录 = 「就在这里面找」,找不到就是找不到 —— 不能替他
    // 换一个别处的二进制,那会让「我指定了路径」这件事失去意义。
    // 解包副本的回落只用于自动发现(app_dir 为 None)。
    let autodiscover = app_dir.is_none();
    let app_dir = resolve_codex_app_dir_with_saved(app_dir, None)?;
    let exe_name = if cfg!(windows) { "codex.exe" } else { "codex" };
    // 两种布局:macOS 的 .app 包,和 Windows 的 app 目录
    let candidates = [
        app_dir.join("Contents").join("Resources").join(exe_name),
        app_dir.join("Contents").join("MacOS").join(exe_name),
        app_dir.join("resources").join(exe_name),
        app_dir.join(exe_name),
    ];
    let found: Vec<PathBuf> = candidates.into_iter().filter(|path| path.is_file()).collect();
    // 先要一个**能执行**的。WindowsApps 下那份文件在、却跑不了,
    // 直接返回它等于把「找到了」当成「能用」—— 用户看到的报错里印着一个
    // 确实存在的路径,只会去查路径配错没有。
    if let Some(path) = found.iter().find(|path| !is_msix_restricted(path)) {
        return Some(path.clone());
    }
    // 应用目录里只有那份跑不了的 —— 用 Codex 自己解包出来的。
    if autodiscover {
        if let Some(path) = windows_unpacked_codex_cli() {
            return Some(path);
        }
    }
    // 两条都没有就把它交出去:让上层报「启动失败」,好过报「根本没找到」。
    found.into_iter().next()
}

/// 这个路径是不是在 MSIX 的受限目录下。
///
/// 只认目录,不做真正的执行权限探测:那要么起一个进程(有副作用),要么读 ACL
/// (Windows 专有、一堆边界)。而线上唯一踩到的就是这一个目录,收窄到它
/// 既准确又不会误伤别的安装方式。
fn is_msix_restricted(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_str()
            .is_some_and(|name| name.eq_ignore_ascii_case("WindowsApps"))
    })
}

/// 微信连接要用的 codex 命令:用户显式配了就用他的,否则去 Codex 客户端旁边找,
/// 再不行才回落 PATH 上的 `codex`(留着这一层是因为有人确实单独装过 CLI)。
pub fn codex_cli_command(configured: &str) -> String {
    let configured = configured.trim();
    if !configured.is_empty() {
        return configured.to_string();
    }
    find_codex_cli(None)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "codex".to_string())
}
