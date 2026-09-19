//! recodex-overlay: 手机远程控制 —— 桌面客户端「手机远程」页的后端(docs/remote-app-plan.md §0/§2)。
//!
//! 与命令行 `recodex app` **共用**同一份运行时(`~/.recodex/remote/runtime/<版本>/`)、同一个数据目录、
//! 同一个开机自启项、同一个守护进程(运行时自带 daemon.state.json 单实例锁)。这里只做托管:
//!
//!   1. 保证运行时(install.rs:按账号取 channel=remote 清单 → 校验 → 解压 → current.json);
//!   2. 把官方 Codex 命令行的位置写进运行时 settings.json 的 `codexPath`(host.rs);
//!   3. 跑 `pair --json --bind-approver`:拿到一次性公钥 → 本机算确认码、登记到后台
//!      (跟随账号,手机弹窗)→ 同时把二维码给面板(扫码兜底)→ 轮询后台看手机是拒绝还是过期;
//!   4. 配好后 `daemon start`(独立常驻,Codex/客户端退出后照跑)+ 登记开机自启。
//!
//! 面板经 CDP 桥 `/remote/*` 调用(routes.rs)。所有状态放在进程内的一个控制器里;
//! 一次只跑一个流程,新流程/取消都会让旧流程作废(generation)。
//!
//! ── 批准方绑定(2026-09-20 审计:抢答劫持;完整协议见 docs/remote-app-plan.md §2.4.1)──
//!
//! 中继对「谁来批准」是先写者赢:拿到这台电脑一次性公钥(二维码、后台库、中继库里都有)的人,
//! 可以用**他自己的**中继账号抢先批准,这台电脑就连到他身上(等于远程执行)。
//! 所以运行时以 `--bind-approver` 启动,登记请求时声明 `approver_check`,这里把后台记下的批准方
//! (手机的内容公钥 + 中继账号 id)经 **stdin** 交给运行时,由它核对中继交来的身份:
//!
//!   - 后台登记成功 → 二维码内容加 `&bind=1`;等后台 approved 后递一行
//!     `{"account":…,"key":…,"type":"approver"}`;
//!   - approved 却没有批准方 → 中止(后台或手机 App 是旧版),绝不降级成不核对;
//!   - 后台**明确**说这条路不可用(400/401/403/404/429/501、未登录)→ 递 `{"type":"unbound"}`,只能扫码;
//!   - 网络错误 / 5xx:后台可能已经建好了记录,不能静默降级 —— 退避重试 3 次,仍失败就中止(审计 S2);
//!   - 运行时是老版本(waiting 里没有 `approverCheck`)→ 它不会核对,这里就**不登记**后台请求,
//!     只给二维码,并提示更新远程组件。

pub mod autostart;
pub mod code;
pub mod host;
pub mod install;
pub mod layout;
pub mod manifest;
pub mod runtime;

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use recodex_integration::remote_pair::{PairApiError, RemotePairCreated, RemotePairStatus};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

use self::layout::{RemoteHome, RuntimeCurrent};
use self::runtime::{PairEvent, Runtime};
use crate::settings::SettingsStore;

const OS: &str = std::env::consts::OS;
const ARCH: &str = std::env::consts::ARCH;
/// 运行时自己 10 分钟超时;多给 30 秒让它先把自己的结论说出来。
const PAIR_TIMEOUT: Duration = Duration::from_secs(10 * 60 + 30);
const PAIR_POLL_INTERVAL: Duration = Duration::from_secs(2);
/// 登记跟随账号请求的重试次数与退避(同命令行:间隔 1s、2s)。
/// 同一台设备重新登记会顶替旧的,所以重试是安全的。
const PAIR_REGISTER_ATTEMPTS: u32 = 3;
const PAIR_REGISTER_BACKOFF: Duration = Duration::from_secs(1);
/// 登记成功后二维码末尾加这个,手机扫到就知道这台电脑会核对批准方(§2.4.1)。
const PAIR_QR_BIND_SUFFIX: &str = "&bind=1";

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
    /// 后台把这条请求记成 `cancelled`:同一台电脑又发起了新的配对(`recodex app`,或别处再开了
    /// 手机远程)把它顶替了。这里的确认码已作废。
    Superseded,
    TimedOut,
    DaemonStarted,
    DaemonFailed(String),
    Cancelled,
}

pub const MSG_REJECTED: &str =
    "已在手机上拒绝这次连接。如果不是你本人操作请忽略;要重新连接,点「连接手机」。";
pub const MSG_EXPIRED: &str = "确认已过期(10 分钟内没有完成),请重新连接。";
pub const MSG_TIMED_OUT: &str = "10 分钟内没有在手机上完成确认,已取消。请重新连接。";
/// 与命令行 errPairSuperseded 同义(桌面端的说法)。
pub const MSG_SUPERSEDED: &str = "这次配对已在别处重新发起(这台电脑上运行了 recodex app,或在别处打开了手机远程),这里的确认码已作废。请以最新显示的确认码为准;要在这里继续,点「连接手机」。";

// ── 批准方核对失败的文案(§2.4.1 的四个机读码 + 两种「批准方缺失」)────────────

/// `approver_mismatch`:中继交来的身份不是后台记下的批准方 —— 被别人抢先确认了。
pub const MSG_APPROVER_MISMATCH: &str = "安全检查未通过:批准这次连接的不是你手机上的 ReCodex 账号(有人抢先确认了这台电脑),已拒绝,没有保存任何凭据。请点「重新配对」,并在手机弹窗里核对确认码后点允许。如果反复出现,请联系 ReCodex 客服。";
/// `approver_missing`:中继上有答复,但一直没有对应的账号批准记录。
pub const MSG_APPROVER_MISSING: &str = "安全检查未通过:中继上的这次确认没有对应的账号批准记录,已拒绝,没有保存任何凭据。可能是被他人抢先确认,也可能是扫码的手机 App 版本较旧、或登录的不是这个 ReCodex 账号。请把手机 ReCodex App 更新到最新版、登录与电脑相同的账号,然后重新连接并在手机弹窗里点允许。";
/// `approver_check_failed`:查不到中继令牌属于哪个账号(网络问题),为安全起见没落盘。
pub const MSG_APPROVER_CHECK_FAILED: &str =
    "无法向中继核实是哪部手机批准的这次连接,为安全起见没有保存任何凭据。请检查网络后重新连接。";
/// `relay_answer_consumed`:中继上的配对结果已被别的程序取走(只发一次)。
pub const MSG_RELAY_ANSWER_CONSUMED: &str = "安全检查未通过:中继上的配对结果已经被别的程序取走,本次配对作废,没有保存任何凭据。请重新连接;如果反复出现,请联系 ReCodex 客服。";
/// 后台是 approved 却没回批准方,且登记时后台连 `approver_binding` 都没回 → 只可能是后台旧。
pub const MSG_APPROVER_ABSENT_SERVER: &str = "手机已允许,但 ReCodex 服务端版本较旧,不支持安全核对,为安全起见已中止连接。请稍后再试,或先用手机 App 扫描二维码连接。";
/// 后台支持绑定却没回批准方 → 分不清是后台被回滚还是手机 App 旧,两个都提(审计 S4)。
pub const MSG_APPROVER_ABSENT_BOTH: &str = "手机已允许,但没有收到安全核对所需的批准方信息(ReCodex 服务端或手机 App 版本较旧),为安全起见已中止连接。请把手机 ReCodex App 更新到最新版后重新连接;如果手机已是最新版,请稍后再试或联系 ReCodex 客服。";

/// 运行时 `error` 事件里的机读码 → 面板上的中文说明。不认识的码返回 None(按普通失败处理)。
pub fn approver_error_message(code: &str, detail: &str) -> Option<String> {
    let detail = detail.trim();
    let message = match code {
        runtime::PAIR_ERR_APPROVER_MISMATCH => MSG_APPROVER_MISMATCH.to_string(),
        runtime::PAIR_ERR_APPROVER_MISSING => MSG_APPROVER_MISSING.to_string(),
        runtime::PAIR_ERR_APPROVER_CHECK_FAILED => {
            let mut message = MSG_APPROVER_CHECK_FAILED.to_string();
            if !detail.is_empty() {
                message.push_str(&format!("(远程组件:{detail})"));
            }
            message
        }
        runtime::PAIR_ERR_RELAY_ANSWER_CONSUMED => MSG_RELAY_ANSWER_CONSUMED.to_string(),
        _ => return None,
    };
    Some(message)
}

/// 后台是 approved 但没给批准方:分清「只有服务端旧」与「服务端或手机 App 旧」。
pub fn approver_absent_message(backend_binding: bool) -> &'static str {
    if backend_binding {
        MSG_APPROVER_ABSENT_BOTH
    } else {
        MSG_APPROVER_ABSENT_SERVER
    }
}

/// 登记这条请求失败、又不能降级(网络错误 / 5xx / 回包解不开):中止,不静默变成不核对。
pub fn register_failed_message(note: &str) -> String {
    format!("无法在 ReCodex 服务端登记这次连接({note}),为安全起见已中止。请检查网络后重新连接。")
}

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
        FlowEvent::Superseded => Phase::Error {
            message: MSG_SUPERSEDED.into(),
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

/// 配对接口失败时给用户的一句话(状态码优先,同命令行 describePairAPIError)。
pub fn describe_pair_error(error: &PairApiError) -> String {
    match error {
        PairApiError::Http(400) => "服务端暂不支持手机弹窗确认(版本较旧)".into(),
        PairApiError::Http(401) => "登录已失效,请到「账号」页重新登录".into(),
        PairApiError::Http(403) => "当前账号不能使用手机远程".into(),
        PairApiError::Http(404) | PairApiError::Http(501) => "服务端尚未开放".into(),
        PairApiError::Http(429) => "待确认的电脑太多,请先在手机上处理".into(),
        PairApiError::Http(status) => format!("服务端返回 HTTP {status}"),
        PairApiError::Adapter(error) => describe_api_error(error),
    }
}

/// 后台**明确**表示没建记录、这条路不可用 —— 只有这些才降级成只扫码(§2.4.1 第 3 条)。
/// 网络错误与 5xx 不在其中:那时后台可能已经建好了记录,降级就等于把核对悄悄关掉。
pub fn pair_unavailable(error: &PairApiError) -> bool {
    use recodex_integration::AdapterError as E;
    match error {
        PairApiError::Http(status) => matches!(status, 400 | 401 | 403 | 404 | 429 | 501),
        // 本机就没有登录态:请求根本没发出去。
        PairApiError::Adapter(E::Unauthorized) | PairApiError::Adapter(E::InvalidConfiguration(_)) => true,
        PairApiError::Adapter(_) => false,
    }
}

/// 跟随账号(手机弹窗)这条路为什么走不通。三种都降级成只扫码,只是提示语不同。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptUnavailable {
    /// 运行时不认 `--bind-approver`:它不会核对批准方,所以**不登记**后台请求。
    RuntimeTooOld,
    /// 后台没把这条登记成「会核对批准方」(已撤回)。
    BackendTooOld,
    /// 后台明确拒绝(401/403/404/429/…)或本机未登录。
    Api(PairApiError),
}

impl PromptUnavailable {
    pub fn note(&self) -> String {
        match self {
            Self::RuntimeTooOld => {
                "远程组件版本较旧、不支持安全核对,这次只能扫码;请稍后重新连接,让客户端把远程组件更新到最新版"
                    .into()
            }
            Self::BackendTooOld => "服务端暂不支持手机弹窗确认(版本较旧)".into(),
            Self::Api(error) => describe_pair_error(error),
        }
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
    let mut args = vec!["pair", "--json", "--bind-approver"];
    if force {
        args.push("--force");
    }
    let mut cmd = rt.command(&args);
    // stdin 接管道:批准方要等手机点了允许才知道,只能在运行时起来之后经 stdin 递进去(§2.4.1)。
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = cmd
        .spawn()
        .map_err(|error| anyhow::anyhow!("无法启动远程组件:{error}"))?;
    let mut directives = Directives {
        stdin: child.stdin.take(),
    };
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
    // 后台那条请求已经有结论(手机拒绝 / 过期 / 已批准),不用再撤回。
    let mut settled_on_server = false;
    // 登记时后台回了 approver_binding:它支持批准方绑定(用来分清是后台旧还是手机 App 旧)。
    let mut backend_binding = false;
    // 后台已 approved,批准方已经递给运行时(只递一次)。
    let mut approved_on_server = false;

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
                    Some(PairEvent::Waiting { public_key, qr, approver_check }) => {
                        match on_waiting(&public_key, &qr, &machine_name, approver_check).await {
                            Ok((info, registered)) => {
                                // 运行时换了一把公钥重来:旧的那条请求撤掉
                                let id = registered.as_ref().map(|r| r.id.clone());
                                if let Some(old) = std::mem::replace(&mut pair_id, id) {
                                    cancel_pair_request(old);
                                }
                                settled_on_server = false;
                                approved_on_server = false;
                                backend_binding = registered.as_ref().is_some_and(|r| r.approver_binding);
                                if registered.is_none() {
                                    // 明确没有后台记录:告诉运行时这次不做批准方绑定(只扫码)。
                                    directives.unbound().await;
                                }
                                apply(generation, FlowEvent::Waiting(info));
                            }
                            Err(error) => break Err(error),
                        }
                    }
                    Some(PairEvent::Authorized { .. }) | Some(PairEvent::AlreadyPaired { .. }) => break Ok(Some(true)),
                    Some(PairEvent::Error { message, code }) => {
                        if let Some(message) = approver_error_message(&code, &message) {
                            // 批准方核对没过:运行时没有保存任何凭据,照 §2.4.1 的表给说明,
                            // 不能说成「手机没把凭据交给中继」。
                            log("phone_remote.approver_check_failed", json!({ "code": code }));
                            apply(generation, FlowEvent::RuntimeError(message));
                            break Ok(None);
                        }
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
                    backend().status(&id)
                })
                .await;
                // 网络抖动不算失败:运行时那边还在等,扫码也还能完成
                let status = status.map(|inner| inner.map(|st| (st.status.clone(), st)));
                match status {
                    Ok(Ok((s, st))) if s == recodex_integration::remote_pair::REMOTE_PAIR_APPROVED => {
                        if !approved_on_server {
                            approved_on_server = true;
                            // approved 是终态,撤回没有意义。
                            settled_on_server = true;
                            match approver_directive_for(&st) {
                                Some((key, account)) => {
                                    directives.approver(&key, &account).await;
                                    apply(generation, FlowEvent::PhoneApproved);
                                }
                                None => {
                                    // 不知道批准方是谁 = 没法核对中继上的答复是不是它给的:中止。
                                    log(
                                        "phone_remote.approver_absent",
                                        json!({ "backend_binding": backend_binding }),
                                    );
                                    apply(
                                        generation,
                                        FlowEvent::RuntimeError(
                                            approver_absent_message(backend_binding).to_string(),
                                        ),
                                    );
                                    break Ok(None);
                                }
                            }
                        }
                    }
                    Ok(Ok((s, _))) if s == recodex_integration::remote_pair::REMOTE_PAIR_REJECTED => {
                        settled_on_server = true;
                        apply(generation, FlowEvent::Rejected);
                        // 用户在手机上明确拒绝了:别在下次启动时再弹
                        let _ = set_follow_account(false);
                        break Ok(None);
                    }
                    Ok(Ok((s, _))) if s == recodex_integration::remote_pair::REMOTE_PAIR_EXPIRED => {
                        settled_on_server = true;
                        apply(generation, FlowEvent::Expired);
                        break Ok(None);
                    }
                    Ok(Ok((s, _))) if s == recodex_integration::remote_pair::REMOTE_PAIR_CANCELLED => {
                        // 被顶替(或已撤回):后台已是终态,不用再撤;别处那次配对照常进行,
                        // 开关保持打开。
                        settled_on_server = true;
                        apply(generation, FlowEvent::Superseded);
                        break Ok(None);
                    }
                    _ => {}
                }
            }
        }
    };
    // 关掉 stdin(已经收到的指示仍然有效,§2.4.1):别让运行时因为管道还开着而一直等下去。
    directives.stdin = None;
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
        if let Err(error) = backend().cancel(&id) {
            log(
                "phone_remote.pair_cancel_failed",
                json!({ "error": error.to_string() }),
            );
        }
    });
}

// ── 后台那三条配对接口(可替换,给集成测试用)──────────────────────────

/// `POST remote/pair` / `GET remote/pair/{id}` / `POST remote/pair/{id}/cancel`。
/// 生产是 recodex-integration 的真实实现;集成测试换成假后台,好把「后台已批准却缺批准方」
/// 这类只在服务端侧出现的分支跑出来(都是阻塞调用,调用方放 spawn_blocking)。
pub trait PairBackend: Send + Sync {
    /// `approver_check` 原样进 body:声明「这台电脑会核对批准方」。
    fn create(
        &self,
        public_key: &str,
        machine_name: &str,
        approver_check: bool,
    ) -> Result<RemotePairCreated, PairApiError>;
    fn status(&self, id: &str) -> Result<RemotePairStatus, PairApiError>;
    fn cancel(&self, id: &str) -> Result<(), recodex_integration::AdapterError>;
}

struct LiveBackend;

impl PairBackend for LiveBackend {
    fn create(
        &self,
        public_key: &str,
        machine_name: &str,
        approver_check: bool,
    ) -> Result<RemotePairCreated, PairApiError> {
        recodex_integration::remote_pair::remote_pair_create(
            public_key,
            machine_name,
            host::platform_name(),
            approver_check,
        )
    }

    fn status(&self, id: &str) -> Result<RemotePairStatus, PairApiError> {
        recodex_integration::remote_pair::remote_pair_status(id)
    }

    fn cancel(&self, id: &str) -> Result<(), recodex_integration::AdapterError> {
        recodex_integration::remote_pair::remote_pair_cancel(id)
    }
}

fn backend_slot() -> &'static Mutex<Option<std::sync::Arc<dyn PairBackend>>> {
    static SLOT: OnceLock<Mutex<Option<std::sync::Arc<dyn PairBackend>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// 集成测试用:换掉后台(与 paths / autostart 的 *_for_tests 同一做法)。传 None 恢复真实后台。
pub fn set_pair_backend_for_tests(backend: Option<std::sync::Arc<dyn PairBackend>>) {
    if let Ok(mut slot) = backend_slot().lock() {
        *slot = backend;
    }
}

fn backend() -> std::sync::Arc<dyn PairBackend> {
    backend_slot()
        .lock()
        .ok()
        .and_then(|slot| slot.clone())
        .unwrap_or_else(|| std::sync::Arc::new(LiveBackend))
}

/// 往运行时 stdin 写批准方指示(§2.4.1)。写失败(运行时已退出)不管。
struct Directives {
    stdin: Option<tokio::process::ChildStdin>,
}

impl Directives {
    async fn send(&mut self, line: String) {
        let Some(stdin) = self.stdin.as_mut() else {
            return;
        };
        if stdin.write_all(line.as_bytes()).await.is_err() {
            self.stdin = None;
            return;
        }
        let _ = stdin.flush().await;
    }

    async fn approver(&mut self, public_key: &str, account_id: &str) {
        self.send(runtime::approver_directive(public_key, account_id))
            .await;
    }

    async fn unbound(&mut self) {
        self.send(runtime::unbound_directive()).await;
    }
}

/// 后台登记成功的那条请求。
struct Registered {
    id: String,
    /// 后台回了 `approver_binding`:它支持批准方绑定。
    approver_binding: bool,
}

/// 后台 approved 时回的批准方 → 交给运行时的两个值。
/// 公钥这里**再严格解一次**(带填充的标准 base64、32 字节):解不开就当没有,按「缺失」中止。
fn approver_directive_for(status: &RemotePairStatus) -> Option<(String, String)> {
    let (public_key, account_id) = status.approver()?;
    let key = code::decode_public_key(public_key)?;
    Some((code::encode_public_key(&key), account_id.to_string()))
}

/// 登记跟随账号请求。网络错误与 5xx 退避重试(同一设备重新登记会顶替旧的,重试是安全的);
/// 后台明确表示不可用时立刻返回,不重试。
async fn register_pair(
    public_key: String,
    machine_name: String,
) -> Result<RemotePairCreated, PairApiError> {
    let mut last = PairApiError::Adapter(recodex_integration::AdapterError::Unavailable);
    for attempt in 0..PAIR_REGISTER_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(PAIR_REGISTER_BACKOFF * attempt).await;
        }
        let (key, name) = (public_key.clone(), machine_name.clone());
        // 声明这台电脑会核对批准方(§2.4.1):老后台不认这个字段会 400,那正是降级信号。
        let result =
            tokio::task::spawn_blocking(move || backend().create(&key, &name, true)).await;
        match result {
            Ok(Ok(created)) => return Ok(created),
            Ok(Err(error)) => {
                if pair_unavailable(&error) {
                    return Err(error);
                }
                last = error;
            }
            Err(error) => {
                last = PairApiError::Adapter(recodex_integration::AdapterError::InvalidResponse(
                    error.to_string(),
                ));
            }
        }
    }
    Err(last)
}

/// 处理运行时给出的一次性公钥:本机算确认码 → 登记到后台(声明会核对批准方)
/// → 与后台的码核对 → 画二维码(登记成功的加 `&bind=1`)。
///
/// 返回的 `Registered` 为 None = 这次只能扫码,调用方要往 stdin 递 `unbound`。
async fn on_waiting(
    public_key: &str,
    qr: &str,
    machine_name: &str,
    approver_check: bool,
) -> anyhow::Result<(WaitingInfo, Option<Registered>)> {
    let key = code::decode_public_key(public_key)
        .ok_or_else(|| anyhow::anyhow!("远程组件给出的公钥格式不对"))?;
    let local_code = code::confirm_code(&key);
    let canonical = code::encode_public_key(&key);

    let registered = if approver_check {
        match register_pair(canonical, machine_name.to_string()).await {
            Ok(created) => {
                if created.code != local_code {
                    log(
                        "phone_remote.confirm_code_mismatch",
                        json!({ "error": "server code differs from the locally computed one" }),
                    );
                    cancel_pair_request(created.id);
                    anyhow::bail!(
                        "安全检查未通过:服务端返回的确认码与本机从公钥算出的不一致,已中止配对。\
                         请不要在手机上允许任何待确认的电脑,并联系 ReCodex 客服。"
                    );
                }
                if !created.approver_binding || !created.approver_check {
                    // 后台没把它登记成「会核对批准方」:这条记录会被下发给手机、却没有批准方可核对。
                    // 撤回它,只用扫码(理论上旧后台直接 400,走不到这里)。
                    log(
                        "phone_remote.pair_registered_without_binding",
                        json!({
                            "approver_binding": created.approver_binding,
                            "approver_check": created.approver_check,
                        }),
                    );
                    cancel_pair_request(created.id);
                    Err(PromptUnavailable::BackendTooOld)
                } else {
                    Ok(Registered {
                        id: created.id,
                        approver_binding: created.approver_binding,
                    })
                }
            }
            Err(error) if pair_unavailable(&error) => Err(PromptUnavailable::Api(error)),
            // 网络错误 / 5xx / 回包解不开:后台可能已经建好了记录,不能悄悄降级成不核对(审计 S2)。
            Err(error) => {
                log(
                    "phone_remote.pair_register_failed",
                    json!({ "error": error.to_string() }),
                );
                anyhow::bail!("{}", register_failed_message(&describe_pair_error(&error)));
            }
        }
    } else {
        // 老运行时不会核对批准方:不登记跟随账号请求,只给二维码。
        log("phone_remote.runtime_without_approver_check", json!({}));
        Err(PromptUnavailable::RuntimeTooOld)
    };

    // 登记成功的二维码带 `&bind=1`:手机扫到它就知道这台电脑会核对批准方,必须经后台记录批准。
    let qr_content = match &registered {
        Ok(_) => format!("{qr}{PAIR_QR_BIND_SUFFIX}"),
        Err(_) => qr.to_string(),
    };
    let qr_svg = crate::connect::weixin::render_qr_svg(&qr_content).unwrap_or_default();
    let phone_note = registered.as_ref().err().map(PromptUnavailable::note);
    Ok((
        WaitingInfo {
            code: local_code,
            qr_svg,
            machine_name: machine_name.to_string(),
            phone_prompt: registered.is_ok(),
            phone_note,
            approved: false,
        },
        registered.ok(),
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
            reduce(&w, FlowEvent::Superseded),
            Phase::Error {
                message: MSG_SUPERSEDED.into()
            }
        );
        assert!(MSG_SUPERSEDED.contains("已在别处重新发起"));
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

    /// §2.4.1 的四个机读码各有自己的说法,且都不能说成「手机没把凭据交给中继」。
    #[test]
    fn approver_error_codes_map_to_their_own_wording() {
        let mismatch = approver_error_message(runtime::PAIR_ERR_APPROVER_MISMATCH, "").unwrap();
        assert!(mismatch.contains("抢先确认") && mismatch.contains("没有保存任何凭据"));
        let missing = approver_error_message(runtime::PAIR_ERR_APPROVER_MISSING, "x").unwrap();
        assert!(missing.contains("没有对应的账号批准记录") && missing.contains("更新到最新版"));
        let consumed =
            approver_error_message(runtime::PAIR_ERR_RELAY_ANSWER_CONSUMED, "").unwrap();
        assert!(consumed.contains("被别的程序取走"));
        // check_failed 把运行时那句原话附在后面,便于排障
        let failed =
            approver_error_message(runtime::PAIR_ERR_APPROVER_CHECK_FAILED, "  profile 502  ")
                .unwrap();
        assert!(failed.starts_with(MSG_APPROVER_CHECK_FAILED));
        assert!(failed.contains("profile 502"));
        assert_eq!(
            approver_error_message(runtime::PAIR_ERR_APPROVER_CHECK_FAILED, ""),
            Some(MSG_APPROVER_CHECK_FAILED.to_string())
        );
        // 四个以外的码按普通失败处理
        assert_eq!(approver_error_message("", "boom"), None);
        assert_eq!(approver_error_message("relay_unreachable", "boom"), None);
        for message in [
            MSG_APPROVER_MISMATCH,
            MSG_APPROVER_MISSING,
            MSG_APPROVER_CHECK_FAILED,
            MSG_RELAY_ANSWER_CONSUMED,
        ] {
            assert!(!message.contains("没把凭据交给中继"), "{message}");
        }
    }

    /// 审计 S4:后台回了 approver_binding 却没给批准方 → 分不清谁旧,两个都提。
    #[test]
    fn absent_approver_tells_apart_old_server_from_old_app() {
        assert_eq!(approver_absent_message(false), MSG_APPROVER_ABSENT_SERVER);
        assert!(!MSG_APPROVER_ABSENT_SERVER.contains("App 版本较旧"));
        assert_eq!(approver_absent_message(true), MSG_APPROVER_ABSENT_BOTH);
        assert!(MSG_APPROVER_ABSENT_BOTH.contains("服务端或手机 App 版本较旧"));
    }

    /// 审计 S2:只有「后台明确没建记录」才降级成只扫码;网络错误与 5xx 必须重试/中止。
    #[test]
    fn only_explicit_refusals_downgrade_to_qr_only() {
        use recodex_integration::AdapterError as E;
        for status in [400u16, 401, 403, 404, 429, 501] {
            assert!(
                pair_unavailable(&PairApiError::Http(status)),
                "HTTP {status} 应降级"
            );
        }
        for status in [409u16, 500, 502, 503, 504] {
            assert!(
                !pair_unavailable(&PairApiError::Http(status)),
                "HTTP {status} 不能降级"
            );
        }
        // 本机没有登录态:请求根本没发出去
        assert!(pair_unavailable(&PairApiError::Adapter(E::Unauthorized)));
        assert!(pair_unavailable(&PairApiError::Adapter(
            E::InvalidConfiguration("no state".into())
        )));
        // 连不上 / 回包解不开:后台可能已经建好了记录
        assert!(!pair_unavailable(&PairApiError::Adapter(E::Unavailable)));
        assert!(!pair_unavailable(&PairApiError::Adapter(
            E::InvalidResponse("bad json".into())
        )));
    }

    #[test]
    fn prompt_unavailable_notes_say_which_side_is_old() {
        assert!(
            PromptUnavailable::RuntimeTooOld
                .note()
                .contains("远程组件版本较旧")
        );
        assert!(
            PromptUnavailable::BackendTooOld
                .note()
                .contains("服务端暂不支持")
        );
        assert!(
            PromptUnavailable::Api(PairApiError::Http(401))
                .note()
                .contains("登录")
        );
        assert!(
            PromptUnavailable::Api(PairApiError::Http(503))
                .note()
                .contains("503")
        );
        assert!(register_failed_message("网络连接失败").contains("为安全起见已中止"));
    }

    /// 递给运行时的批准方必须是**规范写法**的公钥;解不开就当没有(按缺失中止)。
    #[test]
    fn approver_directive_needs_both_fields_and_a_real_key() {
        let good = RemotePairStatus {
            status: "approved".into(),
            approver_public_key: "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=".into(),
            approver_account_id: "acc_1-x".into(),
        };
        assert_eq!(
            approver_directive_for(&good),
            Some((
                "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=".to_string(),
                "acc_1-x".to_string()
            ))
        );
        // 少一个字段
        for status in [
            RemotePairStatus {
                approver_account_id: String::new(),
                ..good.clone()
            },
            RemotePairStatus {
                approver_public_key: String::new(),
                ..good.clone()
            },
            // 33 字节:形状像但不是内容公钥
            RemotePairStatus {
                approver_public_key: "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
                ..good.clone()
            },
        ] {
            assert_eq!(approver_directive_for(&status), None, "{status:?}");
        }
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
