//! Manages the ReCodex-owned regions of the user's Codex configuration on the
//! desktop, so the Codex the launcher spawns talks to the selected ReCodex
//! gateway. This mirrors the CLI's `internal/clientcfg` package byte-for-byte on
//! the parts that matter (markers, template, atomic writes) — the desktop had no
//! equivalent, so selecting a gateway only recorded the choice server-side while
//! Codex kept using its old provider.
//!
//! Three pieces of user state are owned here, all reversible on logout:
//!   - a marker-wrapped block spliced into `~/.codex/config.toml` (never a whole
//!     rewrite: the user's other tables survive byte-for-byte);
//!   - `~/.codex/auth.json` (backed up once so the original can be restored);
//!   - the `RECODEX_KEY` user environment variable Codex reads the key from.
//!
//! ## 写进配置的东西按来源分三档信任(2026-09-05 排查后固化)
//!
//! 这个模块几乎全是**字符串拼接**写 TOML,没有序列化器兜底 —— 所以「这个值从哪
//! 来」直接决定要不要校验:
//!
//!   1. **登录服务器下发的整块 config** —— 完全信任。服务端本来就控制客户端配置,
//!      再校验也没意义(它想改什么都能改)。API base 那侧由
//!      `persist_api_base_if_trusted` 把关。
//!   2. **网关列表里的 endpoint** —— 半信任。同样出自 API base,但会被拼成
//!      `base_url` 塞进托管块,所以过 `base_url_is_safe`。
//!   3. **models manifest 里的 slug** —— **不信任**。manifest 是向**用户自选的网关**
//!      要的,那一端不归我们管;而 slug 会被直接写成 `model = "..."`。
//!      必须过 `model_name_is_safe`,否则一个带引号和换行的值就能再开一张
//!      `[model_providers.*]` 表,把用户的对话全导走。
//!
//! 往这里加新的「写配置」路径时,先问清楚值是哪一档。

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// Managed-block markers. These are byte-identical to the CLI's so a config
/// written by either client is recognised and cleanly removed by the other.
pub const START_MARKER: &str = "# >>> recodex managed block, do not edit >>>";
pub const END_MARKER: &str = "# <<< recodex managed block <<<";

/// The environment variable Codex reads the API key from in `env_key` mode. Must
/// match the `env_key` rendered into the managed block.
pub const SUB2API_ENV_KEY: &str = "RECODEX_KEY";

/// 本进程启动之后 `RECODEX_KEY` 有没有被改过(登录 / 换组织 / 登出)。
///
/// Codex 只在**启动时**读一次这个变量:改了之后本进程的副本是新的,
/// 已经在跑的 Codex 攥着的还是旧值 —— 而旧 key 在服务端已经作废
/// (重新登录会换发)。自诊断靠这个标志把「凭据没问题、只是没重启」
/// 和「凭据坏了」分开,否则两者在用户眼里都是网关 401。
static KEY_CHANGED_SINCE_START: AtomicBool = AtomicBool::new(false);

pub fn key_changed_since_start() -> bool {
    KEY_CHANGED_SINCE_START.load(Ordering::SeqCst)
}

/// 记录一次对 `RECODEX_KEY` 的改动。值没变(重复登录同一把 key)不算。
fn note_key_change(name: &str, current: Option<&str>, next: Option<&str>) {
    if name == SUB2API_ENV_KEY && current != next {
        KEY_CHANGED_SINCE_START.store(true, Ordering::SeqCst);
    }
}

const AUTH_BACKUP_SUFFIX: &str = ".recodex-bak";
const AUTH_MANAGED_SUFFIX: &str = ".recodex-managed";

// The sub2api managed block (recodex.md v2). Plain `env_key` auth — no
// `requires_openai_auth` — matching the CLI's Sub2APIConfigTemplate.
// requires_openai_auth / http_headers 这两行是**生图能不能显示**的开关。
//
// Codex 有两条生图路径:
//   - hosted image_generation:上游生成,回 image_generation_call —— 客户端存得下,
//     但 Codex 界面没有对应的渲染分支,用户看到「模型说生成好了、界面什么都没有」;
//   - 本地 image_gen.imagegen:客户端自己的执行器,打 /v1/images/generations,
//     结果由它自己渲染 —— 这条才看得见。
//
// 本地执行器默认不注册,要靠这两行授权(上游把它叫 API Key Mode)。少了它们,
// 客户端连工具都不声明,模型只能反过来劝用户「去设置 OPENAI_API_KEY」。
//
// 模板保持 env_key —— 服务端下发的也是这个形状,两边必须一致。真正落盘的那份
// 会被 `inline_managed_key` 就地换成 experimental_bearer_token,原因见那个函数。
//
// 改完必须**完全退出 Codex 并新建 task**:工具注册表是启动时建的,热重载看不到。
const SUB2API_TEMPLATE: &str = "model_provider = \"recodex\"\n\n[model_providers.recodex]\nname = \"ReCodex\"\nbase_url = \"{{BASE_URL}}\"\nwire_api = \"responses\"\nenv_key = \"{{ENV_KEY}}\"\nsupports_websockets = {{SUPPORTS_WEBSOCKETS}}\nhttp_headers = { \"x-openai-actor-authorization\" = \"recodex\" }";

fn home_dir() -> io::Result<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "no home directory is available"))
}

/// Codex 用来改数据目录的环境变量名。
pub const CODEX_HOME_ENV: &str = "CODEX_HOME";

/// codex_dir() 这次的取值来自哪里。给诊断和登录提示用 ——
/// 不说清楚是哪一个 config.toml,用户和客服都会默认 ~/.codex。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexDirSource {
    /// 回落到了 ~/.codex。
    Default,
    /// 取自当前进程可见的 CODEX_HOME。
    Env,
}

/// CODEX_HOME 的取值是否可用。
///
/// **这三条规则必须与 Go 侧 internal/clientcfg/codexhome.go 的
/// `usableCodexHome` / `resolveCodexHome` 逐字一致** —— 不一致的后果不是
/// "少支持一个变量":CLI 和桌面端会把托管块写到不同目录,一份生效一份不生效,
/// 比两边都写错更难查(同 base_url_is_safe 那次的教训)。
///
///   - 空值 → 回落。变量存在但为空是"没设置"的常见写法。
///   - 相对路径 → **忽略并回落**。相对路径跟着进程的工作目录跑,
///     写出去的配置连我们自己都找不回来。
///   - 目录不存在 → 仍然采用。Codex 自己会创建它,我们跟随;
///     悄悄退回 ~/.codex 会让用户以为写对了 —— 那正是 2026-09-07 那次事故的形状。
fn usable_codex_home(raw: &std::ffi::OsStr) -> Option<PathBuf> {
    let path = PathBuf::from(raw);
    if path.as_os_str().is_empty() || !path.is_absolute() {
        return None;
    }
    Some(path)
}

/// Codex 的数据目录:CODEX_HOME(绝对路径时)优先,否则 ~/.codex。
///
/// 这是一次线上事故的根因(2026-09-07):我们全仓没有任何一处读过这个变量,
/// 一律写死 ~/.codex。用户把数据目录设到 D:\CodexData 后,**我们写的托管块
/// Codex 一眼都没看过** —— 他那份真正生效的配置里留着第三方中转,表现是
/// "要求登录 ChatGPT"和"401 Invalid token"。远程排查两小时、改了六轮配置
/// 全部无效,因为每一轮都改在错的文件上。
///
/// 桌面端是被 Codex/启动器拉起来的子进程,能拿到继承下来的 CODEX_HOME;
/// 用户级持久变量的兜底在 CLI 侧做(Go 的 UserEnv 已经覆盖三个平台),
/// 这里不重复实现 —— 两侧对**同一份进程环境**的判定一致即可。
pub(crate) fn codex_dir() -> io::Result<PathBuf> {
    Ok(codex_dir_with_source()?.0)
}

/// 与 codex_dir 相同,另外返回取值来源,供诊断展示。
pub fn codex_dir_with_source() -> io::Result<(PathBuf, CodexDirSource)> {
    if let Some(dir) = std::env::var_os(CODEX_HOME_ENV).and_then(|raw| usable_codex_home(&raw)) {
        return Ok((dir, CodexDirSource::Env));
    }
    Ok((home_dir()?.join(".codex"), CodexDirSource::Default))
}

/// 不考虑 CODEX_HOME 时的目录,用来判断当前是不是非默认位置。
pub fn default_codex_dir() -> io::Result<PathBuf> {
    Ok(home_dir()?.join(".codex"))
}

/// Path to `~/.codex/config.toml`.
pub fn config_path() -> io::Result<PathBuf> {
    Ok(codex_dir()?.join("config.toml"))
}

/// Path to `~/.codex/auth.json`.
pub fn auth_path() -> io::Result<PathBuf> {
    Ok(codex_dir()?.join("auth.json"))
}

/// base_url 能不能安全地拼进托管块。
///
/// 和 `model_name_is_safe` 同一个道理:下面是纯字符串 `.replace()`,零转义。
/// 一个含双引号或换行的 endpoint 就能在托管块里再开一张表,把 provider 改掉。
/// endpoint 来自服务端下发的网关列表(API base 那侧有 persist_api_base_if_trusted
/// 把关),风险低于 manifest 里的 slug —— 但这是同一类洞,一起堵。
///
/// 只认 http/https 且不含引号、换行、`#`、`?`:真实网关地址本来就长这样。
///
/// `?` 挡的不是注入,是**静默失效**:网关地址一路都在被拼接
/// (`<endpoint>/backend-api/codex` 写进 config.toml、
/// `<base>/api/cli/auth/portal-check?host=` 用于控制面探活)。
/// endpoint 里带一个 `?`,后面拼的路径就全被吃进查询串 —— Codex 打向网关根路径、
/// 探活永远失败,而两者都**不报错**,只是不工作。
pub fn base_url_is_safe(base_url: &str) -> bool {
    let base_url = base_url.trim();
    (base_url.starts_with("https://") || base_url.starts_with("http://"))
        && base_url.len() <= 512
        && !base_url
            .chars()
            .any(|c| c.is_control() || matches!(c, '"' | '\'' | '#' | '\\' | '?'))
}

/// Renders the sub2api managed block. `base_url` is the gateway root Codex talks
/// to, e.g. `https://sg.gw.recodex.dev/backend-api/codex`.
pub fn render_sub2api_block(base_url: &str, supports_websockets: bool) -> String {
    SUB2API_TEMPLATE
        .replace("{{BASE_URL}}", base_url)
        .replace("{{ENV_KEY}}", SUB2API_ENV_KEY)
        .replace(
            "{{SUPPORTS_WEBSOCKETS}}",
            if supports_websockets { "true" } else { "false" },
        )
}

/// 从托管块里读回 supports_websockets。与 Go 侧 clientcfg.ManagedSupportsWebsockets
/// 同一语义：读不到就当 false，与 Codex 的 `#[serde(default)] bool` 默认值一致。
///
/// 必须有它：apply_config 会把整个 `[model_providers.recodex]` 表连表内所有键一起
/// 重写，本地重渲染时不读回就会把服务端下发的 WS 开关冲掉 —— 用户切一次网关就被
/// 静默打回 HTTP，他只会觉得"又变慢了"，排查不到这里。
/// 入参是**已经取出的块正文**（不含标记行），与 Go 侧
/// `clientcfg.ManagedSupportsWebsockets(body)` 的入参形状一致。
///
/// 之所以不接受整份 config.toml：官方模式的快照 `OfficialModeSnapshot.config_body`
/// 存的就是不带标记的正文（`officialmode::current_managed_body`），要求带标记会让
/// 它永远返回 false，把 WS 开关静默丢掉。
pub fn managed_supports_websockets(body: &str) -> bool {
    body.lines()
        .find_map(|line| {
            let value = line
                .trim_start()
                .strip_prefix("supports_websockets")?
                .trim_start()
                .strip_prefix('=')?;
            Some(value.trim() == "true")
        })
        .unwrap_or(false)
}

/// 读当前 config.toml 的托管块，取回 supports_websockets。
/// 文件不存在、没有托管块时返回 false，不凭空打开 WS。
pub fn current_supports_websockets() -> bool {
    let Ok(path) = config_path() else {
        return false;
    };
    let Ok(content) = fs::read_to_string(path) else {
        return false;
    };
    // 标记没了就回落到「扫我们那张 provider 表」，与 Go 侧 ManagedBody 同一策略。
    //
    // 标记丢失是**常态不是异常**：config.toml 有第三个写入方，Codex++ 重新序列化
    // 整份文件时会丢掉注释标记（共享语料里专门有 markers-lost 用例）。
    // 以标记为前提的后果不是「读不到」而是**静默读错**：这里退化成 false，
    // 于是本地重渲染托管块时把 supports_websockets 写成 false ——
    // 用户切一次网关就被静默打回 HTTP，只会觉得「忽然变慢了」。
    match marked_block_span(&content) {
        Some((start, end)) => managed_supports_websockets(&content[start..end]),
        None => recodex_provider_table_body(&content)
            .is_some_and(|body| managed_supports_websockets(&body)),
    }
}

/// 在**没有标记**时，把 `[model_providers.recodex]` 那张表（含子表）的正文切出来，
/// 供读回类兜底。与 Go 侧 `recodexProviderTableBody` 逐条同判。
///
/// 复用 `is_recodex_provider_table` 判表头，不另造一套 —— 那个谓词已经处理过子表、
/// 引号名、前缀相同不误伤这些边界，再写一份就是等着两边分叉。
fn recodex_provider_table_body(content: &str) -> Option<String> {
    let lines: Vec<&str> = content.split('\n').collect();
    let mut start: Option<usize> = None;
    let mut end = lines.len();
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if !trimmed.starts_with('[') {
            continue;
        }
        if is_recodex_provider_table(trimmed) {
            if start.is_none() {
                start = Some(i);
            }
            continue;
        }
        // 撞到别人的表头：只有已经进过我们的表之后才算结束。
        if start.is_some() {
            end = i;
            break;
        }
    }
    start.map(|s| lines[s..end].join("\n"))
}

/// 托管配置的体检结果。全部只看文件内容,不联网。
///
/// 这三项对应的都是**静默故障** —— 出问题时 Codex 不报错,只是悄悄不走 ReCodex：
///   - 块不在 → 用户以为在用 ReCodex,其实走的官方 provider
///   - 块在但排在表头之后 → 块里的顶层 `model_provider` 被 TOML 归给上面那张表,
///     顶层等于没设,效果同上(2026-08-26 在用户机器上实际发生,1.2.54 装在了第 185 行)
///   - 顶层 `model_provider` 出现两次 → 整份 config.toml 解析失败,日志里是
///     `duplicate key model_provider in document root`,而用户看到的是
///     `Model provider 'recodex' not found`,两者对不上号
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigHealth {
    pub managed: bool,
    pub before_first_table: bool,
    pub top_level_model_provider: usize,
}

impl ConfigHealth {
    /// 三项全过才算真的在托管中。
    pub fn is_healthy(&self) -> bool {
        self.managed && self.before_first_table && self.top_level_model_provider == 1
    }
}

/// 体检 `config.toml` 的内容。
pub fn inspect_config(content: &str) -> ConfigHealth {
    let managed = has_managed_block(content);
    let before_first_table = match (marked_block_span(content), first_table_header_offset(content))
    {
        // 没有表头时,块无论在哪都还在顶层区域
        (Some(_), None) => true,
        (Some((start, _)), Some(table)) => start < table,
        (None, _) => false,
    };
    let top_level_model_provider = content[..top_level_len(content)]
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !trimmed.starts_with('#') && model_provider_value(trimmed).is_some()
        })
        .count();
    ConfigHealth {
        managed,
        before_first_table,
        top_level_model_provider,
    }
}

/// Reports whether content already carries our managed block.
pub fn has_managed_block(content: &str) -> bool {
    content.contains(START_MARKER) && content.contains(END_MARKER)
}

/// 用户原有默认 provider 的存放行,写在托管块**内部**。
///
/// 我们的块拥有顶层 `model_provider`,安装时必须把用户原来那行摘掉 —— 否则顶层出现
/// 重复键,Codex 连整个 config.toml 都解析不了(线上就是这么炸的:日志里
/// `duplicate key model_provider in document root`)。摘掉不等于可以吞掉:
/// 把原值停在这里,卸载时还回去。
///
/// 不另开状态文件:多一份跨实现的共享状态,就是多一个「同一份状态两个主人」。
/// ponytail: 会重新序列化 config.toml 的写入方(Codex++ 就会)会丢掉注释,那种情况下
/// 降级成「还不回去」,不影响正确性;真需要更硬的保存再上状态文件。
const SAVED_PROVIDER_PREFIX: &str = "# recodex-previous-model-provider = ";

/// 解析一行顶层 `model_provider = "x"`,返回去引号的值。
/// 只认这个精确的键:`model_provider_extra = 1`、`model_providers = ...` 都不匹配。
fn model_provider_value(trimmed: &str) -> Option<&str> {
    let rest = trimmed.strip_prefix("model_provider")?.trim_start();
    let rest = rest.strip_prefix('=')?.trim();
    Some(rest.trim_matches('"'))
}

/// 顶层区域的长度 —— 即第一个表头之前。TOML 里表头之后的裸键属于那张表,
/// 我们只拥有顶层那一个 `model_provider`,绝不能碰 `[profiles.x]` 里的同名键。
fn top_level_len(content: &str) -> usize {
    first_table_header_offset(content).unwrap_or(content.len())
}

fn render_marked_block(body: &str, saved: Option<&str>) -> String {
    let body = body.trim_matches('\n');
    match saved {
        Some(prev) => {
            format!("{START_MARKER}\n{SAVED_PROVIDER_PREFIX}\"{prev}\"\n{body}\n{END_MARKER}\n")
        }
        None => format!("{START_MARKER}\n{body}\n{END_MARKER}\n"),
    }
}

// Byte offset of the first TOML table header (a line whose first non-space char
// is `[`). The managed block's top-level `model_provider` key must sit before any
// table, so we insert there rather than at EOF.
fn first_table_header_offset(content: &str) -> Option<usize> {
    let mut offset = 0usize;
    for line in content.split_inclusive('\n') {
        if line.trim_start().starts_with('[') {
            return Some(offset);
        }
        offset += line.len();
    }
    None
}

// Locates the managed block's byte span [start, end): start is the beginning of
// the start-marker line, end is just past the newline after the end-marker line
// (or EOF). Markers and newlines are ASCII, so the returned indices always land
// on char boundaries even when the surrounding config holds UTF-8.
fn marked_block_span(content: &str) -> Option<(usize, usize)> {
    let si = content.find(START_MARKER)?;
    let start = content[..si].rfind('\n').map(|nl| nl + 1).unwrap_or(0);
    let ei_rel = content[si..].find(END_MARKER)?;
    let after_end = si + ei_rel + END_MARKER.len();
    let end = match content[after_end..].find('\n') {
        Some(nl) => after_end + nl + 1,
        None => content.len(),
    };
    Some((start, end))
}

/// 托管块里写的 `base_url`(网关的 `/backend-api/codex` 根)。没有托管块时为 None。
///
/// 自诊断要拿它去问网关「这把 key 还认不认」—— 用户看到的 401 只有网关说得清。
pub fn managed_base_url(content: &str) -> Option<String> {
    let (start, end) = marked_block_span(content)?;
    content[start..end].lines().find_map(|line| {
        let value = line
            .trim_start()
            .strip_prefix("base_url")?
            .trim_start()
            .strip_prefix('=')?;
        let value = value.trim().trim_matches('"');
        (!value.is_empty()).then(|| value.to_string())
    })
}

/// 块体自己定义的顶层键。托管块的内容是**可变的**(服务端可以下发别的模板),
/// 所以「我们拥有哪些顶层键」必须从块体推导,不能写死一张表 ——
/// 写死的那天块体一变,漏掉的键就会在下次安装时变成顶层重复键。
fn top_level_keys_of(body: &str) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    keys.insert("model_provider".to_string());
    for line in body.split_inclusive('\n') {
        let t = line.trim();
        if t.starts_with('[') {
            break;
        }
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let Some(i) = t.find('=') {
            keys.insert(t[..i].trim().to_string());
        }
    }
    keys
}

/// 从一段托管块里取出之前存下的用户默认 provider。
fn saved_provider_in(span: &str) -> Option<String> {
    for line in span.split_inclusive('\n') {
        if let Some(rest) = line.trim().strip_prefix(SAVED_PROVIDER_PREFIX) {
            let value = rest.trim().trim_matches('"');
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// 把连续多个空行收拢成一个。
///
/// 不能用 `replace("\n\n\n", "\n\n")`:config.toml 可能是 CRLF 的,而我们插入的
/// 分隔符是 LF,于是 `"\r\n\r\n\n"` 里根本没有三个连续的 `\n`,收拢不掉 ——
/// 安装再卸载就会比原文多出一个换行,往返不再逐字节一致。
/// 按行判断,空行就是 trim 后为空的行,两种行尾都认。
fn collapse_blank_runs(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_blank = false;
    for line in s.split_inclusive('\n') {
        let blank = line.trim().is_empty();
        if blank && prev_blank {
            continue;
        }
        prev_blank = blank;
        out.push_str(line);
    }
    out
}

// 剥离上一轮 ReCodex 的痕迹,并把用户自己的默认 provider 带出来。
//
// 两段式:
//  1. 标记还在 —— 整段切掉。那是我们写进去的**全部**内容,不管块体当时是什么模板,
//     这是唯一精确的删法。
//  2. 标记没了 —— config.toml 还有第三个写入方,Codex++ 会重新序列化整份文件并丢掉
//     我们的注释标记。这时只能按内容清残留:[model_providers.recodex] 表、
//     块体拥有的顶层键、孤儿标记行。少了这一步,残留会在下次安装时被复制一份,
//     顶层重复键让 Codex 连整份文件都解析不了。
//
// `owned` 为块体拥有的顶层键;None 时只认 model_provider(卸载路径拿不到块体,
// 但那条路上标记通常还在,走的是第 1 段)。
//
// 返回值第二项是**用户的**默认 provider(不是我们的 "recodex"):来自块内保存行,
// 或用户当前真的写在顶层的那一行 —— 后者优先,因为那代表用户此刻的选择。
/// 认 `[model_providers.recodex]` **以及它的子表** `[model_providers.recodex.http_headers]` 之类。
///
/// 子表必须一起清:我们的块里 `http_headers` 是**内联表**,而 TOML 不许再用段头去
/// 扩展一个内联表。残留的子表和新块撞在一起,Codex 连整份文件都读不进去 ——
/// 官方 CLI 实测直接 `Error loading configuration: config.toml:16:26: duplicate key`
/// 硬失败(不是软回落),于是所有配置全部不生效,表现是"客户端用不了"。
///
/// 原先这里是精确匹配 `== "[model_providers.recodex]"`,子表正好从底下溜过去。
/// 而且标记还在时走的是第 1 段(整块切掉),孤儿子表留在块外 —— 也就是说
/// **重装、重新登录都修不好**,每次安装都重新造一份坏文件。
/// 2026-09-08 一个客户就是这么废掉的(那个子表不是我们写的,是第三方切换器留的)。
///
/// 只认**裸的** recodex 段:`[model_providers.'recodex.foo']` 是用户一个真叫
/// recodex.foo 的 provider,它的名字以引号开头,下面的前缀匹配天然不成立 ——
/// 不要再加"含引号就放弃"那种检查:它挡不住这个(已经不成立了),却会把
/// `[model_providers.recodex.'x']` 这种**确实是我们子表**的形状漏掉。
///
/// 与 Go 侧 clientcfg.isRecodexProviderTable 逐字对应,改一侧必须改另一侧。
fn is_recodex_provider_table(trimmed: &str) -> bool {
    let Some(inner) = trimmed.strip_prefix('[') else {
        return false;
    };
    let Some(inner) = inner.strip_suffix(']') else {
        return false;
    };
    let Some(rest) = inner.trim().strip_prefix("model_providers") else {
        return false;
    };
    let Some(rest) = rest.trim().strip_prefix('.') else {
        return false; // 光杆 [model_providers]
    };
    let name = rest.trim();
    name == "recodex" || name.starts_with("recodex.")
}

fn strip_recodex_config(
    content: &str,
    owned: Option<&BTreeSet<String>>,
) -> (String, Option<String>) {
    let fallback: BTreeSet<String> = ["model_provider".to_string()].into_iter().collect();
    let owned = owned.unwrap_or(&fallback);

    let mut saved: Option<String> = None;
    let mut content = content.to_string();
    if let Some((s, e)) = marked_block_span(&content) {
        saved = saved_provider_in(&content[s..e]);
        content = format!("{}{}", &content[..s], &content[e..]);
    }

    let top_len = top_level_len(&content);
    let mut out = String::with_capacity(content.len());
    let mut in_recodex_table = false;
    let mut offset = 0usize;
    for line in content.split_inclusive('\n') {
        let at = offset;
        offset += line.len();
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_recodex_table = is_recodex_provider_table(trimmed);
            if in_recodex_table {
                continue;
            }
            out.push_str(line);
            continue;
        }
        if in_recodex_table {
            continue; // 属于 [model_providers.recodex] 或它子表的键
        }
        if trimmed == START_MARKER || trimmed == END_MARKER {
            continue; // 孤儿标记(另一半被别的写入方吃掉了)
        }
        if let Some(rest) = trimmed.strip_prefix(SAVED_PROVIDER_PREFIX) {
            if saved.is_none() {
                let value = rest.trim().trim_matches('"');
                if !value.is_empty() {
                    saved = Some(value.to_string());
                }
            }
            continue;
        }
        // 顶层的键才可能是我们的;表头之后的同名键属于那张表,不许碰
        // (比如 [profiles.work] 里的 model_provider)。
        if at < top_len {
            if let Some(value) = model_provider_value(trimmed) {
                if value != "recodex" && !value.is_empty() {
                    saved = Some(value.to_string()); // 用户此刻的选择,压过块内旧值
                }
                continue;
            }
            if let Some(i) = trimmed.find('=') {
                if owned.contains(trimmed[..i].trim()) {
                    continue;
                }
            }
        }
        out.push_str(line);
    }
    // 把剥离开出来的空行收拢成一个
    (collapse_blank_runs(&out), saved)
}

// 把一行顶层 `model_provider` 插回第一个表头之前 —— 追加到 EOF 会让它落进最后一张表。
fn insert_top_level_model_provider(content: &str, value: &str) -> String {
    // 尾部带一个空行:还回去的这行紧贴表头虽然合法,但配置是给人看的。
    let line = format!("model_provider = \"{value}\"\n\n");
    match first_table_header_offset(content) {
        Some(idx) => {
            let (before, after) = content.split_at(idx);
            format!("{before}{line}{after}")
        }
        None => {
            let mut out = content.to_string();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(&line);
            out
        }
    }
}

/// Returns content with a fresh managed block installed. Any previous ReCodex
/// region (marked or bare) is stripped first, so repeated writes never duplicate
/// the provider. The block is inserted just before the first TOML table header —
/// appending at EOF would strand the block's top-level `model_provider` key
/// inside the file's last table, so Codex (and Codex++'s config re-serialiser)
/// would silently drop it. With no table the block is appended.
///
/// 用户原本的顶层 `model_provider` 会被摘掉并存进块内 —— 留着它就是顶层重复键,
/// Codex 会整份 config.toml 解析失败,比「设置没生效」严重得多。
pub fn install_block(content: &str, body: &str) -> String {
    let owned = top_level_keys_of(body);
    let (cleaned, saved) = strip_recodex_config(content, Some(&owned));
    let block = render_marked_block(body, saved.as_deref());
    if cleaned.trim().is_empty() {
        return block;
    }
    if let Some(idx) = first_table_header_offset(&cleaned) {
        let (before, after) = cleaned.split_at(idx);
        let mut out = String::with_capacity(cleaned.len() + block.len() + 2);
        out.push_str(before);
        if !before.is_empty() && !before.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&block);
        out.push('\n');
        out.push_str(after);
        return out;
    }
    // 没有表头就追加在末尾。必须先削平尾部空行再补一个空行分隔 —— 否则每装一次
    // 就多一个空行(剥离会留下空行,collapse 只压到两个),安装就不幂等了。
    // 语料测试的 no-tables 用例盯着这一点。
    let mut base = cleaned.trim_end_matches('\n').to_string();
    base.push('\n');
    base.push('\n');
    base.push_str(&block);
    base
}

/// Returns content with every ReCodex-managed region removed (marker-based or a
/// bare surviving `[model_providers.recodex]` table),并把安装时接管掉的
/// 用户默认 provider 还回顶层 —— 我们借走的东西要还,否则用户卸载后
/// 默认 provider 就被我们静默吃掉了。
pub fn remove_block(content: &str) -> String {
    let (stripped, saved) = strip_recodex_config(content, None);
    let restored = match saved {
        Some(prev) => insert_top_level_model_provider(&stripped, &prev),
        None => stripped,
    };
    restored.trim_end_matches('\n').to_string() + if restored.ends_with('\n') { "\n" } else { "" }
}

/// 顶层 `model` 的托管标记。带这个标记的行是我们写的,可以更新;不带的是用户
/// 自己选的,一个字不碰。
const MANAGED_MODEL_MARK: &str = "# recodex-managed-model";

/// 把顶层 `model` 设成 `model`,**仅当**这一行由我们托管、或者本来就没有这个键。
///
/// 为什么用行内注释做标记,而不是另开状态文件、也不塞进托管块:
///   - 状态文件 = 同一份状态两个主人,理由同 `SAVED_PROVIDER_PREFIX`;
///   - 塞进托管块会改变块体,而块体是 Go 与 Rust 两个写入方**逐字节比对**的共享语料
///     (见 docs/recodex-client.md「两侧行为对照语料」)。为一个默认值去动那份契约
///     不划算 —— 那块出过「顶层重复键导致整份 config.toml 解析失败」的事故。
///
/// 只在**第一个表头之前**查找与写入:`[profiles.work]` 里的 `model` 属于那张表,
/// 分毫不能碰。理由与 `top_level_len` 上那段注释一致。
///
/// Codex++ 会重新序列化整份 config.toml 并丢掉注释。标记没了之后我们就再也不动
/// 这一行,用户停在当时的模型上 —— 降级成「不再自动跟进」,而不是覆盖用户的选择。
/// ponytail: 真要跨重新序列化保住托管关系,再上状态文件。
pub fn set_managed_model(content: &str, model: &str) -> String {
    let model = model.trim();
    if !model_name_is_safe(model) {
        return content.to_string();
    }
    let head_len = top_level_len(content);
    let (head, tail) = content.split_at(head_len);

    let mut out = String::with_capacity(content.len() + model.len() + 48);
    let mut replaced = false;
    let mut found_unmanaged = false;
    for line in head.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if !replaced && !trimmed.starts_with('#') && model_value(trimmed).is_some() {
            if line.contains(MANAGED_MODEL_MARK) {
                out.push_str(&format!("model = \"{model}\" {MANAGED_MODEL_MARK}\n"));
                replaced = true;
                continue;
            }
            // 用户自己写的 model:原样保留,并且整份内容不做任何改动。
            found_unmanaged = true;
        }
        out.push_str(line);
    }
    if found_unmanaged {
        return content.to_string();
    }
    if !replaced {
        // 没有这个键 —— 插在顶层区最前面。绝不追加到 EOF:顶层键落在最后一张表
        // 之后,按 TOML 规则就属于那张表,等于没设置(与托管块同一个坑)。
        out = format!("model = \"{model}\" {MANAGED_MODEL_MARK}\n{out}");
    }
    out.push_str(tail);
    out
}

/// 模型名是否安全到可以直接拼进 `config.toml`。
///
/// **这个值来自网络** —— 上游 models manifest 里的 `slug`,而下面是
/// `format!("model = ...")` 直接拼字符串,没有任何转义。一个含双引号和换行的
/// slug 就能注入任意 TOML:让它以 `gpt-5` 加一个双引号结尾、后面接换行和一段
/// `[model_providers.<名字>]` 表,写进去就等于给用户凭空加了一个 provider,
/// 之后所有对话都发去攻击者那边。manifest 走 HTTPS,但**网关是用户可配的** ——
/// 不能假设那一端可信。
///
/// (写这段注释时用多行代码块演示过那个 payload,结果它把 doc comment 撑破、
///  真的变成了源码里的 TOML —— 这个漏洞的杀伤力不用再论证了。)
///
/// 放在这里而不是 `recommended_model`:这是所有写入的必经之路,守住这一处
/// 就守住了全部调用方。真实模型名(gpt-5.6-sol / gpt-6-astra)本来就只用
/// 字母数字和 `-` `.` `_`,这条限制不会误伤。
fn model_name_is_safe(model: &str) -> bool {
    !model.is_empty()
        && model.len() <= 128
        && model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
}

/// 顶层 `model` 是否由我们托管 —— 带标记,或者压根还没有这个键。
///
/// 给调用方做**便宜的前置判断**用:用户自己写过 model 的机器,直接跳过,
/// 连拉 manifest 的网络请求都不发。
pub fn model_is_managed(content: &str) -> bool {
    let head = &content[..top_level_len(content)];
    match head
        .split_inclusive('\n')
        .find(|line| {
            let trimmed = line.trim_start();
            !trimmed.starts_with('#') && model_value(trimmed).is_some()
        }) {
        Some(line) => line.contains(MANAGED_MODEL_MARK),
        None => true,
    }
}

/// 把顶层 `model` 更新成 `model` 并落盘。返回是否真的写了。
///
/// 内容没变就不写 —— 每次启动都重写一遍 config.toml 会无谓地动用户文件的 mtime,
/// 也给「谁改了我的配置」这类排查添噪音。
pub fn apply_managed_model(model: &str) -> io::Result<bool> {
    let path = config_path()?;
    let current = read_or_empty(&path)?;
    let next = set_managed_model(&current, model);
    if next == current {
        return Ok(false);
    }
    refuse_if_would_break(&current, &next)?;
    write_atomic(&path, next.as_bytes())?;
    Ok(true)
}

/// 顶层 `model` 这一行当前的值(不含引号);没有该键时返回 None。
pub fn managed_model(content: &str) -> Option<String> {
    content[..top_level_len(content)]
        .lines()
        .map(str::trim_start)
        .filter(|line| !line.starts_with('#'))
        .find_map(model_value)
        .map(str::to_string)
}

/// 解析一行顶层 `model = "x"`,返回去引号的值。
/// 只认这个精确的键:`model_provider`、`model_providers`、`model_reasoning_effort`
/// 都不匹配 —— 它们在 "model" 之后跟的是 `_`,不是 `=`。
fn model_value(trimmed: &str) -> Option<&str> {
    let rest = trimmed.strip_prefix("model")?.trim_start();
    let rest = rest.strip_prefix('=')?.trim();
    // 去掉行尾注释再剥引号,否则 `"x" # mark` 会被当成值的一部分。
    let rest = match rest.strip_prefix('"') {
        Some(after) => &after[..after.find('"')?],
        None => rest.split('#').next()?.trim(),
    };
    Some(rest)
}

fn read_or_empty(path: &Path) -> io::Result<String> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(String::new()),
        Err(err) => Err(err),
    }
}

// Writes atomically: temp file in the same dir, then rename, so a crash mid-write
// can never leave a truncated config. The temp name keys off the process id;
// codexcfg writes are user-driven (login / gateway select) and never concurrent
// within a process, so that is unique enough.
// ponytail: pid-only temp name; add a per-write counter only if concurrent
// writes ever become possible here.
fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    // 权限由**内容**定,不由调用方定。
    //
    // 第一版是让调用方传 `secret` 的,只有登录那条路传对了。结果:
    //   - `apply_managed_model` 每次启动发现推荐模型变了就整篇重写 config.toml,
    //   - `demote_managed_provider` 在用户点「切回官方模式」时整篇重写,
    //     而这条路是**故意**把密钥留在文件里的。
    // 两处都原样保留了 bearer 那一行却传 false,文件当场退回 0644 明文密钥,
    // 而且没有任何征兆。判断挪进来之后,没有调用方需要记得这件事 ——
    // 以后新增的写入方也一样。
    //
    // 非 UTF-8 只有 auth.json 那条路会遇到,它自己走 write_atomic_mode(.., true)。
    let secret = std::str::from_utf8(data).is_ok_and(managed_key_is_inlined);
    write_atomic_mode(path, data, secret)
}

/// `secret = true` 表示内容里有明文密钥,落盘要收到 0600。
pub(crate) fn write_atomic_mode(path: &Path, data: &[u8], secret: bool) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "path has no parent directory"))?;
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".recodex-{}.tmp", std::process::id()));
    if let Err(err) = write_tmp(&tmp, data, secret) {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    if let Err(err) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}

/// 写临时文件。`secret` 时用 0600 **建**文件,而不是写完再 chmod。
///
/// 顺序有三档,只有第一档是对的:
///   1. 建文件时就带 0600 —— 密钥任何一刻都没以宽权限存在过。
///   2. 先 write 再 chmod 再 rename —— 从 write 到 chmod 之间那份明文长期凭据
///      是 umask 默认权限(通常 0644),窗口虽短但确实存在。
///   3. 先 rename 再 chmod —— 窗口更长,而且文件已经在最终路径上了。
///
/// 第一版写成了第 2 档,审计时改到第 1 档。mac_env::register_launchd 和 Go 侧的
/// writeFileAtomic 写的是同一类文件,三个实现不能一边严一边松。
///
/// rename 会把 tmp 的 inode 连同权限一起搬到目标路径,所以目标原来是 0644 也没关系,
/// 换完就是 0600 —— 不需要在 rename 之后再补一次。
#[cfg(unix)]
fn write_tmp(tmp: &Path, data: &[u8], secret: bool) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::fs::PermissionsExt;
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    if secret {
        opts.mode(0o600);
    }
    let mut file = opts.open(tmp)?;
    if secret {
        // `.mode()` 只作用于**新建**。上一次崩溃留下的同名 tmp 会被复用,权限还是旧的,
        // 所以再显式收一次。
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(data)?;
    sync_tmp(&file)
}

/// Windows 上没有 0600 这一说,`~/.codex` 靠的是用户目录本身的 ACL。
#[cfg(not(unix))]
fn write_tmp(tmp: &Path, data: &[u8], _secret: bool) -> io::Result<()> {
    use std::io::Write;
    let mut file = fs::File::create(tmp)?;
    file.write_all(data)?;
    sync_tmp(&file)
}

/// rename 只保证**目录项**的替换是原子的,不保证内容已经落盘。
///
/// 少了这一步,断电或强杀之后可能留下一个长度为 0 的 config.toml ——
/// 托管块连同内联的长期密钥一起消失,而文件本身看上去是完好的
/// (不是损坏、不是缺失,就是空的),用户只会看到"忽然要重新登录"。
/// 必须在 rename **之前** fsync:rename 之后再补就换不回已经丢掉的数据了。
fn sync_tmp(file: &fs::File) -> io::Result<()> {
    file.sync_all()
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name: OsString = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

/// 把托管块里的 `env_key = "RECODEX_KEY"` 就地换成
/// `experimental_bearer_token = "<key>"`。换不了就返回 None(调用方原样用)。
///
/// 为什么要换:Codex 认钥匙只有三条互斥的路,实测(codex-cli 0.153.4,隔离
/// CODEX_HOME)结论是:
///
/// | 配置 | Authorization | 本地 image_gen |
/// |---|---|---|
/// | `env_key`,变量存在 | `Bearer <env>` | ✅ |
/// | `env_key`,变量缺失 | **本地硬失败,一个请求都不发** | — |
/// | `requires_openai_auth` + auth.json | `Bearer <auth.json>` | ❌ **静默消失** |
/// | `experimental_bearer_token` | `Bearer <config>` | ✅ |
///
/// `env_key` 那条把「Codex 能不能认证」绑在了**进程的环境变量**上,而这正是线上
/// 最大一类 401 的成因:macOS 从 Dock / 访达点开 Codex.app,父进程是 launchd,
/// 根本不继承 shell 环境(24h 内 5005 次,占 401 的 86%)。`requires_openai_auth`
/// 能绕开环境变量,但会把本地 image_gen 工具注册干掉 —— 用户能生成图、界面一张
/// 都看不到,且无任何报错,比 401 更难查。只剩 bearer 这一条既不依赖环境、又保住
/// 工具注册。
///
/// 代价是明文密钥落进 config.toml,而这个文件用户会截图、会贴进工单 —— 这是
/// 本文件此前拒绝这条路的理由。现在接受它,因为:同一把密钥早就以明文躺在
/// `official-mode.json`、Windows 注册表和 macOS LaunchAgent plist 里了,
/// config.toml 并没有新增一个泄露面;而落盘时收到 0600(见 write_atomic_mode)。
///
/// 模板本身**不动**:服务端下发的块仍是 env_key 形状,替换只发生在写盘这一步。
/// 这样旧客户端完全不受影响,回滚是发一版客户端而不是改服务端。
pub fn inline_managed_key(block: &str, key: &str) -> Option<String> {
    let key = key.trim();
    if !key_is_safe_for_toml(key) || managed_key_is_inlined(block) {
        return None;
    }
    // 不止一行 env_key 就整个不换。两条理由:
    //   - Go 侧原来是 ReplaceAllString(全换),块里有第二个 provider 时会把
    //     **我们的网关 Key 写进第三方 provider 的槽位** —— 那一行的 base_url
    //     指向别人的服务器,等于主动把长期凭据发出去;
    //   - 这边原来只替第一行,于是同一份块从 CLI 和从桌面端写出来是两个文件。
    // 两边统一成「拿不准就不换」。这类块只可能来自运维配的 RawBlock,
    // 退回 env_key 正是它今天的行为。Go 侧同名函数注释里有同一段。
    if block
        .lines()
        .filter(|line| is_env_key_line(line.trim_start()))
        .count()
        != 1
    {
        return None;
    }
    let mut out = String::with_capacity(block.len() + key.len());
    let mut replaced = false;
    // split_inclusive 保留原行尾:用 lines() 会把 CRLF 悄悄改成 LF,顺手改到
    // 我们没打算碰的行。
    for segment in block.split_inclusive('\n') {
        let line = segment.trim_end_matches(['\n', '\r']);
        let eol = &segment[line.len()..];
        let indent = &line[..line.len() - line.trim_start().len()];
        if !replaced && is_env_key_line(line.trim_start()) {
            out.push_str(indent);
            out.push_str("experimental_bearer_token = \"");
            out.push_str(key);
            out.push('"');
            out.push_str(eol);
            replaced = true;
        } else {
            out.push_str(segment);
        }
    }
    replaced.then_some(out)
}

/// 这个块已经是 bearer 形态了吗。自诊断靠它分叉:答错了就会让用户去重开终端,
/// 解决一个和终端无关的问题。
pub fn managed_key_is_inlined(block: &str) -> bool {
    block.lines().any(|line| {
        line.trim_start()
            .strip_prefix("experimental_bearer_token")
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    })
}

/// 认一行 `env_key = "..."`。
///
/// **引号必须闭合**。这一条是跨语言对齐的硬要求，不是洁癖：Go 侧
/// `managedEnvKeyLinePattern` 写的是 `env_key\s*=\s*"[^"]*"`，闭合引号是模式的一部分。
/// 这边原来只检查「= 后面以引号开头」，于是一行
/// `env_key = "RECODEX_KEY`（少了闭合引号）在两侧结论相反 ——
/// Go 拒绝替换、Rust 照换，同一份托管块从 CLI 和从桌面端写出来又是两个文件。
///
/// 这正是 inline_managed_key 那段注释要根治的分叉，只是触发条件从「多个 env_key」
/// 换成了「引号没闭合」。实测确认过（2026-09-08 合并前审计）。
///
/// 这种块只可能来自运维手配的 RawBlock（模板渲染的永远闭合），而且引号不闭合的
/// TOML 本来就是坏的、Codex 自己也读不了 —— 但「谁都救不了」不等于「两边可以不一致」，
/// 何况 Rust 那条路会把**真实密钥**拼进一份坏文件里。
fn is_env_key_line(trimmed: &str) -> bool {
    let Some(rest) = trimmed
        .strip_prefix("env_key")
        .map(str::trim_start)
        .and_then(|rest| rest.strip_prefix('='))
        .map(str::trim_start)
    else {
        return false;
    };
    // 开引号之后必须还有一个闭引号，与 Go 的 `"[^"]*"` 同义。
    rest.strip_prefix('"')
        .is_some_and(|after| after.contains('"'))
}

/// 钥匙是**拼**进 TOML 字符串的,不是转义进去的。凡是能撑破那对引号、或让这一行
/// 变成别的语义的字符,一律拒绝 —— 宁可退回今天的 env_key 行为,也不能写出半截
/// 配置(TOML 一坏,Codex 连整份 config 都读不了)。
fn key_is_safe_for_toml(key: &str) -> bool {
    if key.is_empty() || key.len() > 512 {
        return false;
    }
    key.chars().all(|c| {
        c.is_ascii()
            && !c.is_ascii_control()
            && !matches!(c, '"' | '\\' | '\'' | '#' | ' ' | '\t')
    })
}

/// 当前这台机器上持久化的网关密钥。进程环境优先,读不到再去用户作用域
/// (Windows 注册表 / macOS 那个 0600 文件)捞。
fn gateway_key() -> String {
    if let Ok(value) = std::env::var(SUB2API_ENV_KEY) {
        if !value.trim().is_empty() {
            return value.trim().to_string();
        }
    }
    stored_key().unwrap_or_default()
}

fn stored_key() -> Option<String> {
    #[cfg(windows)]
    {
        return read_user_env_from_registry(SUB2API_ENV_KEY);
    }
    #[cfg(target_os = "macos")]
    {
        return mac_env::load(SUB2API_ENV_KEY);
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        None
    }
}

/// Splices the given block into `~/.codex/config.toml`, preserving all other
/// content, and writes atomically.
pub fn apply_config(body: &str) -> io::Result<()> {
    apply_config_with_key(body, "")
}

/// 登录/换发必须走这个,并把**这一次**的钥匙传进来。
///
/// `apply_login` 是先写 config.toml 再写环境变量的,让这里自己去环境里捞会捞到
/// **上一把** —— 写出一个当场 401 的配置,而且用户看不出哪里不对。
pub fn apply_config_with_key(body: &str, key: &str) -> io::Result<()> {
    let path = config_path()?;
    let cur = read_or_empty(&path)?;
    let key = if key.trim().is_empty() {
        gateway_key()
    } else {
        key.trim().to_string()
    };
    let body = inline_managed_key(body, &key).unwrap_or_else(|| body.to_string());
    let mut next = install_block(&cur, &body);
    // 重装块会把上一轮的别名段一起换掉，这里按当前钥匙重新生成（见 with_history_aliases）。
    if let Ok(dir) = codex_dir() {
        next = with_history_aliases(&next, &session_provider_refs(&dir));
    }
    if next == cur {
        return Ok(());
    }
    // 不在这里判 secret:`write_atomic` 自己看内容。这条路以前是唯一判对的,
    // 也正因为「只有它判对」才掩住了另外两条路的问题。
    //
    // 落盘前用真解析器验一遍,三条分支的取舍见 apply_validated。
    apply_validated(&path, &cur, &next, &body)
}

/// 备份文件名。两份用途完全不同,不要合并:
///   - bak    是「我们第一次动这台机器之前,用户原本的样子」,只存一次,永不覆盖。
///   - broken 是「这一次重建之前那份坏的」,每次覆盖,给排查用。
const CONFIG_BACKUP_SUFFIX: &str = ".recodex-bak";
const CONFIG_BROKEN_SUFFIX: &str = ".recodex-broken";

/// 报告一段内容 Codex 能不能读进去。
///
/// 用真正的解析器,不再靠文本扫描 —— 文本扫描上栽过三次:三个扫描函数因为托管块
/// 自己的 `{` 触发「说不清就放弃」而整体成了死代码;清残留的精确匹配漏掉
/// `[model_providers.recodex.http_headers]` 子表,造出一份 `duplicate key` 的文件,
/// 而官方 codex **硬失败不回落**,于是客户所有配置一起失效,表现是"客户端用不了"。
pub fn validate_toml(content: &str) -> Result<(), toml::de::Error> {
    content.parse::<toml::Value>().map(|_| ())
}

fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// 在我们第一次改这份文件之前,把用户原本的样子留下来。
///
/// **只存一次**:第二次再存就会把「用户原样」覆盖成「已经被我们改过的样子」,
/// 那份备份也就失去了意义。存不下来不算失败,备份是额外的保险,不该挡住登录。
fn backup_once(path: &Path, content: &str) {
    let backup = suffixed(path, CONFIG_BACKUP_SUFFIX);
    if backup.exists() {
        return;
    }
    // 用户原本**没有** config.toml 时也要写 —— 写一个空文件当哨兵。
    // 不写的话,第二次写入时 cur 已经是**我们的**内容,会被当成"用户原样"存进去:
    // 备份从此指向一份我们自己的旧块,想还原的时候还原了个寂寞。
    //
    // 无条件 0600:这是我们的诊断产物,没有第二个人需要读它。不能走 write_atomic ——
    // 它靠正则嗅探 bearer 那一行,用户自己的密钥可能是别的写法,嗅不到就摊成 0644。
    let _ = write_atomic_mode(&backup, content.as_bytes(), true);
}

/// 进来是好的、出去会变坏 —— 拒绝。**所有** config.toml 写入方共用的最后一道闸。
///
/// 审计时发现 apply_managed_model(每次启动发现推荐模型变了就整篇重写)和
/// demote_managed_provider(切回官方模式)都是直接 write_atomic,绕过了 apply_validated。
/// 「不许留下读不进去的文件」这条承诺要对**每一个**写入方成立,所以抽出来共用;
/// 以后新增的写入方也一样,落盘前调一下。
fn refuse_if_would_break(cur: &str, next: &str) -> io::Result<()> {
    if validate_toml(next).is_err() && validate_toml(cur).is_ok() {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "refusing to write: the result would not parse as TOML",
        ));
    }
    Ok(())
}

/// 把 next 落盘,但先确认它是能被读进去的。
///
/// 三条分支,区别在于**当前那份是不是好的**:
///
///   next 能解析              → 正常写入。绝大多数情况走这里。
///   next 不能 / cur 能       → **是我们把它写坏的**。拒绝写入,保住用户手上那份。
///   next 不能 / cur 也不能   → 用户那份本来就废了,重建。
///
/// 第三条就是「备份 + 完全新建」,但只用在**已经没有东西可保**的时候 ——
/// cc-switch 的教训正在这里:他们无条件整份覆盖,把用户的 MCP servers、plugins、
/// 项目信任全洗掉(issue #4254 / #1088 / #1863 / #4317)。同样的动作,
/// 放在「文件已经读不进去」这个前提下才是救人,否则就是杀人。
///
/// 与 Go 侧 clientcfg.applyValidated 逐条对应,改一侧必须改另一侧。
fn apply_validated(path: &Path, cur: &str, next: &str, fresh_block: &str) -> io::Result<()> {
    backup_once(path, cur);

    if validate_toml(next).is_ok() {
        return write_atomic(path, next.as_bytes());
    }
    // 进来是好的、出去变坏了 —— 一定是我们的合并逻辑有问题。
    // 宁可这次登录失败,也不能留下一份读不进去的文件。
    refuse_if_would_break(cur, next)?;

    // cur 本来就解析不了。保留证据,然后只写我们的块。
    // 不保留用户的任何内容:那些内容正是解析失败的来源,原样搬过来等于把病带走。
    // 无条件 0600:这份**按定义是坏的**,密钥那一行本身可能就是坏掉的地方
    // (引号没闭合之类),嗅探正则匹配不上就会摊成 0644 明文。
    let _ = write_atomic_mode(&suffixed(path, CONFIG_BROKEN_SUFFIX), cur.as_bytes(), true);
    let rebuilt = install_block("", fresh_block);
    if validate_toml(&rebuilt).is_err() {
        // 我们自己渲染的块都解析不了,那是模板的问题,不是用户的。什么都别写。
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "refusing to write: rebuilt managed block is invalid TOML",
        ));
    }
    write_atomic(path, rebuilt.as_bytes())
}

/// Removes our managed block from `~/.codex/config.toml`. Deletes the file if we
/// created it (nothing else left).
/// 切到官方模式用:**保留 provider 定义,只摘掉默认选择那一行**。
///
/// 为什么不能像 `restore_config` 那样整块删掉:Codex 的每个会话都把当时用的
/// provider 名记在 rollout 文件里(`payload.model_provider = "recodex"`)。
/// 把定义删了之后,恢复旧对话时它找不到这个 provider,直接拒绝打开并报
/// 「Model provider `recodex` not found」—— 用户切回官方账号,**历史对话就全打不开了**
/// (新对话没事,因为新对话用的是官方 provider)。这是实测到的。
///
/// 只摘掉 `model_provider = "recodex"`:新对话回到官方 provider,
/// 旧对话仍然解析得到定义、能继续打开。
pub fn demote_managed_provider() -> io::Result<()> {
    let path = config_path()?;
    let cur = read_or_empty(&path)?;
    // 不再以「标记还在」为前提:Codex++ 重新序列化后标记就没了,
    // 那时旧实现直接返回,`model_provider = "recodex"` 会永久留在用户配置里。
    // 也不再全文件匹配字面量 —— 那会连 [profiles.x] 里的同名键一起删掉。
    let top_len = top_level_len(&cur);
    let mut out = String::with_capacity(cur.len());
    let mut offset = 0usize;
    let mut changed = false;
    for line in cur.split_inclusive('\n') {
        let at = offset;
        offset += line.len();
        if at < top_len && model_provider_value(line.trim()) == Some("recodex") {
            changed = true;
            continue;
        }
        out.push_str(line);
    }
    if !changed {
        return Ok(());
    }
    refuse_if_would_break(&cur, &out)?;
    write_atomic(&path, out.as_bytes())
}

pub fn restore_config() -> io::Result<()> {
    let path = config_path()?;
    let cur = read_or_empty(&path)?;
    // 按内容判断而不是看标记:Codex++ 重新序列化会丢掉注释标记,
    // 旧实现那时会直接返回,把我们的 provider 永久留在用户的配置里。
    let (stripped, _) = strip_recodex_config(&cur, None);
    if stripped == cur {
        return Ok(()); // 这份文件里没有我们的东西
    }
    let next = remove_block(&cur);
    if next == cur {
        return Ok(());
    }
    if next.trim().is_empty() {
        return remove_if_exists(&path);
    }
    write_atomic(&path, next.as_bytes())
}

/// Writes the server-provided `auth.json` bytes, backing up any pre-existing
/// user file exactly once so `restore_auth` can put it back.
pub fn write_auth(data: &[u8]) -> io::Result<()> {
    let path = auth_path()?;
    let backup = with_suffix(&path, AUTH_BACKUP_SUFFIX);
    if path.exists() && !backup.exists() {
        let orig = fs::read(&path)?;
        // auth.json 里是 OAuth token,备份件同样是。`write_atomic` 的内容嗅探
        // 只认 TOML 的 bearer 行,认不出 JSON —— 所以这三处显式标 secret。
        // Go 侧 internal/clientcfg 写这几个文件用的就是 0600,不能一边严一边松。
        write_atomic_mode(&backup, &orig, true)?;
    }
    // Record ownership before replacing auth.json so logout can recover even if
    // the following write fails or the process exits.
    // 这个标记文件内容只有 "recodex",没有秘密,按普通文件写。
    write_atomic(&with_suffix(&path, AUTH_MANAGED_SUFFIX), b"recodex\n")?;
    write_atomic_mode(&path, data, true)
}

/// Reads back the `auth.json` **we** wrote, if we still own it.
///
/// Returns `None` when the ownership marker is absent — that file belongs to the
/// user's own Codex login and is none of our business to copy around.
/// Used by the official-mode snapshot: `restore_auth` deletes our auth outright,
/// so anything that wants it back has to grab it first.
pub fn read_managed_auth() -> io::Result<Option<Vec<u8>>> {
    let path = auth_path()?;
    if !with_suffix(&path, AUTH_MANAGED_SUFFIX).exists() {
        return Ok(None);
    }
    match fs::read(&path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Restores the pre-ReCodex `auth.json` from backup if present, otherwise removes
/// the file we wrote.
pub fn restore_auth() -> io::Result<()> {
    let path = auth_path()?;
    let managed = with_suffix(&path, AUTH_MANAGED_SUFFIX);
    let backup = with_suffix(&path, AUTH_BACKUP_SUFFIX);
    if !managed.exists() && !backup.exists() {
        return Ok(());
    }
    match fs::read(&backup) {
        Err(err) if err.kind() == ErrorKind::NotFound => {
            remove_if_exists(&path)?;
            remove_if_exists(&managed)
        }
        Err(err) => Err(err),
        Ok(orig) => {
            write_atomic_mode(&path, &orig, true)?;
            remove_if_exists(&managed)?;
            remove_if_exists(&backup)
        }
    }
}

/// 测试沙箱:置了它就不碰注册表,改在它指向的目录里读写同名文件。
///
/// `USERPROFILE` / `HOME` 能把**文件**写入关进沙箱 —— mac 的持久化正好落在
/// `codex_dir()` 下,天然被关住。但 Windows 走 `setx`,写的是注册表
/// `HKCU\Environment`,**不受任何环境变量重定向约束**。于是集成测试里一句
/// `apply_login(..., "sk-recodex2")` 会把开发者本机真实的 `RECODEX_KEY`
/// **永久**改成假值 —— 2026-08-26 就这么发生过:本机 Codex 一路 401,
/// 而且因为坏的是持久值,重启、重新登录都好不了,只能手工改回来。
///
/// 光让测试改传空 env_key 修不掉:`officialmode` 还会自己调 `set_user_env`。
/// 所以挡在**唯一那两个碰注册表的函数**里 —— 谁调都逃不掉。
// 不加 cfg(windows):macOS 的 register_launchd / unregister_launchd 也要用它。
// launchctl 写的是进程外的登录会话状态，和 Windows 的 setx 同属「HOME 重定向
// 关不住」那一类，共用同一个沙箱开关才拦得住测试污染开发机。
// 标成 windows-only 会让 macOS 构建直接编译不过（发布通道因此堵过一次）。
const ENV_SANDBOX: &str = "RECODEX_ENV_SANDBOX";

fn env_sandbox_path(name: &str) -> Option<PathBuf> {
    let dir = std::env::var_os(ENV_SANDBOX)?;
    Some(PathBuf::from(dir).join(format!("{name}.env")))
}

// Runs `setx NAME VALUE`, persisting to HKCU\Environment. Newly started
// processes pick the change up; the running Codex++ does not, which is why the
// caller surfaces a "restart the desktop app" hint after first login — the same
// contract the CLI documents.
#[cfg(windows)]
fn setx(name: &str, value: &str) -> io::Result<()> {
    if let Some(path) = env_sandbox_path(name) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        return fs::write(path, value);
    }
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new("setx")
        .arg(name)
        .arg(value)
        .creation_flags(CREATE_NO_WINDOW)
        .output()?;
    if !out.status.success() {
        return Err(io::Error::new(
            ErrorKind::Other,
            format!(
                "setx {name} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        ));
    }
    Ok(())
}

/// Reads a user environment variable straight from `HKCU\Environment`.
///
/// A process's environment block is a snapshot of its parent's, so a launcher
/// started from a shell that predates the last sign-in still carries the old
/// key even though `setx` already wrote the new one. Codex (spawned as our
/// child) then inherits the stale key and the gateway answers
/// `SUBSCRIPTION_NOT_FOUND`. The registry is the authoritative copy.
#[cfg(windows)]
fn read_user_env_from_registry(name: &str) -> Option<String> {
    if let Some(path) = env_sandbox_path(name) {
        return fs::read_to_string(path).ok();
    }
    use std::os::windows::ffi::OsStringExt;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
    };

    let subkey: Vec<u16> = "Environment\0".encode_utf16().collect();
    let value: Vec<u16> = format!("{name}\0").encode_utf16().collect();
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ;

    let mut size: u32 = 0;
    // First call sizes the buffer (in bytes), second call fills it.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            flags,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    if status != ERROR_SUCCESS || size == 0 {
        return None;
    }
    let mut buffer = vec![0u16; (size as usize).div_ceil(2)];
    let mut size_out = size;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value.as_ptr(),
            flags,
            std::ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut size_out,
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let len = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(buffer.len());
    let text = OsString::from_wide(&buffer[..len])
        .to_string_lossy()
        .into_owned();
    (!text.trim().is_empty()).then_some(text)
}

/// Re-reads `RECODEX_KEY` from the user registry into this process, so the Codex
/// we spawn uses the key from the most recent sign-in rather than whatever our
/// parent process happened to hold. Returns true when the value changed.
/// No-op off Windows, where there is no `setx`/registry split.
pub fn refresh_key_env_from_user_scope() -> bool {
    #[cfg(windows)]
    {
        let Some(stored) = read_user_env_from_registry(SUB2API_ENV_KEY) else {
            return false;
        };
        if std::env::var(SUB2API_ENV_KEY).ok().as_deref() == Some(stored.as_str()) {
            return false;
        }
        // 注意:这里**不是**「整个进程只调一次」。launcher 每次拉起 Codex 之前都会再调
        // 一遍(用户中途登录/换组织后,注册表里的 key 已经换了,而本进程环境还是旧的)。
        // 那时诊断回传、启动清理、手机远程这些后台线程已经在跑 —— set_var 与并发
        // getenv 严格来说是竞态。仍然接受的理由:改的只有这一个我们自己的键,读它的
        // 只有我们(以及即将被 spawn 的子进程,它在 set_var 之后才创建);真正的根治
        // 是把 key 用 Command::env 显式传给子进程、不再动进程环境(排到 1.3.9 评估)。
        unsafe { std::env::set_var(SUB2API_ENV_KEY, &stored) };
        true
    }
    #[cfg(target_os = "macos")]
    {
        let Some(stored) = mac_env::load(SUB2API_ENV_KEY) else {
            return false;
        };
        // 存量用户补丁,必须在下面那个早退**之前**。
        //
        // launchd 那条路是这一版才加的:在此之前登录过的 mac,0600 文件里有 key,
        // 但 `~/Library/LaunchAgents` 下什么都没有 —— 他们从 Dock / 访达点开
        // Codex.app 照样 401(线上 24h 内 5005 次)。光升级客户端救不了他们,
        // 得等他们自己想到「重新登录一次」才会补上,而没人会想到。
        //
        // 所以这里发现缺 LaunchAgent 就当场补一次,让**升级本身**把人救回来。
        mac_env::ensure_launchd_registered(SUB2API_ENV_KEY, &stored);
        if std::env::var(SUB2API_ENV_KEY).ok().as_deref() == Some(stored.as_str()) {
            return false;
        }
        // Safe here: called once at startup, before any Codex child is spawned.
        unsafe { std::env::set_var(SUB2API_ENV_KEY, &stored) };
        true
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        false
    }
}

/// macOS LaunchAgent 的文件名/Label。一个变量一个 agent。
///
/// `name` 只会是 `RECODEX_KEY` 这种合法环境变量名(字母数字下划线),
/// 拼进文件名安全。
pub(crate) fn macos_launch_agent_label(name: &str) -> String {
    format!("ai.recodex.env.{name}")
}

/// 登录时重新 `launchctl setenv` 的 LaunchAgent。
///
/// **为什么必须有它**:mac 上 0600 文件里的 key 只有走 ReCodex 启动器才会被
/// `refresh_key_env_from_user_scope` 读回进程环境。用户从 Dock / 访达 / 聚焦
/// 直接点开 Codex.app 时,父进程是 launchd —— 环境里根本没有 `RECODEX_KEY`,
/// 直接 401。线上 24h 内 5005 次 macOS 401 就是这么来的(占全部 401 的 86%)。
/// 把变量交给 launchd 之后,无论从哪里启动都读得到。
///
/// **两个用户可见面,别当成实现细节**:
///
/// 1. macOS Ventura 起,`~/Library/LaunchAgents/` 下的东西会出现在
///    「系统设置 → 通用 → 登录项 → 允许在后台」里,首次还会弹一条系统通知。
///    用户会看到一个自己没主动装的后台项目 —— Label 里带 `recodex` 就是为了
///    让他至少认得出是谁的。
/// 2. 用户可以在那里**把它关掉**。文件还在(所以 `ensure_launchd_registered`
///    不会重建),但登录时不再执行 —— 从 Dock 启动的 Codex 又读不到 key 了。
///    这个状态**客户端自己检测不到**。两条信号只能间接看出来:走启动器时
///    `launcher.recodex_key_refreshed_from_user_scope` 会触发(进程环境里没有 key
///    = launchd 那条路没生效);完全不走启动器的用户则只剩服务端 nginx 上的 401 ——
///    那正是这次修复要压下去的数字,压不下去就说明这条路被绕开了。
///
/// 为什么还是要用 LaunchAgent:只靠 launcher 启动时 `launchctl setenv` 覆盖不了
/// 「重启之后用户直接从 Dock 点 Codex.app」那一次 —— 而那恰恰是要修的场景本身。
///
/// 放在 `cfg(target_os)` **之外**:纯文本构造,非 mac 机器也要能跑它的转义测试。
pub(crate) fn macos_launch_agent_plist(name: &str, value: &str) -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n");
    out.push_str("<plist version=\"1.0\">\n<dict>\n");
    out.push_str(&format!(
        "\t<key>Label</key>\n\t<string>{}</string>\n",
        escape_xml(&macos_launch_agent_label(name))
    ));
    out.push_str("\t<key>ProgramArguments</key>\n\t<array>\n");
    for arg in ["/bin/launchctl", "setenv", name, value] {
        out.push_str(&format!("\t\t<string>{}</string>\n", escape_xml(arg)));
    }
    out.push_str("\t</array>\n");
    out.push_str("\t<key>RunAtLoad</key>\n\t<true/>\n");
    out.push_str("</dict>\n</plist>\n");
    out
}

/// LaunchAgent 在用户家目录下的落点。
///
/// 和 label / plist 一样放在 `cfg(target_os)` **之外**:mac 专属的那段代码在
/// Windows 上根本不参与编译,写错了要到 mac 构建时才炸。能抽出来的纯逻辑就抽出来,
/// 让它在**任何**平台上都被编译和测试覆盖到,cfg 里只剩最直白的 fs / Command 调用。
///
/// 路径本身还是跨语言契约的一部分 —— Go 侧 internal/clientcfg 写的是同一个文件。
pub(crate) fn macos_launch_agent_path_in(home: &Path, name: &str) -> PathBuf {
    home.join("Library")
        .join("LaunchAgents")
        .join(format!("{}.plist", macos_launch_agent_label(name)))
}

/// key 里出现 `&` 或 `<` 而不转义,plist 就是非法 XML,launchd 会**静默**跳过它。
/// 表现是「重启之后又 401 了」,没有任何报错可查。
fn escape_xml(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

/// mac 侧「用户级环境变量」的替身。Windows 有 setx+注册表,mac 没有对应物。
///
/// **不放钥匙串。** 第一版放了,结果 `apply_login` 整条配置写入都被耦合到钥匙串上:
/// 任何把 HOME 指到别处的场景(测试沙箱、多用户)钥匙串解析就会阻塞,
/// CI 上表现是某个测试挂死 46 分钟直到超时 —— 不是失败,是卡住。
///
/// 而且它本来也没换来更高的安全性:同一个 `apply_login` **已经**把
/// `auth.json`(里面就是 ReCodex 的访问令牌)明文写进 `~/.codex/`,
/// Windows 那边 `setx` 也是明文进注册表。所以这里用 0600 的文件,
/// 与既有存储同一量级,而且和这个模块其余部分一样是 HOME 相对的。
#[cfg(target_os = "macos")]
mod mac_env {
    use super::{codex_dir, io};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn env_path(name: &str) -> io::Result<PathBuf> {
        // 文件名跟着变量名走,免得以后多存一个键还要改结构
        Ok(codex_dir()?.join("recodex").join(format!("{name}.env")))
    }

    pub(super) fn save(name: &str, value: &str) -> io::Result<()> {
        // 内容是密钥明文。这里原来是 write 完再 set_permissions —— 中间那一瞬
        // 文件是 umask 默认权限(通常 0644)。改走 write_atomic_mode:它**建文件
        // 时**就带 0600,密钥没有任何一刻以宽权限存在过;顺带拿到原子替换,
        // 换发 key 时不会被别的进程读到半截。
        super::write_atomic_mode(&env_path(name)?, value.as_bytes(), true)
    }

    pub(super) fn clear(name: &str) -> io::Result<()> {
        match fs::remove_file(env_path(name)?) {
            Ok(()) => Ok(()),
            // 本来就没有 = 成功。清理要幂等,卸载会重复调用
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// 读不到就当没设过 —— 与 Windows 侧 `read_user_env_from_registry` 语义一致。
    pub(super) fn load(name: &str) -> Option<String> {
        let text = fs::read_to_string(env_path(name).ok()?).ok()?;
        let text = text.trim().to_owned();
        (!text.is_empty()).then_some(text)
    }

    fn launch_agent_path(name: &str) -> io::Result<PathBuf> {
        let home = std::env::var_os("HOME")
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME 未设置"))?;
        Ok(super::macos_launch_agent_path_in(
            std::path::Path::new(&home),
            name,
        ))
    }

    /// 把变量交给 launchd,这样从 Dock / 访达 / 聚焦启动的 Codex.app 也读得到。
    ///
    /// 两步缺一不可:`setenv` 管**当前登录会话**(不用注销重登),
    /// LaunchAgent 管**下次开机**(setenv 活不过重启)。
    pub(super) fn register_launchd(name: &str, value: &str) -> io::Result<()> {
        // launchctl 写的是**进程外**的登录会话状态,HOME 重定向关不住它 ——
        // 和 Windows 的 setx 同一类风险,所以共用同一个沙箱开关。
        if super::env_sandbox_path(name).is_some() {
            return Ok(());
        }
        let path = launch_agent_path(name)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        // 原子写,而且**权限要在 rename 之前定好**。
        //
        // 两个理由叠在一起:
        //   - 原子:换发 key 时若让 launchd 撞见半个 plist,它会当成非法 XML
        //     静默跳过,表现又是「重启之后 401」;
        //   - 先 chmod 再 rename:反过来的话,从 rename 到 chmod 之间这份含**明文
        //     长期凭据**的 plist 是默认的 0644(~/Library/LaunchAgents 就是 0644)。
        //     Go 侧 internal/clientcfg 的 writeFileAtomic 正是先 chmod tmp 再 rename,
        //     两个实现写的是同一个文件,权限保证必须对齐,不能一边严一边松。
        //
        // 走 write_atomic_mode(secret=true):它建 tmp 时就带 0600
        // （OpenOptions.mode），不存在「建档到 chmod 之间」那段窗口。
        //
        // 这里原来是 fs::write 再 set_permissions —— 也就是上面那段注释判定为
        // 「错的」那个顺序，而它守护的正是**明文长期凭据**（plist 里就是 key）。
        // 注释里说的「Go 侧 writeFileAtomic 正是先 chmod tmp 再 rename，两边必须
        // 对齐」其实也没对上：Go 走 os.CreateTemp，建出来天生 0600，从来没有这段窗口。
        //
        // 这一批的第 4 条意图（消灭 write→chmod 的窗口）只落地了 mac_env::save，
        // 同模块、同一把密钥的另一半漏了。2026-09-08 合并前审计查出。
        super::write_atomic_mode(
            &path,
            super::macos_launch_agent_plist(name, value).as_bytes(),
            true,
        )?;
        // 不做 launchctl load:agent 会在下次登录被 launchd 自动扫到,
        // 本次会话已由下面这行 setenv 覆盖。
        run_launchctl(&["setenv", name, value])
    }

    /// 只补**缺失**的那次注册,已经有 LaunchAgent 就什么都不做。
    ///
    /// 给存量用户用:1.2.66 及以前登录过的机器只有 0600 文件、没有 LaunchAgent。
    /// 每次启动都重写会白白动文件 mtime,给「谁改了我的配置」这类排查添噪音;
    /// 内容过期由 `set_user_env` 在登录/换发时负责更新,不归这里管。
    ///
    /// 全程 best-effort:这是顺手的修补,失败不该影响启动。
    pub(super) fn ensure_launchd_registered(name: &str, value: &str) {
        if super::env_sandbox_path(name).is_some() {
            return;
        }
        let Ok(path) = launch_agent_path(name) else {
            return;
        };
        if path.exists() {
            return;
        }
        let _ = register_launchd(name, value);
    }

    /// 撤销 `register_launchd`。两步都要做:漏掉 plist,下次登录会把已作废的 key
    /// 又 setenv 回去。
    pub(super) fn unregister_launchd(name: &str) -> io::Result<()> {
        if super::env_sandbox_path(name).is_some() {
            return Ok(());
        }
        let mut first_error = None;
        if let Ok(path) = launch_agent_path(name) {
            if let Err(error) = fs::remove_file(&path) {
                if error.kind() != io::ErrorKind::NotFound {
                    first_error = Some(error);
                }
            }
        }
        match run_launchctl(&["unsetenv", name]) {
            Ok(()) => first_error.map_or(Ok(()), Err),
            Err(error) => Err(first_error.unwrap_or(error)),
        }
    }

    fn run_launchctl(args: &[&str]) -> io::Result<()> {
        let status = std::process::Command::new("launchctl").args(args).status()?;
        if status.success() {
            return Ok(());
        }
        // 只回操作名和变量名,**绝不**把 args 整个拼进去 —— `setenv` 的第三个参数
        // 就是明文 API key。现在调用方都把这个错误吞掉了,可它一旦被谁记进日志
        // 或抛给用户,密钥就跟着出去了。错误信息里不该出现密钥,哪怕暂时没人看。
        let operation = args.first().copied().unwrap_or("?");
        let name = args.get(1).copied().unwrap_or("?");
        Err(io::Error::other(format!(
            "launchctl {operation} {name} 失败: {status}"
        )))
    }
}

/// Persists `name=value` to the user environment so a freshly launched Codex can
/// read the key. Also sets it on this process, so the Codex the desktop launcher
/// spawns as a child inherits the key immediately — no app restart needed after
/// the first sign-in. `setx` (Windows only) carries it across restarts.
pub fn set_user_env(name: &str, value: &str) -> io::Result<()> {
    note_key_change(name, std::env::var(name).ok().as_deref(), Some(value));
    // Safe on edition 2021; this is the login-time write, not a hot path.
    std::env::set_var(name, value);
    #[cfg(windows)]
    {
        return setx(name, value);
    }
    #[cfg(target_os = "macos")]
    {
        // mac 没有 setx/注册表这一层,用 0600 文件顶替。
        mac_env::save(name, value)?;
        // 光有文件不够:它只有走 ReCodex 启动器时才会被 refresh_key_env_from_user_scope
        // 读回来。用户从 Dock / 访达直接点 Codex.app 时父进程是 launchd,环境里
        // 什么都没有 —— 线上 5005 次 macOS 401 的来源。所以再交给 launchd 一份。
        //
        // best-effort:失败不能让登录失败。config.toml 和 0600 文件都已经写好了,
        // 半途 abort 只会留下更糟的半套状态;走启动器这条路仍然可用。
        let _ = mac_env::register_launchd(name, value);
        return Ok(());
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Ok(())
    }
}

/// Clears a persisted environment variable (and this process's copy). `setx NAME
/// ""` cannot delete the entry but empties it, which Codex treats as unset —
/// matching the CLI.
pub fn unset_user_env(name: &str) -> io::Result<()> {
    note_key_change(name, std::env::var(name).ok().as_deref(), None);
    std::env::remove_var(name);
    #[cfg(windows)]
    {
        return setx(name, "");
    }
    #[cfg(target_os = "macos")]
    {
        // 文件能真删,不像 setx 只能置空
        let cleared = mac_env::clear(name);
        // 即使删文件失败也要撤 launchd,否则下次登录会把已注销的 key 又设回来。
        let _ = mac_env::unregister_launchd(name);
        return cleared;
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Ok(())
    }
}

/// Materialises everything the auth `approved` response carries so the launched
/// Codex uses ReCodex: the rendered config block, `auth.json`, and the key env
/// var. Empty fields are skipped. Mirrors the CLI's `writeCredentials`.
pub fn apply_login(
    config: &str,
    auth_json: &str,
    env_key: &str,
    env_value: &str,
) -> io::Result<()> {
    if !config.is_empty() {
        // 必须传 env_value:下面那行 set_user_env 还没跑,环境里是上一把钥匙。
        apply_config_with_key(config, env_value)?;
    }
    if !auth_json.is_empty() {
        write_auth(auth_json.as_bytes())?;
    }
    if !env_key.is_empty() {
        set_user_env(env_key, env_value)?;
    }
    Ok(())
}

/// Rewrites `~/.codex/config.toml` so Codex talks to `codex_base_url` (a selected
/// gateway's `/backend-api/codex` root). This is the step the desktop was missing
/// — selecting a gateway now actually routes Codex through it.
pub fn route_through_gateway(codex_base_url: &str) -> io::Result<()> {
    if !base_url_is_safe(codex_base_url) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "网关地址含有不能写进配置的字符",
        ));
    }
    // 保留当前的 supports_websockets：apply_config 会整表重写，不读回就会把
    // 服务端下发的 WS 开关冲掉（与 Go 侧 currentSupportsWebsockets 同一处理）。
    apply_config(&render_sub2api_block(
        codex_base_url,
        current_supports_websockets(),
    ))
}

/// Reverts all ReCodex-owned Codex state (config block, auth.json, key env var).
/// Best-effort ordering: a later failure still leaves earlier steps reverted.
pub fn restore_all() -> io::Result<()> {
    restore_config()?;
    restore_auth()?;
    unset_user_env(SUB2API_ENV_KEY)
}

// ---------------------------------------------------------------------------
// 旧对话的 provider 别名。与 Go 侧 internal/clientcfg/history_aliases.go 逐字节一致，
// 由 testdata/history-aliases 语料锁定 —— 两边都会重装托管块，渲染不一致就会互相把
// 对方写的别名段当成异物，叠出重复表（Codex 对重复表硬失败，所有配置一起失效）。
//
// 为什么要补：Codex 给每个对话记下创建它时用的 model_provider（会话文件首行
// session_meta，与 state_*.sqlite 的 threads 表一致）。从别家中转、cockpit-tools 的
// 「API 服务」（codex_local_access）、cc-switch 切过来之后，那些 provider 表没了，
// 旧对话一打开就是「Model provider `xxx` not found」，而新对话一切正常。
//
// 为什么放托管块里：块里的钥匙是内联的，每次登录都会换发。块外的静态拷贝下一次登录
// 就过期，旧对话从 not found 变成 401；放块里每次重装都按当前钥匙重新生成，
// 登出删块时一起消失。任何一步没把握都原样返回 —— 绝不能把能用的配置写坏。
// ---------------------------------------------------------------------------

/// 标出托管块里的别名段，从这一行到块结束标记之前都是生成的。与 Go 侧逐字一致。
pub const HISTORY_ALIAS_MARKER: &str =
    "# recodex history provider aliases (old conversations were created with these providers)";

/// Codex 自带、无需 provider 表的 provider。只列官方内置的那个（与 Go 的 builtinModelProviders 一致）。
const BUILTIN_MODEL_PROVIDERS: &[&str] = &["openai"];

/// 首行读取上限。实测首行中位 22KB、最大 50KB，model_provider 最远在第 49KB
/// （排在一大段 instructions 后面），所以必须读完整行。
const SESSION_META_MAX_LINE: u64 = 4 << 20;

/// 「会话文件 → provider」缓存，放在 <codex_dir>/recodex/ 下，与 Go 侧共用同一份、同一格式。
/// 会话文件首行写下后不再变，按相对路径缓存永远正确；实测 460 个会话冷扫 1.96s、
/// 走缓存约 40ms —— 桌面端每次启动（拉起 Codex 之前）都要走一遍，必须缓存。
const SESSION_PROVIDER_CACHE_NAME: &str = "session-providers.json";

/// 统计 codex_dir 下每个 provider 被多少个对话引用（含已归档）。
pub fn session_provider_refs(codex_dir: &Path) -> std::collections::BTreeMap<String, usize> {
    use std::collections::BTreeMap;
    let cache_path = codex_dir.join("recodex").join(SESSION_PROVIDER_CACHE_NAME);
    let cached = load_session_provider_cache(&cache_path);
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    let mut fresh = false;
    let mut refs: BTreeMap<String, usize> = BTreeMap::new();
    for sub in ["sessions", "archived_sessions"] {
        let mut stack = vec![codex_dir.join(sub)];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(kind) = entry.file_type() else { continue };
                if kind.is_dir() {
                    stack.push(path);
                    continue;
                }
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !name.starts_with("rollout-") || !name.ends_with(".jsonl") {
                    continue;
                }
                let Ok(rel) = path.strip_prefix(codex_dir) else { continue };
                let rel = rel.to_string_lossy().replace('\\', "/");
                let id = match cached.get(&rel) {
                    Some(id) => id.clone(),
                    None => {
                        // 读不出的不进缓存：可能是刚建的会话首行还没写完，下次再读。
                        let Some(id) = session_provider_of(&path) else { continue };
                        fresh = true;
                        id
                    }
                };
                *refs.entry(id.clone()).or_insert(0) += 1;
                seen.insert(rel, id);
            }
        }
    }
    if fresh || seen.len() != cached.len() {
        save_session_provider_cache(&cache_path, &seen);
    }
    refs
}

fn load_session_provider_cache(path: &Path) -> std::collections::BTreeMap<String, String> {
    let Ok(raw) = fs::read(path) else { return Default::default() };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&raw) else { return Default::default() };
    if value.get("v").and_then(|v| v.as_i64()) != Some(1) {
        return Default::default();
    }
    let Some(files) = value.get("files").and_then(|f| f.as_object()) else { return Default::default() };
    files
        .iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
        .collect()
}

fn save_session_provider_cache(path: &Path, files: &std::collections::BTreeMap<String, String>) {
    let body = serde_json::json!({ "v": 1, "files": files });
    let Ok(raw) = serde_json::to_vec(&body) else { return };
    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let _ = write_atomic_mode(path, &raw, true);
}

/// 读会话文件首行，返回其中的 model_provider；读不出返回 None。
fn session_provider_of(path: &Path) -> Option<String> {
    use std::io::{BufRead, BufReader, Read};
    let file = fs::File::open(path).ok()?;
    let mut line = Vec::new();
    BufReader::with_capacity(64 << 10, file.take(SESSION_META_MAX_LINE))
        .read_until(b'\n', &mut line)
        .ok()?;
    let value: serde_json::Value = serde_json::from_slice(&line).ok()?;
    if value.get("type")?.as_str()? != "session_meta" {
        return None;
    }
    let id = value.get("payload")?.get("model_provider")?.as_str()?.trim();
    (!id.is_empty()).then(|| id.to_string())
}

/// 会被补别名的 id：1–128 个可见 ASCII，不含双引号与反斜杠（与 Go 的 historyAliasIDPattern 一致）。
fn history_alias_id_ok(id: &str) -> bool {
    (1..=128).contains(&id.len()) && id.bytes().all(|b| (0x21..=0x7e).contains(&b) && b != b'"' && b != b'\\')
}

fn defined_providers(content: &str) -> Option<BTreeSet<String>> {
    let doc = content.parse::<toml::Value>().ok()?;
    Some(
        doc.get("model_providers")
            .and_then(|v| v.as_table())
            .map(|t| t.keys().cloned().collect())
            .unwrap_or_default(),
    )
}

/// 需要补别名的 provider：被对话引用、没有定义、不是内置、形状规整，按字节序排序。
/// 配置读不进来返回 None。
pub fn history_alias_ids(
    content: &str,
    refs: &std::collections::BTreeMap<String, usize>,
) -> Option<Vec<String>> {
    let defined = defined_providers(&strip_history_aliases(content))?;
    let mut ids: Vec<String> = refs
        .keys()
        .filter(|id| {
            !defined.contains(*id)
                && !BUILTIN_MODEL_PROVIDERS.contains(&id.as_str())
                && history_alias_id_ok(id)
        })
        .cloned()
        .collect();
    ids.sort();
    Some(ids)
}

/// 在托管块里重建别名段：先剥掉旧的，再按 refs 补上需要的。任何一步没把握就不补。
pub fn with_history_aliases(content: &str, refs: &std::collections::BTreeMap<String, usize>) -> String {
    let stripped = strip_history_aliases(content);
    let Some((s, e)) = marked_block_span(&stripped) else { return stripped };
    let Some(ids) = history_alias_ids(&stripped, refs) else { return stripped };
    if ids.is_empty() {
        return stripped;
    }
    let block = &stripped[s..e];
    let Some(tables) = recodex_provider_tables(block) else { return stripped };
    let Some(end_line) = end_marker_line_start(block) else { return stripped };
    let next = format!(
        "{}{}{}{}{}",
        &stripped[..s],
        &block[..end_line],
        render_history_aliases(&ids, &tables),
        &block[end_line..],
        &stripped[e..]
    );
    if validate_toml(&next).is_err() {
        return stripped;
    }
    next
}

/// 托管块里 [model_providers.recodex] 及其子表的正文：(子表后缀, 行)，主表后缀为空。
/// 整行注释不取 —— 块的结束标记紧贴在 recodex 表后面，抄进别名会留下多余的结束标记。
fn recodex_provider_tables(block: &str) -> Option<Vec<(String, Vec<String>)>> {
    let mut tables: Vec<(String, Vec<String>)> = Vec::new();
    let mut cur: Option<usize> = None;
    for raw in block.split('\n') {
        let t = raw.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if t.starts_with('[') {
            cur = None;
            if t == "[model_providers.recodex]" {
                tables.push((String::new(), Vec::new()));
                cur = Some(tables.len() - 1);
            } else if let Some(suffix) = t
                .strip_prefix("[model_providers.recodex.")
                .and_then(|r| r.strip_suffix(']'))
            {
                tables.push((suffix.to_string(), Vec::new()));
                cur = Some(tables.len() - 1);
            }
            continue;
        }
        if let Some(i) = cur {
            tables[i].1.push(raw.trim_end_matches('\r').to_string());
        }
    }
    match tables.first() {
        Some((suffix, lines)) if suffix.is_empty() && !lines.is_empty() => Some(tables),
        _ => None,
    }
}

fn history_alias_key(id: &str) -> String {
    if id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
        id.to_string()
    } else {
        format!("\"{id}\"") // history_alias_id_ok 已排除引号与反斜杠，无需转义
    }
}

/// 插在块结束标记之前的别名段（以空行开头，以换行结尾）。格式与 Go 侧逐字节一致。
fn render_history_aliases(ids: &[String], tables: &[(String, Vec<String>)]) -> String {
    let mut b = String::new();
    b.push('\n');
    b.push_str(HISTORY_ALIAS_MARKER);
    b.push('\n');
    for (i, id) in ids.iter().enumerate() {
        if i > 0 {
            b.push('\n');
        }
        let key = history_alias_key(id);
        for (j, (suffix, lines)) in tables.iter().enumerate() {
            if j > 0 {
                b.push('\n');
            }
            b.push_str("[model_providers.");
            b.push_str(&key);
            if !suffix.is_empty() {
                b.push('.');
                b.push_str(suffix);
            }
            b.push_str("]\n");
            for l in lines {
                b.push_str(l);
                b.push('\n');
            }
        }
    }
    b
}

fn end_marker_line_start(block: &str) -> Option<usize> {
    let i = block.rfind(END_MARKER)?;
    Some(block[..i].rfind('\n').map(|p| p + 1).unwrap_or(0))
}

/// 去掉托管块里的别名段，是 with_history_aliases 插入的精确逆操作。
pub fn strip_history_aliases(content: &str) -> String {
    let Some((s, e)) = marked_block_span(content) else { return content.to_string() };
    let block = &content[s..e];
    let needle = format!("\n{HISTORY_ALIAS_MARKER}\n");
    let Some(m) = block.find(&needle) else { return content.to_string() };
    let Some(end_line) = end_marker_line_start(block) else { return content.to_string() };
    if end_line <= m {
        return content.to_string();
    }
    // 正常情况下 m 指向别名段开头那个空行；有人手删了那个空行时补回换行，
    // 免得块正文最后一行和结束标记粘成一行。
    let mut head = block[..m].to_string();
    if !head.ends_with('\n') {
        head.push('\n');
    }
    format!("{}{}{}{}", &content[..s], head, &block[end_line..], &content[e..])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// plist 必须是合法 XML。key 里出现 `&` 或 `<` 而不转义,launchd 会**静默**
    /// 跳过这个 agent —— 用户看到的是「重启之后又 401 了」,查无可查。
    #[test]
    fn launch_agent_plist_escapes_xml_specials() {
        let plist = macos_launch_agent_plist(SUB2API_ENV_KEY, "sk-a&b<c>\"d\"");

        assert!(
            !plist.contains("sk-a&b<c>"),
            "值没转义就塞进 XML 了:\n{plist}"
        );
        assert!(plist.contains("sk-a&amp;b&lt;c&gt;&quot;d&quot;"), "{plist}");
        assert!(plist.contains(&format!(
            "<string>{}</string>",
            macos_launch_agent_label(SUB2API_ENV_KEY)
        )));
        // 少了 RunAtLoad,agent 登录时不会跑,重启后 key 就没了 —— 正是要修的病。
        assert!(plist.contains("<key>RunAtLoad</key>"), "{plist}");
        assert!(plist.contains("<true/>"), "{plist}");
        // 参数顺序错了 launchctl 会静默不设值。
        assert!(
            plist.contains("<string>/bin/launchctl</string>")
                && plist.contains("<string>setenv</string>")
                && plist.contains(&format!("<string>{SUB2API_ENV_KEY}</string>")),
            "{plist}"
        );
    }

    /// 落点必须和 Go 侧(internal/clientcfg/envvar_macos.go)逐段一致 ——
    /// 两个实现写的是**同一个文件**,路径漂了就变成两个 agent:
    /// 一个设 key 一个不设,登录时谁后跑谁说了算,表现是「有时候能用有时候 401」。
    #[test]
    fn launch_agent_path_lands_in_the_user_launchagents_dir() {
        let home = std::path::Path::new("/Users/tester");
        let path = macos_launch_agent_path_in(home, SUB2API_ENV_KEY);

        assert!(path.starts_with(home));
        assert_eq!(
            path.parent().and_then(|p| p.file_name()),
            Some(std::ffi::OsStr::new("LaunchAgents"))
        );
        assert_eq!(
            path.parent()
                .and_then(|p| p.parent())
                .and_then(|p| p.file_name()),
            Some(std::ffi::OsStr::new("Library"))
        );
        assert_eq!(
            path.file_name(),
            Some(std::ffi::OsStr::new(&format!(
                "ai.recodex.env.{SUB2API_ENV_KEY}.plist"
            )[..]))
        );
    }

    #[test]
    fn launch_agent_label_is_namespaced_per_variable() {
        assert_eq!(
            macos_launch_agent_label(SUB2API_ENV_KEY),
            format!("ai.recodex.env.{SUB2API_ENV_KEY}")
        );
    }

    #[test]
    fn managed_base_url_reads_the_block_and_only_the_block() {
        let block = render_sub2api_block("https://sg.gw.recodex.dev/backend-api/codex", false);
        let content = install_block(
            "base_url = \"https://user.example/v1\"\n[other]\nbase_url = \"https://other.example\"\n",
            &block,
        );
        assert_eq!(
            managed_base_url(&content).as_deref(),
            Some("https://sg.gw.recodex.dev/backend-api/codex")
        );
        assert_eq!(managed_base_url("base_url = \"https://user.example/v1\"\n"), None);
    }

    /// 只有 RECODEX_KEY 真的变了才算「需要重启」:重复登录同一把 key、
    /// 或者改的是别的变量,都不该让自诊断喊重启。
    #[test]
    fn key_change_flag_only_trips_on_a_real_change() {
        KEY_CHANGED_SINCE_START.store(false, Ordering::SeqCst);
        note_key_change("OTHER_VAR", None, Some("x"));
        note_key_change(SUB2API_ENV_KEY, Some("same"), Some("same"));
        assert!(!key_changed_since_start(), "没变的不该置位");
        note_key_change(SUB2API_ENV_KEY, Some("old"), Some("new"));
        assert!(key_changed_since_start());
        KEY_CHANGED_SINCE_START.store(false, Ordering::SeqCst);
        note_key_change(SUB2API_ENV_KEY, Some("old"), None);
        assert!(key_changed_since_start(), "登出清掉也算变了");
        KEY_CHANGED_SINCE_START.store(false, Ordering::SeqCst);
    }

    // 下面这组守的是同一条命脉:**托管块落盘后 Codex 到底拿不拿得到钥匙**。
    // 与 Go 侧 internal/clientcfg/inline_key_test.go 一一对应 —— 两个实现写的是
    // 同一个 config.toml,行为不一致的话用户在 CLI 和桌面端会看到两种结果。

    #[test]
    fn inline_replaces_the_env_key_line_and_touches_nothing_else() {
        let block = render_sub2api_block("https://gw.example.dev/backend-api/codex", true);
        let got = inline_managed_key(&block, "sk-live-abc123").expect("正常块应该能内联");
        assert!(
            !got.contains("env_key"),
            "内联后不能再留 env_key —— 留着会让环境变量变成硬性要求\n{got}"
        );
        assert!(got.contains("experimental_bearer_token = \"sk-live-abc123\""));
        // 换一行不能顺手动别的行:这些都是有人踩过坑才加上的。
        assert!(
            got.contains("http_headers = { \"x-openai-actor-authorization\" = \"recodex\" }"),
            "actor 头丢了 —— image_gen / web_search 会静默消失\n{got}"
        );
        assert!(
            got.contains("supports_websockets = true"),
            "supports_websockets 被冲掉了 —— 用户被静默打回 HTTP\n{got}"
        );
        assert!(got.contains("base_url = \"https://gw.example.dev/backend-api/codex\""));
        assert!(
            !got.contains("requires_openai_auth"),
            "不能引入 requires_openai_auth —— 实测它会让 image_gen 消失\n{got}"
        );
    }

    /// 拿不到钥匙、或钥匙不能安全地拼进 TOML 时,必须原样退回 env_key 形态。
    /// 写出一行空的或被撑破的 bearer 比不换更糟:TOML 一坏,整份 config 都读不了。
    #[test]
    fn inline_refuses_keys_that_cannot_be_pasted_into_toml() {
        let block = render_sub2api_block("https://gw.example.dev/backend-api/codex", false);
        for bad in [
            "",
            "   ",
            "sk-with\"quote",
            "sk-with\nnewline",
            "sk-with\\backslash",
            "sk-with space",
            "sk-with#hash",
            "sk-with\0nul",
            "sk-带中文",
            &format!("sk-{}", "a".repeat(600)),
        ] {
            assert!(
                inline_managed_key(&block, bad).is_none(),
                "钥匙 {bad:?} 不该被接受"
            );
        }
    }

    /// 幂等:切网关 / 重新登录会反复重写托管块,第二次不能再包一层。
    #[test]
    fn inline_is_idempotent() {
        let block = render_sub2api_block("https://gw.example.dev/backend-api/codex", false);
        let once = inline_managed_key(&block, "sk-live-abc123").expect("第一次应该能内联");
        assert!(
            inline_managed_key(&once, "sk-live-abc123").is_none(),
            "已经是 bearer 形态时不该再报变更"
        );
        assert_eq!(once.matches("experimental_bearer_token").count(), 1);
        assert!(!managed_key_is_inlined(&block));
        assert!(managed_key_is_inlined(&once));
    }

    /// 服务端下发的块不一定长成我们模板的样子(运维配的 RawBlock)。
    /// 没有 env_key 行时不猜、不硬塞。
    #[test]
    fn inline_skips_blocks_without_an_env_key_line() {
        let raw = "model_provider = \"custom\"\n\n[model_providers.custom]\nbase_url = \"https://x.dev\"\n";
        assert!(inline_managed_key(raw, "sk-live-abc123").is_none());
    }

    /// 同一个块里有第二个 provider 时,一行都不许换。
    ///
    /// 这里防的是**把我们的网关 Key 发给第三方**:下面那个 `[model_providers.vendor]`
    /// 的 base_url 指向别人的服务器,它那行 env_key 要是也被换成
    /// `experimental_bearer_token = "<我们的key>"`,凭据就跟着请求出去了。
    /// Go 侧原来正是 ReplaceAllString(全换),这边原来只换第一行 —— 两个都不对,
    /// 而且不一致。现在两边都是「拿不准就不换」。
    #[test]
    fn inline_refuses_blocks_with_more_than_one_env_key_line() {
        let two = concat!(
            "[model_providers.recodex]\n",
            "base_url = \"https://gw.recodex.dev/backend-api/codex\"\n",
            "env_key = \"RECODEX_KEY\"\n",
            "\n",
            "[model_providers.vendor]\n",
            "base_url = \"https://vendor.example.com/v1\"\n",
            "env_key = \"VENDOR_KEY\"\n",
        );
        assert!(
            inline_managed_key(two, "sk-live-abc123").is_none(),
            "两行 env_key 时必须整个放弃内联"
        );
        // 单行的正常路径不能被这条新规则误伤。
        let one = two.split("\n[model_providers.vendor]").next().unwrap();
        assert!(inline_managed_key(one, "sk-live-abc123").is_some());
    }

    /// 内联后的块要经过 install_block 这条生产写入路径,再被那些扫 config.toml 的
    /// 函数扫一遍 —— 线上写进磁盘的是**内联后**的形状,扫描器必须在这个形状上也活着。
    #[test]
    fn scanners_survive_the_inlined_block() {
        let block = render_sub2api_block("https://api.recodex.dev/backend-api/codex", false);
        let inlined = inline_managed_key(&block, "sk-live-abc123").expect("内联失败");
        let content = install_block("model = \"grok-4.5\"\n", &inlined);

        assert_eq!(
            managed_base_url(&content).as_deref(),
            Some("https://api.recodex.dev/backend-api/codex")
        );
        assert!(!managed_supports_websockets(&inlined));
        assert!(has_managed_block(&content));
        // CRLF 的块不能被顺手改成 LF:Windows 上用户的 config.toml 就是 CRLF。
        let crlf = block.replace('\n', "\r\n");
        let inlined_crlf = inline_managed_key(&crlf, "sk-live-abc123").expect("CRLF 块也要能内联");
        assert!(!inlined_crlf.contains("\n\n"), "行尾被改写了\n{inlined_crlf:?}");
        assert_eq!(inlined_crlf.matches("\r\n").count(), crlf.matches("\r\n").count());
    }

    #[test]
    fn render_fills_base_url_and_env_key() {
        let block = render_sub2api_block("https://sg.gw.recodex.dev/backend-api/codex", false);
        assert!(block.contains("base_url = \"https://sg.gw.recodex.dev/backend-api/codex\""));
        assert!(block.contains("env_key = \"RECODEX_KEY\""));
        assert!(block.contains("model_provider = \"recodex\""));
    }

    // supports_websockets 必须两种取值都渲染出来，且恰好一次。
    // 缺省与显式 false 在 Codex 的 `#[serde(default)] bool` 下语义相同，
    // 但"这一行总是存在"才让读回逻辑无歧义。
    #[test]
    fn render_emits_supports_websockets_both_ways() {
        let on = render_sub2api_block("https://gw/backend-api/codex", true);
        let off = render_sub2api_block("https://gw/backend-api/codex", false);
        assert!(on.contains("supports_websockets = true"), "开启时应渲染 true");
        assert!(
            off.contains("supports_websockets = false"),
            "关闭时应渲染 false"
        );
        assert_eq!(on.matches("supports_websockets").count(), 1);
        assert_eq!(off.matches("supports_websockets").count(), 1);
    }

    // 读回的入参是**已经取出的块正文**（不带标记行）—— 官方模式快照
    // OfficialModeSnapshot.config_body 存的就是这个形状，要求带标记会让它
    // 永远返回 false，把 WS 开关静默丢掉。
    #[test]
    fn managed_supports_websockets_reads_body_without_markers() {
        assert!(managed_supports_websockets(&render_sub2api_block(
            "https://gw/backend-api/codex",
            true
        )));
        assert!(!managed_supports_websockets(&render_sub2api_block(
            "https://gw/backend-api/codex",
            false
        )));
        // 老版本写的块里没有这一行 → false，与 Codex 的 serde 默认值一致。
        assert!(!managed_supports_websockets(
            "base_url = \"https://gw\"\nwire_api = \"responses\""
        ));
        assert!(!managed_supports_websockets(""));
        // 空白容忍度要与 Go 侧一致。
        assert!(managed_supports_websockets("  supports_websockets  =  true  "));
        // 认不出的值保守当关闭，不凭空替用户打开 WS。
        assert!(!managed_supports_websockets("supports_websockets = yes"));
    }

    // 🔴 与 Go 侧 clientcfg.ManagedSupportsWebsockets 必须同判。
    // 两边写的是**同一份 config.toml**，只有一侧对等于没对。
    #[test]
    fn websockets_flag_survives_local_rerender() {
        let base = "model_provider = \"other\"\n";
        let installed = install_block(base, &render_sub2api_block("https://old/backend-api/codex", true));
        let (start, end) = marked_block_span(&installed).expect("装完应当有托管块");
        let body = &installed[start..end];
        assert!(managed_supports_websockets(body));

        // 切网关：只换 base_url，开关从旧块读回。
        let rerendered = install_block(
            &installed,
            &render_sub2api_block("https://new/backend-api/codex", managed_supports_websockets(body)),
        );
        let (s2, e2) = marked_block_span(&rerendered).expect("重渲染后仍应有托管块");
        assert!(
            managed_supports_websockets(&rerendered[s2..e2]),
            "切网关之后 supports_websockets 被冲掉了"
        );
        assert!(rerendered.contains("https://new/backend-api/codex"));
        assert!(!rerendered.contains("https://old/"));
    }

    #[test]
    fn install_preserves_existing_config_and_puts_model_provider_before_tables() {
        let base = "model = \"x\"\n[mcp_servers.foo]\ncmd = \"bar\"\n";
        let with = install_block(base, &render_sub2api_block("https://gw/backend-api/codex", false));
        assert!(has_managed_block(&with));
        // The user's top-level key stays first and their table survives intact.
        assert!(with.starts_with("model = \"x\"\n"));
        assert!(with.contains("[mcp_servers.foo]\ncmd = \"bar\""));
        // The crux: model_provider must land above the first table, else TOML
        // parses it as a key of that table and it is silently lost.
        let mp = with
            .find("model_provider = \"recodex\"")
            .expect("model_provider present");
        let table = with.find("[mcp_servers.foo]").expect("user table present");
        assert!(mp < table, "model_provider must precede the first table");
    }

    #[test]
    fn install_before_table_then_remove_clears_recodex_keeps_user() {
        let base = "model = \"x\"\n[t]\nk = 1\n";
        let with = install_block(base, &render_sub2api_block("https://gw/backend-api/codex", false));
        assert!(with.find("model_provider").unwrap() < with.find("[t]").unwrap());
        let back = remove_block(&with);
        assert!(!back.contains("recodex"));
        assert!(back.contains("model = \"x\""));
        assert!(back.contains("[t]\nk = 1"));
    }

    #[test]
    fn install_is_idempotent_even_after_markers_are_lost() {
        // Simulate Codex++ re-serialising away our comment markers: only the bare
        // table + a stray top-level model_provider survive.
        let mangled = "model = \"x\"\nmodel_provider = \"recodex\"\n[t]\nk = 1\n[model_providers.recodex]\nbase_url = \"https://old/backend-api/codex\"\n";
        let with = install_block(
            mangled,
            &render_sub2api_block("https://new/backend-api/codex", false),
        );
        assert_eq!(with.matches("[model_providers.recodex]").count(), 1);
        assert_eq!(with.matches("model_provider = \"recodex\"").count(), 1);
        assert!(with.contains("https://new/backend-api/codex"));
        assert!(!with.contains("https://old/backend-api/codex"));
        assert!(with.find("model_provider = \"recodex\"").unwrap() < with.find("[t]").unwrap());
    }

    #[test]
    fn install_then_remove_roundtrips_for_newline_terminated_config() {
        let base = "model = \"x\"\n";
        let with = install_block(base, &render_sub2api_block("https://gw/backend-api/codex", false));
        assert_eq!(remove_block(&with), base);
    }

    #[test]
    fn second_install_replaces_rather_than_duplicates() {
        let base = "model = \"x\"\n";
        let first = install_block(base, &render_sub2api_block("https://a/backend-api/codex", false));
        let second = install_block(&first, &render_sub2api_block("https://b/backend-api/codex", false));
        assert_eq!(second.matches(START_MARKER).count(), 1);
        assert!(second.contains("https://b/backend-api/codex"));
        assert!(!second.contains("https://a/backend-api/codex"));
    }

    #[test]
    fn remove_on_unmanaged_config_is_noop() {
        let base = "model = \"x\"\nother = 1\n";
        assert_eq!(remove_block(base), base);
    }

    #[test]
    fn login_poll_captures_server_config_fields() {
        let json = r#"{"status":"approved","token":"rct_x","gateway_url":"https://g","config":"MANAGED_BLOCK","auth_json":"{\"k\":1}","env_key":"RECODEX_KEY","env_value":"sk-secret"}"#;
        let poll: crate::LoginPoll = serde_json::from_str(json).expect("deserialize");
        assert_eq!(poll.config, "MANAGED_BLOCK");
        assert_eq!(poll.auth_json, "{\"k\":1}");
        assert_eq!(poll.env_key, "RECODEX_KEY");
        assert_eq!(poll.env_value, "sk-secret");
    }

    // ---- 顶层 model 托管 -------------------------------------------------
    // 契约:只碰带标记的行;用户自己的 model 一个字不动;绝不产生重复键;
    // 绝不碰 [profiles.x] 里的同名键。

    #[test]
    fn managed_model_inserts_when_absent() {
        let out = set_managed_model("model_provider = \"recodex\"
", "gpt-6-astra");
        assert!(out.starts_with("model = \"gpt-6-astra\" # recodex-managed-model
"));
        assert!(out.contains("model_provider = \"recodex\""));
        assert_eq!(managed_model(&out).as_deref(), Some("gpt-6-astra"));
    }

    #[test]
    fn managed_model_updates_its_own_line() {
        let base = set_managed_model("", "gpt-5.6-sol");
        let out = set_managed_model(&base, "gpt-6-astra");
        assert_eq!(managed_model(&out).as_deref(), Some("gpt-6-astra"));
        // 只能有一行 model,重复顶层键会让整份 config.toml 解析失败。
        assert_eq!(out.matches("model = ").count(), 1);
    }

    #[test]
    fn managed_model_never_touches_a_user_owned_model() {
        let base = "model = \"gpt-5.6-terra\"
model_provider = \"recodex\"
";
        assert_eq!(set_managed_model(base, "gpt-6-astra"), base);
    }

    #[test]
    fn managed_model_is_idempotent() {
        let once = set_managed_model("", "gpt-6-astra");
        assert_eq!(set_managed_model(&once, "gpt-6-astra"), once);
    }

    #[test]
    fn managed_model_ignores_keys_inside_tables() {
        // [profiles.work] 里的 model 属于那张表,不是顶层键 —— 碰它就是改用户的 profile。
        let base = "model_provider = \"recodex\"

[profiles.work]
model = \"gpt-5.5\"
";
        let out = set_managed_model(base, "gpt-6-astra");
        assert!(out.contains("[profiles.work]
model = \"gpt-5.5\""));
        assert!(out.starts_with("model = \"gpt-6-astra\" # recodex-managed-model
"));
        assert_eq!(out.matches("model = ").count(), 2); // 顶层一行 + profile 里那行
    }

    #[test]
    fn managed_model_does_not_confuse_similar_keys() {
        // model_provider / model_providers / model_reasoning_effort 都不是 model。
        let base = "model_provider = \"recodex\"
model_reasoning_effort = \"high\"
";
        let out = set_managed_model(base, "gpt-6-astra");
        assert!(out.contains("model_reasoning_effort = \"high\""));
        assert_eq!(managed_model(&out).as_deref(), Some("gpt-6-astra"));
    }

    /// 模型名来自**网络**(上游 manifest 的 slug),而写入是纯字符串拼接。
    /// 不校验的话,一个带双引号和换行的 slug 就能往用户的 config.toml 里注入
    /// 任意 TOML —— 比如凭空加一个 provider,把所有对话导去别处。
    /// 网关地址同样来自服务端,同样是纯字符串拼进托管块。
    ///
    /// 顺序也要对:校验必须排在 `stage_config_for_return` 之前,不然被注入的块
    /// 已经进了官方模式快照,切回 ReCodex 时照样生效。
    #[test]
    fn gateway_url_refuses_anything_that_could_break_out_of_the_block() {
        let quote = '"';

        for good in [
            "https://sg.gw.recodex.dev/backend-api/codex",
            "http://127.0.0.1:8080/backend-api/codex",
        ] {
            assert!(base_url_is_safe(good), "{good} 被误挡了");
        }

        for bad in [
            &format!("https://ok.dev{quote}
[model_providers.evil]
base_url = {quote}https://evil.dev"),
            "https://ok.dev
[x]",
            &format!("https://ok.dev{quote}"),
            "https://ok.dev # 注释",
            "https://ok.dev\\x",
            "ftp://ok.dev",
            "javascript:alert(1)",
            "",
            "   ",
            // `?` 不是注入,是静默失效:这个地址后面还要被拼上 /backend-api/codex,
            // 拼完成了 https://ok.dev/?x=1/backend-api/codex —— 路径整段被吃进查询串,
            // Codex 打向网关根路径且不报错。Go 侧 BaseURLIsSafe 同步拒。
            "https://ok.dev/?x=1",
            "https://ok.dev?",
        ] {
            assert!(!base_url_is_safe(bad), "{bad:?} 不该被接受");
        }
        assert!(!base_url_is_safe(&format!("https://{}", "a".repeat(600))));
    }

    #[test]
    fn managed_model_refuses_names_that_could_inject_toml() {
        let base = "model = \"old\" # recodex-managed-model
";
        let quote = '"';

        // 关掉引号再另起一段表 —— 最直接的注入。
        let injection = format!("gpt-5{quote}
[model_providers.evil]
base_url = {quote}https://evil.example{quote}
name = {quote}x");
        assert_eq!(set_managed_model(base, &injection), base, "注入串被写进去了");

        // 单独的换行、引号、`#`、方括号、空格,一个都不能放行。
        for bad in [
            "gpt-5
evil = 1",
            &format!("gpt{quote}5"),
            "gpt-5 # 注释",
            "[table]",
            "gpt 5",
            "gpt	5",
            &"g".repeat(129),
            "",
        ] {
            assert_eq!(set_managed_model(base, bad), base, "{bad:?} 不该被接受");
        }

        // 真实模型名必须照常工作 —— 校验不能误伤。
        for good in ["gpt-5.6-sol", "gpt-6-astra", "codex_auto_review", "o3"] {
            assert!(
                set_managed_model(base, good).contains(&format!("model = {quote}{good}{quote}")),
                "{good} 被误挡了"
            );
        }
    }

    #[test]
    fn managed_model_ignores_empty_recommendation() {
        // 拿不到推荐值时保持现状,绝不能把用户的 model 写没了。
        let base = "model = \"gpt-5.6-sol\"
";
        assert_eq!(set_managed_model(base, ""), base);
        assert_eq!(set_managed_model(base, "   "), base);
    }

    #[test]
    fn model_is_managed_gates_the_network_call() {
        assert!(model_is_managed(""));                                  // 还没有这个键
        assert!(model_is_managed(&set_managed_model("", "gpt-5.6-sol"))); // 我们写的
        assert!(!model_is_managed("model = \"gpt-5.6-terra\"
"));      // 用户自己写的
        // [profiles.x] 里的 model 不是顶层键,不该让我们误判成「用户接管了」。
        assert!(model_is_managed("model_provider = \"recodex\"
[profiles.work]
model = \"gpt-5.5\"
"));
    }

    #[test]
    fn managed_model_survives_a_commented_out_model_line() {
        // 注释掉的 model 不算数,应当照常插入我们的托管行。
        let base = "# model = \"gpt-5.5\"
";
        let out = set_managed_model(base, "gpt-6-astra");
        assert!(out.starts_with("model = \"gpt-6-astra\" # recodex-managed-model
"));
        assert!(out.contains("# model = \"gpt-5.5\""));
    }

    /// 2026-09-08 工单:客户端"用不了",真因是整份 config.toml 解析不了 ——
    /// 官方 codex CLI 实测 `Error loading configuration: config.toml:16:26: duplicate key`,
    /// 硬失败不回落,所以读这份文件的一切都起不来。
    ///
    /// 托管块里 http_headers 是内联表,块外还留着 [model_providers.recodex.http_headers]
    /// 段头。TOML 不许用段头扩展内联表 → 重复键。那个孤儿不是我们写的,
    /// 但清残留的精确匹配认不出子表,于是**重装、重新登录都修不好**。
    #[test]
    fn install_clears_orphan_provider_sub_table() {
        let cfg = concat!(
            "model = \"gpt-6-astra\"

",
            ">>>BLOCK<<<

",
            "[model_providers.recodex.http_headers]
",
            "x-openai-actor-authorization = \"recodex\"

",
            "[desktop]
",
            "followUpQueueMode = \"steer\"
",
        );
        let block = render_sub2api_block("https://api.recodex.dev/backend-api/codex", false);
        let cfg = cfg.replace(">>>BLOCK<<<", &format!("{START_MARKER}
{block}
{END_MARKER}"));

        let got = install_block(&cfg, &block);

        assert!(
            !got.contains("[model_providers.recodex."),
            "孤儿子表还在,和块里的内联 http_headers 撞车,整份文件解析不了:
{got}"
        );
        assert_eq!(
            got.matches("x-openai-actor-authorization").count(),
            1,
            "该只剩块里内联的那一处:
{got}"
        );
        for keep in ["model = \"gpt-6-astra\"", "[desktop]", "followUpQueueMode = \"steer\""] {
            assert!(got.contains(keep), "把用户自己的配置删了,丢了 {keep}:
{got}");
        }
    }

    /// 带引号的表名是用户一个**真叫** recodex.foo 的 provider,不是我们的子表。
    /// 前缀匹配写松了就会把它连表内所有键一起删掉 —— 那是毁用户配置,比不清更糟。
    #[test]
    fn install_keeps_user_provider_named_like_our_sub_table() {
        let cfg = "model_provider = \"recodex.foo\"

[model_providers.'recodex.foo']
name = \"User Own\"
";
        let block = render_sub2api_block("https://api.recodex.dev/backend-api/codex", false);
        let got = install_block(cfg, &block);
        assert!(
            got.contains("[model_providers.'recodex.foo']") && got.contains("name = \"User Own\""),
            "把用户自己叫 recodex.foo 的 provider 删掉了:
{got}"
        );
    }


    /// 每次调用给一个新的空目录。照仓库现有写法(temp_dir + pid),不引测试依赖。
    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rcx-cfgvalidate-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 备份**只存一次**。
    ///
    /// 存第二次会把「用户原本的样子」覆盖成「已经被我们改过的样子」,
    /// 想还原的时候就还原了个寂寞。换网关、换 key 都会再走一次写入路径,
    /// 所以这条不是理论情况。
    #[test]
    fn apply_validated_backs_up_only_once() {
        let dir = tempdir();
        let path = dir.join("config.toml");
        let original = "model = \"gpt-5.6-codex\"
";
        std::fs::write(&path, original).unwrap();

        let first = render_sub2api_block("https://api.recodex.dev/backend-api/codex", false);
        let next = install_block(original, &first);
        apply_validated(&path, original, &next, &first).unwrap();

        // 第二次写入时,cur 已经是「被我们改过的样子」了。
        let cur2 = std::fs::read_to_string(&path).unwrap();
        let second = render_sub2api_block("https://hk.recodex.dev/backend-api/codex", false);
        let next2 = install_block(&cur2, &second);
        apply_validated(&path, &cur2, &next2, &second).unwrap();

        let backup = std::fs::read_to_string(suffixed(&path, CONFIG_BACKUP_SUFFIX)).unwrap();
        assert_eq!(backup, original, "备份被第二次写入覆盖了,还原点丢失");
    }

    /// 分支一:正常情况 —— 写进去,用户自己的东西一个都不能少。
    /// 三分支最容易犯的错是过度保守,把本来该写的也拒掉 ——
    /// 那是把偶发故障换成必然故障。
    #[test]
    fn apply_validated_keeps_user_content() {
        let dir = tempdir();
        let path = dir.join("config.toml");
        let cur = "model = \"gpt-5.6-codex\"

[mcp_servers.mine]
command = \"echo\"
";
        std::fs::write(&path, cur).unwrap();
        let block = render_sub2api_block("https://api.recodex.dev/backend-api/codex", false);
        let next = install_block(cur, &block);

        apply_validated(&path, cur, &next, &block).unwrap();

        let got = std::fs::read_to_string(&path).unwrap();
        assert!(validate_toml(&got).is_ok(), "写出去的解析不了:
{got}");
        assert!(got.contains("[mcp_servers.mine]"), "用户配置丢了:
{got}");
        assert!(got.contains("model_provider = \"recodex\""), "我们的块没进去:
{got}");
        assert!(
            std::fs::read_to_string(suffixed(&path, CONFIG_BACKUP_SUFFIX)).unwrap() == cur,
            "备份不是用户原样"
        );
    }

    /// 分支二:进来是好的、出去会变坏 —— 必须拒绝,保住用户手上那份。
    ///
    /// 官方 codex 遇到解析失败是**硬失败不回落**(实测
    /// `Error loading configuration: config.toml:16:26: duplicate key`),
    /// 留下坏文件 = 用户所有配置一起消失,且没有任何指向我们的线索。
    #[test]
    fn apply_validated_refuses_to_break_a_good_config() {
        let dir = tempdir();
        let path = dir.join("config.toml");
        let cur = "model = \"gpt-5.6-codex\"
";
        std::fs::write(&path, cur).unwrap();

        let bad = "model_provider = \"recodex\"
this is not toml =
";
        let err = apply_validated(&path, cur, bad, bad).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::InvalidData, "应当拒绝写入");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            cur,
            "用户那份好文件被动过了"
        );
    }

    /// 分支三:用户那份本来就解析不了 —— 重建,并留证。
    /// 只在这个前提下才做「完全新建」。
    #[test]
    fn apply_validated_rebuilds_when_current_is_already_broken() {
        let dir = tempdir();
        let path = dir.join("config.toml");
        // 必须是我们修不好的坏法:孤儿子表那种清残留自己会治好,走不到这条分支。
        let cur = "model = \"gpt-6-astra\"

[mcp_servers.mine]
command = \"echo\"
this line is not valid toml =
";
        assert!(validate_toml(cur).is_err(), "样本前提不成立");
        std::fs::write(&path, cur).unwrap();
        let block = render_sub2api_block("https://api.recodex.dev/backend-api/codex", false);
        let next = install_block(cur, &block);

        apply_validated(&path, cur, &next, &block).unwrap();

        let got = std::fs::read_to_string(&path).unwrap();
        assert!(validate_toml(&got).is_ok(), "重建出来的还是坏的:
{got}");
        assert!(got.contains("model_provider = \"recodex\""));
        assert!(
            suffixed(&path, CONFIG_BROKEN_SUFFIX).exists(),
            "没保留坏掉的那份,用户丢了东西我们连查都没法查"
        );
        assert!(suffixed(&path, CONFIG_BACKUP_SUFFIX).exists(), "没留原样备份");
    }


    /// 用户原本**没有** config.toml 时,备份必须是一个空文件当哨兵。
    /// 不写哨兵:第二次写入时 cur 已是我们的内容,会被当成"用户原样"存进去。审计抓出来的。
    #[test]
    fn apply_validated_backup_is_empty_sentinel_when_user_had_nothing() {
        let dir = tempdir();
        let path = dir.join("config.toml");
        assert!(!path.exists());

        let first = render_sub2api_block("https://api.recodex.dev/backend-api/codex", false);
        apply_validated(&path, "", &install_block("", &first), &first).unwrap();
        let cur2 = std::fs::read_to_string(&path).unwrap();
        let second = render_sub2api_block("https://hk.recodex.dev/backend-api/codex", false);
        apply_validated(&path, &cur2, &install_block(&cur2, &second), &second).unwrap();

        let backup = std::fs::read(suffixed(&path, CONFIG_BACKUP_SUFFIX)).unwrap();
        assert!(
            backup.is_empty(),
            "用户原本什么都没有,备份却有内容 —— 我们自己的旧块被当成了用户原样"
        );
    }

    /// 共用闸门本身的三种情形。
    #[test]
    fn refuse_if_would_break_only_blocks_good_to_bad() {
        let good = "model = \"x\"\n";
        let bad = "model = \n";
        assert!(refuse_if_would_break(good, good).is_ok(), "好→好 该放行");
        assert!(refuse_if_would_break(bad, bad).is_ok(), "坏→坏 不归它管(交给重建分支)");
        assert!(refuse_if_would_break(bad, good).is_ok(), "坏→好 是修复,该放行");
        assert_eq!(
            refuse_if_would_break(good, bad).unwrap_err().kind(),
            ErrorKind::InvalidData,
            "好→坏 必须拒绝"
        );
    }

    /// **每一个** config.toml 写入方落盘前都要过闸。
    ///
    /// 审计时 apply_managed_model / demote_managed_provider 都是直接 write_atomic,
    /// 而前者每次启动都可能跑 —— 安全网对它们不成立。用文本守卫钉住:
    /// 这几个函数体里,write_atomic 之前必须出现 refuse_if_would_break。
    #[test]
    fn every_config_writer_passes_the_gate() {
        let src = include_str!("codexcfg.rs");
        for name in ["fn apply_managed_model", "fn demote_managed_provider", "fn apply_validated"] {
            let start = src.find(name).unwrap_or_else(|| panic!("找不到 {name}"));
            let body_end = src[start..].find("\n}\n").map(|i| start + i).unwrap_or(src.len());
            let body = &src[start..body_end];
            // apply_validated 的第一分支是 validate_toml(next) 直接验过再写,
            // 不需要再过 refuse_if_would_break —— 所以认**任一**校验,
            // 只要求它出现在第一处 write_atomic 之前。
            let gate = [body.find("refuse_if_would_break("), body.find("validate_toml(")]
                .into_iter()
                .flatten()
                .min();
            let write = body.find("write_atomic(");
            assert!(
                matches!((gate, write), (Some(g), Some(w)) if g < w),
                "{name} 落盘前既没过 refuse_if_would_break 也没 validate_toml —— 它写出去的坏文件没人拦"
            );
        }
    }

}
