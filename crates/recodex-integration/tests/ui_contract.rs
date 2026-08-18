use recodex_integration::{PanelState, Usage};

fn usage() -> Usage {
    Usage {
        account_type: "shared".into(),
        available: 8.0,
        total: 10.0,
        used: 2.0,
        windows: vec![],
        refreshed_at: "now".into(),
        source: "recodex-realtime".into(),
        stale: false,
        refresh_error: None,
    }
}

#[test]
fn panel_states_are_explicit() {
    assert_eq!(PanelState::SignedOut.status_text(), "Sign in");
    assert_eq!(PanelState::Loading.status_text(), "Loading");
    assert!(PanelState::Ready {
        usage: usage(),
        gateway: None
    }
    .is_actionable());
    assert!(PanelState::Stale {
        usage: usage(),
        message: "offline".into()
    }
    .is_actionable());
    assert!(!PanelState::Error {
        message: "down".into()
    }
    .is_actionable());
}
