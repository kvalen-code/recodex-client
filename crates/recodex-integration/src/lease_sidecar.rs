//! recodex-overlay: 租约直连的本机代理 sidecar(任务板 Q9)。
//!
//! 本机代理是 Go 写的(与 CLI 同一份代码),安装包里另带一份、改名 `recodex-lease`
//! —— 桌面端自己就叫 recodex.exe,同名会互相覆盖。桌面端只经两个子命令和它打交道:
//!
//! ```text
//! recodex-lease lease desktop-follow --api <控制面>   令牌从 stdin 第一行交过去
//! recodex-lease lease desktop-off
//! ```
//!
//! 两个命令都只往 stdout 打一行 JSON `{"outcome": "..."}`,这里把结果码原样交给调用方
//! 写诊断日志。结果码的含义见 Go 侧 cmd/recodex/lease_desktop.go。
//!
//! ## 为什么令牌走 stdin
//!
//! 桌面端的会话存在系统凭据库里,sidecar 读不到;放命令行参数会被本机其它用户在进程列表
//! 里看到,放环境变量会被 sidecar 拉起的常驻代理继承。stdin 只读这一次,之后令牌只在
//! 常驻代理的内存里。
//!
//! ## 包里没带 sidecar
//!
//! 老安装包、开发构建都没有它。这时一切照旧(走网关),`sidecar_path` 返回 None,
//! 调用方什么都不做、也不记日志 —— 不能让没带 sidecar 的包每次启动都报一条「失败」。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// 启动期跟随的超时。Go 侧最坏约 5s(试签)+ 5s(等代理应答),再留一点进程启动的余量。
/// 这一步排在拉起 Codex 之前,所以不能再放宽。
pub const FOLLOW_TIMEOUT: Duration = Duration::from_secs(15);
/// 登出/卸载时还原的超时。
pub const OFF_TIMEOUT: Duration = Duration::from_secs(10);

/// 结果码最长多少字符。结果码会进诊断日志并被上报,只允许短的机器可读串。
const MAX_OUTCOME_LEN: usize = 64;

/// sidecar 在安装目录里的文件名。
pub fn sidecar_file_name() -> &'static str {
    if cfg!(windows) {
        "recodex-lease.exe"
    } else {
        "recodex-lease"
    }
}

/// 与当前程序同目录的 sidecar。Windows 装在 %LOCALAPPDATA%\Programs\ReCodex\,
/// macOS 在 .app/Contents/MacOS/ —— 都与桌面端本体同目录。
pub fn sidecar_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    sidecar_path_beside(exe.parent()?)
}

pub fn sidecar_path_beside(dir: &Path) -> Option<PathBuf> {
    let path = dir.join(sidecar_file_name());
    path.is_file().then_some(path)
}

/// 跟随服务端的租约直连设置:开着就把令牌交给代理,没开且签得出就切过去。
pub fn follow(sidecar: &Path, api_base: &str, token: &str, timeout: Duration) -> String {
    let api_base = api_base.trim().trim_end_matches('/');
    run(
        sidecar,
        &["lease", "desktop-follow", "--api", api_base],
        Some(token),
        timeout,
    )
}

/// 彻底还原(停代理、撤自启、还原托管块与设备 ID)。不记成「用户不要直连」。
pub fn off(sidecar: &Path, timeout: Duration) -> String {
    run(sidecar, &["lease", "desktop-off"], None, timeout)
}

fn run(sidecar: &Path, args: &[&str], token: Option<&str>, timeout: Duration) -> String {
    run_in(sidecar, args, token, timeout, &[])
}

/// `envs` 只给测试用:把 sidecar 关进临时目录,绝不碰开发机真的 ~/.codex。
fn run_in(
    sidecar: &Path,
    args: &[&str],
    token: Option<&str>,
    timeout: Duration,
    envs: &[(&str, &str)],
) -> String {
    let mut command = Command::new(sidecar);
    command
        .args(args)
        .envs(envs.iter().copied())
        .stdin(if token.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        // stderr 里可能有 Go 侧的提示文字,诊断日志只要结果码,丢掉。
        .stderr(Stdio::null());
    hide_console_window(&mut command);
    let Ok(mut child) = command.spawn() else {
        return "spawn_failed".to_owned();
    };
    if let (Some(token), Some(mut stdin)) = (token, child.stdin.take()) {
        let _ = stdin.write_all(token.as_bytes());
        let _ = stdin.write_all(b"\n");
        // stdin 在这里被 drop、随即关闭:Go 侧读到行尾或 EOF 就往下走。
    }
    // stdout 放到线程里读:sidecar 会拉起一个常驻代理,万一它继承了这根管道,
    // 主线程直接 read_to_string 会一直等到代理退出。Go 的 os/exec 不会让子进程
    // 继承这根管道,这里是纵深防御 —— 启动路径上不能赌。
    let stdout = child.stdout.take();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        if let Some(stdout) = stdout {
            let _ = stdout.take(4096).read_to_string(&mut text);
        }
        let _ = sender.send(text);
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return "timeout".to_owned();
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => return "wait_failed".to_owned(),
        }
    }
    let text = receiver
        .recv_timeout(Duration::from_secs(2))
        .unwrap_or_default();
    parse_outcome(&text)
}

/// 取最后一行 JSON 的 outcome。只放行短的 `[a-z0-9_:-]` 串:这个值会进诊断日志并被上报,
/// 万一 sidecar 输出了意料之外的东西(比如被换成了别的程序),不能把它原样带出去。
pub fn parse_outcome(text: &str) -> String {
    let outcome = text.lines().rev().find_map(|line| {
        let value: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
        value.get("outcome")?.as_str().map(str::to_owned)
    });
    match outcome {
        Some(outcome)
            if !outcome.is_empty()
                && outcome.len() <= MAX_OUTCOME_LEN
                && outcome
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | ':' | '-')) =>
        {
            outcome
        }
        Some(_) => "unexpected_outcome".to_owned(),
        None => "unparsable".to_owned(),
    }
}

/// 这个结果码算不算故障(故障才带 error 字段、被自动上报)。
///
/// 「服务端不签」(not_eligible:*)不是故障:绝大多数账号每次启动都是这个结果,
/// 上报它只会淹没真问题。签成功的情况服务端有租约表,也不必客户端报。
/// 连不上控制面(not_eligible:transport)同样不报 —— 断网时别的事件已经在报了。
pub fn is_failure(outcome: &str) -> bool {
    !(outcome == "enabled"
        || outcome == "active"
        || outcome == "opted_out"
        || outcome == "orphaned"
        || outcome == "signed_out"
        || outcome.starts_with("not_eligible:"))
}

#[cfg(windows)]
fn hide_console_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    // sidecar 是控制台程序:不加这个,每次启动桌面端都会闪一下黑窗口。
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_console_window(_command: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_outcome_takes_the_json_line() {
        assert_eq!(parse_outcome("{\"outcome\":\"enabled\"}\n"), "enabled");
        assert_eq!(
            parse_outcome("noise\n{\"outcome\":\"not_eligible:lease_direct_disabled\"}\n"),
            "not_eligible:lease_direct_disabled"
        );
        assert_eq!(parse_outcome(""), "unparsable");
        assert_eq!(parse_outcome("not json"), "unparsable");
    }

    /// 结果码会被上报:意料之外的内容(大写、空白、超长、像令牌的串)一律不带出去。
    #[test]
    fn parse_outcome_refuses_to_forward_odd_values() {
        for odd in [
            "{\"outcome\":\"Bearer rct_secret\"}",
            "{\"outcome\":\"has space\"}",
            "{\"outcome\":\"\"}",
            &format!("{{\"outcome\":\"{}\"}}", "a".repeat(MAX_OUTCOME_LEN + 1)),
        ] {
            let got = parse_outcome(odd);
            assert!(
                got == "unexpected_outcome" || got == "unparsable",
                "{odd} 被原样放行了: {got}"
            );
        }
    }

    /// 真进程冒烟:用真的 Go sidecar 走一遍 stdin 交令牌 → stdout 读结果码。
    /// 默认不跑(要先构建 Go 程序):
    ///   go build -o <路径> ./cmd/recodex
    ///   RECODEX_LEASE_SIDECAR=<路径> cargo test -p recodex-integration -- --ignored sidecar_real
    #[test]
    #[ignore]
    fn sidecar_real_process_round_trip() {
        let Ok(sidecar) = std::env::var("RECODEX_LEASE_SIDECAR") else {
            panic!("设置 RECODEX_LEASE_SIDECAR 指向构建好的 Go 程序");
        };
        let sidecar = PathBuf::from(sidecar);
        let home = std::env::temp_dir().join(format!("rcx-sidecar-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        let home_s = home.to_string_lossy().into_owned();
        let codex_s = home.join(".codex").to_string_lossy().into_owned();
        let envs = [
            ("HOME", home_s.as_str()),
            ("USERPROFILE", home_s.as_str()),
            ("CODEX_HOME", codex_s.as_str()),
            // 端口 1 上没人听:试签必然连不上,走的是「签不出、什么都不动」那一支。
            ("RECODEX_API", "http://127.0.0.1:1"),
        ];
        let started = Instant::now();
        let outcome = run_in(
            &sidecar,
            &["lease", "desktop-follow", "--api", "http://127.0.0.1:1"],
            Some("rct_smoke_token_1234"),
            FOLLOW_TIMEOUT,
            &envs,
        );
        assert_eq!(outcome, "not_eligible:transport");
        assert!(started.elapsed() < Duration::from_secs(8), "签不出时不该拖慢启动: {:?}", started.elapsed());
        // 没有像样的令牌:不问服务端、直接报。
        let outcome = run_in(&sidecar, &["lease", "desktop-follow"], Some("has space"), FOLLOW_TIMEOUT, &envs);
        assert_eq!(outcome, "no_token");
        let outcome = run_in(&sidecar, &["lease", "desktop-off"], None, OFF_TIMEOUT, &envs);
        assert_eq!(outcome, "not_enabled");
        assert!(!home.join(".codex").join("recodex").join("lease.json").exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn only_real_failures_are_reported() {
        for quiet in ["enabled", "active", "opted_out", "orphaned", "signed_out", "not_eligible:lease_direct_disabled", "not_eligible:transport"] {
            assert!(!is_failure(quiet), "{quiet} 不该当故障上报");
        }
        for loud in ["rolled_back", "daemon_down", "handoff_failed", "enabled_handoff_failed", "timeout", "spawn_failed", "no_token", "unparsable", "state_corrupt"] {
            assert!(is_failure(loud), "{loud} 应当上报");
        }
    }

    #[test]
    fn sidecar_path_requires_a_file() {
        let dir = std::env::temp_dir().join(format!("rcx-sidecar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(sidecar_path_beside(&dir).is_none(), "没有 sidecar 时应为 None");
        std::fs::write(dir.join(sidecar_file_name()), b"").unwrap();
        assert_eq!(
            sidecar_path_beside(&dir).as_deref(),
            Some(dir.join(sidecar_file_name()).as_path())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
