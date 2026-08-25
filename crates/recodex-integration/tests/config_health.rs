//! `inspect_config` 的三项判断，每一项都对应一次**真实发生过的静默故障**：
//! 出问题时 Codex 不报错，只是悄悄不走 ReCodex。所以这里用真实形状的配置钉住。

use recodex_integration::codexcfg;

const BLOCK: &str = "model_provider = \"recodex\"\n\n[model_providers.recodex]\nname = \"ReCodex\"\nbase_url = \"https://gw.example.dev/backend-api/codex\"\nwire_api = \"responses\"\nenv_key = \"RECODEX_KEY\"";

#[test]
fn a_freshly_installed_block_is_healthy() {
    let content = codexcfg::install_block("model = \"gpt-5\"\n\n[experimental]\nfoo = true\n", BLOCK);
    let health = codexcfg::inspect_config(&content);
    assert!(health.is_healthy(), "刚装好的配置应体检通过: {health:?}\n{content}");
}

#[test]
fn a_config_without_our_block_is_reported_unmanaged() {
    let health = codexcfg::inspect_config("model = \"gpt-5\"\n\n[experimental]\nfoo = true\n");
    assert!(!health.managed);
    assert!(!health.is_healthy());
}

/// 1.2.54 把托管块追加到了文件末尾（用户机器上实测在第 185 行，而第一个表头在第 11 行）。
/// 按 TOML 规则，块里的顶层 `model_provider` 归属它上面那张表 —— 顶层等于没设，
/// Codex 悄悄走回官方 provider，**不报任何错**。
#[test]
fn a_block_appended_after_a_table_is_caught() {
    let content = format!(
        "model = \"gpt-5\"\n\n[experimental]\nfoo = true\n\n{}\n",
        format_args!("# >>> recodex managed block, do not edit >>>\n{BLOCK}\n# <<< recodex managed block <<<")
    );
    let health = codexcfg::inspect_config(&content);
    assert!(health.managed, "块确实在文件里");
    assert!(
        !health.before_first_table,
        "块排在 [experimental] 之后，必须判为不健康: {health:?}"
    );
    assert!(!health.is_healthy());
}

/// 顶层出现两个 `model_provider` 时，Codex 报的是
/// `duplicate key model_provider in document root`，而用户看到的是
/// `Model provider 'recodex' not found` —— 两者对不上号，极难自查。
#[test]
fn a_duplicate_top_level_model_provider_is_caught() {
    let content = format!(
        "model_provider = \"custom\"\n# >>> recodex managed block, do not edit >>>\n{BLOCK}\n# <<< recodex managed block <<<\n\n[experimental]\nfoo = true\n"
    );
    let health = codexcfg::inspect_config(&content);
    assert_eq!(health.top_level_model_provider, 2, "{health:?}");
    assert!(!health.is_healthy());
}

/// `[profiles.x]` 里的同名键不是顶层键，不能算进来 —— 误报会把好配置判成坏的，
/// 用户点「修复」反而被重写一遍。
#[test]
fn a_model_provider_inside_a_table_is_not_counted() {
    let content = codexcfg::install_block(
        "[profiles.work]\nmodel_provider = \"other\"\n",
        BLOCK,
    );
    let health = codexcfg::inspect_config(&content);
    assert_eq!(health.top_level_model_provider, 1, "{health:?}\n{content}");
    assert!(health.is_healthy());
}
