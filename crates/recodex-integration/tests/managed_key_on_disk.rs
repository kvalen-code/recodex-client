//! 端到端验证「密钥落到磁盘上到底是什么形状、什么权限」。
//!
//! 补的是 CODEXPP-HARDENING.md 附录 B.3 里自认没验到的两条：
//!   - 第 2 条：取钥匙那个矩阵全是 **CLI** 上的实证，桌面端这条路没单独跑过
//!   - 第 3 条：Rust 侧没有真实的权限断言，守卫只能扫源码
//!
//! 这里走的是**真实的落盘函数** `apply_config_with_key`，不是重新拼一遍逻辑 ——
//! 重拼就只能证明测试自己自洽，证明不了出货的那条路。
//!
//! 用 CODEX_HOME 隔离到临时目录，不碰用户真实的 ~/.codex。

use std::fs;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use recodex_integration::codexcfg;

/// CODEX_HOME 是进程级环境变量，并发改会互相打架。
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

struct SandboxedHome {
    dir: PathBuf,
    prev: Option<std::ffi::OsString>,
}

impl SandboxedHome {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "recodex-keydisk-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).expect("建临时 CODEX_HOME");
        let prev = std::env::var_os(codexcfg::CODEX_HOME_ENV);
        std::env::set_var(codexcfg::CODEX_HOME_ENV, &dir);
        Self { dir, prev }
    }

    fn config(&self) -> PathBuf {
        self.dir.join("config.toml")
    }
}

impl Drop for SandboxedHome {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(v) => std::env::set_var(codexcfg::CODEX_HOME_ENV, v),
            None => std::env::remove_var(codexcfg::CODEX_HOME_ENV),
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

const TEST_KEY: &str = "sk-live-e2e-do-not-use";

/// 🔴 落盘后必须同时满足三条：
///   1. 块里是 experimental_bearer_token，不是 env_key
///      —— env_key 依赖环境变量，macOS 从程序坞启动读不到，这是线上最大一类 401
///   2. 文件权限 0600（unix）
///      —— 明文长期凭据用 0644 写出，同机其他用户直接可读
///   3. 密钥确实在文件里
#[test]
fn managed_key_lands_inlined_and_private() {
    let _guard = env_lock().lock().unwrap();
    let home = SandboxedHome::new("inline");

    let block = codexcfg::render_sub2api_block("https://api.recodex.dev/backend-api/codex", false);
    codexcfg::apply_config_with_key(&block, TEST_KEY).expect("落盘应当成功");

    let body = fs::read_to_string(home.config()).expect("配置文件应当存在");

    assert!(
        body.contains(&format!("experimental_bearer_token = \"{TEST_KEY}\"")),
        "密钥应当以 experimental_bearer_token 内联落盘，实际内容：\n{body}"
    );
    assert!(
        !body.contains("env_key = "),
        "内联之后不该再留 env_key —— 两者并存时 Codex 会硬性要求环境变量，本地就失败：\n{body}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(home.config()).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "含明文长期凭据的 config.toml 必须是 0600，实际 {mode:o}"
        );
    }
}

/// 🔴 重复落盘不得把权限放回去。
///
/// 线上那个 P0 就是这么发生的：某条重写路径没带 secret 标记，
/// 于是第一次写是 0600、之后某次整篇重写又变回 0644。
/// 判据已经挪进 write_atomic 按**内容**判定，这条守住它不再回退。
#[test]
fn rewriting_keeps_permissions_locked_down() {
    let _guard = env_lock().lock().unwrap();
    let home = SandboxedHome::new("rewrite");

    let block = codexcfg::render_sub2api_block("https://api.recodex.dev/backend-api/codex", false);
    codexcfg::apply_config_with_key(&block, TEST_KEY).expect("首次落盘");

    // 再写两次，模拟「启动时发现推荐模型变了就整篇重写」那条路。
    for _ in 0..2 {
        let again =
            codexcfg::render_sub2api_block("https://jp.gw.recodex.dev/backend-api/codex", true);
        codexcfg::apply_config_with_key(&again, TEST_KEY).expect("重写落盘");
    }

    let body = fs::read_to_string(home.config()).unwrap();
    assert!(
        body.contains("experimental_bearer_token"),
        "重写后密钥形态不该退化：\n{body}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(home.config()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "重写之后权限退回了 {mode:o}");
    }
}

/// 🔴 supports_websockets 与密钥内联必须能共存。
///
/// 两个改动落在同一个块上、由不同的人写，合并后没人验过组合形态。
/// 这条就是那次验证。
#[test]
fn websockets_flag_and_inlined_key_coexist() {
    let _guard = env_lock().lock().unwrap();
    let home = SandboxedHome::new("combo");

    let block = codexcfg::render_sub2api_block("https://api.recodex.dev/backend-api/codex", true);
    codexcfg::apply_config_with_key(&block, TEST_KEY).expect("落盘");

    let body = fs::read_to_string(home.config()).unwrap();
    assert!(
        body.contains("supports_websockets = true"),
        "WS 开关丢了：\n{body}"
    );
    assert!(
        body.contains("experimental_bearer_token"),
        "密钥没内联：\n{body}"
    );
    // 读回也要认得出来 —— 切网关时本地重渲染靠它保住开关。
    // 用 current_supports_websockets 而不是自己切块：它就是生产里切网关那条路
    // 真正调用的函数，从整份 config.toml 读，连「怎么定位托管块」都一起验到。
    assert!(
        codexcfg::current_supports_websockets(),
        "读回 supports_websockets 失败，切一次网关就会被静默打回 HTTP"
    );
}

/// 🔴 跨语言一致性：env_key 行的判定边界，两侧必须逐个同判。
///
/// 历史上这里分叉过两次，两次都导致「同一份托管块从 CLI 和从桌面端写出来是
/// 两个不同的文件」，而写进去的是**长期凭据**：
///   - 第一次：Go 用 ReplaceAllString 全量替换，块里有第二个 provider 时
///     把网关 Key 写进第三方 provider 的槽位（那行 base_url 指向别人的服务器）
///   - 第二次（2026-09-08 合并前审计查出）：Go 的正则要求闭合引号，
///     Rust 只检查「= 后面以引号开头」，于是 `env_key = "K`（缺闭合引号）
///     在两侧结论相反
///
/// 下面这张表是两侧的**共同契约**。Go 侧对应的断言在
/// internal/clientcfg 的测试里，改任何一侧都要同时改另一侧。
#[test]
fn env_key_line_boundaries_match_go_side() {
    // (输入块, 是否应当替换)
    let cases: &[(&str, bool)] = &[
        ("[p]\nenv_key = \"RECODEX_KEY\"\n", true),
        // 缺闭合引号：坏 TOML，两侧都必须拒绝。
        ("[p]\nenv_key = \"RECODEX_KEY\n", false),
        // 两行 env_key：拿不准就整个不换。
        ("[p]\nenv_key = \"A\"\nenv_key = \"B\"\n", false),
        // 注释掉的不算。
        ("[p]\n#env_key = \"RECODEX_KEY\"\n", false),
        // TOML 里单引号是字面量串，我们只认双引号形态。
        ("[p]\nenv_key = 'RECODEX_KEY'\n", false),
        // 等号两侧没空格照样认。
        ("[p]\nenv_key=\"RECODEX_KEY\"\n", true),
        // tab 缩进照样认，且缩进要原样保留。
        ("[p]\n\tenv_key = \"RECODEX_KEY\"\n", true),
        // 行尾注释：整行被替换掉（注释一起没了），两侧行为一致即可。
        ("[p]\nenv_key = \"K\" # 尾注释\n", true),
    ];

    for (block, want) in cases {
        let got = codexcfg::inline_managed_key(block, "sk-probe").is_some();
        assert_eq!(
            got, *want,
            "env_key 行判定与 Go 侧不一致，输入：{block:?}（期望替换={want}）"
        );
    }
}

/// tab 缩进必须原样保留 —— 换行/缩进被顺手改掉会让用户的 diff 噪声很大，
/// 也可能碰到我们没打算碰的行。
#[test]
fn inline_preserves_indent_and_line_endings() {
    let crlf = "[p]\r\n\tenv_key = \"RECODEX_KEY\"\r\nname = \"x\"\r\n";
    let got = codexcfg::inline_managed_key(crlf, "sk-probe").expect("应当替换");
    assert!(got.contains("\r\n"), "CRLF 行尾被改成 LF 了：{got:?}");
    assert!(
        got.contains("\texperimental_bearer_token"),
        "tab 缩进丢了：{got:?}"
    );
    assert!(got.contains("name = \"x\""), "其余行被动过：{got:?}");
}

/// 🔴 M1：注释标记被吃掉之后，读回必须仍然读得到（与 Go 侧 ManagedBody 同一策略）。
///
/// 标记丢失是常态：config.toml 有第三个写入方，Codex++ 重新序列化整份文件时会丢掉
/// 注释标记（共享语料里专门有 markers-lost 用例）。以标记为前提的后果不是「读不到」
/// 而是**静默读错** —— 本地重渲染时把 supports_websockets 写成 false，
/// 用户切一次网关就被打回 HTTP，只会觉得「忽然变慢了」。
#[test]
fn read_back_survives_lost_markers() {
    let _guard = env_lock().lock().unwrap();
    let home = SandboxedHome::new("nomarkers");

    // 手工写一份**没有任何 recodex 标记**、但确实有我们那张表的 config.toml，
    // 模拟 Codex++ 重新序列化之后的形态。
    let no_markers = "model = \"gpt-5.6\"\nmodel_provider = \"recodex\"\n\n\
[model_providers.recodex]\nname = \"ReCodex\"\n\
base_url = \"https://gw.recodex.dev/backend-api/codex\"\n\
supports_websockets = true\n\
experimental_bearer_token = \"sk-live-x\"\n\n\
[mcp_servers.foo]\ncommand = \"bar\"\n";
    fs::write(home.config(), no_markers).expect("写测试配置");

    assert!(
        codexcfg::current_supports_websockets(),
        "标记丢失后 supports_websockets 读成了 false —— 切一次网关就会被静默打回 HTTP"
    );
}

/// 文件里根本没有我们的表时，回落必须老实报 false，不能瞎猜。
#[test]
fn read_back_fallback_reports_absence() {
    let _guard = env_lock().lock().unwrap();
    let home = SandboxedHome::new("foreign");
    fs::write(
        home.config(),
        "model = \"x\"\n\n[model_providers.vendor]\nsupports_websockets = true\n",
    )
    .expect("写测试配置");

    assert!(
        !codexcfg::current_supports_websockets(),
        "没有我们的表时不该把别人 provider 的开关当成我们的"
    );
}
