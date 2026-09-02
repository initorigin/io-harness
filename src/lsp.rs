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
//! The client is written over `AsyncRead + AsyncWrite` rather than over a child
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

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::oneshot;

use crate::error::{Error, Result};

/// The per-request bound a server gets when its configuration names none.
///
/// Sixty seconds is the same default an MCP server gets, and for the same reason:
/// a third-party process that never answers must not become a run that never
/// ends. It is a *request* bound, not an indexing bound — the index is warmed in
/// the background from run start, and a request that arrives before it is ready
/// waits this long and then says why.
fn default_timeout_secs() -> u64 {
    60
}

/// One configured language server.
///
/// A server is named here or there is no server: nothing is downloaded at run
/// time, nothing is resolved from `PATH` by ecosystem, and a configured server
/// that is not installed is a refusal naming it rather than a silent fallback to
/// text search. Starting it is an [`Act::Exec`](crate::Act::Exec) check on
/// [`command`](Self::command), so configuring one here does not grant access to
/// it — without `allow_exec` naming that binary the run ends in
/// [`Error::Lsp`] before the process exists.
///
/// ```
/// use io_harness::LspServer;
///
/// let server = LspServer::new("rust", "rust-analyzer")
///     .with_extensions([".rs"])
///     // A server that never answers must not become a run that never ends.
///     .with_timeout(std::time::Duration::from_secs(30));
///
/// assert_eq!(server.id, "rust");
/// assert_eq!(server.timeout_secs, 30);
/// // Which files this server answers for. An empty list answers for every file,
/// // which is what a single-language project wants and what a mixed one does not.
/// assert_eq!(server.extensions, [".rs"]);
/// ```
///
/// `Serialize`/`Deserialize` because an application layer expresses these in its
/// own config files, the same way it already expresses an
/// [`McpServer`](crate::McpServer) or a [`Policy`](crate::Policy). Unlike
/// `McpServer` this carries `deny_unknown_fields`: there is no `#[serde(flatten)]`
/// here to forbid it, and a misspelled key in a table that names a program to
/// spawn is worth rejecting rather than ignoring.
///
/// `Debug` is hand-written and `Serialize` is not, exactly as on
/// [`McpServer`](crate::McpServer): [`args`](Self::args) and [`env`](Self::env)
/// have been through the configuration's `${env:}` substitution by the time they
/// are here, so they are redacted for a formatter and written back verbatim for
/// a tool that persists what the operator typed (0.71.0).
// `Debug` is hand-written below: a derived one prints `env` and `args` verbatim.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LspServer {
    /// Short name for this server, used in the trace and in every error about
    /// it. Keep it stable: it is what an operator reads to know which server
    /// answered.
    pub id: String,
    /// The server binary. Checked as [`Act::Exec`](crate::Act::Exec) before it is
    /// spawned.
    pub command: String,
    /// Arguments passed to it.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment for the child.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// The file suffixes this server answers for, e.g. `[".rs"]`.
    ///
    /// Empty answers for every file. Where two servers claim the same suffix the
    /// first in declaration order wins, which is a documented rule rather than a
    /// discovery.
    #[serde(default)]
    pub extensions: Vec<String>,
    /// Per-request timeout in seconds.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

impl std::fmt::Debug for LspServer {
    /// Everything an operator selects a server *by*, and the names — never the
    /// values — of what its child process was handed.
    ///
    /// `command` and `extensions` stay verbatim on the rule this crate's other
    /// redacting impls follow (see `mcp`): a value the harness executes or
    /// matches a filename against is already named back at the operator by an
    /// [`Act::Exec`](crate::Act::Exec) refusal or a "no server answers for this
    /// file" one, and it is what they debug with. `args` and `env` are the two
    /// documented ways to hand a child a credential, and a `${env:}` in this
    /// table is written for exactly that.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspServer")
            .field("id", &self.id)
            .field("command", &self.command)
            .field("args", &crate::mcp::RedactedArgs(self.args.len()))
            .field("env", &crate::mcp::RedactedMap(&self.env))
            .field("extensions", &self.extensions)
            .field("timeout_secs", &self.timeout_secs)
            .finish()
    }
}

impl LspServer {
    /// A server the harness spawns as a child process and speaks LSP to.
    pub fn new(id: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            command: command.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            extensions: Vec::new(),
            timeout_secs: default_timeout_secs(),
        }
    }

    /// Arguments to start it with.
    #[must_use]
    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Extra environment for the child.
    #[must_use]
    pub fn with_env<I, K, V>(mut self, env: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.env = env.into_iter().map(|(k, v)| (k.into(), v.into())).collect();
        self
    }

    /// Which file suffixes this server answers for.
    #[must_use]
    pub fn with_extensions<I, S>(mut self, extensions: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.extensions = extensions.into_iter().map(Into::into).collect();
        self
    }

    /// Bound every request to this server.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout_secs = timeout.as_secs();
        self
    }

    /// Whether this server answers for `path`.
    pub(crate) fn answers_for(&self, path: &str) -> bool {
        self.extensions.is_empty()
            || self.extensions.iter().any(|ext| {
                path.to_ascii_lowercase()
                    .ends_with(&ext.to_ascii_lowercase())
            })
    }

    /// The bound one request gets.
    pub(crate) fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs)
    }
}

/// The frame header this protocol uses, lowercased for comparison.
const CONTENT_LENGTH: &str = "content-length:";

/// The most bytes one frame body may claim before [`read_frame`] refuses it
/// (0.74.0, audit M12).
///
/// The length is chosen by the server, and the server is not trusted: a
/// repository decides which one a run spawns. Allocating what it asks for is an
/// abort on macOS and Windows and a pinned address space on Linux, from one
/// header line — and a stream that has desynchronised produces the same header
/// by accident, so this is reached without any malice at all.
///
/// Eight mebibytes is the ceiling `provider::MAX_RESPONSE_BYTES`
/// already puts on an untrusted body, and the same number is used here so the
/// crate has one answer to "how big may something a stranger sent get" rather
/// than two. Nothing a server legitimately sends approaches it: the largest
/// frames this client ever asks for are a whole-workspace `workspace/diagnostic`
/// answer and a `documentSymbol` tree, both of which are hundreds of kilobytes
/// of JSON on a large repository. A frame over this is refused by name rather
/// than truncated, because half a JSON body is not a message and a client that
/// resynchronises on a length it did not believe is guessing where the next
/// frame starts.
const MAX_FRAME: usize = 8 * 1024 * 1024;

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
            None => {
                return Err(protocol(&format!(
                    "malformed frame header: {:?}",
                    text.trim()
                )))
            }
        };
        if name.to_ascii_lowercase() + ":" == CONTENT_LENGTH {
            len = Some(value.trim().parse().map_err(|_| {
                protocol(&format!(
                    "Content-Length is not a number: {:?}",
                    value.trim()
                ))
            })?);
        }
    }
    let len = len.ok_or_else(|| protocol("frame has no Content-Length"))?;
    // Checked before the allocation, not after: the failure this stops is the
    // allocation itself, and a length read back off a refused frame is free.
    if len > MAX_FRAME {
        return Err(protocol(&format!(
            "frame claims {len} bytes, over the {MAX_FRAME}-byte ceiling this client allocates for"
        )));
    }
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

/// What a server said when it finished its handshake, or why it never did.
type Handshake = Option<std::result::Result<Value, String>>;

/// One spawned server and everything the run needs to talk to it.
struct Started {
    config: LspServer,
    client: Client,
    /// Set once by the background handshake task. `None` until it finishes.
    ready: tokio::sync::watch::Receiver<Handshake>,
    /// The child, kept so shutdown can end it even if it ignores `exit`.
    child: tokio::sync::Mutex<tokio::process::Child>,
    /// Paths this client has told the server about, so a re-sync knows whether
    /// to close first.
    opened: tokio::sync::Mutex<std::collections::HashSet<String>>,
    /// The root the server was told to index, carried into the event so a trace
    /// says what this server was pointed at.
    root: String,
    /// When the spawn happened, so `ready_ms` is measured rather than guessed.
    spawned: std::time::Instant,
    /// Whether [`EventKind::LspStarted`](crate::EventKind::LspStarted) has been
    /// emitted for this server yet.
    announced: std::sync::atomic::AtomicBool,
    /// Whether this server has been observed to finish its start-up work once.
    /// After that it stays finished, so only the first empty answer of a run ever
    /// pays for the wait.
    warm: std::sync::atomic::AtomicBool,
}

/// Every language server a run configured, for the life of that run.
///
/// The shape is [`McpSession`](crate::mcp)'s, deliberately: a configured child
/// process, gated on its argv, started at run start and ended with the run. The
/// one difference is where the waiting happens. An MCP server's handshake is
/// awaited before the run's first step, because its *tool list* is part of the
/// prompt. A language server's index is minutes on a real repository and no
/// prompt depends on it, so the handshake runs in the background from the moment
/// the child exists and the first navigation call is what waits.
pub(crate) struct LspSession {
    servers: Vec<Started>,
}

impl LspSession {
    /// Spawn every configured server, checking each against `policy` first.
    ///
    /// A server that cannot be spawned fails the run with [`Error::Lsp`] rather
    /// than being skipped, for the reason an MCP server does: silently navigating
    /// by text search while the operator believes a language server is answering
    /// is the worse failure, because the run looks successful.
    pub(crate) async fn connect(
        servers: &[LspServer],
        policy: &crate::Policy,
        root: &std::path::Path,
        store: &crate::Store,
        run_id: i64,
        watch: &crate::run::Watch<'_>,
    ) -> Result<Self> {
        let mut started = Vec::new();
        for config in servers {
            crate::mcp::authorize_spawn(&config.command, policy, store, run_id, watch)?;

            let mut cmd = tokio::process::Command::new(&config.command);
            cmd.args(&config.args)
                .current_dir(root)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                // A language server logs to stderr freely and nothing here reads
                // it. Inheriting it would put a server's chatter in the host
                // application's own output.
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true);
            for (k, v) in &config.env {
                cmd.env(k, v);
            }
            let mut child = cmd.spawn().map_err(|e| Error::Lsp {
                server: config.id.clone(),
                reason: format!("could not spawn {}: {e}", config.command),
            })?;
            let stdin = child.stdin.take().expect("stdin was piped");
            let stdout = child.stdout.take().expect("stdout was piped");
            let client = Client::over(&config.id, stdout, stdin);

            let (tx, ready) = tokio::sync::watch::channel(None);
            handshake(&client, root, config.timeout(), tx);

            started.push(Started {
                config: config.clone(),
                client,
                ready,
                child: tokio::sync::Mutex::new(child),
                opened: tokio::sync::Mutex::new(std::collections::HashSet::new()),
                root: root.to_string_lossy().into_owned(),
                spawned: std::time::Instant::now(),
                announced: std::sync::atomic::AtomicBool::new(false),
                warm: std::sync::atomic::AtomicBool::new(false),
            });
        }
        Ok(Self { servers: started })
    }

    /// Whether any server is configured. The five tool schemas are registered
    /// only when this is true, which is what keeps an unconfigured run's prompt
    /// byte-identical to 0.51.0's.
    pub(crate) fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// The server that answers for `path`, first in declaration order.
    ///
    /// First match wins where two servers claim one suffix. That is a documented
    /// rule rather than a discovery: refusing the configuration would be the
    /// other defensible answer, and it turns a working setup into a startup
    /// failure over an ambiguity the operator can simply not create.
    fn server_for(&self, path: Option<&str>) -> Option<&Started> {
        match path {
            Some(path) => self.servers.iter().find(|s| s.config.answers_for(path)),
            // A workspace-wide question names no file, so no `extensions` list can
            // match one. Asking the first configured server is the answer; matching
            // the empty string against them was a bug the live run found, under
            // which `lsp_symbols` with a query said no server answered while five
            // other tools were being answered by one.
            None => self.servers.first(),
        }
    }

    /// Wait for a server's handshake and hand back its capabilities.
    ///
    /// This is where the background start is paid for, and where an unready
    /// server becomes a reason rather than an empty answer. The wait is bounded
    /// by the server's own configured timeout.
    ///
    /// [`EventKind::LspStarted`](crate::EventKind::LspStarted) is emitted here,
    /// the first time a run observes a server usable, rather than from the
    /// handshake task — a `&Store` cannot cross a task boundary, and a number
    /// re-read later is not the one that was measured.
    async fn ready(
        &self,
        server: &Started,
        run_id: i64,
        watch: &crate::run::Watch<'_>,
    ) -> Result<Value> {
        let mut rx = server.ready.clone();
        let waited = tokio::time::timeout(server.config.timeout(), rx.wait_for(Option::is_some))
            .await
            .map_err(|_| Error::Lsp {
                server: server.config.id.clone(),
                reason: format!(
                    "was still starting up after {}s, so this question was not asked",
                    server.config.timeout_secs
                ),
            })?;
        let seen = waited
            .map_err(|_| Error::Lsp {
                server: server.config.id.clone(),
                reason: "stopped before it finished starting up".into(),
            })?
            .clone();
        let caps = match seen {
            Some(Ok(caps)) => caps,
            Some(Err(reason)) => {
                return Err(Error::Lsp {
                    server: server.config.id.clone(),
                    reason: format!("did not start: {reason}"),
                })
            }
            // `wait_for` returned, so the value is set.
            None => unreachable!("wait_for returned on a value that is still None"),
        };
        if !server
            .announced
            .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            watch.emit(crate::observe::RunEvent::new(
                run_id,
                0,
                crate::observe::EventKind::LspStarted {
                    server: server.config.id.clone(),
                    root: server.root.clone(),
                    ready_ms: u64::try_from(server.spawned.elapsed().as_millis())
                        .unwrap_or(u64::MAX),
                },
            ));
        }
        Ok(caps)
    }

    /// Answer one navigation question, or say why it could not be answered.
    ///
    /// Every navigation tool comes through here, and that is deliberate: the
    /// `deny_read` filter below is one check on one path, and a filter written
    /// four times is a filter missing from one of them.
    ///
    /// **The model's `path` is checked for [`Act::Read`](crate::Act::Read) before
    /// anything reads it** (0.74.0, audit H7). Until this release the path was
    /// only ever `ws.root().join`ed — which discards the root outright when the
    /// argument is absolute, and climbs without bound when it is `../..` — and
    /// then handed to `read_to_string` and shipped to the server as a `didOpen`.
    /// A `lsp_hover {"path": "../../../../etc/shadow"}` moved a file across the
    /// boundary that `read_file` would have refused, and left no row saying so.
    /// The check is here rather than at the five call sites for the reason the
    /// filter below is: a boundary written five times is a boundary missing from
    /// one of them.
    ///
    /// `Ask` passes and only `Deny` refuses, which is the bar
    /// [`Workspace::read_file`](crate::tools::Workspace::read_file) is held to
    /// once its gate has run. Nothing on this path can ask anybody: `navigate`
    /// has no approver, so requiring `Allow` would turn every policy that *asks*
    /// about reads into one where navigation answers nothing — a refusal the
    /// operator never wrote. `Deny` is the verdict that was already unenforced
    /// here, and enforcing it is the narrowing this release is allowed to make.
    /// A path that cannot be resolved under the root at all is a `Deny` no layer
    /// wrote; see [`Workspace::check_path`](crate::tools::Workspace::check_path).
    pub(crate) async fn navigate(
        &self,
        ask: Nav<'_>,
        ws: &crate::tools::Workspace,
        run_id: i64,
        watch: &crate::run::Watch<'_>,
    ) -> Result<String> {
        let path = ask.path();
        if let Some(path) = path {
            // `check_path` refuses everything `Workspace::resolve` refuses — an
            // absolute path, and a `..` that climbs out whether or not the file
            // exists — before it grades anything, so the resolution check and the
            // policy check are this one call rather than two that could drift.
            let verdict = ws.check_path(crate::Act::Read, path);
            if verdict.effect == crate::Effect::Deny {
                return Err(Error::Refused {
                    act: "read".to_string(),
                    target: path.to_string(),
                    rule: verdict.rule,
                    layer: verdict.layer,
                });
            }
        }
        let server = match self.server_for(path) {
            Some(s) => s,
            None => {
                return Err(Error::Lsp {
                    server: String::new(),
                    reason: format!(
                        "no configured server answers for {}",
                        path.unwrap_or("this workspace")
                    ),
                })
            }
        };
        let caps = self.ready(server, run_id, watch).await?;
        let capability = ask.capability();
        if caps
            .get(capability)
            .is_none_or(|v| v == &Value::Bool(false))
        {
            return Err(Error::Lsp {
                server: server.config.id.clone(),
                reason: format!(
                    "does not advertise {capability}, so this question has no answer from it \
                     rather than an empty one"
                ),
            });
        }

        // The document the question is about, re-sent from disk. See `sync`.
        if let Some(path) = path {
            self.sync(server, ws, path).await?;
        }
        let mut result = server
            .client
            .request(ask.method(), ask.params(ws.root()), server.config.timeout())
            .await?;

        // **An empty answer from a server that has not finished starting up is
        // indistinguishable from an empty answer from one that has**, because the
        // protocol has no readiness signal and a busy server answers `[]` rather
        // than erroring. This is the live run's central finding: against a real
        // `rust-analyzer`, `documentSymbol` — which needs only the syntax tree —
        // answered correctly while `definition`, `references` and `hover` all came
        // back empty, and nothing in the response said why.
        //
        // So an empty answer is not believed the first time. The run waits for the
        // work the server announced to finish, and asks once more. A server that
        // announces nothing pays one grace period, once, and a server that has
        // already settled pays nothing at all.
        // Rounds rather than one retry, because a server reports its start-up as a
        // *series* of jobs and the set of outstanding work empties in the gaps
        // between them. Each round waits for the work announced so far to finish
        // and asks again. A server that has announced nothing at all stops after
        // one round — there is no reason to expect a different answer from a
        // server that is not doing anything.
        //
        // The loop is bounded by the server's own configured timeout rather than
        // by a round count: how long a start-up takes is a property of the
        // repository, and a number picked here would be a guess about someone
        // else's machine. `timeout_secs` is the knob, and it is already the knob
        // for everything else this server does.
        if !server.warm.load(std::sync::atomic::Ordering::Relaxed) {
            let until = std::time::Instant::now() + server.config.timeout();
            while std::time::Instant::now() < until {
                if !is_empty(&result) {
                    break;
                }
                let busy = server.client.announced_work();
                if !server.client.quiet(server.config.timeout()).await {
                    break;
                }
                if !busy && !server.client.announced_work() {
                    break;
                }
                if let Some(path) = path {
                    self.sync(server, ws, path).await?;
                }
                result = server
                    .client
                    .request(ask.method(), ask.params(ws.root()), server.config.timeout())
                    .await?;
            }
            // Warm once an answer arrives, so only the first question of a run
            // ever pays for a server's start-up.
            if !is_empty(&result) {
                server
                    .warm
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        Ok(render(&ask, &result, ws))
    }

    /// Tell the server what a file says *now*.
    ///
    /// The server's view of a file is whatever was last sent to it, so a run that
    /// edited a file and then asks about it must not be answered from the text as
    /// it was before its own edit. Re-opening is one file re-parsed and leaves the
    /// index warm; tracking changes incrementally would mean this crate keeping a
    /// second copy of the workspace correct.
    ///
    /// **`path` must already have been checked for
    /// [`Act::Read`](crate::Act::Read).** This is where a file's bytes leave the
    /// boundary — read from disk and sent to a third-party process — and the join
    /// below is a plain `join`, which an absolute `path` replaces outright. Both
    /// callers check first: [`LspSession::navigate`] does it at the top for every
    /// navigation tool, and [`LspSession::diagnose`] is only ever reached with the
    /// whole workspace rather than a model-supplied file. A third caller that
    /// takes a path from a model owes the same check (0.74.0, audit H7).
    async fn sync(&self, server: &Started, ws: &crate::tools::Workspace, path: &str) -> Result<()> {
        let full = ws.root().join(path);
        let text = std::fs::read_to_string(&full).map_err(|e| Error::Lsp {
            server: server.config.id.clone(),
            reason: format!("could not read {path}: {e}"),
        })?;
        let uri = uri_for(&full);
        let mut opened = server.opened.lock().await;
        if opened.contains(&uri) {
            server
                .client
                .notify(
                    "textDocument/didClose",
                    json!({"textDocument": {"uri": uri}}),
                )
                .await?;
        }
        server
            .client
            .notify(
                "textDocument/didOpen",
                json!({"textDocument": {
                    "uri": uri,
                    "languageId": language_id(path),
                    "version": 1,
                    "text": text,
                }}),
            )
            .await?;
        opened.insert(uri);
        Ok(())
    }

    /// What the configured servers say about the code, beside what the project's
    /// own checker said.
    ///
    /// **Appended, never substituted.** `src/tools/diagnostics.rs` explains at
    /// length why this crate reads the compiler's own stream rather than a
    /// server's analysis: rust-analyzer's answer omits borrow-check errors,
    /// monomorphisation errors and every clippy lint, which are precisely the
    /// errors a model writes. That reasoning is unchanged. A server sees things
    /// the compiler does not — unused imports it has already resolved, a lint the
    /// project's cheap check does not run — and those are worth having *as well*.
    ///
    /// Pull only. Push diagnostics have no completion signal, so an empty result
    /// is indistinguishable from a slow one, and that is the exact distinction
    /// [`Outcome`](crate::tools::diagnostics::Outcome) exists to preserve. A
    /// server that does not advertise the pull capability is `Failed` with that
    /// reason, never `Clean`.
    pub(crate) async fn diagnose(
        &self,
        ws: &crate::tools::Workspace,
        path: Option<&str>,
        run_id: i64,
        watch: &crate::run::Watch<'_>,
    ) -> Vec<(String, crate::tools::diagnostics::Outcome)> {
        use crate::tools::diagnostics::Outcome;
        let asked: Vec<&Started> = match path {
            Some(path) => self.server_for(Some(path)).into_iter().collect(),
            None => self.servers.iter().collect(),
        };
        let mut out = Vec::new();
        for server in asked {
            let id = server.config.id.clone();
            let caps = match self.ready(server, run_id, watch).await {
                Ok(caps) => caps,
                Err(e) => {
                    out.push((id, Outcome::Failed(format!("{e}"))));
                    continue;
                }
            };
            if caps.get("diagnosticProvider").is_none() {
                out.push((
                    id,
                    Outcome::Failed(
                        "does not answer diagnostic requests, so it has nothing to add here —                          which is not the same as finding nothing"
                            .into(),
                    ),
                ));
                continue;
            }
            if let Some(path) = path {
                if let Err(e) = self.sync(server, ws, path).await {
                    out.push((id, Outcome::Failed(format!("{e}"))));
                    continue;
                }
            }
            let (method, params) = match path {
                Some(path) => (
                    "textDocument/diagnostic",
                    json!({"textDocument": {"uri": uri_for(&ws.root().join(path))}}),
                ),
                None => ("workspace/diagnostic", json!({"previousResultIds": []})),
            };
            let answer = server
                .client
                .request(method, params, server.config.timeout())
                .await;
            let outcome = match answer {
                Ok(result) => {
                    let lines = diagnostics(&result, ws, path);
                    if lines.is_empty() {
                        Outcome::Clean
                    } else {
                        Outcome::Found(lines.join("\n"))
                    }
                }
                Err(e) => Outcome::Failed(format!("{e}")),
            };
            out.push((id, outcome));
        }
        out
    }

    /// End every server, then the run.
    ///
    /// `shutdown` then `exit` is what the protocol asks for and is sent
    /// best-effort under a short bound; the child is killed either way, because a
    /// language server that ignores `exit` must not outlive the run that spawned
    /// it.
    pub(crate) async fn shutdown(self) {
        for s in self.servers {
            let polite = async {
                let _ = s
                    .client
                    .request("shutdown", Value::Null, Duration::from_secs(2))
                    .await;
                let _ = s.client.notify("exit", Value::Null).await;
            };
            let _ = tokio::time::timeout(Duration::from_secs(2), polite).await;
            let mut child = s.child.lock().await;
            let _ = child.start_kill();
        }
    }
}

/// Run `initialize` and `initialized` in the background, publishing the result.
///
/// Detached on purpose. Nothing in the prompt depends on a server's capabilities,
/// so making run start wait for an index would buy nothing and cost minutes.
fn handshake(
    client: &Client,
    root: &std::path::Path,
    timeout: Duration,
    tx: tokio::sync::watch::Sender<Handshake>,
) {
    // The client is owned by the session and outlives this task; the task talks
    // to the same child through its own handle on the writer and pending map.
    let uri = uri_for(root);
    let params = json!({
        "processId": std::process::id(),
        "rootUri": uri,
        "workspaceFolders": [{"uri": uri, "name": "workspace"}],
        "capabilities": {
            "textDocument": {
                "definition": {"linkSupport": true},
                "references": {},
                "documentSymbol": {"hierarchicalDocumentSymbolSupport": true},
                "hover": {"contentFormat": ["plaintext", "markdown"]},
                "rename": {},
                "diagnostic": {},
                "synchronization": {"didSave": false},
            },
            "workspace": {"symbol": {}, "workspaceFolders": true},
            // Without this a server sends no `$/progress` at all, and the client
            // has no way to tell "still indexing" from "no references" — which is
            // the whole difference between an honest answer and a confident wrong
            // one. Found by the live run: rust-analyzer is silent about its
            // start-up unless asked to speak.
            "window": {"workDoneProgress": true},
        },
    });
    let sender = client.handle();
    tokio::spawn(async move {
        let outcome = match sender.request("initialize", params, timeout).await {
            Ok(result) => {
                let _ = sender.notify("initialized", json!({})).await;
                Ok(result
                    .get("capabilities")
                    .cloned()
                    .unwrap_or(Value::Object(Default::default())))
            }
            Err(e) => Err(format!("{e}")),
        };
        let _ = tx.send(Some(outcome));
    });
}

/// One question a navigation tool asks.
///
/// An enum rather than five methods so every question goes through one funnel:
/// one readiness wait, one capability check, one document re-sync and one
/// `deny_read` filter. Five methods would be five places for one of those to be
/// missing.
pub(crate) enum Nav<'a> {
    /// Where is the thing at this position defined.
    Definition {
        path: &'a str,
        line: u32,
        column: u32,
    },
    /// Everywhere the thing at this position is used.
    References {
        path: &'a str,
        line: u32,
        column: u32,
    },
    /// What is in this file, or — with a query — where in the workspace a symbol
    /// with that name is. One tool, because two schemas for one question is
    /// prompt bytes on every request of every run.
    Symbols {
        path: Option<&'a str>,
        query: Option<&'a str>,
    },
    /// What is the thing at this position.
    Hover {
        path: &'a str,
        line: u32,
        column: u32,
    },
    /// Rename the thing at this position, everywhere. Answers with a patch; see
    /// [`LspSession::navigate`]'s caller — nothing here writes.
    Rename {
        path: &'a str,
        line: u32,
        column: u32,
        new_name: &'a str,
    },
}

impl Nav<'_> {
    /// The file this question is about, if it is about one.
    fn path(&self) -> Option<&str> {
        match self {
            Nav::Definition { path, .. }
            | Nav::References { path, .. }
            | Nav::Hover { path, .. }
            | Nav::Rename { path, .. } => Some(path),
            Nav::Symbols { path, .. } => *path,
        }
    }

    /// The protocol method that answers it.
    fn method(&self) -> &'static str {
        match self {
            Nav::Definition { .. } => "textDocument/definition",
            Nav::References { .. } => "textDocument/references",
            Nav::Symbols { query: Some(_), .. } => "workspace/symbol",
            Nav::Symbols { .. } => "textDocument/documentSymbol",
            Nav::Hover { .. } => "textDocument/hover",
            Nav::Rename { .. } => "textDocument/rename",
        }
    }

    /// The capability a server must advertise to be asked this.
    ///
    /// A server that does not advertise it answers with the reason rather than
    /// being absent from the catalogue: the catalogue is composed before the
    /// handshake has finished, so absence would depend on a race.
    fn capability(&self) -> &'static str {
        match self {
            Nav::Definition { .. } => "definitionProvider",
            Nav::References { .. } => "referencesProvider",
            Nav::Symbols { query: Some(_), .. } => "workspaceSymbolProvider",
            Nav::Symbols { .. } => "documentSymbolProvider",
            Nav::Hover { .. } => "hoverProvider",
            Nav::Rename { .. } => "renameProvider",
        }
    }

    /// The request body, with positions converted to the wire's zero base.
    fn params(&self, root: &std::path::Path) -> Value {
        let at = |path: &str, line: u32, column: u32| {
            json!({
                "textDocument": {"uri": uri_for(&root.join(path))},
                "position": {"line": to_wire(line), "character": to_wire(column)},
            })
        };
        match self {
            Nav::Definition { path, line, column } | Nav::Hover { path, line, column } => {
                at(path, *line, *column)
            }
            Nav::References { path, line, column } => {
                let mut body = at(path, *line, *column);
                body["context"] = json!({"includeDeclaration": true});
                body
            }
            Nav::Rename {
                path,
                line,
                column,
                new_name,
            } => {
                let mut body = at(path, *line, *column);
                body["newName"] = json!(new_name);
                body
            }
            Nav::Symbols {
                query: Some(query), ..
            } => json!({"query": query}),
            Nav::Symbols { path, .. } => json!({
                "textDocument": {"uri": uri_for(&root.join(path.unwrap_or_default()))}
            }),
        }
    }
}

/// Turn a server's answer into the text the model reads.
fn render(ask: &Nav<'_>, result: &Value, ws: &crate::tools::Workspace) -> String {
    match ask {
        Nav::Rename { new_name, .. } => rename_patch(result, ws, new_name),
        Nav::Hover { .. } => {
            let text = hover_text(result);
            if text.trim().is_empty() {
                "The server has nothing to say about that position.".to_string()
            } else {
                text
            }
        }
        Nav::Symbols { .. } => {
            let (lines, omitted) = symbols(result, ws);
            with_omissions(
                if lines.is_empty() {
                    "No symbols.".to_string()
                } else {
                    lines.join("\n")
                },
                omitted,
            )
        }
        _ => {
            let (lines, omitted) = locations(result, ws);
            with_omissions(
                if lines.is_empty() {
                    "No locations.".to_string()
                } else {
                    lines.join("\n")
                },
                omitted,
            )
        }
    }
}

/// State an omission rather than returning a quietly shorter list.
///
/// A list with results removed and nothing said is a wrong answer to "who calls
/// this": the model reads three call sites where there are four and concludes it
/// has seen them all.
fn with_omissions(body: String, omitted: usize) -> String {
    if omitted == 0 {
        return body;
    }
    let plural = if omitted == 1 { "" } else { "s" };
    format!(
        "{body}\n\n({omitted} result{plural} omitted: the policy denies reading \
         the file{plural} they are in.)"
    )
}

/// Whether this run may be told about a path at all.
///
/// Only an outright `Deny` omits. `Ask` does not, and that is deliberate: naming
/// a path is not reading its contents, and a policy that *asks* about reads has
/// nobody to ask on this path — treating `Ask` as an omission would empty every
/// answer this feature gives to such a policy, which is a refusal the operator
/// never wrote.
///
/// **This is the bar for a path that is only ever printed.** A path whose
/// *contents* this crate is about to read on a server's say-so is held to
/// [`may_read_contents`] instead; the distinction is the whole of audit M11.
fn readable(ws: &crate::tools::Workspace, path: &str) -> bool {
    ws.policy().check(crate::Act::Read, path).effect != crate::Effect::Deny
}

/// Where a server-named file's bytes may be read from, or `None` if they may not
/// be (0.74.0, audit M11).
///
/// [`readable`]'s bar is wrong here in both directions. A `WorkspaceEdit` names
/// files the *server* chose, and [`rename_patch`] does not merely print them: it
/// reads each one and renders a diff, so every removed line lands in the model's
/// context. Under that use "naming a path is not reading it" is simply false.
///
/// So two things change, and it is the second that carries the weight.
///
/// The path is **resolved under the workspace root** rather than opened where the
/// server pointed. [`Workspace::check_path`](crate::tools::Workspace::check_path)
/// refuses an absolute path, a `..` that climbs out and a symlink that lands
/// outside, and [`Workspace::resolve`](crate::tools::Workspace::resolve) then
/// yields the path that was actually graded — so the file opened is the file
/// checked rather than the string the server sent. Grading the effect alone never
/// closed this: [`Policy::default`](crate::Policy::default) *allows* reads by
/// default, so a server naming `~/.aws/credentials` was answered `Allow`, not
/// `Ask`, and a bar of "`Allow`, not `Deny`" would have let it through unchanged.
///
/// And `Ask` no longer passes, which closes the narrower case of a policy whose
/// read default is [`Effect::Ask`](crate::Effect::Ask) or that asks about the
/// subtree being renamed. An unanswered question is not permission, and there is
/// no approver on this path to answer it. The cost is stated rather than hidden:
/// under such a policy a rename now renders no patch and says so. A run that
/// wants the patch grants `allow_read` over the tree it is renaming in — the same
/// grant applying that patch with `patch_file` already needs.
fn may_read_contents(ws: &crate::tools::Workspace, rel: &str) -> Option<std::path::PathBuf> {
    if ws.check_path(crate::Act::Read, rel).effect != crate::Effect::Allow {
        return None;
    }
    ws.resolve(rel).ok()
}

/// Every location in an answer, as `path:line:column`, with denied ones dropped.
fn locations(result: &Value, ws: &crate::tools::Workspace) -> (Vec<String>, usize) {
    let mut raw = Vec::new();
    match result {
        Value::Array(items) => raw.extend(items.iter().cloned()),
        Value::Null => {}
        one => raw.push(one.clone()),
    }
    let mut lines = Vec::new();
    let mut omitted = 0;
    for item in raw {
        // A `LocationLink` spells its target differently from a `Location`, and a
        // server may answer with either.
        let uri = item
            .get("uri")
            .or_else(|| item.get("targetUri"))
            .and_then(Value::as_str);
        let range = item
            .get("range")
            .or_else(|| item.get("targetSelectionRange"));
        let (Some(uri), Some(range)) = (uri, range) else {
            continue;
        };
        let Some(path) = path_of(uri) else { continue };
        let shown = relative(ws, &path);
        if !readable(ws, &shown) {
            omitted += 1;
            continue;
        }
        lines.push(format!("{shown}:{}", position(range)));
    }
    (lines, omitted)
}

/// Every symbol in an answer, flattened, with denied ones dropped.
fn symbols(result: &Value, ws: &crate::tools::Workspace) -> (Vec<String>, usize) {
    let mut lines = Vec::new();
    let mut omitted = 0;
    let mut stack: Vec<(&Value, usize)> = Vec::new();
    if let Value::Array(items) = result {
        stack.extend(items.iter().rev().map(|i| (i, 0)));
    }
    while let Some((item, depth)) = stack.pop() {
        let name = item.get("name").and_then(Value::as_str).unwrap_or("?");
        let kind = kind_name(item.get("kind").and_then(Value::as_u64).unwrap_or(0));
        // A `DocumentSymbol` carries its own range; a `SymbolInformation` carries
        // a whole `location`.
        let (uri, range) = match item.get("location") {
            Some(loc) => (
                loc.get("uri").and_then(Value::as_str),
                loc.get("range").cloned(),
            ),
            None => (
                None,
                item.get("selectionRange").or(item.get("range")).cloned(),
            ),
        };
        let where_ = match (uri.and_then(path_of), range) {
            (Some(path), Some(range)) => {
                let shown = relative(ws, &path);
                if !readable(ws, &shown) {
                    omitted += 1;
                    continue;
                }
                format!(" {shown}:{}", position(&range))
            }
            (None, Some(range)) => format!(" :{}", position(&range)),
            _ => String::new(),
        };
        lines.push(format!("{}{name} ({kind}){where_}", "  ".repeat(depth)));
        if let Some(Value::Array(children)) = item.get("children") {
            stack.extend(children.iter().rev().map(|c| (c, depth + 1)));
        }
    }
    (lines, omitted)
}

/// `line:column`, as a reader counts them.
fn position(range: &Value) -> String {
    let start = range.get("start").unwrap_or(range);
    let line = from_wire(start.get("line").and_then(Value::as_u64).unwrap_or(0));
    let column = from_wire(start.get("character").and_then(Value::as_u64).unwrap_or(0));
    format!("{line}:{column}")
}

/// Hover contents, which the protocol spells three different ways.
fn hover_text(result: &Value) -> String {
    let contents = result.get("contents").unwrap_or(&Value::Null);
    match contents {
        Value::String(s) => s.clone(),
        Value::Object(o) => o
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        Value::Array(items) => items
            .iter()
            .map(hover_one)
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string(),
        _ => String::new(),
    }
}

fn hover_one(item: &Value) -> String {
    match item {
        Value::String(s) => s.clone(),
        Value::Object(o) => o
            .get("value")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

/// A path relative to the workspace root, which is how every other tool names one.
fn relative(ws: &crate::tools::Workspace, path: &str) -> String {
    let candidate = std::path::Path::new(path);
    // Both spellings of the root, because on macOS `/tmp` and `/var` are symlinks
    // into `/private` and a server resolves what it opened. A path that failed to
    // strip would be handed to the policy as an absolute one, under which a
    // `deny_read("secret/*")` rule matches nothing — a filter that silently stops
    // filtering is worse than one that is absent.
    let roots = [
        Some(ws.root().to_path_buf()),
        std::fs::canonicalize(ws.root()).ok(),
    ];
    for root in roots.into_iter().flatten() {
        if let Ok(rest) = candidate.strip_prefix(&root) {
            return rest.to_string_lossy().replace('\\', "/");
        }
    }
    path.to_string()
}

/// The `languageId` a `didOpen` carries, from the file's suffix.
///
/// Servers vary in how much they care; the ones that do care refuse a document
/// whose id they do not recognise, so an unknown suffix says `plaintext` rather
/// than guessing.
fn language_id(path: &str) -> &'static str {
    match path
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
    {
        Some(ext) => match ext.as_str() {
            "rs" => "rust",
            "go" => "go",
            "py" => "python",
            "ts" => "typescript",
            "tsx" => "typescriptreact",
            "js" => "javascript",
            "jsx" => "javascriptreact",
            "c" | "h" => "c",
            "cc" | "cpp" | "hpp" => "cpp",
            "java" => "java",
            "rb" => "ruby",
            "cs" => "csharp",
            _ => "plaintext",
        },
        None => "plaintext",
    }
}

/// The protocol's `SymbolKind`, which is a number on the wire.
fn kind_name(kind: u64) -> &'static str {
    const KINDS: [&str; 26] = [
        "file",
        "module",
        "namespace",
        "package",
        "class",
        "method",
        "property",
        "field",
        "constructor",
        "enum",
        "interface",
        "function",
        "variable",
        "constant",
        "string",
        "number",
        "boolean",
        "array",
        "object",
        "key",
        "null",
        "enum member",
        "struct",
        "event",
        "operator",
        "type parameter",
    ];
    kind.checked_sub(1)
        .and_then(|i| usize::try_from(i).ok())
        .and_then(|i| KINDS.get(i).copied())
        .unwrap_or("symbol")
}

/// A `WorkspaceEdit` rendered as a patch series, in 0.51.0's own format.
///
/// **Nothing here writes.** The server resolved the rename; this composes what
/// the change *would* be, per file, from that file's current bytes — and the
/// model applies whichever parts it wants with `patch_file`, one
/// [`Act::Write`](crate::Act::Write) check per path. A tool that wrote N files on
/// a server's say-so would be the multi-file write 0.51.0 excluded on purpose,
/// with the additional property that this crate did not compute the change.
///
/// **It does read, though, and on a server's say-so.** Rendering a diff means
/// reading each named file's current bytes, and every removed line is then in the
/// model's context. The server is untrusted — a repository chooses which one a run
/// spawns — so each path it names is confined under the workspace root and must
/// be an outright [`Effect::Allow`](crate::Effect::Allow) before it is opened. See
/// [`may_read_contents`], which is where the reasoning lives (0.74.0, audit M11).
fn rename_patch(result: &Value, ws: &crate::tools::Workspace, new_name: &str) -> String {
    let mut per_file: Vec<(String, Vec<Value>)> = Vec::new();
    // A server answers with `changes` or with `documentChanges`, and a client
    // that reads only one of them silently renames nothing against half of them.
    if let Some(Value::Object(changes)) = result.get("changes") {
        for (uri, edits) in changes {
            if let Value::Array(edits) = edits {
                per_file.push((uri.clone(), edits.clone()));
            }
        }
    }
    if let Some(Value::Array(docs)) = result.get("documentChanges") {
        for doc in docs {
            let uri = doc["textDocument"]["uri"].as_str().unwrap_or_default();
            if let Value::Array(edits) = &doc["edits"] {
                per_file.push((uri.to_string(), edits.clone()));
            }
        }
    }
    // Path order, so two runs of the same rename produce the same patch.
    per_file.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = String::new();
    let mut skipped = Vec::new();
    let mut omitted = 0;
    for (uri, edits) in per_file {
        let Some(path) = path_of(&uri) else { continue };
        let shown = relative(ws, &path);
        // The server chose this path and this reads what is in it, so the bar is
        // `may_read_contents`, not `readable`. See audit M11.
        let Some(under_root) = may_read_contents(ws, &shown) else {
            omitted += 1;
            continue;
        };
        let Ok(before) = std::fs::read_to_string(&under_root) else {
            skipped.push(format!("{shown} (could not be read)"));
            continue;
        };
        match apply_edits(&before, &edits) {
            Some(after) => match crate::diff::render(&before, &after) {
                Some(hunk) => out.push_str(&format!("--- a/{shown}\n+++ b/{shown}\n{hunk}")),
                // The server asked for a change that changes nothing. Saying so
                // beats emitting a header with no hunk under it, which is a patch
                // `patch_file` would refuse.
                None => skipped.push(format!("{shown} (the edit changes nothing)")),
            },
            None => skipped.push(format!(
                "{shown} (an edit named a position the file does not have)"
            )),
        }
    }

    if out.is_empty() && skipped.is_empty() && omitted == 0 {
        return format!("The server found nothing to rename to {new_name}.");
    }
    let mut text = if out.is_empty() {
        String::new()
    } else {
        format!(
            "Nothing has been written. Apply each file's section below with patch_file.\n\n{out}"
        )
    };
    if !skipped.is_empty() {
        text.push_str(&format!("\n(not patched: {})\n", skipped.join(", ")));
    }
    // Not `with_omissions`: its reason is a `deny_read` rule, and here an omission
    // is just as likely to be a path the policy only *asks* about or one the
    // server named outside the workspace entirely. Saying "the policy denies it"
    // about either would send the model looking for a rule that does not exist.
    if omitted > 0 {
        let plural = if omitted == 1 { "" } else { "s" };
        text.push_str(&format!(
            "\n\n({omitted} file{plural} omitted: the server named {} this run may not read — \
             a rename renders a file's contents, so it needs an outright allow_read, \
             not an unanswered ask.)",
            if omitted == 1 { "a path" } else { "paths" }
        ));
    }
    text
}

/// Apply a file's `TextEdit`s to its text, or `None` if one names a position the
/// file does not have.
///
/// Applied last-first, which is what makes the ranges mean what the server meant:
/// they are all against the text as the server saw it, so applying the first one
/// moves every later one. That is the same reasoning `patch_file` applies its
/// hunks at their own offsets against the original rather than as a running
/// rewrite.
fn apply_edits(text: &str, edits: &[Value]) -> Option<String> {
    let mut offsets: Vec<(usize, usize, String)> = Vec::new();
    for edit in edits {
        let range = edit.get("range")?;
        let start = offset_of(text, range.get("start")?)?;
        let end = offset_of(text, range.get("end")?)?;
        if end < start {
            return None;
        }
        offsets.push((
            start,
            end,
            edit.get("newText").and_then(Value::as_str)?.to_string(),
        ));
    }
    offsets.sort_by_key(|(start, _, _)| *start);
    // Overlapping edits have no defined result, and guessing one is how a rename
    // silently corrupts a file.
    if offsets.windows(2).any(|w| w[0].1 > w[1].0) {
        return None;
    }
    let mut out = text.to_string();
    for (start, end, new_text) in offsets.into_iter().rev() {
        out.replace_range(start..end, &new_text);
    }
    Some(out)
}

/// The byte offset of a wire position, counting UTF-16 code units within a line.
///
/// The protocol measures a character offset in UTF-16 by default, which for every
/// ASCII line is the same number as bytes and for a line with an emoji in it is
/// not. Getting this wrong splits a character, and `String::replace_range` panics
/// on a non-boundary rather than producing a wrong file — so this returns `None`
/// instead of indexing blind.
fn offset_of(text: &str, position: &Value) -> Option<usize> {
    let line = usize::try_from(position.get("line")?.as_u64()?).ok()?;
    let character = usize::try_from(position.get("character")?.as_u64()?).ok()?;
    let mut offset = 0;
    for _ in 0..line {
        offset += text.get(offset..)?.find('\n')? + 1;
    }
    let rest = text.get(offset..)?;
    let mut units = 0;
    for (i, ch) in rest.char_indices() {
        if units == character {
            return Some(offset + i);
        }
        if ch == '\n' && units < character {
            // A position past the end of its line clamps to the line's end, which
            // is what a server means by "the end of this line".
            return Some(offset + i);
        }
        units += ch.len_utf16();
    }
    Some(text.len())
}

/// Every diagnostic in a pull answer, as one line each.
///
/// Both shapes: `textDocument/diagnostic` answers a report for the one file, and
/// `workspace/diagnostic` answers a list of reports each naming its own uri. A
/// reader that handles one of them reports nothing for the other, silently.
fn diagnostics(result: &Value, ws: &crate::tools::Workspace, path: Option<&str>) -> Vec<String> {
    let mut lines = Vec::new();
    let mut take = |items: &Value, where_: String| {
        if where_.is_empty() {
            return;
        }
        if let Value::Array(items) = items {
            for item in items {
                let severity = match item.get("severity").and_then(Value::as_u64) {
                    Some(1) => "error",
                    Some(2) => "warning",
                    Some(3) => "note",
                    Some(4) => "hint",
                    _ => "diagnostic",
                };
                let message = item
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("(no message)");
                let at = item.get("range").map(position).unwrap_or_default();
                lines.push(format!("{where_}:{at}: {severity}: {message}"));
            }
        }
    };
    match result.get("items") {
        // A workspace report: a list of per-file reports.
        Some(Value::Array(reports)) if reports.iter().any(|r| r.get("uri").is_some()) => {
            for report in reports {
                let shown = report
                    .get("uri")
                    .and_then(Value::as_str)
                    .and_then(path_of)
                    .map(|p| relative(ws, &p))
                    .unwrap_or_default();
                // The same filter every other answer passes: a diagnostic names a
                // path, and a path the policy denies reading is not carried here.
                if shown.is_empty() || !readable(ws, &shown) {
                    continue;
                }
                take(report.get("items").unwrap_or(&Value::Null), shown);
            }
        }
        // A single-file report, whose path is the one that was asked about.
        Some(items) => take(items, path.map(str::to_string).unwrap_or_default()),
        None => {}
    }
    lines
}

/// A `file://` URI for a path, which is what every request here carries.
///
/// Built by hand rather than through a URL crate: the only escaping this needs is
/// the one Windows requires, where a path begins with a drive letter rather than
/// with a separator.
pub(crate) fn uri_for(path: &std::path::Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if text.starts_with('/') {
        format!("file://{text}")
    } else {
        format!("file:///{text}")
    }
}

/// The path a `file://` URI names, or `None` for anything else.
pub(crate) fn path_of(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix('/').unwrap_or(rest);
    // A Windows URI is `file:///C:/x`; a POSIX one is `file:///home/x` and needs
    // its leading separator back.
    if rest.len() > 1 && rest.as_bytes()[1] == b':' {
        Some(rest.to_string())
    } else {
        Some(format!("/{rest}"))
    }
}

/// One connected language server, correlated by request id.
///
/// The reader runs as its own task for the life of the client: a response can
/// arrive while nothing is awaiting it, and a notification arrives when the server
/// feels like it, so there is no point in the stream at which "read the next
/// message" belongs to one caller.
pub(crate) struct Client {
    inner: Handle,
    reader: tokio::task::JoinHandle<()>,
}

/// A cloneable way to speak to a client that someone else owns.
///
/// The handshake runs as its own task while the session holds the [`Client`], so
/// there are two speakers on one child from the moment it exists. They share the
/// id counter — two speakers minting the same id would be answered each other's
/// questions, which is exactly the defect correlation exists to prevent.
#[derive(Clone)]
pub(crate) struct Handle {
    /// Work the server has told us it is doing and has not finished.
    ///
    /// This is the completion signal the protocol *does* have, and it is the one
    /// thing standing between "there are no references" and "I have not indexed
    /// yet". A server that is still building its index answers a semantic request
    /// with an empty list — not an error — so a client that does not wait for
    /// this reports "nobody calls this function" about a function with four
    /// callers. Found by the live run against a real `rust-analyzer`, where
    /// `documentSymbol` (syntax only) answered correctly while `definition`,
    /// `references` and `hover` all came back empty.
    working: Arc<Mutex<std::collections::HashSet<String>>>,
    /// Woken whenever `working` changes, so a waiter does not poll.
    settled: Arc<tokio::sync::Notify>,
    /// Whether this server has ever announced work at all. A server that never
    /// does is ready as soon as its handshake is, and waiting on it would be
    /// waiting for something that is never coming.
    announced_work: Arc<std::sync::atomic::AtomicBool>,
    /// The configured id, carried so every error names the server the operator wrote.
    id: String,
    writer: Arc<tokio::sync::Mutex<Box<dyn AsyncWrite + Unpin + Send>>>,
    pending: Arc<Mutex<HashMap<i64, oneshot::Sender<Value>>>>,
    next_id: Arc<AtomicI64>,
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
        let working: Arc<Mutex<std::collections::HashSet<String>>> = Arc::default();
        let settled: Arc<tokio::sync::Notify> = Arc::default();
        let announced_work: Arc<std::sync::atomic::AtomicBool> = Arc::default();
        let writer: Arc<tokio::sync::Mutex<Box<dyn AsyncWrite + Unpin + Send>>> =
            Arc::new(tokio::sync::Mutex::new(Box::new(write)));

        let reader = tokio::spawn(read_loop(
            BufReader::new(read),
            Arc::clone(&pending),
            Arc::clone(&writer),
            Arc::clone(&gone),
            Arc::clone(&working),
            Arc::clone(&settled),
            Arc::clone(&announced_work),
        ));

        Self {
            inner: Handle {
                id,
                writer,
                pending,
                next_id: Arc::new(AtomicI64::new(1)),
                gone,
                working,
                settled,
                announced_work: Arc::clone(&announced_work),
            },
            reader,
        }
    }

    /// A second speaker on the same child, for the background handshake.
    pub(crate) fn handle(&self) -> Handle {
        self.inner.clone()
    }

    /// Send a request and wait for the response with that id.
    pub(crate) async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        self.inner.request(method, params, timeout).await
    }

    /// Send a notification, which by definition is never answered.
    pub(crate) async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.inner.notify(method, params).await
    }

    /// Whether this server has ever announced work at all.
    pub(crate) fn announced_work(&self) -> bool {
        self.inner.announced_work.load(Ordering::Relaxed)
    }

    /// Wait until the server has finished the work it announced, or the bound.
    ///
    /// Returns whether it settled. A server that has announced **no** work settles
    /// at once — that is the fixture's case and every syntax-only server's case,
    /// and it is why nothing in the test suite waits on a clock here.
    ///
    /// A server that has announced work must be *empty and stay empty*. Draining
    /// to zero is not enough, and this is the live run's finding: `rust-analyzer`
    /// reports its start-up as a series of separate jobs — `Fetching`, then
    /// `Building CrateGraph`, then `Roots Scanned` — and the set is momentarily
    /// empty in the gaps between them, which is exactly where a run's first
    /// navigation call lands. A request answered in one of those gaps comes back
    /// empty rather than wrong, which is the failure this whole surface exists to
    /// prevent.
    pub(crate) async fn quiet(&self, bound: Duration) -> bool {
        /// How long the announced work must stay finished before it counts as
        /// finished.
        ///
        /// Not a guess about how long indexing takes — that is bounded by the
        /// server's own `timeout_secs`. This is the width of the *gap* between two
        /// of its start-up jobs, measured against a real `rust-analyzer`, which
        /// reports `Fetching`, then `Building CrateGraph`, then `Roots Scanned`,
        /// then more, with sub-second silences between them. Too small and a gap
        /// reads as "finished"; too large and every genuinely empty answer costs
        /// it once. This is the knob to turn if a server's start-up is reported in
        /// coarser pieces.
        const SETTLE: Duration = Duration::from_millis(1_500);

        tokio::time::timeout(bound, async {
            loop {
                // Registered before the check, so an `end` that lands between the
                // two is not missed.
                let woken = self.inner.settled.notified();
                let (idle, ever) = {
                    let set = self
                        .inner
                        .working
                        .lock()
                        .expect("progress set is not poisoned");
                    (
                        set.is_empty(),
                        self.inner.announced_work.load(Ordering::Relaxed),
                    )
                };
                if idle && !ever {
                    // Nothing announced yet. That is either a server that never
                    // announces work — ready now — or one that has not got round
                    // to saying so, a distinction only time can draw. Wait one
                    // grace period for a first announcement and take silence as
                    // the answer.
                    if tokio::time::timeout(SETTLE, woken).await.is_err() {
                        return;
                    }
                    continue;
                }
                if idle {
                    // Quiet now. Quiet still, after the settle, means finished.
                    match tokio::time::timeout(SETTLE, woken).await {
                        // Nothing happened in the window: the work is done.
                        Err(_) => return,
                        // Something moved; look again.
                        Ok(()) => continue,
                    }
                }
                woken.await;
            }
        })
        .await
        .is_ok()
    }
}

impl Handle {
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
    working: Arc<Mutex<std::collections::HashSet<String>>>,
    settled: Arc<tokio::sync::Notify>,
    announced_work: Arc<std::sync::atomic::AtomicBool>,
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
                        // A created progress token means work is about to be
                        // reported. Counting it here rather than at `begin`
                        // closes the window between the handshake finishing and
                        // the first `begin` arriving — which is exactly the
                        // window a run's first navigation call lands in.
                        if message["method"] == "window/workDoneProgress/create" {
                            if let Some(token) = token_of(&message["params"]["token"]) {
                                working
                                    .lock()
                                    .expect("progress set is not poisoned")
                                    .insert(token);
                                announced_work.store(true, Ordering::Relaxed);
                                settled.notify_waiters();
                            }
                        }
                        let body = json!({"jsonrpc": "2.0", "id": id, "result": Value::Null});
                        if let Ok(text) = serde_json::to_string(&body) {
                            let mut w = writer.lock().await;
                            let _ = w.write_all(&frame(&text)).await;
                            let _ = w.flush().await;
                        }
                    }
                    // A notification. Only progress is subscribed to.
                    _ => {
                        if message["method"] == "$/progress" {
                            let params = &message["params"];
                            if let Some(token) = token_of(&params["token"]) {
                                let kind = params["value"]["kind"].as_str().unwrap_or_default();
                                let mut set = working.lock().expect("progress set is not poisoned");
                                match kind {
                                    "begin" => {
                                        set.insert(token);
                                        announced_work.store(true, Ordering::Relaxed);
                                    }
                                    "end" => {
                                        set.remove(&token);
                                    }
                                    // `report` is progress on work already counted.
                                    _ => {}
                                }
                                drop(set);
                                settled.notify_waiters();
                            }
                        }
                    }
                }
            }
            Ok(None) => break "the server closed its output".to_string(),
            Err(e) => break format!("{e}"),
        }
    };
    *gone.lock().expect("reason is not poisoned") = Some(reason);
    // A server that stopped is not still working, and a waiter must not be left
    // waiting for an `end` that can no longer arrive.
    working
        .lock()
        .expect("progress set is not poisoned")
        .clear();
    settled.notify_waiters();
    // Dropping the senders is what wakes every outstanding request.
    pending.lock().expect("pending map is not poisoned").clear();
}

/// Whether an answer carries nothing — which is not the same as carrying "no".
fn is_empty(result: &Value) -> bool {
    match result {
        Value::Null => true,
        Value::Array(items) => items.is_empty(),
        Value::Object(o) => o.is_empty() || o.values().all(is_empty),
        Value::String(s) => s.is_empty(),
        _ => false,
    }
}

/// A progress token, which the protocol allows to be a string or a number.
fn token_of(token: &Value) -> Option<String> {
    match token {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
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

    /// M12 — a length a server made up is refused before anything is allocated.
    ///
    /// The regression test lives here rather than in `tests/security_lsp.rs`
    /// because `read_frame` is the private half of this module and the frame
    /// tests above are its suite. The exploit needs a server that lies in its
    /// *header*, which the fixture in `examples/lsp_fixture_server.rs` cannot be
    /// asked to do — it lengths every frame from the body it is about to write.
    ///
    /// The first assertion is what fails on 0.73.0's behaviour: the header alone
    /// reached `vec![0u8; len]`, which is a `handle_alloc_error` abort on macOS
    /// and Windows — uncatchable, so no `#[should_panic]` could have caught it —
    /// and a pinned address space on Linux. The second pins the boundary as `>`
    /// rather than `>=`, and the third pins the refusal as the ceiling's rather
    /// than a truncated read's: 0.73.0 refuses an over-long frame too, but only
    /// after it has allocated for it and then run out of stream.
    #[tokio::test]
    async fn m12_a_content_length_over_the_ceiling_is_refused_before_the_allocation() {
        // The audit's own number. A 64-bit host parses it into `usize` and would
        // have tried to allocate 8 petabytes.
        let mut reader = BufReader::new(std::io::Cursor::new(
            b"Content-Length: 9007199254740991\r\n\r\n".to_vec(),
        ));
        assert!(read_frame(&mut reader).await.is_err());

        // One byte over, and the refusal names the ceiling rather than the end of
        // the stream — which is what says the check ran before the allocation.
        let header = format!("Content-Length: {}\r\n\r\n", MAX_FRAME + 1);
        let mut reader = BufReader::new(std::io::Cursor::new(header.into_bytes()));
        let err = read_frame(&mut reader).await.unwrap_err().to_string();
        assert!(err.contains("ceiling"), "{err}");

        // A frame exactly at the ceiling is still a frame. The companion: the cap
        // must not clip a legitimate answer, and a large `workspace/diagnostic`
        // body is the one that gets close.
        let body = format!("\"{}\"", "a".repeat(MAX_FRAME - 2));
        assert_eq!(body.len(), MAX_FRAME);
        let mut reader = BufReader::new(std::io::Cursor::new(frame(&body)));
        let message = read_frame(&mut reader).await.unwrap().unwrap();
        assert_eq!(message.as_str().map(str::len), Some(MAX_FRAME - 2));
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
