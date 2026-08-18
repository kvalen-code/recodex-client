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
const SUB2API_TEMPLATE: &str = "model_provider = \"recodex\"\n\n[model_providers.recodex]\nname = \"ReCodex\"\nbase_url = \"{{BASE_URL}}\"\nwire_api = \"responses\"\nenv_key = \"{{ENV_KEY}}\"";

fn home_dir() -> io::Result<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(ErrorKind::NotFound, "no home directory is available"))
}

fn codex_dir() -> io::Result<PathBuf> {
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

/// Reports whether content already carries our managed block.
pub fn has_managed_block(content: &str) -> bool {
    content.contains(START_MARKER) && content.contains(END_MARKER)
}

fn render_marked_block(body: &str) -> String {
    let body = body.trim_matches('\n');
    format!("{START_MARKER}\n{body}\n{END_MARKER}\n")
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

// Strips every trace of a prior ReCodex managed region, marker-based or not.
// Codex++ re-serialises config.toml and drops our comment markers, so relying on
// them alone would let a stale `[model_providers.recodex]` table survive and get
// duplicated on the next write. This removes: our markers, a top-level
// `model_provider = "recodex"` line, and the whole `[model_providers.recodex]`
// table (header through the line before the next table / EOF).
fn strip_recodex_config(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut in_recodex_table = false;
    for line in content.split_inclusive('\n') {
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
            continue; // a key belonging to [model_providers.recodex]
        }
        if trimmed == START_MARKER
            || trimmed == END_MARKER
            || trimmed == "model_provider = \"recodex\""
        {
            continue;
        }
        out.push_str(line);
    }
    // Collapse any blank-line run we may have opened up into a single newline.
    while out.contains("\n\n\n") {
        out = out.replace("\n\n\n", "\n\n");
    }
    out
}

/// Returns content with a fresh managed block installed. Any previous ReCodex
/// region (marked or bare) is stripped first, so repeated writes never duplicate
/// the provider. The block is inserted just before the first TOML table header —
/// appending at EOF would strand the block's top-level `model_provider` key
/// inside the file's last table, so Codex (and Codex++'s config re-serialiser)
/// would silently drop it. With no table the block is appended.
pub fn install_block(content: &str, body: &str) -> String {
    let block = render_marked_block(body);
    let cleaned = strip_recodex_config(content);
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
    let mut base = cleaned;
    if !base.ends_with('\n') {
        base.push('\n');
    }
    base.push('\n');
    base.push_str(&block);
    base
}

/// Returns content with every ReCodex-managed region removed (marker-based or a
/// bare surviving `[model_providers.recodex]` table).
pub fn remove_block(content: &str) -> String {
    let stripped = strip_recodex_config(content);
    stripped.trim_end_matches('\n').to_string() + if stripped.ends_with('\n') { "\n" } else { "" }
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
pub fn restore_config() -> io::Result<()> {
    let path = config_path()?;
    let cur = read_or_empty(&path)?;
    if !has_managed_block(&cur) {
        return Ok(());
    }
    let next = remove_block(&cur);
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

// Runs `setx NAME VALUE`, persisting to HKCU\Environment. Newly started
// processes pick the change up; the running Codex++ does not, which is why the
// caller surfaces a "restart the desktop app" hint after first login — the same
// contract the CLI documents.
#[cfg(windows)]
fn setx(name: &str, value: &str) -> io::Result<()> {
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
    #[cfg(not(windows))]
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
    #[cfg(not(windows))]
    {
        Ok(())
    }
}

/// Materialises everything the auth `approved` response carries so the launched
/// Codex uses ReCodex: the rendered config block, `auth.json`, and the key env
/// var. Empty fields are skipped. Mirrors the CLI's `writeCredentials`.
pub fn apply_login(config: &str, auth_json: &str, env_key: &str, env_value: &str) -> io::Result<()> {
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
        let mp = with.find("model_provider = \"recodex\"").expect("model_provider present");
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
        let with = install_block(mangled, &render_sub2api_block("https://new/backend-api/codex"));
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
