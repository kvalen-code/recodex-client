//! 面板调的每一个 `/recodex/*` 桥接口，都必须在 `handle_bridge` 里有对应分支。
//!
//! 这两份清单过去只靠人对账 —— 而对账漏掉的后果是**静默的**：桥返回
//! `unknown recodex path`，面板那一块要么空着要么显示一句泛泛的错误，
//! 没有任何东西指向「路由名写错了」。2026-08-26 的 image_gen 就是同一形状的问题
//! （客户端会打的路径 vs 网关注册的路径），所以这里直接钉死。
//!
//! 只管 `/recodex/*`：其余前缀（/settings、/weixin、/open-external、/self-update…）
//! 由 launcher 自己分发，不在这个 crate 里。

use std::collections::BTreeSet;

fn panel_source() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../assets/inject/recodex-panel-inject.js");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("读 {}: {e}", path.display()))
}

fn dispatcher_source() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../recodex-integration/src/desktop.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("读 {}: {e}", path.display()))
}

/// 抓出 `bridge("/recodex/...")` 里的路径。
fn panel_recodex_paths(source: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (idx, _) in source.match_indices("bridge(\"/recodex/") {
        let rest = &source[idx + "bridge(\"".len()..];
        if let Some(end) = rest.find('"') {
            out.insert(rest[..end].to_string());
        }
    }
    out
}

/// 抓出 `handle_bridge` 的 match 分支里写着的路径。
fn dispatcher_recodex_paths(source: &str) -> BTreeSet<String> {
    let body = source
        .split("pub fn handle_bridge")
        .nth(1)
        .expect("handle_bridge 应存在");
    let mut out = BTreeSet::new();
    for (idx, _) in body.match_indices("\"/recodex/") {
        let rest = &body[idx + 1..];
        if let Some(end) = rest.find('"') {
            out.insert(rest[..end].to_string());
        }
    }
    out
}

#[test]
fn every_panel_bridge_call_has_a_dispatcher_arm() {
    let panel = panel_recodex_paths(&panel_source());
    let dispatcher = dispatcher_recodex_paths(&dispatcher_source());

    assert!(
        !panel.is_empty() && !dispatcher.is_empty(),
        "两边都该抓到路径 —— 抓不到说明调用写法变了，守卫已失效。panel={panel:?} dispatcher={dispatcher:?}"
    );

    let missing: Vec<_> = panel.difference(&dispatcher).collect();
    assert!(
        missing.is_empty(),
        "面板在调这些桥接口，但 handle_bridge 没有对应分支 —— \
         运行时只会回 `unknown recodex path`，面板那一块静默失效：{missing:?}"
    );
}

/// 自诊断必须真的接上：这是纯桌面端用户唯一的自救入口
/// （安装包只装 codex-plus-plus.exe，不带 recodex.exe，`recodex doctor` 跑不了）。
#[test]
fn the_panel_exposes_doctor_and_its_fix() {
    let panel = panel_source();
    let dispatcher = dispatcher_source();
    for path in ["/recodex/doctor", "/recodex/doctor/fix"] {
        assert!(panel.contains(&format!("bridge(\"{path}\"")), "面板应调用 {path}");
        assert!(dispatcher.contains(&format!("\"{path}\"")), "handle_bridge 应分发 {path}");
    }
    // 修复动作只能走登录那一个写入口，绕开会漂移掉官方模式快照与顶层 provider 接管。
    assert!(
        dispatcher.contains("fn recodex_doctor_fix")
            && dispatcher
                .split("pub fn recodex_doctor_fix")
                .nth(1)
                .is_some_and(|body| body.contains("install_login_config(")),
        "doctor 的修复必须复用 install_login_config"
    );
}
