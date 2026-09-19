use std::io::{Cursor, Read};
use std::path::{Component, Path, PathBuf};

use anyhow::Context;
use toml_edit::{DocumentMut, Item, Table};

const OPENAI_CURATED_MARKETPLACE: &str = "openai-curated";
const OPENAI_API_CURATED_MARKETPLACE: &str = "openai-api-curated";
const OPENAI_CURATED_REMOTE_MARKETPLACE: &str = "openai-curated-remote";
const ROLE_SPECIFIC_PLUGINS_MARKETPLACE: &str = "role-specific-plugins";
const OPENAI_PLUGINS_ZIP_URL: &str =
    "https://codeload.github.com/openai/plugins/zip/refs/heads/main";
const OPENAI_PLUGINS_DOWNLOAD_LIMIT_BYTES: usize = 128 * 1024 * 1024;
const OPENAI_CURATED_REMOTE_MARKETPLACE_ZIP: &[u8] =
    include_bytes!("../../../assets/plugin-marketplaces/openai-curated-remote.zip");
/// recodex-overlay: 内嵌快照里 marketplace.json 的简介原先写着 Codex++,已在 zip 里改掉。
/// 但快照只在目录不存在时解压一次 —— 以前解压过的机器上那份永远是旧文案。
/// 启动时就地把**这一整句**换掉(只认原句,别的内容一个字节不动;Codex 自己也会写这个目录)。
const LEGACY_REMOTE_MARKETPLACE_DESCRIPTION: &str =
    "Official ChatGPT-auth remote plugins cached locally by Codex++.";
const REMOTE_MARKETPLACE_DESCRIPTION: &str =
    "Official ChatGPT-auth remote plugins cached locally by ReCodex.";

/// 已解压的远程插件市场里还是旧品牌简介时,返回改好的文本;否则 None。
fn rebrand_remote_marketplace_text(text: &str) -> Option<String> {
    text.contains(LEGACY_REMOTE_MARKETPLACE_DESCRIPTION)
        .then(|| text.replace(LEGACY_REMOTE_MARKETPLACE_DESCRIPTION, REMOTE_MARKETPLACE_DESCRIPTION))
}

/// 把 `~/.codex/.tmp/plugins-remote` 里旧快照的简介改成新品牌。返回是否改写了文件。
/// 先写临时文件再改名,不留半截 JSON。
fn refresh_remote_marketplace_branding(home: &Path) -> anyhow::Result<bool> {
    let path = home
        .join(".tmp")
        .join("plugins-remote")
        .join(".agents")
        .join("plugins")
        .join("marketplace.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(false);
    };
    let Some(updated) = rebrand_remote_marketplace_text(&text) else {
        return Ok(false);
    };
    let staging = path.with_extension("json.recodex-tmp");
    std::fs::write(&staging, updated)
        .with_context(|| format!("failed to write {}", staging.display()))?;
    if let Err(error) = std::fs::rename(&staging, &path) {
        let _ = std::fs::remove_file(&staging);
        return Err(error).with_context(|| format!("failed to replace {}", path.display()));
    }
    Ok(true)
}

pub fn ensure_openai_curated_marketplace_config(home: &Path) -> anyhow::Result<bool> {
    let _ = refresh_remote_marketplace_branding(home);
    let Some(marketplace_root) = local_openai_curated_marketplace_root(home)? else {
        return Ok(false);
    };
    let mut changed = ensure_marketplace_configs(
        home,
        &[OPENAI_CURATED_MARKETPLACE, OPENAI_API_CURATED_MARKETPLACE],
        &marketplace_root,
    )?;
    if let Some(remote_marketplace_root) = local_openai_curated_remote_marketplace_root(home)? {
        changed |= ensure_marketplace_configs(
            home,
            &[OPENAI_CURATED_REMOTE_MARKETPLACE],
            &remote_marketplace_root,
        )?;
    }
    Ok(changed)
}

/// 启动时对 `~/.codex/.tmp/plugins*` 做的非配置类维护(目前只有旧品牌简介改写)。
/// 不写 config.toml。
pub fn refresh_marketplace_branding(home: &Path) -> bool {
    refresh_remote_marketplace_branding(home).unwrap_or(false)
}

/// 清理结果:删了哪些条目、备份放在哪。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReservedMarketplaceCleanup {
    pub removed: Vec<String>,
    pub backup: Option<PathBuf>,
}

/// 清掉**我们**以前写进 config.toml 的 `[marketplaces.openai-curated]` /
/// `[marketplaces.openai-api-curated]`。
///
/// 为什么不再写(1.3.8 起):
///   - `openai-*` 是 Codex 的保留名,注册在它们下面的本地 marketplace 会被静默
///     忽略(上游 02a23e1 实测)—— 这两条从来没起过作用;
///   - 插件市场解锁靠的是注入脚本的桥补丁:helper 直接读 `~/.codex/.tmp/plugins`
///     下的 marketplace.json / api_marketplace.json 注入页面(assets.rs
///     `local_plugin_marketplaces`),再在 `list-plugins` 请求/响应上合并
///     (plugin_marketplace_bridge_patch_installed / plugin_marketplace_local_merged),
///     全程不经过 config.toml;
///   - 它还是 config.toml 的又一个写入方,绕开了托管块写入契约(codexcfg 的
///     校验/备份那一套)。
///
/// 只删能证明是我们写的:表里恰好只有 `source_type = "local"` 和 `source`,且
/// `source` 指向我们登记的那个目录(`<home>/.tmp/plugins`,可带 `\\?\` 前缀)。
/// 用户手工加的、Codex 自己写的(比如 `openai-bundled` / `openai-primary-runtime`)
/// 一概不碰。第一次真要删之前把整份 config.toml 备份到
/// `config.toml.recodex-marketplace-cleanup.bak`(已存在就不覆盖,留住最初的样子)。
pub fn cleanup_recodex_reserved_marketplace_configs(
    home: &Path,
) -> anyhow::Result<ReservedMarketplaceCleanup> {
    let config_path = home.join("config.toml");
    let existing = match std::fs::read(&config_path) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => text,
            // 不是 UTF-8 就不是我们能安全改的文件,别动。
            Err(_) => return Ok(ReservedMarketplaceCleanup::default()),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ReservedMarketplaceCleanup::default());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", config_path.display()));
        }
    };
    let Some((updated, removed)) = remove_recodex_reserved_marketplaces(&existing, home) else {
        return Ok(ReservedMarketplaceCleanup::default());
    };
    // 结果必须还能被读回去,读不回去就放弃(与 codexcfg 的 refuse_if_would_break 同一条闸)。
    if updated.trim_start_matches('\u{feff}').parse::<DocumentMut>().is_err() {
        anyhow::bail!("refusing to write config.toml: cleanup result would not parse");
    }
    let backup = config_path.with_file_name("config.toml.recodex-marketplace-cleanup.bak");
    if !backup.exists() {
        crate::settings::atomic_write(&backup, existing.as_bytes())
            .with_context(|| format!("failed to back up {}", config_path.display()))?;
    }
    crate::settings::atomic_write(&config_path, updated.as_bytes())?;
    Ok(ReservedMarketplaceCleanup {
        removed,
        backup: Some(backup),
    })
}

/// 纯文本版本:返回 (改后的文本, 删掉的条目名);没有可删的返回 None。
/// 用 toml_edit 就地删表,其余内容(注释、顺序、格式)原样保留。
pub fn remove_recodex_reserved_marketplaces(
    config_text: &str,
    home: &Path,
) -> Option<(String, Vec<String>)> {
    let had_bom = config_text.starts_with('\u{feff}');
    let mut doc = config_text
        .trim_start_matches('\u{feff}')
        .parse::<DocumentMut>()
        .ok()?;
    let managed_root = home.join(".tmp").join("plugins");
    let marketplaces = doc.get_mut("marketplaces")?.as_table_mut()?;
    let mut removed = Vec::new();
    for name in [OPENAI_CURATED_MARKETPLACE, OPENAI_API_CURATED_MARKETPLACE] {
        let ours = marketplaces
            .get(name)
            .and_then(Item::as_table)
            .is_some_and(|table| is_recodex_marketplace_table(table, &managed_root));
        if ours {
            marketplaces.remove(name);
            removed.push(name.to_string());
        }
    }
    if removed.is_empty() {
        return None;
    }
    // `[marketplaces]` 本身若只是个隐式父表且已空,一并去掉;显式写过的空表保留。
    if marketplaces.is_empty() && marketplaces.is_implicit() {
        doc.as_table_mut().remove("marketplaces");
    }
    let mut text = doc.to_string();
    if had_bom {
        text.insert(0, '\u{feff}');
    }
    Some((text, removed))
}

fn is_recodex_marketplace_table(table: &Table, managed_root: &Path) -> bool {
    if table.len() != 2 {
        return false;
    }
    let source_type = table.get("source_type").and_then(Item::as_str);
    let source = table.get("source").and_then(Item::as_str);
    let (Some("local"), Some(source)) = (source_type, source) else {
        return false;
    };
    let normalize = |value: &str| {
        normalize_windows_extended_path(value)
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_string()
    };
    let expected = normalize(&managed_root.to_string_lossy());
    let actual = normalize(source);
    if cfg!(windows) {
        actual.eq_ignore_ascii_case(&expected)
    } else {
        actual == expected
    }
}

pub fn ensure_openai_curated_remote_marketplace_config(home: &Path) -> anyhow::Result<bool> {
    let Some(marketplace_root) = local_openai_curated_remote_marketplace_root(home)? else {
        return Ok(false);
    };
    ensure_marketplace_configs(
        home,
        &[OPENAI_CURATED_REMOTE_MARKETPLACE],
        &marketplace_root,
    )
}

pub fn ensure_role_specific_plugins_marketplace_config(home: &Path) -> anyhow::Result<bool> {
    let Some(marketplace_root) = local_role_specific_plugins_marketplace_root(home)? else {
        return Ok(false);
    };
    let plugin_ids =
        local_marketplace_plugin_names(&marketplace_root, ROLE_SPECIFIC_PLUGINS_MARKETPLACE)?
            .into_iter()
            .map(|name| format!("{name}@{ROLE_SPECIFIC_PLUGINS_MARKETPLACE}"))
            .collect::<Vec<_>>();
    ensure_marketplace_configs_with_plugins(
        home,
        &[ROLE_SPECIFIC_PLUGINS_MARKETPLACE],
        &marketplace_root,
        &plugin_ids,
    )
}

pub fn ensure_openai_curated_remote_marketplace_available(
    home: &Path,
) -> anyhow::Result<MarketplaceEnsureResult> {
    let mut initialized = false;
    let _ = refresh_remote_marketplace_branding(home);
    if local_openai_curated_remote_marketplace_root(home)?.is_none() {
        install_openai_curated_remote_marketplace_zip(home, OPENAI_CURATED_REMOTE_MARKETPLACE_ZIP)?;
        initialized = true;
    }
    let configured = ensure_openai_curated_remote_marketplace_config(home)?;
    Ok(MarketplaceEnsureResult {
        initialized,
        configured,
    })
}

pub fn preserve_openai_curated_remote_marketplace_config(
    home: &Path,
    config_text: &str,
) -> anyhow::Result<String> {
    let Some(marketplace_root) = local_openai_curated_remote_marketplace_root(home)? else {
        return Ok(config_text.to_string());
    };
    merge_marketplace_configs_into_text(
        config_text,
        &[OPENAI_CURATED_REMOTE_MARKETPLACE],
        &marketplace_root,
    )
}

pub fn openai_curated_marketplace_status(home: &Path) -> MarketplaceStatus {
    let marketplace_root = local_openai_curated_marketplace_root(home).ok().flatten();
    let remote_marketplace_root = local_openai_curated_remote_marketplace_root(home)
        .ok()
        .flatten();
    let config_registered = marketplace_root
        .as_deref()
        .map(|root| {
            marketplace_config_points_to_root(home, OPENAI_CURATED_MARKETPLACE, root)
                && marketplace_config_points_to_root(home, OPENAI_API_CURATED_MARKETPLACE, root)
                && remote_marketplace_root
                    .as_deref()
                    .map(|remote_root| {
                        marketplace_config_points_to_root(
                            home,
                            OPENAI_CURATED_REMOTE_MARKETPLACE,
                            remote_root,
                        )
                    })
                    .unwrap_or(true)
        })
        .unwrap_or(false);
    MarketplaceStatus {
        marketplace_root,
        config_registered,
    }
}

pub fn openai_curated_remote_marketplace_status(home: &Path) -> MarketplaceStatus {
    let marketplace_root = local_openai_curated_remote_marketplace_root(home)
        .ok()
        .flatten();
    let config_registered = marketplace_root
        .as_deref()
        .map(|root| {
            marketplace_config_points_to_root(home, OPENAI_CURATED_REMOTE_MARKETPLACE, root)
        })
        .unwrap_or(false);
    MarketplaceStatus {
        marketplace_root,
        config_registered,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceStatus {
    pub marketplace_root: Option<PathBuf>,
    pub config_registered: bool,
}

impl MarketplaceStatus {
    pub fn needs_repair(&self) -> bool {
        self.marketplace_root.is_none() || !self.config_registered
    }
}

pub async fn initialize_openai_curated_marketplace_and_configure(
    home: &Path,
) -> anyhow::Result<MarketplaceEnsureResult> {
    let mut initialized = false;
    if local_openai_curated_marketplace_root(home)?.is_none() {
        initialize_openai_curated_marketplace_from_github(home).await?;
        initialized = true;
    }
    let configured = ensure_openai_curated_marketplace_config(home)?;
    Ok(MarketplaceEnsureResult {
        initialized,
        configured,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarketplaceEnsureResult {
    pub initialized: bool,
    pub configured: bool,
}

fn local_openai_curated_marketplace_root(home: &Path) -> anyhow::Result<Option<PathBuf>> {
    let root = home.join(".tmp").join("plugins");
    let marketplace_path = root
        .join(".agents")
        .join("plugins")
        .join("marketplace.json");
    if !marketplace_path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&marketplace_path)
        .with_context(|| format!("failed to read {}", marketplace_path.display()))?;
    let marketplace: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse {}", marketplace_path.display()))?;
    if marketplace.get("name").and_then(serde_json::Value::as_str)
        != Some(OPENAI_CURATED_MARKETPLACE)
    {
        return Ok(None);
    }
    let has_plugins = marketplace
        .get("plugins")
        .and_then(serde_json::Value::as_array)
        .map(|plugins| !plugins.is_empty())
        .unwrap_or(false);
    if !has_plugins || !root.join("plugins").is_dir() {
        return Ok(None);
    }
    Ok(Some(root))
}

fn local_role_specific_plugins_marketplace_root(home: &Path) -> anyhow::Result<Option<PathBuf>> {
    let root = home
        .join(".tmp")
        .join("marketplaces")
        .join(ROLE_SPECIFIC_PLUGINS_MARKETPLACE);
    local_marketplace_root_from_root(&root, ROLE_SPECIFIC_PLUGINS_MARKETPLACE)
}

fn local_marketplace_root_from_root(
    root: &Path,
    marketplace_name: &str,
) -> anyhow::Result<Option<PathBuf>> {
    let marketplace_path = root
        .join(".agents")
        .join("plugins")
        .join("marketplace.json");
    if !marketplace_path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&marketplace_path)
        .with_context(|| format!("failed to read {}", marketplace_path.display()))?;
    let marketplace: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse {}", marketplace_path.display()))?;
    if marketplace.get("name").and_then(serde_json::Value::as_str) != Some(marketplace_name) {
        return Ok(None);
    }
    let has_plugins = marketplace
        .get("plugins")
        .and_then(serde_json::Value::as_array)
        .map(|plugins| !plugins.is_empty())
        .unwrap_or(false);
    if !has_plugins || !root.join("plugins").is_dir() {
        return Ok(None);
    }
    Ok(Some(root.to_path_buf()))
}

fn local_marketplace_plugin_names(
    root: &Path,
    marketplace_name: &str,
) -> anyhow::Result<Vec<String>> {
    let marketplace_path = root
        .join(".agents")
        .join("plugins")
        .join("marketplace.json");
    let text = std::fs::read_to_string(&marketplace_path)
        .with_context(|| format!("failed to read {}", marketplace_path.display()))?;
    let marketplace: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse {}", marketplace_path.display()))?;
    if marketplace.get("name").and_then(serde_json::Value::as_str) != Some(marketplace_name) {
        return Ok(Vec::new());
    }
    Ok(marketplace
        .get("plugins")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|plugin| {
            plugin
                .get("name")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string)
        })
        .collect())
}

fn local_openai_curated_remote_marketplace_root(home: &Path) -> anyhow::Result<Option<PathBuf>> {
    let root = home.join(".tmp").join("plugins-remote");
    let marketplace_path = root
        .join(".agents")
        .join("plugins")
        .join("marketplace.json");
    if !marketplace_path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&marketplace_path)
        .with_context(|| format!("failed to read {}", marketplace_path.display()))?;
    let marketplace: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse {}", marketplace_path.display()))?;
    if marketplace.get("name").and_then(serde_json::Value::as_str)
        != Some(OPENAI_CURATED_REMOTE_MARKETPLACE)
    {
        return Ok(None);
    }
    let has_plugins = marketplace
        .get("plugins")
        .and_then(serde_json::Value::as_array)
        .map(|plugins| !plugins.is_empty())
        .unwrap_or(false);
    if !has_plugins || !root.join("plugins").is_dir() {
        return Ok(None);
    }
    Ok(Some(root))
}

async fn initialize_openai_curated_marketplace_from_github(home: &Path) -> anyhow::Result<()> {
    let bytes = download_openai_plugins_zip().await?;
    install_openai_plugins_zip(home, &bytes)
}

async fn download_openai_plugins_zip() -> anyhow::Result<Vec<u8>> {
    let client =
        crate::http_client::proxied_client(&format!("ReCodex/{}", crate::version::VERSION))?;
    let bytes = client
        .get(OPENAI_PLUGINS_ZIP_URL)
        .header(reqwest::header::ACCEPT, "application/zip")
        .send()
        .await
        .context("failed to download openai/plugins marketplace")?
        .error_for_status()
        .context("openai/plugins marketplace download returned an error status")?
        .bytes()
        .await
        .context("failed to read openai/plugins marketplace download body")?;
    if bytes.len() > OPENAI_PLUGINS_DOWNLOAD_LIMIT_BYTES {
        anyhow::bail!(
            "openai/plugins marketplace download is too large: {} bytes",
            bytes.len()
        );
    }
    Ok(bytes.to_vec())
}

fn install_openai_plugins_zip(home: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let destination = home.join(".tmp").join("plugins");
    let staging_parent = home.join(".tmp");
    std::fs::create_dir_all(&staging_parent)
        .with_context(|| format!("failed to create {}", staging_parent.display()))?;
    let staging = staging_parent.join(format!(
        "plugins-download-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    ));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)
            .with_context(|| format!("failed to remove stale {}", staging.display()))?;
    }
    std::fs::create_dir_all(&staging)
        .with_context(|| format!("failed to create {}", staging.display()))?;

    let result = extract_openai_plugins_zip(bytes, &staging)
        .and_then(|_| validate_openai_plugins_marketplace_root(&staging))
        .and_then(|_| replace_directory(&staging, &destination));
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result
}

fn install_openai_curated_remote_marketplace_zip(home: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let destination = home.join(".tmp").join("plugins-remote");
    let staging_parent = home.join(".tmp");
    std::fs::create_dir_all(&staging_parent)
        .with_context(|| format!("failed to create {}", staging_parent.display()))?;
    let staging = staging_parent.join(format!(
        "plugins-remote-embedded-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    ));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)
            .with_context(|| format!("failed to remove stale {}", staging.display()))?;
    }
    std::fs::create_dir_all(&staging)
        .with_context(|| format!("failed to create {}", staging.display()))?;

    let result = extract_zip_exact(bytes, &staging)
        .and_then(|_| validate_openai_curated_remote_marketplace_root(&staging))
        .and_then(|_| {
            replace_directory_with_backup_name(
                &staging,
                &destination,
                "plugins-remote.previous-codex-plus",
            )
        });
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result
}

fn extract_openai_plugins_zip(bytes: &[u8], destination: &Path) -> anyhow::Result<()> {
    let cursor = Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor).context("failed to read openai/plugins zip")?;
    for index in 0..archive.len() {
        let mut file = archive
            .by_index(index)
            .with_context(|| format!("failed to read zip entry {index}"))?;
        let Some(relative_path) = zip_entry_relative_path(file.name()) else {
            continue;
        };
        let output_path = destination.join(relative_path);
        if file.is_dir() {
            std::fs::create_dir_all(&output_path)
                .with_context(|| format!("failed to create {}", output_path.display()))?;
            continue;
        }
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let mut contents = Vec::new();
        file.read_to_end(&mut contents)
            .with_context(|| format!("failed to read zip entry {}", file.name()))?;
        std::fs::write(&output_path, contents)
            .with_context(|| format!("failed to write {}", output_path.display()))?;
    }
    Ok(())
}

fn extract_zip_exact(bytes: &[u8], destination: &Path) -> anyhow::Result<()> {
    let cursor = Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(cursor).context("failed to read embedded plugin zip")?;
    for index in 0..archive.len() {
        let mut file = archive
            .by_index(index)
            .with_context(|| format!("failed to read zip entry {index}"))?;
        let relative_path = safe_zip_path(file.name())?;
        let output_path = destination.join(relative_path);
        if file.is_dir() {
            std::fs::create_dir_all(&output_path)
                .with_context(|| format!("failed to create {}", output_path.display()))?;
            continue;
        }
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let mut contents = Vec::new();
        file.read_to_end(&mut contents)
            .with_context(|| format!("failed to read zip entry {}", file.name()))?;
        std::fs::write(&output_path, contents)
            .with_context(|| format!("failed to write {}", output_path.display()))?;
    }
    Ok(())
}

fn safe_zip_path(name: &str) -> anyhow::Result<PathBuf> {
    let path = Path::new(name);
    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => relative.push(value),
            Component::CurDir => {}
            _ => anyhow::bail!("zip entry escapes destination: {name}"),
        }
    }
    if relative.as_os_str().is_empty() {
        anyhow::bail!("zip entry has empty path");
    }
    Ok(relative)
}

fn zip_entry_relative_path(name: &str) -> Option<PathBuf> {
    let path = Path::new(name);
    let mut components = path.components();
    match components.next()? {
        Component::Normal(_) => {}
        _ => return None,
    }
    let mut relative = PathBuf::new();
    for component in components {
        match component {
            Component::Normal(value) => relative.push(value),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!relative.as_os_str().is_empty()).then_some(relative)
}

fn validate_openai_plugins_marketplace_root(root: &Path) -> anyhow::Result<()> {
    let marketplace = local_openai_curated_marketplace_root_from_root(root)?
        .ok_or_else(|| anyhow::anyhow!("downloaded openai/plugins marketplace is invalid"))?;
    if marketplace != root {
        anyhow::bail!("downloaded openai/plugins marketplace root mismatch");
    }
    Ok(())
}

fn validate_openai_curated_remote_marketplace_root(root: &Path) -> anyhow::Result<()> {
    let marketplace = local_openai_curated_remote_marketplace_root_from_root(root)?
        .ok_or_else(|| anyhow::anyhow!("embedded official remote plugin marketplace is invalid"))?;
    if marketplace != root {
        anyhow::bail!("embedded official remote plugin marketplace root mismatch");
    }
    Ok(())
}

fn local_openai_curated_marketplace_root_from_root(root: &Path) -> anyhow::Result<Option<PathBuf>> {
    let marketplace_path = root
        .join(".agents")
        .join("plugins")
        .join("marketplace.json");
    if !marketplace_path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&marketplace_path)
        .with_context(|| format!("failed to read {}", marketplace_path.display()))?;
    let marketplace: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse {}", marketplace_path.display()))?;
    if marketplace.get("name").and_then(serde_json::Value::as_str)
        != Some(OPENAI_CURATED_MARKETPLACE)
    {
        return Ok(None);
    }
    let has_plugins = marketplace
        .get("plugins")
        .and_then(serde_json::Value::as_array)
        .map(|plugins| !plugins.is_empty())
        .unwrap_or(false);
    if !has_plugins || !root.join("plugins").is_dir() {
        return Ok(None);
    }
    Ok(Some(root.to_path_buf()))
}

fn local_openai_curated_remote_marketplace_root_from_root(
    root: &Path,
) -> anyhow::Result<Option<PathBuf>> {
    let marketplace_path = root
        .join(".agents")
        .join("plugins")
        .join("marketplace.json");
    if !marketplace_path.is_file() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&marketplace_path)
        .with_context(|| format!("failed to read {}", marketplace_path.display()))?;
    let marketplace: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse {}", marketplace_path.display()))?;
    if marketplace.get("name").and_then(serde_json::Value::as_str)
        != Some(OPENAI_CURATED_REMOTE_MARKETPLACE)
    {
        return Ok(None);
    }
    let has_plugins = marketplace
        .get("plugins")
        .and_then(serde_json::Value::as_array)
        .map(|plugins| !plugins.is_empty())
        .unwrap_or(false);
    if !has_plugins || !root.join("plugins").is_dir() {
        return Ok(None);
    }
    Ok(Some(root.to_path_buf()))
}

fn replace_directory(source: &Path, destination: &Path) -> anyhow::Result<()> {
    replace_directory_with_backup_name(source, destination, "plugins.previous-codex-plus")
}

fn replace_directory_with_backup_name(
    source: &Path,
    destination: &Path,
    backup_name: &str,
) -> anyhow::Result<()> {
    let backup = destination.with_file_name(backup_name);
    if backup.exists() {
        std::fs::remove_dir_all(&backup)
            .with_context(|| format!("failed to remove {}", backup.display()))?;
    }
    if destination.exists() {
        std::fs::rename(destination, &backup).with_context(|| {
            format!(
                "failed to move {} to {}",
                destination.display(),
                backup.display()
            )
        })?;
    }
    match std::fs::rename(source, destination) {
        Ok(()) => {
            if backup.exists() {
                let _ = std::fs::remove_dir_all(&backup);
            }
            Ok(())
        }
        Err(error) => {
            if backup.exists() {
                let _ = std::fs::rename(&backup, destination);
            }
            Err(error).with_context(|| {
                format!(
                    "failed to move {} to {}",
                    source.display(),
                    destination.display()
                )
            })
        }
    }
}

fn ensure_marketplace_configs(
    home: &Path,
    marketplace_names: &[&str],
    marketplace_root: &Path,
) -> anyhow::Result<bool> {
    ensure_marketplace_configs_with_plugins(home, marketplace_names, marketplace_root, &[])
}

fn ensure_marketplace_configs_with_plugins(
    home: &Path,
    marketplace_names: &[&str],
    marketplace_root: &Path,
    plugin_ids: &[String],
) -> anyhow::Result<bool> {
    let config_path = home.join("config.toml");
    let existing = match std::fs::read(&config_path) {
        Ok(bytes) => String::from_utf8(bytes)
            .with_context(|| format!("failed to read UTF-8 {}", config_path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("failed to read {}", config_path.display()));
        }
    };
    let without_bom = existing.trim_start_matches('\u{feff}');
    let updated = merge_marketplace_configs_and_plugins_into_text(
        without_bom,
        marketplace_names,
        marketplace_root,
        plugin_ids,
    )?;
    if updated.as_bytes() == without_bom.as_bytes() {
        return Ok(false);
    }
    crate::settings::atomic_write(&config_path, updated.as_bytes())?;
    Ok(true)
}

fn merge_marketplace_configs_into_text(
    config_text: &str,
    marketplace_names: &[&str],
    marketplace_root: &Path,
) -> anyhow::Result<String> {
    merge_marketplace_configs_and_plugins_into_text(
        config_text,
        marketplace_names,
        marketplace_root,
        &[],
    )
}

fn merge_marketplace_configs_and_plugins_into_text(
    config_text: &str,
    marketplace_names: &[&str],
    marketplace_root: &Path,
    plugin_ids: &[String],
) -> anyhow::Result<String> {
    let mut doc = parse_toml_document(config_text)?;
    let marketplaces = table_mut_or_insert(&mut doc, "marketplaces")?;
    for marketplace_name in marketplace_names {
        if marketplaces
            .get(marketplace_name)
            .and_then(Item::as_table)
            .is_none()
        {
            marketplaces[marketplace_name] = toml_edit::table();
        }
        marketplaces[marketplace_name]["source_type"] = toml_edit::value("local");
        marketplaces[marketplace_name]["source"] =
            toml_edit::value(windows_extended_path(marketplace_root));
    }
    if !plugin_ids.is_empty() {
        let plugins = table_mut_or_insert(&mut doc, "plugins")?;
        for plugin_id in plugin_ids {
            let existing_enabled = plugins
                .get(plugin_id)
                .and_then(Item::as_table)
                .and_then(|table| table.get("enabled"))
                .and_then(Item::as_bool);
            if plugins.get(plugin_id).and_then(Item::as_table).is_none() {
                plugins[plugin_id] = toml_edit::table();
            }
            if existing_enabled.is_none() {
                plugins[plugin_id]["enabled"] = toml_edit::value(true);
            }
        }
    }
    Ok(ensure_trailing_newline(doc.to_string()))
}

fn marketplace_config_points_to_root(home: &Path, marketplace_name: &str, root: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(home.join("config.toml")) else {
        return false;
    };
    let Ok(doc) = text.trim_start_matches('\u{feff}').parse::<DocumentMut>() else {
        return false;
    };
    let Some(table) = doc
        .get("marketplaces")
        .and_then(Item::as_table)
        .and_then(|marketplaces| marketplaces.get(marketplace_name))
        .and_then(Item::as_table)
    else {
        return false;
    };
    let source_type = table
        .get("source_type")
        .and_then(Item::as_str)
        .unwrap_or_default();
    let source = table
        .get("source")
        .and_then(Item::as_str)
        .unwrap_or_default();
    source_type == "local" && normalize_windows_extended_path(source) == root.to_string_lossy()
}

fn normalize_windows_extended_path(value: &str) -> String {
    value.strip_prefix(r"\\?\").unwrap_or(value).to_string()
}

fn windows_extended_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    if value.starts_with(r"\\?\") {
        value.into_owned()
    } else {
        format!(r"\\?\{value}")
    }
}

fn parse_toml_document(contents: &str) -> anyhow::Result<DocumentMut> {
    let contents = contents.trim_start_matches('\u{feff}');
    if contents.trim().is_empty() {
        Ok(DocumentMut::new())
    } else {
        contents
            .parse::<DocumentMut>()
            .map_err(|error| anyhow::anyhow!("config.toml TOML parse failed: {error}"))
    }
}

fn table_mut_or_insert<'a>(doc: &'a mut DocumentMut, key: &str) -> anyhow::Result<&'a mut Table> {
    if !doc.as_table().contains_key(key) {
        doc[key] = toml_edit::table();
    }
    if doc.get(key).and_then(Item::as_table).is_none() {
        doc[key] = toml_edit::table();
    }
    doc.get_mut(key)
        .and_then(Item::as_table_mut)
        .ok_or_else(|| anyhow::anyhow!("{key} must be a TOML table"))
}

fn ensure_trailing_newline(mut contents: String) -> String {
    if !contents.ends_with('\n') {
        contents.push('\n');
    }
    contents
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_marketplace(home: &Path) {
        let root = home.join(".tmp").join("plugins");
        std::fs::create_dir_all(root.join(".agents").join("plugins")).unwrap();
        std::fs::create_dir_all(root.join("plugins").join("gmail")).unwrap();
        std::fs::write(
            root.join(".agents")
                .join("plugins")
                .join("marketplace.json"),
            r#"{"name":"openai-curated","plugins":[{"name":"gmail","path":"./plugins/gmail"}]}"#,
        )
        .unwrap();
    }

    fn write_remote_marketplace(home: &Path) {
        let root = home.join(".tmp").join("plugins-remote");
        std::fs::create_dir_all(root.join(".agents").join("plugins")).unwrap();
        std::fs::create_dir_all(root.join("plugins").join("product-design")).unwrap();
        std::fs::write(
            root.join(".agents")
                .join("plugins")
                .join("marketplace.json"),
            r#"{"name":"openai-curated-remote","plugins":[{"name":"product-design","path":"./plugins/product-design"}]}"#,
        )
        .unwrap();
    }

    fn write_role_specific_marketplace(home: &Path) {
        let root = home
            .join(".tmp")
            .join("marketplaces")
            .join("role-specific-plugins");
        std::fs::create_dir_all(root.join(".agents").join("plugins")).unwrap();
        for plugin in [
            "sales",
            "data-analytics",
            "product-design",
            "financial-markets",
            "customer-support",
        ] {
            std::fs::create_dir_all(root.join("plugins").join(plugin)).unwrap();
        }
        std::fs::write(
            root.join(".agents")
                .join("plugins")
                .join("marketplace.json"),
            r#"{"name":"role-specific-plugins","plugins":[{"name":"sales"},{"name":"data-analytics"},{"name":"product-design"},{"name":"financial-markets"},{"name":"customer-support"}]}"#,
        )
        .unwrap();
    }

    #[test]
    fn ensure_openai_curated_marketplace_config_registers_local_marketplace() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        write_marketplace(home);
        write_remote_marketplace(home);

        let changed = ensure_openai_curated_marketplace_config(home).unwrap();

        assert!(changed);
        let config = std::fs::read_to_string(home.join("config.toml")).unwrap();
        let parsed = config.parse::<DocumentMut>().unwrap();
        assert_eq!(
            parsed["marketplaces"]["openai-curated"]["source_type"].as_str(),
            Some("local")
        );
        assert_eq!(
            parsed["marketplaces"]["openai-curated"]["source"].as_str(),
            Some(format!(r"\\?\{}", home.join(".tmp").join("plugins").display()).as_str())
        );
        assert_eq!(
            parsed["marketplaces"]["openai-api-curated"]["source_type"].as_str(),
            Some("local")
        );
        assert_eq!(
            parsed["marketplaces"]["openai-api-curated"]["source"].as_str(),
            Some(format!(r"\\?\{}", home.join(".tmp").join("plugins").display()).as_str())
        );
        assert_eq!(
            parsed["marketplaces"]["openai-curated-remote"]["source_type"].as_str(),
            Some("local")
        );
        assert_eq!(
            parsed["marketplaces"]["openai-curated-remote"]["source"].as_str(),
            Some(
                format!(
                    r"\\?\{}",
                    home.join(".tmp").join("plugins-remote").display()
                )
                .as_str()
            )
        );
    }

    #[test]
    fn cleanup_removes_only_the_reserved_entries_we_wrote() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        write_marketplace(home);
        // 先按旧行为写一遍(1.3.7 及以前启动时会做的事)。
        ensure_openai_curated_marketplace_config(home).unwrap();
        let config_path = home.join("config.toml");
        let mut before = std::fs::read_to_string(&config_path).unwrap();
        before = format!(
            "# 用户的注释\nmodel = \"gpt-5\"\n\n{before}\n[marketplaces.openai-bundled]\nsource_type = \"local\"\nsource = '\\\\?\\C:\\codex\\.tmp\\bundled-marketplaces\\openai-bundled'\n\n[marketplaces.my-kit]\nsource_type = \"local\"\nsource = \"/somewhere\"\n"
        );
        std::fs::write(&config_path, &before).unwrap();

        let result = cleanup_recodex_reserved_marketplace_configs(home).unwrap();
        assert_eq!(
            result.removed,
            vec!["openai-curated".to_string(), "openai-api-curated".to_string()]
        );
        let after = std::fs::read_to_string(&config_path).unwrap();
        assert!(!after.contains("[marketplaces.openai-curated]"), "{after}");
        assert!(!after.contains("[marketplaces.openai-api-curated]"), "{after}");
        assert!(after.contains("[marketplaces.openai-bundled]"), "{after}");
        assert!(after.contains("[marketplaces.my-kit]"), "{after}");
        assert!(after.contains("# 用户的注释"), "{after}");
        assert!(after.contains("model = \"gpt-5\""), "{after}");

        // 备份是改之前的原样;第二次什么都不做、也不覆盖备份。
        let backup = result.backup.unwrap();
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), before);
        let again = cleanup_recodex_reserved_marketplace_configs(home).unwrap();
        assert!(again.removed.is_empty());
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), after);
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), before);
    }

    #[test]
    fn cleanup_leaves_user_owned_reserved_entries_alone() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let plugins = home.join(".tmp").join("plugins");
        let text = format!(
            "[marketplaces.openai-curated]\nsource_type = \"local\"\nsource = \"/elsewhere\"\n\n[marketplaces.openai-api-curated]\nsource_type = \"local\"\nsource = {:?}\nnote = \"mine\"\n",
            plugins.to_string_lossy()
        );
        assert_eq!(remove_recodex_reserved_marketplaces(&text, home), None);
        std::fs::write(home.join("config.toml"), &text).unwrap();
        let result = cleanup_recodex_reserved_marketplace_configs(home).unwrap();
        assert!(result.removed.is_empty());
        assert!(result.backup.is_none());
        assert_eq!(std::fs::read_to_string(home.join("config.toml")).unwrap(), text);
        assert!(!home.join("config.toml.recodex-marketplace-cleanup.bak").exists());
    }

    #[test]
    fn cleanup_drops_the_now_empty_implicit_marketplaces_table() {
        let home = Path::new("/h");
        let text = "model = \"x\"\n\n[marketplaces.openai-curated]\nsource_type = \"local\"\nsource = '\\\\?\\/h/.tmp/plugins'\n";
        let (updated, removed) = remove_recodex_reserved_marketplaces(text, home).unwrap();
        assert_eq!(removed, vec!["openai-curated".to_string()]);
        assert!(!updated.contains("marketplaces"), "{updated}");
        assert!(updated.contains("model = \"x\""));
    }

    #[test]
    fn ensure_openai_curated_marketplace_config_skips_when_snapshot_missing() {
        let temp = tempfile::tempdir().unwrap();

        let changed = ensure_openai_curated_marketplace_config(temp.path()).unwrap();

        assert!(!changed);
        assert!(!temp.path().join("config.toml").exists());
    }

    #[test]
    fn ensure_role_specific_plugins_marketplace_config_repairs_installed_plugin_entries() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        write_role_specific_marketplace(home);
        std::fs::write(
            home.join("config.toml"),
            "model_provider = \"custom\"\nexperimental_bearer_token = \"sk-redacted\"\n",
        )
        .unwrap();

        let changed = ensure_role_specific_plugins_marketplace_config(home).unwrap();

        assert!(changed);
        let config = std::fs::read_to_string(home.join("config.toml")).unwrap();
        let parsed = config.parse::<DocumentMut>().unwrap();
        assert_eq!(
            parsed["marketplaces"]["role-specific-plugins"]["source_type"].as_str(),
            Some("local")
        );
        assert_eq!(
            parsed["marketplaces"]["role-specific-plugins"]["source"].as_str(),
            Some(
                format!(
                    r"\\?\{}",
                    home.join(".tmp")
                        .join("marketplaces")
                        .join("role-specific-plugins")
                        .display()
                )
                .as_str()
            )
        );
        for plugin in [
            "sales@role-specific-plugins",
            "data-analytics@role-specific-plugins",
            "product-design@role-specific-plugins",
            "financial-markets@role-specific-plugins",
            "customer-support@role-specific-plugins",
        ] {
            assert_eq!(parsed["plugins"][plugin]["enabled"].as_bool(), Some(true));
        }
        assert_eq!(
            parsed["experimental_bearer_token"].as_str(),
            Some("sk-redacted")
        );
    }

    #[test]
    fn ensure_role_specific_plugins_marketplace_config_preserves_disabled_plugin_choice() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        write_role_specific_marketplace(home);
        std::fs::write(
            home.join("config.toml"),
            "[plugins.\"sales@role-specific-plugins\"]\nenabled = false\n",
        )
        .unwrap();

        let changed = ensure_role_specific_plugins_marketplace_config(home).unwrap();

        assert!(changed);
        let config = std::fs::read_to_string(home.join("config.toml")).unwrap();
        let parsed = config.parse::<DocumentMut>().unwrap();
        assert_eq!(
            parsed["plugins"]["sales@role-specific-plugins"]["enabled"].as_bool(),
            Some(false)
        );
        assert_eq!(
            parsed["plugins"]["customer-support@role-specific-plugins"]["enabled"].as_bool(),
            Some(true)
        );
    }

    #[test]
    fn openai_curated_marketplace_status_detects_missing_config() {
        let temp = tempfile::tempdir().unwrap();
        write_marketplace(temp.path());

        let status = openai_curated_marketplace_status(temp.path());

        assert!(status.marketplace_root.is_some());
        assert!(!status.config_registered);
        assert!(status.needs_repair());
    }

    #[test]
    fn openai_curated_marketplace_status_requires_api_marketplace_config() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let root = home.join(".tmp").join("plugins");
        write_marketplace(home);
        write_remote_marketplace(home);
        ensure_marketplace_configs(home, &[OPENAI_CURATED_MARKETPLACE], &root).unwrap();

        let status = openai_curated_marketplace_status(home);

        assert!(status.marketplace_root.is_some());
        assert!(!status.config_registered);
        assert!(status.needs_repair());
    }

    #[test]
    fn openai_curated_remote_marketplace_status_detects_cached_marketplace() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        write_remote_marketplace(home);

        let status = openai_curated_remote_marketplace_status(home);

        assert_eq!(
            status.marketplace_root,
            Some(home.join(".tmp").join("plugins-remote"))
        );
        assert!(!status.config_registered);
        assert!(status.needs_repair());
    }

    #[test]
    fn ensure_openai_curated_remote_marketplace_config_registers_remote_only() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        write_remote_marketplace(home);

        let changed = ensure_openai_curated_remote_marketplace_config(home).unwrap();

        assert!(changed);
        let config = std::fs::read_to_string(home.join("config.toml")).unwrap();
        let parsed = config.parse::<DocumentMut>().unwrap();
        assert!(
            parsed
                .get("marketplaces")
                .and_then(Item::as_table)
                .and_then(|marketplaces| marketplaces.get("openai-curated"))
                .is_none()
        );
        assert_eq!(
            parsed["marketplaces"]["openai-curated-remote"]["source_type"].as_str(),
            Some("local")
        );
        assert_eq!(
            parsed["marketplaces"]["openai-curated-remote"]["source"].as_str(),
            Some(
                format!(
                    r"\\?\{}",
                    home.join(".tmp").join("plugins-remote").display()
                )
                .as_str()
            )
        );
    }

    #[test]
    fn embedded_remote_marketplace_snapshot_has_no_legacy_brand() {
        let mut archive =
            zip::ZipArchive::new(Cursor::new(OPENAI_CURATED_REMOTE_MARKETPLACE_ZIP)).unwrap();
        for index in 0..archive.len() {
            let mut file = archive.by_index(index).unwrap();
            let mut contents = Vec::new();
            file.read_to_end(&mut contents).unwrap();
            let text = String::from_utf8_lossy(&contents).to_ascii_lowercase();
            for needle in ["codex++", "codexplusplus", "codex-plus", "bigpizza"] {
                assert!(!text.contains(needle), "{} 里还有 {needle}", file.name());
            }
        }
    }

    #[test]
    fn an_already_extracted_snapshot_gets_the_new_description() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        ensure_openai_curated_remote_marketplace_available(home).unwrap();
        let path = home
            .join(".tmp")
            .join("plugins-remote")
            .join(".agents")
            .join("plugins")
            .join("marketplace.json");
        let fresh = std::fs::read_to_string(&path).unwrap();
        assert!(fresh.contains(REMOTE_MARKETPLACE_DESCRIPTION), "内嵌快照应已是新文案");
        // 模拟以前解压的旧快照
        let stale = fresh.replace(REMOTE_MARKETPLACE_DESCRIPTION, LEGACY_REMOTE_MARKETPLACE_DESCRIPTION);
        std::fs::write(&path, &stale).unwrap();

        assert!(refresh_remote_marketplace_branding(home).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), fresh, "只换那一句,其余不动");
        assert!(!refresh_remote_marketplace_branding(home).unwrap(), "幂等");
        // 启动路径(ensure_openai_curated_marketplace_config)同样会刷
        std::fs::write(&path, &stale).unwrap();
        let _ = ensure_openai_curated_marketplace_config(home);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), fresh);
    }

    #[test]
    fn marketplace_text_without_the_legacy_sentence_is_untouched() {
        assert_eq!(rebrand_remote_marketplace_text(r#"{"shortDescription":"by Codex++ fans"}"#), None);
        assert_eq!(rebrand_remote_marketplace_text(REMOTE_MARKETPLACE_DESCRIPTION), None);
    }

    #[test]
    fn ensure_openai_curated_remote_marketplace_available_installs_embedded_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();

        let result = ensure_openai_curated_remote_marketplace_available(home).unwrap();

        assert!(result.initialized);
        assert!(result.configured);
        let root = home.join(".tmp").join("plugins-remote");
        assert!(
            root.join(".agents")
                .join("plugins")
                .join("marketplace.json")
                .is_file()
        );
        assert!(
            root.join("plugins")
                .join("product-design")
                .join(".codex-plugin")
                .join("plugin.json")
                .is_file()
        );
        let config = std::fs::read_to_string(home.join("config.toml")).unwrap();
        let parsed = config.parse::<DocumentMut>().unwrap();
        assert_eq!(
            parsed["marketplaces"]["openai-curated-remote"]["source_type"].as_str(),
            Some("local")
        );
        assert_eq!(
            parsed["marketplaces"]["openai-curated-remote"]["source"].as_str(),
            Some(
                format!(
                    r"\\?\{}",
                    home.join(".tmp").join("plugins-remote").display()
                )
                .as_str()
            )
        );
    }

    #[test]
    fn zip_entry_relative_path_strips_archive_root_and_rejects_escape() {
        assert_eq!(
            zip_entry_relative_path("plugins-main/plugins/gmail/file.txt"),
            Some(PathBuf::from("plugins").join("gmail").join("file.txt"))
        );
        assert_eq!(zip_entry_relative_path("plugins-main/../evil.txt"), None);
        assert_eq!(zip_entry_relative_path("../evil.txt"), None);
    }

    #[test]
    fn install_openai_plugins_zip_installs_valid_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let mut bytes = Cursor::new(Vec::<u8>::new());
        {
            let mut writer = zip::ZipWriter::new(&mut bytes);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            writer
                .start_file("plugins-main/.agents/plugins/marketplace.json", options)
                .unwrap();
            std::io::Write::write_all(
                &mut writer,
                br#"{"name":"openai-curated","plugins":[{"name":"gmail","path":"./plugins/gmail"}]}"#,
            )
            .unwrap();
            writer
                .start_file(
                    "plugins-main/plugins/gmail/.codex-plugin/plugin.json",
                    options,
                )
                .unwrap();
            std::io::Write::write_all(&mut writer, br#"{"name":"gmail"}"#).unwrap();
            writer.finish().unwrap();
        }

        install_openai_plugins_zip(temp.path(), bytes.get_ref()).unwrap();
        let changed = ensure_openai_curated_marketplace_config(temp.path()).unwrap();

        assert!(changed);
        assert!(
            temp.path()
                .join(".tmp/plugins/.agents/plugins/marketplace.json")
                .is_file()
        );
        assert!(
            temp.path()
                .join(".tmp/plugins/plugins/gmail/.codex-plugin/plugin.json")
                .is_file()
        );
    }
}
