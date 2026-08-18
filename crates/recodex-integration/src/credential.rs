use std::fmt;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use url::Url;

/// Returns the credential-manager target for one normalized API origin.
pub fn credential_target_for_api_url(api_url: &str) -> Result<String, CredentialError> {
    let url = Url::parse(api_url).map_err(|_| CredentialError::InvalidCredential)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(CredentialError::InvalidCredential);
    }
    let origin = url.origin();
    if matches!(origin, url::Origin::Opaque(_)) {
        return Err(CredentialError::InvalidCredential);
    }
    Ok(format!(
        "com.recodex.desktop/session@{}",
        origin.ascii_serialization()
    ))
}

/// A token wrapper whose formatting implementations never reveal the secret.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Result<Self, CredentialError> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(CredentialError::InvalidCredential);
        }
        Ok(Self(value))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret([REDACTED])")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct StoredCredentials {
    pub access_token: Secret,
    pub refresh_token: Option<Secret>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CredentialPayload {
    access_token: String,
    refresh_token: Option<String>,
}

impl StoredCredentials {
    fn to_payload(&self) -> CredentialPayload {
        CredentialPayload {
            access_token: self.access_token.expose().to_owned(),
            refresh_token: self
                .refresh_token
                .as_ref()
                .map(|token| token.expose().to_owned()),
        }
    }

    fn from_payload(payload: CredentialPayload) -> Result<Self, CredentialError> {
        Ok(Self {
            access_token: Secret::new(payload.access_token)?,
            refresh_token: payload.refresh_token.map(Secret::new).transpose()?,
        })
    }
}

impl fmt::Debug for StoredCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoredCredentials")
            .field("access_token", &"[REDACTED]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialError {
    InvalidCredential,
    StoreUnavailable,
}

impl fmt::Display for CredentialError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCredential => formatter.write_str("invalid credential"),
            Self::StoreUnavailable => formatter.write_str("credential store unavailable"),
        }
    }
}

impl std::error::Error for CredentialError {}

pub trait CredentialStore: Send + Sync {
    fn save(&self, credentials: StoredCredentials) -> Result<(), CredentialError>;
    fn load(&self) -> Result<Option<StoredCredentials>, CredentialError>;
    fn clear(&self) -> Result<(), CredentialError>;
}

/// A volatile implementation for tests and sessions that must not persist.
#[derive(Clone, Default)]
pub struct MemoryCredentialStore {
    credentials: Arc<Mutex<Option<StoredCredentials>>>,
}

impl MemoryCredentialStore {
    fn lock(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, Option<StoredCredentials>>, CredentialError> {
        self.credentials
            .lock()
            .map_err(|_| CredentialError::StoreUnavailable)
    }
}

impl CredentialStore for MemoryCredentialStore {
    fn save(&self, credentials: StoredCredentials) -> Result<(), CredentialError> {
        *self.lock()? = Some(credentials);
        Ok(())
    }

    fn load(&self) -> Result<Option<StoredCredentials>, CredentialError> {
        Ok(self.lock()?.clone())
    }

    fn clear(&self) -> Result<(), CredentialError> {
        *self.lock()? = None;
        Ok(())
    }
}

/// Windows Credential Manager integration point.
///
/// It deliberately has no plaintext-file fallback. The production backend can
/// replace this placeholder without changing callers or credential semantics.
#[derive(Debug, Clone)]
pub struct WindowsCredentialStore {
    target_name: String,
}

impl WindowsCredentialStore {
    pub fn new(target_name: impl Into<String>) -> Result<Self, CredentialError> {
        let target_name = target_name.into();
        if target_name.trim().is_empty() || target_name.contains('\0') {
            return Err(CredentialError::InvalidCredential);
        }
        Ok(Self { target_name })
    }

    pub fn target_name(&self) -> &str {
        &self.target_name
    }
}

impl CredentialStore for WindowsCredentialStore {
    fn save(&self, credentials: StoredCredentials) -> Result<(), CredentialError> {
        platform::save(&self.target_name, &credentials)
    }

    fn load(&self) -> Result<Option<StoredCredentials>, CredentialError> {
        platform::load(&self.target_name)
    }

    fn clear(&self) -> Result<(), CredentialError> {
        platform::clear(&self.target_name)
    }
}

#[cfg(not(windows))]
mod platform {
    use super::*;

    pub(super) fn save(_: &str, _: &StoredCredentials) -> Result<(), CredentialError> {
        Err(CredentialError::StoreUnavailable)
    }

    pub(super) fn load(_: &str) -> Result<Option<StoredCredentials>, CredentialError> {
        Err(CredentialError::StoreUnavailable)
    }

    pub(super) fn clear(_: &str) -> Result<(), CredentialError> {
        Err(CredentialError::StoreUnavailable)
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_NOT_FOUND};
    use windows_sys::Win32::Security::Credentials::{
        CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE,
        CRED_TYPE_GENERIC,
    };

    fn wide(value: &str) -> Result<Vec<u16>, CredentialError> {
        if value.encode_utf16().any(|unit| unit == 0) {
            return Err(CredentialError::InvalidCredential);
        }
        Ok(value.encode_utf16().chain(std::iter::once(0)).collect())
    }

    fn last_error() -> CredentialError {
        CredentialError::StoreUnavailable
    }

    pub(super) fn save(
        target: &str,
        credentials: &StoredCredentials,
    ) -> Result<(), CredentialError> {
        let target = wide(target)?;
        let bytes = serde_json::to_vec(&credentials.to_payload())
            .map_err(|_| CredentialError::StoreUnavailable)?;
        let credential = CREDENTIALW {
            Flags: 0,
            Type: CRED_TYPE_GENERIC,
            TargetName: target.as_ptr() as *mut u16,
            Comment: null_mut(),
            LastWritten: windows_sys::Win32::Foundation::FILETIME {
                dwLowDateTime: 0,
                dwHighDateTime: 0,
            },
            CredentialBlobSize: bytes.len() as u32,
            CredentialBlob: bytes.as_ptr() as *mut u8,
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            AttributeCount: 0,
            Attributes: null_mut(),
            TargetAlias: null_mut(),
            UserName: null_mut(),
        };
        // SAFETY: all pointers refer to live, immutable buffers for the call duration.
        let result = unsafe { CredWriteW(&credential, 0) };
        if result == 0 {
            Err(last_error())
        } else {
            Ok(())
        }
    }

    pub(super) fn load(target: &str) -> Result<Option<StoredCredentials>, CredentialError> {
        let target = wide(target)?;
        let mut credential: *mut CREDENTIALW = null_mut();
        // SAFETY: the API initializes the out pointer and ownership is released with CredFree.
        let result = unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) };
        if result == 0 {
            // ERROR_NOT_FOUND is the only expected miss; other failures should
            // remain visible instead of being mistaken for a signed-out user.
            return if unsafe { GetLastError() } == ERROR_NOT_FOUND {
                Ok(None)
            } else {
                Err(last_error())
            };
        }
        if credential.is_null() {
            return Err(CredentialError::StoreUnavailable);
        }
        let value = unsafe {
            let item = &*credential;
            if item.CredentialBlob.is_null() || item.CredentialBlobSize == 0 {
                None
            } else {
                Some(
                    std::slice::from_raw_parts(
                        item.CredentialBlob,
                        item.CredentialBlobSize as usize,
                    )
                    .to_vec(),
                )
            }
        };
        unsafe {
            CredFree(credential as *mut _);
        }
        let value = value.ok_or(CredentialError::StoreUnavailable)?;
        let payload =
            serde_json::from_slice(&value).map_err(|_| CredentialError::StoreUnavailable)?;
        StoredCredentials::from_payload(payload).map(Some)
    }

    pub(super) fn clear(target: &str) -> Result<(), CredentialError> {
        let target = wide(target)?;
        // SAFETY: target is a valid NUL-terminated UTF-16 string.
        let result = unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) };
        if result != 0 || unsafe { GetLastError() } == ERROR_NOT_FOUND {
            Ok(())
        } else {
            Err(last_error())
        }
    }
}

/// Removes known token values before a message is passed to diagnostics.
pub fn redact_tokens(message: &str, credentials: &StoredCredentials) -> String {
    let mut redacted = message.replace(credentials.access_token.expose(), "[REDACTED]");
    if let Some(refresh_token) = &credentials.refresh_token {
        redacted = redacted.replace(refresh_token.expose(), "[REDACTED]");
    }
    redacted
}
