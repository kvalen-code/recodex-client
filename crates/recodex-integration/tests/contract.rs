use recodex_integration::{
    check_compatibility, Adapter, AdapterError, Gateway, HttpTransport, PublicLoginStart, Transport,
};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::time::Duration;

#[derive(Clone)]
struct RefreshFallbackTransport {
    calls: Rc<RefCell<usize>>,
}

impl Transport for RefreshFallbackTransport {
    fn request(
        &self,
        _method: &str,
        _path: &str,
        _access_token: &str,
        _body: Option<&str>,
    ) -> Result<(u16, String), AdapterError> {
        let mut calls = self.calls.borrow_mut();
        *calls += 1;
        if *calls == 1 {
            Ok((200, r#"{"account_type":"shared","available":8,"total":10,"used":2,"refreshed_at":"now","source":"provider","stale":false}"#.into()))
        } else {
            Err(AdapterError::Unavailable)
        }
    }
}

struct FakeTransport {
    calls: RefCell<Vec<String>>,
    response: (u16, String),
}
impl Transport for FakeTransport {
    fn request(
        &self,
        method: &str,
        path: &str,
        _access_token: &str,
        _body: Option<&str>,
    ) -> Result<(u16, String), AdapterError> {
        self.calls.borrow_mut().push(format!("{method} {path}"));
        Ok(self.response.clone())
    }
}

struct QueueTransport {
    calls: Rc<RefCell<Vec<String>>>,
    responses: RefCell<VecDeque<(u16, String)>>,
}

impl QueueTransport {
    fn new(responses: Vec<(u16, String)>) -> Self {
        Self {
            calls: Rc::new(RefCell::new(Vec::new())),
            responses: RefCell::new(responses.into()),
        }
    }
}

impl Transport for QueueTransport {
    fn request(
        &self,
        method: &str,
        path: &str,
        _access_token: &str,
        _body: Option<&str>,
    ) -> Result<(u16, String), AdapterError> {
        self.calls.borrow_mut().push(format!("{method} {path}"));
        self.responses
            .borrow_mut()
            .pop_front()
            .ok_or(AdapterError::Unavailable)
    }
}

#[test]
fn rejects_http_and_missing_token() {
    assert!(Adapter::new(
        FakeTransport {
            calls: RefCell::new(vec![]),
            response: (200, "{}".into())
        },
        "http://api.example"
    )
    .is_err());
    let adapter = Adapter::new(
        FakeTransport {
            calls: RefCell::new(vec![]),
            response: (200, "{}".into()),
        },
        "https://api.example",
    )
    .unwrap();
    assert_eq!(adapter.account().unwrap_err(), AdapterError::Unauthorized);
}

#[test]
fn conflict_errors_do_not_expose_upstream_response_body() {
    let response = (409, r#"{"error":"rct_secret_should_not_escape"}"#.into());
    let transport = FakeTransport {
        calls: RefCell::new(vec![]),
        response,
    };
    let mut adapter = Adapter::new(transport, "https://api.example").unwrap();
    adapter.set_access_token("rct_valid_token".into()).unwrap();
    let error = adapter.account().unwrap_err().to_string();
    assert!(error.contains("conflicts with current ReCodex state"));
    assert!(!error.contains("rct_secret_should_not_escape"));
}

#[test]
fn rejects_credentialed_or_ambiguous_api_urls() {
    for endpoint in [
        "https://",
        "https://user:pass@api.example",
        "https://api.example?token=secret",
        "https://api.example#fragment",
        "http://192.0.2.1",
    ] {
        assert!(
            Adapter::new(
                FakeTransport {
                    calls: RefCell::new(vec![]),
                    response: (200, "{}".into()),
                },
                endpoint,
            )
            .is_err(),
            "accepted unsafe endpoint {endpoint}"
        );
    }
}

#[test]
fn refreshes_usage_and_selects_fastest_gateway() {
    let transport = FakeTransport { calls: RefCell::new(vec![]), response: (200, r#"{"account_type":"shared","available":8,"total":10,"used":2,"refreshed_at":"now","source":"recodex-realtime","stale":false}"#.into()) };
    let mut adapter = Adapter::new(transport, "https://api.example").unwrap();
    adapter.set_access_token("rct_test_token".into()).unwrap();
    let usage = adapter.usage(true).unwrap();
    assert_eq!(usage.available, 8.0);
    let gateways = vec![
        Gateway {
            id: "slow".into(),
            name: "Slow".into(),
            endpoint: "https://slow".into(),
            enabled: true,
            maintenance: false,
            client_latency_ms: Some(80),
            healthy: true,
            selected: false,
        },
        Gateway {
            id: "fast".into(),
            name: "Fast".into(),
            endpoint: "https://fast".into(),
            enabled: true,
            maintenance: false,
            client_latency_ms: Some(12),
            healthy: true,
            selected: false,
        },
    ];
    assert_eq!(adapter.fastest_gateway(&gateways).unwrap().id, "fast");
}

#[test]
fn failed_forced_refresh_returns_stale_cached_usage_with_safe_error() {
    let mut adapter = Adapter::new(
        RefreshFallbackTransport {
            calls: Rc::new(RefCell::new(0)),
        },
        "https://api.example",
    )
    .unwrap();
    adapter
        .set_access_token("rct_refresh_token".into())
        .unwrap();
    assert_eq!(adapter.usage(true).unwrap().available, 8.0);

    let stale = adapter.usage(true).unwrap();
    assert!(stale.stale);
    assert_eq!(stale.available, 8.0);
    let refresh_error = stale.refresh_error.expect("refresh error is required");
    assert_eq!(refresh_error.code, "refresh_unavailable");
    assert_eq!(
        refresh_error.message,
        "latest usage could not be synchronized"
    );
}

#[test]
fn malformed_forced_refresh_returns_stale_cached_usage_with_safe_error() {
    let mut adapter = Adapter::new(
        QueueTransport::new(vec![
            (
                200,
                r#"{"account_type":"shared","available":8,"total":10,"used":2,"refreshed_at":"now","source":"provider","stale":false}"#.into(),
            ),
            (200, "{invalid usage payload".into()),
        ]),
        "https://api.example",
    )
    .unwrap();
    adapter
        .set_access_token("rct_refresh_token".into())
        .unwrap();
    assert_eq!(adapter.usage(true).unwrap().available, 8.0);

    let stale = adapter.usage(true).unwrap();
    assert!(stale.stale);
    assert_eq!(stale.available, 8.0);
    assert_eq!(
        stale.refresh_error.expect("refresh error is required").code,
        "refresh_unavailable"
    );
}

#[test]
fn semantically_invalid_forced_refresh_returns_stale_cached_usage() {
    let mut adapter = Adapter::new(
        QueueTransport::new(vec![
            (
                200,
                r#"{"account_type":"shared","available":8,"total":10,"used":2,"refreshed_at":"now","source":"provider","stale":false}"#.into(),
            ),
            (
                200,
                r#"{"account_type":"shared","available":11,"total":10,"used":2,"refreshed_at":"","source":"","stale":false}"#.into(),
            ),
        ]),
        "https://api.example",
    )
    .unwrap();
    adapter
        .set_access_token("rct_semantic_refresh_token".into())
        .unwrap();
    assert_eq!(adapter.usage(true).unwrap().available, 8.0);

    let stale = adapter.usage(true).unwrap();
    assert!(stale.stale);
    assert_eq!(stale.available, 8.0);
    assert_eq!(
        stale.refresh_error.expect("refresh error is required").code,
        "refresh_unavailable"
    );
}

#[test]
fn forced_refresh_never_reuses_usage_cached_under_another_token() {
    let mut adapter = Adapter::new(
        RefreshFallbackTransport {
            calls: Rc::new(RefCell::new(0)),
        },
        "https://api.example",
    )
    .unwrap();
    adapter.set_access_token("rct_first_token".into()).unwrap();
    adapter.usage(true).unwrap();
    adapter.set_access_token("rct_second_token".into()).unwrap();

    assert_eq!(adapter.usage(true).unwrap_err(), AdapterError::Unavailable);
}

#[test]
fn forced_refresh_does_not_hide_authentication_errors_with_cache() {
    #[derive(Clone)]
    struct AuthFailureAfterSuccess(Rc<RefCell<usize>>);
    impl Transport for AuthFailureAfterSuccess {
        fn request(
            &self,
            _method: &str,
            _path: &str,
            _access_token: &str,
            _body: Option<&str>,
        ) -> Result<(u16, String), AdapterError> {
            let mut calls = self.0.borrow_mut();
            *calls += 1;
            if *calls == 1 {
                Ok((200, r#"{"account_type":"shared","available":8,"total":10,"used":2,"refreshed_at":"now","source":"provider","stale":false}"#.into()))
            } else {
                Ok((401, "{}".into()))
            }
        }
    }

    let mut adapter = Adapter::new(
        AuthFailureAfterSuccess(Rc::new(RefCell::new(0))),
        "https://api.example",
    )
    .unwrap();
    adapter.set_access_token("rct_auth_token".into()).unwrap();
    adapter.usage(true).unwrap();
    assert_eq!(adapter.usage(true).unwrap_err(), AdapterError::Unauthorized);
}

#[test]
fn compatibility_is_explicit() {
    assert!(check_compatibility("1.2.47", "1.2.0").unwrap().supported);
    assert!(!check_compatibility("1.1.0", "1.2.0").unwrap().supported);
    assert!(check_compatibility("1.two.0", "1.2.0").is_err());
    assert!(check_compatibility("1.2.999999999999999999999999", "1.2.0").is_err());
}

#[test]
fn rejects_untrusted_gateway_payloads_before_use() {
    let transport = FakeTransport {
        calls: RefCell::new(vec![]),
        response: (
            200,
            r#"{"gateways":[{"id":"bad\nname","name":"Bad","endpoint":"https://gateway.example","enabled":true,"maintenance":false,"healthy":true,"selected":false}]}"#.into(),
        ),
    };
    let mut adapter = Adapter::new(transport, "https://api.example").unwrap();
    adapter
        .set_access_token("rct_gateway_token".into())
        .unwrap();
    assert!(matches!(
        adapter.gateways(),
        Err(AdapterError::InvalidResponse(_))
    ));
}

#[test]
fn device_login_promotes_approved_token_into_adapter() {
    let transport = FakeTransport {
        calls: RefCell::new(vec![]),
        response: (
            200,
            r#"{"status":"approved","token":"rct_approved_token","gateway_url":"https://gw.example"}"#.into(),
        ),
    };
    let mut adapter = Adapter::new(transport, "https://api.example").unwrap();
    let poll = adapter.poll_login("device-code").unwrap();
    assert_eq!(poll.status, "approved");
    let serialized = serde_json::to_string(&poll).unwrap();
    assert!(!serialized.contains("rct_approved_token"));
    assert!(!serialized.contains("\"token\""));
}

#[test]
fn device_login_rejects_unsafe_verification_urls() {
    let transport = FakeTransport {
        calls: RefCell::new(vec![]),
        response: (
            200,
            r#"{"device_code":"secret","user_code":"ABCD","verify_url":"file:///tmp/fake-login.html","interval_sec":5,"expires_in":600}"#.into(),
        ),
    };
    let adapter = Adapter::new(transport, "https://api.example").unwrap();
    assert!(matches!(
        adapter.start_login("desktop-1", "Desktop", "1.2.47", "windows"),
        Err(AdapterError::InvalidResponse(_))
    ));
}

#[test]
fn device_login_rejects_unsafe_gateway_urls() {
    let transport = FakeTransport {
        calls: RefCell::new(vec![]),
        response: (
            200,
            r#"{"status":"pending","gateway_url":"http://evil.example"}"#.into(),
        ),
    };
    let mut adapter = Adapter::new(transport, "https://api.example").unwrap();
    assert!(matches!(
        adapter.poll_login("device-code"),
        Err(AdapterError::InvalidResponse(_))
    ));
}

#[test]
fn desktop_logout_revokes_the_server_session() {
    #[derive(Clone)]
    struct RecordingTransport(Rc<RefCell<Vec<String>>>);
    impl Transport for RecordingTransport {
        fn request(
            &self,
            method: &str,
            path: &str,
            _access_token: &str,
            _body: Option<&str>,
        ) -> Result<(u16, String), AdapterError> {
            self.0.borrow_mut().push(format!("{method} {path}"));
            Ok((200, r#"{"status":"ok"}"#.into()))
        }
    }
    let calls = Rc::new(RefCell::new(vec![]));
    let transport = RecordingTransport(calls.clone());
    let mut adapter = Adapter::new(transport, "https://api.example").unwrap();
    adapter.set_access_token("rct_logout_token".into()).unwrap();
    adapter.revoke_session().unwrap();
    // 必须是 /api/cli/auth/：sub2api 自己也有 /api/v1/auth/logout（网页会话），
    // 反代不把 /api/v1/auth/* 分给 recodex-auth（那会抢掉网页登录），
    // 走 v1 的话这个请求静默落到 sub2api —— 服务端设备根本没被撤销。
    assert_eq!(*calls.borrow(), vec!["POST /api/cli/auth/logout"]);
}

#[test]
fn use_fastest_tests_authorized_gateways_before_selecting() {
    #[derive(Clone)]
    struct SequencedTransport {
        calls: Rc<RefCell<Vec<String>>>,
        responses: Rc<RefCell<VecDeque<(u16, String)>>>,
    }
    impl Transport for SequencedTransport {
        fn request(
            &self,
            method: &str,
            path: &str,
            _token: &str,
            _body: Option<&str>,
        ) -> Result<(u16, String), AdapterError> {
            self.calls.borrow_mut().push(format!("{method} {path}"));
            self.responses
                .borrow_mut()
                .pop_front()
                .ok_or(AdapterError::Unavailable)
        }
    }
    let calls = Rc::new(RefCell::new(vec![]));
    let responses = Rc::new(RefCell::new(VecDeque::from([
        (200, r#"{"gateways":[{"id":"slow","name":"Slow","endpoint":"https://slow","enabled":true,"maintenance":false,"client_latency_ms":80,"healthy":true,"selected":false},{"id":"fast","name":"Fast","endpoint":"https://fast","enabled":true,"maintenance":false,"client_latency_ms":12,"healthy":true,"selected":false}]}"#.into()),
        (200, r#"{"id":"fast","name":"Fast","endpoint":"https://fast","enabled":true,"maintenance":false,"client_latency_ms":12,"healthy":true,"selected":true}"#.into()),
    ])));
    let transport = SequencedTransport {
        calls: calls.clone(),
        responses,
    };
    let mut adapter = Adapter::new(transport, "https://api.example").unwrap();
    adapter
        .set_access_token("rct_fastest_token".into())
        .unwrap();
    assert_eq!(adapter.use_fastest_gateway().unwrap().id, "fast");
    assert_eq!(
        *calls.borrow(),
        vec!["POST /api/v1/gateways/test", "POST /api/v1/gateways/select"]
    );
}

#[test]
fn public_login_start_never_serializes_the_device_code() {
    let login = recodex_integration::LoginStart {
        device_code: "secret-device-code".into(),
        user_code: "ABCD-1234".into(),
        verify_url: "https://console.recodex.ai/device".into(),
        interval_sec: 5,
        expires_in: 600,
    };

    let serialized = serde_json::to_string(&PublicLoginStart::from(&login)).unwrap();
    assert!(!serialized.contains("device_code"));
    assert!(!serialized.contains("secret-device-code"));
    assert!(serialized.contains("ABCD-1234"));
    assert!(serialized.contains("verify_url"));
}

#[test]
fn http_transport_rejects_oversized_response_bodies() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 2048];
        let _ = stream.read(&mut request);
        let payload = vec![b'x'; 1024 * 1024 + 1];
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            payload.len()
        )
        .unwrap();
        stream.write_all(&payload).unwrap();
    });

    let transport =
        HttpTransport::new(&format!("http://{address}"), Duration::from_secs(2)).unwrap();
    let error = transport
        .request("GET", "/oversized", "", None)
        .unwrap_err();
    server.join().unwrap();
    assert_eq!(
        error,
        AdapterError::InvalidResponse("response body exceeds 1048576 bytes".into())
    );
}

#[test]
fn http_transport_does_not_follow_redirects() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use std::time::{Duration, Instant};

    let target_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    target_listener.set_nonblocking(true).unwrap();
    let target_address = target_listener.local_addr().unwrap();
    let target_was_accessed = Arc::new(AtomicBool::new(false));
    let target_was_accessed_by_server = Arc::clone(&target_was_accessed);
    let target = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_millis(100);
        loop {
            match target_listener.accept() {
                Ok((mut stream, _)) => {
                    target_was_accessed_by_server.store(true, Ordering::SeqCst);
                    let mut request = [0_u8; 2048];
                    let _ = stream.read(&mut request);
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                        )
                        .unwrap();
                    return;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("target accept failed: {error}"),
            }
        }
    });

    let origin_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let origin_address = origin_listener.local_addr().unwrap();
    let origin = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = origin_listener.accept().unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request);
            write!(
                stream,
                "HTTP/1.1 302 Found\r\nLocation: http://{target_address}/redirected\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        }
    });

    let transport =
        HttpTransport::new(&format!("http://{origin_address}"), Duration::from_secs(2)).unwrap();
    assert_eq!(
        transport
            .request("GET", "/account", "rct_test_token", None)
            .unwrap(),
        (302, String::new())
    );
    let mut adapter = Adapter::new(transport, &format!("http://{origin_address}")).unwrap();
    adapter.set_access_token("rct_test_token".into()).unwrap();
    assert_eq!(adapter.account().unwrap_err(), AdapterError::Unavailable);
    origin.join().unwrap();
    target.join().unwrap();
    assert!(!target_was_accessed.load(Ordering::SeqCst));
}

#[test]
fn oversized_successful_usage_refresh_returns_stale_cache() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn read_request(stream: &mut std::net::TcpStream) {
        let mut request = [0; 1024];
        stream.read(&mut request).unwrap();
    }

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut fresh, _) = listener.accept().unwrap();
        read_request(&mut fresh);
        let body = r#"{"account_type":"shared","available":8,"total":10,"used":2,"refreshed_at":"now","source":"provider","stale":false}"#;
        write!(
            fresh,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();

        let (mut oversized, _) = listener.accept().unwrap();
        read_request(&mut oversized);
        let body = vec![b' '; 1_048_577];
        write!(
            oversized,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        oversized.write_all(&body).unwrap();
    });

    let transport =
        HttpTransport::new(&format!("http://{address}"), Duration::from_secs(2)).unwrap();
    let mut adapter = Adapter::new(transport, &format!("http://{address}")).unwrap();
    adapter
        .set_access_token("rct_oversized_token".into())
        .unwrap();
    assert_eq!(adapter.usage(true).unwrap().available, 8.0);

    let stale = adapter.usage(true).unwrap();
    assert!(stale.stale);
    assert_eq!(stale.available, 8.0);
    assert_eq!(
        stale.refresh_error.expect("refresh error is required").code,
        "refresh_unavailable"
    );
    server.join().unwrap();
}

#[test]
fn usage_refresh_preserves_actionable_status_when_error_body_is_not_utf8() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn read_request(stream: &mut std::net::TcpStream) {
        let mut request = [0; 1024];
        stream.read(&mut request).unwrap();
    }

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut direct, _) = listener.accept().unwrap();
        read_request(&mut direct);
        write!(
            direct,
            "HTTP/1.1 401 ReCodex error\r\nContent-Length: 1\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        direct.write_all(&[0xff]).unwrap();

        for status in [401, 403, 409, 429] {
            let (mut fresh, _) = listener.accept().unwrap();
            read_request(&mut fresh);
            let body = r#"{"account_type":"shared","available":8,"total":10,"used":2,"refreshed_at":"now","source":"provider","stale":false}"#;
            write!(
                fresh,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();

            let (mut failure, _) = listener.accept().unwrap();
            read_request(&mut failure);
            write!(
                failure,
                "HTTP/1.1 {status} ReCodex error\r\nContent-Length: 1\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            failure.write_all(&[0xff]).unwrap();
        }
    });

    let direct = HttpTransport::new(&format!("http://{address}"), Duration::from_secs(2)).unwrap();
    assert_eq!(
        direct.request("GET", "/", "", None).unwrap(),
        (401, String::new())
    );

    for (index, (status, expected)) in [
        (401, AdapterError::Unauthorized),
        (403, AdapterError::Forbidden),
        (
            409,
            AdapterError::Conflict("request conflicts with current ReCodex state".into()),
        ),
        (429, AdapterError::RateLimited),
    ]
    .into_iter()
    .enumerate()
    {
        let transport =
            HttpTransport::new(&format!("http://{address}"), Duration::from_secs(2)).unwrap();
        let mut adapter = Adapter::new(transport, &format!("http://{address}")).unwrap();
        adapter
            .set_access_token(format!("rct_status_{index}_token"))
            .unwrap();
        adapter.usage(true).unwrap();
        assert_eq!(
            adapter.usage(true).unwrap_err(),
            expected,
            "status {status}"
        );
    }
    server.join().unwrap();
}

#[test]
fn compatibility_update_and_diagnostics_use_the_v1_client_contract() {
    let transport = QueueTransport::new(vec![
        (
            200,
            r#"{"client_version":"1.2.47","supported":true,"minimum_version":"1.2.47"}"#.into(),
        ),
        (
            200,
            r#"{"channel":"stable","available":false,"latest_version":"","manifest_url":"","reason":"not_configured"}"#.into(),
        ),
        (202, r#"{"status":"accepted"}"#.into()),
    ]);
    let calls = transport.calls.clone();
    let mut adapter = Adapter::new(transport, "https://api.example").unwrap();
    adapter
        .set_access_token("rct_client_contract".into())
        .unwrap();

    let compatibility = adapter.compatibility("1.2.47").unwrap();
    assert!(compatibility.supported);
    assert_eq!(compatibility.minimum_version, "1.2.47");
    let channel = adapter.update_channel("stable").unwrap();
    assert!(!channel.available);
    assert_eq!(channel.reason.as_deref(), Some("not_configured"));
    let accepted = adapter
        .report_diagnostic(&recodex_integration::DiagnosticReport {
            client_version: "1.2.47".into(),
            os: "windows".into(),
            event: "manual_report".into(),
            error_code: None,
            device_id: None,
            category: None,
            gateway: None,
            message: None,
            occurred_at: None,
        })
        .unwrap();
    assert_eq!(accepted.status, "accepted");

    assert_eq!(
        calls.borrow().as_slice(),
        [
            "GET /api/v1/client?version=1.2.47",
            "GET /api/v1/client/update-channel?channel=stable",
            "POST /api/v1/diagnostics",
        ]
    );
}

#[test]
fn compatibility_uses_a_public_request_and_rejects_mismatched_client_version() {
    let transport = QueueTransport::new(vec![(
        200,
        r#"{"client_version":"1.2.48","supported":true,"minimum_version":"1.2.47"}"#.into(),
    )]);
    let calls = transport.calls.clone();
    let adapter = Adapter::new(transport, "https://api.example").unwrap();

    let error = adapter.compatibility("1.2.47").unwrap_err();

    assert!(matches!(error, AdapterError::InvalidResponse(_)));
    assert_eq!(
        calls.borrow().as_slice(),
        ["GET /api/v1/client?version=1.2.47"]
    );
}

#[test]
fn diagnostics_reject_secrets_before_transport() {
    let transport = QueueTransport::new(vec![]);
    let calls = transport.calls.clone();
    let mut adapter = Adapter::new(transport, "https://api.example").unwrap();
    adapter
        .set_access_token("rct_client_contract".into())
        .unwrap();

    let error = adapter
        .report_diagnostic(&recodex_integration::DiagnosticReport {
            client_version: "1.2.47".into(),
            os: "windows".into(),
            event: "Bearer rct_secret".into(),
            error_code: None,
            device_id: None,
            category: None,
            gateway: None,
            message: None,
            occurred_at: None,
        })
        .unwrap_err();
    assert!(matches!(error, AdapterError::InvalidConfiguration(_)));
    assert!(calls.borrow().is_empty());
}

#[test]
fn refresh_token_rotates_the_adapter_credential_without_serializing_it() {
    #[derive(Clone)]
    struct RefreshTransport(Rc<RefCell<Vec<String>>>);
    impl Transport for RefreshTransport {
        fn request(
            &self,
            method: &str,
            path: &str,
            _access_token: &str,
            _body: Option<&str>,
        ) -> Result<(u16, String), AdapterError> {
            self.0.borrow_mut().push(format!("{method} {path}"));
            Ok((200, r#"{"token":"rct_refreshed_token"}"#.into()))
        }
    }
    let calls = Rc::new(RefCell::new(vec![]));
    let transport = RefreshTransport(calls.clone());
    let mut adapter = Adapter::new(transport, "https://api.example").unwrap();
    adapter
        .set_access_token("rct_original_token".into())
        .unwrap();
    adapter.refresh_token().unwrap();
    assert!(adapter.is_authenticated());
    // 同上：走 v1 的话刷新打在 sub2api 的网页会话刷新上，令牌永远不会真的轮换。
    assert_eq!(calls.borrow().as_slice(), ["POST /api/cli/auth/refresh"]);
}

#[test]
fn device_poll_maps_expired_and_unknown_codes_to_stable_errors() {
    for (status, expected) in [
        (400, AdapterError::DeviceCodeUnknown),
        (410, AdapterError::DeviceCodeExpired),
        (500, AdapterError::ServiceUnavailable),
    ] {
        let transport = FakeTransport {
            calls: RefCell::new(vec![]),
            response: (status, "{}".into()),
        };
        let mut adapter = Adapter::new(transport, "https://api.example").unwrap();
        assert_eq!(adapter.poll_login("device-code").unwrap_err(), expected);
    }
}

#[test]
fn update_channel_rejects_an_unsafe_manifest_url_from_the_server() {
    let transport = FakeTransport {
        calls: RefCell::new(vec![]),
        response: (
            200,
            r#"{"channel":"stable","available":true,"latest_version":"1.2.48","manifest_url":"http://evil.example/manifest.json"}"#.into(),
        ),
    };
    let mut adapter = Adapter::new(transport, "https://api.example").unwrap();
    adapter
        .set_access_token("rct_client_contract".into())
        .unwrap();
    assert!(matches!(
        adapter.update_channel("stable"),
        Err(AdapterError::InvalidResponse(_))
    ));
}
