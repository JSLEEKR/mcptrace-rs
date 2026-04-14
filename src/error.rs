//! Crate-wide error type. Library code never panics — it returns [`Error`].

use thiserror::Error;

/// Result alias using the crate's [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// All fallible operations in mcptrace return one of these variants.
#[derive(Debug, Error)]
pub enum Error {
    /// An I/O error bubbled up from std::io.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// serde_json failed to parse or serialize.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    /// toml failed to deserialize.
    #[error("toml parse error: {0}")]
    Toml(#[from] toml::de::Error),

    /// HTTP client failure.
    #[error("http error: {0}")]
    Http(String),

    /// A message exceeded the configured max-frame-bytes cap.
    #[error("frame too large: {size} bytes (max {max})")]
    FrameTooLarge { size: u64, max: u64 },

    /// JSON-RPC wire format violation.
    #[error("invalid jsonrpc: {0}")]
    InvalidJsonRpc(String),

    /// Duration string like "5m" could not be parsed.
    #[error("invalid duration: {0}")]
    InvalidDuration(String),

    /// CLI validation failure (e.g. port out of range, NaN numeric).
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// Configuration file is syntactically valid but semantically invalid.
    #[error("invalid config: {0}")]
    InvalidConfig(String),

    /// A span could not be persisted or exported.
    #[error("observation error: {0}")]
    Observation(String),
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Error::Http(e.to_string())
    }
}
