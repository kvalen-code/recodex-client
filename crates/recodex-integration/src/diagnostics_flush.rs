//! 把本地诊断日志(codex-plus.log,JSONL)里的报错**自动**上传到 /api/v1/diagnostics。
//!
//! 为什么要这个:客户越来越多,「电脑连不上 / 启动就崩」不能每个都远程看。launcher 和
//! manager 早就把每一步都记进本地日志,但之前只有一个手动「发送诊断」按钮,而且要求已登录
//! —— 恰恰连不上、登不进的人用不了。这里让客户端自己 phone home。
//!
//! 设计要点:
//! - **水位续传**:sidecar 文件记「上传到第几字节」,只有真传成功才推进。断网/限流时停下,
//!   下次启动或下个周期从水位续传 —— 本地日志本身就是缓冲,不另造队列。
//! - **只传报错**:日志里绝大多数是正常启动步骤,全传又吵又烧匿名口的限流额度。按事件名
//!   关键词 + detail 里有没有 error 字段挑(见 [`is_reportable`])。
//! - **匿名兜底**:没 token 直接不带 Authorization 发,服务端按匿名收;body 里的 device_id
//!   用 install_id —— 就是登录时注册的设备号,服务端能 join 回用户。带了 token 但 401
//!   (token 中途轮换了)也退一次匿名:报错不能因为身份问题丢掉。
//! - **先 redact 再发**:detail 里可能夹着 rct_/sk-/Bearer/token=。校验是拒收,拒收就把整条
//!   诊断丢了;把密钥替换成 [redacted] 才是保留信息的做法。
//! - **日志被截尾过**(core 50MB→5MB 压缩)文件会变短:水位 > 文件长度就归零重来,代价是
//!   可能重传一小段,可接受。
//!
//! 不做的:不在这里决定「什么时候跑」—— 那由 `desktop::ReCodexState::spawn_diagnostics_flush`
//! 起线程(启动 20s 后 + 每 10 分钟),再由 **launcher** 建完 state 时调它并传入日志路径:这个
//! crate 拿不到 codex-plus-core 的日志路径。注意出货的是 codex-plus-launcher,Tauri manager
//! 已弃用不构建,别把钩子放进 recodex_commands.rs。

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::{validate_diagnostic_report, Adapter, DiagnosticReport, Transport};

const DIAGNOSTICS_PATH: &str = "/api/v1/diagnostics";
/// 事件名里出现这些就当报错传。ponytail: 关键词启发式,漏了再加词,别上分类器。
const ERROR_MARKERS: &[&str] = &[
    "fail", "error", "panic", "crash", "timeout", "denied", "refused", "unreachable", "abort",
];
const MAX_EVENT: usize = 64;
const MAX_SHORT: usize = 128;
const MAX_MESSAGE: usize = 2048;

/// 一轮 flush 的结果,给调用方记日志/决定要不要提前下一轮。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FlushOutcome {
    pub uploaded: usize,
    /// 不是报错、或本地校验不过、或服务端 4xx 拒收 —— 都跳过并推进水位,别卡住后面的。
    pub skipped: usize,
    /// 提前停下的原因:`rate_limited` / `unavailable`。None = 处理到了文件末尾。
    pub stopped: Option<&'static str>,
}

enum SendResult {
    Uploaded,
    Rejected,
    Unauthorized,
    RateLimited,
    Unavailable,
}

impl<T: Transport> Adapter<T> {
    /// 从水位处读 `log_path`,把可上报的条目逐条 POST 上去,最多 `max_per_flush` 条。
    /// 任何 I/O 失败都静默返回空结果 —— 诊断上报绝不能反过来把客户端搞崩。
    pub fn flush_diagnostic_log(
        &self,
        log_path: &Path,
        device_id: &str,
        client_version: &str,
        max_per_flush: usize,
    ) -> FlushOutcome {
        let mut out = FlushOutcome::default();
        let bytes = match std::fs::read(log_path) {
            Ok(bytes) => bytes,
            Err(_) => return out,
        };
        let wm_path = watermark_path(log_path);
        let mut watermark = read_watermark(&wm_path) as usize;
        if watermark > bytes.len() {
            // 文件被截尾/清空过,老水位没意义了。
            watermark = 0;
        }
        let os = std::env::consts::OS;
        let mut consumed = watermark;
        let mut cursor = watermark;
        while cursor < bytes.len() {
            // 只处理完整行;末尾半行是别的进程正在写,下次再说。
            let Some(nl) = bytes[cursor..].iter().position(|b| *b == b'\n') else {
                break;
            };
            let line = &bytes[cursor..cursor + nl];
            let line_end = cursor + nl + 1;
            cursor = line_end;
            if out.uploaded >= max_per_flush {
                break;
            }
            let Some(report) = report_from_line(line, device_id, client_version, os) else {
                out.skipped += 1;
                consumed = line_end;
                continue;
            };
            match self.send(&report) {
                SendResult::Uploaded => {
                    out.uploaded += 1;
                    consumed = line_end;
                }
                SendResult::Rejected | SendResult::Unauthorized => {
                    out.skipped += 1;
                    consumed = line_end;
                }
                SendResult::RateLimited => {
                    out.stopped = Some("rate_limited");
                    break;
                }
                SendResult::Unavailable => {
                    out.stopped = Some("unavailable");
                    break;
                }
            }
        }
        if consumed != watermark {
            write_watermark(&wm_path, consumed as u64);
        }
        out
    }

    fn send(&self, report: &DiagnosticReport) -> SendResult {
        let body = match serde_json::to_string(report) {
            Ok(body) => body,
            Err(_) => return SendResult::Rejected,
        };
        let token = self.access_token.as_deref().unwrap_or("");
        match self.post(&body, token) {
            // token 中途轮换了:这条报错别丢,退一次匿名。
            SendResult::Unauthorized if !token.is_empty() => self.post(&body, ""),
            other => other,
        }
    }

    fn post(&self, body: &str, token: &str) -> SendResult {
        match self.transport.request("POST", DIAGNOSTICS_PATH, token, Some(body)) {
            Ok((200..=299, _)) => SendResult::Uploaded,
            Ok((401, _)) => SendResult::Unauthorized,
            Ok((429, _)) => SendResult::RateLimited,
            Ok((400..=499, _)) => SendResult::Rejected,
            Ok(_) | Err(_) => SendResult::Unavailable,
        }
    }
}

// ---------- 日志行 → DiagnosticReport ----------

/// 解析一行 JSONL,是报错就转成上报体;不是报错、解析失败、校验不过都返回 None(调用方跳过)。
fn report_from_line(line: &[u8], device_id: &str, client_version: &str, os: &str) -> Option<DiagnosticReport> {
    let record: Value = serde_json::from_slice(line).ok()?;
    let event = record.get("event")?.as_str()?;
    let detail = record.get("detail").cloned().unwrap_or(Value::Null);
    if !is_reportable(event, &detail) {
        return None;
    }
    let occurred_at = record
        .get("timestamp_ms")
        .and_then(Value::as_u64)
        .map(epoch_ms_to_rfc3339);
    let report = DiagnosticReport {
        client_version: truncate(client_version, MAX_SHORT).to_owned(),
        os: truncate(os, MAX_SHORT).to_owned(),
        event: truncate(event, MAX_EVENT).to_owned(),
        error_code: pick_str(&detail, &["error_code", "code", "kind"]).map(|s| truncate(s, MAX_SHORT).to_owned()),
        device_id: Some(truncate(device_id, MAX_SHORT).to_owned()),
        category: Some(category(event).to_owned()),
        gateway: pick_str(&detail, &["gateway", "gateway_id", "selected_gateway"]).map(|s| truncate(s, MAX_SHORT).to_owned()),
        message: Some(truncate(&redact(&detail.to_string()), MAX_MESSAGE).to_owned()),
        occurred_at,
    };
    validate_diagnostic_report(&report).ok()?;
    Some(report)
}

fn is_reportable(event: &str, detail: &Value) -> bool {
    let lower = event.to_ascii_lowercase();
    ERROR_MARKERS.iter().any(|m| lower.contains(m))
        || detail.get("error").is_some()
        || detail.get("err").is_some()
}

/// 粗分类给服务端聚合。ponytail: 按事件名前缀猜,猜不到就 runtime。
fn category(event: &str) -> &'static str {
    let e = event.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| e.contains(w));
    if has(&["panic", "crash"]) {
        "crash"
    } else if has(&["launcher", "startup", "boot", "install"]) {
        "startup"
    } else if has(&["gateway", "connect", "network", "relay", "dns", "tls", "proxy"]) {
        "connect"
    } else if has(&["login", "auth", "token", "credential"]) {
        "auth"
    } else {
        "runtime"
    }
}

fn pick_str<'a>(detail: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| detail.get(k).and_then(Value::as_str))
}

/// 把 rct_/sk-/Bearer /token= 后面那串换成 [redacted]。只在 ASCII 上操作,多字节字符原样穿过。
pub(crate) fn redact(input: &str) -> String {
    const MARKERS: &[&str] = &["bearer ", "token=", "rct_", "sk-"];
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    'outer: while i < bytes.len() {
        for marker in MARKERS {
            let m = marker.as_bytes();
            if i + m.len() <= bytes.len() && bytes[i..i + m.len()].eq_ignore_ascii_case(m) {
                out.extend_from_slice(b"[redacted]");
                i += m.len();
                while i < bytes.len() && is_token_byte(bytes[i]) {
                    i += 1;
                }
                continue 'outer;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    // 只增删了 ASCII 段,UTF-8 结构不会坏;保险起见还是走 lossy。
    String::from_utf8_lossy(&out).into_owned()
}

fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.')
}

/// 按字节上限截断,退到字符边界,别把一个汉字劈成半个。
fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// epoch 毫秒 → RFC3339(UTC)。不引 chrono,一个 civil_from_days 就够(Howard Hinnant)。
pub(crate) fn epoch_ms_to_rfc3339(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

// ---------- 水位 ----------

fn watermark_path(log_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.uploaded", log_path.display()))
}

fn read_watermark(path: &Path) -> u64 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn write_watermark(path: &Path, value: u64) {
    // 先写临时文件再改名:半截写入读出来解析失败会归零重传,不致命但没必要。
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    if std::fs::write(&tmp, value.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{contains_secret_marker, AdapterError};
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// 记录每次 POST,并按预设顺序回状态码(用完了一直回最后一个)。
    #[derive(Clone)]
    struct FakeTransport {
        calls: std::rc::Rc<RefCell<Vec<(String, String)>>>, // (token, body)
        statuses: std::rc::Rc<RefCell<Vec<u16>>>,
    }

    impl FakeTransport {
        fn returning(statuses: &[u16]) -> Self {
            Self {
                calls: Default::default(),
                statuses: std::rc::Rc::new(RefCell::new(statuses.to_vec())),
            }
        }
    }

    impl Transport for FakeTransport {
        fn request(&self, method: &str, path: &str, token: &str, body: Option<&str>) -> Result<(u16, String), AdapterError> {
            assert_eq!((method, path), ("POST", DIAGNOSTICS_PATH));
            self.calls.borrow_mut().push((token.to_owned(), body.unwrap_or("").to_owned()));
            let mut statuses = self.statuses.borrow_mut();
            let status = if statuses.len() > 1 { statuses.remove(0) } else { statuses[0] };
            if status == 0 {
                return Err(AdapterError::Unavailable);
            }
            Ok((status, "{\"status\":\"accepted\"}".into()))
        }
    }

    fn adapter(transport: FakeTransport, token: Option<&str>) -> Adapter<FakeTransport> {
        let mut a = Adapter::new(transport, "https://api.example.test").unwrap();
        if let Some(t) = token {
            a.set_access_token(t.to_owned()).unwrap();
        }
        a
    }

    static SEQ: AtomicU64 = AtomicU64::new(0);
    fn temp_log(lines: &[&str]) -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("recodex-diagflush-{}-{n}.log", std::process::id()));
        let mut body = lines.join("\n");
        if !lines.is_empty() {
            body.push('\n');
        }
        std::fs::write(&path, body).unwrap();
        path
    }

    const OK_LINE: &str = r#"{"timestamp_ms":1756944000000,"pid":1,"event":"launcher.step","detail":{"step":"spawn"}}"#;
    const FAIL_LINE: &str = r#"{"timestamp_ms":1756944000000,"pid":1,"event":"launcher.spawn_failed","detail":{"error":"exit 1","gateway":"jp"}}"#;
    const ERRKEY_LINE: &str = r#"{"timestamp_ms":1756944000000,"pid":1,"event":"relay.status","detail":{"error":"refused"}}"#;

    #[test]
    fn uploads_only_error_lines_and_advances_watermark() {
        let log = temp_log(&[OK_LINE, FAIL_LINE, OK_LINE, ERRKEY_LINE]);
        let t = FakeTransport::returning(&[202]);
        let out = adapter(t.clone(), Some("rct_abcdefgh")).flush_diagnostic_log(&log, "desktop-x", "1.0.0", 20);
        assert_eq!(out, FlushOutcome { uploaded: 2, skipped: 2, stopped: None });
        let calls = t.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "rct_abcdefgh", "有 token 走已认证");
        let body: Value = serde_json::from_str(&calls[0].1).unwrap();
        assert_eq!(body["event"], "launcher.spawn_failed");
        assert_eq!(body["category"], "startup");
        assert_eq!(body["gateway"], "jp");
        assert_eq!(body["device_id"], "desktop-x");
        assert_eq!(body["occurred_at"], "2025-09-04T00:00:00Z");
        // 水位推到文件末尾
        assert_eq!(read_watermark(&watermark_path(&log)), std::fs::metadata(&log).unwrap().len());
        // 再跑一次:没新东西,什么都不发
        let out2 = adapter(t.clone(), Some("rct_abcdefgh")).flush_diagnostic_log(&log, "desktop-x", "1.0.0", 20);
        assert_eq!(out2, FlushOutcome::default());
        assert_eq!(t.calls.borrow().len(), 2);
    }

    #[test]
    fn anonymous_when_no_token_sends_without_authorization() {
        let log = temp_log(&[FAIL_LINE]);
        let t = FakeTransport::returning(&[202]);
        adapter(t.clone(), None).flush_diagnostic_log(&log, "desktop-anon", "1.0.0", 20);
        let calls = t.calls.borrow();
        assert_eq!(calls[0].0, "", "没 token 必须不带 Authorization(服务端按匿名收)");
        assert!(calls[0].1.contains("\"device_id\":\"desktop-anon\""));
    }

    #[test]
    fn stale_token_falls_back_to_anonymous_once() {
        let log = temp_log(&[FAIL_LINE]);
        let t = FakeTransport::returning(&[401, 202]);
        let out = adapter(t.clone(), Some("rct_stale123")).flush_diagnostic_log(&log, "d", "1.0.0", 20);
        assert_eq!(out.uploaded, 1);
        let calls = t.calls.borrow();
        assert_eq!((calls[0].0.as_str(), calls[1].0.as_str()), ("rct_stale123", ""), "401 后退匿名重发");
    }

    #[test]
    fn rate_limit_stops_and_keeps_watermark_at_last_success() {
        let log = temp_log(&[FAIL_LINE, ERRKEY_LINE, FAIL_LINE]);
        let t = FakeTransport::returning(&[202, 429]);
        let out = adapter(t.clone(), None).flush_diagnostic_log(&log, "d", "1.0.0", 20);
        assert_eq!(out.uploaded, 1);
        assert_eq!(out.stopped, Some("rate_limited"));
        let first_line_len = (FAIL_LINE.len() + 1) as u64;
        assert_eq!(read_watermark(&watermark_path(&log)), first_line_len, "水位停在最后一条成功的行尾");
    }

    #[test]
    fn network_down_stops_without_advancing() {
        let log = temp_log(&[FAIL_LINE]);
        let t = FakeTransport::returning(&[0]); // 0 = transport error
        let out = adapter(t, None).flush_diagnostic_log(&log, "d", "1.0.0", 20);
        assert_eq!(out.stopped, Some("unavailable"));
        assert_eq!(read_watermark(&watermark_path(&log)), 0);
    }

    #[test]
    fn compacted_log_resets_watermark() {
        let log = temp_log(&[FAIL_LINE]);
        write_watermark(&watermark_path(&log), 999_999); // 老水位远大于文件
        let t = FakeTransport::returning(&[202]);
        let out = adapter(t, None).flush_diagnostic_log(&log, "d", "1.0.0", 20);
        assert_eq!(out.uploaded, 1, "水位归零后从头重传");
    }

    #[test]
    fn max_per_flush_caps_and_leaves_rest_for_next_round() {
        let log = temp_log(&[FAIL_LINE, FAIL_LINE, FAIL_LINE]);
        let t = FakeTransport::returning(&[202]);
        let out = adapter(t.clone(), None).flush_diagnostic_log(&log, "d", "1.0.0", 2);
        assert_eq!(out.uploaded, 2);
        let out2 = adapter(t, None).flush_diagnostic_log(&log, "d", "1.0.0", 2);
        assert_eq!(out2.uploaded, 1, "剩下那条下一轮补上");
    }

    #[test]
    fn redacts_secrets_before_sending() {
        let line = r#"{"timestamp_ms":0,"pid":1,"event":"auth.refresh_failed","detail":{"error":"Bearer rct_AAA.bbb rejected, token=xyz sk-123"}}"#;
        let log = temp_log(&[line]);
        let t = FakeTransport::returning(&[202]);
        let out = adapter(t.clone(), None).flush_diagnostic_log(&log, "d", "1.0.0", 20);
        assert_eq!(out.uploaded, 1, "带密钥的报错必须 redact 后照传,不能丢");
        let body = &t.calls.borrow()[0].1;
        assert!(!contains_secret_marker(body), "发出去的 body 里不能再有密钥标记: {body}");
        assert!(body.contains("[redacted]"));
    }

    #[test]
    fn helpers() {
        assert_eq!(epoch_ms_to_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(epoch_ms_to_rfc3339(1_700_000_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(redact("x Bearer abc.def y"), "x [redacted] y");
        assert_eq!(redact("汉字 rct_k1 混排"), "汉字 [redacted] 混排");
        assert_eq!(truncate("汉字汉字", 4), "汉");
        assert_eq!(category("gateway.connect_timeout"), "connect");
        assert_eq!(category("app.panic"), "crash");
        assert!(is_reportable("x.failed", &Value::Null));
        assert!(is_reportable("x.ok", &serde_json::json!({"error": "e"})));
        assert!(!is_reportable("x.ok", &Value::Null));
    }
}
