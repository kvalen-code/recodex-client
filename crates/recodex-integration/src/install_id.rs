use std::{
    fs::{self, OpenOptions},
    io::{self, ErrorKind, Write},
    path::{Path, PathBuf},
};

const PREFIX: &str = "desktop-";
const RANDOM_BYTES: usize = 16;

/// Returns the stable, non-secret desktop installation identifier.
pub fn load_or_create_install_id() -> io::Result<String> {
    load_or_create_install_id_at(&install_id_path()?)
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
