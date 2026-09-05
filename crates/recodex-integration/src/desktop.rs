//! ReCodex desktop bridge：ReCodexState + 命令实现,从 manager 的 Tauri IPC 迁进本 crate,
//! 供 launcher 的 CDP 桥调用(不再依赖 manager app)。recodex-overlay 核心,逻辑照搬。
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex,
};
use std::sync::{MutexGuard, TryLockError};

use crate::{
    credential::{credential_target_for_api_url, CredentialStore, PlatformCredentialStore},
    Adapter, AdapterError, DiagnosticReport, HttpTransport, PublicLoginStart,
};
use serde_json::{json, Value};

pub struct ReCodexState {
    adapter: Mutex<Option<Adapter<HttpTransport>>>,
    credentials: Option<PlatformCredentialStore>,
    refresh_lock: Mutex<()>,
    snapshot_lock: Mutex<()>,
    pending_device_code: Mutex<Option<String>>,
    auth_epoch: AtomicU64,
    init_error: Option<String>,
    /// 启动时凭据存在、却没能用起来的原因。
    ///
    /// 原先这两步的失败都被 `let _ =` 吞掉:凭据读取报错当成"没有凭据",
    /// token 校验不过也无声跳过。用户看到的是「明明登录过,重启后变成未登录」,
    /// 而且**没有任何解释**,只能重新登录一次 —— 如果原因是凭据存储本身坏了,
    /// 重新登录也白搭,他会一直循环。
    credential_notice: Option<String>,
    /// 用户端站点地址。面板里「使用情况 / 邀请好友」要跳到网页,
    /// 地址放在这里而不是写死在注入脚本里 —— 指向测试环境时链接要跟着走。
    web_url: String,
}

fn parallel_snapshot_requests<UsageResult, AccountResult, GatewayResult>(
    usage: impl FnOnce() -> UsageResult + Send,
    account: impl FnOnce() -> AccountResult + Send,
    gateways: impl FnOnce() -> GatewayResult + Send,
) -> (UsageResult, AccountResult, GatewayResult)
where
    UsageResult: Send,
    AccountResult: Send,
    GatewayResult: Send,
{
    std::thread::scope(|scope| {
        let usage = scope.spawn(usage);
        let account = scope.spawn(account);
        let gateways = scope.spawn(gateways);
        (
            usage.join().expect("usage request worker panicked"),
            account.join().expect("account request worker panicked"),
            gateways.join().expect("gateway request worker panicked"),
        )
    })
}

// saved_api_base 读 ~/.codex/recodex/api-base（CLI 安装脚本与 login 写入的
// 来源站点）。桌面端与命令行同机共用一个 device_id，也共用这份来源：
// 代理站装过 CLI 的机器，桌面端自动跟随代理域名，不再露出平台主站。
// 文件缺失或不是 http(s) URL 一律当没有，行为与从前一致。
fn saved_api_base() -> Option<String> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    let path = std::path::Path::new(&home)
        .join(".codex")
        .join("recodex")
        .join("api-base");
    let value = std::fs::read_to_string(path).ok()?;
    let value = value.trim();
    if value.starts_with("https://") || value.starts_with("http://") {
        Some(value.to_owned())
    } else {
        None
    }
}

fn api_base_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    Some(
        std::path::Path::new(&home)
            .join(".codex")
            .join("recodex")
            .join("api-base"),
    )
}

/// persist_api_base 把来源站点写进 api-base(与 CLI 共用同一个文件)。
/// 只接受 http(s) origin;写入的是服务端校验过的值,这里不再做域名判断。
pub fn persist_api_base(origin: &str) -> std::io::Result<()> {
    let origin = origin.trim();
    if !origin.starts_with("https://") && !origin.starts_with("http://") {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "api base must be http(s)"));
    }
    let Some(path) = api_base_path() else {
        return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no home directory"));
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, format!("{origin}\n"))
}

/// macOS 的下载来源线索:Finder 给下载的文件写 `kMDItemWhereFroms`(下载地址列表),
/// 从 dmg 拖进「应用程序」时会带到 app 上。`mdls -raw` 直接打印那个数组;
/// 取第一个 http(s) 地址的 origin。没有(非浏览器下载、被清过)就 None。
#[cfg(target_os = "macos")]
fn portal_from_where_froms() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    // 属性挂在 .app 包上,不在里面的可执行文件上
    let bundle = exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app"))?;
    let out = std::process::Command::new("mdls")
        .args(["-raw", "-name", "kMDItemWhereFroms"])
        .arg(bundle)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let url = text
        .split('"')
        .find(|s| s.starts_with("https://") || s.starts_with("http://"))?;
    let rest = url.split("://").nth(1)?;
    let host = rest.split(['/', '?', '#']).next()?;
    if host.is_empty() {
        return None;
    }
    let scheme = if url.starts_with("https://") { "https" } else { "http" };
    Some(format!("{scheme}://{host}"))
}

#[cfg(not(target_os = "macos"))]
fn portal_from_where_froms() -> Option<String> {
    None
}

/// 平台主 API:只用来做一件事 —— 问「这个域名是不是已验证的代理站」。
/// 它不出现在任何界面上,归属定下来之后客户端也不再打它。
const PLATFORM_API: &str = "https://api.recodex.dev";

/// 线索来自客户端侧(文件名、下载来源、WhereFroms),持久化前必须让平台确认那是
/// 已验证的代理域名 —— 否则把安装包放到任意域名下,就能让这台机器把登录流量指过去。
/// 平台联系不上时不写(返回 false),交给登录时的最后防线;宁可多输一次码。
pub fn portal_trusted(origin: &str) -> bool {
    let Some(rest) = origin.split("://").nth(1) else { return false };
    let Some(host) = rest.split(['/', '?', '#']).next() else { return false };
    if host.is_empty() {
        return false;
    }
    let url = format!("{PLATFORM_API}/api/cli/auth/portal-check?host={host}");
    ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .get(&url)
        .call()
        .map(|resp| resp.status() == 204)
        .unwrap_or(false)
}

/// 经平台确认后再持久化。安装器收尾(--import-installer-tag)与 mac 首启共用。
pub fn persist_api_base_if_trusted(origin: &str) -> bool {
    if !portal_trusted(origin) {
        return false;
    }
    persist_api_base(origin).is_ok()
}

/// 首次启动的归属发现:api-base 还没有时,用安装包留下的线索补上一次。
/// Windows 的线索在安装器收尾时已由 --import-installer-tag 写入;这里兜 macOS。
pub fn discover_portal_once() {
    if portal_known() {
        return;
    }
    if let Some(origin) = portal_from_where_froms() {
        persist_api_base_if_trusted(&origin);
    }
}

/// portal_known 报告这台机器有没有归属(显式环境变量或 api-base 文件)。
/// 没有归属的客户端不该自动弹平台主站的授权页 —— 那正是贴牌漏出的地方;
/// 面板改为显示验证码,让用户去自己购买服务的网站输码,授权时归属随凭据回写。
pub fn portal_known() -> bool {
    std::env::var("RECODEX_API_URL").is_ok() || saved_api_base().is_some()
}

impl ReCodexState {
    pub fn from_env() -> Self {
        discover_portal_once();
        let api_url = std::env::var("RECODEX_API_URL")
            .ok()
            .or_else(saved_api_base)
            .unwrap_or_else(|| "https://api.recodex.dev".to_owned());
        let web_url = std::env::var("RECODEX_WEB_URL")
            .map(|value| value.trim_end_matches('/').to_owned())
            .ok()
            .or_else(|| saved_api_base().map(|v| v.trim_end_matches('/').to_owned()))
            .unwrap_or_else(|| "https://recodex.dev".to_owned());
        let result = HttpTransport::new(&api_url, std::time::Duration::from_secs(10))
            .and_then(|transport| Adapter::new(transport, &api_url))
            .and_then(|adapter| {
                let target = credential_target_for_api_url(&api_url).map_err(|_| {
                    crate::AdapterError::InvalidConfiguration(
                        "invalid credential origin".into(),
                    )
                })?;
                let credentials = PlatformCredentialStore::new(target).map_err(|_| {
                    crate::AdapterError::InvalidConfiguration(
                        "invalid credential target".into(),
                    )
                })?;
                Ok((adapter, credentials))
            });
        match result {
            Ok((mut adapter, credentials)) => {
                // 两处失败都要留下线索,不能再当作"本来就没登录"
                let credential_notice = match credentials.load() {
                    Ok(Some(saved)) => adapter
                        .set_access_token(saved.access_token.expose().to_owned())
                        .err()
                        .map(|error| format!("已保存的登录凭据无法使用({error}),请重新登录")),
                    Ok(None) => None,
                    Err(error) => Some(format!("读取已保存的登录凭据失败:{error}")),
                };
                Self {
                    adapter: Mutex::new(Some(adapter)),
                    credentials: Some(credentials),
                    refresh_lock: Mutex::new(()),
                    snapshot_lock: Mutex::new(()),
                    pending_device_code: Mutex::new(None),
                    auth_epoch: AtomicU64::new(0),
                    init_error: None,
                    credential_notice,
                    web_url,
                }
            }
            Err(error) => Self {
                adapter: Mutex::new(None),
                credentials: None,
                refresh_lock: Mutex::new(()),
                snapshot_lock: Mutex::new(()),
                pending_device_code: Mutex::new(None),
                auth_epoch: AtomicU64::new(0),
                init_error: Some(error.to_string()),
                credential_notice: None,
                web_url,
            },
        }
    }

    /// 后台把本地诊断日志里的报错传回服务器(逻辑在 crate::diagnostics_flush)。
    ///
    /// launcher 建好 state 后调一次。这个 crate 拿不到 codex-plus-core 的日志路径,由调用方
    /// 传进来。启动先等 20s 让主流程起来、别抢 I/O;之后每 10 分钟一轮,每轮最多 20 条 ——
    /// 匿名口按 IP 限流 0.2/s,这个节奏扛得住。
    ///
    /// 持有的是启动时的 adapter fork:中途重新登录不会同步过来,那些报错会按匿名带 device_id
    /// 上去(device_id 就是登录注册的设备号,服务端能 join 回用户),重启后回到已认证。
    /// adapter 没建起来(init_error)就什么都不做 —— 诊断上报绝不能反过来影响启动。
    /// ponytail: 先这样;真要实时跟身份,每轮从 self.adapter 重新 fork 即可。
    pub fn spawn_diagnostics_flush(&self, log_path: std::path::PathBuf, client_version: &'static str) {
        // 幂等:state 若被建了不止一次,也只起一个线程 —— 两个线程抢同一个水位会重复上传。
        static SPAWNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if SPAWNED.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        let adapter = match self.adapter.lock() {
            Ok(guard) => match guard.as_ref() {
                Some(adapter) => adapter.fork(),
                None => return,
            },
            Err(_) => return,
        };
        let _ = std::thread::Builder::new()
            .name("recodex-diag-flush".into())
            .spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(20));
                let device_id = crate::load_or_create_install_id().unwrap_or_default();
                loop {
                    let _ = adapter.flush_diagnostic_log(&log_path, &device_id, client_version, 20);
                    std::thread::sleep(std::time::Duration::from_secs(600));
                }
            });
    }
}

fn error(code: &str, message: impl Into<String>) -> Value {
    json!({"status":"error", "error":{"code":code,"message":message.into()}})
}

/// 这一批请求里有没有 401。
///
/// 只看 usage 不够:在网页端撤销设备之后 account 一样会 401,而它的错误原先只是
/// 被塞进 `account_error` 当成「数据陈旧」展示 —— 用户看到的是一份过期额度,
/// 而不是"你需要重新登录"。
pub fn any_unauthorized(errors: &[Option<&AdapterError>]) -> bool {
    errors
        .iter()
        .any(|error| matches!(error, Some(AdapterError::Unauthorized)))
}

/// 服务端不认这份凭据了 —— 最常见的原因是用户在网页端把这台设备撤销了。
///
/// 必须回 `signed_out` 而**不是** error:面板的 error 分支只画一个「重试」按钮,
/// 而这里重试多少次都是同一个 401,用户永远走不到登录入口,等于被锁死在面板里。
/// `signed_out` 才会渲染「登录 ReCodex」。
pub fn credentials_rejected() -> Value {
    json!({
        "status": "signed_out",
        "notice": "设备已在网页端被撤销,或凭据已失效。请重新登录。",
    })
}

/// 把一次**带凭据**的上游调用失败翻成面板能处理的 Value。
///
/// `Unauthorized` 必须走 `credentials_rejected` —— 理由见它的文档注释。
/// 这条规则属于每一个带凭据的入口,不只是 snapshot:1.2.53 就是因为
/// 只有 usage 一处照做,设备被撤销后面板停在「重试」上再也走不到登录入口。
/// 登录相关的两个入口不在此列 —— 它们本来就不带凭据,那里的 401 另有含义。
pub fn adapter_failure(code: &str, adapter_error: &AdapterError) -> Value {
    if matches!(adapter_error, AdapterError::Unauthorized) {
        return credentials_rejected();
    }
    error(code, adapter_error.to_string())
}

// Point Codex at the just-selected gateway by rewriting the managed config
// block in ~/.codex/config.toml. Returns a warning string if the rewrite failed
// (the server-side selection still succeeded); an empty endpoint routes nothing.
// ── 写 `~/.codex` 的两个入口 ─────────────────────────────────────────────
//
// 「官方模式」是个状态机,而往 `~/.codex` 写 ReCodex 配置的地方不止一处 ——
// 各写各的,就会出现"面板显示官方模式、Codex 却已走回 ReCodex"这类无声的不一致。
// (已经因此踩过四次:快照漏 auth.json / 卸载时被还原回来 / 登出不清快照 / 换网关破坏模式。)
//
// 所以本文件只允许**这两个函数**碰 `codexcfg` 的安装类接口,策略在各自处写死:
//   - `install_login_config`  登录 = 用户明确要用 ReCodex → 丢弃快照,写活配置
//   - `route_codex_through_gateway` 换网关 = 不表态用哪个模式 → 官方模式下只记快照
//
// 下面的 `config_writers_are_centralised` 测试会盯着这条约束,免得再冒出第三个写入口。

/// 登录成功后把服务端下发的配置装进 `~/.codex`。
///
/// **登录即表态**:用户在官方模式下重新登录 ReCodex,意思就是现在要用 ReCodex。
/// 所以先丢掉官方模式快照 —— 留着的话 `is_official_mode()` 仍为真,
/// 状态灯不显示、按钮显示"切回 ReCodex",一点就用陈旧的网关和 key 盖掉刚登录的配置。
fn install_login_config(
    config: &str,
    auth_json: &str,
    env_key: &str,
    env_value: &str,
) -> std::io::Result<()> {
    let _ = crate::officialmode::discard_snapshot();
    crate::codexcfg::apply_login(config, auth_json, env_key, env_value)
}

fn route_codex_through_gateway(endpoint: &str) -> Option<String> {
    let endpoint = endpoint.trim().trim_end_matches('/');
    if endpoint.is_empty() {
        return None;
    }
    let base = format!("{endpoint}/backend-api/codex");
    // 校验必须排在 stage_config_for_return **之前**:那一步会把块写进官方模式
    // 快照,等到后面 route_through_gateway 才拒绝就晚了 —— 快照里已经留下了
    // 一个被注入的托管块,切回 ReCodex 时照样生效。
    if !crate::codexcfg::base_url_is_safe(&base) {
        return Some("网关地址含有不能写进配置的字符".to_string());
    }
    // 官方模式下不能碰活配置 —— 否则「用最快网关」会把官方模式悄悄破坏掉:
    // 面板还显示官方模式,Codex 下次启动却已经走回 ReCodex 网关。
    // 记进快照,切回 ReCodex 时自动生效。
    let block = crate::codexcfg::render_sub2api_block(&base);
    match crate::officialmode::stage_config_for_return(&block) {
        Ok(true) => {
            return Some("当前是官方模式,新网关已记下,切回 ReCodex 后生效".to_string());
        }
        Ok(false) => {}
        Err(io_error) => return Some(io_error.to_string()),
    }
    crate::codexcfg::route_through_gateway(&base)
        .err()
        .map(|adapter_error| adapter_error.to_string())
}

fn try_snapshot_lock(lock: &Mutex<()>) -> Result<MutexGuard<'_, ()>, Value> {
    match lock.try_lock() {
        Ok(guard) => Ok(guard),
        Err(TryLockError::WouldBlock) => Err(error(
            "busy",
            "A ReCodex status refresh is already in progress",
        )),
        Err(TryLockError::Poisoned(_)) => {
            Err(error("state_unavailable", "ReCodex state is unavailable"))
        }
    }
}

/// 拿快照锁。锁本身是必要的:snapshot 末尾会 `merge_cache_from`,并发合并会把缓存搅乱。
///
/// 但**普通轮询和「点刷新」不该同等对待**:
/// - 点刷新要打上游,可能几秒;这期间面板每隔几秒的轮询全都撞锁,
///   于是用户看到一连串「A ReCodex status refresh is already in progress」——
///   一次无害的重叠被变成了可见故障,而面板上本来就有数据可以继续显示。
/// - 反过来,用户连点两次刷新,如实告诉他「正在刷新」是对的。
///
/// 所以:轮询**等一会儿**(有上限,不会挂死),刷新仍然 try_lock 立即返回。
fn acquire_snapshot_lock(lock: &Mutex<()>, refresh: bool) -> Result<MutexGuard<'_, ()>, Value> {
    if refresh {
        return try_snapshot_lock(lock);
    }
    // ponytail: 轮询等最多 2 秒(20 × 100ms)。std 的 Mutex 没有带超时的 lock,
    // 又不值得为此引入 parking_lot;真需要更精细再说。
    for _ in 0..SNAPSHOT_LOCK_WAIT_TRIES {
        match lock.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::WouldBlock) => {
                std::thread::sleep(SNAPSHOT_LOCK_WAIT_STEP);
            }
            Err(TryLockError::Poisoned(_)) => {
                return Err(error("state_unavailable", "ReCodex state is unavailable"));
            }
        }
    }
    try_snapshot_lock(lock)
}

const SNAPSHOT_LOCK_WAIT_TRIES: u32 = 20;
const SNAPSHOT_LOCK_WAIT_STEP: std::time::Duration = std::time::Duration::from_millis(100);

fn snapshot_epoch_is_current(started_epoch: u64, current_epoch: u64) -> bool {
    started_epoch == current_epoch
}

fn snapshot(state: &ReCodexState, refresh: bool) -> Value {
    let _snapshot_guard = match acquire_snapshot_lock(&state.snapshot_lock, refresh) {
        Ok(guard) => guard,
        Err(value) => return value,
    };
    let epoch = state.auth_epoch.load(Ordering::SeqCst);
    let worker = match state.adapter.lock() {
        Ok(guard) => match guard.as_ref() {
            Some(adapter) => adapter.fork(),
            None => {
                return error(
                    "configuration",
                    state
                        .init_error
                        .clone()
                        .unwrap_or_else(|| "ReCodex is not configured".to_owned()),
                );
            }
        },
        Err(_) => return error("state_unavailable", "ReCodex state is unavailable"),
    };
    if !worker.is_authenticated() {
        // 带上"为什么没登录上"。没有 notice 时就是普通的未登录。
        return match state.credential_notice.as_deref() {
            Some(notice) => json!({"status":"signed_out", "notice": notice}),
            None => json!({"status":"signed_out"}),
        };
    }
    // Network I/O runs on an isolated adapter copy. Logout and other IPC
    // commands can acquire the real state mutex while upstream requests wait.
    let account_worker = worker.fork();
    let gateway_worker = worker.fork();
    let org_worker = worker.fork();
    let ((usage, usage_worker), account, gateways) = parallel_snapshot_requests(
        move || worker.usage_in_fork(refresh),
        move || account_worker.account(),
        move || gateway_worker.gateways(),
    );
    if any_unauthorized(&[usage.as_ref().err(), account.as_ref().err(), gateways.as_ref().err()]) {
        return credentials_rejected();
    }
    let usage = match usage {
        Ok(value) => value,
        Err(adapter_error) => return adapter_failure("usage", &adapter_error),
    };
    let account_error = account.as_ref().err().map(ToString::to_string);
    let gateway_error = gateways.as_ref().err().map(ToString::to_string);
    let gateways = gateways.unwrap_or_default();
    let selected = gateways.iter().find(|gateway| gateway.selected).cloned();
    // 组织列表随快照一起带回,面板才画得出切换器。
    //
    // 取失败只当「没有组织可切」,不参与 stale 判定 —— 切换是附加能力,
    // 而账号和用量才是这个面板的主功能。让它把整块打成 stale 的话,
    // 一个可选能力的故障会显示成「你的额度数据不可信」。
    // 串行取而不是塞进 parallel_snapshot_requests:那个辅助函数只收三路,
    // 为一个可选能力改它的签名会波及所有调用点。组织列表是一次带索引的
    // 小查询,多这一跳不值得动公共代码。
    let organizations = org_worker.organizations().unwrap_or_default();
    let mut guard = match state.adapter.lock() {
        Ok(value) => value,
        Err(_) => return error("state_unavailable", "ReCodex state is unavailable"),
    };
    if !snapshot_epoch_is_current(epoch, state.auth_epoch.load(Ordering::SeqCst)) {
        return json!({"status":"signed_out"});
    }
    if let Some(adapter) = guard.as_mut() {
        adapter.merge_cache_from(&usage_worker);
    }
    drop(guard);
    let stale = usage.stale || account_error.is_some() || gateway_error.is_some();
    json!({"status": if stale { "stale" } else { "ready" }, "data":{"account":account.ok(),"usage":usage,"gateways":gateways,"selected_gateway":selected,"account_error":account_error,"gateway_error":gateway_error,"organizations":organizations,"web_url":state.web_url}})
}

pub fn recodex_status(state: &ReCodexState) -> Value {
    snapshot(&state, false)
}

pub fn recodex_refresh_usage(state: &ReCodexState) -> Value {
    snapshot(&state, true)
}

pub fn recodex_refresh_token(state: &ReCodexState) -> Value {
    let _refresh_guard = match state.refresh_lock.lock() {
        Ok(value) => value,
        Err(_) => return error("state_unavailable", "ReCodex state is unavailable"),
    };
    let epoch = state.auth_epoch.load(Ordering::SeqCst);
    let guard = match state.adapter.lock() {
        Ok(value) => value,
        Err(_) => return error("state_unavailable", "ReCodex state is unavailable"),
    };
    let Some(adapter) = guard.as_ref() else {
        return error(
            "configuration",
            state
                .init_error
                .clone()
                .unwrap_or_else(|| "ReCodex is not configured".to_owned()),
        );
    };
    if !adapter.is_authenticated() {
        return json!({"status":"signed_out"});
    }
    let mut worker = adapter.fork();
    drop(guard);
    let token = match worker.refresh_token() {
        Ok(value) => value,
        Err(adapter_error) => return adapter_failure("refresh", &adapter_error),
    };
    let secret = match crate::credential::Secret::new(token.clone()) {
        Ok(value) => value,
        Err(_) => return error("credential_store", "Refreshed token is invalid"),
    };
    if state.auth_epoch.load(Ordering::SeqCst) != epoch {
        return error("refresh_cancelled", "ReCodex refresh was cancelled");
    }
    if state.credentials.as_ref().is_none_or(|store| {
        store
            .save(crate::credential::StoredCredentials {
                access_token: secret,
                refresh_token: None,
            })
            .is_err()
    }) {
        return error(
            "credential_store",
            "Unable to persist refreshed credentials",
        );
    }
    let mut guard = match state.adapter.lock() {
        Ok(value) => value,
        Err(_) => return error("state_unavailable", "ReCodex state is unavailable"),
    };
    if state.auth_epoch.load(Ordering::SeqCst) != epoch {
        let _ = state.credentials.as_ref().map(CredentialStore::clear);
        return error("refresh_cancelled", "ReCodex refresh was cancelled");
    }
    let Some(adapter) = guard.as_mut() else {
        let _ = state.credentials.as_ref().map(CredentialStore::clear);
        return error("configuration", "ReCodex is not configured");
    };
    if adapter.set_access_token(token).is_err() {
        let _ = state.credentials.as_ref().map(CredentialStore::clear);
        return error("credential_store", "Refreshed token is invalid");
    }
    json!({"status":"ready"})
}

pub fn recodex_check_client(state: &ReCodexState) -> Value {
    let guard = match state.adapter.lock() {
        Ok(value) => value,
        Err(_) => return error("state_unavailable", "ReCodex state is unavailable"),
    };
    let Some(adapter) = guard.as_ref() else {
        return error(
            "configuration",
            state
                .init_error
                .clone()
                .unwrap_or_else(|| "ReCodex is not configured".to_owned()),
        );
    };
    let authenticated = adapter.is_authenticated();
    let adapter = adapter.fork();
    drop(guard);
    let compatibility = match adapter.compatibility(env!("CARGO_PKG_VERSION")) {
        Ok(value) => value,
        Err(adapter_error) => return adapter_failure("compatibility", &adapter_error),
    };
    if !authenticated {
        return json!({"status":"signed_out", "data":{"compatibility":compatibility}});
    }
    let update_channel = match adapter.update_channel("stable") {
        Ok(value) => value,
        Err(adapter_error) => return adapter_failure("update", &adapter_error),
    };
    json!({"status":"ready", "data":{"compatibility":compatibility,"update_channel":update_channel}})
}

pub fn recodex_report_diagnostics(state: &ReCodexState) -> Value {
    let guard = match state.adapter.lock() {
        Ok(value) => value,
        Err(_) => return error("state_unavailable", "ReCodex state is unavailable"),
    };
    let Some(adapter) = guard.as_ref() else {
        return error(
            "configuration",
            state
                .init_error
                .clone()
                .unwrap_or_else(|| "ReCodex is not configured".to_owned()),
        );
    };
    if !adapter.is_authenticated() {
        return json!({"status":"signed_out"});
    }
    let adapter = adapter.fork();
    drop(guard);
    let report = DiagnosticReport {
        client_version: env!("CARGO_PKG_VERSION").to_owned(),
        os: std::env::consts::OS.to_owned(),
        event: "manual_report".to_owned(),
        error_code: None,
        device_id: None,
        category: None,
        gateway: None,
        message: None,
        occurred_at: None,
    };
    match adapter.report_diagnostic(&report) {
        Ok(value) => json!({"status":"ready", "data":{"diagnostics":value}}),
        Err(adapter_error) => adapter_failure("diagnostics", &adapter_error),
    }
}

pub fn recodex_select_gateway(state: &ReCodexState, id: String) -> Value {
    let worker = match state.adapter.lock() {
        Ok(value) => value,
        Err(_) => return error("state_unavailable", "ReCodex state is unavailable"),
    };
    let Some(adapter) = worker.as_ref() else {
        return error(
            "configuration",
            state
                .init_error
                .clone()
                .unwrap_or_else(|| "ReCodex is not configured".to_owned()),
        );
    };
    if !adapter.is_authenticated() {
        return json!({"status":"signed_out"});
    }
    let adapter = adapter.fork();
    drop(worker);
    match adapter.select_gateway(&id) {
        Ok(gateway) => match route_codex_through_gateway(&gateway.endpoint) {
            Some(warning) => json!({"status":"ready", "data":{"selected_gateway":gateway}, "warning":{"code":"codex_config","message":warning}}),
            None => json!({"status":"ready", "data":{"selected_gateway":gateway}}),
        },
        Err(adapter_error) => adapter_failure("gateway", &adapter_error),
    }
}

pub fn recodex_use_fastest_gateway(state: &ReCodexState) -> Value {
    let guard = match state.adapter.lock() {
        Ok(value) => value,
        Err(_) => return error("state_unavailable", "ReCodex state is unavailable"),
    };
    let Some(adapter) = guard.as_ref() else {
        return error(
            "configuration",
            state
                .init_error
                .clone()
                .unwrap_or_else(|| "ReCodex is not configured".to_owned()),
        );
    };
    if !adapter.is_authenticated() {
        return json!({"status":"signed_out"});
    }
    let adapter = adapter.fork();
    drop(guard);
    match adapter.use_fastest_gateway() {
        Ok(gateway) => match route_codex_through_gateway(&gateway.endpoint) {
            Some(warning) => json!({"status":"ready", "data":{"selected_gateway":gateway}, "warning":{"code":"codex_config","message":warning}}),
            None => json!({"status":"ready", "data":{"selected_gateway":gateway}}),
        },
        Err(adapter_error) => adapter_failure("gateway", &adapter_error),
    }
}

/// 列出这个用户能切到哪些组织。
pub fn recodex_organizations(state: &ReCodexState) -> Value {
    let worker = match state.adapter.lock() {
        Ok(value) => value,
        Err(_) => return error("state_unavailable", "ReCodex state is unavailable"),
    };
    let Some(adapter) = worker.as_ref() else {
        return error(
            "configuration",
            state
                .init_error
                .clone()
                .unwrap_or_else(|| "ReCodex is not configured".to_owned()),
        );
    };
    if !adapter.is_authenticated() {
        return json!({"status":"signed_out"});
    }
    let adapter = adapter.fork();
    drop(worker);
    match adapter.organizations() {
        Ok(organizations) => json!({"status":"ready", "data":{"organizations":organizations}}),
        Err(adapter_error) => adapter_failure("org", &adapter_error),
    }
}

/// 把这台设备切到目标组织。
///
/// **网关 Key 不进 webview。** 它只在进程内用于写用户环境 —— 送给前端就等于
/// 把一份长期有效的凭据暴露给任何能开 devtools 的人,而前端拿它也没有用途。
/// 返回体里只有组织名和套餐名。
///
/// 顺序:切换 → 写环境变量 → 回报。写失败必须让整个调用失败:
/// 服务端已经改了设备归属,本地不写就是「服务端认为你在新组织、Codex 还拿着
/// 旧组织的 Key」—— 请求照常成功,只是用量记到旧组织,没有任何一处报错。
/// 消耗一次官方重置额度。
///
/// 不收任何参数:账号由服务端按 (用户, 这台设备的组织) 解析 —— 让客户端指定
/// 就等于把「重置任意账号」开放出去。能不能按也由服务端判定,面板只是照着画。
pub fn recodex_reset_quota(state: &ReCodexState) -> Value {
    let worker = match state.adapter.lock() {
        Ok(value) => value,
        Err(_) => return error("state_unavailable", "ReCodex state is unavailable"),
    };
    let Some(adapter) = worker.as_ref() else {
        return error(
            "configuration",
            state
                .init_error
                .clone()
                .unwrap_or_else(|| "ReCodex is not configured".to_owned()),
        );
    };
    if !adapter.is_authenticated() {
        return json!({"status":"signed_out"});
    }
    let adapter = adapter.fork();
    drop(worker);

    match adapter.reset_quota() {
        Ok(result) => json!({"status":"ready","data":{
            "account_id": result.account_id,
            "remaining": result.remaining,
            "state_recovered": result.state_recovered,
        }}),
        Err(adapter_error) => adapter_failure("reset", &adapter_error),
    }
}

pub fn recodex_switch_org(state: &ReCodexState, org_id: i64) -> Value {
    let worker = match state.adapter.lock() {
        Ok(value) => value,
        Err(_) => return error("state_unavailable", "ReCodex state is unavailable"),
    };
    let Some(adapter) = worker.as_ref() else {
        return error(
            "configuration",
            state
                .init_error
                .clone()
                .unwrap_or_else(|| "ReCodex is not configured".to_owned()),
        );
    };
    if !adapter.is_authenticated() {
        return json!({"status":"signed_out"});
    }
    let adapter = adapter.fork();
    drop(worker);

    let switched = match adapter.switch_organization(org_id) {
        Ok(value) => value,
        Err(adapter_error) => return adapter_failure("org", &adapter_error),
    };

    if let Err(io_error) = crate::codexcfg::set_user_env(
        crate::codexcfg::SUB2API_ENV_KEY,
        switched.gateway_key.trim(),
    ) {
        return error(
            "org_env",
            format!(
                "已切到「{}」，但写入 {} 失败：{io_error}。请重新尝试切换。",
                switched.org_name,
                crate::codexcfg::SUB2API_ENV_KEY
            ),
        );
    }

    // 让**当前进程**也立刻用上新 Key,而不是等下次启动 —— 桌面端拉起 Codex
    // 子进程时会继承自己的环境。不同步的话用户切完还得重启客户端,
    // 而「切了没生效」看起来就是切换坏了。
    //
    // Safety: 与 refresh_key_env_from_user_scope 同一个理由 —— 这里是用户
    // 主动触发的单次调用,不在拉起子进程的过程中。
    unsafe {
        std::env::set_var(
            crate::codexcfg::SUB2API_ENV_KEY,
            switched.gateway_key.trim(),
        )
    };

    json!({
        "status": "ready",
        "data": {
            "org_id": switched.org_id,
            "org_name": switched.org_name,
            "plan_name": switched.plan_name,
        }
    })
}

pub fn recodex_login_start(state: &ReCodexState) -> Value {
    let epoch = state.auth_epoch.fetch_add(1, Ordering::SeqCst) + 1;
    let guard = match state.adapter.lock() {
        Ok(value) => value,
        Err(_) => return error("state_unavailable", "ReCodex state is unavailable"),
    };
    let Some(adapter) = guard.as_ref() else {
        return error(
            "configuration",
            state
                .init_error
                .clone()
                .unwrap_or_else(|| "ReCodex is not configured".to_owned()),
        );
    };
    let adapter = adapter.fork();
    drop(guard);
    let device_id = match crate::load_or_create_install_id() {
        Ok(value) => value,
        Err(_) => return error("device_identity", "Unable to load ReCodex desktop identity"),
    };
    match adapter.start_login(
        &device_id,
        "ReCodex Desktop",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
    ) {
        Ok(login) => {
            let mut pending = match state.pending_device_code.lock() {
                Ok(value) => value,
                Err(_) => return error("state_unavailable", "ReCodex state is unavailable"),
            };
            if state.auth_epoch.load(Ordering::SeqCst) != epoch {
                return error("login_cancelled", "ReCodex login was cancelled");
            }
            *pending = Some(login.device_code.clone());
            let mut data = serde_json::to_value(PublicLoginStart::from(&login)).unwrap_or_default();
            if let Some(map) = data.as_object_mut() {
                // 面板据此决定:知道归属 → 照常自动打开授权页;
                // 不知道 → 只显示验证码,提示去自己购买服务的网站输码。
                map.insert("portal_known".into(), json!(portal_known()));
            }
            json!({"status":"pending", "data":data})
        }
        Err(adapter_error) => error("login", adapter_error.to_string()),
    }
}

pub fn recodex_login_poll(state: &ReCodexState) -> Value {
    let epoch = state.auth_epoch.load(Ordering::SeqCst);
    let device_code = match state.pending_device_code.lock() {
        Ok(pending) => pending.clone(),
        Err(_) => return error("state_unavailable", "ReCodex state is unavailable"),
    };
    let Some(device_code) = device_code else {
        return error("login", "No pending ReCodex login");
    };
    let guard = match state.adapter.lock() {
        Ok(value) => value,
        Err(_) => return error("state_unavailable", "ReCodex state is unavailable"),
    };
    let Some(adapter) = guard.as_ref() else {
        return error(
            "configuration",
            state
                .init_error
                .clone()
                .unwrap_or_else(|| "ReCodex is not configured".to_owned()),
        );
    };
    let mut worker = adapter.fork();
    drop(guard);
    match worker.poll_login(&device_code) {
        Ok(result) if result.status == "approved" => {
            let Some(token) = (!result.token.is_empty()).then_some(result.token.clone()) else {
                return error("login", "Approved login did not return a token");
            };
            let secret = match crate::credential::Secret::new(token.clone()) {
                Ok(value) => value,
                Err(_) => return error("credential_store", "Approved token is invalid"),
            };
            let credentials = crate::credential::StoredCredentials {
                access_token: secret,
                refresh_token: None,
            };
            let mut guard = match state.adapter.lock() {
                Ok(value) => value,
                Err(_) => return error("state_unavailable", "ReCodex state is unavailable"),
            };
            if state.auth_epoch.load(Ordering::SeqCst) != epoch {
                return error("login_cancelled", "ReCodex login was cancelled");
            }
            if state
                .credentials
                .as_ref()
                .is_none_or(|store| store.save(credentials).is_err())
            {
                return error("credential_store", "Unable to persist ReCodex credentials");
            }
            if state.auth_epoch.load(Ordering::SeqCst) != epoch {
                let _ = state.credentials.as_ref().map(CredentialStore::clear);
                return error("login_cancelled", "ReCodex login was cancelled");
            }
            let Some(adapter) = guard.as_mut() else {
                let _ = state.credentials.as_ref().map(CredentialStore::clear);
                return error("configuration", "ReCodex is not configured");
            };
            if adapter.set_access_token(token).is_err() {
                let _ = state.credentials.as_ref().map(CredentialStore::clear);
                return error("credential_store", "Approved token is invalid");
            }
            drop(guard);
            if let Ok(mut pending) = state.pending_device_code.lock() {
                *pending = None;
            }
            // Route Codex through ReCodex: write the config.toml block, auth.json
            // and RECODEX_KEY env var the server just handed us, so the launched
            // Codex uses ReCodex. Non-fatal — the session is valid regardless.
            if let Err(config_error) = install_login_config(
                &result.config,
                &result.auth_json,
                &result.env_key,
                &result.env_value,
            ) {
                return json!({"status":"approved", "warning":{"code":"codex_config","message":config_error.to_string()}});
            }
            // 归属回写:服务端说授权是在哪个站点完成的,这台机器从此就归那个站点
            // (贴牌的最后防线)。写失败只影响下次启动的默认值,不算登录失败。
            if !result.portal.is_empty() {
                let _ = persist_api_base(&result.portal);
            }
            json!({"status":"approved"})
        }
        // device_limit 会带上占用名额的设备名单,必须一起透给面板 ——
        // 只透 status 的话,面板只能显示「等待确认…」,而服务端其实早就说明了原因。
        Ok(result) => json!({"status":result.status, "devices":result.devices}),
        Err(adapter_error) => error("login", adapter_error.to_string()),
    }
}

pub fn recodex_logout(state: &ReCodexState) -> Value {
    state.auth_epoch.fetch_add(1, Ordering::SeqCst);
    let mut guard = match state.adapter.lock() {
        Ok(value) => value,
        Err(_) => return error("state_unavailable", "ReCodex state is unavailable"),
    };
    let worker = if let Some(adapter) = guard.as_mut() {
        let worker = adapter.fork();
        adapter.clear_access_token();
        Some(worker)
    } else {
        None
    };
    drop(guard);
    let revoke_failed = worker.is_some_and(|adapter| adapter.revoke_session().is_err());
    if let Ok(mut pending) = state.pending_device_code.lock() {
        *pending = None;
    }
    // Revert the Codex config we own so Codex stops using ReCodex. Best-effort:
    // a failure here must not block sign-out.
    let _ = crate::codexcfg::restore_all();
    // 快照里的托管块和 key 都是这次会话的,登出后已经作废。
    // 留着的话:下次登录后 `is_official_mode()` 仍为真(灯不显示、按钮显示"切回"),
    // 用户一点"切回 ReCodex",陈旧的网关和 key 就会盖掉刚登录的正确配置。
    let _ = crate::officialmode::discard_snapshot();
    if state
        .credentials
        .as_ref()
        .is_none_or(|store| store.clear().is_err())
    {
        return error("credential_store", "Unable to clear ReCodex credentials");
    }
    if revoke_failed {
        return json!({"status":"signed_out", "error":{"code":"server_logout_failed","message":"Signed out locally; the server session could not be revoked"}});
    }
    json!({"status":"signed_out"})
}

/// 官方模式状态(面板用来决定显示哪种按钮,以及状态灯是否隐藏)。
pub fn recodex_official_mode_status() -> Value {
    json!({
        "status": "ready",
        "data": { "official": crate::officialmode::is_official_mode() }
    })
}

/// 切到官方 ChatGPT 模式(可逆:先存快照再撤配置)。
pub fn recodex_official_mode_enable() -> Value {
    match crate::officialmode::switch_to_official() {
        Ok(()) => json!({"status":"ready","data":{"official":true},
            "message":"已切到官方模式,重启 Codex 后生效"}),
        Err(io_error) => error("official_mode", io_error.to_string()),
    }
}

/// 切回 ReCodex(按快照写回,无需重新登录)。
pub fn recodex_official_mode_disable() -> Value {
    match crate::officialmode::switch_to_recodex() {
        Ok(()) => json!({"status":"ready","data":{"official":false},
            "message":"已切回 ReCodex,重启 Codex 后生效"}),
        Err(io_error) => error("official_mode", io_error.to_string()),
    }
}

/// 卸载前的本地清理:登出(服务端吊销设备 + 清凭据 + 还原 Codex 配置),
/// 并清掉官方模式快照(残留会让下次安装误判仍在官方模式)。
/// 删目录/快捷方式/自删 exe 由 core 的 uninstall 模块接手 —— 放在 core 是为了
/// 避免本 crate 反向依赖 codex-plus-core。
pub fn recodex_prepare_uninstall(state: &ReCodexState) -> Value {
    let logout = recodex_logout(state);
    // 登出里的 restore_all() 是 best-effort(登出本身不该被配置写入失败卡住),
    // 但卸载不一样:配置没还原就删程序,用户会被扔在"配置被改过而程序没了"的死局。
    // 所以这里**显式**再还原一次并把结果报上去,由 core 决定中止。
    if let Err(restore_error) = crate::codexcfg::restore_all() {
        return json!({
            "status": "failed",
            "message": format!("还原 Codex 配置失败:{restore_error}")
        });
    }
    // 这里必须是**丢弃**而不是 switch_to_recodex():后者会把登出刚撤掉的托管块和
    // RECODEX_KEY 重新装回去,程序随即自删 —— 用户剩下一个指向已吊销网关的 Codex。
    // (recodex_logout 内部已经丢过一次,这里兜底,顺序换了也不会漏。)
    let _ = crate::officialmode::discard_snapshot();
    json!({
        "status": "ready",
        "warning": logout.get("error").and_then(|v| v.get("message")).cloned()
    })
}

// ── 自诊断 ──────────────────────────────────────────────────────────────
//
// 为什么桌面端非有不可：命令行有 `recodex doctor [--fix]`，桌面端一直没有，
// 于是「配置被清掉 / key 被清空」这类故障只能去命令行救 —— 而安装包**只装
// codex-plus-plus.exe，不带 recodex.exe**（scripts/installer/windows/ReCodex.nsi），
// 纯桌面端用户根本没有那个命令可跑。
//
// 检查全部本地可算（不联网），因为要诊断的恰恰是「联不上」之前的那一层。

fn check(id: &str, ok: bool, detail: &str) -> Value {
    json!({"id": id, "status": if ok { "ok" } else { "fail" }, "detail": detail})
}

/// 读 `~/.codex/config.toml` 并体检托管块。
fn doctor_config_checks(checks: &mut Vec<Value>) -> bool {
    let content = crate::codexcfg::config_path()
        .and_then(std::fs::read_to_string)
        .unwrap_or_default();
    let health = crate::codexcfg::inspect_config(&content);
    checks.push(check(
        "config_managed",
        health.managed,
        if health.managed { "config.toml 已由 ReCodex 托管" } else { "config.toml 没有托管块 —— Codex 走的是官方 provider，不经过 ReCodex" },
    ));
    checks.push(check(
        "config_top_level",
        !health.managed || health.before_first_table,
        if health.before_first_table { "托管块在第一个表头之前" } else { "托管块排在表头之后 —— 顶层 model_provider 被归给上面那张表，等于没设" },
    ));
    checks.push(check(
        "config_no_duplicate",
        health.top_level_model_provider <= 1,
        match health.top_level_model_provider {
            0 | 1 => "顶层 model_provider 唯一",
            _ => "顶层 model_provider 重复 —— 整份 config.toml 都解析不了",
        },
    ));
    health.is_healthy()
}

/// 本地自诊断。返回 `status: ready`，`data.checks` 是逐项结果，
/// `data.fixable` 表示「重装配置」这一步能不能修好当前问题。
pub fn recodex_doctor(state: &ReCodexState) -> Value {
    let mut checks = Vec::new();
    let mut gateway_switchable = false;
    let config_ok = doctor_config_checks(&mut checks);

    let key = std::env::var(crate::codexcfg::SUB2API_ENV_KEY).unwrap_or_default();
    let key_ok = !key.trim().is_empty();
    checks.push(check(
        "credential",
        key_ok,
        if key_ok { "Codex 能读到 ReCodex 凭据" } else { "环境变量里没有 ReCodex 凭据 —— Codex 会被网关拒掉" },
    ));

    // 有凭据不等于凭据能用。被后台停用、或重新登录时被服务端换掉的 key,
    // 本地看起来一模一样 —— 只有网关说了算。这是全部检查里唯一一项联网的:
    // 它诊断的正是「本地全绿、网关却一直 401」(2026-09-04 两个用户同一天撞上)。
    let mut key_rejected = false;
    let mut gateway_ok = true;
    if key_ok {
        if let Some(root) = gateway_root_from_managed_config() {
            match probe_gateway_key(&root, key.trim()) {
                KeyProbe::Accepted => {
                    checks.push(check("credential_accepted", true, "网关确认凭据有效"));
                }
                KeyProbe::Rejected(detail) => {
                    key_rejected = true;
                    checks.push(check("credential_accepted", false, &detail));
                }
                KeyProbe::Unreachable(detail) => {
                    gateway_ok = false;
                    // 光说「连不上」帮不上忙 —— 用户不知道是自己网络的事，
                    // 还是恰好被分到了一条坏线路上。顺手在本机测一遍所有网关，
                    // 有能用的就直接说「切到 X 就行」，这是他自己点一下就能修的。
                    let alternative = reachable_gateway_hint(state);
                    let detail = match &alternative {
                        Some(hint) => format!("{detail}；{hint}"),
                        None => detail.clone(),
                    };
                    gateway_switchable = alternative.is_some();
                    checks.push(check("gateway_reachable", false, &detail));
                }
            }
        }
    }

    // Codex 只在启动时读一次 RECODEX_KEY。登录 / 换组织之后新 key 只到了本进程,
    // 正在跑的 Codex 攥着的还是旧值 —— 而旧 key 在服务端已经作废。
    // 这种情况什么都不用修,只需要重启;不单列出来的话,它和「凭据坏了」
    // 在用户眼里是同一个 401,于是反复重新登录、反复换 key、反复失败。
    let restart_pending = crate::codexcfg::key_changed_since_start();
    checks.push(check(
        "codex_restarted",
        !restart_pending,
        if restart_pending { "凭据已更新,但正在运行的 Codex 仍拿着旧凭据 —— 请重启客户端" } else { "Codex 使用的是当前凭据" },
    ));

    let signed_in = match state.adapter.lock() {
        Ok(guard) => guard.as_ref().is_some_and(|a| a.is_authenticated()),
        Err(_) => return error("state_unavailable", "ReCodex state is unavailable"),
    };
    checks.push(check(
        "session",
        signed_in,
        if signed_in { "已登录 ReCodex" } else { "未登录 —— 请先登录，自动修复需要它" },
    ));

    json!({
        "status": "ready",
        "data": {
            "healthy": config_ok && key_ok && !key_rejected && gateway_ok && !restart_pending && signed_in,
            // 「重装配置」能修好的只有托管块那几项，且必须先登录才取得到。
            "fixable": !config_ok && signed_in,
            // 凭据是另一回事：服务端**刻意只在登录时交付一次**
            // （server_authed.go handleConfig 的注释写得很清楚），
            // 重装配置拿不回被清掉的 key。这时候唯一的出路是重新登录 ——
            // 不如实说的话，用户会一直点「自动修复」而问题纹丝不动。
            // 网关明确拒掉的 key 同理：停用 / 换发过的 key 重装也回不来。
            "needs_relogin": !key_ok || key_rejected,
            // 只差一次重启：面板据此给「立即重启」而不是「重新登录」。
            "needs_restart": restart_pending,
            // 当前网关不通、但本机测出别的网关能用：面板据此给「切到最快网关」。
            // 这条独立于 fixable —— 重装配置改不了「这条线路本身不通」。
            "gateway_switchable": gateway_switchable,
            "checks": checks,
        }
    })
}

/// 当前网关不通时，在本机测一遍所有网关，给出可切换的建议。
///
/// 用本机实测而不是服务端给的延迟：服务端那份是它自己机房到网关的距离，
/// 与用户所在网络无关（线上后台显示新加坡「30ms 最快」，而国内实测 230ms、
/// 40% 丢包）。拿不到清单或一个都不通时返回 None —— 那说明是用户整体网络
/// 的问题，建议切网关只会误导他。
fn reachable_gateway_hint(state: &ReCodexState) -> Option<String> {
    let guard = state.adapter.lock().ok()?;
    let adapter = guard.as_ref()?;
    let gateways = adapter.gateways().ok()?;
    let candidates: Vec<crate::Gateway> = gateways
        .into_iter()
        .filter(|g| g.enabled && !g.maintenance && !g.endpoint.trim().is_empty())
        .collect();
    if candidates.is_empty() {
        return None;
    }
    let probes = crate::gateway_probe::probe_gateways(&candidates);
    let best = crate::gateway_probe::fastest_reachable(&probes)?;
    Some(format!(
        "本机测下来「{}」可用（{}ms），可以切过去试试",
        best.gateway.name.trim(),
        best.latency_ms
    ))
}

/// 网关对这把 key 的态度。
enum KeyProbe {
    Accepted,
    /// 网关明确拒绝,附用户可读的原因。
    Rejected(String),
    /// 没问到网关(网络 / 5xx),不能据此说凭据坏了。
    Unreachable(String),
}

/// 托管块里的 base_url 去掉 `/backend-api/codex` 就是网关根。
fn gateway_root(base_url: &str) -> String {
    base_url
        .trim_end_matches('/')
        .trim_end_matches("/backend-api/codex")
        .to_string()
}

fn gateway_root_from_managed_config() -> Option<String> {
    let content = crate::codexcfg::config_path()
        .and_then(std::fs::read_to_string)
        .ok()?;
    crate::codexcfg::managed_base_url(&content).map(|base| gateway_root(&base))
}

/// 拿这把 key 打一次网关上最便宜的鉴权接口。只问「认不认」,不拉模型、不碰上游。
fn probe_gateway_key(root: &str, key: &str) -> KeyProbe {
    let agent = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(5))
        .build();
    match agent
        .get(&format!("{root}/v1/key/billing"))
        .set("Authorization", &format!("Bearer {key}"))
        .call()
    {
        Ok(response) => classify_key_probe(response.status(), ""),
        Err(ureq::Error::Status(status, response)) => {
            let body = response.into_string().unwrap_or_default();
            classify_key_probe(status, &body)
        }
        Err(ureq::Error::Transport(transport)) => {
            KeyProbe::Unreachable(format!("连不上网关,无法确认凭据:{transport}"))
        }
    }
}

/// 把网关的应答翻成诊断结论。401 的 `code` 是 sub2api 鉴权中间件定死的那几个值。
fn classify_key_probe(status: u16, body: &str) -> KeyProbe {
    let code = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| value.get("code").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default();
    match (status, code.as_str()) {
        (200..=299, _) => KeyProbe::Accepted,
        (401, "API_KEY_DISABLED") => {
            KeyProbe::Rejected("这把凭据已被停用 —— 请重新登录获取新凭据".to_string())
        }
        (401, "INVALID_API_KEY") => KeyProbe::Rejected(
            "这把凭据已失效(不存在或已被更换)—— 请重新登录获取新凭据".to_string(),
        ),
        (401, _) | (403, _) => KeyProbe::Rejected(format!(
            "网关不接受这把凭据({status} {code})—— 请重新登录;仍失败请联系客服"
        )),
        // simple 模式没有计费接口,但能走到 handler 就说明鉴权已经过了
        (404, _) => KeyProbe::Accepted,
        _ => KeyProbe::Unreachable(format!("网关返回 {status},暂时无法确认凭据")),
    }
}

/// 自动修复：把服务端当前应下发的托管块与凭据重新装回去。
///
/// 走的是**登录那一个写入口**（install_login_config），不是另开一条：
/// 官方模式快照、顶层 model_provider 接管这些策略都写在那里，绕开就会漂移。
pub fn recodex_doctor_fix(state: &ReCodexState) -> Value {
    let worker = match state.adapter.lock() {
        Ok(guard) => match guard.as_ref() {
            Some(adapter) if adapter.is_authenticated() => adapter.fork(),
            Some(_) => return json!({"status":"signed_out"}),
            None => return error("configuration", "ReCodex is not configured"),
        },
        Err(_) => return error("state_unavailable", "ReCodex state is unavailable"),
    };
    let managed = match worker.managed_config() {
        Ok(value) => value,
        Err(adapter_error) => return adapter_failure("doctor", &adapter_error),
    };
    if managed.config.trim().is_empty() {
        return error("doctor", "服务端没有下发配置，无法重装");
    }
    if let Err(io_error) = install_login_config(
        &managed.config,
        &managed.auth_json,
        &managed.env_key,
        &managed.env_value,
    ) {
        return error("doctor", io_error.to_string());
    }
    // 重装完立刻复检，让面板显示的是修完之后的真实状态，而不是「已修复」的一句空话。
    recodex_doctor(state)
}

/// CDP 桥分发器:把 /recodex/* 路径映射到命令实现。由 launcher 的 RecodexBridge impl 调用(持 ReCodexState)。
pub fn handle_bridge(state: &ReCodexState, path: &str, payload: &Value) -> Value {
    match path {
        "/recodex/status" => recodex_status(state),
        "/recodex/refresh-usage" => recodex_refresh_usage(state),
        "/recodex/refresh-token" => recodex_refresh_token(state),
        "/recodex/check-client" => recodex_check_client(state),
        "/recodex/doctor" => recodex_doctor(state),
        "/recodex/doctor/fix" => recodex_doctor_fix(state),
        "/recodex/report-diagnostics" => recodex_report_diagnostics(state),
        "/recodex/login/start" => recodex_login_start(state),
        "/recodex/login/poll" => recodex_login_poll(state),
        "/recodex/gateway/select" => match payload.get("id").and_then(Value::as_str) {
            Some(id) => recodex_select_gateway(state, id.to_owned()),
            None => error("invalid", "gateway id is required"),
        },
        "/recodex/gateway/fastest" => recodex_use_fastest_gateway(state),
        "/recodex/org/list" => recodex_organizations(state),
        "/recodex/org/switch" => match payload.get("org_id").and_then(Value::as_i64) {
            Some(id) => recodex_switch_org(state, id),
            None => error("invalid", "organization id is required"),
        },
        "/recodex/quota/reset" => recodex_reset_quota(state),
        "/recodex/logout" => recodex_logout(state),
        "/recodex/official-mode" => recodex_official_mode_status(),
        "/recodex/official-mode/enable" => recodex_official_mode_enable(),
        "/recodex/official-mode/disable" => recodex_official_mode_disable(),
        "/recodex/prepare-uninstall" => recodex_prepare_uninstall(state),
        _ => error("not_found", format!("unknown recodex path: {path}")),
    }
}

#[cfg(test)]
mod config_writer_tests {
    use super::{classify_key_probe, gateway_root, KeyProbe};

    /// 写 `~/.codex` 的入口必须只有两个,而且各自带着官方模式的策略。
    ///
    /// 这条约束靠人记是记不住的 —— 已经因此踩过四次(快照漏 auth.json /
    /// 卸载时被还原回来 / 登出不清快照 / 换网关破坏模式)。所以直接读自己的源码断言:
    /// `apply_login` 和 `route_through_gateway` 各自只能出现一次,
    /// 也就是只能待在 `install_login_config` 和 `route_codex_through_gateway` 里面。
    /// 谁想加第三个写入口,这条测试会先红给他看。
    /// 正文(剔注释、截断测试模块)—— 否则守卫会把注释和自己的断言算进去。
    fn body() -> String {
        let source = include_str!("desktop.rs");
        let source = source
            .split("mod config_writer_tests")
            .next()
            .expect("测试模块之前的正文");
        source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("
")
    }

    /// 网关 401 的几种 code 要翻成不同的话,而且都得是「回登录」;
    /// 网络问题不能被说成凭据坏了 —— 否则用户会对着好好的凭据反复重登。
    #[test]
    fn key_probe_classifies_gateway_answers() {
        assert!(matches!(classify_key_probe(200, ""), KeyProbe::Accepted));
        assert!(matches!(classify_key_probe(404, ""), KeyProbe::Accepted));
        match classify_key_probe(401, r#"{"code":"API_KEY_DISABLED","message":"x"}"#) {
            KeyProbe::Rejected(detail) => assert!(detail.contains("停用"), "{detail}"),
            _ => panic!("停用的 key 必须判为拒绝"),
        }
        match classify_key_probe(401, r#"{"code":"INVALID_API_KEY","message":"x"}"#) {
            KeyProbe::Rejected(detail) => assert!(detail.contains("失效"), "{detail}"),
            _ => panic!("不存在的 key 必须判为拒绝"),
        }
        assert!(matches!(classify_key_probe(401, "not json"), KeyProbe::Rejected(_)));
        assert!(matches!(classify_key_probe(502, ""), KeyProbe::Unreachable(_)));
    }

    #[test]
    fn gateway_root_strips_codex_suffix() {
        assert_eq!(
            gateway_root("https://sg.gw.recodex.dev/backend-api/codex/"),
            "https://sg.gw.recodex.dev"
        );
    }

    /// 自诊断必须真的问网关、真的看重启标志 —— 这两项是 2026-09-04 两个用户
    /// 「本地全绿、网关一直 401」的根因,拿掉任何一个,上面的单元测试照样绿。
    #[test]
    fn doctor_consults_gateway_and_restart_flag() {
        let doctor = body()
            .split("pub fn recodex_doctor(")
            .nth(1)
            .expect("recodex_doctor 应存在")
            .to_string();
        let doctor = &doctor[..doctor.find("\npub fn ").unwrap_or(doctor.len())];
        assert!(doctor.contains("probe_gateway_key("), "自诊断必须拿 key 问网关");
        assert!(doctor.contains("key_changed_since_start()"), "自诊断必须区分「只差重启」");
        assert!(
            doctor.contains("\"needs_restart\": restart_pending"),
            "面板靠 needs_restart 给「立即重启」"
        );
        assert!(
            doctor.contains("\"needs_relogin\": !key_ok || key_rejected"),
            "网关拒掉的 key 必须走「重新登录」—— 重装配置拿不回被停用 / 换发过的 key"
        );
    }

    /// 官方模式下换网关只能改快照。把这个判断拿掉,官方模式会被悄悄破坏 ——
    /// 而 round-trip 测试直接调 `stage_config_for_return`,拿掉调用它照样绿(实测过)。
    #[test]
    fn gateway_change_consults_official_mode() {
        assert!(
            body().contains("officialmode::stage_config_for_return(&block)"),
            "换网关必须先问官方模式,否则会把托管块悄悄写进活配置"
        );
    }

    /// 登出与卸载都必须**丢弃**快照(不是还原)。
    /// 同样地,round-trip 测试直接调 `discard_snapshot`,把这两个调用删掉照样绿。
    #[test]
    fn logout_and_uninstall_discard_the_official_mode_snapshot() {
        let body = body();
        assert_eq!(
            body.matches("officialmode::discard_snapshot()").count(),
            3,
            "登录/登出/卸载各一处 —— 少一处就会留下会覆盖新配置的陈旧快照"
        );
        // 注意范围:`switch_to_recodex()` 本身是正当的 —— 面板「切回 ReCodex」就靠它。
        // 不能一刀切禁掉,只能盯住**卸载准备**这个函数:那里用它会把登出刚撤掉的
        // 托管块重新装回去,程序随即自删。
        let prepare = body
            .split("pub fn recodex_prepare_uninstall")
            .nth(1)
            .expect("recodex_prepare_uninstall 应存在");
        let prepare = &prepare[..prepare.find("
pub fn ").unwrap_or(prepare.len())];
        assert!(
            !prepare.contains("switch_to_recodex()"),
            "卸载准备里用 switch_to_recodex 会把配置重新装回去"
        );
        assert!(
            prepare.contains("discard_snapshot()"),
            "卸载准备必须丢弃快照"
        );
    }

    /// 401 只能翻成「回登录」,而且这条规则要**留在唯一一处**。
    ///
    /// 新加一个带凭据的入口时顺手写 `error(code, adapter_error.to_string())`,
    /// 设备被撤销后面板就又会停在「重试」上再也走不到登录 —— 1.2.53 原样复发,
    /// 而且这种回归任何行为测试都照不到(它只在真被撤销时才显形)。
    #[test]
    fn unauthorized_always_routes_back_to_login() {
        use super::{adapter_failure, AdapterError};
        use serde_json::json;

        assert_eq!(
            json!({"status":"signed_out","notice":"x"})["status"],
            adapter_failure("usage", &AdapterError::Unauthorized)["status"],
            "带凭据的入口遇到 401 必须回 signed_out,面板才会画「登录 ReCodex」"
        );
        assert_eq!(
            adapter_failure("usage", &AdapterError::RateLimited)["status"],
            "error",
            "其余错误仍是可重试的普通错误,别一并吞成登出"
        );

        // 允许出现的三处:`adapter_failure` 自己,以及两个**不带凭据**的登录入口
        // (login/start、login/poll —— 那里的 401 另有含义,不该把用户登出)。
        let body = body();
        let sites = body.matches(", adapter_error.to_string())").count();
        assert_eq!(
            sites, 3,
            "带凭据的入口不得自己拼错误信息,请改用 adapter_failure(code, &err);             当前直接拼装的地方有 {sites} 处(只允许 adapter_failure 自身 + 两个登录入口)"
        );
    }

    #[test]
    fn config_writers_are_centralised() {
        let body = body();

        for symbol in ["crate::codexcfg::apply_login(", "crate::codexcfg::route_through_gateway("] {
            let count = body.matches(symbol).count();
            assert_eq!(
                count, 1,
                "{symbol} 只应出现在唯一的受管写入口里,实际出现 {count} 次 —— \
                 新增写入口请走 install_login_config / route_codex_through_gateway,\
                 它们各自处理了官方模式"
            );
        }

        assert!(
            body.contains("fn install_login_config"),
            "登录写入口应保留具名函数,策略写在它的文档注释里"
        );
    }
}
