//! 托管块里那行 `x-openai-actor-authorization` 不是给上游看的,是**开关**。
//!
//! Codex 决定要不要注册本地 `image_gen` / `web_search` 工具,看的是:
//!
//! ```text
//! provider.is_openai() || provider.requires_openai_auth || provider.uses_openai_actor_authorization()
//! ```
//!
//! 三个里只有第三个能用(另两个会改请求整形、或废掉 env_key 认证)。少了这一行,
//! 客户端连工具都不声明 —— 用户能生成图片(服务端 hosted 桥接),但**界面一张都
//! 显示不出来**,因为 Codex 只渲染它自己那个工具的结果。而且没有任何报错:
//! 模型说「已生成」,界面上什么都没有。线上 2026-08-29 查了整晚就是这个。
//!
//! 值本身无意义,网关按头名把它挡在白名单外、根本不会转发给上游。
//!
//! 这条守卫存在的理由:同一份契约有两个实现(Go 命令行 / Rust 桌面端),
//! 桌面端这份已经落后过一次 —— 命令行早就有这行,桌面端没有,而用户的
//! config.toml 是桌面端写的。

use recodex_integration::codexcfg;

#[test]
fn managed_block_declares_the_actor_authorization_header() {
    let rendered = codexcfg::render_sub2api_block("https://example.test/backend-api/codex");

    assert!(
        rendered.contains("x-openai-actor-authorization"),
        "少了这一行,客户端不注册本地 image_gen,生成的图在界面上一张都看不到:\n{rendered}"
    );
    // requires_openai_auth 必须**不出现**或为 false:置 true 会强制弹 OpenAI 登录页、
    // 改读 auth.json —— 那会废掉 env_key 认证,比不显示图片严重得多。
    assert!(
        !rendered.contains("requires_openai_auth = true"),
        "requires_openai_auth = true 会废掉 env_key 认证:\n{rendered}"
    );
    // 密钥必须走环境变量。experimental_bearer_token 会把明文密钥写进 config.toml,
    // 而这个文件用户会截图、会贴进工单。
    assert!(
        !rendered.contains("experimental_bearer_token"),
        "密钥不能明文写进 config.toml:\n{rendered}"
    );
}
