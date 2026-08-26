use crate::codexcfg::codex_dir;
use std::{
    fs::{self, OpenOptions},
    io::{self, ErrorKind, Write},
    path::{Path, PathBuf},
};

const PREFIX: &str = "desktop-";
const RANDOM_BYTES: usize = 16;

/// Returns the stable, non-secret desktop installation identifier.
///
/// 取值优先级（每一档都有理由，别随手调换）：
///   1. 共用身份文件里的 `device_id` —— 命令行已经注册过就沿用它，同机只占一个名额
///   2. 旧的桌面端私有文件 —— **存量用户不能被踢下线**；顺手把它写进共用文件，
///      这样命令行以后也认这个 id
///   3. 都没有 —— 新生成，直接落在共用文件里
pub fn load_or_create_install_id() -> io::Result<String> {
    if let Ok(shared) = shared_identity_path() {
        if let Some(id) = read_shared_device_id(&shared)? {
            return Ok(id);
        }
        let legacy_path = install_id_path()?;
        if let Some(legacy) = read_existing(&legacy_path)? {
            // 存量桌面端：沿用原 id，并登记进共用文件
            write_shared_device_id(&shared, &legacy)?;
            return Ok(legacy);
        }
        let fresh = new_install_id()?;
        write_shared_device_id(&shared, &fresh)?;
        return Ok(fresh);
    }
    // 连 ~/.codex 都定位不到（无 HOME 的怪环境）：退回旧的私有文件，不要因此登录不了
    load_or_create_install_id_at(&install_id_path()?)
}

fn new_install_id() -> io::Result<String> {
    let mut random = [0_u8; RANDOM_BYTES];
    getrandom::getrandom(&mut random)
        .map_err(|error| io::Error::other(format!("install id randomness failed: {error}")))?;
    Ok(format!("{PREFIX}{}", hex(&random)))
}

pub fn load_or_create_install_id_at(path: &Path) -> io::Result<String> {
    if let Some(value) = read_existing(path)? {
        return Ok(value);
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "install id path has no parent"))?;
    fs::create_dir_all(parent)?;
    let mut random = [0_u8; RANDOM_BYTES];
    getrandom::getrandom(&mut random)
        .map_err(|error| io::Error::other(format!("install id randomness failed: {error}")))?;
    let value = format!("{PREFIX}{}", hex(&random));
    match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(fs::Permissions::from_mode(0o600))?;
            }
            file.write_all(value.as_bytes())?;
            file.sync_all()?;
            Ok(value)
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            read_existing(path)?.ok_or_else(|| {
                io::Error::new(
                    ErrorKind::InvalidData,
                    "install id was created concurrently but is unreadable",
                )
            })
        }
        Err(error) => Err(error),
    }
}

/// 命令行与桌面端**共用**的设备身份文件。
///
/// 两边从前各生成各的随机 id、各存各的地方(命令行 `~/.codex/recodex/identity.json`,
/// 桌面端 `<app-data>/ReCodex/device-id`),谁也读不到谁 —— 于是同一台机器占掉两个
/// 设备名额(上限才 3),`recodex login` 之后桌面端仍是未登录,撤销时用户也分不清
/// 哪个是哪个。共用一份就都解决了。
///
/// 契约:**`device_id` 是共享字段,ed25519 密钥归命令行所有**。桌面端只读写
/// `device_id`,绝不碰也绝不覆盖密钥字段 —— 那是命令行签名用的。
fn shared_identity_path() -> io::Result<PathBuf> {
    Ok(codex_dir()?.join("recodex").join("identity.json"))
}

/// 读共用身份里的 device_id。
///
/// 三种结果含义完全不同，必须分清：
///   `Ok(Some(id))` 读到了；`Ok(None)` 文件**不存在**（新机器，可以生成）；
///   `Err(..)` 文件在、但读不动或没有 device_id。
///
/// 最后一种**绝不能当成「没有」**：那样会凭空生成一个新身份，于是这台机器在
/// 服务端多占一个设备名额，而用户什么都没做、也看不到任何报错 ——
/// 只是设备列表里莫名多出一台。宁可让登录失败并说清原因。
fn read_shared_device_id(path: &Path) -> io::Result<Option<String>> {
    let text = match fs::read_to_string(path) {
        Ok(value) => value,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    // Windows 上写文件很容易带 UTF-8 BOM（PowerShell 5.1 的 -Encoding utf8 就写），
    // serde_json 见到它直接失败。
    let text = text.trim_start_matches('\u{feff}');
    let value: serde_json::Value = serde_json::from_str(text).map_err(|error| {
        io::Error::new(
            ErrorKind::InvalidData,
            format!("shared identity is unreadable: {error}"),
        )
    })?;
    match value.get("device_id").and_then(|v| v.as_str()) {
        Some(id) if !id.trim().is_empty() => Ok(Some(id.trim().to_string())),
        _ => Err(io::Error::new(
            ErrorKind::InvalidData,
            "shared identity has no device_id",
        )),
    }
}

/// 把 device_id 写进共用身份文件,**保留文件里已有的其它字段**(命令行的密钥就在里面,
/// 整份覆盖会把它的签名密钥抹掉)。
fn write_shared_device_id(path: &Path, device_id: &str) -> io::Result<()> {
    let mut doc = match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str::<serde_json::Value>(&text)
            .unwrap_or_else(|_| serde_json::json!({})),
        Err(error) if error.kind() == ErrorKind::NotFound => serde_json::json!({}),
        Err(error) => return Err(error),
    };
    if !doc.is_object() {
        doc = serde_json::json!({});
    }
    doc["device_id"] = serde_json::Value::String(device_id.to_string());
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "identity path has no parent"))?;
    fs::create_dir_all(parent)?;
    let body = serde_json::to_vec_pretty(&doc)
        .map_err(|error| io::Error::other(format!("encode identity: {error}")))?;
    write_private(path, &body)
}

fn write_private(path: &Path, body: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(body)?;
    file.sync_all()
}

fn install_id_path() -> io::Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .or_else(|| std::env::var_os("APPDATA"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .ok_or_else(|| {
            io::Error::new(ErrorKind::NotFound, "no user data directory is available")
        })?;
    Ok(base.join("ReCodex").join("device-id"))
}

fn read_existing(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(value) if valid(&value) => Ok(Some(value)),
        Ok(_) => Err(io::Error::new(
            ErrorKind::InvalidData,
            "install id is invalid",
        )),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn valid(value: &str) -> bool {
    value.len() == PREFIX.len() + RANDOM_BYTES * 2
        && value.starts_with(PREFIX)
        && value[PREFIX.len()..]
            .chars()
            .all(|character| character.is_ascii_hexdigit())
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}
