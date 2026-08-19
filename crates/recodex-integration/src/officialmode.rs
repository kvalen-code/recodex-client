//! recodex-overlay: 「切到官方 ChatGPT 模式」的状态机。
//!
//! 登出会把 ReCodex 拥有的 Codex 配置**清掉**,想回来就得重新登录;而"临时切到官方"
//! 应当是可逆的。所以切走之前先把我们写进 `~/.codex` 的三样东西存成快照:
//!   - `config.toml` 里的托管块正文
//!   - `RECODEX_KEY` 的值
//!   - 我们写的 `auth.json`
//! 切回来时按快照原样写回,**不需要重新登录**。
//!
//! ⚠️ `auth.json` 曾经漏在快照之外,那是个真会咬人的 bug:切走时 `restore_auth()`
//! 会把我们的 `auth.json` 连同备份、归属标记一起删掉 —— 删完就找不回来了。
//! 切回来只写 config 和 env,Codex 要么完全没有登录态(用户此前没用过官方 Codex),
//! 要么拿着用户自己的 ChatGPT 凭据去连我们的网关。两种都与上面那句承诺矛盾。
//!
//! 快照放在应用数据目录(不是 `~/.codex/recodex/`)—— 后者在卸载流程里会被整个删掉。

use std::fs;
use std::io::{self, ErrorKind};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::codexcfg::{self, SUB2API_ENV_KEY};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct OfficialModeSnapshot {
    /// 托管块正文(不含 marker),空表示切走时本就没有托管块。
    #[serde(default)]
    pub config_body: String,
    /// `RECODEX_KEY` 的值,空表示当时没设。
    #[serde(default)]
    pub env_value: String,
    /// 我们写的 `auth.json` 正文,空表示切走时它并不归我们所有。
    ///
    /// 令牌落在 `%LOCALAPPDATA%` 明文里 —— 与它原本待的 `~/.codex/auth.json`
    /// 是同一个暴露面,没有更差;卸载时随数据目录一起删。
    #[serde(default)]
    pub auth_json: String,
}

fn state_dir() -> io::Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("APPDATA"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "no user data directory is available"))?;
    Ok(base.join("ReCodex"))
}

fn snapshot_path() -> io::Result<PathBuf> {
    Ok(state_dir()?.join("official-mode.json"))
}

/// 从当前 `~/.codex/config.toml` 里取出托管块正文(去掉 marker 行)。
pub fn current_managed_body() -> io::Result<String> {
    let path = codexcfg::config_path()?;
    let content = match fs::read_to_string(&path) {
        Ok(value) => value,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(String::new()),
        Err(error) => return Err(error),
    };
    if !codexcfg::has_managed_block(&content) {
        return Ok(String::new());
    }
    // 托管块 = START_MARKER 行 + 正文 + END_MARKER 行,取中间。
    let Some(start) = content.find(codexcfg::START_MARKER) else {
        return Ok(String::new());
    };
    let body_start = match content[start..].find('\n') {
        Some(nl) => start + nl + 1,
        None => return Ok(String::new()),
    };
    let Some(end_rel) = content[body_start..].find(codexcfg::END_MARKER) else {
        return Ok(String::new());
    };
    Ok(content[body_start..body_start + end_rel].trim_end().to_string())
}

pub fn is_official_mode() -> bool {
    snapshot_path()
        .map(|path| path.exists())
        .unwrap_or(false)
}

pub fn load_snapshot() -> io::Result<Option<OfficialModeSnapshot>> {
    let path = snapshot_path()?;
    match fs::read_to_string(&path) {
        Ok(text) => Ok(serde_json::from_str(&text).ok()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn save_snapshot(snapshot: &OfficialModeSnapshot) -> io::Result<()> {
    let path = snapshot_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(snapshot)
        .map_err(|error| io::Error::new(ErrorKind::InvalidData, error))?;
    fs::write(&path, text)
}

fn clear_snapshot() -> io::Result<()> {
    let path = snapshot_path()?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// 切到官方模式:先存快照,再把我们写的东西撤掉。
/// 快照存成功才动 `~/.codex` —— 否则就成了不可逆的登出。
pub fn switch_to_official() -> io::Result<()> {
    if is_official_mode() {
        return Ok(());
    }
    let snapshot = OfficialModeSnapshot {
        config_body: current_managed_body()?,
        env_value: std::env::var(SUB2API_ENV_KEY).unwrap_or_default(),
        // 必须在 restore_all() 之前读 —— 那一步会把它删掉
        auth_json: codexcfg::read_managed_auth()?
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .unwrap_or_default(),
    };
    save_snapshot(&snapshot)?;
    if let Err(error) = codexcfg::restore_all() {
        // 撤销失败就把快照删掉,避免状态卡在"以为在官方模式但配置还在"
        let _ = clear_snapshot();
        return Err(error);
    }
    Ok(())
}

/// 官方模式期间改了网关:把新的托管块记进**快照**,不碰 `~/.codex`。
///
/// 用户在官方模式下点「用最快网关」是很自然的动作(按钮就摆在账号页)。
/// 直接写活配置会**悄悄把官方模式破坏掉** —— 面板还显示「官方模式」,
/// Codex 下次启动却已经走回 ReCodex 网关了。
/// 写进快照则三方一致:官方模式保持、服务端的选择被记住、切回来时自动生效。
///
/// 不在官方模式时返回 `Ok(false)`,由调用方照常写活配置。
pub fn stage_config_for_return(config_body: &str) -> io::Result<bool> {
    let Some(mut snapshot) = load_snapshot()? else {
        return Ok(false);
    };
    snapshot.config_body = config_body.to_string();
    save_snapshot(&snapshot)?;
    Ok(true)
}

/// 丢弃快照,**不写回任何东西**。
///
/// 与 `switch_to_recodex()` 的区别是决定性的:那个会把托管块和 `RECODEX_KEY`
/// 重新装回 `~/.codex`。登出和卸载都不能用它 ——
/// 登出后快照里的 key 已经失效,卸载更是要把我们的配置**清干净**。
/// 卸载路径上一度就是用 `switch_to_recodex()` 做"清理"的,结果是:
/// 登出刚撤掉的托管块又被装回去,程序随即自删,
/// 用户剩下一个指向已吊销网关的 Codex,连能修它的程序都没了。
pub fn discard_snapshot() -> io::Result<()> {
    clear_snapshot()
}

/// 切回 ReCodex:按快照原样写回,不需要重新登录。
pub fn switch_to_recodex() -> io::Result<()> {
    let Some(snapshot) = load_snapshot()? else {
        return Ok(()); // 本来就不在官方模式
    };
    if !snapshot.config_body.is_empty() {
        codexcfg::apply_config(&snapshot.config_body)?;
    }
    if !snapshot.auth_json.is_empty() {
        codexcfg::write_auth(snapshot.auth_json.as_bytes())?;
    }
    if !snapshot.env_value.is_empty() {
        codexcfg::set_user_env(SUB2API_ENV_KEY, &snapshot.env_value)?;
    }
    clear_snapshot()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_round_trips_through_json() {
        let snapshot = OfficialModeSnapshot {
            config_body: "model_provider = \"recodex\"".to_string(),
            env_value: "sk-test".to_string(),
            auth_json: "{\"tokens\":{\"access_token\":\"x\"}}".to_string(),
        };
        let text = serde_json::to_string(&snapshot).unwrap();
        let parsed: OfficialModeSnapshot = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, snapshot);
    }

    #[test]
    fn missing_fields_default_to_empty() {
        // 旧版本写的快照可能缺字段,不能因此解析失败
        let parsed: OfficialModeSnapshot = serde_json::from_str("{}").unwrap();
        assert!(parsed.config_body.is_empty());
        assert!(parsed.env_value.is_empty());
        assert!(parsed.auth_json.is_empty());
    }
}
