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
    // recodex-overlay: 卸载程序调用的两个入口,都在单实例锁与接班之前处理、做完即退出。
    //   --remote-cleanup   NSIS 卸载程序:停手机远程守护进程、撤开机自启(不删数据);
    //   --legacy-uninstall 1.3.4 前的安装留下的卸载项被改指这里(见 legacy_install),
    //                      跑完老卸载程序后把 recodex.exe 与安装目录一并删掉。
    if args.iter().any(|arg| arg == "--remote-cleanup") {
        let notes = codex_plus_core::phone_remote::uninstall_cleanup();
        let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
            "launcher.remote_cleanup",
            json!({ "notes": notes }),
        );
        return Ok(());
    }
    if args
        .iter()
        .any(|arg| arg == codex_plus_core::legacy_install::LEGACY_UNINSTALL_FLAG)
    {
        if let Err(error) = codex_plus_core::uninstall::run_legacy_uninstall() {
            let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
                "launcher.legacy_uninstall_failed",
                json!({ "error": error.to_string() }),
            );
        }
        return Ok(());
    }
    let helper_only = args.iter().any(|arg| arg == "--helper-only");
    // recodex-overlay: 老安装自更新上来仍叫 codex-plus-plus.exe(自更新只换内容不换文件名)。
    // 复制成同目录的 recodex.exe、改好快捷方式与卸载项,从新名重新拉起,本进程直接退出。
    // 必须早于单实例锁与拉起 Codex;失败时返回 false,照常以旧名运行(下次启动再试)。
    // helper 进程不做:它是被外部按端口约定拉起的,换进程会让对方失联。
    if !helper_only && codex_plus_core::legacy_install::handoff_from_legacy_binary(&args) {
        return Ok(());
    }
    // 这里是唯一允许弹窗的地方:user_alert 默认关闭,免得任何链接了 codex-plus-core
    // 的东西(尤其是集成测试里故意触发错误路径的用例)往用户桌面上弹框。
    codex_plus_core::user_alert::enable();
    let options = parse_launch_options(args.iter());
    if let Err(failure) = launcher_main(args, helper_only, options.clone()).await {
        let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
            "launcher.failed",
            json!({
                "message": failure.error.to_string(),
                "role": if failure.owns_status { "primary" } else { "secondary" },
            }),
        );
        if !helper_only {
            record_launch_failure(&options, &failure);
        }
        return Err(failure.error);
    }
    Ok(())
}

/// launcher_main 的失败,带上「这个进程有没有资格写 latest-status.json」。
///
/// latest-status.json 归**主实例**(单实例锁的持有者)所有:它记着主实例实际在用的
/// 调试/helper 端口,第二个 launcher 靠它找到主实例。第二个 launcher 自己失败
/// (比如激活窗口失败)时拿**请求端口**写一条 failed 进去,就把主实例的真实端口
/// 冲掉了 —— 下一次双击读到的是错的端口。
struct LauncherFailure {
    error: anyhow::Error,
    owns_status: bool,
}

impl From<anyhow::Error> for LauncherFailure {
    fn from(error: anyhow::Error) -> Self {
        Self {
            error,
            owns_status: true,
        }
    }
}

impl LauncherFailure {
    fn secondary(error: anyhow::Error) -> Self {
        Self {
            error,
            owns_status: false,
        }
    }
}

fn record_launch_failure(options: &LaunchOptions, failure: &LauncherFailure) {
    if !failure.owns_status {
        return;
    }
    let _ = options.status_store.save_latest(&LaunchStatus {
        status: "failed".to_string(),
        message: failure.error.to_string(),
        started_at_ms: current_timestamp_ms(),
        debug_port: Some(options.debug_port),
        helper_port: Some(options.helper_port),
        codex_app: options
            .app_dir
            .as_ref()
            .map(|path| path.to_string_lossy().to_string()),
        // AUMID 与错误码已写进 message(见 PackagedActivationFailure::into_error)。
        aumid: None,
    });
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
/// recodex-overlay: 启动时把服务端当前应下发的托管配置同步到本机。
///
/// 背景(2026-09-08):Codex Desktop 用户登录后**永远不会再从服务端拿配置** ——
/// refresh_token 只轮换 token,响应里连 config 字段都没有。20:00 全量开 WS 后,
/// 17 个活跃用户只有 2 个走上 WS,恰好是之后重新登录过的两个;其余 13 人的
/// config.toml 里没有 supports_websockets = true。缺口不止 WS:网关切换、任何
/// 服务端配置变更都到不了桌面端(还有 3 个用户挂在已停用的 sg 网关上)。
///
/// 走的是面板「修复」按钮同一条路(desktop::sync_managed_config →
/// install_login_config),不另写写入逻辑。与按钮唯一的差别是
/// respect_official_mode = true:官方模式下只记快照,不把用户拽回 ReCodex。
///
/// 失败静默(不弹任何 UI),只写诊断日志;每次启动跑一次,没有轮询。
/// 网络调用是阻塞的(ureq,transport 10s 超时),放到 spawn_blocking 里。
async fn sync_managed_config_from_server() {
    let outcome = tokio::task::spawn_blocking(|| {
        let state = recodex_integration::desktop::ReCodexState::from_env();
        recodex_integration::desktop::sync_managed_config(&state, true)
    })
    .await;
    use recodex_integration::desktop::ManagedConfigSync as Sync;
    let (label, error) = match &outcome {
        Ok(Sync::Fetch(adapter_error)) => ("fetch_failed", Some(adapter_error.to_string())),
        Ok(Sync::Write(message)) => ("write_failed", Some(message.clone())),
        Ok(other) => (other.label(), None),
        Err(join_error) => ("panicked", Some(join_error.to_string())),
    };
    // 带 error 字段的会被 diagnostics_flush 自动上报;其余靠 ALWAYS_REPORT 里的
    // 这个事件名传回 —— 没有"applied"的分母,就永远说不清那 13 个人到底拿到没有。
    let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
        "launcher.managed_config_sync",
        json!({ "outcome": label, "error": error }),
    );
}

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
) -> std::result::Result<(), LauncherFailure> {
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
            return Err(anyhow::anyhow!(
                "helper 端口 {} 被占用(只能绑到 {helper_port})。请关掉占用该端口的程序后重试。",
                options.helper_port
            )
            .into());
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
    // recodex-overlay: 先同步服务端托管配置,再跟随推荐模型 —— 两者都写 config.toml,
    // 顺序固定就不会互相冲掉;而且必须在拉起 Codex **之前**:Codex 是启动时读一次
    // config.toml,后台线程写完时它已经拿着旧配置跑了,用户还得再重启一次。
    sync_managed_config_from_server().await;
    follow_upstream_recommended_model().await;
    // recodex-overlay: 由「切换模式/更新后重启」拉起时带 --await-guard —— 旧 launcher
    // 还要 1 秒左右才退出,不等的话会误判成「已有实例」而直接退出,页面就失去后端。
    let await_guard = args.iter().any(|arg| arg == "--await-guard");
    // 两条路都要用它(第二实例可能在等待期间接手成主实例),只建一次:
    // LauncherHooks::default() 会起诊断回传线程,建两次就有两条线程抢同一个水位文件。
    let hooks = LauncherHooks::default();
    let _guard = match acquire_guard_maybe_waiting(options.debug_port, await_guard)? {
        Some(guard) => guard,
        None => {
            // latest-status.json 归主实例所有,这里不写:它记着主实例**实际**的调试/helper
            // 端口,拿本进程的请求值覆盖掉,下一个 launcher 就读不到真实端口了。
            let env = Arc::new(SecondInstanceEnv::new(options.debug_port));
            let outcome = activate_existing_codex_app(&hooks, env.clone(), &options)
                .await
                .map_err(LauncherFailure::secondary)?;
            // 等待期间主实例放开了锁(用户刚关掉 Codex 又马上点图标):接手它,
            // 按主实例继续往下走,而不是空等到超时。
            match env.take_guard() {
                Some(guard) => {
                    let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
                        "launcher.second_instance_took_over",
                        json!({ "outcome": outcome.label() }),
                    );
                    guard
                }
                None => return Ok(()),
            }
        }
    };
    // recodex-overlay: 旧 exe / 旧引用 / 卸载项版本号 / 旧数据残留的清理,后台线程做,不拖慢启动。
    // 必须在拿到单实例锁之后:只有锁的持有者能改快捷方式、删旧 exe;这里也是旧名接班的报到点。
    codex_plus_core::legacy_install::spawn_startup_housekeeping();
    // 这里原先每次启动都无条件去拉
    // https://github.com/BigPizzaV3/CodexPlusPlus/releases/latest/download/latest.json,
    // 是上游遗留。两个问题:
    //   1. 它是**活的网络请求**,防火墙日志、抓包、企业代理里直接暴露上游仓库名 ——
    //      比二进制里那些字符串外露得多;
    //   2. 真判断出「有更新」时它去拉 MANAGER_BINARY,而 slim fork 根本不构建管理工具。
    // 我们真正的自更新走 selfupdate.rs + 服务端下发的清单(routes.rs 的 /self-update),
    // 与这条毫无关系。删掉。
    // recodex-overlay: 微信连接按已保存设置自动拉起(原由 manager 负责)
    codex_plus_core::connect::control::start_from_saved_settings();
    // recodex-overlay: 手机远程「跟随账号」开着就自动接入:没配对则发起配对(手机弹窗),
    // 已配对则保证守护进程在跑。与微信一样放在单实例锁之后,后台进行、不拖慢启动。
    codex_plus_core::phone_remote::start_from_saved_settings();
    // recodex: 扫掉「会话删除」留在索引/侧边栏里的残骸。放在单实例锁之后(只由持锁者做)、
    // 拉起 Codex 之前(Codex 运行中会把内存里的全局状态整份写回);Codex 已在跑则顺延。
    // 同步但**限时**(STARTUP_SWEEP_BUDGET,1.5 秒):备份可能有几百 MB,首次启动不能为它
    // 拖住 Codex。每份备份只轻量分类一次并缓存,没做完的顺延到下次启动。不放后台线程与
    // Codex 并行:Codex 启动时读进内存的全局状态之后会整份写回,清了也白清,而标记已记成
    // 「已清理」,再也不会重试。
    codex_plus_data::sweep_deleted_thread_leftovers_at_startup(
        &codex_plus_core::codex_sqlite::default_codex_home_dir(),
        &codex_plus_core::paths::default_app_state_dir().join("backups"),
    );
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
    let guard = acquire_guard_with(
        debug_port,
        allow_stale_recovery,
        &mut try_acquire_single_instance_guard,
        &SystemStaleLauncherRecovery,
    )?;
    if let Some(fallback_lock_path) = guard.as_ref().and_then(|guard| guard.fallback_path()) {
        log_launcher_guard_fallback(fallback_lock_path);
    }
    Ok(guard)
}

/// 「锁被占了,要不要把占着的 launcher 当残留杀掉」这一步能碰到的外部副作用。
trait StaleLauncherRecovery {
    fn should_recover(&self, debug_port: u16) -> bool;
    fn stop_launcher_processes(&self);
    fn pause(&self);
}

struct SystemStaleLauncherRecovery;

impl StaleLauncherRecovery for SystemStaleLauncherRecovery {
    fn should_recover(&self, debug_port: u16) -> bool {
        should_recover_stale_launcher(debug_port)
    }
    fn stop_launcher_processes(&self) {
        codex_plus_core::watcher::stop_launcher_processes();
    }
    fn pause(&self) {
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// 两种「被占」必须分开对待:
///   - `WouldBlock`:锁文件被持有 —— 一个**活着的**当前版本 launcher 就是主实例。
///     它此刻可能还没有 Codex 进程、也没有 CDP(商店激活带退避重试,最长约 20 秒),
///     「无进程无 CDP」在这里**不代表**它是残留。绝不杀,交给第二实例路径去等/激活。
///   - `AddrInUse`:锁文件拿到了(没有当前版本的 launcher 活着),端口却被占着 ——
///     只可能是不认锁文件的旧版 launcher 或别的程序。这时才按「无 Codex 无 CDP」
///     判定残留并清理,且只试一次。
fn acquire_guard_with<G>(
    debug_port: u16,
    allow_stale_recovery: bool,
    try_acquire: &mut dyn FnMut() -> std::io::Result<G>,
    recovery: &dyn StaleLauncherRecovery,
) -> anyhow::Result<Option<G>> {
    match try_acquire() {
        Ok(guard) => Ok(Some(guard)),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            log_launcher_already_running(debug_port);
            Ok(None)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            log_launcher_already_running(debug_port);
            if allow_stale_recovery && recovery.should_recover(debug_port) {
                recovery.stop_launcher_processes();
                recovery.pause();
                return acquire_guard_with(debug_port, false, try_acquire, recovery);
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

/// 第二个 launcher(单实例锁在主实例手里)只做一件事:把正在跑的 Codex 叫到前台。
///
/// **不**调用 `hooks.launch_codex`:那是主实例的拉起逻辑,里面有「Codex 在跑但
/// CDP 探不通 → 先杀掉再拉起」,第二实例探 CDP 的条件并不可靠(主实例刚拉起、
/// CDP 还没监听;Codex 忙;页面刷新中;状态文件坏了退回请求端口),会把用户正在
/// 用的 Codex 杀掉。这里只经 `ExistingInstanceEnv` 激活窗口,不探 CDP、不杀进程、
/// 不拉起新的 Codex(见 codex_plus_core::existing_instance)。
///
/// 也**不**起自己的 helper、不注入、不起看门狗:本进程马上就退出,旧做法会把页面
/// 的 helperBase 改指到本进程的临时端口,进程一退面板就对着死端口(线上实测一次挂
/// 了约 9 小时)。主实例的 helper 与看门狗一直在,只确认它还活着。
async fn activate_existing_codex_app<H: LaunchHooks>(
    hooks: &H,
    env: Arc<dyn codex_plus_core::existing_instance::ExistingInstanceEnv>,
    options: &LaunchOptions,
) -> anyhow::Result<codex_plus_core::existing_instance::ExistingActivation> {
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
    // 主实例在用的端口以它写的 latest-status.json 为准(见 existing_instance_ports)。
    // 这里只拿 helper 端口确认增强功能活着;调试端口仅作诊断,不据此做任何动作。
    let primary = codex_plus_core::launcher::resolve_existing_instance_ports(
        &options.status_store,
        options.debug_port,
        options.helper_port,
    );
    let activation = {
        let app_dir = app_dir.clone();
        let policy = codex_plus_core::existing_instance::ExistingActivationPolicy::default();
        tokio::task::spawn_blocking(move || {
            codex_plus_core::existing_instance::activate_existing_codex(
                env.as_ref(),
                &app_dir,
                &policy,
            )
        })
        .await
        .map_err(|error| anyhow::anyhow!("existing Codex activation task failed: {error}"))?
    };
    use codex_plus_core::existing_instance::ExistingActivation;
    // 接手成锁的持有者:后面按主实例正常启动,helper 由本进程自己起,这里什么都不用等、
    // 不用提示。
    if activation == ExistingActivation::BecamePrimary {
        let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
            "launcher.activate_existing_codex",
            json!({
                "outcome": activation.label(),
                "requested_debug_port": options.debug_port,
            }),
        );
        return Ok(activation);
    }
    let no_codex = activation == ExistingActivation::NoCodexProcess;
    let primary_helper_port = if settings.enhancements_enabled && !no_codex {
        codex_plus_core::launcher::wait_for_existing_helper(
            &primary.helper_ports,
            std::time::Duration::from_secs(10),
        )
        .await
    } else {
        None
    };
    let helper_available = !settings.enhancements_enabled || primary_helper_port.is_some();
    if no_codex {
        // 主实例还拿着锁却迟迟没有 Codex:多半卡在拉起(商店正在更新 Codex 之类)。
        // 不替它拉 —— 我们拉起的那个没有调试端口,会被主实例当成「无 CDP」杀掉。
        codex_plus_core::user_alert::alert_once_blocking(
            "Codex 正在启动",
            "ReCodex 已经在启动 Codex,但还没有出现窗口。请稍等片刻;
如果一分钟后仍然没有窗口,请在任务管理器里结束 recodex.exe 后重新打开。",
        );
    } else if !helper_available {
        // 用**阻塞**版:这条路返回之后进程随即退出,非阻塞弹窗会一闪而过。
        codex_plus_core::user_alert::alert_once_blocking(
            "ReCodex 增强功能未启动",
            "已经切回正在运行的 Codex,但汉化、宠物、侧边栏等增强功能的后台服务没有在运行。
请先完全退出 Codex,再用 ReCodex 重新启动;若仍然不行请联系客服。",
        );
    }
    let (process_ids, activation_error) = match &activation {
        ExistingActivation::WindowActivated { process_ids }
        | ExistingActivation::AppActivated { process_ids } => (process_ids.clone(), None),
        ExistingActivation::AppActivationFailed { process_ids, error } => {
            (process_ids.clone(), Some(error.clone()))
        }
        ExistingActivation::NoCodexProcess | ExistingActivation::BecamePrimary => (Vec::new(), None),
    };
    let _ = codex_plus_core::diagnostic_log::append_diagnostic_log(
        "launcher.activate_existing_codex",
        json!({
            "app_dir": app_dir.to_string_lossy(),
            "debug_port": primary.debug_port,
            "requested_debug_port": options.debug_port,
            "helper_port": primary_helper_port,
            "helper_candidates": primary.helper_ports,
            "requested_helper_port": options.helper_port,
            "process_ids": process_ids,
            "outcome": activation.label(),
            "activated": matches!(activation, ExistingActivation::WindowActivated { .. } | ExistingActivation::AppActivated { .. }),
            "helper_available": helper_available,
            "activation_error": activation_error,
        }),
    );
    match activation_error {
        Some(error) => Err(anyhow::anyhow!("激活已在运行的 Codex 失败:{error}")),
        None => Ok(activation),
    }
}

/// 第二实例用的环境:系统实现 + 「试着接手单实例锁」。
///
/// guard 必须活到进程结束,所以只能由这一层(launcher)持有;core 的系统实现
/// 永远返回 false。接手时**不做**残留清理:这条路上「无 Codex 进程、无 CDP」是
/// 常态(用户刚关掉 Codex),据此杀别的 launcher 正是第一轮审计 S2 要防的事。
struct SecondInstanceEnv {
    inner: codex_plus_core::existing_instance::SystemExistingInstanceEnv,
    debug_port: u16,
    guard: Mutex<Option<codex_plus_core::ports::LoopbackPortGuard>>,
}

impl SecondInstanceEnv {
    fn new(debug_port: u16) -> Self {
        Self {
            inner: codex_plus_core::existing_instance::SystemExistingInstanceEnv,
            debug_port,
            guard: Mutex::new(None),
        }
    }

    fn take_guard(&self) -> Option<codex_plus_core::ports::LoopbackPortGuard> {
        self.guard.lock().ok().and_then(|mut guard| guard.take())
    }
}

impl codex_plus_core::existing_instance::ExistingInstanceEnv for SecondInstanceEnv {
    fn codex_process_ids(&self) -> Vec<u32> {
        self.inner.codex_process_ids()
    }
    fn activate_process_window(&self, process_id: u32) -> bool {
        self.inner.activate_process_window(process_id)
    }
    fn process_has_window(&self, process_id: u32) -> bool {
        self.inner.process_has_window(process_id)
    }
    fn activate_app(&self, app_dir: &Path) -> anyhow::Result<()> {
        self.inner.activate_app(app_dir)
    }
    fn sleep(&self, duration: std::time::Duration) {
        self.inner.sleep(duration);
    }
    fn try_take_over(&self) -> bool {
        let Ok(mut slot) = self.guard.lock() else {
            return false;
        };
        if slot.is_some() {
            return true;
        }
        match acquire_single_instance_guard_with_retry(self.debug_port, false) {
            Ok(Some(guard)) => {
                *slot = Some(guard);
                true
            }
            _ => false,
        }
    }
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
            codex_plus_data::delete_local_from_paths(
                db_paths,
                backup_store,
                &session,
                Some(&codex_plus_core::codex_sqlite::default_codex_home_dir()),
            )
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
        .with_codex_home(codex_plus_core::codex_sqlite::default_codex_home_dir())
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
            .find("existing_instance::activate_existing_codex(")
            .expect("Codex activation");

        assert!(recovery < launch);
        assert!(body[recovery..launch].contains("find_session_index_cleanup_blocking_processes"));
        assert!(body[recovery..launch].contains("should_finalize_pending_remote_control_recovery"));
        assert!(
            body[recovery..launch].contains("hooks.run_remote_control_session_recovery().await?")
        );
    }

    // ---- 第二实例路径的行为测试(替换原先只做源码字符串断言的守卫) ----

    use codex_plus_core::existing_instance::ExistingInstanceEnv;
    use codex_plus_core::launcher::CodexLaunch;
    use codex_plus_core::settings::BackendSettings;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 主实例拉起逻辑的替身:第二实例路径下这些方法一次都不该被调到。
    #[derive(Default)]
    struct RecordingHooks {
        launch_codex_calls: AtomicUsize,
        terminate_codex_calls: AtomicUsize,
        start_helper_calls: AtomicUsize,
        inject_calls: AtomicUsize,
        write_status_calls: AtomicUsize,
    }

    #[async_trait::async_trait(?Send)]
    impl LaunchHooks for RecordingHooks {
        fn resolve_app_dir(
            &self,
            _app_dir: Option<&Path>,
            _settings: &BackendSettings,
        ) -> anyhow::Result<PathBuf> {
            Ok(PathBuf::from(
                r"C:\Program Files\WindowsApps\OpenAI.Codex_26.915.3509.0_x64__2p2nqsd0c76g0\app",
            ))
        }
        fn select_debug_port(&self, requested: u16) -> u16 {
            requested
        }
        fn select_helper_port(&self, requested: u16) -> u16 {
            requested
        }
        async fn load_settings(&self) -> anyhow::Result<BackendSettings> {
            // 关掉增强功能:否则会去真实端口上等 helper 10 秒。
            Ok(BackendSettings {
                enhancements_enabled: false,
                ..BackendSettings::default()
            })
        }
        async fn run_provider_sync(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn run_remote_control_session_recovery(&self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn start_helper(&self, helper_port: u16) -> anyhow::Result<u16> {
            self.start_helper_calls.fetch_add(1, Ordering::SeqCst);
            Ok(helper_port)
        }
        async fn launch_codex(
            &self,
            _app_dir: &Path,
            _debug_port: u16,
            _settings: &BackendSettings,
            _extra_args: &[String],
        ) -> anyhow::Result<CodexLaunch> {
            // 真实实现在「有进程但 CDP 不通」时会 stop_codex_processes_and_wait()。
            self.launch_codex_calls.fetch_add(1, Ordering::SeqCst);
            anyhow::bail!("second instance must never launch (or restart) Codex")
        }
        async fn inject(&self, _debug_port: u16, _helper_port: u16) -> anyhow::Result<()> {
            self.inject_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn write_status(&self, _status: &str) {
            self.write_status_calls.fetch_add(1, Ordering::SeqCst);
        }
        async fn wait_for_codex_exit(
            &self,
            _launch: &CodexLaunch,
            _debug_port: u16,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        async fn shutdown_helper(&self, _helper_port: u16) {}
        async fn terminate_codex(&self, _launch: &CodexLaunch) {
            self.terminate_codex_calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// 假的进程/窗口状态。场景里 CDP 一律「探不通」—— 第二实例路径根本不该去探它,
    /// env 上也就没有探 CDP 的入口。
    struct FakeProcessEnv {
        process_ids: Vec<u32>,
        window_activates: bool,
        has_window: bool,
        takes_over: bool,
        app_fails: bool,
        window_calls: AtomicUsize,
        app_calls: AtomicUsize,
        take_over_calls: AtomicUsize,
    }

    impl FakeProcessEnv {
        fn new(process_ids: Vec<u32>, window_activates: bool) -> Arc<Self> {
            Arc::new(Self {
                process_ids,
                window_activates,
                has_window: true,
                takes_over: false,
                app_fails: false,
                window_calls: AtomicUsize::new(0),
                app_calls: AtomicUsize::new(0),
                take_over_calls: AtomicUsize::new(0),
            })
        }
        fn taking_over(process_ids: Vec<u32>) -> Arc<Self> {
            Arc::new(Self {
                process_ids,
                window_activates: false,
                has_window: false,
                takes_over: true,
                app_fails: false,
                window_calls: AtomicUsize::new(0),
                app_calls: AtomicUsize::new(0),
                take_over_calls: AtomicUsize::new(0),
            })
        }
        fn failing_activation(process_ids: Vec<u32>) -> Arc<Self> {
            Arc::new(Self {
                process_ids,
                window_activates: false,
                has_window: true,
                takes_over: false,
                app_fails: true,
                window_calls: AtomicUsize::new(0),
                app_calls: AtomicUsize::new(0),
                take_over_calls: AtomicUsize::new(0),
            })
        }
    }

    impl ExistingInstanceEnv for FakeProcessEnv {
        fn codex_process_ids(&self) -> Vec<u32> {
            self.process_ids.clone()
        }
        fn activate_process_window(&self, _process_id: u32) -> bool {
            self.window_calls.fetch_add(1, Ordering::SeqCst);
            self.window_activates
        }
        fn process_has_window(&self, _process_id: u32) -> bool {
            self.has_window
        }
        fn try_take_over(&self) -> bool {
            self.take_over_calls.fetch_add(1, Ordering::SeqCst);
            self.takes_over
        }
        fn activate_app(&self, _app_dir: &Path) -> anyhow::Result<()> {
            self.app_calls.fetch_add(1, Ordering::SeqCst);
            if self.app_fails {
                anyhow::bail!("activation refused");
            }
            Ok(())
        }
        fn sleep(&self, _duration: std::time::Duration) {}
    }

    fn secondary_options(dir: &Path) -> LaunchOptions {
        LaunchOptions {
            status_store: codex_plus_core::status::StatusStore::new(
                dir.join("latest-status.json"),
            ),
            ..LaunchOptions::default()
        }
    }

    fn primary_status(options: &LaunchOptions) -> LaunchStatus {
        LaunchStatus {
            status: "running".to_string(),
            message: "ReCodex launcher ready".to_string(),
            started_at_ms: 1,
            debug_port: Some(options.debug_port),
            helper_port: Some(options.helper_port),
            codex_app: None,
            aumid: None,
        }
    }

    fn assert_primary_runtime_untouched(hooks: &RecordingHooks) {
        assert_eq!(
            hooks.launch_codex_calls.load(Ordering::SeqCst),
            0,
            "不能走拉起/重启 Codex 的逻辑(里面会杀无 CDP 的 Codex)"
        );
        assert_eq!(hooks.terminate_codex_calls.load(Ordering::SeqCst), 0, "不能结束 Codex");
        assert_eq!(hooks.start_helper_calls.load(Ordering::SeqCst), 0, "不能起自己的 helper");
        assert_eq!(hooks.inject_calls.load(Ordering::SeqCst), 0, "不能注入");
        assert_eq!(hooks.write_status_calls.load(Ordering::SeqCst), 0, "不能写状态");
    }

    /// B1:Codex 在跑、CDP 探不通(刚拉起/忙/刷新中/状态文件坏)—— 第二实例只激活窗口。
    #[tokio::test]
    async fn second_instance_with_running_codex_and_no_cdp_only_activates_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let options = secondary_options(dir.path());
        options
            .status_store
            .save_latest(&primary_status(&options))
            .unwrap();
        let before = std::fs::read(dir.path().join("latest-status.json")).unwrap();
        let hooks = RecordingHooks::default();
        let env = FakeProcessEnv::new(vec![4242], true);

        activate_existing_codex_app(&hooks, env.clone(), &options)
            .await
            .unwrap();

        assert_primary_runtime_untouched(&hooks);
        assert_eq!(env.window_calls.load(Ordering::SeqCst), 1);
        assert_eq!(env.app_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            std::fs::read(dir.path().join("latest-status.json")).unwrap(),
            before
        );
    }

    /// B1:状态文件写坏了(读不出端口,退回请求端口)也一样只激活。
    #[tokio::test]
    async fn second_instance_with_corrupt_status_file_still_only_activates() {
        let dir = tempfile::tempdir().unwrap();
        let options = secondary_options(dir.path());
        std::fs::write(dir.path().join("latest-status.json"), b"{not json").unwrap();
        let hooks = RecordingHooks::default();
        let env = FakeProcessEnv::new(vec![11], true);

        activate_existing_codex_app(&hooks, env.clone(), &options)
            .await
            .unwrap();

        assert_primary_runtime_untouched(&hooks);
        assert_eq!(
            std::fs::read(dir.path().join("latest-status.json")).unwrap(),
            b"{not json"
        );
    }

    /// 窗口前置不了(还在建/在托盘):退到系统激活,仍然不碰进程。
    #[tokio::test]
    async fn second_instance_falls_back_to_system_activation_without_killing() {
        let dir = tempfile::tempdir().unwrap();
        let options = secondary_options(dir.path());
        let hooks = RecordingHooks::default();
        let env = FakeProcessEnv::new(vec![7, 8], false);

        activate_existing_codex_app(&hooks, env.clone(), &options)
            .await
            .unwrap();

        assert_primary_runtime_untouched(&hooks);
        assert_eq!(env.app_calls.load(Ordering::SeqCst), 1);
    }

    /// S2 场景:主实例还在激活重试,一个 Codex 进程都没有 —— 第二实例既不拉起
    /// (拉起的那个没有调试端口),也不做系统激活,只等待/提示。
    #[tokio::test]
    async fn second_instance_without_codex_waits_instead_of_launching() {
        let dir = tempfile::tempdir().unwrap();
        let options = secondary_options(dir.path());
        let hooks = RecordingHooks::default();
        let env = FakeProcessEnv::new(Vec::new(), true);

        activate_existing_codex_app(&hooks, env.clone(), &options)
            .await
            .unwrap();

        assert_primary_runtime_untouched(&hooks);
        assert_eq!(env.window_calls.load(Ordering::SeqCst), 0);
        assert_eq!(env.app_calls.load(Ordering::SeqCst), 0);
        assert!(
            !dir.path().join("latest-status.json").exists(),
            "第二实例不能写状态文件"
        );
    }

    struct CountingRecovery {
        recover: bool,
        stops: AtomicUsize,
    }

    impl StaleLauncherRecovery for CountingRecovery {
        fn should_recover(&self, _debug_port: u16) -> bool {
            self.recover
        }
        fn stop_launcher_processes(&self) {
            self.stops.fetch_add(1, Ordering::SeqCst);
        }
        fn pause(&self) {}
    }

    /// S2:锁文件被持有 = 主 launcher 活着。即便此刻「无 Codex 进程、无 CDP」
    /// (它正在重试激活),也绝不能把它当残留杀掉。
    #[test]
    fn held_instance_lock_never_kills_the_primary_launcher() {
        let recovery = CountingRecovery {
            recover: true,
            stops: AtomicUsize::new(0),
        };
        let mut attempts = 0;
        let guard = acquire_guard_with::<()>(
            9229,
            true,
            &mut || {
                attempts += 1;
                Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "held"))
            },
            &recovery,
        )
        .unwrap();
        assert!(guard.is_none());
        assert_eq!(attempts, 1);
        assert_eq!(recovery.stops.load(Ordering::SeqCst), 0);
    }

    /// 锁文件空闲、端口却被占(不认锁文件的旧版 launcher):保留原有的残留清理,只试一次。
    #[test]
    fn port_held_without_lock_still_recovers_a_stale_legacy_launcher_once() {
        let recovery = CountingRecovery {
            recover: true,
            stops: AtomicUsize::new(0),
        };
        let mut attempts = 0;
        let guard = acquire_guard_with::<()>(
            9229,
            true,
            &mut || {
                attempts += 1;
                if attempts == 1 {
                    Err(std::io::Error::new(std::io::ErrorKind::AddrInUse, "port"))
                } else {
                    Ok(())
                }
            },
            &recovery,
        )
        .unwrap();
        assert!(guard.is_some());
        assert_eq!(recovery.stops.load(Ordering::SeqCst), 1);
    }

    /// R4:第二实例失败不写 latest-status.json;主实例失败照写。
    #[test]
    fn only_the_primary_records_a_failed_launch_status() {
        let dir = tempfile::tempdir().unwrap();
        let options = secondary_options(dir.path());
        options
            .status_store
            .save_latest(&primary_status(&options))
            .unwrap();
        let before = std::fs::read(dir.path().join("latest-status.json")).unwrap();

        record_launch_failure(&options, &LauncherFailure::secondary(anyhow::anyhow!("boom")));
        assert_eq!(
            std::fs::read(dir.path().join("latest-status.json")).unwrap(),
            before
        );

        record_launch_failure(&options, &LauncherFailure::from(anyhow::anyhow!("boom")));
        let saved = options.status_store.load_latest().unwrap().unwrap();
        assert_eq!(saved.status, "failed");
        assert_eq!(saved.message, "boom");
    }

    /// R4 的接线(行为版):已有实例分支失败时,错误是 secondary —— 状态文件一个字节
    /// 都不能变(激活前后各验一次)。
    #[tokio::test]
    async fn second_instance_activation_failure_never_touches_the_status_file() {
        let dir = tempfile::tempdir().unwrap();
        let options = secondary_options(dir.path());
        options
            .status_store
            .save_latest(&primary_status(&options))
            .unwrap();
        let before = std::fs::read(dir.path().join("latest-status.json")).unwrap();
        let hooks = RecordingHooks::default();
        let env = FakeProcessEnv::failing_activation(vec![5]);

        let error = activate_existing_codex_app(&hooks, env.clone(), &options)
            .await
            .expect_err("系统激活失败必须向上报错");

        assert_primary_runtime_untouched(&hooks);
        assert_eq!(
            std::fs::read(dir.path().join("latest-status.json")).unwrap(),
            before
        );
        record_launch_failure(&options, &LauncherFailure::secondary(error));
        assert_eq!(
            std::fs::read(dir.path().join("latest-status.json")).unwrap(),
            before,
            "第二实例的失败不能覆盖主实例的状态文件"
        );
    }

    /// 应修 2:等待期间主实例放开了锁 —— 转为主实例(BecamePrimary),不弹提示、
    /// 不做系统激活,也不去等主实例的 helper。
    #[tokio::test]
    async fn second_instance_becomes_primary_when_the_lock_is_released() {
        let dir = tempfile::tempdir().unwrap();
        let options = secondary_options(dir.path());
        let hooks = RecordingHooks::default();
        // 只剩正在退出、没有窗口的 Codex 残留进程。
        let env = FakeProcessEnv::taking_over(vec![4242]);

        let outcome = activate_existing_codex_app(&hooks, env.clone(), &options)
            .await
            .unwrap();

        assert_eq!(
            outcome,
            codex_plus_core::existing_instance::ExistingActivation::BecamePrimary
        );
        assert_primary_runtime_untouched(&hooks);
        assert_eq!(env.app_calls.load(Ordering::SeqCst), 0, "不能做系统激活");
        assert!(env.take_over_calls.load(Ordering::SeqCst) >= 1);
        assert!(!dir.path().join("latest-status.json").exists());
    }

    /// SecondInstanceEnv 真的能拿到锁并把 guard 交出来(锁空闲时)。
    #[test]
    fn second_instance_env_hands_over_the_acquired_guard() {
        use codex_plus_core::existing_instance::ExistingInstanceEnv;
        let env = SecondInstanceEnv::new(codex_plus_core::ports::find_available_loopback_port());
        assert!(env.try_take_over(), "锁空闲时必须拿得到");
        assert!(env.try_take_over(), "已经拿到就直接复用");
        assert!(env.take_guard().is_some(), "guard 必须交给调用方(要活到进程结束)");
        assert!(env.take_guard().is_none(), "只能交出一次");
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

#[cfg(test)]
mod managed_config_sync_placement_tests {
    /// 托管配置同步必须在 launcher_main 里、紧贴在跟随推荐模型之前,
    /// 也就是在拉起 Codex 之前。挪到 launch_and_inject_with_hooks 之后
    /// = Codex 已经拿着旧配置跑起来了,用户还得再重启一次,而表面上"同步做了"。
    ///
    /// 钉的是**真实锚点**(两个 await 的相邻关系),不是文件里的文本先后 ——
    /// 后者在 config_health 那条守卫上被变异测试证明是假的。
    #[test]
    fn managed_config_sync_runs_right_before_model_follow_in_launcher_main() {
        let source = include_str!("main.rs");
        let main_start = source
            .find("async fn launcher_main(")
            .expect("找不到 launcher_main");
        let body = &source[main_start..];
        let sync = body
            .find("sync_managed_config_from_server().await;")
            .expect("launcher_main 里没有调用 sync_managed_config_from_server");
        let follow = body
            .find("follow_upstream_recommended_model().await;")
            .expect("launcher_main 里没有调用 follow_upstream_recommended_model");
        let launch = body
            .find("launch_and_inject_with_hooks(")
            .expect("launcher_main 里没有拉起 Codex");
        assert!(sync < follow && follow < launch,
            "同步(@{sync})必须先于跟随模型(@{follow})、再先于拉起 Codex(@{launch})");
        // 两个 await 之间只允许注释和空白 —— 中间插别的步骤就可能把顺序约束绕开。
        let between = &body[sync..follow];
        assert!(
            between.lines().skip(1).all(|l| { let t = l.trim(); t.is_empty() || t.starts_with("//") }),
            "同步与跟随模型之间不该有别的语句:\n{between}"
        );
    }
}

#[cfg(test)]
mod legacy_handoff_placement_tests {
    /// 旧名接班必须发生在 main 里、`launcher_main` 之前 —— 进了 launcher_main 就会去抢
    /// 单实例锁、拉起 Codex,那时再换进程,新进程会以为「已有实例在跑」而直接退出。
    /// 同时必须跳过 helper 进程。钉文本是因为这段只在 Windows 旧名安装上才走得到。
    #[test]
    fn handoff_runs_in_main_before_launcher_main_and_skips_helpers() {
        let source = include_str!("main.rs");
        let main_start = source.find("async fn main()").expect("找不到 main");
        let body = &source[main_start..];
        // main 里第一处 launcher_main 调用;接班必须在它之前
        let launcher = body.find("launcher_main(args").expect("main 里没有调用 launcher_main");
        let handoff = body[..launcher]
            .find("legacy_install::handoff_from_legacy_binary(&args)")
            .expect("main 里、launcher_main 之前没有调用旧名接班");
        let line = body[..handoff].rsplit('\n').next().unwrap_or_default();
        assert!(line.contains("!helper_only &&"), "helper 进程不能接班:{line}");
    }

    /// 旧 exe 清理/改入口只能由单实例锁的持有者做;它也是旧名接班的报到点。
    #[test]
    fn housekeeping_runs_only_after_the_single_instance_guard() {
        // 按 LF 找函数结尾:Windows 上 core.autocrlf=true 检出的是 CRLF
        let source = include_str!("main.rs").replace("\r\n", "\n");
        let start = source.find("async fn launcher_main(").expect("找不到 launcher_main");
        let body = &source[start..];
        let body = &body[..body.find("\n}\n").expect("launcher_main 没有结尾")];
        let guard = body.find("acquire_guard_maybe_waiting(options.debug_port").expect("没有抢锁");
        let calls: Vec<_> = body.match_indices("spawn_startup_housekeeping();").collect();
        assert_eq!(calls.len(), 1, "launcher_main 里只能调用一次");
        assert!(calls[0].0 > guard, "必须在拿到单实例锁之后");
    }

    /// 启动顺序:单实例锁 → 旧安装清理(接班报到点)→ 手机远程自动接入,各只一次。
    /// 手机远程要是跑在锁前面,第二个实例(只是来激活窗口的)也会去发起配对、拉守护进程。
    #[test]
    fn phone_remote_starts_once_after_guard_and_housekeeping() {
        // 按 LF 找函数结尾:Windows 上 core.autocrlf=true 检出的是 CRLF
        let source = include_str!("main.rs").replace("\r\n", "\n");
        let start = source.find("async fn launcher_main(").expect("找不到 launcher_main");
        let body = &source[start..];
        let body = &body[..body.find("\n}\n").expect("launcher_main 没有结尾")];
        let guard = body.find("acquire_guard_maybe_waiting(options.debug_port").expect("没有抢锁");
        let housekeeping = body.find("spawn_startup_housekeeping();").expect("没有清理");
        // 拼出来,免得这条测试自己的字面量也被数进去
        let remote_call = ["phone_remote::", "start_from_saved_settings();"].concat();
        assert_eq!(source.matches(remote_call.as_str()).count(), 1, "整个启动器里只能调用一次");
        let remote = body.find(remote_call.as_str()).expect("launcher_main 里没有手机远程自动接入");
        assert!(guard < housekeeping && housekeeping < remote, "顺序必须是 锁 → 清理 → 手机远程");
    }
}
