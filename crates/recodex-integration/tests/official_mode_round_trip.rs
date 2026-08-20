//! 官方模式切换的**整链路**回归:登录 → 切到官方 → 切回来。
//!
//! 单独一个测试文件、只放一个测试:这里要改 `USERPROFILE` / `LOCALAPPDATA` 这类
//! 进程级环境变量,和别的测试并行跑会互相踩。
//!
//! 盯的是一条曾经真实存在的缺陷:快照只存了 config 和 env,漏了 `auth.json`。
//! 切走时 `restore_auth()` 会把我们的 `auth.json` 连同备份、归属标记一并删除,
//! 于是切回来之后 Codex 根本没有登录态 —— 而这个功能的全部卖点就是"切回来不用重登"。

use recodex_integration::codexcfg;
use recodex_integration::officialmode;

const OUR_AUTH: &str = r#"{"tokens":{"access_token":"rct_recodex_token"}}"#;
const GATEWAY_BLOCK: &str = "model_provider = \"recodex\"\n\n[model_providers.recodex]\nname = \"ReCodex\"\nbase_url = \"https://sg.gw.example.dev/backend-api/codex\"\nwire_api = \"responses\"\nenv_key = \"RECODEX_KEY\"";

#[test]
fn switching_to_official_and_back_keeps_the_user_logged_in() {
    let sandbox = std::env::temp_dir().join(format!("recodex-officialmode-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sandbox);
    std::fs::create_dir_all(sandbox.join(".codex")).unwrap();

    // SAFETY:本文件只有这一个测试,没有并发读写这些变量的线程。
    unsafe {
        std::env::set_var("USERPROFILE", &sandbox);
        std::env::set_var("HOME", &sandbox);
        std::env::set_var("LOCALAPPDATA", sandbox.join("localappdata"));
        std::env::set_var("APPDATA", sandbox.join("appdata"));
    }

    // 用户此前用过官方 Codex —— 留着他自己的 auth.json,切走时要能还原成它
    let auth_path = codexcfg::auth_path().unwrap();
    std::fs::write(&auth_path, r#"{"tokens":{"access_token":"user_own_chatgpt"}}"#).unwrap();

    // 1) 登录 ReCodex
    codexcfg::apply_login(GATEWAY_BLOCK, OUR_AUTH, codexcfg::SUB2API_ENV_KEY, "sk-recodex").unwrap();
    assert_eq!(std::fs::read_to_string(&auth_path).unwrap(), OUR_AUTH);
    assert!(!officialmode::is_official_mode());

    // 2) 切到官方模式
    officialmode::switch_to_official().unwrap();
    assert!(officialmode::is_official_mode(), "应进入官方模式");
    assert_eq!(
        std::fs::read_to_string(&auth_path).unwrap(),
        r#"{"tokens":{"access_token":"user_own_chatgpt"}}"#,
        "切到官方后应还原用户自己的 auth.json"
    );
    let config = std::fs::read_to_string(codexcfg::config_path().unwrap()).unwrap_or_default();
    // 注意这里钉的是**默认选择**没了,而不是定义没了。
    //
    // 原先钉的是「整块都不该留」,而那正是实测到的坏例:Codex 把每个会话当时用的
    // provider 名记在 rollout 文件里,定义一删,用户切回官方账号之后所有历史对话
    // 都打不开,报「Model provider `recodex` not found」。所以要改的是意图本身 ——
    // 定义留着(旧对话能继续),默认选择摘掉(新对话回官方)。
    // 详见 official_mode_keeps_history.rs。
    assert!(
        !config.contains("model_provider = \"recodex\""),
        "切到官方后默认 provider 不该还指着 recodex:{config}"
    );

    // 3) 切回 ReCodex —— 这是缺陷曾经暴露的地方
    officialmode::switch_to_recodex().unwrap();
    assert!(!officialmode::is_official_mode(), "应回到 ReCodex 模式");
    assert_eq!(
        std::fs::read_to_string(&auth_path).unwrap(),
        OUR_AUTH,
        "切回来必须恢复我们的 auth.json,否则用户还得重新登录"
    );
    let config = std::fs::read_to_string(codexcfg::config_path().unwrap()).unwrap();
    assert!(
        config.contains("model_providers.recodex"),
        "切回来必须恢复网关配置:{config}"
    );

    // 4) 再切一轮,确认状态机没有单向退化
    officialmode::switch_to_official().unwrap();
    officialmode::switch_to_recodex().unwrap();
    assert_eq!(
        std::fs::read_to_string(&auth_path).unwrap(),
        OUR_AUTH,
        "第二轮往返同样要保住登录态"
    );

    // 5) 卸载/登出走的是**丢弃**,不是还原 —— 两者结果必须相反
    officialmode::switch_to_official().unwrap();
    let after_switch = std::fs::read_to_string(codex_config()).unwrap_or_default();
    // 同上:撤掉的是**默认选择**,不是定义 —— 定义要留给历史对话。
    assert!(
        !after_switch.contains("model_provider = \"recodex\""),
        "切到官方后默认 provider 应已撤掉"
    );

    officialmode::discard_snapshot().unwrap();
    assert!(!officialmode::is_official_mode(), "丢弃后不该再认为在官方模式");
    let after_discard = std::fs::read_to_string(codex_config()).unwrap_or_default();
    assert!(
        !after_discard.contains("model_providers.recodex"),
        "丢弃**不能**把托管块装回去 —— 卸载路径上装回去就等于留了个指向已吊销网关的 Codex:{after_discard}"
    );
    assert!(
        !officialmode::load_snapshot().unwrap().is_some(),
        "快照文件应已删除"
    );

    // 6) 官方模式下换网关:只能改快照,不能碰活配置
    officialmode::switch_to_official().unwrap();
    let new_block = codexcfg::render_sub2api_block("https://jp.gw.example.dev/backend-api/codex");
    assert!(
        officialmode::stage_config_for_return(&new_block).unwrap(),
        "官方模式下应写进快照"
    );
    let during_official = std::fs::read_to_string(codex_config()).unwrap_or_default();
    assert!(
        !during_official.contains("model_providers.recodex"),
        "官方模式下换网关**不能**把托管块写进活配置 —— 那会让面板显示官方模式而 Codex 已走回 ReCodex:{during_official}"
    );

    officialmode::switch_to_recodex().unwrap();
    let after_return = std::fs::read_to_string(codex_config()).unwrap();
    assert!(
        after_return.contains("jp.gw.example.dev"),
        "切回来时应用的是官方模式期间选的新网关:{after_return}"
    );

    // 不在官方模式时该函数不接管,交回给调用方写活配置
    assert!(
        !officialmode::stage_config_for_return(&new_block).unwrap(),
        "非官方模式下不该接管写入"
    );

    // 7) 官方模式下重新登录 = 用户明确要用 ReCodex,快照必须被丢掉。
    //    留着的话 is_official_mode() 仍为真,一点"切回 ReCodex"就用陈旧的
    //    网关和 key 盖掉刚登录的配置。(login 路径走的就是这两步。)
    officialmode::switch_to_official().unwrap();
    assert!(officialmode::is_official_mode());
    officialmode::discard_snapshot().unwrap();
    codexcfg::apply_login(GATEWAY_BLOCK, OUR_AUTH, codexcfg::SUB2API_ENV_KEY, "sk-recodex2").unwrap();
    assert!(
        !officialmode::is_official_mode(),
        "登录之后不该还被认为在官方模式"
    );
    let after_login = std::fs::read_to_string(codex_config()).unwrap();
    assert!(
        after_login.contains("model_providers.recodex"),
        "登录应把托管块写进活配置:{after_login}"
    );

    let _ = std::fs::remove_dir_all(&sandbox);
}

fn codex_config() -> std::path::PathBuf {
    codexcfg::config_path().unwrap()
}
