use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};

use crate::models::{DeleteResult, DeleteStatus, ExportResult, ExportStatus, SessionRef};
use crate::settings::{BackendSettings, SettingsStore};
use crate::status::StatusStore;
use crate::user_scripts::UserScriptManager;

pub type UserScriptEvaluator = Arc<dyn Fn(&str, &str) -> anyhow::Result<Value> + Send + Sync>;
pub type DevtoolsOpener = Arc<dyn Fn(&str) -> anyhow::Result<()> + Send + Sync>;

// recodex-overlay: ReCodex 命令桥(账号/额度/网关/登录/登出)。同步实现(阻塞网络 I/O),
// async handler 用 spawn_blocking 调用。impl 由 launcher 注入,逻辑在 recodex-integration crate。
pub trait RecodexBridge: Send + Sync {
    fn handle(&self, path: &str, payload: &Value) -> Value;
}

#[derive(Clone)]
pub struct BridgeContext {
    settings: Arc<dyn BridgeSettingsService>,
    runtime: Arc<dyn BridgeRuntimeService>,
    data: Arc<dyn BridgeDataService>,
    recodex: Option<Arc<dyn RecodexBridge>>, // recodex-overlay:field
}

impl BridgeContext {
    pub fn new(
        settings: Arc<dyn BridgeSettingsService>,
        runtime: Arc<dyn BridgeRuntimeService>,
        data: Arc<dyn BridgeDataService>,
    ) -> Self {
        Self {
            settings,
            runtime,
            data,
            recodex: None, // recodex-overlay:field-init
        }
    }

    // recodex-overlay: launcher 注入 ReCodex 桥;其余构造保持 None。
    pub fn with_recodex(mut self, recodex: Arc<dyn RecodexBridge>) -> Self {
        self.recodex = Some(recodex);
        self
    }

    pub fn core(runtime: Arc<dyn BridgeRuntimeService>) -> Self {
        Self::core_with_data(runtime, Arc::new(UnavailableDataService))
    }

    pub fn core_with_data(
        runtime: Arc<dyn BridgeRuntimeService>,
        data: Arc<dyn BridgeDataService>,
    ) -> Self {
        Self::new(Arc::new(CoreSettingsService::default()), runtime, data)
    }

    pub fn core_with_data_and_app_dir(
        runtime: Arc<dyn BridgeRuntimeService>,
        data: Arc<dyn BridgeDataService>,
        app_dir: PathBuf,
    ) -> Self {
        Self::new(
            Arc::new(CoreSettingsService::with_app_dir(app_dir)),
            runtime,
            data,
        )
    }
}

#[async_trait]
pub trait BridgeSettingsService: Send + Sync {
    async fn get_settings(&self) -> anyhow::Result<BackendSettings>;
    async fn set_settings(&self, payload: Value) -> anyhow::Result<BackendSettings>;

    async fn codex_app_version(&self) -> anyhow::Result<String> {
        Ok(String::new())
    }
}

#[async_trait]
pub trait BridgeRuntimeService: Send + Sync {
    async fn user_script_inventory(&self) -> anyhow::Result<Value>;
    async fn user_script_inventory_with_runtime_status(
        &self,
        _payload: Value,
    ) -> anyhow::Result<Value> {
        self.user_script_inventory().await
    }
    async fn set_user_scripts_enabled(&self, enabled: bool) -> anyhow::Result<Value>;
    async fn set_user_script_enabled(&self, key: String, enabled: bool) -> anyhow::Result<Value>;
    async fn delete_user_script(&self, key: String) -> anyhow::Result<Value>;
    async fn reload_user_scripts(&self) -> anyhow::Result<Value>;
    async fn open_devtools(&self) -> anyhow::Result<Value>;
    // recodex-overlay: 默认不支持,由 launcher 覆盖
    async fn restart_codex(&self) -> anyhow::Result<Value> {
        anyhow::bail!("restarting Codex is unavailable")
    }
    // recodex-overlay: 卸载后退出。默认不支持,由 launcher 覆盖
    async fn quit(&self) -> anyhow::Result<Value> {
        anyhow::bail!("quitting is unavailable")
    }
    // recodex-overlay: 默认拒绝,由 launcher 覆盖实现
    async fn open_external(&self, _url: String) -> anyhow::Result<Value> {
        anyhow::bail!("opening external URLs is unavailable")
    }
    async fn open_manager(&self) -> anyhow::Result<Value>;
    async fn open_transient_manager(&self) -> anyhow::Result<Value> {
        self.open_manager().await
    }
    async fn backend_status(&self) -> anyhow::Result<Value>;
    async fn codex_model_catalog(&self) -> anyhow::Result<Value>;
    async fn zed_remote_status(&self) -> anyhow::Result<Value>;
    async fn resolve_zed_remote_host(&self, payload: Value) -> anyhow::Result<Value>;
    async fn fallback_zed_remote_request(&self, payload: Value) -> anyhow::Result<Value>;
    async fn open_zed_remote(&self, payload: Value) -> anyhow::Result<Value>;
    async fn list_zed_remote_projects(&self, payload: Value) -> anyhow::Result<Value>;
    async fn remember_zed_remote_project(&self, payload: Value) -> anyhow::Result<Value>;
    async fn forget_zed_remote_project(&self, payload: Value) -> anyhow::Result<Value>;
    async fn upstream_worktree_status(&self) -> anyhow::Result<Value>;
    async fn upstream_worktree_defaults(&self, payload: Value) -> anyhow::Result<Value>;
    async fn upstream_worktree_prepare(&self, payload: Value) -> anyhow::Result<Value>;
    async fn upstream_worktree_create(&self, payload: Value) -> anyhow::Result<Value>;
}

#[async_trait]
pub trait BridgeDataService: Send + Sync {
    async fn delete(&self, session: SessionRef) -> anyhow::Result<DeleteResult>;
    async fn undo(&self, undo_token: String) -> anyhow::Result<DeleteResult>;
    async fn export_markdown(&self, session: SessionRef) -> anyhow::Result<ExportResult>;
    async fn thread_usage_history(&self, session: SessionRef) -> anyhow::Result<Value>;
    async fn find_archived_thread_by_title(
        &self,
        title: String,
    ) -> anyhow::Result<Option<SessionRef>>;
    async fn recover_remote_control_session(&self, _thread_id: String) -> anyhow::Result<Value> {
        anyhow::bail!("Remote Control session recovery is unavailable")
    }
}

pub async fn handle_bridge_request(
    ctx: BridgeContext,
    path: &str,
    payload: Value,
) -> serde_json::Value {
    let started = Instant::now();
    let _ = crate::diagnostic_log::append_diagnostic_log(
        "bridge.request",
        json!({
            "path": path,
            "payload_keys": payload
                .as_object()
                .map(|object| object.keys().cloned().collect::<Vec<_>>())
                .unwrap_or_default()
        }),
    );
    // recodex-overlay:hook — /recodex/* 委托给 launcher 注入的 ReCodex 桥。
    // 这些命令是**同步阻塞网络 I/O**(账号/额度/网关都要打上游),直接在 async 线程里跑
    // 会把执行器占住,导致同期的其它桥请求全部卡死(表现为面板永远「加载中」)。
    // 所以丢到 spawn_blocking。
    if path.starts_with("/recodex/") {
        let Some(recodex) = ctx.recodex.clone() else {
            return json!({"status":"error","error":{"code":"unavailable","message":"recodex bridge not configured"}});
        };
        let owned_path = path.to_string();
        let owned_payload = payload.clone();
        return tokio::task::spawn_blocking(move || recodex.handle(&owned_path, &owned_payload))
            .await
            .unwrap_or_else(|error| {
                json!({"status":"error","error":{"code":"panic","message":error.to_string()}})
            });
    }
    // recodex-overlay:hook — /weixin/* 微信连接控制面(原 manager Tauri 命令,已迁到 core)。
    if path.starts_with("/weixin/") {
        return crate::connect::control::handle_bridge(path, &payload).await;
    }
    let result = match path {
        "/settings/get" => settings_value(&ctx, ctx.settings.get_settings().await).await,
        "/settings/set" => {
            settings_value(&ctx, ctx.settings.set_settings(payload.clone()).await).await
        }
        "/user-scripts/list" => {
            ctx.runtime
                .user_script_inventory_with_runtime_status(payload.clone())
                .await
        }
        "/user-scripts/set-enabled" => {
            let enabled = payload
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            ctx.runtime.set_user_scripts_enabled(enabled).await
        }
        "/user-scripts/set-script-enabled" => {
            let key = payload
                .get("key")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let enabled = payload
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            ctx.runtime.set_user_script_enabled(key, enabled).await
        }
        "/user-scripts/delete" => {
            let key = payload
                .get("key")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            ctx.runtime.delete_user_script(key).await
        }
        "/user-scripts/reload" => ctx.runtime.reload_user_scripts().await,
        "/devtools/open" => ctx.runtime.open_devtools().await,
        // recodex-overlay: 切换官方模式 / 更新配置后需要 Codex 重新起来才生效
        "/restart-codex" => ctx.runtime.restart_codex().await,
        // recodex-overlay: 卸载后退出 —— 与 /restart-codex 的区别是**不拉接班进程**,
        // 否则新进程会重新锁住待删的 exe,自删脚本必然失败
        "/quit" => ctx.runtime.quit().await,
        // recodex-overlay: 自更新 —— 下载并校验后就位,由前端接着触发重启。
        //
        // **更新地址只认服务端下发的,不接受页面传参**。这条桥暴露在渲染进程里,
        // 而注入目标只要求"是个 page",并不限制来源 —— 一旦注入落到非官方页面,
        // 让页面指定 manifest 地址就等于把「安装任意 exe」的权限交出去。
        // 所以这里现问一次带认证的 check-client,用它给的地址。
        "/self-update" => {
            let manifest_url = match ctx.recodex.as_ref() {
                Some(recodex) => {
                    let checked = recodex.handle("/recodex/check-client", &json!({}));
                    server_manifest_url(&checked)
                }
                None => None,
            };
            match manifest_url {
                Some(url) => Ok(self_update_value(url).await),
                None => Ok(json!({
                    "status": "failed",
                    "message": "服务端没有下发可用的更新地址,已拒绝更新"
                })),
            }
        }
        // recodex-overlay: 卸载 —— 先让 ReCodex 桥做登出与凭据清理(它持有账号状态),
        // 再由本模块删目录/快捷方式并安排 exe 自删。面板侧已二次确认。
        "/uninstall" => {
            let prepared = match ctx.recodex.as_ref() {
                Some(recodex) => recodex.handle("/recodex/prepare-uninstall", &payload),
                None => json!({"status": "ready"}),
            };
            let mut result = crate::uninstall::perform_uninstall(|| Ok(()));
            if let (Some(warning), Some(list)) = (
                prepared.get("warning").and_then(Value::as_str),
                result.get_mut("warnings").and_then(Value::as_array_mut),
            ) {
                list.push(Value::String(format!("登出:{warning}")));
            }
            Ok(result)
        }
        // recodex-overlay: 用系统浏览器打开外部链接(注入页里 window.open 会被 Electron 拦)
        "/open-external" => {
            let url = payload
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            ctx.runtime.open_external(url).await
        }
        "/manager/open" => ctx.runtime.open_manager().await,
        "/manager/open-transient" => ctx.runtime.open_transient_manager().await,
        "/backend/status" => backend_status_value(
            ctx.runtime.backend_status().await,
            ctx.settings.get_settings().await,
        ),
        "/codex-model-catalog" | "/codex-config-model" => ctx.runtime.codex_model_catalog().await,
        "/diagnostics/log" => diagnostic_log_value(payload.clone()),
        "/llm-proxy" => llm_proxy_value(payload.clone()).await,
        "/zed-remote/status" => ctx.runtime.zed_remote_status().await,
        "/zed-remote/resolve-host" => ctx.runtime.resolve_zed_remote_host(payload.clone()).await,
        "/zed-remote/fallback-request" => {
            ctx.runtime
                .fallback_zed_remote_request(payload.clone())
                .await
        }
        "/zed-remote/open" => ctx.runtime.open_zed_remote(payload.clone()).await,
        "/zed-remote/projects" => ctx.runtime.list_zed_remote_projects(payload.clone()).await,
        "/zed-remote/remember-project" => {
            ctx.runtime
                .remember_zed_remote_project(payload.clone())
                .await
        }
        "/zed-remote/forget-project" => {
            ctx.runtime.forget_zed_remote_project(payload.clone()).await
        }
        "/upstream-worktree/status" => ctx.runtime.upstream_worktree_status().await,
        "/upstream-worktree/defaults" => {
            ctx.runtime
                .upstream_worktree_defaults(payload.clone())
                .await
        }
        "/upstream-worktree/prepare" => {
            ctx.runtime.upstream_worktree_prepare(payload.clone()).await
        }
        "/upstream-worktree/create" => ctx.runtime.upstream_worktree_create(payload.clone()).await,
        "/delete" => result_value(ctx.data.delete(session_from_payload(&payload)).await),
        "/undo" => {
            let undo_token = payload
                .get("undo_token")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            result_value(ctx.data.undo(undo_token).await)
        }
        "/export-markdown" => result_value(
            ctx.data
                .export_markdown(session_from_payload(&payload))
                .await,
        ),
        "/thread-usage-history" => {
            ctx.data
                .thread_usage_history(session_from_payload(&payload))
                .await
        }
        "/archived-thread" => {
            let title = payload
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            archived_thread_value(ctx.data.find_archived_thread_by_title(title).await)
        }
        "/remote-control-session/recover" => {
            let thread_id = payload
                .get("thread_id")
                .or_else(|| payload.get("threadId"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            ctx.data.recover_remote_control_session(thread_id).await
        }
        _ => {
            let _ = crate::diagnostic_log::append_diagnostic_log(
                "bridge.unknown_path",
                json!({
                    "path": path
                }),
            );
            return json!({
                "status": "failed",
                "session_id": "",
                "message": "Unknown bridge path"
            });
        }
    };

    let response = result.unwrap_or_else(|error| failed_from_error(&payload, error));
    let _ = crate::diagnostic_log::append_diagnostic_log(
        "bridge.response",
        json!({
            "path": path,
            "elapsed_ms": started.elapsed().as_millis() as u64,
            "status": response.get("status").and_then(Value::as_str).unwrap_or("")
        }),
    );
    response
}

#[derive(Default)]
pub struct CoreSettingsService {
    store: SettingsStore,
    app_dir: Option<PathBuf>,
}

impl CoreSettingsService {
    fn with_app_dir(app_dir: PathBuf) -> Self {
        Self {
            store: SettingsStore::default(),
            app_dir: Some(app_dir),
        }
    }
}

#[async_trait]
impl BridgeSettingsService for CoreSettingsService {
    async fn get_settings(&self) -> anyhow::Result<BackendSettings> {
        self.store.load()
    }

    async fn set_settings(&self, payload: Value) -> anyhow::Result<BackendSettings> {
        self.store.update(payload)
    }

    async fn codex_app_version(&self) -> anyhow::Result<String> {
        if let Some(app_dir) = self.app_dir.as_deref() {
            return Ok(crate::app_paths::codex_app_version(app_dir).unwrap_or_default());
        }
        let settings = self.store.load().unwrap_or_default();
        let app_dir = crate::app_paths::resolve_codex_app_dir_with_saved(
            None,
            Some(settings.codex_app_path.as_str()),
        );
        Ok(app_dir
            .as_deref()
            .and_then(crate::app_paths::codex_app_version)
            .unwrap_or_default())
    }
}

#[derive(Clone)]
pub struct CoreRuntimeService {
    debug_port: u16,
    status_store: StatusStore,
    user_scripts: Option<UserScriptManager>,
    websocket_url: Option<String>,
    user_script_evaluator: Option<UserScriptEvaluator>,
    devtools_opener: Option<DevtoolsOpener>,
    devtools_target_id: Option<String>,
}

impl CoreRuntimeService {
    pub fn new(debug_port: u16, status_store: StatusStore) -> Self {
        Self {
            debug_port,
            status_store,
            user_scripts: None,
            websocket_url: None,
            user_script_evaluator: None,
            devtools_opener: None,
            devtools_target_id: None,
        }
    }

    pub fn with_user_scripts(mut self, user_scripts: UserScriptManager) -> Self {
        self.user_scripts = Some(user_scripts);
        self
    }

    pub fn with_websocket_url(mut self, websocket_url: impl Into<String>) -> Self {
        self.websocket_url = Some(websocket_url.into());
        self
    }

    pub fn with_user_script_evaluator(mut self, evaluator: UserScriptEvaluator) -> Self {
        self.user_script_evaluator = Some(evaluator);
        self
    }

    pub fn with_devtools_opener(mut self, opener: DevtoolsOpener) -> Self {
        self.devtools_opener = Some(opener);
        self
    }

    pub fn with_devtools_target_id(mut self, target_id: impl Into<String>) -> Self {
        self.devtools_target_id = Some(target_id.into());
        self
    }
}

#[async_trait]
impl BridgeRuntimeService for CoreRuntimeService {
    async fn user_script_inventory(&self) -> anyhow::Result<Value> {
        match &self.user_scripts {
            Some(user_scripts) => user_scripts.inventory(),
            None => Ok(empty_user_script_inventory()),
        }
    }

    async fn set_user_scripts_enabled(&self, enabled: bool) -> anyhow::Result<Value> {
        match &self.user_scripts {
            Some(user_scripts) => {
                user_scripts.set_global_enabled(enabled)?;
                user_scripts.inventory()
            }
            None => {
                let mut inventory = empty_user_script_inventory();
                inventory["enabled"] = json!(enabled);
                Ok(inventory)
            }
        }
    }

    async fn set_user_script_enabled(&self, key: String, enabled: bool) -> anyhow::Result<Value> {
        match &self.user_scripts {
            Some(user_scripts) => {
                user_scripts.set_script_enabled(&key, enabled)?;
                user_scripts.inventory()
            }
            None => Ok(empty_user_script_inventory()),
        }
    }

    async fn delete_user_script(&self, key: String) -> anyhow::Result<Value> {
        match &self.user_scripts {
            Some(user_scripts) => {
                user_scripts.delete_user_script(&key)?;
                user_scripts.inventory()
            }
            None => Ok(empty_user_script_inventory()),
        }
    }

    async fn reload_user_scripts(&self) -> anyhow::Result<Value> {
        if let (Some(user_scripts), Some(websocket_url), Some(evaluator)) = (
            &self.user_scripts,
            self.websocket_url.as_deref(),
            &self.user_script_evaluator,
        ) {
            let bundle = user_scripts.build_enabled_bundle()?;
            if !bundle.trim().is_empty() {
                evaluator(websocket_url, &bundle)?;
            }
        }
        self.user_script_inventory().await
    }

    async fn open_devtools(&self) -> anyhow::Result<Value> {
        let target_id = self
            .devtools_target_id
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("No DevTools target configured"))?;
        let url = devtools_url(self.debug_port, target_id);
        if let Some(opener) = &self.devtools_opener {
            opener(&url)?;
        }
        Ok(json!({
            "status": "ok",
            "target_id": target_id,
            "url": url
        }))
    }

    async fn open_manager(&self) -> anyhow::Result<Value> {
        let target = crate::install::spawn_companion(
            crate::install::MANAGER_BINARY,
            std::iter::empty::<&str>(),
        )?;
        Ok(json!({
            "status": "ok",
            "path": target
        }))
    }

    async fn open_transient_manager(&self) -> anyhow::Result<Value> {
        let target =
            crate::install::spawn_companion(crate::install::MANAGER_BINARY, ["--transient"])?;
        Ok(json!({
            "status": "ok",
            "path": target
        }))
    }

    async fn backend_status(&self) -> anyhow::Result<Value> {
        let _ = self.status_store.load_latest();
        let _ = crate::diagnostic_log::append_diagnostic_log(
            "bridge.backend_status_ok",
            json!({
                "debug_port": self.debug_port,
                "version": crate::version::VERSION
            }),
        );
        Ok(json!({"status": "ok", "message": "后端已连接", "version": crate::version::VERSION}))
    }

    async fn codex_model_catalog(&self) -> anyhow::Result<Value> {
        Ok(crate::model_catalog::read_codex_model_catalog().await)
    }

    async fn zed_remote_status(&self) -> anyhow::Result<Value> {
        Ok(crate::zed_remote::zed_remote_status())
    }

    async fn resolve_zed_remote_host(&self, payload: Value) -> anyhow::Result<Value> {
        Ok(crate::zed_remote::resolve_ssh_target_response(&payload))
    }

    async fn fallback_zed_remote_request(&self, payload: Value) -> anyhow::Result<Value> {
        Ok(crate::zed_remote::fallback_open_request_response(&payload))
    }

    async fn open_zed_remote(&self, payload: Value) -> anyhow::Result<Value> {
        Ok(crate::zed_remote::open_zed_remote(&payload))
    }

    async fn list_zed_remote_projects(&self, payload: Value) -> anyhow::Result<Value> {
        Ok(crate::zed_remote::list_zed_remote_projects_response(
            &payload,
        ))
    }

    async fn remember_zed_remote_project(&self, payload: Value) -> anyhow::Result<Value> {
        Ok(crate::zed_remote::remember_zed_remote_project_response(
            &payload,
        ))
    }

    async fn forget_zed_remote_project(&self, payload: Value) -> anyhow::Result<Value> {
        Ok(crate::zed_remote::forget_zed_remote_project_response(
            &payload,
        ))
    }

    async fn upstream_worktree_status(&self) -> anyhow::Result<Value> {
        Ok(crate::upstream_worktree::status_response())
    }

    async fn upstream_worktree_defaults(&self, payload: Value) -> anyhow::Result<Value> {
        Ok(crate::upstream_worktree::defaults_response(&payload))
    }

    async fn upstream_worktree_prepare(&self, payload: Value) -> anyhow::Result<Value> {
        Ok(crate::upstream_worktree::prepare_response(&payload))
    }

    async fn upstream_worktree_create(&self, payload: Value) -> anyhow::Result<Value> {
        Ok(crate::upstream_worktree::create_response(&payload))
    }
}

struct UnavailableDataService;

#[async_trait]
impl BridgeDataService for UnavailableDataService {
    async fn delete(&self, session: SessionRef) -> anyhow::Result<DeleteResult> {
        Ok(DeleteResult {
            status: DeleteStatus::Failed,
            session_id: session.session_id,
            message: "Delete service is not wired in core launcher hooks".to_string(),
            undo_token: None,
            backup_path: None,
        })
    }

    async fn undo(&self, undo_token: String) -> anyhow::Result<DeleteResult> {
        Ok(DeleteResult {
            status: DeleteStatus::Failed,
            session_id: String::new(),
            message: "Undo service is not wired in core launcher hooks".to_string(),
            undo_token: Some(undo_token),
            backup_path: None,
        })
    }

    async fn export_markdown(&self, session: SessionRef) -> anyhow::Result<ExportResult> {
        Ok(ExportResult {
            status: ExportStatus::Failed,
            session_id: session.session_id,
            message: "Markdown export service is not wired in core launcher hooks".to_string(),
            filename: None,
            markdown: None,
        })
    }

    async fn thread_usage_history(&self, session: SessionRef) -> anyhow::Result<Value> {
        Ok(json!({
            "status": "failed",
            "session_id": session.session_id,
            "message": "Thread usage history service is not wired in core launcher hooks",
            "history": []
        }))
    }

    async fn find_archived_thread_by_title(
        &self,
        _title: String,
    ) -> anyhow::Result<Option<SessionRef>> {
        Ok(None)
    }

}

fn settings_payload_value(
    settings: BackendSettings,
    codex_app_version: String,
) -> anyhow::Result<Value> {
    let mut value = serde_json::to_value(settings)?;
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "codexAppVersion".to_string(),
            Value::String(codex_app_version),
        );
    }
    Ok(value)
}

async fn settings_value(
    ctx: &BridgeContext,
    result: anyhow::Result<BackendSettings>,
) -> anyhow::Result<Value> {
    let settings = result?;
    let codex_app_version = ctx.settings.codex_app_version().await.unwrap_or_default();
    settings_payload_value(settings, codex_app_version)
}

// 全流程失败都返回结构化结果而不是 Err —— 更新失败要让用户看到原因,
// 而不是一个笼统的桥错误。
/// 从 `/recodex/check-client` 的返回里取出服务端下发的 manifest 地址。
/// 只在 `available` 为真时才认 —— 没有可用更新就不该有任何下载动作。
fn server_manifest_url(checked: &Value) -> Option<String> {
    let channel = checked.get("data")?.get("update_channel")?;
    if !channel.get("available").and_then(Value::as_bool).unwrap_or(false) {
        return None;
    }
    let url = channel.get("manifest_url").and_then(Value::as_str)?.trim();
    (!url.is_empty()).then(|| url.to_string())
}

async fn self_update_value(manifest_url: String) -> Value {
    if manifest_url.trim().is_empty() {
        return json!({"status": "failed", "message": "服务端未提供更新清单地址"});
    }
    let manifest = match crate::selfupdate::fetch_manifest(&manifest_url).await {
        Ok(value) => value,
        Err(error) => return json!({"status":"failed","message":format!("获取更新清单失败:{error}")}),
    };
    let bytes = match crate::selfupdate::download_verified(&manifest).await {
        Ok(value) => value,
        Err(error) => return json!({"status":"failed","message":format!("{error}")}),
    };
    let exe = match std::env::current_exe() {
        Ok(value) => value,
        Err(error) => return json!({"status":"failed","message":format!("无法定位程序文件:{error}")}),
    };
    match crate::selfupdate::stage_replacement(&exe, &bytes) {
        Ok(_) => json!({
            "status": "ok",
            "message": format!("已更新到 {},正在重启…", manifest.version)
        }),
        Err(error) => json!({"status":"failed","message":format!("替换程序文件失败:{error}")}),
    }
}

fn backend_status_value(
    status: anyhow::Result<Value>,
    settings: anyhow::Result<BackendSettings>,
) -> anyhow::Result<Value> {
    let mut status = status?;
    if let Some(object) = status.as_object_mut() {
        let hide = settings
            .map(|settings| crate::assets::hide_official_usage_alert_config(&settings))
            .unwrap_or(false);
        object.insert("hideOfficialUsageAlert".to_string(), Value::Bool(hide));
    }
    Ok(status)
}

fn result_value<T>(result: anyhow::Result<T>) -> anyhow::Result<Value>
where
    T: serde::Serialize,
{
    Ok(serde_json::to_value(result?)?)
}

async fn llm_proxy_value(payload: Value) -> anyhow::Result<Value> {
    let url = validate_llm_proxy_url(
        payload
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    )?;
    let method = payload
        .get("method")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("POST");
    if !method.eq_ignore_ascii_case("POST") {
        anyhow::bail!("LLM Bridge 仅支持 POST 请求");
    }

    let timeout_ms = payload
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(60_000)
        .clamp(1_000, 60_000);
    let body = payload
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if body.len() > 1_048_576 {
        anyhow::bail!("LLM Bridge 请求体过大");
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(timeout_ms))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let response = client
        .post(url)
        .headers(llm_proxy_headers(&payload)?)
        .body(body)
        .send()
        .await?;
    let http_status = response.status().as_u16();
    let ok = response.status().is_success();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    if let Some(length) = response.content_length() {
        if length > 4 * 1024 * 1024 {
            anyhow::bail!("LLM Bridge 响应体过大");
        }
    }
    let bytes = response.bytes().await?;
    if bytes.len() > 4 * 1024 * 1024 {
        anyhow::bail!("LLM Bridge 响应体过大");
    }
    let body_text = String::from_utf8_lossy(&bytes).to_string();
    let body_json = if content_type
        .split(';')
        .next()
        .map(str::trim)
        .is_some_and(|value| value.eq_ignore_ascii_case("application/json"))
    {
        serde_json::from_slice::<Value>(&bytes).ok()
    } else {
        None
    };

    Ok(json!({
        "status": "ok",
        "http_status": http_status,
        "ok": ok,
        "body_text": body_text,
        "body_json": body_json,
    }))
}

fn validate_llm_proxy_url(raw: &str) -> anyhow::Result<reqwest::Url> {
    let url = reqwest::Url::parse(raw.trim()).map_err(|_| anyhow::anyhow!("Base URL 格式无效"))?;
    if url.scheme() != "https" {
        anyhow::bail!("Base URL 必须使用 HTTPS");
    }
    if !url.username().is_empty() || url.password().is_some() {
        anyhow::bail!("Base URL 不得包含用户名或密码");
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("Base URL 缺少主机名"))?;
    if is_blocked_llm_proxy_host(host) {
        anyhow::bail!("Base URL 不得指向本机或私有网络");
    }
    Ok(url)
}

fn is_blocked_llm_proxy_host(host: &str) -> bool {
    let host = host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_lowercase();
    if host.is_empty()
        || host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
    {
        return true;
    }
    if let Ok(ip) = std::net::IpAddr::from_str(&host) {
        return match ip {
            std::net::IpAddr::V4(ip) => {
                ip.is_loopback()
                    || ip.is_private()
                    || ip.is_link_local()
                    || ip.is_multicast()
                    || ip.is_broadcast()
                    || ip.is_documentation()
                    || ip.octets()[0] == 0
                    || ip.octets()[0] == 100 && (64..=127).contains(&ip.octets()[1])
            }
            std::net::IpAddr::V6(ip) => {
                ip.is_loopback()
                    || ip.is_unspecified()
                    || ip.is_multicast()
                    || (ip.segments()[0] & 0xfe00) == 0xfc00
                    || (ip.segments()[0] & 0xffc0) == 0xfe80
            }
        };
    }
    false
}

fn llm_proxy_headers(payload: &Value) -> anyhow::Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    let Some(raw_headers) = payload.get("headers").and_then(Value::as_object) else {
        return Ok(headers);
    };
    for (name, value) in raw_headers {
        if !is_allowed_llm_proxy_header(name) {
            continue;
        }
        let Some(value) = value.as_str() else {
            continue;
        };
        let header_name = HeaderName::from_bytes(name.as_bytes())?;
        let header_value = HeaderValue::from_str(value)?;
        headers.insert(header_name, header_value);
    }
    Ok(headers)
}

fn is_allowed_llm_proxy_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "accept"
            | "api-key"
            | "anthropic-beta"
            | "anthropic-version"
            | "authorization"
            | "content-type"
            | "openai-organization"
            | "openai-project"
            | "x-api-key"
    )
}

fn diagnostic_log_value(payload: Value) -> anyhow::Result<Value> {
    let event = payload
        .get("event")
        .and_then(Value::as_str)
        .map(sanitize_diagnostic_event)
        .unwrap_or_else(|| "event".to_string());
    crate::diagnostic_log::append_diagnostic_log(&format!("renderer.{event}"), payload)?;
    Ok(json!({
        "status": "ok",
        "message": "日志已记录"
    }))
}

fn sanitize_diagnostic_event(event: &str) -> String {
    let sanitized = event
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if sanitized.is_empty() {
        "event".to_string()
    } else {
        sanitized
    }
}

fn archived_thread_value(result: anyhow::Result<Option<SessionRef>>) -> anyhow::Result<Value> {
    Ok(match result? {
        Some(session) => json!({"session_id": session.session_id, "title": session.title}),
        None => json!({"session_id": "", "title": ""}),
    })
}

fn failed_from_error(payload: &Value, error: anyhow::Error) -> Value {
    json!({
        "status": "failed",
        "session_id": payload
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        "message": error.to_string()
    })
}

fn session_from_payload(payload: &Value) -> SessionRef {
    SessionRef {
        session_id: payload
            .get("session_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        title: payload
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
    }
}

pub fn devtools_url(debug_port: u16, target_id: &str) -> String {
    format!(
        "http://127.0.0.1:{debug_port}/devtools/inspector.html?ws=127.0.0.1:{debug_port}/devtools/page/{target_id}"
    )
}

fn empty_user_script_inventory() -> Value {
    json!({
        "enabled": true,
        "scripts": []
    })
}

#[cfg(test)]
mod self_update_source_tests {
    use super::*;

    /// 更新地址必须来自服务端。这条桥暴露在渲染进程里,而注入目标只要求"是个 page",
    /// 页面能指定 manifest 就等于把「安装任意 exe」的权限交出去。
    #[test]
    fn only_the_server_can_name_the_update_source() {
        let served = json!({
            "status": "ok",
            "data": {
                "update_channel": {
                    "available": true,
                    "latest_version": "1.2.50",
                    "manifest_url": "https://oss.example.com/recodex/1.2.50/manifest.json"
                }
            }
        });
        assert_eq!(
            server_manifest_url(&served).as_deref(),
            Some("https://oss.example.com/recodex/1.2.50/manifest.json")
        );
    }

    #[test]
    fn no_available_update_means_no_download() {
        // 没有可用更新时不该有任何下载动作,哪怕响应里还带着地址
        let served = json!({
            "status": "ok",
            "data": {
                "update_channel": {
                    "available": false,
                    "reason": "not_in_rollout",
                    "manifest_url": "https://oss.example.com/stale/manifest.json"
                }
            }
        });
        assert!(server_manifest_url(&served).is_none());
    }

    #[test]
    fn malformed_or_signed_out_responses_yield_nothing() {
        assert!(server_manifest_url(&json!({})).is_none());
        assert!(server_manifest_url(&json!({"status": "signed_out", "data": {}})).is_none());
        // 空地址不算地址
        assert!(
            server_manifest_url(&json!({
                "data": {"update_channel": {"available": true, "manifest_url": "  "}}
            }))
            .is_none()
        );
    }
}
