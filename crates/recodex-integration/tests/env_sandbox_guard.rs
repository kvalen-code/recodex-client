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
        // 注释掉的调用不算
        let code: String = source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        if !PERSISTS_ENV.iter().any(|call| code.contains(call)) {
            continue;
        }
        checked += 1;
        if !code.contains("RECODEX_ENV_SANDBOX") {
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
