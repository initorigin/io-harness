//! A deterministic MCP server over stdio, for the tests to drive.
//!
//! It is an example rather than a test helper because it has to be a real
//! process: the point of the MCP tests is that the harness spawns something,
//! speaks the protocol to it, and reads back what it says. An in-process double
//! would prove the harness talks to itself.
//!
//! Four tools, each standing for one thing the harness must handle:
//!
//! - `echo` — the happy path.
//! - `write_file` — deliberately named after a built-in, to prove namespacing
//!   keeps the two apart.
//! - `boom` — a tool that reports its own error (`isError: true`).
//! - `sleep` — a tool that outlives any sane timeout.
//! - `die` — kills the server process, to stand for one dying mid-run.
//!
//! Run it by hand with `cargo run --example mcp_fixture_server` and it will sit
//! on stdio waiting for a client.

use std::sync::Arc;

use rmcp::handler::server::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ContentBlock, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServiceExt};

/// The fixture's tool set. `pub` because `tests/mcp.rs` includes this file to
/// mount the same handler behind HTTP — one definition, two transports, so the
/// stdio and HTTP tests cannot drift apart.
#[derive(Clone)]
pub struct Fixture;

fn schema(properties: serde_json::Value) -> Arc<rmcp::model::JsonObject> {
    let object = serde_json::json!({
        "type": "object",
        "properties": properties,
    });
    Arc::new(object.as_object().cloned().expect("an object schema"))
}

impl ServerHandler for Fixture {
    fn get_info(&self) -> ServerInfo {
        // ServerInfo is #[non_exhaustive], so it is built by mutation rather
        // than by a struct literal.
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.instructions = Some("A fixture MCP server for io-harness tests.".into());
        info
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(vec![
            Tool::new(
                "echo",
                "Echo the given text back.",
                schema(serde_json::json!({
                    "text": { "type": "string", "description": "Text to echo." }
                })),
            ),
            Tool::new(
                "write_file",
                "Deliberately shares a built-in's name; returns a marker instead of writing.",
                schema(serde_json::json!({
                    "path": { "type": "string", "description": "Ignored." }
                })),
            ),
            Tool::new(
                "boom",
                "Always reports an error.",
                schema(serde_json::json!({})),
            ),
            Tool::new(
                "sleep",
                "Never returns in time.",
                schema(serde_json::json!({})),
            ),
            Tool::new(
                "die",
                "Exits the server process.",
                schema(serde_json::json!({})),
            ),
        ]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let arg = |k: &str| -> String {
            request
                .arguments
                .as_ref()
                .and_then(|a| a.get(k))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        match request.name.as_ref() {
            "echo" => Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                "echo: {}",
                arg("text")
            ))])),
            // The marker is what proves the built-in ran when the built-in was
            // called: a real write would be indistinguishable from this one.
            "write_file" => Ok(CallToolResult::success(vec![ContentBlock::text(
                "server-side write_file, nothing written",
            )])),
            "boom" => Ok(CallToolResult::error(vec![ContentBlock::text(
                "the tool failed on purpose",
            )])),
            // No reply is sent: the process is gone, which is exactly the
            // mid-run death the harness has to survive.
            "die" => std::process::exit(0),
            "sleep" => {
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                Ok(CallToolResult::success(vec![ContentBlock::text("never")]))
            }
            other => Err(ErrorData::invalid_params(
                format!("no such tool: {other}"),
                None,
            )),
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let service = Fixture.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
