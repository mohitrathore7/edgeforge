use thiserror::Error;

/// Every failure EdgeForge can report, grouped so the HTTP API can map
/// them to sensible status codes.
#[derive(Debug, Error)]
pub enum EdgeError {
    /// The artifact failed integrity or format checks. Nothing was changed on disk.
    #[error("artifact verification failed: {0}")]
    Verify(String),

    /// The artifact could not be downloaded (network error, timeout, truncated body).
    #[error("artifact fetch failed: {0}")]
    Fetch(String),

    #[error("app not found: {0}")]
    NotFound(String),

    #[error("app already installed: {0}")]
    AlreadyExists(String),

    #[error("invalid request: {0}")]
    Invalid(String),

    /// An update was applied but failed its health gate and was reverted.
    #[error("update rolled back: {0}")]
    RolledBack(String),

    #[error("runtime error: {0}")]
    Runtime(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, EdgeError>;

impl EdgeError {
    pub fn http_status(&self) -> u16 {
        match self {
            EdgeError::Verify(_) => 422,
            EdgeError::Invalid(_) => 400,
            EdgeError::NotFound(_) => 404,
            EdgeError::AlreadyExists(_) | EdgeError::RolledBack(_) => 409,
            EdgeError::Fetch(_) => 502,
            EdgeError::Runtime(_) | EdgeError::Io(_) | EdgeError::Json(_) => 500,
        }
    }

    /// Short machine-readable kind, used in API responses.
    pub fn kind(&self) -> &'static str {
        match self {
            EdgeError::Verify(_) => "verification_failed",
            EdgeError::Fetch(_) => "fetch_failed",
            EdgeError::NotFound(_) => "not_found",
            EdgeError::AlreadyExists(_) => "already_exists",
            EdgeError::Invalid(_) => "invalid_request",
            EdgeError::RolledBack(_) => "rolled_back",
            EdgeError::Runtime(_) => "runtime_error",
            EdgeError::Io(_) => "io_error",
            EdgeError::Json(_) => "json_error",
        }
    }
}
