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
// 密钥仍走 env_key,不用 experimental_bearer_token —— 后者要把明文密钥写进
// config.toml,而这个文件用户会截图、会贴进工单。
//
// 改完必须**完全退出 Codex 并新建 task**:工具注册表是启动时建的,热重载看不到。
const SUB2API_TEMPLATE: &str = "model_provider = \"recodex\"\n\n[model_providers.recodex]\nname = \"ReCodex\"\nbase_url = \"{{BASE_URL}}\"\nwire_api = \"responses\"\nenv_key = \"{{ENV_KEY}}\"\nhttp_headers = { \"x-openai-actor-authorization\" = \"recodex\" }";

fn home_dir() -> io::Result<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "no home directory is available"))
}

pub(crate) fn codex_dir() -> io::Result<PathBuf> {
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
pub fn render_sub2api_block(base_url: &str) -> String {
    SUB2API_TEMPLATE
        .replace("{{BASE_URL}}", base_url)
        .replace("{{ENV_KEY}}", SUB2API_ENV_KEY)
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
            in_recodex_table = trimmed == "[model_providers.recodex]";
            if in_recodex_table {
                continue;
            }
            out.push_str(line);
            continue;
        }
        if in_recodex_table {
            continue; // 属于 [model_providers.recodex] 的键
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
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "path has no parent directory"))?;
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".recodex-{}.tmp", std::process::id()));
    fs::write(&tmp, data)?;
    if let Err(err) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
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

/// Splices the given block into `~/.codex/config.toml`, preserving all other
/// content, and writes atomically.
pub fn apply_config(body: &str) -> io::Result<()> {
    let path = config_path()?;
    let cur = read_or_empty(&path)?;
    let next = install_block(&cur, body);
    if next == cur {
        return Ok(());
    }
    write_atomic(&path, next.as_bytes())
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
        write_atomic(&backup, &orig)?;
    }
    // Record ownership before replacing auth.json so logout can recover even if
    // the following write fails or the process exits.
    write_atomic(&with_suffix(&path, AUTH_MANAGED_SUFFIX), b"recodex\n")?;
    write_atomic(&path, data)
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
            write_atomic(&path, &orig)?;
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
#[cfg(windows)]
const ENV_SANDBOX: &str = "RECODEX_ENV_SANDBOX";

#[cfg(windows)]
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
        // Safe here: called once at startup, before any Codex child is spawned.
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
        let path = env_path(name)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, value.as_bytes())?;
        // 先写后改权限会有一瞬间是默认权限;这里内容是密钥,所以写完立刻收紧
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        Ok(())
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
        // tmp 名带 pid:同一台机器上 CLI 和桌面端可能同时走到这里。
        let tmp = path.with_file_name(format!(
            "{}.{}.tmp",
            super::macos_launch_agent_label(name),
            std::process::id()
        ));
        debug_assert_eq!(tmp.parent(), path.parent(), "tmp 必须和目标同目录才能 rename");
        fs::write(&tmp, super::macos_launch_agent_plist(name, value).as_bytes())?;
        if let Err(error) = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))
            .and_then(|()| fs::rename(&tmp, &path))
        {
            let _ = fs::remove_file(&tmp);
            return Err(error);
        }
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
        apply_config(config)?;
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
    apply_config(&render_sub2api_block(codex_base_url))
}

/// Reverts all ReCodex-owned Codex state (config block, auth.json, key env var).
/// Best-effort ordering: a later failure still leaves earlier steps reverted.
pub fn restore_all() -> io::Result<()> {
    restore_config()?;
    restore_auth()?;
    unset_user_env(SUB2API_ENV_KEY)
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
        let block = render_sub2api_block("https://sg.gw.recodex.dev/backend-api/codex");
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

    #[test]
    fn render_fills_base_url_and_env_key() {
        let block = render_sub2api_block("https://sg.gw.recodex.dev/backend-api/codex");
        assert!(block.contains("base_url = \"https://sg.gw.recodex.dev/backend-api/codex\""));
        assert!(block.contains("env_key = \"RECODEX_KEY\""));
        assert!(block.contains("model_provider = \"recodex\""));
    }

    #[test]
    fn install_preserves_existing_config_and_puts_model_provider_before_tables() {
        let base = "model = \"x\"\n[mcp_servers.foo]\ncmd = \"bar\"\n";
        let with = install_block(base, &render_sub2api_block("https://gw/backend-api/codex"));
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
        let with = install_block(base, &render_sub2api_block("https://gw/backend-api/codex"));
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
            &render_sub2api_block("https://new/backend-api/codex"),
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
        let with = install_block(base, &render_sub2api_block("https://gw/backend-api/codex"));
        assert_eq!(remove_block(&with), base);
    }

    #[test]
    fn second_install_replaces_rather_than_duplicates() {
        let base = "model = \"x\"\n";
        let first = install_block(base, &render_sub2api_block("https://a/backend-api/codex"));
        let second = install_block(&first, &render_sub2api_block("https://b/backend-api/codex"));
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
}
