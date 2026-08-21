//! 设备在网页端被撤销之后,面板必须还能走回登录。
//!
//! 实测踩到的:用户在网页端撤销了当前设备,客户端从此一直显示
//! 「ReCodex login is required」,而面板上**只有一个「重试」按钮** ——
//! 重试多少次都是同一个 401,登录入口永远出不来,人就被锁死在面板里。
//!
//! 原因在读状态那一层:401 被当成普通错误返回(`status: "error"`),
//! 而面板只有拿到 `signed_out` 才会画「登录 ReCodex」。

use recodex_integration::desktop::{any_unauthorized, credentials_rejected};
use recodex_integration::AdapterError;

#[test]
fn a_401_from_any_request_means_signed_out_not_error() {
    let unauthorized = AdapterError::Unauthorized;
    let other = AdapterError::Unavailable;

    // 撤销设备后先炸的是哪个请求不一定 —— 三个位置都得认
    assert!(any_unauthorized(&[Some(&unauthorized), None, None]), "usage 401 没被认出来");
    assert!(any_unauthorized(&[None, Some(&unauthorized), None]), "account 401 没被认出来");
    assert!(any_unauthorized(&[None, None, Some(&unauthorized)]), "gateways 401 没被认出来");

    // 别的故障不能被误判成"未登录",否则一次网络抖动就把用户踢去重登
    assert!(!any_unauthorized(&[Some(&other), Some(&other), Some(&other)]));
    assert!(!any_unauthorized(&[None, None, None]));
}

#[test]
fn the_signed_out_payload_is_what_the_panel_needs_to_show_a_login_button() {
    let payload = credentials_rejected();

    // 面板按 status 分支:signed_out 才画登录按钮,error 只画重试
    assert_eq!(
        payload["status"], "signed_out",
        "回 error 的话面板只会给一个重试按钮,用户走不到登录入口"
    );
    // 光说"未登录"会让用户以为是自己没登过 —— 得说清楚是被撤销了
    let notice = payload["notice"].as_str().unwrap_or_default();
    assert!(!notice.is_empty(), "没有 notice,用户不知道为什么突然掉登录");
    assert!(
        notice.contains("撤销") || notice.contains("失效"),
        "notice 没解释原因:{notice}"
    );
}
