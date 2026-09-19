use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::Deserialize;
use serde_json::{Map, Value};
use toml_edit::{DocumentMut, Item};

use crate::zed_remote::ZedOpenStrategy;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LaunchMode {
    #[default]
    Patch,
    Relay,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayContextSelection {
    #[serde(default)]
    pub mcp_servers: Vec<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub plugins: Vec<String>,
}

impl Default for RelayContextSelection {
    fn default() -> Self {
        Self {
            mcp_servers: Vec::new(),
            skills: Vec::new(),
            plugins: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayProfile {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing)]
    pub model: String,
    #[serde(default = "default_relay_base_url", skip_serializing)]
    pub base_url: String,
    #[serde(rename = "upstreamBaseUrl", default)]
    pub upstream_base_url: String,
    #[serde(
        default,
        skip_serializing,
        deserialize_with = "deserialize_profile_api_key"
    )]
    pub api_key: String,
    #[serde(default)]
    pub protocol: RelayProtocol,
    #[serde(rename = "relayMode", default)]
    pub relay_mode: RelayMode,
    #[serde(rename = "officialMixApiKey", default)]
    pub official_mix_api_key: bool,
    #[serde(rename = "hideOfficialUsageAlert", default)]
    pub hide_official_usage_alert: bool,
    #[serde(rename = "testModel", default)]
    pub test_model: String,
    #[serde(rename = "configContents", default)]
    pub config_contents: String,
    #[serde(rename = "authContents", default)]
    pub auth_contents: String,
    #[serde(rename = "useCommonConfig", default = "default_true")]
    pub use_common_config: bool,
    #[serde(rename = "contextSelection", default)]
    pub context_selection: RelayContextSelection,
    #[serde(rename = "contextSelectionInitialized", default)]
    pub context_selection_initialized: bool,
    #[serde(rename = "contextWindow", default)]
    pub context_window: String,
    #[serde(rename = "autoCompactLimit", default)]
    pub auto_compact_limit: String,
    #[serde(rename = "modelInsertMode", default)]
    pub model_insert_mode: RelayModelInsertMode,
    #[serde(rename = "modelList", default)]
    pub model_list: String,
    #[serde(
        rename = "modelWindows",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub model_windows: String,
    #[serde(rename = "modelVlm", default, skip_serializing_if = "String::is_empty")]
    pub model_vlm: String,
    #[serde(
        rename = "vlmApiKey",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub vlm_api_key: String,
    #[serde(rename = "vlmModel", default)]
    pub vlm_model: String,
    #[serde(rename = "vlmBaseUrl", default)]
    pub vlm_base_url: String,
    #[serde(
        rename = "userAgent",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub user_agent: String,
    #[serde(rename = "sub2apiEnabled", default)]
    pub sub2api_enabled: bool,
    #[serde(
        rename = "sub2apiMultiplier",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub sub2api_multiplier: String,
    #[serde(rename = "modelRoutes", default, skip_serializing_if = "Vec::is_empty")]
    pub model_routes: Vec<RelayModelRoute>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayModelRoute {
    pub model: String,
    #[serde(rename = "targetRelayId")]
    pub target_relay_id: String,
    #[serde(
        rename = "targetModel",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub target_model: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum AggregateRelayStrategy {
    #[default]
    Failover,
    ConversationRoundRobin,
    RequestRoundRobin,
    WeightedRoundRobin,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AggregateRelayMember {
    #[serde(rename = "relayId")]
    pub relay_id: String,
    #[serde(default = "default_aggregate_member_weight")]
    pub weight: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AggregateRelayProfile {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub strategy: AggregateRelayStrategy,
    #[serde(default)]
    pub members: Vec<AggregateRelayMember>,
}

impl Default for RelayProfile {
    fn default() -> Self {
        Self {
            id: "default".to_string(),
            name: "默认中转".to_string(),
            model: String::new(),
            base_url: default_relay_base_url(),
            upstream_base_url: String::new(),
            api_key: String::new(),
            protocol: RelayProtocol::Responses,
            relay_mode: RelayMode::Official,
            official_mix_api_key: false,
            hide_official_usage_alert: false,
            test_model: String::new(),
            config_contents: String::new(),
            auth_contents: String::new(),
            use_common_config: true,
            context_selection: RelayContextSelection::default(),
            context_selection_initialized: false,
            context_window: String::new(),
            auto_compact_limit: String::new(),
            model_insert_mode: RelayModelInsertMode::Patch,
            model_list: String::new(),
            model_windows: String::new(),
            model_vlm: String::new(),
            vlm_api_key: String::new(),
            vlm_model: String::new(),
            vlm_base_url: String::new(),
            user_agent: String::new(),
            sub2api_enabled: false,
            sub2api_multiplier: String::new(),
            model_routes: Vec::new(),
        }
    }
}

impl RelayProfile {
    pub fn has_model_routes(&self) -> bool {
        self.model_routes
            .iter()
            .any(|route| !route.model.trim().is_empty() && !route.target_relay_id.trim().is_empty())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum RelayModelInsertMode {
    ModelCatalog,
    #[default]
    Patch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum RelayProtocol {
    #[default]
    Responses,
    ChatCompletions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum RelayMode {
    Official,
    #[default]
    MixedApi,
    PureApi,
    Aggregate,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BackendSettings {
    #[serde(rename = "recodexAutoSwitchOrg", default)] // recodex-overlay:auto-switch-field
    pub recodex_auto_switch_org: bool,
    #[serde(rename = "codexAppPath", default)]
    pub codex_app_path: String,
    #[serde(rename = "codexExtraArgs", default)]
    pub codex_extra_args: Vec<String>,
    #[serde(rename = "providerSyncEnabled", default)]
    pub provider_sync_enabled: bool,
    #[serde(rename = "providerSyncSavedProviders", default)]
    pub provider_sync_saved_providers: Vec<String>,
    #[serde(rename = "providerSyncManualProviders", default)]
    pub provider_sync_manual_providers: Vec<String>,
    #[serde(rename = "providerSyncLastSelectedProvider", default)]
    pub provider_sync_last_selected_provider: String,
    #[serde(rename = "relayProfilesEnabled", default = "default_true")]
    pub relay_profiles_enabled: bool,
    #[serde(rename = "enhancementsEnabled", default = "default_true")]
    pub enhancements_enabled: bool,
    #[serde(rename = "codexAppPluginMarketplaceUnlock", default = "default_true")]
    pub codex_app_plugin_marketplace_unlock: bool,
    // recodex-overlay: 模型白名单解锁(codexAppModelWhitelistUnlock)与服务模式控件
    // (codexAppServiceTierControls)1.3.8 起永久下线。老用户设置文件里残留的这两个键
    // 反序列化时直接忽略(本结构不拒绝未知字段),写回时自然消失。
    #[serde(rename = "codexAppSessionDelete", default = "default_true")]
    pub codex_app_session_delete: bool,
    #[serde(rename = "codexAppMarkdownExport", default = "default_true")]
    pub codex_app_markdown_export: bool,
    // 会话「更多」菜单里的「原地复制会话」(借官方的「从这里创建聊天分支」)
    #[serde(rename = "codexAppSessionCopy", default = "default_true")]
    pub codex_app_session_copy: bool,
    // 最新回答有 ≥2 个标题时,对话区右上角的「回答大纲」
    #[serde(rename = "codexAppAnswerOutline", default = "default_true")]
    pub codex_app_answer_outline: bool,
    #[serde(rename = "codexAppPasteFix", default)]
    pub codex_app_paste_fix: bool,
    #[serde(rename = "codexAppForceChineseLocale", default = "default_true")]
    pub codex_app_force_chinese_locale: bool,
    #[serde(rename = "codexAppFastStartup", default)]
    pub codex_app_fast_startup: bool,
    #[serde(rename = "codexAppThreadIdBadge", default)]
    pub codex_app_thread_id_badge: bool,
    #[serde(rename = "codexAppConversationView", default)]
    pub codex_app_conversation_view: bool,
    #[serde(rename = "codexAppThreadScrollRestore", default = "default_true")]
    pub codex_app_thread_scroll_restore: bool,
    #[serde(rename = "codexAppZedRemoteOpen", default = "default_true")]
    pub codex_app_zed_remote_open: bool,
    #[serde(rename = "zedRemoteOpenStrategy", default)]
    pub zed_remote_open_strategy: ZedOpenStrategy,
    #[serde(rename = "zedRemoteProjectRegistryEnabled", default = "default_true")]
    pub zed_remote_project_registry_enabled: bool,
    #[serde(rename = "zedRemoteSyncToZedSettings", default)]
    pub zed_remote_sync_to_zed_settings: bool,
    #[serde(rename = "codexAppUpstreamWorktreeCreate", default = "default_true")]
    pub codex_app_upstream_worktree_create: bool,
    #[serde(rename = "codexAppNativeMenuPlacement", default = "default_true")]
    pub codex_app_native_menu_placement: bool,
    // 默认关。`--inspect` 走的是 Electron 的 Node inspector,而 OpenAI 打包时烧了
    // fuse `EnableNodeCliInspectArguments = 0` —— Codex 152 起这条路**永远不通**,
    // 参数被静默忽略、端口永不监听(实证见 native_menu.rs 开头那段)。
    //
    // 开关留着是为了 fuse 万一放开,但默认必须是关的:开着的唯一效果,是给一个
    // 处理用户凭据的应用多传一个 `--inspect`,而换不回任何功能。
    #[serde(rename = "codexAppNativeMenuLocalization", default)]
    pub codex_app_native_menu_localization: bool,
    #[serde(rename = "codexAppPetRealMouseLook", default)]
    pub codex_app_pet_real_mouse_look: bool,
    // recodex-overlay: Stepwise 已下线,10 个配置项一并移除
    #[serde(rename = "codexAppImageOverlayEnabled", default)]
    pub codex_app_image_overlay_enabled: bool,
    #[serde(rename = "codexAppImageOverlayPath", default)]
    pub codex_app_image_overlay_path: String,
    #[serde(
        rename = "codexAppImageOverlayOpacity",
        default = "default_image_overlay_opacity",
        deserialize_with = "deserialize_image_overlay_opacity"
    )]
    pub codex_app_image_overlay_opacity: u8,
    #[serde(
        rename = "codexAppImageOverlayFitMode",
        default = "default_image_overlay_fit_mode",
        deserialize_with = "deserialize_image_overlay_fit_mode"
    )]
    pub codex_app_image_overlay_fit_mode: String,
    #[serde(rename = "codexGoalsEnabled", default)]
    pub codex_goals_enabled: bool,
    // recodex-overlay: 手机远程「跟随账号自动连接」开关(phone_remote)。开 = 启动时自动接入手机
    // + 开机自启;只经 /remote/* 桥写,不进 merge_known_setting_fields(/settings/set 改不了它)。
    #[serde(rename = "phoneRemoteFollowAccount", default)]
    pub phone_remote_follow_account: bool,
    #[serde(rename = "weixinConnectEnabled", default)]
    pub weixin_connect_enabled: bool,
    #[serde(
        rename = "weixinConnectBaseUrl",
        default = "default_weixin_connect_base_url"
    )]
    pub weixin_connect_base_url: String,
    #[serde(rename = "weixinConnectToken", default)]
    pub weixin_connect_token: String,
    #[serde(rename = "weixinConnectAccountId", default)]
    pub weixin_connect_account_id: String,
    #[serde(rename = "weixinConnectAllowFrom", default)]
    pub weixin_connect_allow_from: String,
    #[serde(rename = "weixinConnectRouteTag", default)]
    pub weixin_connect_route_tag: String,
    #[serde(rename = "weixinConnectWorkDir", default)]
    pub weixin_connect_work_dir: String,
    #[serde(rename = "weixinConnectModel", default)]
    pub weixin_connect_model: String,
    #[serde(
        rename = "weixinConnectSandbox",
        default = "default_weixin_connect_sandbox"
    )]
    pub weixin_connect_sandbox: String,
    #[serde(rename = "weixinConnectCodexPath", default)]
    pub weixin_connect_codex_path: String,
    #[serde(rename = "launchMode", default)]
    pub launch_mode: LaunchMode,
    #[serde(rename = "relayBaseUrl", default = "default_relay_base_url")]
    pub relay_base_url: String,
    #[serde(rename = "relayApiKey", default)]
    pub relay_api_key: String,
    #[serde(rename = "relayProfiles", default = "default_relay_profiles")]
    pub relay_profiles: Vec<RelayProfile>,
    #[serde(rename = "relayCommonConfigContents", default)]
    pub relay_common_config_contents: String,
    #[serde(rename = "relayContextConfigContents", default)]
    pub relay_context_config_contents: String,
    #[serde(rename = "activeRelayId", default = "default_active_relay_id")]
    pub active_relay_id: String,
    #[serde(rename = "aggregateRelayProfiles", default)]
    pub aggregate_relay_profiles: Vec<AggregateRelayProfile>,
    #[serde(rename = "activeAggregateRelayId", default)]
    pub active_aggregate_relay_id: String,
    #[serde(rename = "relayTestModel", default = "default_relay_test_model")]
    pub relay_test_model: String,
    /// recodex-overlay: 上游 Codex++ 遗留设置的一次性清理已做过(见
    /// `SettingsStore::sanitize_legacy_upstream_settings_once`)。必须是正式字段:
    /// 别的写入方走 `save()` 会按结构体重新序列化,不认识的键会被丢掉,标记一丢
    /// 就会再清一次 —— 把用户之后自己建的东西也清掉。
    #[serde(rename = "recodexLegacySettingsSanitized", default)]
    pub recodex_legacy_settings_sanitized: bool,
}

impl Default for BackendSettings {
    fn default() -> Self {
        Self {
            recodex_auto_switch_org: false, // recodex-overlay:auto-switch-default
            codex_app_path: String::new(),
            codex_extra_args: Vec::new(),
            provider_sync_enabled: false,
            provider_sync_saved_providers: Vec::new(),
            provider_sync_manual_providers: Vec::new(),
            provider_sync_last_selected_provider: String::new(),
            relay_profiles_enabled: true,
            enhancements_enabled: true,
            codex_app_plugin_marketplace_unlock: true,
            codex_app_session_delete: true,
            codex_app_markdown_export: true,
            codex_app_session_copy: true,
            codex_app_answer_outline: true,
            codex_app_paste_fix: false,
            codex_app_force_chinese_locale: true,
            codex_app_fast_startup: false,
            codex_app_thread_id_badge: false,
            codex_app_conversation_view: false,
            codex_app_thread_scroll_restore: true,
            codex_app_zed_remote_open: true,
            zed_remote_open_strategy: ZedOpenStrategy::AddToFocusedWorkspace,
            zed_remote_project_registry_enabled: true,
            zed_remote_sync_to_zed_settings: false,
            codex_app_upstream_worktree_create: true,
            codex_app_native_menu_placement: true,
            // fuse 关死了这条路,默认不开(理由见字段上的注释)
            codex_app_native_menu_localization: false,
            codex_app_pet_real_mouse_look: false,
            codex_app_image_overlay_enabled: false,
            codex_app_image_overlay_path: String::new(),
            codex_app_image_overlay_opacity: default_image_overlay_opacity(),
            codex_app_image_overlay_fit_mode: default_image_overlay_fit_mode(),
            codex_goals_enabled: false,
            phone_remote_follow_account: false,
            weixin_connect_enabled: false,
            weixin_connect_base_url: default_weixin_connect_base_url(),
            weixin_connect_token: String::new(),
            weixin_connect_account_id: String::new(),
            weixin_connect_allow_from: String::new(),
            weixin_connect_route_tag: String::new(),
            weixin_connect_work_dir: String::new(),
            weixin_connect_model: String::new(),
            weixin_connect_sandbox: default_weixin_connect_sandbox(),
            weixin_connect_codex_path: String::new(),
            launch_mode: LaunchMode::Patch,
            relay_base_url: default_relay_base_url(),
            relay_api_key: String::new(),
            relay_profiles: default_relay_profiles(),
            relay_common_config_contents: String::new(),
            relay_context_config_contents: String::new(),
            active_relay_id: default_active_relay_id(),
            aggregate_relay_profiles: Vec::new(),
            active_aggregate_relay_id: String::new(),
            relay_test_model: default_relay_test_model(),
            recodex_legacy_settings_sanitized: false,
        }
    }
}

impl BackendSettings {
    pub fn active_relay_profile(&self) -> RelayProfile {
        if self.active_relay_id == default_active_relay_id()
            && self.relay_profiles.len() == 1
            && self.relay_profiles[0] == RelayProfile::default()
            && (!self.relay_api_key.is_empty() || self.relay_base_url != default_relay_base_url())
        {
            return RelayProfile {
                id: default_active_relay_id(),
                name: "默认中转".to_string(),
                model: String::new(),
                base_url: if self.relay_base_url.is_empty() {
                    default_relay_base_url()
                } else {
                    self.relay_base_url.clone()
                },
                upstream_base_url: if self.relay_base_url.is_empty() {
                    default_relay_base_url()
                } else {
                    self.relay_base_url.clone()
                },
                api_key: self.relay_api_key.clone(),
                protocol: RelayProtocol::Responses,
                relay_mode: RelayMode::MixedApi,
                official_mix_api_key: true,
                hide_official_usage_alert: false,
                test_model: String::new(),
                config_contents: String::new(),
                auth_contents: String::new(),
                use_common_config: true,
                context_selection: RelayContextSelection::default(),
                context_selection_initialized: false,
                context_window: String::new(),
                auto_compact_limit: String::new(),
                model_insert_mode: RelayModelInsertMode::Patch,
                model_list: String::new(),
                model_windows: String::new(),
                model_vlm: String::new(),
                vlm_api_key: String::new(),
                vlm_model: String::new(),
                vlm_base_url: String::new(),
                user_agent: String::new(),
                sub2api_enabled: false,
                sub2api_multiplier: String::new(),
                model_routes: Vec::new(),
            };
        }

        if let Some(profile) = self
            .relay_profiles
            .iter()
            .find(|profile| profile.id == self.active_relay_id)
        {
            return profile.clone();
        }

        RelayProfile {
            id: if self.active_relay_id.is_empty() {
                default_active_relay_id()
            } else {
                self.active_relay_id.clone()
            },
            name: "默认中转".to_string(),
            model: String::new(),
            base_url: if self.relay_base_url.is_empty() {
                default_relay_base_url()
            } else {
                self.relay_base_url.clone()
            },
            upstream_base_url: if self.relay_base_url.is_empty() {
                default_relay_base_url()
            } else {
                self.relay_base_url.clone()
            },
            api_key: self.relay_api_key.clone(),
            protocol: RelayProtocol::Responses,
            relay_mode: RelayMode::Official,
            official_mix_api_key: false,
            hide_official_usage_alert: false,
            test_model: String::new(),
            config_contents: String::new(),
            auth_contents: String::new(),
            use_common_config: true,
            context_selection: RelayContextSelection::default(),
            context_selection_initialized: false,
            context_window: String::new(),
            auto_compact_limit: String::new(),
            model_insert_mode: RelayModelInsertMode::Patch,
            model_list: String::new(),
            model_windows: String::new(),
            model_vlm: String::new(),
            vlm_api_key: String::new(),
            vlm_model: String::new(),
            vlm_base_url: String::new(),
            user_agent: String::new(),
            sub2api_enabled: false,
            sub2api_multiplier: String::new(),
            model_routes: Vec::new(),
        }
    }

    pub fn active_aggregate_relay_profile(&self) -> Option<AggregateRelayProfile> {
        let active_relay = self
            .relay_profiles
            .iter()
            .find(|profile| profile.id == self.active_relay_id)?;
        if active_relay.relay_mode != RelayMode::Aggregate {
            return None;
        }

        let active_aggregate_id = if self.active_aggregate_relay_id.trim().is_empty() {
            active_relay.id.as_str()
        } else {
            self.active_aggregate_relay_id.trim()
        };

        if active_aggregate_id != active_relay.id {
            return None;
        }

        self.aggregate_relay_profiles
            .iter()
            .find(|profile| profile.id == active_aggregate_id)
            .cloned()
    }

    pub fn active_relay_uses_protocol_proxy(&self) -> bool {
        self.active_aggregate_relay_profile().is_some()
            || self.active_relay_profile().protocol == RelayProtocol::ChatCompletions
            || self.active_relay_profile().has_model_routes()
    }
}

fn default_image_overlay_opacity() -> u8 {
    35
}

fn clamp_image_overlay_opacity(value: u8) -> u8 {
    value.clamp(1, 100)
}

pub fn default_image_overlay_fit_mode() -> String {
    "fit".to_string()
}

fn normalize_image_overlay_fit_mode(value: &str) -> String {
    match value {
        "fill" | "fit" | "stretch" | "tile" | "center" => value.to_string(),
        _ => default_image_overlay_fit_mode(),
    }
}

pub fn default_true() -> bool {
    true
}

pub fn default_relay_base_url() -> String {
    String::new()
}

fn default_weixin_connect_base_url() -> String {
    crate::connect::DEFAULT_WEIXIN_BASE_URL.to_string()
}

fn default_weixin_connect_sandbox() -> String {
    "read-only".to_string()
}

pub fn default_active_relay_id() -> String {
    "default".to_string()
}

pub fn default_relay_test_model() -> String {
    "gpt-5.4-mini".to_string()
}

pub fn default_relay_profiles() -> Vec<RelayProfile> {
    vec![RelayProfile::default()]
}

pub fn default_aggregate_member_weight() -> u32 {
    1
}

fn deserialize_image_overlay_opacity<'de, D>(deserializer: D) -> Result<u8, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<u8>::deserialize(deserializer)?
        .map(clamp_image_overlay_opacity)
        .unwrap_or_else(default_image_overlay_opacity))
}

fn deserialize_image_overlay_fit_mode<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?
        .map(|value| normalize_image_overlay_fit_mode(&value))
        .unwrap_or_else(default_image_overlay_fit_mode))
}

fn deserialize_profile_api_key<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

pub fn normalize_codex_extra_args(args: &[String]) -> Vec<String> {
    args.iter()
        .map(|arg| arg.trim())
        .filter(|arg| !arg.is_empty())
        .map(ToString::to_string)
        .collect()
}

#[derive(Debug, Clone)]
pub struct SettingsStore {
    path: PathBuf,
}

impl Default for SettingsStore {
    fn default() -> Self {
        Self::new(crate::paths::default_settings_path())
    }
}

impl SettingsStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load(&self) -> anyhow::Result<BackendSettings> {
        let contents = match fs::read_to_string(&self.path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(BackendSettings::default());
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read settings {}", self.path.display()));
            }
        };

        Ok(normalize_settings_config_sections(
            serde_json::from_str(&contents).unwrap_or_default(),
        ))
    }

    pub fn save(&self, settings: &BackendSettings) -> anyhow::Result<()> {
        let mut settings = normalize_settings_config_sections(settings.clone());
        settings.codex_extra_args = normalize_codex_extra_args(&settings.codex_extra_args);
        let bytes = serde_json::to_vec_pretty(&settings)?;
        atomic_write(&self.path, &bytes)
    }

    pub fn update(&self, payload: Value) -> anyhow::Result<BackendSettings> {
        let Value::Object(payload) = payload else {
            return self.load();
        };

        let mut raw = self.load_raw_object()?;
        merge_known_setting_fields(&mut raw, &payload);
        let settings = normalize_settings_config_sections(
            serde_json::from_value(Value::Object(raw.clone())).unwrap_or_default(),
        );
        raw.insert(
            "relayCommonConfigContents".to_string(),
            Value::String(settings.relay_common_config_contents.clone()),
        );
        raw.insert(
            "relayContextConfigContents".to_string(),
            Value::String(settings.relay_context_config_contents.clone()),
        );
        let bytes = serde_json::to_vec_pretty(&Value::Object(raw))?;
        atomic_write(&self.path, &bytes)?;
        Ok(settings)
    }

    fn load_raw_object(&self) -> anyhow::Result<Map<String, Value>> {
        let contents = match fs::read_to_string(&self.path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(settings_to_object(&BackendSettings::default()));
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read settings {}", self.path.display()));
            }
        };

        match serde_json::from_str::<Value>(&contents) {
            Ok(Value::Object(map)) => Ok(map),
            Ok(_) | Err(_) => Ok(settings_to_object(&BackendSettings::default())),
        }
    }
}

/// 一次性清理上游遗留设置的结果。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LegacySettingsSanitizeReport {
    pub provider_sync_disabled: bool,
    pub dropped_relay_profiles: Vec<String>,
    pub dropped_aggregate_profiles: usize,
    pub active_relay_reset: bool,
    /// 顶层遗留字段 `relayBaseUrl` / `relayApiKey` 被清掉了。
    pub legacy_relay_fields_cleared: bool,
    pub backup: Option<PathBuf>,
}

impl LegacySettingsSanitizeReport {
    pub fn changed(&self) -> bool {
        self.provider_sync_disabled
            || !self.dropped_relay_profiles.is_empty()
            || self.dropped_aggregate_profiles > 0
            || self.active_relay_reset
            || self.legacy_relay_fields_cleared
    }
}

const LEGACY_SANITIZED_KEY: &str = "recodexLegacySettingsSanitized";

impl SettingsStore {
    /// 从上游 Codex++ 数据目录(`~/.codex-session-delete`)整体搬过来的设置里,
    /// 有两类东西在 ReCodex 里是**有害**的,一次性清掉:
    ///
    ///   - `providerSyncEnabled = true`:每次启动跑上游的 provider_sync,改写会话库里
    ///     的 provider 归属 —— ReCodex 的托管配置自己管 provider,两边会互相打架;
    ///   - 中转 / 聚合 / chat 协议(以及「官方 + 混入 API Key」)的 relay 配置:启动时
    ///     按它改写 config.toml,并拉起一个没人用的协议代理占着 57321。ReCodex 面板里
    ///     没有这些设置的入口,用户看不见也关不掉。
    ///
    /// ReCodex 自己的安装不会产生这两类状态(面板不暴露这些开关),所以对「已经搬过、
    /// 还没清过」的设置同样适用:以 `recodexLegacySettingsSanitized` 为标记只做一次。
    /// 动手前把原文件备份成 `settings.json.pre-recodex-legacy-sanitize.bak`(已有不覆盖)。
    /// 文件不存在(全新安装)时什么都不做,也不写标记。
    pub fn sanitize_legacy_upstream_settings_once(
        &self,
    ) -> anyhow::Result<Option<LegacySettingsSanitizeReport>> {
        let contents = match fs::read_to_string(&self.path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to read settings {}", self.path.display()));
            }
        };
        // 读不成对象的文件 load() 会当默认值处理;这里不去碰它。
        let Ok(Value::Object(mut raw)) = serde_json::from_str::<Value>(&contents) else {
            return Ok(None);
        };
        if raw.get(LEGACY_SANITIZED_KEY).and_then(Value::as_bool) == Some(true) {
            return Ok(None);
        }
        let mut report = sanitize_legacy_upstream_settings_object(&mut raw);
        raw.insert(LEGACY_SANITIZED_KEY.to_string(), Value::Bool(true));
        if report.changed() {
            let backup = PathBuf::from(format!(
                "{}.pre-recodex-legacy-sanitize.bak",
                self.path.display()
            ));
            if !backup.exists() {
                atomic_write(&backup, contents.as_bytes())?;
            }
            report.backup = Some(backup);
        }
        let bytes = serde_json::to_vec_pretty(&Value::Object(raw))?;
        atomic_write(&self.path, &bytes)?;
        Ok(Some(report))
    }
}

/// 纯数据版本,便于测试。只改需要改的键,其余原样保留(包括本结构体不认识的键)。
pub fn sanitize_legacy_upstream_settings_object(
    raw: &mut Map<String, Value>,
) -> LegacySettingsSanitizeReport {
    let mut report = LegacySettingsSanitizeReport::default();
    if raw.get("providerSyncEnabled").and_then(Value::as_bool) == Some(true) {
        raw.insert("providerSyncEnabled".to_string(), Value::Bool(false));
        report.provider_sync_disabled = true;
    }

    if let Some(Value::Array(profiles)) = raw.get_mut("relayProfiles") {
        let mut kept = Vec::with_capacity(profiles.len());
        for profile in profiles.drain(..) {
            if is_plain_official_relay_profile(&profile) {
                kept.push(profile);
            } else {
                report.dropped_relay_profiles.push(
                    profile
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                );
            }
        }
        *profiles = kept;
    }
    if !report.dropped_relay_profiles.is_empty() {
        let remaining_ids = raw
            .get("relayProfiles")
            .and_then(Value::as_array)
            .map(|profiles| {
                profiles
                    .iter()
                    .filter_map(|profile| profile.get("id").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if remaining_ids.is_empty() {
            // 全删光了就交回默认值(一条「官方」配置),别留一个空数组。
            raw.remove("relayProfiles");
        }
        let active = raw
            .get("activeRelayId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !remaining_ids.contains(&active) {
            match remaining_ids.first() {
                Some(first) => {
                    raw.insert("activeRelayId".to_string(), Value::String(first.clone()));
                }
                None => {
                    raw.remove("activeRelayId");
                }
            }
            report.active_relay_reset = true;
        }
    }

    if let Some(Value::Array(aggregates)) = raw.get("aggregateRelayProfiles") {
        report.dropped_aggregate_profiles = aggregates.len();
    }
    if report.dropped_aggregate_profiles > 0 {
        raw.insert("aggregateRelayProfiles".to_string(), Value::Array(Vec::new()));
    }
    // 顶层的 relayBaseUrl / relayApiKey 是 relayProfiles 出现之前的旧存法。只清
    // relayProfiles 不够:profiles 回到默认那一条之后,`active_relay_profile()` 的
    // 兼容分支看到这两个字段非默认,会用它们**重新造出**一条 MixedApi + 混入 Key
    // 的中转配置 —— 等于什么都没清。一并删掉,交回默认值。
    let legacy_base_url = raw
        .get("relayBaseUrl")
        .and_then(Value::as_str)
        .is_some_and(|url| !url.is_empty() && url != default_relay_base_url());
    let legacy_api_key = raw
        .get("relayApiKey")
        .and_then(Value::as_str)
        .is_some_and(|key| !key.is_empty());
    if legacy_base_url || legacy_api_key {
        report.legacy_relay_fields_cleared = true;
    }
    // 值是默认/空的也删:无害,只是让文件干净;算不算「改动」看上面的判断。
    raw.remove("relayBaseUrl");
    raw.remove("relayApiKey");
    if raw
        .get("activeAggregateRelayId")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.is_empty())
    {
        raw.insert(
            "activeAggregateRelayId".to_string(),
            Value::String(String::new()),
        );
        report.active_relay_reset = true;
    }
    report
}

/// ReCodex 里唯一「无害」的 relay 配置:官方模式、Responses 协议、不混入 API Key、
/// 没有按模型分流。其余都是上游 Codex++ 的中转玩法。注意 relayMode 缺省是
/// mixedApi(与 `RelayMode::default()` 一致),缺字段不能当成官方。
fn is_plain_official_relay_profile(profile: &Value) -> bool {
    let relay_mode = profile
        .get("relayMode")
        .and_then(Value::as_str)
        .unwrap_or("mixedApi");
    let protocol = profile
        .get("protocol")
        .and_then(Value::as_str)
        .unwrap_or("responses");
    let mix_api_key = profile
        .get("officialMixApiKey")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let has_routes = profile
        .get("modelRoutes")
        .and_then(Value::as_array)
        .is_some_and(|routes| !routes.is_empty());
    relay_mode == "official" && protocol == "responses" && !mix_api_key && !has_routes
}

fn merge_known_setting_fields(target: &mut Map<String, Value>, source: &Map<String, Value>) {
    target.remove("codexAppPluginAutoExpand");
    target.remove("computerUseGuardEnabled");
    if let Some(value) = source.get("codexAppPath").and_then(Value::as_str) {
        target.insert("codexAppPath".to_string(), Value::String(value.to_string()));
    }
    if let Some(value) = source.get("codexExtraArgs").and_then(Value::as_array) {
        let args = value
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        target.insert(
            "codexExtraArgs".to_string(),
            Value::Array(
                normalize_codex_extra_args(&args)
                    .into_iter()
                    .map(Value::String)
                    .collect(),
            ),
        );
    }
    if let Some(value) = source.get("providerSyncEnabled").and_then(Value::as_bool) {
        target.insert("providerSyncEnabled".to_string(), Value::Bool(value));
    }
    if let Some(value) = source.get("relayProfilesEnabled").and_then(Value::as_bool) {
        target.insert("relayProfilesEnabled".to_string(), Value::Bool(value));
    }
    if let Some(value) = source.get("enhancementsEnabled").and_then(Value::as_bool) {
        target.insert("enhancementsEnabled".to_string(), Value::Bool(value));
    }
    merge_bool_setting(target, source, "codexAppPluginMarketplaceUnlock");
    merge_bool_setting(target, source, "codexAppSessionDelete");
    merge_bool_setting(target, source, "codexAppMarkdownExport");
    merge_bool_setting(target, source, "codexAppSessionCopy");
    merge_bool_setting(target, source, "codexAppAnswerOutline");
    merge_bool_setting(target, source, "codexAppPasteFix");
    merge_bool_setting(target, source, "codexAppForceChineseLocale");
    merge_bool_setting(target, source, "recodexAutoSwitchOrg"); // recodex-overlay:auto-switch-merge
    merge_bool_setting(target, source, "codexAppFastStartup");
    merge_bool_setting(target, source, "codexAppThreadIdBadge");
    merge_bool_setting(target, source, "codexAppConversationView");
    merge_bool_setting(target, source, "codexAppThreadScrollRestore");
    merge_bool_setting(target, source, "codexAppZedRemoteOpen");
    if let Some(value) = source.get("zedRemoteOpenStrategy") {
        if serde_json::from_value::<ZedOpenStrategy>(value.clone()).is_ok() {
            target.insert("zedRemoteOpenStrategy".to_string(), value.clone());
        }
    }
    merge_bool_setting(target, source, "zedRemoteProjectRegistryEnabled");
    merge_bool_setting(target, source, "zedRemoteSyncToZedSettings");
    merge_bool_setting(target, source, "codexAppUpstreamWorktreeCreate");
    merge_bool_setting(target, source, "codexAppNativeMenuPlacement");
    merge_bool_setting(target, source, "codexAppNativeMenuLocalization");
    merge_bool_setting(target, source, "codexAppPetRealMouseLook");

    merge_bool_setting(target, source, "codexAppImageOverlayEnabled");
    if let Some(value) = source
        .get("codexAppImageOverlayPath")
        .and_then(Value::as_str)
    {
        target.insert(
            "codexAppImageOverlayPath".to_string(),
            Value::String(value.to_string()),
        );
    }
    if let Some(value) = source
        .get("codexAppImageOverlayOpacity")
        .and_then(Value::as_u64)
        .and_then(|value| u8::try_from(value).ok())
    {
        target.insert(
            "codexAppImageOverlayOpacity".to_string(),
            Value::Number(serde_json::Number::from(clamp_image_overlay_opacity(value))),
        );
    }
    if let Some(value) = source
        .get("codexAppImageOverlayFitMode")
        .and_then(Value::as_str)
    {
        target.insert(
            "codexAppImageOverlayFitMode".to_string(),
            Value::String(normalize_image_overlay_fit_mode(value)),
        );
    }
    if let Some(value) = source.get("codexGoalsEnabled").and_then(Value::as_bool) {
        target.insert("codexGoalsEnabled".to_string(), Value::Bool(value));
    }
    merge_bool_setting(target, source, "weixinConnectEnabled");
    for key in [
        "weixinConnectBaseUrl",
        "weixinConnectToken",
        "weixinConnectAccountId",
        "weixinConnectAllowFrom",
        "weixinConnectRouteTag",
        "weixinConnectWorkDir",
        "weixinConnectModel",
        "weixinConnectSandbox",
        "weixinConnectCodexPath",
    ] {
        if let Some(value) = source.get(key).and_then(Value::as_str) {
            target.insert(key.to_string(), Value::String(value.trim().to_string()));
        }
    }
    if let Some(value) = source.get("launchMode").and_then(Value::as_str) {
        if matches!(value, "patch" | "relay") {
            target.insert("launchMode".to_string(), Value::String(value.to_string()));
        }
    }
    if let Some(value) = source.get("relayBaseUrl").and_then(Value::as_str) {
        target.insert("relayBaseUrl".to_string(), Value::String(value.to_string()));
    }
    if let Some(value) = source.get("relayApiKey").and_then(Value::as_str) {
        target.insert("relayApiKey".to_string(), Value::String(value.to_string()));
    }
    if let Some(value) = source.get("relayProfiles").and_then(Value::as_array) {
        let mut profiles = serde_json::from_value::<Vec<RelayProfile>>(Value::Array(value.clone()))
            .unwrap_or_default();
        preserve_official_mix_bearer_tokens(&mut profiles, target);
        target.insert(
            "relayProfiles".to_string(),
            serde_json::to_value(profiles).unwrap_or_else(|_| Value::Array(Vec::new())),
        );
    }
    if let Some(value) = source
        .get("relayCommonConfigContents")
        .and_then(Value::as_str)
    {
        target.insert(
            "relayCommonConfigContents".to_string(),
            Value::String(value.to_string()),
        );
    }
    if let Some(value) = source
        .get("relayContextConfigContents")
        .and_then(Value::as_str)
    {
        target.insert(
            "relayContextConfigContents".to_string(),
            Value::String(value.to_string()),
        );
    }
    if let Some(value) = source.get("activeRelayId").and_then(Value::as_str) {
        target.insert(
            "activeRelayId".to_string(),
            Value::String(value.to_string()),
        );
    }
    if let Some(value) = source
        .get("aggregateRelayProfiles")
        .and_then(Value::as_array)
    {
        target.insert(
            "aggregateRelayProfiles".to_string(),
            Value::Array(value.clone()),
        );
    }
    if let Some(value) = source.get("activeAggregateRelayId").and_then(Value::as_str) {
        target.insert(
            "activeAggregateRelayId".to_string(),
            Value::String(value.to_string()),
        );
    }
    if let Some(value) = source.get("relayTestModel").and_then(Value::as_str) {
        target.insert(
            "relayTestModel".to_string(),
            Value::String(if value.trim().is_empty() {
                default_relay_test_model()
            } else {
                value.trim().to_string()
            }),
        );
    }
}

fn merge_bool_setting(target: &mut Map<String, Value>, source: &Map<String, Value>, key: &str) {
    if let Some(value) = source.get(key).and_then(Value::as_bool) {
        target.insert(key.to_string(), Value::Bool(value));
    }
}

fn preserve_official_mix_bearer_tokens(
    profiles: &mut [RelayProfile],
    previous: &Map<String, Value>,
) {
    let previous_tokens = previous
        .get("relayProfiles")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|value| serde_json::from_value::<RelayProfile>(value.clone()).ok())
        .filter_map(|profile| {
            if profile.relay_mode != RelayMode::Official || !profile.official_mix_api_key {
                return None;
            }
            let token = experimental_bearer_token_from_config_text(&profile.config_contents)?;
            Some((profile.id, token))
        })
        .collect::<HashMap<_, _>>();

    for profile in profiles {
        if profile.relay_mode != RelayMode::Official || !profile.official_mix_api_key {
            continue;
        }
        if experimental_bearer_token_from_config_text(&profile.config_contents).is_some() {
            continue;
        }
        let token = if profile.api_key.trim().is_empty() {
            previous_tokens.get(&profile.id).cloned()
        } else {
            Some(profile.api_key.trim().to_string())
        };
        let Some(token) = token else {
            continue;
        };
        profile.config_contents =
            set_or_replace_experimental_bearer_token(&profile.config_contents, &token);
    }
}

fn set_or_replace_experimental_bearer_token(contents: &str, token: &str) -> String {
    let mut doc = parse_toml_document(contents).unwrap_or_else(|_| DocumentMut::new());
    let provider_id = active_provider_id(&doc).unwrap_or_else(|| "codex-plus-relay".to_string());
    doc["model_provider"] = toml_edit::value(provider_id.as_str());
    doc["model_providers"][provider_id.as_str()]["experimental_bearer_token"] =
        toml_edit::value(token.trim());
    ensure_text_newline(doc.to_string())
}

fn ensure_text_newline(mut value: String) -> String {
    if !value.is_empty() && !value.ends_with('\n') {
        value.push('\n');
    }
    value
}

fn experimental_bearer_token_from_config_text(contents: &str) -> Option<String> {
    let doc = parse_toml_document(contents).ok()?;
    let provider_id = active_provider_id(&doc)?;
    doc.get("model_providers")
        .and_then(Item::as_table)
        .and_then(|providers| providers.get(&provider_id))
        .and_then(Item::as_table)
        .and_then(|provider| provider.get("experimental_bearer_token"))
        .and_then(Item::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn active_provider_id(doc: &DocumentMut) -> Option<String> {
    doc.get("model_provider")
        .and_then(Item::as_str)
        .map(str::trim)
        .filter(|provider| !provider.is_empty())
        .map(ToString::to_string)
}

fn parse_toml_document(contents: &str) -> anyhow::Result<DocumentMut> {
    let contents = contents.trim_start_matches('\u{feff}');
    if contents.trim().is_empty() {
        Ok(DocumentMut::new())
    } else {
        contents
            .parse::<DocumentMut>()
            .map_err(|error| anyhow::anyhow!("config.toml TOML 解析失败：{error}"))
    }
}

fn settings_to_object(settings: &BackendSettings) -> Map<String, Value> {
    match serde_json::to_value(settings).unwrap_or_else(|_| Value::Object(Map::new())) {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

fn normalize_settings_config_sections(mut settings: BackendSettings) -> BackendSettings {
    let (common, extracted_context) =
        split_context_config_sections(&settings.relay_common_config_contents);
    let context = join_config_sections(&[
        settings.relay_context_config_contents.as_str(),
        extracted_context.as_str(),
    ]);
    settings.relay_common_config_contents = crate::relay_config::normalize_config_text(&common);
    settings.relay_context_config_contents = crate::relay_config::normalize_config_text(&context);
    for profile in &mut settings.relay_profiles {
        let _ = crate::relay_config::normalize_relay_profile_for_storage(profile);
    }
    settings.codex_app_image_overlay_opacity =
        clamp_image_overlay_opacity(settings.codex_app_image_overlay_opacity);
    settings.codex_app_image_overlay_fit_mode =
        normalize_image_overlay_fit_mode(&settings.codex_app_image_overlay_fit_mode);
    settings.weixin_connect_base_url = settings
        .weixin_connect_base_url
        .trim()
        .trim_end_matches('/')
        .to_string();
    if settings.weixin_connect_base_url.is_empty() {
        settings.weixin_connect_base_url = default_weixin_connect_base_url();
    }
    settings.weixin_connect_token = settings.weixin_connect_token.trim().to_string();
    settings.weixin_connect_account_id = settings.weixin_connect_account_id.trim().to_string();
    settings.weixin_connect_allow_from = settings.weixin_connect_allow_from.trim().to_string();
    settings.weixin_connect_route_tag = settings.weixin_connect_route_tag.trim().to_string();
    settings.weixin_connect_work_dir = settings.weixin_connect_work_dir.trim().to_string();
    settings.weixin_connect_model = settings.weixin_connect_model.trim().to_string();
    settings.weixin_connect_sandbox = match settings.weixin_connect_sandbox.trim() {
        "workspace-write" => "workspace-write",
        "danger-full-access" => "danger-full-access",
        _ => "read-only",
    }
    .to_string();
    settings.weixin_connect_codex_path = settings.weixin_connect_codex_path.trim().to_string();
    settings
}

fn split_context_config_sections(config: &str) -> (String, String) {
    let mut common = Vec::new();
    let mut context = Vec::new();
    let mut in_context_table = false;

    for line in config.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_context_table = is_context_table_header(trimmed);
        }
        if in_context_table {
            context.push(line);
        } else {
            common.push(line);
        }
    }

    (
        normalize_text_config(common.join("\n")),
        normalize_text_config(context.join("\n")),
    )
}

fn is_context_table_header(header: &str) -> bool {
    header.starts_with("[mcp_servers.")
        || header.starts_with("[skills.")
        || header.starts_with("[plugins.")
}

fn join_config_sections(sections: &[&str]) -> String {
    let joined = sections
        .iter()
        .map(|section| section.trim())
        .filter(|section| !section.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    normalize_text_config(joined)
}

fn normalize_text_config(contents: String) -> String {
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}\n")
    }
}

/// 这份内容里有没有明文长期凭据。
///
/// 认两种形态，因为这个 crate 的 atomic_write 同时写 config.toml 和 auth.json：
///   - TOML：`experimental_bearer_token = …`（托管块内联密钥后的形态）
///   - JSON：`"OPENAI_API_KEY"`（auth.json）
///
/// recodex-integration 那边的 write_atomic 只认 TOML 一种，auth.json 靠调用方
/// 显式传 secret。这里两种都认，因为本函数的调用方（relay_config）两种文件都写，
/// 而且它没有 secret 参数可传。
fn contains_plaintext_credential(bytes: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return false;
    };
    text.lines().any(|line| {
        let t = line.trim_start();
        t.strip_prefix("experimental_bearer_token")
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    }) || text.contains("\"OPENAI_API_KEY\"")
}

/// 原子写。内容里含明文长期凭据时落 0600，否则沿用默认权限。
///
/// ⚠️ 这条路**目前还没接进启动流程**（`apply_active_relay_profile` 是 LaunchHooks
/// 的实现，但 run_launch 里没有调它）。修在这里是因为：
///   - 它写的是 `~/.codex/{config.toml,auth.json}`，与 recodex-integration
///     写的是**同一批文件**，而那边已经收到 0600；
///   - 谁哪天把这个 hook 接进启动流程，0600 会被每次启动重置回 0644，
///     而那时没人会想到是这里 —— 潜伏 bug 比现行 bug 更贵。
///
/// 与 recodex-integration 的 `write_atomic_mode`、Go 侧的
/// `writeFileAtomicSecretAware` 同一策略：**按要落盘的内容判**，不靠调用方记得传参。
pub fn atomic_write(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create directory {}", parent.display()))?;
    }

    let temp_path = temp_path_for(path);
    fs::write(&temp_path, bytes)
        .with_context(|| format!("failed to write temp file {}", temp_path.display()))?;
    // 收紧要在 rename 之前：rename 把 tmp 的 inode 连权限一起搬过去，
    // 反过来做的话从 rename 到 chmod 之间那份明文凭据是 umask 默认权限。
    #[cfg(unix)]
    if contains_plaintext_credential(bytes) {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = fs::set_permissions(&temp_path, fs::Permissions::from_mode(0o600)) {
            let _ = fs::remove_file(&temp_path);
            return Err(error).with_context(|| {
                format!("failed to restrict permissions on {}", temp_path.display())
            });
        }
    }
    if let Err(error) = replace_file(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(error).with_context(|| {
            format!(
                "failed to replace {} with {}",
                path.display(),
                temp_path.display()
            )
        });
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(source: &Path, target: &Path) -> anyhow::Result<()> {
    fs::rename(source, target)?;
    Ok(())
}

#[cfg(windows)]
fn replace_file(source: &Path, target: &Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    use windows::core::PCWSTR;

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let target = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    unsafe {
        MoveFileExW(
            PCWSTR(source.as_ptr()),
            PCWSTR(target.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )?;
    }
    Ok(())
}

fn temp_path_for(path: &Path) -> PathBuf {
    let mut temp_path = path.to_path_buf();
    let extension = path.extension().and_then(|value| value.to_str());
    temp_path.set_extension(match extension {
        Some(extension) => format!("{extension}.tmp"),
        None => "tmp".to_string(),
    });
    temp_path
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "codex-plus-core-settings-test-{}-{}",
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn legacy_upstream_settings() -> Value {
        json!({
            "providerSyncEnabled": true,
            "relayProfilesEnabled": true,
            "activeRelayId": "relay-chat",
            "activeAggregateRelayId": "agg-1",
            "someUnknownKey": 7,
            "relayBaseUrl": "https://relay.example.test/v1",
            "relayApiKey": "sk-legacy-relay",
            "relayProfiles": [
                { "id": "default", "name": "官方", "relayMode": "official", "protocol": "responses" },
                { "id": "relay-chat", "name": "中转", "relayMode": "pureApi", "protocol": "chatCompletions" },
                { "id": "mixed-no-mode", "name": "缺字段=mixedApi" },
                { "id": "official-mix", "relayMode": "official", "officialMixApiKey": true },
                { "id": "routes", "relayMode": "official", "modelRoutes": [{ "model": "x" }] }
            ],
            "aggregateRelayProfiles": [{ "id": "agg-1", "name": "聚合", "members": [] }]
        })
    }

    #[test]
    fn legacy_upstream_settings_are_sanitized_once_with_backup() {
        let dir = temp_dir();
        let path = dir.join("settings.json");
        let original = serde_json::to_string_pretty(&legacy_upstream_settings()).unwrap();
        std::fs::write(&path, &original).unwrap();
        let store = SettingsStore::new(path.clone());

        let report = store.sanitize_legacy_upstream_settings_once().unwrap().unwrap();
        assert!(report.provider_sync_disabled);
        assert_eq!(
            report.dropped_relay_profiles,
            vec!["relay-chat", "mixed-no-mode", "official-mix", "routes"]
        );
        assert_eq!(report.dropped_aggregate_profiles, 1);
        assert!(report.active_relay_reset);
        assert!(report.legacy_relay_fields_cleared);
        let backup = report.backup.clone().unwrap();
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), original);

        let settings = store.load().unwrap();
        assert!(!settings.provider_sync_enabled);
        assert_eq!(settings.active_relay_id, "default");
        assert_eq!(settings.relay_profiles.len(), 1);
        assert!(settings.aggregate_relay_profiles.is_empty());
        assert!(settings.active_aggregate_relay_id.is_empty());
        assert!(!settings.active_relay_uses_protocol_proxy());
        // 遗留的顶层中转字段也要清掉,否则 active_relay_profile() 会拿它们
        // 重新拼出一条 MixedApi + 混入 Key 的配置。
        assert_eq!(settings.relay_base_url, default_relay_base_url());
        assert!(settings.relay_api_key.is_empty());
        let active = settings.active_relay_profile();
        assert_eq!(active.relay_mode, RelayMode::Official);
        assert!(!active.official_mix_api_key);
        assert!(active.api_key.is_empty());
        assert!(settings.recodex_legacy_settings_sanitized);
        let raw: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["someUnknownKey"], 7, "不认识的键要原样保留");

        // 标记在:之后用户自己再开的东西不会被再清一次(即便经过 save() 往返)。
        let mut again = store.load().unwrap();
        again.provider_sync_enabled = true;
        store.save(&again).unwrap();
        assert_eq!(store.sanitize_legacy_upstream_settings_once().unwrap(), None);
        assert!(store.load().unwrap().provider_sync_enabled);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn legacy_sanitize_marks_clean_settings_without_backup_and_skips_missing_file() {
        let dir = temp_dir();
        let path = dir.join("settings.json");
        let store = SettingsStore::new(path.clone());
        assert_eq!(store.sanitize_legacy_upstream_settings_once().unwrap(), None);
        assert!(!path.exists(), "全新安装不应凭空写出设置文件");

        std::fs::write(&path, r#"{"providerSyncEnabled":false,"enhancementsEnabled":true}"#)
            .unwrap();
        let report = store.sanitize_legacy_upstream_settings_once().unwrap().unwrap();
        assert!(!report.changed());
        assert!(report.backup.is_none());
        assert!(store.load().unwrap().recodex_legacy_settings_sanitized);
        assert!(!dir.join("settings.json.pre-recodex-legacy-sanitize.bak").exists());

        // 全部 relay 都是遗留的 → 回到默认的一条官方配置。
        let mut raw = legacy_upstream_settings().as_object().unwrap().clone();
        raw["relayProfiles"] = json!([{ "id": "relay-chat", "relayMode": "pureApi" }]);
        let report = sanitize_legacy_upstream_settings_object(&mut raw);
        assert_eq!(report.dropped_relay_profiles, vec!["relay-chat"]);
        assert!(!raw.contains_key("relayProfiles"));
        assert!(!raw.contains_key("activeRelayId"));
        let settings: BackendSettings = serde_json::from_value(Value::Object(raw)).unwrap();
        assert_eq!(settings.active_relay_profile().relay_mode, RelayMode::Official);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn atomic_write_replaces_existing_file_and_removes_temp_file() {
        let dir = temp_dir();
        let path = dir.join("settings.json");
        std::fs::write(&path, b"old").unwrap();

        atomic_write(&path, b"new").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert!(!dir.join("settings.json.tmp").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn settings_deserialize_ignores_removed_cli_wrapper_keys() {
        let settings: BackendSettings = serde_json::from_str(
            r#"{"codexAppPath":"C:\\Portable\\Codex\\app","providerSyncEnabled":true,"codexGoalsEnabled":true,"cliWrapperEnabled":true,"cliWrapperBaseUrl":"https://example.test","cliWrapperApiKey":"sk-test","cliWrapperApiKeyEnv":""}"#,
        )
        .unwrap();
        assert_eq!(settings.codex_app_path, r"C:\Portable\Codex\app");
        assert!(settings.provider_sync_enabled);
        assert!(settings.codex_goals_enabled);
        assert_eq!(settings.relay_base_url, default_relay_base_url());
        assert!(settings.codex_extra_args.is_empty());
        let saved = serde_json::to_value(&settings).unwrap();
        assert!(saved.get("cliWrapperEnabled").is_none());
        assert!(saved.get("cliWrapperBaseUrl").is_none());
        assert!(saved.get("cliWrapperApiKey").is_none());
        assert!(saved.get("cliWrapperApiKeyEnv").is_none());
    }

    #[test]
    fn settings_deserialize_keeps_plugin_marketplace_unlock_switch() {
        let settings: BackendSettings = serde_json::from_str(
            r#"{
                "codexAppPluginMarketplaceUnlock": true,
                "codexAppPluginAutoExpand": false
            }"#,
        )
        .unwrap();

        assert!(settings.codex_app_plugin_marketplace_unlock);
        let saved = serde_json::to_value(&settings).unwrap();
        assert!(saved.get("codexAppPluginAutoExpand").is_none());

        let legacy_settings: BackendSettings = serde_json::from_str(
            r#"{
                "codexAppForcePluginInstall": false
            }"#,
        )
        .unwrap();

        assert!(legacy_settings.codex_app_plugin_marketplace_unlock);
    }

    #[test]
    fn settings_deserialize_reads_codex_extra_args() {
        let settings: BackendSettings = serde_json::from_str(
            r#"{"codexExtraArgs":["--force_high_performance_gpu"," --ignored-trimmed-by-ui "]}"#,
        )
        .unwrap();

        assert_eq!(
            settings.codex_extra_args,
            vec![
                "--force_high_performance_gpu".to_string(),
                " --ignored-trimmed-by-ui ".to_string(),
            ]
        );
    }

    #[test]
    fn relay_profile_official_mix_api_key_defaults_to_false() {
        let profile: RelayProfile =
            serde_json::from_str(r#"{"id":"official","name":"官方","relayMode":"official"}"#)
                .unwrap();

        assert_eq!(profile.relay_mode, RelayMode::Official);
        assert!(!profile.official_mix_api_key);
        assert!(!profile.hide_official_usage_alert);
        assert!(profile.test_model.is_empty());
    }

    #[test]
    fn relay_profile_context_fields_default_to_empty() {
        let profile = RelayProfile::default();

        assert!(profile.context_selection.mcp_servers.is_empty());
        assert!(profile.context_selection.skills.is_empty());
        assert!(profile.context_selection.plugins.is_empty());
        assert!(profile.use_common_config);
        assert!(!profile.context_selection_initialized);
        assert!(profile.context_window.is_empty());
        assert!(profile.auto_compact_limit.is_empty());
        assert_eq!(profile.model_insert_mode, RelayModelInsertMode::Patch);
        assert!(profile.model_list.is_empty());
        assert!(profile.model_routes.is_empty());
        assert!(!profile.has_model_routes());
    }

    #[test]
    fn relay_profile_model_routes_roundtrip_in_camel_case() {
        let profile: RelayProfile = serde_json::from_str(
            r#"{
                "id":"relay-a",
                "name":"供应商 A",
                "modelRoutes":[{
                    "model":"gpt-5.6-luna",
                    "targetRelayId":"relay-b",
                    "targetModel":"provider-luna"
                }]
            }"#,
        )
        .unwrap();

        assert!(profile.has_model_routes());
        assert_eq!(profile.model_routes[0].model, "gpt-5.6-luna");
        assert_eq!(profile.model_routes[0].target_relay_id, "relay-b");
        assert_eq!(profile.model_routes[0].target_model, "provider-luna");

        let saved = serde_json::to_value(profile).unwrap();
        assert_eq!(saved["modelRoutes"][0]["targetRelayId"], "relay-b");
        assert_eq!(saved["modelRoutes"][0]["targetModel"], "provider-luna");
    }

    #[test]
    fn relay_profile_context_fields_deserialize_from_camel_case() {
        let profile: RelayProfile = serde_json::from_str(
            r#"{
                "id":"relay-a",
                "name":"供应商 A",
                "contextSelection":{
                    "mcpServers":["context7"],
                    "skills":["writer"],
                    "plugins":["local"]
                },
                "contextSelectionInitialized":true,
                "useCommonConfig":false,
                "contextWindow":"200000",
                "autoCompactLimit":"160000",
                "modelInsertMode":"patch",
                "modelList":"qwen3-coder\ndeepseek-coder"
            }"#,
        )
        .unwrap();

        assert_eq!(profile.context_selection.mcp_servers, vec!["context7"]);
        assert_eq!(profile.context_selection.skills, vec!["writer"]);
        assert_eq!(profile.context_selection.plugins, vec!["local"]);
        assert!(!profile.use_common_config);
        assert!(profile.context_selection_initialized);
        assert_eq!(profile.context_window, "200000");
        assert_eq!(profile.auto_compact_limit, "160000");
        assert_eq!(profile.model_insert_mode, RelayModelInsertMode::Patch);
        assert_eq!(profile.model_list, "qwen3-coder\ndeepseek-coder");
    }

    #[test]
    fn relay_profile_derived_fields_are_read_but_not_serialized() {
        let profile: RelayProfile = serde_json::from_str(
            r#"{
                "id":"relay-a",
                "name":"供应商 A",
                "model":"gpt-5.4",
                "baseUrl":"https://relay.example/v1",
                "apiKey":"sk-test",
                "configContents":"model = \"gpt-5.4\"\n",
                "authContents":"{\"OPENAI_API_KEY\":\"sk-test\"}"
            }"#,
        )
        .unwrap();

        assert_eq!(profile.model, "gpt-5.4");
        assert_eq!(profile.base_url, "https://relay.example/v1");
        assert_eq!(profile.api_key, "sk-test");

        let saved = serde_json::to_value(&profile).unwrap();
        assert!(saved.get("model").is_none());
        assert!(saved.get("baseUrl").is_none());
        assert!(saved.get("apiKey").is_none());
        assert_eq!(saved["configContents"], "model = \"gpt-5.4\"\n");
        assert_eq!(saved["authContents"], "{\"OPENAI_API_KEY\":\"sk-test\"}");
    }

    #[test]
    fn chat_protocol_profile_roundtrip_migrates_upstream_base_url_out_of_config() {
        let dir = temp_dir();
        let store = SettingsStore::new(dir.join("settings.json"));
        let settings = BackendSettings {
            relay_profiles: vec![RelayProfile {
                id: "relay-chat".to_string(),
                name: "DeepSeek".to_string(),
                protocol: RelayProtocol::ChatCompletions,
                relay_mode: RelayMode::PureApi,
                config_contents: r#"model = "deepseek-chat"
codex_plus_chat_base_url = "https://api.deepseek.com"
model_provider = "custom"

[model_providers.custom]
name = "custom"
wire_api = "responses"
requires_openai_auth = true
base_url = "http://127.0.0.1:57321/v1"
"#
                .to_string(),
                auth_contents: r#"{"OPENAI_API_KEY":"sk-test"}"#.to_string(),
                ..RelayProfile::default()
            }],
            active_relay_id: "relay-chat".to_string(),
            ..BackendSettings::default()
        };

        store.save(&settings).unwrap();
        let loaded = store.load().unwrap();
        let active = loaded.active_relay_profile();

        assert_eq!(active.protocol, RelayProtocol::ChatCompletions);
        assert_eq!(active.base_url, "https://api.deepseek.com");
        assert_eq!(active.upstream_base_url, "https://api.deepseek.com");
        assert_eq!(active.api_key, "sk-test");
        assert!(!active.config_contents.contains("codex_plus_chat_base_url"));

        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("settings.json")).unwrap())
                .unwrap();
        let profile = &saved["relayProfiles"][0];
        assert!(profile.get("baseUrl").is_none());
        assert_eq!(profile["upstreamBaseUrl"], "https://api.deepseek.com");
        assert!(profile.get("apiKey").is_none());
        assert!(
            !profile["configContents"]
                .as_str()
                .unwrap()
                .contains("codex_plus_chat_base_url")
        );
    }

    #[test]
    fn official_profile_without_mix_does_not_persist_api_config() {
        let settings = BackendSettings {
            relay_profiles: vec![RelayProfile {
                id: "official".to_string(),
                name: "官方".to_string(),
                relay_mode: RelayMode::Official,
                official_mix_api_key: false,
                hide_official_usage_alert: false,
                model: "gpt-5.5".to_string(),
                base_url: "https://relay.example/v1".to_string(),
                api_key: "sk-test".to_string(),
                config_contents: r#"model = "gpt-5.5"
model_provider = "custom"

[model_providers.custom]
requires_openai_auth = true
"#
                .to_string(),
                auth_contents: r#"{"OPENAI_API_KEY":"sk-test"}"#.to_string(),
                ..RelayProfile::default()
            }],
            active_relay_id: "official".to_string(),
            ..BackendSettings::default()
        };

        let value = settings_to_object(&normalize_settings_config_sections(settings));
        let profile = &value["relayProfiles"][0];
        assert_eq!(profile["relayMode"], "official");
        assert_eq!(profile["officialMixApiKey"], false);
        assert_eq!(profile["configContents"], "");
        assert_eq!(profile["authContents"], "");
        assert!(profile.get("model").is_none());
        assert!(profile.get("baseUrl").is_none());
        assert!(profile.get("apiKey").is_none());
    }

    #[test]
    fn official_mix_profile_keeps_key_in_config_not_auth() {
        let dir = temp_dir();
        let store = SettingsStore::new(dir.join("settings.json"));
        let settings = BackendSettings {
            relay_profiles: vec![RelayProfile {
                id: "official-mix".to_string(),
                name: "官方混入".to_string(),
                relay_mode: RelayMode::Official,
                official_mix_api_key: true,
                hide_official_usage_alert: false,
                model: "gpt-5.5".to_string(),
                base_url: "https://relay.example/v1".to_string(),
                api_key: "sk-mix".to_string(),
                config_contents: r#"model = "gpt-5.5"
model_provider = "custom"

[model_providers.custom]
requires_openai_auth = true
base_url = "https://relay.example/v1"
experimental_bearer_token = "sk-mix"
"#
                .to_string(),
                auth_contents: r#"{"OPENAI_API_KEY":"sk-mix","auth_mode":"chatgpt"}"#.to_string(),
                ..RelayProfile::default()
            }],
            active_relay_id: "official-mix".to_string(),
            ..BackendSettings::default()
        };

        store.save(&settings).unwrap();
        let loaded = store.load().unwrap();
        let profile = &loaded.relay_profiles[0];

        assert_eq!(profile.relay_mode, RelayMode::Official);
        assert!(profile.official_mix_api_key);
        assert_eq!(profile.api_key, "sk-mix");
        assert!(!profile.auth_contents.contains("OPENAI_API_KEY"));
        assert!(
            profile
                .config_contents
                .contains(r#"experimental_bearer_token = "sk-mix""#)
        );

        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("settings.json")).unwrap())
                .unwrap();
        assert!(saved["relayProfiles"][0].get("apiKey").is_none());
        assert!(
            !saved["relayProfiles"][0]["authContents"]
                .as_str()
                .unwrap()
                .contains("OPENAI_API_KEY")
        );
        assert!(
            saved["relayProfiles"][0]["configContents"]
                .as_str()
                .unwrap()
                .contains(r#"experimental_bearer_token = "sk-mix""#)
        );
    }

    #[test]
    fn settings_update_preserves_official_mix_key_when_payload_loses_it() {
        let dir = temp_dir();
        let store = SettingsStore::new(dir.join("settings.json"));
        store
            .save(&BackendSettings {
                relay_profiles: vec![RelayProfile {
                    id: "official-mix".to_string(),
                    name: "官方混入".to_string(),
                    relay_mode: RelayMode::Official,
                    official_mix_api_key: true,
                    hide_official_usage_alert: false,
                    config_contents: r#"model_provider = "custom"

[model_providers.other]
base_url = "https://other.example/v1"
experimental_bearer_token = "sk-other"

[model_providers.custom]
base_url = "https://relay.example/v1"
experimental_bearer_token = "sk-existing"
"#
                    .to_string(),
                    ..RelayProfile::default()
                }],
                active_relay_id: "official-mix".to_string(),
                ..BackendSettings::default()
            })
            .unwrap();

        let updated = store
            .update(json!({
                "relayProfiles": [{
                    "id": "official-mix",
                    "name": "官方混入",
                    "relayMode": "official",
                    "officialMixApiKey": true,
                    "configContents": "model_provider = \"custom\"\n\n[model_providers.other]\nbase_url = \"https://other.example/v1\"\nexperimental_bearer_token = \"sk-other\"\n\n[model_providers.custom]\nbase_url = \"https://relay.example/v1\"\nexperimental_bearer_token = \"\"\n",
                    "authContents": ""
                }],
                "activeRelayId": "official-mix"
            }))
            .unwrap();

        let profile = &updated.relay_profiles[0];
        assert_eq!(profile.api_key, "sk-existing");
        assert!(!profile.config_contents.contains("sk-other"));
        assert!(profile.config_contents.contains(
            r#"[model_providers.custom]
base_url = "https://relay.example/v1"
experimental_bearer_token = "sk-existing""#
        ));
    }

    #[test]
    fn official_mix_update_uses_api_key_when_config_token_missing() {
        let dir = temp_dir();
        let store = SettingsStore::new(dir.join("settings.json"));

        let updated = store
            .update(json!({
                "relayProfiles": [{
                    "id": "official-mix",
                    "name": "官方混入",
                    "relayMode": "official",
                    "officialMixApiKey": true,
                    "baseUrl": "https://relay.example/v1",
                    "apiKey": "sk-new",
                    "configContents": "model_provider = \"custom\"\n\n[model_providers.custom]\nbase_url = \"https://relay.example/v1\"\n",
                    "authContents": ""
                }],
                "activeRelayId": "official-mix"
            }))
            .unwrap();

        let profile = &updated.relay_profiles[0];
        assert_eq!(profile.api_key, "sk-new");
        assert!(
            profile
                .config_contents
                .contains(r#"experimental_bearer_token = "sk-new""#)
        );
        assert!(!profile.auth_contents.contains("OPENAI_API_KEY"));
    }

    #[test]
    fn settings_update_preserves_manual_official_mix_config_token() {
        let dir = temp_dir();
        let store = SettingsStore::new(dir.join("settings.json"));

        let updated = store
            .update(json!({
                "relayProfiles": [{
                    "id": "official-mix",
                    "name": "官方混入",
                    "relayMode": "official",
                    "officialMixApiKey": true,
                    "configContents": "model_provider = \"custom\"\n\n[model_providers.custom]\nbase_url = \"https://relay.example/v1\"\nexperimental_bearer_token = \"22222222222222222222222222222222222\"\n",
                    "authContents": ""
                }],
                "activeRelayId": "official-mix"
            }))
            .unwrap();

        let profile = &updated.relay_profiles[0];
        assert_eq!(profile.relay_mode, RelayMode::Official);
        assert!(profile.official_mix_api_key);
        assert_eq!(profile.api_key, "22222222222222222222222222222222222");
        assert!(
            profile
                .config_contents
                .contains(r#"experimental_bearer_token = "22222222222222222222222222222222222""#)
        );
        assert!(!profile.auth_contents.contains("OPENAI_API_KEY"));
    }

    #[test]
    fn settings_store_load_missing_file_returns_default() {
        let dir = temp_dir();
        let store = SettingsStore::new(dir.join("settings.json"));

        assert_eq!(store.load().unwrap(), BackendSettings::default());
    }


    /// 🔴 含明文长期凭据的内容必须落 0600。
    ///
    /// 这条路目前还没接进启动流程，但它写的是 ~/.codex/{config.toml,auth.json} ——
    /// 与 recodex-integration 写的是同一批文件，那边已经收到 0600。
    /// 谁哪天把 apply_active_relay_profile 接进启动流程，0600 会被每次启动重置回
    /// 0644，而那时没人会想到是这里。
    #[test]
    fn atomic_write_locks_down_files_containing_credentials() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();

        // TOML 形态（托管块内联密钥后）
        let toml_path = dir.join("config.toml");
        atomic_write(&toml_path, b"[p]
experimental_bearer_token = \"sk-live-x\"
").unwrap();
        // JSON 形态（auth.json）
        let json_path = dir.join("auth.json");
        atomic_write(&json_path, b"{\"OPENAI_API_KEY\":\"sk-live-x\"}
").unwrap();
        // 不含凭据的内容不必收紧
        let plain_path = dir.join("plain.toml");
        atomic_write(&plain_path, b"model = \"gpt-5.6\"
").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for p in [&toml_path, &json_path] {
                let mode = std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "{} 含明文凭据却是 {:o}", p.display(), mode);
            }
            let mode = std::fs::metadata(&plain_path).unwrap().permissions().mode() & 0o777;
            assert_ne!(mode, 0o600, "不含凭据的文件不该被无谓收紧");
        }

        // 无论平台，内容都要写对。
        assert!(std::fs::read_to_string(&toml_path).unwrap().contains("sk-live-x"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 判据要认得出两种形态，也不能把普通内容误判成凭据。
    #[test]
    fn credential_sniffer_recognises_both_shapes() {
        assert!(contains_plaintext_credential(b"experimental_bearer_token = \"sk-x\""));
        assert!(contains_plaintext_credential(b"  experimental_bearer_token='sk-x'"));
        assert!(contains_plaintext_credential(b"{\"OPENAI_API_KEY\":\"sk-x\"}"));
        // 不是键而是值里提到，不算。
        assert!(!contains_plaintext_credential(b"note = \"use experimental_bearer_token\""));
        assert!(!contains_plaintext_credential(b"model = \"gpt-5.6\""));
        assert!(!contains_plaintext_credential(b""));
    }
}
