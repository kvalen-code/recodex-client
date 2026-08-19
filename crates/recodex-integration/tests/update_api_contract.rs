//! recodex-overlay: 更新接口的**契约测试**。
//!
//! 这些用例逐条对应 `docs/recodex-backend-update-api.md` 里写给后端的约束。
//! 目的不是测客户端代码,而是**把文档钉死**:后端照着文档实现,这里就能通过;
//! 哪天客户端校验改了而文档没跟上,这里会先红。

use std::cell::RefCell;

use recodex_integration::{Adapter, AdapterError, Transport};

/// 按 (method, path 前缀) 返回预设响应的假 transport。
struct FakeTransport {
    responses: RefCell<Vec<(String, u16, String)>>,
    seen: RefCell<Vec<String>>,
}

impl FakeTransport {
    fn new(responses: Vec<(&str, u16, &str)>) -> Self {
        Self {
            responses: RefCell::new(
                responses
                    .into_iter()
                    .map(|(p, s, b)| (p.to_string(), s, b.to_string()))
                    .collect(),
            ),
            seen: RefCell::new(Vec::new()),
        }
    }
}

impl Transport for FakeTransport {
    fn request(
        &self,
        _method: &str,
        path: &str,
        _access_token: &str,
        _body: Option<&str>,
    ) -> Result<(u16, String), AdapterError> {
        self.seen.borrow_mut().push(path.to_string());
        for (prefix, status, body) in self.responses.borrow().iter() {
            if path.starts_with(prefix.as_str()) {
                return Ok((*status, body.clone()));
            }
        }
        Ok((404, "{}".into()))
    }
}

fn adapter(responses: Vec<(&str, u16, &str)>) -> Adapter<FakeTransport> {
    Adapter::new(FakeTransport::new(responses), "https://api.example.com").unwrap()
}

/// update-channel 是**带认证**的接口 —— 未登录直接 Unauthorized,连请求都不会发出去。
/// 这正是灰度推送的基础:后端天然知道是哪个用户在问。
fn authed_adapter(responses: Vec<(&str, u16, &str)>) -> Adapter<FakeTransport> {
    let mut a = adapter(responses);
    a.set_access_token("rct_contract_test_token".into()).unwrap();
    a
}

#[test]
fn update_channel_requires_authentication() {
    let anonymous = adapter(vec![(
        "/api/v1/client/update-channel",
        200,
        r#"{"channel":"stable","available":false}"#,
    )]);
    assert!(
        matches!(anonymous.update_channel("stable"), Err(AdapterError::Unauthorized)),
        "未登录时不应发出请求 —— 灰度推送依赖后端能识别用户"
    );
}

// ── /api/v1/client ────────────────────────────────────────────

#[test]
fn compatibility_accepts_a_well_formed_response() {
    let a = adapter(vec![(
        "/api/v1/client?",
        200,
        r#"{"client_version":"1.2.49","supported":true,"minimum_version":"1.2.47"}"#,
    )]);
    let result = a.compatibility("1.2.49").unwrap();
    assert!(result.supported);
    assert_eq!(result.minimum_version, "1.2.47");
}

#[test]
fn compatibility_rejects_echoing_a_different_version() {
    // 文档:client_version 必须原样回显请求里的 version
    let a = adapter(vec![(
        "/api/v1/client?",
        200,
        r#"{"client_version":"9.9.9","supported":true,"minimum_version":"1.0.0"}"#,
    )]);
    assert!(a.compatibility("1.2.49").is_err());
}

#[test]
fn compatibility_rejects_self_contradicting_support_flag() {
    // 文档:supported=true 时必须满足 version >= minimum_version。
    // 后端不能一边说"支持",一边给出更高的最低版本。
    let a = adapter(vec![(
        "/api/v1/client?",
        200,
        r#"{"client_version":"1.2.49","supported":true,"minimum_version":"2.0.0"}"#,
    )]);
    assert!(a.compatibility("1.2.49").is_err());
}

#[test]
fn compatibility_reports_unsupported_for_outdated_clients() {
    // 强制更新的正路:抬高 minimum_version,supported=false
    let a = adapter(vec![(
        "/api/v1/client?",
        200,
        r#"{"client_version":"1.2.49","supported":false,"minimum_version":"1.3.0"}"#,
    )]);
    let result = a.compatibility("1.2.49").unwrap();
    assert!(!result.supported);
}

// ── /api/v1/client/update-channel ─────────────────────────────

#[test]
fn update_channel_accepts_no_update_available() {
    let a = authed_adapter(vec![(
        "/api/v1/client/update-channel",
        200,
        r#"{"channel":"stable","available":false,"reason":"already_latest"}"#,
    )]);
    let result = a.update_channel("stable").unwrap();
    assert!(!result.available);
    assert_eq!(result.reason.as_deref(), Some("already_latest"));
}

#[test]
fn update_channel_accepts_a_well_formed_update() {
    let a = authed_adapter(vec![(
        "/api/v1/client/update-channel",
        200,
        r#"{"channel":"stable","available":true,"latest_version":"1.2.50",
            "manifest_url":"https://oss.example.com/recodex/1.2.50/manifest.json"}"#,
    )]);
    let result = a.update_channel("stable").unwrap();
    assert!(result.available);
    assert_eq!(result.latest_version, "1.2.50");
}

#[test]
fn update_channel_rejects_insecure_or_unsafe_manifest_urls() {
    // 文档里的四条硬性约束,逐条验
    let cases = [
        ("http 明文", "http://oss.example.com/m.json"),
        ("带用户名密码", "https://user:pass@oss.example.com/m.json"),
        ("带 fragment", "https://oss.example.com/m.json#frag"),
    ];
    // 注:`https:///m.json` 不在此列 —— URL 解析器会把 `m.json` 当成主机名,
    // 这一步拦不住它;真正拦住它的是下载阶段的 DNS 解析失败。
    for (label, url) in cases {
        let body = format!(
            r#"{{"channel":"stable","available":true,"latest_version":"1.2.50","manifest_url":"{url}"}}"#
        );
        let a = authed_adapter(vec![("/api/v1/client/update-channel", 200, body.as_str())]);
        assert!(
            a.update_channel("stable").is_err(),
            "{label} 的 manifest_url 必须被拒绝:{url}"
        );
    }
}

#[test]
fn update_channel_rejects_invalid_channel_names() {
    // 文档:非空、<=32 字符、首字符小写字母,其余小写字母/数字/连字符
    let a = authed_adapter(vec![("/api/v1/client/update-channel", 200, "{}")]);
    for bad in ["", "Stable", "1stable", "sta ble", "stable_x", &"a".repeat(33)] {
        assert!(a.update_channel(bad).is_err(), "非法 channel 应被拒:{bad:?}");
    }
    // 合法的应放行(响应缺字段不影响 channel 校验本身)
    let ok = authed_adapter(vec![(
        "/api/v1/client/update-channel",
        200,
        r#"{"channel":"beta-2","available":false}"#,
    )]);
    assert!(ok.update_channel("beta-2").is_ok());
}
