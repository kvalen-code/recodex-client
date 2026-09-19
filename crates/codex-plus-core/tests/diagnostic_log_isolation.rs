//! 集成测试/下游 crate 的测试链接的是**不带 cfg(test)** 的 core,所以
//! 「测试不写真实诊断日志」必须在这里单独守一次(单元测试那条守不住这一层)。

use codex_plus_core::diagnostic_log::{
    append_diagnostic_log, diagnostic_log_path, test_harness_log_dir,
};

#[test]
fn integration_tests_never_write_the_real_diagnostic_log() {
    let real = codex_plus_core::paths::default_diagnostic_log_path();
    let path = diagnostic_log_path();
    assert_ne!(path, real, "集成测试默认写进了真实的 recodex.log");
    assert!(path.starts_with(test_harness_log_dir()), "{}", path.display());

    let real_len = std::fs::metadata(&real).map(|meta| meta.len()).ok();
    append_diagnostic_log(
        "test.diagnostic_log_isolation_probe",
        serde_json::json!({ "probe": true }),
    )
    .unwrap();
    let written = std::fs::read_to_string(&path).unwrap();
    assert!(written.contains("test.diagnostic_log_isolation_probe"));
    // 真实日志的长度不因这次写入变化(别的进程可能同时在写,这里只查本探针)。
    if let Ok(real_contents) = std::fs::read_to_string(&real) {
        assert!(
            !real_contents.contains("test.diagnostic_log_isolation_probe"),
            "探针事件出现在真实日志里(写入前长度 {real_len:?})"
        );
    }
}
