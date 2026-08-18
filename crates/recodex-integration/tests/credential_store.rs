#[path = "../src/credential.rs"]
mod credential;

use credential::{
    credential_target_for_api_url, redact_tokens, CredentialError, CredentialStore,
    MemoryCredentialStore, Secret, StoredCredentials, WindowsCredentialStore,
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

#[test]
#[cfg(not(windows))]
fn windows_placeholder_has_no_plaintext_fallback() {
    let store = WindowsCredentialStore::new("com.recodex.desktop/session").unwrap();
    assert_eq!(store.target_name(), "com.recodex.desktop/session");
    assert_eq!(
        store.save(credentials()),
        Err(CredentialError::StoreUnavailable)
    );
    assert_eq!(store.load(), Err(CredentialError::StoreUnavailable));
    assert_eq!(store.clear(), Err(CredentialError::StoreUnavailable));
}

#[test]
#[cfg(windows)]
fn windows_backend_rejects_invalid_target_without_writing() {
    let error = WindowsCredentialStore::new("bad\0target").unwrap_err();
    assert_eq!(error, CredentialError::InvalidCredential);
    let store = WindowsCredentialStore::new("com.recodex.desktop/session").unwrap();
    assert_eq!(store.target_name(), "com.recodex.desktop/session");
}

#[cfg(windows)]
struct CredentialCleanup<'a>(&'a WindowsCredentialStore);

#[cfg(windows)]
impl Drop for CredentialCleanup<'_> {
    fn drop(&mut self) {
        let _ = self.0.clear();
    }
}

#[test]
#[cfg(windows)]
fn windows_backend_round_trips_through_credential_manager() {
    let target = format!(
        "com.recodex.test/credential-roundtrip-{}",
        std::process::id()
    );
    let store = WindowsCredentialStore::new(target).unwrap();
    let _cleanup = CredentialCleanup(&store);

    store.save(credentials()).unwrap();
    assert_eq!(store.load().unwrap(), Some(credentials()));

    store.clear().unwrap();
    assert_eq!(store.load().unwrap(), None);
}

#[test]
fn empty_secrets_and_target_names_are_rejected_without_echoing_input() {
    let secret_error = Secret::new("   ").unwrap_err();
    let target_error = WindowsCredentialStore::new(" ").unwrap_err();
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
