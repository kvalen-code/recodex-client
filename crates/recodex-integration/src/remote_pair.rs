//! 手机远程控制「跟随账号配对」的电脑侧两条接口(docs/remote-app-plan.md §2.4),
//! 以及取 `channel=remote` 更新渠道。调用方是 codex-plus-core 的 phone_remote 模块。
//!
//! 与命令行 `recodex app` 同一口径(internal/authflow/client_remote.go):
//!   - `POST /api/cli/auth/remote/pair` body `{public_key, machine_name, platform}` → `{id, code, expires_at}`;
//!   - `GET /api/cli/auth/remote/pair/{id}` → `{status: pending|approved|rejected|expired}`;
//!   - `POST /api/cli/auth/remote/pair/{id}/cancel` 撤回(幂等,只认发起设备)。
//!
//! 服务端错误体是 `{"error":"<文本>"}`,不是结构化 code —— 这里只按状态码分流
//! (401 → Unauthorized / 429 → RateLimited / 其余 → Unavailable),由调用方翻成人话。
use serde::{Deserialize, Serialize};

use crate::desktop::ReCodexState;
use crate::{Adapter, AdapterError, Transport, UpdateChannel};

pub const REMOTE_PAIR_PENDING: &str = "pending";
pub const REMOTE_PAIR_APPROVED: &str = "approved";
pub const REMOTE_PAIR_REJECTED: &str = "rejected";
pub const REMOTE_PAIR_EXPIRED: &str = "expired";

/// 远程组件运行时的更新渠道名(sub2api P65)。
pub const REMOTE_UPDATE_CHANNEL: &str = "remote";

#[derive(Debug, Clone, Serialize)]
struct RemotePairRequest<'a> {
    public_key: &'a str,
    machine_name: &'a str,
    platform: &'a str,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct RemotePairCreated {
    pub id: String,
    /// 后台从公钥算出的确认码(纯 6 位数字)。调用方必须和自己算的比对 —— 这正是确认码的意义。
    pub code: String,
    #[serde(default)]
    pub expires_at: String,
}

#[derive(Debug, Clone, Deserialize)]
struct RemotePairStatusResponse {
    status: String,
}

fn is_six_digits(code: &str) -> bool {
    code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit())
}

/// 配对请求 id 是后台生成的 24 位 base64url;拼进路径前再收一道,防止怪字符改写路径。
fn valid_pair_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl<T: Transport> Adapter<T> {
    /// 登记远程组件的一次性公钥,拿回请求 id 与后台算出的确认码。
    pub fn remote_pair(
        &self,
        public_key: &str,
        machine_name: &str,
        platform: &str,
    ) -> Result<RemotePairCreated, AdapterError> {
        let body = serde_json::to_string(&RemotePairRequest {
            public_key,
            machine_name,
            platform,
        })
        .map_err(|error| AdapterError::InvalidConfiguration(error.to_string()))?;
        let created: RemotePairCreated =
            self.request("POST", "/api/cli/auth/remote/pair", Some(&body))?;
        if !valid_pair_id(&created.id) || !is_six_digits(&created.code) {
            return Err(AdapterError::InvalidResponse(
                "remote pair response is malformed".into(),
            ));
        }
        Ok(created)
    }

    /// 查自己发起的配对请求:pending / approved / rejected / expired。
    pub fn remote_pair_status(&self, id: &str) -> Result<String, AdapterError> {
        if !valid_pair_id(id) {
            return Err(AdapterError::InvalidConfiguration(
                "remote pair id is invalid".into(),
            ));
        }
        let path = format!("/api/cli/auth/remote/pair/{id}");
        let response: RemotePairStatusResponse = self.request("GET", &path, None)?;
        match response.status.as_str() {
            REMOTE_PAIR_PENDING | REMOTE_PAIR_APPROVED | REMOTE_PAIR_REJECTED
            | REMOTE_PAIR_EXPIRED => Ok(response.status),
            _ => Err(AdapterError::InvalidResponse(
                "remote pair status is malformed".into(),
            )),
        }
    }

    /// 撤回自己发起的配对请求(`POST remote/pair/{id}/cancel`,幂等):用户关开关、取消、
    /// 扫码已完成或出错时调用,让手机上那条「允许电脑 XX 连接?」立刻消失,
    /// 而不是挂满 10 分钟。回包内容不关心,2xx 即成功。
    pub fn remote_pair_cancel(&self, id: &str) -> Result<(), AdapterError> {
        if !valid_pair_id(id) {
            return Err(AdapterError::InvalidConfiguration(
                "remote pair id is invalid".into(),
            ));
        }
        let path = format!("/api/cli/auth/remote/pair/{id}/cancel");
        let _: serde_json::Value = self.request("POST", &path, Some("{}"))?;
        Ok(())
    }
}

// ── 给 codex-plus-core 用的入口 ─────────────────────────────────────────
//
// 每次现建一份 ReCodexState:令牌会被刷新轮换,常驻的那份 state 在另一个 crate 里拿不到,
// 从凭据库现读保证用的是最新的。都是阻塞网络 I/O(ureq,10 秒超时),调用方放 spawn_blocking。

fn fresh_adapter() -> Result<Adapter<crate::HttpTransport>, AdapterError> {
    ReCodexState::from_env().authenticated_adapter()
}

/// `GET /api/v1/client/update-channel?channel=remote`(带设备令牌)。
pub fn remote_update_channel() -> Result<UpdateChannel, AdapterError> {
    fresh_adapter()?.update_channel(REMOTE_UPDATE_CHANNEL)
}

pub fn remote_pair_create(
    public_key: &str,
    machine_name: &str,
    platform: &str,
) -> Result<RemotePairCreated, AdapterError> {
    fresh_adapter()?.remote_pair(public_key, machine_name, platform)
}

pub fn remote_pair_status(id: &str) -> Result<String, AdapterError> {
    fresh_adapter()?.remote_pair_status(id)
}

pub fn remote_pair_cancel(id: &str) -> Result<(), AdapterError> {
    fresh_adapter()?.remote_pair_cancel(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct FakeTransport {
        reply: (u16, String),
        calls: RefCell<Vec<(String, String, String, String)>>,
    }

    impl Transport for FakeTransport {
        fn request(
            &self,
            method: &str,
            path: &str,
            token: &str,
            body: Option<&str>,
        ) -> Result<(u16, String), AdapterError> {
            self.calls.borrow_mut().push((
                method.to_owned(),
                path.to_owned(),
                token.to_owned(),
                body.unwrap_or("").to_owned(),
            ));
            Ok(self.reply.clone())
        }
    }

    fn adapter(status: u16, body: &str) -> Adapter<FakeTransport> {
        let mut a = Adapter::new(
            FakeTransport {
                reply: (status, body.to_owned()),
                calls: RefCell::new(Vec::new()),
            },
            "https://api.example.test",
        )
        .unwrap();
        a.set_access_token("rct_testtoken".into()).unwrap();
        a
    }

    #[test]
    fn pair_posts_the_contract_body_with_the_device_token() {
        let a = adapter(
            200,
            r#"{"id":"abcDEF123_-xyzabcDEF123","code":"848873","expires_at":"2026-09-19T12:10:00Z"}"#,
        );
        let created = a
            .remote_pair(
                "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=",
                "DESKTOP-ABC",
                "windows",
            )
            .unwrap();
        assert_eq!(created.code, "848873");
        let calls = a.transport_calls();
        assert_eq!(calls[0].0, "POST");
        assert_eq!(calls[0].1, "/api/cli/auth/remote/pair");
        assert_eq!(calls[0].2, "rct_testtoken");
        let body: serde_json::Value = serde_json::from_str(&calls[0].3).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "public_key": "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=",
                "machine_name": "DESKTOP-ABC",
                "platform": "windows"
            })
        );
    }

    #[test]
    fn malformed_pair_answers_are_rejected() {
        for body in [
            r#"{"id":"abc","code":"84887"}"#,
            r#"{"id":"abc","code":"8488a3"}"#,
            r#"{"id":"","code":"848873"}"#,
            r#"{"id":"a/b","code":"848873"}"#,
        ] {
            assert!(
                matches!(
                    adapter(200, body).remote_pair("k", "m", "windows"),
                    Err(AdapterError::InvalidResponse(_))
                ),
                "{body}"
            );
        }
    }

    #[test]
    fn pair_errors_map_by_status() {
        assert_eq!(
            adapter(401, "")
                .remote_pair("k", "m", "windows")
                .unwrap_err(),
            AdapterError::Unauthorized
        );
        assert_eq!(
            adapter(429, "")
                .remote_pair("k", "m", "windows")
                .unwrap_err(),
            AdapterError::RateLimited
        );
        assert_eq!(
            adapter(501, "")
                .remote_pair("k", "m", "windows")
                .unwrap_err(),
            AdapterError::Unavailable
        );
    }

    #[test]
    fn status_accepts_only_the_four_states() {
        for status in ["pending", "approved", "rejected", "expired"] {
            let a = adapter(200, &format!(r#"{{"status":"{status}"}}"#));
            assert_eq!(a.remote_pair_status("abc_DEF-1").unwrap(), status);
            assert_eq!(
                a.transport_calls()[0].1,
                "/api/cli/auth/remote/pair/abc_DEF-1"
            );
        }
        assert!(adapter(200, r#"{"status":"weird"}"#)
            .remote_pair_status("abc")
            .is_err());
    }

    #[test]
    fn status_refuses_ids_that_would_rewrite_the_path() {
        let a = adapter(200, r#"{"status":"pending"}"#);
        for id in ["", "../x", "a/b", "a?b", "a b"] {
            assert!(a.remote_pair_status(id).is_err(), "{id}");
        }
        assert!(a.transport_calls().is_empty());
    }

    #[test]
    fn cancel_posts_to_the_request_it_owns() {
        let a = adapter(200, r#"{"status":"cancelled"}"#);
        a.remote_pair_cancel("abc_DEF-1").unwrap();
        let calls = a.transport_calls();
        assert_eq!(
            (calls[0].0.as_str(), calls[0].1.as_str()),
            ("POST", "/api/cli/auth/remote/pair/abc_DEF-1/cancel")
        );
        assert!(adapter(200, "{}").remote_pair_cancel("../x").is_err());
        assert_eq!(
            adapter(404, "").remote_pair_cancel("abc").unwrap_err(),
            AdapterError::Unavailable
        );
    }

    #[test]
    fn signed_out_never_hits_the_network() {
        let a = Adapter::new(
            FakeTransport {
                reply: (200, "{}".into()),
                calls: RefCell::new(Vec::new()),
            },
            "https://api.example.test",
        )
        .unwrap();
        assert_eq!(
            a.remote_pair("k", "m", "windows").unwrap_err(),
            AdapterError::Unauthorized
        );
        assert!(a.transport_calls().is_empty());
    }

    impl Adapter<FakeTransport> {
        fn transport_calls(&self) -> Vec<(String, String, String, String)> {
            self.transport.calls.borrow().clone()
        }
    }
}
