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
/// The `'a` is the borrow of `&self`, which is the useful part: the future may
/// hold the tool's own state — a connection pool, an HTTP client, a handle to
/// the application it lives in — rather than cloning it on every call. Building
/// one is `Box::pin(async move { … })` and nothing else.
///
/// ```
/// use std::collections::HashMap;
/// use std::sync::Mutex;
///
/// use io_harness::tools::{Tool, ToolFuture};
/// use io_harness::ToolSpec;
/// # use serde_json::{json, Value};
///
/// /// Whatever the embedding program already holds — a connection pool, an
/// /// HTTP client, a cache. Here, a map behind a lock.
/// struct Customers {
///     rows: Mutex<HashMap<String, String>>,
/// }
///
/// impl Tool for Customers {
///     # fn spec(&self) -> ToolSpec {
///     #     ToolSpec { name: "customer".into(), description: "Look a customer up by id.".into(),
///     #                parameters: json!({"type": "object"}) }
///     # }
///     fn invoke<'a>(&'a self, arguments: &'a Value) -> ToolFuture<'a> {
///         Box::pin(async move {
///             // `self` is borrowed for the life of the future, so the tool
///             // reads the program's own state instead of being handed a
///             // clone of it on every call.
///             let id = arguments.get("id").and_then(Value::as_str).unwrap_or_default();
///             let found = self.rows.lock().unwrap().get(id).cloned();
///             // `Err` is not a run failure: the text becomes an observation
///             // the model can act on, and only it can decide whether a miss
///             // means "try another id" or "give up".
///             found.ok_or_else(|| io_harness::Error::Config(format!("no customer {id}")))
///         })
///     }
/// }
/// ```
///
/// [`Provider`]: crate::Provider
pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>>;

/// Whether calling a tool can change anything (0.41.0).
///
/// The step loop dispatches the read-only calls in one completion concurrently
/// and everything else one at a time, in the order the model asked. This is what
/// a tool says about itself so the loop can tell the two apart:
///
/// ```
/// use io_harness::tools::{Tool, ToolEffect, ToolFuture};
/// use io_harness::ToolSpec;
/// # use serde_json::{json, Value};
///
/// struct Lookup;
///
/// impl Tool for Lookup {
///     # fn spec(&self) -> ToolSpec {
///     #     ToolSpec { name: "lookup".into(), description: "Read a row.".into(),
///     #                parameters: json!({"type": "object"}) }
///     # }
///     # fn invoke<'a>(&'a self, _a: &'a Value) -> ToolFuture<'a> {
///     #     Box::pin(async { Ok(String::new()) })
///     # }
///     fn effect(&self) -> ToolEffect {
///         ToolEffect::ReadOnly
///     }
/// }
///
/// assert_eq!(Lookup.effect(), ToolEffect::ReadOnly);
/// // The default is the conservative answer, so a tool written before 0.41.0
/// // keeps running one at a time.
/// # struct Older;
/// # impl Tool for Older {
/// #     fn spec(&self) -> ToolSpec {
/// #         ToolSpec { name: "older".into(), description: "…".into(),
/// #                    parameters: json!({"type": "object"}) }
/// #     }
/// #     fn invoke<'a>(&'a self, _a: &'a Value) -> ToolFuture<'a> {
/// #         Box::pin(async { Ok(String::new()) })
/// #     }
/// # }
/// assert_eq!(Older.effect(), ToolEffect::Mutating);
/// ```
///
/// The declaration is a promise the tool makes about itself. The harness cannot
/// check it — the tool is arbitrary code the embedding program compiled in — so a
/// tool that reports [`ToolEffect::ReadOnly`] and then writes breaks its own
/// invariants and nobody else's. That is why the default is
/// [`ToolEffect::Mutating`]: concurrency is something an author opts into, never
/// something a tool is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolEffect {
    /// The call observes and changes nothing, so it may run at the same time as
    /// other read-only calls in the same completion.
    ReadOnly,
    /// The call may change something. It runs on its own, in the order the model
    /// asked for it — which is what every tool did before 0.41.0.
    Mutating,
}

/// Whether a call that was in flight when the process died is safe to make again
/// (0.65.0).
///
/// A separate axis from [`ToolEffect`], and deliberately so. `ToolEffect` answers
/// whether two calls may run at the same time; this answers whether one call may
/// happen twice. A tool that must be serialised is not thereby unsafe to repeat —
/// an upsert is both — and reading one question's answer as the other's would
/// either serialise what need not be or repeat what must not be.
///
/// `#[non_exhaustive]` from birth: a later release may want to name an answer
/// between these two, and doing that must not break a caller who matched on them.
///
/// ```
/// use io_harness::tools::{Tool, ToolEffect, ToolFuture, ToolRecovery};
/// use io_harness::ToolSpec;
/// # use serde_json::{json, Value};
///
/// struct Forecast;
///
/// impl Tool for Forecast {
///     # fn spec(&self) -> ToolSpec {
///     #     ToolSpec { name: "forecast".into(), description: "Look the weather up.".into(),
///     #                parameters: json!({"type": "object"}) }
///     # }
///     # fn invoke<'a>(&'a self, _a: &'a Value) -> ToolFuture<'a> {
///     #     Box::pin(async { Ok("fine".to_string()) })
///     # }
///     fn effect(&self) -> ToolEffect {
///         ToolEffect::ReadOnly
///     }
/// }
///
/// // Nothing was declared about recovery, and nothing had to be: a tool that
/// // observes and changes nothing is safe to call again after a crash.
/// assert_eq!(Forecast.recovery(), ToolRecovery::Replayable);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ToolRecovery {
    /// Making this call a second time reaches the same end state as making it
    /// once, so a resumed run may simply replay it. Every built-in tool is this:
    /// a file the run wrote can be read back and written again.
    Replayable,
    /// Whether the call landed cannot be established from here, so a resumed run
    /// must not decide on its own. The run pauses at
    /// [`RunOutcome::AwaitingRecovery`](crate::RunOutcome::AwaitingRecovery) and
    /// an operator says what to do.
    ///
    /// This is where a charge, a deployment, a posted message and every MCP call
    /// sit — not because they are known to be unsafe, but because nothing here
    /// knows they are safe, and the two defaults are not equally recoverable: a
    /// pause costs a decision, a duplicate charge costs money.
    Indeterminate,
}

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

    /// Whether this tool changes anything (0.41.0).
    ///
    /// Defaulted to [`ToolEffect::Mutating`], so a tool written against any
    /// earlier release compiles unchanged and keeps being called one at a time.
    /// Override it with [`ToolEffect::ReadOnly`] to let the step loop run this
    /// tool at the same time as the other read-only calls in one completion —
    /// bounded by
    /// [`TaskContract::max_parallel_reads`](crate::TaskContract::max_parallel_reads).
    ///
    /// ```
    /// use io_harness::tools::{Tool, ToolEffect, ToolFuture};
    /// use io_harness::ToolSpec;
    /// # use serde_json::{json, Value};
    ///
    /// struct Weather;
    ///
    /// impl Tool for Weather {
    ///     # fn spec(&self) -> ToolSpec {
    ///     #     ToolSpec { name: "weather".into(), description: "Look the forecast up.".into(),
    ///     #                parameters: json!({"type": "object"}) }
    ///     # }
    ///     # fn invoke<'a>(&'a self, _a: &'a Value) -> ToolFuture<'a> {
    ///     #     Box::pin(async { Ok("fine".to_string()) })
    ///     # }
    ///     // Asking an upstream service for a forecast changes nothing here or
    ///     // there, so two of these may be in flight at once.
    ///     fn effect(&self) -> ToolEffect {
    ///         ToolEffect::ReadOnly
    ///     }
    /// }
    ///
    /// assert_eq!(Weather.effect(), ToolEffect::ReadOnly);
    /// ```
    ///
    /// Read once per call while the loop partitions a completion, and it must
    /// answer the same way every time: a tool that changed its mind between the
    /// partition and the call would be run in a way it had just said it must not
    /// be.
    fn effect(&self) -> ToolEffect {
        ToolEffect::Mutating
    }

    /// Whether an interrupted call is safe to make again (0.65.0).
    ///
    /// Defaulted from [`Tool::effect`], so a tool written against any earlier
    /// release compiles unchanged and gets the answer its own declaration already
    /// implies: [`ToolEffect::ReadOnly`] says the call "observes and changes
    /// nothing", which is a statement about the world and carries replay safety
    /// with it, so it is [`ToolRecovery::Replayable`]. [`ToolEffect::Mutating`] —
    /// what a tool that declares nothing gets — says only that the call must run
    /// on its own, which is no claim about repeating it, so it is
    /// [`ToolRecovery::Indeterminate`] and a resumed run pauses instead of
    /// calling it again.
    ///
    /// The derivation runs in one direction only. Nothing reads `Mutating` as
    /// replayable; the only way for a mutating tool to be replayed is to say so:
    ///
    /// ```
    /// use io_harness::tools::{Tool, ToolFuture, ToolRecovery};
    /// use io_harness::ToolSpec;
    /// # use serde_json::{json, Value};
    ///
    /// struct Upsert;
    ///
    /// impl Tool for Upsert {
    ///     # fn spec(&self) -> ToolSpec {
    ///     #     ToolSpec { name: "upsert".into(), description: "Write the row.".into(),
    ///     #                parameters: json!({"type": "object"}) }
    ///     # }
    ///     # fn invoke<'a>(&'a self, _a: &'a Value) -> ToolFuture<'a> {
    ///     #     Box::pin(async { Ok("written".to_string()) })
    ///     # }
    ///     // It writes, so it runs on its own — but writing the same row twice
    ///     // is the same row, so a resumed run may simply repeat it.
    ///     fn recovery(&self) -> ToolRecovery {
    ///         ToolRecovery::Replayable
    ///     }
    /// }
    ///
    /// assert_eq!(Upsert.recovery(), ToolRecovery::Replayable);
    /// ```
    ///
    /// Read before the call is made and again while a resumed run decides what to
    /// do about it, so — as with [`Tool::effect`] — it must answer the same way
    /// every time.
    fn recovery(&self) -> ToolRecovery {
        match self.effect() {
            ToolEffect::ReadOnly => ToolRecovery::Replayable,
            ToolEffect::Mutating => ToolRecovery::Indeterminate,
        }
    }

    /// The containment mode this tool needs (0.48.0).
    ///
    /// Defaulted to `None`, which means *whatever this run was granted* — so a
    /// tool written against any earlier release compiles unchanged and is treated
    /// exactly as it was. Return [`ExecMode::ReadOnly`](crate::ExecMode::ReadOnly) to say this tool never
    /// needs to write, or [`ExecMode::WorkspaceWrite`](crate::ExecMode::WorkspaceWrite) to say it does.
    ///
    /// **This is a refusal mechanism and not a confinement one, and the
    /// difference matters.** A registered tool spawns its own processes; the
    /// harness never sees that spawn and cannot wrap it. What a declaration buys
    /// is that a tool needing more than the run grants is *not called at all* —
    /// refused before it runs, with the reason handed to the model — instead of
    /// being called and failing on a permission error it has to explain. The
    /// crate makes no claim that a registered tool's own child is contained.
    ///
    /// For the built-in tools whose spawn the harness *does* own — `exec`,
    /// `shell`, `shell_start` and the git built-ins — the same declaration also
    /// narrows: a call runs under
    /// [`ExecMode::narrower`](crate::ExecMode::narrower) of what it needs and what
    /// the contract granted.
    ///
    /// ```
    /// use io_harness::tools::{Tool, ToolFuture};
    /// use io_harness::{ExecMode, ToolSpec};
    /// # use serde_json::{json, Value};
    ///
    /// struct Lookup;
    ///
    /// impl Tool for Lookup {
    ///     # fn spec(&self) -> ToolSpec {
    ///     #     ToolSpec { name: "lookup".into(), description: "Read a record.".into(),
    ///     #                parameters: json!({"type": "object"}) }
    ///     # }
    ///     # fn invoke<'a>(&'a self, _a: &'a Value) -> ToolFuture<'a> {
    ///     #     Box::pin(async { Ok("one row".to_string()) })
    ///     # }
    ///     // Reading a record writes nothing, so this tool is still callable in a
    ///     // run that granted no more than read-only.
    ///     fn exec_mode(&self) -> Option<ExecMode> {
    ///         Some(ExecMode::ReadOnly)
    ///     }
    /// }
    ///
    /// assert_eq!(Lookup.exec_mode(), Some(ExecMode::ReadOnly));
    /// ```
    ///
    /// Read once per call, before the call is dispatched, and it must answer the
    /// same way every time: a tool that changed its mind after the resolution
    /// would run under a grant it had just said it did not want.
    fn exec_mode(&self) -> Option<crate::sandbox::ExecMode> {
        None
    }
}

/// The set of [`Tool`]s registered for a run.
///
/// Collect them, hand the box to
/// [`TaskContract::with_tools`](crate::TaskContract::with_tools), and the model
/// is offered them beside `grep`, `find`, `read_file`, and `write_file`.
///
/// ```
/// use io_harness::tools::{Tool, ToolFuture, Toolbox};
/// use io_harness::{TaskContract, ToolSpec, Verification};
/// # use serde_json::{json, Value};
/// # struct Now;
/// # impl Tool for Now {
/// #     fn spec(&self) -> ToolSpec {
/// #         ToolSpec { name: "now".into(), description: "The current time, ISO 8601.".into(),
/// #                    parameters: json!({"type": "object"}) }
/// #     }
/// #     fn invoke<'a>(&'a self, _a: &'a Value) -> ToolFuture<'a> {
/// #         Box::pin(async { Ok("2026-07-28T09:00:00Z".to_string()) })
/// #     }
/// # }
/// # struct OpenTicket;
/// # impl Tool for OpenTicket {
/// #     fn spec(&self) -> ToolSpec {
/// #         ToolSpec { name: "open_ticket".into(), description: "File a ticket.".into(),
/// #                    parameters: json!({"type": "object"}) }
/// #     }
/// #     fn invoke<'a>(&'a self, _a: &'a Value) -> ToolFuture<'a> {
/// #         Box::pin(async { Ok("PROJ-1".to_string()) })
/// #     }
/// # }
/// let tools = Toolbox::new().with(Now).with(OpenTicket);
/// assert_eq!(tools.names(), vec!["now", "open_ticket"]);
///
/// let contract = TaskContract::workspace("triage the failing build", "/path/to/repo")
/// .with_verification(Verification::WorkspaceFileContains {
///     file: "TRIAGE.md".into(),
///     needle: "#".into(),
/// })
/// .with_tools(tools);
/// # let _ = contract;
/// ```
///
/// Registration makes a tool *available*; it does not authorize it. Each call
/// is an [`Act::Exec`](crate::Act::Exec) check on the tool's name, so an
/// operator can be handed this box and still refuse one tool in it:
/// `deny_exec("open_ticket")` leaves `now` working.
///
/// [`Toolbox::validate`] runs before the first completion, which is what turns
/// a naming mistake into "your config is wrong" rather than an agent that has
/// silently stopped being able to write files:
///
/// ```
/// use io_harness::tools::{Tool, ToolFuture, Toolbox};
/// use io_harness::ToolSpec;
/// # use serde_json::{json, Value};
///
/// struct Impostor;
///
/// impl Tool for Impostor {
///     fn spec(&self) -> ToolSpec {
///         // The name of a built-in. Dispatch matches the built-in first, so
///         // this tool would be registered, offered, and never reached.
///         ToolSpec { name: "write_file".into(), description: "…".into(),
///                    parameters: json!({"type": "object"}) }
///     }
///     # fn invoke<'a>(&'a self, _a: &'a Value) -> ToolFuture<'a> {
///     #     Box::pin(async { Ok(String::new()) })
///     # }
/// }
///
/// let err = Toolbox::new().with(Impostor).validate().unwrap_err();
/// assert!(err.to_string().contains("write_file"));
/// ```
///
/// The same rejection covers an empty name, a name using the `mcp__` prefix
/// reserved for server tools, and two tools sharing one name.
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
///
/// **Every** built-in, as of 0.17.0. It listed seven until then — the set as it
/// stood in 0.9.0 — while dispatch grew the five git tools, the image tool, the
/// nine document tools, and now `exec` and `edit_file`. The gap was not cosmetic:
/// `Toolbox::validate` accepted a registered tool called `git_status` or
/// `xlsx_read`, and dispatch then tested every built-in arm first, so that tool
/// was permanently unreachable and nothing said so. A tool that validates and
/// never runs is the exact silent shadowing this set exists to prevent, and it
/// was recorded as an open defect in `docs/CONTRACT.md` from 0.15.0.
///
/// The feature-gated built-ins this set names are reserved in every build,
/// including builds that do not contain them — see
/// [`VIEW_IMAGE_TOOL`](super::VIEW_IMAGE_TOOL) for why the alternative is worse.
/// That is true of **every** feature-gated built-in as of 0.61.0, which is what
/// ungating the six `browser_*` name constants bought: a name the harness owns is
/// owned in all builds, and a list cannot name a constant a default build does
/// not compile.
///
/// **Completed in 0.61.0, as a rule rather than a longer list.** 0.17.0 closed
/// this once by hand-patching the names it was missing, and every built-in added
/// afterwards reopened it by one — the worktree tool (0.36.0), `patch_file` and
/// `check` (0.51.0), LSP navigation (0.52.0), the browser (0.53.0), `forget`
/// (0.56.0), the mailbox (0.60.0), eighteen names in all. The list that closes it
/// is not what keeps it closed:
/// `every_name_the_harness_answers_is_reserved` in `tests/custom_tools.rs`
/// derives the built-in set from the crate's own `*_TOOL` constants and fails
/// when this slice does not hold it, in either direction. **Adding a built-in and
/// not adding it here is a red test, not a defect found by the next audit.**
///
/// Three names here are not dispatch arms:
/// [`SPAWN_TOOL`](crate::SPAWN_TOOL), [`SEND_MESSAGE_TOOL`](crate::SEND_MESSAGE_TOOL)
/// and [`READ_MESSAGES_TOOL`](crate::READ_MESSAGES_TOOL), which the tree loop
/// intercepts before `dispatch` is reached. Inside a tree they shadow just as
/// completely; a flat run is not offered them at all, so reserving them takes a
/// name a caller could have used rather than one that was quietly broken. That is
/// deliberate and it is `SPAWN_TOOL`'s own precedent: which run shape a program
/// happens to start must not decide which names are safe to register.
pub(crate) const RESERVED_TOOL_NAMES: &[&str] = &[
    super::WRITE_FILE_TOOL,
    super::EDIT_FILE_TOOL,
    super::PATCH_FILE_TOOL,
    super::CHECK_TOOL,
    super::EXEC_TOOL,
    super::SHELL_TOOL,
    super::SHELL_START_TOOL,
    super::SHELL_POLL_TOOL,
    super::SHELL_KILL_TOOL,
    super::GREP_TOOL,
    super::FIND_TOOL,
    super::LIST_DIR_TOOL,
    super::READ_FILE_TOOL,
    super::READ_SKILL_TOOL,
    super::REMEMBER_TOOL,
    super::FORGET_TOOL,
    super::TODO_WRITE_TOOL,
    super::ASK_QUESTION_TOOL,
    super::PROPOSE_PLAN_TOOL,
    super::GIT_LOG_TOOL,
    super::GIT_STATUS_TOOL,
    super::GIT_DIFF_TOOL,
    super::GIT_ADD_TOOL,
    super::GIT_COMMIT_TOOL,
    super::GIT_BRANCH_TOOL,
    super::GIT_WORKTREE_TOOL,
    super::LSP_DEFINITION_TOOL,
    super::LSP_REFERENCES_TOOL,
    super::LSP_SYMBOLS_TOOL,
    super::LSP_HOVER_TOOL,
    super::LSP_RENAME_TOOL,
    super::BROWSER_NAVIGATE_TOOL,
    super::BROWSER_READ_TOOL,
    super::BROWSER_SCREENSHOT_TOOL,
    super::BROWSER_CLICK_TOOL,
    super::BROWSER_TYPE_TOOL,
    super::BROWSER_SCROLL_TOOL,
    super::VIEW_IMAGE_TOOL,
    super::XLSX_READ_TOOL,
    super::XLSX_SHEETS_TOOL,
    super::XLSX_WRITE_TOOL,
    super::XLSX_SET_CELL_TOOL,
    super::DOCX_READ_TOOL,
    super::DOCX_WRITE_TOOL,
    super::PPTX_READ_TOOL,
    super::PDF_READ_TOOL,
    super::PDF_WRITE_TOOL,
    super::PDF_WATERMARK_TOOL,
    super::PDF_FILL_FORM_TOOL,
    super::BARCODE_DECODE_TOOL,
    crate::run::SPAWN_TOOL,
    crate::run::SEND_MESSAGE_TOOL,
    crate::run::READ_MESSAGES_TOOL,
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
