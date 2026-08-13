//! The one error type the crate returns.

use std::time::Duration;

/// Why a provider call failed, and therefore whether trying it again — or trying
/// a different provider — is worth doing.
///
/// Before 0.11.0 every provider failure was one string: a 429, a 503, a 401 and
/// a DNS failure differed only in prose, so nothing above the provider could
/// branch on them. This enum is that branch, and [`ProviderErrorKind::is_retryable`]
/// is the decision the rest of the crate reads.
///
/// A caller branches on the kind rather than on the message, because the message
/// wording is explicitly *not* part of the public contract and the kind is:
///
/// ```
/// use io_harness::{Error, ProviderErrorKind};
/// use std::time::Duration;
///
/// # fn next_move(failure: &Error) -> (&'static str, Option<Duration>) {
/// // What a run that exhausted its retries hands back. The three answers a
/// // caller actually has are "wait this long", "try again now", and "stop".
/// match failure {
///     // The only kind that carries *when*. Honour the server's own number
///     // rather than backing off blind, then resume under the original run id.
///     Error::Provider { kind: ProviderErrorKind::RateLimited, retry_after, .. } => {
///         ("wait, then resume", Some(retry_after.unwrap_or(Duration::from_secs(30))))
///     }
///     // Auth and Request are terminal: the key will not become valid, and the
///     // server has already read this exact request and refused it. Retrying
///     // burns the budget to reach the same failure and hides it behind a later one.
///     Error::Provider { kind, .. } if !kind.is_retryable() => ("stop and page a human", None),
///     Error::Provider { .. } => ("resume; another attempt can succeed", None),
///     _ => ("not a provider failure", None),
/// }
/// # }
/// ```
// `#[non_exhaustive]` since 0.43.0, which added `ContextOverflow`. Paid together
// with the variant, once, for the reason `Verification` (0.34.0), `TaskContract`
// (0.35.0), `AgentDef` (0.36.0) and `TurnResult` (0.37.0) each paid it: a caller
// matching this enum exhaustively adds a wildcard arm now, and the next kind of
// failure a vendor invents costs nobody anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProviderErrorKind {
    /// The request never completed: connection refused, DNS failure, TLS failure,
    /// a mid-stream byte error.
    Transport,
    /// The request had a deadline and passed it.
    Timeout,
    /// 429. Carries the server's `Retry-After` when it sent one.
    RateLimited,
    /// 5xx.
    Server,
    /// 401 or 403 — the key is wrong, missing, or not entitled to this model.
    Auth,
    /// 4xx other than 401/403: the request itself is unacceptable.
    Request,
    /// The response arrived and nothing in it could be read.
    Malformed,
    /// The request did not fit the model's context window (0.43.0).
    ///
    /// A 4xx like any other as far as the wire is concerned, and told apart by the
    /// vendor's own wording — see [`from_response`](Self::from_response). It is
    /// **not** retryable, and that is the correct answer rather than a compromise:
    /// re-sending bytes a server has said were too many cannot work.
    ///
    /// What answers it is a *different* request. The run loop folds the older half
    /// of its observations into a summary and asks once more with the shorter one;
    /// a second overflow after that escalates. So the recovery belongs to the loop
    /// and not to the retry policy, and this variant exists so the loop has
    /// something to branch on.
    ContextOverflow,
}

impl ProviderErrorKind {
    /// Whether re-sending the same request could plausibly succeed.
    ///
    /// Per arm, and the reasoning is the point — a retry that cannot work costs
    /// the budget twice and hides the real failure behind a later one:
    ///
    /// - [`Transport`](Self::Transport): the request never reached the model, so
    ///   nothing about it was rejected. Networks drop connections for reasons
    ///   that do not repeat.
    /// - [`Timeout`](Self::Timeout): the deadline says nothing about the request.
    ///   A hung socket or an overloaded server is the usual cause and neither is
    ///   permanent.
    /// - [`RateLimited`](Self::RateLimited): the server is explicitly telling you
    ///   to come back later. This is the one kind that carries *when*.
    /// - [`Server`](Self::Server): a 5xx is the server's own admission that the
    ///   fault is its side, not the request's.
    /// - [`Malformed`](Self::Malformed): a garbled or empty stream is far more
    ///   often a truncated transfer than a model that genuinely produced nothing,
    ///   and re-asking is cheap — cheaper than a run that reads the emptiness as
    ///   "the model chose not to call a tool" and quietly stops.
    /// - [`Auth`](Self::Auth): the key will not become valid between two calls a
    ///   second apart. Retrying burns the retry budget to reach the same 401.
    /// - [`Request`](Self::Request): the server has already read this exact
    ///   request and refused it. Sending it again is two failures instead of one.
    /// - [`ContextOverflow`](Self::ContextOverflow): the same request cannot
    ///   become small enough by being sent again. The answer is a smaller request,
    ///   which the run loop builds by compacting; that is a recovery, not a retry.
    pub fn is_retryable(self) -> bool {
        match self {
            Self::Transport
            | Self::Timeout
            | Self::RateLimited
            | Self::Server
            | Self::Malformed => true,
            Self::Auth | Self::Request | Self::ContextOverflow => false,
        }
    }

    /// The kind an HTTP status maps to.
    ///
    /// The one place that mapping lives, so OpenRouter, OpenAI and Anthropic
    /// cannot drift apart: they were byte-identical before this existed and this
    /// is what keeps them so. A status the buckets do not cover (1xx, 3xx — the
    /// client follows no redirects, so a 3xx is a boundary hop, not a success)
    /// is [`Request`](Self::Request): unexpected, and not something a retry fixes.
    pub fn from_status(status: u16) -> Self {
        match status {
            429 => Self::RateLimited,
            401 | 403 => Self::Auth,
            500..=599 => Self::Server,
            _ => Self::Request,
        }
    }

    /// The kind a status *and the server's own words* map to (0.43.0).
    ///
    /// [`from_status`](Self::from_status) cannot tell an over-window request from
    /// any other rejected one, because every vendor reports it as a plain 4xx.
    /// Only the message says which it was, so this is the one classification that
    /// reads it — and it stays deliberately conservative: a signature this list
    /// misses costs the run exactly what it costs today, while a false positive
    /// makes the loop compact and re-send a request the server had already read
    /// and refused. The asymmetry is in the safe direction and should stay there;
    /// widening the match is the tempting fix and the wrong one.
    ///
    /// Only a 400 or a 413 is eligible. A 429 is a rate limit whatever it says,
    /// and a 500 is the server's own admission of fault.
    ///
    /// ```
    /// use io_harness::ProviderErrorKind;
    ///
    /// let over = ProviderErrorKind::from_response(400, "This model's maximum context length is 8192 tokens");
    /// assert_eq!(over, ProviderErrorKind::ContextOverflow);
    /// assert!(!over.is_retryable(), "the same bytes cannot fit on a second try");
    ///
    /// // A 400 that is simply a bad request stays a bad request.
    /// assert_eq!(
    ///     ProviderErrorKind::from_response(400, "unknown parameter: temperture"),
    ///     ProviderErrorKind::Request
    /// );
    /// // And nothing else moves.
    /// assert_eq!(ProviderErrorKind::from_response(429, "too many tokens per minute"),
    ///            ProviderErrorKind::RateLimited);
    /// ```
    pub fn from_response(status: u16, message: &str) -> Self {
        if matches!(status, 400 | 413) && is_context_overflow(message) {
            return Self::ContextOverflow;
        }
        Self::from_status(status)
    }
}

/// The vendor wordings that mean "this request did not fit".
///
/// A list rather than a regex, and short rather than clever: each entry is a
/// phrase a vendor actually sends, and anything not here is left alone. Matched
/// case-insensitively because the same vendor capitalises it differently in an
/// error code and in a human-readable message.
const CONTEXT_OVERFLOW_SIGNATURES: &[&str] = &[
    // OpenAI and every wire that copies its error shape, including OpenRouter.
    "context_length_exceeded",
    "maximum context length",
    "reduce the length of the messages",
    // Anthropic.
    "prompt is too long",
    "exceed context limit",
    // Common to several compatible endpoints.
    "context window",
    "too many tokens",
];

/// Whether a rejection's message is one of [`CONTEXT_OVERFLOW_SIGNATURES`].
fn is_context_overflow(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    CONTEXT_OVERFLOW_SIGNATURES
        .iter()
        .any(|needle| lower.contains(needle))
}

/// Errors io-harness can return from a run.
///
/// The variants are separate because the *response* to each is separate — a
/// refusal is not a malfunction, a bad checkpoint is not a bad key, and only one
/// arm here is ever worth retrying. This is the match a caller writes around an
/// entry point:
///
/// ```
/// use io_harness::Error;
///
/// # fn handle(failure: Error) -> String {
/// match failure {
///     // The policy said no. Nothing happened and nothing is broken: either
///     // widen the rule that refused it, or accept the refusal. The rule and
///     // layer are carried so the operator knows which line of config to edit.
///     Error::Refused { act, target, rule, layer } => format!(
///         "{act} {target} refused by {} in layer {}",
///         rule.unwrap_or_else(|| "the tier default".into()),
///         layer.unwrap_or_else(|| "-".into()),
///     ),
///     // The checkpoint could not be honoured — newer format, missing run, or a
///     // run started under a policy this resume would have silently dropped.
///     // Never retried in a loop: re-resume the way the message names.
///     Error::Resume { reason } => format!("resume refused: {reason}"),
///     // Operator error, raised before the provider is called once. Fail the
///     // job; a second attempt reaches the same missing key or duplicate tool.
///     Error::Config(message) => format!("fix the configuration: {message}"),
///     // The tool server never came up. The run fails rather than quietly
///     // proceeding without a capability it was told it had.
///     Error::Mcp { server, reason } => format!("server {server} did not start: {reason}"),
///     // The gate never ran the code, as opposed to running it and failing it.
///     Error::Sandbox { reason } => format!("verification never executed: {reason}"),
///     // The one arm where another attempt is a real option — and only for some
///     // kinds; see `ProviderErrorKind::is_retryable`.
///     Error::Provider { kind, message, .. } if kind.is_retryable() => format!("retry: {message}"),
///     other => other.to_string(),
/// }
/// # }
/// ```
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A filesystem tool operation failed.
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),

    /// The state store (rusqlite) failed.
    #[error("state store error: {0}")]
    State(#[from] rusqlite::Error),

    /// The provider request or its streamed response failed.
    ///
    /// [`ProviderErrorKind`] is what a caller branches on; `status` and
    /// `retry_after` are kept rather than folded into the message so a retry can
    /// honour what the server actually said.
    #[error("provider error ({kind:?}{}): {message}", .status.map(|s| format!(", HTTP {s}")).unwrap_or_default())]
    Provider {
        /// Why the call failed, and whether retrying is worth it.
        kind: ProviderErrorKind,
        /// The HTTP status, when the failure had one.
        status: Option<u16>,
        /// The server's `Retry-After`, when it sent one.
        retry_after: Option<Duration>,
        /// What the provider or the transport reported.
        message: String,
    },

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

    /// An MCP server could not be reached or set up. Typed separately from
    /// [`Error::Provider`] so a caller can tell "the model call failed" from
    /// "the tool server the operator configured never came up" — the second is a
    /// configuration problem, and the run fails on it rather than quietly
    /// proceeding without a capability it was told it had.
    ///
    /// Failures *during* a call — a timeout, a dead transport, a tool reporting
    /// its own error — are not this. They come back to the model as observations
    /// it can adapt to, like a refused path or a bad regex.
    #[error("mcp server {server}: {reason}")]
    Mcp {
        /// The configured server's id.
        server: String,
        /// What went wrong.
        reason: String,
    },

    /// A language server could not be started or spoke something this client
    /// could not read (0.52.0). Typed separately from [`Error::Mcp`] for the same
    /// reason that one is separate from [`Error::Provider`]: a configured
    /// capability that never came up is a configuration problem, and the run says
    /// so rather than quietly navigating by text search instead.
    ///
    /// Failures *during* a call — a request that timed out, a server with no
    /// answer for this position, a capability the server does not advertise — are
    /// not this. They come back to the model as observations naming the reason,
    /// because an empty answer that means "I could not ask" and an empty answer
    /// that means "there are no references" are the distinction the whole surface
    /// exists to preserve.
    #[error("language server {server}: {reason}")]
    Lsp {
        /// The configured server's id, empty only for a framing error found
        /// before any server was named.
        server: String,
        /// What went wrong.
        reason: String,
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

impl Error {
    /// A provider failure of `kind` carrying no HTTP status — the request never
    /// completed, or the response could not be read.
    pub fn provider(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self::Provider {
            kind,
            status: None,
            retry_after: None,
            message: message.into(),
        }
    }

    /// The request never completed: connection refused, DNS, TLS, a mid-stream
    /// byte error.
    pub fn provider_transport(message: impl Into<String>) -> Self {
        Self::provider(ProviderErrorKind::Transport, message)
    }

    /// The response arrived and nothing in it could be read.
    pub fn provider_malformed(message: impl Into<String>) -> Self {
        Self::provider(ProviderErrorKind::Malformed, message)
    }

    /// A non-success HTTP status, with the kind derived once by
    /// [`ProviderErrorKind::from_response`] so no provider can classify it
    /// differently.
    ///
    /// Every built-in provider builds its status failures here, which is why
    /// 0.43.0's over-window classification needed no per-vendor edit: one funnel,
    /// three wires, and no way for OpenRouter, OpenAI and Anthropic to drift apart
    /// on what an over-window rejection is.
    pub fn provider_status(
        status: u16,
        retry_after: Option<Duration>,
        message: impl Into<String>,
    ) -> Self {
        let message = message.into();
        Self::Provider {
            kind: ProviderErrorKind::from_response(status, &message),
            status: Some(status),
            retry_after,
            message,
        }
    }
}

/// Every `reqwest` failure the crate sees is a provider call that did not
/// complete, so the timeout/transport split is decided here once instead of at
/// each of the four call sites (three providers plus the shared SSE reader).
impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        let kind = if e.is_timeout() {
            ProviderErrorKind::Timeout
        } else {
            ProviderErrorKind::Transport
        };
        Self::provider(kind, e.to_string())
    }
}

/// Crate result alias.
///
/// Every fallible call in the crate returns this, so an embedding function that
/// adopts it gets `?` across all of them with no error mapping in between —
/// opening the store, building a provider from the environment, and driving the
/// run are three different failure sources and one type:
///
/// ```no_run
/// use io_harness::{run_with, ApproveAll, OpenRouter, Policy, Result, Store,
///                  TaskContract, Verification};
///
/// // `Result<RunOutcome>` is `std::result::Result<RunOutcome, io_harness::Error>`.
/// # async fn build_notes(repo: &str) -> Result<io_harness::RunOutcome> {
/// let store = Store::open("runs.db")?;          // rusqlite failure -> Error::State
/// let provider = OpenRouter::from_env()?;       // missing key   -> Error::Config
/// let contract = TaskContract::workspace("summarise the README into NOTES.md", repo)
///     .with_verification(Verification::WorkspaceFileContains {
///         file: "NOTES.md".into(),
///         needle: "#".into(),
///     });
/// let policy = Policy::default().layer("app").allow_write("NOTES.md");
/// let result = run_with(&contract, &provider, &store, &policy, &ApproveAll).await?;
/// Ok(result.outcome)
/// # }
/// ```
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_status_always_maps_to_the_same_kind() {
        use ProviderErrorKind::*;
        for (status, kind) in [
            (429, RateLimited),
            (401, Auth),
            (403, Auth),
            (400, Request),
            (404, Request),
            (422, Request),
            (500, Server),
            (503, Server),
            (599, Server),
        ] {
            assert_eq!(ProviderErrorKind::from_status(status), kind, "{status}");
        }
    }

    #[test]
    fn retrying_is_worth_it_for_everything_except_a_rejected_request() {
        use ProviderErrorKind::*;
        for kind in [Transport, Timeout, RateLimited, Server, Malformed] {
            assert!(kind.is_retryable(), "{kind:?}");
        }
        for kind in [Auth, Request] {
            assert!(!kind.is_retryable(), "{kind:?}");
        }
    }

    #[test]
    fn a_status_failure_keeps_its_status_and_retry_after() {
        let e = Error::provider_status(429, Some(Duration::from_secs(7)), "slow down");
        let Error::Provider {
            kind,
            status,
            retry_after,
            ..
        } = &e
        else {
            panic!("expected a provider error");
        };
        assert_eq!(*kind, ProviderErrorKind::RateLimited);
        assert_eq!(*status, Some(429));
        assert_eq!(*retry_after, Some(Duration::from_secs(7)));
        // The rendering names the kind and the status; the fields are what code
        // branches on.
        let shown = e.to_string();
        assert!(shown.contains("RateLimited"), "{shown}");
        assert!(shown.contains("429"), "{shown}");
    }
}
