use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::settings::{RelayProfile, SettingsStore};
use serde_json::{Map, Value, json};

const BASE_URL_ENV_KEYS: &[&str] = &[
    "CODEX_PLUS_OPENAI_BASE_URL",
    "CODEX_PLUS_BASE_URL",
    "OPENAI_BASE_URL",
    "OPENAI_API_BASE_URL",
    "OPENAI_API_BASE",
    "OPENAI_API_URL",
];
const API_KEY_ENV_KEYS: &[&str] = &[
    "CODEX_PLUS_OPENAI_API_KEY",
    "CODEX_PLUS_API_KEY",
    "OPENAI_API_KEY",
];

#[derive(Debug, Clone)]
struct ModelSource {
    source_id: String,
    source_type: String,
    name: String,
    base_url: String,
    api_key: String,
}

#[derive(Debug, Default)]
struct CodexConfig {
    root: HashMap<String, String>,
    profiles: HashMap<String, HashMap<String, String>>,
    model_providers: HashMap<String, HashMap<String, String>>,
}

pub async fn read_codex_model_catalog() -> Value {
    let home = codex_home_dir();
    let settings_path = crate::paths::default_settings_path();
    if settings_path.exists() {
        if let Ok(settings) = SettingsStore::new(settings_path).load() {
            let profile = settings.active_relay_profile();
            let catalog = relay_profile_model_catalog_value(&home, &profile);
            if catalog
                .get("models")
                .and_then(Value::as_array)
                .map_or(false, |m| !m.is_empty())
            {
                return catalog;
            }
        }
    }
    let env = std::env::vars().collect::<HashMap<_, _>>();
    let client = match crate::http_client::proxied_client("CodexPlusPlus/1.0") {
        Ok(client) => client,
        Err(error) => {
            return json!({
                "status": "failed",
                "path": home.join("config.toml").to_string_lossy(),
                "message": error.to_string(),
                "service_tier": config_service_tier_value(&home),
                "model": "",
                "model_provider": "",
                "provider_name": "",
                "default_model": "",
                "models": [],
                "modelMetadata": {},
                "sources": [],
                "responses_api": responses_api_status("unknown", "", "")
            });
        }
    };
    read_codex_model_catalog_from_home(&home, &env, client).await
}

fn relay_profile_model_catalog_value(home: &Path, profile: &RelayProfile) -> Value {
    let models = relay_profile_model_ids(profile);
    let model = profile.model.trim().to_string();
    let codex_model_provider = codex_model_provider_for_relay_profile(home, profile);
    let default_model = models.first().cloned().unwrap_or_default();
    let provider_name = if profile.name.trim().is_empty() {
        profile.id.trim()
    } else {
        profile.name.trim()
    };
    let model_count = models.len();
    let model_metadata = model_ui_metadata_map(&models);
    json!({
        "status": if models.is_empty() { "not_configured" } else { "ok" },
        "path": home.join("config.toml").to_string_lossy(),
        "service_tier": config_service_tier_value(home),
        "model": model,
        "model_provider": profile.id.trim(),
        "codex_model_provider": codex_model_provider,
        "provider_name": provider_name,
        "default_model": default_model,
        "models": models,
        "modelMetadata": model_metadata,
        "sources": [
            {
                "id": format!("relay-profile:{}", profile.id),
                "type": "relay_profile_model_list",
                "name": provider_name,
                "base_url": profile.base_url.trim(),
                "status": "ok",
                "models": model_count,
                "responses_api": responses_api_status("unknown", "", "")
            }
        ],
        "responses_api": responses_api_status("unknown", "", "")
    })
}

pub fn codex_model_provider_for_relay_profile(home: &Path, profile: &RelayProfile) -> String {
    let profile_config = parse_codex_config(&profile.config_contents);
    let profile_provider = string_value(profile_config.root.get("model_provider"));
    if !profile_provider.is_empty() {
        return profile_provider;
    }

    let (live_config, _, error) = load_codex_config(&home.join("config.toml"));
    if error.is_some() {
        return String::new();
    }
    string_value(live_config.root.get("model_provider"))
}

fn relay_profile_model_ids(profile: &RelayProfile) -> Vec<String> {
    unique_strings(
        profile
            .model_list
            .split(['\r', '\n', ','])
            .chain(std::iter::once(profile.model.as_str()))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .collect(),
    )
}

fn model_ui_metadata_map(models: &[String]) -> Value {
    let mut metadata = Map::new();
    for model in models {
        if let Some(value) = crate::model_suffix::model_ui_metadata(model) {
            metadata.insert(model.clone(), value);
        }
    }
    Value::Object(metadata)
}

pub async fn read_codex_model_catalog_from_home(
    home: &Path,
    env: &HashMap<String, String>,
    client: reqwest::Client,
) -> Value {
    let config_path = home.join("config.toml");
    let auth_api_key = read_codex_auth_api_key(&home.join("auth.json"));
    let (config, effective, error) = load_codex_config(&config_path);
    let mut model = string_value(effective.get("model"));
    let mut model_provider = string_value(effective.get("model_provider"));
    let (resolved_provider, provider_config) =
        provider_config_for_model_provider(&config, &model_provider);
    if model_provider.is_empty() && !resolved_provider.is_empty() {
        model_provider = resolved_provider;
    }
    let provider_name = provider_config
        .as_ref()
        .and_then(|provider| provider.get("name"))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| model_provider.clone());

    if let Some(error) = error.as_ref().filter(|error| *error != "missing") {
        return json!({
            "status": "failed",
            "path": config_path.to_string_lossy(),
            "message": error,
            "service_tier": service_tier_value(&effective),
            "model": model,
            "model_provider": model_provider,
            "provider_name": provider_name,
            "default_model": "",
            "models": [],
            "modelMetadata": {},
            "sources": [],
            "responses_api": responses_api_status("unknown", "", "")
        });
    }

    let mut sources = model_sources_from_environment(env, &auth_api_key);
    if error.is_none() {
        if let Some(source) = model_source_from_config(&config, &effective, env, &auth_api_key) {
            if sources
                .iter()
                .all(|existing| trim_url(&existing.base_url) != trim_url(&source.base_url))
            {
                sources.push(source);
            }
        }
    }

    let mut source_statuses = Vec::new();
    let mut models = Vec::new();
    for source in sources.iter() {
        let (source_models, mut source_status) = fetch_models_from_source(&client, source).await;
        source_status["responses_api"] = responses_api_status("unknown", "", "");
        models.extend(source_models);
        source_statuses.push(source_status);
    }
    let (catalog_models, catalog_status) = models_from_config_model_catalog_json(home, &effective);
    models.extend(catalog_models);
    if let Some(status) = catalog_status {
        source_statuses.push(status);
    }

    models = unique_strings(models);
    if model.is_empty() {
        model = string_value(effective.get("default_model"));
    }
    let default_model = if models.iter().any(|item| item == &model) {
        model.clone()
    } else {
        models.first().cloned().unwrap_or_default()
    };
    let status = if !models.is_empty() {
        "ok"
    } else if !source_statuses.is_empty()
        && source_statuses
            .iter()
            .any(|source| source.get("status").and_then(Value::as_str) == Some("failed"))
    {
        "failed"
    } else if error.as_deref() == Some("missing") {
        "missing"
    } else {
        "not_configured"
    };
    let responses_api = preferred_responses_api_status(&source_statuses);
    let model_metadata = model_ui_metadata_map(&models);

    json!({
        "status": status,
        "path": config_path.to_string_lossy(),
        "service_tier": service_tier_value(&effective),
        "model": model,
        "model_provider": model_provider,
        "provider_name": provider_name,
        "default_model": default_model,
        "models": models,
        "modelMetadata": model_metadata,
        "sources": source_statuses,
        "responses_api": responses_api
    })
}

fn codex_home_dir() -> PathBuf {
    crate::codex_home::default_codex_home_dir()
}

// 读取 config.toml（含 profile 覆盖）里生效的 service_tier；未配置时返回 null。
fn service_tier_value(effective: &HashMap<String, String>) -> Value {
    let tier = string_value(effective.get("service_tier"));
    if tier.is_empty() {
        Value::Null
    } else {
        Value::String(tier)
    }
}

fn config_service_tier_value(home: &Path) -> Value {
    let (_, effective, _) = load_codex_config(&home.join("config.toml"));
    service_tier_value(&effective)
}

fn load_codex_config(path: &Path) -> (CodexConfig, HashMap<String, String>, Option<String>) {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (
                CodexConfig::default(),
                HashMap::new(),
                Some("missing".to_string()),
            );
        }
        Err(error) => {
            return (
                CodexConfig::default(),
                HashMap::new(),
                Some(error.to_string()),
            );
        }
    };
    let config = parse_codex_config(&contents);
    let mut effective = config.root.clone();
    if let Some(profile) = config.root.get("profile") {
        if let Some(profile_values) = config.profiles.get(profile) {
            effective.extend(profile_values.clone());
        }
    }
    (config, effective, None)
}

fn parse_codex_config(contents: &str) -> CodexConfig {
    let mut config = CodexConfig::default();
    let mut section = ConfigSection::Root;
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            section = ConfigSection::from_header(trimmed.trim_matches(&['[', ']'][..]));
            continue;
        }
        let Some((key, value)) = trimmed.split_once('=') else {
            continue;
        };
        let key = key.trim().to_string();
        let value = unquote_toml_string(value);
        match &section {
            ConfigSection::Root => {
                config.root.insert(key, value);
            }
            ConfigSection::Profile(name) => {
                config
                    .profiles
                    .entry(name.clone())
                    .or_default()
                    .insert(key, value);
            }
            ConfigSection::ModelProvider(name) => {
                config
                    .model_providers
                    .entry(name.clone())
                    .or_default()
                    .insert(key, value);
            }
            ConfigSection::Other => {}
        }
    }
    config
}

#[derive(Debug, Clone)]
enum ConfigSection {
    Root,
    Profile(String),
    ModelProvider(String),
    Other,
}

impl ConfigSection {
    fn from_header(header: &str) -> Self {
        if let Some(name) = header.strip_prefix("profiles.") {
            return Self::Profile(name.trim_matches('"').to_string());
        }
        if let Some(name) = header.strip_prefix("model_providers.") {
            return Self::ModelProvider(name.trim_matches('"').to_string());
        }
        Self::Other
    }
}

fn read_codex_auth_api_key(path: &Path) -> String {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let Ok(payload) = serde_json::from_str::<Value>(&contents) else {
        return String::new();
    };
    for key in [
        "OPENAI_API_KEY",
        "api_key",
        "apikey",
        "access_token",
        "token",
    ] {
        let value = payload
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        if !value.is_empty() {
            return value.to_string();
        }
    }
    String::new()
}

fn provider_config_for_model_provider(
    config: &CodexConfig,
    model_provider: &str,
) -> (String, Option<HashMap<String, String>>) {
    if !model_provider.is_empty() {
        return (
            model_provider.to_string(),
            config.model_providers.get(model_provider).cloned(),
        );
    }
    if config.model_providers.len() == 1 {
        if let Some((name, provider)) = config.model_providers.iter().next() {
            return (name.clone(), Some(provider.clone()));
        }
    }
    (model_provider.to_string(), None)
}

fn model_sources_from_environment(
    env: &HashMap<String, String>,
    auth_api_key: &str,
) -> Vec<ModelSource> {
    let base_url = first_env_value(env, BASE_URL_ENV_KEYS);
    if base_url.is_empty() {
        return Vec::new();
    }
    let api_key = first_env_value(env, API_KEY_ENV_KEYS);
    vec![ModelSource {
        source_id: "env:openai-compatible".to_string(),
        source_type: "environment".to_string(),
        name: "Environment".to_string(),
        base_url,
        api_key: if api_key.is_empty() {
            auth_api_key.to_string()
        } else {
            api_key
        },
    }]
}

fn model_source_from_config(
    config: &CodexConfig,
    effective: &HashMap<String, String>,
    env: &HashMap<String, String>,
    auth_api_key: &str,
) -> Option<ModelSource> {
    let model_provider = string_value(effective.get("model_provider"));
    let (resolved_provider, provider_config) =
        provider_config_for_model_provider(config, &model_provider);
    let provider_config = provider_config?;
    let base_url = string_value(provider_config.get("base_url"));
    if base_url.is_empty() {
        return None;
    }
    let name = string_value(provider_config.get("name"));
    let api_key = provider_api_key(&provider_config, env, auth_api_key);
    Some(ModelSource {
        source_id: format!(
            "config:{}",
            if resolved_provider.is_empty() {
                &name
            } else {
                &resolved_provider
            }
        ),
        source_type: "config".to_string(),
        name: if name.is_empty() {
            resolved_provider
        } else {
            name
        },
        base_url,
        api_key,
    })
}

fn provider_api_key(
    provider_config: &HashMap<String, String>,
    env: &HashMap<String, String>,
    auth_api_key: &str,
) -> String {
    for key in [
        "experimental_bearer_token",
        "api_key",
        "apikey",
        "bearer_token",
        "token",
    ] {
        let value = string_value(provider_config.get(key));
        if !value.is_empty() {
            return value;
        }
    }
    for key in [
        "env_key",
        "api_key_env",
        "api_key_env_var",
        "key_env",
        "bearer_token_env",
    ] {
        let env_name = string_value(provider_config.get(key));
        if !env_name.is_empty() {
            let value = first_env_value(env, &[&env_name]);
            if !value.is_empty() {
                return value;
            }
        }
    }
    let env_key = first_env_value(env, API_KEY_ENV_KEYS);
    if env_key.is_empty() {
        auth_api_key.to_string()
    } else {
        env_key
    }
}

async fn fetch_models_from_source(
    client: &reqwest::Client,
    source: &ModelSource,
) -> (Vec<String>, Value) {
    let endpoint = models_endpoint(&source.base_url);
    let mut safe_source = json!({
        "id": source.source_id,
        "type": source.source_type,
        "name": source.name,
        "base_url": safe_url_for_status(&source.base_url),
        "endpoint": safe_url_for_status(&endpoint),
        "auth": if source.api_key.is_empty() { "missing" } else { "present" },
    });
    if endpoint.is_empty() {
        safe_source["status"] = json!("failed");
        safe_source["message"] = json!("Missing base URL");
        safe_source["models"] = json!(0);
        return (Vec::new(), safe_source);
    }

    let mut request = client
        .get(&endpoint)
        .header(reqwest::header::ACCEPT, "application/json");
    if !source.api_key.is_empty() {
        request = request.bearer_auth(&source.api_key);
    }

    match request.send().await {
        Ok(response) if response.status().is_success() => match response.json::<Value>().await {
            Ok(payload) => {
                let models = unique_strings(parse_model_payload(&payload));
                safe_source["status"] = json!("ok");
                safe_source["models"] = json!(models.len());
                (models, safe_source)
            }
            Err(error) => failed_source(safe_source, error.to_string()),
        },
        Ok(response) => failed_source(safe_source, format!("HTTP {}", response.status().as_u16())),
        Err(error) => failed_source(safe_source, error.to_string()),
    }
}

fn failed_source(mut source: Value, message: String) -> (Vec<String>, Value) {
    source["status"] = json!("failed");
    source["message"] = json!(message);
    source["models"] = json!(0);
    source["responses_api"] = responses_api_status("unknown", "", "");
    (Vec::new(), source)
}

fn responses_api_status(status: &str, endpoint: &str, message: &str) -> Value {
    json!({
        "status": status,
        "endpoint": endpoint,
        "message": message
    })
}

pub async fn fetch_relay_profile_model_ids(
    profile: &RelayProfile,
) -> anyhow::Result<(Vec<String>, String)> {
    let source = ModelSource {
        source_id: format!("relay-profile:{}", profile.id),
        source_type: "relay_profile".to_string(),
        name: if profile.name.trim().is_empty() {
            profile.id.clone()
        } else {
            profile.name.trim().to_string()
        },
        base_url: if profile.upstream_base_url.trim().is_empty() {
            profile.base_url.trim().to_string()
        } else {
            profile.upstream_base_url.trim().to_string()
        },
        api_key: profile.api_key.trim().to_string(),
    };
    if source.base_url.is_empty() {
        anyhow::bail!("Base URL 不能为空");
    }
    let endpoint = models_endpoint(&source.base_url);
    let client = crate::http_client::proxied_client(&profile.user_agent)?;
    let (models, status) = fetch_models_from_source(&client, &source).await;
    if models.is_empty() {
        let message = status
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("上游没有返回可用模型");
        anyhow::bail!("{message}");
    }
    Ok((models, endpoint))
}

fn preferred_responses_api_status(sources: &[Value]) -> Value {
    let statuses = sources
        .iter()
        .filter_map(|source| source.get("responses_api"))
        .collect::<Vec<_>>();
    for wanted in ["unsupported", "supported", "failed"] {
        if let Some(status) = statuses
            .iter()
            .find(|status| status.get("status").and_then(Value::as_str) == Some(wanted))
        {
            return (*status).clone();
        }
    }
    responses_api_status("unknown", "", "")
}

fn models_endpoint(base_url: &str) -> String {
    let cleaned = safe_url_for_status(base_url)
        .trim_end_matches('/')
        .to_string();
    if cleaned.is_empty() {
        return String::new();
    }
    if cleaned.ends_with("/models") {
        return cleaned;
    }
    // Only append the default `/v1` version prefix when the base URL does not
    // already carry a version segment. Providers such as Volcano Engine ARK use
    // a versioned base (e.g. `.../api/coding/v3`), so blindly appending
    // `/v1/models` produced `.../api/coding/v3/v1/models` and 404'd. This mirrors
    // the version handling already used by the protocol proxy. See issue #1349.
    if crate::protocol_proxy::has_version_suffix(&cleaned) {
        return format!("{cleaned}/models");
    }
    // ReCodex 的 base 是 `.../backend-api/codex` —— codex 不是版本段,上面那条判定
    // 不成立,于是拼出 `/backend-api/codex/v1/models` 打了 404(线上一天 88 次)。
    // 网关服务的是 `{base}/models`(见 sub2api handler/gateway_models_test.go)。
    if cleaned.ends_with("/backend-api/codex") {
        return format!("{cleaned}/models");
    }
    format!("{cleaned}/v1/models")
}

fn parse_model_payload(payload: &Value) -> Vec<String> {
    if let Some(array) = payload.as_array() {
        return array
            .iter()
            .filter_map(|item| {
                item.as_str().map(str::to_string).or_else(|| {
                    item.as_object().and_then(|object| {
                        ["id", "model", "name"]
                            .iter()
                            .filter_map(|key| object.get(*key).and_then(Value::as_str))
                            .find(|value| !value.trim().is_empty())
                            .map(|value| value.trim().to_string())
                    })
                })
            })
            .collect();
    }
    let Some(object) = payload.as_object() else {
        return Vec::new();
    };
    // Codex 的 models manifest 用 `slug`,不是 `id`/`model`/`name` —— 下面那圈通用
    // 解析对它一个都取不出来(网关返回的就是这个形状,表现是模型列表为空)。
    // 直接复用本地 model_catalog_json 那套:它认 slug,还顺带按 visibility 过滤掉
    // `hide` 的条目(gpt-reserve、codex-auto-review 不该出现在选择器里)。
    let manifest = parse_model_catalog_json_models(payload);
    if !manifest.is_empty() {
        return manifest;
    }
    for key in ["data", "models", "items"] {
        if let Some(value) = object.get(key) {
            let nested = parse_model_payload(value);
            if !nested.is_empty() {
                return nested;
            }
        }
    }
    ["id", "model", "name"]
        .iter()
        .filter_map(|key| object.get(*key).and_then(Value::as_str))
        .find(|value| !value.trim().is_empty())
        .map(|value| vec![value.trim().to_string()])
        .unwrap_or_default()
}

fn models_from_config_model_catalog_json(
    home: &Path,
    effective: &HashMap<String, String>,
) -> (Vec<String>, Option<Value>) {
    let raw_path = string_value(effective.get("model_catalog_json"));
    if raw_path.is_empty() {
        return (Vec::new(), None);
    }
    let path = resolve_config_path(home, &raw_path);
    let safe_path = path.to_string_lossy().to_string();
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) => {
            return (
                Vec::new(),
                Some(json!({
                    "id": "config:model_catalog_json",
                    "type": "model_catalog_json",
                    "name": "Codex model catalog",
                    "path": safe_path,
                    "status": "failed",
                    "message": error.to_string(),
                    "models": 0,
                    "responses_api": responses_api_status("unknown", "", "")
                })),
            );
        }
    };
    let payload = match serde_json::from_str::<Value>(&contents) {
        Ok(payload) => payload,
        Err(error) => {
            return (
                Vec::new(),
                Some(json!({
                    "id": "config:model_catalog_json",
                    "type": "model_catalog_json",
                    "name": "Codex model catalog",
                    "path": safe_path,
                    "status": "failed",
                    "message": error.to_string(),
                    "models": 0,
                    "responses_api": responses_api_status("unknown", "", "")
                })),
            );
        }
    };
    let models = unique_strings(parse_model_catalog_json_models(&payload));
    let count = models.len();
    (
        models,
        Some(json!({
            "id": "config:model_catalog_json",
            "type": "model_catalog_json",
            "name": "Codex model catalog",
            "path": safe_path,
            "status": "ok",
            "models": count,
            "responses_api": responses_api_status("unknown", "", "")
        })),
    )
}

fn resolve_config_path(home: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        home.join(path)
    }
}

fn parse_model_catalog_json_models(payload: &Value) -> Vec<String> {
    let Some(models) = payload.get("models").and_then(Value::as_array) else {
        return Vec::new();
    };
    models
        .iter()
        .filter(|model| catalog_model_visible_in_api(model))
        .filter_map(|model| model.get("slug").and_then(Value::as_str))
        .map(str::trim)
        .filter(|slug| !slug.is_empty())
        .map(str::to_string)
        .collect()
}

/// 取当前 `config.toml` 指向的 provider 的 models manifest,算出推荐模型。
///
/// 单开一个入口而不是把 `client_version` 串进 `read_codex_model_catalog_from_home`:
/// 那条链上有两个 `fetch_models_from_source` 调用点和一串签名,而这件事只有写
/// config.toml 时才用得上,不值得为它改动正在发版的公共路径。
///
/// 任何一步取不到都返回 `None`,由调用方**保持现状** —— 宁可让用户停在旧模型上,
/// 也不能因为一次网络抖动把他的 `model` 改掉或写没了。
pub async fn recommended_model_for_home(
    home: &Path,
    env: &HashMap<String, String>,
    client_version: &str,
    timeout: std::time::Duration,
) -> Option<String> {
    // 自带客户端而不是让调用方传:launcher 不依赖 reqwest,为这一件事给它加一个
    // 传递依赖不值当。超时必须有 —— 这段跑在启动路径上,不能被慢网络拖住。
    let client = reqwest::Client::builder()
        .user_agent("CodexPlusPlus/1.0")
        .timeout(timeout)
        .build()
        .ok()?;
    let config_path = home.join("config.toml");
    let auth_api_key = read_codex_auth_api_key(&home.join("auth.json"));
    let (config, effective, error) = load_codex_config(&config_path);
    // "missing" = 还没有 config.toml,不算错;其余解析错误就别猜了。
    if error.as_deref().is_some_and(|error| error != "missing") {
        return None;
    }
    let source = model_source_from_config(&config, &effective, env, &auth_api_key)?;
    let endpoint = models_endpoint(&source.base_url);
    if endpoint.is_empty() {
        return None;
    }
    let mut request = client
        .get(&endpoint)
        .header(reqwest::header::ACCEPT, "application/json");
    if !source.api_key.is_empty() {
        request = request.bearer_auth(&source.api_key);
    }
    let response = request.send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let payload = response.json::<Value>().await.ok()?;
    recommended_model(&payload, client_version)
}

/// 从 Codex models manifest 里选出「推荐模型」。
///
/// 规则**全部来自 manifest 自身**,我们不维护任何模型清单 —— 上游上新模型时它自带
/// `priority = 1`,客户端下次刷新就跟上,不用改配置、不用发版、不用有人盯着:
///
///   - `visibility != "list"` 或 `supported_in_api == false` 的排除。
///     `gpt-reserve`(hide)、`codex-auto-review`(hide)不该出现在选择器里。
///   - `minimal_client_version` 高于本机 codex 版本的排除 —— 推荐一个本地跑不动的
///     模型比不推荐更糟。`gpt-6-astra` 标的就是 `0.153.0`,与实测门槛一致。
///   - 余下按 `priority` 升序取第一个(astra = 1,gpt-5.6-sol = 6)。并列时取 manifest
///     里靠前的那个(`min_by_key` 对相等元素返回第一个)。
///
/// **档位不用我们判断**:manifest 是上游按**该账号的实际权限**裁剪后下发的,
/// Plus 号的列表里根本没有 astra,所以 Plus 用户自然落到 gpt-5.6-sol。
///
/// `client_version` 解析不出来时不做版本过滤 —— responses 端点本身并不校验版本
/// (实测 `codex_cli_rs/0.60.0` 一样能调 astra),版本只影响 manifest 里给不给看,
/// 所以宁可多给一个选项,也不要因为版本号认不出来把用户卡在旧模型上。
pub fn recommended_model(payload: &Value, client_version: &str) -> Option<String> {
    let models = payload.get("models").and_then(Value::as_array)?;
    let client = crate::update::parse_version_tag(client_version).ok();
    models
        .iter()
        .filter(|model| catalog_model_visible_in_api(model))
        .filter(|model| client_supports_model(model, client.as_deref()))
        .filter_map(|model| {
            let slug = model.get("slug").and_then(Value::as_str)?.trim();
            if slug.is_empty() {
                return None;
            }
            // priority 缺失的排到最后,而不是当成 0 抢占第一。
            let priority = model
                .get("priority")
                .and_then(Value::as_u64)
                .unwrap_or(u64::MAX);
            Some((priority, slug.to_string()))
        })
        .min_by_key(|(priority, _)| *priority)
        .map(|(_, slug)| slug)
}

/// 本机 codex 版本是否满足这个模型的 `minimal_client_version`。
fn client_supports_model(model: &Value, client: Option<&[u64]>) -> bool {
    let Some(minimum) = model.get("minimal_client_version").and_then(Value::as_str) else {
        return true;
    };
    let minimum = minimum.trim();
    if minimum.is_empty() {
        return true;
    }
    let Ok(minimum) = crate::update::parse_version_tag(minimum) else {
        return true;
    };
    let Some(client) = client else {
        return true;
    };
    let len = client.len().max(minimum.len());
    let mut left = client.to_vec();
    left.resize(len, 0);
    let mut right = minimum;
    right.resize(len, 0);
    left >= right
}

fn catalog_model_visible_in_api(model: &Value) -> bool {
    let supported_in_api = model
        .get("supported_in_api")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if !supported_in_api {
        return false;
    }
    let visibility = model
        .get("visibility")
        .and_then(Value::as_str)
        .unwrap_or("list")
        .trim();
    visibility.eq_ignore_ascii_case("list")
}

fn unique_strings(values: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for value in values {
        let value = value.trim();
        if value.is_empty() || !seen.insert(value.to_string()) {
            continue;
        }
        result.push(value.to_string());
    }
    result
}

fn first_env_value(env: &HashMap<String, String>, names: &[&str]) -> String {
    names
        .iter()
        .filter_map(|name| env.get(*name))
        .map(|value| value.trim())
        .find(|value| !value.is_empty())
        .unwrap_or_default()
        .to_string()
}

fn safe_url_for_status(url: &str) -> String {
    let mut cleaned = url
        .split('?')
        .next()
        .unwrap_or_default()
        .split('#')
        .next()
        .unwrap_or_default()
        .to_string();
    if let Ok(parsed) = reqwest::Url::parse(&cleaned) {
        let host = parsed.host_str().unwrap_or_default();
        let authority = parsed
            .port()
            .map(|port| format!("{host}:{port}"))
            .unwrap_or_else(|| host.to_string());
        cleaned = format!("{}://{}{}", parsed.scheme(), authority, parsed.path());
    }
    cleaned
}

fn trim_url(url: &str) -> String {
    url.trim_end_matches('/').to_string()
}

fn string_value(value: Option<&String>) -> String {
    value
        .map(|value| value.trim().to_string())
        .unwrap_or_default()
}

fn unquote_toml_string(value: &str) -> String {
    let value = value.trim();
    if let Ok(parsed) = toml::from_str::<toml::Value>(&format!("value = {value}")) {
        if let Some(value) = parsed.get("value").and_then(toml::Value::as_str) {
            return value.to_string();
        }
    }
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(value)
        .to_string()
}

#[cfg(test)]
mod models_endpoint_tests {
    use super::models_endpoint;

    // 这个函数踩过两次拼接错:先是 ARK 的版本化 base 拼出 `/v3/v1/models`,
    // 后是 ReCodex 的 `/backend-api/codex` 拼出 `/codex/v1/models`(线上一天 404 了 88 次)。
    // 每种 base 形状钉一条,别再靠人肉推断。
    #[test]
    fn recodex_native_base_appends_models_directly() {
        assert_eq!(
            models_endpoint("https://api.recodex.dev/backend-api/codex"),
            "https://api.recodex.dev/backend-api/codex/models"
        );
        // 末尾多个斜杠是同一件事
        assert_eq!(
            models_endpoint("https://api.recodex.dev/backend-api/codex/"),
            "https://api.recodex.dev/backend-api/codex/models"
        );
    }

    #[test]
    fn versioned_base_appends_models_without_v1() {
        assert_eq!(
            models_endpoint("https://ark.example.com/api/coding/v3"),
            "https://ark.example.com/api/coding/v3/models"
        );
        assert_eq!(
            models_endpoint("https://api.openai.com/v1"),
            "https://api.openai.com/v1/models"
        );
    }

    #[test]
    fn plain_base_still_gets_v1_prefix() {
        assert_eq!(
            models_endpoint("https://api.example.com"),
            "https://api.example.com/v1/models"
        );
    }

    #[test]
    fn already_models_is_left_alone() {
        assert_eq!(
            models_endpoint("https://api.example.com/v1/models"),
            "https://api.example.com/v1/models"
        );
    }

    #[test]
    fn empty_base_yields_empty() {
        assert_eq!(models_endpoint(""), "");
    }
}
