//! 客户端的**认证类**调用不得使用 `/api/v1/auth/*`。
//!
//! sub2api 自己就有 `/api/v1/auth/login|refresh|logout`（网页会话），而反代按前缀
//! 分流：`/api/v1/auth/*` 不在给 recodex-auth 的白名单里，放进去会抢掉网页登录。
//! 于是任何走这些路径的桌面端请求都会**静默落到 sub2api 上** —— 2026-08-26 实测
//! `/api/v1/auth/config` 返回 404、`/api/v1/auth/login` 返回 sub2api 的 400，
//! 也就是说桌面端的令牌刷新与服务端登出从来没有真正生效过（登出只清了本地，
//! 服务端设备还活着，连同它的网关 Key）。
//!
//! `/api/cli/auth/` 前缀反代整体放行，且不与任何 sub2api 路由撞名。
//!
//! 其余 `/api/v1/*`（account、usage、gateways、client、diagnostics）都在反代
//! 白名单里逐条列着，不受此限。

const ADAPTER: &str = include_str!("../src/lib.rs");

#[test]
fn auth_calls_never_use_the_colliding_v1_prefix() {
    let offenders: Vec<&str> = ADAPTER
        .lines()
        .filter(|line| {
            let code = line.split("//").next().unwrap_or("");
            code.contains("\"/api/v1/auth/")
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "这些调用会静默落到 sub2api（网页会话）而不是 recodex-auth，\
         请改用 /api/cli/auth/ 前缀：\n{}",
        offenders.join("\n")
    );
}

/// 反过来钉住：认证三件事确实都还在，别在搬家时搬丢了。
#[test]
fn the_authenticated_client_calls_are_all_present() {
    for path in [
        "/api/cli/auth/start",
        "/api/cli/auth/poll",
        "/api/cli/auth/refresh",
        "/api/cli/auth/logout",
        "/api/cli/auth/config",
    ] {
        assert!(
            ADAPTER.contains(&format!("\"{path}\"")),
            "适配器应调用 {path}"
        );
    }
}
