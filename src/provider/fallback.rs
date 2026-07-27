//! One provider standing behind another.
//!
//! Three providers are implemented and, before 0.11, only ever one was reachable
//! in a run: every entry point is generic over a single `P: Provider`, and
//! [`Provider::complete`](crate::provider::Provider::complete) returns
//! `impl Future`, so there is no `dyn Provider` to hold a list behind.
//!
//! [`Fallback`] is how fallback happens without changing that. It is itself a
//! `Provider`, so `run(&contract, &Fallback::new(a, b), &store)` needs no new
//! entry point, and it nests — `Fallback::new(a, Fallback::new(b, c))` — for three.
//!
//! It falls through on a failure another provider might not have and refuses to on
//! one it would meet too. Retrying a request the server read and rejected is two
//! failures instead of one, and a wrong API key is not more valid at a different
//! vendor.
//!
//! ## What it does not promise
//!
//! That the two providers are equivalent. Falling over swaps the model mid-run, and
//! the harness cannot see whether the replacement is as capable, as well tuned, or
//! even the same size. An operator configuring a fallback should expect that a run
//! which fell over may behave differently from one that did not — which is why the
//! provider that actually answered is recorded per step rather than inferred from
//! the run's configuration.

use std::sync::atomic::{AtomicU8, Ordering};

use crate::error::{Error, Result};
use crate::provider::{CompletionRequest, CompletionResponse, Provider};

/// Try `primary`; on a failure another provider might survive, try `secondary`.
///
/// ```no_run
/// use io_harness::provider::Fallback;
/// use io_harness::{Anthropic, OpenRouter};
///
/// # fn main() -> io_harness::Result<()> {
/// let provider = Fallback::new(OpenRouter::from_env()?, Anthropic::from_env()?);
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct Fallback<A, B> {
    primary: A,
    secondary: B,
    /// `name()` returns `&str`, so the combined label has to be owned somewhere.
    name: String,
    /// Which one answered last: 0 nobody, 1 primary, 2 secondary. An atomic rather
    /// than a lock because a `MutexGuard` is not `Send` and `complete`'s future has
    /// to be. Read by the run loop to record, per step, the provider that actually
    /// served it — `runs.provider` is one label for a whole run and stops being true
    /// the moment a run can use two.
    served: AtomicU8,
}

impl<A: Provider, B: Provider> Fallback<A, B> {
    /// `secondary` answers when `primary` fails in a way it might survive.
    pub fn new(primary: A, secondary: B) -> Self {
        let name = format!("{} -> {}", primary.name(), secondary.name());
        Self {
            primary,
            secondary,
            name,
            served: AtomicU8::new(0),
        }
    }

    fn note(&self, who: u8) {
        self.served.store(who, Ordering::Relaxed);
    }
}

/// Whether a different provider is worth trying.
///
/// The same question [`ProviderErrorKind::is_retryable`](crate::error::ProviderErrorKind::is_retryable)
/// answers for a retry, and the same answer: a failure that was about the request or
/// about the caller's own configuration will happen again at the next vendor, so
/// falling over just fails twice and takes twice as long doing it.
fn worth_another_provider(e: &Error) -> bool {
    matches!(e, Error::Provider { kind, .. } if kind.is_retryable())
}

// `Sync` on both halves is what makes `complete`'s future `Send`: the combinator
// holds `&self` across the primary's await, so `&Fallback<A, B>` has to be `Send`,
// which needs its fields `Sync`. Every real provider is (they hold a `reqwest::Client`
// and two `String`s), so this costs nothing a caller would notice.
impl<A: Provider + Sync, B: Provider + Sync> Provider for Fallback<A, B> {
    /// Both, or neither. Reporting the primary's answer would let an image reach
    /// a secondary that cannot read it on the one call that matters — the
    /// fallover — and a fallover already means something has gone wrong. The
    /// secondary would refuse it (`ensure_media_accepted` runs inside every
    /// built-in provider too), so the only thing the conjunction changes is
    /// *when* the caller finds out: before the run, rather than mid-fallover.
    #[cfg(feature = "media")]
    fn accepts_images(&self) -> bool {
        self.primary.accepts_images() && self.secondary.accepts_images()
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse> {
        match self.primary.complete(request.clone()).await {
            Ok(response) => {
                self.note(1);
                Ok(response)
            }
            Err(e) if worth_another_provider(&e) => {
                tracing::warn!(
                    primary = self.primary.name(),
                    secondary = self.secondary.name(),
                    error = %e,
                    "provider failed; falling over"
                );
                let out = self.secondary.complete(request).await;
                self.note(if out.is_ok() { 2 } else { 0 });
                out
            }
            Err(e) => {
                // Not worth another provider. The caller gets the primary's error,
                // unchanged, so the kind they branch on is the real one. Nobody
                // served this call, and nothing must be able to read that anybody
                // did — a stale marker is worse than none.
                self.note(0);
                Err(e)
            }
        }
    }

    fn name(&self) -> &str {
        &self.name
    }

    /// The primary's, for the single-endpoint callers that predate fallback.
    /// [`endpoints`](Provider::endpoints) is what authorization uses.
    fn endpoint(&self) -> Option<&str> {
        self.primary.endpoint()
    }

    /// BOTH providers' endpoints.
    ///
    /// This is the reason `endpoints` exists at all. The 0.8.0 egress policy is
    /// deny-by-default and a run authorizes its provider's host before its first
    /// step; a combinator that reported only the primary's host would leave the
    /// secondary's completely ungoverned, so a fallback would be a way to reach a
    /// host the policy never saw.
    fn endpoints(&self) -> Vec<&str> {
        let mut out = self.primary.endpoints();
        out.extend(self.secondary.endpoints());
        out
    }

    /// The LEAF that answered, not the branch it sits in.
    ///
    /// A nested `Fallback::new(a, Fallback::new(b, c))` whose `c` answered must
    /// record `"c"`, not `"b -> c"`: the row exists because one label for a whole
    /// run stops being true the moment a run can use two, and naming a sub-chain
    /// reintroduces exactly that ambiguity one level down.
    fn last_served(&self) -> Option<String> {
        match self.served.load(Ordering::Relaxed) {
            1 => Some(
                self.primary
                    .last_served()
                    .unwrap_or_else(|| self.primary.name().to_string()),
            ),
            2 => Some(
                self.secondary
                    .last_served()
                    .unwrap_or_else(|| self.secondary.name().to_string()),
            ),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ProviderErrorKind;

    struct Fixed {
        label: &'static str,
        result: fn() -> Result<CompletionResponse>,
        endpoint: Option<&'static str>,
    }

    impl Provider for Fixed {
        async fn complete(&self, _request: CompletionRequest) -> Result<CompletionResponse> {
            (self.result)()
        }
        fn name(&self) -> &str {
            self.label
        }
        fn endpoint(&self) -> Option<&str> {
            self.endpoint
        }
    }

    fn ok() -> Result<CompletionResponse> {
        Ok(CompletionResponse {
            text: Some("hello".into()),
            ..Default::default()
        })
    }
    fn down() -> Result<CompletionResponse> {
        Err(Error::provider_status(503, None, "down"))
    }
    fn bad_key() -> Result<CompletionResponse> {
        Err(Error::provider_status(401, None, "bad key"))
    }
    fn boom() -> Result<CompletionResponse> {
        panic!("the secondary must not be called");
    }

    #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
    fn req() -> CompletionRequest {
        CompletionRequest {
            system: String::new(),
            user: "hi".into(),
            tools: Vec::new(),
            ..Default::default()
        }
    }

    fn p(label: &'static str, result: fn() -> Result<CompletionResponse>) -> Fixed {
        Fixed {
            label,
            result,
            endpoint: None,
        }
    }

    #[tokio::test]
    async fn a_down_primary_is_answered_by_the_secondary() {
        let f = Fallback::new(p("first", down), p("second", ok));
        let out = f.complete(req()).await.unwrap();
        assert_eq!(out.text.as_deref(), Some("hello"));
        assert_eq!(f.last_served().as_deref(), Some("second"));
        assert_eq!(f.name(), "first -> second");
    }

    #[tokio::test]
    async fn a_working_primary_is_never_backed_up() {
        let f = Fallback::new(p("first", ok), p("second", boom));
        assert!(f.complete(req()).await.is_ok());
        assert_eq!(f.last_served().as_deref(), Some("first"));
    }

    #[tokio::test]
    async fn a_failure_the_secondary_would_share_does_not_fall_over() {
        // A wrong key is not more valid at a different vendor, and an unacceptable
        // request is unacceptable twice. `boom` panics if this is got wrong.
        let f = Fallback::new(p("first", bad_key), p("second", boom));
        let err = f.complete(req()).await.unwrap_err();
        let Error::Provider { kind, status, .. } = err else {
            panic!("expected a provider error, got {err:?}");
        };
        assert_eq!(kind, ProviderErrorKind::Auth);
        assert_eq!(status, Some(401));
        // Nothing served it, so nothing is claimed to have.
        assert_eq!(f.last_served(), None);
    }

    #[tokio::test]
    async fn both_failing_reports_the_secondarys_error() {
        let f = Fallback::new(p("first", down), p("second", bad_key));
        let err = f.complete(req()).await.unwrap_err();
        let Error::Provider { kind, .. } = err else {
            panic!("expected a provider error");
        };
        // The last thing tried is the thing that failed, which is what a caller
        // needs to act on — the primary's failure is in the trace and the log.
        assert_eq!(kind, ProviderErrorKind::Auth);
    }

    #[tokio::test]
    async fn three_providers_nest_and_the_leaf_is_what_gets_recorded() {
        let f = Fallback::new(p("a", down), Fallback::new(p("b", down), p("c", ok)));
        assert!(f.complete(req()).await.is_ok());
        assert_eq!(f.name(), "a -> b -> c");
        // Not "b -> c": the row has to name the provider, not the branch.
        assert_eq!(f.last_served().as_deref(), Some("c"));
    }

    #[test]
    fn authorization_sees_every_endpoint_in_the_chain() {
        // The security property: a combinator reporting only the primary's host
        // would let a fallback reach a host the deny-by-default egress policy never
        // checked.
        let a = Fixed {
            label: "a",
            result: ok,
            endpoint: Some("https://a.example/v1"),
        };
        let b = Fixed {
            label: "b",
            result: ok,
            endpoint: Some("https://b.example/v1"),
        };
        let f = Fallback::new(a, b);
        assert_eq!(
            f.endpoints(),
            vec!["https://a.example/v1", "https://b.example/v1"]
        );
        // A provider with no endpoint contributes none, rather than a blank.
        let g = Fallback::new(p("x", ok), p("y", ok));
        assert!(g.endpoints().is_empty());
    }
}
