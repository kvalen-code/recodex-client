//! 注入脚本的行为测试:原地复制(R6)、回答大纲(S5)、删除结果 → 界面(第二轮 应修 1)。
//!
//! 与 cdp_bridge.rs 里的插件市场用例同一种办法:把**完整**注入脚本交给 node,
//! 用最小的假 DOM 跑起来,再调脚本在测试开关下暴露的钩子。单独成一个测试目标,
//! 不和 cdp_bridge 挤在一起(那个目标全量跑会被别的 node harness 挂住)。
//! harness 结尾必须显式 process.exit:脚本会注册常驻定时器,不退 node 就不结束。

use codex_plus_core::{assets, bridge};
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
  setAttribute() {} removeAttribute() {} addEventListener() {} append() {}
  appendChild(child) { this.children.push(child); return child; }
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
window.__CODEX_PLUS_TEST_DELETE_RESULT__ = true;
const toasts = [];
window.addEventListener = () => {};
window.removeEventListener = () => {};
window.dispatchEvent = () => true;
const fakeBody = new FakeElement();
fakeBody.appendChild = (child) => { toasts.push(child); return child; };
globalThis.document = {
  scripts: [], documentElement: node(), body: fakeBody, createElement: () => node(),
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
// ---- 观察范围 / 失败日志上限(第三轮 9) ----
const scrollContainer = new FakeElement({ name: "scroll-1" });
const observed = [];
globalThis.MutationObserver = class {
  constructor(callback) { this.callback = callback; }
  observe(target, options) { observed.push({ target, options }); this.target = target; }
  disconnect() { observed.push({ target: null, disconnected: true }); }
  takeRecords() { return []; }
};
document.querySelector = (selector) => (selector === ".thread-scroll-container" ? scrollContainer : null);
outline.disconnect();
observed.length = 0;
outline.observe();
const firstTarget = observed.at(-1)?.target;
outline.observe();
const observeCallsAfterSameTarget = observed.filter((entry) => entry.target).length;
const swapped = new FakeElement({ name: "scroll-2" });
document.querySelector = (selector) => (selector === ".thread-scroll-container" ? swapped : null);
outline.observe();
const secondTarget = observed.at(-1)?.target;
// 失败日志上限
window.__codexSessionDeleteScanFailures = [];
for (let i = 0; i < 80; i += 1) {
  outline.runScanStep(() => { throw new Error(`boom-${i}`); });
}
const failures = window.__codexSessionDeleteScanFailures;

const outlineCases = {
  observedScrollContainer: firstTarget === scrollContainer,
  observeIsIdempotentForTheSameTarget: observeCallsAfterSameTarget === 1,
  reobservesWhenTheContainerIsSwapped: secondTarget === swapped,
  scanFailureCap: failures.length,
  scanFailureKeepsLatest: failures[failures.length - 1].includes("boom-79"),
  staleOutlineRemovedOnReinject: staleOutline.removed === 1,
  items: items.map((item) => ({ text: item.text, depth: item.depth })),
  headingYes: ["1. 背景", "总结", "方案：", "Next steps"].map(outline.looksLikeHeading),
  headingNo: ["这是一句完整的话。", "x", "普通的加粗强调但不像标题"].map(outline.looksLikeHeading),
  stateClosed: outline.state().open === false,
};

// ---- 删除结果 → 界面(应修 1) ----
const deleteApi = window.__codexPlusDeleteResultTest;
function deleteCase(result) {
  toasts.length = 0;
  let removed = 0;
  const kind = deleteApi.handle(result, () => { removed += 1; });
  const toast = toasts[toasts.length - 1];
  return {
    kind,
    removed,
    text: toast ? toast.textContent : null,
    hasUndoButton: !!toast && toast.children.some((child) => child.textContent === "撤销"),
  };
}
const deleteCases = {
  localDeleted: deleteCase({ status: "local_deleted", message: "已从本地存储删除", undo_token: "t1" }),
  partial: deleteCase({ status: "partial", message: "本地数据库已删除，但文件删除失败：x", undo_token: "t2" }),
  partialKeepsWording: deleteCase({ status: "partial", message: "部分失败：rollout 被占用", undo_token: "t3" }),
  unknown: deleteCase({ status: "unknown", message: "桥接已重新连接,这次操作的结果未知,请刷新列表确认" }),
  failedWithToken: deleteCase({ status: "failed", message: "事务回滚", undo_token: "t4" }),
  failedPlain: deleteCase({ status: "failed", message: "删不掉" }),
};

process.stdout.write(JSON.stringify({ cases, outlineCases, deleteCases }), () => process.exit(0));
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

/// 第三轮 9:大纲观察者只盯对话滚动区(不是整个 body),容器换了要跟着换;
/// scan 失败日志有上限,别无限涨。
#[test]
fn answer_outline_observes_only_the_conversation_and_scan_failures_are_capped() {
    let result = run_harness();
    let outline = &result["outlineCases"];
    assert_eq!(outline["observedScrollContainer"], true);
    assert_eq!(outline["observeIsIdempotentForTheSameTarget"], true);
    assert_eq!(outline["reobservesWhenTheContainerIsSwapped"], true);
    assert_eq!(outline["scanFailureCap"], 50);
    assert_eq!(outline["scanFailureKeepsLatest"], true);
}

/// 应修 1(前端):删到一半(partial)必须移除行**并且**给撤销按钮,文案保留「部分失败」;
/// 桥重连导致结果未知时既不说失败、也不移除行。
#[test]
fn delete_result_ui_keeps_the_undo_token_for_partial_deletes() {
    let result = run_harness();
    let cases = &result["deleteCases"];

    assert_eq!(cases["localDeleted"]["kind"], "deleted");
    assert_eq!(cases["localDeleted"]["removed"], 1);
    assert_eq!(cases["localDeleted"]["hasUndoButton"], true);

    assert_eq!(cases["partial"]["kind"], "partial");
    assert_eq!(cases["partial"]["removed"], 1, "行已经从库里删了,列表也要去掉");
    assert_eq!(
        cases["partial"]["hasUndoButton"], true,
        "部分失败时撤销 token 必须交给用户,否则会话就找不回来了"
    );
    assert!(
        cases["partial"]["text"].as_str().unwrap().starts_with("部分删除失败"),
        "{:?}",
        cases["partial"]["text"]
    );
    assert_eq!(
        cases["partialKeepsWording"]["text"], "部分失败：rollout 被占用",
        "后端已经说明了「部分」就别再套一层"
    );

    assert_eq!(cases["unknown"]["kind"], "unknown");
    assert_eq!(cases["unknown"]["removed"], 0);
    assert_eq!(cases["unknown"]["hasUndoButton"], false);
    assert!(cases["unknown"]["text"].as_str().unwrap().contains("未知"));

    assert_eq!(cases["failedWithToken"]["kind"], "failed");
    assert_eq!(cases["failedWithToken"]["removed"], 0, "没删掉就别把行拿走");
    assert_eq!(cases["failedWithToken"]["hasUndoButton"], true);
    assert_eq!(cases["failedPlain"]["hasUndoButton"], false);
}

/// 建议 D:桥重注入时,进行中的 /delete、/undo 不能被了结成「失败」—— 后端照样会把
/// 删除做完,前端却当没做,撤销 token 就丢了。改成「结果未知,请刷新确认」。
#[test]
fn bridge_reinjection_marks_mutating_requests_unknown_instead_of_failed() {
    let temp = tempfile::tempdir().expect("temp dir");
    let harness_path = temp.path().join("bridge-reinject-harness.cjs");
    let script = bridge::build_bridge_script(bridge::BRIDGE_BINDING_NAME);
    let harness = format!(
        r#"
globalThis.window = globalThis;
window.{binding} = () => {{}};
const script = {script};
eval(script);
const pending = {{
  del: window.__codexSessionDeleteBridge("/delete", {{}}),
  undo: window.__codexSessionDeleteBridge("/undo", {{}}),
  status: window.__codexSessionDeleteBridge("/backend/status", {{}}),
}};
// 桥换代:重新注入同一段脚本。
eval(script);
Promise.all([pending.del, pending.undo, pending.status]).then(([del, undo, status]) => {{
  process.stdout.write(JSON.stringify({{ del, undo, status }}), () => process.exit(0));
}});
"#,
        binding = bridge::BRIDGE_BINDING_NAME,
        script = serde_json::to_string(&script).expect("script serializes"),
    );
    std::fs::write(&harness_path, harness).expect("write harness");
    let output = Command::new("node")
        .arg(&harness_path)
        .output()
        .expect("node should run");
    assert!(
        output.status.success(),
        "harness failed
stderr:
{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).expect("JSON");

    assert_eq!(result["del"]["status"], "unknown");
    assert_eq!(result["undo"]["status"], "unknown");
    assert!(result["del"]["message"].as_str().unwrap().contains("未知"));
    // 只读请求照旧:说清楚可以重试。
    assert_eq!(result["status"]["status"], "failed");
    assert!(result["status"]["message"].as_str().unwrap().contains("重试"));
}
