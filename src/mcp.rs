//! MCP — tools the harness did not ship, reachable without a fork.
//!
//! The harness is an MCP **client**. It connects to servers the operator
//! configured, discovers their tools, and offers them to the model beside the
//! built-in `write_file`, `read_file`, `grep`, and `find`. A capability the crate
//! lacks is added by pointing it at a server, not by patching it.
//!
//! Two transports: [`McpTransport::Stdio`], where the harness spawns the server
//! as a child process, and [`McpTransport::Http`], where it dials a URL. Both
//! pass through the permission model before anything happens — spawning a server
//! is an [`Act::Exec`] check on its binary, dialling one is an [`Act::Net`] check
//! on its host — and every discovered tool is namespaced `mcp__<server>__<tool>`
//! so a server can never shadow a built-in.
//!
//! # What this does not govern
//!
//! Once a stdio server is running it is a separate process, and it dials whatever
//! it likes. The harness decides whether it may start and which of its tools may
//! be called; it does not sit between that process and the network. Isolating a
//! server's own egress would need OS-level containment, which is not what 0.8
//! builds.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use rmcp::model::CallToolRequestParams;
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use rmcp::ServiceExt;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::error::{Error, Result};
use crate::net::{self, NetGuard};
use crate::observe::{EventKind, RunEvent};
use crate::policy::{Act, Effect, Policy};
use crate::provider::ToolSpec;
use crate::run::{refused, Watch};
use crate::state::{McpEvent, PolicyEvent, Store};

/// The prefix every MCP-provided tool name carries.
///
/// Namespacing is not cosmetic: without it a server advertising `write_file`
/// would shadow the built-in that edits the workspace, and the model would have
/// no way to tell which one it was calling.
pub const MCP_TOOL_PREFIX: &str = "mcp__";

/// Default per-call timeout. A third-party tool that never returns must not
/// become a run that never ends.
const DEFAULT_TIMEOUT_SECS: u64 = 60;

fn default_timeout_secs() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

/// How to reach one MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub enum McpTransport {
    /// Spawn the server as a child process and speak over its stdio.
    Stdio {
        /// The server binary. Checked as [`Act::Exec`] before it is spawned.
        command: String,
        /// Arguments passed to it.
        #[serde(default)]
        args: Vec<String>,
        /// Extra environment for the child (e.g. an API token the server needs).
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    /// Dial a remote server over streamable HTTP.
    Http {
        /// The server's endpoint. Its host is checked as [`Act::Net`].
        url: String,
        /// Static headers sent with every request (e.g. `Authorization`).
        #[serde(default)]
        headers: BTreeMap<String, String>,
    },
}

/// One configured MCP server.
///
/// `Serialize`/`Deserialize` because io-cli and io-studio will express these in
/// their own config files, the same way they already express a [`Policy`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServer {
    /// Short name for this server, used in tool names and in the trace. Keep it
    /// stable: renaming it renames every tool the model sees.
    pub id: String,
    /// Where and how to reach it.
    #[serde(flatten)]
    pub transport: McpTransport,
    /// Per-call timeout in seconds.
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

impl McpServer {
    /// A server the harness spawns as a child process.
    pub fn stdio(id: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            transport: McpTransport::Stdio {
                command: command.into(),
                args: Vec::new(),
                env: BTreeMap::new(),
            },
            timeout_secs: DEFAULT_TIMEOUT_SECS,
        }
    }

    /// A server the harness dials over streamable HTTP.
    pub fn http(id: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            transport: McpTransport::Http {
                url: url.into(),
                headers: BTreeMap::new(),
            },
            timeout_secs: DEFAULT_TIMEOUT_SECS,
        }
    }

    /// Arguments for a stdio server. No-op for an HTTP one.
    pub fn with_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        if let McpTransport::Stdio { args: a, .. } = &mut self.transport {
            *a = args.into_iter().map(Into::into).collect();
        }
        self
    }

    /// Per-call timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout_secs = timeout.as_secs().max(1);
        self
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs.max(1))
    }
}

/// One connected server and the tools it offered.
struct Connected {
    id: String,
    service: RunningService<RoleClient, ()>,
    timeout: Duration,
    tools: Vec<ToolSpec>,
}

/// Every MCP server a run is connected to, for the life of that run.
///
/// One session per run, shared by the whole agent tree rather than one per
/// agent: a server is a stateful process, and 100 concurrent agents opening 100
/// connections to it would be the concurrency problem 0.5.0 already solved once.
pub(crate) struct McpSession {
    servers: Vec<Connected>,
}

impl McpSession {
    /// Connect to every configured server, checking each against `policy` first.
    ///
    /// A server that cannot be reached fails the run with a typed error rather
    /// than being skipped. Silently running without a tool the operator asked
    /// for is the worse failure: the agent would work around a capability it was
    /// supposed to have, and the run would look successful.
    pub(crate) async fn connect(
        servers: &[McpServer],
        policy: &Policy,
        store: &Store,
        run_id: i64,
        watch: &Watch<'_>,
    ) -> Result<Self> {
        let mut connected = Vec::new();
        for server in servers {
            let started = Instant::now();
            let service = match &server.transport {
                McpTransport::Stdio { command, args, env } => {
                    authorize_spawn(command, policy, store, run_id, watch)?;
                    let mut cmd = tokio::process::Command::new(command);
                    cmd.args(args);
                    for (k, v) in env {
                        cmd.env(k, v);
                    }
                    let transport = TokioChildProcess::new(cmd).map_err(|e| Error::Mcp {
                        server: server.id.clone(),
                        reason: format!("could not spawn {command}: {e}"),
                    })?;
                    ().serve(transport).await.map_err(|e| Error::Mcp {
                        server: server.id.clone(),
                        reason: format!("handshake failed: {e}"),
                    })?
                }
                McpTransport::Http { url, headers } => {
                    NetGuard::new(policy).tracing(store, run_id, 0).check(url)?;
                    let mut config = StreamableHttpClientTransportConfig::with_uri(url.clone());
                    // Built as one map and set once: `custom_headers` replaces
                    // the whole map, so setting it per header would keep only
                    // the last one — and an auth header silently dropped is the
                    // kind of bug that looks like the server rejecting you.
                    let custom: std::collections::HashMap<_, _> = headers
                        .iter()
                        .filter_map(|(k, v)| {
                            match (
                                k.parse::<reqwest::header::HeaderName>(),
                                v.parse::<reqwest::header::HeaderValue>(),
                            ) {
                                (Ok(name), Ok(value)) => Some((name, value)),
                                _ => None,
                            }
                        })
                        .collect();
                    if !custom.is_empty() {
                        config = config.custom_headers(custom);
                    }
                    let transport =
                        StreamableHttpClientTransport::with_client(net::http_client(), config);
                    ().serve(transport).await.map_err(|e| Error::Mcp {
                        server: server.id.clone(),
                        reason: format!("could not connect to {url}: {e}"),
                    })?
                }
            };

            let listed = tokio::time::timeout(server.timeout(), service.list_all_tools())
                .await
                .map_err(|_| Error::Mcp {
                    server: server.id.clone(),
                    reason: "timed out listing tools".into(),
                })?
                .map_err(|e| Error::Mcp {
                    server: server.id.clone(),
                    reason: format!("could not list tools: {e}"),
                })?;

            let tools: Vec<ToolSpec> = listed
                .iter()
                .map(|t| ToolSpec {
                    name: tool_name(&server.id, &t.name),
                    description: t.description.as_deref().unwrap_or_default().to_string(),
                    parameters: serde_json::Value::Object((*t.input_schema).clone()),
                })
                .collect();

            // `detail` carries the transport and nothing else — the tool
            // count is already implied by the `discovered` events that
            // follow, and overwriting it here would lose the one fact only
            // this event records.
            let ev = McpEvent::connected(&server.id, transport_name(&server.transport))
                .with_millis(started.elapsed().as_millis() as u64);
            store.record_mcp(run_id, &ev)?;
            announce(watch, run_id, 0, &ev);
            for t in &tools {
                let ev = McpEvent::discovered(&server.id, &t.name);
                store.record_mcp(run_id, &ev)?;
                announce(watch, run_id, 0, &ev);
            }
            info!(server = %server.id, tools = tools.len(), "mcp server connected");

            connected.push(Connected {
                id: server.id.clone(),
                service,
                timeout: server.timeout(),
                tools,
            });
        }
        Ok(Self { servers: connected })
    }

    /// Every discovered tool, ready to offer to the model beside the built-ins.
    pub(crate) fn tool_specs(&self) -> Vec<ToolSpec> {
        self.servers
            .iter()
            .flat_map(|s| s.tools.iter().cloned())
            .collect()
    }

    /// Does this namespaced name belong to a connected server?
    pub(crate) fn owns(&self, name: &str) -> bool {
        self.servers
            .iter()
            .any(|s| s.tools.iter().any(|t| t.name == name))
    }

    /// Call one namespaced tool, returning the text the model should see.
    ///
    /// Every failure mode — unknown tool, timeout, transport death, a tool that
    /// reports its own error — comes back as `Ok(text)`, because they are all
    /// things the *model* should react to rather than things that should end the
    /// run. That is the same choice the built-in tools already make for a bad
    /// regex or a refused path.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn call(
        &self,
        name: &str,
        arguments: &serde_json::Value,
        store: &Store,
        run_id: i64,
        step: u32,
        cap: usize,
        watch: &Watch<'_>,
        depth: u32,
    ) -> Result<String> {
        let Some(server) = self
            .servers
            .iter()
            .find(|s| s.tools.iter().any(|t| t.name == name))
        else {
            return Ok(format!("[unknown tool {name}]"));
        };
        let Some(bare) = bare_name(&server.id, name) else {
            return Ok(format!("[unknown tool {name}]"));
        };

        let mut params = CallToolRequestParams::default();
        params.name = bare.to_string().into();
        params.arguments = arguments.as_object().cloned();

        let started = Instant::now();
        let outcome = tokio::time::timeout(server.timeout, server.service.call_tool(params)).await;
        let millis = started.elapsed().as_millis() as u64;

        let (text, ok) = match outcome {
            Err(_) => (
                format!("[{name} timed out after {}s]", server.timeout.as_secs()),
                false,
            ),
            Ok(Err(e)) => (format!("[{name} failed] {e}"), false),
            Ok(Ok(result)) => {
                let body = render(&result);
                let failed = result.is_error.unwrap_or(false);
                if failed {
                    (format!("[{name} reported an error] {body}"), false)
                } else {
                    (body, true)
                }
            }
        };

        let (text, truncated) = crate::tools::cap_result(text, cap);
        let ev = McpEvent::called(&server.id, name, ok)
            .at_step(step)
            .with_millis(millis)
            .with_detail(if truncated { "truncated" } else { "" });
        store.record_mcp(run_id, &ev)?;
        announce(watch, run_id, depth, &ev);
        Ok(text)
    }

    /// Close every connection. Best-effort: a server that already died needs no
    /// goodbye, and a shutdown failure must not mask the run's own outcome.
    pub(crate) async fn shutdown(self, store: &Store, run_id: i64, watch: &Watch<'_>) {
        for s in self.servers {
            let ev = McpEvent::disconnected(&s.id);
            let _ = store.record_mcp(run_id, &ev);
            announce(watch, run_id, 0, &ev);
            let _ = s.service.cancel().await;
        }
    }
}

/// Announce one MCP row to the observer, built from the row itself so the event
/// cannot report a server, tool, outcome or duration the `mcp_events` row does
/// not. The row's own `step` is used — `0` for connect, discover and disconnect,
/// which happen outside any step.
fn announce(watch: &Watch<'_>, run_id: i64, depth: u32, e: &McpEvent) {
    watch.emit(RunEvent::at_depth(
        run_id,
        e.step,
        depth,
        EventKind::Mcp {
            server: e.server.clone(),
            tool: e.tool.clone(),
            ok: e.ok,
            millis: e.millis,
        },
    ));
}

/// Spawning a server binary is an exec, and the exec policy already governs it.
///
/// `Ask` is refused rather than routed to a human: connecting happens before the
/// run's first step, and a server is configuration the operator wrote, not an
/// action the agent chose. Allow it in the policy or do not configure it.
fn authorize_spawn(
    command: &str,
    policy: &Policy,
    store: &Store,
    run_id: i64,
    watch: &Watch<'_>,
) -> Result<()> {
    let verdict = policy.check(Act::Exec, command);
    let mut ev = if verdict.effect == Effect::Allow {
        PolicyEvent::decision(0, "exec", command, "allow", "policy")
    } else {
        PolicyEvent::refusal(0, "exec", command)
    };
    ev.rule = verdict.rule.clone();
    ev.layer = verdict.layer.clone();
    store.record_event(run_id, &ev)?;
    if verdict.effect == Effect::Allow {
        Ok(())
    } else {
        refused(watch, run_id, 0, &ev);
        Err(Error::Refused {
            act: "exec".into(),
            target: command.to_string(),
            rule: verdict.rule,
            layer: verdict.layer,
        })
    }
}

/// `mcp__<server>__<tool>`.
fn tool_name(server: &str, tool: &str) -> String {
    format!("{MCP_TOOL_PREFIX}{server}__{tool}")
}

/// The server-side tool name inside a namespaced one.
fn bare_name<'a>(server: &str, namespaced: &'a str) -> Option<&'a str> {
    namespaced.strip_prefix(&format!("{MCP_TOOL_PREFIX}{server}__"))
}

fn transport_name(t: &McpTransport) -> &'static str {
    match t {
        McpTransport::Stdio { .. } => "stdio",
        McpTransport::Http { .. } => "http",
    }
}

/// Flatten a tool result into text the model can read.
fn render(result: &rmcp::model::CallToolResult) -> String {
    let mut parts: Vec<String> = result
        .content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect();
    if parts.is_empty() {
        if let Some(structured) = &result.structured_content {
            parts.push(structured.to_string());
        }
    }
    if parts.is_empty() {
        // Non-text content (an image, an embedded resource) is not passed
        // through in 0.8 — media is 0.11 — so say so rather than return blank.
        return "(no text content)".to_string();
    }
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_are_namespaced_and_reversible() {
        let n = tool_name("files", "write_file");
        assert_eq!(n, "mcp__files__write_file");
        assert!(n.starts_with(MCP_TOOL_PREFIX));
        assert_ne!(n, "write_file", "a server must not shadow a built-in");
        assert_eq!(bare_name("files", &n), Some("write_file"));
        assert_eq!(bare_name("other", &n), None);
    }

    #[test]
    fn a_server_config_round_trips_through_serde() {
        for server in [
            McpServer::stdio("files", "mcp-files").with_args(["--root", "/tmp"]),
            McpServer::http("remote", "https://mcp.example.com/mcp")
                .with_timeout(Duration::from_secs(5)),
        ] {
            let json = serde_json::to_string(&server).unwrap();
            let back: McpServer = serde_json::from_str(&json).unwrap();
            assert_eq!(server, back, "{json}");
        }
    }

    #[test]
    fn a_stdio_config_omitting_optional_fields_still_parses() {
        let s: McpServer =
            serde_json::from_str(r#"{"id":"files","transport":"stdio","command":"mcp-files"}"#)
                .unwrap();
        assert_eq!(s.timeout_secs, DEFAULT_TIMEOUT_SECS);
        assert!(matches!(s.transport, McpTransport::Stdio { .. }));
    }

    #[test]
    fn an_oversized_result_is_cut_on_a_char_boundary_and_says_so() {
        // The cap is the run's derived per-entry cap (0.10.0) rather than a
        // constant of this module's own; the boundary behaviour is what is asserted.
        let cap_chars =
            crate::context::entry_cap_chars(crate::context::ContextBudget::default().max_tokens);
        let (short, cut) = crate::tools::cap_result("hello".into(), cap_chars);
        assert_eq!((short.as_str(), cut), ("hello", false));

        // Multi-byte characters, so a naive slice would panic.
        let (long, cut) = crate::tools::cap_result("é".repeat(cap_chars), cap_chars);
        assert!(cut);
        assert!(long.contains("[truncated at"));
        assert!(long.len() < 2 * cap_chars);
    }
}
