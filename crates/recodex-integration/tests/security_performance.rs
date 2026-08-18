use recodex_integration::{Adapter, AdapterError, Gateway, Transport};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct FailingTransport;

impl Transport for FailingTransport {
    fn request(
        &self,
        _method: &str,
        _path: &str,
        _access_token: &str,
        _body: Option<&str>,
    ) -> Result<(u16, String), AdapterError> {
        Err(AdapterError::Unavailable)
    }
}

#[test]
fn transport_errors_do_not_expose_access_tokens() {
    let token = "rct_super_secret_token";
    let mut adapter = Adapter::new(FailingTransport, "https://api.example").unwrap();
    adapter.set_access_token(token.into()).unwrap();
    let message = adapter.account().unwrap_err().to_string();
    assert!(!message.contains(token));
}

#[test]
fn fastest_gateway_selection_is_bounded_for_large_lists() {
    let adapter = Adapter::new(FailingTransport, "https://api.example").unwrap();
    let gateways: Vec<_> = (0..10_000)
        .map(|index| Gateway {
            id: index.to_string(),
            name: format!("gateway-{index}"),
            endpoint: format!("https://gateway-{index}.example"),
            enabled: true,
            maintenance: false,
            client_latency_ms: Some(10_000 - index),
            healthy: true,
            selected: false,
        })
        .collect();
    let start = Instant::now();
    let fastest = adapter.fastest_gateway(&gateways).unwrap();
    assert_eq!(fastest.client_latency_ms, Some(1));
    assert!(start.elapsed() < Duration::from_millis(100));
}

#[derive(Clone)]
struct SharedTransport {
    response: Arc<Mutex<Result<(u16, String), AdapterError>>>,
}

impl Transport for SharedTransport {
    fn request(
        &self,
        _method: &str,
        _path: &str,
        _access_token: &str,
        _body: Option<&str>,
    ) -> Result<(u16, String), AdapterError> {
        self.response.lock().unwrap().clone()
    }
}

#[test]
fn fork_cache_merge_requires_the_same_authenticated_session() {
    let response = Arc::new(Mutex::new(Ok((200, r#"{"account_type":"shared","available":8,"total":10,"used":2,"refreshed_at":"now","source":"provider","stale":false}"#.into()))));
    let mut adapter = Adapter::new(
        SharedTransport {
            response: response.clone(),
        },
        "https://api.example",
    )
    .unwrap();
    adapter.set_access_token("rct_first_token".into()).unwrap();

    let mut fork = adapter.fork();
    fork.usage(true).unwrap();
    *response.lock().unwrap() = Err(AdapterError::Unavailable);

    adapter.clear_access_token();
    adapter.set_access_token("rct_second_token".into()).unwrap();
    adapter.merge_cache_from(&fork);
    assert_eq!(adapter.usage(false).unwrap_err(), AdapterError::Unavailable);

    adapter.clear_access_token();
    adapter.set_access_token("rct_first_token".into()).unwrap();
    adapter.merge_cache_from(&fork);
    assert!(adapter.usage(false).unwrap().stale);
}

#[test]
fn isolated_usage_request_returns_the_worker_that_owns_the_fresh_cache() {
    let response = Arc::new(Mutex::new(Ok((200, r#"{"account_type":"shared","available":8,"total":10,"used":2,"refreshed_at":"now","source":"provider","stale":false}"#.into()))));
    let mut adapter = Adapter::new(
        SharedTransport {
            response: response.clone(),
        },
        "https://api.example",
    )
    .unwrap();
    adapter.set_access_token("rct_first_token".into()).unwrap();

    let (usage, usage_worker) = adapter.usage_in_fork(true);
    assert_eq!(usage.unwrap().available, 8.0);
    adapter.merge_cache_from(&usage_worker);
    *response.lock().unwrap() = Err(AdapterError::Unavailable);

    assert!(adapter.usage(false).unwrap().stale);
}
