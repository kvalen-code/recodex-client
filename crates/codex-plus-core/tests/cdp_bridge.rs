use base64::Engine;
use codex_plus_core::assets;
use codex_plus_core::bridge::{self, BRIDGE_BINDING_NAME};
use codex_plus_core::cdp::{
    CdpTarget, is_avatar_overlay_page_target, is_primary_codex_page_target,
    is_quick_chat_page_target, list_targets, pick_injectable_codex_page_target, pick_page_target,
    validate_cdp_websocket_url,
};

use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use std::future::Future;
use std::io::Write;
use std::net::SocketAddr;
use std::pin::Pin;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{Notify, oneshot};
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

fn target(id: &str, kind: &str, title: &str, url: &str, websocket_url: Option<&str>) -> CdpTarget {
    CdpTarget {
        id: id.to_string(),
        target_type: kind.to_string(),
        title: title.to_string(),
        url: url.to_string(),
        web_socket_debugger_url: websocket_url.map(str::to_string),
    }
}

#[test]
fn bridge_script_defines_expected_globals_and_binding() {
    let script = bridge::build_bridge_script(BRIDGE_BINDING_NAME);

    assert!(script.contains("window.__codexSessionDeleteBridge"));
    assert!(script.contains("window.__codexSessionDeleteResolve"));
    assert!(script.contains("window.__codexSessionDeleteReject"));
    assert!(script.contains("codexSessionDeleteV2"));
}

#[test]
fn screenshot_command_uses_png_from_surface() {
    assert_eq!(
        bridge::capture_screenshot_params(),
        json!({
            "format": "png",
            "fromSurface": true,
            "captureBeyondViewport": false
        })
    );
}

#[test]
fn injection_script_prefixes_helper_url_and_metadata() {
    let script = assets::injection_script(57321);

    assert!(script.contains("!window.electronBridge"));
    assert!(script.contains(r#"!/^app:\/\/\-\//i.test(window.location.href)"#));
    assert!(script.contains("window.__CODEX_SESSION_DELETE_HELPER__"));
    assert!(script.contains("http://127.0.0.1:57321"));
    assert!(!script.contains("window.__CODEX_PLUS_SPONSOR_IMAGES__"));
    assert!(script.contains("window.__CODEX_PLUS_VERSION__"));
    assert!(script.contains(codex_plus_core::version::VERSION));
    // 去品牌是硬要求:上游的 Discord 入口不能出现在我们发出去的包里。
    // 断言从「它在」翻成「它不在」—— 以后合上游把它带回来,这里当场报警。
    assert!(!script.contains("discord.gg"));
    assert!(!script.contains("data-codex-plus-discord"));
}

#[test]
fn pet_real_mouse_settings_are_gated_to_windows_in_injected_ui() {
    let script = assets::injection_script(57321);

    assert!(script.contains("codexPlusIsWindowsPlatform"));
    assert!(script.contains(r#"/\bWindows\b/i.test(navigator.userAgent || "")"#));
    // 顶栏菜单整套已被 recodex-slim 下线,`codexPlusIsWindowsPlatform ? ` 那段
    // 三元渲染随之消失 —— 开关搬到了悬浮面板。守面板侧的入口,别再守死掉的菜单。
    assert!(script.contains("桌宠跟随真实鼠标"));
}

#[test]
fn pet_real_mouse_script_uses_cdp_push_and_native_avatar_event() {
    let script = assets::pet_real_mouse_script();

    assert!(script.contains("avatar-overlay-computer-use-cursor-changed"));
    assert!(script.contains("data-avatar-mascot"));
    assert!(script.contains("nativeCursorActive"));
    assert!(script.contains("transport: \"cdp-push\""));
    assert!(script.contains("updateScreenPoint(point)"));
    assert!(script.contains("localPoint.x >= bounds.left"));
    assert!(script.contains("localPoint.y <= bounds.bottom"));
    assert!(!script.contains("document.elementFromPoint"));
    assert!(script.contains("if (mascotHovered)"));
    assert!(script.contains(
        "document.visibilityState !== \"visible\" || interaction.active() || nativeCursorActive"
    ));
    assert!(script.contains("sendPoint(null).catch(disableUpdates)"));
    assert!(script.contains("void cleared.catch(disableUpdates)"));
    assert!(script.contains("dispatcher.dispatchHostMessage({ type: eventType, point: null })"));
    assert!(script.contains("__codexPlusPetInteraction"));
    assert!(script.contains("setPointerCapture"));
    assert!(script.contains("releasePointerCapture"));
    assert!(script.contains("mascotAtPoint"));
    assert!(script.contains("if (!ownsPointer) return"));
    assert!(script.contains("document.addEventListener(\"pointermove\", onPointerMove, true)"));
    assert!(script.contains("movementHoldMs = 1400"));
    assert!(script.contains("activationRadius = 480"));
    assert!(!script.contains("/pet/cursor-position"));
    assert!(!script.contains("X-Codex-Plus-Pet-Token"));
    assert!(script.contains("delete window.__codexPlusPetRealMouseLook"));
    assert!(script.contains("retired during dispatcher setup"));
    assert!(script.contains("nextUnsubscribe?.()"));
    assert!(script.contains("const runtimeVersion = \"7\""));
}

#[test]
fn pet_real_mouse_cancel_releases_pointer_capture_on_blur_and_stop() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let script_path = temp.path().join("pet-real-mouse.js");
    let harness_path = temp.path().join("pet-real-mouse-cancel-harness.cjs");
    std::fs::write(&script_path, assets::pet_real_mouse_script())
        .expect("pet real-mouse script should be written");
    let mut harness = std::fs::File::create(&harness_path).expect("harness should be created");
    write!(
        harness,
        r#"
const scriptPath = {script_path};
const documentListeners = new Map();
const windowListeners = new Map();
const setCalls = [];
const releaseCalls = [];

class MockElement {{
  closest(selector) {{ return selector === '[data-avatar-mascot="true"]' ? this : null; }}
  getBoundingClientRect() {{ return {{ left: 0, top: 0, right: 100, bottom: 100, width: 100, height: 100 }}; }}
  setPointerCapture(pointerId) {{ setCalls.push(pointerId); }}
  releasePointerCapture(pointerId) {{ releaseCalls.push(pointerId); }}
}}

const mascot = new MockElement();
globalThis.Element = MockElement;
globalThis.window = globalThis;
window.screenX = 0;
window.screenY = 0;
window.addEventListener = (type, listener) => windowListeners.set(type, listener);
window.removeEventListener = (type, listener) => {{
  if (windowListeners.get(type) === listener) windowListeners.delete(type);
}};
globalThis.document = {{
  scripts: [],
  visibilityState: "visible",
  querySelector: (selector) => selector === '[data-avatar-mascot="true"]' ? mascot : null,
  querySelectorAll: () => [],
  addEventListener: (type, listener) => documentListeners.set(type, listener),
  removeEventListener: (type, listener) => {{
    if (documentListeners.get(type) === listener) documentListeners.delete(type);
  }},
}};
globalThis.performance = {{ getEntriesByType: () => [] }};

require(scriptPath);
const runtime = window.__codexPlusPetRealMouseLook;
const pointerEvent = (pointerId) => ({{
  pointerId,
  target: mascot,
  clientX: 50,
  clientY: 50,
  preventDefault() {{}},
}});

documentListeners.get("pointerdown")(pointerEvent(7));
windowListeners.get("blur")();
const activeAfterBlur = runtime.isVisualOverrideActive();

documentListeners.get("pointerdown")(pointerEvent(8));
documentListeners.get("pointerup")(pointerEvent(9));
const activeAfterForeignPointer = runtime.isVisualOverrideActive();
runtime.stop();

process.stdout.write(JSON.stringify({{
  setCalls,
  releaseCalls,
  activeAfterBlur,
  activeAfterForeignPointer,
  runtimeRemoved: window.__codexPlusPetRealMouseLook == null,
}}));
"#,
        script_path = serde_json::to_string(&script_path.to_string_lossy().to_string())
            .expect("script path should serialize")
    )
    .expect("harness should be written");
    drop(harness);

    let output = Command::new("node")
        .arg(&harness_path)
        .output()
        .expect("node should run pet pointer-cancel harness");
    assert!(
        output.status.success(),
        "node harness failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("harness stdout should be JSON");
    assert_eq!(result["setCalls"], json!([7, 8]));
    assert_eq!(result["releaseCalls"], json!([7, 8]));
    assert_eq!(result["activeAfterBlur"], false);
    assert_eq!(result["activeAfterForeignPointer"], true);
    assert_eq!(result["runtimeRemoved"], true);
}

#[test]
fn pet_real_mouse_capability_probe_rejects_v1_without_explicit_v2_evidence() {
    let probe = assets::pet_real_mouse_capability_probe_script();

    assert!(probe.contains("data-avatar-mascot"));
    assert!(probe.contains("image.naturalWidth === 1536"));
    assert!(probe.contains("image.naturalHeight === 2288"));
    assert!(probe.contains("getComputedStyle(element).backgroundImage"));
    assert!(probe.contains("const image = new Image()"));
    assert!(probe.contains("await image.decode()"));
    assert!(probe.contains("if (!await isV2Sprite(mascot)) return false"));
    assert!(!probe.contains("spriteVersionNumber"));
    assert!(probe.contains("dispatchHostMessage"));
    assert!(probe.contains("typeof value.subscribe === \"function\""));
    assert!(!probe.contains("__codexPlusPetRealMouseLook"));
    assert!(!probe.contains("runtimeVersion"));
}

#[test]
fn pet_real_mouse_update_script_stops_when_runtime_capability_is_missing() {
    let script = assets::pet_real_mouse_update_script(-125, 640);

    assert!(script.contains("data-avatar-mascot"));
    assert!(script.contains("image.naturalWidth === 1536"));
    assert!(script.contains("image.naturalHeight === 2288"));
    assert!(script.contains("getComputedStyle(element).backgroundImage"));
    assert!(script.contains("await image.decode()"));
    assert!(script.contains("__codexPlusPetV2SpriteProbe"));
    assert!(script.contains("updateScreenPoint?.({ x: -125, y: 640 }) === true"));
}

#[test]
fn pet_real_mouse_update_script_accepts_png_webp_and_blob_v2_but_rejects_v1() {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let script_path = temp.path().join("pet-update.js");
    let harness_path = temp.path().join("pet-update-harness.cjs");
    std::fs::write(&script_path, assets::pet_real_mouse_update_script(120, 240))
        .expect("pet update script should be written");
    let mut harness = std::fs::File::create(&harness_path).expect("harness should be created");
    write!(
        harness,
        r#"
const fs = require("fs");
const vm = require("vm");
const script = fs.readFileSync({script_path}, "utf8");
const sources = {{
  pngV2: "data:image/png;base64,png-v2",
  webpV2: "data:image/webp;base64,webp-v2",
  webpV1: "data:image/webp;base64,webp-v1",
  blobV2: "blob:codex-plus-pet-v2",
  unknown: "data:image/webp;base64,unknown",
}};
const dimensions = new Map([
  [sources.pngV2, [1536, 2288]],
  [sources.webpV2, [1536, 2288]],
  [sources.webpV1, [1536, 1872]],
  [sources.blobV2, [1536, 2288]],
]);
async function run({{ image = null, source = null }} = {{}}) {{
  let calls = 0;
  let decodes = 0;
  const element = {{ querySelectorAll: () => [] }};
  const mascot = {{
    querySelectorAll: (selector) => selector === "img" && image ? [image] : [element],
  }};
  class MockImage {{
    set src(value) {{ this.source = value; }}
    async decode() {{
      decodes += 1;
      const size = dimensions.get(this.source);
      if (!size) throw new Error("unsupported image");
      [this.naturalWidth, this.naturalHeight] = size;
    }}
  }}
  const context = {{
    document: {{ querySelector: () => mascot }},
    getComputedStyle: (target) => ({{ backgroundImage: target === element && source ? `url("${{source}}")` : "none" }}),
    Image: MockImage,
    window: {{ __codexPlusPetRealMouseLook: {{ updateScreenPoint: () => {{ calls += 1; return true; }} }} }},
  }};
  const result = await vm.runInNewContext(script, context);
  return {{ result, calls, decodes }};
}}
async function runSwitchSequence() {{
  let calls = 0;
  let decodes = 0;
  let source = sources.webpV2;
  const element = {{ querySelectorAll: () => [] }};
  const mascot = {{ querySelectorAll: () => [element] }};
  class MockImage {{
    set src(value) {{ this.source = value; }}
    async decode() {{
      decodes += 1;
      [this.naturalWidth, this.naturalHeight] = dimensions.get(this.source);
    }}
  }}
  const context = {{
    document: {{ querySelector: () => mascot }},
    getComputedStyle: (target) => ({{ backgroundImage: target === element ? `url("${{source}}")` : "none" }}),
    Image: MockImage,
    window: {{ __codexPlusPetRealMouseLook: {{ updateScreenPoint: () => {{ calls += 1; return true; }} }} }},
  }};
  const first = await vm.runInNewContext(script, context);
  const cached = await vm.runInNewContext(script, context);
  source = sources.webpV1;
  const afterV1Switch = await vm.runInNewContext(script, context);
  return {{ first, cached, afterV1Switch, calls, decodes }};
}}
async function runDecodeRace() {{
  let calls = 0;
  let source = sources.webpV2;
  let finishDecode;
  const element = {{ querySelectorAll: () => [] }};
  const mascot = {{ querySelectorAll: () => [element] }};
  class MockImage {{
    set src(value) {{ this.source = value; }}
    async decode() {{
      await new Promise((resolve) => {{ finishDecode = resolve; }});
      [this.naturalWidth, this.naturalHeight] = dimensions.get(this.source);
    }}
  }}
  const context = {{
    document: {{ querySelector: () => mascot }},
    getComputedStyle: (target) => ({{ backgroundImage: target === element ? `url("${{source}}")` : "none" }}),
    Image: MockImage,
    window: {{ __codexPlusPetRealMouseLook: {{ updateScreenPoint: () => {{ calls += 1; return true; }} }} }},
  }};
  const pending = vm.runInNewContext(script, context);
  source = sources.webpV1;
  finishDecode();
  return {{ result: await pending, calls }};
}}
(async () => {{
  process.stdout.write(JSON.stringify({{
    pngV2: await run({{ source: sources.pngV2 }}),
    webpV2: await run({{ source: sources.webpV2 }}),
    blobV2: await run({{ source: sources.blobV2 }}),
    webpV1: await run({{ source: sources.webpV1 }}),
    imgV2: await run({{ image: {{ naturalWidth: 1536, naturalHeight: 2288 }} }}),
    unknown: await run({{ source: sources.unknown }}),
    missing: await run(),
    switchSequence: await runSwitchSequence(),
    decodeRace: await runDecodeRace(),
  }}));
}})().catch((error) => {{ console.error(error); process.exitCode = 1; }});
"#,
        script_path = serde_json::to_string(&script_path.to_string_lossy().to_string())
            .expect("script path should serialize")
    )
    .expect("harness should be written");
    drop(harness);

    let output = Command::new("node")
        .arg(&harness_path)
        .output()
        .expect("node should run pet update harness");
    assert!(
        output.status.success(),
        "node harness failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let cases: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("harness stdout should be JSON");
    assert_eq!(
        cases["pngV2"],
        json!({ "result": true, "calls": 1, "decodes": 1 })
    );
    assert_eq!(
        cases["webpV2"],
        json!({ "result": true, "calls": 1, "decodes": 1 })
    );
    assert_eq!(
        cases["blobV2"],
        json!({ "result": true, "calls": 1, "decodes": 1 })
    );
    assert_eq!(
        cases["imgV2"],
        json!({ "result": true, "calls": 1, "decodes": 0 })
    );
    assert_eq!(
        cases["webpV1"],
        json!({ "result": false, "calls": 0, "decodes": 1 })
    );
    assert_eq!(
        cases["unknown"],
        json!({ "result": false, "calls": 0, "decodes": 1 })
    );
    assert_eq!(
        cases["missing"],
        json!({ "result": false, "calls": 0, "decodes": 0 })
    );
    assert_eq!(
        cases["switchSequence"],
        json!({
            "first": true,
            "cached": true,
            "afterV1Switch": false,
            "calls": 2,
            "decodes": 2
        })
    );
    assert_eq!(cases["decodeRace"], json!({ "result": false, "calls": 0 }));
}

#[test]
fn pet_real_mouse_stop_script_retires_existing_runtime() {
    assert!(assets::pet_real_mouse_stop_script().contains("__codexPlusPetRealMouseLook?.stop?.()"));
}

#[test]
fn injection_script_exposes_image_overlay_config() {
    let temp = tempfile::tempdir().unwrap();
    let image_path = temp.path().join("overlay.png");
    std::fs::write(
        &image_path,
        base64::engine::general_purpose::STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/p9sAAAAASUVORK5CYII=")
            .unwrap(),
    )
    .unwrap();
    let settings = codex_plus_core::settings::BackendSettings {
        codex_app_image_overlay_enabled: true,
        codex_app_image_overlay_path: image_path.to_string_lossy().to_string(),
        codex_app_image_overlay_opacity: 42,
        codex_app_image_overlay_fit_mode: "fill".to_string(),
        ..Default::default()
    };
    let script = assets::injection_script_with_settings(57321, &settings);

    assert!(script.contains("window.__CODEX_PLUS_IMAGE_OVERLAY__"));
    assert!(script.contains("\"enabled\":true"));
    assert!(script.contains("\"opacity\":0.42"));
    assert!(script.contains("\"fitMode\":\"fill\""));
    assert!(script.contains("\"dataUrl\":\"data:image/png;base64,"));
    assert!(script.contains("http://127.0.0.1:57321/overlay/image"));
}

#[test]
fn official_login_usage_alert_setting_controls_renderer_injection() {
    use codex_plus_core::settings::{RelayMode, RelayProfile};

    let settings = |relay_mode, hide_official_usage_alert, official_mix_api_key| {
        codex_plus_core::settings::BackendSettings {
            active_relay_id: "official".to_string(),
            relay_profiles: vec![RelayProfile {
                id: "official".to_string(),
                relay_mode,
                official_mix_api_key,
                hide_official_usage_alert,
                ..Default::default()
            }],
            ..Default::default()
        }
    };

    assert!(
        assets::injection_script_with_settings(57321, &settings(RelayMode::Official, true, false))
            .contains("window.__CODEX_PLUS_HIDE_OFFICIAL_USAGE_ALERT__ = true;")
    );
    assert!(
        assets::injection_script_with_settings(57321, &settings(RelayMode::Official, true, true))
            .contains("window.__CODEX_PLUS_HIDE_OFFICIAL_USAGE_ALERT__ = true;")
    );
    assert!(
        assets::injection_script_with_settings(57321, &settings(RelayMode::Official, false, false))
            .contains("window.__CODEX_PLUS_HIDE_OFFICIAL_USAGE_ALERT__ = false;")
    );
    assert!(
        assets::injection_script_with_settings(57321, &settings(RelayMode::PureApi, true, false))
            .contains("window.__CODEX_PLUS_HIDE_OFFICIAL_USAGE_ALERT__ = false;")
    );
}

#[test]
fn usage_alert_hider_uses_sidebar_semantics_instead_of_percentage_copy() {
    let script = assets::injection_script(57321);

    assert!(script.contains("officialUsageAlertCards"));
    assert!(script.contains("progress[max=\"100\"]"));
    assert!(script.contains("dismiss usage alert|关闭使用量提醒"));
    assert!(script.contains("codexPlusUsageAlertHidden"));
}

#[test]
fn injection_script_installs_image_overlay_from_data_uri() {
    let script = assets::injection_script(57321);

    assert!(script.contains("const source = config.dataUrl || \"\""));
    assert!(script.contains("backgroundImage: `url(\"${source.replace(/\"/g, \"%22\")}\")`"));
    assert!(script.contains(
        "fit: { size: \"contain\", position: \"center center\", repeat: \"no-repeat\" }"
    ));
    assert!(script.contains("image_overlay_installed"));
}

#[test]
fn rejects_non_loopback_cdp_websocket() {
    let error =
        validate_cdp_websocket_url("ws://example.com:9222/devtools/page/1", 9222).unwrap_err();

    assert!(error.to_string().contains("loopback"));
}

#[test]
fn rejects_mismatched_cdp_websocket_port() {
    let error =
        validate_cdp_websocket_url("ws://127.0.0.1:9333/devtools/page/1", 9222).unwrap_err();

    assert!(error.to_string().contains("port"));
}

#[test]
fn validates_ipv4_and_ipv6_loopback_cdp_websockets() {
    validate_cdp_websocket_url("ws://127.0.0.1:9222/devtools/page/1", 9222).unwrap();
    validate_cdp_websocket_url("ws://[::1]:9222/devtools/page/1", 9222).unwrap();
}

#[test]
fn rejects_cdp_websocket_with_wrong_scheme_or_missing_port() {
    assert!(validate_cdp_websocket_url("http://127.0.0.1:9222/devtools/page/1", 9222).is_err());
    assert!(validate_cdp_websocket_url("ws://127.0.0.1/devtools/page/1", 9222).is_err());
}

#[test]
fn injection_script_marks_diagnostic_build_and_reports_script_loaded() {
    let script = assets::injection_script(57321);

    assert!(script.contains("window.__CODEX_PLUS_BUILD__"));
    assert!(script.contains(codex_plus_core::assets::DIAGNOSTIC_BUILD_ID));
    assert!(script.contains("script_loaded"));
    // `data-codex-plus-build` 是挂在顶栏菜单上的 DOM 标记,菜单下线后它也没了。
    // 遥测认的是 `window.__CODEX_PLUS_BUILD__`(上面两条已经守住),不受影响。
}

#[test]
/// 上游的广告位在 recodex-slim 里整套下线了。这条断言因此**整个翻过来**:
/// 守的不再是「广告能拉到」,而是「广告一行都不许回来」—— 合上游时它是唯一
/// 会当场拦住的东西。
fn injection_script_ships_without_upstream_ads() {
    let script = assets::injection_script(57321);

    assert!(!script.contains("directFetchCodexPlusAds"));
    assert!(!script.contains("cacheBustCodexPlusAdUrl"));
    assert!(!script.contains("BigPizzaV3/Ad-List"));
    assert!(!script.contains("normalizeCodexPlusAds"));
    assert!(!script.contains("codexPlusAds"));
    // recodex-slim 删广告时把这两个函数漏在了原地,零调用的死代码。
    assert!(!script.contains("isCodexPlusAdExpired"));
    assert!(!script.contains("CodexPlusAd"));
}

#[test]
fn injection_script_times_out_backend_bridge_calls_and_falls_back_to_helper() {
    let script = assets::injection_script(57321);

    assert!(script.contains("bridgeWithBackendTimeout"));
    assert!(script.contains("backend_bridge_timeout"));
    assert!(!script.contains("/backend/repair"));
    assert!(script.contains("backend_status_bridge_failed_http_fallback_ok"));
    assert!(script.contains("backend_status_bridge_and_http_failed"));
}

#[test]
fn injection_script_explains_plugin_patch_is_unneeded_in_relay_mode() {
    let script = assets::injection_script(57321);

    // 菜单下线后这句提示从菜单文案变成了开关上的 CSS ::after,文字也短了。
    // 要守的是「relay 模式下用户看得到『不用开』」这件事,不是那串旧文案。
    assert!(script.contains(r#"[data-relay-unneeded="true"]"#));
    assert!(script.contains(r#"content: "无需开启""#));
    // 判定本身:后端设置没到位、或 launchMode 是 relay,就算「无需开启」。
    assert!(script.contains(r#"codexPlusBackendSettings.launchMode === "relay""#));
}

#[test]
fn injection_script_menu_exposes_marketplace_plugin_switch_only() {
    let script = assets::injection_script(57321);

    // 开关搬到了悬浮面板,那里用 (后端键, 中文标签) 的元组声明,不再是
    // `data-codex-plus-setting` 属性。守新写法,否定断言原样保留 —— 那几个
    // 已经砍掉的入口,任何一个回来都要当场拦住。
    assert!(script.contains(r#"["codexAppPluginMarketplaceUnlock", "插件市场解锁"]"#));
    assert!(script.contains("pluginMarketplaceUnlock: \"codexAppPluginMarketplaceUnlock\""));
    assert!(!script.contains("特殊插件强制安装"));
    assert!(!script.contains("data-codex-plus-setting=\"forcePluginInstall\""));
    assert!(!script.contains("forcePluginInstall"));
    assert!(!script.contains("强制解锁入口"));
    assert!(!script.contains("data-codex-plus-setting=\"pluginEntryUnlock\""));
}

#[test]
fn injection_script_defers_backend_mapped_toggles_until_settings_load() {
    let script = assets::injection_script(57321);

    // 这批断言原来守的是**顶栏菜单**怎么把开关置灰,菜单整套已被 recodex-slim
    // 下线,那几行渲染代码随之消失。剩下能守、也真正要守的是那份「哪些开关的
    // 真值在后端」的映射本身 —— 少一项就会出现「本地显示开着、后端其实没开」。
    assert!(script.contains("const codexPlusBackendMappedSettings = new Set"));
    assert!(script.contains("const codexPlusBackendSettingMap = {"));
    assert!(script.contains("let codexPlusBackendSettingsLoaded = false"));
    // 后端设置没回来之前不能把它当已加载 —— 否则会拿默认值去覆盖用户的真实开关。
    assert!(script.contains("!codexPlusBackendSettingsLoaded"));
}

#[test]
fn injection_script_ignores_stale_backend_settings_responses() {
    let script = assets::injection_script(57321);

    assert!(script.contains("let codexPlusBackendSettingsSeq = 0"));
    assert!(script.contains("const seq = codexPlusBackendSettingsSeq"));
    assert!(script.contains("if (seq !== codexPlusBackendSettingsSeq)"));
    assert!(script.contains("const seq = ++codexPlusBackendSettingsSeq"));
    assert!(script.contains("if (seq === codexPlusBackendSettingsSeq)"));
}

#[test]
fn injection_script_skips_plugin_patch_work_in_relay_mode() {
    let script = assets::injection_script(57321);

    assert!(script.contains("function pluginPatchDisabledInRelayMode()"));
    assert!(script.contains("!codexPlusBackendSettingsLoaded"));
    assert!(script.contains("if (pluginPatchDisabledInRelayMode()) return"));
    assert!(script.contains("clearPluginPatchArtifacts()"));
}

#[test]
fn injection_script_omits_plugin_auto_expand() {
    let script = assets::injection_script(57321);

    assert!(!script.contains("schedulePluginAutoExpand"));
    assert!(!script.contains("pluginAutoExpand"));
    assert!(!script.contains("codexPluginAutoExpand"));
    assert!(!script.contains("plugin_auto_expand"));
}

#[test]
fn injection_script_defines_version_gated_plugin_unlock_strategy() {
    let script = assets::injection_script(57321);

    assert!(script.contains("codexPluginLegacyEntryUnlockBeforeVersion = \"26.601.2237\""));
    assert!(script.contains("function parseCodexVersionParts(version)"));
    assert!(script.contains("function compareCodexVersions(left, right)"));
    assert!(script.contains("function codexPluginUnlockStrategy()"));
    assert!(script.contains("const comparison = compareCodexVersions(version, codexPluginLegacyEntryUnlockBeforeVersion)"));
    assert!(script.contains("return comparison < 0 ? \"legacy\" : \"modern\""));
}

#[test]
fn injection_script_gates_legacy_and_modern_plugin_unlock_by_codex_version() {
    let script = assets::injection_script(57321);

    assert!(script.contains("const pluginUnlockStrategy = codexPluginUnlockStrategy()"));
    assert!(script.contains("if ((pluginUnlockStrategy === \"modern\" || pluginUnlockStrategy === \"unknown\") && settings.pluginMarketplaceUnlock)"));
    assert!(script.contains("plugin_unlock_strategy_selected"));
    assert!(script.contains("window.__codexPluginUnlockStrategyLogged"));
}

#[test]
fn injection_script_removes_legacy_plugin_sidebar_entry_unlock() {
    let script = assets::injection_script(57321);

    assert!(!script.contains("pluginEntryUnlock"));
    assert!(!script.contains("codexAppPluginEntryUnlock"));
    assert!(!script.contains("function spoofChatGPTAuthMethod(element)"));
    assert!(!script.contains("auth.setAuthMethod(\"chatgpt\")"));
    assert!(!script.contains("function pluginEntryButton()"));
    assert!(!script.contains("function enablePluginEntry()"));
    assert!(!script.contains("插件 - 已解锁"));
    assert!(!script.contains("Plugins - Unlocked"));
}

#[test]
fn injection_script_keeps_plugin_marketplace_unlock_separate_from_entry_unlock() {
    let script = assets::injection_script(57321);

    assert!(script.contains("pluginMarketplaceUnlock: true"));
    assert!(script.contains("pluginMarketplaceUnlock: \"codexAppPluginMarketplaceUnlock\""));
    assert!(script.contains("if (!codexPlusSettings().pluginMarketplaceUnlock) return"));
    assert!(script.contains("installPluginBuildFlavorFilterPatch"));
    assert!(script.contains("installPluginMarketplaceRequestPatch"));
}

#[test]
fn injection_script_localizes_codex_menu_commands() {
    let script = assets::injection_script(57321);

    assert!(script.contains("const codexMenuLocalizationMap = new Map"));
    assert!(script.contains("[\"Toggle Sidebar\", \"切换侧边栏\"]"));
    assert!(script.contains("[\"Toggle Bottom Panel\", \"切换底部面板\"]"));
    assert!(script.contains("[\"Toggle Pinned Summary\", \"切换置顶摘要\"]"));
    assert!(script.contains("[\"Open Terminal\", \"打开终端\"]"));
    assert!(script.contains("[\"Open Browser Tab\", \"打开浏览器标签页\"]"));
    assert!(script.contains("[\"Focus Browser Address Bar\", \"聚焦浏览器地址栏\"]"));
    assert!(script.contains("[\"Reload Browser Page\", \"重新加载浏览器页面\"]"));
    assert!(script.contains("[\"Toggle Side Panel\", \"切换侧边面板\"]"));
    assert!(script.contains("[\"Actual Size\", \"实际大小\"]"));
    assert!(script.contains("function localizeCodexMenus"));
    assert!(script.contains("localizeCodexMenus();"));
}

#[test]
fn injection_script_does_not_unlock_disabled_plugin_install_buttons() {
    let script = assets::injection_script(57321);

    assert!(script.contains("button[aria-disabled=\"true\"]"));
    assert!(script.contains("[role=\"button\"][data-disabled]"));
    assert!(!script.contains("installButtonUnlockNodes"));
    assert!(!script.contains("patchReactDisabledProps"));
    assert!(!script.contains("props[\"data-disabled\"] = undefined"));
    assert!(!script.contains("button.querySelectorAll?.(\"button, [role='button'], [disabled], [aria-disabled], [data-disabled]"));
    assert!(!script.contains("button.dataset.codexForceInstallUnlocked"));
}

#[test]
fn injection_script_keeps_bundled_marketplace_name_for_default_filter() {
    let script = assets::injection_script(57321);

    assert!(script.contains("codexPluginMarketplaceUnlockVersion = \"15\""));
    assert!(!script.contains("function pluginMarketplaceAliasForName"));
    assert!(
        !script.contains("if (name === \"openai-bundled\") return \"codex-plus-openai-bundled\"")
    );
    assert!(script.contains("if (name === \"openai-bundled\") return \"OpenAI插件1(Codex++)\""));
}

#[test]
fn injection_script_does_not_bypass_plugin_marketplace_search_filters() {
    let script = assets::injection_script(57321);

    assert!(script.contains("codexPluginMarketplaceUnlockVersion = \"15\""));
    assert!(script.contains("isCodexPluginBuildFlavorFilter"));
    assert!(script.contains("source.includes(\"!u(e.marketplaceName)||e.marketplaceName===r\")"));
    assert!(script.contains("source.includes(\"!Eu(e.marketplaceName)||e.marketplaceName===n\")"));
    assert!(script.contains("source.includes(\"!t.includes(e.name)\")"));
    assert!(!script.contains("if (!source.includes(\"marketplaceName\")) return false"));
    assert!(!script.contains("if (!source.includes(\"name\")) return false"));
}

#[test]
fn injection_script_expands_api_key_plugin_marketplace_requests() {
    let script = assets::injection_script(57321);

    assert!(script.contains("codexPluginMarketplaceUnlockVersion = \"15\""));
    assert!(script.contains("installPluginMarketplaceRequestPatch"));
    assert!(script.contains("installPluginMarketplaceBridgePatch"));
    assert!(script.contains("installPluginBuildFlavorFilterPatch"));
    assert!(script.contains("Array.prototype.filter"));
    assert!(script.contains("codexPluginBuildFlavorFilterPatch"));
    assert!(script.contains("isCodexPluginBuildFlavorFilter"));
    assert!(script.contains(
        "codexPluginOfficialMarketplaceName(plugin?.marketplaceName) && !callback(plugin)"
    ));
    assert!(script.contains("isCodexPluginMarketplaceHiddenFilter"));
    assert!(script.contains(
        "codexPluginOfficialMarketplaceName(marketplace?.name) && !callback(marketplace)"
    ));
    assert!(script.contains("plugin_marketplace_hidden_filter_bypassed"));
    assert!(script.contains("method === \"list-plugins\""));
    assert!(script.contains("method === \"vscode://codex/list-plugins\""));
    assert!(script.contains("message.type === \"fetch\""));
    assert!(script.contains("data?.type === \"fetch-response\""));
    assert!(script.contains("__codexPluginMarketplaceFetchRequestIds"));
    assert!(script.contains("__codexPluginMarketplaceFetchRequestProfiles"));
    assert!(script.contains("__codexPluginMarketplaceRequestProfiles"));
    assert!(script.contains("pluginMarketplaceRequestProfile"));
    assert!(script.contains("remoteOnlyPluginMarketplaceFallbackResult"));
    assert!(script.contains("let nextKinds = Array.isArray(next.marketplaceKinds)"));
    assert!(script.contains("if (!nextKinds.includes(\"local\")) nextKinds.push(\"local\")"));
    assert!(script.contains("if (!nextKinds.includes(\"vertical\")) nextKinds.push(\"vertical\")"));
    assert!(script.contains("next.marketplaceKinds = Array.from(new Set(nextKinds))"));
    assert!(script.contains("codexPluginBroadCatalogKindsFromVersion = \"26.803.0\""));
    assert!(script.contains("broadCatalogPreserved: true"));
    assert!(script.contains("patchPluginMarketplaceResult"));
    assert!(script.contains("__CODEX_PLUS_PLUGIN_MARKETPLACES__"));
    assert!(script.contains("mergeLocalPluginMarketplaces(result)"));
    assert!(script.contains("plugin_marketplace_local_merged"));
    assert!(script.contains("plugin_marketplace_remote_auth_fallback"));
    assert!(script.contains("cloned.marketplaceName = marketplaceName"));
    assert!(script.contains("cloned.marketplacePath = marketplaceName"));
    assert!(script.contains("restorePluginMarketplaceName"));
    assert!(script.contains(
        "next.remoteMarketplaceName = restorePluginMarketplaceName(next.remoteMarketplaceName)"
    ));
    assert!(!script.contains("marketplace.name = alias"));
    assert!(script.contains("if (name === \"openai-curated\") return \"OpenAI插件2(Codex++)\""));
    assert!(
        script.contains("if (name === \"openai-primary-runtime\") return \"OpenAI插件3(Codex++)\"")
    );
    assert!(script.contains("restored === \"openai-api-curated\""));
    assert!(script.contains("restored === \"openai-curated-remote\""));
    assert!(
        script.contains("if (name === \"openai-curated-remote\") return \"OpenAI插件5(Codex++)\"")
    );
    assert!(script.contains(
        "if (name === \"codex-plus-openai-curated-remote\") return \"openai-curated-remote\""
    ));
    assert!(script.contains("OpenAI插件1(Codex++)"));
    assert!(script.contains("OpenAI插件2(Codex++)"));
    assert!(script.contains("OpenAI插件3(Codex++)"));
    assert!(script.contains("method === \"install-plugin\""));
    assert!(script.contains("plugin_marketplace_response_expanded"));
    assert!(script.contains("plugin_build_flavor_filter_bypassed"));
    assert!(script.contains("plugin_install_request_debug"));
    assert!(script.contains("plugin_install_request_failed"));
    assert!(!script.contains("marketplace.path ="));
    assert!(!script.contains("codexPluginMarketplacePathAliasForName"));
    assert!(!script.contains("spoofAnyCodexAuthContext"));
}

#[test]
fn injection_script_preserves_vertical_marketplace_kind_for_official_plugins() {
    let script = assets::injection_script(57321);

    assert!(script.contains("plugin_marketplace_request_expanded"));
    assert!(script.contains("if (!nextKinds.includes(\"vertical\")) nextKinds.push(\"vertical\")"));
    assert!(!script.contains("codexPluginAllowedMarketplaceKinds"));
    assert!(!script.contains("codexPluginExpandedMarketplaceKinds"));
    assert!(!script.contains("delete next.marketplaceKinds"));
}

#[test]
fn injection_script_logs_marketplace_grouping_diagnostics() {
    let script = assets::injection_script(57321);

    assert!(script.contains("plugin_marketplace_response_debug"));
    assert!(script.contains("marketplaces: result.marketplaces.map"));
    assert!(script.contains("pluginMarketplaceCounts"));
    assert!(script.contains("remoteMarketplaceName"));
}

#[test]
fn injection_script_recovers_plugin_search_from_remote_auth_errors() {
    let cases = run_plugin_marketplace_search_contract_harness();

    assert_eq!(cases["initialKinds"], json!(["local", "vertical"]));
    assert_eq!(cases["latestBroadOmittedHasKinds"], false);
    assert_eq!(cases["latestBroadOmittedKinds"], serde_json::Value::Null);
    assert_eq!(cases["latestBroadNullHasKinds"], true);
    assert_eq!(cases["latestBroadNullKinds"], serde_json::Value::Null);
    assert_eq!(cases["latestExplicitKinds"], json!(["local", "vertical"]));
    assert_eq!(cases["searchKinds"], json!(["created-by-me-remote"]));
    assert_eq!(cases["searchCwds"], serde_json::Value::Null);
    assert_eq!(cases["searchRemoteOnly"], true);
    assert_eq!(cases["responsePatched"], true);
    assert_eq!(cases["responseHasError"], false);
    assert_eq!(cases["fallbackMarketplaceNames"], json!([]));
    assert_eq!(cases["fallbackPluginNames"], json!([]));
    assert_eq!(cases["fallbackFeaturedPluginIds"], json!([]));
    assert_eq!(cases["fallbackMarketplaceLoadErrors"], json!([]));
    assert_eq!(cases["remoteUnavailable"], true);
    assert_eq!(cases["subsequentKinds"], json!(["created-by-me-remote"]));
    assert_eq!(cases["subsequentCwds"], serde_json::Value::Null);
    assert_eq!(
        cases["generalAfterFallbackKinds"],
        json!(["local", "vertical"])
    );
    assert_eq!(
        cases["latestBroadAfterFallbackKinds"],
        json!(["local", "vertical"])
    );
    assert_eq!(cases["generalAfterFallbackCwds"], json!(["C:/workspace"]));
    assert_eq!(
        cases["localFallbackMarketplaceNames"],
        json!(["fixture-local"])
    );
    assert_eq!(cases["localFallbackPluginNames"], json!(["alpha"]));
    assert_eq!(cases["chatGptKinds"], json!(["created-by-me-remote"]));
    assert_eq!(cases["unrelatedErrorMatched"], false);
}

fn run_plugin_marketplace_search_contract_harness() -> serde_json::Value {
    let temp = tempfile::tempdir().expect("temp dir should be created");
    let script_path = temp.path().join("renderer-inject.js");
    let harness_path = temp.path().join("plugin-marketplace-harness.cjs");
    std::fs::write(&script_path, assets::injection_script(57321))
        .expect("injection script should be written");
    let mut harness = std::fs::File::create(&harness_path).expect("harness should be created");
    write!(
        harness,
        r#"
const scriptPath = {script_path};
const store = new Map();
function node() {{
  return {{
    appendChild() {{}}, prepend() {{}}, remove() {{}}, setAttribute() {{}}, removeAttribute() {{}},
    addEventListener() {{}}, querySelector() {{ return null; }}, querySelectorAll() {{ return []; }},
    closest() {{ return null; }},
    classList: {{ add() {{}}, remove() {{}}, toggle() {{}}, contains() {{ return false; }} }},
    dataset: {{}}, style: {{}}, children: [], isConnected: true, textContent: "", innerHTML: "",
  }};
}}
globalThis.window = globalThis;
window.__CODEX_PLUS_TEST_PLUGIN_MARKETPLACE__ = true;
window.addEventListener = () => {{}};
window.removeEventListener = () => {{}};
window.dispatchEvent = () => true;
globalThis.document = {{
  scripts: [], documentElement: node(), body: node(), createElement: () => node(),
  getElementById: () => null, querySelector: () => null, querySelectorAll: () => [],
  addEventListener() {{}}, removeEventListener() {{}},
}};
globalThis.localStorage = {{
  getItem: (key) => store.has(key) ? store.get(key) : null,
  setItem: (key, value) => store.set(key, String(value)), removeItem: (key) => store.delete(key),
}};
globalThis.sessionStorage = globalThis.localStorage;
globalThis.location = {{ href: "https://codex.test/index.html", pathname: "/index.html", search: "", hash: "" }};
window.location = globalThis.location;
globalThis.navigator = {{ userAgent: "node-test", sendBeacon: () => false }};
globalThis.performance = {{ getEntriesByType: () => [] }};
globalThis.fetch = async () => ({{ ok: true, json: async () => ({{}}) }});
require(scriptPath);
window.__CODEX_PLUS_PLUGIN_MARKETPLACES__ = [{{
  name: "fixture-local",
  displayName: "Fixture Local",
  path: "C:/fixture/marketplace.json",
  plugins: [{{ id: "alpha@fixture-local", name: "alpha", marketplaceName: "fixture-local" }}],
}}];
const api = window.__codexPlusPluginMarketplaceTest;
api.reset();
const initial = api.patchRequestParams("list-plugins", {{ cwds: ["C:/workspace"] }});
api.setCodexAppVersion("26.803.41515");
const latestBroadOmitted = api.patchRequestParams("list-plugins", {{ cwds: ["C:/workspace"] }});
const latestBroadNull = api.patchRequestParams("list-plugins", {{ cwds: ["C:/workspace"], marketplaceKinds: null }});
const latestExplicit = api.patchRequestParams("list-plugins", {{ marketplaceKinds: ["local"] }});
api.setCodexAppVersion("");
const searchMessage = api.patchRequestMessage({{
  type: "mcp-request",
  request: {{
    id: "search-1",
    method: "vscode://codex/list-plugins",
    params: {{ marketplaceKinds: ["created-by-me-remote"] }},
  }},
}});
const remoteAuthMessage = "list remote plugin catalog: chatgpt authentication required for remote plugin catalog; api key auth is not supported";
const response = {{
  type: "mcp-response",
  message: {{ id: "search-1", error: {{ code: -32600, message: remoteAuthMessage }} }},
}};
const responsePatched = api.patchResponseData(response);
const subsequent = api.patchRequestParams("list-plugins", {{ marketplaceKinds: ["created-by-me-remote"] }});
const generalAfterFallback = api.patchRequestParams("list-plugins", {{ marketplaceKinds: ["created-by-me-remote", "local", "vertical"] }});
api.setCodexAppVersion("26.803.41515");
const latestBroadAfterFallback = api.patchRequestParams("list-plugins", {{ cwds: ["C:/workspace"] }});
const fallbackMarketplaces = response.message.result?.marketplaces || [];
const localFallbackMarketplaces = api.localFallback().marketplaces || [];
const remoteUnavailable = api.remoteCatalogUnavailable();
api.reset();
const chatGpt = api.patchRequestParams("list-plugins", {{ marketplaceKinds: ["created-by-me-remote"] }});
const cases = {{
  initialKinds: initial.marketplaceKinds,
  latestBroadOmittedHasKinds: Object.prototype.hasOwnProperty.call(latestBroadOmitted, "marketplaceKinds"),
  latestBroadOmittedKinds: latestBroadOmitted.marketplaceKinds ?? null,
  latestBroadNullHasKinds: Object.prototype.hasOwnProperty.call(latestBroadNull, "marketplaceKinds"),
  latestBroadNullKinds: latestBroadNull.marketplaceKinds,
  latestExplicitKinds: latestExplicit.marketplaceKinds,
  searchKinds: searchMessage.request.params.marketplaceKinds,
  searchCwds: searchMessage.request.params.cwds ?? null,
  searchRemoteOnly: api.requestProfile({{ marketplaceKinds: ["created-by-me-remote"] }}).remoteOnly,
  responsePatched,
  responseHasError: Object.prototype.hasOwnProperty.call(response.message, "error"),
  fallbackMarketplaceNames: fallbackMarketplaces.map((marketplace) => marketplace.name),
  fallbackPluginNames: fallbackMarketplaces.flatMap((marketplace) => marketplace.plugins || []).map((plugin) => plugin.name),
  fallbackFeaturedPluginIds: response.message.result?.featuredPluginIds || [],
  fallbackMarketplaceLoadErrors: response.message.result?.marketplaceLoadErrors || [],
  remoteUnavailable,
  subsequentKinds: subsequent.marketplaceKinds,
  subsequentCwds: subsequent.cwds ?? null,
  generalAfterFallbackKinds: generalAfterFallback.marketplaceKinds,
  generalAfterFallbackCwds: generalAfterFallback.cwds,
  latestBroadAfterFallbackKinds: latestBroadAfterFallback.marketplaceKinds,
  localFallbackMarketplaceNames: localFallbackMarketplaces.map((marketplace) => marketplace.name),
  localFallbackPluginNames: localFallbackMarketplaces.flatMap((marketplace) => marketplace.plugins || []).map((plugin) => plugin.name),
  chatGptKinds: chatGpt.marketplaceKinds,
  unrelatedErrorMatched: api.remoteAuthError({{ message: "network unavailable" }}),
}};
// 必须显式退出:这个 harness `require` 的是**完整**注入脚本,它会注册
// setInterval 之类的常驻定时器,node 的 event loop 因此永远不空 —— 不退的话
// 子进程一直挂着,Command::output() 跟着无限等待,整个 cdp_bridge 目标就卡死在
// 这一个用例上,它后面的测试一个都跑不到(表现是「跑全量测试十几分钟不出结果」)。
// 用 write 的回调而不是裸 exit:stdout 是管道时是异步写,直接退会丢结果。
process.stdout.write(JSON.stringify(cases), () => process.exit(0));
"#,
        script_path = serde_json::to_string(&script_path.to_string_lossy().to_string())
            .expect("script path should serialize")
    )
    .expect("harness should be written");

    let output = std::process::Command::new("node")
        .arg(&harness_path)
        .output()
        .expect("node should execute plugin marketplace harness");
    assert!(
        output.status.success(),
        "plugin marketplace harness failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout)
        .expect("plugin marketplace harness output should be JSON")
}

#[test]
fn injection_script_omits_force_install_unlock_loop() {
    let script = assets::injection_script(57321);

    assert!(!script.contains("codex-force-install-unlocked"));
    assert!(!script.contains("codexForcePluginInstallRefreshIntervalMs"));
    assert!(!script.contains("refreshForcePluginInstallUnlockLoop"));
    assert!(!script.contains("__codexForcePluginInstallRefreshTimer"));
}

#[test]
fn injection_script_loads_backend_settings_before_initial_scan() {
    let script = assets::injection_script(57321);
    let startup_call = script
        .rfind("void loadBackendSettingsForStartup();")
        .expect("script should load backend settings on startup");
    let footer = &script[startup_call..];
    let initial_scan = footer
        .find("scan();")
        .expect("script should perform an initial scan");
    let footer_marker = footer
        .find("window.removeEventListener(\"resize\"")
        .expect("script should continue bootstrapping after the initial scan");

    assert!(initial_scan < footer_marker);
    assert!(script.contains("if (attempt < 60)"));
}

#[test]
fn injection_script_exposes_conversation_view_width_control() {
    let script = assets::injection_script(57321);

    assert!(script.contains("conversationView: false"));
    assert!(script.contains("conversationView"));
    assert!(script.contains("conversationViewMaxWidth"));
    assert!(script.contains("对话居中宽度"));
    assert!(script.contains("data-codex-plus-conversation-view-width"));
    assert!(script.contains("conversationViewWidth()"));
    assert!(script.contains("normalizeConversationViewWidth"));
}

#[test]
fn injection_script_exposes_sidebar_thread_id_badge_control() {
    let script = assets::injection_script(57321);

    assert!(script.contains("threadIdBadge: false"));
    assert!(script.contains("threadIdBadge: \"codexAppThreadIdBadge\""));
    // 开关本身搬到了悬浮面板(元组声明),菜单里的 data-codex-plus-setting 属性
    // 已随菜单下线。功能实现仍在 renderer 侧,下面几条继续守着。
    assert!(script.contains(r#"["codexAppThreadIdBadge", "会话 ID 标识"]"#));
    assert!(script.contains("codex-thread-id-badge"));
    assert!(script.contains("data-codex-thread-id-badge-wrap=\"true\""));
    assert!(script.contains("let threadIdBadgeActive = false"));
    assert!(script.contains("if (threadIdBadgeActive)"));
    assert!(script.contains("function refreshThreadIdBadges()"));
    assert!(script.contains("uuidV7TimestampMs(sessionId)"));
    assert!(script.contains("refreshThreadIdBadges();"));
}

#[test]
fn injection_script_keeps_session_action_buttons_in_pr_style() {
    let script = assets::injection_script(57321);

    assert!(script.contains("actionButtonClass = \"codex-session-action-button\""));
    assert!(script.contains("background: transparent;"));
    assert!(script.contains("background: #363839;"));
    assert!(script.contains("cursor: default;"));
}

/// 匹配不上时必须把**当时看见了什么**一并说出来。
///
/// 四条优先级里三条依赖 is_primary_codex_page_target,Codex 一次界面改版就可能让它们
/// 全部失配。线上这条报了 28 次,每次都只有一句 "No injectable Codex page target found",
/// 结果三种完全不同的情况分不开:页面还没加载完(重试会好)、CDP 里根本没有 page
/// (Codex 没起到那一步)、有页面但规则全过期了(**必须改代码**,重试一万次也没用)。
#[test]
fn missing_codex_target_error_lists_what_was_observed() {
    let targets = vec![
        target("a", "page", "Some Other App", "https://example.com/", Some("ws://x/1")),
        target("b", "service_worker", "sw", "https://example.com/sw.js", None),
    ];

    let error = pick_injectable_codex_page_target(&targets)
        .expect_err("这些 target 都不该匹配上");
    let message = format!("{error:#}");

    assert!(
        message.contains("example.com"),
        "错误里要能看到当时的 target,否则分不清是时序还是匹配规则过期:{message}"
    );
    assert!(
        message.contains("service_worker") && message.contains("page"),
        "target 类型要留下来 —— 只有 service_worker 没有 page,说明 Codex 还没起到那一步:{message}"
    );
}

/// 一个 target 都没有时也要说清「没有」,不能只留一句没上下文的报错。
#[test]
fn missing_codex_target_error_says_none_when_list_is_empty() {
    let error = pick_injectable_codex_page_target(&[]).expect_err("空列表必然匹配不上");
    assert!(
        format!("{error:#}").contains("(none)"),
        "空列表要显式说明,不能和「有页面但不匹配」长得一样"
    );
}

/// 上报要能装下,但不能被一个开了几十个标签页的用户撑爆。
#[test]
fn observed_targets_are_capped_and_truncated() {
    let long_title = "标".repeat(200);
    let mut targets: Vec<CdpTarget> = (0..20)
        .map(|i| target(&i.to_string(), "page", &long_title, "https://example.com/", Some("ws://x/1")))
        .collect();
    targets.push(target("z", "other", "z", "z", None));

    let message = format!("{:#}", pick_injectable_codex_page_target(&targets).expect_err("不匹配"));

    assert!(message.contains("more"), "超出上限要留计数,不能悄悄丢掉:{message}");
    // 按字符截断而不是字节:标题常含中文,切在多字节中间会切出非法 UTF-8。
    assert!(
        message.chars().filter(|c| *c == '…').count() > 0,
        "过长的标题要截断"
    );
    assert!(message.len() < 4000, "单条诊断不能无上限增长:{} 字节", message.len());
}
