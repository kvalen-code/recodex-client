mod compatibility;
pub mod codexcfg;
pub mod desktop;
pub mod officialmode; // recodex-overlay: 官方模式切换(可逆)
pub mod credential;
mod error;
mod install_id;
mod ui;

pub use compatibility::{check as check_compatibility, Compatibility};
pub use error::AdapterError;
pub use install_id::{load_or_create_install_id, load_or_create_install_id_at};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::net::IpAddr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
pub use ui::PanelState;
use url::Url;

const MAX_RESPONSE_BODY_BYTES: u64 = 1024 * 1024;

#[derive(Clone)]
pub struct HttpTransport {
    base_url: Url,
    agent: ureq::Agent,
}

impl HttpTransport {
    pub fn new(base_url: &str, timeout: Duration) -> Result<Self, AdapterError> {
        let base_url =
            Url::parse(base_url).map_err(|e| AdapterError::InvalidConfiguration(e.to_string()))?;
        validate_base_url(&base_url)?;
        // The configured API origin is part of the trust boundary. Redirects
        // may otherwise turn a trusted API request into an arbitrary 2xx
        // payload, so expose 3xx as an actionable status instead.
        let agent = ureq::AgentBuilder::new()
            .timeout(timeout)
            .redirects(0)
            .build();
        Ok(Self { base_url, agent })
    }
}

impl Transport for HttpTransport {
    fn request(
        &self,
        method: &str,
        path: &str,
        access_token: &str,
        body: Option<&str>,
    ) -> Result<(u16, String), AdapterError> {
        let url = self
            .base_url
            .join(path)
            .map_err(|e| AdapterError::InvalidConfiguration(e.to_string()))?;
        let mut request = self
            .agent
            .request(method, url.as_str())
            .set("Accept", "application/json");
        if !access_token.is_empty() {
            request = request.set("Authorization", &format!("Bearer {access_token}"));
        }
        let result = match body {
            Some(value) => request
                .set("Content-Type", "application/json")
                .send_string(value),
            None => request.call(),
        };
        match result {
            Ok(response) => {
                let status = response.status();
                if !(200..300).contains(&status) {
                    return Ok((status, String::new()));
                }
                let payload = read_response_body(response)?;
                Ok((status, payload))
            }
            // Error bodies are neither part of the adapter contract nor safe
            // to render. Preserve the actionable status even when that body
            // is malformed, oversized, or non-UTF-8.
            Err(ureq::Error::Status(status, _)) => Ok((status, String::new())),
            Err(ureq::Error::Transport(_)) => Err(AdapterError::Unavailable),
        }
    }
}

fn read_response_body(response: ureq::Response) -> Result<String, AdapterError> {
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_RESPONSE_BODY_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| AdapterError::Unavailable)?;
    if bytes.len() as u64 > MAX_RESPONSE_BODY_BYTES {
        return Err(AdapterError::InvalidResponse(format!(
            "response body exceeds {MAX_RESPONSE_BODY_BYTES} bytes"
        )));
    }
    String::from_utf8(bytes)
        .map_err(|_| AdapterError::InvalidResponse("response body is not UTF-8".into()))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Account {
    pub user_id: i64,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub plan: String,
    pub account_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub account_type: String,
    pub available: f64,
    pub total: f64,
    pub used: f64,
    #[serde(default)]
    pub windows: Vec<UsageWindow>,
    pub refreshed_at: String,
    pub source: String,
    pub stale: bool,
    #[serde(default)]
    pub refresh_error: Option<UsageError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UsageError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsageWindow {
    pub window: String,
    pub limit: f64,
    pub used: f64,
    pub remaining: f64,
    pub reset_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Gateway {
    pub id: String,
    pub name: String,
    pub endpoint: String,
    pub enabled: bool,
    pub maintenance: bool,
    #[serde(default)]
    pub client_latency_ms: Option<i64>,
    pub healthy: bool,
    pub selected: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GatewayList {
    pub gateways: Vec<Gateway>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoginStart {
    pub device_code: String,
    pub user_code: String,
    pub verify_url: String,
    pub interval_sec: u64,
    pub expires_in: u64,
}

/// Login metadata safe to cross the Tauri/WebView boundary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublicLoginStart {
    pub user_code: String,
    pub verify_url: String,
    pub interval_sec: u64,
    pub expires_in: u64,
}

impl From<&LoginStart> for PublicLoginStart {
    fn from(login: &LoginStart) -> Self {
        Self {
            user_code: login.user_code.clone(),
            verify_url: login.verify_url.clone(),
            interval_sec: login.interval_sec,
            expires_in: login.expires_in,
        }
    }
}

/// 占用设备名额的一台设备。达到上限时服务端会把名单一起回过来,
/// 好让用户知道该撤销哪一台 —— 光说"超限"没法让人动手。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct DeviceSlot {
    pub device_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub last_seen: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoginPoll {
    pub status: String,
    /// 只有 `device_limit` 会带:哪几台占着名额。
    /// 原先这个字段整个被丢掉,于是客户端只知道"不是 approved",
    /// 面板就一直显示「等待确认…」—— 服务端明说了原因,我们自己吞了。
    #[serde(default)]
    pub devices: Vec<DeviceSlot>,
    #[serde(default, skip_serializing)]
    pub token: String,
    #[serde(default)]
    pub gateway_url: String,
    // The `approved` response also carries the Codex config so the desktop can
    // route Codex through ReCodex — previously these were silently dropped.
    // `env_value` is the secret sub2api key: never serialise it back out.
    #[serde(default, skip_serializing)]
    pub config: String,
    #[serde(default, skip_serializing)]
    pub auth_json: String,
    #[serde(default)]
    pub env_key: String,
    #[serde(default, skip_serializing)]
    pub env_value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClientCompatibility {
    pub client_version: String,
    pub supported: bool,
    pub minimum_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateChannel {
    pub channel: String,
    #[serde(default)]
    pub available: bool,
    #[serde(default)]
    pub latest_version: String,
    #[serde(default)]
    pub manifest_url: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiagnosticReport {
    pub client_version: String,
    pub os: String,
    pub event: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurred_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiagnosticAccepted {
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct TokenRefresh {
    #[serde(skip_serializing)]
    token: String,
}

pub trait Transport {
    fn request(
        &self,
        method: &str,
        path: &str,
        access_token: &str,
        body: Option<&str>,
    ) -> Result<(u16, String), AdapterError>;
}

pub struct Adapter<T> {
    transport: T,
    base_url: Url,
    access_token: Option<String>,
    cached_usage: Option<Usage>,
}

impl<T: Transport> Adapter<T> {
    pub fn new(transport: T, base_url: &str) -> Result<Self, AdapterError> {
        let base_url =
            Url::parse(base_url).map_err(|e| AdapterError::InvalidConfiguration(e.to_string()))?;
        validate_base_url(&base_url)?;
        Ok(Self {
            transport,
            base_url,
            access_token: None,
            cached_usage: None,
        })
    }

    pub fn set_access_token(&mut self, token: String) -> Result<(), AdapterError> {
        if token.len() < 8 || !token.starts_with("rct_") {
            return Err(AdapterError::InvalidConfiguration(
                "invalid access token format".into(),
            ));
        }
        if self.access_token.as_deref() != Some(token.as_str()) {
            self.cached_usage = None;
        }
        self.access_token = Some(token);
        Ok(())
    }

    pub fn clear_access_token(&mut self) {
        self.access_token = None;
        self.cached_usage = None;
    }

    pub fn refresh_token(&mut self) -> Result<String, AdapterError> {
        let refreshed: TokenRefresh = self.request("POST", "/api/v1/auth/refresh", Some("{}"))?;
        self.set_access_token(refreshed.token.clone())?;
        Ok(refreshed.token)
    }

    pub fn revoke_session(&self) -> Result<(), AdapterError> {
        let _: serde_json::Value = self.request("POST", "/api/v1/auth/logout", Some("{}"))?;
        Ok(())
    }

    pub fn is_authenticated(&self) -> bool {
        self.access_token.is_some()
    }

    pub fn account(&self) -> Result<Account, AdapterError> {
        self.get("/api/v1/account")
    }

    pub fn compatibility(&self, version: &str) -> Result<ClientCompatibility, AdapterError> {
        validate_semver(version)?;
        let path = format!("/api/v1/client?version={version}");
        let result: ClientCompatibility = self.public_get(&path)?;
        validate_semver(&result.client_version)
            .map_err(|_| AdapterError::InvalidResponse("client version is invalid".into()))?;
        validate_semver(&result.minimum_version).map_err(|_| {
            AdapterError::InvalidResponse("minimum client version is invalid".into())
        })?;
        if result.client_version != version {
            return Err(AdapterError::InvalidResponse(
                "server returned a different client version".into(),
            ));
        }
        if result.supported && !compatibility::check(version, &result.minimum_version)?.supported {
            return Err(AdapterError::InvalidResponse(
                "server marked an older client version as supported".into(),
            ));
        }
        Ok(result)
    }

    pub fn update_channel(&self, channel: &str) -> Result<UpdateChannel, AdapterError> {
        validate_channel(channel)?;
        let path = format!("/api/v1/client/update-channel?channel={channel}");
        let result: UpdateChannel = self.get(&path)?;
        if result.available {
            validate_semver(&result.latest_version).map_err(|_| {
                AdapterError::InvalidResponse("update channel version is invalid".into())
            })?;
            let manifest = Url::parse(&result.manifest_url).map_err(|_| {
                AdapterError::InvalidResponse("update manifest URL is invalid".into())
            })?;
            if manifest.scheme() != "https"
                || manifest.host_str().is_none()
                || !manifest.username().is_empty()
                || manifest.password().is_some()
                || manifest.fragment().is_some()
            {
                return Err(AdapterError::InvalidResponse(
                    "update manifest URL is invalid".into(),
                ));
            }
        }
        Ok(result)
    }

    pub fn report_diagnostic(
        &self,
        report: &DiagnosticReport,
    ) -> Result<DiagnosticAccepted, AdapterError> {
        validate_diagnostic_report(report)?;
        let body = serde_json::to_string(report)
            .map_err(|error| AdapterError::InvalidConfiguration(error.to_string()))?;
        self.request("POST", "/api/v1/diagnostics", Some(&body))
    }

    pub fn start_login(
        &self,
        device_id: &str,
        device_name: &str,
        client_version: &str,
        os: &str,
    ) -> Result<LoginStart, AdapterError> {
        if device_id.trim().is_empty() {
            return Err(AdapterError::InvalidConfiguration(
                "device id is empty".into(),
            ));
        }
        let body = serde_json::json!({
            "device_id": device_id,
            "device_name": device_name,
            "pubkey": "desktop-managed",
            "client_version": client_version,
            "os": os
        });
        let login: LoginStart =
            self.public_request("POST", "/api/cli/auth/start", Some(&body.to_string()))?;
        validate_verification_url(&login.verify_url)?;
        Ok(login)
    }

    pub fn poll_login(&mut self, device_code: &str) -> Result<LoginPoll, AdapterError> {
        let body = serde_json::json!({"device_code": device_code});
        let result: LoginPoll =
            self.public_request("POST", "/api/cli/auth/poll", Some(&body.to_string()))?;
        if !result.gateway_url.is_empty() {
            validate_gateway_url(&result.gateway_url)?;
        }
        if result.status == "approved" {
            self.set_access_token(result.token.clone())?;
        }
        Ok(result)
    }

    pub fn usage(&mut self, refresh: bool) -> Result<Usage, AdapterError> {
        let result: Result<Usage, AdapterError> = self
            .request(
                if refresh { "POST" } else { "GET" },
                if refresh {
                    "/api/v1/usage/refresh"
                } else {
                    "/api/v1/usage"
                },
                None,
            )
            .and_then(|usage| {
                validate_usage(&usage)?;
                Ok(usage)
            });
        match result {
            Ok(value) => {
                self.cached_usage = Some(value.clone());
                Ok(value)
            }
            Err(AdapterError::Unavailable | AdapterError::InvalidResponse(_))
                if self.cached_usage.is_some() =>
            {
                let mut cached = self.cached_usage.clone().expect("checked above");
                cached.stale = true;
                // 非刷新读取失败时原先只标 stale、不写原因,于是面板拿到
                // 「数据过期」却说不出为什么 —— 排查额度不刷新时就卡在这里:
                // stale=true + refresh_error=null 这个组合看着像"谁都没干过",
                // 实际是一次失败的 GET。两条路径都记原因,只是 code 不同。
                cached.refresh_error = Some(if refresh {
                    UsageError {
                        code: "refresh_unavailable".into(),
                        message: "latest usage could not be synchronized".into(),
                    }
                } else {
                    UsageError {
                        code: "usage_unavailable".into(),
                        message: "usage could not be read, showing the last known values".into(),
                    }
                });
                // 注:不必把这份 stale 副本写回缓存 —— 每次回落都会重新置 stale,
                // 写回与不写回从外部观察不到差别。试过加写回并给它配了条测试,
                // 结果删掉写回测试照样绿,说明那条守卫是假的,两样一起去掉。
                Ok(cached)
            }
            Err(error) => Err(error),
        }
    }

    pub fn gateways(&self) -> Result<Vec<Gateway>, AdapterError> {
        let response: GatewayList = self.get("/api/v1/gateways")?;
        validate_gateways(&response.gateways)?;
        Ok(response.gateways)
    }

    pub fn test_gateways(&self, ids: &[String]) -> Result<Vec<Gateway>, AdapterError> {
        let response: GatewayList = self.request(
            "POST",
            "/api/v1/gateways/test",
            Some(&serde_json::json!({"ids": ids}).to_string()),
        )?;
        validate_gateways(&response.gateways)?;
        Ok(response.gateways)
    }

    /// Tests server-authorized gateways immediately before selection. Stale
    /// latency values returned by the list endpoint are never used here.
    pub fn use_fastest_gateway(&self) -> Result<Gateway, AdapterError> {
        let tested = self.test_gateways(&[])?;
        let fastest = self
            .fastest_gateway(&tested)
            .ok_or_else(|| AdapterError::InvalidResponse("no healthy enabled gateway".into()))?;
        self.select_gateway(&fastest.id)
    }

    pub fn select_gateway(&self, id: &str) -> Result<Gateway, AdapterError> {
        validate_gateway_id(id)?;
        let gateway: Gateway = self.request(
            "POST",
            "/api/v1/gateways/select",
            Some(&serde_json::json!({"id": id}).to_string()),
        )?;
        validate_gateway(&gateway)?;
        Ok(gateway)
    }

    pub fn fastest_gateway<'a>(&self, gateways: &'a [Gateway]) -> Option<&'a Gateway> {
        gateways
            .iter()
            .filter(|g| g.enabled && !g.maintenance && g.healthy)
            .min_by_key(|g| g.client_latency_ms.unwrap_or(i64::MAX))
    }

    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    fn get<R: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<R, AdapterError> {
        self.request("GET", path, None)
    }

    fn public_get<R: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<R, AdapterError> {
        self.public_request("GET", path, None)
    }

    fn public_request<R: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<R, AdapterError> {
        let (status, payload) = self.transport.request(method, path, "", body)?;
        match status {
            200..=299 => serde_json::from_str(&payload)
                .map_err(|e| AdapterError::InvalidResponse(e.to_string())),
            429 => Err(AdapterError::RateLimited),
            400 if path == "/api/cli/auth/poll" => Err(AdapterError::DeviceCodeUnknown),
            410 if path == "/api/cli/auth/poll" => Err(AdapterError::DeviceCodeExpired),
            500 if path == "/api/cli/auth/poll" => Err(AdapterError::ServiceUnavailable),
            _ => Err(AdapterError::Unavailable),
        }
    }

    fn request<R: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<R, AdapterError> {
        if self.access_token.is_none() {
            return Err(AdapterError::Unauthorized);
        }
        let access_token = self
            .access_token
            .as_deref()
            .ok_or(AdapterError::Unauthorized)?;
        let (status, payload) = self.transport.request(method, path, access_token, body)?;
        match status {
            200..=299 => serde_json::from_str(&payload)
                .map_err(|e| AdapterError::InvalidResponse(e.to_string())),
            401 => Err(AdapterError::Unauthorized),
            403 => Err(AdapterError::Forbidden),
            409 => Err(AdapterError::Conflict(
                "request conflicts with current ReCodex state".into(),
            )),
            429 => Err(AdapterError::RateLimited),
            _ => Err(AdapterError::Unavailable),
        }
    }
}

fn validate_base_url(url: &Url) -> Result<(), AdapterError> {
    let loopback = url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
    });
    if url.host_str().is_none() {
        return Err(AdapterError::InvalidConfiguration(
            "API endpoint must include a host".into(),
        ));
    }
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err(AdapterError::InvalidConfiguration(
            "API endpoint must use HTTPS".into(),
        ));
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(AdapterError::InvalidConfiguration(
            "API endpoint must not contain credentials, query, or fragment".into(),
        ));
    }
    Ok(())
}

fn validate_gateway_id(id: &str) -> Result<(), AdapterError> {
    if id.is_empty()
        || id.len() > 128
        || id.trim() != id
        || id.chars().any(|c| matches!(c, '\r' | '\n' | '\t'))
    {
        return Err(AdapterError::InvalidResponse(
            "gateway id is invalid".into(),
        ));
    }
    Ok(())
}

fn validate_usage(usage: &Usage) -> Result<(), AdapterError> {
    if usage.account_type != "exclusive" && usage.account_type != "shared" {
        return Err(AdapterError::InvalidResponse(
            "usage account type is invalid".into(),
        ));
    }
    if [usage.available, usage.total, usage.used]
        .iter()
        .any(|value| !value.is_finite() || *value < 0.0)
    {
        return Err(AdapterError::InvalidResponse(
            "usage values are invalid".into(),
        ));
    }
    if usage.available > usage.total || usage.used > usage.total {
        return Err(AdapterError::InvalidResponse(
            "usage values are inconsistent".into(),
        ));
    }
    if usage.source.trim().is_empty() || usage.refreshed_at.trim().is_empty() {
        return Err(AdapterError::InvalidResponse(
            "usage freshness metadata is missing".into(),
        ));
    }
    Ok(())
}

fn validate_gateway(gateway: &Gateway) -> Result<(), AdapterError> {
    validate_gateway_id(&gateway.id)?;
    if gateway.client_latency_ms.is_some_and(|latency| latency < 0) {
        return Err(AdapterError::InvalidResponse(
            "gateway latency is invalid".into(),
        ));
    }
    if gateway.endpoint.is_empty() {
        return Ok(());
    }
    let endpoint = Url::parse(&gateway.endpoint)
        .map_err(|_| AdapterError::InvalidResponse("gateway endpoint is invalid".into()))?;
    if endpoint.scheme() != "https"
        || endpoint.host_str().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(AdapterError::InvalidResponse(
            "gateway endpoint is invalid".into(),
        ));
    }
    Ok(())
}

fn validate_gateways(gateways: &[Gateway]) -> Result<(), AdapterError> {
    if gateways.len() > 256 {
        return Err(AdapterError::InvalidResponse("too many gateways".into()));
    }
    let mut ids = std::collections::HashSet::with_capacity(gateways.len());
    for gateway in gateways {
        validate_gateway(gateway)?;
        if !ids.insert(gateway.id.as_str()) {
            return Err(AdapterError::InvalidResponse("duplicate gateway id".into()));
        }
    }
    Ok(())
}

fn validate_verification_url(value: &str) -> Result<(), AdapterError> {
    if value.len() > 2048 {
        return Err(AdapterError::InvalidResponse(
            "verification URL is too long".into(),
        ));
    }
    let url = Url::parse(value)
        .map_err(|_| AdapterError::InvalidResponse("verification URL is invalid".into()))?;
    let loopback = url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
    });
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        return Err(AdapterError::InvalidResponse(
            "verification URL must use HTTPS".into(),
        ));
    }
    if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        return Err(AdapterError::InvalidResponse(
            "verification URL contains an unsafe authority".into(),
        ));
    }
    Ok(())
}

fn validate_gateway_url(value: &str) -> Result<(), AdapterError> {
    let url = Url::parse(value)
        .map_err(|_| AdapterError::InvalidResponse("gateway URL is invalid".into()))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(AdapterError::InvalidResponse(
            "gateway URL contains an unsafe authority".into(),
        ));
    }
    Ok(())
}

fn validate_semver(value: &str) -> Result<(), AdapterError> {
    // Single source of truth for the semver format lives in `compatibility`;
    // delegating keeps the two validators from drifting apart.
    compatibility::numeric_version(value).map(|_| ())
}

fn validate_channel(value: &str) -> Result<(), AdapterError> {
    if value.is_empty()
        || value.len() > 32
        || !value.chars().enumerate().all(|(index, c)| {
            if index == 0 {
                c.is_ascii_lowercase()
            } else {
                c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'
            }
        })
    {
        return Err(AdapterError::InvalidConfiguration(
            "update channel is invalid".into(),
        ));
    }
    Ok(())
}

fn validate_diagnostic_report(report: &DiagnosticReport) -> Result<(), AdapterError> {
    for (name, value) in [
        ("client_version", report.client_version.as_str()),
        ("os", report.os.as_str()),
        ("event", report.event.as_str()),
        ("error_code", report.error_code.as_deref().unwrap_or("")),
    ] {
        if value.len() > 128 || value.chars().any(|c| matches!(c, '\r' | '\n' | '\t')) {
            return Err(AdapterError::InvalidConfiguration(format!(
                "diagnostic {name} is invalid"
            )));
        }
        let lower = value.to_ascii_lowercase();
        if lower.contains("bearer ")
            || lower.contains("rct_")
            || lower.contains("sk-")
            || lower.contains("token=")
        {
            return Err(AdapterError::InvalidConfiguration(
                "diagnostic payload contains a secret".into(),
            ));
        }
    }
    if report.event.is_empty() {
        return Err(AdapterError::InvalidConfiguration(
            "diagnostic event is required".into(),
        ));
    }
    Ok(())
}

impl<T: Transport + Clone> Adapter<T> {
    /// Creates an isolated request client so network I/O can run without
    /// holding the desktop state's global mutex.
    pub fn fork(&self) -> Self {
        Self {
            transport: self.transport.clone(),
            base_url: self.base_url.clone(),
            access_token: self.access_token.clone(),
            cached_usage: self.cached_usage.clone(),
        }
    }

    /// Runs a usage request on an isolated adapter and returns that same
    /// worker so callers can merge its refreshed cache after network I/O.
    pub fn usage_in_fork(&self, refresh: bool) -> (Result<Usage, AdapterError>, Self) {
        let mut worker = self.fork();
        let usage = worker.usage(refresh);
        (usage, worker)
    }

    /// Preserves a refreshed usage cache only when authentication did not
    /// change while the isolated request was in flight.
    pub fn merge_cache_from(&mut self, other: &Self) {
        if self.access_token == other.access_token {
            self.cached_usage = other.cached_usage.clone();
        }
    }
}

pub fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}
