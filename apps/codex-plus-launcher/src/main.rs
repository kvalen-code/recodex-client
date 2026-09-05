#![cfg_attr(windows, windows_subsystem = "windows")]

use anyhow::{Context, Result};
use codex_plus_core::launcher::{
    BridgeReinjector, DefaultLaunchHooks, LaunchHooks, LaunchOptions, launch_and_inject_with_hooks,
};
use codex_plus_core::models::{DeleteResult, ExportResult, SessionRef};
use codex_plus_core::routes::{BridgeContext, BridgeDataService, BridgeRuntimeService};
use codex_plus_core::status::LaunchStatus;
use codex_plus_core::user_scripts::UserScriptManager;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// recodex-overlay: ReCodex 桥 launcher 侧薄接线;逻辑在 recodex-integration crate 的 desktop 模块。
struct LauncherRecodexBridge {
    state: recodex_integration::desktop::ReCodexState,
}

impl codex_plus_core::routes::RecodexBridge for LauncherRecodexBridge {
    fn handle(&self, path: &str, payload: &Value) -> Value {
        let result = recodex_integration::desktop::handle_bridge(&self.state, path, payload);
        // recodex-overlay:diag-flush — 任何 ReCodex 操作失败(登录/选网关/刷新额度…)都留一条
        // 诊断,后台 flush 会传回服务器。事件名带上操作(select-gateway/login-start…)方便聚合,
        // detail 里带 path、错误码和网关 id —— 连接类故障才分得清是哪条线。
        if result.get("status").and_then(Value::as_str) == Some("error") {
            let op = path.rsplit('/').next().unwrap_or("unknown");
            let error = result.get("error").cloned().unwrap_or(Value::Null);
            let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
                &format!("recodex.bridge_error.{op}"),
                serde_json::json!({
                    "path": path,
                    "code": error.get("code").cloned().unwrap_or(Value::Null),
                    "message": error.get("message").cloned().unwrap_or(Value::Null),
                    "gateway": payload.get("id").or(payload.get("gateway")).or(payload.get("gateway_id")).cloned().unwrap_or(Value::Null),
                }),
            );
        }
        result
    }
}

#[derive(Clone)]
struct LauncherHooks {
    core: Arc<DefaultLaunchHooks>,
    data: Arc<LauncherDataService>,
    runtime: Arc<LauncherRuntimeService>,
    bridge_context: Arc<Mutex<Option<BridgeContext>>>,
    recodex: Arc<LauncherRecodexBridge>, // recodex-overlay:field
}

impl Default for LauncherHooks {
    fn default() -> Self {
        Self {
            core: Arc::new(DefaultLaunchHooks::default()),
            data: Arc::new(LauncherDataService::default()),
            runtime: Arc::new(LauncherRuntimeService::new(
                9229,
                default_user_script_manager(),
            )),
            bridge_context: Arc::new(Mutex::new(None)),
            // recodex-overlay: ReCodexState 只建一次(load 保存的凭据),跨重注入共享,不丢登录态。
            recodex: Arc::new(LauncherRecodexBridge {
                state: {
                    let state = recodex_integration::desktop::ReCodexState::from_env();
                    // recodex-overlay:diag-flush — 后台把本地诊断日志里的报错(启动失败/连不上/
                    // 任何 fail|error|panic)传回服务器;登录前也传(匿名口)。日志路径只有这层拿得到。
                    state.spawn_diagnostics_flush(
                        codex_plus_core::diagnostic_log::diagnostic_log_path(),
                        env!("CARGO_PKG_VERSION"),
                    );
                    state
                },
            }),
        }
    }
}

impl LauncherHooks {
    fn watchdog_bridge_context(&self) -> anyhow::Result<BridgeContext> {
        self.bridge_context
            .lock()
            .map_err(|_| anyhow::anyhow!("bridge context lock poisoned"))?
            .clone()
            .ok_or_else(|| anyhow::anyhow!("bridge context is not initialized"))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    // 安装器收尾时调用:`--import-installer-tag <安装包路径>`。安装包在下发时被打上
    // 「来自哪个站点」的标签(签名后追加,见 recodex_integration::installer_tag),
    // 这里读出来写进 api-base,这台机器从此知道自己归哪个代理站,登录直接打开
    // 对应站点的授权页。读不到标签(主站直下、老安装包)什么都不做,静默退出。
    if let Some(pos) = args.iter().position(|arg| arg == "--import-installer-tag") {
        if let Some(path) = args.get(pos + 1) {
            // 线索是客户端侧读到的,持久化前由平台确认域名(persist_api_base_if_trusted)。
            let imported = recodex_integration::installer_tag::read_portal(std::path::Path::new(path))
                .filter(|_| !recodex_integration::desktop::portal_known())
                .map(|origin| recodex_integration::desktop::persist_api_base_if_trusted(&origin))
                .unwrap_or(false);
            let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
                "launcher.installer_tag_import",
                json!({ "imported": imported }),
            );
        }
        return Ok(());
    }
    let helper_only = args.iter().any(|arg| arg == "--helper-only");
    // 这里是唯一允许弹窗的地方:user_alert 默认关闭,免得任何链接了 codex-plus-core
    // 的东西(尤其是集成测试里故意触发错误路径的用例)往用户桌面上弹框。
    codex_plus_core::user_alert::enable();
    let options = parse_launch_options(args.iter());
    if let Err(error) = launcher_main(args, helper_only, options.clone()).await {
        let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
            "launcher.failed",
            json!({
                "message": error.to_string()
            }),
        );
        if !helper_only {
            let _ = options.status_store.save_latest(&LaunchStatus {
                status: "failed".to_string(),
                message: error.to_string(),
                started_at_ms: current_timestamp_ms(),
                debug_port: Some(options.debug_port),
                helper_port: Some(options.helper_port),
                codex_app: options
                    .app_dir
                    .map(|path| path.to_string_lossy().to_string()),
            });
        }
        return Err(error);
    }
    Ok(())
}

/// recodex-overlay: 把顶层 `model` 跟到上游 manifest 的推荐值。
///
/// 为什么要替用户改:装了 ReCodex 之后 Codex 以自定义 provider 接入,多数机器上
/// 它**根本不从我们的网关拉模型列表**(线上实测:某用户 6349 次 `/responses`、
/// 0 次 `/models`),于是上游新出的模型对他不可见 —— 只能靠人告诉他名字,
/// 再手工去改 config.toml。这里替他改掉。
///
/// 推荐值完全由 manifest 推导(priority 最小的可见模型),我们**不维护任何模型
/// 清单**:上游上新模型自带 priority=1,下次启动就跟上,不用改配置也不用发版。
/// 档位也不用判断 —— manifest 是上游按该账号权限裁剪后下发的,Plus 号的列表里
/// 根本没有 Pro 专属模型。
///
/// 三条自保:
///   - 用户自己写过 `model`(那一行没有我们的标记)→ 直接返回,连网络都不发;
///   - 拉不到 / 超时 / manifest 解析不了 → 保持现状,绝不动他的配置;
///   - 5 秒超时,不为这件事拖慢启动。
async fn follow_upstream_recommended_model() {
    let Ok(config_path) = recodex_integration::codexcfg::config_path() else {
        return;
    };
    let content = std::fs::read_to_string(&config_path).unwrap_or_default();
    if !recodex_integration::codexcfg::model_is_managed(&content) {
        return;
    }
    let Some(home) = config_path.parent().map(Path::to_path_buf) else {
        return;
    };
    let client_version = codex_plus_core::app_paths::resolve_codex_app_dir_with_saved(None, None)
        .as_deref()
        .and_then(codex_plus_core::app_paths::codex_app_version)
        .unwrap_or_default();
    let env: std::collections::HashMap<String, String> = std::env::vars().collect();
    let Some(model) = codex_plus_core::model_catalog::recommended_model_for_home(
        &home,
        &env,
        &client_version,
        std::time::Duration::from_secs(5),
    )
    .await
    else {
        return;
    };
    match recodex_integration::codexcfg::apply_managed_model(&model) {
        Ok(true) => {
            let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
                "launcher.recodex_model_followed_upstream",
                json!({ "model": model, "client_version": client_version }),
            );
        }
        Ok(false) => {}
        Err(error) => {
            let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
                "launcher.recodex_model_write_failed",
                json!({ "model": model, "error": error.to_string() }),
            );
        }
    }
}

async fn launcher_main(
    args: Vec<String>,
    helper_only: bool,
    options: LaunchOptions,
) -> Result<()> {
    // recodex-overlay: 必须早于任何子进程(Codex / 微信 app-server)——它们继承本进程
    // 环境块,而环境块是父进程的旧快照;上次登录后 setx 写的新 key 只在注册表里。
    // 拿旧 key 请求网关会被拒(SUBSCRIPTION_NOT_FOUND)。
    // recodex-overlay: 清掉上一轮自更新留下的 .old/.new(那时它们已不再被占用)
    codex_plus_core::selfupdate::cleanup_previous_update();
    if recodex_integration::codexcfg::refresh_key_env_from_user_scope() {
        let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
            "launcher.recodex_key_refreshed_from_user_scope",
            // macOS 上这条**同时**是「launchd 那条通道没生效」的指标,而且比「注册那
            // 一刻有没有报错」更可靠 —— 它看的是最终状态。launchd 注册对了的话,
            // 从 Dock / 访达启动的进程环境里本来就该带着正确的 key,这里根本不会触发。
            // 拿它盯 5005 次 macOS 401 收敛得怎么样(注册失败本身在适配器里是静默的,
            // recodex-integration 不依赖 core,写不了诊断日志)。
            json!({ "os": std::env::consts::OS }),
        );
    }
    if helper_only {
        let hooks = LauncherHooks::default();
        // 用实际绑定端口:请求端口被占时会换一个,拿旧值去 shutdown 会关错对象。
        let helper_port = hooks.start_helper(options.helper_port).await?;
        // --helper-only 是让**外部**按约定端口来连的(协议代理的 base_url 就写在
        // config.toml 里)。换了端口就等于失联:进程活着、日志正常、没人连得上。
        // 与其静默跑一个找不到的 helper,不如当场失败。
        if helper_port != options.helper_port {
            hooks.shutdown_helper(helper_port).await;
            anyhow::bail!(
                "helper 端口 {} 被占用(只能绑到 {helper_port})。请关掉占用该端口的程序后重试。",
                options.helper_port
            );
        }
        std::future::pending::<()>().await;
        hooks.shutdown_helper(helper_port).await;
        return Ok(());
    }
    // recodex-overlay: 让上游新出的模型自动生效。
    // 位置有两个约束:必须在 key 刷新**之后**(拉 manifest 要带 key),
    // 也必须在 helper_only 分支**之后** —— helper 进程根本不启动 Codex,
    // 让它白等一次网络请求只会拖慢每一次 helper 拉起,还会和主进程抢着写
    // 同一份 config.toml。
    follow_upstream_recommended_model().await;
    // recodex-overlay: 由「切换模式/更新后重启」拉起时带 --await-guard —— 旧 launcher
    // 还要 1 秒左右才退出,不等的话会误判成「已有实例」而直接退出,页面就失去后端。
    let await_guard = args.iter().any(|arg| arg == "--await-guard");
    let Some(_guard) = acquire_guard_maybe_waiting(options.debug_port, await_guard)? else {
        activate_existing_codex_app(&options).await?;
        options.status_store.save_latest(&LaunchStatus {
            status: "running".to_string(),
            message: "Existing Codex instance activated".to_string(),
            started_at_ms: current_timestamp_ms(),
            debug_port: Some(options.debug_port),
            helper_port: Some(options.helper_port),
            codex_app: options
                .app_dir
                .map(|path| path.to_string_lossy().to_string()),
        })?;
        return Ok(());
    };
    tokio::spawn(async {
        let _ = notify_manager_when_update_available().await;
    });
    // recodex-overlay: 微信连接按已保存设置自动拉起(原由 manager 负责)
    codex_plus_core::connect::control::start_from_saved_settings();
    let hooks = LauncherHooks::default();
    let handle = launch_and_inject_with_hooks(options, &hooks).await?;
    handle.wait_for_codex_exit().await?;
    Ok(())
}

fn current_timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// 等旧实例释放单实例锁(最多约 10 秒);正常双击启动不受影响(wait=false 直接判断)。
fn acquire_guard_maybe_waiting(
    debug_port: u16,
    wait: bool,
) -> anyhow::Result<Option<codex_plus_core::ports::LoopbackPortGuard>> {
    if !wait {
        return acquire_single_instance_guard(debug_port);
    }
    for _ in 0..40 {
        if let Some(guard) = acquire_single_instance_guard(debug_port)? {
            return Ok(Some(guard));
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    acquire_single_instance_guard(debug_port)
}

fn acquire_single_instance_guard(
    debug_port: u16,
) -> anyhow::Result<Option<codex_plus_core::ports::LoopbackPortGuard>> {
    acquire_single_instance_guard_with_retry(debug_port, true)
}

fn acquire_single_instance_guard_with_retry(
    debug_port: u16,
    allow_stale_recovery: bool,
) -> anyhow::Result<Option<codex_plus_core::ports::LoopbackPortGuard>> {
    match try_acquire_single_instance_guard() {
        Ok(guard) => {
            if let Some(fallback_lock_path) = guard.fallback_path() {
                log_launcher_guard_fallback(fallback_lock_path);
            }
            Ok(Some(guard))
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            log_launcher_already_running(debug_port);
            Ok(None)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            log_launcher_already_running(debug_port);
            if allow_stale_recovery && should_recover_stale_launcher(debug_port) {
                codex_plus_core::watcher::stop_launcher_processes();
                std::thread::sleep(std::time::Duration::from_millis(250));
                return acquire_single_instance_guard_with_retry(debug_port, false);
            }
            Ok(None)
        }
        Err(error) => Err(error)
            .with_context(|| {
                format!(
                    "failed to acquire launcher guard port {}",
                    codex_plus_core::ports::launcher_guard_port()
                )
            })
            .map(Some),
    }
}

fn try_acquire_single_instance_guard() -> std::io::Result<codex_plus_core::ports::LoopbackPortGuard>
{
    codex_plus_core::ports::acquire_resilient_loopback_port_guard(
        codex_plus_core::ports::launcher_guard_port(),
    )
}

fn log_launcher_guard_fallback(fallback_lock_path: &Path) {
    let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
        "launcher.guard_fallback",
        json!({
            "requested_guard_port": codex_plus_core::ports::launcher_guard_port(),
            "fallback_lock_path": fallback_lock_path
        }),
    );
}

fn should_recover_stale_launcher(debug_port: u16) -> bool {
    let has_codex_process = !codex_plus_core::watcher::find_codex_processes().is_empty();
    let cdp_listening = codex_plus_core::watcher::cdp_listening(debug_port);
    let recover =
        codex_plus_core::watcher::should_recover_stale_launcher(has_codex_process, cdp_listening);
    let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
        "launcher.stale_recovery_check",
        json!({
            "debug_port": debug_port,
            "has_codex_process": has_codex_process,
            "cdp_listening": cdp_listening,
            "recover": recover
        }),
    );
    recover
}

async fn activate_existing_codex_app(options: &LaunchOptions) -> anyhow::Result<()> {
    let hooks = LauncherHooks::default();
    let mut helper_port = hooks.select_helper_port(options.helper_port);
    let settings = hooks.load_settings().await?;
    let app_dir = hooks.resolve_app_dir(options.app_dir.as_deref(), &settings)?;
    let has_pending_recovery = hooks.has_pending_remote_control_session_recoveries();
    let blocking_process_ids = if has_pending_recovery {
        codex_plus_core::watcher::find_session_index_cleanup_blocking_processes()
    } else {
        Vec::new()
    };
    if should_finalize_pending_remote_control_recovery(has_pending_recovery, &blocking_process_ids)
    {
        hooks.run_remote_control_session_recovery().await?;
    } else if has_pending_recovery {
        let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
            "launcher.remote_control_session_finalization_deferred_existing_app",
            json!({"blocking_process_ids": blocking_process_ids}),
        );
    }
    let launch_result = hooks
        .launch_codex(
            &app_dir,
            options.debug_port,
            &settings,
            &settings.codex_extra_args,
        )
        .await;
    if settings.enhancements_enabled {
        // 接住实际绑定端口:下面的 ensure_injection / start_bridge_watchdog 都用它,
        // 换过端口还拿旧值 = 注入和看门狗一直连一个没人监听的地址。
        helper_port = hooks.start_helper(helper_port).await?;
    }
    let process_ids = codex_plus_core::watcher::find_codex_processes();
    #[cfg(windows)]
    let activated = process_ids
        .iter()
        .copied()
        .any(codex_plus_core::windows_activate_process_window);
    #[cfg(not(windows))]
    let activated = false;
    let injection_ready = if settings.enhancements_enabled {
        hooks
            .ensure_injection(options.debug_port, helper_port, &app_dir)
            .await
    } else {
        false
    };
    if injection_ready {
        hooks
            .start_bridge_watchdog(options.debug_port, helper_port)
            .await?;
        hooks.write_status("running").await;
    } else if settings.enhancements_enabled {
        hooks.write_status("running_degraded").await;
        // 「激活已在运行的 Codex」这条路同样会降级,别让它成为唯一不吭声的分支。
        //
        // 用**阻塞**版:这条路返回之后 launcher_main 紧跟着就 `return Ok(())`,
        // 进程随即退出。Windows 上那条弹窗线程会跟着被掐掉,对话框一闪而过 ——
        // 判据不是「是不是致命错误」,而是「提示之后进程还活不活着」。
        codex_plus_core::user_alert::alert_once_blocking(
            "ReCodex 增强功能未启动",
            "已经切回正在运行的 Codex,但汉化、宠物、侧边栏等增强功能没能接上。
请先完全退出 Codex,再用 ReCodex 重新启动;若仍然不行请联系客服。",
        );
    }
    let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
        "launcher.activate_existing_codex",
        json!({
            "app_dir": app_dir.to_string_lossy(),
            "debug_port": options.debug_port,
            "helper_port": helper_port,
            "requested_helper_port": options.helper_port,
            "process_ids": process_ids,
            "activated": activated,
            "injection_ready": injection_ready,
            "launch_ok": launch_result.is_ok(),
            "launch_error": launch_result.as_ref().err().map(|error| error.to_string())
        }),
    );
    launch_result.map(|_| ())
}

fn should_finalize_pending_remote_control_recovery(
    has_pending_recovery: bool,
    blocking_process_ids: &[u32],
) -> bool {
    has_pending_recovery && blocking_process_ids.is_empty()
}

fn log_launcher_already_running(debug_port: u16) {
    let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
        "launcher.already_running",
        json!({
            "guard_port": codex_plus_core::ports::launcher_guard_port(),
            "debug_port": debug_port
        }),
    );
}

async fn notify_manager_when_update_available() -> anyhow::Result<bool> {
    let update =
        codex_plus_core::update::check_for_update(codex_plus_core::version::VERSION).await?;
    if !update.update_available {
        return Ok(false);
    }
    open_manager_with_update_prompt()?;
    Ok(true)
}

fn open_manager_with_update_prompt() -> anyhow::Result<()> {
    codex_plus_core::install::spawn_companion(
        codex_plus_core::install::MANAGER_BINARY,
        ["--show-update"],
    )
    .map(|_| ())
    .map_err(|error| anyhow::anyhow!("启动管理工具失败：{error}"))
}

fn parse_launch_options<I, S>(args: I) -> LaunchOptions
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut options = LaunchOptions::default();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_ref() {
            "--app-path" => {
                if let Some(value) = iter.next() {
                    let value = value.as_ref().trim();
                    if !value.is_empty() {
                        options.app_dir = Some(PathBuf::from(value));
                    }
                }
            }
            "--debug-port" => {
                if let Some(value) = iter.next() {
                    if let Ok(port) = value.as_ref().parse::<u16>() {
                        options.debug_port = port;
                    }
                }
            }
            "--helper-port" => {
                if let Some(value) = iter.next() {
                    if let Ok(port) = value.as_ref().parse::<u16>() {
                        options.helper_port = port;
                    }
                }
            }
            _ => {}
        }
    }
    options
}

#[async_trait::async_trait(?Send)]
impl LaunchHooks for LauncherHooks {
    fn resolve_app_dir(
        &self,
        app_dir: Option<&std::path::Path>,
        settings: &codex_plus_core::settings::BackendSettings,
    ) -> anyhow::Result<std::path::PathBuf> {
        self.core.resolve_app_dir(app_dir, settings)
    }

    fn select_debug_port(&self, requested: u16) -> u16 {
        self.core.select_debug_port(requested)
    }

    fn select_helper_port(&self, requested: u16) -> u16 {
        self.core.select_helper_port(requested)
    }

    async fn load_settings(&self) -> anyhow::Result<codex_plus_core::settings::BackendSettings> {
        self.core.load_settings().await
    }

    async fn run_provider_sync(&self) -> anyhow::Result<()> {
        let _ = tokio::task::spawn_blocking(|| codex_plus_data::run_provider_sync(None))
            .await
            .map_err(|error| anyhow::anyhow!("provider sync task failed: {error}"))?;
        Ok(())
    }

    fn has_pending_remote_control_session_recoveries(&self) -> bool {
        codex_plus_core::paths::default_pending_remote_control_recovery_path().exists()
    }

    fn remote_control_session_recovery_is_safe_to_run(&self) -> bool {
        codex_plus_core::watcher::find_session_index_cleanup_blocking_processes().is_empty()
    }

    async fn run_remote_control_session_recovery(&self) -> anyhow::Result<()> {
        let outcomes = tokio::task::spawn_blocking(|| {
            let requests = codex_plus_core::remote_control_recovery::load_pending_remote_control_recoveries(None)?;
            let settings = codex_plus_core::settings::SettingsStore::default()
                .load()?;
            let mut outcomes = Vec::with_capacity(requests.len());
            for request in requests {
                let current_profile = settings
                    .relay_profiles
                    .iter()
                    .find(|profile| profile.id == request.profile_id);
                let request_is_current = settings.active_relay_id == request.profile_id
                    && current_profile.is_some_and(|profile| {
                    codex_plus_core::remote_control_recovery::config_generation(
                        profile,
                        &request.target_provider,
                    ) == request.config_generation
                });
                if !request_is_current {
                    outcomes.push((
                        request,
                        codex_plus_data::ProviderSyncResult {
                            status: codex_plus_data::ProviderSyncStatus::Skipped,
                            message: "Remote Control session finalization deferred after relay profile changed".to_string(),
                            target_provider: String::new(),
                            backup_dir: None,
                            changed_session_files: 0,
                            sqlite_rows_updated: 0,
                            sqlite_provider_rows_updated: 0,
                            sqlite_user_event_rows_updated: 0,
                            sqlite_cwd_rows_updated: 0,
                            sqlite_catalog_rows_inserted: 0,
                            sqlite_catalog_rows_removed: 0,
                            updated_workspace_roots: 0,
                            skipped_locked_rollout_files: Vec::new(),
                            encrypted_content_warning: None,
                        },
                        None,
                    ));
                    continue;
                }
                let result = codex_plus_data::run_remote_control_session_finalization_for_thread_with_target(
                    None,
                    &request.thread_id,
                    &request.target_provider,
                );
                let completed = result.status == codex_plus_data::ProviderSyncStatus::Synced;
                let completion_error = if completed {
                    codex_plus_core::remote_control_recovery::complete_pending_remote_control_recovery(
                        None,
                        &request.thread_id,
                    )
                    .err()
                    .map(|error| error.to_string())
                } else {
                    None
                };
                outcomes.push((request, result, completion_error));
            }
            Ok::<_, anyhow::Error>(outcomes)
        })
        .await
        .map_err(|error| anyhow::anyhow!("Remote Control session recovery task failed: {error}"))?;
        match outcomes {
            Ok(outcomes) => {
                for (request, result, completion_error) in outcomes {
                    let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
                        "launcher.remote_control_session_finalization",
                        json!({
                            "thread_id": request.thread_id,
                            "profile_id": request.profile_id,
                            "target_provider": request.target_provider,
                            "config_generation": request.config_generation,
                            "status": result.status,
                            "message": result.message,
                            "completion_error": completion_error
                        }),
                    );
                }
            }
            Err(error) => {
                let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
                    "launcher.remote_control_session_finalization_failed_nonfatal",
                    json!({"message": error.to_string()}),
                );
            }
        }
        Ok(())
    }

    async fn apply_active_relay_profile(
        &self,
        settings: &codex_plus_core::settings::BackendSettings,
    ) -> anyhow::Result<()> {
        self.core.apply_active_relay_profile(settings).await
    }

    async fn ensure_plugin_marketplace_config(
        &self,
        settings: &codex_plus_core::settings::BackendSettings,
    ) -> anyhow::Result<()> {
        self.core.ensure_plugin_marketplace_config(settings).await
    }

    async fn start_helper(&self, helper_port: u16) -> anyhow::Result<u16> {
        self.core.start_helper(helper_port).await
    }

    async fn launch_codex(
        &self,
        app_dir: &Path,
        debug_port: u16,
        settings: &codex_plus_core::settings::BackendSettings,
        extra_args: &[String],
    ) -> anyhow::Result<codex_plus_core::launcher::CodexLaunch> {
        // Codex 子进程继承的是**本进程**的环境。启动器活着期间用户可能登录/换组织,
        // RECODEX_KEY 在用户级环境里已经换了,而本进程环境还是启动时那份 ——
        // 这时拉起的 Codex 拿到的是旧 Key 甚至没有 Key(线上 api_key_required 每 2 分钟一次)。
        // 每次拉起前从用户级环境刷一遍,子进程才拿到当前有效的那把。
        if recodex_integration::codexcfg::refresh_key_env_from_user_scope() {
            let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
                "launcher.recodex_key_refreshed_before_launch",
                json!({ "os": std::env::consts::OS }),
            );
        }
        self.core
            .launch_codex(app_dir, debug_port, settings, extra_args)
            .await
    }

    async fn bridge_context(
        &self,
        debug_port: u16,
        app_dir: &Path,
    ) -> anyhow::Result<Option<BridgeContext>> {
        self.runtime.set_debug_port(debug_port);
        let ctx = BridgeContext::core_with_data_and_app_dir(
            self.runtime.clone(),
            self.data.clone(),
            app_dir.to_path_buf(),
        )
        .with_recodex(self.recodex.clone()); // recodex-overlay:wire
        *self
            .bridge_context
            .lock()
            .map_err(|_| anyhow::anyhow!("bridge context lock poisoned"))? = Some(ctx.clone());
        Ok(Some(ctx))
    }

    async fn inject_bridge(
        &self,
        debug_port: u16,
        helper_port: u16,
        ctx: BridgeContext,
    ) -> anyhow::Result<()> {
        inject_with_context(debug_port, helper_port, ctx, self.runtime.clone()).await
    }

    async fn inject(&self, debug_port: u16, helper_port: u16) -> anyhow::Result<()> {
        self.core.inject(debug_port, helper_port).await
    }

    async fn start_bridge_watchdog(&self, debug_port: u16, helper_port: u16) -> anyhow::Result<()> {
        let ctx = self.watchdog_bridge_context()?;
        let runtime = self.runtime.clone();
        let reinjector: BridgeReinjector = Arc::new(move || {
            let ctx = ctx.clone();
            let runtime = runtime.clone();
            Box::pin(
                async move { inject_with_context(debug_port, helper_port, ctx, runtime).await },
            )
        });
        self.core.set_bridge_reinjector(reinjector).await;

        // 桥被判定为彻底断掉之后的恢复动作：停掉没有 CDP 的 Codex，再由启动器
        // 带着调试端口重新拉起。
        //
        // 这正是原来那个弹窗要用户手工做的事（「请先退出 Codex，再用 ReCodex
        // 重新启动」）。线上诊断上报里这一类占了一半（281 条里 146 条），根因
        // 全是 CDP 端口连接被拒 —— Codex 不是被我们拉起的，端口根本不存在，
        // 光靠重新注入永远修不好。既然我们做得到，就不该让用户去做。
        let recovery: codex_plus_core::launcher::BridgeRecovery = Arc::new(move || {
            Box::pin(async move {
                // 和 /restart-codex 走同一条路：拉起接班的 launcher，然后**本进程必须退出**。
                // 不退的话两个 launcher 会抢单实例锁，接班那个会以为已有实例在跑，
                // 直接退出 —— 于是谁都没带起调试端口，桥还是断的。
                codex_plus_core::watcher::restart_with_fresh_launcher()
                    .context("restart codex to restore the bridge")?;
                tokio::spawn(async {
                    // 留出时间让接班进程起来、也让本轮诊断日志落盘。
                    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
                    std::process::exit(0);
                });
                Ok(())
            })
        });
        self.core.set_bridge_recovery(recovery).await;

        self.core
            .start_bridge_watchdog(debug_port, helper_port)
            .await
    }

    async fn write_status(&self, status: &str) {
        self.core.write_status(status).await;
    }

    async fn wait_for_codex_exit(
        &self,
        launch: &codex_plus_core::launcher::CodexLaunch,
        debug_port: u16,
    ) -> anyhow::Result<()> {
        self.core.wait_for_codex_exit(launch, debug_port).await
    }

    async fn shutdown_helper(&self, helper_port: u16) {
        self.core.shutdown_helper(helper_port).await;
    }

    async fn terminate_codex(&self, launch: &codex_plus_core::launcher::CodexLaunch) {
        self.core.terminate_codex(launch).await;
    }
}

#[derive(Debug, Clone)]
struct LauncherDataService {
    db_path: PathBuf,
    backup_dir: PathBuf,
}

impl Default for LauncherDataService {
    fn default() -> Self {
        Self {
            db_path: default_codex_db_path(),
            backup_dir: codex_plus_core::paths::default_app_state_dir().join("backups"),
        }
    }
}

#[async_trait::async_trait]
impl BridgeDataService for LauncherDataService {
    async fn delete(&self, session: SessionRef) -> anyhow::Result<DeleteResult> {
        let db_paths = self.candidate_db_paths();
        let backup_store = codex_plus_data::BackupStore::new(self.backup_dir.clone());
        tokio::task::spawn_blocking(move || {
            codex_plus_data::delete_local_from_paths(db_paths, backup_store, &session)
        })
        .await
        .map_err(|error| anyhow::anyhow!("delete task failed: {error}"))
    }

    async fn undo(&self, undo_token: String) -> anyhow::Result<DeleteResult> {
        let adapter = self.storage_adapter();
        tokio::task::spawn_blocking(move || adapter.undo(&undo_token))
            .await
            .map_err(|error| anyhow::anyhow!("undo task failed: {error}"))
    }

    async fn export_markdown(&self, session: SessionRef) -> anyhow::Result<ExportResult> {
        let db_paths = self.candidate_db_paths();
        tokio::task::spawn_blocking(move || {
            codex_plus_data::export_markdown_from_paths(db_paths, &session)
        })
        .await
        .map_err(|error| anyhow::anyhow!("export markdown task failed: {error}"))
    }

    async fn thread_usage_history(&self, session: SessionRef) -> anyhow::Result<Value> {
        let adapter = self.storage_adapter();
        tokio::task::spawn_blocking(move || adapter.codex_thread_usage_history(&session))
            .await
            .map_err(|error| anyhow::anyhow!("thread usage history task failed: {error}"))
    }

    async fn find_archived_thread_by_title(
        &self,
        title: String,
    ) -> anyhow::Result<Option<SessionRef>> {
        let adapter = self.storage_adapter();
        tokio::task::spawn_blocking(move || adapter.find_archived_thread_by_title(&title))
            .await
            .map_err(|error| anyhow::anyhow!("archived lookup task failed: {error}"))
    }

    async fn recover_remote_control_session(&self, thread_id: String) -> anyhow::Result<Value> {
        let settings = codex_plus_core::settings::SettingsStore::default()
            .load()
            .unwrap_or_default();
        let profile = settings.active_relay_profile();
        if !settings.relay_profiles_enabled
            || profile.relay_mode != codex_plus_core::settings::RelayMode::Official
            || !profile.official_mix_api_key
        {
            return Ok(json!({
                "status": "skipped",
                "message": "Remote Control session recovery is disabled for the active profile"
            }));
        }
        let home = codex_plus_core::codex_sqlite::default_codex_home_dir();
        let target_provider =
            codex_plus_core::model_catalog::codex_model_provider_for_relay_profile(&home, &profile);
        if target_provider.trim().is_empty() || target_provider == "openai" {
            return Ok(json!({
                "status": "skipped",
                "message": "Remote Control session recovery requires a non-openai target provider"
            }));
        }
        let candidate_thread_id = thread_id.clone();
        let candidate = tokio::task::spawn_blocking(move || {
            codex_plus_data::remote_control_session_recovery_candidate_exists(
                None,
                &candidate_thread_id,
            )
        })
        .await
        .map_err(|error| anyhow::anyhow!("Remote Control candidate check failed: {error}"))??;
        if !candidate {
            return Ok(json!({
                "status": "skipped",
                "message": "Remote Control session recovery is waiting for a recent openai thread"
            }));
        }
        let request = codex_plus_core::remote_control_recovery::PendingRemoteControlRecovery {
            thread_id: thread_id.clone(),
            profile_id: profile.id.clone(),
            target_provider: target_provider.clone(),
            config_generation: codex_plus_core::remote_control_recovery::config_generation(
                &profile,
                &target_provider,
            ),
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64,
        };
        codex_plus_core::remote_control_recovery::enqueue_pending_remote_control_recovery(
            None, request,
        )?;
        tokio::task::spawn_blocking(move || {
            serde_json::to_value(
                codex_plus_data::run_remote_control_session_catalog_recovery_for_thread_with_target(
                    None,
                    &thread_id,
                    &target_provider,
                ),
            )
            .map_err(anyhow::Error::from)
        })
        .await
        .map_err(|error| anyhow::anyhow!("Remote Control session recovery task failed: {error}"))?
    }
}

impl LauncherDataService {
    fn candidate_db_paths(&self) -> Vec<PathBuf> {
        let mut paths = vec![self.db_path.clone()];
        for path in codex_plus_core::codex_sqlite::codex_session_db_paths_from_home(
            &codex_plus_core::codex_sqlite::default_codex_home_dir(),
        ) {
            if !paths.iter().any(|candidate| candidate == &path) {
                paths.push(path);
            }
        }
        paths
    }

    fn storage_adapter(&self) -> codex_plus_data::SQLiteStorageAdapter {
        let allowed_db_paths = self.candidate_db_paths();
        codex_plus_data::SQLiteStorageAdapter::new(
            self.db_path.clone(),
            codex_plus_data::BackupStore::new(self.backup_dir.clone()),
        )
        .with_allowed_db_paths(allowed_db_paths)
    }
}

struct LauncherRuntimeService {
    debug_port: Mutex<u16>,
    websocket_url: Mutex<Option<String>>,
    user_scripts: UserScriptManager,
}

impl LauncherRuntimeService {
    fn new(debug_port: u16, user_scripts: UserScriptManager) -> Self {
        Self {
            debug_port: Mutex::new(debug_port),
            websocket_url: Mutex::new(None),
            user_scripts,
        }
    }

    fn set_debug_port(&self, debug_port: u16) {
        *self.debug_port.lock().unwrap() = debug_port;
    }

    fn set_websocket_url(&self, websocket_url: &str) {
        *self.websocket_url.lock().unwrap() = Some(websocket_url.to_string());
    }
}

#[async_trait::async_trait]
impl BridgeRuntimeService for LauncherRuntimeService {
    async fn user_script_inventory(&self) -> anyhow::Result<Value> {
        self.user_scripts.inventory()
    }

    async fn user_script_inventory_with_runtime_status(
        &self,
        payload: Value,
    ) -> anyhow::Result<Value> {
        self.user_scripts
            .inventory_with_runtime_status(payload.get("runtime_status"))
    }

    async fn set_user_scripts_enabled(&self, enabled: bool) -> anyhow::Result<Value> {
        self.user_scripts.set_global_enabled(enabled)?;
        self.user_scripts.inventory()
    }

    async fn set_user_script_enabled(&self, key: String, enabled: bool) -> anyhow::Result<Value> {
        self.user_scripts.set_script_enabled(&key, enabled)?;
        self.user_scripts.inventory()
    }

    async fn delete_user_script(&self, key: String) -> anyhow::Result<Value> {
        self.user_scripts.delete_user_script(&key)?;
        self.user_scripts.inventory()
    }

    async fn reload_user_scripts(&self) -> anyhow::Result<Value> {
        let bundle = self.user_scripts.build_enabled_bundle()?;
        let websocket_url = self.websocket_url.lock().unwrap().clone();
        if let Some(websocket_url) = websocket_url.filter(|_| !bundle.trim().is_empty()) {
            codex_plus_core::bridge::evaluate_script(&websocket_url, &bundle).await?;
        }
        self.user_scripts.inventory()
    }

    // recodex-overlay: 只放行 https 链接 —— 注入脚本运行在页面里,
    // 不限制协议等于把 file:// / 自定义协议的启动能力暴露给页面。
    async fn open_external(&self, url: String) -> anyhow::Result<Value> {
        let parsed = url::Url::parse(url.trim())
            .map_err(|error| anyhow::anyhow!("invalid external URL: {error}"))?;
        if parsed.scheme() != "https" {
            anyhow::bail!("only https URLs can be opened externally");
        }
        open_url(parsed.as_str())?;
        Ok(json!({ "status": "ok", "url": parsed.as_str() }))
    }

    // recodex-overlay: 拉起接班的 launcher 后,当前进程延迟退出 —— 先让这次桥调用
    // 把响应回给页面,否则面板会看到连接被掐断而不是成功。
    async fn restart_codex(&self) -> anyhow::Result<Value> {
        codex_plus_core::watcher::restart_with_fresh_launcher()?;
        tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
            std::process::exit(0);
        });
        Ok(json!({ "status": "ok", "message": "Codex 正在重启" }))
    }

    // recodex-overlay: 卸载专用退出 —— 杀 Codex 但**不拉接班进程**。
    // 接班进程会重新锁住待删的 exe,自删脚本必然失败(见 watcher::shutdown_for_uninstall)。
    async fn quit(&self) -> anyhow::Result<Value> {
        codex_plus_core::watcher::shutdown_for_uninstall();
        tokio::spawn(async {
            // 给桥调用留出回包时间,面板才能显示卸载结果
            tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
            std::process::exit(0);
        });
        Ok(json!({ "status": "ok", "message": "ReCodex 正在退出" }))
    }

    async fn open_devtools(&self) -> anyhow::Result<Value> {
        let debug_port = *self.debug_port.lock().unwrap();
        let targets = codex_plus_core::cdp::list_targets(debug_port).await?;
        let target = codex_plus_core::cdp::pick_page_target(&targets)?;
        let url = codex_plus_core::routes::devtools_url(debug_port, &target.id);
        open_url(&url)?;
        Ok(json!({
            "status": "ok",
            "target_id": target.id,
            "url": url
        }))
    }

    async fn open_manager(&self) -> anyhow::Result<Value> {
        let target = codex_plus_core::install::spawn_companion(
            codex_plus_core::install::MANAGER_BINARY,
            std::iter::empty::<&str>(),
        )
        .map_err(|error| anyhow::anyhow!("启动管理工具失败：{error}"))?;
        Ok(json!({
            "status": "ok",
            "path": target
        }))
    }

    async fn open_transient_manager(&self) -> anyhow::Result<Value> {
        let target = codex_plus_core::install::spawn_companion(
            codex_plus_core::install::MANAGER_BINARY,
            ["--transient"],
        )
        .map_err(|error| anyhow::anyhow!("启动管理工具失败：{error}"))?;
        Ok(json!({
            "status": "ok",
            "path": target
        }))
    }

    async fn backend_status(&self) -> anyhow::Result<Value> {
        Ok(
            json!({"status": "ok", "message": "后端已连接", "version": codex_plus_core::version::VERSION}),
        )
    }

    async fn codex_model_catalog(&self) -> anyhow::Result<Value> {
        Ok(codex_plus_core::model_catalog::read_codex_model_catalog().await)
    }

    async fn zed_remote_status(&self) -> anyhow::Result<Value> {
        Ok(codex_plus_core::zed_remote::zed_remote_status())
    }

    async fn resolve_zed_remote_host(&self, payload: Value) -> anyhow::Result<Value> {
        Ok(codex_plus_core::zed_remote::resolve_ssh_target_response(
            &payload,
        ))
    }

    async fn fallback_zed_remote_request(&self, payload: Value) -> anyhow::Result<Value> {
        Ok(codex_plus_core::zed_remote::fallback_open_request_response(
            &payload,
        ))
    }

    async fn open_zed_remote(&self, payload: Value) -> anyhow::Result<Value> {
        Ok(codex_plus_core::zed_remote::open_zed_remote(&payload))
    }

    async fn list_zed_remote_projects(&self, payload: Value) -> anyhow::Result<Value> {
        Ok(codex_plus_core::zed_remote::list_zed_remote_projects_response(&payload))
    }

    async fn remember_zed_remote_project(&self, payload: Value) -> anyhow::Result<Value> {
        Ok(codex_plus_core::zed_remote::remember_zed_remote_project_response(&payload))
    }

    async fn forget_zed_remote_project(&self, payload: Value) -> anyhow::Result<Value> {
        Ok(codex_plus_core::zed_remote::forget_zed_remote_project_response(&payload))
    }

    async fn upstream_worktree_status(&self) -> anyhow::Result<Value> {
        Ok(codex_plus_core::upstream_worktree::status_response())
    }

    async fn upstream_worktree_defaults(&self, payload: Value) -> anyhow::Result<Value> {
        Ok(codex_plus_core::upstream_worktree::defaults_response(
            &payload,
        ))
    }

    async fn upstream_worktree_prepare(&self, payload: Value) -> anyhow::Result<Value> {
        Ok(codex_plus_core::upstream_worktree::prepare_response(
            &payload,
        ))
    }

    async fn upstream_worktree_create(&self, payload: Value) -> anyhow::Result<Value> {
        Ok(codex_plus_core::upstream_worktree::create_response(
            &payload,
        ))
    }
}

async fn inject_with_context(
    debug_port: u16,
    helper_port: u16,
    ctx: BridgeContext,
    runtime: Arc<LauncherRuntimeService>,
) -> anyhow::Result<()> {
    let mut last_error = None;
    for _ in 0..20 {
        match try_inject_with_context(debug_port, helper_port, ctx.clone(), runtime.clone()).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                last_error = Some(error);
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("Codex injection failed")))
}

async fn try_inject_with_context(
    debug_port: u16,
    helper_port: u16,
    ctx: BridgeContext,
    runtime: Arc<LauncherRuntimeService>,
) -> anyhow::Result<()> {
    let targets = codex_plus_core::cdp::list_targets(debug_port).await?;
    let target = codex_plus_core::cdp::pick_injectable_codex_page_target(&targets)?;
    let websocket_url = target
        .web_socket_debugger_url
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("selected CDP target has no websocket URL"))?;
    runtime.set_websocket_url(websocket_url);
    let settings = codex_plus_core::settings::SettingsStore::default()
        .load()
        .unwrap_or_default();
    let script = codex_plus_core::assets::injection_script_with_settings(helper_port, &settings);
    let user_bundle = runtime
        .user_scripts
        .build_enabled_bundle()
        .unwrap_or_default();
    let new_document_scripts = if user_bundle.is_empty() {
        vec![script]
    } else {
        vec![script, user_bundle]
    };
    codex_plus_core::bridge::install_bridge(
        websocket_url,
        codex_plus_core::bridge::BRIDGE_BINDING_NAME,
        Arc::new(move |path, payload| {
            let ctx = ctx.clone();
            Box::pin(async move {
                Ok(codex_plus_core::routes::handle_bridge_request(ctx, &path, payload).await)
            })
        }),
        &new_document_scripts,
    )
    .await
}

fn default_codex_db_path() -> PathBuf {
    codex_plus_core::codex_sqlite::codex_session_db_path()
}

fn open_url(url: &str) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        codex_plus_core::windows_open_url(url)
            .map_err(|error| anyhow::anyhow!("failed to open DevTools URL: {error}"))
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|error| anyhow::anyhow!("failed to open DevTools URL: {error}"))
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|error| anyhow::anyhow!("failed to open DevTools URL: {error}"))
    }

    #[cfg(not(any(windows, target_os = "macos", unix)))]
    {
        let _ = url;
        anyhow::bail!("opening DevTools URL is not supported on this platform")
    }
}

fn default_user_script_manager() -> UserScriptManager {
    let config_dir = default_user_scripts_config_dir();
    UserScriptManager::new(
        builtin_user_scripts_dir(),
        config_dir.join("user_scripts"),
        config_dir.join("user_scripts.json"),
    )
}

// recodex-overlay: 用户脚本配置目录去品牌 `Codex++` → `ReCodex`,
// 并把旧目录整个搬过来 —— 否则用户装的脚本会「凭空消失」。
fn default_user_scripts_config_dir() -> PathBuf {
    let (current, legacy) = if cfg!(windows) {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .or_else(|| {
                directories::BaseDirs::new()
                    .map(|dirs| dirs.home_dir().join("AppData").join("Roaming"))
            })
            .unwrap_or_else(|| PathBuf::from("."));
        (base.join("ReCodex"), base.join("Codex++"))
    } else {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| directories::BaseDirs::new().map(|dirs| dirs.home_dir().join(".config")))
            .unwrap_or_else(|| PathBuf::from(".config"));
        (base.join("ReCodex"), base.join("Codex++"))
    };
    if !current.exists() && legacy.exists() {
        let _ = std::fs::rename(&legacy, &current);
    }
    current
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_launch_options_accepts_manager_forwarded_ports_and_app_path() {
        let options = parse_launch_options([
            "--app-path",
            "C:/Codex/App",
            "--debug-port",
            "9333",
            "--helper-port",
            "57322",
        ]);

        assert_eq!(options.app_dir, Some(PathBuf::from("C:/Codex/App")));
        assert_eq!(options.debug_port, 9333);
        assert_eq!(options.helper_port, 57322);
    }

    #[test]
    fn parse_launch_options_ignores_invalid_ports() {
        let options = parse_launch_options(["--debug-port", "nope", "--helper-port", "70000"]);

        assert_eq!(options.debug_port, LaunchOptions::default().debug_port);
        assert_eq!(options.helper_port, LaunchOptions::default().helper_port);
    }

    #[test]
    fn launcher_uses_single_instance_guard_before_launching() {
        let source = include_str!("main.rs");

        assert!(source.contains("acquire_single_instance_guard(options.debug_port)?"));
        assert!(source.contains("launcher_guard_port"));
        assert!(source.contains("launcher.already_running"));
        assert!(source.contains("Existing Codex instance activated"));
        assert!(source.contains("status: \"failed\".to_string()"));
    }

    #[test]
    fn existing_launcher_path_drains_pending_remote_control_recovery_before_activation() {
        let source = include_str!("main.rs");
        let start = source
            .find("async fn activate_existing_codex_app")
            .expect("existing launcher activation function");
        let body = &source[start..];
        let recovery = body
            .find(
                "let has_pending_recovery = hooks.has_pending_remote_control_session_recoveries()",
            )
            .expect("pending recovery guard");
        let launch = body
            .find("let launch_result = hooks")
            .expect("Codex activation");

        assert!(recovery < launch);
        assert!(body[recovery..launch].contains("find_session_index_cleanup_blocking_processes"));
        assert!(body[recovery..launch].contains("should_finalize_pending_remote_control_recovery"));
        assert!(
            body[recovery..launch].contains("hooks.run_remote_control_session_recovery().await?")
        );
    }

    #[test]
    fn pending_remote_control_finalization_requires_an_idle_desktop() {
        assert!(should_finalize_pending_remote_control_recovery(true, &[]));
        assert!(!should_finalize_pending_remote_control_recovery(false, &[]));
        assert!(!should_finalize_pending_remote_control_recovery(
            true,
            &[42]
        ));
    }

    #[test]
    fn launcher_hooks_forward_runtime_watchdog_and_marketplace_methods() {
        let source = include_str!("main.rs");

        assert!(source.contains("async fn start_bridge_watchdog"));
        assert!(source.contains("self.watchdog_bridge_context()?"));
        assert!(source.contains("set_bridge_reinjector(reinjector)"));
        assert!(source.contains("inject_with_context(debug_port, helper_port, ctx, runtime)"));
        assert!(source.contains("async fn ensure_plugin_marketplace_config"));
        assert!(source.contains("self.core.ensure_plugin_marketplace_config(settings).await"));
    }

    #[tokio::test]
    async fn watchdog_reuses_bridge_context_with_data_service() {
        let test_dir = std::env::temp_dir().join(format!(
            "codex-plus-launcher-watchdog-test-{}",
            std::process::id()
        ));
        let hooks = LauncherHooks {
            core: Arc::new(DefaultLaunchHooks::default()),
            data: Arc::new(LauncherDataService {
                db_path: test_dir.join("state.sqlite"),
                backup_dir: test_dir.join("backups"),
            }),
            runtime: Arc::new(LauncherRuntimeService::new(
                9229,
                UserScriptManager::new(
                    test_dir.join("builtin"),
                    test_dir.join("user"),
                    test_dir.join("settings.json"),
                ),
            )),
            bridge_context: Arc::new(Mutex::new(None)),
            // 桥迁到 launcher 之后新增的字段。这个测试验的是 watchdog 复用桥上下文,
            // 用不到 ReCodex 状态;`from_env()` 在没有配置时会带着 init_error 构造出来,
            // 不会 panic,拿来占位正合适。
            recodex: Arc::new(LauncherRecodexBridge {
                state: recodex_integration::desktop::ReCodexState::from_env(),
            }),
        };

        hooks.bridge_context(9229, &test_dir).await.unwrap();
        let ctx = hooks.watchdog_bridge_context().unwrap();
        let result =
            codex_plus_core::routes::handle_bridge_request(ctx, "/backend/status", json!({})).await;

        assert_ne!(result["message"], "Unknown bridge path");
    }
}

fn builtin_user_scripts_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .map(|path| path.join("user_scripts"))
        .unwrap_or_else(|| PathBuf::from("user_scripts"))
}
