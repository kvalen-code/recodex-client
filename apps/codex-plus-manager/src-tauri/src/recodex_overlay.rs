// ReCodex overlay command bridge. The implementation lives in the copied
// recodex-integration adapter crate; this file only owns Tauri IPC plumbing.
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex,
};
use std::sync::{MutexGuard, TryLockError};

use recodex_integration::{
    credential::{credential_target_for_api_url, CredentialStore, WindowsCredentialStore},
    Adapter, DiagnosticReport, HttpTransport, PublicLoginStart,
};
use serde_json::{json, Value};
use tauri::menu::{Menu, MenuItem};
use tauri::{Manager, Runtime};

const TRAY_MENU_SHOW: &str = "tray_show_main";
const TRAY_MENU_DREAM_SKIN_APPLY: &str = "tray_apply_dream_skin";
const TRAY_MENU_QUIT: &str = "tray_quit_app";
pub const TRAY_MENU_RECODEX_LOGIN: &str = "tray_recodex_login";
pub const TRAY_MENU_RECODEX_REFRESH: &str = "tray_recodex_refresh";
pub const TRAY_MENU_RECODEX_FASTEST: &str = "tray_recodex_fastest";
pub const TRAY_MENU_RECODEX_UPDATE: &str = "tray_recodex_update";
pub const TRAY_MENU_RECODEX_DIAGNOSTICS: &str = "tray_recodex_diagnostics";

pub fn build_tray_menu<R: Runtime, M: Manager<R>>(
    manager: &M,
    show_label: &str,
    apply_skin_label: &str,
    recodex_login_label: &str,
    recodex_refresh_label: &str,
    recodex_fastest_label: &str,
    recodex_update_label: &str,
    recodex_diagnostics_label: &str,
    quit_label: &str,
) -> tauri::Result<Menu<R>> {
    let show = MenuItem::with_id(manager, TRAY_MENU_SHOW, show_label, true, None::<&str>)?;
    let apply_skin = MenuItem::with_id(
        manager,
        TRAY_MENU_DREAM_SKIN_APPLY,
        apply_skin_label,
        true,
        None::<&str>,
    )?;
    let login = MenuItem::with_id(
        manager,
        TRAY_MENU_RECODEX_LOGIN,
        recodex_login_label,
        true,
        None::<&str>,
    )?;
    let refresh = MenuItem::with_id(
        manager,
        TRAY_MENU_RECODEX_REFRESH,
        recodex_refresh_label,
        true,
        None::<&str>,
    )?;
    let fastest = MenuItem::with_id(
        manager,
        TRAY_MENU_RECODEX_FASTEST,
        recodex_fastest_label,
        true,
        None::<&str>,
    )?;
    let update = MenuItem::with_id(
        manager,
        TRAY_MENU_RECODEX_UPDATE,
        recodex_update_label,
        true,
        None::<&str>,
    )?;
    let diagnostics = MenuItem::with_id(
        manager,
        TRAY_MENU_RECODEX_DIAGNOSTICS,
        recodex_diagnostics_label,
        true,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(manager, TRAY_MENU_QUIT, quit_label, true, None::<&str>)?;
    Menu::with_items(
        manager,
        &[
            &show,
            &apply_skin,
            &login,
            &refresh,
            &fastest,
            &update,
            &diagnostics,
            &quit,
        ],
    )
}

pub fn tray_action(id: &str) -> Option<&'static str> {
    match id {
        TRAY_MENU_RECODEX_LOGIN => Some("login"),
        TRAY_MENU_RECODEX_REFRESH => Some("refresh"),
        TRAY_MENU_RECODEX_FASTEST => Some("fastest"),
        TRAY_MENU_RECODEX_UPDATE => Some("update"),
        TRAY_MENU_RECODEX_DIAGNOSTICS => Some("diagnostics"),
        _ => None,
    }
}

#[cfg(test)]
mod tray_action_tests {
    use super::*;

    #[test]
    fn maps_only_recodex_tray_action_ids() {
        assert_eq!(tray_action(TRAY_MENU_RECODEX_LOGIN), Some("login"));
        assert_eq!(tray_action(TRAY_MENU_RECODEX_REFRESH), Some("refresh"));
        assert_eq!(tray_action(TRAY_MENU_RECODEX_FASTEST), Some("fastest"));
        assert_eq!(tray_action(TRAY_MENU_RECODEX_UPDATE), Some("update"));
        assert_eq!(
            tray_action(TRAY_MENU_RECODEX_DIAGNOSTICS),
            Some("diagnostics")
        );
        assert_eq!(tray_action("tray_recodex_unknown"), None);
        assert_eq!(tray_action(TRAY_MENU_SHOW), None);
        assert_eq!(tray_action(""), None);
    }
}

#[cfg(test)]
mod snapshot_parallel_tests {
    use super::*;
    use std::sync::{mpsc, Arc, Barrier};
    use std::time::Duration;

    #[test]
    fn runs_independent_snapshot_requests_concurrently() {
        let (started_tx, started_rx) = mpsc::channel();
        let release = Arc::new(Barrier::new(4));
        let usage_tx = started_tx.clone();
        let account_tx = started_tx.clone();
        let gateways_tx = started_tx;
        let usage_release = Arc::clone(&release);
        let account_release = Arc::clone(&release);
        let gateways_release = Arc::clone(&release);
        let worker = std::thread::spawn(move || {
            parallel_snapshot_requests(
                move || {
                    usage_tx.send("usage").unwrap();
                    usage_release.wait();
                    1
                },
                move || {
                    account_tx.send("account").unwrap();
                    account_release.wait();
                    2
                },
                move || {
                    gateways_tx.send("gateways").unwrap();
                    gateways_release.wait();
                    3
                },
            )
        });
        for _ in 0..3 {
            started_rx.recv_timeout(Duration::from_millis(100)).unwrap();
        }
        release.wait();
        assert_eq!(worker.join().unwrap(), (1, 2, 3));
    }

    #[test]
    fn snapshot_lock_rejects_overlapping_requests_without_waiting() {
        let lock = Mutex::new(());
        let held = lock.lock().unwrap();
        let result = try_snapshot_lock(&lock);
        assert!(result.is_err());
        drop(held);
        assert!(try_snapshot_lock(&lock).is_ok());
    }

    #[test]
    fn snapshot_epoch_rejects_data_collected_before_logout() {
        let epoch = AtomicU64::new(7);
        let started_epoch = epoch.load(Ordering::SeqCst);
        epoch.fetch_add(1, Ordering::SeqCst);

        assert!(!snapshot_epoch_is_current(
            started_epoch,
            epoch.load(Ordering::SeqCst)
        ));
    }
}

pub fn open_recodex_action<R: Runtime>(app: &tauri::AppHandle<R>, action: &str) {
    let script = match action {
        "login" => "window.location.hash = '#recodex-login'; window.location.reload();",
        "refresh" => "window.location.hash = '#recodex-refresh'; window.location.reload();",
        "fastest" => "window.location.hash = '#recodex-fastest'; window.location.reload();",
        "update" => "window.location.hash = '#recodex-update'; window.location.reload();",
        "diagnostics" => "window.location.hash = '#recodex-diagnostics'; window.location.reload();",
        _ => return,
    };
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        let _ = window.eval(script);
    }
}

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
                    recodex_integration::AdapterError::InvalidConfiguration(
                        "invalid credential origin".into(),
                    )
                })?;
                let credentials = WindowsCredentialStore::new(target).map_err(|_| {
                    recodex_integration::AdapterError::InvalidConfiguration(
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
    recodex_integration::codexcfg::route_through_gateway(&base)
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

#[tauri::command]
pub fn recodex_status(state: tauri::State<'_, ReCodexState>) -> Value {
    snapshot(&state, false)
}

#[tauri::command]
pub fn recodex_refresh_usage(state: tauri::State<'_, ReCodexState>) -> Value {
    snapshot(&state, true)
}

#[tauri::command]
pub fn recodex_refresh_token(state: tauri::State<'_, ReCodexState>) -> Value {
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
    let secret = match recodex_integration::credential::Secret::new(token.clone()) {
        Ok(value) => value,
        Err(_) => return error("credential_store", "Refreshed token is invalid"),
    };
    if state.auth_epoch.load(Ordering::SeqCst) != epoch {
        return error("refresh_cancelled", "ReCodex refresh was cancelled");
    }
    if state.credentials.as_ref().is_none_or(|store| {
        store
            .save(recodex_integration::credential::StoredCredentials {
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

#[tauri::command]
pub fn recodex_check_client(state: tauri::State<'_, ReCodexState>) -> Value {
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

#[tauri::command]
pub fn recodex_report_diagnostics(state: tauri::State<'_, ReCodexState>) -> Value {
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
        Err(adapter_error) => error("diagnostics", adapter_error.to_string()),
    }
}

#[tauri::command]
pub fn recodex_organizations(state: tauri::State<'_, ReCodexState>) -> Value {
    recodex_integration::desktop::recodex_organizations(&state)
}

/// 把这台设备切到目标组织。
///
/// 逻辑全在 recodex_integration::desktop 里 —— 这里只做 Tauri 包装。
/// 那边有测试覆盖(tests/org_switch.rs)，把逻辑抄一份到这里就等于抄一份
/// **没人测的**代码:两边迟早分叉，而分叉的症状是「CLI 能切、桌面端不能」。
///
/// 网关 Key 不会出现在返回值里 —— 它只在进程内用于写用户环境。
#[tauri::command]
pub fn recodex_switch_org(state: tauri::State<'_, ReCodexState>, org_id: i64) -> Value {
    recodex_integration::desktop::recodex_switch_org(&state, org_id)
}

#[tauri::command]
pub fn recodex_select_gateway(state: tauri::State<'_, ReCodexState>, id: String) -> Value {
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

#[tauri::command]
pub fn recodex_use_fastest_gateway(state: tauri::State<'_, ReCodexState>) -> Value {
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

#[tauri::command]
pub fn recodex_login_start(state: tauri::State<'_, ReCodexState>) -> Value {
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
    let device_id = match recodex_integration::load_or_create_install_id() {
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

#[tauri::command]
pub fn recodex_login_poll(state: tauri::State<'_, ReCodexState>) -> Value {
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
            let secret = match recodex_integration::credential::Secret::new(token.clone()) {
                Ok(value) => value,
                Err(_) => return error("credential_store", "Approved token is invalid"),
            };
            let credentials = recodex_integration::credential::StoredCredentials {
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
            if let Err(config_error) = recodex_integration::codexcfg::apply_login(
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

#[tauri::command]
pub fn recodex_logout(state: tauri::State<'_, ReCodexState>) -> Value {
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
    let _ = recodex_integration::codexcfg::restore_all();
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
