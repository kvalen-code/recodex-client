use codex_plus_core::install::{
    InstallOptions, MANAGER_BUNDLE_ID, SILENT_BINARY, SILENT_BUNDLE_ID, app_bundle_names,
    build_macos_app_bundle, build_windows_entrypoint_plan, companion_binary_path_from_exe,
    default_install_root_strategy, macos_companion_bundle_identifier_from_exe, shortcut_names,
};

#[test]
fn windows_entrypoint_plan_contains_silent_and_manager_entrypoints() {
    let options = InstallOptions {
        install_root: Some("C:/Users/A/Desktop".into()),
        launcher_path: Some("C:/Tools/recodex.exe".into()),
        manager_path: Some("C:/Tools/recodex-manager.exe".into()),
        remove_owned_data: false,
    };

    let plan = build_windows_entrypoint_plan(&options);

    assert!(plan.silent_shortcut.ends_with("ReCodex.lnk"));
    assert!(plan.manager_shortcut.ends_with("ReCodex.lnk"));
    assert_eq!(plan.launcher_path, "C:/Tools/recodex.exe");
    assert_eq!(plan.manager_path, "C:/Tools/recodex-manager.exe");
    assert_eq!(plan.silent_icon_path, "C:/Tools/recodex.exe");
    assert_eq!(
        plan.manager_icon_path,
        "C:/Tools/recodex-manager.exe"
    );
    assert_eq!(plan.uninstall_key, "ReCodex");
    // legacy 键刻意**不**跟着去品牌:它的作用是清理**上游 Codex++** 装出来的
    // 注册表项,改成 ReCodex 就等于再也清不掉那份残留。
    assert_eq!(plan.legacy_uninstall_key, "CodexPlusPlus");
    assert_eq!(
        plan.uninstaller_path.replace('\\', "/"),
        "C:/Tools/uninstall.exe"
    );
    assert_eq!(
        plan.uninstall_command.replace('\\', "/"),
        "\"C:/Tools/uninstall.exe\""
    );
    assert_eq!(
        plan.quiet_uninstall_command.replace('\\', "/"),
        "\"C:/Tools/uninstall.exe\" /S"
    );
    assert_ne!(
        plan.uninstall_command,
        "\"C:/Tools/recodex-manager.exe\""
    );
}

#[test]
fn windows_entrypoint_plan_can_request_owned_data_removal_without_shell_script() {
    let options = InstallOptions {
        install_root: Some("C:/Users/A/Desktop".into()),
        launcher_path: None,
        manager_path: None,
        remove_owned_data: true,
    };

    let plan = build_windows_entrypoint_plan(&options);

    assert!(plan.silent_shortcut.ends_with("ReCodex.lnk"));
    assert!(plan.manager_shortcut.ends_with("ReCodex.lnk"));
    assert!(plan.remove_owned_data);
}

#[test]
fn macos_bundle_metadata_contains_silent_and_manager_apps() {
    let options = InstallOptions {
        install_root: Some("/Applications".into()),
        launcher_path: Some("/opt/ReCodex/recodex".into()),
        manager_path: Some("/opt/ReCodex/recodex-manager".into()),
        remove_owned_data: false,
    };

    let silent = build_macos_app_bundle(&options, false);
    let manager = build_macos_app_bundle(&options, true);

    assert!(silent.app_path.ends_with("ReCodex.app"));
    assert!(manager.app_path.ends_with("ReCodex.app"));
    assert!(silent.info_plist.contains("<string>ReCodex</string>"));
    assert!(
        manager
            .info_plist
            .contains("<string>ReCodex</string>")
    );
    assert!(manager.info_plist.contains("<string>dreamskin</string>"));
    assert!(manager.info_plist.contains("<string>codexplusplus</string>"));
    assert!(!silent.info_plist.contains("<string>dreamskin</string>"));
    assert_eq!(
        silent.binary_target_name.as_deref(),
        Some("recodex")
    );
    assert_eq!(
        manager.binary_target_name.as_deref(),
        Some("recodex-manager")
    );
    assert!(silent.launch_script.contains("$DIR/recodex"));
    assert!(
        manager
            .launch_script
            .contains("$DIR/recodex-manager")
    );
}

#[test]
fn installer_exports_expected_two_entrypoint_names() {
    assert_eq!(shortcut_names(), ("ReCodex.lnk", "ReCodex.lnk"));
    assert_eq!(app_bundle_names(), ("ReCodex.app", "ReCodex.app"));
}

#[test]
fn macos_dmg_includes_applications_shortcut_for_drag_install() {
    let script = std::fs::read_to_string("../../scripts/installer/macos/package-dmg.sh")
        .expect("read macOS DMG packaging script");

    assert!(script.contains("ln -s /Applications \"$STAGE/Applications\""));
}

#[test]
fn companion_binary_path_resolves_macos_silent_app_next_to_manager_app() {
    // 原来直接写死 /Applications/…,而实现是靠 `.exists()` 挑文件的 —— 于是只有
    // 「真装了 ReCodex 的 mac」才跑得过,别的机器一律红,等于常年没有这条守卫。
    // 自己把 bundle 造出来,任何平台都能真正验到这段路径解析。
    let macos_dir = macos_bundle_fixture(&["ReCodex", "ReCodexManager"]);
    let manager_exe = macos_dir.path().join("ReCodexManager");

    let companion = companion_binary_path_from_exe(&manager_exe, SILENT_BINARY);

    // 只比文件名、且不分大小写:改名之后 SILENT_BINARY("recodex")与
    // MACOS_SILENT_EXECUTABLE("ReCodex")只差大小写,而 Windows 与 macOS 默认卷
    // 都**不区分大小写** —— 实现里那句 `.exists()` 探测因此分不出两者。
    // 功能上无所谓(指向同一个文件),但断言不能再钉死某一种拼写。
    let name = companion
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert_eq!(name, "recodex", "同伴应解析到 silent 那个可执行文件,实际 {companion:?}");
    assert_eq!(companion.parent(), Some(macos_dir.path()));
    // 原来这条钉的是「不能拿一个不存在的名字顶包」。改名后新旧只差大小写,
    // 只比文件名的话,在**区分大小写**的卷上返回一个根本不存在的路径也会绿 ——
    // 所以直接要求它指向一个真实存在的文件。
    assert!(companion.exists(), "同伴路径指向了一个不存在的文件:{companion:?}");
}

/// 造一个 `<tmp>/ReCodex.app/Contents/MacOS/` 并放进指定的可执行文件,
/// 返回那个 MacOS 目录(持有 TempDir,出作用域才删)。
fn macos_bundle_fixture(binaries: &[&str]) -> MacosBundleFixture {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let macos_dir = temp
        .path()
        .join("ReCodex.app")
        .join("Contents")
        .join("MacOS");
    std::fs::create_dir_all(&macos_dir).expect("bundle dirs should be created");
    for binary in binaries {
        std::fs::write(macos_dir.join(binary), "").expect("bundle binary should be written");
    }
    MacosBundleFixture {
        _temp: temp,
        macos_dir,
    }
}

struct MacosBundleFixture {
    _temp: tempfile::TempDir,
    macos_dir: std::path::PathBuf,
}

impl MacosBundleFixture {
    fn path(&self) -> &std::path::Path {
        &self.macos_dir
    }
}

#[test]
fn companion_binary_path_resolves_macos_manager_app_next_to_silent_app() {
    let macos_dir = macos_bundle_fixture(&["ReCodex", "ReCodexManager"]);
    let silent_exe = macos_dir.path().join("ReCodex");

    let companion =
        companion_binary_path_from_exe(&silent_exe, codex_plus_core::install::MANAGER_BINARY);

    assert_eq!(companion, macos_dir.path().join("ReCodexManager"));
}

#[test]
fn macos_companion_launch_uses_bundle_ids_from_app_translocation() {
    let manager_exe = std::path::Path::new(
        "/private/var/folders/x/AppTranslocation/manager-id/d/ReCodex.app/Contents/MacOS/ReCodexManager",
    );
    let silent_exe = std::path::Path::new(
        "/private/var/folders/x/AppTranslocation/silent-id/d/ReCodex.app/Contents/MacOS/ReCodex",
    );

    assert_eq!(
        macos_companion_bundle_identifier_from_exe(manager_exe, SILENT_BINARY),
        Some(SILENT_BUNDLE_ID)
    );
    assert_eq!(
        macos_companion_bundle_identifier_from_exe(
            silent_exe,
            codex_plus_core::install::MANAGER_BINARY,
        ),
        Some(MANAGER_BUNDLE_ID)
    );
}

#[test]
fn macos_companion_launch_keeps_bare_binary_development_mode() {
    let manager_exe = std::path::Path::new("/tmp/target/debug/recodex-manager");

    assert_eq!(
        macos_companion_bundle_identifier_from_exe(manager_exe, SILENT_BINARY),
        None
    );
}

#[test]
fn macos_bundle_does_not_wrap_the_bundle_executable_in_itself() {
    let options = InstallOptions {
        install_root: Some("/Applications".into()),
        launcher_path: Some("/Applications/ReCodex.app/Contents/MacOS/ReCodex".into()),
        manager_path: Some(
            "/Applications/ReCodex.app/Contents/MacOS/ReCodexManager".into(),
        ),
        remove_owned_data: false,
    };

    let silent = build_macos_app_bundle(&options, false);
    let manager = build_macos_app_bundle(&options, true);

    assert_eq!(
        silent.binary_source,
        Some(std::path::PathBuf::from(
            "/Applications/ReCodex.app/Contents/MacOS/ReCodex"
        ))
    );
    assert_eq!(
        manager.binary_source,
        Some(std::path::PathBuf::from(
            "/Applications/ReCodex.app/Contents/MacOS/ReCodexManager"
        ))
    );
    assert!(silent.launch_script.contains("$DIR/recodex"));
    assert!(
        manager
            .launch_script
            .contains("$DIR/recodex-manager")
    );
}

#[test]
fn windows_default_install_root_uses_known_folder_before_userprofile_desktop() {
    let strategy = default_install_root_strategy();

    if cfg!(windows) {
        assert_eq!(strategy, "windows-known-folder");
    } else if cfg!(target_os = "macos") {
        assert_eq!(strategy, "macos-applications");
    } else {
        assert_eq!(strategy, "user-dirs-desktop");
    }
}
