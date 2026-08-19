//! recodex-overlay: 自更新 —— 拉 manifest、下载、校验、旁路替换。
//!
//! 安全前提(缺一不可):
//!   - manifest 与安装包**必须走 https**;
//!   - 包体**必须有 sha256 并校验通过**才落地。少了这条,任何能改动下载链路的人
//!     都能把任意 exe 推给用户 —— 这是自更新最危险的地方,所以宁可更新失败也不放行。
//!
//! Windows 上运行中的 exe 不能被覆盖,但**可以被改名**。所以采用:
//!   新包写成 `xxx.new` → 现有 exe 改名为 `xxx.old` → 新包改名就位 → 重启后清理 `.old`。
//! 这样即使中途断电,`.old` 还在,用户手动改回即可。

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateManifest {
    pub version: String,
    /// 安装包下载地址(https)
    pub url: String,
    /// 包体 sha256(十六进制)
    pub sha256: String,
}

fn require_https(url: &str, what: &str) -> anyhow::Result<url::Url> {
    let parsed = url::Url::parse(url.trim())
        .map_err(|error| anyhow::anyhow!("{what}地址无效:{error}"))?;
    if parsed.scheme() != "https" {
        anyhow::bail!("{what}必须使用 https");
    }
    Ok(parsed)
}

pub async fn fetch_manifest(manifest_url: &str) -> anyhow::Result<UpdateManifest> {
    let url = require_https(manifest_url, "更新清单")?;
    let client = crate::http_client::proxied_client(&format!("ReCodex/{}", crate::version::VERSION))?;
    let manifest: UpdateManifest = client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if manifest.sha256.trim().is_empty() {
        anyhow::bail!("更新清单缺少 sha256,拒绝安装");
    }
    Ok(manifest)
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 下载并校验包体。校验不过直接报错,**不落地任何文件**。
pub async fn download_verified(manifest: &UpdateManifest) -> anyhow::Result<Vec<u8>> {
    let url = require_https(&manifest.url, "安装包")?;
    let client = crate::http_client::proxied_client(&format!("ReCodex/{}", crate::version::VERSION))?;
    let bytes = client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?
        .to_vec();
    let actual = hex_digest(&bytes);
    let expected = manifest.sha256.trim().to_ascii_lowercase();
    if actual != expected {
        anyhow::bail!("安装包校验失败(期望 {expected},实际 {actual}),已丢弃");
    }
    Ok(bytes)
}

fn with_extension(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// 把新包就位。返回被保留的旧文件路径(供调用方在下次启动时清理)。
pub fn stage_replacement(exe: &Path, bytes: &[u8]) -> anyhow::Result<PathBuf> {
    let new_path = with_extension(exe, ".new");
    let old_path = with_extension(exe, ".old");

    let mut file = std::fs::File::create(&new_path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);

    // 上一轮更新可能留下 .old,先清掉否则改名会失败
    let _ = std::fs::remove_file(&old_path);
    // 运行中的 exe 不能覆盖,但可以改名 —— 这是 Windows 自更新的关键手法
    std::fs::rename(exe, &old_path)?;
    if let Err(error) = std::fs::rename(&new_path, exe) {
        // 就位失败要把旧文件放回去,否则用户连旧版都启动不了
        let _ = std::fs::rename(&old_path, exe);
        return Err(error.into());
    }
    Ok(old_path)
}

/// 启动时清理上一轮更新留下的 `.old`(那时它已不再被占用)。
pub fn cleanup_previous_update() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(with_extension(&exe, ".old"));
        let _ = std::fs::remove_file(with_extension(&exe, ".new"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_https_urls_are_rejected() {
        // 自更新链路必须是 https,否则等于把任意 exe 的分发权交出去
        assert!(require_https("http://example.com/a.json", "更新清单").is_err());
        assert!(require_https("file:///C:/evil.exe", "安装包").is_err());
        assert!(require_https("https://example.com/a.json", "更新清单").is_ok());
    }

    #[test]
    fn digest_matches_known_vector() {
        // 空串的 sha256,用来确认摘要实现没写反
        assert_eq!(
            hex_digest(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn staging_keeps_old_binary_for_rollback() {
        let dir = std::env::temp_dir().join(format!("recodex-stage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("app.exe");
        std::fs::write(&exe, b"old").unwrap();

        let old = stage_replacement(&exe, b"new").unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        assert_eq!(std::fs::read(&old).unwrap(), b"old", "旧版本要留着以便回退");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
