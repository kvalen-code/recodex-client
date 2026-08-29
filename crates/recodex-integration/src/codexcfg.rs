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

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

/// Managed-block markers. These are byte-identical to the CLI's so a config
/// written by either client is recognised and cleanly removed by the other.
pub const START_MARKER: &str = "# >>> recodex managed block, do not edit >>>";
pub const END_MARKER: &str = "# <<< recodex managed block <<<";

/// The environment variable Codex reads the API key from in `env_key` mode. Must
/// match the `env_key` rendered into the managed block.
pub const SUB2API_ENV_KEY: &str = "RECODEX_KEY";

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
}

/// Persists `name=value` to the user environment so a freshly launched Codex can
/// read the key. Also sets it on this process, so the Codex the desktop launcher
/// spawns as a child inherits the key immediately — no app restart needed after
/// the first sign-in. `setx` (Windows only) carries it across restarts.
pub fn set_user_env(name: &str, value: &str) -> io::Result<()> {
    // Safe on edition 2021; this is the login-time write, not a hot path.
    std::env::set_var(name, value);
    #[cfg(windows)]
    {
        return setx(name, value);
    }
    #[cfg(target_os = "macos")]
    {
        // mac 没有 setx/注册表这一层。这个值是 sub2api 的 API key —— 是**密钥**,
        // 所以落钥匙串,而不是往 ~/.zshrc 或明文文件里写。
        return mac_env::save(name, value);
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
    std::env::remove_var(name);
    #[cfg(windows)]
    {
        return setx(name, "");
    }
    #[cfg(target_os = "macos")]
    {
        // 钥匙串能真删,不像 setx 只能置空
        return mac_env::clear(name);
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
}
