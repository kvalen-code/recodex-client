//! 调运行时进程(docs/remote-app-plan.md §2.2):`recodex-remote(.exe) app/entry.mjs <cmd>`。
//!
//! - 一律显式传 `RECODEX_REMOTE_HOME`:不让运行时再去猜(它还认旧的兜底变量),
//!   保证客户端看到的目录与运行时用的目录永远是同一个;
//! - Windows 上一律 `CREATE_NO_WINDOW`:从 GUI 进程拉起的任何子进程都不能闪黑框;
//! - `daemon start` 会再拉起一个常驻的孙进程。它的标准输出/错误**不接管道** —— 否则孙进程
//!   继承了管道句柄,读端永远等不到 EOF(命令行那边是靠 WaitDelay 兜住的)。

use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;

use super::layout::{REMOTE_HOME_ENV, RemoteHome, RuntimeCurrent};

/// `pair --json` 的一行事件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairEvent {
    Waiting { public_key: String, qr: String },
    Authorized { machine_id: String },
    AlreadyPaired { machine_id: String },
    Error { message: String },
}

#[derive(Debug, Deserialize)]
struct RawPairEvent {
    event: String,
    #[serde(default, rename = "publicKey")]
    public_key: String,
    #[serde(default)]
    qr: String,
    #[serde(default, rename = "machineId")]
    machine_id: String,
    #[serde(default)]
    message: String,
}

/// 解析一行。不是 JSON、事件不认识、或必填字段缺失的行一律跳过 ——
/// 运行时的依赖偶尔会往 stdout 打杂讯,不能因为一行日志就判配对失败(同命令行 parsePairEventLine)。
pub fn parse_pair_event_line(line: &str) -> Option<PairEvent> {
    let line = line.trim();
    if !line.starts_with('{') {
        return None;
    }
    let raw: RawPairEvent = serde_json::from_str(line).ok()?;
    match raw.event.as_str() {
        "waiting" if !raw.public_key.is_empty() && !raw.qr.is_empty() => Some(PairEvent::Waiting {
            public_key: raw.public_key,
            qr: raw.qr,
        }),
        "authorized" => Some(PairEvent::Authorized {
            machine_id: raw.machine_id,
        }),
        "already-paired" => Some(PairEvent::AlreadyPaired {
            machine_id: raw.machine_id,
        }),
        "error" => Some(PairEvent::Error {
            message: raw.message,
        }),
        _ => None,
    }
}

/// `status --json`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct RuntimeStatus {
    #[serde(default)]
    pub paired: bool,
    #[serde(default, rename = "machineId")]
    pub machine_id: Option<String>,
    #[serde(default)]
    pub server: String,
    #[serde(default)]
    pub daemon: DaemonStatus,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct DaemonStatus {
    #[serde(default)]
    pub running: bool,
    #[serde(default)]
    pub pid: Option<i64>,
    #[serde(default)]
    pub version: Option<String>,
}

/// 先按整段 JSON 解,不行再取最后一行合法 JSON 对象(前面可能混着日志)。
pub fn parse_runtime_status(out: &str) -> Option<RuntimeStatus> {
    if let Ok(status) = serde_json::from_str::<RuntimeStatus>(out.trim()) {
        return Some(status);
    }
    out.lines()
        .rev()
        .map(str::trim)
        .filter(|line| line.starts_with('{'))
        .find_map(|line| serde_json::from_str::<RuntimeStatus>(line).ok())
}

/// 一份可以拉起的运行时。
#[derive(Debug, Clone)]
pub struct Runtime {
    pub current: RuntimeCurrent,
    pub home: RemoteHome,
    pub os: &'static str,
}

impl Runtime {
    pub fn command(&self, args: &[&str]) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(self.current.executable(self.os));
        cmd.arg(self.current.entry());
        cmd.args(args);
        cmd.current_dir(self.current.dir_path());
        cmd.env(REMOTE_HOME_ENV, &self.home.root);
        cmd.stdin(Stdio::null());
        cmd.kill_on_drop(true);
        #[cfg(windows)]
        {
            cmd.creation_flags(crate::windows_integration::CREATE_NO_WINDOW);
        }
        cmd
    }

    /// 跑一条短命令并收 stdout(status / unpair)。超时即杀。
    pub async fn run_capture(&self, args: &[&str], timeout: Duration) -> anyhow::Result<String> {
        let mut cmd = self.command(args);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let child = cmd
            .spawn()
            .map_err(|error| anyhow::anyhow!("无法启动远程组件:{error}"))?;
        let output = tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .map_err(|_| anyhow::anyhow!("远程组件没有在 {} 秒内响应", timeout.as_secs()))??;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        if !output.status.success() {
            anyhow::bail!(
                "{}",
                exit_error(
                    output.status.code(),
                    &String::from_utf8_lossy(&output.stderr)
                )
            );
        }
        Ok(stdout)
    }

    /// 跑一条会拉起常驻孙进程的命令(daemon start / stop):输出不接管道,只看退出码。
    pub async fn run_detached_io(&self, args: &[&str], timeout: Duration) -> anyhow::Result<()> {
        let mut cmd = self.command(args);
        cmd.stdout(Stdio::null()).stderr(Stdio::null());
        let mut child = cmd
            .spawn()
            .map_err(|error| anyhow::anyhow!("无法启动远程组件:{error}"))?;
        let status = match tokio::time::timeout(timeout, child.wait()).await {
            Ok(status) => status?,
            Err(_) => {
                let _ = child.kill().await;
                anyhow::bail!("远程组件没有在 {} 秒内响应", timeout.as_secs());
            }
        };
        if !status.success() {
            anyhow::bail!("{}", exit_error(status.code(), ""));
        }
        Ok(())
    }

    pub async fn status(&self) -> anyhow::Result<RuntimeStatus> {
        let out = self
            .run_capture(&["status", "--json"], Duration::from_secs(15))
            .await?;
        parse_runtime_status(&out).ok_or_else(|| anyhow::anyhow!("远程组件没有返回状态"))
    }
}

/// 把子进程放进一个「最后一个句柄关闭就杀掉里面所有进程」的作业对象。
///
/// 作业句柄本进程持有到退出为止;启动器无论怎么退(包括 process::exit、被任务管理器结束),
/// 系统关句柄时都会带走这个子进程。只给 `pair` 用 —— `daemon start` 拉起的守护进程
/// 必须活过启动器,绝不能放进来(子进程默认继承作业)。失败只记日志,不影响配对。
#[cfg(windows)]
pub fn contain_child(child: &tokio::process::Child) {
    use std::sync::OnceLock;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    // 句柄存成整数:HANDLE 本身不是 Send/Sync。整个进程只建一个,永不关闭。
    static JOB: OnceLock<Option<usize>> = OnceLock::new();
    let job = JOB.get_or_init(|| unsafe {
        let job = CreateJobObjectW(None, windows::core::PCWSTR::null()).ok()?;
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
        .ok()?;
        Some(job.0 as usize)
    });
    let (Some(job), Some(process)) = (job, child.raw_handle()) else {
        let _ = crate::diagnostic_log::append_diagnostic_log(
            "phone_remote.job_object_failed",
            serde_json::json!({ "error": "job object unavailable" }),
        );
        return;
    };
    let assigned =
        unsafe { AssignProcessToJobObject(HANDLE(*job as *mut std::ffi::c_void), HANDLE(process)) };
    if let Err(error) = assigned {
        let _ = crate::diagnostic_log::append_diagnostic_log(
            "phone_remote.job_object_failed",
            serde_json::json!({ "error": error.to_string() }),
        );
    }
}

/// 类 Unix:子进程由 kill_on_drop 与正常退出路径负责。启动器被强杀时 pair 进程会成为孤儿,
/// 它自己 10 分钟超时后退出。
#[cfg(not(windows))]
pub fn contain_child(_child: &tokio::process::Child) {}

/// 退出码 + stderr 最后一行,便于排障;只取尾部,免得刷屏。
fn exit_error(code: Option<i32>, stderr: &str) -> String {
    let last = stderr.trim().lines().last().unwrap_or("").trim();
    let code = code.map(|c| c.to_string()).unwrap_or_else(|| "?".into());
    if last.is_empty() {
        format!("远程组件退出码 {code}")
    } else {
        format!("远程组件退出码 {code}:{}", truncate(last, 300))
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_events_parse_and_noise_is_skipped() {
        assert_eq!(
            parse_pair_event_line(
                r#"{"event":"waiting","publicKey":"AAAA","qr":"recodex://terminal?abc"}"#
            ),
            Some(PairEvent::Waiting {
                public_key: "AAAA".into(),
                qr: "recodex://terminal?abc".into()
            })
        );
        assert_eq!(
            parse_pair_event_line(r#"  {"event":"authorized","machineId":"m-1"}  "#),
            Some(PairEvent::Authorized {
                machine_id: "m-1".into()
            })
        );
        assert_eq!(
            parse_pair_event_line(r#"{"event":"already-paired","machineId":"m-2"}"#),
            Some(PairEvent::AlreadyPaired {
                machine_id: "m-2".into()
            })
        );
        assert_eq!(
            parse_pair_event_line(r#"{"event":"error","message":"无法连接中继"}"#),
            Some(PairEvent::Error {
                message: "无法连接中继".into()
            })
        );
        for noise in [
            "",
            "Debugger attached.",
            "{not json",
            r#"{"event":"waiting","publicKey":"AAAA"}"#,
            r#"{"event":"waiting","qr":"x"}"#,
            r#"{"event":"progress"}"#,
            r#"["event"]"#,
        ] {
            assert_eq!(parse_pair_event_line(noise), None, "{noise}");
        }
    }

    #[test]
    fn status_parses_whole_or_last_json_line() {
        let line = r#"{"paired":true,"machineId":"m","server":"https://relay.recodex.owtale.com","daemon":{"running":true,"pid":42,"version":"0.1.0"}}"#;
        let st = parse_runtime_status(line).unwrap();
        assert!(st.paired && st.daemon.running);
        assert_eq!(st.daemon.pid, Some(42));
        let noisy = format!("some log\n{line}\n");
        assert_eq!(parse_runtime_status(&noisy), Some(st));
        let nulls = r#"{"paired":false,"machineId":null,"server":"x","daemon":{"running":false,"pid":null,"version":null}}"#;
        let st = parse_runtime_status(nulls).unwrap();
        assert!(!st.paired && !st.daemon.running);
        assert_eq!(parse_runtime_status("nothing here"), None);
    }

    #[test]
    fn exit_errors_keep_only_the_last_stderr_line() {
        assert_eq!(
            exit_error(Some(1), "a\nb\n  boom  \n"),
            "远程组件退出码 1:boom"
        );
        assert_eq!(exit_error(None, ""), "远程组件退出码 ?");
    }
}
