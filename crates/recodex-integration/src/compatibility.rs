use crate::error::AdapterError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compatibility {
    pub supported: bool,
    pub minimum_version: String,
    pub reason: Option<String>,
}

pub fn check(version: &str, minimum: &str) -> Result<Compatibility, AdapterError> {
    if version.trim().is_empty() || minimum.trim().is_empty() {
        return Err(AdapterError::InvalidConfiguration(
            "client version is empty".into(),
        ));
    }
    let current = numeric_version(version)?;
    let minimum_version = numeric_version(minimum)?;
    let supported = current >= minimum_version;
    Ok(Compatibility {
        supported,
        minimum_version: minimum.to_owned(),
        reason: (!supported).then(|| format!("Codex++ {version} is older than {minimum}")),
    })
}

pub(crate) fn numeric_version(value: &str) -> Result<Vec<u64>, AdapterError> {
    let parts: Vec<_> = value.split('.').collect();
    if parts.len() != 3
        || parts.iter().any(|part| {
            part.is_empty()
                || (part.len() > 1 && part.starts_with('0'))
                || !part.chars().all(|c| c.is_ascii_digit())
        })
    {
        return Err(AdapterError::InvalidConfiguration(
            "client version must be major.minor.patch".into(),
        ));
    }
    parts
        .into_iter()
        .map(|part| {
            part.parse::<u64>().map_err(|_| {
                AdapterError::InvalidConfiguration("client version is too large".into())
            })
        })
        .collect()
}
