//! 手机远程控制「跟随账号配对」的电脑侧两条接口(docs/remote-app-plan.md §2.4),
//! 以及取 `channel=remote` 更新渠道。调用方是 codex-plus-core 的 phone_remote 模块。
//!
//! 与命令行 `recodex app` 同一口径(internal/authflow/client_remote.go):
//!   - `POST /api/cli/auth/remote/pair` body `{public_key, machine_name, platform}` → `{id, code, expires_at}`;
//!   - `GET /api/cli/auth/remote/pair/{id}` → `{status: pending|approved|rejected|expired|cancelled}`;
//!   - `POST /api/cli/auth/remote/pair/{id}/cancel` 撤回:Bearer 设备令牌、body 可空(这里发 `{}`),
//!     对发起它的设备永远 200 且幂等(pending → `cancelled`,终态原样回),别人的/不存在的 id 回 404。
//!
//! `cancelled` = 发起它的电脑撤回了,或同一设备又发起了新的配对把它顶替了。
//!
//! 服务端错误体是 `{"error":"<文本>"}`,不是结构化 code —— 配对这三条接口**原样带回状态码**
//! (`PairApiError::Http`),由调用方翻成人话:它必须分清「后台明确没建记录」(400/401/403/404/429/501,
//! 可以降级成只扫码)与「后台可能已经建好了记录」(5xx / 网络错误 / 回包解不开,只能重试或中止)。
//! 更新渠道那条仍走通用的 `AdapterError` 口径。
//!
//! 批准方绑定(docs/remote-app-plan.md §2.4.1):登记时带 `approver_check: true`,
//! 响应回 `approver_binding` / `approver_check`;approved 时状态里带
//! `approver_public_key` / `approver_account_id`(两者齐全且形状合法才给出,否则一并清空 ——
//! 调用方按「缺失」中止,不会拿坏值去比)。
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::desktop::ReCodexState;
use crate::{Adapter, AdapterError, Transport, UpdateChannel};

pub const REMOTE_PAIR_PENDING: &str = "pending";
pub const REMOTE_PAIR_APPROVED: &str = "approved";
pub const REMOTE_PAIR_REJECTED: &str = "rejected";
pub const REMOTE_PAIR_EXPIRED: &str = "expired";
pub const REMOTE_PAIR_CANCELLED: &str = "cancelled";

/// 远程组件运行时的更新渠道名(sub2api P65)。
pub const REMOTE_UPDATE_CHANNEL: &str = "remote";

/// 配对接口的失败。`Http` 是后台给出的状态码(错误体不可信也不渲染),`Adapter` 是没到后台
/// 或回包解不开(连不上、超时、未登录、JSON 坏了)。调用方据此决定「降级只扫码」还是「重试/中止」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairApiError {
    Http(u16),
    Adapter(AdapterError),
}

impl PairApiError {
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Http(status) => Some(*status),
            Self::Adapter(_) => None,
        }
    }
}

impl From<AdapterError> for PairApiError {
    fn from(error: AdapterError) -> Self {
        Self::Adapter(error)
    }
}

impl fmt::Display for PairApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Http(status) => write!(f, "HTTP {status}"),
            Self::Adapter(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for PairApiError {}

#[derive(Debug, Clone, Serialize)]
struct RemotePairRequest<'a> {
    public_key: &'a str,
    machine_name: &'a str,
    platform: &'a str,
    /// 这台电脑会核对批准方(§2.4.1)。老后台不认这个字段会 400 —— 那正是「降级只扫码」的信号。
    approver_check: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
pub struct RemotePairCreated {
    pub id: String,
    /// 后台从公钥算出的确认码(纯 6 位数字)。调用方必须和自己算的比对 —— 这正是确认码的意义。
    pub code: String,
    #[serde(default)]
    pub expires_at: String,
    /// 后台支持批准方绑定(approved 时会回批准方)。用来区分「后台旧」与「手机 App 旧」。
    #[serde(default)]
    pub approver_binding: bool,
    /// 这条请求登记成了「会核对批准方」(手机才看得到它)。
    #[serde(default)]
    pub approver_check: bool,
}

/// `GET remote/pair/{id}` 的回包。approved 且后台记下了批准方时,两个 approver_* 都有值。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq, Default)]
pub struct RemotePairStatus {
    pub status: String,
    /// 批准方手机的远程身份内容公钥(标准 base64,32 字节)。
    #[serde(default)]
    pub approver_public_key: String,
    /// 批准方的中继账号 id。
    #[serde(default)]
    pub approver_account_id: String,
}

impl RemotePairStatus {
    /// 两个字段都在且形状合法 = 可以交给运行时去核对。
    pub fn approver(&self) -> Option<(&str, &str)> {
        if self.approver_public_key.is_empty() || self.approver_account_id.is_empty() {
            return None;
        }
        Some((&self.approver_public_key, &self.approver_account_id))
    }
}

/// 内容公钥的形状:带填充的标准 base64、43 个字符 + `=`(解出来恰好 32 字节)。
/// 这个 crate 里没有 base64 依赖,只查形状;调用方落到运行时之前还会严格解一次。
fn valid_approver_public_key(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    bytes.len() == 44
        && bytes[43] == b'='
        && bytes[..43]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'+' || *b == b'/')
}

/// 中继账号 id 的形状(与后台 remoteRelayAccountShape、运行时那边一致):`[A-Za-z0-9_-]{1,128}`。
fn valid_approver_account_id(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= 128
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
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
    /// 配对专用的请求:非 2xx 原样带回状态码(见文件头),不按通用口径压成 `Unavailable`。
    fn pair_request<R: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<R, PairApiError> {
        let Some(token) = self.access_token.as_deref() else {
            return Err(PairApiError::Adapter(AdapterError::Unauthorized));
        };
        let (status, payload) = self.transport.request(method, path, token, body)?;
        if !(200..300).contains(&status) {
            return Err(PairApiError::Http(status));
        }
        serde_json::from_str(&payload)
            .map_err(|error| PairApiError::Adapter(AdapterError::InvalidResponse(error.to_string())))
    }

    /// 登记远程组件的一次性公钥,拿回请求 id 与后台算出的确认码。
    /// `approver_check` = 这台电脑会核对批准方(§2.4.1);桌面端与命令行一律传 true。
    pub fn remote_pair(
        &self,
        public_key: &str,
        machine_name: &str,
        platform: &str,
        approver_check: bool,
    ) -> Result<RemotePairCreated, PairApiError> {
        let body = serde_json::to_string(&RemotePairRequest {
            public_key,
            machine_name,
            platform,
            approver_check,
        })
        .map_err(|error| PairApiError::Adapter(AdapterError::InvalidConfiguration(error.to_string())))?;
        let created: RemotePairCreated =
            self.pair_request("POST", "/api/cli/auth/remote/pair", Some(&body))?;
        if !valid_pair_id(&created.id) || !is_six_digits(&created.code) {
            return Err(PairApiError::Adapter(AdapterError::InvalidResponse(
                "remote pair response is malformed".into(),
            )));
        }
        Ok(created)
    }

    /// 查自己发起的配对请求:pending / approved / rejected / expired / cancelled,
    /// approved 时可能带批准方。批准方公钥或账号 id 形状不对 → 两个一起清空
    /// (调用方按「缺失」中止,绝不拿坏值去核对)。
    pub fn remote_pair_status(&self, id: &str) -> Result<RemotePairStatus, PairApiError> {
        if !valid_pair_id(id) {
            return Err(PairApiError::Adapter(AdapterError::InvalidConfiguration(
                "remote pair id is invalid".into(),
            )));
        }
        let path = format!("/api/cli/auth/remote/pair/{id}");
        let mut response: RemotePairStatus = self.pair_request("GET", &path, None)?;
        match response.status.as_str() {
            REMOTE_PAIR_PENDING | REMOTE_PAIR_APPROVED | REMOTE_PAIR_REJECTED
            | REMOTE_PAIR_EXPIRED | REMOTE_PAIR_CANCELLED => {}
            _ => {
                return Err(PairApiError::Adapter(AdapterError::InvalidResponse(
                    "remote pair status is malformed".into(),
                )))
            }
        }
        if !valid_approver_public_key(&response.approver_public_key)
            || !valid_approver_account_id(&response.approver_account_id)
        {
            response.approver_public_key.clear();
            response.approver_account_id.clear();
        }
        Ok(response)
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
    approver_check: bool,
) -> Result<RemotePairCreated, PairApiError> {
    fresh_adapter()?.remote_pair(public_key, machine_name, platform, approver_check)
}

pub fn remote_pair_status(id: &str) -> Result<RemotePairStatus, PairApiError> {
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
            r#"{"id":"abcDEF123_-xyzabcDEF123","code":"848873","expires_at":"2026-09-19T12:10:00Z","approver_binding":true,"approver_check":true}"#,
        );
        let created = a
            .remote_pair(
                "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=",
                "DESKTOP-ABC",
                "windows",
                true,
            )
            .unwrap();
        assert_eq!(created.code, "848873");
        assert!(created.approver_binding && created.approver_check);
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
                "platform": "windows",
                "approver_check": true
            })
        );
    }

    /// 老后台不回 approver_binding / approver_check:默认 false,调用方据此撤回、只扫码。
    #[test]
    fn missing_approver_flags_default_to_false() {
        let created = adapter(
            200,
            r#"{"id":"abcDEF123_-xyzabcDEF123","code":"848873"}"#,
        )
        .remote_pair("k", "m", "windows", true)
        .unwrap();
        assert!(!created.approver_binding && !created.approver_check);
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
                    adapter(200, body).remote_pair("k", "m", "windows", true),
                    Err(PairApiError::Adapter(AdapterError::InvalidResponse(_)))
                ),
                "{body}"
            );
        }
    }

    /// 状态码原样带回 —— 调用方要靠它分「后台明确没建记录」与「可能建了,得重试」。
    #[test]
    fn pair_errors_keep_the_http_status() {
        for status in [400u16, 401, 403, 404, 429, 500, 501, 502] {
            assert_eq!(
                adapter(status, "")
                    .remote_pair("k", "m", "windows", true)
                    .unwrap_err(),
                PairApiError::Http(status)
            );
        }
    }

    #[test]
    fn status_accepts_only_the_five_states() {
        for status in ["pending", "approved", "rejected", "expired", "cancelled"] {
            let a = adapter(200, &format!(r#"{{"status":"{status}"}}"#));
            assert_eq!(a.remote_pair_status("abc_DEF-1").unwrap().status, status);
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
    fn approved_status_carries_the_approver() {
        let st = adapter(
            200,
            r#"{"status":"approved","approver_public_key":"AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=","approver_account_id":"acc_123-x"}"#,
        )
        .remote_pair_status("abc")
        .unwrap();
        assert_eq!(
            st.approver(),
            Some((
                "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=",
                "acc_123-x"
            ))
        );
    }

    /// 形状不对 = 两个一起清空:绝不拿坏值去核对,调用方按「缺失」中止。
    #[test]
    fn malformed_approver_fields_are_dropped_together() {
        for body in [
            // 无填充 / 长度不对
            r#"{"status":"approved","approver_public_key":"AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8","approver_account_id":"acc1"}"#,
            // base64url 字符
            r#"{"status":"approved","approver_public_key":"AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh_=","approver_account_id":"acc1"}"#,
            // 账号 id 有非法字符
            r#"{"status":"approved","approver_public_key":"AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=","approver_account_id":"acc/1"}"#,
            // 只有一半
            r#"{"status":"approved","approver_public_key":"AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8="}"#,
            r#"{"status":"approved","approver_account_id":"acc1"}"#,
        ] {
            let st = adapter(200, body).remote_pair_status("abc").unwrap();
            assert_eq!(st.approver(), None, "{body}");
            assert!(st.approver_public_key.is_empty() && st.approver_account_id.is_empty());
        }
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
        // Bearer 设备令牌;body 与命令行一样发 `{}`(后台允许空 body)
        assert_eq!((calls[0].2.as_str(), calls[0].3.as_str()), ("rct_testtoken", "{}"));
        // 已是终态时后台原样回当前状态(仍是 200):同样算撤回成功
        assert!(adapter(200, r#"{"status":"approved"}"#).remote_pair_cancel("abc").is_ok());
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
            a.remote_pair("k", "m", "windows", true).unwrap_err(),
            PairApiError::Adapter(AdapterError::Unauthorized)
        );
        assert!(a.transport_calls().is_empty());
    }

    impl Adapter<FakeTransport> {
        fn transport_calls(&self) -> Vec<(String, String, String, String)> {
            self.transport.calls.borrow().clone()
        }
    }
}
