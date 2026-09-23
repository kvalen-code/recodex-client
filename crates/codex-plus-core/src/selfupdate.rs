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
    /// 运维显式开的回滚开关:允许装一个**比当前旧**的版本。
    ///
    /// 没有它的话,下面那道降级闸会把**回滚这条退路一起堵死** —— 而回滚正是
    /// 发版出问题时唯一的应急手段(把服务端的 manifest_url 指回上一版,
    /// 让所有人自愈)。以现在的发版节奏,这条退路比多一道闸值钱。
    ///
    /// 默认 false:清单是我们自己生成的,不写这个字段就是正常发版。
    /// 要回滚时手工在那一版的 manifest.json 里加 `"allow_downgrade": true`。
    #[serde(default)]
    pub allow_downgrade: bool,
    /// recodex-overlay: 租约直连的本机代理(recodex-lease),与主程序同目录。
    ///
    /// 可选:只有当次发布捆了 sidecar 时 CI 才写这个字段。老客户端不认识它、照常只换
    /// 主程序;新客户端在主程序换好之后**尽力**换它 —— 它失败绝不让整次更新失败,
    /// 没有它时桌面端一切照旧走网关。
    #[serde(default)]
    pub sidecar: Option<SidecarAsset>,
}

/// 附带文件的下载地址与哈希。安全要求与主程序完全相同:https + sha256 + 可执行格式。
#[derive(Debug, Clone, Deserialize)]
pub struct SidecarAsset {
    pub url: String,
    pub sha256: String,
}

/// sidecar 在安装目录里的文件名。与 recodex_integration::lease_sidecar 一致。
pub fn sidecar_file_name() -> &'static str {
    if cfg!(windows) {
        "recodex-lease.exe"
    } else {
        "recodex-lease"
    }
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
    if !manifest.allow_downgrade {
        reject_non_upgrade(&manifest.version, crate::version::VERSION)?;
    }
    Ok(manifest)
}

/// 拒绝安装一个不比当前新的包。
///
/// 2026-09-07 线上踩到:服务端有**两个**独立设置项 —— `recodex_client_latest_version`
/// 和 `recodex_client_manifest_url`,发版时要一起改。只改了前者,于是所有用户被告知
/// 「有新版本 1.3.3」,点下去下载的却是 1.3.0 的包。更糟的是服务端的 `AvailableFor`
/// 根本不比较版本号(只看两个字段非空),所以连已经在 1.3.3 上的用户也被推更新 ——
/// 装完变 1.3.0,重启后还是被告知有 1.3.3,**无限降级循环**。
///
/// 客户端这边本来一路照装:`self_update_value` 拿到清单就下载、校验 sha256、替换。
/// sha256 只能保证「包没被掉包」,保证不了「这是个更新」。加这道闸之后,同类配置
/// 失误只会变成一句"已经是最新版本",而不是把用户降级并锁死。
///
/// 版本号解析不出来时**放行**:出货的版本号一律是三段纯数字(publish-desktop.sh 会校验),
/// 解析失败多半是将来换了格式(带 -beta 之类)。为一个没见过的格式把自更新永久堵死,
/// 比放行一次更糟 —— sha256 与 https 那两道闸仍然在。
fn reject_non_upgrade(candidate: &str, current: &str) -> anyhow::Result<()> {
    let (Some(candidate_parts), Some(current_parts)) =
        (parse_three_part(candidate), parse_three_part(current))
    else {
        return Ok(());
    };
    if candidate_parts > current_parts {
        return Ok(());
    }
    if candidate_parts == current_parts {
        anyhow::bail!("已经是最新版本 {current},无需更新");
    }
    anyhow::bail!(
        "更新清单给的是 {candidate},比当前的 {current} 还旧 —— 拒绝降级。\
         这多半是服务端的更新配置指错了版本,请联系管理员"
    )
}

fn parse_three_part(value: &str) -> Option<[u64; 3]> {
    let mut parts = value.trim().split('.');
    let mut out = [0u64; 3];
    for slot in &mut out {
        *slot = parts.next()?.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(out)
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
    download_checked(&manifest.url, &manifest.sha256, "安装包").await
}

/// 下载并校验 sidecar。规矩与主程序一样。
pub async fn download_sidecar_verified(asset: &SidecarAsset) -> anyhow::Result<Vec<u8>> {
    if asset.sha256.trim().is_empty() {
        anyhow::bail!("sidecar 缺少 sha256,拒绝安装");
    }
    download_checked(&asset.url, &asset.sha256, "sidecar").await
}

async fn download_checked(url: &str, sha256: &str, what: &str) -> anyhow::Result<Vec<u8>> {
    let url = require_https(url, what)?;
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
    let expected = sha256.trim().to_ascii_lowercase();
    if actual != expected {
        anyhow::bail!("{what}校验失败(期望 {expected},实际 {actual}),已丢弃");
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
    ensure_native_executable_format(bytes, std::env::consts::OS)
}

/// 包体必须是**本平台**的可执行格式。
///
/// 服务端的更新清单不分平台:同一个 manifest_url 发给所有客户端,指着的是 Windows 的
/// recodex.exe。原先这里只在 Windows 上查 MZ 头,macOS 上什么都不查 —— 于是 mac 用户
/// 在面板里点「更新」,会把一个 Windows exe 写进 `.app/Contents/MacOS/`,重启即死,
/// 而且和上面 JSON 那种情况一样没有任何代码会把 `.old` 换回来。
///
/// 按目标系统认魔数,不认识的平台一律拒绝(宁可更新不了,也不能把自己换成打不开的东西)。
/// `os` 取 `std::env::consts::OS`,做成参数是为了让每个平台的分支在任何机器上都测得到。
fn ensure_native_executable_format(bytes: &[u8], os: &str) -> anyhow::Result<()> {
    let ok = match os {
        "windows" => bytes.starts_with(b"MZ"),
        "macos" => is_mach_o(bytes),
        "linux" => bytes.starts_with(b"\x7fELF"),
        _ => false,
    };
    if ok {
        return Ok(());
    }
    let expected = match os {
        "windows" => "Windows 可执行文件(MZ 头)",
        "macos" => "macOS 可执行文件(Mach-O)",
        "linux" => "Linux 可执行文件(ELF)",
        _ => "本平台支持的可执行文件",
    };
    anyhow::bail!(
        "安装包不是{expected},已拒绝替换{}",
        if detect_executable_os(bytes).is_some_and(|found| found != os) {
            "(下载到的是别的平台的安装包,多半是服务端的更新清单没有区分平台)"
        } else {
            ""
        }
    )
}

/// Mach-O 的几种魔数:32/64 位、两种字节序,以及 universal(fat)包。
fn is_mach_o(bytes: &[u8]) -> bool {
    const MAGICS: [[u8; 4]; 6] = [
        [0xcf, 0xfa, 0xed, 0xfe], // MH_MAGIC_64,小端(arm64 / x86_64 实际就是这个)
        [0xce, 0xfa, 0xed, 0xfe], // MH_MAGIC,小端
        [0xfe, 0xed, 0xfa, 0xcf], // MH_MAGIC_64,大端
        [0xfe, 0xed, 0xfa, 0xce], // MH_MAGIC,大端
        [0xca, 0xfe, 0xba, 0xbe], // FAT_MAGIC(universal)
        [0xca, 0xfe, 0xba, 0xbf], // FAT_MAGIC_64
    ];
    bytes.len() >= 4 && MAGICS.iter().any(|magic| bytes[..4] == magic[..])
}

/// 只用于把错误信息说清楚:认得出是哪个平台的包就说出来。
fn detect_executable_os(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"MZ") {
        Some("windows")
    } else if is_mach_o(bytes) {
        Some("macos")
    } else if bytes.starts_with(b"\x7fELF") {
        Some("linux")
    } else {
        None
    }
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

/// 把新的 sidecar 就位到 `target`(与主程序同目录)。
///
/// 与 `stage_replacement` 的区别只有一处:目标可以**不存在**(老包装上来的机器没有它,
/// 这正是要靠自更新补上的情况)。存在时同样先改名成 `.old` —— 常驻代理可能正跑着它,
/// Windows 上运行中的 exe 不能覆盖、只能改名。旧进程继续用改名后的文件跑,
/// 下次桌面端启动时发现版本不同会把它换掉(Go 侧 desktop-follow)。
pub fn stage_sidecar(target: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    ensure_executable_payload(bytes)?;
    let new_path = with_extension(target, ".new");
    let old_path = with_extension(target, ".old");

    let mut file = std::fs::File::create(&new_path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&new_path, std::fs::Permissions::from_mode(0o755))?;
    }

    if target.exists() {
        let _ = std::fs::remove_file(&old_path);
        if let Err(error) = std::fs::rename(target, &old_path) {
            let _ = std::fs::remove_file(&new_path);
            return Err(error.into());
        }
    }
    if let Err(error) = std::fs::rename(&new_path, target) {
        // 放回旧的:宁可留着旧版 sidecar,也不能让它凭空消失
        let _ = std::fs::rename(&old_path, target);
        let _ = std::fs::remove_file(&new_path);
        return Err(error.into());
    }
    Ok(())
}

/// 启动时清理上一轮更新留下的 `.old`(那时它已不再被占用)。
///
/// sidecar 的 `.old` 可能还被常驻代理的旧进程占着 —— 删不掉就留到下次,不报错。
pub fn cleanup_previous_update() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(with_extension(&exe, ".old"));
        let _ = std::fs::remove_file(with_extension(&exe, ".new"));
        if let Some(dir) = exe.parent() {
            let sidecar = dir.join(sidecar_file_name());
            let _ = std::fs::remove_file(with_extension(&sidecar, ".old"));
            let _ = std::fs::remove_file(with_extension(&sidecar, ".new"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_native_exe() -> Vec<u8> {
        let mut bytes = vec![0u8; 70 * 1024];
        let magic: &[u8] = match std::env::consts::OS {
            "windows" => b"MZ",
            "macos" => &[0xcf, 0xfa, 0xed, 0xfe],
            _ => b"\x7fELF",
        };
        bytes[..magic.len()].copy_from_slice(magic);
        bytes
    }

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rcx-selfupdate-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 老清单没有 sidecar 字段:必须照常解析(老服务端配置 + 新客户端)。
    #[test]
    fn manifest_without_sidecar_still_parses() {
        let manifest: UpdateManifest =
            serde_json::from_str(r#"{"version":"1.3.9","url":"https://x/recodex.exe","sha256":"ab"}"#).unwrap();
        assert!(manifest.sidecar.is_none());
        let manifest: UpdateManifest = serde_json::from_str(
            r#"{"version":"1.3.9","url":"https://x/recodex.exe","sha256":"ab","sidecar":{"url":"https://x/recodex-lease.exe","sha256":"cd"}}"#,
        )
        .unwrap();
        assert_eq!(manifest.sidecar.unwrap().sha256, "cd");
    }

    /// 老包装上来的机器上没有 sidecar:自更新要能把它**新放**进去。
    #[test]
    fn stage_sidecar_installs_when_absent_and_replaces_when_present() {
        let dir = scratch_dir("sidecar");
        let target = dir.join(sidecar_file_name());
        let first = fake_native_exe();
        stage_sidecar(&target, &first).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), first);
        assert!(!with_extension(&target, ".new").exists(), "不该留下 .new");

        let mut second = fake_native_exe();
        second[100] = 7;
        stage_sidecar(&target, &second).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), second);
        assert_eq!(std::fs::read(with_extension(&target, ".old")).unwrap(), first, "旧版应改名留着");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// sidecar 同样不能被换成一个不是可执行文件的东西(清单填错地址的老坑)。
    #[test]
    fn stage_sidecar_rejects_non_executables_without_touching_the_old_one() {
        let dir = scratch_dir("sidecar-bad");
        let target = dir.join(sidecar_file_name());
        let good = fake_native_exe();
        stage_sidecar(&target, &good).unwrap();
        assert!(stage_sidecar(&target, br#"{"version":"1.3.9"}"#).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), good, "拒绝后旧的必须原样留着");
        let _ = std::fs::remove_dir_all(&dir);
    }

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

    /// 本平台的一个「像样的」包体:对的魔数 + 足够大。
    fn native_payload(fill: u8) -> Vec<u8> {
        let mut payload = match std::env::consts::OS {
            "windows" => b"MZ".to_vec(),
            "macos" => vec![0xcf, 0xfa, 0xed, 0xfe],
            _ => b"\x7fELF".to_vec(),
        };
        payload.resize(128 * 1024, fill);
        payload
    }

    fn sized(prefix: &[u8]) -> Vec<u8> {
        let mut payload = prefix.to_vec();
        payload.resize(128 * 1024, 0);
        payload
    }

    #[test]
    fn payload_without_native_header_is_rejected() {
        // 够大但不是可执行文件 —— 比如 OSS 返回的 HTML 错误页
        let junk = vec![b'<'; 128 * 1024];
        assert!(ensure_executable_payload(&junk).is_err());
    }

    #[test]
    fn plausible_executable_passes() {
        assert!(ensure_executable_payload(&native_payload(0)).is_ok());
    }

    /// 线上真实会发生的:更新清单不分平台,mac 客户端拿到的是 Windows 的 recodex.exe。
    /// 原先 mac 上根本不查格式,exe 会被写进 .app 里,重启即死。
    #[test]
    fn a_windows_exe_is_refused_on_macos() {
        let error = ensure_native_executable_format(&sized(b"MZ"), "macos")
            .unwrap_err()
            .to_string();
        assert!(error.contains("Mach-O"), "{error}");
        assert!(error.contains("别的平台"), "要说清是拿错了平台的包:{error}");
    }

    #[test]
    fn a_mach_o_binary_is_refused_on_windows() {
        let error = ensure_native_executable_format(&sized(&[0xcf, 0xfa, 0xed, 0xfe]), "windows")
            .unwrap_err()
            .to_string();
        assert!(error.contains("MZ"), "{error}");
        assert!(error.contains("别的平台"), "{error}");
    }

    #[test]
    fn every_mach_o_flavour_is_accepted_on_macos() {
        for magic in [
            [0xcf, 0xfa, 0xed, 0xfe],
            [0xce, 0xfa, 0xed, 0xfe],
            [0xfe, 0xed, 0xfa, 0xcf],
            [0xfe, 0xed, 0xfa, 0xce],
            [0xca, 0xfe, 0xba, 0xbe],
            [0xca, 0xfe, 0xba, 0xbf],
        ] {
            ensure_native_executable_format(&sized(&magic), "macos")
                .unwrap_or_else(|error| panic!("{magic:02x?} 应当被认作 Mach-O:{error}"));
        }
    }

    #[test]
    fn native_formats_pass_on_their_own_platform() {
        ensure_native_executable_format(&sized(b"MZ"), "windows").unwrap();
        ensure_native_executable_format(&sized(b"\x7fELF"), "linux").unwrap();
        assert!(ensure_native_executable_format(&sized(b"MZ"), "linux").is_err());
    }

    /// 没见过的平台一律拒绝:宁可更新不了,也不能把自己换成打不开的东西。
    #[test]
    fn unknown_platforms_refuse_everything() {
        for payload in [sized(b"MZ"), sized(&[0xcf, 0xfa, 0xed, 0xfe]), sized(b"\x7fELF")] {
            assert!(ensure_native_executable_format(&payload, "freebsd").is_err());
        }
    }

    #[test]
    fn staging_keeps_old_binary_for_rollback() {
        let dir = std::env::temp_dir().join(format!("recodex-stage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("app.exe");
        std::fs::write(&exe, b"old").unwrap();

        // 形态检查在最前面,所以这里得给一个像样的包体(本平台魔数 + 足够大)
        let payload = native_payload(7);

        let old = stage_replacement(&exe, &payload).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), payload);
        assert_eq!(std::fs::read(&old).unwrap(), b"old", "旧版本要留着以便回退");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod upgrade_guard_tests {
    use super::{parse_three_part, reject_non_upgrade, UpdateManifest};

    // 线上那次的形状:清单指着 1.3.0,用户在 1.3.2。照装就是降级。
    #[test]
    fn rejects_an_older_package() {
        let error = reject_non_upgrade("1.3.0", "1.3.2").unwrap_err().to_string();
        assert!(error.contains("拒绝降级"), "错误信息要说清是降级:{error}");
        assert!(error.contains("1.3.0") && error.contains("1.3.2"));
    }

    // 服务端的 AvailableFor 不比较版本号,所以已经在最新版的用户也会被推更新。
    // 装一遍同版本不致命,但那正是「更新完还提示有更新」看起来的样子。
    #[test]
    fn rejects_the_same_version() {
        let error = reject_non_upgrade("1.3.3", "1.3.3").unwrap_err().to_string();
        assert!(error.contains("已经是最新版本"), "{error}");
    }

    #[test]
    fn allows_a_real_upgrade() {
        for (candidate, current) in [("1.3.4", "1.3.3"), ("1.4.0", "1.3.9"), ("2.0.0", "1.9.9")] {
            reject_non_upgrade(candidate, current)
                .unwrap_or_else(|error| panic!("{candidate} 应当放行(当前 {current}):{error}"));
        }
    }

    // 逐段按数字比,不是按字符串 —— "1.3.10" 字符串比大小小于 "1.3.9"。
    #[test]
    fn compares_numerically_not_lexically() {
        reject_non_upgrade("1.3.10", "1.3.9").expect("1.3.10 比 1.3.9 新");
        assert!(reject_non_upgrade("1.3.9", "1.3.10").is_err());
    }

    // 解析不出来就放行:为一个没见过的版本号格式把自更新永久堵死,比放行一次更糟。
    #[test]
    fn allows_when_either_side_is_unparseable() {
        for (candidate, current) in [
            ("1.3.4-beta", "1.3.3"),
            ("1.3.4", "dev"),
            ("1.3", "1.3.3"),
            ("1.3.4.5", "1.3.3"),
        ] {
            reject_non_upgrade(candidate, current)
                .unwrap_or_else(|error| panic!("解析不了就该放行({candidate}/{current}):{error}"));
        }
    }

    /// 光有纯函数不算数 —— 得确认它**真的被挂在了** fetch_manifest 上。
    ///
    /// 实测过:把 fetch_manifest 里那句调用删掉,上面 12 条全绿。
    /// 而今天这次事故的形状恰恰就是「两个设置项只改了一个」——一个没人验证的接线。
    /// fetch_manifest 本身要发 https 请求,单测里跑不起来,所以钉文本。
    #[test]
    fn the_guard_is_actually_wired_into_fetch_manifest() {
        let source = include_str!("selfupdate.rs");
        let body = source
            .split_once("pub async fn fetch_manifest")
            .expect("找不到 fetch_manifest")
            .1;
        let body = &body[..body.find("
/// ").unwrap_or(body.len())];
        assert!(
            body.contains("reject_non_upgrade("),
            "fetch_manifest 没有调用 reject_non_upgrade —— 降级闸只是躺在那儿"
        );
        assert!(
            body.contains("allow_downgrade"),
            "回滚开关没接上 —— 一旦某版有问题,把清单指回上一版也救不回用户"
        );
    }

    /// 回滚开关放行降级,这是发版出事时唯一的自愈手段。
    #[test]
    fn allow_downgrade_lets_operations_roll_back() {
        // 闸本身仍然拒绝(它不看这个字段),放行发生在调用点。
        assert!(reject_non_upgrade("1.3.0", "1.3.4").is_err());
        // 字段默认必须是 false:清单里不写就是正常发版,不能默认开着。
        let manifest: UpdateManifest = serde_json::from_str(
            r#"{"version":"1.3.5","url":"https://x/y.exe","sha256":"ab"}"#,
        )
        .expect("清单少了 allow_downgrade 也要能解析");
        assert!(!manifest.allow_downgrade, "回滚开关默认必须是关的");
        let rollback: UpdateManifest = serde_json::from_str(
            r#"{"version":"1.3.3","url":"https://x/y.exe","sha256":"ab","allow_downgrade":true}"#,
        )
        .expect("带 allow_downgrade 的清单要能解析");
        assert!(rollback.allow_downgrade);
    }

    #[test]
    fn parses_only_three_numeric_parts() {
        assert_eq!(parse_three_part("1.3.3"), Some([1, 3, 3]));
        assert_eq!(parse_three_part(" 1.3.3 "), Some([1, 3, 3]));
        assert_eq!(parse_three_part("1.3"), None);
        assert_eq!(parse_three_part("1.3.3.1"), None);
        assert_eq!(parse_three_part("1.3.x"), None);
    }
}
