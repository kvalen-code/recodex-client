//! recodex-overlay: 微信连接的控制面(扫码/启停/状态),从 manager 的 Tauri 命令迁到 core,
//! 供 launcher 的 CDP 桥 `/weixin/*` 调用。逻辑照搬 manager,仅把 tauri::async_runtime::spawn
//! 换成 tokio::spawn,返回值换成 JSON(桥友好)。
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use serde_json::{Value, json};

use super::{
    DEFAULT_WEIXIN_BASE_URL, SharedWeixinConnectStatus, WeixinConnectConfig, WeixinConnectStatus,
    run_weixin_connect,
    weixin::{WeixinClient, render_qr_svg},
};
use crate::settings::{BackendSettings, SettingsStore};

struct QrSession {
    base_url: String,
    route_tag: String,
    qr_code: String,
    qr_content: String,
    qr_svg: String,
}

struct WeixinRuntime {
    stop: Arc<AtomicBool>,
}

fn qr_session() -> &'static Mutex<Option<QrSession>> {
    static SESSION: OnceLock<Mutex<Option<QrSession>>> = OnceLock::new();
    SESSION.get_or_init(|| Mutex::new(None))
}

fn runtime_slot() -> &'static Mutex<Option<WeixinRuntime>> {
    static RUNTIME: OnceLock<Mutex<Option<WeixinRuntime>>> = OnceLock::new();
    RUNTIME.get_or_init(|| Mutex::new(None))
}

fn shared_status() -> SharedWeixinConnectStatus {
    static STATUS: OnceLock<SharedWeixinConnectStatus> = OnceLock::new();
    Arc::clone(STATUS.get_or_init(|| Arc::new(Mutex::new(WeixinConnectStatus::default()))))
}

fn current_status() -> WeixinConnectStatus {
    shared_status()
        .lock()
        .map(|status| status.clone())
        .unwrap_or_default()
}

fn status_value(status: &str, message: &str) -> Value {
    let current = current_status();
    json!({
        "status": status,
        "message": message,
        "connect": serde_json::to_value(&current).unwrap_or(Value::Null),
    })
}

fn qr_payload(
    status: &str,
    message: &str,
    qr_status: &str,
    qr_content: &str,
    qr_svg: &str,
    account_id: &str,
    linked_user_id: &str,
    has_token: bool,
) -> Value {
    json!({
        "status": status,
        "message": message,
        "qrStatus": qr_status,
        "qrContent": qr_content,
        "qrSvg": qr_svg,
        "accountId": account_id,
        "linkedUserId": linked_user_id,
        "hasToken": has_token,
    })
}

/// 生成微信登录二维码,并记住本次扫码会话(供 `qr_status` 轮询)。
pub async fn qr_start(base_url: &str, route_tag: &str) -> Value {
    match WeixinClient::fetch_qr_code(base_url, route_tag).await {
        Ok(qr) => {
            let qr_svg = render_qr_svg(&qr.qr_content).unwrap_or_default();
            let normalized_base = if base_url.trim().is_empty() {
                DEFAULT_WEIXIN_BASE_URL.to_string()
            } else {
                base_url.trim().trim_end_matches('/').to_string()
            };
            if let Ok(mut current) = qr_session().lock() {
                *current = Some(QrSession {
                    base_url: normalized_base,
                    route_tag: route_tag.trim().to_string(),
                    qr_code: qr.qr_code,
                    qr_content: qr.qr_content.clone(),
                    qr_svg: qr_svg.clone(),
                });
            }
            qr_payload(
                "ok",
                "微信登录二维码已生成。",
                "wait",
                &qr.qr_content,
                &qr_svg,
                "",
                "",
                false,
            )
        }
        Err(error) => qr_payload(
            "failed",
            &format!("生成微信登录二维码失败：{error}"),
            "failed",
            "",
            "",
            "",
            "",
            false,
        ),
    }
}

/// 轮询扫码状态;确认后把网关下发的凭据写进设置。
pub async fn qr_status() -> Value {
    let session = qr_session().lock().ok().and_then(|current| {
        current.as_ref().map(|session| QrSession {
            base_url: session.base_url.clone(),
            route_tag: session.route_tag.clone(),
            qr_code: session.qr_code.clone(),
            qr_content: session.qr_content.clone(),
            qr_svg: session.qr_svg.clone(),
        })
    });
    let Some(session) = session else {
        return qr_payload(
            "failed",
            "当前没有待确认的微信二维码。",
            "missing",
            "",
            "",
            "",
            "",
            false,
        );
    };

    let polled =
        WeixinClient::poll_qr_status(&session.base_url, &session.route_tag, &session.qr_code).await;
    let qr = match polled {
        Ok(value) => value,
        Err(error) => {
            return qr_payload(
                "failed",
                &format!("查询微信扫码状态失败：{error}"),
                "failed",
                &session.qr_content,
                &session.qr_svg,
                "",
                "",
                false,
            );
        }
    };

    if qr.status != "confirmed" {
        return qr_payload(
            "ok",
            "微信扫码状态已更新。",
            &qr.status,
            &session.qr_content,
            &session.qr_svg,
            "",
            "",
            false,
        );
    }

    if qr.bot_token.trim().is_empty() || qr.ilink_bot_id.trim().is_empty() {
        return qr_payload(
            "failed",
            "微信已确认登录，但网关未返回完整凭据。",
            "failed",
            &session.qr_content,
            &session.qr_svg,
            "",
            "",
            false,
        );
    }

    let store = SettingsStore::default();
    let mut settings = store.load().unwrap_or_default();
    settings.weixin_connect_token = qr.bot_token;
    settings.weixin_connect_account_id = qr.ilink_bot_id.clone();
    settings.weixin_connect_base_url = if qr.baseurl.trim().is_empty() {
        session.base_url.clone()
    } else {
        qr.baseurl.trim().trim_end_matches('/').to_string()
    };
    if settings.weixin_connect_allow_from.trim().is_empty()
        && !qr.ilink_user_id.trim().is_empty()
    {
        // 默认只允许扫码本人触发,避免任何人都能驱动本机 Codex。
        settings.weixin_connect_allow_from = qr.ilink_user_id.clone();
    }
    settings.weixin_connect_route_tag = session.route_tag;
    if let Err(error) = store.save(&settings) {
        return qr_payload(
            "failed",
            &format!("微信登录成功，但保存连接凭据失败：{error}"),
            "failed",
            &session.qr_content,
            &session.qr_svg,
            &qr.ilink_bot_id,
            &qr.ilink_user_id,
            false,
        );
    }
    if let Ok(mut current) = qr_session().lock() {
        *current = None;
    }
    qr_payload(
        "ok",
        "微信扫码登录成功。",
        "confirmed",
        "",
        "",
        &qr.ilink_bot_id,
        &qr.ilink_user_id,
        true,
    )
}

/// 读取当前连接状态 + 面板需要的配置回显。
pub fn status() -> Value {
    let settings = SettingsStore::default().load().unwrap_or_default();
    let mut value = status_value("ok", "微信连接状态已读取。");
    value["config"] = json!({
        "enabled": settings.weixin_connect_enabled,
        "hasToken": !settings.weixin_connect_token.trim().is_empty(),
        "accountId": settings.weixin_connect_account_id,
        "baseUrl": settings.weixin_connect_base_url,
        "allowFrom": settings.weixin_connect_allow_from,
        "workDir": settings.weixin_connect_work_dir,
        "model": settings.weixin_connect_model,
        "sandbox": settings.weixin_connect_sandbox,
        "codexPath": settings.weixin_connect_codex_path,
    });
    value
}

/// 启动微信连接(需已扫码拿到 token)。
pub fn start() -> Value {
    let store = SettingsStore::default();
    let mut settings = store.load().unwrap_or_default();
    if settings.weixin_connect_token.trim().is_empty() {
        return status_value("failed", "请先扫码登录微信。");
    }
    settings.weixin_connect_enabled = true;
    if let Err(error) = store.save(&settings) {
        return status_value("failed", &format!("保存微信连接设置失败：{error}"));
    }
    match spawn(settings) {
        Ok(()) => status_value("ok", "微信连接正在启动。"),
        Err(error) => status_value("failed", &format!("启动微信连接失败：{error}")),
    }
}

/// 停止微信连接。长轮询结束后线程才真正退出,所以中间态是 stopping。
pub fn stop() -> Value {
    let stopping = runtime_slot()
        .lock()
        .ok()
        .and_then(|runtime| runtime.as_ref().map(|runtime| Arc::clone(&runtime.stop)))
        .map(|stop| {
            stop.store(true, Ordering::SeqCst);
            true
        })
        .unwrap_or(false);
    let store = SettingsStore::default();
    if let Ok(mut settings) = store.load() {
        settings.weixin_connect_enabled = false;
        let _ = store.save(&settings);
    }
    if let Ok(mut status) = shared_status().lock() {
        if stopping {
            status.state = "stopping".to_string();
            status.message = "正在停止微信连接，当前长轮询结束后生效。".to_string();
        } else {
            status.state = "stopped".to_string();
            status.message = "微信连接已停止。".to_string();
        }
    }
    status_value(
        "ok",
        if stopping {
            "正在停止微信连接。"
        } else {
            "微信连接已停止。"
        },
    )
}

/// launcher 启动时按已保存的设置自动拉起(与 manager 旧行为一致)。
pub fn start_from_saved_settings() {
    let settings = SettingsStore::default().load().unwrap_or_default();
    if settings.weixin_connect_enabled && !settings.weixin_connect_token.trim().is_empty() {
        let _ = spawn(settings);
    }
}

fn spawn(settings: BackendSettings) -> anyhow::Result<()> {
    let config = WeixinConnectConfig {
        base_url: settings.weixin_connect_base_url,
        token: settings.weixin_connect_token,
        account_id: settings.weixin_connect_account_id,
        allow_from: settings.weixin_connect_allow_from,
        route_tag: settings.weixin_connect_route_tag,
        work_dir: settings.weixin_connect_work_dir,
        model: settings.weixin_connect_model,
        sandbox: settings.weixin_connect_sandbox,
        codex_path: settings.weixin_connect_codex_path,
    }
    .normalized();
    if config.token.is_empty() {
        anyhow::bail!("微信连接 token 为空");
    }
    let stop = Arc::new(AtomicBool::new(false));
    let mut runtime = runtime_slot()
        .lock()
        .map_err(|_| anyhow::anyhow!("微信连接运行锁已损坏"))?;
    if runtime.is_some() {
        anyhow::bail!("微信连接已在运行或正在停止");
    }
    *runtime = Some(WeixinRuntime {
        stop: Arc::clone(&stop),
    });
    drop(runtime);

    let status = shared_status();
    if let Ok(mut current) = status.lock() {
        current.state = "starting".to_string();
        current.message = "正在启动微信连接...".to_string();
        current.account_id = config.account_id.clone();
        current.has_token = true;
    }
    let task_status = Arc::clone(&status);
    let task_stop = Arc::clone(&stop);
    tokio::spawn(async move {
        if let Err(error) = run_weixin_connect(config, stop, Arc::clone(&task_status)).await {
            if let Ok(mut current) = task_status.lock() {
                current.state = "error".to_string();
                current.message = format!("微信连接已停止：{error}");
            }
        }
        // 只清理自己占的槽位,避免把后续新建的运行时误清掉。
        if let Ok(mut runtime) = runtime_slot().lock() {
            let owned = runtime
                .as_ref()
                .map(|runtime| Arc::ptr_eq(&runtime.stop, &task_stop))
                .unwrap_or(false);
            if owned {
                *runtime = None;
            }
        }
    });
    Ok(())
}

/// CDP 桥分发:`/weixin/*` → 控制面命令。
pub async fn handle_bridge(path: &str, payload: &Value) -> Value {
    match path {
        "/weixin/status" => status(),
        "/weixin/start" => start(),
        "/weixin/stop" => stop(),
        "/weixin/qr-start" => {
            let base_url = payload
                .get("baseUrl")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let route_tag = payload
                .get("routeTag")
                .and_then(Value::as_str)
                .unwrap_or_default();
            qr_start(base_url, route_tag).await
        }
        "/weixin/qr-status" => qr_status().await,
        _ => json!({"status":"failed","message":format!("unknown weixin path: {path}")}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stop_without_runtime_reports_stopped() {
        let value = stop();
        assert_eq!(value["status"], "ok");
        assert_eq!(value["connect"]["state"], "stopped");
    }

    #[test]
    fn unknown_path_is_rejected() {
        let value = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(handle_bridge("/weixin/nope", &json!({})));
        assert_eq!(value["status"], "failed");
    }
}
