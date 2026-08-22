//! codexcfg 的**行为对照语料**:同一批 input,Go 命令行与 Rust 桌面端算出的
//! install / remove 结果必须逐字节相同。
//!
//! 为什么需要它:`~/.codex/config.toml` 有三个写入方(Go 命令行、Rust 桌面端、
//! Codex++ 自己),两个客户端各有一份实现。历史上它们**一致地错**过一次 ——
//! 托管块往顶层塞 `model_provider`,而用户可能已经有一个,顶层重复键让 Codex
//! 连整份文件都解析不了。语料把两份实现钉在同一个契约上:分叉就是红测试,
//! 而不是用户机器上一份打不开的配置。
//!
//! 更新期望值:`RECODEX_UPDATE_GOLDEN=1 cargo test -p recodex-integration --test codexcfg_corpus`
//! ——然后**逐字看一遍**再提交。金文件的价值全在那一眼上。

use recodex_integration::codexcfg;
use std::fs;
use std::path::{Path, PathBuf};

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/codexcfg")
}

// 语料以 LF 存;Windows 上的 checkout 可能带回 CR,那是搬运问题不是被测行为。
fn read_lf(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("读不到 {}: {e}", path.display()))
        .replace("\r\n", "\n")
}

fn check(case: &Path, name: &str, actual: &str) {
    let expected_path = case.join(name);
    if std::env::var_os("RECODEX_UPDATE_GOLDEN").is_some() {
        fs::write(&expected_path, actual.as_bytes()).unwrap();
        return;
    }
    let expected = read_lf(&expected_path);
    assert_eq!(
        expected,
        actual,
        "{} 与期望不符\n--- 期望 ---\n{}\n--- 实得 ---\n{}",
        expected_path.display(),
        expected,
        actual
    );
}

#[test]
fn install_and_remove_match_the_shared_corpus() {
    let root = corpus_dir();
    let body = read_lf(&root.join("body.toml"));
    let mut cases: Vec<PathBuf> = fs::read_dir(&root)
        .expect("语料目录不存在")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    cases.sort();
    assert!(!cases.is_empty(), "语料目录里一个用例都没有");

    for case in cases {
        let input = read_lf(&case.join("input.toml"));
        let installed = codexcfg::install_block(&input, &body);
        check(&case, "installed.toml", &installed);

        // 安装必须幂等:同一份 body 再装一次不能有任何变化,更不能把块复制一份
        assert_eq!(
            installed,
            codexcfg::install_block(&installed, &body),
            "{} 的安装不幂等",
            case.display()
        );

        // 顶层 model_provider 有且只能有一个 —— 多一个就是 TOML 重复键,Codex 整份文件读不了
        assert_eq!(
            1,
            top_level_model_provider_count(&installed),
            "{} 安装后顶层 model_provider 不是恰好一个:\n{}",
            case.display(),
            installed
        );

        check(&case, "removed.toml", &codexcfg::remove_block(&installed));
    }
}

// 只数第一个表头之前的 model_provider —— 表头之后的属于那张表,不是顶层键。
fn top_level_model_provider_count(content: &str) -> usize {
    let mut count = 0;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            break;
        }
        let Some(rest) = trimmed.strip_prefix("model_provider") else {
            continue;
        };
        if rest.trim_start().starts_with('=') {
            count += 1;
        }
    }
    count
}
