use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("aria2c is required but was not found in PATH")]
    MissingAria2,

    #[error("unsupported protocol for URL: {0}")]
    UnsupportedProtocol(String),

    #[error("no valid download URL candidate was found")]
    NoCandidates,

    #[error("could not determine host from URL: {0}")]
    MissingHost(String),
}
