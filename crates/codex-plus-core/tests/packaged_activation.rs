//! Windows 商店版 Codex 的路径解析与激活(上游 2a41afb / 6c11bf7 / f4f9bae / be67b30
//! 的 ReCodex 版本)。这些用例不碰真实的 WindowsApps,全部在临时目录里造包。

use std::path::{Path, PathBuf};

use codex_plus_core::app_paths::{
    find_latest_codex_app_dir, find_latest_codex_app_dir_from_roots, manifest_application_id,
    packaged_app_user_model_id, resolve_saved_store_path,
};
use codex_plus_core::launcher::{
    HRESULT_APPLICATION_NOT_FOUND, HRESULT_PACKAGE_REGISTRATION_IN_PROGRESS,
    activation_error_hresult, direct_launch_fallback_is_sensible, format_hresult,
    packaged_activation_error_is_transient,
};

const CODEX_MANIFEST: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<Package xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10">
  <Applications>
    <Application Id="App" Executable="app/ChatGPT.exe" EntryPoint="Windows.FullTrustApplication">
    </Application>
    <Application Id="CodexCoreCommandRunner" Executable="app/resources/codex-command-runner.exe" EntryPoint="Windows.FullTrustApplication">
    </Application>
  </Applications>
</Package>"#;

fn make_package(root: &Path, name: &str) -> PathBuf {
    let app = root.join(name).join("app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(app.join("ChatGPT.exe"), "").unwrap();
    app
}

fn mark_codex_host(app: &Path) {
    std::fs::create_dir_all(app.join("resources")).unwrap();
    std::fs::write(app.join("resources").join("codex.exe"), "").unwrap();
}

#[test]
fn manifest_application_id_prefers_the_main_executable() {
    // 实测 26.915 的真实 manifest 形态:两个 Application,主程序排第一。
    assert_eq!(manifest_application_id(CODEX_MANIFEST).as_deref(), Some("App"));

    // 主程序不排第一也要认出来(新版 ChatGPT Desktop 的 Id 与顺序都可能变)。
    let reordered = r#"<Applications>
      <Application Id='Runner' Executable='app\resources\runner.exe'/>
      <Application EntryPoint="x" Id = "ChatGPTDesktop" Executable="app\ChatGPT.exe"/>
    </Applications>"#;
    assert_eq!(
        manifest_application_id(reordered).as_deref(),
        Some("ChatGPTDesktop")
    );

    // 都不是主程序 → 取第一个;属性名只认完整的 `Id`(不被 `AppId=` 之类误伤)。
    let none_main = r#"<Application SomeAppId="bad" Id="First" Executable="a.exe"/><Application Id="Second"/>"#;
    assert_eq!(manifest_application_id(none_main).as_deref(), Some("First"));
    assert_eq!(manifest_application_id("<Applications></Applications>"), None);
}

/// S7:小写的 `codex.exe` 是包里带的 CLI,命令执行器也不是主程序 —— 它们排在前面
/// 时不能被 `eq_ignore_ascii_case` 当成主程序选中。
#[test]
fn manifest_application_id_ignores_lowercase_cli_and_command_runner() {
    let cli_first = r#"<Applications>
      <Application Id="CodexCli" Executable="app\resources\codex.exe"/>
      <Application Id="CodexCoreCommandRunner" Executable="app/codex-command-runner.exe"/>
      <Application Id="App" Executable="app/ChatGPT.exe"/>
    </Applications>"#;
    assert_eq!(manifest_application_id(cli_first).as_deref(), Some("App"));

    // 小写 codex.exe 即便不在 resources 下也不算主程序。
    let lowercase_cli = r#"<Application Id="Cli" Executable="app/codex.exe"/><Application Id="Main" Executable="app/Codex.exe"/>"#;
    assert_eq!(manifest_application_id(lowercase_cli).as_deref(), Some("Main"));

    // 没有主程序时,跳过辅助程序取第一个普通条目。
    let no_main = r#"<Application Id="CodexCoreCommandRunner" Executable="app/resources/runner.exe"/><Application Id="Other" Executable="app/other.exe"/>"#;
    assert_eq!(manifest_application_id(no_main).as_deref(), Some("Other"));
}

#[test]
fn aumid_reads_application_id_from_the_package_manifest() {
    let temp = tempfile::tempdir().unwrap();
    let app = make_package(
        temp.path(),
        "OpenAI.ChatGPT-Desktop_1.2026.190.0_x64__2p2nqsd0c76g0",
    );
    // 没有 manifest → 历史默认值 App。
    assert_eq!(
        packaged_app_user_model_id(&app).as_deref(),
        Some("OpenAI.ChatGPT-Desktop_2p2nqsd0c76g0!App")
    );
    std::fs::write(
        app.parent().unwrap().join("AppxManifest.xml"),
        r#"<Applications><Application Id="ChatGPT" Executable="app/ChatGPT.exe"></Application></Applications>"#,
    )
    .unwrap();
    assert_eq!(
        packaged_app_user_model_id(&app).as_deref(),
        Some("OpenAI.ChatGPT-Desktop_2p2nqsd0c76g0!ChatGPT")
    );
}

#[test]
fn package_full_names_with_tilde_resource_id_are_understood() {
    let app = PathBuf::from(r"C:\x\OpenAI.ChatGPT-Desktop_1.2026.190.0_neutral_~_2p2nqsd0c76g0\app");
    assert_eq!(
        packaged_app_user_model_id(&app).as_deref(),
        Some("OpenAI.ChatGPT-Desktop_2p2nqsd0c76g0!App")
    );
    let classic = PathBuf::from(r"C:\x\OpenAI.Codex_26.915.3509.0_x64__2p2nqsd0c76g0\app");
    assert_eq!(
        packaged_app_user_model_id(&classic).as_deref(),
        Some("OpenAI.Codex_2p2nqsd0c76g0!App")
    );
}

#[test]
fn chatgpt_desktop_wins_only_when_it_actually_hosts_codex() {
    let temp = tempfile::tempdir().unwrap();
    let codex = make_package(temp.path(), "OpenAI.Codex_26.915.3509.0_x64__abc");
    let chatgpt = make_package(temp.path(), "OpenAI.ChatGPT-Desktop_1.2026.190.0_x64__abc");

    // 老的纯聊天版 ChatGPT(没有 Codex 运行时)绝不能压过 OpenAI.Codex。
    assert_eq!(find_latest_codex_app_dir(temp.path()).unwrap(), codex);

    // 新版 ChatGPT Desktop 带着 Codex → 优先新宿主(上游 f4f9bae)。
    mark_codex_host(&chatgpt);
    assert_eq!(find_latest_codex_app_dir(temp.path()).unwrap(), chatgpt);
    assert_eq!(
        find_latest_codex_app_dir_from_roots(&[temp.path().to_path_buf()]).unwrap(),
        chatgpt
    );
}

#[test]
fn saved_store_path_prefers_current_registered_package() {
    let saved = PathBuf::from(r"C:\old\app");
    let current = PathBuf::from(r"C:\new\app");
    assert_eq!(
        resolve_saved_store_path(saved.clone(), Ok(Some(current.clone()))),
        current
    );
    assert_eq!(resolve_saved_store_path(saved.clone(), Ok(None)), saved);
    assert_eq!(
        resolve_saved_store_path(saved.clone(), Err(anyhow::anyhow!("query failed"))),
        saved
    );
}

#[test]
fn activation_errors_are_classified_by_hresult() {
    // 线上 2026-09-18 的原文(Store 正在更新 Codex)。
    let updating = anyhow::anyhow!(
        "OpenAI.Codex_26.915.3509.0_x64__2p2nqsd0c76g0(0,0): 错误 0x80073D28: 无法注册 OpenAI.Codex_26.915.3509.0_x64__2p2nqsd0c76g0 程序包。需要管理员权限才能安装打包服务 (0x80073D28)"
    );
    assert_eq!(
        activation_error_hresult(&updating),
        Some(HRESULT_PACKAGE_REGISTRATION_IN_PROGRESS)
    );
    let wrong_app_id = anyhow::anyhow!("activation failed (0x80270254)").context("wrapped");
    assert_eq!(
        activation_error_hresult(&wrong_app_id),
        Some(HRESULT_APPLICATION_NOT_FOUND)
    );
    assert_eq!(activation_error_hresult(&anyhow::anyhow!("boom 0x12")), None);

    assert!(packaged_activation_error_is_transient(Some(
        HRESULT_PACKAGE_REGISTRATION_IN_PROGRESS
    )));
    assert!(packaged_activation_error_is_transient(Some(
        HRESULT_APPLICATION_NOT_FOUND
    )));
    assert!(!packaged_activation_error_is_transient(Some(0x8000_4005)));
    assert!(!packaged_activation_error_is_transient(None));
    assert_eq!(format_hresult(0x8007_3D28), "0x80073D28");
}

#[test]
fn direct_exe_fallback_skips_windows_apps() {
    let temp = tempfile::tempdir().unwrap();
    let exe = temp.path().join("ChatGPT.exe");
    assert!(!direct_launch_fallback_is_sensible(&exe), "exe 不存在");
    std::fs::write(&exe, "").unwrap();
    assert!(direct_launch_fallback_is_sensible(&exe));

    let store = temp.path().join("WindowsApps").join("OpenAI.Codex_1.0.0.0_x64__abc");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join("ChatGPT.exe"), "").unwrap();
    assert!(!direct_launch_fallback_is_sensible(&store.join("ChatGPT.exe")));
}

#[cfg(windows)]
#[tokio::test]
async fn activation_retry_gives_up_with_aumid_and_error_code() {
    // 不存在的包:ActivateApplication 立刻失败,重试序列走完后带着 AUMID/错误码返回。
    //
    // 包身份**必须**不是 OpenAI.Codex / CodexBeta / ChatGPT-Desktop:重试会按身份重新
    // 解析系统里注册的包 —— 用 OpenAI.Codex_* 造假目录,第二次尝试就会解析到本机真实
    // 安装的 Codex 并把它激活(第一版这条用例就这样在开发机上激活过一次真 Codex)。
    let app_dir = PathBuf::from(r"C:\nowhere\ReCodex.TestOnly_0.0.0.1_x64__recodextest\app");
    let failure = codex_plus_core::launcher::activate_packaged_app_with_retry(
        &app_dir,
        "ReCodex.TestOnly_recodextest!App",
        "",
        &[1, 1],
    )
    .await
    .expect_err("不存在的 AUMID 不可能激活成功");
    assert_eq!(failure.app_user_model_id, "ReCodex.TestOnly_recodextest!App");
    assert!(failure.error_code.is_some(), "{:#}", failure.error);
    let expected_attempts = if packaged_activation_error_is_transient(failure.error_code) {
        3
    } else {
        2
    };
    assert_eq!(failure.attempts, expected_attempts);
    let message = format!("{:#}", failure.into_error());
    assert!(message.contains("ReCodex.TestOnly_recodextest!App"), "{message}");
    assert!(message.contains("0x"), "{message}");
}

#[test]
fn registered_package_lookup_is_not_cached_for_the_process_lifetime() {
    // 微信连接 / 手机远程在启动很久之后才调 find_codex_cli,期间商店可能已经把
    // Codex 更新了 —— 注册信息不能用进程级缓存(上游 f4f9bae)。
    let source = include_str!("../src/app_paths.rs");
    let start = source
        .find("pub(crate) fn registered_windows_packages()")
        .expect("registered_windows_packages");
    // 以下一个函数名为结尾,不找 "\n}\n":autocrlf 检出的是 CRLF。
    let end = source[start..]
        .find("fn query_registered_windows_packages")
        .unwrap();
    let body = &source[start..start + end];
    assert!(!body.contains("OnceLock"), "{body}");
    // 注册信息优先于目录扫描(残留的未注册高版本目录不能压过注册的包)。
    let default_start = source
        .find("pub fn find_latest_codex_app_dir_default()")
        .unwrap();
    let default_body = &source[default_start..];
    let registered = default_body
        .find("find_latest_codex_app_dir_from_appx_package()")
        .unwrap();
    let scanned = default_body
        .find("find_latest_codex_app_dir_from_roots(")
        .unwrap();
    assert!(registered < scanned);
}

// ---- 退避重试的行为测试(第二轮审计 F):激活/重新解析/等待都注入,不碰真实 COM ----

use std::cell::RefCell;

/// 记录每次激活用的 AUMID 与每次等待的时长;按预设结果依次返回。
#[derive(Default)]
struct ActivationSpy {
    attempts: RefCell<Vec<String>>,
    delays: RefCell<Vec<u64>>,
    reresolves: RefCell<usize>,
}

fn hresult_error(code: u32) -> anyhow::Error {
    anyhow::anyhow!("ActivateApplication failed 0x{code:08X}")
}

async fn run_retry(
    spy: &ActivationSpy,
    results: Vec<anyhow::Result<u32>>,
    delays_ms: &[u64],
    reresolve_to: Option<(&str, &str)>,
) -> Result<(u32, String), codex_plus_core::launcher::PackagedActivationFailure> {
    let results = RefCell::new(results.into_iter().collect::<std::collections::VecDeque<_>>());
    codex_plus_core::launcher::activate_packaged_app_with_retry_using(
        Path::new(r"C:\pkg\OpenAI.Codex_26.915.3509.0_x64__abc\app"),
        "OpenAI.Codex_abc!App",
        "--args",
        delays_ms,
        |aumid, _arguments| {
            spy.attempts.borrow_mut().push(aumid);
            let next = results
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Err(hresult_error(0x8007_3D28)));
            async move { next }
        },
        |_app_dir| {
            *spy.reresolves.borrow_mut() += 1;
            reresolve_to.map(|(dir, aumid)| (PathBuf::from(dir), aumid.to_string()))
        },
        |delay_ms| {
            spy.delays.borrow_mut().push(delay_ms);
            async move {}
        },
    )
    .await
}

/// 瞬时错误(商店正在注册包)按整条退避序列重试,中途成功就返回;每次重试前重新解析包,
/// 用的是新解析出来的 AUMID。
#[tokio::test]
async fn transient_activation_errors_retry_with_the_reresolved_aumid() {
    let spy = ActivationSpy::default();
    let outcome = run_retry(
        &spy,
        vec![
            Err(hresult_error(0x8007_3D28)),
            Err(hresult_error(0x8027_0254)),
            Ok(4242),
        ],
        &[1500, 3000, 5000],
        Some((r"C:\pkg\OpenAI.Codex_26.915.4065.0_x64__abc\app", "OpenAI.Codex_abc!ChatGPT")),
    )
    .await;

    let (process_id, aumid) = outcome.expect("第三次应当成功");
    assert_eq!(process_id, 4242);
    assert_eq!(aumid, "OpenAI.Codex_abc!ChatGPT");
    assert_eq!(
        *spy.attempts.borrow(),
        vec![
            "OpenAI.Codex_abc!App",
            "OpenAI.Codex_abc!ChatGPT",
            "OpenAI.Codex_abc!ChatGPT",
        ]
    );
    assert_eq!(*spy.delays.borrow(), vec![1500, 3000], "按退避序列等待");
    assert_eq!(*spy.reresolves.borrow(), 2, "每次重试前都重新解析包");
}

/// 非瞬时错误只再试一次(刚杀完进程时 COM 侧的激活锁),不走完整条序列。
#[tokio::test]
async fn non_transient_activation_errors_retry_only_once() {
    let spy = ActivationSpy::default();
    let outcome = run_retry(
        &spy,
        vec![
            Err(anyhow::anyhow!("ActivateApplication failed 0x80004005")),
            Err(anyhow::anyhow!("ActivateApplication failed 0x80004005")),
        ],
        &[1500, 3000, 5000],
        None,
    )
    .await;

    let failure = outcome.expect_err("两次都失败");
    assert_eq!(failure.attempts, 2);
    assert_eq!(spy.attempts.borrow().len(), 2);
    assert_eq!(*spy.delays.borrow(), vec![1500]);
}

/// 瞬时错误一直不好:用完整条序列后放弃,次数 = 序列长度 + 1,错误里带 AUMID 和错误码。
#[tokio::test]
async fn transient_activation_errors_give_up_after_the_whole_backoff() {
    let spy = ActivationSpy::default();
    let outcome = run_retry(&spy, Vec::new(), &[1, 2, 3], None).await;

    let failure = outcome.expect_err("一直失败");
    assert_eq!(failure.attempts, 4);
    assert_eq!(failure.error_code, Some(0x8007_3D28));
    assert_eq!(*spy.delays.borrow(), vec![1, 2, 3]);
    let message = format!("{:#}", failure.into_error());
    assert!(message.contains("OpenAI.Codex_abc!App"), "{message}");
    assert!(message.contains("0x80073D28"), "{message}");
}

/// 重新解析失败(包还没注册回来)时保持原 AUMID 继续重试,不会把目录/AUMID 弄丢。
#[tokio::test]
async fn activation_keeps_the_original_aumid_when_reresolve_fails() {
    let spy = ActivationSpy::default();
    let outcome = run_retry(
        &spy,
        vec![Err(hresult_error(0x8007_3D28)), Ok(7)],
        &[1],
        None,
    )
    .await;

    assert_eq!(outcome.expect("第二次成功").1, "OpenAI.Codex_abc!App");
    assert_eq!(
        *spy.attempts.borrow(),
        vec!["OpenAI.Codex_abc!App", "OpenAI.Codex_abc!App"]
    );
}
