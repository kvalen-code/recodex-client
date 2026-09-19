//! 远程组件运行时的下载与安装(docs/remote-app-plan.md §2.7),与命令行
//! cmd/recodex/app_install.go 同一流程:按账号拿 `channel=remote` 的清单地址 →
//! 验清单 → 挑本平台资产 → 下载并校验 → 解压到 `runtime/<版本>/` → 原子改 current.json。
//! 信任链见 manifest.rs 开头。

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt;

use super::layout::{self, RemoteHome, RuntimeCurrent};
use super::manifest::{self, Manifest};

/// 服务端说 remote 渠道还没配置,本机也没装过运行时。
pub const NOT_RELEASED: &str =
    "手机远程控制功能还没有正式发布,暂时无法安装远程组件。请留意 ReCodex 公告。";

#[derive(Debug, Clone)]
pub struct EnsureOutcome {
    pub current: RuntimeCurrent,
    /// 这次装了新版本且此前装过:调用方据此停掉旧守护进程。
    pub previous: Option<RuntimeCurrent>,
    pub updated: bool,
    /// 检查更新失败但本机已有可用运行时:继续用旧的,这里给一句说明(只进诊断日志)。
    pub warning: Option<String>,
}

/// 服务端下发的渠道(由 recodex-integration 取,阻塞调用在 spawn_blocking 里做)。
#[derive(Debug, Clone)]
pub struct ChannelInfo {
    pub available: bool,
    pub manifest_url: String,
}

/// 下载进度回调:(已下载字节, 总字节)。
pub type Progress<'a> = &'a (dyn Fn(u64, u64) + Send + Sync);

/// 整个下载安装的上限。单次连接/读取卡住由 http_client 的连接与读超时兜住,
/// 这里兜「一直在动但慢得离谱」。不设总超时的 client:35 MB 在慢线路上几分钟是常态。
const INSTALL_DEADLINE: Duration = Duration::from_secs(10 * 60);

/// 保证本机有可用运行时,有新版就装新版。检查更新本身失败时,已装过就继续用旧的。
pub async fn ensure_runtime(
    home: &RemoteHome,
    os: &'static str,
    arch: &'static str,
    channel: anyhow::Result<ChannelInfo>,
    progress: Progress<'_>,
) -> anyhow::Result<EnsureOutcome> {
    let installed = layout::read_runtime_current(home, os);
    let keep = |warning: String, error: anyhow::Error| -> anyhow::Result<EnsureOutcome> {
        match &installed {
            Some(current) => Ok(EnsureOutcome {
                current: current.clone(),
                previous: None,
                updated: false,
                warning: Some(warning),
            }),
            None => Err(error),
        }
    };
    let channel = match channel {
        Ok(channel) => channel,
        Err(error) => {
            return keep(
                format!("检查远程组件更新失败,继续使用已安装的版本:{error}"),
                anyhow::anyhow!("获取远程组件下载地址失败:{error}"),
            );
        }
    };
    if !channel.available {
        return match installed {
            Some(current) => Ok(EnsureOutcome {
                current,
                previous: None,
                updated: false,
                warning: None,
            }),
            None => Err(anyhow::anyhow!(NOT_RELEASED)),
        };
    }
    if !manifest::is_safe_https_url(&channel.manifest_url) {
        return keep(
            "服务端下发的远程组件清单地址不安全,已忽略".into(),
            anyhow::anyhow!("服务端下发的远程组件清单地址不安全"),
        );
    }
    let Some((m_os, m_arch)) = manifest::manifest_platform(os, arch) else {
        return keep(
            "远程组件不支持这个平台".into(),
            anyhow::anyhow!("远程组件暂不支持这个平台({os}/{arch})"),
        );
    };
    let installing = install_from_manifest(
        home,
        os,
        m_os,
        m_arch,
        &channel.manifest_url,
        installed.as_ref(),
        progress,
    );
    let result = tokio::time::timeout(INSTALL_DEADLINE, installing)
        .await
        .unwrap_or_else(|_| {
            Err(anyhow::anyhow!(
                "下载远程组件超时(10 分钟),请检查网络后重试"
            ))
        });
    match result {
        Ok(outcome) => Ok(outcome),
        Err(error) => keep(
            format!("远程组件更新失败,继续使用已安装的版本:{error}"),
            error,
        ),
    }
}

async fn install_from_manifest(
    home: &RemoteHome,
    os: &'static str,
    m_os: &str,
    m_arch: &str,
    manifest_url: &str,
    installed: Option<&RuntimeCurrent>,
    progress: Progress<'_>,
) -> anyhow::Result<EnsureOutcome> {
    let client = http_client()?;
    let public_key = manifest::release_public_key();
    let raw = fetch(&client, manifest_url, manifest::MAX_MANIFEST_SIZE, None)
        .await
        .map_err(|error| anyhow::anyhow!("下载远程组件清单失败:{error}"))?;
    let signature = match public_key {
        Some(_) => Some(
            fetch(
                &client,
                &format!("{manifest_url}.minisig"),
                manifest::MAX_SIGNATURE_SIZE,
                None,
            )
            .await
            .map_err(|error| anyhow::anyhow!("下载远程组件清单签名失败:{error}"))?,
        ),
        None => None,
    };
    let manifest: Manifest =
        manifest::decode_manifest(&raw, signature.as_deref(), public_key, chrono::Utc::now())?;
    if let Some(current) = installed {
        let newer = match (
            layout::parse_stable_version(&manifest.version),
            layout::parse_stable_version(&current.version),
        ) {
            (Some(next), Some(have)) => next > have,
            _ => false,
        };
        if !newer {
            // 同版本或清单更旧:保持现状。运行时不做降级,回滚靠发一个更高的版本号。
            return Ok(EnsureOutcome {
                current: current.clone(),
                previous: None,
                updated: false,
                warning: None,
            });
        }
    }
    let asset = manifest::select_asset(&manifest, m_os, m_arch, manifest::MAX_ARCHIVE_SIZE)?;
    progress(0, asset.size as u64);
    let archive = fetch(&client, &asset.url, asset.size as u64, Some(progress))
        .await
        .map_err(|error| anyhow::anyhow!("下载远程组件失败:{error}"))?;
    let archive_signature = match public_key {
        Some(_) => Some(
            fetch(
                &client,
                &asset.signature_url,
                manifest::MAX_SIGNATURE_SIZE,
                None,
            )
            .await
            .map_err(|error| anyhow::anyhow!("下载远程组件签名失败:{error}"))?,
        ),
        None => None,
    };
    manifest::verify_archive(&archive, &asset, archive_signature.as_deref(), public_key)?;
    let home_for_task = home.clone();
    let version = manifest.version.clone();
    // 解压是几十 MB 的同步 I/O,别占 async 线程
    let dir = tokio::task::spawn_blocking(move || {
        install_archive(&home_for_task, &version, &archive, os)
    })
    .await??;
    let next = RuntimeCurrent {
        version: manifest.version.clone(),
        dir: dir.to_string_lossy().into_owned(),
    };
    layout::write_runtime_current(home, &next)
        .map_err(|error| anyhow::anyhow!("记录当前远程组件版本失败:{error}"))?;
    Ok(EnsureOutcome {
        current: next,
        previous: installed.cloned(),
        updated: true,
        warning: None,
    })
}

/// 解压到 `runtime/<版本>/`:先解到同级临时目录、校验齐全,再整体改名 ——
/// 半截的解压结果永远不会出现在正式目录里(同命令行 installRuntimeArchive)。
pub fn install_archive(
    home: &RemoteHome,
    version: &str,
    archive: &[u8],
    os: &str,
) -> anyhow::Result<PathBuf> {
    if layout::parse_stable_version(version).is_none() {
        anyhow::bail!("远程组件版本号无效:{version}");
    }
    let root = home.runtime_root();
    std::fs::create_dir_all(&root)?;
    let staging = root.join(format!(".staging-{version}-{}", layout::random_suffix()));
    std::fs::create_dir(&staging)?;
    let result = (|| -> anyhow::Result<PathBuf> {
        manifest::extract_zip(archive, &staging)?;
        let probe = RuntimeCurrent {
            version: version.to_string(),
            dir: staging.to_string_lossy().into_owned(),
        };
        for path in [probe.executable(os), probe.entry()] {
            if !path.is_file() {
                anyhow::bail!(
                    "远程组件压缩包里缺少 {}",
                    path.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                );
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(probe.executable(os), std::fs::Permissions::from_mode(0o755))?;
        }
        let mut target = root.join(version);
        if std::fs::symlink_metadata(&target).is_ok() {
            // 同版本目录已存在(current.json 丢了之类):先挪开再删。Windows 上它可能正被
            // 运行中的守护进程占着,挪不动就换个目录名装 —— current.json 记的是绝对目录。
            let aside = root.join(format!(".old-{version}-{}", layout::random_suffix()));
            if std::fs::rename(&target, &aside).is_ok() {
                let _ = std::fs::remove_dir_all(&aside);
            } else {
                target = root.join(format!("{version}-{}", layout::random_suffix()));
            }
        }
        std::fs::rename(&staging, &target)
            .map_err(|error| anyhow::anyhow!("启用远程组件失败:{error}"))?;
        Ok(absolute(&target))
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

fn http_client() -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(format!("ReCodex/{}", crate::version::VERSION))
        // 不设总超时(大包在慢线路上要好几分钟):连接 15 秒、两次读之间 60 秒没动静就判断卡死,
        // 整体上限在 ensure_runtime 里(INSTALL_DEADLINE)。
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        // 对象存储/CDN 可能跳一次;跳到非 https 的在 fetch 里拒掉
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()?)
}

/// 下载到内存,超过上限立刻中止(不先信 Content-Length)。
async fn fetch(
    client: &reqwest::Client,
    url: &str,
    max: u64,
    progress: Option<Progress<'_>>,
) -> anyhow::Result<Vec<u8>> {
    if !manifest::is_safe_https_url(url) {
        anyhow::bail!("地址必须是 https");
    }
    let response = client.get(url).send().await?.error_for_status()?;
    if response.url().scheme() != "https" {
        anyhow::bail!("下载被重定向到了非 https 地址");
    }
    let mut stream = response.bytes_stream();
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if out.len() as u64 + chunk.len() as u64 > max {
            anyhow::bail!("内容超过大小上限 {max} 字节");
        }
        out.extend_from_slice(&chunk);
        if let Some(progress) = progress {
            progress(out.len() as u64, max);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn zip_with(entries: &[&str]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buf);
            let options = zip::write::SimpleFileOptions::default();
            for name in entries {
                writer.start_file(*name, options).unwrap();
                writer.write_all(b"x").unwrap();
            }
            writer.finish().unwrap();
        }
        buf.into_inner()
    }

    fn home(temp: &tempfile::TempDir) -> RemoteHome {
        RemoteHome {
            root: temp.path().join("remote"),
            custom: true,
        }
    }

    #[test]
    fn archive_installs_into_a_versioned_directory() {
        let temp = tempfile::tempdir().unwrap();
        let home = home(&temp);
        let dir = install_archive(
            &home,
            "1.2.3",
            &zip_with(&["recodex-remote.exe", "app/entry.mjs"]),
            "windows",
        )
        .unwrap();
        assert_eq!(dir, home.runtime_root().join("1.2.3"));
        assert!(dir.join("app").join("entry.mjs").is_file());
        // 没有残留的 staging 目录
        let leftovers: Vec<_> = std::fs::read_dir(home.runtime_root())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with('.'))
            .collect();
        assert!(leftovers.is_empty());
        // 同版本再装一次:旧目录让开,新的就位
        let again = install_archive(
            &home,
            "1.2.3",
            &zip_with(&["recodex-remote.exe", "app/entry.mjs"]),
            "windows",
        )
        .unwrap();
        assert!(again.join("recodex-remote.exe").is_file());
    }

    #[test]
    fn incomplete_archives_are_refused_and_cleaned_up() {
        let temp = tempfile::tempdir().unwrap();
        let home = home(&temp);
        assert!(install_archive(&home, "1.2.3", &zip_with(&["app/entry.mjs"]), "windows").is_err());
        assert!(
            install_archive(
                &home,
                "1.2.3",
                &zip_with(&["recodex-remote", "app/entry.mjs"]),
                "windows"
            )
            .is_err()
        );
        assert!(
            install_archive(
                &home,
                "1.2",
                &zip_with(&["recodex-remote.exe", "app/entry.mjs"]),
                "windows"
            )
            .is_err()
        );
        let entries: Vec<_> = std::fs::read_dir(home.runtime_root())
            .unwrap()
            .flatten()
            .collect();
        assert!(entries.is_empty(), "失败的安装不能留下目录: {entries:?}");
    }

    #[tokio::test]
    async fn unreleased_channel_without_a_runtime_says_so() {
        let temp = tempfile::tempdir().unwrap();
        let home = home(&temp);
        let error = ensure_runtime(
            &home,
            "windows",
            "x86_64",
            Ok(ChannelInfo {
                available: false,
                manifest_url: String::new(),
            }),
            &|_, _| {},
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), NOT_RELEASED);
    }

    #[tokio::test]
    async fn channel_failure_keeps_an_installed_runtime() {
        let temp = tempfile::tempdir().unwrap();
        let home = home(&temp);
        let dir = install_archive(
            &home,
            "1.0.0",
            &zip_with(&["recodex-remote.exe", "app/entry.mjs"]),
            "windows",
        )
        .unwrap();
        let current = RuntimeCurrent {
            version: "1.0.0".into(),
            dir: dir.to_string_lossy().into_owned(),
        };
        layout::write_runtime_current(&home, &current).unwrap();
        let outcome = ensure_runtime(
            &home,
            "windows",
            "x86_64",
            Err(anyhow::anyhow!("offline")),
            &|_, _| {},
        )
        .await
        .unwrap();
        assert_eq!(outcome.current, current);
        assert!(!outcome.updated && outcome.warning.is_some());
        // 不安全的清单地址:同样继续用旧的
        let outcome = ensure_runtime(
            &home,
            "windows",
            "x86_64",
            Ok(ChannelInfo {
                available: true,
                manifest_url: "http://evil.example/m.json".into(),
            }),
            &|_, _| {},
        )
        .await
        .unwrap();
        assert_eq!(outcome.current, current);
        // 没装过又拿不到渠道:报错
        let empty = RemoteHome {
            root: temp.path().join("other"),
            custom: true,
        };
        assert!(
            ensure_runtime(
                &empty,
                "windows",
                "x86_64",
                Err(anyhow::anyhow!("offline")),
                &|_, _| {}
            )
            .await
            .is_err()
        );
    }
}
