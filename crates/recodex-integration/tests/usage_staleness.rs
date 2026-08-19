//! 额度读取失败时,「过期」这件事必须说得出原因,而且要记得住。
//!
//! 这两条是排查「刷新额度不刷新」时被逼出来的:实测客户端返回
//! `stale=true` + `refresh_error=null`,这个组合看着像"谁都没干过",
//! 查下来是**一次失败的 GET** —— 它把数据标成过期却不写原因。
//! 光有 stale 没有原因,面板只能说一句"可能不是最新",帮不上任何忙。

use std::cell::RefCell;

use recodex_integration::{Adapter, AdapterError, Transport};

/// 先成功一次(把缓存喂上),之后一律失败 —— 复现"服务端挂了但本地还有旧数据"。
struct FlakyTransport {
    ok_body: String,
    calls: RefCell<u32>,
    fail_after: u32,
    seen: RefCell<Vec<String>>,
}

impl FlakyTransport {
    fn new(ok_body: &str, fail_after: u32) -> Self {
        Self {
            ok_body: ok_body.to_string(),
            calls: RefCell::new(0),
            fail_after,
            seen: RefCell::new(Vec::new()),
        }
    }
}

impl Transport for FlakyTransport {
    fn request(
        &self,
        method: &str,
        path: &str,
        _access_token: &str,
        _body: Option<&str>,
    ) -> Result<(u16, String), AdapterError> {
        self.seen.borrow_mut().push(format!("{method} {path}"));
        let mut calls = self.calls.borrow_mut();
        *calls += 1;
        if *calls <= self.fail_after {
            Ok((200, self.ok_body.clone()))
        } else {
            // 503 在客户端映射为 Unavailable —— 正是会走缓存回落的那类错误
            Ok((503, "{}".into()))
        }
    }
}

const USAGE_OK: &str = r#"{
  "account_type": "shared",
  "available": 96.0,
  "total": 100.0,
  "used": 4.0,
  "windows": [{"window": "7d", "limit": 100.0, "used": 4.0, "remaining": 96.0}],
  "refreshed_at": "2026-08-18T20:21:41+08:00",
  "source": "sub2api_official_account",
  "stale": false
}"#;

fn authed(transport: FlakyTransport) -> Adapter<FlakyTransport> {
    let mut adapter = Adapter::new(transport, "https://api.example.com").unwrap();
    adapter
        .set_access_token("rct_usage_staleness_test".into())
        .unwrap();
    adapter
}

#[test]
fn failed_plain_read_marks_stale_and_says_why() {
    let mut adapter = authed(FlakyTransport::new(USAGE_OK, 1));

    let fresh = adapter.usage(false).expect("第一次应成功");
    assert!(!fresh.stale, "刚拿到的数据不该是过期的");
    assert!(fresh.refresh_error.is_none());

    let fallback = adapter.usage(false).expect("失败时应回落到缓存而不是报错");
    assert!(fallback.stale, "回落到旧数据就必须标成过期");
    let reason = fallback
        .refresh_error
        .as_ref()
        .expect("非刷新读取失败也要写明原因 —— 只标 stale 不说为什么,面板没法解释");
    assert_eq!(reason.code, "usage_unavailable");
}

#[test]
fn failed_refresh_keeps_its_own_reason_code() {
    let mut adapter = authed(FlakyTransport::new(USAGE_OK, 1));
    adapter.usage(false).expect("先喂上缓存");

    let fallback = adapter.usage(true).expect("刷新失败也应回落");
    assert!(fallback.stale);
    // 两条路径要能区分:是"刷新没成功"还是"连读都读不到"
    assert_eq!(
        fallback.refresh_error.expect("刷新失败要有原因").code,
        "refresh_unavailable"
    );
}
