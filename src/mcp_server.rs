//! Serving this crate's own tools over MCP, on stdio.
//!
//! The MCP client has shipped since 0.8.0: this crate calls other people's
//! tools. This is the other direction — another harness calls **these** tools,
//! and gets this crate's boundary with them. That is the point of it. A tool is
//! a few lines of process spawning; a deny-first layered policy, an approval
//! tier, a three-OS sandbox and a durable journal are not, and an MCP server is
//! the standard way to lend them.
//!
//! # What is served, and under what
//!
//! Every served call goes through the same dispatch a model's call goes
//! through, so the policy gate sees it, a `policy_events` row records the
//! decision, the journal opens and closes an attempt for it, and it is
//! announced on the [`Observer`](crate::Observer) channel. A served session
//! owns a real run, so it is readable afterwards with
//! [`Attach`](crate::Attach) and the rest, exactly as any run is.
//!
//! There is no human at the far end of a pipe. The default approver is
//! [`DenyAll`](crate::DenyAll), so a rule whose effect is
//! [`Ask`](crate::Effect::Ask) resolves as a refusal carrying this crate's own
//! words rather than blocking on somebody who is not there. An operator who
//! wants a different answer passes their own [`Approver`](crate::Approver) to
//! [`serve_mcp_with`].
//!
//! [`MCP_SERVER_UNSERVED`] names the tools this server does not offer, each
//! because it needs something a served session does not have — a person to
//! answer, a plan gate to decide, or children to talk to.
//!
//! # The protocol
//!
//! Newline-delimited JSON-RPC 2.0 on stdin and stdout, written here over
//! `serde_json` and `tokio`, the way [`crate::lsp`] writes the client half of a
//! different JSON-RPC protocol. `rmcp` is a client-side dependency of this
//! crate and stays one: its server half would add nine packages to derive tool
//! schemas this crate already writes by hand and already sends to providers on
//! every step.
//!
//! **Nothing but JSON-RPC may reach stdout.** A stray line corrupts the stream
//! and the symptom is a client that cannot parse, not an error anyone raises.
//! Diagnostics go to stderr.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::agent::Agents;
use crate::approve::ResponderNone;
use crate::context::{entry_cap_chars, ContextBudget, ObsKind};
use crate::lsp::LspSession;
use crate::mcp::McpSession;
use crate::observe::{Ignore, Observer};
use crate::policy::Policy;
use crate::provider::ToolCall;
use crate::run::dispatch::dispatch;
use crate::run::gate::{Dispatched, PlanPhase};
use crate::run::memory::memory_key;
use crate::run::{PendingMedia, Watch};
use crate::skills::Skills;
use crate::tools::git::Identity;
use crate::tools::handles::{Handles, MAX_LIVE_HANDLES};
use crate::tools::workspace::Workspace;
use crate::tools::Toolbox;
use crate::{
    Approver, DenyAll, Error, MemoryLimits, Result, Store, ToolMask, ToolSpec,
    DEFAULT_EXEC_TIMEOUT, SUCCESS_OUTCOME,
};

// The browser session a served call is dispatched against. The real type when the
// feature is on, and the run loop's own shim when it is off — the same pair every
// dispatch site in this crate threads, named here rather than `#[cfg]`-ed at the
// two places that use it.
#[cfg(feature = "browser")]
use crate::tools::browser::BrowserSession;

#[cfg(not(feature = "browser"))]
use crate::run::BrowserSession;

/// The MCP protocol version this server offers.
///
/// The same version this crate's own client speaks through `rmcp`, so the two
/// halves of this product agree about the protocol they are on. A client that
/// asks for a version this server also supports is answered with its own
/// choice; anything else is answered with this one.
///
/// ```
/// use io_harness::MCP_SERVER_PROTOCOL_VERSION;
///
/// assert_eq!(MCP_SERVER_PROTOCOL_VERSION, "2025-11-25");
/// ```
pub const MCP_SERVER_PROTOCOL_VERSION: &str = "2025-11-25";

/// Every protocol version this server will agree to speak.
///
/// The set `rmcp` 3.0.0 knows, which is the client half of this same product:
/// negotiating down to a version this crate's own client cannot speak would
/// leave the two halves unable to talk to each other. A client that names one of
/// these is answered with its own choice, because MCP's negotiation is the
/// client proposing and the server confirming, not the server dictating.
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &[
    "2025-03-26",
    "2025-06-18",
    MCP_SERVER_PROTOCOL_VERSION,
    "2026-07-28",
];

// The JSON-RPC 2.0 error codes this half of the protocol returns. `-32602`
// (invalid params) and `-32603` (internal error) belong to `tools/call` and are
// declared where that lands, not here, so nothing in this file is a code no
// path emits.
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;

/// The tools this server does not offer, and will not.
///
/// Each needs something a served session has not got. `ask_question` and
/// `ask_questions` need a person; `propose_plan` needs a plan gate to decide
/// it; `spawn`, `send_message` and `read_messages` need a tree of children this
/// server does not run. Offering one and refusing every call to it would be a
/// worse answer than not offering it.
///
/// The served set and this set partition the catalogue, which is asserted
/// rather than assumed — so a tool added in a later release lands in one of
/// them rather than in neither.
///
/// ```
/// use io_harness::MCP_SERVER_UNSERVED;
///
/// assert!(MCP_SERVER_UNSERVED.contains(&"ask_question"));
/// assert!(!MCP_SERVER_UNSERVED.contains(&"read_file"));
/// ```
pub const MCP_SERVER_UNSERVED: &[&str] = &[
    crate::tools::ASK_QUESTION_TOOL,
    crate::tools::ASK_QUESTIONS_TOOL,
    crate::PROPOSE_PLAN_TOOL,
    crate::SPAWN_TOOL,
    crate::SEND_MESSAGE_TOOL,
    crate::READ_MESSAGES_TOOL,
    crate::tools::READ_SKILL_TOOL,
];

/// What a served session may reach, and what it is called.
///
/// Built rather than declared. The fields are private, which is what makes the
/// type extensible without `#[non_exhaustive]`: a caller outside this crate has
/// no struct literal to break.
///
/// ```
/// use io_harness::{McpServerConfig, Policy};
///
/// let config = McpServerConfig::new(".", "runs.db")
///     .with_policy(Policy::default().allow_read("src/**"))
///     .with_server_name("io-harness tools");
///
/// assert_eq!(config.server_name(), "io-harness tools");
/// ```
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    root: PathBuf,
    store_path: PathBuf,
    policy: Policy,
    tools: Toolbox,
    server_name: String,
    server_version: String,
}

impl McpServerConfig {
    /// A server over `root`, journalling to the store at `store_path`.
    ///
    /// The policy starts as [`Policy::default`] — the tiered boundary, where a
    /// write or an exec is an [`Ask`](crate::Effect::Ask) and egress is denied
    /// outright. Under the default [`DenyAll`] approver that makes reads work
    /// and every mutation refuse until an operator names it, which is the right
    /// way round for something reachable through a pipe. It is emphatically not
    /// [`Policy::permissive`], which is what a bare
    /// [`run`](crate::run) applies and would lend everything.
    pub fn new(root: impl AsRef<Path>, store_path: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
            store_path: store_path.as_ref().to_path_buf(),
            policy: Policy::default(),
            tools: Toolbox::new(),
            server_name: env!("CARGO_PKG_NAME").to_string(),
            server_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// Serve under `policy` instead of the tiered default.
    pub fn with_policy(mut self, policy: Policy) -> Self {
        self.policy = policy;
        self
    }

    /// Also serve the caller's own registered tools.
    ///
    /// A client cannot register a tool; whoever starts the server decides what
    /// is in the box, which is what makes this the operator's decision rather
    /// than the caller's.
    pub fn with_tools(mut self, tools: Toolbox) -> Self {
        self.tools = tools;
        self
    }

    /// The name reported to a client in `initialize`.
    pub fn with_server_name(mut self, name: impl Into<String>) -> Self {
        self.server_name = name.into();
        self
    }

    /// The workspace root every served path resolves against.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The store a served session journals to.
    pub fn store_path(&self) -> &Path {
        &self.store_path
    }

    /// The policy every served call is checked against.
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// The caller's own registered tools, if any were added.
    pub fn tools(&self) -> &Toolbox {
        &self.tools
    }

    /// The name reported to a client in `initialize`.
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// The version reported to a client in `initialize`.
    pub fn server_version(&self) -> &str {
        &self.server_version
    }
}

/// What a served session's run row says it is for.
///
/// [`Store::start_run`] takes a goal, and a served session has none in the sense
/// a [`TaskContract`](crate::TaskContract) does: the client decides what each
/// call is for, one call at a time. So the goal names the session rather than a
/// task. It is also what an
/// [`Approver`] is told when it is asked about a served call, which is the one
/// place the string has to be a sentence a human can act on.
const SERVED_GOAL: &str = "serving io-harness tools over MCP";

/// The outcome a session that died on its transport is finished with.
///
/// A broken pipe ends the session as surely as a clean close does, and a run row
/// left `running` would say the opposite of both.
const SERVED_FAILED: &str = "failed";

/// One served session: the run every call is a step against, and the things a
/// [`dispatch`] needs that belong to the session rather than to one call.
///
/// Built once per session for the reason the run loop builds its own once per
/// run: the process handles a `shell_start` leaves behind outlive the call that
/// opened them, a [`Watch`] carries a cancellation from one call to the next, and
/// a store connection opened per call would be a new SQLite handle per JSON-RPC
/// message.
struct Served<'a> {
    store: Store,
    run_id: i64,
    /// The step the next served call is, numbered from 1 as a run's steps are: a
    /// served session is a run whose steps arrive one JSON-RPC message at a time.
    ///
    /// A [`Cell`] is enough, and for the reason [`Watch`]'s is: [`Store`] is
    /// `!Sync` and the whole session is driven on one task, so a lock here would
    /// be the only synchronisation in the file and would protect nothing.
    step: Cell<u32>,
    approver: &'a dyn Approver,
    watch: Watch<'a>,
    /// No MCP servers of its own. Serving tools is not proxying them: a client
    /// that wants another server's tools connects to that server. Connected with
    /// an empty list rather than faked, so the roster answers `owns` false for
    /// every name through the same code a run's session does.
    mcp: McpSession,
    /// No language servers, for the same reason: an operator who wants one
    /// configures it on a run, and a served session has no contract to carry one.
    lsp: LspSession,
    /// No browser. The tools that need one are never in this catalogue —
    /// [`crate::run::prompts::workspace_tools`] builds them only for a run whose
    /// contract configured a browser — so the session that would drive it is the
    /// one a run with no `browser` config carries.
    browser: BrowserSession,
    /// A live process registry, because `shell_start` **is** served: a handle
    /// opened by one call is polled by the next, and the registry kills whatever
    /// is still live when the session drops.
    handles: Arc<Handles>,
    /// No skills. `read_skill` is in [`MCP_SERVER_UNSERVED`] precisely because
    /// there is no bundle to read, and an empty [`Skills`] is what a contract that
    /// configures none carries.
    skills: Skills,
    /// An empty roster, held rather than built per call only because
    /// [`PlanPhase`] borrows it.
    agents: Agents,
    /// The key this root's durable memory is stored under, through the run
    /// loop's own function rather than a second spelling of it: `remember` and
    /// `forget` are served, and a served session writing under a different key
    /// than a run over the same directory would split one workspace's memory in
    /// two without saying so.
    memory_key: String,
}

impl<'a> Served<'a> {
    /// Open the store, start the session's run, and connect the empty sessions a
    /// served call is dispatched against.
    async fn start(
        config: &McpServerConfig,
        approver: &'a dyn Approver,
        observer: &'a dyn Observer,
    ) -> Result<Self> {
        let store = Store::open(config.store_path())?;
        // The run row every served call is a step against. `file` is the root
        // every served path resolves against, which is the most specific true
        // thing there is to write: a session has no one file it is about.
        let run_id = store.start_run(SERVED_GOAL, &config.root().to_string_lossy())?;
        let watch = Watch::new(observer);
        let mcp = McpSession::connect(&[], config.policy(), &store, run_id, &watch).await?;
        let lsp = LspSession::connect(&[], config.policy(), config.root(), &store, run_id, &watch)
            .await?;
        Ok(Self {
            memory_key: memory_key(config.root()),
            run_id,
            step: Cell::new(1),
            approver,
            watch,
            mcp,
            lsp,
            browser: browser_session(),
            handles: Arc::new(Handles::new(MAX_LIVE_HANDLES)),
            skills: Skills::none(),
            agents: Agents::new(),
            store,
        })
    }

    /// Route one call through [`dispatch`] — the same choke point a model's tool
    /// call takes, and the whole point of serving these tools rather than
    /// re-implementing them.
    ///
    /// Everything below that is empty or defaulted is named at its own parameter.
    /// Nothing here is a shorter path: a served call is gated, journalled and
    /// announced because it goes through this function, not because this function
    /// asks it to be.
    async fn call(&self, config: &McpServerConfig, call: &ToolCall) -> Result<Dispatched> {
        let ws = Workspace::with_policy(config.root(), config.policy().clone());
        let step = self.step.get();
        self.step.set(step + 1);
        dispatch(
            &ws,
            call,
            self.approver,
            // Nothing served asks a question: `ask_question` and `ask_questions`
            // are both in `MCP_SERVER_UNSERVED`, so the only thing a responder
            // could answer here is a question that cannot be asked.
            &ResponderNone,
            &self.store,
            self.run_id,
            step,
            &self.mcp,
            &self.lsp,
            &self.browser,
            config.tools(),
            &self.skills,
            // The cap a run with a default context budget applies. A served
            // session assembles no context of its own, but the cap is also what
            // bounds a shell's captured output, so `usize::MAX` would be a
            // different decision than "no budget to protect" — it would be "no
            // limit on what one call returns".
            entry_cap_chars(ContextBudget::default().effective_tokens(None)),
            // No per-read ceiling, which is what a default contract carries.
            None,
            &self.memory_key,
            MemoryLimits::default(),
            &self.watch,
            // A served session runs no children, so every call is at the root.
            0,
            // Fresh per call and dropped with it. A run carries images from one
            // step into the next request; a served call's answer *is* its result,
            // and there is no next step of this session to carry one into.
            //
            // Built at the call site rather than bound to a local, because
            // `PendingMedia` is feature-dependent: a `Vec<Media>` under `media`
            // and `()` without it, so a binding is a unit value on the default
            // build and `clippy::let_unit_value` fails one polarity of the lint
            // matrix while the other passes.
            &mut PendingMedia::default(),
            // No commit identity configured, which is the same default a contract
            // that names none carries: `git_commit` is served, and it commits as
            // whoever the repository's own git config says.
            &Identity::default(),
            // The ceiling a contract that names none applies, read from the
            // constant rather than by building a whole `TaskContract` per call.
            DEFAULT_EXEC_TIMEOUT,
            // No containment. A served session resolves none, which reads as
            // `ExecMode::FullAccess` — and is exactly why `exec` and the shell
            // tools are an `Ask` under `Policy::default` and refused by `DenyAll`:
            // the boundary that holds here is the policy's, not the sandbox's. An
            // operator who wants a contained server confines the process it runs
            // in, which is the containment an MCP client cannot argue with.
            None,
            // No ecosystem detection. Detecting one reads the directory, which the
            // run loop pays once per run; a served session would pay it per
            // session for edit diagnostics no MCP client asked for.
            None,
            &self.handles,
            // No plan gate, and no planning phase to be in. `propose_plan` is in
            // `MCP_SERVER_UNSERVED` because there is nobody here to decide a plan,
            // so a session that started `active` would deny every write forever.
            PlanPhase {
                gate: None,
                agents: &self.agents,
                active: false,
            },
            SERVED_GOAL,
            // No `before_tool` hooks. They are a contract's, and a server has no
            // contract; the operator's boundary here is the policy they passed.
            None,
            // Nothing withheld. What a session serves is decided once, by
            // `served_tools`, rather than per turn.
            &ToolMask::none(),
        )
        .await
    }

    /// Write the session's outcome, so the run row does not sit `running` after
    /// the client has gone.
    ///
    /// [`Store::finish_run`] rather than the run loop's own `finish`: that one
    /// also emits a `Finished` event carrying the run's token spend, and a served
    /// session spends no tokens — this crate never calls a provider for it.
    fn finish(&self, outcome: &str) -> Result<()> {
        self.store.finish_run(self.run_id, outcome)
    }
}

/// A browser session for a run that configured no browser.
///
/// Two lines behind a function because the shim and the real type are different
/// types with different constructors, and the alternative is a `#[cfg]` pair in
/// the middle of [`Served::start`]'s field list.
fn browser_session() -> BrowserSession {
    #[cfg(feature = "browser")]
    {
        BrowserSession::new(None)
    }
    #[cfg(not(feature = "browser"))]
    {
        BrowserSession
    }
}

/// Serve MCP on stdin and stdout until the client disconnects, refusing
/// anything the policy would ask about.
///
/// The unattended door. Use [`serve_mcp_with`] to supply an approver.
///
/// ```no_run
/// use io_harness::{serve_mcp, McpServerConfig, Policy};
///
/// # async fn run() -> io_harness::Result<()> {
/// serve_mcp(McpServerConfig::new(".", "runs.db").with_policy(Policy::default())).await
/// # }
/// ```
pub async fn serve_mcp(config: McpServerConfig) -> Result<()> {
    serve_mcp_with(config, &DenyAll).await
}

/// Serve MCP on stdin and stdout, routing an asking rule to `approver`.
///
/// ```no_run
/// use io_harness::{serve_mcp_with, ApproveAll, McpServerConfig};
///
/// # async fn run() -> io_harness::Result<()> {
/// serve_mcp_with(McpServerConfig::new(".", "runs.db"), &ApproveAll).await
/// # }
/// ```
pub async fn serve_mcp_with(config: McpServerConfig, approver: &dyn Approver) -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    // Nothing listens. The events a served call emits are emitted either way —
    // that is what makes it observable to anything reading the store afterwards —
    // and `McpServerConfig` deliberately carries no observer: a server started
    // over a pipe has no second channel to report on, and adding one to the
    // config would be a public knob with no caller.
    let observer = Ignore;
    let served = Served::start(&config, approver, &observer).await?;
    // One line in, at most one line out, until the client closes its end. The
    // loop holds no protocol knowledge of its own: everything a message means is
    // decided by `handle_line`, so the tests drive the whole protocol without a
    // process or a pipe.
    let outcome = async {
        while let Some(line) = lines.next_line().await? {
            if let Some(response) = handle_line(&config, &served, &line).await {
                write_response(&mut out, &response).await?;
            }
        }
        Ok::<(), Error>(())
    }
    .await;
    // Finished however the loop left, including by a broken pipe, because the
    // alternative is a `running` row for a session whose client is gone and no
    // way for a later reader to tell it from one still being served. Reported
    // beside the loop's own answer rather than through `?`, so a store that fails
    // to close does not replace the error that ended the session.
    let closed = served.finish(if outcome.is_ok() {
        SUCCESS_OUTCOME
    } else {
        SERVED_FAILED
    });
    outcome.and(closed)
}

/// The one place in this crate that writes to stdout.
///
/// Deliberately the only one. Anything else printed there lands in the middle of
/// the client's message stream, and the symptom is a client that cannot parse
/// rather than an error anyone raises — so diagnostics go to stderr through
/// `tracing`, and every response goes through here. A later test drives a real
/// child process and asserts that stdout carried nothing but JSON-RPC.
///
/// One object per line, newline-terminated and flushed. MCP's stdio transport is
/// newline-delimited — it does *not* use the `Content-Length` header framing
/// [`crate::lsp`] writes for LSP — so a response holding a literal newline would
/// be two messages, which is why it is written with `to_string` rather than
/// pretty-printed.
async fn write_response(out: &mut tokio::io::Stdout, response: &Value) -> Result<()> {
    let mut line = serde_json::to_string(response).expect("a response built here serializes");
    line.push('\n');
    out.write_all(line.as_bytes()).await?;
    out.flush().await?;
    Ok(())
}

/// Answer one line of the client's stream, or nothing when nothing is owed.
///
/// A line that is not JSON is answered with `-32700` against a null id — the id
/// is unknowable when the parse failed, and JSON-RPC says to answer a parse
/// error with a null id rather than to say nothing. The stream continues: one
/// unparseable line from a client is not a reason to stop serving the ones after
/// it.
async fn handle_line(config: &McpServerConfig, served: &Served<'_>, line: &str) -> Option<Value> {
    if line.trim().is_empty() {
        return None;
    }
    match serde_json::from_str::<Value>(line) {
        Ok(message) => handle(config, served, &message).await,
        Err(e) => {
            tracing::warn!("mcp server: a line was not JSON: {e}");
            Some(rpc_error(
                Value::Null,
                PARSE_ERROR,
                "the line was not valid JSON",
            ))
        }
    }
}

/// Answer one parsed message, or nothing when it is a notification.
async fn handle(config: &McpServerConfig, served: &Served<'_>, message: &Value) -> Option<Value> {
    // A batch is an array and a bare scalar is neither. MCP's stdio transport is
    // one message per line and does not carry JSON-RPC batches, so the object
    // check is the whole of what "is this a message" means here.
    let Some(object) = message.as_object() else {
        return Some(rpc_error(
            Value::Null,
            INVALID_REQUEST,
            "a JSON-RPC message is an object",
        ));
    };
    // The absence of `id` is what makes a message a notification, and a
    // notification MUST NOT be answered. Answering `notifications/initialized`
    // sends a response carrying an id the client never issued, which a strict
    // client reads as a protocol error from the first exchange onwards. An `id`
    // that is present but null is a request with a null id, not a notification,
    // so the test is for the key rather than for the value.
    let id = object.get("id")?;
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        return Some(rpc_error(
            id.clone(),
            INVALID_REQUEST,
            "a request carries a `method` string",
        ));
    };
    match method {
        "initialize" => Some(ok(
            id.clone(),
            initialize_result(config, object.get("params")),
        )),
        "tools/list" => Some(ok(id.clone(), tools_list_result(config))),
        "tools/call" => Some(
            match tools_call_result(config, served, object.get("params")).await {
                Ok(result) => ok(id.clone(), result),
                Err((code, why)) => rpc_error(id.clone(), code, &why),
            },
        ),
        _ => Some(rpc_error(
            id.clone(),
            METHOD_NOT_FOUND,
            &format!("`{method}` is not a method this server answers"),
        )),
    }
}

/// The `initialize` result: the negotiated version, what this server can do, and
/// what it is called.
fn initialize_result(config: &McpServerConfig, params: Option<&Value>) -> Value {
    let asked = params
        .and_then(|p| p.get("protocolVersion"))
        .and_then(Value::as_str);
    let version = match asked {
        Some(v) if SUPPORTED_PROTOCOL_VERSIONS.contains(&v) => v,
        // A version this server does not speak, or none named at all, is
        // answered with this server's own. The client then decides whether it
        // can live with it, which is where that decision belongs.
        _ => MCP_SERVER_PROTOCOL_VERSION,
    };
    json!({
        "protocolVersion": version,
        // `tools` and nothing else. This server lends a tool boundary; it serves
        // no resources, no prompts and no completions, and advertising a
        // capability it does not implement buys a client that calls a method
        // answered with `-32601`.
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": config.server_name(),
            "version": config.server_version(),
        },
    })
}

/// The `tools/list` result: this crate's catalogue, minus what a served session
/// cannot honour.
fn tools_list_result(config: &McpServerConfig) -> Value {
    let tools: Vec<Value> = served_tools(config)
        .into_iter()
        // `ToolSpec::parameters` is already the JSON Schema of the arguments
        // object — the same schema this crate sends providers on every step — and
        // MCP's `inputSchema` is that schema under a different key. It is moved
        // across whole rather than rebuilt, because two descriptions of one tool
        // is the thing that drifts.
        .map(|spec| {
            json!({
                "name": spec.name,
                "description": spec.description,
                "inputSchema": spec.parameters,
            })
        })
        .collect();
    json!({ "tools": tools })
}

/// Every tool a served session could name, offered or not.
///
/// The workspace catalogue plus whatever the operator put in the box. It is not
/// the tree catalogue: a served session runs no children, so `spawn`,
/// `send_message` and `read_messages` are never built here — which is the same
/// reason [`MCP_SERVER_UNSERVED`] names them.
fn catalogue(config: &McpServerConfig) -> Vec<ToolSpec> {
    let mut tools = crate::run::prompts::workspace_tools();
    tools.extend(config.tools().specs());
    tools
}

/// What `tools/list` offers: the catalogue with [`MCP_SERVER_UNSERVED`] removed.
///
/// Filtered here rather than refused at call time, because a tool a client can
/// see is a tool a model will spend a step calling, and "there is nobody here to
/// answer your question" is a better answer given before the step than after it.
fn served_tools(config: &McpServerConfig) -> Vec<ToolSpec> {
    catalogue(config)
        .into_iter()
        .filter(|spec| !MCP_SERVER_UNSERVED.contains(&spec.name.as_str()))
        .collect()
}

// The two JSON-RPC codes `tools/call` returns, declared where that lands. A
// malformed request is `-32602` and a failed exchange is `-32603`; a *refused*
// action is neither, and [`tools_call_result`] says why.
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

/// Answer one `tools/call` by routing it through [`dispatch`], or say why the
/// request itself could not be answered.
///
/// `Err` here is a JSON-RPC error: the exchange failed. `Ok` is a `tools/call`
/// result, and that is where every *decided* answer goes — including a refusal.
/// A policy that says no has answered the call, and MCP's shape for that is
/// `isError: true` on a successful exchange. Reported as a `-32xxx` instead it
/// would tell a client its request was malformed, which it was not, and would
/// deny a model the words it needs to try something else.
async fn tools_call_result(
    config: &McpServerConfig,
    served: &Served<'_>,
    params: Option<&Value>,
) -> std::result::Result<Value, (i64, String)> {
    let Some(name) = params.and_then(|p| p.get("name")).and_then(Value::as_str) else {
        return Err((
            INVALID_PARAMS,
            "a `tools/call` names its tool in `name`".to_string(),
        ));
    };
    // Checked against what this server offers before anything is dispatched, and
    // the ordering is the point: [`MCP_SERVER_UNSERVED`] holds tools that would
    // otherwise *work*. `ask_question` dispatched from here would persist a
    // question and stop the session waiting for somebody who is not at the far
    // end of a pipe, so a name `tools/list` never showed must not reach the
    // dispatch that would honour it.
    if !served_tools(config).iter().any(|spec| spec.name == name) {
        return Err((
            INVALID_PARAMS,
            format!("`{name}` is not a tool this server offers"),
        ));
    }
    let call = ToolCall {
        name: name.to_string(),
        // MCP makes `arguments` optional and every tool here takes an object, so
        // an absent one is the empty object rather than an error: it is what a
        // tool with no required argument is called with.
        arguments: params
            .and_then(|p| p.get("arguments"))
            .cloned()
            .unwrap_or_else(|| json!({})),
    };
    match served.call(config, &call).await {
        // The tool answered, or the gate refused it. `ObsKind::Error` is what the
        // dispatch marks both a refusal and a tool failure with — `Dispatched::go`
        // is the one constructor for both — so `isError` reads that rather than a
        // second classification made here, which could disagree with it.
        Ok(Dispatched::Continue { obs, kind, .. }) => {
            Ok(call_content(&obs, kind == ObsKind::Error))
        }
        // An approver deferred. Never under [`DenyAll`], which decides every
        // time; reachable when an operator's own [`Approver`] returns
        // `Decision::Defer`. The pending row is durable, so the answer names it:
        // this is a decided outcome about the action, not a failed exchange.
        Ok(Dispatched::Pause { request_id }) => Ok(call_content(
            &format!(
                "\n[{name} deferred] the approver did not decide; request {request_id} is \
                 pending and an attached process can answer it.\n"
            ),
            true,
        )),
        // Neither of the next two can arrive: `ask_question`, `ask_questions` and
        // `propose_plan` are the only tools that produce them and all three are
        // unserved. Answered by name rather than by a wildcard, so a tool moved
        // into the served set without an answer for its outcome is a compile
        // error here rather than a silent `-32603` in production.
        Ok(Dispatched::Ask { question_id }) => Ok(call_content(
            &format!("\n[{name} asked] question {question_id} has nobody here to answer it.\n"),
            true,
        )),
        Ok(Dispatched::Plan { plan_id, .. }) => Ok(call_content(
            &format!("\n[{name} proposed] plan {plan_id} has no gate here to decide it.\n"),
            true,
        )),
        // A refusal that leaves as an error rather than as a result — the network
        // guard raises one — is the same decision wearing a different shape, and
        // is answered the same way. Its `Display` is this crate's own refusal
        // text, so the client reads the words the policy wrote.
        Err(e @ Error::Refused { .. }) => Ok(call_content(&format!("\n[refused] {e}\n"), true)),
        // Everything else failed rather than decided: a store that would not
        // write, a root that would not resolve. That is not an answer about the
        // action and is not dressed up as one — `-32603` says the server could
        // not complete the exchange, which is what happened.
        Err(e) => Err((INTERNAL_ERROR, e.to_string())),
    }
}

/// One `tools/call` result: the text a client renders, and whether it reports a
/// refused or failed action.
///
/// One text block, because a dispatched observation is already one string — the
/// same string a model is handed. Splitting it here would invent a structure the
/// crate does not have.
fn call_content(text: &str, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    })
}

/// A JSON-RPC response carrying a result.
///
/// The `id` is carried back as the value it arrived as — a number stays a
/// number, a string stays a string — because a client matches a response to its
/// request by equality on that value, and a coerced id matches nothing.
fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// A JSON-RPC response carrying an error. Exactly one of `result` and `error` is
/// present in a response, so this is never merged with [`ok`].
fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::approve::ApproveAll;
    use crate::observe::{Flow, RunEvent};
    use crate::tools::{Tool, ToolFuture};

    /// Every event a session announced, in order.
    ///
    /// `serve_mcp_with` registers [`Ignore`], so this is the only place the
    /// channel is read. What it proves is not that somebody listens — nobody has
    /// to — but that a served call announces itself on the way through, which is
    /// a property of the dispatch and would be lost by a shorter path.
    #[derive(Default)]
    struct Seen(Mutex<Vec<RunEvent>>);

    impl Observer for Seen {
        fn event(&self, event: &RunEvent) -> Flow {
            self.0
                .lock()
                .expect("the recorder is not poisoned")
                .push(event.clone());
            Flow::Continue
        }
    }

    impl Seen {
        /// Each event's kind, rendered. Compared by text because [`EventKind`] is
        /// `#[non_exhaustive]` and a match here would be a second copy of the
        /// variant list to keep up to date.
        ///
        /// [`EventKind`]: crate::observe::EventKind
        fn kinds(&self) -> Vec<String> {
            self.0
                .lock()
                .expect("the recorder is not poisoned")
                .iter()
                .map(|event| format!("{:?}", event.kind))
                .collect()
        }
    }

    /// A server over a temporary root, and the two things a session borrows.
    ///
    /// One struct because [`Served`] borrows its approver and its observer for
    /// the length of the session: a helper that returned a `Served` alone would
    /// have to return those too.
    struct Harness {
        _dir: tempfile::TempDir,
        config: McpServerConfig,
        seen: Seen,
        approver: Box<dyn Approver>,
    }

    impl Harness {
        fn new(policy: Policy, tools: Toolbox, approver: Box<dyn Approver>) -> Self {
            let dir = tempfile::tempdir().expect("a temporary directory");
            let config = McpServerConfig::new(dir.path(), dir.path().join("runs.db"))
                .with_policy(policy)
                .with_tools(tools);
            Self {
                _dir: dir,
                config,
                seen: Seen::default(),
                approver,
            }
        }

        /// The unattended server: the tiered default policy, no registered tools,
        /// and the approver a bare [`serve_mcp`] uses.
        fn unattended() -> Self {
            Self::new(Policy::default(), Toolbox::new(), Box::new(DenyAll))
        }

        async fn session(&self) -> Served<'_> {
            Served::start(&self.config, self.approver.as_ref(), &self.seen)
                .await
                .expect("a session starts")
        }

        async fn answer(
            &self,
            served: &Served<'_>,
            id: Value,
            method: &str,
            params: Value,
        ) -> Value {
            handle(&self.config, served, &request(id, method, params))
                .await
                .expect("a request is answered")
        }

        /// One `tools/call`, answered.
        async fn call(&self, served: &Served<'_>, name: &str, arguments: Value) -> Value {
            self.answer(
                served,
                json!(1),
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            )
            .await
        }
    }

    /// The text a `tools/call` result carries, joined.
    fn text_of(response: &Value) -> String {
        response["result"]["content"]
            .as_array()
            .expect("a result carries a content array")
            .iter()
            .map(|block| {
                block["text"]
                    .as_str()
                    .expect("a text block carries text")
                    .to_string()
            })
            .collect()
    }

    fn request(id: Value, method: &str, params: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    fn config() -> McpServerConfig {
        McpServerConfig::new(".", "runs.db")
    }

    fn served_names() -> Vec<String> {
        served_tools(&config())
            .into_iter()
            .map(|spec| spec.name)
            .collect()
    }

    /// The `initialize` result for a client asking for `version`, or for one that
    /// names none.
    async fn initialize(harness: &Harness, served: &Served<'_>, version: Option<&str>) -> Value {
        let params = match version {
            Some(v) => json!({ "protocolVersion": v, "capabilities": {} }),
            None => json!({ "capabilities": {} }),
        };
        harness.answer(served, json!(1), "initialize", params).await["result"].clone()
    }

    #[tokio::test]
    async fn f10_a_line_that_is_not_json_is_answered_with_a_parse_error_and_a_null_id() {
        let h = Harness::unattended();
        let s = h.session().await;
        let response = handle_line(&h.config, &s, "{ this is not json")
            .await
            .expect("a parse error answers");
        assert_eq!(response["error"]["code"], json!(PARSE_ERROR));
        assert_eq!(response["id"], Value::Null);
        assert_eq!(response["jsonrpc"], "2.0");
        assert!(response.get("result").is_none(), "one of result or error");
    }

    #[tokio::test]
    async fn f10_a_bad_line_does_not_end_the_stream() {
        let h = Harness::unattended();
        let s = h.session().await;
        assert!(handle_line(&h.config, &s, "not json").await.is_some());
        let next = handle_line(
            &h.config,
            &s,
            r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#,
        )
        .await
        .expect("the line after a bad one is still answered");
        assert!(next["result"]["tools"].is_array());
        assert_eq!(next["id"], json!(7));
    }

    #[tokio::test]
    async fn f10_a_request_with_no_method_is_answered_with_invalid_request() {
        let h = Harness::unattended();
        let s = h.session().await;
        let response = handle(&h.config, &s, &json!({ "jsonrpc": "2.0", "id": "abc" }))
            .await
            .expect("a request is answered");
        assert_eq!(response["error"]["code"], json!(INVALID_REQUEST));
        assert_eq!(
            response["id"],
            json!("abc"),
            "the id comes back as a string"
        );
    }

    #[tokio::test]
    async fn f10_a_message_that_is_not_an_object_is_answered_with_invalid_request() {
        let h = Harness::unattended();
        let s = h.session().await;
        let response = handle(&h.config, &s, &json!([1, 2, 3]))
            .await
            .expect("a request is answered");
        assert_eq!(response["error"]["code"], json!(INVALID_REQUEST));
    }

    #[tokio::test]
    async fn f10_a_blank_line_is_not_answered() {
        let h = Harness::unattended();
        let s = h.session().await;
        assert!(handle_line(&h.config, &s, "   ").await.is_none());
    }

    #[tokio::test]
    async fn f10_an_unknown_method_is_answered_with_method_not_found() {
        let h = Harness::unattended();
        let s = h.session().await;
        let response = h.answer(&s, json!(2), "resources/list", json!({})).await;
        assert_eq!(response["error"]["code"], json!(METHOD_NOT_FOUND));
    }

    #[tokio::test]
    async fn f11_initialize_echoes_back_a_version_the_server_also_speaks() {
        let h = Harness::unattended();
        let s = h.session().await;
        for &asked in SUPPORTED_PROTOCOL_VERSIONS {
            assert_eq!(
                initialize(&h, &s, Some(asked)).await["protocolVersion"],
                json!(asked),
                "a client asking for {asked} is answered with it"
            );
        }
    }

    #[tokio::test]
    async fn f11_initialize_answers_with_its_own_version_when_the_client_asks_for_one_it_does_not_speak(
    ) {
        let h = Harness::unattended();
        let s = h.session().await;
        for asked in [Some("2024-11-05"), Some(""), None] {
            assert_eq!(
                initialize(&h, &s, asked).await["protocolVersion"],
                json!(MCP_SERVER_PROTOCOL_VERSION),
                "an unsupported or absent request falls back to the server's own"
            );
        }
    }

    #[tokio::test]
    async fn f11_initialize_advertises_tools_and_no_other_capability() {
        let h = Harness::unattended();
        let s = h.session().await;
        let result = initialize(&h, &s, Some(MCP_SERVER_PROTOCOL_VERSION)).await;
        let capabilities = result["capabilities"]
            .as_object()
            .expect("capabilities is an object");
        assert_eq!(
            capabilities.keys().collect::<Vec<_>>(),
            vec!["tools"],
            "tools and nothing else"
        );
        assert_eq!(result["serverInfo"]["name"], json!(env!("CARGO_PKG_NAME")));
        assert_eq!(
            result["serverInfo"]["version"],
            json!(env!("CARGO_PKG_VERSION"))
        );
    }

    #[tokio::test]
    async fn f11_a_notification_is_never_answered() {
        let h = Harness::unattended();
        let s = h.session().await;
        for method in ["notifications/initialized", "notifications/cancelled"] {
            let notification = json!({ "jsonrpc": "2.0", "method": method });
            assert!(
                handle(&h.config, &s, &notification).await.is_none(),
                "{method} carries no id and so is owed no response"
            );
        }
    }

    #[tokio::test]
    async fn f11_a_request_with_a_null_id_is_still_a_request() {
        // Present-but-null is a request with a null id, which JSON-RPC
        // discourages but does not forbid. Only an absent `id` is a
        // notification, and conflating the two would silence a client that
        // sends one.
        let h = Harness::unattended();
        let s = h.session().await;
        let response = handle(
            &h.config,
            &s,
            &request(Value::Null, "tools/list", json!({})),
        )
        .await;
        assert!(response.is_some());
    }

    #[test]
    fn f12_every_served_tool_maps_its_spec_across_verbatim() {
        let config = config();
        let listed = tools_list_result(&config);
        let listed = listed["tools"].as_array().expect("tools is an array");
        let source = served_tools(&config);
        assert_eq!(listed.len(), source.len());
        assert!(!source.is_empty(), "the catalogue is not empty");
        for (entry, spec) in listed.iter().zip(source.iter()) {
            assert_eq!(entry["name"], json!(spec.name));
            assert_eq!(entry["description"], json!(spec.description));
            assert_eq!(
                entry["inputSchema"], spec.parameters,
                "`{}`'s schema is moved across, not rebuilt",
                spec.name
            );
            assert_eq!(
                entry.as_object().expect("an entry is an object").len(),
                3,
                "`{}` carries the three MCP keys and no fourth",
                spec.name
            );
        }
    }

    #[test]
    fn f12_the_served_and_unserved_sets_partition_the_catalogue() {
        let config = config();
        let catalogue: Vec<String> = catalogue(&config)
            .into_iter()
            .map(|spec| spec.name)
            .collect();
        let served = served_names();
        for name in &catalogue {
            let is_served = served.contains(name);
            let is_unserved = MCP_SERVER_UNSERVED.contains(&name.as_str());
            assert!(
                is_served != is_unserved,
                "`{name}` is in exactly one of the served and unserved sets, not {}",
                match is_served {
                    true => "both",
                    false => "neither",
                }
            );
        }
        assert_eq!(
            served.len()
                + catalogue
                    .iter()
                    .filter(|name| MCP_SERVER_UNSERVED.contains(&name.as_str()))
                    .count(),
            catalogue.len(),
            "nothing is dropped between the catalogue and the served set"
        );
    }

    #[test]
    fn f12_no_name_in_the_unserved_set_is_served() {
        let served = served_names();
        for &unserved in MCP_SERVER_UNSERVED {
            assert!(
                !served.iter().any(|name| name.as_str() == unserved),
                "`{unserved}` needs something a served session has not got"
            );
        }
        // The two the workspace catalogue actually builds. The rest —
        // `propose_plan`, the tree tools and `read_skill` — are never built for
        // a served session at all, and the loop above is what says so if that
        // ever changes.
        assert!(
            catalogue(&config())
                .iter()
                .any(|spec| spec.name == crate::tools::ASK_QUESTION_TOOL),
            "`ask_question` is in the catalogue, so its exclusion is doing work"
        );
    }

    // ------------------------------------------------------------ F13: the gate

    #[tokio::test]
    async fn f13_a_served_call_leaves_one_gate_decision_per_call_in_policy_events() {
        // The sabotage arm of F13. A `tools/call` answered by anything that does
        // not reach the policy gate — a shorter path written to avoid assembling
        // `dispatch`'s arguments — leaves no row here at all, so this fails at
        // zero rather than passing quietly.
        let h = Harness::unattended();
        let s = h.session().await;
        for (n, path) in ["a.txt", "b.txt"].iter().enumerate() {
            h.call(&s, "write_file", json!({ "path": path, "content": "x" }))
                .await;
            let events = s.store.events(s.run_id).expect("the trace is readable");
            assert_eq!(
                events.len(),
                n + 1,
                "one gate decision per served call, not {}",
                events.len()
            );
            let last = events.last().expect("a row was just written");
            assert_eq!(last.act, "write");
            assert_eq!(last.target, *path);
            assert_eq!(
                last.step,
                n as u32 + 1,
                "the row is attributed to the step the call was"
            );
        }
    }

    #[tokio::test]
    async fn f13_a_served_call_opens_a_journal_attempt_and_announces_itself() {
        // A built-in tool is `ToolRecovery::Replayable` and writes no attempt at
        // all, which is the journal working as designed — so the subject here is
        // a registered tool, whose default recovery is `Indeterminate`.
        let open = Arc::new(Mutex::new(Vec::new()));
        let dir = tempfile::tempdir().expect("a temporary directory");
        let store_path = dir.path().join("runs.db");
        let config = McpServerConfig::new(dir.path(), &store_path)
            // The tool is an `Act::Exec` check on its own name, so it has to be
            // allowed for the call to get past the gate and reach the journal at
            // all.
            .with_policy(Policy::default().allow_exec(JournalProbe::NAME))
            .with_tools(Toolbox::new().with(JournalProbe {
                store_path: store_path.clone(),
                open: Arc::clone(&open),
            }));
        let h = Harness {
            _dir: dir,
            config,
            seen: Seen::default(),
            approver: Box::new(ApproveAll),
        };
        let s = h.session().await;
        assert_eq!(
            s.run_id, 1,
            "the store is fresh, so the probe's hard-coded run id is this session's"
        );
        let response = h.call(&s, JournalProbe::NAME, json!({})).await;
        assert_eq!(
            response["result"]["isError"],
            json!(false),
            "the call was allowed: {}",
            text_of(&response)
        );
        assert_eq!(
            *open.lock().expect("the probe is not poisoned"),
            vec![JournalProbe::NAME.to_string()],
            "the attempt was open while the call ran"
        );
        assert!(
            h.seen
                .kinds()
                .iter()
                .any(|kind| kind.starts_with("ToolCall") && kind.contains(JournalProbe::NAME)),
            "the call is announced on the observer channel: {:?}",
            h.seen.kinds()
        );
    }

    /// A registered tool that reads the journal from a second connection while it
    /// is running.
    ///
    /// The attempt row is opened before the call and closed after it, and nothing
    /// public reads a *closed* one — so the only honest place to see it is from
    /// inside the call. A second [`Store`] over the same file is what makes that
    /// possible, and is exactly what `open_attempt` means by committing "on its
    /// own, outside every step transaction": the row is durable before the work
    /// starts, which is the whole claim.
    ///
    /// The run id is hard-coded to 1 because the store is created for one test
    /// and holds one session; the test asserts that rather than assuming it.
    struct JournalProbe {
        store_path: PathBuf,
        open: Arc<Mutex<Vec<String>>>,
    }

    impl JournalProbe {
        const NAME: &'static str = "journal_probe";
    }

    impl Tool for JournalProbe {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: Self::NAME.to_string(),
                description: "Report the journal attempts open while it runs.".to_string(),
                parameters: json!({ "type": "object", "properties": {} }),
            }
        }

        fn invoke<'a>(&'a self, _arguments: &'a Value) -> ToolFuture<'a> {
            // Opened, read and dropped with no `await` between, because `Store` is
            // `!Send` and a `ToolFuture` is not.
            let store = Store::open(&self.store_path).expect("the session's store is readable");
            let names: Vec<String> = store
                .open_attempts(1)
                .expect("the journal is readable")
                .into_iter()
                .map(|attempt| attempt.tool)
                .collect();
            self.open
                .lock()
                .expect("the probe is not poisoned")
                .extend(names);
            Box::pin(async { Ok("probed".to_string()) })
        }
    }

    // ----------------------------------------------------- F14: refusal, not a hang

    #[tokio::test]
    async fn f14_a_denied_call_is_a_result_with_is_error_carrying_the_policy_s_own_words() {
        let h = Harness::new(
            Policy::default().layer("test").deny_write("a.txt"),
            Toolbox::new(),
            Box::new(DenyAll),
        );
        let s = h.session().await;
        let response = h
            .call(&s, "write_file", json!({ "path": "a.txt", "content": "x" }))
            .await;
        assert!(
            response.get("error").is_none(),
            "a refusal is a decided exchange, not a transport error: {response}"
        );
        assert_eq!(response["result"]["isError"], json!(true));
        let text = text_of(&response);
        assert!(
            text.contains("write refused") && text.contains("the policy forbids this"),
            "the client is handed this crate's own refusal words: {text}"
        );
        assert!(
            !h.config.root().join("a.txt").exists(),
            "nothing was written"
        );
    }

    #[tokio::test]
    async fn f14_an_asking_rule_under_deny_all_is_refused_rather_than_waited_on() {
        // `Policy::default` puts a write in the asking tier, and `DenyAll` is what
        // `serve_mcp` registers. No timeout appears below on purpose: if the
        // server ever grew a path that blocks on an approver who is not there,
        // this test would hang, and a hang is the honest report of a hang.
        let h = Harness::unattended();
        let s = h.session().await;
        let response = h
            .call(&s, "write_file", json!({ "path": "a.txt", "content": "x" }))
            .await;
        assert!(response.get("error").is_none(), "{response}");
        assert_eq!(response["result"]["isError"], json!(true));
        assert!(
            text_of(&response).contains("no approver available"),
            "the asking tier resolves as a refusal carrying the approver's reason: {}",
            text_of(&response)
        );
    }

    #[tokio::test]
    async fn f14_a_tool_this_server_does_not_offer_never_reaches_dispatch() {
        let h = Harness::unattended();
        let s = h.session().await;
        for name in [crate::tools::ASK_QUESTION_TOOL, "no_such_tool"] {
            let response = h.call(&s, name, json!({})).await;
            assert_eq!(
                response["error"]["code"],
                json!(INVALID_PARAMS),
                "`{name}` is refused by name before anything is dispatched"
            );
        }
        assert!(
            s.store
                .events(s.run_id)
                .expect("the trace is readable")
                .is_empty(),
            "nothing was gated, because nothing was dispatched"
        );
    }

    #[tokio::test]
    async fn f14_a_call_naming_no_tool_is_an_invalid_params_error() {
        let h = Harness::unattended();
        let s = h.session().await;
        let response = h.answer(&s, json!(3), "tools/call", json!({})).await;
        assert_eq!(response["error"]["code"], json!(INVALID_PARAMS));
        assert!(response.get("result").is_none(), "one of result or error");
    }

    // --------------------------------------------- F15: a served session is a run

    #[tokio::test]
    async fn f15_a_session_is_one_run_whose_steps_are_its_calls_and_whose_outcome_is_written() {
        let h = Harness::unattended();
        let s = h.session().await;
        assert_eq!(
            s.store.runs().expect("the runs are readable"),
            vec![s.run_id],
            "one run row per session, started once"
        );
        for path in ["a.txt", "b.txt", "c.txt"] {
            h.call(&s, "write_file", json!({ "path": path, "content": "x" }))
                .await;
        }
        assert_eq!(
            s.store.runs().expect("the runs are readable"),
            vec![s.run_id],
            "a call is a step against the session's run, never a run of its own"
        );
        assert_eq!(
            s.store
                .events(s.run_id)
                .expect("the trace is readable")
                .iter()
                .map(|event| event.step)
                .collect::<Vec<_>>(),
            vec![1, 2, 3],
            "the steps are numbered from 1, one per call"
        );
        assert_eq!(
            s.store.outcome(s.run_id).expect("the run is readable"),
            None,
            "the outcome is not written until the client disconnects"
        );
        s.finish(SUCCESS_OUTCOME).expect("the run is finished");
        assert_eq!(
            s.store.outcome(s.run_id).expect("the run is readable"),
            Some(SUCCESS_OUTCOME.to_string()),
            "a finished session does not sit `running`"
        );
    }

    #[tokio::test]
    async fn f15_a_served_session_reads_back_like_any_other_run() {
        // Through `Broadcast`, which is how *any* run's events become durable —
        // the run loop does not write `run_events` either, the observer a caller
        // wraps does. A served session accepts one because it announces on the
        // ordinary channel, and that is the whole of what F15 claims: no reader
        // needs to know an MCP client was at the far end.
        let h = Harness::unattended();
        let broadcast = crate::observe::Broadcast::new(
            Store::open(h.config.store_path()).expect("a second connection opens"),
            &h.seen,
        );
        let s = Served::start(&h.config, h.approver.as_ref(), &broadcast)
            .await
            .expect("a session starts");
        h.call(&s, "write_file", json!({ "path": "a.txt", "content": "x" }))
            .await;
        s.finish(SUCCESS_OUTCOME).expect("the run is finished");
        // From a third connection opened after the fact, so nothing being read
        // depends on the session still existing.
        let store = Store::open(h.config.store_path()).expect("the store reopens");
        let events = store
            .events_since(s.run_id, 0, 100)
            .expect("the events are readable");
        assert!(
            events
                .iter()
                .any(|(_, event)| format!("{:?}", event.kind).starts_with("ToolCall")),
            "the call is in the durable event stream, not only on the channel: {:?}",
            events
                .iter()
                .map(|(_, event)| format!("{:?}", event.kind))
                .collect::<Vec<_>>()
        );
        assert_eq!(
            store.outcome(s.run_id).expect("the run is readable"),
            Some(SUCCESS_OUTCOME.to_string())
        );
    }

    // ------------------------------------------- N5: the promotions are bounded

    #[test]
    fn nf5_every_promoted_item_is_reachable_from_the_crate_root_and_absent_from_the_public_surface()
    {
        // Reachability is a compile-time fact, stated by full `crate::` path
        // rather than through this file's `use` lines — an import would resolve
        // against itself and prove nothing. Each of these fails to compile if
        // either its module or the item stays `pub(super)`.
        let _: fn(&Path) -> String = crate::run::memory::memory_key;
        let _: Option<crate::run::gate::Dispatched> = None;
        let _: Option<crate::run::gate::PlanPhase<'_>> = None;
        let _ = crate::run::dispatch::dispatch;

        // And no wider than that. `mod run;` is private in `src/lib.rs`, so a
        // promoted name reaching the public surface would be a mistake nothing
        // else catches: `tests/public_api.rs` compares this file against what the
        // crate exports, and an item inside a private module is exported by
        // neither. The name is read out of the snapshot rather than assumed
        // absent from it.
        let surface = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/public-api.txt"),
        )
        .expect("the public API snapshot is readable from its own crate")
        .replace("\r\n", "\n");
        for promoted in ["dispatch", "Dispatched", "PlanPhase", "memory_key"] {
            assert!(
                !surface
                    .lines()
                    .filter(|line| !line.starts_with('#'))
                    .any(|line| line.split_whitespace().nth(1) == Some(promoted)),
                "`{promoted}` is crate-private machinery and must not be in the public surface"
            );
        }
    }
}
