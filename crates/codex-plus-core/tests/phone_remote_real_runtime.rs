//! 用**真的**远程组件压缩包做一次本地核对(默认不跑,发布前手工跑):
//!
//! ```text
//! RECODEX_REMOTE_E2E_ZIP=<recodex-remote-x.y.z-win-x64.zip> \
//!   cargo test -p codex-plus-core --test phone_remote_real_runtime -- --ignored --nocapture
//! ```
//!
//! 解压走客户端自己的安全解压;数据目录是临时目录(不碰 ~/.recodex/remote、不登记开机自启)。
//! `pair --json` 只读到第一条 waiting 就杀掉 —— **不会完成真实配对**(中继上只留一条没人确认的请求)。

use std::time::Duration;

use codex_plus_core::phone_remote::{code, install, layout, runtime};
use tokio::io::AsyncBufReadExt;

#[tokio::test]
#[ignore]
async fn real_runtime_installs_reports_status_and_starts_pairing() {
    let Ok(zip) = std::env::var("RECODEX_REMOTE_E2E_ZIP") else {
        eprintln!("没设 RECODEX_REMOTE_E2E_ZIP,跳过");
        return;
    };
    let archive = std::fs::read(&zip).expect("读压缩包");
    let temp = tempfile::tempdir().unwrap();
    let home = layout::RemoteHome {
        root: temp.path().join("remote"),
        custom: true,
    };
    let os = std::env::consts::OS;
    let dir = install::install_archive(&home, "0.1.0", &archive, os).expect("安装");
    let current = layout::RuntimeCurrent {
        version: "0.1.0".into(),
        dir: dir.to_string_lossy().into_owned(),
    };
    layout::write_runtime_current(&home, &current).unwrap();
    assert_eq!(
        layout::read_runtime_current(&home, os),
        Some(current.clone())
    );

    let rt = runtime::Runtime {
        current,
        home: home.clone(),
        os: if os == "windows" { "windows" } else { "unix" },
    };
    let status = rt.status().await.expect("status --json");
    eprintln!("status: {status:?}");
    assert!(!status.paired, "临时目录里不该有凭据");
    assert!(!status.daemon.running);

    let mut cmd = rt.command(&["pair", "--json"]);
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = cmd.spawn().expect("pair --json");
    let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let event = tokio::time::timeout(Duration::from_secs(60), async {
        while let Ok(Some(line)) = lines.next_line().await {
            eprintln!("pair> {line}");
            if let Some(event) = runtime::parse_pair_event_line(&line) {
                return Some(event);
            }
        }
        None
    })
    .await
    .expect("60 秒内没有配对事件");
    let _ = child.kill().await;
    match event {
        Some(runtime::PairEvent::Waiting { public_key, qr }) => {
            let key =
                code::decode_public_key(&public_key).expect("公钥是带填充的标准 base64、32 字节");
            let c = code::confirm_code(&key);
            eprintln!("确认码 {}  二维码 {qr}", code::format_confirm_code(&c));
            assert!(qr.starts_with("recodex://terminal?"), "{qr}");
            assert_eq!(c.len(), 6);
        }
        other => panic!("第一条事件应是 waiting:{other:?}"),
    }
}
