//! Serving recorded answers, so an evaluation case runs identically twice.
//!
//! [`Replay`] is a [`Provider`] with no provider behind it. It loads a file
//! written by [`Record`](crate::provider::Record) and answers from it, and it has
//! no endpoint, no client and no key — [`Provider::endpoint`] is `None`, so a run
//! driven by a replay opens no socket and needs no egress grant to do so.
//!
//! ## How a request finds its answer
//!
//! By its *content* — `system`, `user` and `tools`, which is the whole of a
//! [`CompletionRequest`] — and never by a call counter.
//!
//! Every scripted mock in `tests/` keys on an `AtomicUsize` index, and every one
//! of them is resume-unsafe for the same reason: 0.7.0's resume re-runs the step
//! that was in flight when the process died, that step calls `complete` again, and
//! a counter-keyed script hands it the response meant for the *next* step. The run
//! then continues from a script one place ahead of itself, silently. The stateless
//! prompt-driven fixture in `tests/checkpoint.rs` exists because of exactly this.
//!
//! Content-keying makes the re-ask return what the first ask returned, because it
//! is the same question.
//!
//! ## What that guarantees, precisely
//!
//! - The same request asked any number of times gets the same response, unless a
//!   *different* request is asked in between and the recording holds more than one
//!   answer for it (see below). A re-run step is therefore reproducible.
//! - A request the recording never saw is [`Error::Provider`] of kind
//!   [`Request`](crate::ProviderErrorKind::Request) — loud and non-retryable —
//!   rather than a default response. `CompletionResponse::default()` reads exactly
//!   like "the model chose not to call a tool", so a silent default would make a
//!   diverged replay look like a successful one.
//!
//! ## What it does not guarantee
//!
//! - **Anything about a diverged prompt.** The key is the exact request text, so a
//!   replay must be driven against the same contract and the same workspace state
//!   as the recording. A goal reworded, a fixture file edited, or a step that reads
//!   a file the previous run's replay left different produces a request that was
//!   never recorded — reported as a missing recording, not silently absorbed.
//! - **Duplicate requests across a process restart.** A recording that answered
//!   one identical request differently twice is served in recorded order, tracked
//!   in memory. A resume in a *new* process starts that tracking over, so a re-run
//!   step whose request duplicates an earlier step's gets the earlier answer. Only
//!   identical requests are affected: a run's prompt carries its observations, so
//!   consecutive steps normally differ.
//! - **Ordering beyond what was recorded.** Once a key's recorded answers are
//!   exhausted, the last one is served again for every further ask. The stated
//!   guarantee is same-request-same-answer, and a request recorded once must still
//!   be answerable twice; erroring instead would break the common recording where
//!   each request appears exactly once.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use crate::error::{Error, ProviderErrorKind, Result};
use crate::provider::record::Recording;
use crate::provider::{CompletionRequest, CompletionResponse, Provider};

/// The lookup key for a request: its content, canonically.
///
/// JSON of the whole request rather than a hash, so a mismatch can be read by a
/// human debugging a divergence, and rather than a hand-rolled concatenation, so
/// no separator can be forged by a prompt containing it. Serialising a
/// `CompletionRequest` cannot fail: every field is a `String`, a `Vec`, or a
/// `serde_json::Value`, none of which has a failing `Serialize`.
///
/// **(0.49.0) `messages` is excluded, and the two cache markers are not.**
///
/// The transcript is a *rendering* of content the key already covers: the run loop
/// derives `user` from the same emission it builds the transcript from, byte for
/// byte, which `the_derived_user_is_the_flat_prompt_the_transcript_was_built_from`
/// holds. A key over `user` therefore asks the same question, in a form that does
/// not move when the rendering does. That buys two things a stricter key loses:
///
/// - **a recording made before 0.49.0 still replays.** The key is byte-identical
///   to the one that release computed, so nothing has to be re-recorded.
/// - **an interrupted run still replays.** A resumed run rebuilds its ledger from
///   the store, which holds observation text and not tool-call structure, so its
///   requests carry no transcript where the recorded ones did. Keying on the
///   transcript makes every post-resume step a miss — which is how this was found:
///   `a_workspace_replay_survives_being_interrupted_and_resumed` failed, and the
///   guarantee it asserts is older than this release.
///
/// The cache markers stay in the key, because 0.44.0 decided they are part of the
/// question and `a_boundary_is_absent_from_an_unmarked_key_and_present_in_a_marked_one`
/// holds that decision. The two rulings are not in tension: a marker changes what
/// is asked of the vendor, where the transcript changes only how content already
/// in the key is spelt.
///
/// The limit, stated rather than left to be discovered: two requests whose
/// transcripts differ while their `user` strings do not would share a key. This
/// loop cannot produce such a pair, since both are built from one emission — but a
/// caller hand-building requests could.
fn key(request: &CompletionRequest) -> String {
    let stable = CompletionRequest {
        messages: Vec::new(),
        ..request.clone()
    };
    serde_json::to_string(&stable).expect("a CompletionRequest is always serialisable")
}

/// Which answer each key is currently on, and which key was asked last.
#[derive(Debug, Default)]
struct Cursor {
    /// Index of the answer most recently served for a key.
    at: HashMap<String, usize>,
    /// The key of the previous call, so an immediate re-ask is recognised as the
    /// same question rather than the next one.
    last: Option<String>,
}

/// Answer from a recording instead of from a provider.
///
/// ```no_run
/// use io_harness::provider::Replay;
///
/// # fn main() -> io_harness::Result<()> {
/// let provider = Replay::load("recording.json")?;
/// // ... run(&contract, &provider, &store).await? — no network, no key.
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct Replay {
    /// Every response recorded for a request key, in recorded order.
    answers: HashMap<String, Vec<CompletionResponse>>,
    /// A `Mutex` is fine where [`Fallback`](crate::provider::Fallback) needed an
    /// atomic: `complete` here awaits nothing at all, so no guard can span an await
    /// point and the future stays `Send`.
    cursor: Mutex<Cursor>,
}

impl Replay {
    /// Load a recording written by [`Record::save`](crate::provider::Record::save).
    ///
    /// Refuses a recording from another release series rather than misreading it.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let recording = Recording::load(path.as_ref())?;
        let mut answers: HashMap<String, Vec<CompletionResponse>> = HashMap::new();
        for exchange in recording.exchanges {
            answers
                .entry(key(&exchange.request))
                .or_default()
                .push(exchange.response);
        }
        Ok(Self {
            answers,
            cursor: Mutex::new(Cursor::default()),
        })
    }
}

impl Provider for Replay {
    /// Always. A replay answers from a recording, and if the recorded request
    /// carried an image then refusing it here would make a case unreplayable for
    /// a reason that has nothing to do with the model. The request's media is
    /// part of the key either way, so a media request cannot match a text-only
    /// recording.
    #[cfg(feature = "media")]
    fn accepts_images(&self) -> bool {
        true
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        let key = key(&request);
        let Some(answers) = self.answers.get(&key) else {
            // Non-retryable on purpose: the same request will be missing next
            // time too, so a retry burns the budget to reach the same place, and
            // the run should fail here rather than proceed on an invented answer.
            return Err(Error::provider(
                ProviderErrorKind::Request,
                format!(
                    "no recorded response for this request (system {} chars, {} tools, {} \
                     message(s), user begins {:?}): the replay has diverged from what was \
                     recorded",
                    request.system.len(),
                    request.tools.len(),
                    request.messages.len(),
                    request.user.chars().take(120).collect::<String>(),
                ),
            ));
        };
        let mut cursor = self.cursor.lock().unwrap_or_else(|e| e.into_inner());
        let at = match cursor.at.get(&key) {
            // Asked again immediately: the same question, so the same answer. This
            // is the resume case — a step that died before committing re-runs and
            // must not consume the next recorded answer.
            Some(&i) if cursor.last.as_deref() == Some(key.as_str()) => i,
            // A different request came in between, so this is a genuinely new ask.
            // Advance, saturating at the last recorded answer.
            Some(&i) => (i + 1).min(answers.len() - 1),
            None => 0,
        };
        cursor.at.insert(key.clone(), at);
        cursor.last = Some(key);
        Ok(answers[at].clone())
        // The guard drops here, at the end of a body that contains no `.await`.
    }

    fn name(&self) -> &str {
        "replay"
    }
}
