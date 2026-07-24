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

    /// The sandbox failed to start (e.g. the backend or the program could not be
    /// spawned). Typed separately from [`Error::Io`] so a calling agent can tell
    /// "the sandbox never ran the code" apart from "the code ran and failed", and
    /// adapt — one failed child does not take down its siblings or the tree.
    #[error("sandbox failed to start: {reason}")]
    Sandbox {
        /// Why the sandbox could not start the command.
        reason: String,
    },

    /// The permission policy refused the action. Typed separately from
    /// [`Error::Config`] so a refusal is distinguishable from a malfunction —
    /// a verification that was refused is not a verification that ran and
    /// failed, and the model is told the difference.
    #[error("refused by policy: {act} {target}{}", .rule.as_ref().map(|r| format!(" (rule {r} in layer {})", .layer.as_deref().unwrap_or("?"))).unwrap_or_default())]
    Refused {
        /// The action attempted.
        act: String,
        /// The path or binary it targeted.
        target: String,
        /// The glob that refused it, when a rule rather than a default did.
        rule: Option<String>,
        /// The layer that rule came from.
        layer: Option<String>,
    },

    /// A durable run could not be resumed from its checkpoint — the checkpoint
    /// format is newer than this binary supports, the run row is missing or
    /// corrupt, or the run has already finished. Typed separately so a caller
    /// handles a bad-checkpoint resume as a recoverable error instead of a panic
    /// or a silent half-resume. A partially written (crashed mid-commit) step is
    /// never surfaced here: the transaction rolls it back, so resume always sees
    /// the prior consistent checkpoint, not a torn one.
    #[error("cannot resume: {reason}")]
    Resume {
        /// Why the run could not be resumed.
        reason: String,
    },
}

/// Crate result alias.
pub type Result<T> = std::result::Result<T, Error>;
