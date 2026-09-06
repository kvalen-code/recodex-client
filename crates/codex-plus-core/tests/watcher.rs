use codex_plus_core::watcher::{
    build_spawn_launcher_command, build_watcher_install_plan, cdp_listening, codex_process_ids,
    disable_watcher_at, enable_watcher_at, filter_killable_launcher_processes,
    process_id_is_running, process_ids_still_running, should_recover_stale_launcher,
    watcher_disabled_flag,
};

#[cfg(windows)]
use codex_plus_core::watcher::{
    WindowsProcessInfo, find_codex_processes_from_snapshot,
    find_session_index_cleanup_blocking_processes_from_snapshot,
};

#[test]
fn cdp_listening_returns_true_for_bound_loopback_port() {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();

    assert!(cdp_listening(port));
}

#[test]
fn cdp_listening_returns_true_for_bound_ipv6_loopback_port() {
    let listener = std::net::TcpListener::bind("[::1]:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    assert!(cdp_listening(port));
}

#[test]
fn cdp_listening_returns_false_for_closed_port() {
    let port = {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.local_addr().unwrap().port()
    };

    assert!(!cdp_listening(port));
}

#[test]
fn watcher_enable_and_disable_toggle_flag() {
    let dir = tempfile::tempdir().unwrap();
    let flag = watcher_disabled_flag(dir.path());

    disable_watcher_at(dir.path()).unwrap();
    assert!(flag.exists());

    enable_watcher_at(dir.path()).unwrap();
    assert!(!flag.exists());
}

#[test]
fn watcher_install_plan_registers_rust_launcher_at_logon() {
    let plan = build_watcher_install_plan("C:/Tools/codex-plus-plus.exe".into(), 9333);

    assert_eq!(plan.run_value_name, "CodexPlusPlusWatcher");
    assert_eq!(
        plan.run_value,
        "\"C:/Tools/codex-plus-plus.exe\" --debug-port 9333"
    );
    assert_eq!(plan.shortcut_name, "CodexPlusPlusWatcher.lnk");
    assert_eq!(plan.shortcut_target, "C:/Tools/codex-plus-plus.exe");
    assert_eq!(plan.shortcut_arguments, "--debug-port 9333");
}

#[test]
fn spawn_launcher_command_points_to_silent_binary_only() {
    let command = build_spawn_launcher_command("C:/Tools/codex-plus-plus.exe", 9444);

    assert_eq!(command[0], "C:/Tools/codex-plus-plus.exe");
    assert!(command.contains(&"--debug-port".to_string()));
    assert!(command.contains(&"9444".to_string()));
    assert!(!command.iter().any(|part| part.contains("manager")));
}

#[test]
fn codex_process_filter_keeps_only_windowsapps_codex_processes() {
    let processes = [
        (
            11,
            r"C:\Program Files\WindowsApps\OpenAI.Codex_1.0.0.0_x64__abc\app\Codex.exe",
        ),
        (12, r"C:\Tools\Codex.exe"),
        (
            13,
            r"C:\Program Files\WindowsApps\Other.App_1.0.0.0_x64__abc\app\Codex.exe",
        ),
    ];

    assert_eq!(codex_process_ids(processes), vec![11]);
}

#[test]
fn codex_process_filter_keeps_chatgpt_desktop_package_processes() {
    let processes = [
        (
            21,
            r"C:\Program Files\WindowsApps\OpenAI.ChatGPT-Desktop_1.2026.133.0_x64__abc\app\ChatGPT.exe",
        ),
        (
            22,
            r"C:\Program Files\WindowsApps\OpenAI.Codex_26.707.3748.0_x64__abc\app\ChatGPT.exe",
        ),
        (
            23,
            r"C:\Program Files\WindowsApps\OpenAI.ChatGPT-Desktop_1.2026.133.0_x64__abc\app\resources\ChatGPT.exe",
        ),
        (
            24,
            r"C:\Program Files\WindowsApps\Other.ChatGPT_1.0.0.0_x64__abc\app\ChatGPT.exe",
        ),
    ];

    assert_eq!(codex_process_ids(processes), vec![21, 22]);
}

#[test]
fn launcher_process_filter_protects_current_process_ancestry() {
    let processes = [
        (10, 0, "codex-plus-plus.exe"),
        (20, 10, "codex-plus-plus.exe"),
        (30, 20, "codex-plus-plus.exe"),
        (40, 10, "codex-plus-plus.exe"),
        (50, 10, "codex-plus-plus-manager.exe"),
    ];

    assert_eq!(filter_killable_launcher_processes(processes, 30), vec![40]);
}

#[test]
fn stale_launcher_recovery_only_runs_when_codex_and_cdp_are_absent() {
    assert!(should_recover_stale_launcher(false, false));
    assert!(!should_recover_stale_launcher(true, false));
    assert!(!should_recover_stale_launcher(false, true));
    assert!(!should_recover_stale_launcher(true, true));
}

#[test]
fn stop_wait_tracks_only_expected_process_ids() {
    assert_eq!(
        process_ids_still_running(&[10, 20, 30], [5, 20, 40, 30]),
        vec![20, 30]
    );
}

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
#[test]
fn process_liveness_distinguishes_current_and_missing_processes() {
    assert_eq!(process_id_is_running(std::process::id()), Some(true));
    assert_eq!(process_id_is_running(u32::MAX), Some(false));
}

#[cfg(windows)]
#[test]
fn find_codex_processes_finds_local_install_with_capitial_c() {
    let processes = [WindowsProcessInfo {
        process_id: 42,
        parent_process_id: 0,
        exe_file: "Codex.exe".to_string(),
        executable_path: Some(std::path::PathBuf::from(
            r"D:\360Downloads\codexapp\app\Codex.exe",
        )),
    }];

    assert_eq!(find_codex_processes_from_snapshot(&processes), vec![42]);
}

#[cfg(windows)]
#[test]
fn find_codex_processes_ignores_lowercase_local_cli_binary() {
    let processes = [WindowsProcessInfo {
        process_id: 43,
        parent_process_id: 0,
        exe_file: "codex.exe".to_string(),
        executable_path: Some(std::path::PathBuf::from(
            r"D:\360Downloads\codexapp\app\codex.exe",
        )),
    }];

    assert!(find_codex_processes_from_snapshot(&processes).is_empty());
}

#[cfg(windows)]
#[test]
fn find_codex_processes_ignores_npm_cli_binary() {
    let processes = [WindowsProcessInfo {
        process_id: 44,
        parent_process_id: 0,
        exe_file: "codex.exe".to_string(),
        executable_path: Some(std::path::PathBuf::from(
            r"C:\Users\me\AppData\Roaming\npm\node_modules\@openai\codex\node_modules\@openai\codex-win32-x64\vendor\x86_64-pc-windows-msvc\bin\codex.exe",
        )),
    }];

    assert!(find_codex_processes_from_snapshot(&processes).is_empty());
}

#[cfg(windows)]
#[test]
fn find_codex_processes_ignores_packaged_resource_cli_binary() {
    let processes = [WindowsProcessInfo {
        process_id: 45,
        parent_process_id: 0,
        exe_file: "codex.exe".to_string(),
        executable_path: Some(std::path::PathBuf::from(
            r"C:\Program Files\WindowsApps\OpenAI.Codex_1.0.0.0_x64__abc\app\resources\codex.exe",
        )),
    }];

    assert!(find_codex_processes_from_snapshot(&processes).is_empty());
}

#[cfg(windows)]
#[test]
fn find_codex_processes_combines_store_and_local_installs() {
    let processes = [
        WindowsProcessInfo {
            process_id: 11,
            parent_process_id: 0,
            exe_file: "ChatGPT.exe".to_string(),
            executable_path: Some(std::path::PathBuf::from(
                r"C:\Program Files\WindowsApps\OpenAI.ChatGPT-Desktop_1.2026.133.0_x64__abc\app\ChatGPT.exe",
            )),
        },
        WindowsProcessInfo {
            process_id: 42,
            parent_process_id: 0,
            exe_file: "Codex.exe".to_string(),
            executable_path: Some(std::path::PathBuf::from(
                r"D:\360Downloads\codexapp\app\Codex.exe",
            )),
        },
    ];

    assert_eq!(find_codex_processes_from_snapshot(&processes), vec![11, 42]);
}

#[cfg(windows)]
#[test]
fn session_index_cleanup_process_guard_blocks_desktop_apps_but_not_cli() {
    let processes = [
        WindowsProcessInfo {
            process_id: 11,
            parent_process_id: 0,
            exe_file: "ChatGPT.exe".to_string(),
            executable_path: Some(std::path::PathBuf::from(
                r"C:\Program Files\WindowsApps\OpenAI.ChatGPT-Desktop_1.2026.133.0_x64__abc\app\ChatGPT.exe",
            )),
        },
        WindowsProcessInfo {
            process_id: 12,
            parent_process_id: 0,
            exe_file: "ChatGPT.exe".to_string(),
            executable_path: Some(std::path::PathBuf::from(r"D:\Portable\ChatGPT\ChatGPT.exe")),
        },
        WindowsProcessInfo {
            process_id: 13,
            parent_process_id: 0,
            exe_file: "Codex.exe".to_string(),
            executable_path: Some(std::path::PathBuf::from(r"D:\Portable\Codex\Codex.exe")),
        },
        WindowsProcessInfo {
            process_id: 14,
            parent_process_id: 0,
            exe_file: "codex.exe".to_string(),
            executable_path: Some(std::path::PathBuf::from(
                r"C:\Users\me\AppData\Roaming\npm\node_modules\@openai\codex\bin\codex.exe",
            )),
        },
    ];

    assert_eq!(
        find_session_index_cleanup_blocking_processes_from_snapshot(&processes),
        vec![11, 12, 13]
    );
    assert_eq!(find_codex_processes_from_snapshot(&processes), vec![11, 13]);
}

#[cfg(windows)]
#[test]
fn find_codex_processes_ignores_unrelated_processes() {
    let processes = [
        WindowsProcessInfo {
            process_id: 10,
            parent_process_id: 0,
            exe_file: "notepad.exe".to_string(),
            executable_path: Some(std::path::PathBuf::from(r"C:\Windows\notepad.exe")),
        },
        WindowsProcessInfo {
            process_id: 20,
            parent_process_id: 0,
            exe_file: "codex-plus-plus.exe".to_string(),
            executable_path: Some(std::path::PathBuf::from(
                r"D:\Programs\Codex++\codex-plus-plus.exe",
            )),
        },
    ];

    assert!(find_codex_processes_from_snapshot(&processes).is_empty());
}

/// macOS 上启动器可能以两个名字出现在进程表里,`pgrep -x` 精确匹配可执行名,
/// 少查一个就看不见还活着的旧实例 —— 旧实例占着 helper 端口,新实例绑不上就
/// 退化成 helper.port_fallback(线上 4 台设备见过 Address already in use)。
///
/// 两个名字来自两条不同的安装布局:
///   - **出货 DMG**:package-recodex-dmg.sh 把二进制直接命名为 ReCodex,
///     进程名就是 ReCodex(= SILENT_NAME)。用户装的都是这条。
///   - **应用内安装器**:install/macos.rs 写的是启动脚本,exec 后进程名
///     变回 codex-plus-plus(= SILENT_BINARY)。
#[cfg(target_os = "macos")]
#[test]
fn macos_launcher_process_names_cover_both_install_layouts() {
    let names = codex_plus_core::watcher::macos_launcher_process_names();
    assert!(
        names.contains(&codex_plus_core::install::SILENT_BINARY),
        "少了应用内安装器那条布局(exec 之后的进程名)"
    );
    assert!(
        names.contains(&codex_plus_core::install::MACOS_SILENT_EXECUTABLE),
        "少了出货 DMG 那条布局 —— 用户点图标起的进程就是这个"
    );
}

/// 出货包的进程名由**打包脚本**决定,不是由 install/macos.rs 决定。
///
/// 第一版守卫钉的是 install/macos.rs 里那个硬编码字符串,而那条布局写的是启动脚本、
/// exec 之后进程名根本不是它 —— 守卫全绿,常量却是错的,等于什么都没守住。
/// 钉在这里:改 package-recodex-dmg.sh 的可执行名而不同步常量,这条会红。
#[test]
fn macos_shipping_bundle_executable_name_matches_constant() {
    let script = include_str!("../../../scripts/installer/macos/package-recodex-dmg.sh");
    let expected = format!(
        "Contents/MacOS/{}",
        codex_plus_core::install::MACOS_SILENT_EXECUTABLE
    );
    assert!(
        script.contains(&expected),
        "package-recodex-dmg.sh 写的可执行名与 MACOS_SILENT_EXECUTABLE 不一致 ——          pgrep 会找不到用户实际跑着的进程"
    );
}
