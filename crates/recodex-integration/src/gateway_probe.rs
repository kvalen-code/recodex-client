//! 在**本机**测各网关的延迟。
//!
//! 为什么不能用服务端给的 `client_latency_ms`：那个数字是 recodex-auth（在香港）
//! 探测各网关得到的往返，跟用户所在网络毫无关系，字段名却带着 client 前缀。
//! 线上实测同一批网关：后台显示新加坡 30ms「最快」，而从国内测是 230ms、40% 丢包，
//! 日本反而只要 75ms —— 面板上点「用最快网关」于是把用户分到了对他最差的那条线
//! （2026-09-05 的客户就这么被钉住，24 小时内 111 次连接中断有 91 次来自那条线）。
//!
//! 分工：服务端说了算的是「哪些网关可用」（启用、非维护、它自己探得通），
//! 本机说了算的是「哪个对我最快」—— 后者服务端根本测不出来。

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::Gateway;

/// 单个网关的探测预算。跨境链路光 TLS 握手就可能几百毫秒，给太短会把
/// 能用但慢的网关误判成不可达。
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);

/// 一个网关在本机的实测结果。
#[derive(Debug, Clone)]
pub struct GatewayProbe {
    pub gateway: Gateway,
    pub reachable: bool,
    pub latency_ms: u128,
}

/// 并发探测所有网关，返回按「可达优先、延迟升序」排好的结果。
///
/// 必须并发：网关多了串行探，最慢的那个会把整个操作拖到用户以为卡死。
/// 用标准库线程而不是引 rayon/tokio —— 这里只是几个一次性的 HTTP 请求。
pub fn probe_gateways(gateways: &[Gateway]) -> Vec<GatewayProbe> {
    let (tx, rx) = mpsc::channel();
    let mut spawned = 0usize;

    for gateway in gateways {
        let owned = gateway.clone();
        let tx_thread = tx.clone();
        let name = format!("gw-probe-{}", owned.id);
        // 交给线程的那份先克隆出来；线程起不来时还要用原件同步探一次。
        let for_thread = owned.clone();
        match thread::Builder::new().name(name).spawn(move || {
            let _ = tx_thread.send(probe_one(for_thread));
        }) {
            Ok(_) => spawned += 1,
            // 线程起不来（系统资源紧张）不该让整个选路失败：
            // 就地同步探一个，慢一点但结果仍然正确。
            Err(_) => {
                let _ = tx.send(probe_one(owned));
                spawned += 1;
            }
        }
    }
    drop(tx);

    let mut results: Vec<GatewayProbe> = Vec::with_capacity(spawned);
    for _ in 0..spawned {
        match rx.recv() {
            Ok(probe) => results.push(probe),
            Err(_) => break,
        }
    }

    sort_probes(&mut results);
    results
}

/// 可达的排前面；同为可达则快的排前面。
pub fn sort_probes(results: &mut [GatewayProbe]) {
    results.sort_by(|a, b| match (a.reachable, b.reachable) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.latency_ms.cmp(&b.latency_ms),
    });
}

/// 最快的那个可达网关。全都不可达时返回 None —— 这种时候不能瞎选一个，
/// 那只会把用户从一条坏线换到另一条坏线。
pub fn fastest_reachable(results: &[GatewayProbe]) -> Option<&GatewayProbe> {
    results.iter().find(|p| p.reachable)
}

fn probe_one(gateway: Gateway) -> GatewayProbe {
    let started = Instant::now();
    let reachable = ping_endpoint(&gateway.endpoint);
    GatewayProbe {
        gateway,
        reachable,
        latency_ms: started.elapsed().as_millis(),
    }
}

/// 打 `/health` 而不是网关根路径：根路径可能返回几十 KB 的前端首页，
/// 慢链路上光传完就超时，把活着的网关判死。
///
/// 只要拿到**任何** HTTP 响应就算活着 —— 包括 404。网关只反代特定路径前缀，
/// 没配 /health 的会回 404，那也证明主机在、TLS 通、链路可用。
fn ping_endpoint(endpoint: &str) -> bool {
    let url = format!("{}/health", endpoint.trim_end_matches('/'));
    let agent = ureq::AgentBuilder::new().timeout(PROBE_TIMEOUT).build();
    match agent.get(&url).call() {
        Ok(_) => true,
        // ureq 把 4xx/5xx 当成 Err(Status)，但那是「服务器答话了」，算可达。
        Err(ureq::Error::Status(_, _)) => true,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gw(id: &str, endpoint: &str) -> Gateway {
        Gateway {
            id: id.into(),
            name: id.into(),
            endpoint: endpoint.into(),
            enabled: true,
            maintenance: false,
            client_latency_ms: None,
            healthy: true,
            selected: false,
        }
    }

    #[test]
    fn sorts_reachable_first_then_by_latency() {
        let mut results = vec![
            GatewayProbe { gateway: gw("slow", "https://slow"), reachable: true, latency_ms: 300 },
            GatewayProbe { gateway: gw("dead", "https://dead"), reachable: false, latency_ms: 0 },
            GatewayProbe { gateway: gw("fast", "https://fast"), reachable: true, latency_ms: 40 },
        ];
        sort_probes(&mut results);
        assert_eq!(results[0].gateway.id, "fast");
        assert_eq!(results[1].gateway.id, "slow");
        assert_eq!(results[2].gateway.id, "dead");
    }

    #[test]
    fn fastest_skips_unreachable() {
        let mut results = vec![
            GatewayProbe { gateway: gw("dead", "https://dead"), reachable: false, latency_ms: 0 },
            GatewayProbe { gateway: gw("ok", "https://ok"), reachable: true, latency_ms: 90 },
        ];
        sort_probes(&mut results);
        assert_eq!(fastest_reachable(&results).map(|p| p.gateway.id.as_str()), Some("ok"));
    }

    #[test]
    fn nothing_reachable_yields_none() {
        let results = vec![
            GatewayProbe { gateway: gw("a", "https://a"), reachable: false, latency_ms: 0 },
            GatewayProbe { gateway: gw("b", "https://b"), reachable: false, latency_ms: 0 },
        ];
        assert!(fastest_reachable(&results).is_none());
    }

    // 探测不能因为某个网关挂了就整体失败或卡住。
    #[test]
    fn probes_every_gateway_even_when_one_is_dead() {
        let results = probe_gateways(&[
            gw("dead1", "https://127.0.0.1:1"),
            gw("dead2", "https://127.0.0.1:2"),
        ]);
        assert_eq!(results.len(), 2);
        assert!(results.iter().all(|p| !p.reachable));
    }
}
