//! 手机远程整条流程的端到端测试,用一份**假运行时**跑:
//! 把本机的 node 复制成 `recodex-remote(.exe)`,`app/entry.mjs` 换成一个按 §2.2 契约
//! 应答的小脚本(pair --json / status --json / daemon start|stop / unpair)。
//!
//! 走的是面板真正调用的桥入口(`/remote/*`),覆盖:确认码本机计算、二维码、
//! 没登录时退回「只能扫码」、配对完成后拉起守护进程、关开关停服务、断开删凭据、取消杀进程。
//!
//! 隔离:数据目录用 RECODEX_REMOTE_HOME 指到临时目录;桌面设置与诊断日志走 *_for_tests;
//! 开机自启被压住(不碰真实注册表);RECODEX_API_URL 指到一个没有凭据的回环地址 ——
//! 取更新渠道与登记配对都在「未登录」处直接返回,不发任何网络请求。
//! 本机没有 node 时跳过。

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use codex_plus_core::phone_remote;
use serde_json::{Value, json};

const FAKE_ENTRY: &str = r#"
import fs from 'node:fs';
import path from 'node:path';
const home = process.env.RECODEX_REMOTE_HOME;
const flag = (name) => path.join(home, name);
const has = (name) => fs.existsSync(flag(name));
const touch = (name) => fs.writeFileSync(flag(name), String(process.pid));
const rm = (name) => { try { fs.unlinkSync(flag(name)); } catch {} };
const [cmd, sub] = process.argv.slice(2);
const out = (obj) => process.stdout.write(JSON.stringify(obj) + '\n');
if (cmd === 'status') {
  out({ paired: has('paired'), machineId: has('paired') ? 'm-1' : null, server: 'https://relay.example',
        daemon: { running: has('daemon'), pid: has('daemon') ? 42 : null, version: '0.1.0' } });
} else if (cmd === 'pair') {
  touch('pair.pid');
  if (has('paired') && !process.argv.includes('--force')) { out({ event: 'already-paired', machineId: 'm-1' }); process.exit(0); }
  console.log('some noise from a dependency');
  out({ event: 'waiting', publicKey: 'FRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRU=', qr: 'recodex://terminal?abc' });
  const timer = setInterval(() => {
    if (has('approve')) { clearInterval(timer); rm('approve'); touch('paired'); out({ event: 'authorized', machineId: 'm-1' }); process.exit(0); }
  }, 50);
} else if (cmd === 'daemon' && sub === 'start') {
  touch('daemon');
} else if (cmd === 'daemon' && sub === 'stop') {
  rm('daemon');
} else if (cmd === 'unpair') {
  rm('daemon'); rm('paired');
  out({ event: 'unpaired' });
} else {
  process.exit(2);
}
"#;

fn find_node() -> Option<PathBuf> {
    let name = if cfg!(windows) { "node.exe" } else { "node" };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

fn install_fake_runtime(home: &Path, node: &Path) {
    let dir = home.join("runtime").join("1.0.0");
    std::fs::create_dir_all(dir.join("app")).unwrap();
    let exe = if cfg!(windows) {
        "recodex-remote.exe"
    } else {
        "recodex-remote"
    };
    std::fs::copy(node, dir.join(exe)).unwrap();
    std::fs::write(dir.join("app").join("entry.mjs"), FAKE_ENTRY).unwrap();
    std::fs::write(
        home.join("runtime").join("current.json"),
        serde_json::to_vec_pretty(&json!({ "version": "1.0.0", "dir": dir })).unwrap(),
    )
    .unwrap();
}

async fn bridge(path: &str) -> Value {
    phone_remote::handle_bridge(path, &json!({})).await
}

async fn wait_for(phase: &str, timeout: Duration) -> Value {
    let deadline = Instant::now() + timeout;
    loop {
        let status = bridge("/remote/status").await;
        if status["phase"] == phase {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "等 {phase} 超时,最后状态:{status}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_flow_with_a_fake_runtime() {
    let Some(node) = find_node() else {
        eprintln!("本机没有 node,跳过");
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("remote");
    std::fs::create_dir_all(&home).unwrap();
    install_fake_runtime(&home, &node);
    // SAFETY: 本测试文件只有这一个测试,进程内没有别的线程在读这些变量。
    unsafe {
        std::env::set_var("RECODEX_REMOTE_HOME", &home);
        std::env::set_var("RECODEX_API_URL", "http://127.0.0.1:9");
    }
    codex_plus_core::paths::set_settings_path_for_tests(Some(temp.path().join("settings.json")));
    codex_plus_core::diagnostic_log::set_diagnostic_log_path_for_tests(Some(
        temp.path().join("diag.log"),
    ));
    phone_remote::autostart::set_suppressed_for_tests(true);

    // 初始:没配对、开关关着
    let status = bridge("/remote/status").await;
    assert_eq!(status["status"], "ok", "{status}");
    assert_eq!(status["phase"], "idle");
    assert_eq!(status["enabled"], false);
    assert_eq!(status["paired"], false);
    assert_eq!(status["runtime"]["version"], "1.0.0");

    // 打开开关 → 发起配对 → 等手机确认
    let started = bridge("/remote/enable").await;
    assert_eq!(started["enabled"], true, "{started}");
    let waiting = wait_for("waiting", Duration::from_secs(30)).await;
    assert_eq!(
        waiting["code"], "003172",
        "确认码必须由本机从公钥算出(固定向量 32×0x15)"
    );
    assert_eq!(waiting["codeDisplay"], "003 172");
    assert!(
        waiting["qrSvg"].as_str().unwrap().contains("<svg"),
        "{waiting}"
    );
    assert_eq!(
        waiting["phonePrompt"], false,
        "没登录:跟随账号那条路不可用,只能扫码"
    );
    assert!(waiting["phoneNote"].as_str().unwrap().contains("登录"));
    assert!(!waiting["machineName"].as_str().unwrap().is_empty());
    assert!(home.join("pair.pid").exists());

    // 手机那边完成(扫码)→ 运行时给出 authorized → 拉起守护进程
    std::fs::write(home.join("approve"), b"").unwrap();
    let connected = wait_for("connected", Duration::from_secs(30)).await;
    assert_eq!(connected["paired"], true, "{connected}");
    assert_eq!(connected["daemonRunning"], true);
    assert!(home.join("daemon").exists());

    // 关开关:停服务,保留配对
    let disabled = bridge("/remote/disable").await;
    assert_eq!(disabled["status"], "ok", "{disabled}");
    assert_eq!(disabled["enabled"], false);
    assert!(!home.join("daemon").exists());
    assert!(home.join("paired").exists());

    // 再打开:已配对 → 不再配对,直接拉起守护进程
    std::fs::remove_file(home.join("pair.pid")).unwrap();
    bridge("/remote/enable").await;
    wait_for("connected", Duration::from_secs(30)).await;
    assert!(home.join("daemon").exists());
    assert!(!home.join("pair.pid").exists(), "已配对时不该再跑 pair");

    // 断开这台电脑:停服务、删凭据、开关关掉
    let unpaired = bridge("/remote/unpair").await;
    assert_eq!(unpaired["status"], "ok", "{unpaired}");
    assert_eq!(unpaired["paired"], false);
    assert_eq!(unpaired["enabled"], false);
    assert!(!home.join("daemon").exists());

    // 取消:进行中的配对被中止,配对进程被杀
    bridge("/remote/connect").await;
    wait_for("waiting", Duration::from_secs(30)).await;
    let pid: u32 = std::fs::read_to_string(home.join("pair.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let cancelled = bridge("/remote/cancel").await;
    assert_eq!(cancelled["phase"], "idle", "{cancelled}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while process_alive(pid) {
        assert!(Instant::now() < deadline, "取消后配对进程 {pid} 还活着");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // 桌面设置里记下了开关(面板刷新后仍是这个值)
    let settings: Value =
        serde_json::from_str(&std::fs::read_to_string(temp.path().join("settings.json")).unwrap())
            .unwrap();
    assert_eq!(
        settings["phoneRemoteFollowAccount"], true,
        "「连接手机」会把开关打开"
    );

    // 卸载清理:停守护进程(这里压住了开机自启,不碰注册表)
    std::fs::write(home.join("daemon"), b"").unwrap();
    let notes = phone_remote::uninstall_cleanup();
    assert!(!home.join("daemon").exists(), "{notes:?}");
}

fn process_alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let out = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).contains(&pid.to_string())
    }
    #[cfg(not(windows))]
    {
        Path::new(&format!("/proc/{pid}")).exists()
            || std::process::Command::new("kill")
                .args(["-0", &pid.to_string()])
                .status()
                .is_ok_and(|s| s.success())
    }
}
