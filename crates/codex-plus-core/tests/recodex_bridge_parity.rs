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

/// 面板不能把「更新」按钮显示给已经在最新版的人。
///
/// 服务端的 `AvailableFor` **不比较版本号**(只看两个设置项非空 + 灰度名单),
/// 所以它对所有人都回 available。1.3.4 起客户端会拒绝装不比自己新的包,
/// 于是那一点就变成一个红色的「已经是最新版本」—— 看着像更新失败,会来工单。
///
/// 判断只能放在面板里(服务端那边改不动),所以在这儿钉住它还在。
#[test]
fn panel_hides_the_update_button_when_it_is_not_actually_newer() {
    let panel = include_str!("../../../assets/inject/recodex-panel-inject.js");
    assert!(
        panel.contains("function isNewerVersion("),
        "面板少了版本比较 —— 已在最新版的用户会被推更新,点下去得到一个红色报错"
    );
    assert!(
        panel.contains("channel.available && isNewerVersion(channel.latest_version, current)"),
        "isNewerVersion 定义了却没接到 hasUpdate 上 —— 等于没写"
    );
    // 逐段按数字比,不能按字符串:"1.3.10" 的字符串序小于 "1.3.9"。
    assert!(
        panel.contains("a[i] > b[i]"),
        "版本比较退化成字符串比较了 —— 1.3.10 会被当成比 1.3.9 旧"
    );
    // 认不出的格式要放行,否则将来换版本号格式会把更新入口静默关掉。
    assert!(
        panel.contains("if (!a || !b) return true;"),
        "解析失败时没有放行 —— 换个版本号格式就会把更新入口关死"
    );
}
