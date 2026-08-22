//! 命令行与桌面端共用设备身份的回归。
//!
//! 单独一个测试文件、只放一个测试:这里要改 `USERPROFILE` / `HOME` / `LOCALAPPDATA`
//! 这类进程级环境变量,和别的测试并行跑会互相踩(与 official_mode_round_trip 同理)。
//!
//! 盯的是三档取值优先级,任意一档搞错都有真实后果:
//!   1. 共用文件里的 device_id —— 错了就同机又占两个名额(上限才 3)
//!   2. 旧的桌面端私有文件 —— 错了就把**存量用户踢下线**
//!   3. 都没有才新建
//! 另外:写共用文件时**绝不能**覆盖命令行的 ed25519 密钥。

use recodex_integration::load_or_create_install_id;

#[test]
fn desktop_identity_shares_the_cli_device_id() {
    let sandbox = std::env::temp_dir().join(format!("recodex-devid-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&sandbox);
    std::fs::create_dir_all(sandbox.join(".codex/recodex")).unwrap();
    std::fs::create_dir_all(sandbox.join("appdata")).unwrap();

    // SAFETY:本文件只有这一个测试,没有并发读写这些变量的线程。
    unsafe {
        std::env::set_var("USERPROFILE", &sandbox);
        std::env::set_var("HOME", &sandbox);
        std::env::set_var("LOCALAPPDATA", sandbox.join("appdata"));
        std::env::remove_var("APPDATA");
    }

    let shared = sandbox.join(".codex/recodex/identity.json");
    let legacy = sandbox.join("appdata/ReCodex/device-id");

    // 档 1:共用文件里已有命令行注册的 id + 它的签名密钥
    std::fs::write(
        &shared,
        r#"{"device_id":"cli-device-abc","private_key":"SEED","public_key":"PUB"}"#,
    )
    .unwrap();
    assert_eq!(
        "cli-device-abc",
        load_or_create_install_id().unwrap(),
        "命令行已注册时必须沿用它的 device_id,否则同机占两个名额"
    );
    let after = std::fs::read_to_string(&shared).unwrap();
    assert!(
        after.contains("SEED") && after.contains("PUB"),
        "绝不能覆盖命令行的 ed25519 密钥,那会让它的签名失效:{after}"
    );

    // 档 2:没有共用文件,但存量桌面端有自己的 id —— 沿用,并登记进共用文件
    std::fs::remove_file(&shared).unwrap();
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    let legacy_id = format!("desktop-{}", "ab".repeat(16));
    std::fs::write(&legacy, &legacy_id).unwrap();
    assert_eq!(
        legacy_id,
        load_or_create_install_id().unwrap(),
        "存量桌面端不能被换 id —— 那等于把用户踢下线"
    );
    assert!(
        std::fs::read_to_string(&shared).unwrap().contains(&legacy_id),
        "存量 id 必须登记进共用文件,命令行以后才认它"
    );

    // 档 3:全新机器 —— 新建，且必须稳定
    std::fs::remove_file(&shared).unwrap();
    std::fs::remove_file(&legacy).unwrap();
    let fresh = load_or_create_install_id().unwrap();
    assert!(fresh.starts_with("desktop-"), "格式不对:{fresh}");
    assert_eq!(fresh, load_or_create_install_id().unwrap(), "设备身份必须稳定");

    let _ = std::fs::remove_dir_all(&sandbox);
}
