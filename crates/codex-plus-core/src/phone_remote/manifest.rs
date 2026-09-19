//! 远程组件运行时的清单、验签与安全解压(docs/remote-app-plan.md §2.7)。
//!
//! 清单格式与命令行 `recodex update` / `recodex app` 共用(schema 1,cmd/recodex/update_manifest.go):
//!
//! ```json
//! {"schema":1,"version":"x.y.z","expires_at":"<RFC3339>",
//!  "assets":[{"os":"windows","arch":"amd64","url":"https://…","size":N,
//!             "sha256":"<hex>","signature_url":"https://…"}]}
//! ```
//!
//! **信任链**:
//!   - 构建时注入了发布公钥(`RECODEX_UPDATE_PUBLIC_KEY`,与命令行 `-X main.updatePublicKey`
//!     同一把 minisign 公钥)→ 与命令行完全一致:清单(`<manifest>.minisig`)与压缩包
//!     (`signature_url`)都要 minisign 验签通过,再对 SHA-256 与大小;
//!   - 没注入 → 退回桌面端自更新同一档:清单地址来自带设备令牌的服务端接口、全程 HTTPS,
//!     压缩包对清单里的 SHA-256 与大小。桌面端自己的 exe 就是这么更新的,
//!     远程组件不会比客户端本体更容易被掉包。
//!
//! 纯逻辑(解析、验签、挑资产、解压判定)都在 cfg 门控之外,任何平台都编译、都有单测。

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::layout::parse_stable_version;

pub const MAX_MANIFEST_SIZE: u64 = 1 << 20;
pub const MAX_SIGNATURE_SIZE: u64 = 16 << 10;
/// 便携 Node 本身就有几十 MB;与命令行同一上限。
pub const MAX_ARCHIVE_SIZE: u64 = 200 << 20;
/// 解压后总量与文件数上限,防压缩炸弹(同命令行)。
pub const MAX_EXTRACTED_SIZE: u64 = 800 << 20;
pub const MAX_ARCHIVE_FILES: usize = 50_000;

const MINISIGN_ED: [u8; 2] = *b"Ed";

/// 构建时注入的发布公钥(可为空)。
pub fn release_public_key() -> Option<&'static str> {
    option_env!("RECODEX_UPDATE_PUBLIC_KEY")
        .map(str::trim)
        .filter(|key| !key.is_empty())
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema: i64,
    pub version: String,
    pub expires_at: String,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Asset {
    pub os: String,
    pub arch: String,
    pub url: String,
    pub size: i64,
    pub sha256: String,
    pub signature_url: String,
}

/// 本平台在清单里的 (os, arch) 名 —— 用 Go 的 GOOS/GOARCH 命名,与命令行一致。
pub fn manifest_platform(os: &str, arch: &str) -> Option<(&'static str, &'static str)> {
    let os = match os {
        "windows" => "windows",
        "macos" => "darwin",
        "linux" => "linux",
        _ => return None,
    };
    let arch = match arch {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        _ => return None,
    };
    Some((os, arch))
}

/// 解析并校验 schema-1 清单(未过期、版本号合法、没有多余字段)。
/// `signature` 与 `public_key` 同时给出时先验签;调用方负责「有公钥就必须带签名」。
pub fn decode_manifest(
    raw: &[u8],
    signature: Option<&[u8]>,
    public_key: Option<&str>,
    now: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<Manifest> {
    if let Some(key) = public_key {
        let signature = signature.ok_or_else(|| anyhow::anyhow!("远程组件清单缺少签名"))?;
        verify_minisign(raw, signature, key)
            .map_err(|error| anyhow::anyhow!("远程组件清单签名无效:{error}"))?;
    }
    let manifest: Manifest = serde_json::from_slice(raw)
        .map_err(|error| anyhow::anyhow!("远程组件清单格式不对:{error}"))?;
    if manifest.schema != 1 {
        anyhow::bail!("不支持的远程组件清单 schema {}", manifest.schema);
    }
    let expires = chrono::DateTime::parse_from_rfc3339(&manifest.expires_at)
        .map_err(|_| anyhow::anyhow!("远程组件清单的有效期无效"))?;
    if now >= expires {
        anyhow::bail!("远程组件清单已过期");
    }
    if parse_stable_version(&manifest.version).is_none() {
        anyhow::bail!("远程组件清单的版本号无效:{}", manifest.version);
    }
    Ok(manifest)
}

/// 挑出本平台唯一的资产并校验;同平台重复视为清单有误。
pub fn select_asset(
    manifest: &Manifest,
    os: &str,
    arch: &str,
    max_size: u64,
) -> anyhow::Result<Asset> {
    let mut selected: Option<&Asset> = None;
    for asset in &manifest.assets {
        if asset.os != os || asset.arch != arch {
            continue;
        }
        if selected.is_some() {
            anyhow::bail!("远程组件清单里 {os}/{arch} 出现了两次");
        }
        validate_asset(asset, max_size)?;
        selected = Some(asset);
    }
    selected
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("远程组件暂不支持这个平台({os}/{arch})"))
}

fn validate_asset(asset: &Asset, max_size: u64) -> anyhow::Result<()> {
    if asset.size <= 0 || asset.size as u64 > max_size {
        anyhow::bail!("远程组件大小无效:{}", asset.size);
    }
    let digest = asset.sha256.trim();
    if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        anyhow::bail!("远程组件的 SHA-256 无效");
    }
    for url in [&asset.url, &asset.signature_url] {
        if !is_safe_https_url(url) {
            anyhow::bail!("远程组件下载地址必须是不带凭据的 https");
        }
    }
    Ok(())
}

pub fn is_safe_https_url(raw: &str) -> bool {
    url::Url::parse(raw).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str().is_some_and(|host| !host.is_empty())
            && url.username().is_empty()
            && url.password().is_none()
    })
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 压缩包校验:大小、(有公钥时)minisign、SHA-256。任何一步不过都不落盘。
pub fn verify_archive(
    archive: &[u8],
    asset: &Asset,
    signature: Option<&[u8]>,
    public_key: Option<&str>,
) -> anyhow::Result<()> {
    if archive.len() as u64 != asset.size as u64 {
        anyhow::bail!(
            "远程组件大小不符(期望 {},实际 {})",
            asset.size,
            archive.len()
        );
    }
    if let Some(key) = public_key {
        let signature = signature.ok_or_else(|| anyhow::anyhow!("远程组件缺少签名"))?;
        verify_minisign(archive, signature, key)
            .map_err(|error| anyhow::anyhow!("远程组件签名无效:{error}"))?;
    }
    let actual = sha256_hex(archive);
    if !actual.eq_ignore_ascii_case(asset.sha256.trim()) {
        anyhow::bail!("远程组件校验失败(SHA-256 不符),已丢弃");
    }
    Ok(())
}

/// 最小 minisign 校验器,与命令行 verifyMinisign(改编自 aead.dev/minisign)逐条一致:
/// 只收非预哈希的 Ed 算法;验消息签名,再验 trusted comment 的全局签名。
pub fn verify_minisign(
    message: &[u8],
    signature_text: &[u8],
    public_key_text: &str,
) -> anyhow::Result<()> {
    use base64::Engine as _;
    use ring::signature::{ED25519, UnparsedPublicKey};
    let b64 = base64::engine::general_purpose::STANDARD;

    let mut key_text = public_key_text.trim();
    if key_text.starts_with("untrusted comment:") {
        if let Some(newline) = key_text.find('\n') {
            key_text = key_text[newline + 1..].trim();
        }
    }
    let key_raw = b64
        .decode(key_text)
        .map_err(|_| anyhow::anyhow!("invalid Minisign public key"))?;
    if key_raw.len() != 2 + 8 + 32 || key_raw[..2] != MINISIGN_ED {
        anyhow::bail!("invalid Minisign public key");
    }
    let key_id = &key_raw[2..10];
    let public = UnparsedPublicKey::new(&ED25519, &key_raw[10..]);

    let text = String::from_utf8_lossy(signature_text).replace("\r\n", "\n");
    let segments: Vec<&str> = text.splitn(4, '\n').collect();
    if segments.len() != 4
        || !segments[0].starts_with("untrusted comment: ")
        || !segments[2].starts_with("trusted comment: ")
    {
        anyhow::bail!("invalid Minisign signature");
    }
    let sig_raw = b64
        .decode(segments[1])
        .map_err(|_| anyhow::anyhow!("invalid Minisign message signature"))?;
    if sig_raw.len() != 2 + 8 + 64 || sig_raw[..2] != MINISIGN_ED {
        anyhow::bail!("invalid Minisign message signature");
    }
    if &sig_raw[2..10] != key_id {
        anyhow::bail!("Minisign key ID mismatch");
    }
    let message_signature = &sig_raw[10..];
    public
        .verify(message, message_signature)
        .map_err(|_| anyhow::anyhow!("Minisign message signature verification failed"))?;
    let comment_signature = b64
        .decode(segments[3].trim())
        .map_err(|_| anyhow::anyhow!("invalid Minisign trusted-comment signature"))?;
    if comment_signature.len() != 64 {
        anyhow::bail!("invalid Minisign trusted-comment signature");
    }
    let trusted = &segments[2]["trusted comment: ".len()..];
    let mut comment_message = message_signature.to_vec();
    comment_message.extend_from_slice(trusted.as_bytes());
    public
        .verify(&comment_message, &comment_signature)
        .map_err(|_| anyhow::anyhow!("Minisign trusted-comment verification failed"))?;
    Ok(())
}

/// 压缩包条目名 → 安全的相对路径。拒绝路径穿越(zip-slip)、绝对路径、反斜杠与盘符、NUL。
/// 与命令行 safeZipEntryPath 同一套规则。
pub fn safe_entry_path(name: &str) -> Option<PathBuf> {
    if name.is_empty() || name.contains(['\\', ':', '\0']) || name.starts_with('/') {
        return None;
    }
    let mut out = PathBuf::new();
    for part in name.split('/') {
        match part {
            "" | "." => continue,
            ".." => return None,
            other => out.push(other),
        }
    }
    if out.as_os_str().is_empty() {
        return None;
    }
    // 再过一遍组件:只允许普通段(防平台特有的前缀/根)
    if !out.components().all(|c| matches!(c, Component::Normal(_))) {
        return None;
    }
    Some(out)
}

/// 安全解压:条目名走 [`safe_entry_path`];符号链接等非普通文件一律拒绝;
/// 限制文件数与解压总量。`dest` 必须是新建的空目录(用 create_new 写文件,不覆盖任何东西)。
pub fn extract_zip(archive: &[u8], dest: &Path) -> anyhow::Result<()> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive))
        .map_err(|error| anyhow::anyhow!("打不开远程组件压缩包:{error}"))?;
    if zip.len() > MAX_ARCHIVE_FILES {
        anyhow::bail!("远程组件压缩包条目过多");
    }
    let mut total: u64 = 0;
    for index in 0..zip.len() {
        let mut file = zip
            .by_index(index)
            .map_err(|error| anyhow::anyhow!("读远程组件压缩包失败:{error}"))?;
        let name = file.name().to_string();
        let relative = safe_entry_path(&name)
            .ok_or_else(|| anyhow::anyhow!("远程组件压缩包条目 {name:?} 越出了安装目录"))?;
        let target = dest.join(&relative);
        let unix_mode = file.unix_mode();
        const S_IFMT: u32 = 0o170000;
        const S_IFDIR: u32 = 0o040000;
        const S_IFREG: u32 = 0o100000;
        let kind = unix_mode
            .map(|mode| mode & S_IFMT)
            .filter(|kind| *kind != 0);
        if file.is_dir() || kind == Some(S_IFDIR) {
            std::fs::create_dir_all(&target)?;
            continue;
        }
        if kind.is_some_and(|kind| kind != S_IFREG) || file.is_symlink() {
            anyhow::bail!("远程组件压缩包条目 {name:?} 不是普通文件");
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let remaining = MAX_EXTRACTED_SIZE.saturating_sub(total);
        if remaining == 0 {
            anyhow::bail!("远程组件解压后超出大小上限");
        }
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|error| anyhow::anyhow!("解压 {name} 失败:{error}"))?;
        let written = std::io::copy(&mut (&mut file).take(remaining + 1), &mut out)
            .map_err(|error| anyhow::anyhow!("解压 {name} 失败:{error}"))?;
        drop(out);
        if written > remaining {
            anyhow::bail!("远程组件解压后超出大小上限");
        }
        total += written;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let executable = unix_mode.is_some_and(|mode| mode & 0o111 != 0);
            let perm = if executable { 0o755 } else { 0o644 };
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(perm))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use ring::signature::KeyPair as _;
    use std::io::Write as _;

    fn now() -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339("2026-09-19T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    fn asset(os: &str, arch: &str) -> serde_json::Value {
        serde_json::json!({
            "os": os, "arch": arch,
            "url": format!("https://dl.example.com/recodex-remote-1.2.3-{os}-{arch}.zip"),
            "size": 10, "sha256": "ab".repeat(32),
            "signature_url": format!("https://dl.example.com/recodex-remote-1.2.3-{os}-{arch}.zip.minisig"),
        })
    }

    fn manifest_json(assets: Vec<serde_json::Value>) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schema": 1, "version": "1.2.3", "expires_at": "2026-10-01T00:00:00Z", "assets": assets,
        }))
        .unwrap()
    }

    // ── minisign:测试里现场造一把密钥,按 minisign 的格式签 ──
    struct TestSigner {
        pair: ring::signature::Ed25519KeyPair,
        key_id: [u8; 8],
    }

    impl TestSigner {
        fn new(seed: u8) -> Self {
            Self {
                pair: ring::signature::Ed25519KeyPair::from_seed_unchecked(&[seed; 32]).unwrap(),
                key_id: [seed, 1, 2, 3, 4, 5, 6, 7],
            }
        }

        fn public_key_text(&self) -> String {
            let mut raw = b"Ed".to_vec();
            raw.extend_from_slice(&self.key_id);
            raw.extend_from_slice(self.pair.public_key().as_ref());
            format!(
                "untrusted comment: minisign public key\n{}\n",
                base64::engine::general_purpose::STANDARD.encode(raw)
            )
        }

        fn sign(&self, message: &[u8]) -> Vec<u8> {
            let b64 = base64::engine::general_purpose::STANDARD;
            let sig = self.pair.sign(message);
            let mut raw = b"Ed".to_vec();
            raw.extend_from_slice(&self.key_id);
            raw.extend_from_slice(sig.as_ref());
            let trusted = "timestamp:1758240000\tfile:manifest.json";
            let mut global = sig.as_ref().to_vec();
            global.extend_from_slice(trusted.as_bytes());
            let global_sig = self.pair.sign(&global);
            format!(
                "untrusted comment: signature from minisign secret key\n{}\ntrusted comment: {trusted}\n{}\n",
                b64.encode(raw),
                b64.encode(global_sig.as_ref())
            )
            .into_bytes()
        }
    }

    #[test]
    fn minisign_accepts_a_genuine_signature() {
        let signer = TestSigner::new(9);
        let msg = b"hello manifest";
        verify_minisign(msg, &signer.sign(msg), &signer.public_key_text()).unwrap();
        // 公钥只给 base64 那一行也行
        let bare = signer.public_key_text().lines().nth(1).unwrap().to_string();
        verify_minisign(msg, &signer.sign(msg), &bare).unwrap();
        // CRLF 签名文件(在 Windows 上被编辑过)
        let crlf = String::from_utf8(signer.sign(msg))
            .unwrap()
            .replace('\n', "\r\n");
        verify_minisign(msg, crlf.as_bytes(), &bare).unwrap();
    }

    #[test]
    fn minisign_rejects_tampering_and_foreign_keys() {
        let signer = TestSigner::new(9);
        let other = TestSigner::new(10);
        let msg = b"hello manifest";
        let sig = signer.sign(msg);
        assert!(verify_minisign(b"hello manifesT", &sig, &signer.public_key_text()).is_err());
        assert!(verify_minisign(msg, &sig, &other.public_key_text()).is_err());
        // 改 trusted comment:全局签名不再成立
        let forged = String::from_utf8(sig.clone())
            .unwrap()
            .replace("file:manifest.json", "file:evil.json");
        assert!(verify_minisign(msg, forged.as_bytes(), &signer.public_key_text()).is_err());
        assert!(verify_minisign(msg, b"garbage", &signer.public_key_text()).is_err());
    }

    #[test]
    fn signed_manifest_requires_its_signature_when_a_key_is_built_in() {
        let signer = TestSigner::new(3);
        let raw = manifest_json(vec![asset("windows", "amd64")]);
        let key = signer.public_key_text();
        assert!(decode_manifest(&raw, None, Some(&key), now()).is_err());
        let manifest = decode_manifest(&raw, Some(&signer.sign(&raw)), Some(&key), now()).unwrap();
        assert_eq!(manifest.version, "1.2.3");
        // 没有公钥的构建:只做格式校验
        assert!(decode_manifest(&raw, None, None, now()).is_ok());
    }

    #[test]
    fn manifest_shape_is_strict() {
        let expired = serde_json::to_vec(&serde_json::json!({
            "schema": 1, "version": "1.2.3", "expires_at": "2026-09-01T00:00:00Z", "assets": [],
        }))
        .unwrap();
        assert!(decode_manifest(&expired, None, None, now()).is_err());
        let schema2 = serde_json::to_vec(&serde_json::json!({
            "schema": 2, "version": "1.2.3", "expires_at": "2026-10-01T00:00:00Z", "assets": [],
        }))
        .unwrap();
        assert!(decode_manifest(&schema2, None, None, now()).is_err());
        let unknown = serde_json::to_vec(&serde_json::json!({
            "schema": 1, "version": "1.2.3", "expires_at": "2026-10-01T00:00:00Z", "assets": [], "extra": 1,
        }))
        .unwrap();
        assert!(decode_manifest(&unknown, None, None, now()).is_err());
        let bad_version = serde_json::to_vec(&serde_json::json!({
            "schema": 1, "version": "1.2", "expires_at": "2026-10-01T00:00:00Z", "assets": [],
        }))
        .unwrap();
        assert!(decode_manifest(&bad_version, None, None, now()).is_err());
    }

    #[test]
    fn platform_names_follow_the_cli() {
        assert_eq!(
            manifest_platform("windows", "x86_64"),
            Some(("windows", "amd64"))
        );
        assert_eq!(
            manifest_platform("macos", "aarch64"),
            Some(("darwin", "arm64"))
        );
        assert_eq!(
            manifest_platform("macos", "x86_64"),
            Some(("darwin", "amd64"))
        );
        assert_eq!(
            manifest_platform("linux", "x86_64"),
            Some(("linux", "amd64"))
        );
        assert_eq!(manifest_platform("windows", "x86"), None);
    }

    #[test]
    fn asset_selection_per_platform() {
        let raw = manifest_json(vec![
            asset("windows", "amd64"),
            asset("darwin", "arm64"),
            asset("darwin", "amd64"),
            asset("linux", "amd64"),
        ]);
        let manifest = decode_manifest(&raw, None, None, now()).unwrap();
        for (os, arch) in [
            ("windows", "amd64"),
            ("darwin", "arm64"),
            ("darwin", "amd64"),
            ("linux", "amd64"),
        ] {
            let picked = select_asset(&manifest, os, arch, MAX_ARCHIVE_SIZE).unwrap();
            assert_eq!((picked.os.as_str(), picked.arch.as_str()), (os, arch));
        }
        assert!(select_asset(&manifest, "windows", "arm64", MAX_ARCHIVE_SIZE).is_err());
        let dup = decode_manifest(
            &manifest_json(vec![asset("windows", "amd64"), asset("windows", "amd64")]),
            None,
            None,
            now(),
        )
        .unwrap();
        assert!(select_asset(&dup, "windows", "amd64", MAX_ARCHIVE_SIZE).is_err());
    }

    #[test]
    fn asset_urls_sizes_and_digests_are_validated() {
        let mut bad_url = asset("windows", "amd64");
        bad_url["url"] = "http://dl.example.com/x.zip".into();
        let mut creds = asset("windows", "amd64");
        creds["signature_url"] = "https://user:pw@dl.example.com/x.minisig".into();
        let mut size = asset("windows", "amd64");
        size["size"] = 0.into();
        let mut digest = asset("windows", "amd64");
        digest["sha256"] = "zz".into();
        for bad in [bad_url, creds, size, digest] {
            let manifest =
                decode_manifest(&manifest_json(vec![bad.clone()]), None, None, now()).unwrap();
            assert!(
                select_asset(&manifest, "windows", "amd64", MAX_ARCHIVE_SIZE).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn archive_verification_checks_size_signature_and_digest() {
        let signer = TestSigner::new(5);
        let key = signer.public_key_text();
        let archive = b"0123456789".to_vec();
        let mut good: Asset = serde_json::from_value(asset("windows", "amd64")).unwrap();
        good.sha256 = sha256_hex(&archive);
        verify_archive(&archive, &good, Some(&signer.sign(&archive)), Some(&key)).unwrap();
        verify_archive(&archive, &good, None, None).unwrap();
        assert!(
            verify_archive(&archive, &good, None, Some(&key)).is_err(),
            "有公钥时必须带签名"
        );
        assert!(
            verify_archive(b"0123456788", &good, None, None).is_err(),
            "摘要不符"
        );
        assert!(
            verify_archive(b"012345678", &good, None, None).is_err(),
            "大小不符"
        );
    }

    #[test]
    fn zip_slip_names_are_rejected() {
        for bad in [
            "../evil",
            "a/../../evil",
            "/abs",
            "C:/x",
            "a\\b",
            "..",
            "",
            "a/\0b",
            "./",
        ] {
            assert!(safe_entry_path(bad).is_none(), "{bad:?}");
        }
        assert_eq!(
            safe_entry_path("app/entry.mjs"),
            Some(Path::new("app").join("entry.mjs"))
        );
        assert_eq!(
            safe_entry_path("./recodex-remote.exe"),
            Some(PathBuf::from("recodex-remote.exe"))
        );
        assert_eq!(safe_entry_path("app/"), Some(PathBuf::from("app")));
    }

    fn build_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buf);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .unix_permissions(0o755);
            for (name, data) in entries {
                if name.ends_with('/') {
                    writer.add_directory(*name, options).unwrap();
                } else {
                    writer.start_file(*name, options).unwrap();
                    writer.write_all(data).unwrap();
                }
            }
            writer.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn extraction_writes_the_runtime_layout() {
        let temp = tempfile::tempdir().unwrap();
        let archive = build_zip(&[
            ("recodex-remote.exe", b"MZ"),
            ("app/", b""),
            ("app/entry.mjs", b"console.log(1)"),
        ]);
        extract_zip(&archive, temp.path()).unwrap();
        assert!(temp.path().join("recodex-remote.exe").is_file());
        assert_eq!(
            std::fs::read(temp.path().join("app").join("entry.mjs")).unwrap(),
            b"console.log(1)"
        );
    }

    #[test]
    fn extraction_refuses_zip_slip_before_writing_outside() {
        let temp = tempfile::tempdir().unwrap();
        let dest = temp.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let archive = build_zip(&[("ok.txt", b"1"), ("../escaped.txt", b"2")]);
        assert!(extract_zip(&archive, &dest).is_err());
        assert!(!temp.path().join("escaped.txt").exists());
    }

    #[test]
    fn extraction_refuses_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buf);
            writer
                .add_symlink(
                    "link",
                    "/etc/passwd",
                    zip::write::SimpleFileOptions::default(),
                )
                .unwrap();
            writer.finish().unwrap();
        }
        assert!(extract_zip(&buf.into_inner(), temp.path()).is_err());
    }
}
