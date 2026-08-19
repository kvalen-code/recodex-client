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

/// 下载的东西**必须真的是个可执行文件**才允许替换。
///
/// sha256 只能证明「拿到的和 manifest 说的一致」,证明不了「manifest 说的是对的」。
/// 现实里最容易犯的错:manifest.json 和 exe 传在同一个 OSS 目录下,
/// 管理后台把 manifest 地址粘进了安装包地址 —— 哈希一致、https 一致、一路绿灯,
/// 然后用一个 JSON 文件覆盖掉 exe。
///
/// 而这一步是**不可逆**的:替换成功后面板就去重启,拉起一个非可执行文件必然失败,
/// `.old` 明明躺在旁边却没有任何代码会去还原它 —— 客户端直接死透,用户无路可走。
/// 所以宁可在这里挑剔一点。
fn ensure_executable_payload(bytes: &[u8]) -> anyhow::Result<()> {
    // 真实包体是十几 MB;几百字节的东西一定不是我们要的(多半是 JSON 或错误页)
    const MIN_PLAUSIBLE_SIZE: usize = 64 * 1024;
    if bytes.len() < MIN_PLAUSIBLE_SIZE {
        anyhow::bail!(
            "安装包只有 {} 字节,不像可执行文件,已拒绝替换(检查 manifest 里的 url 是不是填成了 manifest 自己)",
            bytes.len()
        );
    }
    #[cfg(windows)]
    if !bytes.starts_with(b"MZ") {
        anyhow::bail!("安装包不是 Windows 可执行文件(缺少 MZ 头),已拒绝替换");
    }
    Ok(())
}

/// 把新包就位。返回被保留的旧文件路径(供调用方在下次启动时清理)。
pub fn stage_replacement(exe: &Path, bytes: &[u8]) -> anyhow::Result<PathBuf> {
    // 放在最前面:这个函数往后每一步都是不可逆的
    ensure_executable_payload(bytes)?;
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

    /// 一个真实会发生的误配:manifest 里的 url 指向了 manifest 自己。
    /// 哈希对得上、https 也对,但装进去就是把 exe 换成 JSON —— 必须在替换前拦下。
    #[test]
    fn json_payload_is_rejected_before_the_point_of_no_return() {
        let dir = std::env::temp_dir().join(format!("recodex-badpayload-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("app.exe");
        std::fs::write(&exe, b"original binary").unwrap();

        let manifest_json = br#"{"version":"1.2.50","url":"https://oss.example.com/app.exe","sha256":"deadbeef"}"#;
        assert!(stage_replacement(&exe, manifest_json).is_err(), "JSON 不该被当成安装包");
        assert_eq!(
            std::fs::read(&exe).unwrap(),
            b"original binary",
            "拒绝之后原程序必须原封不动"
        );
        assert!(!exe.with_extension("exe.old").exists(), "不该留下半截的 .old");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn payload_without_mz_header_is_rejected() {
        // 够大但不是 PE —— 比如 OSS 返回的 HTML 错误页
        let junk = vec![b'<'; 128 * 1024];
        assert!(ensure_executable_payload(&junk).is_err());
    }

    #[test]
    fn plausible_executable_passes() {
        let mut payload = vec![b'M', b'Z'];
        payload.resize(128 * 1024, 0);
        assert!(ensure_executable_payload(&payload).is_ok());
    }

    #[test]
    fn staging_keeps_old_binary_for_rollback() {
        let dir = std::env::temp_dir().join(format!("recodex-stage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("app.exe");
        std::fs::write(&exe, b"old").unwrap();

        // 形态检查在最前面,所以这里得给一个像样的包体(MZ 头 + 足够大)
        let mut payload = vec![b'M', b'Z'];
        payload.resize(128 * 1024, 7);

        let old = stage_replacement(&exe, &payload).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), payload);
        assert_eq!(std::fs::read(&old).unwrap(), b"old", "旧版本要留着以便回退");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
