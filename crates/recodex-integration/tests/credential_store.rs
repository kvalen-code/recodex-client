#[path = "../src/credential.rs"]
mod credential;

use credential::{
    credential_target_for_api_url, redact_tokens, CredentialError, CredentialStore,
    MemoryCredentialStore, Secret, StoredCredentials, PlatformCredentialStore,
};

fn credentials() -> StoredCredentials {
    StoredCredentials {
        access_token: Secret::new("rct_access_super_secret").unwrap(),
        refresh_token: Some(Secret::new("rct_refresh_super_secret").unwrap()),
    }
}

#[test]
fn logout_clear_removes_all_stored_credentials() {
    let store = MemoryCredentialStore::default();
    store.save(credentials()).unwrap();
    assert!(store.load().unwrap().is_some());

    store.clear().unwrap();

    assert_eq!(store.load().unwrap(), None);
}

#[test]
fn token_formatting_and_diagnostic_redaction_never_expose_secrets() {
    let credentials = credentials();
    let message = format!(
        "request failed access={} refresh={}",
        credentials.access_token.expose(),
        credentials.refresh_token.as_ref().unwrap().expose()
    );
    let redacted = redact_tokens(&message, &credentials);

    for secret in ["rct_access_super_secret", "rct_refresh_super_secret"] {
        assert!(!format!("{credentials:?}").contains(secret));
        assert!(!format!("{}", credentials.access_token).contains(secret));
        assert!(!redacted.contains(secret));
    }
    assert_eq!(redacted.matches("[REDACTED]").count(), 2);
}

/// 没有系统级密钥库的平台(实际上只剩 Linux)必须明确地"存不了",而**不是**
/// 悄悄回落到明文文件 —— 把令牌写进明文比登不上更糟。
#[test]
#[cfg(not(any(windows, target_os = "macos")))]
fn platforms_without_a_keystore_have_no_plaintext_fallback() {
    let store = PlatformCredentialStore::new("com.recodex.desktop/session").unwrap();
    assert_eq!(store.target_name(), "com.recodex.desktop/session");
    assert_eq!(
        store.save(credentials()),
        Err(CredentialError::StoreUnavailable)
    );
    assert_eq!(store.load(), Err(CredentialError::StoreUnavailable));
    assert_eq!(store.clear(), Err(CredentialError::StoreUnavailable));
}

#[test]
#[cfg(any(windows, target_os = "macos"))]
fn keystore_backend_rejects_invalid_target_without_writing() {
    let error = PlatformCredentialStore::new("bad\0target").unwrap_err();
    assert_eq!(error, CredentialError::InvalidCredential);
    let store = PlatformCredentialStore::new("com.recodex.desktop/session").unwrap();
    assert_eq!(store.target_name(), "com.recodex.desktop/session");
}

#[cfg(any(windows, target_os = "macos"))]
struct CredentialCleanup<'a>(&'a PlatformCredentialStore);

#[cfg(any(windows, target_os = "macos"))]
impl Drop for CredentialCleanup<'_> {
    fn drop(&mut self) {
        let _ = self.0.clear();
    }
}

/// 同一条契约,两个后端(Windows 凭据管理器 / macOS 钥匙串)都必须满足:
/// 存得进、读得出同一份、删完读到 None。
///
/// 这是 macOS 支持的**关键回归**:此前非 Windows 一律返回 StoreUnavailable,
/// 也就是登录状态存不下来 —— 包能编译,却登不上。
#[test]
#[cfg(any(windows, target_os = "macos"))]
fn keystore_backend_round_trips_through_the_os() {
    let target = format!(
        "com.recodex.test/credential-roundtrip-{}",
        std::process::id()
    );
    let store = PlatformCredentialStore::new(target).unwrap();
    let _cleanup = CredentialCleanup(&store);

    store.save(credentials()).unwrap();
    assert_eq!(store.load().unwrap(), Some(credentials()));

    store.clear().unwrap();
    assert_eq!(store.load().unwrap(), None);
    // clear 必须幂等 —— 卸载流程会重复调用
    store.clear().unwrap();
}

#[test]
fn empty_secrets_and_target_names_are_rejected_without_echoing_input() {
    let secret_error = Secret::new("   ").unwrap_err();
    let target_error = PlatformCredentialStore::new(" ").unwrap_err();
    assert_eq!(secret_error.to_string(), "invalid credential");
    assert_eq!(target_error.to_string(), "invalid credential");
}

#[test]
fn credential_target_is_bound_to_the_normalized_api_origin() {
    let production = credential_target_for_api_url("https://api.recodex.ai/v1").unwrap();
    let production_default_port =
        credential_target_for_api_url("https://api.recodex.ai:443/another/path").unwrap();
    let staging = credential_target_for_api_url("https://staging.recodex.ai/v1").unwrap();
    let local_a = credential_target_for_api_url("http://127.0.0.1:8080").unwrap();
    let local_b = credential_target_for_api_url("http://127.0.0.1:8081").unwrap();

    assert_eq!(production, production_default_port);
    assert_ne!(production, staging);
    assert_ne!(production, local_a);
    assert_ne!(local_a, local_b);
    assert!(production.starts_with("com.recodex.desktop/session@"));
}
