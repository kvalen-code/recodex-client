//! 远程组件守护进程的开机自启(docs/remote-app-plan.md §2.7)。
//!
//! 命令行 `recodex app` 与桌面客户端写的是**同一项、同一个名字、逐字相同的内容**,谁写都幂等:
//!   - Windows:`HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 下的 `ReCodexRemote`
//!   - macOS:`~/Library/LaunchAgents/dev.recodex.remote.plist`
//!   - Linux:`~/.config/autostart/recodex-remote.desktop`
//!
//! 内容一律是「运行时 daemon start-sync」,由纯函数生成 —— 与 cmd/recodex/app_autostart.go
//! 的 buildWindowsRunValue / buildLaunchAgentPlist / buildLinuxDesktopEntry 逐字对照,
//! 单测里钉了三平台的完整期望文本。不加 conhost/vbs 之类藏窗口的包装:运行时的
//! recodex-remote.exe 本身是 GUI 子系统(A 期打包时改的)。

use std::path::{Path, PathBuf};

use super::layout::{REMOTE_HOME_ENV, RemoteHome, RuntimeCurrent};

pub const WINDOWS_RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
pub const WINDOWS_RUN_VALUE_NAME: &str = "ReCodexRemote";
pub const MACOS_LABEL: &str = "dev.recodex.remote";
pub const LINUX_AUTOSTART_FILE: &str = "recodex-remote.desktop";
const DAEMON_SYNC_VERB: &str = "start-sync";
/// macOS LaunchAgent 里给守护进程的 PATH(Apple Silicon 与 Intel 的 Homebrew 都在前面)。
/// 命令行写同一个 plist,这个值两边必须逐字一致。
pub const MACOS_DAEMON_PATH: &str =
    "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";

/// 要自启的命令。`home` 仅在数据目录是自定义的时候非空。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutostartEntry {
    pub executable: String,
    pub entry: String,
    pub home: Option<String>,
}

impl AutostartEntry {
    pub fn new(current: &RuntimeCurrent, home: &RemoteHome, os: &str) -> Self {
        Self {
            executable: current.executable(os).to_string_lossy().into_owned(),
            entry: current.entry().to_string_lossy().into_owned(),
            home: home
                .custom
                .then(|| home.root.to_string_lossy().into_owned()),
        }
    }

    fn args(&self) -> [&str; 4] {
        [&self.executable, &self.entry, "daemon", DAEMON_SYNC_VERB]
    }
}

/// Run 值的命令行。自定义数据目录时借 cmd 设环境变量 —— Run 项本身没有设环境变量的办法。
pub fn windows_run_value(entry: &AutostartEntry) -> String {
    let [exe, script, daemon, verb] = entry.args();
    let cmdline = format!("\"{exe}\" \"{script}\" {daemon} {verb}");
    match &entry.home {
        None => cmdline,
        Some(home) => format!("cmd.exe /d /s /c \"set \"{REMOTE_HOME_ENV}={home}\"&& {cmdline}\""),
    }
}

pub fn launch_agent_plist(entry: &AutostartEntry) -> String {
    let mut b = String::new();
    b.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    b.push_str("<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n");
    b.push_str("<plist version=\"1.0\">\n<dict>\n");
    b.push_str(&format!(
        "\t<key>Label</key>\n\t<string>{}</string>\n",
        xml_text(MACOS_LABEL)
    ));
    b.push_str("\t<key>ProgramArguments</key>\n\t<array>\n");
    for arg in entry.args() {
        b.push_str(&format!("\t\t<string>{}</string>\n", xml_text(arg)));
    }
    b.push_str("\t</array>\n");
    // launchd 拉起的进程 PATH 只有 /usr/bin:/bin:/usr/sbin:/sbin —— 守护进程要在 PATH 上找
    // codex(codexPath 没写时)、git、brew 装的工具,必须显式给。
    b.push_str("\t<key>EnvironmentVariables</key>\n\t<dict>\n");
    b.push_str(&format!(
        "\t\t<key>PATH</key>\n\t\t<string>{}</string>\n",
        xml_text(MACOS_DAEMON_PATH)
    ));
    if let Some(home) = &entry.home {
        b.push_str(&format!(
            "\t\t<key>{REMOTE_HOME_ENV}</key>\n\t\t<string>{}</string>\n",
            xml_text(home)
        ));
    }
    b.push_str("\t</dict>\n");
    // 只 RunAtLoad,不 KeepAlive:守护进程自己有单实例锁;KeepAlive 会把正常退出当崩溃反复拉起。
    // 不设 ProcessType=Background:那会让 launchd 给整棵进程树降 I/O 与 CPU 优先级,
    // 守护进程拉起的 Codex 会被限速。
    b.push_str("\t<key>RunAtLoad</key>\n\t<true/>\n");
    b.push_str("</dict>\n</plist>\n");
    b
}

/// 与 Go 的 xml.EscapeText 同一套转义(引号用数字实体,控制空白也转)。
fn xml_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("&#34;"),
            '\'' => out.push_str("&#39;"),
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\t' => out.push_str("&#x9;"),
            '\n' => out.push_str("&#xA;"),
            '\r' => out.push_str("&#xD;"),
            c if !is_xml_char(c) => out.push('\u{FFFD}'),
            c => out.push(c),
        }
    }
    out
}

fn is_xml_char(c: char) -> bool {
    matches!(c as u32,
        0x09 | 0x0A | 0x0D | 0x20..=0xD7FF | 0xE000..=0xFFFD | 0x10000..=0x10FFFF)
}

pub fn linux_desktop_entry(entry: &AutostartEntry) -> String {
    let mut parts = Vec::new();
    if let Some(home) = &entry.home {
        parts.push("env".to_string());
        parts.push(desktop_exec_quote(&format!("{REMOTE_HOME_ENV}={home}")));
    }
    for arg in entry.args() {
        parts.push(desktop_exec_quote(arg));
    }
    format!(
        "[Desktop Entry]\nType=Application\nName=ReCodex Remote\nComment=ReCodex mobile remote control\nExec={}\nTerminal=false\nNoDisplay=true\nX-GNOME-Autostart-enabled=true\n",
        parts.join(" ")
    )
}

fn desktop_exec_quote(arg: &str) -> String {
    const RESERVED: &str = " \t\n\"'\\><~|&;$*?#()`=%";
    if !arg.is_empty() && !arg.chars().any(|c| RESERVED.contains(c)) {
        return arg.to_string();
    }
    let mut inner = String::new();
    for ch in arg.chars() {
        match ch {
            '\\' => inner.push_str("\\\\"),
            '"' => inner.push_str("\\\""),
            '`' => inner.push_str("\\`"),
            '$' => inner.push_str("\\$"),
            c => inner.push(c),
        }
    }
    // Desktop Entry 的值本身还要再转义一次反斜杠,% 要写成 %%。
    let quoted = format!("\"{inner}\"").replace('\\', "\\\\");
    quoted.replace('%', "%%")
}

pub fn launch_agent_path(user_home: &Path) -> PathBuf {
    user_home
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{MACOS_LABEL}.plist"))
}

pub fn linux_autostart_path(user_home: &Path, xdg_config_home: Option<&str>) -> PathBuf {
    let base = xdg_config_home
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| user_home.join(".config"));
    base.join("autostart").join(LINUX_AUTOSTART_FILE)
}

// ── 落地(平台相关,不做单测;内容由上面的纯函数决定)────────────────────

static SUPPRESSED_FOR_TESTS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 集成测试用:整条流程照跑,但不碰真实的注册表 / LaunchAgents(与 paths 的 *_for_tests 同一做法)。
pub fn set_suppressed_for_tests(suppressed: bool) {
    SUPPRESSED_FOR_TESTS.store(suppressed, std::sync::atomic::Ordering::SeqCst);
}

fn suppressed() -> bool {
    SUPPRESSED_FOR_TESTS.load(std::sync::atomic::Ordering::SeqCst)
}

/// 登记(幂等:已是同一内容就什么都不做 —— macOS 上重登记会顺手停掉 launchd 拉起的守护进程)。
pub fn install(entry: &AutostartEntry) -> anyhow::Result<()> {
    if suppressed() {
        return Ok(());
    }
    platform::install(entry)
}

/// 撤销。不存在视为成功。
pub fn remove() -> anyhow::Result<()> {
    if suppressed() {
        return Ok(());
    }
    platform::remove()
}

pub fn registered() -> bool {
    !suppressed() && platform::registered()
}

#[cfg(windows)]
mod platform {
    use super::*;

    fn current_value() -> Option<String> {
        crate::windows_integration::read_current_user_string_values(WINDOWS_RUN_KEY)
            .ok()?
            .into_iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(WINDOWS_RUN_VALUE_NAME))
            .and_then(|(_, value)| value)
    }

    pub fn install(entry: &AutostartEntry) -> anyhow::Result<()> {
        let value = windows_run_value(entry);
        if current_value().as_deref() == Some(value.as_str()) {
            return Ok(());
        }
        crate::windows_integration::set_current_user_string_value(
            WINDOWS_RUN_KEY,
            WINDOWS_RUN_VALUE_NAME,
            &value,
        )
    }

    pub fn remove() -> anyhow::Result<()> {
        if current_value().is_none() {
            return Ok(());
        }
        crate::windows_integration::delete_current_user_value(
            WINDOWS_RUN_KEY,
            WINDOWS_RUN_VALUE_NAME,
        )
    }

    pub fn registered() -> bool {
        current_value().is_some()
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    fn user_home() -> anyhow::Result<PathBuf> {
        directories::BaseDirs::new()
            .map(|dirs| dirs.home_dir().to_path_buf())
            .ok_or_else(|| anyhow::anyhow!("找不到用户目录"))
    }

    fn domain() -> String {
        format!("gui/{}", unsafe { libc::getuid() })
    }

    fn launchctl(args: &[&str]) -> std::io::Result<std::process::Output> {
        std::process::Command::new("launchctl").args(args).output()
    }

    pub fn install(entry: &AutostartEntry) -> anyhow::Result<()> {
        let path = launch_agent_path(&user_home()?);
        let content = launch_agent_plist(entry);
        if std::fs::read_to_string(&path).is_ok_and(|existing| existing == content) {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        super::super::layout::write_file_replace(&path, content.as_bytes())?;
        let path_text = path.to_string_lossy().into_owned();
        // 先卸掉旧登记(可能指向旧版本目录;失败说明本来没登记),再 bootstrap;老系统退回 load -w。
        let _ = launchctl(&["bootout", &format!("{}/{MACOS_LABEL}", domain())]);
        let bootstrapped =
            launchctl(&["bootstrap", &domain(), &path_text]).is_ok_and(|out| out.status.success());
        if !bootstrapped {
            let out = launchctl(&["load", "-w", &path_text])?;
            if !out.status.success() {
                anyhow::bail!(
                    "launchctl bootstrap/load 失败:{}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
        }
        Ok(())
    }

    pub fn remove() -> anyhow::Result<()> {
        let path = launch_agent_path(&user_home()?);
        if !path.exists() {
            return Ok(());
        }
        let _ = launchctl(&["bootout", &format!("{}/{MACOS_LABEL}", domain())]);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn registered() -> bool {
        user_home().is_ok_and(|home| launch_agent_path(&home).exists())
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod platform {
    use super::*;

    fn path() -> anyhow::Result<PathBuf> {
        let home = directories::BaseDirs::new()
            .map(|dirs| dirs.home_dir().to_path_buf())
            .ok_or_else(|| anyhow::anyhow!("找不到用户目录"))?;
        Ok(linux_autostart_path(
            &home,
            std::env::var("XDG_CONFIG_HOME").ok().as_deref(),
        ))
    }

    pub fn install(entry: &AutostartEntry) -> anyhow::Result<()> {
        let path = path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        super::super::layout::write_file_replace(&path, linux_desktop_entry(entry).as_bytes())?;
        Ok(())
    }

    pub fn remove() -> anyhow::Result<()> {
        match std::fs::remove_file(path()?) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn registered() -> bool {
        path().is_ok_and(|path| path.exists())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win_entry(home: Option<&str>) -> AutostartEntry {
        AutostartEntry {
            executable: r"C:\Users\me\.recodex\remote\runtime\1.2.3\recodex-remote.exe".into(),
            entry: r"C:\Users\me\.recodex\remote\runtime\1.2.3\app\entry.mjs".into(),
            home: home.map(str::to_string),
        }
    }

    /// 与命令行 buildWindowsRunValue 的输出逐字一致(值名 ReCodexRemote)。
    #[test]
    fn windows_run_value_matches_the_cli() {
        assert_eq!(
            windows_run_value(&win_entry(None)),
            r#""C:\Users\me\.recodex\remote\runtime\1.2.3\recodex-remote.exe" "C:\Users\me\.recodex\remote\runtime\1.2.3\app\entry.mjs" daemon start-sync"#
        );
        assert_eq!(
            windows_run_value(&win_entry(Some(r"D:\rr"))),
            r#"cmd.exe /d /s /c "set "RECODEX_REMOTE_HOME=D:\rr"&& "C:\Users\me\.recodex\remote\runtime\1.2.3\recodex-remote.exe" "C:\Users\me\.recodex\remote\runtime\1.2.3\app\entry.mjs" daemon start-sync""#
        );
        assert_eq!(WINDOWS_RUN_VALUE_NAME, "ReCodexRemote");
    }

    #[test]
    fn launch_agent_plist_matches_the_cli() {
        let entry = AutostartEntry {
            executable: "/Users/me/.recodex/remote/runtime/1.2.3/recodex-remote".into(),
            entry: "/Users/me/.recodex/remote/runtime/1.2.3/app/entry.mjs".into(),
            home: None,
        };
        let expected = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n<dict>\n\
\t<key>Label</key>\n\t<string>dev.recodex.remote</string>\n\
\t<key>ProgramArguments</key>\n\t<array>\n\
\t\t<string>/Users/me/.recodex/remote/runtime/1.2.3/recodex-remote</string>\n\
\t\t<string>/Users/me/.recodex/remote/runtime/1.2.3/app/entry.mjs</string>\n\
\t\t<string>daemon</string>\n\
\t\t<string>start-sync</string>\n\
\t</array>\n\
\t<key>EnvironmentVariables</key>\n\t<dict>\n\
\t\t<key>PATH</key>\n\t\t<string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>\n\
\t</dict>\n\
\t<key>RunAtLoad</key>\n\t<true/>\n\
</dict>\n</plist>\n";
        assert_eq!(launch_agent_plist(&entry), expected);

        let custom = AutostartEntry {
            home: Some("/tmp/a&b".into()),
            ..entry
        };
        let text = launch_agent_plist(&custom);
        assert!(text.contains(
            "\t\t<string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>\n\t\t<key>RECODEX_REMOTE_HOME</key>\n\t\t<string>/tmp/a&amp;b</string>\n\t</dict>\n\t<key>RunAtLoad</key>"
        ), "{text}");
        assert!(!text.contains("ProcessType"), "Background 会给 Codex 限速");
        assert_eq!(
            launch_agent_path(Path::new("/Users/me")),
            Path::new("/Users/me/Library/LaunchAgents/dev.recodex.remote.plist")
        );
    }

    #[test]
    fn xml_escaping_follows_go() {
        assert_eq!(xml_text(r#"a"b'c<d>e&f"#), "a&#34;b&#39;c&lt;d&gt;e&amp;f");
        assert_eq!(xml_text("t\tn\nr\r"), "t&#x9;n&#xA;r&#xD;");
    }

    #[test]
    fn linux_desktop_entry_matches_the_cli() {
        let entry = AutostartEntry {
            executable: "/home/me/.recodex/remote/runtime/1.2.3/recodex-remote".into(),
            entry: "/home/me/My Stuff/app/entry.mjs".into(),
            home: Some("/home/me/rr 1".into()),
        };
        assert_eq!(
            linux_desktop_entry(&entry),
            "[Desktop Entry]\nType=Application\nName=ReCodex Remote\nComment=ReCodex mobile remote control\n\
Exec=env \"RECODEX_REMOTE_HOME=/home/me/rr 1\" /home/me/.recodex/remote/runtime/1.2.3/recodex-remote \"/home/me/My Stuff/app/entry.mjs\" daemon start-sync\n\
Terminal=false\nNoDisplay=true\nX-GNOME-Autostart-enabled=true\n"
        );
        assert_eq!(desktop_exec_quote("50%"), "\"50%%\"");
        assert_eq!(desktop_exec_quote(r"a\b"), r#""a\\\\b""#);
        assert_eq!(desktop_exec_quote("$x"), r#""\\$x""#);
        assert_eq!(
            linux_autostart_path(Path::new("/home/me"), None),
            Path::new("/home/me/.config/autostart/recodex-remote.desktop")
        );
        assert_eq!(
            linux_autostart_path(Path::new("/home/me"), Some("relative")),
            Path::new("/home/me/.config/autostart/recodex-remote.desktop")
        );
    }

    #[test]
    fn entry_carries_the_home_only_when_custom() {
        let current = RuntimeCurrent {
            version: "1.2.3".into(),
            dir: "/r/1.2.3".into(),
        };
        let default = RemoteHome {
            root: "/u/.recodex/remote".into(),
            custom: false,
        };
        let custom = RemoteHome {
            root: "/x/rr".into(),
            custom: true,
        };
        assert_eq!(AutostartEntry::new(&current, &default, "linux").home, None);
        assert_eq!(
            AutostartEntry::new(&current, &custom, "linux")
                .home
                .as_deref(),
            Some("/x/rr")
        );
        assert!(
            AutostartEntry::new(&current, &default, "windows")
                .executable
                .ends_with("recodex-remote.exe")
        );
    }
}
