//! 「原地复制会话」与「回答大纲」的行为测试(1.3.8 审计 R6 / S5)。
//!
//! 与 cdp_bridge.rs 里的插件市场用例同一种办法:把**完整**注入脚本交给 node,
//! 用最小的假 DOM 跑起来,再调脚本在测试开关下暴露的钩子。单独成一个测试目标,
//! 不和 cdp_bridge 挤在一起(那个目标全量跑会被别的 node harness 挂住)。
//! harness 结尾必须显式 process.exit:脚本会注册常驻定时器,不退 node 就不结束。

use codex_plus_core::assets;
use serde_json::Value;
use std::process::Command;

const HARNESS: &str = r#"
const scriptPath = process.argv[2];
const store = new Map();
globalThis.window = globalThis;
class FakeElement {
  constructor(props = {}) {
    Object.assign(this, { isConnected: true, children: [], dataset: {}, style: {}, textContent: "" }, props);
    this.classList = { add() {}, remove() {}, toggle() {}, contains() { return false; } };
  }
  getBoundingClientRect() { return this.rect || { width: 100, height: 20, top: 0, right: 100 }; }
  closest(selector) { return this.closestMap?.[selector] ?? null; }
  getAttribute(name) { return this.attrs?.[name] ?? null; }
  setAttribute() {} removeAttribute() {} addEventListener() {} appendChild() {} append() {}
  querySelector() { return null; }
  querySelectorAll() { return []; }
  remove() { this.removed = (this.removed || 0) + 1; this.isConnected = false; }
  contains() { return true; }
}
globalThis.Element = FakeElement;
globalThis.HTMLElement = FakeElement;
globalThis.Node = { DOCUMENT_POSITION_FOLLOWING: 4 };
const node = () => new FakeElement();

// 上一份注入脚本留下的大纲节点(S5)。
const staleOutline = new FakeElement({ id: "codex-answer-outline" });
window.__CODEX_PLUS_TEST_ANSWER_OUTLINE__ = true;
window.__CODEX_PLUS_TEST_SESSION_COPY__ = true;
window.addEventListener = () => {};
window.removeEventListener = () => {};
window.dispatchEvent = () => true;
globalThis.document = {
  scripts: [], documentElement: node(), body: node(), createElement: () => node(),
  getElementById: (id) => (id === "codex-answer-outline" && staleOutline.isConnected ? staleOutline : null),
  querySelector: () => null, querySelectorAll: () => [],
  addEventListener() {}, removeEventListener() {},
};
globalThis.localStorage = {
  getItem: (key) => store.has(key) ? store.get(key) : null,
  setItem: (key, value) => store.set(key, String(value)), removeItem: (key) => store.delete(key),
};
globalThis.sessionStorage = globalThis.localStorage;
globalThis.location = { href: "https://codex.test/index.html", pathname: "/index.html", search: "", hash: "" };
window.location = globalThis.location;
globalThis.navigator = { userAgent: "node-test", sendBeacon: () => false };
globalThis.performance = { getEntriesByType: () => [] };
globalThis.fetch = async () => ({ ok: true, json: async () => ({}) });
globalThis.MutationObserver = class { observe() {} disconnect() {} takeRecords() { return []; } };
globalThis.requestAnimationFrame = (callback) => setTimeout(callback, 0);
globalThis.cancelAnimationFrame = (id) => clearTimeout(id);
globalThis.getComputedStyle = () => ({ overflowY: "visible", getPropertyValue: () => "" });
window.matchMedia = () => ({ matches: false, addEventListener() {}, removeEventListener() {} });
require(scriptPath);

const copy = window.__codexPlusSessionCopyTest;
const outline = window.__codexPlusAnswerOutlineTest;
const T = "019a1b2c-3d4e-7f00-8a9b-0c1d2e3f4a5b";
const OTHER = "019a1b2c-3d4e-7f00-8a9b-0c1d2e3f4a5c";
const oldA = new FakeElement({ name: "oldA" });
const oldB = new FakeElement({ name: "oldB" });
const fresh = new FakeElement({ name: "fresh" });
const name = (button) => button ? button.name : null;
const pick = (args) => name(copy.pickForkButton(args));

const cases = {
  // 路由显示的还是别的会话:不管有什么按钮,都不点。
  routeOnOtherThread: pick({ targetId: `local:${T}`, wasSelected: false, before: new Set([oldA]), buttons: [oldA, fresh], currentThreadId: OTHER }),
  // 路由已是目标,但新会话的按钮还没挂上、只剩旧按钮:不点旧的。
  routeOnTargetOnlyOldButtons: pick({ targetId: T, wasSelected: false, before: new Set([oldA, oldB]), buttons: [oldA, oldB], currentThreadId: `local:${T}` }),
  // 路由是目标、新按钮出现了:点新的(即便旧按钮还挂着、排在后面)。
  routeOnTargetNewButton: pick({ targetId: T, wasSelected: false, before: new Set([oldA]), buttons: [fresh, oldA], currentThreadId: T }),
  // 路由读不到 id、切换中旧按钮还在:无法确认,不点。
  noRouteOldStillConnected: pick({ targetId: T, wasSelected: false, before: new Set([oldB]), buttons: [oldB, fresh], currentThreadId: "" }),
  // 路由读不到 id、旧按钮全部下线、新的出现:点新的。
  noRouteAfterSwap: (() => { oldB.isConnected = false; return pick({ targetId: T, wasSelected: false, before: new Set([oldB]), buttons: [fresh], currentThreadId: "" }); })(),
  // 路由里是个非 UUID 片段(设置页之类):当作读不到。
  nonUuidRoute: pick({ targetId: T, wasSelected: true, before: new Set([oldA]), buttons: [oldA], currentThreadId: "settings" }),
  // 本来就在这个会话:没有切换,当前最后一个按钮就是它的。
  alreadySelected: pick({ targetId: T, wasSelected: true, before: new Set([oldA]), buttons: [oldA], currentThreadId: T }),
  // 没有目标 id:不点。
  noTarget: pick({ targetId: "", wasSelected: true, before: new Set(), buttons: [fresh], currentThreadId: "" }),
  // 一个按钮都没有。
  noButtons: pick({ targetId: T, wasSelected: true, before: new Set(), buttons: [], currentThreadId: T }),
};

// ---- 回答大纲 ----
const heading = (tag, text, extra = {}) => new FakeElement({ tagName: tag, textContent: text, ...extra });
const inCode = heading("H2", "代码里的标题", { closestMap: { "pre, code, table, thead, tbody, [role='table'], [role='grid'], blockquote, .cm-editor, .monaco-editor, .sr-only": node() } });
const hidden = heading("H2", "看不见的标题", { rect: { width: 0, height: 0 } });
const root = new FakeElement({
  querySelectorAll: (selector) => selector.startsWith("h1") ? [
    heading("H2", "  背景  "), heading("H3", "细节 A"), inCode, hidden, heading("H2", "背景"), heading("H2", "方案"),
  ] : [],
});
const items = outline.collect(root);
const outlineCases = {
  staleOutlineRemovedOnReinject: staleOutline.removed === 1,
  items: items.map((item) => ({ text: item.text, depth: item.depth })),
  headingYes: ["1. 背景", "总结", "方案：", "Next steps"].map(outline.looksLikeHeading),
  headingNo: ["这是一句完整的话。", "x", "普通的加粗强调但不像标题"].map(outline.looksLikeHeading),
  stateClosed: outline.state().open === false,
};

process.stdout.write(JSON.stringify({ cases, outlineCases }), () => process.exit(0));
"#;

fn run_harness() -> Value {
    let temp = tempfile::tempdir().expect("temp dir");
    let script_path = temp.path().join("renderer-inject.js");
    let harness_path = temp.path().join("session-copy-outline-harness.cjs");
    std::fs::write(&script_path, assets::injection_script(57321)).expect("write script");
    std::fs::write(&harness_path, HARNESS).expect("write harness");
    let output = Command::new("node")
        .arg(&harness_path)
        .arg(&script_path)
        .output()
        .expect("node should run");
    assert!(
        output.status.success(),
        "harness failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("harness stdout should be JSON")
}

/// R6:只有确认对话区已经是目标会话,才点它**自己的**分叉按钮;确认不了就不点。
#[test]
fn session_copy_only_forks_the_confirmed_target_conversation() {
    let result = run_harness();
    let cases = &result["cases"];
    assert_eq!(cases["routeOnOtherThread"], Value::Null);
    assert_eq!(cases["routeOnTargetOnlyOldButtons"], Value::Null);
    assert_eq!(cases["routeOnTargetNewButton"], "fresh");
    assert_eq!(cases["noRouteOldStillConnected"], Value::Null);
    assert_eq!(cases["noRouteAfterSwap"], "fresh");
    assert_eq!(cases["nonUuidRoute"], "oldA");
    assert_eq!(cases["alreadySelected"], "oldA");
    assert_eq!(cases["noTarget"], Value::Null);
    assert_eq!(cases["noButtons"], Value::Null);
}

/// S5 + 大纲基本行为:重注入拆掉旧节点;标题收集排除代码块/不可见/重复,按层级缩进。
#[test]
fn answer_outline_reinjection_and_heading_collection() {
    let result = run_harness();
    let outline = &result["outlineCases"];
    assert_eq!(
        outline["staleOutlineRemovedOnReinject"], true,
        "重注入必须拆掉上一份脚本留下的大纲节点"
    );
    assert_eq!(outline["stateClosed"], true);
    assert_eq!(
        outline["items"],
        serde_json::json!([
            { "text": "背景", "depth": 0 },
            { "text": "细节 A", "depth": 1 },
            { "text": "方案", "depth": 0 },
        ])
    );
    assert_eq!(outline["headingYes"], serde_json::json!([true, true, true, true]));
    assert_eq!(outline["headingNo"], serde_json::json!([false, false, false]));
}
