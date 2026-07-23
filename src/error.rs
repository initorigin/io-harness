//! The one error type the crate returns.

/// Errors io-harness can return from a run.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A filesystem tool operation failed.
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),

    /// The state store (rusqlite) failed.
    #[error("state store error: {0}")]
    State(#[from] rusqlite::Error),

    /// The provider request or its streamed response failed.
    #[error("provider error: {0}")]
    Provider(String),

    /// Configuration was missing or invalid (e.g. no API key).
    #[error("configuration error: {0}")]
    Config(String),
}

/// Crate result alias.
pub type Result<T> = std::result::Result<T, Error>;
