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

/// Windows 上停 Codex 必须**先请求它自己关**,只对赖着不走的才 TerminateProcess。
///
/// 为什么用文本守卫:这条路整段在 `#[cfg(windows)]` 里,而且真去跑它就得真杀一个
/// 进程 —— 单测里没法验。而改错的代价是静默的:把 request_process_close 那一段
/// 删掉,编译照过、所有测试照绿,只是从此每个 Windows 用户被硬杀,
/// 没有 beforeunload、没有保存。
///
/// 这段逻辑从前写在 MSIX 分支的 return 之后,所有 Windows 用户都执行不到
/// (线上诊断 0 次 0 设备),所以粗暴也没人碰到。挪到分支之前之后它对每个人生效了。
#[test]
fn windows_stop_asks_before_it_kills() {
    let source = include_str!("../src/watcher.rs");
    let body = source
        .split_once("fn terminate_and_wait_for_exit")
        .expect("找不到 terminate_and_wait_for_exit —— 改名了就更新这条守卫")
        .1;

    let ask = body
        .find("request_process_close")
        .expect("停进程时没有先请求优雅关闭 —— 每个 Windows 用户都会被硬杀");
    let kill = body
        .find("terminate_process")
        .expect("找不到硬杀兜底 —— 赖着不走的进程会让重启永远等下去");
    assert!(
        ask < kill,
        "TerminateProcess 排在 WM_CLOSE 之前 —— 那等于没有优雅关闭"
    );

    // 光有「先发 WM_CLOSE 再 TerminateProcess」的顺序还不够 —— 中间**必须等**。
    // PostMessageW 是异步投递,不等的话目标的消息循环还没取到这条消息,
    // 硬杀就落下了,功能上等同于原来的直接瞬杀,而这条守卫却在说"先问了再杀"。
    // 实测过:把等待整段删掉,只留 request_process_close,上面那两条断言全绿。
    let wait = body
        .find("wait_for_process_exit")
        .expect("发完 WM_CLOSE 之后没有等待 —— 等于没有优雅关闭");
    assert!(
        ask < wait && wait < kill,
        "顺序必须是「请求关闭 → 等 → 硬杀」,实际 ask={ask} wait={wait} kill={kill}"
    );

    // 兜底必须还在:只发 WM_CLOSE 不硬杀的话,一个卡住的 Codex 能把重启堵死,
    // 而调用方接着就要去激活新实例,端口还被占着。
    assert!(
        body.contains("GRACEFUL_CLOSE_WAIT_MS"),
        "优雅关闭没有超时上限 —— 卡住的进程会把启动流程堵死"
    );
}

/// WM_CLOSE 必须用 Post 不能用 Send。
///
/// SendMessageW 要等对方消息循环处理完才返回:目标弹一个「确定要退出吗」,
/// 我们就跟着一起卡死,而且是卡在启动路径上 —— 用户点了启动之后界面一直不动。
#[test]
fn windows_close_request_does_not_block_on_the_target() {
    let source = include_str!("../src/windows_integration.rs");
    let body = source
        .split_once("pub fn request_process_close")
        .expect("找不到 request_process_close")
        .1;
    let body = &body[..body.find("\npub fn ").unwrap_or(body.len())];
    assert!(
        body.contains("PostMessageW"),
        "WM_CLOSE 要用 PostMessageW(只投递不等)"
    );
    assert!(
        !body.contains("SendMessageW"),
        "用了 SendMessageW —— 目标弹对话框时我们会跟着卡死在启动路径上"
    );
}

/// 改名之后必须**同时**认新旧两个二进制名。
///
/// 自更新是「用新内容盖掉自己那个 exe」(selfupdate.rs 的 stage_replacement 走
/// `current_exe()`),**文件名不跟着变**。所以老安装升级到新版之后,磁盘上那个
/// exe 仍然叫 codex-plus-plus.exe,里面跑的却是新代码。只认新名字的话:
/// 清理不掉残留实例 → 新实例以为端口没人用 → 去绑已被占住的 helper 端口 →
/// 退化成 helper.port_fallback。**全程没有任何报错**,只有增强功能悄悄不工作。
///
/// 这和上一轮审计翻出来的 macOS 进程名是同一个坑:名字对不上,代码就永远
/// 在找一个不存在的东西,而且不会失败。
#[test]
fn launcher_process_matching_still_accepts_the_pre_rename_name() {
    use codex_plus_core::install::{LEGACY_SILENT_BINARY, SILENT_BINARY};

    assert_eq!(SILENT_BINARY, "recodex", "出货二进制名变了就更新这条守卫");
    assert_eq!(
        LEGACY_SILENT_BINARY, "codex-plus-plus",
        "旧名字不能改 —— 它是升级上来的老安装在磁盘上的真实文件名"
    );
    assert_ne!(
        SILENT_BINARY, LEGACY_SILENT_BINARY,
        "两个常量相等说明改名没生效,或者有人把旧名字覆盖掉了"
    );

    // Windows:两个名字的进程都要被认出来,自己那条链不能杀。
    let processes = vec![
        (100u32, 0u32, "recodex.exe"),
        (101, 0, "codex-plus-plus.exe"),
        (102, 0, "ReCodex.exe"),
        (103, 0, "chrome.exe"),
    ];
    let mut killable = codex_plus_core::watcher::filter_killable_launcher_processes(processes, 999);
    killable.sort_unstable();
    assert_eq!(
        killable,
        vec![100, 101, 102],
        "新名、旧名、以及大小写不同的写法都该认出来,别的进程不能碰"
    );
}
