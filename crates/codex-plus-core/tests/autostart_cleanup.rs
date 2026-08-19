//! 卸载时清理开机自启的**正向**验证。
//!
//! 需要外部先在 HKCU Run 里写好一条指向 `RECODEX_TEST_EXE` 的项;
//! 没设这个变量就跳过 —— 免得在别人机器上跑测试时改注册表。

#[cfg(windows)]
#[test]
fn cleans_autostart_that_points_at_the_given_exe() {
    let Ok(exe) = std::env::var("RECODEX_TEST_EXE") else {
        eprintln!("未设置 RECODEX_TEST_EXE,跳过");
        return;
    };
    let exe = std::path::PathBuf::from(exe);
    assert!(
        codex_plus_core::watcher::uninstall_watcher_pointing_at(&exe),
        "指向本 exe 的自启项应被清掉"
    );
    assert!(
        !codex_plus_core::watcher::uninstall_watcher_pointing_at(&exe),
        "清完之后再调一次应返回 false(没有可清的了)"
    );
}
