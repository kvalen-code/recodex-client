//! recodex-overlay: 手机远程控制 —— 桌面客户端「手机远程」页的后端(docs/remote-app-plan.md §0/§2)。
//!
//! 与命令行 `recodex app` **共用**同一份运行时(`~/.recodex/remote/runtime/<版本>/`)、同一个数据目录、
//! 同一个开机自启项、同一个守护进程(运行时自带 daemon.state.json 单实例锁)。这里只做托管:
//!
//!   1. 保证运行时(install.rs:按账号取 channel=remote 清单 → 校验 → 解压 → current.json);
//!   2. 把官方 Codex 命令行的位置写进运行时 settings.json 的 `codexPath`(host.rs);
//!   3. 跑 `pair --json`:拿到一次性公钥 → 本机算确认码、登记到后台(跟随账号,手机弹窗)
//!      → 同时把二维码给面板(扫码兜底)→ 轮询后台看手机是拒绝还是过期;
//!   4. 配好后 `daemon start`(独立常驻,Codex/客户端退出后照跑)+ 登记开机自启。
//!
//! 面板经 CDP 桥 `/remote/*` 调用(routes.rs)。所有状态放在进程内的一个控制器里;
//! 一次只跑一个流程,新流程/取消都会让旧流程作废(generation)。

pub mod autostart;
pub mod code;
pub mod host;
pub mod install;
pub mod layout;
pub mod manifest;
pub mod runtime;

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::AsyncBufReadExt;

use self::layout::{RemoteHome, RuntimeCurrent};
use self::runtime::{PairEvent, Runtime};
use crate::settings::SettingsStore;

const OS: &str = std::env::consts::OS;
const ARCH: &str = std::env::consts::ARCH;
/// 运行时自己 10 分钟超时;多给 30 秒让它先把自己的结论说出来。
const PAIR_TIMEOUT: Duration = Duration::from_secs(10 * 60 + 30);
const PAIR_POLL_INTERVAL: Duration = Duration::from_secs(2);

// ── 状态机(纯逻辑,单测覆盖)────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitingInfo {
    /// 本机从公钥算出的 6 位确认码(纯数字)。
    pub code: String,
    pub qr_svg: String,
    pub machine_name: String,
    /// 后台登记成功 = 手机 App 会弹窗;失败时只能扫码,`phone_note` 说明原因。
    pub phone_prompt: bool,
    pub phone_note: Option<String>,
    /// 手机已点允许,正在等运行时拿到凭据。
    pub approved: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Preparing {
        detail: String,
    },
    Waiting(WaitingInfo),
    /// 已配对,正在拉起后台服务。
    Finishing,
    Connected,
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlowEvent {
    Preparing(String),
    Waiting(WaitingInfo),
    PhoneApproved,
    Paired,
    RuntimeError(String),
    Rejected,
    Expired,
    TimedOut,
    DaemonStarted,
    DaemonFailed(String),
    Cancelled,
}

pub const MSG_REJECTED: &str =
    "已在手机上拒绝这次连接。如果不是你本人操作请忽略;要重新连接,点「连接手机」。";
pub const MSG_EXPIRED: &str = "确认已过期(10 分钟内没有完成),请重新连接。";
pub const MSG_TIMED_OUT: &str = "10 分钟内没有在手机上完成确认,已取消。请重新连接。";

pub fn reduce(phase: &Phase, event: FlowEvent) -> Phase {
    match event {
        FlowEvent::Preparing(detail) => Phase::Preparing { detail },
        FlowEvent::Waiting(info) => Phase::Waiting(info),
        FlowEvent::PhoneApproved => match phase {
            Phase::Waiting(info) => Phase::Waiting(WaitingInfo {
                approved: true,
                ..info.clone()
            }),
            other => other.clone(),
        },
        FlowEvent::Paired => Phase::Finishing,
        FlowEvent::RuntimeError(message) | FlowEvent::DaemonFailed(message) => {
            Phase::Error { message }
        }
        FlowEvent::Rejected => Phase::Error {
            message: MSG_REJECTED.into(),
        },
        FlowEvent::Expired => Phase::Error {
            message: MSG_EXPIRED.into(),
        },
        FlowEvent::TimedOut => Phase::Error {
            message: MSG_TIMED_OUT.into(),
        },
        FlowEvent::DaemonStarted => Phase::Connected,
        FlowEvent::Cancelled => Phase::Idle,
    }
}

impl Phase {
    fn key(&self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Preparing { .. } => "preparing",
            Phase::Waiting(_) => "waiting",
            Phase::Finishing => "finishing",
            Phase::Connected => "connected",
            Phase::Error { .. } => "error",
        }
    }

    fn busy(&self) -> bool {
        matches!(
            self,
            Phase::Preparing { .. } | Phase::Waiting(_) | Phase::Finishing
        )
    }
}

/// 后台接口失败时给用户的一句话(按状态码分流,同命令行 describePairAPIError)。
pub fn describe_api_error(error: &recodex_integration::AdapterError) -> String {
    use recodex_integration::AdapterError as E;
    match error {
        E::Unauthorized => "还没有登录 ReCodex(或登录已失效),请先在「账号」页登录".into(),
        E::RateLimited => "待确认的电脑太多,请先在手机上处理".into(),
        E::Forbidden => "当前账号不能使用手机远程".into(),
        E::Unavailable | E::ServiceUnavailable => "暂时连不上 ReCodex 服务,或服务端尚未开放".into(),
        other => other.to_string(),
    }
}

// ── 控制器 ────────────────────────────────────────────────────────────

struct Controller {
    phase: Phase,
    generation: u64,
    cancel: Option<tokio::sync::watch::Sender<bool>>,
}

fn controller() -> &'static Mutex<Controller> {
    static CONTROLLER: OnceLock<Mutex<Controller>> = OnceLock::new();
    CONTROLLER.get_or_init(|| {
        Mutex::new(Controller {
            phase: Phase::Idle,
            generation: 0,
            cancel: None,
        })
    })
}

fn current_phase() -> Phase {
    controller()
        .lock()
        .map(|c| c.phase.clone())
        .unwrap_or(Phase::Idle)
}

/// 应用一个事件;旧流程(generation 不符)发来的事件直接丢弃。
fn apply(generation: u64, event: FlowEvent) -> bool {
    let Ok(mut c) = controller().lock() else {
        return false;
    };
    if c.generation != generation {
        return false;
    }
    c.phase = reduce(&c.phase, event);
    true
}

fn is_current(generation: u64) -> bool {
    controller()
        .lock()
        .is_ok_and(|c| c.generation == generation)
}

/// 作废当前流程(杀掉它的配对进程),回到 Idle。返回新的 generation。
fn cancel_current() -> u64 {
    let Ok(mut c) = controller().lock() else {
        return 0;
    };
    c.generation += 1;
    if let Some(cancel) = c.cancel.take() {
        let _ = cancel.send(true);
    }
    c.phase = Phase::Idle;
    c.generation
}

/// 开一个新流程:作废旧的,登记取消通道。
fn begin_flow() -> (u64, tokio::sync::watch::Receiver<bool>) {
    let (tx, rx) = tokio::sync::watch::channel(false);
    let Ok(mut c) = controller().lock() else {
        return (0, rx);
    };
    c.generation += 1;
    if let Some(old) = c.cancel.replace(tx) {
        let _ = old.send(true);
    }
    c.phase = Phase::Preparing {
        detail: "正在检查远程组件…".into(),
    };
    (c.generation, rx)
}

fn log(event: &str, detail: Value) {
    let _ = crate::diagnostic_log::append_diagnostic_log(event, detail);
}

fn follow_account_enabled() -> bool {
    SettingsStore::default()
        .load()
        .map(|s| s.phone_remote_follow_account)
        .unwrap_or(false)
}

fn set_follow_account(enabled: bool) -> anyhow::Result<()> {
    let store = SettingsStore::default();
    let mut settings = store.load().unwrap_or_default();
    if settings.phone_remote_follow_account == enabled {
        return Ok(());
    }
    settings.phone_remote_follow_account = enabled;
    store.save(&settings)
}

fn home() -> anyhow::Result<RemoteHome> {
    RemoteHome::resolve().ok_or_else(|| anyhow::anyhow!("找不到用户目录"))
}

fn installed_runtime(home: &RemoteHome) -> Option<Runtime> {
    layout::read_runtime_current(home, OS).map(|current| Runtime {
        current,
        home: home.clone(),
        os: OS,
    })
}

// ── 流程 ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trigger {
    /// 面板上的开关/按钮。
    User,
    /// 启动器启动时按已保存的开关自动做。
    Startup,
}

fn spawn_flow(force: bool, trigger: Trigger) {
    let (generation, cancel) = begin_flow();
    tokio::spawn(async move {
        let result = run_flow(generation, force, trigger, cancel).await;
        if let Err(error) = result {
            let message = format!("{error:#}");
            log(
                "phone_remote.flow_failed",
                json!({ "error": message, "trigger": format!("{trigger:?}") }),
            );
            apply(generation, FlowEvent::RuntimeError(message));
        }
        // 只清理自己的取消通道
        if let Ok(mut c) = controller().lock() {
            if c.generation == generation {
                c.cancel = None;
            }
        }
    });
}

async fn cancelled(cancel: &mut tokio::sync::watch::Receiver<bool>) {
    loop {
        if *cancel.borrow() {
            return;
        }
        if cancel.changed().await.is_err() {
            // 发送端没了 = 被新流程顶替
            return;
        }
    }
}

async fn run_flow(
    generation: u64,
    force: bool,
    trigger: Trigger,
    mut cancel: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let home = home()?;
    if layout::read_runtime_current(&home, OS).is_none() {
        apply(
            generation,
            FlowEvent::Preparing("正在下载远程组件(首次需要一两分钟)…".into()),
        );
    }
    let channel = tokio::task::spawn_blocking(|| {
        recodex_integration::remote_pair::remote_update_channel()
            .map(|channel| install::ChannelInfo {
                available: channel.available,
                manifest_url: channel.manifest_url,
            })
            .map_err(|error| anyhow::anyhow!(describe_api_error(&error)))
    })
    .await?;
    // 下载进度写进状态,面板每 2 秒刷新时能看到「12.3 / 35.5 MB」;按 1% 节流。
    let last_percent = std::sync::atomic::AtomicU64::new(u64::MAX);
    let progress = |done: u64, total: u64| {
        let percent = if total == 0 {
            0
        } else {
            done.saturating_mul(100) / total
        };
        if last_percent.swap(percent, std::sync::atomic::Ordering::Relaxed) != percent {
            apply(
                generation,
                FlowEvent::Preparing(download_detail(done, total)),
            );
        }
    };
    let outcome = tokio::select! {
        outcome = install::ensure_runtime(&home, OS, ARCH, channel, &progress) => outcome?,
        _ = cancelled(&mut cancel) => return Ok(()),
    };
    if let Some(warning) = &outcome.warning {
        log(
            "phone_remote.runtime_update_skipped",
            json!({ "warning": warning }),
        );
    }
    if outcome.updated {
        log(
            "phone_remote.runtime_installed",
            json!({
                "version": outcome.current.version,
                "previous": outcome.previous.as_ref().map(|p| p.version.clone()),
                "signature_verified": manifest::release_public_key().is_some(),
            }),
        );
        if let Some(previous) = &outcome.previous {
            // 旧守护进程还指着旧目录跑,先停掉;新的稍后拉起。失败不致命。
            let old = Runtime {
                current: previous.clone(),
                home: home.clone(),
                os: OS,
            };
            let _ = old
                .run_detached_io(&["daemon", "stop"], Duration::from_secs(30))
                .await;
        }
    }
    let rt = Runtime {
        current: outcome.current.clone(),
        home: home.clone(),
        os: OS,
    };
    sync_codex_path(&home).await;

    let paired = if force {
        false
    } else {
        rt.status()
            .await
            .map(|status| status.paired)
            .unwrap_or(false)
    };
    if !paired {
        if trigger == Trigger::Startup && !signed_in().await {
            // 启动时自动接入只走跟随账号那条路:没登录就别起一个没人看得见的扫码会话。
            apply(generation, FlowEvent::Cancelled);
            log(
                "phone_remote.startup_skipped",
                json!({ "reason": "signed_out" }),
            );
            return Ok(());
        }
        apply(generation, FlowEvent::Preparing("正在发起配对…".into()));
        let paired_now = pair(generation, &rt, force, &mut cancel).await?;
        if !paired_now {
            return Ok(());
        }
    }
    if !is_current(generation) {
        // 等配对期间用户关了开关 / 点了取消:别再把守护进程拉起来
        return Ok(());
    }
    apply(generation, FlowEvent::Paired);
    match ensure_daemon_and_autostart(generation, &rt).await {
        Ok(()) => {
            apply(generation, FlowEvent::DaemonStarted);
        }
        Err(error) => {
            apply(
                generation,
                FlowEvent::DaemonFailed(format!(
                    "启动后台服务失败:{error:#}。日志在 {}",
                    home.logs_dir().display()
                )),
            );
        }
    }
    Ok(())
}

fn download_detail(done: u64, total: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    format!(
        "正在下载远程组件… {:.1} / {:.1} MB",
        done as f64 / MB,
        total as f64 / MB
    )
}

async fn signed_in() -> bool {
    tokio::task::spawn_blocking(|| {
        recodex_integration::desktop::ReCodexState::from_env()
            .authenticated_adapter()
            .is_ok()
    })
    .await
    .unwrap_or(false)
}

async fn sync_codex_path(home: &RemoteHome) {
    let home = home.clone();
    let result = tokio::task::spawn_blocking(move || {
        let found = host::runnable_codex_cli();
        host::sync_codex_path(&home, found.as_deref()).map(|changed| (changed, found))
    })
    .await;
    match result {
        Ok(Ok((true, found))) => log(
            "phone_remote.codex_path_synced",
            json!({ "found": found.is_some() }),
        ),
        Ok(Ok((false, _))) => {}
        Ok(Err(error)) => log(
            "phone_remote.codex_path_failed",
            json!({ "error": format!("{error:#}") }),
        ),
        Err(error) => log(
            "phone_remote.codex_path_failed",
            json!({ "error": error.to_string() }),
        ),
    }
}

/// 跑一次 `pair --json`,直到运行时给出结论、手机拒绝、过期、超时或被取消。
/// 返回 true = 已配对(authorized / already-paired)。
async fn pair(
    generation: u64,
    rt: &Runtime,
    force: bool,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<bool> {
    let mut args = vec!["pair", "--json"];
    if force {
        args.push("--force");
    }
    let mut cmd = rt.command(&args);
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = cmd
        .spawn()
        .map_err(|error| anyhow::anyhow!("无法启动远程组件:{error}"))?;
    // 启动器被直接结束(process::exit / 任务管理器)时 kill_on_drop 不会执行:
    // Windows 上把配对进程放进「句柄关闭即杀」的作业对象兜底,不留孤儿进程。
    runtime::contain_child(&child);
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("远程组件没有输出"))?;
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    let machine_name = host::machine_name();
    let deadline = tokio::time::sleep(PAIR_TIMEOUT);
    tokio::pin!(deadline);
    let mut poll = tokio::time::interval(PAIR_POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut pair_id: Option<String> = None;
    // 后台那条请求已经有结论(手机拒绝 / 过期),不用再撤回。
    let mut settled_on_server = false;

    // Ok(Some(true)) = 已配对;Ok(None) = 没配成(原因已写进状态);Err = 出错。
    let outcome: anyhow::Result<Option<bool>> = loop {
        tokio::select! {
            _ = cancelled(cancel) => break Ok(None),
            _ = &mut deadline => {
                apply(generation, FlowEvent::TimedOut);
                break Ok(None);
            }
            line = lines.next_line() => {
                let line = match line {
                    Ok(Some(line)) => line,
                    Ok(None) | Err(_) => {
                        let status = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
                        let code = status.ok().and_then(Result::ok).and_then(|s| s.code());
                        break Err(anyhow::anyhow!(
                            "远程组件没有给出配对结果就退出了(退出码 {}),日志在 {}",
                            code.map(|c| c.to_string()).unwrap_or_else(|| "?".into()),
                            rt.home.logs_dir().display()
                        ));
                    }
                };
                match runtime::parse_pair_event_line(&line) {
                    Some(PairEvent::Waiting { public_key, qr }) => {
                        match on_waiting(&public_key, &qr, &machine_name).await {
                            Ok((info, id)) => {
                                // 运行时换了一把公钥重来:旧的那条请求撤掉
                                if let Some(old) = std::mem::replace(&mut pair_id, id) {
                                    cancel_pair_request(old);
                                }
                                apply(generation, FlowEvent::Waiting(info));
                            }
                            Err(error) => break Err(error),
                        }
                    }
                    Some(PairEvent::Authorized { .. }) | Some(PairEvent::AlreadyPaired { .. }) => break Ok(Some(true)),
                    Some(PairEvent::Error { message }) => {
                        let message = if message.trim().is_empty() { "未知错误".to_string() } else { message };
                        apply(generation, FlowEvent::RuntimeError(format!("配对失败:{message}")));
                        break Ok(None);
                    }
                    None => {}
                }
            }
            _ = poll.tick(), if pair_id.is_some() => {
                let id = pair_id.clone().unwrap_or_default();
                let status = tokio::task::spawn_blocking(move || {
                    recodex_integration::remote_pair::remote_pair_status(&id)
                })
                .await;
                // 网络抖动不算失败:运行时那边还在等,扫码也还能完成
                match status {
                    Ok(Ok(s)) if s == recodex_integration::remote_pair::REMOTE_PAIR_REJECTED => {
                        settled_on_server = true;
                        apply(generation, FlowEvent::Rejected);
                        // 用户在手机上明确拒绝了:别在下次启动时再弹
                        let _ = set_follow_account(false);
                        break Ok(None);
                    }
                    Ok(Ok(s)) if s == recodex_integration::remote_pair::REMOTE_PAIR_EXPIRED => {
                        settled_on_server = true;
                        apply(generation, FlowEvent::Expired);
                        break Ok(None);
                    }
                    Ok(Ok(s)) if s == recodex_integration::remote_pair::REMOTE_PAIR_APPROVED => {
                        apply(generation, FlowEvent::PhoneApproved);
                    }
                    _ => {}
                }
            }
        }
    };
    if matches!(outcome, Ok(Some(_))) {
        // 正常结束时运行时会自己退出;卡住的话 10 秒后再杀。
        if tokio::time::timeout(Duration::from_secs(10), child.wait())
            .await
            .is_err()
        {
            let _ = child.kill().await;
        }
    } else {
        let _ = child.kill().await;
    }
    // 取消、出错、超时、扫码已完成:都把手机上那条待确认撤掉,别让它挂满 10 分钟。
    if let Some(id) = pair_id.filter(|_| !settled_on_server) {
        cancel_pair_request(id);
    }
    outcome.map(|paired| paired.unwrap_or(false))
}

/// 撤回后台那条配对请求(发出去就不管,失败只记日志:过期后后台自己会清)。
fn cancel_pair_request(id: String) {
    tokio::task::spawn_blocking(move || {
        if let Err(error) = recodex_integration::remote_pair::remote_pair_cancel(&id) {
            log(
                "phone_remote.pair_cancel_failed",
                json!({ "error": error.to_string() }),
            );
        }
    });
}

/// 处理运行时给出的一次性公钥:本机算确认码 → 登记到后台 → 与后台的码核对 → 画二维码。
async fn on_waiting(
    public_key: &str,
    qr: &str,
    machine_name: &str,
) -> anyhow::Result<(WaitingInfo, Option<String>)> {
    let key = code::decode_public_key(public_key)
        .ok_or_else(|| anyhow::anyhow!("远程组件给出的公钥格式不对"))?;
    let local_code = code::confirm_code(&key);
    let canonical = code::encode_public_key(&key);
    let name = machine_name.to_string();
    let registered = tokio::task::spawn_blocking(move || {
        recodex_integration::remote_pair::remote_pair_create(
            &canonical,
            &name,
            host::platform_name(),
        )
    })
    .await?;
    let (phone_prompt, phone_note, id) = match registered {
        Ok(created) => {
            if created.code != local_code {
                log(
                    "phone_remote.confirm_code_mismatch",
                    json!({ "error": "server code differs from the locally computed one" }),
                );
                anyhow::bail!(
                    "安全检查未通过:服务端返回的确认码与本机从公钥算出的不一致,已中止配对。\
                     请不要在手机上允许任何待确认的电脑,并联系 ReCodex 客服。"
                );
            }
            (true, None, Some(created.id))
        }
        // 跟随账号这条路走不通(没登录、服务端未开放、待确认太多……)不致命:扫码照样能连。
        Err(error) => (false, Some(describe_api_error(&error)), None),
    };
    let qr_svg = crate::connect::weixin::render_qr_svg(qr).unwrap_or_default();
    Ok((
        WaitingInfo {
            code: local_code,
            qr_svg,
            machine_name: machine_name.to_string(),
            phone_prompt,
            phone_note,
            approved: false,
        },
        id,
    ))
}

/// 守护进程没跑就拉起来;开机自启项指到当前运行时(幂等)。
async fn ensure_daemon_and_autostart(generation: u64, rt: &Runtime) -> anyhow::Result<()> {
    let running = rt
        .status()
        .await
        .map(|status| status.daemon.running)
        .unwrap_or(false);
    if !running {
        rt.run_detached_io(&["daemon", "start"], Duration::from_secs(60))
            .await?;
    }
    if !is_current(generation) {
        return Ok(());
    }
    // PATH 里拼的是守护进程实际会用的 codexPath(settings.json 里保留下来的那个,
    // 不一定是这次找到的),同命令行 startDaemonAndAutostart。
    let home = rt.home.clone();
    let current = rt.current.clone();
    let installed = tokio::task::spawn_blocking(move || {
        let codex_path = host::effective_codex_path(&home);
        let entry = autostart::AutostartEntry::new(&current, &home, OS, codex_path.as_deref());
        autostart::install(&entry)
    })
    .await?;
    if let Err(error) = installed {
        // 不致命:守护进程已经在跑,只是重启后不会自动起来。
        log(
            "phone_remote.autostart_failed",
            json!({ "error": format!("{error:#}") }),
        );
    }
    Ok(())
}

// ── 桥命令 ────────────────────────────────────────────────────────────

async fn status_value() -> Value {
    let phase = current_phase();
    let enabled = follow_account_enabled();
    let mut value = json!({
        "status": "ok",
        "enabled": enabled,
        "phase": phase.key(),
        "busy": phase.busy(),
        "machineName": host::machine_name(),
    });
    match &phase {
        Phase::Preparing { detail } => value["detail"] = json!(detail),
        Phase::Waiting(info) => {
            value["code"] = json!(info.code);
            value["codeDisplay"] = json!(code::format_confirm_code(&info.code));
            value["qrSvg"] = json!(info.qr_svg);
            value["machineName"] = json!(info.machine_name);
            value["phonePrompt"] = json!(info.phone_prompt);
            value["phoneNote"] = json!(info.phone_note);
            value["approved"] = json!(info.approved);
        }
        Phase::Error { message } => value["message"] = json!(message),
        _ => {}
    }
    // 流程进行中不去碰运行时(每次都要起一个 node 进程);空闲时现查一次真实状态。
    if !phase.busy() {
        let home = home().ok();
        let rt = home.as_ref().and_then(installed_runtime);
        value["runtime"] = json!({
            "installed": rt.is_some(),
            "version": rt.as_ref().map(|rt| rt.current.version.clone()),
        });
        if let Some(rt) = rt {
            match rt.status().await {
                Ok(st) => {
                    value["paired"] = json!(st.paired);
                    value["daemonRunning"] = json!(st.daemon.running);
                }
                Err(error) => value["statusError"] = json!(format!("{error:#}")),
            }
        } else {
            value["paired"] = json!(false);
            value["daemonRunning"] = json!(false);
        }
        value["autostart"] = json!(
            tokio::task::spawn_blocking(autostart::registered)
                .await
                .unwrap_or(false)
        );
    }
    value
}

fn failed(message: impl Into<String>) -> Value {
    json!({ "status": "failed", "message": message.into() })
}

/// 开关打开 / 「连接手机」/「重新配对」:记住开关,后台开始流程,立刻返回。
async fn start(force: bool) -> Value {
    if let Err(error) = set_follow_account(true) {
        return failed(format!("保存设置失败:{error:#}"));
    }
    spawn_flow(force, Trigger::User);
    status_value().await
}

/// 开关关闭:停后台服务、撤开机自启,保留配对(再打开时不用重新配对)。
async fn disable() -> Value {
    cancel_current();
    if let Err(error) = set_follow_account(false) {
        return failed(format!("保存设置失败:{error:#}"));
    }
    let mut problems = Vec::new();
    if let Ok(home) = home() {
        if let Some(rt) = installed_runtime(&home) {
            if let Err(error) = rt
                .run_detached_io(&["daemon", "stop"], Duration::from_secs(60))
                .await
            {
                problems.push(format!("停止后台服务失败:{error:#}"));
            }
        }
    }
    if let Err(error) = tokio::task::spawn_blocking(autostart::remove)
        .await
        .unwrap_or_else(|e| Err(e.into()))
    {
        problems.push(format!("撤销开机自启失败:{error:#}"));
    }
    if !problems.is_empty() {
        log(
            "phone_remote.disable_failed",
            json!({ "error": problems.join("; ") }),
        );
        return failed(problems.join(";"));
    }
    status_value().await
}

/// 断开这台电脑:停服务 + 删本机凭据 + 撤开机自启 + 关开关。
async fn unpair() -> Value {
    cancel_current();
    let _ = set_follow_account(false);
    let mut problems = Vec::new();
    if let Ok(home) = home() {
        if let Some(rt) = installed_runtime(&home) {
            if let Err(error) = rt
                .run_capture(&["unpair", "--json"], Duration::from_secs(60))
                .await
            {
                problems.push(format!("断开失败:{error:#}"));
            }
        }
    }
    if let Err(error) = tokio::task::spawn_blocking(autostart::remove)
        .await
        .unwrap_or_else(|e| Err(e.into()))
    {
        problems.push(format!("撤销开机自启失败:{error:#}"));
    }
    if !problems.is_empty() {
        log(
            "phone_remote.unpair_failed",
            json!({ "error": problems.join("; ") }),
        );
        return failed(problems.join(";"));
    }
    status_value().await
}

/// CDP 桥分发:`/remote/*`。
pub async fn handle_bridge(path: &str, _payload: &Value) -> Value {
    match path {
        "/remote/status" => status_value().await,
        "/remote/enable" | "/remote/connect" => start(false).await,
        "/remote/repair" => start(true).await,
        "/remote/cancel" => {
            cancel_current();
            status_value().await
        }
        "/remote/disable" => disable().await,
        "/remote/unpair" => unpair().await,
        _ => failed(format!("unknown remote path: {path}")),
    }
}

/// 启动器启动时(单实例锁之后)按已保存的开关自动接入:没配对就发起跟随账号配对
/// (手机弹窗;面板打开就能看到确认码和二维码),已配对就只保证守护进程在跑。
pub fn start_from_saved_settings() {
    if follow_account_enabled() {
        spawn_flow(false, Trigger::Startup);
    }
}

/// 卸载时调用(同步,不依赖 tokio):停守护进程、撤开机自启。返回给用户看的说明。
///
/// 运行时与数据目录本身由卸载流程随 `~/.recodex` 一并删除(应用内卸载);
/// NSIS 卸载程序只经 `--remote-cleanup` 调到这里,不删数据。
pub fn uninstall_cleanup() -> Vec<String> {
    let mut notes = Vec::new();
    if let Some(home) = RemoteHome::resolve() {
        if let Some(current) = layout::read_runtime_current(&home, OS) {
            match run_sync(
                &current,
                &home,
                &["daemon", "stop"],
                Duration::from_secs(20),
            ) {
                Ok(()) => notes.push("已停止手机远程后台服务".to_string()),
                Err(error) => notes.push(format!("停止手机远程后台服务失败:{error:#}")),
            }
        }
    }
    if autostart::registered() {
        match autostart::remove() {
            Ok(()) => notes.push("已撤销手机远程的开机自启".to_string()),
            Err(error) => notes.push(format!("撤销手机远程开机自启失败:{error:#}")),
        }
    }
    notes
}

fn run_sync(
    current: &RuntimeCurrent,
    home: &RemoteHome,
    args: &[&str],
    timeout: Duration,
) -> anyhow::Result<()> {
    let mut cmd = std::process::Command::new(current.executable(OS));
    cmd.arg(current.entry())
        .args(args)
        .current_dir(current.dir_path())
        .env(layout::REMOTE_HOME_ENV, &home.root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(crate::windows_integration::CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn()?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(());
            }
            anyhow::bail!("退出码 {:?}", status.code());
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            anyhow::bail!("超时");
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn waiting() -> WaitingInfo {
        WaitingInfo {
            code: "003172".into(),
            qr_svg: "<svg/>".into(),
            machine_name: "DESKTOP-ABC".into(),
            phone_prompt: true,
            phone_note: None,
            approved: false,
        }
    }

    #[test]
    fn happy_path_transitions() {
        let mut phase = Phase::Idle;
        for (event, expected) in [
            (FlowEvent::Preparing("x".into()), "preparing"),
            (FlowEvent::Waiting(waiting()), "waiting"),
            (FlowEvent::PhoneApproved, "waiting"),
            (FlowEvent::Paired, "finishing"),
            (FlowEvent::DaemonStarted, "connected"),
        ] {
            phase = reduce(&phase, event);
            assert_eq!(phase.key(), expected);
        }
    }

    #[test]
    fn approval_only_marks_a_waiting_session() {
        let approved = reduce(&Phase::Waiting(waiting()), FlowEvent::PhoneApproved);
        assert!(
            matches!(approved, Phase::Waiting(ref info) if info.approved && info.code == "003172")
        );
        assert_eq!(reduce(&Phase::Idle, FlowEvent::PhoneApproved), Phase::Idle);
        assert_eq!(
            reduce(&Phase::Connected, FlowEvent::PhoneApproved),
            Phase::Connected
        );
    }

    #[test]
    fn failures_carry_a_message_and_cancel_goes_idle() {
        let w = Phase::Waiting(waiting());
        assert_eq!(
            reduce(&w, FlowEvent::Rejected),
            Phase::Error {
                message: MSG_REJECTED.into()
            }
        );
        assert_eq!(
            reduce(&w, FlowEvent::Expired),
            Phase::Error {
                message: MSG_EXPIRED.into()
            }
        );
        assert_eq!(
            reduce(&w, FlowEvent::TimedOut),
            Phase::Error {
                message: MSG_TIMED_OUT.into()
            }
        );
        assert_eq!(
            reduce(&w, FlowEvent::RuntimeError("boom".into())),
            Phase::Error {
                message: "boom".into()
            }
        );
        assert_eq!(
            reduce(&Phase::Finishing, FlowEvent::DaemonFailed("x".into())),
            Phase::Error {
                message: "x".into()
            }
        );
        assert_eq!(reduce(&w, FlowEvent::Cancelled), Phase::Idle);
        // 新的一次公钥(运行时重来)替换旧的确认码
        let fresh = WaitingInfo {
            code: "848873".into(),
            ..waiting()
        };
        assert_eq!(
            reduce(&w, FlowEvent::Waiting(fresh.clone())),
            Phase::Waiting(fresh)
        );
    }

    #[test]
    fn busy_phases() {
        assert!(
            Phase::Preparing {
                detail: String::new()
            }
            .busy()
        );
        assert!(Phase::Waiting(waiting()).busy());
        assert!(Phase::Finishing.busy());
        assert!(!Phase::Idle.busy() && !Phase::Connected.busy());
        assert!(
            !Phase::Error {
                message: String::new()
            }
            .busy()
        );
    }

    #[test]
    fn stale_flows_cannot_overwrite_the_state() {
        let (old, _rx_old) = begin_flow();
        let (new, _rx_new) = begin_flow();
        assert!(
            !apply(old, FlowEvent::DaemonStarted),
            "被顶替的流程发来的事件要丢掉"
        );
        assert!(apply(new, FlowEvent::Waiting(waiting())));
        assert_eq!(current_phase().key(), "waiting");
        let after_cancel = cancel_current();
        assert!(after_cancel > new);
        assert!(!apply(new, FlowEvent::DaemonStarted));
        assert_eq!(current_phase(), Phase::Idle);
    }

    #[test]
    fn download_progress_reads_in_megabytes() {
        assert_eq!(
            download_detail(12_900_000, 37_224_448),
            "正在下载远程组件… 12.3 / 35.5 MB"
        );
    }

    #[test]
    fn api_errors_read_like_the_cli() {
        use recodex_integration::AdapterError as E;
        assert!(describe_api_error(&E::Unauthorized).contains("登录"));
        assert!(describe_api_error(&E::RateLimited).contains("太多"));
        assert!(describe_api_error(&E::Unavailable).contains("尚未开放"));
    }

    #[tokio::test]
    async fn unknown_paths_are_rejected() {
        let value = handle_bridge("/remote/nope", &json!({})).await;
        assert_eq!(value["status"], "failed");
    }

    /// 用户看得见的文字里不能出现上游品牌。
    #[test]
    fn no_upstream_brand_in_user_visible_text() {
        let sources = [
            include_str!("mod.rs"),
            include_str!("install.rs"),
            include_str!("runtime.rs"),
            include_str!("manifest.rs"),
            include_str!("host.rs"),
            include_str!("autostart.rs"),
        ];
        for source in sources {
            for line in source
                .lines()
                .filter(|l| l.contains('"') && !l.trim_start().starts_with("//"))
            {
                let lower = line.to_ascii_lowercase();
                let hit = ["hap", "py"].concat();
                let pp = ["codex", "++"].concat();
                assert!(
                    !lower.contains(&hit) && !lower.contains(&pp),
                    "用户可见文字里出现了上游品牌: {line}"
                );
            }
        }
    }
}
