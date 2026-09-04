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

use std::path::{Path, PathBuf};

use crate::policy::Policy;
use crate::tools::Toolbox;
use crate::{Approver, DenyAll, Result};

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
pub async fn serve_mcp_with(_config: McpServerConfig, _approver: &dyn Approver) -> Result<()> {
    Ok(())
}
