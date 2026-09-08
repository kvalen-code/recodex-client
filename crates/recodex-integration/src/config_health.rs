//! 启动时验一次 config.toml 能不能被 Codex 读进去,读不进去就上报。
//!
//! 为什么需要:官方 codex 遇到解析失败是**硬失败不回落**(实测
//! `Error loading configuration: config.toml:16:26: duplicate key`),
//! 所以一份坏文件 = 用户所有配置一起消失,Codex 起不来,而我们这边只会看到
//! `bridge.resolve_failed` 之类的下游症状,根本分不清是桥的问题还是配置废了。
//!
//! 2026-09-08 远程排查了 4 台客户机器,全是 config.toml 没配好,形态各不相同。
//! 而查 30 天的 client_diagnostic_reports,含 config/toml/provider 字样的事件
//! **总共只有 2 条** —— 不是问题不存在,是我们对这件事零可见度。
//!
//! 这一条补的就是可见度:分母用现成的 `launcher.ready` 算,这里只报失败。

use std::path::Path;

/// 解析失败时要上报的东西。**绝不包含文件内容**。
///
/// TOML 解析器的报错默认会把出错那一行**原样**渲染出来:
///
/// ```text
/// 16 | experimental_bearer_token = "sk-..."
///    | ^
/// ```
///
/// 那一行很可能正是我们内联进去的网关 Key。所以只取行列号和一句归类,
/// 原始报错一个字都不往外发。
pub struct ConfigHealthFailure {
    /// 出错位置。TOML 错误不一定带位置(比如整体不是对象),没有就是 None。
    pub line: Option<usize>,
    pub column: Option<usize>,
    /// 粗分类,给服务端聚合用。见 classify。
    pub kind: &'static str,
    /// 文件里还有没有我们的托管块 —— 用来分「我们写坏的」和「用户自己弄坏的」。
    pub has_managed_block: bool,
}

/// 把解析错误归成几类。**只看错误文本里的关键词,不回传文本本身。**
///
/// duplicate_key 单独一类是因为它就是 2026-09-08 那次事故的签名:
/// 托管块里 http_headers 是内联表,块外还留着 [model_providers.recodex.http_headers]
/// 段头,TOML 不许用段头扩展内联表。要是这一类占了大头,说明还有别的写入方
/// 在往同一张表里塞东西,那是要单独处理的。
fn classify(message: &str) -> &'static str {
    let m = message.to_ascii_lowercase();
    if m.contains("duplicate") {
        "duplicate_key"
    } else if m.contains("expected") || m.contains("invalid") || m.contains("unterminated") {
        "syntax"
    } else {
        "other"
    }
}

/// 检查一份 config.toml 的健康状况。
///
/// 返回 None 表示没问题(**文件不存在也算没问题** —— 那是全新机器,
/// 不是故障,报上去只会把噪声灌满)。
pub fn check(path: &Path) -> Option<ConfigHealthFailure> {
    let content = std::fs::read_to_string(path).ok()?;
    let err = match content.parse::<toml::Value>() {
        Ok(_) => return None,
        Err(err) => err,
    };
    let (line, column) = match err.span() {
        Some(_) => line_col(&content, &err),
        None => (None, None),
    };
    Some(ConfigHealthFailure {
        line,
        column,
        kind: classify(&err.to_string()),
        has_managed_block: crate::codexcfg::has_managed_block(&content),
    })
}

/// 从字节偏移换算行列(1-based)。
///
/// 不用错误里现成的渲染结果:那份渲染**带原文**。这里只从 span 的起点数换行符,
/// 数出来的是两个整数,不可能夹带内容。
fn line_col(content: &str, err: &toml::de::Error) -> (Option<usize>, Option<usize>) {
    let Some(span) = err.span() else {
        return (None, None);
    };
    let start = span.start.min(content.len());
    // 按**字节**偏移切 &str,不在字符边界上会 panic —— 客户配置里有中文
    // (`[projects.'d:\projects\社科院数字访问员']`),真会撞上。
    // 健康检查自己把启动器搞崩,那就本末倒置了:切不动就只报"位置未知"。
    let Some(before) = content.get(..start) else {
        return (None, None);
    };
    let line = before.matches('\n').count() + 1;
    let column = before.rsplit('\n').next().map(|s| s.chars().count() + 1);
    (Some(line), column)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 上报内容里**绝不能出现文件里的任何一段原文**。
    ///
    /// TOML 报错默认会把出错那一行原样渲染出来,而那一行可能正是
    /// `experimental_bearer_token = "sk-..."`。这条守卫盯的就是这个:
    /// 把密钥放在出错行上,然后检查我们产出的每一个字段。
    #[test]
    fn failure_never_carries_file_content() {
        let secret = "sk-recodex-super-secret-value";
        let content = format!(
            "model = \"gpt-5.6-codex\"\n[model_providers.recodex]\nexperimental_bearer_token = \"{secret}\" oops\n"
        );
        let err = content.parse::<toml::Value>().unwrap_err();
        assert!(
            err.to_string().contains(secret),
            "前提不成立:这个样本的原始报错本来就不带密钥,守卫等于没测"
        );

        let dir = std::env::temp_dir().join(format!("rcx-cfghealth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, &content).unwrap();

        let failure = check(&path).expect("这份应当是坏的");
        // 我们产出的字段:两个数字 + 一个固定分类 + 一个布尔。没有一个能装下密钥。
        assert!(!failure.kind.contains(secret));
        assert_eq!(failure.kind, "syntax");
        assert_eq!(failure.line, Some(3));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文件不存在 = 全新机器,不是故障。报上去只会把噪声灌满。
    #[test]
    fn missing_config_is_not_a_failure() {
        let path = std::env::temp_dir().join("rcx-cfghealth-does-not-exist.toml");
        let _ = std::fs::remove_file(&path);
        assert!(check(&path).is_none());
    }

    /// duplicate_key 要单独分类:那是 2026-09-08 那次事故的签名,
    /// 占比高说明还有别的写入方在往同一张表里塞东西。
    #[test]
    fn duplicate_key_is_its_own_class() {
        assert_eq!(classify("duplicate key `http_headers`"), "duplicate_key");
        assert_eq!(classify("expected `=`"), "syntax");
        assert_eq!(classify("something else entirely"), "other");
    }

    /// 按字节偏移切 &str,不在字符边界上会 panic。客户配置里有中文,
    /// 出错点紧跟在多字节字符后面是真实会发生的形状 —— 健康检查自己把启动器
    /// 搞崩,那就本末倒置了。这条只要求"不 panic 且仍能报出分类"。
    #[test]
    fn multibyte_content_never_panics() {
        let dir = std::env::temp_dir().join(format!("rcx-cfghealth-mb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        // 出错的 `=` 紧跟在中文后面
        std::fs::write(&path, "[projects.'d:/社科院数字访问员']
trust_level = 社科院 =
").unwrap();
        let failure = check(&path).expect("这份应当是坏的");
        assert_eq!(failure.kind, "syntax");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
