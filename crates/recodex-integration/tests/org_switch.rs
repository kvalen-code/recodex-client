use recodex_integration::{Adapter, AdapterError, Transport};
use std::cell::RefCell;

struct FakeTransport {
    calls: RefCell<Vec<String>>,
    response: (u16, String),
}

impl Transport for FakeTransport {
    fn request(
        &self,
        method: &str,
        path: &str,
        _access_token: &str,
        _body: Option<&str>,
    ) -> Result<(u16, String), AdapterError> {
        self.calls.borrow_mut().push(format!("{method} {path}"));
        Ok(self.response.clone())
    }
}

fn adapter_with(response: (u16, String)) -> Adapter<FakeTransport> {
    let mut adapter = Adapter::new(
        FakeTransport {
            calls: RefCell::new(vec![]),
            response,
        },
        "https://api.example",
    )
    .unwrap();
    adapter.set_access_token("rct_valid_token".into()).unwrap();
    adapter
}

#[test]
fn organizations_parses_upstream_payload() {
    // recodex-auth /api/v1/org 的实际响应体。
    let body = r#"{"organizations":[
        {"id":17,"kind":"personal","name":"kvalen@qq.com","member_count":2,"plan_name":"20x 独享","is_current":true},
        {"id":74,"kind":"team","name":"666","member_count":1}
    ]}"#;
    let orgs = adapter_with((200, body.into())).organizations().unwrap();
    assert_eq!(orgs.len(), 2);
    assert_eq!(orgs[0].id, 17);
    assert!(orgs[0].is_current, "is_current 没解析出来，切换器标不出当前组织");
    // 没订阅的组织仍然列出,plan_name 为空 —— 用户要看到「我在这里但没额度」。
    assert_eq!(orgs[1].plan_name, "");
    assert!(!orgs[1].is_current);
}

#[test]
fn organizations_tolerates_empty_list() {
    // 一个组织都没有时也要能解析 —— 那恰恰是最该把界面画出来的时候。
    let orgs = adapter_with((200, r#"{}"#.into())).organizations().unwrap();
    assert!(orgs.is_empty());
}

#[test]
fn switch_organization_returns_gateway_key() {
    let body = r#"{"org_id":17,"org_name":"kvalen@qq.com","plan_name":"20x 独享","gateway_key":"sk-abc"}"#;
    let switched = adapter_with((200, body.into()))
        .switch_organization(17)
        .unwrap();
    assert_eq!(switched.org_id, 17);
    assert_eq!(switched.gateway_key, "sk-abc");
}

/// 服务端说成功却没给 Key 时必须当失败。
///
/// 放行的话调用方会把空串写进用户环境,把一个能用的配置改成不能用的 ——
/// 而且失败发生在下一次 Codex 请求时,那时已经很难联想到是切换那一步。
#[test]
fn switch_organization_rejects_empty_key() {
    let body = r#"{"org_id":17,"org_name":"x","plan_name":"y","gateway_key":"   "}"#;
    let err = adapter_with((200, body.into()))
        .switch_organization(17)
        .unwrap_err();
    assert!(
        format!("{err}").contains("no gateway key"),
        "空 Key 必须报错，实际: {err}"
    );
}

#[test]
fn switch_organization_rejects_invalid_id() {
    let err = adapter_with((200, "{}".into()))
        .switch_organization(0)
        .unwrap_err();
    assert!(format!("{err}").contains("invalid organization id"));
}
