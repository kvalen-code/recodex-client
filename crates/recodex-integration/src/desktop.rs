//! ReCodex desktop bridge：ReCodexState + 命令实现,从 manager 的 Tauri IPC 迁进本 crate,
//! 供 launcher 的 CDP 桥调用(不再依赖 manager app)。recodex-overlay 核心,逻辑照搬。
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex,
};
use std::sync::{MutexGuard, TryLockError};

use crate::{
    credential::{credential_target_for_api_url, CredentialStore, PlatformCredentialStore},
    Adapter, DiagnosticReport, HttpTransport, PublicLoginStart,
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

impl ReCodexState {
    pub fn from_env() -> Self {
        let api_url = std::env::var("RECODEX_API_URL")
            .unwrap_or_else(|_| "https://api.recodex.dev".to_owned());
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
            },
        }
    }
}

fn error(code: &str, message: impl Into<String>) -> Value {
    json!({"status":"error", "error":{"code":code,"message":message.into()}})
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

fn snapshot_epoch_is_current(started_epoch: u64, current_epoch: u64) -> bool {
    started_epoch == current_epoch
}

fn snapshot(state: &ReCodexState, refresh: bool) -> Value {
    let _snapshot_guard = match try_snapshot_lock(&state.snapshot_lock) {
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
    let ((usage, usage_worker), account, gateways) = parallel_snapshot_requests(
        move || worker.usage_in_fork(refresh),
        move || account_worker.account(),
        move || gateway_worker.gateways(),
    );
    let usage = match usage {
        Ok(value) => value,
        Err(adapter_error) => return error("usage", adapter_error.to_string()),
    };
    let account_error = account.as_ref().err().map(ToString::to_string);
    let gateway_error = gateways.as_ref().err().map(ToString::to_string);
    let gateways = gateways.unwrap_or_default();
    let selected = gateways.iter().find(|gateway| gateway.selected).cloned();
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
    json!({"status": if stale { "stale" } else { "ready" }, "data":{"account":account.ok(),"usage":usage,"gateways":gateways,"selected_gateway":selected,"account_error":account_error,"gateway_error":gateway_error}})
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
        Err(adapter_error) => return error("refresh", adapter_error.to_string()),
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
        Err(adapter_error) => return error("compatibility", adapter_error.to_string()),
    };
    if !authenticated {
        return json!({"status":"signed_out", "data":{"compatibility":compatibility}});
    }
    let update_channel = match adapter.update_channel("stable") {
        Ok(value) => value,
        Err(adapter_error) => return error("update", adapter_error.to_string()),
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
        occurred_at: None,
    };
    match adapter.report_diagnostic(&report) {
        Ok(value) => json!({"status":"ready", "data":{"diagnostics":value}}),
        Err(adapter_error) => error("diagnostics", adapter_error.to_string()),
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
        Err(adapter_error) => error("gateway", adapter_error.to_string()),
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
        Err(adapter_error) => error("gateway", adapter_error.to_string()),
    }
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
            json!({"status":"pending", "data":PublicLoginStart::from(&login)})
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
            json!({"status":"approved"})
        }
        Ok(result) => json!({"status":result.status}),
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

/// CDP 桥分发器:把 /recodex/* 路径映射到命令实现。由 launcher 的 RecodexBridge impl 调用(持 ReCodexState)。
pub fn handle_bridge(state: &ReCodexState, path: &str, payload: &Value) -> Value {
    match path {
        "/recodex/status" => recodex_status(state),
        "/recodex/refresh-usage" => recodex_refresh_usage(state),
        "/recodex/refresh-token" => recodex_refresh_token(state),
        "/recodex/check-client" => recodex_check_client(state),
        "/recodex/report-diagnostics" => recodex_report_diagnostics(state),
        "/recodex/login/start" => recodex_login_start(state),
        "/recodex/login/poll" => recodex_login_poll(state),
        "/recodex/gateway/select" => match payload.get("id").and_then(Value::as_str) {
            Some(id) => recodex_select_gateway(state, id.to_owned()),
            None => error("invalid", "gateway id is required"),
        },
        "/recodex/gateway/fastest" => recodex_use_fastest_gateway(state),
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
