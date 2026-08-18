use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

use recodex_integration::load_or_create_install_id_at;

#[test]
fn reuses_one_non_sensitive_install_id_across_desktop_logins() {
    let directory = std::env::temp_dir().join(format!(
        "recodex-install-id-test-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&directory).unwrap();
    let path = directory.join("device-id");

    let first = load_or_create_install_id_at(&path).unwrap();
    let second = load_or_create_install_id_at(&path).unwrap();

    assert_eq!(first, second);
    assert!(first.starts_with("desktop-"));
    assert_eq!(first.len(), "desktop-".len() + 32);
    assert!(first["desktop-".len()..]
        .chars()
        .all(|character| character.is_ascii_hexdigit()));
    assert_eq!(fs::read_to_string(path).unwrap(), first);
    fs::remove_dir_all(directory).unwrap();
}
