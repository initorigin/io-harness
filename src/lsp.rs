//! A language-server client, so the agent asks the questions an editor answers.
//!
//! Until 0.52.0 the only way an agent learned where a symbol was defined was to
//! grep for the spellings a definition might have, read the files that matched,
//! and decide which hit was the definition and which were uses. Every one of those
//! is a provider round trip carrying the whole system prefix, and the answer at the
//! end is a text match that resembles a resolution rather than one. A language
//! server has resolved it already.
//!
//! ## Written here, over a byte stream
//!
//! The protocol is three things: `Content-Length: N\r\n\r\n` framing, JSON-RPC 2.0
//! correlated by `id`, and a handshake. That is a few hundred lines against
//! `serde_json`, which this crate already depends on, so no client crate, no
//! JSON-RPC crate and no `lsp-types` — the dependency discipline this crate has kept
//! since 0.1.0 is worth more than the six request bodies below.
//!
//! [`Client`] is written over `AsyncRead + AsyncWrite` rather than over a child
//! process, and the spawn is a thin wrapper. That is not abstraction for its own
//! sake: it is what lets the tests drive a server that misbehaves on purpose —
//! answering out of order, interleaving notifications, omitting a capability,
//! hanging the handshake — over an in-process pipe, on every platform, with no
//! binary installed and no cold start. None of that is expressible against a real
//! server, and a real server's index is minutes.
//!
//! ## What arrives that is not an answer
//!
//! A server sends `window/logMessage`, `$/progress` and `textDocument/publishDiagnostics`
//! unprompted, from the first request onward. A client that treats the next message
//! as its answer works until the first one of those arrives, which is why every
//! response here is matched by `id` and everything else is dropped. A server
//! *request* — `workspace/configuration`, `client/registerCapability` — carries an
//! `id` too, and is answered `null` rather than dropped: a server waiting forever
//! for a reply is a run that hangs for a reason no log explains.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;

use crate::error::{Error, Result};

/// The frame header this protocol uses, lowercased for comparison.
const CONTENT_LENGTH: &str = "content-length:";

/// Frame one JSON body the way the protocol requires.
///
/// The length is the body's **byte** count, not its character count, and the
/// header terminator is `\r\n` twice. Both are the mistakes that pass every test
/// written on the host that wrote the fixture.
pub(crate) fn frame(body: &str) -> Vec<u8> {
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

/// Read one frame, or `None` at a clean end of stream.
///
/// Headers are read line by line and split by hand. `str::lines()` is not used
/// here for the reason 0.51.0's patch parser does not use it: it strips a trailing
/// carriage return, and a protocol whose terminator *is* the carriage return
/// cannot afford a helper that silently removes it.
async fn read_frame<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> Result<Option<Value>> {
    let mut len: Option<usize> = None;
    loop {
        let mut line = Vec::new();
        let read = reader.read_until(b'\n', &mut line).await?;
        if read == 0 {
            // End of stream. Mid-header is a truncated frame and is an error;
            // before any header is a server that closed, which is not.
            return if len.is_none() && line.is_empty() {
                Ok(None)
            } else {
                Err(protocol("stream ended inside a frame header"))
            };
        }
        if line == b"\r\n" || line == b"\n" {
            break;
        }
        let text = String::from_utf8_lossy(&line);
        let (name, value) = match text.split_once(':') {
            Some((n, v)) => (n, v),
            // A header line with no colon is not a header. Refused by name
            // rather than skipped, because a client that skips what it does not
            // understand desynchronises silently.
            None => return Err(protocol(&format!("malformed frame header: {:?}", text.trim()))),
        };
        if name.to_ascii_lowercase() + ":" == CONTENT_LENGTH {
            len = Some(value.trim().parse().map_err(|_| {
                protocol(&format!("Content-Length is not a number: {:?}", value.trim()))
            })?);
        }
    }
    let len = len.ok_or_else(|| protocol("frame has no Content-Length"))?;
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    let value: Value = serde_json::from_slice(&body)
        .map_err(|e| protocol(&format!("frame body is not JSON: {e}")))?;
    Ok(Some(value))
}

fn protocol(reason: &str) -> Error {
    Error::Lsp {
        server: String::new(),
        reason: reason.to_string(),
    }
}

/// One connected language server, correlated by request id.
///
/// The reader runs as its own task for the life of the client: a response can
/// arrive while nothing is awaiting it, and a notification arrives when the server
/// feels like it, so there is no point in the stream at which "read the next
/// message" belongs to one caller.
pub(crate) struct Client {
    /// The configured id, carried so every error names the server the operator wrote.
    id: String,
    writer: Arc<tokio::sync::Mutex<Box<dyn AsyncWrite + Unpin + Send>>>,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
    next_id: AtomicI64,
    reader: tokio::task::JoinHandle<()>,
    /// Why the reader stopped, if it did. Read when a request finds its channel
    /// closed, so "the server exited" is reported instead of "channel closed".
    gone: Arc<Mutex<Option<String>>>,
}

impl Client {
    /// Take a duplex pair and start reading.
    pub(crate) fn over<R, W>(id: impl Into<String>, read: R, write: W) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let id = id.into();
        let pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>> = Arc::default();
        let gone: Arc<Mutex<Option<String>>> = Arc::default();
        let writer: Arc<tokio::sync::Mutex<Box<dyn AsyncWrite + Unpin + Send>>> =
            Arc::new(tokio::sync::Mutex::new(Box::new(write)));

        let reader = tokio::spawn(read_loop(
            BufReader::new(read),
            Arc::clone(&pending),
            Arc::clone(&writer),
            Arc::clone(&gone),
        ));

        Self {
            id,
            writer,
            pending,
            next_id: AtomicI64::new(1),
            reader,
            gone,
        }
    }

    /// Send a request and wait for the response with that id.
    pub(crate) async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("pending map is not poisoned")
            .insert(id, tx);

        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(e) = self.send(&body).await {
            self.pending
                .lock()
                .expect("pending map is not poisoned")
                .remove(&id);
            return Err(e);
        }

        let answer = match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(v)) => v,
            // The sender was dropped, which happens only when the reader stopped.
            Ok(Err(_)) => {
                self.pending
                    .lock()
                    .expect("pending map is not poisoned")
                    .remove(&id);
                let why = self
                    .gone
                    .lock()
                    .expect("reason is not poisoned")
                    .clone()
                    .unwrap_or_else(|| "the server closed its output".into());
                return Err(self.fail(&format!("{method} was not answered: {why}")));
            }
            Err(_) => {
                self.pending
                    .lock()
                    .expect("pending map is not poisoned")
                    .remove(&id);
                return Err(self.fail(&format!(
                    "{method} did not answer within {}s",
                    timeout.as_secs()
                )));
            }
        };

        if let Some(error) = answer.get("error") {
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("no message");
            return Err(self.fail(&format!("{method} failed: {message}")));
        }
        Ok(answer.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Send a notification, which by definition is never answered.
    pub(crate) async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    async fn send(&self, body: &Value) -> Result<()> {
        let text = serde_json::to_string(body).map_err(|e| self.fail(&format!("{e}")))?;
        let mut writer = self.writer.lock().await;
        writer
            .write_all(&frame(&text))
            .await
            .map_err(|e| self.fail(&format!("writing to the server failed: {e}")))?;
        writer
            .flush()
            .await
            .map_err(|e| self.fail(&format!("writing to the server failed: {e}")))
    }

    /// An error naming this server, which is the only kind this module returns
    /// once a client exists.
    fn fail(&self, reason: &str) -> Error {
        Error::Lsp {
            server: self.id.clone(),
            reason: reason.to_string(),
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// Read frames until the stream ends, routing each one.
async fn read_loop<R: AsyncRead + Unpin>(
    mut reader: BufReader<R>,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
    writer: Arc<tokio::sync::Mutex<Box<dyn AsyncWrite + Unpin + Send>>>,
    gone: Arc<Mutex<Option<String>>>,
) {
    let reason = loop {
        match read_frame(&mut reader).await {
            Ok(Some(message)) => {
                let id = message.get("id").and_then(Value::as_i64);
                let is_request = message.get("method").is_some();
                match (id, is_request) {
                    // A response: route it to whoever is waiting for that id.
                    (Some(id), false) => {
                        let waiting = pending
                            .lock()
                            .expect("pending map is not poisoned")
                            .remove(&id);
                        if let Some(tx) = waiting {
                            let _ = tx.send(message);
                        }
                    }
                    // A server request. Answered `null` rather than dropped: a
                    // server blocked on a reply is a hang with no explanation.
                    (Some(id), true) => {
                        let body = json!({"jsonrpc": "2.0", "id": id, "result": Value::Null});
                        if let Ok(text) = serde_json::to_string(&body) {
                            let mut w = writer.lock().await;
                            let _ = w.write_all(&frame(&text)).await;
                            let _ = w.flush().await;
                        }
                    }
                    // A notification. Nothing here subscribes to one.
                    _ => {}
                }
            }
            Ok(None) => break "the server closed its output".to_string(),
            Err(e) => break format!("{e}"),
        }
    };
    *gone.lock().expect("reason is not poisoned") = Some(reason);
    // Dropping the senders is what wakes every outstanding request.
    pending
        .lock()
        .expect("pending map is not poisoned")
        .clear();
}

/// A line or character number as a reader counts it, on the wire.
///
/// The protocol counts from zero and every surface a model reads — `read_file`,
/// a compiler diagnostic, a stack trace — counts from one. The conversion lives
/// here, in both directions, because an off-by-one produces the neighbouring
/// line, which is a wrong answer that reads exactly like a right one.
pub(crate) fn to_wire(one_based: u32) -> u32 {
    one_based.saturating_sub(1)
}

/// A line or character number from the wire, as a reader counts it.
pub(crate) fn from_wire(zero_based: u64) -> u32 {
    u32::try_from(zero_based.saturating_add(1)).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_is_lengthed_in_bytes_and_terminated_by_carriage_returns() {
        assert_eq!(frame("{}"), b"Content-Length: 2\r\n\r\n{}".to_vec());

        // Four characters, seven bytes. A client that lengths in `chars` writes
        // 4 here, and every reader after it is three bytes out of step.
        let body = "\"é€\"";
        assert_eq!(body.chars().count(), 4);
        assert_eq!(body.len(), 7);
        let framed = frame(body);
        assert!(
            framed.starts_with(b"Content-Length: 7\r\n\r\n"),
            "{:?}",
            String::from_utf8_lossy(&framed)
        );
        assert_eq!(framed.len(), "Content-Length: 7\r\n\r\n".len() + 7);
    }

    /// Feed the reader one byte at a time. Every partial read lands mid-header
    /// and mid-body, which is what a real pipe does under load.
    #[tokio::test]
    async fn a_frame_split_across_chunks_reads_whole() {
        let (mut client, server) = tokio::io::duplex(64);
        let body = r#"{"jsonrpc":"2.0","id":1,"result":"é€"}"#;
        let bytes = frame(body);
        tokio::spawn(async move {
            for byte in bytes {
                client.write_all(&[byte]).await.unwrap();
                client.flush().await.unwrap();
                tokio::task::yield_now().await;
            }
        });
        let mut reader = BufReader::new(server);
        let message = read_frame(&mut reader).await.unwrap().unwrap();
        assert_eq!(message["result"], "é€");
    }

    #[tokio::test]
    async fn a_header_this_client_does_not_use_is_skipped_and_a_broken_one_is_named() {
        let framed = b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\n\
                       Content-Length: 2\r\n\r\n{}"
            .to_vec();
        let mut reader = BufReader::new(std::io::Cursor::new(framed));
        assert_eq!(read_frame(&mut reader).await.unwrap().unwrap(), json!({}));

        let mut reader = BufReader::new(std::io::Cursor::new(b"not a header\r\n\r\n{}".to_vec()));
        let err = read_frame(&mut reader).await.unwrap_err().to_string();
        assert!(err.contains("malformed frame header"), "{err}");
    }

    #[tokio::test]
    async fn a_clean_end_of_stream_is_not_an_error_and_a_truncated_frame_is() {
        let mut reader = BufReader::new(std::io::Cursor::new(Vec::new()));
        assert!(read_frame(&mut reader).await.unwrap().is_none());

        let mut reader = BufReader::new(std::io::Cursor::new(b"Content-Length: 9\r\n".to_vec()));
        assert!(read_frame(&mut reader).await.is_err());
    }

    /// A client and the server end of its pipe, split so each side can be read
    /// and written independently.
    fn paired() -> (
        Client,
        BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
        tokio::io::WriteHalf<tokio::io::DuplexStream>,
    ) {
        let (client_side, server_side) = tokio::io::duplex(8192);
        let (cr, cw) = tokio::io::split(client_side);
        let (sr, sw) = tokio::io::split(server_side);
        (Client::over("fix", cr, cw), BufReader::new(sr), sw)
    }

    async fn say<W: AsyncWrite + Unpin>(w: &mut W, body: Value) {
        w.write_all(&frame(&body.to_string())).await.unwrap();
        w.flush().await.unwrap();
    }

    /// The claim: a response is matched to its request by `id`, not by arrival.
    ///
    /// The server logs, reports progress, and answers the *second* question
    /// first. A client that takes the next message as its answer gives both
    /// callers the wrong value, and both callers are asserted.
    #[tokio::test]
    async fn answers_are_matched_by_id_through_interleaved_notifications() {
        let (client, mut sr, mut sw) = paired();

        let server = tokio::spawn(async move {
            let mut ids = Vec::new();
            for _ in 0..2 {
                let msg = read_frame(&mut sr).await.unwrap().unwrap();
                ids.push(msg["id"].as_i64().unwrap());
            }
            say(
                &mut sw,
                json!({"jsonrpc":"2.0","method":"window/logMessage",
                       "params":{"type":3,"message":"indexing"}}),
            )
            .await;
            say(
                &mut sw,
                json!({"jsonrpc":"2.0","method":"$/progress",
                       "params":{"token":"idx","value":{"kind":"begin"}}}),
            )
            .await;
            // Reverse order, which is the whole point.
            say(
                &mut sw,
                json!({"jsonrpc":"2.0","id":ids[1],"result":"second"}),
            )
            .await;
            say(
                &mut sw,
                json!({"jsonrpc":"2.0","id":ids[0],"result":"first"}),
            )
            .await;
            sw
        });

        let one = client.request("one", json!({}), Duration::from_secs(5));
        let two = client.request("two", json!({}), Duration::from_secs(5));
        let (one, two) = tokio::join!(one, two);
        assert_eq!(one.unwrap(), "first");
        assert_eq!(two.unwrap(), "second");
        let _ = server.await;
    }

    /// A server request carries an `id` and must not be mistaken for an answer —
    /// and must be answered, or the server waits forever.
    #[tokio::test]
    async fn a_server_request_is_answered_null_and_is_not_taken_for_a_response() {
        let (client, mut sr, mut sw) = paired();

        let server = tokio::spawn(async move {
            let asked = read_frame(&mut sr).await.unwrap().unwrap();
            // Ask the client something first, using an id it also uses.
            say(
                &mut sw,
                json!({"jsonrpc":"2.0","id":asked["id"],"method":"workspace/configuration",
                       "params":{"items":[]}}),
            )
            .await;
            let reply = read_frame(&mut sr).await.unwrap().unwrap();
            say(
                &mut sw,
                json!({"jsonrpc":"2.0","id":asked["id"],"result":"the answer"}),
            )
            .await;
            reply
        });

        let answer = client
            .request("ask", json!({}), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(answer, "the answer");
        let reply = server.await.unwrap();
        assert_eq!(reply["result"], Value::Null, "{reply}");
        assert!(reply.get("method").is_none(), "{reply}");
    }

    /// A server that dies mid-request is named, and does not hang the caller
    /// until its timeout.
    #[tokio::test]
    async fn a_server_that_closes_is_reported_by_name_rather_than_waited_out() {
        let (client, mut sr, sw) = paired();
        tokio::spawn(async move {
            let _ = read_frame(&mut sr).await;
            drop(sw);
        });
        let err = client
            // A timeout long enough that waiting it out would fail the test.
            .request("gone", json!({}), Duration::from_secs(120))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("language server fix"), "{err}");
        assert!(err.contains("closed its output"), "{err}");
    }

    /// An error object is the server's answer, not this client's failure, and it
    /// carries the server's own message.
    #[tokio::test]
    async fn an_error_response_names_what_the_server_said() {
        let (client, mut sr, mut sw) = paired();
        tokio::spawn(async move {
            let msg = read_frame(&mut sr).await.unwrap().unwrap();
            say(
                &mut sw,
                json!({"jsonrpc":"2.0","id":msg["id"],
                       "error":{"code":-32601,"message":"method not found"}}),
            )
            .await;
            // Held so the stream does not close and race the assertion.
            tokio::time::sleep(Duration::from_millis(50)).await;
        });
        let err = client
            .request("nope", json!({}), Duration::from_secs(5))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("method not found"), "{err}");
    }

    #[test]
    fn positions_convert_in_both_directions_and_the_wire_never_goes_negative() {
        assert_eq!(to_wire(1), 0);
        assert_eq!(to_wire(12), 11);
        // A model that sends 0 for a line no file has must not wrap to u32::MAX.
        assert_eq!(to_wire(0), 0);
        assert_eq!(from_wire(0), 1);
        assert_eq!(from_wire(11), 12);
    }
}
