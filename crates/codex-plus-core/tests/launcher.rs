use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use codex_plus_core::app_paths::{
    build_codex_executable, codex_app_version, find_latest_codex_app_dir,
    find_latest_codex_app_dir_from_roots, find_macos_codex_app, normalize_codex_app_path,
    packaged_app_user_model_id, resolve_codex_app_dir_with_saved, user_data_candidates_from,
};
use codex_plus_core::launcher::{
    CodexLaunch, DefaultLaunchHooks, LaunchHooks, LaunchOptions, MacosCleanupPolicy,
    MacosDebugLaunchAction, browser_identity_changed, build_codex_arguments,
    build_codex_arguments_for_settings, build_codex_arguments_with_native_menu_inspector,
    build_codex_command, build_codex_command_with_native_menu_inspector,
    build_macos_cleanup_command, build_macos_open_command,
    build_macos_open_command_with_native_menu_inspector, build_packaged_activation,
    build_packaged_activation_with_native_menu_inspector, launch_and_inject_with_hooks,
    select_macos_debug_launch_action,
};
#[cfg(windows)]
use codex_plus_core::launcher::{WindowsProcessControlStrategy, windows_process_control_strategy};
use codex_plus_core::ports::{
    select_packaged_codex_debug_port_with, select_platform_loopback_port_with,
};
use codex_plus_core::settings::{
    BackendSettings, RelayMode, RelayModelRoute, RelayProfile, RelayProtocol,
};
use codex_plus_core::status::StatusStore;

#[test]
fn browser_identity_change_requires_two_distinct_observations() {
    assert!(!browser_identity_changed(None, "browser-a"));
    assert!(!browser_identity_changed(Some("browser-a"), "browser-a"));
    assert!(browser_identity_changed(Some("browser-a"), "browser-b"));
}

#[test]
fn app_paths_find_latest_windows_package_prefers_highest_version_app_dir() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join("OpenAI.Codex_1.2.3.0_x64__abc/app")).unwrap();
    std::fs::create_dir_all(temp.path().join("OpenAI.Codex_26.429.8261.0_x64__abc/app")).unwrap();
    std::fs::create_dir_all(temp.path().join("OpenAI.Codex_not-a-version_x64__abc")).unwrap();

    let latest = find_latest_codex_app_dir(temp.path()).unwrap();

    assert_eq!(
        latest,
        temp.path().join("OpenAI.Codex_26.429.8261.0_x64__abc/app")
    );
}

#[test]
fn app_paths_find_latest_windows_package_ignores_chatgpt_desktop_package() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join("OpenAI.Codex_26.707.3748.0_x64__abc/app")).unwrap();
    std::fs::create_dir_all(
        temp.path()
            .join("OpenAI.ChatGPT-Desktop_1.2026.133.0_x64__abc/app"),
    )
    .unwrap();
    std::fs::create_dir_all(
        temp.path()
            .join("OpenAI.ChatGPT-Desktop_2026.514.421.0_neutral_~_abc"),
    )
    .unwrap();

    let latest = find_latest_codex_app_dir(temp.path()).unwrap();

    assert_eq!(
        latest,
        temp.path().join("OpenAI.Codex_26.707.3748.0_x64__abc/app")
    );
    assert_eq!(codex_app_version(&latest).as_deref(), Some("26.707.3748.0"));
    assert_eq!(
        packaged_app_user_model_id(&latest).as_deref(),
        Some("OpenAI.Codex_abc!App")
    );
}

#[test]
fn app_paths_find_latest_windows_package_detects_beta_package() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(
        temp.path()
            .join("OpenAI.CodexBeta_26.527.7698.0_x64__2p2nqsd0c76g0/app"),
    )
    .unwrap();

    let latest = find_latest_codex_app_dir(temp.path()).unwrap();

    assert_eq!(
        latest,
        temp.path()
            .join("OpenAI.CodexBeta_26.527.7698.0_x64__2p2nqsd0c76g0/app")
    );
    assert_eq!(codex_app_version(&latest).as_deref(), Some("26.527.7698.0"));
    assert_eq!(
        packaged_app_user_model_id(&latest).as_deref(),
        Some("OpenAI.CodexBeta_2p2nqsd0c76g0!App")
    );
}

#[test]
fn app_paths_find_latest_windows_package_returns_package_when_app_dir_missing() {
    let temp = tempfile::tempdir().unwrap();
    let package = temp.path().join("OpenAI.Codex_26.429.8261.0_x64__abc");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(package.join("ChatGPT.exe"), "").unwrap();

    assert_eq!(find_latest_codex_app_dir(temp.path()).unwrap(), package);
}

#[test]
fn app_paths_find_latest_windows_package_checks_roots_before_fallback() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("WindowsApps");
    std::fs::create_dir_all(root.join("OpenAI.Codex_1.0.0.0_x64__abc/app")).unwrap();
    std::fs::create_dir_all(root.join("OpenAI.Codex_26.513.3673.0_x64__abc/app")).unwrap();

    let latest = find_latest_codex_app_dir_from_roots(&[root]).unwrap();

    assert!(latest.ends_with("OpenAI.Codex_26.513.3673.0_x64__abc/app"));
}

#[test]
fn app_paths_find_latest_windows_package_ignores_chatgpt_across_roots() {
    let temp = tempfile::tempdir().unwrap();
    let root_a = temp.path().join("WindowsAppsA");
    let root_b = temp.path().join("WindowsAppsB");
    std::fs::create_dir_all(root_a.join("OpenAI.Codex_26.999.0.0_x64__abc/app")).unwrap();
    std::fs::create_dir_all(root_b.join("OpenAI.ChatGPT-Desktop_1.2026.133.0_x64__abc/app"))
        .unwrap();

    let latest = find_latest_codex_app_dir_from_roots(&[root_a, root_b]).unwrap();

    assert!(latest.ends_with("OpenAI.Codex_26.999.0.0_x64__abc/app"));
}

#[test]
fn app_paths_extracts_codex_version_from_windows_package_app_dir() {
    let app_dir =
        PathBuf::from(r"C:\Program Files\WindowsApps\OpenAI.Codex_26.513.3673.0_x64__abc\app");

    assert_eq!(
        codex_app_version(&app_dir).as_deref(),
        Some("26.513.3673.0")
    );
}

#[test]
fn app_paths_extracts_codex_version_from_portable_version_file() {
    let temp = tempfile::tempdir().unwrap();
    let app_dir = temp.path().join("versions").join("current");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::write(app_dir.join("Codex.exe"), "").unwrap();
    std::fs::write(app_dir.join("version"), "42.1.0\n").unwrap();

    assert_eq!(codex_app_version(&app_dir).as_deref(), Some("42.1.0"));
}

#[test]
fn app_paths_prefers_portable_directory_version_over_internal_version_file() {
    let temp = tempfile::tempdir().unwrap();
    let app_dir = temp.path().join("versions").join("26.519.2736.0");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::write(app_dir.join("Codex.exe"), "").unwrap();
    std::fs::write(app_dir.join("version"), "42.1.0\n").unwrap();

    assert_eq!(
        codex_app_version(&app_dir).as_deref(),
        Some("26.519.2736.0")
    );
}

#[cfg(windows)]
#[test]
fn app_paths_resolves_portable_current_link_to_directory_version() {
    let temp = tempfile::tempdir().unwrap();
    let versions = temp.path().join("versions");
    let target = versions.join("26.519.2736.0");
    let current = versions.join("current");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("Codex.exe"), "").unwrap();
    std::fs::write(target.join("version"), "42.1.0\n").unwrap();
    // 建目录符号链接在 Windows 上要管理员权限或开发者模式。没有就跳过,别把
    // 「这台机器没权限」报成「这段逻辑坏了」—— 一个常年红的用例会让整套门禁
    // 失去意义,真回归反而没人信。
    if std::os::windows::fs::symlink_dir(&target, &current).is_err() {
        eprintln!("跳过:当前账号无权创建目录符号链接(需管理员或开发者模式)");
        return;
    }

    assert_eq!(
        codex_app_version(&current).as_deref(),
        Some("26.519.2736.0")
    );
}

#[test]
fn app_paths_extracts_codex_version_from_macos_bundle_plist() {
    let temp = tempfile::tempdir().unwrap();
    let app = temp.path().join("OpenAI Codex.app");
    let contents = app.join("Contents");
    std::fs::create_dir_all(&contents).unwrap();
    std::fs::write(
        contents.join("Info.plist"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
  <key>CFBundleVersion</key>
  <string>26.500.0</string>
  <key>CFBundleShortVersionString</key>
  <string>26.513.3673</string>
</dict>
</plist>
"#,
    )
    .unwrap();

    assert_eq!(codex_app_version(&app).as_deref(), Some("26.513.3673"));
}

#[test]
fn app_paths_user_data_candidates_include_local_and_roaming_variants() {
    let local = PathBuf::from(r"C:\Users\me\AppData\Local");
    let roaming = PathBuf::from(r"C:\Users\me\AppData\Roaming");

    let candidates = user_data_candidates_from(Some(&local), Some(&roaming));

    assert_eq!(
        candidates,
        vec![
            local.join("OpenAI").join("ChatGPT"),
            local.join("OpenAI.ChatGPT-Desktop"),
            local.join("ChatGPT"),
            local.join("OpenAI").join("Codex"),
            local.join("OpenAI.Codex"),
            local.join("Codex"),
            roaming.join("OpenAI").join("ChatGPT"),
            roaming.join("OpenAI.ChatGPT-Desktop"),
            roaming.join("ChatGPT"),
            roaming.join("OpenAI").join("Codex"),
            roaming.join("OpenAI.Codex"),
            roaming.join("Codex"),
        ]
    );
}

#[test]
fn app_paths_find_macos_codex_app_prefers_first_search_root_and_known_names() {
    let temp = tempfile::tempdir().unwrap();
    let system_root = temp.path().join("Applications");
    let user_root = temp.path().join("Users/me/Applications");
    let system_app = system_root.join("OpenAI Codex.app");
    let user_app = user_root.join("Codex.app");
    std::fs::create_dir_all(&system_app).unwrap();
    std::fs::create_dir_all(&user_app).unwrap();

    assert_eq!(
        find_macos_codex_app(&[system_root, user_root]).unwrap(),
        system_app
    );
}

#[test]
fn app_paths_prefers_codex_app_over_chatgpt_app() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("Applications");
    let codex = root.join("Codex.app");
    let chatgpt = root.join("ChatGPT.app");
    std::fs::create_dir_all(&codex).unwrap();
    std::fs::create_dir_all(&chatgpt).unwrap();

    assert_eq!(
        find_macos_codex_app(&[root]).as_deref(),
        Some(codex.as_path())
    );
}

#[test]
fn app_paths_preserves_legacy_macos_candidates_before_chatgpt_app() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("Applications");
    let legacy = root.join("OpenAI Codex.app");
    let chatgpt = root.join("ChatGPT.app");
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::create_dir_all(&chatgpt).unwrap();

    assert_eq!(
        find_macos_codex_app(&[root]).as_deref(),
        Some(legacy.as_path())
    );
}

#[test]
fn app_paths_build_macos_bundle_executable() {
    let app = PathBuf::from("/Applications/OpenAI Codex.app");

    assert_eq!(
        build_codex_executable(&app),
        PathBuf::from("/Applications/OpenAI Codex.app/Contents/MacOS/Codex")
    );
}

#[test]
fn app_paths_finds_chatgpt_bundle_and_uses_its_declared_executable() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("Applications");
    let app = root.join("ChatGPT.app");
    let contents = app.join("Contents");
    let macos = contents.join("MacOS");
    std::fs::create_dir_all(&macos).unwrap();
    std::fs::write(
        contents.join("Info.plist"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key>
  <string>com.openai.codex</string>
  <key>CFBundleExecutable</key>
  <string>ChatGPT</string>
</dict>
</plist>
"#,
    )
    .unwrap();
    std::fs::write(macos.join("ChatGPT"), "").unwrap();

    assert_eq!(
        find_macos_codex_app(&[root]).as_deref(),
        Some(app.as_path())
    );
    assert_eq!(build_codex_executable(&app), macos.join("ChatGPT"));
}

#[test]
fn app_paths_normalizes_executable_and_package_paths() {
    let temp = tempfile::tempdir().unwrap();
    let portable = temp.path().join("CodexPortable");
    let app = portable.join("app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(app.join("Codex.exe"), "").unwrap();

    assert_eq!(
        normalize_codex_app_path(&app.join("Codex.exe")).as_deref(),
        Some(app.as_path())
    );
    assert_eq!(
        normalize_codex_app_path(&portable).as_deref(),
        Some(app.as_path())
    );
}

#[test]
fn app_paths_prefers_chatgpt_entrypoint_when_portable_bundle_contains_codex_shim() {
    let temp = tempfile::tempdir().unwrap();
    let app = temp.path().join("current");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(app.join("Codex.exe"), "").unwrap();
    std::fs::write(app.join("ChatGPT.exe"), "").unwrap();

    assert_eq!(build_codex_executable(&app), app.join("ChatGPT.exe"));
}

#[test]
fn app_paths_normalizes_chatgpt_desktop_executable_and_builds_it() {
    let temp = tempfile::tempdir().unwrap();
    let app = temp
        .path()
        .join("OpenAI.Codex_1.2026.133.0_x64__abc")
        .join("app");
    std::fs::create_dir_all(&app).unwrap();
    std::fs::write(app.join("ChatGPT.exe"), "").unwrap();

    assert_eq!(
        normalize_codex_app_path(&app.join("ChatGPT.exe")).as_deref(),
        Some(app.as_path())
    );
    assert_eq!(build_codex_executable(&app), app.join("ChatGPT.exe"));
    assert_eq!(
        packaged_app_user_model_id(&app).as_deref(),
        Some("OpenAI.Codex_abc!App")
    );
}

#[test]
fn app_paths_saved_path_is_used_when_no_explicit_path_is_provided() {
    let temp = tempfile::tempdir().unwrap();
    let app = temp.path().join("Codex.app");
    std::fs::create_dir_all(&app).unwrap();

    assert_eq!(
        resolve_codex_app_dir_with_saved(None, Some(&app.to_string_lossy())).as_deref(),
        Some(app.as_path())
    );
}

#[test]
fn app_paths_rejects_codex_plus_plus_install_dir_as_codex_app() {
    let temp = tempfile::tempdir().unwrap();
    let manager = temp.path().join("Programs").join("Codex++");
    std::fs::create_dir_all(&manager).unwrap();
    std::fs::write(manager.join("Codex++ Manager.exe"), "").unwrap();

    assert_eq!(normalize_codex_app_path(&manager), None);
    assert_eq!(
        normalize_codex_app_path(&manager.join("Codex++ Manager.exe")),
        None
    );

    let resolved = resolve_codex_app_dir_with_saved(None, Some(&manager.to_string_lossy()));
    assert_ne!(resolved.as_deref(), Some(manager.as_path()));
}

#[test]
fn app_paths_rejects_plain_directory_without_codex_executable() {
    let temp = tempfile::tempdir().unwrap();
    let plain = temp.path().join("not-a-codex-app");
    std::fs::create_dir_all(&plain).unwrap();
    std::fs::write(plain.join("readme.txt"), "nope").unwrap();

    assert_eq!(normalize_codex_app_path(&plain), None);
    assert_eq!(normalize_codex_app_path(&plain.join("readme.txt")), None);
}

#[test]
fn app_paths_empty_saved_path_matches_no_saved_path() {
    assert_eq!(
        resolve_codex_app_dir_with_saved(None, Some("")),
        resolve_codex_app_dir_with_saved(None, None)
    );
    assert_eq!(
        resolve_codex_app_dir_with_saved(None, Some("   ")),
        resolve_codex_app_dir_with_saved(None, None)
    );
}

#[test]
fn app_paths_invalid_saved_path_falls_back_instead_of_sticking() {
    let temp = tempfile::tempdir().unwrap();
    let junk = temp.path().join("Codex++");
    std::fs::create_dir_all(&junk).unwrap();

    // 合法独立安装：即使 saved 指向 Codex++，规范化失败后应能落到该候选
    // （通过显式 app_dir 验证回退链之外的合法路径仍可用）
    let standalone = temp.path().join("OpenAI").join("Codex").join("bin");
    std::fs::create_dir_all(&standalone).unwrap();
    std::fs::write(standalone.join("codex.exe"), "").unwrap();

    assert_eq!(normalize_codex_app_path(&junk), None);
    assert_eq!(
        normalize_codex_app_path(&standalone).as_deref(),
        Some(standalone.as_path())
    );
    assert_eq!(
        resolve_codex_app_dir_with_saved(Some(&standalone), Some(&junk.to_string_lossy()))
            .as_deref(),
        Some(standalone.as_path())
    );
}

#[test]
fn launcher_builds_debug_arguments_and_commands() {
    let app_dir = PathBuf::from(r"C:\Codex\app");

    assert_eq!(
        build_codex_arguments(9229, &[]),
        vec![
            "--remote-debugging-port=9229".to_string(),
            "--remote-allow-origins=http://127.0.0.1:9229".to_string(),
        ]
    );
    let command = build_codex_command(&app_dir, 9229, &[]);
    assert_eq!(command[1], "--remote-debugging-port=9229");
    assert_eq!(command[2], "--remote-allow-origins=http://127.0.0.1:9229");
}

#[test]
fn launcher_does_not_override_codex_app_environment() {
    let source = include_str!("../src/launcher.rs");

    assert!(!source.contains(".envs(codex_process_environment())"));
    assert!(!source.contains("activate_packaged_app_with_environment"));
    assert!(!source.contains("with_temporary_proxy_environment"));
}

#[test]
fn launcher_uses_all_com_server_contexts_for_packaged_app_activation() {
    let source = include_str!("../src/launcher.rs");

    assert!(source.contains("CoCreateInstance(&ApplicationActivationManager, None, CLSCTX_ALL)?"));
    assert!(!source.contains("CLSCTX_LOCAL_SERVER"));
}

#[test]
fn launcher_does_not_prepare_projectless_main_window() {
    let source = include_str!("../src/launcher.rs");

    assert!(!source.contains("prepare_projectless_main_window_nonfatal"));
    assert!(!source.contains("launcher.prelaunch"));
}

#[test]
fn launcher_windows_process_wait_uses_platform_cfg_guards() {
    let source = include_str!("../src/launcher.rs").replace("\r\n", "\n");

    assert!(source.contains(
        "#[cfg(windows)]\nasync fn wait_for_windows_process_id(process_id: u32) -> anyhow::Result<()>"
    ));
    assert!(source.contains(
        "#[cfg(not(windows))]\nasync fn wait_for_windows_process_id(process_id: u32) -> anyhow::Result<()>"
    ));
    assert!(source.contains(
        "#[cfg(windows)]\nfn wait_for_windows_process_id_blocking(process_id: u32) -> anyhow::Result<()>"
    ));
}

#[test]
fn launcher_appends_extra_codex_arguments_after_debug_arguments() {
    let app_dir = PathBuf::from(r"C:\Codex\app");
    let extra_args = vec![
        "--force_high_performance_gpu".to_string(),
        "  ".to_string(),
        "--enable-features=UseOzonePlatform".to_string(),
    ];

    assert_eq!(
        build_codex_arguments(9229, &extra_args),
        vec![
            "--remote-debugging-port=9229".to_string(),
            "--remote-allow-origins=http://127.0.0.1:9229".to_string(),
            "--force_high_performance_gpu".to_string(),
            "--enable-features=UseOzonePlatform".to_string(),
        ]
    );
    let command = build_codex_command(&app_dir, 9229, &extra_args);
    assert_eq!(command[1], "--remote-debugging-port=9229");
    assert_eq!(command[2], "--remote-allow-origins=http://127.0.0.1:9229");
    assert_eq!(command[3], "--force_high_performance_gpu");
    assert_eq!(command[4], "--enable-features=UseOzonePlatform");
}

#[test]
fn launcher_fast_startup_adds_statsig_fast_fail_argument_when_enabled() {
    let settings = BackendSettings {
        codex_app_fast_startup: true,
        ..BackendSettings::default()
    };
    let args = build_codex_arguments_for_settings(9229, &settings);

    assert!(args.iter().any(|arg| {
        arg.starts_with("--host-resolver-rules=")
            && arg.contains("MAP ab.chatgpt.com 127.0.0.1")
            && arg.contains("MAP featureassets.org 127.0.0.1")
            && arg.contains("MAP cloudflare-dns.com 127.0.0.1")
    }));

    let settings = BackendSettings {
        codex_app_fast_startup: true,
        codex_extra_args: vec!["--host-resolver-rules=MAP example.test 127.0.0.1".to_string()],
        ..BackendSettings::default()
    };
    let args = build_codex_arguments_for_settings(9229, &settings);
    assert_eq!(
        args.iter()
            .filter(|arg| arg.starts_with("--host-resolver-rules="))
            .count(),
        1
    );

    let settings = BackendSettings {
        codex_app_fast_startup: false,
        ..BackendSettings::default()
    };
    let args = build_codex_arguments_for_settings(9229, &settings);
    assert!(
        !args
            .iter()
            .any(|arg| arg.starts_with("--host-resolver-rules="))
    );
}

#[test]
fn launcher_native_menu_inspector_arguments_are_added_before_extra_args() {
    let app_dir = PathBuf::from(r"C:\Codex\app");
    let extra_args = vec!["--force_high_performance_gpu".to_string()];

    assert_eq!(
        build_codex_arguments_with_native_menu_inspector(9229, 9329, &extra_args),
        vec![
            "--remote-debugging-port=9229".to_string(),
            "--remote-allow-origins=http://127.0.0.1:9229".to_string(),
            "--inspect=127.0.0.1:9329".to_string(),
            "--force_high_performance_gpu".to_string(),
        ]
    );
    let command = build_codex_command_with_native_menu_inspector(&app_dir, 9229, 9329, &extra_args);
    assert_eq!(command[1], "--remote-debugging-port=9229");
    assert_eq!(command[2], "--remote-allow-origins=http://127.0.0.1:9229");
    assert_eq!(command[3], "--inspect=127.0.0.1:9329");
    assert_eq!(command[4], "--force_high_performance_gpu");
}

#[test]
fn launcher_constructs_windows_packaged_activation_without_real_app() {
    let app_dir = PathBuf::from(
        r"C:\Program Files\WindowsApps\OpenAI.Codex_26.506.2212.0_x64__2p2nqsd0c76g0\app",
    );

    assert_eq!(
        packaged_app_user_model_id(&app_dir).unwrap(),
        "OpenAI.Codex_2p2nqsd0c76g0!App"
    );
    assert_eq!(
        build_packaged_activation(&app_dir, 9229, &[]).unwrap(),
        CodexLaunch::PackagedActivation {
            app_user_model_id: "OpenAI.Codex_2p2nqsd0c76g0!App".to_string(),
            arguments: "--remote-debugging-port=9229 --remote-allow-origins=http://127.0.0.1:9229"
                .to_string(),
            process_id: None,
        }
    );
}

#[test]
fn launcher_packaged_activation_appends_extra_codex_arguments() {
    let app_dir = PathBuf::from(
        r"C:\Program Files\WindowsApps\OpenAI.Codex_26.506.2212.0_x64__2p2nqsd0c76g0\app",
    );
    let extra_args = vec!["--force_high_performance_gpu".to_string()];

    assert_eq!(
        build_packaged_activation(&app_dir, 9229, &extra_args).unwrap(),
        CodexLaunch::PackagedActivation {
            app_user_model_id: "OpenAI.Codex_2p2nqsd0c76g0!App".to_string(),
            arguments:
                "--remote-debugging-port=9229 --remote-allow-origins=http://127.0.0.1:9229 --force_high_performance_gpu"
                    .to_string(),
            process_id: None,
        }
    );
}

#[test]
fn launcher_packaged_activation_adds_native_menu_inspector_argument() {
    let app_dir = PathBuf::from(
        r"C:\Program Files\WindowsApps\OpenAI.Codex_26.506.2212.0_x64__2p2nqsd0c76g0\app",
    );

    assert_eq!(
        build_packaged_activation_with_native_menu_inspector(&app_dir, 9229, 9329, &[]).unwrap(),
        CodexLaunch::PackagedActivation {
            app_user_model_id: "OpenAI.Codex_2p2nqsd0c76g0!App".to_string(),
            arguments:
                "--remote-debugging-port=9229 --remote-allow-origins=http://127.0.0.1:9229 --inspect=127.0.0.1:9329"
                    .to_string(),
            process_id: None,
        }
    );
}

#[test]
fn launcher_packaged_activation_can_preserve_process_id() {
    let launch = CodexLaunch::PackagedActivation {
        app_user_model_id: "OpenAI.Codex_2p2nqsd0c76g0!App".to_string(),
        arguments: "--remote-debugging-port=9229".to_string(),
        process_id: Some(4242),
    };

    assert_eq!(launch.process_id(), Some(4242));
}

#[test]
fn launcher_applies_codexplusplus_window_icon_after_packaged_activation() {
    let source = include_str!("../src/launcher.rs");

    assert!(source.contains("apply_codexplusplus_window_icon_after_launch(process_id);"));
    assert!(source.contains("windows_apply_codexplusplus_icon_to_process_window"));
}

#[test]
fn launcher_no_longer_contains_mobile_control_runtime() {
    let launcher_source = include_str!("../src/launcher.rs");
    let settings_source = include_str!("../src/settings.rs");
    let workspace_toml = include_str!("../../../Cargo.toml");

    assert!(!workspace_toml.contains("apps/codex-plus-mobile-relay"));
    assert!(!launcher_source.contains("MobileRelay"));
    assert!(!launcher_source.contains("mobile_relay"));
    assert!(!launcher_source.contains("\"/mobile\""));
    assert!(!launcher_source.contains("CODEX_PLUS_MOBILE"));
    assert!(!settings_source.contains("mobileControl"));
}

#[test]
fn launcher_plugin_marketplace_unlock_repairs_role_specific_plugins() {
    let launcher_source = include_str!("../src/launcher.rs");

    assert!(launcher_source.contains("ensure_openai_curated_marketplace_config(&home)"));
    assert!(launcher_source.contains("ensure_role_specific_plugins_marketplace_config(&home)"));
}

#[test]
fn app_paths_uses_native_windows_package_api_without_powershell() {
    let source =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/app_paths.rs")).unwrap();

    assert!(source.contains("GetPackagesByPackageFamily"));
    assert!(source.contains("GetPackagePathByFullName"));
    assert!(!source.contains("Command::new(\"powershell\")"));
}

#[test]
fn launcher_packaged_activation_does_not_directly_fallback_to_windowsapps_exe() {
    let source = include_str!("../src/launcher.rs");

    assert!(!source.contains("launcher.packaged_activation_cdp_unready_direct_fallback"));
    assert!(!source.contains("terminate_windows_process_id(process_id).await"));
}

#[cfg(windows)]
#[test]
fn launcher_windows_packaged_process_management_uses_native_api() {
    assert_eq!(
        windows_process_control_strategy(),
        WindowsProcessControlStrategy::NativeWindowsApi
    );
}

#[test]
fn launcher_macos_open_command_waits_for_app_exit() {
    let command = build_macos_open_command(Path::new("/Applications/Codex.app"), 9229, &[]);

    assert_eq!(command[0], "open");
    assert!(command.contains(&"-W".to_string()));
    assert!(command.contains(&"-a".to_string()));
    assert!(command.contains(&"--args".to_string()));
    assert!(command.contains(&"--remote-debugging-port=9229".to_string()));
}

#[test]
fn launcher_macos_open_command_appends_extra_codex_arguments_after_args() {
    let extra_args = vec!["--force_high_performance_gpu".to_string()];
    let command = build_macos_open_command(Path::new("/Applications/Codex.app"), 9229, &extra_args);
    let args_index = command
        .iter()
        .position(|part| part == "--args")
        .expect("macOS command should contain --args");

    assert_eq!(
        &command[args_index + 1..],
        &[
            "--remote-debugging-port=9229".to_string(),
            "--remote-allow-origins=http://127.0.0.1:9229".to_string(),
            "--force_high_performance_gpu".to_string(),
        ]
    );
}

#[test]
fn launcher_macos_open_command_adds_native_menu_inspector_argument() {
    let command = build_macos_open_command_with_native_menu_inspector(
        Path::new("/Applications/Codex.app"),
        9229,
        9329,
        &[],
    );
    let args_index = command
        .iter()
        .position(|part| part == "--args")
        .expect("macOS command should contain --args");

    assert_eq!(
        &command[args_index + 1..],
        &[
            "--remote-debugging-port=9229".to_string(),
            "--remote-allow-origins=http://127.0.0.1:9229".to_string(),
            "--inspect=127.0.0.1:9329".to_string(),
        ]
    );
}

#[test]
fn ports_windows_falls_back_to_ephemeral_when_requested_is_busy() {
    let selected = select_platform_loopback_port_with(9229, true, |_| false, || 43001);

    assert_eq!(selected, 43001);
}

#[test]
fn ports_windows_packaged_debug_falls_back_to_ephemeral_when_requested_is_busy() {
    let selected =
        select_packaged_codex_debug_port_with(9229, true, |_| false, |_| false, || 43001);

    assert_eq!(selected, 43001);
}

#[test]
fn ports_windows_packaged_debug_keeps_requested_when_existing_cdp_is_available() {
    let selected = select_packaged_codex_debug_port_with(9229, true, |_| false, |_| true, || 43001);

    assert_eq!(selected, 9229);
}

#[test]
fn ports_non_windows_keeps_requested_even_when_busy() {
    let selected = select_platform_loopback_port_with(9229, false, |_| false, || 43001);

    assert_eq!(selected, 9229);
}

/// 看门狗必须在连续失败后退避,否则死端口会被无限打。
///
/// 线上实测:客户机器 Codex 一直没有 CDP,看门狗固定 5 秒一跳、失败路径本身又要 10 秒
/// (retry_injection 20×500ms),tokio interval 还会补发积压 tick —— 9 小时里对着
/// 127.0.0.1:9229 连续打 HTTP,本地诊断日志刷了上万条。
#[test]
fn bridge_watchdog_backs_off_after_repeated_failures() {
    use codex_plus_core::launcher::bridge_watchdog_delay;
    use std::time::Duration;

    // 健康(和刚失败一两次)保持 5 秒:桥只是抖一下时要马上补回来。
    assert_eq!(bridge_watchdog_delay(0), Duration::from_secs(5));
    assert_eq!(bridge_watchdog_delay(2), Duration::from_secs(5));

    // 持续失败逐级退避
    assert_eq!(bridge_watchdog_delay(3), Duration::from_secs(15));
    assert_eq!(bridge_watchdog_delay(6), Duration::from_secs(30));
    assert_eq!(bridge_watchdog_delay(11), Duration::from_secs(60));

    // 封顶 60 秒:再长会让「Codex 刚起来」的恢复迟迟等不到。
    assert_eq!(bridge_watchdog_delay(10_000), Duration::from_secs(60));
    assert_eq!(bridge_watchdog_delay(u32::MAX), Duration::from_secs(60));

    // 单调不减 —— 退避不能忽大忽小
    let mut previous = bridge_watchdog_delay(0);
    for failures in 1..=50u32 {
        let current = bridge_watchdog_delay(failures);
        assert!(current >= previous, "退避必须单调不减: {failures} 处回落了");
        previous = current;
    }
}

/// 端口被占时 helper 必须换端口起来,而不是让整个启动失败。
///
/// 线上实测:一台付费客户机器 07:01 绑 57321 失败(端口被上一次没退干净的自己占着),
/// 老行为直接 `?` 中止启动 → 桥/注入/汉化/宠物全线不可用,9 小时没恢复,
/// 而用户只看到「功能没了」。这条钉住「被占也要活」。
#[tokio::test]
async fn helper_falls_back_to_another_port_when_requested_is_taken() {
    // 真占住它,并且**持有不放** —— 这正是线上的形态(别人还在监听)。
    let squatter = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let taken = squatter.local_addr().unwrap().port();

    let hooks = DefaultLaunchHooks::default();
    let bound = hooks
        .start_helper(taken)
        .await
        .expect("端口被占不该让启动失败");

    assert_ne!(bound, taken, "必须换到另一个端口");
    assert_ne!(bound, 0, "必须返回真实端口,不能是占位 0");

    // 换端口后 helper 得真的在新端口上服务 —— 否则「没报错但也没用」更难查。
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let response = client
        .post(format!("http://127.0.0.1:{bound}/backend/status"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .expect("换端口后 helper 应当可用");
    assert!(response.status().is_success());

    hooks.shutdown_helper(bound).await;
    drop(squatter);
}

/// 端口空闲时必须原样用请求的端口,不能无谓地漂走。
///
/// 漂走本身不致命,但会让「helper_port 固定」这个前提失效,
/// 也会让日志里的端口和用户配置对不上,排查时多绕一圈。
#[tokio::test]
async fn helper_keeps_requested_port_when_available() {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let hooks = DefaultLaunchHooks::default();
    let bound = hooks.start_helper(port).await.unwrap();
    assert_eq!(bound, port, "端口可用时不该换");

    hooks.shutdown_helper(bound).await;
}

#[tokio::test]
async fn default_helper_serves_backend_status_over_http() {
    let hooks = DefaultLaunchHooks::default();
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    hooks.start_helper(port).await.unwrap();
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let response = client
        .post(format!("http://127.0.0.1:{port}/backend/status"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let payload: serde_json::Value = response.json().await.unwrap();
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["transport"], "http-helper");
    assert!(payload["hideOfficialUsageAlert"].is_boolean());

    let repair_response = client
        .post(format!("http://127.0.0.1:{port}/backend/repair"))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert!(!repair_response.status().is_success());

    hooks.shutdown_helper(port).await;
}

#[tokio::test]
async fn default_helper_accepts_diagnostic_log_events_over_http() {
    let temp = tempfile::tempdir().unwrap();
    let log_path = temp.path().join("codex-plus.log");
    codex_plus_core::diagnostic_log::set_diagnostic_log_path_for_tests(Some(log_path.clone()));
    let hooks = DefaultLaunchHooks::default();
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    hooks.start_helper(port).await.unwrap();
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("http://127.0.0.1:{port}/diagnostics/log"))
        .json(&serde_json::json!({
            "event": "backend_check_failed",
            "message": "fetch failed",
            "helperBase": format!("http://127.0.0.1:{port}")
        }))
        .send()
        .await
        .unwrap();

    assert!(response.status().is_success());
    let payload: serde_json::Value = response.json().await.unwrap();
    assert_eq!(payload["status"], "ok");
    hooks.shutdown_helper(port).await;

    let contents = std::fs::read_to_string(&log_path).unwrap();
    assert!(contents.contains("renderer.backend_check_failed"));
    assert!(contents.contains("fetch failed"));
    codex_plus_core::diagnostic_log::set_diagnostic_log_path_for_tests(None);
}

#[tokio::test]
async fn launch_lifecycle_runs_enabled_maintenance_without_applying_relay_profile() {
    let temp = tempfile::tempdir().unwrap();
    let app_dir = temp.path().join("Codex.app");
    std::fs::create_dir_all(&app_dir).unwrap();
    let status_store = StatusStore::new(temp.path().join("latest-status.json"));
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let hooks = FakeHooks::new(events.clone())
        .with_settings(BackendSettings {
            provider_sync_enabled: true,
            relay_profiles_enabled: true,
            codex_app_plugin_marketplace_unlock: true,
            ..BackendSettings::default()
        })
        .with_launch_result(CodexLaunch::Process {
            command: vec!["codex".to_string()],
            wait_strategy: codex_plus_core::launcher::ProcessWaitStrategy::TrackedChild,
            macos_cleanup_policy: None,
        });

    let handle = launch_and_inject_with_hooks(
        LaunchOptions {
            app_dir: Some(app_dir.clone()),
            debug_port: 9229,
            helper_port: 57321,
            status_store,
        },
        &hooks,
    )
    .await
    .unwrap();
    handle.wait_for_codex_exit().await.unwrap();

    assert_eq!(
        *events.lock().unwrap(),
        vec![
            "select-debug:9229",
            "select-helper:57321",
            "load-settings",
            "provider-sync",
            "start-helper:57321",
            "launch:9229",
            "inject:9229:57321",
            "status:running",
            "wait-codex",
            "shutdown-helper:57321",
        ]
    );
    let events = events.lock().unwrap().clone();
    assert!(!events.contains(&"apply-relay".to_string()));
    assert!(events.contains(&"provider-sync".to_string()));
    assert_eq!(
        handle
            .status_store
            .load_latest()
            .unwrap()
            .unwrap()
            .codex_app
            .as_deref(),
        Some(app_dir.to_string_lossy().as_ref())
    );
}

#[tokio::test]
async fn launch_lifecycle_passes_configured_extra_args_to_codex_launch() {
    let temp = tempfile::tempdir().unwrap();
    let app_dir = temp.path().join("Codex.app");
    std::fs::create_dir_all(&app_dir).unwrap();
    let status_store = StatusStore::new(temp.path().join("latest-status.json"));
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let hooks = FakeHooks::new(events.clone()).with_settings(BackendSettings {
        codex_extra_args: vec!["--force_high_performance_gpu".to_string()],
        ..BackendSettings::default()
    });

    let handle = launch_and_inject_with_hooks(
        LaunchOptions {
            app_dir: Some(app_dir),
            debug_port: 9229,
            helper_port: 57321,
            status_store,
        },
        &hooks,
    )
    .await
    .unwrap();
    handle.wait_for_codex_exit().await.unwrap();

    assert!(
        events
            .lock()
            .unwrap()
            .contains(&"launch:9229:--force_high_performance_gpu".to_string())
    );
}

#[tokio::test]
async fn launch_lifecycle_passes_native_menu_localization_switch_to_codex_launch() {
    let temp = tempfile::tempdir().unwrap();
    let app_dir = temp.path().join("Codex.app");
    std::fs::create_dir_all(&app_dir).unwrap();
    let status_store = StatusStore::new(temp.path().join("latest-status.json"));
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let hooks = FakeHooks::new(events.clone()).with_settings(BackendSettings {
        codex_app_native_menu_localization: false,
        ..BackendSettings::default()
    });

    let handle = launch_and_inject_with_hooks(
        LaunchOptions {
            app_dir: Some(app_dir),
            debug_port: 9229,
            helper_port: 57321,
            status_store,
        },
        &hooks,
    )
    .await
    .unwrap();
    handle.wait_for_codex_exit().await.unwrap();

    assert!(
        events
            .lock()
            .unwrap()
            .contains(&"launch:9229:native-menu-off".to_string())
    );
}

#[tokio::test]
async fn launch_lifecycle_keeps_js_injection_in_relay_mode() {
    let temp = tempfile::tempdir().unwrap();
    let app_dir = temp.path().join("Codex.app");
    std::fs::create_dir_all(&app_dir).unwrap();
    let status_store = StatusStore::new(temp.path().join("latest-status.json"));
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let hooks = FakeHooks::new(events.clone()).with_settings(BackendSettings {
        launch_mode: codex_plus_core::settings::LaunchMode::Relay,
        ..BackendSettings::default()
    });

    let handle = launch_and_inject_with_hooks(
        LaunchOptions {
            app_dir: Some(app_dir),
            debug_port: 9229,
            helper_port: 57321,
            status_store,
        },
        &hooks,
    )
    .await
    .unwrap();
    handle.wait_for_codex_exit().await.unwrap();

    assert_eq!(
        *events.lock().unwrap(),
        vec![
            "select-debug:9229",
            "select-helper:57321",
            "load-settings",
            "start-helper:57321",
            "launch:9229",
            "inject:9229:57321",
            "status:running",
            "wait-codex",
            "shutdown-helper:57321",
        ]
    );
}

#[tokio::test]
async fn launch_lifecycle_skips_helper_and_injection_when_enhancements_disabled() {
    let temp = tempfile::tempdir().unwrap();
    let app_dir = temp.path().join("Codex.app");
    std::fs::create_dir_all(&app_dir).unwrap();
    let status_store = StatusStore::new(temp.path().join("latest-status.json"));
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let hooks = FakeHooks::new(events.clone()).with_settings(BackendSettings {
        enhancements_enabled: false,
        ..BackendSettings::default()
    });

    let handle = launch_and_inject_with_hooks(
        LaunchOptions {
            app_dir: Some(app_dir),
            debug_port: 9229,
            helper_port: 57321,
            status_store,
        },
        &hooks,
    )
    .await
    .unwrap();
    handle.wait_for_codex_exit().await.unwrap();

    assert_eq!(
        *events.lock().unwrap(),
        vec![
            "select-debug:9229",
            "select-helper:57321",
            "load-settings",
            "launch:9229",
            "status:running",
            "wait-codex",
        ]
    );
}

#[tokio::test]
async fn official_mix_responses_profile_starts_fixed_protocol_proxy_without_enhancements() {
    let temp = tempfile::tempdir().unwrap();
    let app_dir = temp.path().join("Codex.app");
    std::fs::create_dir_all(&app_dir).unwrap();
    let status_store = StatusStore::new(temp.path().join("latest-status.json"));
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let hooks = FakeHooks::new(events.clone()).with_settings(BackendSettings {
        enhancements_enabled: false,
        relay_profiles_enabled: true,
        active_relay_id: "official-mix".to_string(),
        relay_profiles: vec![RelayProfile {
            id: "official-mix".to_string(),
            relay_mode: RelayMode::Official,
            official_mix_api_key: true,
            hide_official_usage_alert: false,
            protocol: RelayProtocol::Responses,
            ..RelayProfile::default()
        }],
        ..BackendSettings::default()
    });

    let handle = launch_and_inject_with_hooks(
        LaunchOptions {
            app_dir: Some(app_dir),
            debug_port: 9229,
            helper_port: 58123,
            status_store,
        },
        &hooks,
    )
    .await
    .unwrap();
    handle.wait_for_codex_exit().await.unwrap();

    let events = events.lock().unwrap().clone();
    assert!(!events.contains(&"remote-control-session-recovery".to_string()));
    assert!(!events.contains(&"provider-sync".to_string()));
    assert!(events.contains(&"select-helper:58123".to_string()));
    assert!(events.contains(&"start-helper:57321".to_string()));
    assert!(events.contains(&"shutdown-helper:57321".to_string()));
    assert!(!events.iter().any(|event| event.starts_with("inject:")));
}

#[tokio::test]
async fn pending_remote_control_recovery_runs_without_an_official_mix_profile() {
    let temp = tempfile::tempdir().unwrap();
    let app_dir = temp.path().join("Codex.app");
    std::fs::create_dir_all(&app_dir).unwrap();
    let status_store = StatusStore::new(temp.path().join("latest-status.json"));
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let hooks = FakeHooks::new(events.clone()).with_pending_remote_control_session_recoveries();

    let handle = launch_and_inject_with_hooks(
        LaunchOptions {
            app_dir: Some(app_dir),
            debug_port: 9229,
            helper_port: 58123,
            status_store,
        },
        &hooks,
    )
    .await
    .unwrap();
    handle.wait_for_codex_exit().await.unwrap();

    assert!(
        events
            .lock()
            .unwrap()
            .contains(&"remote-control-session-recovery".to_string())
    );
}

#[tokio::test]
async fn official_mix_responses_profile_keeps_proxy_when_profile_switching_is_disabled() {
    let temp = tempfile::tempdir().unwrap();
    let app_dir = temp.path().join("Codex.app");
    std::fs::create_dir_all(&app_dir).unwrap();
    let status_store = StatusStore::new(temp.path().join("latest-status.json"));
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let hooks = FakeHooks::new(events.clone()).with_settings(BackendSettings {
        enhancements_enabled: false,
        relay_profiles_enabled: false,
        active_relay_id: "official-mix".to_string(),
        relay_profiles: vec![RelayProfile {
            id: "official-mix".to_string(),
            relay_mode: RelayMode::Official,
            official_mix_api_key: true,
            hide_official_usage_alert: false,
            protocol: RelayProtocol::Responses,
            ..RelayProfile::default()
        }],
        ..BackendSettings::default()
    });

    let handle = launch_and_inject_with_hooks(
        LaunchOptions {
            app_dir: Some(app_dir),
            debug_port: 9229,
            helper_port: 58123,
            status_store,
        },
        &hooks,
    )
    .await
    .unwrap();
    handle.wait_for_codex_exit().await.unwrap();

    let events = events.lock().unwrap().clone();
    assert!(events.contains(&"select-helper:58123".to_string()));
    assert!(events.contains(&"start-helper:57321".to_string()));
    assert!(events.contains(&"shutdown-helper:57321".to_string()));
    assert!(!events.iter().any(|event| event.starts_with("inject:")));
}

#[tokio::test]
async fn launch_lifecycle_does_not_apply_relay_profile_before_launching_codex() {
    let temp = tempfile::tempdir().unwrap();
    let app_dir = temp.path().join("Codex.app");
    std::fs::create_dir_all(&app_dir).unwrap();
    let status_store = StatusStore::new(temp.path().join("latest-status.json"));
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let hooks = FakeHooks::new(events.clone()).with_settings(BackendSettings {
        relay_profiles_enabled: true,
        ..BackendSettings::default()
    });

    let handle = launch_and_inject_with_hooks(
        LaunchOptions {
            app_dir: Some(app_dir),
            debug_port: 9229,
            helper_port: 57321,
            status_store,
        },
        &hooks,
    )
    .await
    .unwrap();
    handle.wait_for_codex_exit().await.unwrap();

    let events = events.lock().unwrap().clone();
    assert!(!events.contains(&"apply-relay".to_string()));
    assert!(events.contains(&"launch:9229".to_string()));
}

#[tokio::test]
async fn launch_lifecycle_skips_active_relay_profile_when_supplier_config_disabled() {
    let temp = tempfile::tempdir().unwrap();
    let app_dir = temp.path().join("Codex.app");
    std::fs::create_dir_all(&app_dir).unwrap();
    let status_store = StatusStore::new(temp.path().join("latest-status.json"));
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let hooks = FakeHooks::new(events.clone()).with_settings(BackendSettings {
        relay_profiles_enabled: false,
        ..BackendSettings::default()
    });

    let handle = launch_and_inject_with_hooks(
        LaunchOptions {
            app_dir: Some(app_dir),
            debug_port: 9229,
            helper_port: 57321,
            status_store,
        },
        &hooks,
    )
    .await
    .unwrap();
    handle.wait_for_codex_exit().await.unwrap();

    let events = events.lock().unwrap().clone();
    assert!(!events.contains(&"apply-relay".to_string()));
    assert!(events.contains(&"launch:9229".to_string()));
}

#[tokio::test]
async fn launch_lifecycle_tolerates_duplicate_context_parent_tables_without_applying_relay() {
    let temp = tempfile::tempdir().unwrap();
    let app_dir = temp.path().join("Codex.app");
    std::fs::create_dir_all(&app_dir).unwrap();
    let status_store = StatusStore::new(temp.path().join("latest-status.json"));
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let hooks = FakeHooks::new(events.clone()).with_settings(BackendSettings {
        relay_common_config_contents: "[mcp_servers]\n".to_string(),
        relay_context_config_contents: "[mcp_servers]\n\n[mcp_servers.ida]\ncommand = \"python\"\n"
            .to_string(),
        relay_profiles: vec![RelayProfile {
            id: "relay-a".to_string(),
            name: "Relay A".to_string(),
            relay_mode: codex_plus_core::settings::RelayMode::PureApi,
            config_contents: r#"model = "gpt-5.5"
model_provider = "custom"

[model_providers.custom]
name = "custom"
wire_api = "responses"
requires_openai_auth = true
base_url = "https://relay.example/v1"
experimental_bearer_token = "sk-test"
"#
            .to_string(),
            auth_contents: r#"{"OPENAI_API_KEY":"sk-test"}"#.to_string(),
            ..RelayProfile::default()
        }],
        active_relay_id: "relay-a".to_string(),
        ..BackendSettings::default()
    });

    let handle = launch_and_inject_with_hooks(
        LaunchOptions {
            app_dir: Some(app_dir),
            debug_port: 9229,
            helper_port: 57321,
            status_store,
        },
        &hooks,
    )
    .await
    .unwrap();
    handle.wait_for_codex_exit().await.unwrap();

    let events = events.lock().unwrap().clone();
    assert!(!events.contains(&"apply-relay".to_string()));
    assert!(events.contains(&"launch:9229".to_string()));
}

#[tokio::test]
async fn launch_lifecycle_enters_degraded_mode_and_retries_when_injection_fails() {
    let temp = tempfile::tempdir().unwrap();
    let app_dir = temp.path().join("Codex.app");
    std::fs::create_dir_all(&app_dir).unwrap();
    let status_store = StatusStore::new(temp.path().join("latest-status.json"));
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let hooks = FakeHooks::new(events.clone()).with_inject_error("inject failed");

    let handle = launch_and_inject_with_hooks(
        LaunchOptions {
            app_dir: Some(app_dir),
            debug_port: 9229,
            helper_port: 57321,
            status_store: status_store.clone(),
        },
        &hooks,
    )
    .await
    .unwrap();

    assert_eq!(
        *events.lock().unwrap(),
        vec![
            "select-debug:9229",
            "select-helper:57321",
            "load-settings",
            "start-helper:57321",
            "launch:9229",
            "inject:9229:57321",
            "status:running_degraded",
        ]
    );
    let status = status_store.load_latest().unwrap().unwrap();
    assert_eq!(status.status, "running_degraded");
    assert!(status.message.contains("Codex launched"));

    handle.wait_for_codex_exit().await.unwrap();
    let events = events.lock().unwrap().clone();
    assert!(events.contains(&"wait-codex".to_string()));
    assert!(events.contains(&"shutdown-helper:57321".to_string()));
    assert!(!events.contains(&"terminate-codex".to_string()));
}

#[tokio::test]
async fn launch_lifecycle_cleans_helper_when_launch_fails_after_helper_started() {
    let temp = tempfile::tempdir().unwrap();
    let app_dir = temp.path().join("Codex.app");
    std::fs::create_dir_all(&app_dir).unwrap();
    let status_store = StatusStore::new(temp.path().join("latest-status.json"));
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let hooks = FakeHooks::new(events.clone()).with_launch_error("launch failed");

    let error = launch_and_inject_with_hooks(
        LaunchOptions {
            app_dir: Some(app_dir),
            debug_port: 9229,
            helper_port: 57321,
            status_store: status_store.clone(),
        },
        &hooks,
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("launch failed"));
    assert_eq!(
        *events.lock().unwrap(),
        vec![
            "select-debug:9229",
            "select-helper:57321",
            "load-settings",
            "start-helper:57321",
            "launch:9229",
            "shutdown-helper:57321",
            "status:failed",
        ]
    );
}

#[tokio::test]
async fn launch_starts_helper_when_chat_protocol_proxy_is_enabled() {
    let temp = tempfile::tempdir().unwrap();
    let app_dir = temp.path().join("Codex.app");
    std::fs::create_dir_all(&app_dir).unwrap();
    let status_store = StatusStore::new(temp.path().join("latest-status.json"));
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let settings = BackendSettings {
        enhancements_enabled: false,
        relay_profiles: vec![RelayProfile {
            id: "relay-chat".to_string(),
            name: "Chat".to_string(),
            model: String::new(),
            base_url: "https://chat-only.example.test/v1".to_string(),
            upstream_base_url: "https://chat-only.example.test/v1".to_string(),
            api_key: "sk-test".to_string(),
            protocol: RelayProtocol::ChatCompletions,
            relay_mode: codex_plus_core::settings::RelayMode::MixedApi,
            official_mix_api_key: false,
            hide_official_usage_alert: false,
            test_model: String::new(),
            config_contents: String::new(),
            auth_contents: String::new(),
            use_common_config: true,
            context_selection: codex_plus_core::settings::RelayContextSelection::default(),
            context_selection_initialized: false,
            context_window: String::new(),
            auto_compact_limit: String::new(),
            model_insert_mode: codex_plus_core::settings::RelayModelInsertMode::default(),
            model_list: String::new(),
            model_windows: String::new(),
            model_vlm: String::new(),
            vlm_api_key: String::new(),
            vlm_model: String::new(),
            vlm_base_url: String::new(),
            user_agent: String::new(),
            sub2api_enabled: false,
            sub2api_multiplier: String::new(),
            model_routes: Vec::new(),
        }],
        active_relay_id: "relay-chat".to_string(),
        ..BackendSettings::default()
    };
    let hooks = FakeHooks::new(events.clone()).with_settings(settings);

    let handle = launch_and_inject_with_hooks(
        LaunchOptions {
            app_dir: Some(app_dir),
            debug_port: 9229,
            helper_port: 58000,
            status_store,
        },
        &hooks,
    )
    .await
    .unwrap();

    let before_stop = events.lock().unwrap().clone();
    assert!(before_stop.contains(&"select-helper:58000".to_string()));
    assert!(before_stop.contains(&"start-helper:57321".to_string()));
    assert!(!before_stop.contains(&"inject:9229:57321".to_string()));

    handle.wait_for_codex_exit().await.unwrap();

    let after_stop = events.lock().unwrap().clone();
    assert!(after_stop.contains(&"wait-codex".to_string()));
    assert!(after_stop.contains(&"shutdown-helper:57321".to_string()));
}

#[tokio::test]
async fn launch_starts_helper_when_model_routing_is_enabled() {
    let temp = tempfile::tempdir().unwrap();
    let app_dir = temp.path().join("Codex.app");
    std::fs::create_dir_all(&app_dir).unwrap();
    let status_store = StatusStore::new(temp.path().join("latest-status.json"));
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let settings = BackendSettings {
        enhancements_enabled: false,
        active_relay_id: "source".to_string(),
        relay_profiles: vec![
            RelayProfile {
                id: "source".to_string(),
                name: "Source".to_string(),
                base_url: "https://source.example.test/v1".to_string(),
                api_key: "sk-source".to_string(),
                model_routes: vec![RelayModelRoute {
                    model: "gpt-5.6-luna".to_string(),
                    target_relay_id: "target".to_string(),
                    target_model: String::new(),
                }],
                ..RelayProfile::default()
            },
            RelayProfile {
                id: "target".to_string(),
                name: "Target".to_string(),
                base_url: "https://target.example.test/v1".to_string(),
                api_key: "sk-target".to_string(),
                ..RelayProfile::default()
            },
        ],
        ..BackendSettings::default()
    };
    let hooks = FakeHooks::new(events.clone()).with_settings(settings);

    let handle = launch_and_inject_with_hooks(
        LaunchOptions {
            app_dir: Some(app_dir),
            debug_port: 9229,
            helper_port: 58000,
            status_store,
        },
        &hooks,
    )
    .await
    .unwrap();

    let before_stop = events.lock().unwrap().clone();
    assert!(before_stop.contains(&"select-helper:58000".to_string()));
    assert!(before_stop.contains(&"start-helper:57321".to_string()));
    assert!(!before_stop.contains(&"inject:9229:57321".to_string()));

    handle.wait_for_codex_exit().await.unwrap();
    let after_stop = events.lock().unwrap().clone();
    assert!(after_stop.contains(&"shutdown-helper:57321".to_string()));
}

#[tokio::test]
async fn launch_lifecycle_cleans_helper_and_codex_when_status_save_fails() {
    let temp = tempfile::tempdir().unwrap();
    let app_dir = temp.path().join("Codex.app");
    std::fs::create_dir_all(&app_dir).unwrap();
    std::fs::write(temp.path().join("status-parent-file"), "not a directory").unwrap();
    let status_store = StatusStore::new(
        temp.path()
            .join("status-parent-file")
            .join("latest-status.json"),
    );
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let hooks =
        FakeHooks::new(events.clone()).with_launch_result(CodexLaunch::PackagedActivation {
            app_user_model_id: "OpenAI.Codex_2p2nqsd0c76g0!App".to_string(),
            arguments: "--remote-debugging-port=9229".to_string(),
            process_id: Some(4242),
        });

    let error = launch_and_inject_with_hooks(
        LaunchOptions {
            app_dir: Some(app_dir),
            debug_port: 9229,
            helper_port: 57321,
            status_store,
        },
        &hooks,
    )
    .await
    .unwrap_err();

    assert!(error.to_string().contains("failed to create directory"));
    assert_eq!(
        *events.lock().unwrap(),
        vec![
            "select-debug:9229",
            "select-helper:57321",
            "load-settings",
            "start-helper:57321",
            "launch:9229",
            "inject:9229:57321",
            "shutdown-helper:57321",
            "terminate-packaged:4242",
            "status:failed",
        ]
    );
}

#[tokio::test]
async fn launch_lifecycle_keeps_packaged_process_id_running_and_retries_when_injection_fails() {
    let temp = tempfile::tempdir().unwrap();
    let app_dir = temp.path().join("Codex.app");
    std::fs::create_dir_all(&app_dir).unwrap();
    let status_store = StatusStore::new(temp.path().join("latest-status.json"));
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let hooks = FakeHooks::new(events.clone())
        .with_launch_result(CodexLaunch::PackagedActivation {
            app_user_model_id: "OpenAI.Codex_2p2nqsd0c76g0!App".to_string(),
            arguments: "--remote-debugging-port=9229".to_string(),
            process_id: Some(4242),
        })
        .with_inject_error("inject failed");

    let handle = launch_and_inject_with_hooks(
        LaunchOptions {
            app_dir: Some(app_dir),
            debug_port: 9229,
            helper_port: 57321,
            status_store,
        },
        &hooks,
    )
    .await
    .unwrap();

    assert!(
        !events
            .lock()
            .unwrap()
            .contains(&"terminate-packaged:4242".to_string())
    );
    handle.wait_for_codex_exit().await.unwrap();
}

#[tokio::test]
async fn default_provider_sync_enabled_fails_instead_of_silently_skipping() {
    let hooks = FakeHooks::new(Arc::new(Mutex::new(Vec::new()))).with_provider_sync_unsupported();

    let error = hooks
        .run_provider_sync()
        .await
        .expect_err("default-style provider sync should be explicit");

    assert!(
        error
            .to_string()
            .contains("provider sync requires launcher hooks")
    );
}

#[tokio::test]
async fn launch_continues_when_plugin_marketplace_config_fails() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let hooks = FakeHooks::new(events.clone())
        .with_plugin_marketplace_error("config.toml TOML parse failed");

    let handle = launch_and_inject_with_hooks(
        LaunchOptions {
            app_dir: Some(PathBuf::from("/Applications/Codex.app")),
            debug_port: 9229,
            helper_port: 57321,
            status_store: StatusStore::new(tempfile::tempdir().unwrap().path().join("status.json")),
        },
        &hooks,
    )
    .await
    .unwrap();

    assert_eq!(handle.debug_port, 9229);
    assert_eq!(
        events.lock().unwrap().as_slice(),
        [
            "select-debug:9229",
            "select-helper:57321",
            "load-settings",
            "plugin-marketplace",
            "start-helper:57321",
            "launch:9229",
            "inject:9229:57321",
            "status:running"
        ]
    );
}

#[test]
fn launcher_macos_cleanup_command_targets_specific_app_bundle() {
    let command = build_macos_cleanup_command(
        Path::new("/Applications/OpenAI Codex.app"),
        MacosCleanupPolicy::QuitIfNotPreviouslyRunning,
    )
    .expect("cleanup command should be allowed");

    assert_eq!(command[0], "osascript");
    assert!(command.iter().any(|part| part.contains("OpenAI Codex")));
    assert!(!command.iter().any(|part| part == "Codex"));
}

#[test]
fn launcher_macos_cleanup_is_skipped_when_app_was_already_running() {
    let command = build_macos_cleanup_command(
        Path::new("/Applications/OpenAI Codex.app"),
        MacosCleanupPolicy::SkipQuitBecauseAlreadyRunning,
    );

    assert_eq!(command, None);
}

#[test]
fn launcher_macos_debug_launch_starts_when_app_is_not_running() {
    assert_eq!(
        select_macos_debug_launch_action(false, false),
        MacosDebugLaunchAction::LaunchNew
    );
}

#[test]
fn launcher_macos_debug_launch_reuses_existing_codex_cdp_instance() {
    assert_eq!(
        select_macos_debug_launch_action(true, true),
        MacosDebugLaunchAction::ReuseRunningDebugApp
    );
}

#[test]
fn launcher_macos_debug_launch_restarts_existing_non_cdp_instance() {
    assert_eq!(
        select_macos_debug_launch_action(true, false),
        MacosDebugLaunchAction::RestartRunningApp
    );
}

#[tokio::test]
async fn default_launch_hooks_provider_sync_enabled_returns_explicit_error() {
    let error = DefaultLaunchHooks::default()
        .run_provider_sync()
        .await
        .expect_err("default provider sync should not silently skip");

    assert!(
        error
            .to_string()
            .contains("provider sync requires launcher hooks")
    );
}

#[derive(Clone)]
struct FakeHooks {
    events: Arc<Mutex<Vec<String>>>,
    settings: BackendSettings,
    launch_result: CodexLaunch,
    launch_error: Option<String>,
    inject_error: Option<String>,
    provider_sync_unsupported: bool,
    plugin_marketplace_error: Option<String>,
    has_pending_remote_control_session_recoveries: bool,
}

impl FakeHooks {
    fn new(events: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            events,
            settings: BackendSettings::default(),
            launch_result: CodexLaunch::Process {
                command: vec!["codex".to_string()],
                wait_strategy: codex_plus_core::launcher::ProcessWaitStrategy::TrackedChild,
                macos_cleanup_policy: None,
            },
            launch_error: None,
            inject_error: None,
            provider_sync_unsupported: false,
            plugin_marketplace_error: None,
            has_pending_remote_control_session_recoveries: false,
        }
    }

    fn with_settings(mut self, settings: BackendSettings) -> Self {
        self.settings = settings;
        self
    }

    fn with_launch_result(mut self, launch_result: CodexLaunch) -> Self {
        self.launch_result = launch_result;
        self
    }

    fn with_inject_error(mut self, message: &str) -> Self {
        self.inject_error = Some(message.to_string());
        self
    }

    fn with_launch_error(mut self, message: &str) -> Self {
        self.launch_error = Some(message.to_string());
        self
    }

    fn with_provider_sync_unsupported(mut self) -> Self {
        self.provider_sync_unsupported = true;
        self
    }

    fn with_plugin_marketplace_error(mut self, message: &str) -> Self {
        self.plugin_marketplace_error = Some(message.to_string());
        self
    }

    fn with_pending_remote_control_session_recoveries(mut self) -> Self {
        self.has_pending_remote_control_session_recoveries = true;
        self
    }

    fn event(&self, event: impl Into<String>) {
        self.events.lock().unwrap().push(event.into());
    }
}

#[async_trait::async_trait(?Send)]
impl LaunchHooks for FakeHooks {
    fn resolve_app_dir(
        &self,
        app_dir: Option<&Path>,
        _settings: &BackendSettings,
    ) -> anyhow::Result<PathBuf> {
        app_dir
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow::anyhow!("missing app dir"))
    }

    fn select_debug_port(&self, requested: u16) -> u16 {
        self.event(format!("select-debug:{requested}"));
        requested
    }

    fn select_helper_port(&self, requested: u16) -> u16 {
        self.event(format!("select-helper:{requested}"));
        requested
    }

    async fn load_settings(&self) -> anyhow::Result<BackendSettings> {
        self.event("load-settings");
        Ok(self.settings.clone())
    }

    async fn run_provider_sync(&self) -> anyhow::Result<()> {
        self.event("provider-sync");
        if self.provider_sync_unsupported {
            anyhow::bail!("provider sync requires launcher hooks");
        }
        Ok(())
    }

    fn has_pending_remote_control_session_recoveries(&self) -> bool {
        self.has_pending_remote_control_session_recoveries
    }

    async fn run_remote_control_session_recovery(&self) -> anyhow::Result<()> {
        self.event("remote-control-session-recovery");
        Ok(())
    }

    async fn apply_active_relay_profile(&self, settings: &BackendSettings) -> anyhow::Result<()> {
        if !settings.relay_profiles_enabled {
            return Ok(());
        }
        self.event("apply-relay");
        Ok(())
    }

    async fn ensure_plugin_marketplace_config(
        &self,
        _settings: &BackendSettings,
    ) -> anyhow::Result<()> {
        if let Some(message) = &self.plugin_marketplace_error {
            self.event("plugin-marketplace");
            anyhow::bail!(message.clone());
        }
        Ok(())
    }

    async fn start_helper(&self, helper_port: u16) -> anyhow::Result<u16> {
        self.event(format!("start-helper:{helper_port}"));
        Ok(helper_port)
    }

    async fn launch_codex(
        &self,
        app_dir: &Path,
        debug_port: u16,
        settings: &BackendSettings,
        extra_args: &[String],
    ) -> anyhow::Result<CodexLaunch> {
        assert!(app_dir.ends_with("Codex.app"));
        let launch_detail = if extra_args.is_empty() {
            format!("launch:{debug_port}")
        } else {
            format!("launch:{debug_port}:{}", extra_args.join(","))
        };
        if settings.codex_app_native_menu_localization {
            self.event(launch_detail);
        } else {
            self.event(format!("{launch_detail}:native-menu-off"));
        }
        if let Some(message) = &self.launch_error {
            anyhow::bail!(message.clone());
        }
        Ok(self.launch_result.clone())
    }

    async fn inject(&self, debug_port: u16, helper_port: u16) -> anyhow::Result<()> {
        self.event(format!("inject:{debug_port}:{helper_port}"));
        if let Some(message) = &self.inject_error {
            anyhow::bail!(message.clone());
        }
        Ok(())
    }

    async fn ensure_injection(&self, debug_port: u16, helper_port: u16, _app_dir: &Path) -> bool {
        self.event(format!("inject:{debug_port}:{helper_port}"));
        self.inject_error.is_none()
    }

    async fn start_bridge_watchdog(
        &self,
        _debug_port: u16,
        _helper_port: u16,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn write_status(&self, status: &str) {
        self.event(format!("status:{status}"));
    }

    async fn wait_for_codex_exit(
        &self,
        _launch: &CodexLaunch,
        _debug_port: u16,
    ) -> anyhow::Result<()> {
        self.event("wait-codex");
        Ok(())
    }

    async fn shutdown_helper(&self, helper_port: u16) {
        self.event(format!("shutdown-helper:{helper_port}"));
    }

    async fn terminate_codex(&self, launch: &CodexLaunch) {
        if let Some(process_id) = launch.process_id() {
            self.event(format!("terminate-packaged:{process_id}"));
        } else {
            self.event("terminate-codex");
        }
    }
}

/// 微信连接跑的是 `codex app-server`。原先没配路径时直接用 PATH 上的 "codex",
/// 而 macOS 的 .app 和 Windows 的 MSIX 都不会把它放进 PATH —— 一发消息就报
/// 「无法启动 Codex app-server」。用户显式配了路径的话必须原样用他的。
/// Windows: 商店(MSIX)装的 codex.exe 文件确实存在,但 WindowsApps 的 ACL 不允许
/// 普通进程执行它 —— is_file() 为真、CreateProcess 却 Access is denied。
/// 所以解析必须优先取 LOCALAPPDATA 下那份解包出来的、真的能跑的 CLI。
///
/// 这条测试只在**装了 Codex 的 Windows 机器**上有意义,其余环境自动跳过 ——
/// 它守的是「存在 != 能执行」这个区别,而不是某台机器的具体路径。
#[test]
#[cfg(windows)]
fn wechat_prefers_the_runnable_cli_over_the_msix_copy() {
    use codex_plus_core::app_paths::codex_cli_command;

    let resolved = codex_cli_command("");
    if resolved == "codex" {
        return; // 这台机器没装 Codex,没什么可断言的
    }
    assert!(
        !resolved.contains("WindowsApps"),
        "解析到了 WindowsApps 下的副本,那份执行会被 ACL 拒绝: {resolved}"
    );
}

#[test]
fn wechat_uses_the_configured_codex_path_verbatim() {
    use codex_plus_core::app_paths::codex_cli_command;

    assert_eq!(codex_cli_command("/opt/my/codex"), "/opt/my/codex");
    assert_eq!(codex_cli_command("  /opt/my/codex  "), "/opt/my/codex");
}

/// 没配路径时不能是空字符串 —— 空的会让 Command::new("") 直接失败,
/// 错误信息还什么都看不出来。
#[test]
fn wechat_falls_back_to_something_runnable_when_unconfigured() {
    use codex_plus_core::app_paths::codex_cli_command;

    let resolved = codex_cli_command("");
    assert!(!resolved.is_empty(), "回落值不能是空的");
    assert!(
        resolved.ends_with("codex") || resolved.ends_with("codex.exe"),
        "回落值应指向 codex 二进制:{resolved}"
    );
}

/// 上面两条微信测试**抓不住**它们本来要防的 bug:回落值是字面量 "codex",
/// 断言只要求「以 codex 结尾」,所以把 find_codex_cli 改成永远找不到,它们照样全绿
/// (2026-08-22 实测过)。真正要钉的是「能在 Codex 客户端的安装包里找到 codex」。
///
/// 用户 mac 上的真实路径是 /Applications/ChatGPT.app/Contents/Resources/codex ——
/// 第一个用例就照着它摆。
#[test]
fn wechat_finds_codex_inside_the_client_app_bundle() {
    use codex_plus_core::app_paths::find_codex_cli;

    let exe = if cfg!(windows) { "codex.exe" } else { "codex" };
    let root = std::env::temp_dir().join(format!("recodex-findcodex-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);

    // 布局一:macOS 的 .app 包 —— Contents/Resources 下(线上用户就是这种)
    let bundle = root.join("ChatGPT.app");
    let resources = bundle.join("Contents").join("Resources");
    std::fs::create_dir_all(&resources).unwrap();
    let expected = resources.join(exe);
    std::fs::write(&expected, b"#!/bin/sh\n").unwrap();
    assert_eq!(
        Some(expected.clone()),
        find_codex_cli(Some(&bundle)),
        "没能在 .app 包里找到 codex —— 微信一发消息就会报「无法启动 Codex app-server」"
    );

    // 布局二:同一个包里只有 Contents/MacOS/codex 时也要找得到
    std::fs::remove_file(&expected).unwrap();
    let macos_dir = bundle.join("Contents").join("MacOS");
    std::fs::create_dir_all(&macos_dir).unwrap();
    let alt = macos_dir.join(exe);
    std::fs::write(&alt, b"#!/bin/sh\n").unwrap();
    assert_eq!(Some(alt), find_codex_cli(Some(&bundle)), "Contents/MacOS 布局没覆盖到");

    // 包里根本没有 codex 时必须如实返回 None,而不是瞎给一个不存在的路径
    let empty = root.join("Empty.app");
    std::fs::create_dir_all(empty.join("Contents").join("Resources")).unwrap();
    assert_eq!(None, find_codex_cli(Some(&empty)), "找不到就该是 None");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn bridge_alert_threshold_lands_about_a_minute_after_the_bridge_dies() {
    use codex_plus_core::launcher;
    // 太早 = Codex 正常重启时误报;太晚 = 客户又要蒙半天。
    // 阈值前累计等待要落在「明显不是抖动」但「一分钟量级」的区间。
    let elapsed = launcher::bridge_watchdog_elapsed_secs(launcher::BRIDGE_ALERT_AFTER_FAILURES);
    assert_eq!(elapsed, 60);
    assert!(elapsed >= 30, "提醒太早,Codex 重启会误报: {elapsed}s");
    assert!(elapsed <= 120, "提醒太晚,失去可见性的意义: {elapsed}s");

    // 阈值本身必须落在退避的前两档里 —— 否则提醒会等到 30/60 秒一跳时才发出。
    assert!(launcher::bridge_watchdog_delay(launcher::BRIDGE_ALERT_AFTER_FAILURES - 1)
        .as_secs()
        <= 15);
}

#[test]
fn bridge_watchdog_elapsed_is_zero_before_any_failure() {
    use codex_plus_core::launcher;
    assert_eq!(launcher::bridge_watchdog_elapsed_secs(0), 0);
    assert_eq!(launcher::bridge_watchdog_elapsed_secs(1), 5);
    assert_eq!(launcher::bridge_watchdog_elapsed_secs(3), 15);
}

#[test]
fn retry_log_sampling_keeps_the_first_cause_and_the_magnitude() {
    use codex_plus_core::diagnostic_log::should_log_retry_attempt;

    // 第一次必须留:它带着首因(CDP 拒连 / 注入脚本报错)。
    assert!(should_log_retry_attempt(1));
    for attempt in [2u32, 4, 8, 16, 32, 64] {
        assert!(should_log_retry_attempt(attempt), "attempt {attempt} 应保留");
    }
    for attempt in [3u32, 5, 7, 9, 15, 17, 72, 119, 120] {
        assert!(!should_log_retry_attempt(attempt), "attempt {attempt} 应丢弃");
    }

    // 线上实测的两个上限:ensure_injection 120 次、菜单汉化 20 次。
    // 采样后剩下的条数要小到「不会把首因冲掉」。
    let kept = |limit: u32| (1..=limit).filter(|a| should_log_retry_attempt(*a)).count();
    assert_eq!(kept(120), 7, "120 次重试最多留 7 条");
    assert_eq!(kept(20), 5, "20 次重试最多留 5 条");
    assert_eq!(kept(0), 0);

    // 两个上限本身都**不是** 2 的幂,也就是说「最后一次」一定会被采样丢掉。
    // 所以两条路都必须另有终态日志兜底(ensure_injection_exhausted /
    // native_menu.localization_failed),否则「跑满了没成」这件事就无处可查。
    assert!(!should_log_retry_attempt(120));
    assert!(!should_log_retry_attempt(20));
}

#[test]
fn helper_port_fallback_is_only_safe_when_the_port_is_not_a_contract() {
    use codex_plus_core::launcher::helper_port_fallback_is_safe;

    // 绑到了请求的端口 —— 两种模式都没问题。
    assert!(helper_port_fallback_is_safe(false, 57321, 57321));
    assert!(helper_port_fallback_is_safe(true, 57321, 57321));

    // 只开增强功能:端口是进程内实现细节,换一个远好过整个增强层不可用。
    assert!(helper_port_fallback_is_safe(false, 57321, 49812));

    // 协议代理:端口被写进了 config.toml 的 base_url,换掉之后 Codex 会一直往
    // 老端口发请求 ——「能启动但一句话都发不出去」,必须当失败处理。
    assert!(!helper_port_fallback_is_safe(true, 57321, 49812));
    assert!(!helper_port_fallback_is_safe(true, 57321, 57322));
}

#[test]
fn bridge_watchdog_only_backs_off_when_the_bridge_stays_broken() {
    use codex_plus_core::launcher::BridgeWatchdogOutcome;

    // 这条守的是一个真实踩过的坑:原来的 bool 返回值里,「桥好好的」和
    // 「重注入失败」都是 false。照那个 bool 退避,一台**完全正常**的机器
    // 也会一路退到 60 秒一跳,看门狗形同虚设。
    assert!(BridgeWatchdogOutcome::Healthy.bridge_is_usable());
    assert!(BridgeWatchdogOutcome::Reinjected.bridge_is_usable());
    assert!(!BridgeWatchdogOutcome::Failed.bridge_is_usable());

    // 模拟看门狗的计数走向:健康 → 断一次修好 → 连续修不好才开始拉长间隔。
    let mut failures: u32 = 0;
    let mut delays = Vec::new();
    for outcome in [
        BridgeWatchdogOutcome::Healthy,
        BridgeWatchdogOutcome::Healthy,
        BridgeWatchdogOutcome::Reinjected,
        BridgeWatchdogOutcome::Healthy,
        BridgeWatchdogOutcome::Failed,
        BridgeWatchdogOutcome::Failed,
        BridgeWatchdogOutcome::Failed,
        BridgeWatchdogOutcome::Failed,
    ] {
        if outcome.bridge_is_usable() {
            failures = 0;
        } else {
            failures += 1;
        }
        delays.push(codex_plus_core::launcher::bridge_watchdog_delay(failures).as_secs());
    }
    // 前四跳(健康/刚修好)必须全都保持 5 秒的快节奏。
    assert_eq!(&delays[..4], &[5, 5, 5, 5], "正常状态被误判成失败了: {delays:?}");
    // 之后连续失败才逐级拉长。
    assert_eq!(&delays[4..], &[5, 5, 15, 15], "{delays:?}");

    // 修好一次就必须立刻回到快节奏,不能带着历史失败继续退避。
    let mut failures: u32 = 9;
    if BridgeWatchdogOutcome::Reinjected.bridge_is_usable() {
        failures = 0;
    }
    assert_eq!(
        codex_plus_core::launcher::bridge_watchdog_delay(failures).as_secs(),
        5
    );
}

/// 弹窗必须**默认关闭**,而且必须由启动器的 main 显式打开。
///
/// 这条守卫是踩出来的:`alert_once_blocking` 一度在跑 `cargo test` 时真往开发者
/// 桌面上弹框,还阻塞等人点确定 —— 集成测试链接的是 dev 编译的 lib,`cfg(test)`
/// 对它是 false,挡不住。测试里本来就有故意触发错误路径的用例
/// (launch_lifecycle_cleans_helper_and_codex_when_status_save_fails)。
#[test]
fn user_alert_is_off_unless_the_launcher_turns_it_on() {
    use codex_plus_core::user_alert;

    // 这个测试进程从头到尾没人调过 enable(),所以必须是关的。
    assert!(
        !user_alert::is_enabled(),
        "弹窗在测试进程里是开的 —— 跑 cargo test 会往桌面弹框并阻塞等点击"
    );
    // 关着的时候提示必须是「不弹、返回 false」,而不是弹了不告诉调用方。
    assert!(!user_alert::alert_once("t", "b"));
    assert!(!user_alert::alert_once_blocking("t", "b"));

    // 另一半:出货的 main 必须真的打开它,否则线上又变回一声不吭。
    let main_rs = include_str!("../../../apps/codex-plus-launcher/src/main.rs");
    let code: String = main_rs
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("
");
    assert!(
        code.contains("user_alert::enable()"),
        // 破坏验证过:把 main.rs 里那行注释掉,这条会红。
        "launcher 的 main 没有调 user_alert::enable(),线上将不会有任何提示"
    );
}

/// 菜单汉化和注入等的是**同一个 Codex 进程**开出来的两个端口,超时必须同量级。
///
/// 老配置是 20×500ms=10 秒,而注入等 120 秒 —— 12 倍差距。线上后果:6 台设备报
/// 菜单重试失败、3 台彻底失败,其中一台**桥完全正常只有菜单挂了**,证明不是
/// Codex 没起来,是慢机器上 inspector 就绪得比 10 秒晚。
#[test]
fn menu_localization_waits_as_long_as_injection_does() {
    let source = include_str!("../src/native_menu.rs");
    let retries: usize = source
        .split_once("const MENU_LOCALIZATION_RETRIES: usize = ")
        .and_then(|(_, rest)| rest.split_once(';'))
        .and_then(|(value, _)| value.trim().parse().ok())
        .expect("读不到 MENU_LOCALIZATION_RETRIES");
    let delay_secs: u64 = source
        .split_once("const MENU_LOCALIZATION_RETRY_DELAY: Duration = Duration::from_secs(")
        .and_then(|(_, rest)| rest.split_once(')'))
        .and_then(|(value, _)| value.trim().parse().ok())
        .expect("读不到 MENU_LOCALIZATION_RETRY_DELAY(改成非整秒了?)");

    let budget = retries as u64 * delay_secs;
    // ensure_injection 是 120 次 × 1 秒。菜单这边不必分毫不差,但不能再差一个量级。
    assert!(
        budget >= 60,
        "菜单汉化只等 {budget} 秒,慢机器上 inspector 还没就绪就放弃了"
    );
    assert!(
        budget <= 180,
        "等 {budget} 秒太久:Codex 真不支持 inspector 时会空转这么长"
    );
}

/// `ensure_injection` 彻底失败时必须记下「端口现在还能不能绑」。
///
/// 线上那几台设备只报「连不上 CDP」,而这背后是两种完全不同的故障:
/// 端口被别人占了(冲突),还是没人占、Codex 自己没起 CDP。分不清就只能猜 ——
/// 2026-09-05 就为此浪费了一轮排查。
#[test]
fn exhausted_diagnostic_distinguishes_port_conflict_from_missing_cdp() {
    let source = include_str!("../src/launcher.rs");
    let code: String = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("
");
    let exhausted = code
        .split_once("launcher.ensure_injection_exhausted")
        .expect("终态诊断不见了")
        .1;
    let exhausted = &exhausted[..exhausted.find(");").unwrap_or(exhausted.len())];

    assert!(
        // 带引号精确比对:字段改名成 debug_port_free_v2 时服务端就查不到了,
        // 而子串匹配会照样绿 —— 今晚已经在白名单守卫上栽过一次。
        exhausted.contains("\"debug_port_free\""),
        "终态诊断没记端口占用情况,现场又会分不清端口冲突和 Codex 没起 CDP"
    );
    // 只认函数名,不认「函数名(参数)」连写:rustfmt 把参数换到下一行就会误报,
    // 而守卫误报的代价是被人整条删掉。这个函数在本块里只可能用于探端口,够精确了。
    assert!(
        exhausted.contains("can_bind_loopback_port"),
        "debug_port_free 必须是真去绑一次得出的,不能是别处抄来的旧值"
    );
}

/// 看门狗判定「彻底断了」时必须留下一条带证据的标记。
///
/// 现场那 4 台设备只有一串 bridge.health_check_failed —— 既看不出哪一刻算断定,
/// 也分不清端口被占还是 Codex 没起 CDP。弹窗本身只记标题,不带这些上下文。
#[test]
fn give_up_marker_carries_the_evidence_the_field_data_was_missing() {
    let source = include_str!("../src/launcher.rs");
    let code: String = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("
");
    let marker = code
        .split_once("bridge.gave_up")
        .expect("放弃标记不见了")
        .1;
    let marker = &marker[..marker.find("alert_once").unwrap_or(marker.len())];

    // 字段名一律带引号比对。子串匹配会让 `debug_port_free_v2` 这种改名照样绿,
    // 而字段一改服务端就查不到了 —— 今晚已经在白名单守卫上栽过一次。
    for field in [
        "\"debug_port_free\"",
        "\"consecutive_failures\"",
        "\"elapsed_secs\"",
        "\"error\"",
    ] {
        assert!(marker.contains(field), "放弃标记缺少 {field}");
    }
    // 必须真去绑一次,不能写死。同上只认函数名,避免 rustfmt 换行导致误报。
    assert!(marker.contains("can_bind_loopback_port"));
    // 没有 error 字段就不会被上报挑中(事件名里没有 fail/error 关键词)。
    assert!(
        marker.contains("\"error\":"),
        "bridge.gave_up 名字里没有上报关键词,必须靠 error 字段才传得回来"
    );
}

/// 三条「等 CDP」的路在放弃时都必须留下同样的证据,少一条就又要靠猜。
///
/// 线上那轮排查卡住,正是因为只看得到「连不上」,分不清端口被占还是对端没起来:
///   - launcher.ensure_injection_exhausted —— 启动时注入等不到 CDP
///   - bridge.gave_up                     —— 运行中桥断了修不回来
///   - native_menu.localization_failed    —— 菜单汉化等不到 inspector
#[test]
fn all_three_cdp_terminal_diagnostics_carry_port_evidence() {
    let source = include_str!("../src/launcher.rs");
    let code: String = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("
");

    for (event, port_field) in [
        ("launcher.ensure_injection_exhausted", "debug_port_free"),
        ("bridge.gave_up", "debug_port_free"),
        ("native_menu.localization_failed", "inspector_port_free"),
    ] {
        let body = code
            .split_once(event)
            .unwrap_or_else(|| panic!("找不到终态诊断 {event}"))
            .1;
        let body = &body[..body.find(");").unwrap_or(body.len())];
        assert!(
            body.contains(&format!("\"{port_field}\"")),
            "{event} 缺少 {port_field} —— 现场又会分不清端口冲突和对端没起来"
        );
        // 事件名里没有 fail/error 关键词的,得靠 error 字段才上报得回来。
        assert!(
            body.contains("\"error\""),
            "{event} 缺少 error 字段,可能传不回服务端"
        );
    }
}

/// 菜单汉化放弃时,错误里必须带上「等了多久」。
///
/// 线上 24 条终态失败清一色是 `failed to query CDP targets` —— 只知道连不上,
/// 不知道是等满了才放弃还是中途退出。而这一版恰好把超时从 10 秒提到 120 秒,
/// 没有这个数字就无法判断改动是否生效。
#[test]
fn menu_failure_says_how_long_it_waited() {
    let source = include_str!("../src/native_menu.rs");
    let code: String = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("
");

    assert!(
        code.contains("MENU_LOCALIZATION_RETRIES") && code.contains(".context(format!("),
        "终态错误没带上重试次数/等待时长"
    );
    // 必须由常量算出来,不能写死 —— 否则改了超时这句话就开始撒谎。
    let tail = code
        .split_once(".context(format!(")
        .expect("找不到终态 context")
        .1;
    let tail = &tail[..tail.find(")))").unwrap_or(tail.len())];
    assert!(
        tail.contains("MENU_LOCALIZATION_RETRIES") && tail.contains("MENU_LOCALIZATION_RETRY_DELAY"),
        "等待时长必须由两个常量算出,写死的话改超时就对不上了"
    );
}

/// helper 绑的地址只有 loopback 才是安全的。
///
/// `CODEX_PLUS_HELPER_BIND` 没有校验,设成 0.0.0.0 就把账号/额度/登录接口
/// 暴露到局域网。判定写错的后果是「暴露了却不告警」,比不告警更糟 ——
/// 所以两个方向都要钉住。
#[test]
fn loopback_bind_host_detection_covers_both_directions() {
    use codex_plus_core::launcher::is_loopback_bind_host;

    // 只有本机能连的写法,一个都不能误报成「暴露了」。
    for safe in ["127.0.0.1", "localhost", "LOCALHOST", "::1", "[::1]", " 127.0.0.1 ", "127.7.7.7"] {
        assert!(is_loopback_bind_host(safe), "{safe} 被误判成对外暴露");
    }
    // 这些是真暴露,漏一个就等于出了事也不知道。
    for exposed in ["0.0.0.0", "::", "192.168.1.10", "10.0.0.5", "example.com", ""] {
        assert!(!is_loopback_bind_host(exposed), "{exposed} 没被判成对外暴露");
    }
}

/// 分辨「本机 Electron 页面」和「外面的网站」。
///
/// helper 现在对所有响应都回 `Access-Control-Allow-Origin: *` 且不校验来源,
/// 而它背后有 /uninstall、/delete、/llm-proxy 这些接口。这个判定是后续
/// 上白名单的依据,判错一边就等于漏报攻击、判错另一边就等于把自家脚本告成攻击。
#[test]
fn helper_origin_classification_separates_electron_from_the_web() {
    use codex_plus_core::launcher::helper_origin_is_local;

    // Electron 页面的几种真实形态:file:// 页面发出的字面量就是 "null"。
    for local in [
        "null",
        "NULL",
        "app://-",
        "app://./index.html",
        "file://",
        "http://127.0.0.1:57321",
        "http://localhost:3000",
        "http://[::1]:8080",
        "  null  ",
    ] {
        assert!(helper_origin_is_local(local), "{local} 被当成外部网站了");
    }
    // 外面的网站:漏判一个就等于有网页在调 /uninstall 而我们收不到告警。
    for remote in [
        "https://evil.example",
        "http://evil.example",
        "https://chatgpt.com",
        "https://127.0.0.1.evil.com",
        "http://localhost.evil.com",
        "",
    ] {
        assert!(!helper_origin_is_local(remote), "{remote} 被当成本机了");
    }
}

/// Origin 观察集合必须封顶 —— 它的内容完全由**攻击者**决定。
///
/// 恶意页面轮换 sub1.evil.com / sub2.evil.com… 每个都是新 origin;不封顶的话
/// 我为了观察攻击反倒给了对方一个内存与日志的放大器。
#[test]
fn origin_tracking_is_capped_because_the_input_is_attacker_controlled() {
    let source = include_str!("../src/launcher.rs");
    let code: String = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("
");

    assert!(
        code.contains("MAX_TRACKED_HELPER_ORIGINS"),
        "origin 观察集合没有上限,恶意页面能用轮换子域把它撑爆"
    );
    // 上限必须在插入**之前**判,否则每次都会先插再看,照样无限长。
    let note = code
        .split_once("fn note_helper_request_origin")
        .expect("找不到 origin 观察器")
        .1;
    let cap_at = note.find("MAX_TRACKED_HELPER_ORIGINS");
    let insert_at = note.find("seen.insert(");
    assert!(
        matches!((cap_at, insert_at), (Some(cap), Some(insert)) if cap < insert),
        "上限判断必须排在 insert 之前"
    );
}

/// 非 Windows 上端口选择是**直通**的 —— 这不是疏漏,是有意的,
/// 因为 `start_helper` 那层已经改成「直接 bind、失败退 port 0」。
///
/// 钉住这个组合:哪天有人把 start_helper 的兜底拿掉,mac 用户遇到端口冲突
/// 就会退回到「增强功能全挂且无补救」——那正是这一版要修的东西。
#[test]
fn non_windows_port_selection_relies_on_start_helper_fallback() {
    use codex_plus_core::ports::select_platform_loopback_port_with;

    // 非 Windows:哪怕端口明摆着占着,也原样返回,不换。
    assert_eq!(
        select_platform_loopback_port_with(57321, false, |_| false, || 49999),
        57321
    );
    // Windows:占用时才换。
    assert_eq!(
        select_platform_loopback_port_with(57321, true, |_| false, || 49999),
        49999
    );
    assert_eq!(
        select_platform_loopback_port_with(57321, true, |_| true, || 49999),
        57321
    );

    // 兜底必须还在:start_helper 绑不上时要退到 port 0 自己持有 listener。
    let launcher = include_str!("../src/launcher.rs");
    assert!(
        launcher.contains("bind((bind_host.as_str(), 0))"),
        "start_helper 的 port 0 兜底没了 —— 非 Windows 上端口冲突将无处可退"
    );
}

/// 上报白名单靠**事件名字符串**匹配,而事件名写在另一个 crate 里。
///
/// 改一处忘了另一处的后果是**静默不上报** —— 事件照写、日志照有,就是永远传不
/// 回服务端,而且没有任何报错。这一版新加的几个信号(user_alert / reinject_ok /
/// key_refreshed)全靠白名单,名字漂了就等于白做。
#[test]
fn always_report_whitelist_matches_the_events_we_actually_emit() {
    let flush = include_str!("../../recodex-integration/src/diagnostics_flush.rs");
    let whitelist = flush
        .split_once("const ALWAYS_REPORT")
        .expect("找不到 ALWAYS_REPORT")
        .1;
    let whitelist = &whitelist[..whitelist.find("];").unwrap_or(whitelist.len())];

    // 事件散在三个文件里,逐一搜过去 —— 写这条守卫时只查了 launcher.rs,
    // 结果把「user_alert 写在 user_alert.rs」误报成名字漂了。
    let emitters = [
        include_str!("../src/launcher.rs"),
        include_str!("../src/user_alert.rs"),
        include_str!("../../../apps/codex-plus-launcher/src/main.rs"),
    ];

    for event in [
        "launcher.user_alert",
        "bridge.reinject_ok",
        "launcher.recodex_key_refreshed_from_user_scope",
    ] {
        // 必须带引号比对:白名单里是 `"launcher.user_alert",` 这种字面量,
        // 光用 contains(event) 的话,改成 `"launcher.user_alert_RENAMED"` 也能
        // 匹配上 —— 实测过,那样这条守卫就是假的。
        assert!(
            whitelist.contains(&format!("\"{event}\"")),
            "{event} 不在 ALWAYS_REPORT 里,它名字里没有 fail/error 关键词,传不回来"
        );
        assert!(
            emitters
                .iter()
                .any(|source| source.contains(&format!("\"{event}\""))),
            "{event} 在白名单里却没有任何地方写它 —— 名字漂了?"
        );
    }
}

/// 外来 origin 的告警必须带上它**想调什么**。
///
/// 只有一个陌生域名的话,分不清是某个页面误触了 /settings/get,还是冲着
/// /uninstall、/llm-proxy 来的 —— 而这两者的处置完全不同。
/// 反过来自家 origin 不记路径:那是用户的正常操作轨迹,不该传上去。
#[test]
fn external_origin_alert_says_what_it_tried_to_call() {
    let source = include_str!("../src/launcher.rs");
    let code: String = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("
");
    let note = code
        .split_once("fn note_helper_request_origin")
        .expect("找不到 origin 观察器")
        .1;
    let note = &note[..note.find("
fn ").unwrap_or(note.len())];

    for field in ["\"method\"", "\"path\"", "\"origin\"", "\"local\""] {
        assert!(note.contains(field), "外来 origin 告警缺少 {field}");
    }
    // method/path 必须按 local 分支给,否则会把自家请求的路径也传上去。
    assert!(
        note.matches("if local").count() >= 3,
        "method/path/error 都该只在非本机 origin 时才带值"
    );
}

/// 截断攻击者可控的字符串时**绝不能按字节切**。
///
/// origin 和 path 由请求方随手填,塞几个中文或 emoji 就能让 `&s[..n]` 切在
/// 多字节字符中间 —— 那是 panic,而 panic 在 helper 的连接处理任务里就是
/// 一个现成的 DoS 触发器。这条用真实的多字节输入把它钉死。
#[test]
fn truncation_never_panics_on_multibyte_input() {
    use codex_plus_core::launcher::truncate_for_log;

    // 每个字符 3 字节,长度远超上限 —— 按字节切必然落在字符中间。
    let chinese = "冲".repeat(500);
    let truncated = truncate_for_log(&chinese);
    assert_eq!(truncated.chars().count(), 256);
    assert!(truncated.chars().all(|c| c == '冲'), "截出了半个字符");

    // emoji 是 4 字节,同样不能切坏。
    let emoji = "🔥".repeat(500);
    assert_eq!(truncate_for_log(&emoji).chars().count(), 256);

    // 短输入原样返回,别把正常 origin 也改了。
    assert_eq!(truncate_for_log("https://evil.example"), "https://evil.example");
    assert_eq!(truncate_for_log(""), "");
}

/// 桥彻底断掉时要先自己修，而不是直接叫用户手动退出 Codex。
///
/// 线上诊断上报里这一类占了一半（281 条里 146 条），根因全是 CDP 端口连接被拒 ——
/// Codex 不是被启动器带调试端口拉起的，端口根本不存在，光重新注入永远修不好。
#[test]
fn bridge_recovery_fires_exactly_once_at_the_give_up_point() {
    use codex_plus_core::launcher::{
        should_recover_bridge, BRIDGE_ALERT_AFTER_FAILURES, BRIDGE_RECOVER_AFTER_FAILURES,
    };

    // 阈值之前不动手：要给「Codex 正在重启」这类几秒自愈的抖动留窗口，
    // 重启会打断用户正在进行的对话。
    for failures in 0..BRIDGE_RECOVER_AFTER_FAILURES {
        assert!(
            !should_recover_bridge(failures),
            "第 {failures} 次失败就重启会打断正常抖动"
        );
    }

    assert!(should_recover_bridge(BRIDGE_RECOVER_AFTER_FAILURES));

    // 之后不再反复重启：桥一直不通计数会继续涨，每涨一次重启一遍
    // 会让用户彻底没法用。修一次不成就交给提醒。
    for failures in (BRIDGE_RECOVER_AFTER_FAILURES + 1)..(BRIDGE_RECOVER_AFTER_FAILURES + 10) {
        assert!(
            !should_recover_bridge(failures),
            "第 {failures} 次不该再重启一遍"
        );
    }

    // 自愈和提醒在同一时刻判定：先尝试修，修不动才打扰用户。
    assert_eq!(BRIDGE_RECOVER_AFTER_FAILURES, BRIDGE_ALERT_AFTER_FAILURES);
}
