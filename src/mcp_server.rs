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

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::policy::Policy;
use crate::tools::Toolbox;
use crate::{Approver, DenyAll, Result, ToolSpec};

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
pub async fn serve_mcp_with(config: McpServerConfig, _approver: &dyn Approver) -> Result<()> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    // One line in, at most one line out, until the client closes its end. The
    // loop holds no protocol knowledge of its own: everything a message means is
    // decided by `handle_line`, which is a pure function over text, so the tests
    // drive the whole protocol without a process, a store or a pipe.
    while let Some(line) = lines.next_line().await? {
        if let Some(response) = handle_line(&config, &line) {
            write_response(&mut out, &response).await?;
        }
    }
    Ok(())
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
fn handle_line(config: &McpServerConfig, line: &str) -> Option<Value> {
    if line.trim().is_empty() {
        return None;
    }
    match serde_json::from_str::<Value>(line) {
        Ok(message) => handle(config, &message),
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
fn handle(config: &McpServerConfig, message: &Value) -> Option<Value> {
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
        // `tools/call` is answered here as unknown until dispatch lands, which
        // is the next task: a call routed through the policy gate, the approver
        // and the journal is the whole point of serving, and half of it —
        // accepting the call and doing something narrower than a run does —
        // would be a worse answer than not offering the method yet.
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
    use super::*;

    fn config() -> McpServerConfig {
        McpServerConfig::new(".", "runs.db")
    }

    fn request(id: Value, method: &str, params: Value) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
    }

    fn answer(message: &Value) -> Value {
        handle(&config(), message).expect("a request is answered")
    }

    fn initialize(version: Option<&str>) -> Value {
        let params = match version {
            Some(v) => json!({ "protocolVersion": v, "capabilities": {} }),
            None => json!({ "capabilities": {} }),
        };
        answer(&request(json!(1), "initialize", params))["result"].clone()
    }

    fn served_names() -> Vec<String> {
        served_tools(&config())
            .into_iter()
            .map(|spec| spec.name)
            .collect()
    }

    #[test]
    fn f10_a_line_that_is_not_json_is_answered_with_a_parse_error_and_a_null_id() {
        let response = handle_line(&config(), "{ this is not json").expect("a parse error answers");
        assert_eq!(response["error"]["code"], json!(PARSE_ERROR));
        assert_eq!(response["id"], Value::Null);
        assert_eq!(response["jsonrpc"], "2.0");
        assert!(response.get("result").is_none(), "one of result or error");
    }

    #[test]
    fn f10_a_bad_line_does_not_end_the_stream() {
        let config = config();
        assert!(handle_line(&config, "not json").is_some());
        let next = handle_line(&config, r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#)
            .expect("the line after a bad one is still answered");
        assert!(next["result"]["tools"].is_array());
        assert_eq!(next["id"], json!(7));
    }

    #[test]
    fn f10_a_request_with_no_method_is_answered_with_invalid_request() {
        let response = answer(&json!({ "jsonrpc": "2.0", "id": "abc" }));
        assert_eq!(response["error"]["code"], json!(INVALID_REQUEST));
        assert_eq!(
            response["id"],
            json!("abc"),
            "the id comes back as a string"
        );
    }

    #[test]
    fn f10_a_message_that_is_not_an_object_is_answered_with_invalid_request() {
        let response = answer(&json!([1, 2, 3]));
        assert_eq!(response["error"]["code"], json!(INVALID_REQUEST));
    }

    #[test]
    fn f10_a_blank_line_is_not_answered() {
        assert!(handle_line(&config(), "   ").is_none());
    }

    #[test]
    fn f10_an_unknown_method_is_answered_with_method_not_found() {
        let response = answer(&request(json!(2), "resources/list", json!({})));
        assert_eq!(response["error"]["code"], json!(METHOD_NOT_FOUND));
    }

    #[test]
    fn f10_tools_call_is_not_answered_until_dispatch_lands() {
        let response = answer(&request(json!(3), "tools/call", json!({})));
        assert_eq!(response["error"]["code"], json!(METHOD_NOT_FOUND));
    }

    #[test]
    fn f11_initialize_echoes_back_a_version_the_server_also_speaks() {
        for &asked in SUPPORTED_PROTOCOL_VERSIONS {
            assert_eq!(
                initialize(Some(asked))["protocolVersion"],
                json!(asked),
                "a client asking for {asked} is answered with it"
            );
        }
    }

    #[test]
    fn f11_initialize_answers_with_its_own_version_when_the_client_asks_for_one_it_does_not_speak()
    {
        for asked in [Some("2024-11-05"), Some(""), None] {
            assert_eq!(
                initialize(asked)["protocolVersion"],
                json!(MCP_SERVER_PROTOCOL_VERSION),
                "an unsupported or absent request falls back to the server's own"
            );
        }
    }

    #[test]
    fn f11_initialize_advertises_tools_and_no_other_capability() {
        let result = initialize(Some(MCP_SERVER_PROTOCOL_VERSION));
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

    #[test]
    fn f11_a_notification_is_never_answered() {
        let config = config();
        for method in ["notifications/initialized", "notifications/cancelled"] {
            let notification = json!({ "jsonrpc": "2.0", "method": method });
            assert!(
                handle(&config, &notification).is_none(),
                "{method} carries no id and so is owed no response"
            );
        }
    }

    #[test]
    fn f11_a_request_with_a_null_id_is_still_a_request() {
        // Present-but-null is a request with a null id, which JSON-RPC
        // discourages but does not forbid. Only an absent `id` is a
        // notification, and conflating the two would silence a client that
        // sends one.
        let response = handle(&config(), &request(Value::Null, "tools/list", json!({})));
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
}
