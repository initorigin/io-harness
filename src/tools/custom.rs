//! In-process extension: tools the embedding program supplies itself.
//!
//! 0.8.0 made the crate extensible *out of process* — point it at an MCP server
//! and its tools reach the model. That is the right boundary for a capability
//! that already lives elsewhere, and the wrong one for a capability that is
//! already linked into the same binary: a second process, a transport, and a
//! serialization hop to call a function that is one `await` away.
//!
//! 0.9.0 adds the in-process half. A caller implements [`Tool`], registers it
//! with [`TaskContract::with_tools`](crate::TaskContract::with_tools), and the
//! model is offered it beside `grep`, `find`, `read_file`, and `write_file`.
//!
//! # What registration is, and what it is not
//!
//! Registering a tool makes it *available*. It does not authorize it. Calling a
//! registered tool is an [`Act::Exec`](crate::Act::Exec) check on its name,
//! decided by the same deny-first policy stack that decides paths, binaries, and
//! hosts — so an operator can hand the agent a toolbox and still refuse one tool
//! in it, and the refusal lands in the trace attributed to the rule that made it.
//!
//! And the boundary stops there. A registered tool runs **in the harness's own
//! process, with the embedding program's privileges**. The policy governs whether
//! it is *called*; it does not govern what the tool does once running — no
//! sandbox, no path scoping, no egress control applies inside it. This is exactly
//! the bound 0.8.0 already states for a stdio MCP server, and for the same
//! reason: the harness decides what starts, not what a started thing then does.
//! A tool that shells out, writes outside the workspace, or dials a host has done
//! so with the caller's full authority.

use std::collections::BTreeSet;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;

use crate::error::{Error, Result};
use crate::provider::ToolSpec;

/// What [`Tool::invoke`] returns.
///
/// A boxed future, because [`Tool`] has to be a trait object: the toolbox holds
/// a heterogeneous set of caller types behind one pointer. [`Provider`] gets to
/// stay generic over `impl Future` precisely because a run has exactly one of
/// them; a run has many tools.
///
/// [`Provider`]: crate::Provider
pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

/// An action the embedding program lets the agent take.
///
/// Implement it for anything the model should be able to do that the built-in
/// filesystem tools cannot: query your database, call your internal API, render
/// your template, ask your UI. The result is text, as an MCP tool result already
/// is — the model reads it, so it has to be readable.
///
/// ```
/// use io_harness::tools::{Tool, ToolFuture};
/// use io_harness::ToolSpec;
/// use serde_json::json;
///
/// struct Uppercase;
///
/// impl Tool for Uppercase {
///     fn spec(&self) -> ToolSpec {
///         ToolSpec {
///             name: "uppercase".into(),
///             description: "Upper-case the `text` argument.".into(),
///             parameters: json!({
///                 "type": "object",
///                 "properties": { "text": { "type": "string" } },
///                 "required": ["text"]
///             }),
///         }
///     }
///
///     fn invoke<'a>(&'a self, arguments: &'a serde_json::Value) -> ToolFuture<'a> {
///         let text = arguments.get("text").and_then(|v| v.as_str()).unwrap_or("").to_uppercase();
///         Box::pin(async move { Ok(text) })
///     }
/// }
/// ```
///
/// Returning `Err` is not a run failure. The error text becomes an observation
/// the agent can adapt to, the same treatment `grep` gives a malformed regex —
/// only the model can decide whether a failed lookup means "try another id" or
/// "give up on this approach".
pub trait Tool: Send + Sync {
    /// How the tool is described to the model: its name, what it does, and the
    /// JSON Schema of its arguments.
    ///
    /// Called once per run, not once per step. The name it returns is the name
    /// the model calls and the name the policy decides on, so it must be stable
    /// for the life of the run.
    fn spec(&self) -> ToolSpec;

    /// Perform the action.
    ///
    /// `arguments` is the parsed object the model sent — the same
    /// [`ToolCall::arguments`](crate::ToolCall::arguments) value, so an
    /// implementer never parses JSON the harness has already parsed. It is not
    /// validated against the schema in [`Tool::spec`]: a model can and does send
    /// a missing or mistyped field, and treating that as a crash rather than an
    /// observation would end runs that could have recovered. Read defensively
    /// and return `Err` with a message the model can act on.
    fn invoke<'a>(&'a self, arguments: &'a Value) -> ToolFuture<'a>;
}

/// The set of [`Tool`]s registered for a run.
///
/// Cheap to clone (each tool is behind an `Arc`), so a whole 0.5.0 tree shares
/// one toolbox and every child is offered what its parent was — inheritance
/// grants the tool, and the child's own narrowed policy still decides each call.
#[derive(Clone, Default)]
pub struct Toolbox {
    tools: Vec<Arc<dyn Tool>>,
}

impl Toolbox {
    /// An empty toolbox. A contract carrying one behaves exactly as a 0.8.1
    /// contract does.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a tool, returning the toolbox for chaining.
    #[must_use]
    pub fn with(mut self, tool: impl Tool + 'static) -> Self {
        self.tools.push(Arc::new(tool));
        self
    }

    /// Register an already-shared tool, for a caller holding its own handle to
    /// one — a tool wrapping a connection pool it also uses elsewhere, say.
    #[must_use]
    pub fn with_arc(mut self, tool: Arc<dyn Tool>) -> Self {
        self.tools.push(tool);
        self
    }

    /// True if nothing is registered.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// How many tools are registered.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// The specs offered to the model, in registration order.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|t| t.spec()).collect()
    }

    /// The registered names, in registration order.
    pub fn names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.spec().name).collect()
    }

    /// True if `name` is a registered tool. Used by the dispatcher to tell a
    /// caller's tool apart from a built-in or an MCP tool.
    pub fn owns(&self, name: &str) -> bool {
        self.tools.iter().any(|t| t.spec().name == name)
    }

    /// The tool registered under `name`, if any.
    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.spec().name == name)
    }

    /// Reject a toolbox that cannot be dispatched unambiguously.
    ///
    /// Run once, before the first completion, so a naming mistake is a typed
    /// [`Error::Config`] the caller gets immediately rather than a shadowed
    /// built-in discovered at dispatch — the difference between "your config is
    /// wrong" and "your agent silently stopped being able to write files".
    pub fn validate(&self) -> Result<()> {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for tool in &self.tools {
            let name = tool.spec().name;
            if name.trim().is_empty() {
                return Err(Error::Config(
                    "a registered tool has an empty name; the model addresses a tool by name, so \
                     every Tool::spec() must return a non-empty one"
                        .into(),
                ));
            }
            if RESERVED_TOOL_NAMES.contains(&name.as_str()) {
                return Err(Error::Config(format!(
                    "registered tool {name:?} takes the name of a built-in tool. Built-ins are \
                     {}; rename the registered tool so a caller cannot silently replace the \
                     harness's own file, search, spawn, or skill tools",
                    RESERVED_TOOL_NAMES.join(", ")
                )));
            }
            if name.starts_with(crate::mcp::MCP_TOOL_PREFIX) {
                return Err(Error::Config(format!(
                    "registered tool {name:?} uses the {:?} prefix, which is reserved for MCP \
                     server tools. An in-process tool that took it could impersonate a tool the \
                     operator believes came from a configured server",
                    crate::mcp::MCP_TOOL_PREFIX
                )));
            }
            if !seen.insert(name.clone()) {
                return Err(Error::Config(format!(
                    "two registered tools are both named {name:?}; which one a model call reached \
                     would depend on registration order, so the set is rejected instead"
                )));
            }
        }
        Ok(())
    }
}

/// Names the harness owns. A registered tool taking one of these would shadow a
/// built-in the agent and the verification layer depend on.
pub(crate) const RESERVED_TOOL_NAMES: &[&str] = &[
    super::WRITE_FILE_TOOL,
    super::GREP_TOOL,
    super::FIND_TOOL,
    super::READ_FILE_TOOL,
    super::READ_SKILL_TOOL,
    crate::run::SPAWN_TOOL,
];

impl fmt::Debug for Toolbox {
    /// Lists the registered names. A `Tool` is caller code with no `Debug`
    /// bound, and [`TaskContract`](crate::TaskContract) derives `Debug`, so the
    /// useful thing to print is what is in the box rather than what each item is.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Toolbox").field(&self.names()).finish()
    }
}

impl<T: Tool + 'static> FromIterator<T> for Toolbox {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        iter.into_iter().fold(Toolbox::new(), |b, t| b.with(t))
    }
}
