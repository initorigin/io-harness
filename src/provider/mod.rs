//! The provider layer — provider-agnostic by design.
//!
//! No vendor type appears in these public types. A [`Provider`] takes a
//! [`CompletionRequest`] and returns a [`CompletionResponse`]; OpenRouter,
//! Anthropic, and OpenAI are implementation details behind the trait.

pub mod anthropic;
pub mod openai;
pub(crate) mod openai_wire;
pub mod openrouter;

pub mod fallback;
pub use anthropic::Anthropic;
pub use fallback::Fallback;
pub use openai::OpenAi;
pub use openrouter::OpenRouter;

use futures_util::StreamExt;

use crate::error::{Error, Result};

/// Turn a non-success response into a typed [`Error::Provider`], preserving the
/// status and the server's `Retry-After`.
///
/// Every provider funnels through here rather than inspecting `status()` itself:
/// the three did it identically before this existed, and one place is what stops
/// them from drifting into disagreeing about what a 429 means.
pub(crate) async fn ensure_success(resp: reqwest::Response) -> Result<reqwest::Response> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    // Read the header before the body: `text()` consumes the response.
    let retry_after = crate::net::retry_after(resp.headers());
    let detail = resp.text().await.unwrap_or_default();
    let detail = detail.trim();
    Err(Error::provider_status(
        status.as_u16(),
        retry_after,
        if detail.is_empty() {
            status.canonical_reason().unwrap_or("no detail").to_string()
        } else {
            detail.to_string()
        },
    ))
}

/// Reject a response that parsed to nothing at all.
///
/// A stream that completed but yielded no text, no tool call and no usage is a
/// failure the loop cannot see: [`CompletionResponse::default`] reads exactly
/// like "the model chose not to call a tool", so a truncated or garbled transfer
/// ends the run as if the model had decided to stop. Naming it
/// [`crate::error::ProviderErrorKind::Malformed`] makes it retryable instead of
/// invisible.
///
/// A response with text and no tool call is *not* this: that is a model that
/// answered without calling anything, and it keeps its meaning exactly.
pub(crate) fn ensure_parsed(response: CompletionResponse) -> Result<CompletionResponse> {
    if response.text.is_none() && response.tool_calls.is_empty() && response.usage.is_none() {
        return Err(Error::provider_malformed(
            "the response stream yielded no text, no tool call and no usage",
        ));
    }
    Ok(response)
}

/// Read an SSE byte stream line by line, handing each `data:` payload (the text
/// after `data:`) to `ingest`. `ingest` returns `true` to stop early on a
/// provider's terminal event. Shared by every provider so the transport lives
/// in one place; each provider supplies its own JSON accumulation.
pub(crate) async fn read_sse<F>(resp: reqwest::Response, mut ingest: F) -> Result<()>
where
    F: FnMut(&str) -> bool,
{
    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    while let Some(chunk) = stream.next().await {
        // A mid-stream byte error is Transport; a stream that stalls past the
        // client's deadline is Timeout. `From<reqwest::Error>` decides both.
        let chunk = chunk?;
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(nl) = buf.find('\n') {
            let line = buf[..nl].trim_end_matches('\r').to_string();
            buf.drain(..=nl);
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() {
                continue;
            }
            if ingest(data) {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// A tool the model may call, described in a vendor-neutral shape.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolSpec {
    /// Tool name the model calls.
    pub name: String,
    /// What the tool does, for the model.
    pub description: String,
    /// JSON Schema of the arguments object.
    pub parameters: serde_json::Value,
}

/// A request for one model completion.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CompletionRequest {
    /// System instructions.
    pub system: String,
    /// The user turn.
    pub user: String,
    /// Tools the model may call.
    pub tools: Vec<ToolSpec>,
}

/// A tool call the model decided to make.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolCall {
    /// Tool name.
    pub name: String,
    /// Parsed arguments object.
    pub arguments: serde_json::Value,
}

/// Token usage for one completion, in a vendor-neutral shape. Used to enforce
/// the cost budget and to record spend in the trace.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Usage {
    /// Tokens in the prompt.
    pub prompt_tokens: u64,
    /// Tokens the model generated.
    pub completion_tokens: u64,
    /// Total tokens billed for this completion.
    pub total_tokens: u64,
}

/// One model completion.
///
/// Construct with `..Default::default()` for forward compatibility — fields are
/// added in minor releases (e.g. `usage` in 0.2.0).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CompletionResponse {
    /// Any free text the model returned.
    pub text: Option<String>,
    /// Tool calls the model requested, in order.
    pub tool_calls: Vec<ToolCall>,
    /// Token usage, when the provider reports it. `None` if unknown.
    pub usage: Option<Usage>,
}

/// Anything that can turn a [`CompletionRequest`] into a [`CompletionResponse`].
///
/// Implemented by [`OpenRouter`], [`Anthropic`], and [`OpenAi`]; tests supply
/// their own to run the loop offline. Selecting a provider is just constructing
/// a different implementer and handing it to [`crate::run`] — no vendor type
/// appears in the task contract.
pub trait Provider {
    /// Perform one completion.
    fn complete(
        &self,
        request: CompletionRequest,
    ) -> impl std::future::Future<Output = Result<CompletionResponse>> + Send;

    /// A short label recorded in the run's trace so an audit shows which
    /// provider ran. Defaults to `"provider"` so existing implementers keep
    /// compiling; the built-in providers override it.
    fn name(&self) -> &str {
        "provider"
    }

    /// The URL this provider dials, if it dials one.
    ///
    /// The run authorizes this against the policy's [`crate::Act::Net`] rules
    /// before the first completion, and contributes its host as the named
    /// `provider` layer so a network-deny base can still reach its model.
    ///
    /// Defaults to `None`, which means "opens no connection" — the honest answer
    /// for the mock providers tests drive the loop with, and what keeps every
    /// existing implementer compiling. A `None` provider is not exempt from the
    /// boundary; it simply has no connection for the boundary to govern.
    fn endpoint(&self) -> Option<&str> {
        None
    }

    /// Every host this provider may dial, for the 0.8.0 egress policy to authorize
    /// before the run's first step.
    ///
    /// Defaults to whatever [`endpoint`](Provider::endpoint) reports, so an existing
    /// implementation needs no change. A combinator that can reach more than one
    /// host — [`Fallback`] — overrides it, because reporting only the first would
    /// leave the rest ungoverned by a policy that is deny-by-default everywhere
    /// else.
    fn endpoints(&self) -> Vec<&str> {
        self.endpoint().into_iter().collect()
    }

    /// The provider that actually answered the last call, when this is a combinator
    /// and the answer is not obvious from configuration.
    ///
    /// `None` for a plain provider: [`name`](Provider::name) already says who it
    /// was. [`Fallback`] returns whichever of its two served, so the run loop can
    /// record it per step rather than recording one label for the whole run.
    fn last_served(&self) -> Option<String> {
        None
    }
}

/// Every provider failure, against a real socket.
///
/// These live here rather than in `tests/` because the endpoint override the
/// fixtures need is crate-internal — the public API pins each provider to its
/// vendor's URL, and a test that mocked the error instead of serving it would not
/// exercise the status parsing, the header parsing, or the deadline.
#[cfg(test)]
mod failures {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    use super::*;
    use crate::error::ProviderErrorKind as Kind;
    use crate::net::{http_date, unix_now};

    /// A local HTTP server that answers every connection with one canned raw
    /// response, then closes.
    fn serve(response: String) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                drain_request(&mut stream);
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        url
    }

    /// Read the request head and its body, so the client is never answered before
    /// its own write has been consumed — an unread body can surface as a reset
    /// instead of the status under test.
    fn drain_request(stream: &mut std::net::TcpStream) {
        let mut seen = Vec::new();
        let mut byte = [0u8; 1];
        while stream.read(&mut byte).unwrap_or(0) == 1 {
            seen.push(byte[0]);
            if seen.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let head = String::from_utf8_lossy(&seen).to_ascii_lowercase();
        let len: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; len];
        let _ = stream.read_exact(&mut body);
    }

    /// A status response with `extra` header lines (each already `\r\n`-free).
    fn status_response(status: &str, extra: &[&str]) -> String {
        let body = "{\"error\":\"nope\"}";
        let mut head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n", body.len());
        for line in extra {
            head.push_str(line);
            head.push_str("\r\n");
        }
        format!("{head}\r\n{body}")
    }

    /// A 200 whose body is `events`, delimited by the close rather than a length —
    /// what a real streamed response looks like.
    fn stream_response(events: &str) -> String {
        format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{events}")
    }

    fn request() -> CompletionRequest {
        CompletionRequest {
            system: "s".into(),
            user: "u".into(),
            tools: Vec::new(),
        }
    }

    /// The kind and status of a failed call, so no assertion reads the rendered
    /// string.
    fn failure(result: Result<CompletionResponse>) -> (Kind, Option<u16>, Option<Duration>) {
        match result {
            Err(Error::Provider {
                kind,
                status,
                retry_after,
                ..
            }) => (kind, status, retry_after),
            other => panic!("expected a provider error, got {other:?}"),
        }
    }

    /// A provider pointed at `url`, one second of patience.
    fn openrouter(url: &str) -> OpenRouter {
        OpenRouter::at(url, Duration::from_secs(1))
    }

    #[tokio::test]
    async fn a_refused_connection_is_transport() {
        // Bind then drop: nothing is listening on that port now.
        let dead = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", dead.local_addr().unwrap());
        drop(dead);

        let (kind, status, _) = failure(openrouter(&url).complete(request()).await);
        // `Transport` on unix, where the stack refuses immediately. On Windows a
        // connect to a closed local port is retransmitted rather than refused, so
        // the client's own deadline fires first and the kind is `Timeout`. Both mean
        // the request never reached a model and both are retryable, which is the
        // property that matters; which of the two the OS reports is not ours to
        // decide, and asserting one made the Windows leg red for a platform
        // difference rather than a defect.
        assert!(
            matches!(kind, Kind::Transport | Kind::Timeout),
            "a connection that never opened must be Transport or Timeout, got {kind:?}"
        );
        assert!(
            kind.is_retryable(),
            "a connection that never opened is worth another attempt"
        );
        assert_eq!(
            status, None,
            "a connection that never happened has no status"
        );
        assert!(kind.is_retryable());
    }

    /// F3 — a socket that accepts and then never writes ends as `Timeout`, not as
    /// a run that hangs forever.
    #[tokio::test]
    async fn a_server_that_accepts_and_never_answers_ends_as_a_timeout() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/v1", listener.local_addr().unwrap());
        // Hold the accepted connection open and write nothing to it.
        std::thread::spawn(move || {
            let held: Vec<_> = listener.incoming().filter_map(|s| s.ok()).collect();
            std::thread::sleep(Duration::from_secs(30));
            drop(held);
        });

        let started = std::time::Instant::now();
        let (kind, status, _) = failure(
            OpenRouter::at(&url, Duration::from_millis(300))
                .complete(request())
                .await,
        );
        assert_eq!(kind, Kind::Timeout);
        assert_eq!(status, None);
        assert!(kind.is_retryable());
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the deadline, not the server, ended the call"
        );
    }

    #[tokio::test]
    async fn a_rate_limit_without_a_retry_after_is_rate_limited_and_carries_no_wait() {
        let url = serve(status_response("429 Too Many Requests", &[]));
        let (kind, status, retry_after) = failure(openrouter(&url).complete(request()).await);
        assert_eq!(kind, Kind::RateLimited);
        assert_eq!(status, Some(429));
        assert_eq!(retry_after, None);
    }

    #[tokio::test]
    async fn a_rate_limit_with_delta_seconds_keeps_the_wait_the_server_asked_for() {
        let url = serve(status_response(
            "429 Too Many Requests",
            &["Retry-After: 11"],
        ));
        let (kind, status, retry_after) = failure(openrouter(&url).complete(request()).await);
        assert_eq!(kind, Kind::RateLimited);
        assert_eq!(status, Some(429));
        assert_eq!(retry_after, Some(Duration::from_secs(11)));
    }

    #[tokio::test]
    async fn a_rate_limit_with_an_http_date_keeps_the_wait_until_that_date() {
        let header = format!("Retry-After: {}", http_date(unix_now() + 45));
        let url = serve(status_response("429 Too Many Requests", &[&header]));
        let (kind, status, retry_after) = failure(openrouter(&url).complete(request()).await);
        assert_eq!(kind, Kind::RateLimited);
        assert_eq!(status, Some(429));
        let waited = retry_after.expect("the date is a wait");
        assert!(
            waited > Duration::from_secs(40) && waited <= Duration::from_secs(45),
            "{waited:?}"
        );
    }

    #[tokio::test]
    async fn a_server_error_is_server_and_retryable() {
        let url = serve(status_response("503 Service Unavailable", &[]));
        let (kind, status, _) = failure(openrouter(&url).complete(request()).await);
        assert_eq!(kind, Kind::Server);
        assert_eq!(status, Some(503));
        assert!(kind.is_retryable());
    }

    #[tokio::test]
    async fn a_rejected_key_is_auth_and_not_retryable() {
        let url = serve(status_response("401 Unauthorized", &[]));
        let (kind, status, _) = failure(openrouter(&url).complete(request()).await);
        assert_eq!(kind, Kind::Auth);
        assert_eq!(status, Some(401));
        assert!(!kind.is_retryable(), "a wrong key stays wrong");
    }

    #[tokio::test]
    async fn a_bad_request_is_request_and_not_retryable() {
        let url = serve(status_response("400 Bad Request", &[]));
        let (kind, status, _) = failure(openrouter(&url).complete(request()).await);
        assert_eq!(kind, Kind::Request);
        assert_eq!(status, Some(400));
        assert!(!kind.is_retryable(), "the same request fails the same way");
    }

    #[tokio::test]
    async fn a_stream_that_parses_to_nothing_is_malformed_not_an_empty_answer() {
        let url = serve(stream_response(
            "data: not json at all\n\ndata: {\"unterminated\n\n",
        ));
        let (kind, status, _) = failure(openrouter(&url).complete(request()).await);
        assert_eq!(kind, Kind::Malformed);
        assert_eq!(status, None, "the status was fine; the body was not");
        assert!(kind.is_retryable(), "re-asking is cheap");
    }

    /// The other half of the pair: a stream that parses and simply contains no
    /// tool call keeps its meaning. Collapsing these two in either direction is
    /// the regression this test exists to catch.
    #[tokio::test]
    async fn a_stream_with_text_and_no_tool_call_stays_a_quiet_success() {
        let url = serve(stream_response(
            "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\ndata: [DONE]\n\n",
        ));
        let out = openrouter(&url).complete(request()).await.unwrap();
        assert_eq!(out.text.as_deref(), Some("done"));
        assert!(out.tool_calls.is_empty());
    }

    /// Usage alone is enough to have parsed: a provider that streams only its
    /// usage summary reported something, so it is not malformed.
    #[tokio::test]
    async fn a_stream_carrying_only_usage_is_not_malformed() {
        let url = serve(stream_response(
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":0,\"total_tokens\":3}}\n\ndata: [DONE]\n\n",
        ));
        let out = openrouter(&url).complete(request()).await.unwrap();
        assert_eq!(out.usage.unwrap().total_tokens, 3);
    }

    #[tokio::test]
    async fn anthropics_own_stream_shape_is_held_to_the_same_two_meanings() {
        let empty = serve(stream_response("data: {\"type\":\"whatever\"}\n\n"));
        let (kind, _, _) = failure(
            Anthropic::at(&empty, Duration::from_secs(1))
                .complete(request())
                .await,
        );
        assert_eq!(kind, Kind::Malformed);

        let quiet = serve(stream_response(
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n",
        ));
        let out = Anthropic::at(&quiet, Duration::from_secs(1))
            .complete(request())
            .await
            .unwrap();
        assert_eq!(out.text.as_deref(), Some("hi"));
        assert!(out.tool_calls.is_empty());
    }

    /// The three providers were byte-identical here before the taxonomy existed
    /// and must stay so: one status, one kind, whoever asked.
    #[tokio::test]
    async fn all_three_providers_map_a_status_to_the_same_kind() {
        for (status, want) in [
            ("429 Too Many Requests", Kind::RateLimited),
            ("503 Service Unavailable", Kind::Server),
            ("403 Forbidden", Kind::Auth),
            ("404 Not Found", Kind::Request),
        ] {
            let url = serve(status_response(status, &["Retry-After: 3"]));
            let code: u16 = status[..3].parse().unwrap();
            let timeout = Duration::from_secs(1);

            let seen = [
                failure(OpenRouter::at(&url, timeout).complete(request()).await),
                failure(OpenAi::at(&url, timeout).complete(request()).await),
                failure(Anthropic::at(&url, timeout).complete(request()).await),
            ];
            for observed in &seen {
                assert_eq!(
                    *observed,
                    (want, Some(code), Some(Duration::from_secs(3))),
                    "{status}"
                );
            }
        }
    }
}
