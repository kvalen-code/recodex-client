//! ReCodex desktop bridge：ReCodexState + 命令实现,从 manager 的 Tauri IPC 迁进本 crate,
//! 供 launcher 的 CDP 桥调用(不再依赖 manager app)。recodex-overlay 核心,逻辑照搬。
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex,
};
use std::sync::{MutexGuard, TryLockError};

use crate::{
    credential::{credential_target_for_api_url, CredentialStore, WindowsCredentialStore},
    Adapter, DiagnosticReport, HttpTransport, PublicLoginStart,
};
use serde_json::{json, Value};

pub struct ReCodexState {
    adapter: Mutex<Option<Adapter<HttpTransport>>>,
    credentials: Option<WindowsCredentialStore>,
    refresh_lock: Mutex<()>,
    snapshot_lock: Mutex<()>,
    pending_device_code: Mutex<Option<String>>,
    auth_epoch: AtomicU64,
    init_error: Option<String>,
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
                let credentials = WindowsCredentialStore::new(target).map_err(|_| {
                    crate::AdapterError::InvalidConfiguration(
                        "invalid credential target".into(),
                    )
                })?;
                Ok((adapter, credentials))
            });
        match result {
            Ok((mut adapter, credentials)) => {
                if let Ok(Some(saved)) = credentials.load() {
                    let _ = adapter.set_access_token(saved.access_token.expose().to_owned());
                }
                Self {
                    adapter: Mutex::new(Some(adapter)),
                    credentials: Some(credentials),
                    refresh_lock: Mutex::new(()),
                    snapshot_lock: Mutex::new(()),
                    pending_device_code: Mutex::new(None),
                    auth_epoch: AtomicU64::new(0),
                    init_error: None,
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
fn route_codex_through_gateway(endpoint: &str) -> Option<String> {
    let endpoint = endpoint.trim().trim_end_matches('/');
    if endpoint.is_empty() {
        return None;
    }
    let base = format!("{endpoint}/backend-api/codex");
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
        return json!({"status":"signed_out"});
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
            if let Err(config_error) = crate::codexcfg::apply_login(
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
    let _ = crate::officialmode::switch_to_recodex();
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
