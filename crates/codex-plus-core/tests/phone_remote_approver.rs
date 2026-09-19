//! 批准方绑定(docs/remote-app-plan.md §2.4.1)的行为测试:一份**假运行时**(node 装成
//! `recodex-remote`)+ 一个**假后台**,把真实网络里很难造的那几种分支跑出来:
//!
//!   1. 老运行时(waiting 里没有 `approverCheck`)→ 不登记后台请求,只扫码,stdin 收到 `unbound`;
//!   2. 正常:登记带 `approver_check`、二维码加 `&bind=1`、批准方原样写进 stdin;
//!   3. 后台已批准却缺批准方 → 中止(绝不降级成不核对);
//!   4. 攻击者抢先批准(运行时报 `approver_mismatch`)→ 面板给「被他人抢先确认」的说法;
//!   5. 后台 5xx → 退避重试 3 次仍失败就中止,不静默降级;
//!   6. 后台没把请求登记成「会核对批准方」→ 撤回它,只扫码。
//!
//! 隔离与 phone_remote_flow 相同:RECODEX_REMOTE_HOME 指到临时目录、设置与诊断日志走 *_for_tests、
//! 开机自启压住、RECODEX_API_URL 指到回环(取更新渠道在「未登录」处就返回)。
//! 本机没有 node 时跳过。全部场景在**一个** #[tokio::test] 里顺序跑:环境变量与那个全局控制器
//! 都是进程级的,并行会互相踩。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use codex_plus_core::phone_remote::{self, PairBackend};
use recodex_integration::AdapterError;
use recodex_integration::remote_pair::{PairApiError, RemotePairCreated, RemotePairStatus};
use serde_json::{Value, json};

/// 假运行时:公钥固定为 32×0x15(确认码 003172),把 stdin 收到的每一行原样记到 directives.log。
const FAKE_ENTRY: &str = r#"
import fs from 'node:fs';
import path from 'node:path';
const home = process.env.RECODEX_REMOTE_HOME;
const flag = (name) => path.join(home, name);
const has = (name) => fs.existsSync(flag(name));
const touch = (name) => fs.writeFileSync(flag(name), String(process.pid));
const args = process.argv.slice(2);
const [cmd, sub] = args;
const out = (obj) => process.stdout.write(JSON.stringify(obj) + '\n');
if (cmd === 'status') {
  out({ paired: has('paired'), machineId: has('paired') ? 'm-1' : null, server: 'https://relay.example',
        daemon: { running: has('daemon'), pid: has('daemon') ? 42 : null, version: '0.1.0' } });
} else if (cmd === 'pair') {
  fs.writeFileSync(flag('pair.args'), args.join(' '));
  const waiting = { event: 'waiting', publicKey: 'FRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRU=', qr: 'recodex://terminal?abc' };
  // 老运行时不认 --bind-approver:waiting 里没有 approverCheck,它也不会核对批准方。
  if (args.includes('--bind-approver') && !has('old-runtime')) waiting.approverCheck = true;
  out(waiting);
  let buffer = '';
  process.stdin.setEncoding('utf8');
  process.stdin.on('data', (chunk) => {
    buffer += chunk;
    let index;
    while ((index = buffer.indexOf('\n')) >= 0) {
      const line = buffer.slice(0, index);
      buffer = buffer.slice(index + 1);
      if (!line) continue;
      fs.appendFileSync(flag('directives.log'), line + '\n');
      let parsed = null;
      try { parsed = JSON.parse(line); } catch {}
      if (parsed && parsed.type === 'approver') {
        if (has('mismatch')) {
          // 中继交来的身份不是后台记下的批准方:拒绝,不落盘任何凭据。
          out({ event: 'error', code: 'approver_mismatch', message: 'relay answer came from another account' });
          process.exit(0);
        }
        touch('paired');
        out({ event: 'authorized', machineId: 'm-1' });
        process.exit(0);
      }
    }
  });
  setInterval(() => {}, 1000);
} else if (cmd === 'daemon' && sub === 'start') {
  touch('daemon');
} else if (cmd === 'daemon' && sub === 'stop') {
  try { fs.unlinkSync(flag('daemon')); } catch {}
} else {
  process.exit(2);
}
"#;

const PAIR_ID: &str = "pairid_0123456789abcdef";
const APPROVER_KEY: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
const APPROVER_ACCOUNT: &str = "acc_relay-1";

#[derive(Default)]
struct BackendState {
    /// 每次登记的 (公钥, 机器名, approver_check)。
    creates: Vec<(String, String, bool)>,
    cancels: Vec<String>,
    /// 有值就让登记失败(每次尝试都失败)。
    create_error: Option<PairApiError>,
    /// 登记成功时回的内容。
    created: RemotePairCreated,
    /// 轮询看到的状态。
    status: RemotePairStatus,
}

#[derive(Clone, Default)]
struct FakeBackend(Arc<Mutex<BackendState>>);

impl FakeBackend {
    fn with(&self, edit: impl FnOnce(&mut BackendState)) {
        edit(&mut self.0.lock().unwrap());
    }

    fn read<R>(&self, view: impl FnOnce(&BackendState) -> R) -> R {
        view(&self.0.lock().unwrap())
    }

    /// 一条「新后台 + 还没人批准」的干净底子。
    fn reset(&self) {
        self.with(|state| {
            *state = BackendState {
                created: RemotePairCreated {
                    id: PAIR_ID.into(),
                    // 与假运行时那把公钥对得上,否则会被当成后台掉包公钥
                    code: "003172".into(),
                    expires_at: String::new(),
                    approver_binding: true,
                    approver_check: true,
                },
                status: RemotePairStatus {
                    status: "pending".into(),
                    ..RemotePairStatus::default()
                },
                ..BackendState::default()
            };
        });
    }
}

impl PairBackend for FakeBackend {
    fn create(
        &self,
        public_key: &str,
        machine_name: &str,
        approver_check: bool,
    ) -> Result<RemotePairCreated, PairApiError> {
        let mut state = self.0.lock().unwrap();
        state
            .creates
            .push((public_key.into(), machine_name.into(), approver_check));
        match &state.create_error {
            Some(error) => Err(error.clone()),
            None => Ok(state.created.clone()),
        }
    }

    fn status(&self, _id: &str) -> Result<RemotePairStatus, PairApiError> {
        Ok(self.0.lock().unwrap().status.clone())
    }

    fn cancel(&self, id: &str) -> Result<(), AdapterError> {
        self.0.lock().unwrap().cancels.push(id.into());
        Ok(())
    }
}

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

struct Harness {
    home: PathBuf,
    backend: FakeBackend,
}

impl Harness {
    fn flag(&self, name: &str) -> PathBuf {
        self.home.join(name)
    }

    fn touch(&self, name: &str) {
        std::fs::write(self.flag(name), b"").unwrap();
    }

    fn remove(&self, name: &str) {
        let _ = std::fs::remove_file(self.flag(name));
    }

    fn directives(&self) -> String {
        std::fs::read_to_string(self.flag("directives.log")).unwrap_or_default()
    }

    /// 每个场景从同一个干净底子开始。
    async fn reset(&self) {
        bridge("/remote/cancel").await;
        for flag in ["paired", "daemon", "directives.log", "old-runtime", "mismatch", "pair.args"] {
            self.remove(flag);
        }
        self.backend.reset();
    }

    /// stdin 上收到的指示(逐行解析)。
    fn directive_values(&self) -> Vec<Value> {
        self.directives()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str(line).expect(line))
            .collect()
    }

    /// 等 stdin 上出现至少 n 行指示。
    async fn wait_directives(&self, n: usize, timeout: Duration) -> Vec<Value> {
        let deadline = Instant::now() + timeout;
        loop {
            let values = self.directive_values();
            if values.len() >= n {
                return values;
            }
            assert!(
                Instant::now() < deadline,
                "等 {n} 行 stdin 指示超时,现在是:{:?}",
                self.directives()
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn approver_binding_end_to_end() {
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
    let backend = FakeBackend::default();
    phone_remote::set_pair_backend_for_tests(Some(Arc::new(backend.clone())));
    let h = Harness {
        home: home.clone(),
        backend: backend.clone(),
    };

    // ①老运行时(waiting 里没有 approverCheck):它不会核对批准方 ——
    //   这里就**不登记**后台请求(不能让手机看到一条没人核对的请求),只显示二维码。
    h.reset().await;
    h.touch("old-runtime");
    bridge("/remote/connect").await;
    let waiting = wait_for("waiting", Duration::from_secs(30)).await;
    assert!(
        std::fs::read_to_string(h.flag("pair.args"))
            .unwrap()
            .contains("--bind-approver"),
        "运行时必须以 pair --json --bind-approver 起来"
    );
    assert_eq!(waiting["phonePrompt"], false, "{waiting}");
    let note = waiting["phoneNote"].as_str().unwrap();
    assert!(note.contains("远程组件版本较旧"), "{note}");
    assert!(
        backend.read(|state| state.creates.is_empty()),
        "老运行时不核对批准方 → 一条后台请求都不能登记"
    );
    assert_eq!(
        h.wait_directives(1, Duration::from_secs(10)).await,
        vec![json!({ "type": "unbound" })],
        "只能扫码时要明确告诉运行时这次不做绑定"
    );
    // 二维码不带 &bind=1(手机会按老流程扫)
    assert_eq!(
        waiting["qrSvg"],
        json!(codex_plus_core::connect::weixin::render_qr_svg("recodex://terminal?abc").unwrap())
    );

    // ②正常一条龙:登记带 approver_check → 二维码加 &bind=1 → 手机批准 →
    //   批准方原样进 stdin → 运行时给出 authorized。
    h.reset().await;
    bridge("/remote/connect").await;
    let waiting = wait_for("waiting", Duration::from_secs(30)).await;
    assert_eq!(waiting["phonePrompt"], true, "{waiting}");
    assert_eq!(waiting["code"], "003172");
    assert_eq!(
        backend.read(|state| state.creates.clone()),
        vec![(
            "FRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRUVFRU=".to_string(),
            waiting["machineName"].as_str().unwrap().to_string(),
            true,
        )],
        "登记必须声明 approver_check"
    );
    assert_eq!(
        waiting["qrSvg"],
        json!(
            codex_plus_core::connect::weixin::render_qr_svg("recodex://terminal?abc&bind=1")
                .unwrap()
        ),
        "登记成功后二维码内容要加 &bind=1"
    );
    backend.with(|state| {
        state.status = RemotePairStatus {
            status: "approved".into(),
            approver_public_key: APPROVER_KEY.into(),
            approver_account_id: APPROVER_ACCOUNT.into(),
        };
    });
    assert_eq!(
        h.wait_directives(1, Duration::from_secs(30)).await,
        vec![json!({
            "type": "approver",
            "key": APPROVER_KEY,
            "account": APPROVER_ACCOUNT,
        })],
        "批准方要按 §2.4.1 原样写成一行"
    );
    let connected = wait_for("connected", Duration::from_secs(30)).await;
    assert_eq!(connected["paired"], true, "{connected}");

    // ③后台说已批准,却没给批准方:没法核对中继交来的身份 —— 中止,不降级成不核对。
    h.reset().await;
    bridge("/remote/repair").await;
    wait_for("waiting", Duration::from_secs(30)).await;
    backend.with(|state| {
        state.status = RemotePairStatus {
            status: "approved".into(),
            ..RemotePairStatus::default()
        };
    });
    let error = wait_for("error", Duration::from_secs(30)).await;
    assert_eq!(
        error["message"],
        json!(phone_remote::MSG_APPROVER_ABSENT_BOTH),
        "后台支持绑定却没给批准方:服务端或手机 App 旧,两个都要提"
    );
    assert!(
        !h.directives().contains("approver"),
        "缺字段时绝不能往 stdin 递半份批准方:{}",
        h.directives()
    );

    // ④攻击者用自己的中继账号抢先批准:运行时核对不过报 approver_mismatch,
    //   面板要说「被他人抢先确认」,不能说成「手机没把凭据交给中继」。
    h.reset().await;
    h.touch("mismatch");
    bridge("/remote/repair").await;
    wait_for("waiting", Duration::from_secs(30)).await;
    backend.with(|state| {
        state.status = RemotePairStatus {
            status: "approved".into(),
            approver_public_key: APPROVER_KEY.into(),
            approver_account_id: APPROVER_ACCOUNT.into(),
        };
    });
    let error = wait_for("error", Duration::from_secs(30)).await;
    assert_eq!(error["message"], json!(phone_remote::MSG_APPROVER_MISMATCH));
    assert!(!h.flag("paired").exists(), "核对没过就不能落盘凭据");

    // ⑤后台 5xx:它可能已经建好了记录,降级就等于把核对悄悄关掉 —— 重试 3 次后中止。
    h.reset().await;
    backend.with(|state| state.create_error = Some(PairApiError::Http(503)));
    bridge("/remote/repair").await;
    let error = wait_for("error", Duration::from_secs(60)).await;
    let message = error["message"].as_str().unwrap();
    assert!(message.contains("为安全起见已中止"), "{message}");
    assert!(message.contains("503"), "{message}");
    assert_eq!(
        backend.read(|state| state.creates.len()),
        3,
        "网络错误 / 5xx 要退避重试 3 次"
    );
    assert!(
        !h.directives().contains("unbound"),
        "不可降级的失败不能递 unbound(那等于关掉核对)"
    );

    // ⑥后台收下了请求却没登记成「会核对批准方」:撤回它,只用扫码。
    h.reset().await;
    backend.with(|state| {
        state.created.approver_binding = false;
        state.created.approver_check = false;
    });
    bridge("/remote/repair").await;
    let waiting = wait_for("waiting", Duration::from_secs(30)).await;
    assert_eq!(waiting["phonePrompt"], false, "{waiting}");
    assert!(
        waiting["phoneNote"]
            .as_str()
            .unwrap()
            .contains("服务端暂不支持")
    );
    assert_eq!(
        backend.read(|state| state.cancels.clone()),
        vec![PAIR_ID.to_string()],
        "没登记成绑定的那条请求必须撤回,别让手机看到它"
    );
    assert_eq!(
        h.wait_directives(1, Duration::from_secs(10)).await,
        vec![json!({ "type": "unbound" })]
    );

    bridge("/remote/cancel").await;
    phone_remote::set_pair_backend_for_tests(None);
}
