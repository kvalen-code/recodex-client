//! 共用身份读不动时，**绝不能**悄悄换一个新身份。
//!
//! 换新身份的后果是：这台机器在服务端多占一个设备名额，而用户什么都没做、
//! 也看不到任何报错 —— 只是设备列表里莫名多出一台
//! （2026-08-26 用户就问「我授权后为什么显示两个设备」）。
//! 宁可让登录失败并说清是哪个文件坏了。
//!
//! 单独一个文件、只放一个测试：这里要改 USERPROFILE / LOCALAPPDATA 这类
//! **进程级**环境变量，和别的测试并行跑会互相踩（第一版就是这么假绿的）。

use recodex_integration::load_or_create_install_id;
use std::fs;
use std::path::{Path, PathBuf};

fn sandbox(root: &Path, name: &str) -> PathBuf {
    let dir = root.join(name);
    fs::create_dir_all(dir.join(".codex").join("recodex")).unwrap();
    // SAFETY: 本文件只有这一个测试，没有并发读写这些变量的线程。
    unsafe {
        std::env::set_var("USERPROFILE", &dir);
        std::env::set_var("HOME", &dir);
        std::env::set_var("LOCALAPPDATA", dir.join("localappdata"));
        std::env::set_var("APPDATA", dir.join("appdata"));
    }
    dir
}

fn shared_path(home: &Path) -> PathBuf {
    home.join(".codex").join("recodex").join("identity.json")
}

#[test]
fn a_broken_shared_identity_never_turns_into_a_second_device() {
    let root = std::env::temp_dir().join(format!("recodex-identity-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);

    // 1) 文件损坏 → 必须报错，且不能覆盖原文件（覆盖了就再也查不出原来是什么）
    let home = sandbox(&root, "corrupt");
    fs::write(shared_path(&home), "{ this is not json").unwrap();
    let got = load_or_create_install_id();
    assert!(
        got.is_err(),
        "身份文件损坏时返回了 {got:?} —— 那是一个**新**设备 id，会凭空多占一个名额"
    );
    assert_eq!(
        fs::read_to_string(shared_path(&home)).unwrap(),
        "{ this is not json",
        "读不动的身份文件不该被覆盖"
    );

    // 2) 有文件但缺 device_id（命令行写过密钥、还没登录过就是这形状）→ 同样报错
    let home = sandbox(&root, "nodev");
    fs::write(
        shared_path(&home),
        r#"{"private_key":"x","public_key":"y"}"#,
    )
    .unwrap();
    assert!(
        load_or_create_install_id().is_err(),
        "缺 device_id 时也必须报错，不能顺手生成一个"
    );

    // 3) 带 UTF-8 BOM 不算损坏：Windows 上写 JSON 太容易带上它
    //    （PowerShell 5.1 的 -Encoding utf8 就写），一律判损坏等于把
    //    每台 Windows 机器都锁在门外。
    let home = sandbox(&root, "bom");
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(br#"{"device_id":"HZ57S7gMhaqDeWFuIsYlLw"}"#);
    fs::write(shared_path(&home), bytes).unwrap();
    assert_eq!(
        load_or_create_install_id().expect("带 BOM 的身份文件必须仍读得出来"),
        "HZ57S7gMhaqDeWFuIsYlLw"
    );

    // 4) 文件不存在是正常的新机器：生成，并且**必须落盘** ——
    //    不落盘的话下次又是一个新 id，又多一个名额。
    let home = sandbox(&root, "fresh");
    let id = load_or_create_install_id().expect("新机器应生成身份");
    assert!(id.starts_with("desktop-"), "{id}");
    assert!(
        fs::read_to_string(shared_path(&home)).unwrap().contains(&id),
        "生成的身份没有落盘"
    );
    assert_eq!(
        load_or_create_install_id().expect("再读一次"),
        id,
        "同一台机器两次调用必须给出同一个 id"
    );
}
