//! 旧对话 provider 别名的**行为对照语料**：同一份 input + refs，Go 命令行与 Rust 桌面端
//! 算出的 with_history_aliases 结果必须逐字节相同。两边都会重装托管块，渲染一旦分叉，
//! 就会互相把对方写的别名段当成异物、叠出重复表 —— Codex 对重复表硬失败。
//!
//! 更新期望值：`RECODEX_UPDATE_GOLDEN=1 cargo test -p recodex-integration --test history_aliases_corpus`
//! ——然后逐字看一遍再提交。Go 侧同名测试读的是同一批文件。

use recodex_integration::codexcfg;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/history-aliases")
}

fn read_lf(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("读不到 {}: {e}", path.display()))
        .replace("\r\n", "\n")
}

#[test]
fn history_aliases_match_the_shared_corpus() {
    let mut cases: Vec<PathBuf> = fs::read_dir(corpus_dir())
        .expect("语料目录不存在")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    cases.sort();
    assert!(!cases.is_empty(), "语料目录里一个用例都没有");
    for case in cases {
        let input = read_lf(&case.join("input.toml"));
        let refs: BTreeMap<String, usize> =
            serde_json::from_str(&read_lf(&case.join("refs.json"))).expect("refs.json 不是合法 JSON");
        let got = codexcfg::with_history_aliases(&input, &refs);
        let expected_path = case.join("expected.toml");
        if std::env::var_os("RECODEX_UPDATE_GOLDEN").is_some() {
            fs::write(&expected_path, got.as_bytes()).unwrap();
            continue;
        }
        let expected = read_lf(&expected_path);
        assert_eq!(
            expected, got,
            "{} 与 Go 侧写下的期望不符\n--- 期望 ---\n{}\n--- 实得 ---\n{}",
            expected_path.display(), expected, got
        );
        assert_eq!(
            codexcfg::with_history_aliases(&got, &refs),
            got,
            "{} 不幂等",
            case.display()
        );
        assert!(got.matches(codexcfg::END_MARKER).count() <= 1, "{} 结束标记重复", case.display());
    }
}

#[test]
fn history_aliases_carry_the_current_key_after_reinstall() {
    let mut refs = BTreeMap::new();
    refs.insert("codex_local_access".to_string(), 2usize);
    let old_body = "model_provider = \"recodex\"\n\n[model_providers.recodex]\nname = \"ReCodex\"\nbase_url = \"https://gw.example.dev/v1\"\nexperimental_bearer_token = \"rk-OLD\"\n";
    let new_body = old_body.replace("rk-OLD", "rk-NEW");
    let first = codexcfg::with_history_aliases(&codexcfg::install_block("", old_body), &refs);
    assert!(first.contains("[model_providers.codex_local_access]"), "{first}");
    let second = codexcfg::with_history_aliases(&codexcfg::install_block(&first, &new_body), &refs);
    assert!(!second.contains("rk-OLD"), "重装后还留着旧钥匙：\n{second}");
    assert_eq!(second.matches("rk-NEW").count(), 2, "{second}");
    assert_eq!(second.matches(codexcfg::HISTORY_ALIAS_MARKER).count(), 1, "{second}");
}

#[test]
fn session_provider_refs_reads_first_lines_and_caches() {
    let dir = std::env::temp_dir().join(format!("recodex-histalias-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let write = |rel: &str, first: &str| {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, format!("{first}\n{{\"type\":\"response_item\"}}\n")).unwrap();
    };
    let meta = |provider: &str| {
        serde_json::json!({"type":"session_meta","payload":{
            "base_instructions":{"text": format!("decoy \"model_provider\": \"decoy\" {}", "x".repeat(60 << 10))},
            "model_provider": provider}}).to_string()
    };
    write("sessions/2026/09/23/rollout-a.jsonl", &meta("codex_local_access"));
    write("sessions/2026/09/23/rollout-b.jsonl", &meta("codex_local_access"));
    write("archived_sessions/rollout-c.jsonl", &meta("custom"));
    write("sessions/rollout-d.jsonl", "{\"type\":\"response_item\",\"payload\":{\"model_provider\":\"nope\"}}");
    write("sessions/notes.jsonl", &meta("nope2"));
    let want: BTreeMap<String, usize> =
        [("codex_local_access".to_string(), 2), ("custom".to_string(), 1)].into_iter().collect();
    assert_eq!(codexcfg::session_provider_refs(&dir), want);
    assert!(dir.join("recodex").join("session-providers.json").exists(), "应写出缓存");
    assert_eq!(codexcfg::session_provider_refs(&dir), want, "走缓存结果应一致");
    let _ = fs::remove_dir_all(&dir);
}
