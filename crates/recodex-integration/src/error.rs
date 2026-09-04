use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterError {
    Unauthorized,
    Forbidden,
    /// 已购买、分配池暂无容量:服务端 403 + code=allocation_pending。
    /// 单独成一档:它和 Forbidden 走同一个状态码,但含义是「等一等」而不是「不允许」——
    /// 混在一起用户就会以为登录坏了去反复重登(线上 user 115 重登 8 次)。
    AllocationPending,
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
            Self::AllocationPending => write!(
                f,
                "账号分配中，请稍候几分钟再试 / your account is being allocated, please wait a few minutes"
            ),
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
