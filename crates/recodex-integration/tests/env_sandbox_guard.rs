//! 任何会写持久环境变量的集成测试,都必须先把注册表关进沙箱。
//!
//! 背景:`USERPROFILE` / `HOME` 只能重定向**文件**写入。Windows 的
//! `set_user_env` 走 `setx`,写的是注册表 `HKCU\Environment` —— 不受任何
//! 环境变量重定向约束。2026-08-26 因此把开发者本机真实的 `RECODEX_KEY`
//! 永久改成了测试夹具值 `sk-recodex2`,Codex 一路 401,而且重启、重新登录
//! 都好不了(坏的是持久值),最后靠手工写回注册表才恢复。
//!
//! 这条守卫直接读同目录下的测试源码:谁调了会落到 setx 的接口,
//! 谁就必须设 `RECODEX_ENV_SANDBOX`。

use std::fs;

/// 会一路走到 `set_user_env` / `unset_user_env` 的入口。
const PERSISTS_ENV: [&str; 4] = [
    "apply_login(",
    "switch_to_official(",
    "switch_to_recodex(",
    "restore_all(",
];

/// 去掉行注释和字符串字面量的内容,只留真正会被编译执行的代码。
///
/// 为什么必须去掉字符串:这个仓库里有好几处「读自己源码做守卫」的测试,它们的
/// 断言里原样写着 `apply_login(` 这种名字。纯文本 contains 会把那些字面量当成真
/// 调用 —— 而一次假阳性就足以让人把整条守卫 `#[ignore]` 掉或直接删了,那比没有更糟。
///
/// 刻意不处理 raw string:漏报只是少一层保护(回到现状),误报却会让守卫被丢掉。
/// 两种错误的代价不对等。
///
/// ⚠️ 两个用它时必须知道的行为(都是踩出来的):
///  1. **`///` doc comment 也会被剥掉** —— 它同样以 `//` 开头。所以**别拿 doc
///     comment 当函数边界**:去找一个 `///` 开头的位置永远找不到,切片会一路
///     吃到文件末尾。用换行加 `pub fn `、或者缩进的右花括号来定界。
///  2. 它**不**切掉 `#[cfg(test)]` 段。想只扫生产代码的,自己先
///     `source.split("#[cfg(test)]").next()` —— 否则会匹配到测试里自己写的调用,
///     那正是「守卫永远绿」的经典成因。
fn executable_code(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    for line in source.lines() {
        if line.trim_start().starts_with("//") {
            continue;
        }
        let mut in_string = false;
        let mut escaped = false;
        for ch in line.chars() {
            match (in_string, escaped, ch) {
                (true, true, _) => escaped = false,
                (true, false, '\\') => escaped = true,
                (true, false, '"') => in_string = false,
                (true, false, _) => {}
                (false, _, '"') => in_string = true,
                (false, _, _) => out.push(ch),
            }
        }
        out.push('\n');
    }
    out
}

#[test]
fn executable_code_ignores_names_that_only_appear_inside_strings() {
    let source = "let x = \"apply_login(\"; codexcfg::restore_all();";
    let code = executable_code(source);
    assert!(!code.contains("apply_login("), "字符串字面量没被剥掉: {code}");
    assert!(code.contains("restore_all("), "真调用被误删了: {code}");
}

#[test]
fn tests_that_persist_env_must_sandbox_the_registry() {
    // 用 CARGO_MANIFEST_DIR 而不是 file!():后者是相对**工作区根**的路径,
    // 而测试进程的 CWD 是包目录,拼出来的相对路径打不开。
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut checked = 0;
    let mut offenders = Vec::new();

    for entry in fs::read_dir(&dir).expect("读 tests 目录") {
        let path = entry.expect("目录项").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        if path.file_name().and_then(|n| n.to_str()) == Some("env_sandbox_guard.rs") {
            continue;
        }
        let source = fs::read_to_string(&path).expect("读测试源码");
        // 注释和字符串字面量里的名字都不算真调用
        let code = executable_code(&source);
        if !PERSISTS_ENV.iter().any(|call| code.contains(call)) {
            continue;
        }
        checked += 1;
        // 注意用的是原始 source,不是剥过字符串的 code:设沙箱的写法必然是
        // `set_var("RECODEX_ENV_SANDBOX", …)` —— 变量名本来就在字符串里,
        // 拿剥离后的代码去找它永远找不到,会把守好的用例全判成违规。
        if !source.contains("RECODEX_ENV_SANDBOX") {
            offenders.push(path.file_name().unwrap().to_string_lossy().into_owned());
        }
    }

    assert!(
        checked > 0,
        "没扫到任何会写持久环境变量的测试 —— 守卫失效了(入口名改过?),\
         请更新 PERSISTS_ENV"
    );
    assert!(
        offenders.is_empty(),
        "这些测试会把开发者本机真实的 RECODEX_KEY 永久改成夹具值,\
         请在重定向 USERPROFILE 的同时设 RECODEX_ENV_SANDBOX:{offenders:?}"
    );
}

/// 同一道防线必须**也**盖住 `src/` 里的单测。
///
/// 上面那条只扫 `tests/` 目录,可 `#[cfg(test)] mod tests` 里的用例跑在同一台机器上,
/// 一样能把开发者真实的 `RECODEX_KEY` 写进注册表(Windows setx)或登录环境
/// (macOS launchctl)—— 那正是 2026-08-26 那次事故的路径:夹具值 `sk-recodex2`
/// 覆盖了真值,Codex 一路 INVALID_API_KEY,重启 / 重新登录 / 重装都好不了。
///
/// macOS 上这条尤其要紧:`mac-verify.yml` 会在**真机** runner 上跑
/// `cargo test -p recodex-integration`,漏一个就是往 runner 的登录会话里真写变量。
#[test]
fn unit_tests_that_persist_env_must_sandbox_too() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();

    for entry in fs::read_dir(&dir).expect("读 src 目录") {
        let path = entry.expect("目录项").path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let source = fs::read_to_string(&path).expect("读源码");
        // 只看 `#[cfg(test)]` 之后:之前是生产代码,它本来就该调这些入口。
        let Some((_, tests)) = source.split_once("#[cfg(test)]") else {
            continue;
        };
        let code = executable_code(tests);
        if !PERSISTS_ENV.iter().any(|call| code.contains(call)) {
            continue;
        }
        // 用 `tests` 的原文,不是剥过字符串的 code,也不是整个 source:
        //   - 剥过的找不到 —— 设沙箱的写法是 `set_var("RECODEX_ENV_SANDBOX", …)`,
        //     变量名本来就在字符串里;
        //   - 整个 source 会撞上生产代码里的 `const ENV_SANDBOX: &str =
        //     "RECODEX_ENV_SANDBOX"`(codexcfg.rs 就有),把没设沙箱的用例判成合规。
        if !tests.contains("RECODEX_ENV_SANDBOX") {
            offenders.push(path.file_name().unwrap().to_string_lossy().into_owned());
        }
    }

    // 这里**不要**断言 checked > 0:src 单测目前一个都不碰持久环境变量,
    // 那是好事,不是守卫失效。上面那条 tests/ 守卫已经在盯着入口名有没有改。
    assert!(
        offenders.is_empty(),
        "这些 src 单测会把开发者本机(以及 mac CI runner)真实的 RECODEX_KEY \
         改成夹具值,请在用例里设 RECODEX_ENV_SANDBOX:{offenders:?}"
    );
}

/// macOS 上写用户环境的两个入口(以及升级时的补丁路径)都必须经过 launchd。
///
/// 背景:mac 上 `RECODEX_KEY` 落在 `~/.codex/recodex/*.env`(0600),而那个文件
/// **只有走 ReCodex 启动器**时才会被 `refresh_key_env_from_user_scope` 读回进程
/// 环境。用户从 Dock / 访达 / 聚焦直接点开 Codex.app 时父进程是 launchd,环境里
/// 什么都没有 —— 线上 24h 内 5005 次 macOS 401 就是这么来的(占全部 401 的 86%)。
///
/// 这条守卫防的是「以后有人重构时把 launchd 那一半顺手删了」。它必须放在
/// `tests/` 而不是 codexcfg.rs 的单测里:那边没有 `executable_code`,纯文本
/// contains 连**被注释掉的**调用都算数 —— 实测过,把真调用注释掉守卫照样绿。
#[test]
fn macos_env_writes_must_also_go_through_launchd() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("codexcfg.rs");
    let source = fs::read_to_string(&path).expect("读 codexcfg.rs");
    let code = executable_code(&source);

    for (func, call) in [
        ("pub fn set_user_env", "mac_env::register_launchd("),
        ("pub fn unset_user_env", "mac_env::unregister_launchd("),
        // 存量用户那条路:升级后第一次启动就得把缺失的 LaunchAgent 补上,
        // 否则这一版之前登录过的 mac 会一直 401,直到用户自己想到重新登录 ——
        // 而没人会想到。
        (
            "pub fn refresh_key_env_from_user_scope",
            "mac_env::ensure_launchd_registered(",
        ),
    ] {
        let body = code
            .split_once(func)
            .unwrap_or_else(|| panic!("codexcfg.rs 里找不到 {func}"))
            .1;
        // 只看到下一个 pub 项为止,别把后面函数里的调用算进来。
        let body = body.split_once("\npub ").map_or(body, |(head, _)| head);
        assert!(
            body.contains(call),
            "{func} 的 macOS 分支没有调用 {call} —— 从 Dock 启动的 Codex 会重新 401"
        );
    }
}

/// launchctl 的错误信息里绝不能出现被设置的值 —— 那就是明文 API key。
///
/// `setenv` 的调用形如 `["setenv", "RECODEX_KEY", "<明文key>"]`,一旦把 args
/// 整个拼进错误信息,这个错误再被谁记进日志或抛给用户,密钥就跟着出去了。
/// 现在调用方都把它吞掉了,但那是**当下**的调用方 —— 守卫盯的是这一行本身。
#[test]
fn launchctl_errors_must_not_carry_the_value() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("codexcfg.rs");
    let source = fs::read_to_string(&path).expect("读 codexcfg.rs");
    let code = executable_code(&source);
    let run = code
        .split_once("fn run_launchctl")
        .expect("找不到 run_launchctl")
        .1;
    let run = &run[..run.find("
    }").map(|end| end + 6).unwrap_or(run.len())];

    assert!(
        !run.contains("args.join("),
        "run_launchctl 把整个 args 拼进了错误信息 —— 里面第三个参数是明文 API key"
    );
}

/// 两个防注入校验必须**真的被调用**。
///
/// 它们各自的单测只验函数本身 —— 把调用点删掉,那些测试照样绿(实测过)。
/// 而这两个值都来自服务端、都被纯字符串拼进 config.toml:
///   - `model_name_is_safe`  ← manifest 的 slug
///   - `base_url_is_safe`    ← 网关 endpoint
/// 少一处校验,一个带引号和换行的值就能往用户配置里塞一整张 provider 表。
#[test]
fn toml_injection_guards_are_actually_called() {
    // 先切掉测试段再剥注释。两个都必要:
    //   - 不切测试段,守卫会匹配到测试里自己写的 `base_url_is_safe(good)`;
    //   - `executable_code` 连 `///` 也一起剥掉(它同样以 `//` 开头),所以下面
    //     不能拿 doc comment 当函数边界,只能用 `pub fn`。
    // 这两点都是破坏验证时才暴露出来的 —— 一度是条假守卫。
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let production = |name: &str| {
        let source = fs::read_to_string(dir.join(name)).unwrap_or_else(|_| panic!("读 {name}"));
        executable_code(source.split("#[cfg(test)]").next().unwrap_or(&source))
    };
    let codexcfg = production("codexcfg.rs");
    let desktop = production("desktop.rs");

    // 写模型名的必经之路。
    let setter = codexcfg
        .split_once("pub fn set_managed_model")
        .expect("找不到 set_managed_model")
        .1;
    let setter = &setter[..setter.find("
pub fn ").unwrap_or(setter.len())];
    assert!(
        setter.contains("model_name_is_safe("),
        "set_managed_model 没校验模型名 —— manifest 的 slug 能直接注入 TOML"
    );

    // 写网关地址的两条路:pub API 自己防御,UI 入口还要挡住 staging。
    let router = codexcfg
        .split_once("pub fn route_through_gateway")
        .expect("找不到 route_through_gateway")
        .1;
    let router = &router[..router.find("
pub fn ").unwrap_or(router.len())];
    assert!(
        router.contains("base_url_is_safe("),
        "route_through_gateway 没校验网关地址"
    );
    assert!(
        desktop.contains("base_url_is_safe("),
        "route_codex_through_gateway 没在 stage_config_for_return 之前校验 ——          被注入的块会先进官方模式快照,切回来照样生效"
    );
}
