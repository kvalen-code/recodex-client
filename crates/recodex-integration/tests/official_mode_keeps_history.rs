//! 切到官方模式之后,**历史对话必须还能打开**。
//!
//! 单独一个文件、只放一个测试 —— 和 official_mode_round_trip.rs 同样的理由:
//! 这里要改 `USERPROFILE`/`HOME` 这类进程级环境变量,和别的测试同进程跑会互相踩。
//! (我第一次就是把它塞进那个文件里,结果把原有的测试搞挂了。)

use recodex_integration::codexcfg;
use recodex_integration::officialmode;

const OUR_AUTH: &str = r#"{"tokens":{"access_token":"rct_recodex_token"}}"#;
const GATEWAY_BLOCK: &str = "model_provider = \"recodex\"

[model_providers.recodex]
name = \"ReCodex\"
base_url = \"https://sg.gw.example.dev/backend-api/codex\"
wire_api = \"responses\"
env_key = \"RECODEX_KEY\"";

/// 切到官方模式**不能**把 provider 定义也删掉。
///
/// 实测到的坏例:用户切回官方 ChatGPT 之后,所有历史对话都打不开,报
/// 「ChatGPT 无法加载 config.toml … Model provider `recodex` not found」。
/// 原因是 Codex 把每个会话当时用的 provider 名记在 rollout 文件里
/// (`payload.model_provider = "recodex"`),定义一删,旧会话就解析不到了。
/// 新对话没事 —— 它用的是官方 provider,所以问题只在历史对话上暴露。
#[test]
fn switching_to_official_keeps_provider_definition_for_old_threads() {
    let sandbox = std::env::temp_dir().join(format!("recodex-officialmode-keep-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sandbox);
    std::fs::create_dir_all(sandbox.join(".codex")).unwrap();

    // SAFETY:本文件的测试串行跑,没有并发读写这些变量的线程。
    unsafe {
        std::env::set_var("USERPROFILE", &sandbox);
        std::env::set_var("HOME", &sandbox);
        std::env::set_var("LOCALAPPDATA", sandbox.join("localappdata"));
        std::env::set_var("APPDATA", sandbox.join("appdata"));
        // 没有这一句,下面的 apply_login 会用 setx 把**本机真实的** RECODEX_KEY
        // 永久写成 "sk-recodex" —— USERPROFILE 重定向管不到注册表。
        std::env::set_var("RECODEX_ENV_SANDBOX", sandbox.join("env"));
    }

    let auth_path = codexcfg::auth_path().unwrap();
    std::fs::write(&auth_path, r#"{"tokens":{"access_token":"user_own_chatgpt"}}"#).unwrap();
    codexcfg::apply_login(GATEWAY_BLOCK, OUR_AUTH, codexcfg::SUB2API_ENV_KEY, "sk-recodex").unwrap();

    officialmode::switch_to_official().unwrap();
    assert!(officialmode::is_official_mode(), "应进入官方模式");

    let config = std::fs::read_to_string(codexcfg::config_path().unwrap()).unwrap_or_default();
    assert!(
        config.contains("[model_providers.recodex]"),
        "provider 定义被删了 —— 历史对话会报 `Model provider recodex not found` 并打不开:{config}"
    );
    assert!(
        !config.contains("model_provider = \"recodex\""),
        "默认 provider 还指着 recodex —— 新对话不会走官方账号:{config}"
    );
    assert_eq!(
        std::fs::read_to_string(&auth_path).unwrap(),
        r#"{"tokens":{"access_token":"user_own_chatgpt"}}"#,
        "切到官方后仍应还原用户自己的 auth.json"
    );

    // 切回来之后一切照旧
    officialmode::switch_to_recodex().unwrap();
    let back = std::fs::read_to_string(codexcfg::config_path().unwrap()).unwrap_or_default();
    assert!(back.contains("model_provider = \"recodex\""), "切回来应恢复默认 provider:{back}");

    let _ = std::fs::remove_dir_all(&sandbox);
}
