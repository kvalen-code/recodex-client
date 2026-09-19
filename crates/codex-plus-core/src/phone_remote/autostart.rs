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
/// macOS/Linux 自启项里 PATH 的固定部分(Apple Silicon 与 Intel 的 Homebrew 在前)。
/// launchd 默认只给 /usr/bin:/bin:/usr/sbin:/sbin,找不到 npm 装的 codex,codex 自己
/// (`#!/usr/bin/env node`)也找不到 node。前面再拼上 codexPath 所在目录(见
/// [`AutostartEntry::daemon_path`])。与命令行 remoteDaemonBasePath 逐字一致。
pub const DAEMON_BASE_PATH: &str = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";
/// Windows 自定义数据目录时给运行时(Node)`--env-file` 读的文件,放在数据目录里。
pub const WINDOWS_AUTOSTART_ENV_FILE: &str = "autostart.env";

/// 要自启的命令。`home` 仅在数据目录是自定义的时候有值;`codex_dir` 是守护进程实际会用的
/// codexPath 所在目录(macOS/Linux 拼进 PATH 最前面),Windows 上恒为 None。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutostartEntry {
    pub executable: String,
    pub entry: String,
    pub home: Option<String>,
    pub codex_dir: Option<String>,
}

impl AutostartEntry {
    /// `codex_path` 取守护进程实际会用的那个([`super::host::effective_codex_path`]),
    /// 同命令行 newAutostartEntry。
    pub fn new(
        current: &RuntimeCurrent,
        home: &RemoteHome,
        os: &str,
        codex_path: Option<&str>,
    ) -> Self {
        Self {
            executable: current.executable(os).to_string_lossy().into_owned(),
            entry: current.entry().to_string_lossy().into_owned(),
            home: home
                .custom
                .then(|| home.root.to_string_lossy().into_owned()),
            codex_dir: codex_path
                .filter(|path| os != "windows" && !path.is_empty())
                .map(unix_path_dir),
        }
    }

    fn args(&self) -> [&str; 4] {
        [&self.executable, &self.entry, "daemon", DAEMON_SYNC_VERB]
    }

    /// macOS/Linux 自启项给守护进程的 PATH:`[codexPath 所在目录:]固定部分`,不去重。
    pub fn daemon_path(&self) -> String {
        match &self.codex_dir {
            Some(dir) if !dir.is_empty() => format!("{dir}:{DAEMON_BASE_PATH}"),
            _ => DAEMON_BASE_PATH.to_string(),
        }
    }
}

/// 与 Go 的 `path.Dir` 相同(按 `/` 取目录再 Clean;单测在 Windows 上也要按 Unix 路径算)。
fn unix_path_dir(path: &str) -> String {
    let dir = match path.rfind('/') {
        Some(index) => &path[..=index],
        None => "",
    };
    unix_path_clean(dir)
}

/// Go 的 `path.Clean`。
fn unix_path_clean(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let rooted = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else if !rooted {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    match (rooted, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".into(),
        (false, false) => joined,
    }
}

/// Run 值的命令行(同命令行 buildWindowsRunValue)。默认数据目录:
///
/// `"<exe>" "<entry>" daemon start-sync`
///
/// 自定义数据目录时 Run 项本身没法设环境变量。不经 cmd.exe(开机闪黑窗、路径里的 `%` 会被
/// 当变量展开),改用运行时(便携 Node 22)自带的 `--env-file`,从数据目录里的 autostart.env 读:
///
/// `"<exe>" "--env-file=<home>\autostart.env" "<entry>" daemon start-sync`
pub fn windows_run_value(entry: &AutostartEntry) -> String {
    let [exe, script, daemon, verb] = entry.args();
    match &entry.home {
        None => format!("\"{exe}\" \"{script}\" {daemon} {verb}"),
        Some(home) => format!(
            "\"{exe}\" \"--env-file={}\" \"{script}\" {daemon} {verb}",
            windows_autostart_env_path(home)
        ),
    }
}

/// `<home>\autostart.env`(同命令行 windowsAutostartEnvPath:先去掉结尾的斜杠)。
pub fn windows_autostart_env_path(home: &str) -> String {
    format!(
        "{}\\{WINDOWS_AUTOSTART_ENV_FILE}",
        home.trim_end_matches(['\\', '/'])
    )
}

/// autostart.env 的内容(Node `--env-file` 的 dotenv 格式,一行、LF 结尾),同命令行
/// buildWindowsAutostartEnv。Node 22 的规则:单引号里原样;双引号里 `\n` 会变成换行。
/// Windows 路径不会含 `"`,但可能含 `'`(O'Brien 这种用户名),所以:
///   - 没有 `'` → 单引号,反斜杠原样;
///   - 有 `'` → 双引号,反斜杠换成 `/`(Node 在 Windows 上照认)。
pub fn windows_autostart_env(home: &str) -> String {
    if !home.contains('\'') {
        format!("{REMOTE_HOME_ENV}='{home}'\n")
    } else {
        format!("{REMOTE_HOME_ENV}=\"{}\"\n", home.replace('\\', "/"))
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
    // launchd 拉起的进程 PATH 只有 /usr/bin:/bin:/usr/sbin:/sbin —— 守护进程要找 codex、
    // codex 要找 node,必须显式给(公式见 daemon_path)。
    b.push_str("\t<key>EnvironmentVariables</key>\n\t<dict>\n");
    b.push_str(&format!(
        "\t\t<key>PATH</key>\n\t\t<string>{}</string>\n",
        xml_text(&entry.daemon_path())
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

/// XDG autostart 条目(同命令行 buildLinuxDesktopEntry)。一律经 env 带上 PATH(与 macOS 同一
/// 公式),自定义数据目录时再带 RECODEX_REMOTE_HOME;PATH 在前(与 plist 一致)。
pub fn linux_desktop_entry(entry: &AutostartEntry) -> String {
    let mut parts = vec![
        "env".to_string(),
        desktop_exec_quote(&format!("PATH={}", entry.daemon_path())),
    ];
    if let Some(home) = &entry.home {
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

/// 写 `<home>\autostart.env`(内容已相同就不动),同命令行 windowsAutostart.Install 的第一步。
/// 平台无关,单测在任何系统上都能跑。
pub fn write_env_file(home: &str) -> anyhow::Result<()> {
    let path = PathBuf::from(windows_autostart_env_path(home));
    let content = windows_autostart_env(home);
    if std::fs::read_to_string(&path).is_ok_and(|existing| existing == content) {
        return Ok(());
    }
    std::fs::create_dir_all(home)?;
    super::layout::write_file_replace(&path, content.as_bytes())
        .map_err(|error| anyhow::anyhow!("写 {WINDOWS_AUTOSTART_ENV_FILE} 失败:{error}"))
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
        if let Some(home) = &entry.home {
            // 先落 env 文件再登记:Run 项指向一个不存在的文件,开机就起不来(同命令行)。
            write_env_file(home)?;
        }
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
    //! 期望全文逐字抄自命令行 cmd/recodex/app_autostart_test.go(recodex/cli-app 分支)的
    //! TestWindowsRunValueFullText / TestWindowsAutostartEnvFile / TestLaunchAgentPlistFullText /
    //! TestLinuxDesktopEntryFullText。任何一边改了内容,两边这几条测试都要一起改 ——
    //! 否则两边会轮流覆盖同一个自启项(macOS 上每覆盖一次还会 bootout 一次守护进程)。
    use super::*;

    const WIN_RUNTIME: &str = r"C:\Users\me\.recodex\remote\runtime\1.2.3";
    const MAC_EXE: &str = "/Users/me/.recodex/remote/runtime/1.2.3/recodex-remote";
    const MAC_ENTRY: &str = "/Users/me/.recodex/remote/runtime/1.2.3/app/entry.mjs";
    const LINUX_EXE: &str = "/home/me/.recodex/remote/runtime/1.2.3/recodex-remote";

    fn win_entry(home: Option<&str>) -> AutostartEntry {
        AutostartEntry {
            executable: format!(r"{WIN_RUNTIME}\recodex-remote.exe"),
            entry: format!(r"{WIN_RUNTIME}\app\entry.mjs"),
            home: home.map(str::to_string),
            codex_dir: None,
        }
    }

    /// CLI TestWindowsRunValueFullText。
    #[test]
    fn windows_run_value_matches_the_cli() {
        assert_eq!(
            windows_run_value(&win_entry(None)),
            r#""C:\Users\me\.recodex\remote\runtime\1.2.3\recodex-remote.exe" "C:\Users\me\.recodex\remote\runtime\1.2.3\app\entry.mjs" daemon start-sync"#
        );
        let custom = windows_run_value(&win_entry(Some(r"D:\rr 50%")));
        assert_eq!(
            custom,
            r#""C:\Users\me\.recodex\remote\runtime\1.2.3\recodex-remote.exe" "--env-file=D:\rr 50%\autostart.env" "C:\Users\me\.recodex\remote\runtime\1.2.3\app\entry.mjs" daemon start-sync"#
        );
        assert!(
            !custom.to_ascii_lowercase().contains("cmd.exe"),
            "自定义目录不能再经 cmd.exe(开机闪窗、% 被展开)"
        );
        // 结尾带斜杠的目录不出现双斜杠
        assert_eq!(windows_autostart_env_path(r"D:\rr\"), r"D:\rr\autostart.env");
        assert_eq!(WINDOWS_RUN_VALUE_NAME, "ReCodexRemote");
    }

    /// Windows 上 codexPath 不进自启项(没有 PATH 覆盖),同 CLI def.CodexDir == ""。
    #[test]
    fn windows_entries_never_carry_a_codex_dir() {
        let current = RuntimeCurrent {
            version: "1.2.3".into(),
            dir: WIN_RUNTIME.into(),
        };
        let home = RemoteHome {
            root: r"C:\Users\me\.recodex\remote".into(),
            custom: false,
        };
        let entry = AutostartEntry::new(&current, &home, "windows", Some(r"C:\npm\codex.cmd"));
        assert_eq!(entry.codex_dir, None);
        assert_eq!(entry.home, None);
    }

    /// CLI TestWindowsAutostartEnvFile。
    #[test]
    fn windows_autostart_env_matches_the_cli() {
        for (home, want) in [
            (r"D:\rr", "RECODEX_REMOTE_HOME='D:\\rr'\n"),
            (r"C:\new dir\t%P%#$x", "RECODEX_REMOTE_HOME='C:\\new dir\\t%P%#$x'\n"),
            (r"C:\Users\O'Brien\r", "RECODEX_REMOTE_HOME=\"C:/Users/O'Brien/r\"\n"),
        ] {
            assert_eq!(windows_autostart_env(home), want, "{home}");
        }
    }

    /// 落地:自定义目录先写 autostart.env(CLI TestWindowsAutostartWritesEnvFileForCustomHome 的文件部分)。
    #[cfg(windows)]
    #[test]
    fn env_file_is_written_before_registering() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("rr");
        let home = home.to_string_lossy().into_owned();
        write_env_file(&home).unwrap();
        let written = std::fs::read(Path::new(&home).join("autostart.env")).unwrap();
        assert_eq!(written, windows_autostart_env(&home).as_bytes());
        assert!(!written.contains(&b'\r'), "只能是 LF");
        // 幂等
        write_env_file(&home).unwrap();
    }

    fn mac_plist_expected(path_value: &str, home_block: &str) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
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
\t\t<key>PATH</key>\n\t\t<string>{path_value}</string>\n\
{home_block}\
\t</dict>\n\
\t<key>RunAtLoad</key>\n\t<true/>\n\
</dict>\n</plist>\n"
        )
    }

    /// CLI TestLaunchAgentPlistFullText:没解析到 codexPath(默认目录)、
    /// 以及解析到 codexPath + 自定义目录两种全文。
    #[test]
    fn launch_agent_plist_matches_the_cli() {
        let entry = AutostartEntry {
            executable: MAC_EXE.into(),
            entry: MAC_ENTRY.into(),
            home: None,
            codex_dir: None,
        };
        assert_eq!(
            launch_agent_plist(&entry),
            mac_plist_expected("/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin", "")
        );

        let current = RuntimeCurrent {
            version: "1.2.3".into(),
            dir: "/Users/me/.recodex/remote/runtime/1.2.3".into(),
        };
        let home = RemoteHome {
            root: "/tmp/a&b".into(),
            custom: true,
        };
        let mut with_codex = AutostartEntry::new(
            &current,
            &home,
            "darwin",
            Some("/Users/me/.nvm/versions/node/v22.1.0/bin/codex"),
        );
        assert_eq!(with_codex.codex_dir.as_deref(), Some("/Users/me/.nvm/versions/node/v22.1.0/bin"));
        assert_eq!(with_codex.home.as_deref(), Some("/tmp/a&b"));
        // 单测跑在 Windows 上时 PathBuf 会拼出反斜杠,程序参数用固定值(同 CLI 的做法)
        with_codex.executable = MAC_EXE.into();
        with_codex.entry = MAC_ENTRY.into();
        let text = launch_agent_plist(&with_codex);
        assert_eq!(
            text,
            mac_plist_expected(
                "/Users/me/.nvm/versions/node/v22.1.0/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin",
                "\t\t<key>RECODEX_REMOTE_HOME</key>\n\t\t<string>/tmp/a&amp;b</string>\n"
            )
        );
        let hit = ["hap", "py"].concat();
        for bad in ["ProcessType", "Background", "KeepAlive", hit.as_str()] {
            assert!(!text.contains(bad), "plist 不能含 {bad}");
        }
        assert_eq!(
            launch_agent_path(Path::new("/Users/me")),
            Path::new("/Users/me/Library/LaunchAgents/dev.recodex.remote.plist")
        );
    }

    /// CLI TestLinuxDesktopEntryFullText。
    #[test]
    fn linux_desktop_entry_matches_the_cli() {
        let default = AutostartEntry {
            executable: LINUX_EXE.into(),
            entry: "/home/me/.recodex/remote/runtime/1.2.3/app/entry.mjs".into(),
            home: None,
            codex_dir: None,
        };
        assert_eq!(
            linux_desktop_entry(&default),
            "[Desktop Entry]\nType=Application\nName=ReCodex Remote\nComment=ReCodex mobile remote control\n\
Exec=env \"PATH=/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin\" /home/me/.recodex/remote/runtime/1.2.3/recodex-remote /home/me/.recodex/remote/runtime/1.2.3/app/entry.mjs daemon start-sync\n\
Terminal=false\nNoDisplay=true\nX-GNOME-Autostart-enabled=true\n"
        );
        let custom = AutostartEntry {
            executable: LINUX_EXE.into(),
            entry: "/home/me/My Stuff/app/entry.mjs".into(),
            home: Some("/home/me/rr 1".into()),
            codex_dir: Some("/home/me/.nvm/versions/node/v22.1.0/bin".into()),
        };
        assert_eq!(
            linux_desktop_entry(&custom),
            "[Desktop Entry]\nType=Application\nName=ReCodex Remote\nComment=ReCodex mobile remote control\n\
Exec=env \"PATH=/home/me/.nvm/versions/node/v22.1.0/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin\" \"RECODEX_REMOTE_HOME=/home/me/rr 1\" /home/me/.recodex/remote/runtime/1.2.3/recodex-remote \"/home/me/My Stuff/app/entry.mjs\" daemon start-sync\n\
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
    fn xml_escaping_follows_go() {
        assert_eq!(xml_text(r#"a"b'c<d>e&f"#), "a&#34;b&#39;c&lt;d&gt;e&amp;f");
        assert_eq!(xml_text("t\tn\nr\r"), "t&#x9;n&#xA;r&#xD;");
    }

    #[test]
    fn codex_dir_follows_go_path_dir() {
        for (input, want) in [
            ("/usr/local/bin/codex", "/usr/local/bin"),
            ("/codex", "/"),
            ("codex", "."),
            ("/a//b/./c/../codex", "/a/b"),
            ("rel/dir/codex", "rel/dir"),
        ] {
            assert_eq!(unix_path_dir(input), want, "{input}");
        }
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
        assert_eq!(AutostartEntry::new(&current, &default, "linux", None).home, None);
        assert_eq!(
            AutostartEntry::new(&current, &custom, "linux", None)
                .home
                .as_deref(),
            Some("/x/rr")
        );
        let plain = AutostartEntry::new(&current, &default, "linux", None);
        assert_eq!(plain.daemon_path(), DAEMON_BASE_PATH);
        assert!(
            AutostartEntry::new(&current, &default, "windows", None)
                .executable
                .ends_with("recodex-remote.exe")
        );
    }
}
