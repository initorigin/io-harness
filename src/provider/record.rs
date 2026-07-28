//! Capturing what a provider actually answered, losslessly.
//!
//! [`Record`] wraps a real provider, forwards every call to it, and keeps the
//! request paired with the response it produced. [`Record::save`] writes that to
//! a file [`Replay`](crate::provider::Replay) can serve from, which is how an
//! evaluation case runs twice and gets the same answers both times.
//!
//! ## Why a file rather than the store
//!
//! The trace in the [`Store`](crate::Store) already records something per step,
//! and it is not enough:
//!
//! - the step row keeps `CompletionResponse::text` only when there were no tool
//!   calls, so the commentary a model emits alongside a call is dropped;
//! - it keeps `Usage::total_tokens` and discards the prompt/completion split;
//! - it flattens the calls into `"name:{json}"` joined with `" | "`, which any
//!   `|` inside an argument silently corrupts.
//!
//! It could not be fixed by writing more columns either. [`Provider::complete`]
//! is RPITIT and its future must be `Send`; `rusqlite::Connection` is
//! `Send + !Sync`, so a `&Store` captured across the inner provider's `.await`
//! makes the future non-`Send` and the trait bound fails. A recorder therefore
//! cannot hold a store, and the recording goes to a plain file.
//!
//! ## What it does not capture
//!
//! Failures. [`Error`] is not serialisable and a recorded failure would be a
//! recorded *decision* about retry and fall-over rather than an answer. Only
//! successful completions are recorded, so replaying a run whose provider failed
//! reports a missing recording for that request rather than reproducing the
//! failure.

use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::provider::{CompletionRequest, CompletionResponse, Provider};

/// One recorded exchange: the request as it was sent, the response as it came
/// back. Both whole — this is the losslessness the trace format cannot offer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Exchange {
    pub(crate) request: CompletionRequest,
    pub(crate) response: CompletionResponse,
}

/// A recording file: the build that made it, and every exchange in call order.
///
/// The order matters — it is what lets a replay distinguish two identical
/// requests that were answered differently (see
/// [`Replay`](crate::provider::Replay)).
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Recording {
    /// The io-harness version that recorded this. Checked on load: a recording
    /// read by an incompatible build would be misread rather than refused.
    pub(crate) harness: String,
    pub(crate) exchanges: Vec<Exchange>,
}

/// The `major.minor` of a version — the axis a breaking change moves on while
/// the crate is 0.x, so a recording is accepted across patch releases and
/// refused across minor ones.
fn series(version: &str) -> String {
    version.split('.').take(2).collect::<Vec<_>>().join(".")
}

impl Recording {
    fn new(exchanges: Vec<Exchange>) -> Self {
        Self {
            harness: env!("CARGO_PKG_VERSION").to_string(),
            exchanges,
        }
    }

    fn save(&self, path: &Path) -> Result<()> {
        // Pretty, because a recording is a fixture a human reads and diffs.
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| Error::Config(format!("cannot serialise the recording: {e}")))?;
        std::fs::write(path, bytes)?;
        Ok(())
    }

    /// Read a recording, refusing one this build would misread.
    ///
    /// A missing or unreadable file is [`Error::Io`]; a file that is not a
    /// recording, or is one from another series, is [`Error::Config`] — the path
    /// is configuration the caller supplied, and it is wrong rather than broken.
    pub(crate) fn load(path: &Path) -> Result<Self> {
        let bytes = std::fs::read(path)?;
        let recording: Self = serde_json::from_slice(&bytes).map_err(|e| {
            Error::Config(format!(
                "{} is not a readable io-harness recording: {e}",
                path.display()
            ))
        })?;
        let current = env!("CARGO_PKG_VERSION");
        if series(&recording.harness) != series(current) {
            return Err(Error::Config(format!(
                "{} was recorded by io-harness {} and this build is {current}: refusing to \
                 replay across a series, because a recording read by a build whose request or \
                 response shape changed replays something other than what was recorded",
                path.display(),
                recording.harness,
            )));
        }
        Ok(recording)
    }
}

/// Wrap a provider to keep every request and the response it produced.
///
/// ```no_run
/// use io_harness::provider::Record;
/// use io_harness::OpenRouter;
///
/// # #[tokio::main] async fn main() -> io_harness::Result<()> {
/// let provider = Record::new(OpenRouter::from_env()?);
/// // ... run(&contract, &provider, &store).await? ...
/// provider.save("recording.json")?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct Record<P> {
    inner: P,
    /// The exchanges so far, in call order.
    ///
    /// A `Mutex` is safe here where [`Fallback`](crate::provider::Fallback) needed
    /// an atomic, but only because of how it is used: a `MutexGuard` is not `Send`,
    /// so holding one across the inner provider's `.await` would make
    /// `complete`'s future non-`Send` and fail the trait's bound. The one write
    /// site awaits *first* and then locks, mutates and drops within a single
    /// statement, so no guard can ever span an await point.
    seen: Mutex<Vec<Exchange>>,
}

impl<P: Provider> Record<P> {
    /// Record everything `inner` answers.
    pub fn new(inner: P) -> Self {
        Self {
            inner,
            seen: Mutex::new(Vec::new()),
        }
    }

    /// Write every exchange captured so far to `path` as JSON.
    ///
    /// Callable mid-run and repeatedly: it snapshots rather than drains, so a
    /// long run can checkpoint its recording and still record more.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        Recording::new(self.exchanges()).save(path.as_ref())
    }

    fn exchanges(&self) -> Vec<Exchange> {
        // A poisoned lock means something panicked elsewhere; the recording it
        // guards is still intact and losing it would be the worse outcome.
        self.seen.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

// `Sync` on the wrapped provider for the same reason `Fallback` needs it: `&self`
// is held across the inner call's await, so `&Record<P>` has to be `Send`, which
// needs its fields `Sync`.
impl<P: Provider + Sync> Provider for Record<P> {
    /// Whatever it wraps. Recording changes what is stored, not what the model
    /// can read.
    #[cfg(feature = "media")]
    fn accepts_images(&self) -> bool {
        self.inner.accepts_images()
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        let response = self.inner.complete(request.clone()).await;
        if let Ok(response) = &response {
            // Await already finished. Lock, push, drop — one statement, so the
            // guard cannot outlive it and the future stays `Send`.
            self.seen
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(Exchange {
                    request,
                    response: response.clone(),
                });
        }
        response
    }

    /// The inner provider's — a recorder is not a provider anyone chose, and the
    /// trace should name the one that answered.
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn endpoint(&self) -> Option<&str> {
        self.inner.endpoint()
    }

    /// The inner provider's, whatever it reports.
    ///
    /// Forwarded rather than defaulted for the reason `Fallback::endpoints` exists:
    /// the egress policy is deny-by-default and authorizes a provider's hosts before
    /// the first step, so a wrapper that reported fewer hosts than it can reach would
    /// be a way past a policy that never saw them. Recording a `Fallback` must still
    /// declare both of its endpoints.
    fn endpoints(&self) -> Vec<&str> {
        self.inner.endpoints()
    }

    /// The inner provider's, so wrapping a [`Fallback`](crate::provider::Fallback)
    /// in a `Record` does not hide which of its halves served the step.
    fn last_served(&self) -> Option<String> {
        self.inner.last_served()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recording_is_refused_across_a_series_and_accepted_within_one() {
        assert_eq!(series("0.12.0"), series("0.12.7"));
        assert_ne!(series("0.11.0"), series("0.12.0"));
        // A version string that is not one still compares by what it has, rather
        // than panicking on a fixture someone hand-wrote.
        assert_eq!(series("nightly"), "nightly");
    }
}
