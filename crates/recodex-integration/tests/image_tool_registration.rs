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
    let rendered = codexcfg::render_sub2api_block("https://example.test/backend-api/codex", false);

    assert!(
        rendered.contains("x-openai-actor-authorization"),
        "少了这一行,客户端不注册本地 image_gen,生成的图在界面上一张都看不到:\n{rendered}"
    );
    // requires_openai_auth 必须**不出现**或为 false。2026-09-08 实测确认:置 true
    // 之后 Codex 改读 auth.json,而且**本地 image_gen 工具直接不注册了** ——
    // 即上面那个 OR 表达式在实机上并不成立,别照着源码注释推断。
    assert!(
        !rendered.contains("requires_openai_auth = true"),
        "requires_openai_auth = true 会让 image_gen 静默消失:\n{rendered}"
    );
    // 模板必须保持 env_key 形状:服务端下发的就是这个形状,两边不一致的话
    // `install_block` 每次都判定有变更、反复重写用户的 config.toml。
    //
    // 注意这**不是**禁止 bearer:真正落盘的那份会由 `inline_managed_key` 换成
    // experimental_bearer_token(理由见该函数)。这条守的是「替换只发生在写盘
    // 这一步」,别把它提前到模板里。
    assert!(
        !rendered.contains("experimental_bearer_token"),
        "模板要保持 env_key 形状,内联只在写盘时做:\n{rendered}"
    );
}
