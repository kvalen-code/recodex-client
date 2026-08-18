use crate::{Gateway, Usage};

#[derive(Debug, Clone, PartialEq)]
pub enum PanelState {
    SignedOut,
    Loading,
    Ready {
        usage: Usage,
        gateway: Option<Gateway>,
    },
    Stale {
        usage: Usage,
        message: String,
    },
    Error {
        message: String,
    },
}

impl PanelState {
    pub fn is_actionable(&self) -> bool {
        matches!(self, Self::Ready { .. } | Self::Stale { .. })
    }
    pub fn status_text(&self) -> &'static str {
        match self {
            Self::SignedOut => "Sign in",
            Self::Loading => "Loading",
            Self::Ready { .. } => "Connected",
            Self::Stale { .. } => "Stale",
            Self::Error { .. } => "Unavailable",
        }
    }
}
