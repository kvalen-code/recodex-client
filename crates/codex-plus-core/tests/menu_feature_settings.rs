//! 1.3.8 菜单改造的设置契约:
//! - 新开关「原地复制会话」「回答大纲」默认开,能经 /settings/set 的合并路径落盘;
//! - 已下线的「模型白名单解锁」「服务模式控件」残留在老用户设置文件里时必须无害:
//!   不能让整份设置反序列化失败(load 失败会 unwrap_or_default,等于把用户所有设置清零),
//!   也不能再被当成已知键写回。
use codex_plus_core::settings::{BackendSettings, SettingsStore};
use serde_json::json;

#[test]
fn new_menu_toggles_default_on() {
    let settings = BackendSettings::default();
    assert!(settings.codex_app_session_copy);
    assert!(settings.codex_app_answer_outline);

    let value = serde_json::to_value(&settings).expect("serialize default settings");
    assert_eq!(value["codexAppSessionCopy"], json!(true));
    assert_eq!(value["codexAppAnswerOutline"], json!(true));
    assert!(value.get("codexAppModelWhitelistUnlock").is_none());
    assert!(value.get("codexAppServiceTierControls").is_none());
}

#[test]
fn new_menu_toggles_missing_from_old_json_default_on() {
    let parsed: BackendSettings = serde_json::from_value(json!({
        "codexAppPath": "",
        "enhancementsEnabled": true,
    }))
    .expect("old settings JSON should still load");
    assert!(parsed.codex_app_session_copy);
    assert!(parsed.codex_app_answer_outline);
}

#[test]
fn removed_feature_keys_in_old_json_are_ignored_not_fatal() {
    let parsed: BackendSettings = serde_json::from_value(json!({
        "codexAppModelWhitelistUnlock": true,
        "codexAppServiceTierControls": true,
        // 用户自己关掉的开关必须原样保留 —— 反序列化一旦失败会整份回落默认值
        "codexAppSessionDelete": false,
        "codexAppFastStartup": true,
    }))
    .expect("settings with removed keys must still deserialize");
    assert!(!parsed.codex_app_session_delete);
    assert!(parsed.codex_app_fast_startup);
}

#[test]
fn settings_store_persists_new_toggles_and_drops_removed_keys_from_merge() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("settings.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({
            "codexAppServiceTierControls": true,
            "codexAppSessionDelete": false,
        }))
        .unwrap(),
    )
    .unwrap();
    let store = SettingsStore::new(path.clone());

    let loaded = store.load().expect("load settings with legacy keys");
    assert!(!loaded.codex_app_session_delete, "旧文件里的真实设置不能丢");

    let updated = store
        .update(json!({
            "codexAppSessionCopy": false,
            "codexAppAnswerOutline": false,
            "codexAppModelWhitelistUnlock": false,
        }))
        .expect("update settings");
    assert!(!updated.codex_app_session_copy);
    assert!(!updated.codex_app_answer_outline);

    let reloaded = store.load().expect("reload");
    assert!(!reloaded.codex_app_session_copy);
    assert!(!reloaded.codex_app_answer_outline);
    assert!(!reloaded.codex_app_session_delete);

    let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(
        raw.get("codexAppModelWhitelistUnlock").is_none(),
        "已下线的键不再是已知设置,/settings/set 不该把它合并进文件"
    );
}
