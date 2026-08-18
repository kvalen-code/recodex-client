use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterError {
    Unauthorized,
    Forbidden,
    Conflict(String),
    RateLimited,
    DeviceCodeUnknown,
    DeviceCodeExpired,
    ServiceUnavailable,
    Unavailable,
    InvalidResponse(String),
    InvalidConfiguration(String),
}

impl fmt::Display for AdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unauthorized => write!(f, "ReCodex login is required"),
            Self::Forbidden => write!(f, "ReCodex account is not allowed to perform this action"),
            Self::Conflict(message) => write!(f, "client or gateway conflict: {message}"),
            Self::RateLimited => write!(f, "ReCodex temporarily rate limited this request"),
            Self::DeviceCodeUnknown => write!(f, "ReCodex login code is invalid or already used"),
            Self::DeviceCodeExpired => {
                write!(f, "ReCodex login code has expired; start login again")
            }
            Self::ServiceUnavailable => write!(f, "ReCodex service is unavailable"),
            Self::Unavailable => write!(f, "ReCodex service is unavailable"),
            Self::InvalidResponse(message) => write!(f, "invalid ReCodex response: {message}"),
            Self::InvalidConfiguration(message) => {
                write!(f, "invalid ReCodex configuration: {message}")
            }
        }
    }
}

impl std::error::Error for AdapterError {}
