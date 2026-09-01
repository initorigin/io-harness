//! One config file, layered, projected onto the typed API (0.19.0).
//!
//! # What this is
//!
//! An operator writes `io.toml` and gets a permission boundary, sandbox caps,
//! run budgets, per-ecosystem toolchain commands, MCP servers, a price table and
//! — since 0.27.0 — which provider and model to run with what standing behind it,
//! without compiling anything. An application layer reads the same file rather
//! than inventing two formats that would have to be reconciled later, and `[app]`
//! is the section that makes that literally true: the crate stores it and never
//! looks inside.
//!
//! # What this is not
//!
//! **The typed API is the authority.** Every key here lands in a type this crate
//! already had — [`Policy`], [`SandboxConfig`], [`Toolchain`], [`PriceTable`],
//! [`McpServer`], [`WebAccess`], [`TaskContract`] — and a file can express nothing the typed
//! API cannot. Configuration is the ergonomic front end to that API, never a
//! second path into the run loop.
//!
//! **Nothing is loaded implicitly.** No entry point in this crate discovers a
//! config on its own: the caller calls [`Config::discover`] and decides what to
//! do with the result. That is what makes the one guarantee this module can
//! honestly make about an agent true — a config file the agent writes *during* a
//! run cannot widen the boundary that run is already under, because the boundary
//! was read once, by the caller, before the run started.
//!
//! **A config file is not a security boundary against the agent.** The boundary
//! is the [`Policy`] the caller loaded. A file is where that policy was written
//! down.
//!
//! # The four scopes
//!
//! Later wins, key by key:
//!
//! 1. the crate's own defaults — whatever the typed API produces with no file;
//! 2. **user** — `$IO_CONFIG` outright, else `$IO_CONFIG_HOME/io.toml`, else
//!    `$XDG_CONFIG_HOME/io/io.toml` or `~/.config/io/io.toml` on unix,
//!    `%APPDATA%\io\io.toml` on Windows;
//! 3. **project** — `io.toml` in the workspace root. Meant to be committed;
//! 4. **local** — `io.local.toml` beside it. Meant to be gitignored.
//!
//! Still four. `$IO_CONFIG` names a scope's *file*; it does not bypass the merge.
//! [`Config::with_profile`] overlays `[profile.<name>]` on the merged result, which
//! is a section of a file already read rather than a fifth scope.
//!
//! Discovery reads the root it was given and does **not** walk upward out of it:
//! a run's configuration comes from the directory the caller named, never from
//! somewhere above it that the caller did not choose.
//!
//! ```
//! use io_harness::Config;
//!
//! # fn demo() -> io_harness::Result<()> {
//! let dir = tempfile::tempdir()?;
//! std::fs::write(dir.path().join("io.toml"), r#"
//!     [policy.defaults]
//!     write = "ask"
//!
//!     [run]
//!     max_steps = 20
//! "#)?;
//! // The individual overrides one key of the team's file and nothing else.
//! std::fs::write(dir.path().join("io.local.toml"), "[run]\nmax_steps = 4\n")?;
//!
//! let config = Config::discover(dir.path())?;
//! let contract = config.apply_to(io_harness::TaskContract::new(
//!     "tidy the module",
//!     dir.path().join("src/lib.rs"),
//!     io_harness::Verification::FileContains("fn".into()),
//! ));
//!
//! assert_eq!(contract.max_steps, 4, "the local scope wins");
//! assert_eq!(
//!     config.policy().expect("a [policy] section").defaults.write,
//!     io_harness::Effect::Ask,
//! );
//! # Ok(()) }
//! # demo().unwrap();
//! ```
//!
//! # Three rules that make it trustworthy
//!
//! **An unknown key is an error.** A typo in a permission rule that is silently
//! ignored leaves an operator believing in a boundary that is not there. Two
//! sections are exempt and they are named together: a `[[mcp]]` table, because
//! `McpServer` is `#[serde(flatten)]`-based, and `[app]`, which exists to be
//! unvalidated.
//!
//! **A substitution resolves or fails; it never empties.** `${env:NAME}`,
//! `${file:path}` and `${cmd:program args}` keep a credential out of a committed
//! file. A variable that is unset, a file that cannot be read, a command that is
//! missing or exits non-zero, and a value that resolves to nothing are all errors
//! naming the key — because an empty string in a boundary rule is a rule that
//! matches nothing.
//!
//! **A credential file readable by other accounts is named, not refused**
//! (0.74.0). On unix, `io.local.toml`, the user-scope `io.toml` and every
//! `${file:}` target are warned about through `tracing` when any group or other
//! permission bit is set — the file, its mode, and the `chmod 600` that fixes it.
//! It is a warning rather than `ssh`'s refusal because this is a library inside
//! somebody else's binary and `0644` is what a `umask 022` host produces by
//! default: refusing would turn an upgrade into a startup failure for the common
//! case. The committed `io.toml` is not checked at all — it is world-readable by
//! design, and a warning on every run is one an operator learns to ignore.
//!
//! **A file inside a workspace may narrow the boundary and may never widen it**
//! (0.27.0; extended from the project scope to `io.local.toml` in 0.74.0).
//! `io.toml` arrives with a `git clone`, and `io.local.toml` sits in the workspace
//! root a run's own agent can write to — one `write_file` of an unremarkable path
//! — so both are held to one rule. Five keys are refused there when the value
//! written is the widening one: `policy.defaults.exec = "allow"`,
//! `policy.defaults.net = "allow"`, `sandbox.allow_network = true`,
//! `sandbox.force_floor = false` and `sandbox.mode = "full-access"`. So are
//! `[[hook]]`, `[browser]`, `[[provider]]`, `[[mcp]]` and `[[lsp]]` whole, each
//! because it names a program to run or an endpoint a credential is sent to. So
//! are `${cmd:}` and `${file:}` anywhere in a file inside the workspace — the
//! first runs a program while the file is being parsed, which is the same door
//! reached without any of the tables above. And `run.skills`
//! and `run.templates` may not leave the workspace root there (0.74.0): both are
//! joined onto the discovery root, where an absolute value replaces it and a `..`
//! climbs out, and every `*.md` under whatever they name is composed into the
//! model's system prompt on every turn.
//!
//! The **user scope** is where all of them are written instead. It is the one file
//! nothing in a workspace can reach, which is the whole of why it is the one still
//! trusted. This does not claim that a cloned repository is safe: `[toolchain]`
//! still names an argv, and the boundary against the agent is still the [`Policy`]
//! the caller loaded.
//!
//! ```
//! use io_harness::Config;
//!
//! // Rejected, naming the key: `max_stepz` is not a key this crate has.
//! let err = Config::from_toml("[run]\nmax_stepz = 3\n").unwrap_err();
//! assert!(err.to_string().contains("max_stepz"), "{err}");
//!
//! // Rejected, naming the key: an unset variable is not an empty string.
//! let err = Config::from_toml(
//!     "[[mcp]]\nid = \"gh\"\ntransport = \"stdio\"\ncommand = \"${env:IO_HARNESS_NOT_SET}\"\n",
//! )
//! .unwrap_err();
//! assert!(err.to_string().contains("IO_HARNESS_NOT_SET"), "{err}");
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::context::ContextBudget;
use crate::error::{Error, Result};
use crate::mcp::McpServer;
use crate::policy::{Defaults, Effect, Layer, Policy};
use crate::pricing::{Price, PriceTable};
use crate::provider::redacted_endpoint;
use crate::resilience::{RetryPolicy, StallPolicy};
use crate::sandbox::SandboxConfig;
use crate::toolchain::Toolchain;
use crate::tools::git::Identity;
use crate::web::WebAccess;
use crate::TaskContract;

/// The project-scope file name: committed, and inherited by everyone on the team.
pub const PROJECT_FILE: &str = "io.toml";

/// The local-scope file name: gitignored, and an individual's own overrides.
pub const LOCAL_FILE: &str = "io.local.toml";

/// Environment variable that names the user-scope config directory outright,
/// ahead of every platform convention.
pub const CONFIG_HOME_VAR: &str = "IO_CONFIG_HOME";

/// Environment variable that names the user-scope config *file* outright (0.27.0),
/// ahead of [`CONFIG_HOME_VAR`] and every platform convention.
///
/// It names a scope; it does not bypass the merge. A project file still wins the
/// keys it names, which is why the scopes stay four and [`Scope`] gains no variant.
pub const CONFIG_VAR: &str = "IO_CONFIG";

/// The instruction files [`Config::discover`] looks for when `[instructions]` is
/// present and names none itself.
const DEFAULT_INSTRUCTIONS: &[&str] = &["AGENTS.md"];

/// Where a refusal tells an operator to write what a workspace file may not
/// (0.74.0).
///
/// Every scope refusal in this module ends with this, because a refusal that
/// names only what is forbidden leaves an operator with a setting and nowhere to
/// put it. It names the **user scope** and nothing else: 0.74.0 holds
/// `io.local.toml` to the same rule as `io.toml`, so the answer 0.27.0 gave —
/// "write it in the local file" — is no longer an answer for a workspace whose
/// root that file sits in.
///
/// The spelling tracks [`user_path`], which is the function that resolves it; the
/// two are a pair, and a lookup order changed in one is a lie told by the other.
#[cfg(windows)]
const USER_SCOPE: &str = "the user-scope file (`%IO_CONFIG%`, else \
                          `%IO_CONFIG_HOME%\\io.toml`, else `%APPDATA%\\io\\io.toml`)";
/// See the Windows arm above: the same sentence, this platform's lookup order.
#[cfg(not(windows))]
const USER_SCOPE: &str = "the user-scope file (`$IO_CONFIG`, else `$IO_CONFIG_HOME/io.toml`, \
                          else `$XDG_CONFIG_HOME/io/io.toml` or `~/.config/io/io.toml`)";

/// Which file a value came from.
///
/// Reported by [`Config::sources`] so an operator whose setting did not take
/// effect can see which file won rather than guess.
///
/// ```
/// use io_harness::config::Scope;
///
/// // Later wins: the individual's own file is the last word.
/// assert!(Scope::Local > Scope::Project);
/// assert!(Scope::Project > Scope::User);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// The operator's own file, outside any project.
    User,
    /// `io.toml` in the workspace root — the committed, shared one.
    Project,
    /// `io.local.toml` in the workspace root — the gitignored, personal one.
    Local,
}

/// Which file decided one key (0.30.0).
///
/// [`Scope`] answers "which files were read"; this answers "which of them won
/// *this* key", which is the half a reader needs when a value is not the one they
/// set. Reported by [`Config::origin`] and [`Config::origins`].
///
/// ```
/// use io_harness::config::{Config, Scope};
///
/// # fn demo() -> io_harness::Result<()> {
/// let dir = tempfile::tempdir()?;
/// std::fs::write(dir.path().join("io.toml"), "[run]\nmax_steps = 30\n")?;
/// std::fs::write(dir.path().join("io.local.toml"), "[run]\nmax_steps = 5\n")?;
///
/// let config = Config::discover(dir.path())?;
/// let origin = &config.origin("run.max_steps")[0];
/// assert_eq!(origin.scope, Scope::Local);
/// assert!(origin.path.ends_with("io.local.toml"));
///
/// // A key no file named has no origin at all — that is the crate's default
/// // speaking, and naming a file for it would be an invention.
/// assert!(config.origin("run.max_retries").is_empty());
/// # Ok(()) }
/// # demo().unwrap();
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    /// The scope whose file decided the key.
    pub scope: Scope,
    /// That file's path, as it was read.
    pub path: PathBuf,
}

// ---------------------------------------------------------------------------
// The file format
// ---------------------------------------------------------------------------

/// The whole file, every section optional.
///
/// `deny_unknown_fields` is on every section: an unknown key is the failure this
/// module exists to make loud.
///
/// `Debug` is hand-written below rather than derived (0.71.0): every string in
/// here has already been through [`substitute`], so a derived one would print
/// resolved `${env:}`/`${file:}`/`${cmd:}` values — a provider's `api_key`, an
/// `[[mcp]]` server's `Authorization` header, an `[[lsp]]` child's environment.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    policy: Option<PolicySection>,
    #[serde(default)]
    sandbox: Option<SandboxSection>,
    #[serde(default)]
    run: Option<RunSection>,
    // 0.56.0 — the three durable-memory caps. Its own table rather than three
    // more keys under `[run]`: these bound a workspace's store, which outlives
    // every run over it, where everything in `[run]` bounds one run.
    #[serde(default)]
    memory: Option<MemorySection>,
    #[serde(default)]
    toolchain: BTreeMap<String, ToolchainSection>,
    #[serde(default)]
    prices: Option<PricesSection>,
    // `McpServer` is `#[serde(flatten)]`-based and serde refuses `flatten`
    // beside `deny_unknown_fields`, so an unknown key *inside* one of these
    // tables is not rejected. Stated in the guide rather than papered over.
    //
    // 0.74.0 — in `REFUSED_SECTIONS`, so no file under the workspace root may
    // declare one. The 0.19.0 argument for allowing it there was that the spawn is
    // an `Act::Exec` check on the named binary, so the boundary is the caller's
    // policy rather than the scope of the file: that holds for the *binary* and
    // says nothing about the argv beside it, which the same table supplies.
    #[serde(default)]
    mcp: Vec<McpServer>,
    // 0.52.0. Language servers, for the navigation tools. In `REFUSED_SECTIONS`
    // since 0.74.0 for the reason `[[mcp]]` is, and it was allowed at project
    // scope until then on the same argument that stopped holding there.
    // `LspServer` carries `deny_unknown_fields` of its own — there is no
    // `#[serde(flatten)]` here to forbid it — so a misspelled key in a table that
    // names a program to spawn IS rejected, unlike `[[mcp]]`. Deliberately *not*
    // in `APPENDING`: a later scope replaces the set whole, because a half-
    // appended set of servers is not a set.
    #[serde(default)]
    lsp: Vec<crate::lsp::LspServer>,
    // 0.53.0. The browser a run may drive. In `REFUSED_SECTIONS` for the reason
    // `[[hook]]` is: it names a program to execute on this machine, and `io.toml`
    // arrives with a `git clone`.
    #[cfg(feature = "browser")]
    #[serde(default)]
    browser: Option<crate::browser::BrowserConfig>,
    // 0.21.0. `AgentDef` carries `deny_unknown_fields` of its own, so unlike
    // `[[mcp]]` above a misspelled key inside one of these tables IS rejected —
    // which matters more here than anywhere else in this file, because the keys
    // being misspelled are the ones that narrow a boundary.
    #[serde(default)]
    agent: Vec<crate::agent::AgentDef>,
    // 0.22.0. There is no `WebSection` because there is nothing for one to do:
    // `WebAccess` already carries `#[serde(default, deny_unknown_fields)]`, every
    // field of it is already optional-by-default, and the merge below has already
    // reconciled the scopes key by key before this deserializes. A section struct
    // here would be a second spelling of the same five keys, which is exactly the
    // drift this module's "the typed API is the authority" rule exists to prevent.
    #[serde(default)]
    web: Option<WebAccess>,
    // 0.27.0. The first entry is the provider a run uses; each later one is the
    // next link in the fallback chain. Deliberately *not* in `APPENDING`: a later
    // scope replaces the chain whole, because a half-appended fallback chain is
    // not a chain.
    //
    // 0.74.0 — in `REFUSED_SECTIONS`. `base_url` chooses where every completion
    // goes and `api_key` chooses which of this host's secrets goes with it, and
    // the request leaves before the run's first step, so a file under the
    // workspace root may not write either.
    #[serde(default)]
    provider: Vec<ProviderSpec>,
    // 0.27.0. The one section this crate stores and never validates, so an
    // application layer keeps its own settings in the same file. A `toml::value::Table`
    // rather than a typed section is the whole feature; `Config::app` is generic
    // so no `toml` type reaches the public API.
    #[serde(default)]
    app: Option<toml::value::Table>,
    // 0.27.0. A profile body is the file format again, so a typo inside a profile
    // that is never selected is still rejected at load. The recursion is bounded
    // by `refuse_nested_profiles` rather than by the type.
    #[serde(default)]
    profile: BTreeMap<String, File>,
    #[serde(default)]
    instructions: Option<InstructionsSection>,
    // 0.28.0. Lifecycle hooks over the observer channel. Refused in the project
    // scope whole rather than by action, for the reason `${cmd:}` is: `io.toml` is
    // the file a `git clone` delivers, and both a hook that runs an argv and a hook
    // that appends to a path the file chose are things a stranger should not be able
    // to make this process do. Deliberately *not* in `APPENDING`: a later scope
    // replaces the set whole, so the hooks that run are the hooks of one file rather
    // than a pile assembled from three.
    #[serde(default)]
    hook: Vec<crate::hooks::Hook>,
    // 0.35.0. Each entry names a directory holding a `plugin.toml`. In `APPENDING`
    // for the reason `[[agent]]` is: a project's bundles and an individual's own
    // are both wanted, and a local file that silently deleted the project's would
    // be a roster nobody could rely on. What a *plugin* may contribute depends on
    // the scope that declared it, which is why `Config` keeps `plugin_decls`
    // beside this — the merge that concatenates these arrays cannot say afterwards
    // which file contributed which element.
    #[serde(default)]
    plugin: Vec<crate::plugin::Declaration>,
}

/// Which provider a run uses, as a value a configuration can carry (0.27.0).
///
/// A **spec**, never a constructed provider: [`Provider::complete`](crate::Provider::complete)
/// returns `impl Future`, so the trait is not dyn-compatible and there is no
/// `Box<dyn Provider>` for an accessor to return. The application reads the spec and
/// builds from it, which is three lines and keeps every entry point generic.
///
/// **`[[provider]]` is a user-scope table** (0.74.0). It names the endpoint every
/// completion of the run is sent to and the credential sent with it, so a file
/// inside a workspace — `io.toml`, which a `git clone` delivers, or
/// `io.local.toml`, which a run's own agent can write — may not declare it.
///
/// ```
/// use io_harness::{Config, ProviderSpec};
///
/// # fn demo() -> io_harness::Result<()> {
/// let home = tempfile::tempdir()?;
/// std::env::set_var("IO_CONFIG_HOME", home.path());
/// std::fs::write(home.path().join("io.toml"), r#"
///     [[provider]]
///     kind = "openrouter"
///     model = "anthropic/claude-sonnet-4"
///
///     [[provider]]
///     kind = "anthropic"
///     model = "claude-sonnet-4"
/// "#)?;
///
/// let workspace = tempfile::tempdir()?;
/// let config = Config::discover(workspace.path())?;
///
/// // The first entry is the provider; the rest are the chain behind it, in order.
/// let ProviderSpec::OpenRouter { model, api_key } = config.provider_spec().unwrap() else {
///     panic!("the file named openrouter");
/// };
/// assert_eq!(model, "anthropic/claude-sonnet-4");
/// assert_eq!(*api_key, None, "no key written means the provider's own environment variable");
/// assert_eq!(config.fallback_specs().len(), 1);
///
/// // The same two tables in the workspace are refused, naming the key.
/// let committed = "[[provider]]\nkind = \"openai\"\nmodel = \"x\"\n";
/// std::fs::write(workspace.path().join("io.toml"), committed)?;
/// let err = Config::discover(workspace.path()).unwrap_err();
/// assert!(err.to_string().contains("key `provider`"), "{err}");
/// # Ok(()) }
/// # demo().unwrap();
/// ```
///
/// It is `#[non_exhaustive]` from the first release it exists, because a later one
/// adds a variant: a consumer matching it needs a `_ =>` arm, and paying that once
/// now is what keeps the addition from being a break.
///
/// `Debug` is hand-written and prints `api_key` as `<redacted>` when it is set and
/// `None` when it is not (0.71.0) — the distinction an operator needs, without the
/// credential. `Serialize` is untouched: an application layer persists a spec the
/// operator typed, and a redacted round trip would write the placeholder into their
/// settings file.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
#[non_exhaustive]
pub enum ProviderSpec {
    /// [`OpenRouter`](crate::OpenRouter). `api_key` unset means `OPENROUTER_API_KEY`.
    #[serde(rename = "openrouter")]
    OpenRouter {
        /// The model id, as OpenRouter spells it.
        model: String,
        /// The key, or `None` to read the provider's own environment variable.
        #[serde(default)]
        api_key: Option<String>,
    },
    /// [`Anthropic`](crate::Anthropic). `api_key` unset means `ANTHROPIC_API_KEY`.
    #[serde(rename = "anthropic")]
    Anthropic {
        /// The model id, as Anthropic spells it.
        model: String,
        /// The key, or `None` to read the provider's own environment variable.
        #[serde(default)]
        api_key: Option<String>,
    },
    /// [`OpenAi`](crate::OpenAi). `api_key` unset means `OPENAI_API_KEY`.
    #[serde(rename = "openai")]
    OpenAi {
        /// The model id, as OpenAI spells it.
        model: String,
        /// The key, or `None` to read the provider's own environment variable.
        #[serde(default)]
        api_key: Option<String>,
    },
    /// [`Compatible`](crate::Compatible) — any OpenAI-shaped endpoint (0.29.0).
    ///
    /// The variant 0.27.0's `#[non_exhaustive]` was added for. Exactly one of
    /// `preset` and `base_url` is required: a preset supplies a vendor's
    /// documented base URL and auth style, and a `base_url` is anything else that
    /// speaks the format — a proxy, a gateway, a runtime on a port of your own.
    ///
    /// Unlike the three above there is no environment variable to fall back to,
    /// because there is no single vendor to name one for. `${env:}` in the file
    /// is the stated path and has worked since 0.19.0.
    ///
    /// ```
    /// use io_harness::{Config, ProviderSpec};
    ///
    /// # fn demo() -> io_harness::Result<()> {
    /// let home = tempfile::tempdir()?;
    /// std::env::set_var("IO_CONFIG_HOME", home.path());
    /// std::fs::write(home.path().join("io.toml"), r#"
    ///     [[provider]]
    ///     kind = "compatible"
    ///     preset = "groq"
    ///     model = "llama-3.3-70b-versatile"
    ///
    ///     [[provider]]
    ///     kind = "compatible"
    ///     base_url = "http://localhost:11434/v1"
    ///     model = "llama3.2"
    ///     auth = "none"
    /// "#)?;
    ///
    /// let workspace = tempfile::tempdir()?;
    /// let config = Config::discover(workspace.path())?;
    ///
    /// let ProviderSpec::Compatible { preset, model, .. } = config.provider_spec().unwrap() else {
    ///     panic!("the file named a compatible provider");
    /// };
    /// assert_eq!(preset.as_deref(), Some("groq"));
    /// assert_eq!(model, "llama-3.3-70b-versatile");
    ///
    /// // The fallback is a model on this machine, which costs nothing to run.
    /// assert_eq!(config.fallback_specs().len(), 1);
    /// # Ok(()) }
    /// # demo().unwrap();
    /// ```
    #[serde(rename = "compatible")]
    Compatible {
        /// The model id, as this endpoint spells it. Never defaulted: a guessed
        /// slug is a wrong model that ships quietly.
        model: String,
        /// A vendor preset — `groq`, `ollama`, `zhipu` and the rest. Exactly one
        /// of this and `base_url`.
        #[serde(default)]
        preset: Option<String>,
        /// The whole base URL this endpoint documents, with `/chat/completions`
        /// appended to it. Exactly one of this and `preset`.
        #[serde(default)]
        base_url: Option<String>,
        /// The key, or `None` for an endpoint that needs none. There is no
        /// environment variable fallback here; write `"${env:GROQ_API_KEY}"`.
        #[serde(default)]
        api_key: Option<String>,
        /// How to authenticate, or `None` to take the preset's own style —
        /// [`Auth::Bearer`](crate::Auth::Bearer) for a bare `base_url`.
        #[serde(default)]
        auth: Option<crate::provider::Auth>,
        /// The label recorded in the trace, or `None` for the preset's name and
        /// `"compatible"` for a bare `base_url`.
        #[serde(default)]
        name: Option<String>,
        /// Opt in to filling missing prices from the reference catalogue.
        ///
        /// **This turns on an outbound request to a host this file did not
        /// name.** When it is set the reference host appears in
        /// [`Provider::endpoints`](crate::Provider::endpoints) and the run
        /// authorises it against the policy's [`Act::Net`](crate::Act) rules
        /// before the first step — denied means the run refuses rather than the
        /// lookup being quietly skipped.
        #[serde(default)]
        reference_prices: bool,
    },
}

impl ProviderSpec {
    /// Refuse a `compatible` entry that names neither base or both (0.29.0).
    ///
    /// Exactly-one is enforced in code rather than in the type for the reason
    /// `[[hook]]`'s `append`/`run` pair is: a tagged enum for the two shapes
    /// would need `#[serde(flatten)]` for the keys they share, and serde refuses
    /// `flatten` beside `deny_unknown_fields` — which would silently accept a
    /// misspelled key inside the table, the standing `[[mcp]]` defect
    /// (`src/config.rs:205-207`).
    fn validate(&self, index: usize) -> Result<()> {
        let Self::Compatible {
            preset, base_url, ..
        } = self
        else {
            return Ok(());
        };
        let named = match (preset.as_deref(), base_url.as_deref()) {
            (Some(_), Some(_)) => {
                return Err(Error::Config(format!(
                    "[[provider]] #{index} names both `preset` and `base_url`; \
                     a compatible provider takes exactly one — a preset supplies \
                     the base URL, so naming both means one of them is ignored"
                )))
            }
            (None, None) => {
                return Err(Error::Config(format!(
                    "[[provider]] #{index} of kind \"compatible\" names neither \
                     `preset` nor `base_url`; one is required. The presets are: {}",
                    crate::provider::compatible::preset_list()
                )))
            }
            (Some(p), None) => Some(p),
            (None, Some(_)) => None,
        };
        if let Some(p) = named {
            if !crate::provider::compatible::preset_names().contains(&p) {
                return Err(Error::Config(format!(
                    "[[provider]] #{index} names unknown preset {p:?}; \
                     the presets are: {}",
                    crate::provider::compatible::preset_list()
                )));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// What a formatter is allowed to see (0.71.0)
// ---------------------------------------------------------------------------
//
// `parse` resolves every `${env:}`, `${file:}` and `${cmd:}` *before* the table is
// stored, so from that point on a configuration is a bag of plaintext secrets: a
// provider's `api_key`, an `[[mcp]]` server's `Authorization` header, the
// environment handed to an `[[lsp]]` child, whatever an application layer keeps
// under `[app]`. Deriving `Debug` on `Config`, `File` or `ProviderSpec` puts all of
// it in the first log line anyone writes while debugging a misconfiguration — and
// puts it there twice, because `Config` keeps both the typed sections and the raw
// table they came from.
//
// The three impls below print the *shape* instead. `f.debug_struct("Config")
// .finish()` would also hide the secrets, and is rejected: an operator formats a
// config precisely when it is behaving unexpectedly, and an empty rendering answers
// nothing. Key names, nesting, value kinds, section presence and the ids of the
// things a file declared are all safe — none of them is a value a substitution
// filled in — and together they are what the question "why did this config do that"
// is actually asking.

/// A bare word standing in for a value a formatter must not print.
///
/// A newtype rather than a `&str` because `&str`'s own `Debug` quotes it, and
/// `api_key: "<redacted>"` reads like a key whose value happens to be that text.
pub(crate) struct Marker(pub(crate) &'static str);

impl std::fmt::Debug for Marker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

/// The one spelling of a withheld value, for every module that withholds one.
///
/// It is a constant rather than a literal repeated per module for the reason this
/// whole release exists: the seven hand-written `Debug` impls added in 0.71.0 span
/// five files, and a second spelling of this word would make an operator's output
/// disagree with itself depending on which type printed it.
pub(crate) const REDACTED: Marker = Marker("<redacted>");

/// `<redacted>` for a credential that is set, `None` for one that is not.
///
/// The distinction is deliberate and is the whole operator-facing value of the
/// field: "this file supplied a key and it was still wrong" and "this file supplied
/// no key, so the provider read its own environment variable" are different
/// misconfigurations with different fixes. Nothing else is said — not the length,
/// not a prefix, not a suffix — because each of those narrows *which* key it is.
fn secret<T>(value: &Option<T>) -> Marker {
    if value.is_some() {
        REDACTED
    } else {
        Marker("None")
    }
}

/// `<set>` for a section a file wrote, `None` for one no file mentioned.
///
/// A section's contents are omitted rather than redacted key by key: every one of
/// them has been through [`substitute`], and a rule that lists the fields safe to
/// print is a rule that goes stale the next time a section gains a field. The key
/// names and value kinds are still visible through [`TableShape`] on `Config`'s raw
/// table, which is where the detail belongs.
fn section<T>(value: &Option<T>) -> Marker {
    Marker(if value.is_some() { "<set>" } else { "None" })
}

/// The shape of one parsed TOML value: nesting and kinds, never a leaf's value.
struct Shape<'a>(&'a toml::Value);

impl std::fmt::Debug for Shape<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            toml::Value::Table(table) => std::fmt::Debug::fmt(&TableShape(table), f),
            toml::Value::Array(items) => f.debug_list().entries(items.iter().map(Shape)).finish(),
            // Every leaf collapses to its kind. A string is the only one a
            // substitution can fill, but an integer or a boolean printed beside it
            // would invite the next field to be printed too.
            toml::Value::String(_) => f.write_str("string"),
            toml::Value::Integer(_) => f.write_str("integer"),
            toml::Value::Float(_) => f.write_str("float"),
            toml::Value::Boolean(_) => f.write_str("boolean"),
            toml::Value::Datetime(_) => f.write_str("datetime"),
        }
    }
}

/// The same, for a table — the form `Config`'s raw merge and `[app]` are held in.
struct TableShape<'a>(&'a toml::value::Table);

impl std::fmt::Debug for TableShape<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map()
            .entries(self.0.iter().map(|(key, value)| (key, Shape(value))))
            .finish()
    }
}

impl std::fmt::Debug for ProviderSpec {
    /// Every field but `api_key` verbatim; `api_key` as `<redacted>` when it is
    /// set and `None` when it is not.
    ///
    /// The model id is the field an operator is usually looking for, so it is
    /// printed exactly as written. `base_url` goes through the same endpoint
    /// redaction the providers' own impls use: a gateway or Azure-style endpoint
    /// routinely carries the credential inside the URL, and printing it verbatim
    /// would reopen the leak through the neighbouring field.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OpenRouter { model, api_key } => f
                .debug_struct("OpenRouter")
                .field("model", model)
                .field("api_key", &secret(api_key))
                .finish(),
            Self::Anthropic { model, api_key } => f
                .debug_struct("Anthropic")
                .field("model", model)
                .field("api_key", &secret(api_key))
                .finish(),
            Self::OpenAi { model, api_key } => f
                .debug_struct("OpenAi")
                .field("model", model)
                .field("api_key", &secret(api_key))
                .finish(),
            Self::Compatible {
                model,
                preset,
                base_url,
                api_key,
                auth,
                name,
                reference_prices,
            } => f
                .debug_struct("Compatible")
                .field("model", model)
                .field("preset", preset)
                .field("base_url", &base_url.as_deref().map(redacted_endpoint))
                .field("api_key", &secret(api_key))
                .field("auth", auth)
                .field("name", name)
                .field("reference_prices", reference_prices)
                .finish(),
        }
    }
}

impl std::fmt::Debug for File {
    /// Which sections a file set, and the ids of what it declared.
    ///
    /// `[[provider]]` is the one array printed in full, through
    /// [`ProviderSpec`]'s own impl, because the model a run is about to use is
    /// the single most-asked question of a configuration and that impl already
    /// withholds the key. Everything else is named and counted rather than
    /// rendered: `[[mcp]]` carries `Authorization` headers, `[[lsp]]` carries a
    /// child's environment, `[[hook]]` carries an argv, and each of those is a
    /// string a `${env:}` may have filled.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("File");
        s.field("policy", &section(&self.policy));
        s.field("sandbox", &section(&self.sandbox));
        s.field("run", &section(&self.run));
        s.field("memory", &section(&self.memory));
        s.field("prices", &section(&self.prices));
        s.field("web", &section(&self.web));
        s.field("instructions", &section(&self.instructions));
        #[cfg(feature = "browser")]
        s.field("browser", &section(&self.browser));
        s.field("toolchain", &self.toolchain.keys());
        s.field("provider", &self.provider);
        s.field("mcp", &self.mcp.iter().map(|m| &m.id).collect::<Vec<_>>());
        s.field("lsp", &self.lsp.iter().map(|l| &l.id).collect::<Vec<_>>());
        s.field(
            "agent",
            &self.agent.iter().map(|a| &a.name).collect::<Vec<_>>(),
        );
        s.field("hook", &self.hook.len());
        s.field("plugin", &self.plugin.len());
        s.field("app", &self.app.as_ref().map(TableShape));
        s.field("profile", &self.profile.keys());
        s.finish()
    }
}

impl std::fmt::Debug for Config {
    /// Which files were read, what they set, and the shape of the merged table.
    ///
    /// `origins` is printed whole: it is a map from a dotted key *name* to the
    /// files that decided it, and holds no value from any of them — which makes
    /// it the most useful thing here and one of the few carrying no risk.
    /// `instructions` and `plugin_decls` are counted, being file contents and
    /// declared paths respectively.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("Config");
        s.field("dir", &self.dir);
        s.field("sources", &self.sources);
        s.field("file", &self.file);
        s.field("raw", &TableShape(&self.raw));
        s.field("origins", &self.origins);
        s.field("instructions", &self.instructions.len());
        s.field("plugin_decls", &self.plugin_decls.len());
        s.finish()
    }
}

/// Which files carry a repository's own instructions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InstructionsSection {
    /// Relative to the discovery root. Absent means [`DEFAULT_INSTRUCTIONS`]. A
    /// named file that does not exist is skipped: this is discovery, not
    /// substitution.
    files: Option<Vec<PathBuf>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicySection {
    #[serde(default)]
    defaults: Option<DefaultsSection>,
    #[serde(default)]
    layers: Vec<Layer>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DefaultsSection {
    read: Option<Effect>,
    write: Option<Effect>,
    exec: Option<Effect>,
    net: Option<Effect>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SandboxSection {
    #[serde(default)]
    limits: Option<LimitsSection>,
    allow_network: Option<bool>,
    force_floor: Option<bool>,
    /// Where a command may write (0.46.0), by [`ExecMode`]'s own kebab-case
    /// labels: `read-only`, `workspace-write`, `full-access`.
    mode: Option<crate::sandbox::ExecMode>,
}

/// Every cap optional and merged one key at a time — unlike [`SandboxLimits`](crate::sandbox::SandboxLimits)
/// itself, which a config naming `limits` at all would otherwise have to spell
/// out whole. `0` means *no cap*, since TOML has no null to write.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LimitsSection {
    max_cpu_secs: Option<u64>,
    max_wall_secs: Option<u64>,
    max_memory_bytes: Option<u64>,
    max_processes: Option<u64>,
    max_open_files: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunSection {
    max_steps: Option<u32>,
    max_duration_secs: Option<u64>,
    max_tokens: Option<u64>,
    max_retries: Option<u32>,
    exec_timeout_secs: Option<u64>,
    skills: Option<PathBuf>,
    templates: Option<PathBuf>,
    retry: Option<RetryPolicy>,
    stall: Option<StallPolicy>,
    context: Option<ContextBudget>,
    // 0.55.0 — the ceiling a read is refused against, in characters. Beside the
    // other budgets because it is one: what a run may spend, what one request may
    // carry, and what one read may be.
    max_read_chars: Option<u64>,
    // 0.60.0 — the ceiling on a blocking mailbox read, in seconds. Beside
    // `max_read_chars` because it is the same kind of key: a number an operator
    // chose, which a project scope may lower and may not raise.
    max_wait_secs: Option<u64>,
    commit_identity: Option<Identity>,
}

/// What a workspace's durable memory may hold (0.56.0).
///
/// Every cap optional and applied one key at a time, like [`LimitsSection`]: a
/// file that wants a bigger store should not have to restate the other two
/// numbers, and a section-wide default would silently reset the ones it did not
/// mention.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemorySection {
    max_entries: Option<u64>,
    max_chars: Option<u64>,
    max_entry_chars: Option<u64>,
}

/// An operator's override for one ecosystem, applied onto what
/// [`crate::toolchain::detect`] found. Every command optional: a file that
/// changes the test command should not have to restate the build one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolchainSection {
    manager: Option<String>,
    install: Option<Vec<String>>,
    build: Option<Vec<String>>,
    test: Option<Vec<String>>,
    lint: Option<Vec<String>>,
    format: Option<Vec<String>>,
    run: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PricesSection {
    /// Required, for the same reason [`PriceTable::new`] requires it: a price
    /// list with no date is a claim with no expiry.
    as_of: String,
    #[serde(default)]
    models: BTreeMap<String, Price>,
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// A loaded configuration: the merged value, and which files it came from.
///
/// ```
/// use io_harness::Config;
///
/// // A config carrying no sections is the crate's defaults, and says so.
/// let empty = Config::from_toml("").unwrap();
/// assert!(empty.is_empty());
/// assert!(empty.policy().is_none(), "no [policy] section is not an empty policy");
/// ```
///
/// `Debug` is hand-written (0.71.0). A loaded config holds the merged table with
/// every `${env:}`, `${file:}` and `${cmd:}` already resolved, so a derived one
/// printed the operator's credentials — twice, once through the typed sections and
/// once through the raw table. What it prints instead is the *structure*: which
/// scopes were read, which sections a file set, and the key names, nesting and
/// value kinds of the merged table. No leaf value, ever.
#[derive(Clone, Default)]
pub struct Config {
    file: File,
    sources: Vec<(Scope, PathBuf)>,
    /// The merged table the typed `file` was deserialized from, kept so
    /// [`Config::with_profile`] can overlay through the same [`merge`] the scopes
    /// use rather than inventing a second set of merge semantics.
    raw: toml::value::Table,
    /// What `[instructions]` found, already worded and attributed to the file it
    /// came from. Read once, in [`Config::discover`], which is the caller's own
    /// call — the run loop never reads a file. Carried in the system block since
    /// 0.45.0, not in `TaskContract::constraints`.
    instructions: Vec<String>,
    /// Which file decided each key, by dotted path (0.30.0).
    ///
    /// Built as the scopes merge rather than derived afterwards, because the
    /// merged table has no memory of who wrote what. A `Vec` rather than one
    /// `Origin` for the two keys in [`APPENDING`], where more than one file
    /// genuinely contributed and naming a single winner would be a lie.
    origins: BTreeMap<String, Vec<Origin>>,
    /// The root a `[[hook]]`'s relative `append` path resolves against (0.28.0).
    ///
    /// The *discovery root*, not the declaring file's directory, and the two are the
    /// same thing for a local-scope file. It matters for a user-scope one, where an
    /// operator writing `append = "audit.jsonl"` means the project they are pointing
    /// the harness at rather than their own home directory.
    dir: PathBuf,
    /// Every `[[plugin]]` entry as it was written, with the scope of the file
    /// that declared it (0.35.0).
    ///
    /// Recorded per scope as the scopes are read, like `origins` and for the same
    /// reason: the merge concatenates the arrays and nothing afterwards can say
    /// which file wrote which element. The trust rule needs exactly that answer,
    /// so it is kept rather than derived. The whole `Declaration` rather than its
    /// path, because since 0.70.0 the entry also carries whether the bundle is
    /// switched on.
    plugin_decls: Vec<(Scope, crate::plugin::Declaration)>,
}

impl Config {
    /// Read every scope that exists for `root` and merge them.
    ///
    /// A scope whose file is absent is skipped; a workspace with no files at all
    /// is not an error, it is the crate's defaults.
    ///
    /// **Both files under `root` are widening-checked, not just the committed one**
    /// (0.74.0). `io.local.toml` is read from the workspace root, and the workspace
    /// root is where a run's agent writes: a `write_file` of that one path used to
    /// be a way to declare a `[[hook]]` argv, an `[[mcp]]` command or a
    /// `[[provider]]` endpoint that the next call to this function would act on,
    /// with no `Policy` and no sandbox in front of it. So the rule that has bounded
    /// `io.toml` since 0.27.0 now bounds `io.local.toml` too, and the user scope —
    /// which is not under `root`, and which this function reaches by
    /// [`user_path`] rather than by walking anywhere — is the one scope that may
    /// still widen. What is refused, and where to write it instead, is in the
    /// module documentation.
    ///
    /// ```
    /// use io_harness::config::{Config, Scope};
    ///
    /// # fn demo() -> io_harness::Result<()> {
    /// let dir = tempfile::tempdir()?;
    /// std::fs::write(dir.path().join("io.toml"), "[run]\nmax_tokens = 100000\n")?;
    ///
    /// let config = Config::discover(dir.path())?;
    /// assert_eq!(config.sources()[0].0, Scope::Project);
    /// # Ok(()) }
    /// # demo().unwrap();
    /// ```
    pub fn discover(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref();
        let mut candidates: Vec<(Scope, PathBuf)> = Vec::new();
        if let Some(user) = user_path() {
            candidates.push((Scope::User, user));
        }
        candidates.push((Scope::Project, root.join(PROJECT_FILE)));
        candidates.push((Scope::Local, root.join(LOCAL_FILE)));

        let mut merged = toml::value::Table::new();
        let mut sources = Vec::new();
        let mut origins = BTreeMap::new();
        let mut plugin_decls = Vec::new();
        for (scope, path) in candidates {
            if !path.is_file() {
                continue;
            }
            let table = read_scope(scope, &path)?;
            // Captured here, from this scope's own table, for the reason
            // `record_origins` runs here: after the merge the arrays are one array.
            for decl in declared_plugins(&table, &path)? {
                plugin_decls.push((scope, decl));
            }
            // Recorded from the scope's own table, before the merge folds it into
            // everything read so far — afterwards there is nothing left to say
            // which file a key came from.
            record_origins(
                &table,
                &mut Vec::new(),
                &Origin {
                    scope,
                    path: path.clone(),
                },
                &mut origins,
            );
            merge(&mut merged, table, &mut Vec::new(), scope == Scope::Project);
            sources.push((scope, path));
        }

        let file: File = deserialize(toml::Value::Table(merged.clone()), Path::new("<merged>"))?;
        let instructions = read_instructions(&file, root)?;
        Ok(Self {
            file,
            sources,
            raw: merged,
            instructions,
            origins,
            dir: root.to_path_buf(),
            plugin_decls,
        })
    }

    /// Parse one config from text, as the project scope.
    ///
    /// For a caller that holds its configuration somewhere this crate does not
    /// know about, and for tests. `${file:...}` resolves against the current
    /// directory, since there is no file to resolve against.
    ///
    /// **It is the project scope**, so every rule that bounds that scope applies:
    /// `${cmd:...}` and `${file:...}` are refused here, a key whose value would
    /// widen a boundary is refused here, and so is any section that names a program
    /// to run or an endpoint a credential is sent to — `[[hook]]`, `[browser]`,
    /// `[[provider]]`, `[[mcp]]` and `[[lsp]]`. A caller holding one of those in
    /// text of its own is holding the user scope's content and wants
    /// [`Config::discover`] against a `$IO_CONFIG_HOME` it controls.
    /// `[instructions]` finds nothing, because there is no root to discover
    /// against.
    ///
    /// ```
    /// use io_harness::Config;
    ///
    /// let config = Config::from_toml("[sandbox]\nforce_floor = true\n").unwrap();
    /// assert!(config.sandbox().unwrap().force_floor);
    ///
    /// // The same key set the other way is refused: this is the project scope.
    /// let err = Config::from_toml("[sandbox]\nforce_floor = false\n").unwrap_err();
    /// assert!(err.to_string().contains("widens"), "{err}");
    ///
    /// // 0.46.0 — a repository may narrow what its own commands may write to,
    /// // and may not hand whoever cloned it the host's privileges.
    /// use io_harness::ExecMode;
    /// let narrowed = Config::from_toml("[sandbox]\nmode = \"read-only\"\n").unwrap();
    /// assert_eq!(narrowed.sandbox().unwrap().mode, ExecMode::ReadOnly);
    ///
    /// let err = Config::from_toml("[sandbox]\nmode = \"full-access\"\n").unwrap_err();
    /// assert!(err.to_string().contains("widens"), "{err}");
    /// ```
    pub fn from_toml(text: &str) -> Result<Self> {
        let path = Path::new(PROJECT_FILE);
        let table = parse(Scope::Project, text, path)?;
        refuse_widening(Scope::Project, &table, path)?;
        let file: File = deserialize(toml::Value::Table(table.clone()), path)?;
        refuse_nested_profiles(&file, path)?;
        crate::hooks::Hooks::check(&file.hook, path)?;
        check_providers(&file.provider)?;
        // The same check `read_scope` makes, because this function repeats that
        // validator row rather than sharing it (0.70.0).
        crate::mcp::check_enabled_spelling(&table, path)?;
        // Parsed text is the project scope, so every plugin it declares is
        // declared from the scope a `git clone` delivers.
        let plugin_decls = file
            .plugin
            .iter()
            .map(|d| (Scope::Project, d.clone()))
            .collect();
        Ok(Self {
            file,
            sources: Vec::new(),
            raw: table,
            instructions: Vec::new(),
            // Empty for the same reason `sources` is: there is no file behind
            // parsed text, and reporting `io.toml` here would name a file that was
            // never read.
            origins: BTreeMap::new(),
            dir: PathBuf::from("."),
            plugin_decls,
        })
    }

    /// This configuration with `[profile.<name>]` overlaid on it (0.27.0).
    ///
    /// The overlay uses the same merge the scopes use, so a profile has no merge
    /// semantics of its own: a scalar replaces, a table merges key by key, an array
    /// replaces whole. Scopes merge first and the profile applies to the result, so a
    /// profile in any scope beats a base key in every scope.
    ///
    /// A name the file does not carry is an error naming it — a `--profile` argument
    /// that silently does nothing is the same failure class as an unknown key.
    /// Profiles do not compose: the returned configuration carries no `[profile]`
    /// section, so a second call fails.
    ///
    /// ```
    /// use io_harness::Config;
    ///
    /// let config = Config::from_toml(r#"
    ///     [run]
    ///     max_steps = 30
    ///     max_retries = 4
    ///
    ///     [profile.cheap]
    ///     run = { max_steps = 5 }
    /// "#).unwrap();
    ///
    /// let cheap = config.with_profile("cheap").unwrap();
    /// let base = |c: &Config| c.apply_to(io_harness::TaskContract::new(
    ///     "x", "src/lib.rs", io_harness::Verification::None));
    /// assert_eq!(base(&cheap).max_steps, 5);
    /// assert_eq!(base(&cheap).max_retries, 4, "a key the profile never named is untouched");
    /// assert_eq!(base(&config).max_steps, 30, "and the configuration it came from is unchanged");
    ///
    /// let err = config.with_profile("careful").unwrap_err();
    /// assert!(err.to_string().contains("careful"), "{err}");
    /// ```
    pub fn with_profile(&self, name: &str) -> Result<Self> {
        let overlay = self
            .raw
            .get("profile")
            .and_then(toml::Value::as_table)
            .and_then(|t| t.get(name))
            .and_then(toml::Value::as_table)
            .ok_or_else(|| Error::Config(format!("no `[profile.{name}]` in this configuration")))?
            .clone();

        let mut merged = self.raw.clone();
        merged.remove("profile");
        // A profile overlay is applied after every scope has been folded
        // together, so the scope its keys came from is no longer knowable here.
        // Narrowing is therefore not re-applied; see the release record.
        merge(&mut merged, overlay.clone(), &mut Vec::new(), false);

        // A profile key's origin is the file the *profile* was written in, which is
        // recorded under `profile.<name>.<key>`. Move each one onto the key it now
        // decides, then drop the `profile.` entries — the returned configuration
        // carries no `[profile]` section, so an origin for one would describe a key
        // that is no longer there.
        let mut origins = self.origins.clone();
        let prefix = format!("profile.{name}.");
        let overlaid: Vec<(String, Vec<Origin>)> = origins
            .iter()
            .filter_map(|(key, at)| {
                key.strip_prefix(&prefix)
                    .map(|rest| (rest.to_string(), at.clone()))
            })
            .collect();
        origins.retain(|key, _| !key.starts_with("profile."));
        origins.extend(overlaid);

        Ok(Self {
            file: deserialize(toml::Value::Table(merged.clone()), Path::new("<profile>"))?,
            sources: self.sources.clone(),
            raw: merged,
            instructions: self.instructions.clone(),
            origins,
            dir: self.dir.clone(),
            // A profile overlays keys; it does not re-declare bundles. Carried
            // whole so `with_profile` does not quietly change what is loaded.
            plugin_decls: self.plugin_decls.clone(),
        })
    }

    /// The files this configuration was merged from, in the order they were
    /// applied — so the last one that names a key is the one that won it.
    ///
    /// ```
    /// use io_harness::Config;
    ///
    /// let config = Config::from_toml("").unwrap();
    /// assert!(config.sources().is_empty(), "parsed text has no file behind it");
    /// ```
    pub fn sources(&self) -> &[(Scope, PathBuf)] {
        &self.sources
    }

    /// Which file decided `key`, by dotted path — `"run.max_steps"`,
    /// `"sandbox.limits.max_wall_secs"`, `"toolchain.cargo.test"` (0.30.0).
    ///
    /// Empty when no file named the key, which is the crate's default answering
    /// and is deliberately not dressed up as a file. Exactly one entry for every
    /// key but the two whose arrays append across scopes — `policy.layers` and
    /// `agent` — where every contributing file is listed in the order they were
    /// applied, because a single winner there would be a lie about a value more
    /// than one file built.
    ///
    /// This is the *deciding* scope, not the last scope read: a key set only in
    /// the user file reports the user file even when a project file exists and
    /// names other keys.
    ///
    /// ```
    /// use io_harness::config::{Config, Scope};
    ///
    /// # fn demo() -> io_harness::Result<()> {
    /// let dir = tempfile::tempdir()?;
    /// std::fs::write(dir.path().join("io.toml"), "[run]\nmax_steps = 30\nmax_retries = 4\n")?;
    /// std::fs::write(dir.path().join("io.local.toml"), "[run]\nmax_steps = 5\n")?;
    ///
    /// let config = Config::discover(dir.path())?;
    /// assert_eq!(config.origin("run.max_steps")[0].scope, Scope::Local);
    /// assert_eq!(
    ///     config.origin("run.max_retries")[0].scope,
    ///     Scope::Project,
    ///     "a key the later file never named is still the earlier file's"
    /// );
    /// # Ok(()) }
    /// # demo().unwrap();
    /// ```
    pub fn origin(&self, key: &str) -> &[Origin] {
        self.origins.get(key).map_or(&[], Vec::as_slice)
    }

    /// Every key a file set, with the file that decided it, in key order
    /// (0.30.0).
    ///
    /// The whole-settings-list form of [`Config::origin`], for a caller rendering
    /// what a workspace resolved to rather than asking about one key. Keys a file
    /// never named are absent rather than present-and-empty, so what this yields
    /// is exactly the configuration that was written down.
    ///
    /// ```
    /// use io_harness::config::Config;
    ///
    /// # fn demo() -> io_harness::Result<()> {
    /// let dir = tempfile::tempdir()?;
    /// std::fs::write(dir.path().join("io.toml"), "[run]\nmax_steps = 30\n")?;
    ///
    /// let config = Config::discover(dir.path())?;
    /// let keys: Vec<_> = config.origins().map(|(key, _)| key).collect();
    /// assert_eq!(keys, ["run.max_steps"]);
    /// # Ok(()) }
    /// # demo().unwrap();
    /// ```
    pub fn origins(&self) -> impl Iterator<Item = (&str, &[Origin])> {
        self.origins
            .iter()
            .map(|(key, at)| (key.as_str(), at.as_slice()))
    }

    /// Does this configuration set anything at all?
    ///
    /// ```
    /// use io_harness::Config;
    ///
    /// assert!(Config::from_toml("").unwrap().is_empty());
    /// assert!(!Config::from_toml("[run]\nmax_steps = 3\n").unwrap().is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.file.policy.is_none()
            && self.file.sandbox.is_none()
            && self.file.run.is_none()
            && self.file.toolchain.is_empty()
            && self.file.prices.is_none()
            && self.file.mcp.is_empty()
            && self.file.agent.is_empty()
            && self.file.web.is_none()
            && self.file.provider.is_empty()
            && self.file.app.is_none()
            && self.file.profile.is_empty()
            && self.file.instructions.is_none()
    }

    /// The provider this configuration says to run, or `None` where it declares no
    /// `[[provider]]` (0.27.0).
    ///
    /// `None` means the file said nothing — never that the crate picked a default.
    /// This is the rule every accessor in this module holds, and it matters most
    /// here: a defaulted provider would be a vendor the operator never named.
    ///
    /// ```
    /// use io_harness::{Config, ProviderSpec};
    ///
    /// # fn demo() -> io_harness::Result<()> {
    /// let home = tempfile::tempdir()?;
    /// std::env::set_var("IO_CONFIG_HOME", home.path());
    /// std::fs::write(home.path().join("io.toml"), r#"
    ///     [[provider]]
    ///     kind = "anthropic"
    ///     model = "claude-sonnet-4"
    ///     api_key = "sk-written-here-or-left-out-for-ANTHROPIC_API_KEY"
    /// "#)?;
    ///
    /// let config = Config::discover(tempfile::tempdir()?.path())?;
    /// assert!(matches!(config.provider_spec(), Some(ProviderSpec::Anthropic { .. })));
    /// assert!(Config::from_toml("").unwrap().provider_spec().is_none());
    ///
    /// // 0.71.0 — formatting a spec says whether a key was written, never what it
    /// // was. `Serialize` is untouched: what an operator typed is what is persisted.
    /// let rendered = format!("{:?}", config.provider_spec().unwrap());
    /// assert!(rendered.contains("api_key: <redacted>"), "{rendered}");
    /// assert!(rendered.contains("claude-sonnet-4"), "{rendered}");
    /// assert!(!rendered.contains("sk-written-here"), "{rendered}");
    /// # Ok(()) }
    /// # demo().unwrap();
    /// ```
    pub fn provider_spec(&self) -> Option<&ProviderSpec> {
        self.file.provider.first()
    }

    /// The chain standing behind [`Config::provider_spec`], in the order written
    /// (0.27.0).
    ///
    /// Empty where the file names one provider or none. The application nests them:
    /// [`Fallback`](crate::provider::Fallback) is generic over two type parameters and
    /// composes — `Fallback::new(a, Fallback::new(b, c))` — so a chain of three is
    /// three lines of the caller's own code rather than a `dyn` the trait cannot have.
    ///
    /// ```
    /// use io_harness::{Config, ProviderSpec};
    ///
    /// # fn demo() -> io_harness::Result<()> {
    /// let home = tempfile::tempdir()?;
    /// std::env::set_var("IO_CONFIG_HOME", home.path());
    /// std::fs::write(home.path().join("io.toml"), r#"
    ///     [[provider]]
    ///     kind = "openrouter"
    ///     model = "primary"
    ///
    ///     [[provider]]
    ///     kind = "anthropic"
    ///     model = "second"
    ///
    ///     [[provider]]
    ///     kind = "openai"
    ///     model = "third"
    /// "#)?;
    ///
    /// let config = Config::discover(tempfile::tempdir()?.path())?;
    ///
    /// // Order is the configuration, not a detail of it.
    /// let models: Vec<&str> = config.fallback_specs().iter().map(|s| match s {
    ///     ProviderSpec::Anthropic { model, .. }
    ///     | ProviderSpec::OpenAi { model, .. }
    ///     | ProviderSpec::OpenRouter { model, .. } => model.as_str(),
    ///     _ => "unknown",
    /// }).collect();
    /// assert_eq!(models, ["second", "third"]);
    /// # Ok(()) }
    /// # demo().unwrap();
    /// ```
    pub fn fallback_specs(&self) -> &[ProviderSpec] {
        self.file.provider.get(1..).unwrap_or_default()
    }

    /// The application's own settings under `[app.<key>]`, deserialized into the
    /// application's own type (0.27.0).
    ///
    /// This crate stores `[app]` and **never validates it**. That is the whole
    /// feature: an application layer keeps its settings in the same file without the
    /// harness pretending to understand them, and an unknown key here is the caller's
    /// business rather than an error. Every other section still rejects what it does
    /// not know — this is one hole with a wall around it, not the wall coming down.
    ///
    /// Generic rather than returning a `toml::Value`, so no `toml` type reaches this
    /// crate's public API and no version of it becomes a semver commitment.
    ///
    /// ```
    /// use io_harness::Config;
    ///
    /// #[derive(serde::Deserialize)]
    /// struct Cli { theme: String, width: u32 }
    ///
    /// let config = Config::from_toml(r#"
    ///     [app.cli]
    ///     theme = "dark"
    ///     width = 100
    /// "#).unwrap();
    ///
    /// let cli: Cli = config.app("cli").unwrap().expect("the file carries [app.cli]");
    /// assert_eq!(cli.theme, "dark");
    /// // A key the file does not carry is absent, not an error and not a default.
    /// assert!(config.app::<Cli>("studio").unwrap().is_none());
    /// ```
    pub fn app<T: serde::de::DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let Some(value) = self.file.app.as_ref().and_then(|t| t.get(key)) else {
            return Ok(None);
        };
        value
            .clone()
            .try_into()
            .map(Some)
            .map_err(|e: toml::de::Error| Error::Config(format!("`[app.{key}]`: {}", e.message())))
    }

    /// What `[instructions]` discovered, worded and attributed (0.27.0).
    ///
    /// One entry per file that existed and carried text, each naming the file it came
    /// from, and applied to a contract by [`Config::apply_to`] — which since 0.45.0
    /// puts it in [`TaskContract::instructions`](crate::TaskContract::instructions)
    /// and carries it in the **system block**, not in
    /// [`constraints`](crate::TaskContract::constraints) and not in the user turn. A
    /// constraint is a rule the goal is checked against; this is guidance the agent
    /// reads. Empty where no named file exists, where a project opted out with
    /// `[instructions] files = []`, and always for [`Config::from_toml`], which has no
    /// root to discover against.
    ///
    /// Since 0.45.0 the search runs whether or not an `[instructions]` table is
    /// present, so a repository carrying `AGENTS.md` and no `io.toml` is read.
    ///
    /// The files are read inside [`Config::discover`] — the caller's own call, before
    /// the run — so "nothing is loaded implicitly" still holds exactly.
    ///
    /// **They are untrusted text**, and 0.45.0 moved them somewhere more
    /// authoritative. A discovered `AGENTS.md` reaches the model inside a delimited
    /// section that frames it as the repository's guidance, ahead of the boundary
    /// section and the crate's own ending, and it grants nothing: the boundary is
    /// still the [`Policy`] the caller loaded, enforced before any call runs.
    pub fn instructions(&self) -> &[String] {
        &self.instructions
    }

    // -----------------------------------------------------------------------
    // Projection
    // -----------------------------------------------------------------------

    /// The [`Policy`] this configuration describes, or `None` where it has no
    /// `[policy]` section.
    ///
    /// The base is [`Policy::default`] — the tiered default, with the secret
    /// patterns already denied — not [`Policy::permissive`]: a file that names a
    /// layer and forgets a default must not end up enforcing less than a caller
    /// who wrote no file at all. Configured layers are appended after the
    /// built-in ones, and the type's own rule still holds across the seam: a
    /// later layer may add capability and may never re-allow an earlier deny.
    ///
    /// ```
    /// use io_harness::{Act, Config, Effect};
    ///
    /// let config = Config::from_toml(r#"
    ///     [policy.defaults]
    ///     write = "deny"
    ///
    ///     [[policy.layers]]
    ///     name = "ops-baseline"
    ///     rules = [{ act = "read", effect = "allow", pattern = "src/*" }]
    /// "#).unwrap();
    ///
    /// let policy = config.policy().unwrap();
    /// assert_eq!(policy.check(Act::Write, "src/lib.rs").effect, Effect::Deny);
    /// assert_eq!(policy.check(Act::Read, "src/lib.rs").effect, Effect::Allow);
    /// // The built-in secret denies survive a file that never mentioned them.
    /// assert_eq!(policy.check(Act::Read, ".env").effect, Effect::Deny);
    /// ```
    pub fn policy(&self) -> Option<Policy> {
        let section = self.file.policy.as_ref()?;
        let base = Policy::default();
        let defaults = match &section.defaults {
            None => base.defaults,
            Some(d) => Defaults {
                read: d.read.unwrap_or(base.defaults.read),
                write: d.write.unwrap_or(base.defaults.write),
                exec: d.exec.unwrap_or(base.defaults.exec),
                net: d.net.unwrap_or(base.defaults.net),
            },
        };
        let mut layers = base.layers;
        layers.extend(section.layers.iter().cloned());
        Some(Policy { layers, defaults })
    }

    /// The [`SandboxConfig`] this configuration describes, or `None` where it has
    /// no `[sandbox]` section.
    ///
    /// Caps merge one key at a time onto [`SandboxLimits::default`](crate::sandbox::SandboxLimits::default), so a file
    /// that lowers the wall clock keeps the default memory cap. `0` means *no
    /// cap* — TOML has no null, and "absent" already means "inherit".
    ///
    /// ```
    /// use io_harness::Config;
    ///
    /// let config = Config::from_toml(r#"
    ///     [sandbox.limits]
    ///     max_wall_secs = 30
    ///     max_cpu_secs = 0
    /// "#).unwrap();
    ///
    /// let sandbox = config.sandbox().unwrap();
    /// assert_eq!(sandbox.limits.max_wall_secs, Some(30));
    /// assert_eq!(sandbox.limits.max_cpu_secs, None, "0 is no cap, not a zero-second cap");
    /// assert_eq!(sandbox.limits.max_open_files, Some(512), "untouched caps keep their default");
    /// assert!(!sandbox.allow_network, "and the default-deny on egress still holds");
    /// ```
    pub fn sandbox(&self) -> Option<SandboxConfig> {
        let section = self.file.sandbox.as_ref()?;
        let mut config = SandboxConfig::new();
        if let Some(limits) = &section.limits {
            let base = &mut config.limits;
            for (from, to) in [
                (limits.max_cpu_secs, &mut base.max_cpu_secs),
                (limits.max_wall_secs, &mut base.max_wall_secs),
                (limits.max_memory_bytes, &mut base.max_memory_bytes),
                (limits.max_processes, &mut base.max_processes),
                (limits.max_open_files, &mut base.max_open_files),
            ] {
                if let Some(v) = from {
                    *to = if v == 0 { None } else { Some(v) };
                }
            }
        }
        if let Some(v) = section.allow_network {
            config.allow_network = v;
        }
        if let Some(v) = section.force_floor {
            config.force_floor = v;
        }
        if let Some(v) = section.mode {
            config.mode = v;
        }
        Some(config)
    }

    /// The [`PriceTable`] this configuration describes, or `None` where it has no
    /// `[prices]` section.
    ///
    /// This is where a price comes from. The crate ships none — it cannot keep a
    /// vendor's list accurate on its own release schedule — so until an operator
    /// writes one down, [`crate::pricing`] reports unpriced calls rather than a
    /// cost.
    ///
    /// ```
    /// use io_harness::{Config, Usage};
    ///
    /// let config = Config::from_toml(r#"
    ///     [prices]
    ///     as_of = "2026-07-29"
    ///
    ///     [prices.models."some-vendor/some-model"]
    ///     input = 3_000_000
    ///     output = 15_000_000
    /// "#).unwrap();
    ///
    /// let prices = config.prices().unwrap();
    /// let usage = Usage { prompt_tokens: 1_000_000, completion_tokens: 0, ..Default::default() };
    /// assert_eq!(prices.cost_micros("some-vendor/some-model", &usage), Some(3_000_000));
    /// assert_eq!(prices.as_of(), "2026-07-29");
    /// ```
    pub fn prices(&self) -> Option<PriceTable> {
        let section = self.file.prices.as_ref()?;
        let mut table = PriceTable::new(section.as_of.clone());
        for (model, price) in &section.models {
            table = table.with(model.clone(), *price);
        }
        Some(table)
    }

    /// `detected` with this configuration's override for its ecosystem applied.
    ///
    /// Keyed on [`Toolchain::ecosystem`], so a file may carry an override for
    /// every ecosystem a team works in and only the matching one is used. A
    /// detection with no override comes back unchanged.
    ///
    /// This projection is for the embedding application: the harness's own run
    /// loop detects for itself and does not consult a config, because reaching it
    /// would mean a new `TaskContract` field, which is a break this release does
    /// not carry.
    ///
    /// ```
    /// use io_harness::Config;
    ///
    /// # fn demo() -> std::io::Result<()> {
    /// let dir = tempfile::tempdir()?;
    /// std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n")?;
    /// let detected = io_harness::toolchain::detect(dir.path()).unwrap();
    /// assert_eq!(detected.test, ["cargo", "test"]);
    ///
    /// let config = Config::from_toml(r#"
    ///     [toolchain.cargo]
    ///     test = ["cargo", "nextest", "run"]
    /// "#).unwrap();
    ///
    /// let tuned = config.toolchain(detected);
    /// assert_eq!(tuned.test, ["cargo", "nextest", "run"]);
    /// assert_eq!(tuned.build, ["cargo", "build"], "what the file did not name is unchanged");
    /// # Ok(()) }
    /// # demo().unwrap();
    /// ```
    pub fn toolchain(&self, detected: Toolchain) -> Toolchain {
        let Some(section) = self.file.toolchain.get(&detected.ecosystem) else {
            return detected;
        };
        let mut out = detected;
        if let Some(v) = &section.manager {
            out.manager = v.clone();
        }
        for (from, to) in [
            (&section.install, &mut out.install),
            (&section.build, &mut out.build),
            (&section.test, &mut out.test),
            (&section.lint, &mut out.lint),
            (&section.format, &mut out.format),
            (&section.run, &mut out.run),
        ] {
            if let Some(v) = from {
                *to = v.clone();
            }
        }
        out
    }

    /// The MCP servers this configuration declares.
    ///
    /// **`[[mcp]]` is a user-scope table** (0.74.0). It names a command, an argv
    /// and an environment that this process spawns at run start, and the spawn gate
    /// is an `Act::Exec` check on the binary name alone — so a workspace file that
    /// could write `command = "node"` in a repository that legitimately allows
    /// `node` could run anything. `plugin.rs` has refused the same declaration from
    /// a project-scoped bundle since 0.35.0; this closes the route around it.
    ///
    /// ```
    /// use io_harness::Config;
    ///
    /// # fn demo() -> io_harness::Result<()> {
    /// let home = tempfile::tempdir()?;
    /// std::env::set_var("IO_CONFIG_HOME", home.path());
    /// std::fs::write(home.path().join("io.toml"), r#"
    ///     [[mcp]]
    ///     id = "github"
    ///     transport = "stdio"
    ///     command = "github-mcp-server"
    ///     args = ["stdio"]
    /// "#)?;
    ///
    /// let config = Config::discover(tempfile::tempdir()?.path())?;
    /// assert_eq!(config.mcp_servers()[0].id, "github");
    ///
    /// // The same table in a committed `io.toml` is refused, naming the key.
    /// let err = Config::from_toml("[[mcp]]\nid = \"x\"\ntransport = \"stdio\"\ncommand = \"x\"\n")
    ///     .unwrap_err();
    /// assert!(err.to_string().contains("key `mcp`"), "{err}");
    /// # Ok(()) }
    /// # demo().unwrap();
    /// ```
    pub fn mcp_servers(&self) -> &[McpServer] {
        &self.file.mcp
    }

    /// The language servers this configuration declares (0.52.0).
    ///
    /// **`[[lsp]]` is a user-scope table** (0.74.0), for the reason `[[mcp]]` is
    /// and stated in full there: it names a command this process spawns at run
    /// start. Until 0.74.0 it was allowed at project scope on the argument that the
    /// boundary is the `Act::Exec` check on the named binary — which an argv the
    /// same table supplies walks straight through.
    ///
    /// ```
    /// use io_harness::Config;
    ///
    /// # fn demo() -> io_harness::Result<()> {
    /// let home = tempfile::tempdir()?;
    /// std::env::set_var("IO_CONFIG_HOME", home.path());
    /// std::fs::write(home.path().join("io.toml"), r#"
    ///     [[lsp]]
    ///     id = "rust"
    ///     command = "rust-analyzer"
    ///     extensions = [".rs"]
    /// "#)?;
    ///
    /// let config = Config::discover(tempfile::tempdir()?.path())?;
    /// assert_eq!(config.lsp_servers()[0].id, "rust");
    /// // Unset keys take their defaults rather than becoming absent.
    /// assert_eq!(config.lsp_servers()[0].timeout_secs, 60);
    ///
    /// // The same table in a committed `io.toml` is refused, naming the key.
    /// let err = Config::from_toml(
    ///     "[[lsp]]\nid = \"rust\"\ncommand = \"rust-analyzer\"\nextensions = [\".rs\"]\n",
    /// ).unwrap_err();
    /// assert!(err.to_string().contains("key `lsp`"), "{err}");
    ///
    /// // A misspelled key is still rejected by name rather than ignored, in the
    /// // scope that may write the table: this one names a program to spawn.
    /// std::fs::write(
    ///     home.path().join("io.toml"),
    ///     "[[lsp]]\nid = \"rust\"\ncommand = \"rust-analyzer\"\nextension = [\".rs\"]\n",
    /// )?;
    /// let err = Config::discover(tempfile::tempdir()?.path()).unwrap_err();
    /// assert!(err.to_string().contains("extension"), "{err}");
    /// # Ok(()) }
    /// # demo().unwrap();
    /// ```
    pub fn lsp_servers(&self) -> &[crate::lsp::LspServer] {
        &self.file.lsp
    }

    /// The browser this configuration declares, if any (0.53.0).
    ///
    /// `[browser]` is refused in any file inside a workspace, because it names a
    /// program to execute: `io.toml` arrives with a `git clone`, and since 0.74.0
    /// `io.local.toml` is held to the same rule because a run's own agent can write
    /// it. The same rule refuses `[[hook]]`, for the same reason.
    ///
    /// Write it in the user-scope file, which [`Config::discover`] also reads;
    /// there is no route to a browser from a file under the workspace root.
    ///
    /// ```
    /// use io_harness::Config;
    ///
    /// // The project scope refuses the table by name, before anything is run.
    /// let err = Config::from_toml(r#"
    ///     [browser]
    ///     binary = "/usr/bin/chromium"
    /// "#).unwrap_err();
    /// assert!(err.to_string().contains("browser"), "{err}");
    ///
    /// // A configuration that declares none simply has none — the default, and
    /// // what keeps a run byte-identical to one built before this release.
    /// let plain = Config::from_toml("[run]\nmax_steps = 3\n").unwrap();
    /// assert!(plain.browser().is_none());
    /// ```
    #[cfg(feature = "browser")]
    #[cfg_attr(docsrs, doc(cfg(feature = "browser")))]
    pub fn browser(&self) -> Option<&crate::browser::BrowserConfig> {
        self.file.browser.as_ref()
    }

    /// The named agent definitions this configuration declares (0.21.0).
    ///
    /// `[[agent]]` tables **accumulate** across scopes the way `policy.layers` does,
    /// rather than the narrower scope replacing the wider one: a project roster and a
    /// developer's own extra agent are both wanted, and a local file that silently
    /// deleted the project's agents would be a roster nobody could rely on. A later
    /// scope registering the same *name* still replaces that one definition, because
    /// [`Agents`](crate::Agents) is keyed by name.
    ///
    /// A definition can only ever narrow a child's boundary — there is no
    /// `allow_write` to write here — so a roster in a config file grants nothing.
    ///
    /// ```
    /// use io_harness::Config;
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let config = Config::from_toml(
    ///     r#"
    ///     [[agent]]
    ///     name = "searcher"
    ///     model = "cheap-model"
    ///     deny_write = true
    ///
    ///     [[agent]]
    ///     name = "author"
    ///     role = "You make the edit the searcher located."
    ///     "#,
    /// )?;
    ///
    /// let agents = config.agents();
    /// assert_eq!(agents.names(), vec!["author", "searcher"]);
    /// assert!(agents.get("searcher").unwrap().deny_write);
    /// # Ok(())
    /// # }
    /// ```
    pub fn agents(&self) -> crate::agent::Agents {
        self.file
            .agent
            .iter()
            .cloned()
            .fold(crate::agent::Agents::new(), |roster, def| roster.with(def))
    }

    /// The `[[hook]]` tables of this configuration, as an [`Observer`](crate::Observer)
    /// (0.28.0).
    ///
    /// Empty where the file declared none, which is the rule every accessor in this
    /// module holds: the file saying nothing stays distinguishable from the crate
    /// choosing something. The caller installs the result — `run_observed`,
    /// `resume_observed` or any of the tree forms — so a hook obeys the same
    /// "nothing happens implicitly" rule as every other projection here.
    ///
    /// A relative `append` path resolves against the discovery root this
    /// configuration was loaded for, so a user-scope hook writes its log beside the
    /// project it is watching rather than beside itself.
    ///
    /// ```
    /// use io_harness::Config;
    ///
    /// # fn demo() -> io_harness::Result<()> {
    /// let home = tempfile::tempdir()?;
    /// std::env::set_var("IO_CONFIG_HOME", home.path());
    /// std::fs::write(
    ///     home.path().join("io.toml"),
    ///     "[[hook]]\non = [\"finished\"]\nrun = [\"cargo\", \"fmt\"]\n",
    /// )?;
    ///
    /// let dir = tempfile::tempdir()?;
    /// let config = Config::discover(dir.path())?;
    /// assert!(!config.hooks().is_empty());
    ///
    /// // The same table in either file under the workspace root is refused:
    /// // `io.toml` arrives with a clone, and `io.local.toml` is a path the run's
    /// // own agent can write (0.74.0).
    /// let err = Config::from_toml("[[hook]]\nrun = [\"cargo\", \"fmt\"]\n").unwrap_err();
    /// assert!(err.to_string().contains("may not declare hooks"), "{err}");
    ///
    /// std::fs::write(
    ///     dir.path().join("io.local.toml"),
    ///     "[[hook]]\non = [\"finished\"]\nrun = [\"cargo\", \"fmt\"]\n",
    /// )?;
    /// let err = Config::discover(dir.path()).unwrap_err();
    /// assert!(err.to_string().contains("may not declare hooks"), "{err}");
    /// # Ok(()) }
    /// # demo().unwrap();
    /// ```
    pub fn hooks(&self) -> crate::hooks::Hooks {
        crate::hooks::Hooks::new(self.file.hook.clone(), &self.dir)
    }

    /// Every `[[plugin]]` this configuration declares, loaded (0.35.0).
    ///
    /// Infallible, and that is the feature: a bundle that cannot be loaded is
    /// **dropped** onto [`Plugins::dropped`](crate::Plugins::dropped) with its
    /// reason rather than failing the call, so one broken directory cannot take a
    /// run with it. See [`crate::plugin`] for the manifest, the trust rule that
    /// bounds what a project-scoped declaration may contribute, and the
    /// namespacing that puts a plugin's id into the trace.
    ///
    /// A relative `path` resolves against the discovery root, which is the
    /// project the harness was pointed at rather than the directory the declaring
    /// file lives in — the rule a `[[hook]]`'s `append` already follows.
    ///
    /// ```
    /// use io_harness::Config;
    ///
    /// # fn demo() -> io_harness::Result<()> {
    /// let dir = tempfile::tempdir()?;
    /// let root = dir.path();
    /// std::fs::create_dir(root.join("bundle"))?;
    /// std::fs::write(root.join("bundle").join("plugin.toml"), "name = \"review\"\n")?;
    /// std::fs::write(root.join("io.toml"), "[[plugin]]\npath = \"bundle\"\n")?;
    ///
    /// let plugins = Config::discover(root)?.plugins();
    /// assert_eq!(plugins.names(), vec!["review"]);
    /// assert!(plugins.dropped().is_empty());
    ///
    /// // A configuration that declares none reads no directory at all.
    /// assert!(Config::from_toml("").unwrap().plugins().is_empty());
    /// # Ok(()) }
    /// # demo().unwrap();
    /// ```
    pub fn plugins(&self) -> crate::plugin::Plugins {
        crate::plugin::Plugins::load(&self.plugin_decls, &self.dir)
    }

    /// The prompt-template directory this configuration points at, if any (0.21.0).
    ///
    /// Discovery is the caller's to do — [`Templates::discover`](crate::Templates) is
    /// fallible and this is not, and rendering a template happens before a run exists.
    pub fn templates(&self) -> Option<&std::path::Path> {
        self.file.run.as_ref().and_then(|r| r.templates.as_deref())
    }

    /// `contract` with everything this configuration's `[run]`, `[[mcp]]`,
    /// `[[agent]]` and `[web]` sections set.
    ///
    /// A method on `Config` rather than a field on [`TaskContract`]: a new public
    /// field is a break, and this release carries none. What the file cannot set
    /// is the task itself — `goal`, `file`, `root` and `verify` are what the
    /// caller is asking for now, not a property of the project.
    ///
    /// ```
    /// use std::time::Duration;
    /// use io_harness::{Config, TaskContract, Verification, WebAccess};
    ///
    /// let config = Config::from_toml(r#"
    ///     [run]
    ///     max_steps = 30
    ///     max_duration_secs = 900
    ///     max_retries = 4
    ///
    ///     [run.retry]
    ///     base_ms = 2000
    ///
    ///     [run.stall]
    ///     window = 5
    ///
    ///     [web]
    ///     search = true
    ///     max_uses = 3
    ///     allowed_domains = ["docs.rs"]
    /// "#).unwrap();
    ///
    /// let contract = config.apply_to(TaskContract::new(
    ///     "make the suite pass",
    ///     "src/lib.rs",
    ///     Verification::FileContains("fn".into()),
    /// ));
    ///
    /// assert_eq!(contract.max_steps, 30);
    /// assert_eq!(contract.max_duration, Some(Duration::from_secs(900)));
    /// assert_eq!(contract.max_retries, 4);
    /// assert_eq!(contract.retry.base, Duration::from_millis(2000));
    /// // A key the file left out keeps the type's own default.
    /// assert_eq!(contract.retry.max, Duration::from_secs(30));
    /// assert_eq!(contract.stall.window, 5);
    /// // `[web]` is the same value the programmatic builder produces, which is
    /// // what makes the file a projection of the typed API rather than a second
    /// // way of describing web access.
    /// assert_eq!(
    ///     contract.web,
    ///     Some(WebAccess::search().max_uses(3).allow("docs.rs")),
    /// );
    /// ```
    #[must_use]
    pub fn apply_to(&self, contract: TaskContract) -> TaskContract {
        let mut out = contract;
        if !self.file.mcp.is_empty() {
            out = out.with_mcp(self.file.mcp.iter().cloned());
        }
        // 0.52.0 — same shape as `[[mcp]]` above, and top-level for the same
        // reason: a file that declares only servers must still get its servers.
        if !self.file.lsp.is_empty() {
            out = out.with_lsp(self.file.lsp.iter().cloned());
        }
        // 0.53.0 — `[browser]` is top-level, and carried whenever the table is
        // present. No process starts from this: the contract records that a
        // browser is configured, and one is spawned only if an action needs it.
        #[cfg(feature = "browser")]
        if let Some(browser) = &self.file.browser {
            out = out.with_browser(browser.clone());
        }
        // 0.21.0 — `[[agent]]` is top-level, not part of `[run]`, so it is applied
        // before the `[run]` guard below: a file that declares a roster and nothing
        // else must still get its roster.
        if !self.file.agent.is_empty() {
            out = out.with_agents(self.agents());
        }
        // 0.22.0 — `[web]` is top-level too, and it is carried whenever the table
        // is present rather than only when a switch is on: a file that writes
        // `[web]` with `search = false` is stating a decision, and dropping it here
        // would make the contract say "nothing was configured" instead.
        if let Some(web) = &self.file.web {
            out = out.with_web(web.clone());
        }
        // 0.45.0 — discovered project instructions land in `instructions`, not in
        // `constraints`. 0.27.0 put them in `constraints` because a new
        // `TaskContract` field was a break at the time; the type has been
        // `#[non_exhaustive]` since 0.35.0, so the field is free, and the two things
        // were never the same: a constraint is a rule the goal is checked against and
        // rides in the user turn on every step, while this is a repository's guidance,
        // carried once in the system block.
        for instruction in &self.instructions {
            out = out.with_instruction(instruction.clone());
        }

        // 0.56.0 — `[memory]` is top-level and applied before the `[run]` guard,
        // for the reason `[[agent]]` is: a file that sets a cap and nothing else
        // must still get its cap. Key by key onto whatever the contract already
        // carries, so naming one cap leaves the other two where they were rather
        // than resetting them to the defaults of a section nobody wrote.
        if let Some(memory) = &self.file.memory {
            let mut limits = out.memory;
            if let Some(v) = memory.max_entries {
                limits.max_entries = v as usize;
            }
            if let Some(v) = memory.max_chars {
                limits.max_chars = v as usize;
            }
            if let Some(v) = memory.max_entry_chars {
                limits.max_entry_chars = v as usize;
            }
            out = out.with_memory_limits(limits);
        }

        let Some(run) = &self.file.run else {
            return out;
        };
        if let Some(v) = run.max_steps {
            out = out.with_max_steps(v);
        }
        if let Some(v) = run.max_duration_secs {
            out = out.with_time_budget(Duration::from_secs(v));
        }
        if let Some(v) = run.max_tokens {
            out = out.with_token_budget(v);
        }
        if let Some(v) = run.max_retries {
            out = out.with_max_retries(v);
        }
        if let Some(v) = run.exec_timeout_secs {
            out = out.with_exec_timeout(Duration::from_secs(v));
        }
        if let Some(v) = &run.skills {
            out = out.with_skills(v.clone());
        }
        if let Some(v) = run.retry {
            out = out.with_retry_policy(v);
        }
        if let Some(v) = run.stall {
            out = out.with_stall_policy(v);
        }
        if let Some(v) = run.context {
            out = out.with_context_budget(v);
        }
        if let Some(v) = run.max_read_chars {
            out = out.with_max_read_chars(v);
        }
        if let Some(v) = run.max_wait_secs {
            out = out.with_max_wait_secs(v);
        }
        if let Some(v) = &run.commit_identity {
            out = out.with_commit_identity(v.name.clone(), v.email.clone());
        }
        out
    }
}

/// Where the user-scope file lives on this platform, or `None` where no home
/// directory could be determined.
///
/// `$IO_CONFIG` names the file itself and wins outright (0.27.0), then
/// `$IO_CONFIG_HOME` names its directory, then `$XDG_CONFIG_HOME/io` or
/// `~/.config/io` on unix and `%APPDATA%\io` on Windows.
///
/// `$IO_CONFIG` names the **user scope**. It does not bypass the merge, so a project
/// file still wins the keys it names — which is what keeps the scopes at four.
///
/// ```
/// // Whatever it resolves to, it is the same answer twice — a config path that
/// // moved between two calls would make "which file won" unanswerable.
/// assert_eq!(io_harness::config::user_path(), io_harness::config::user_path());
/// ```
#[must_use]
pub fn user_path() -> Option<PathBuf> {
    if let Some(file) = env_dir(CONFIG_VAR) {
        return Some(file);
    }
    if let Some(dir) = env_dir(CONFIG_HOME_VAR) {
        return Some(dir.join(PROJECT_FILE));
    }
    #[cfg(windows)]
    let base = env_dir("APPDATA")?;
    #[cfg(not(windows))]
    let base = match env_dir("XDG_CONFIG_HOME") {
        Some(dir) => dir,
        None => env_dir("HOME")?.join(".config"),
    };
    Some(base.join("io").join(PROJECT_FILE))
}

/// A directory named by an environment variable, ignoring one set to nothing —
/// an empty `HOME` is not a home directory at the filesystem root.
fn env_dir(var: &str) -> Option<PathBuf> {
    let value = std::env::var_os(var)?;
    if value.is_empty() {
        return None;
    }
    Some(PathBuf::from(value))
}

// ---------------------------------------------------------------------------
// Parse, substitute, validate, merge
// ---------------------------------------------------------------------------

/// Name a credential file that other accounts on this host can reach (0.74.0).
///
/// `io.local.toml`, the user-scope `io.toml` and every `${file:}` target hold the
/// one thing in this format worth stealing — an `api_key`, a bearer token, a
/// header. They were read with `read_to_string` and their mode was never looked
/// at, so a file left at `0644` by an editor's umask handed every account on a
/// shared host a working credential, and nothing anywhere said so.
///
/// **A warning, not a refusal**, and the difference is the whole decision. `ssh`
/// refuses because it is an interactive client that can say why and be re-run a
/// second later; this is a library inside somebody else's binary, where the same
/// rule turns an upgrade into a startup failure for every operator whose config
/// arrived at `0644` — which is the default outcome of a `umask 022` host and so
/// is the *common* case, not the exceptional one. Refusing would break far more
/// working setups than the finding is worth, and an operator who reads the
/// warning fixes it with one `chmod`.
///
/// `Scope::Project` is deliberately not checked. `io.toml` is committed and
/// arrives from a `git clone` world-readable by design; warning on it every time
/// is what teaches an operator to ignore the warning that matters.
///
/// Unix only. Windows expresses this with ACLs, which have no mode to compare and
/// no `chmod` to recommend, so there is nothing here that would be true there.
#[cfg(unix)]
fn warn_if_exposed(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    let mode = meta.permissions().mode() & 0o777;
    // The same mask `ssh` uses: any group or other bit at all, not the read bits
    // alone. A file another account may *write* is a file whose credential that
    // account chooses next time.
    if mode & 0o077 != 0 {
        tracing::warn!(
            "{}: mode {mode:04o} — this file is accessible to accounts other than its owner, \
             and a credential in it is readable by every one of them. Run `chmod 600 {}`.",
            path.display(),
            path.display()
        );
    }
}

/// Windows has no mode to compare: see the unix half.
#[cfg(not(unix))]
fn warn_if_exposed(_path: &Path) {}

/// Read one scope: parse it, substitute against its own directory, and validate
/// it on its own so an error can name the file it came from.
fn read_scope(scope: Scope, path: &Path) -> Result<toml::value::Table> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
    if scope != Scope::Project {
        warn_if_exposed(path);
    }
    let table = parse(scope, &text, path)?;
    // 0.74.0 — `Scope::Local` is held to this too. `io.local.toml` is the
    // operator's own file in intent; in fact it is a path in the workspace root,
    // and a run's agent writes paths in the workspace root. One `write_file` of an
    // unremarkable name declared a `[[hook]]`, an `[[mcp]]` command or a
    // `[[provider]]` endpoint that the next `Config::discover` would act on,
    // outside the `Policy` and outside the sandbox, and nothing about that write
    // looked like an escalation.
    //
    // Only [`Scope::User`] is exempt, and the exemption is the whole reason the
    // scope exists: `$IO_CONFIG`, `$IO_CONFIG_HOME` and `~/.config/io` are outside
    // every workspace, so a run that can write its own root cannot reach them.
    if scope != Scope::User {
        refuse_widening(scope, &table, path)?;
    }
    // Validated here and discarded: the value that is kept is the merged one,
    // but an error found now can name this file rather than "<merged>".
    let file: File = deserialize(toml::Value::Table(table.clone()), path)?;
    refuse_nested_profiles(&file, path)?;
    crate::hooks::Hooks::check(&file.hook, path)?;
    check_providers(&file.provider)?;
    // Against the raw table, not `file`: `[[mcp]]` is the one section exempt from
    // `deny_unknown_fields`, so by the time it has deserialized the misspelling
    // is already gone (0.70.0).
    crate::mcp::check_enabled_spelling(&table, path)?;
    Ok(table)
}

/// The `[[plugin]]` entries one scope's own table declares (0.35.0).
///
/// Read from the scope's table rather than the merged one, because which file
/// declared a bundle decides what that bundle may contribute.
fn declared_plugins(
    table: &toml::value::Table,
    path: &Path,
) -> Result<Vec<crate::plugin::Declaration>> {
    let Some(value) = table.get("plugin") else {
        return Ok(Vec::new());
    };
    value.clone().try_into().map_err(|e: toml::de::Error| {
        Error::Config(format!("{}: key `plugin`: {}", path.display(), e.message()))
    })
}

/// Parse and substitute, in that order — a substitution is a value, not syntax.
///
/// `scope` reaches all the way down to [`expand`] because two substitutions —
/// `${cmd:...}`, which runs a program, and `${file:...}` (0.74.0), whose argument
/// is joined onto the file's directory and so names any path when written absolute
/// — are refused in every file inside the workspace. `io.toml` arrives with a
/// `git clone`; `io.local.toml` is a path the run's own agent can write. Both are
/// therefore somewhere a program to run may not be named, and `${cmd:}` runs its
/// one here, during parsing, before any `Policy` or sandbox exists.
pub(crate) fn parse(scope: Scope, text: &str, path: &Path) -> Result<toml::value::Table> {
    let mut table: toml::value::Table = toml::from_str(text)
        .map_err(|e| Error::Config(format!("{}: {}", path.display(), e.message())))?;
    let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let mut key = Vec::new();
    for (k, v) in table.iter_mut() {
        key.push(k.clone());
        substitute(scope, v, &dir, &mut key, path)?;
        key.pop();
    }
    Ok(table)
}

/// The keys a file inside a workspace may not set to the value that *widens* a
/// boundary, paired with that value (0.27.0).
///
/// `io.toml` is committed and arrives with a `git clone`; since 0.74.0 the same
/// rule covers `io.local.toml`, which a run's own agent can write. These five are
/// the keys that turn reading a file nobody vetted into a risk: two that default
/// an act to `allow`, one that re-opens egress inside the sandbox, one that
/// switches the portable floor off, and one that hands the host's own privileges
/// over. The *narrowing* value of each is still legal in both files, because a
/// project file denying `exec` is exactly what the scope is for.
///
/// Where [`REFUSED_SECTIONS`] refuses a section outright, these are keys with a
/// widening *value* and a narrowing one, so the value decides. Both rules run in
/// [`refuse_widening`].
const PROJECT_WIDENING: &[(&[&str], &str)] = &[
    (&["policy", "defaults", "exec"], "allow"),
    (&["policy", "defaults", "net"], "allow"),
    (&["sandbox", "allow_network"], "true"),
    (&["sandbox", "force_floor"], "false"),
    // 0.46.0 — `full-access` is the widest grant the crate makes, so a repository
    // may not hand it to whoever clones it. The narrowing values (`read-only`,
    // and `workspace-write` where the caller asked for less) stay legal at
    // project scope, which is what the scope is for.
    (&["sandbox", "mode"], "full-access"),
];

/// The sections a file inside a workspace may not declare at all, each with the
/// noun a refusal names it by and the reason it is on the list (0.28.0, 0.53.0,
/// 0.74.0).
///
/// **The rule, written once: anything that names a program to run, or names an
/// endpoint a credential is sent to, is refused at project scope, without
/// exception.** This list is that sentence's implementation and its only one. A
/// table added to the file format later belongs here because the sentence covers
/// it — not because someone happened to think of it, and not after an exploit
/// demonstrates the omission. A section that names neither a program nor a
/// credentialled endpoint stays legal, which is what keeps the scope worth
/// having: `[policy]`, `[[agent]]`, `[run]`, `[[plugin]]` and the rest still let a
/// repository narrow a boundary from its own committed file.
///
/// Refusing the **section** rather than the hazardous key inside it is the 0.28.0
/// argument about `[[hook]]`'s `run`/`append` pair, generalised: a rule that
/// permits half a table is a rule a reader has to hold two halves of, and the next
/// key added to that table lands on the permitted side by default. `[[provider]]`
/// is refused whole for exactly that reason rather than `base_url` and `api_key`
/// being refused individually.
///
/// `browser` is on the list in every build. The `[browser]` field only exists
/// with the feature, but [`refuse_widening`] runs against the raw table before
/// anything deserializes, and a boundary that moved with a feature flag would be
/// one an operator could not state.
const REFUSED_SECTIONS: &[(&str, &str, &str)] = &[
    // 0.28.0. The whole array, not its executing half: `run` is the `${cmd:}`
    // primitive by another name, and `append` is a write to a path the file chose,
    // which is the same hazard by a shorter route. Refusing one and allowing the
    // other would be a rule a reader has to hold two halves of.
    (
        "hook",
        "hooks",
        "a hook runs an argv on this machine, or appends to a path the file itself \
         chose, on an event the file itself picks",
    ),
    // 0.53.0. Same hazard, same answer: `[browser]` names a binary to execute, and
    // the run then drives it against whatever the page contains. A repository that
    // could choose the browser could choose the program.
    (
        "browser",
        "a browser",
        "a browser is a program on this machine, which the run then drives against \
         whatever a page contains",
    ),
    // 0.74.0. `base_url` redirects every completion of the run, and `api_key` —
    // through `${env:}` or `${file:}` — decides which of this host's secrets is
    // sent as the `Authorization` header of that redirected request. The endpoint
    // is contacted before the run's first step, so nothing the policy does later
    // is in front of it.
    (
        "provider",
        "providers",
        "a provider names the endpoint this run's credential is sent to, and it is \
         contacted before the run's first step",
    ),
    // 0.74.0. Both name a command, an argv and an environment, and both are
    // spawned at run start. The spawn gate is an `Act::Exec` check on the binary
    // name alone, so `command = "node"` in a repository that legitimately allows
    // `node` is arbitrary execution with the argument doing the work. `plugin.rs`
    // has refused precisely this for a project-scoped *plugin* since 0.35.0; these
    // two are the same declaration reached without a bundle around it.
    (
        "mcp",
        "MCP servers",
        "an MCP server is a command, an argv and an environment that this process \
         spawns at run start",
    ),
    (
        "lsp",
        "language servers",
        "a language server is a command, an argv and an environment that this process \
         spawns at run start",
    ),
];

/// How a refusal names a scope, and why that scope is not trusted to widen.
///
/// One fragment pair per scope, so every rule in [`refuse_widening`] produces a
/// message with the same shape and an operator reading two of them is reading one
/// rule. [`Scope::User`] never reaches here — it is the scope the refusals point
/// *to* — and is worded as the project scope rather than given a fourth spelling
/// nothing can print.
fn untrusted(scope: Scope) -> (&'static str, &'static str) {
    match scope {
        Scope::Local => (
            "a workspace-root `io.local.toml`",
            "it sits in the workspace root a run's own agent can write to",
        ),
        Scope::Project | Scope::User => (
            "a project-scoped file",
            "`io.toml` arrives with a `git clone`",
        ),
    }
}

/// Validate every `[[provider]]` entry, reporting the index of the one at fault
/// (0.29.0).
///
/// The index rather than the name, because an entry that named nothing usable is
/// exactly the entry with no name to quote — the shape `[[hook]]` already uses
/// for its own exactly-one rule.
fn check_providers(providers: &[ProviderSpec]) -> Result<()> {
    for (index, spec) in providers.iter().enumerate() {
        spec.validate(index)?;
    }
    Ok(())
}

/// A file inside a workspace may narrow the boundary and may never widen it.
///
/// Three rules, and [`REFUSED_SECTIONS`] states the first of them as a sentence:
/// no section that names a program to run or an endpoint a credential is sent to,
/// no [`PROJECT_WIDENING`] key set to its widening value, and no `run.skills` or
/// `run.templates` naming a directory outside the workspace root (0.74.0) — that
/// third one is a *read* rather than an act, and it is here because the directory
/// it names is composed into the model's system prompt on every turn.
///
/// Held against the project scope since 0.27.0 and against `io.local.toml` since
/// 0.74.0. `io.toml` arrives with a `git clone`; `io.local.toml` is the operator's
/// own file in intent, and in fact is a path inside the workspace root the run's
/// agent can write to, so a single `write_file` of it declared an argv the next
/// [`Config::discover`] would run — outside the [`Policy`] and outside the
/// sandbox. The user scope is the one file no workspace can reach, so it is the
/// one still trusted to widen and the one every refusal here points at.
///
/// What this does **not** claim: that a cloned repository is safe. `[toolchain]`
/// still names an argv, and the boundary against the agent is still the [`Policy`]
/// the caller loaded.
///
/// Profile bodies are checked too. A widening key hidden in `[profile.x.sandbox]`
/// would otherwise reach the same place by a different path.
fn refuse_widening(scope: Scope, table: &toml::value::Table, path: &Path) -> Result<()> {
    let (who, because) = untrusted(scope);
    for (key, plural, why) in REFUSED_SECTIONS {
        if table.contains_key(*key) {
            return Err(Error::Config(format!(
                "{}: key `{key}`: {who} may not declare {plural} — {why} — and {because}. \
                 Write it in {USER_SCOPE} instead.",
                path.display()
            )));
        }
    }
    for (keys, widening) in PROJECT_WIDENING {
        let mut node = table.get(keys[0]);
        for key in &keys[1..] {
            node = node
                .and_then(toml::Value::as_table)
                .and_then(|t| t.get(*key));
        }
        let Some(value) = node else { continue };
        let written = match value {
            toml::Value::String(s) => s.as_str(),
            toml::Value::Boolean(true) => "true",
            toml::Value::Boolean(false) => "false",
            _ => continue,
        };
        if written == *widening {
            return Err(Error::Config(format!(
                "{}: key `{}`: `{written}` widens the boundary, and {who} may narrow it and \
                 never widen it, because {because}. Write it in {USER_SCOPE} instead.",
                path.display(),
                keys.join(".")
            )));
        }
    }
    // 0.74.0, audit L13. `run.skills` and `run.templates` name directories whose
    // `*.md` frontmatter is composed into the system prompt of every turn, and
    // both are resolved by joining onto the discovery root — where an absolute
    // value replaces that root outright and a relative one climbs out of it with
    // `..`. A cloned `io.toml` saying `skills = "/home/you/.ssh"` therefore put
    // this host's files into the model's context on the first turn, read-only and
    // unasked, before any `Policy` existed to have an opinion about the read.
    //
    // Refused only in the scopes a workspace can supply. The user scope still
    // points wherever the operator wants, which is what keeps a shared skills
    // directory kept outside the project working.
    //
    // Lexical, like `plugin.rs`'s `[[bin]]` rule and for the same reason: nothing
    // on this path touches the filesystem, and a directory that has not been
    // checked out yet is not a config error. `Component` rather than a string
    // test, so a Windows prefix (`C:\`), a bare root (`/etc`, which
    // `Path::is_absolute` calls relative on Windows) and a `..` buried mid-path
    // are one rule.
    for key in ["skills", "templates"] {
        let Some(written) = table
            .get("run")
            .and_then(toml::Value::as_table)
            .and_then(|t| t.get(key))
            .and_then(toml::Value::as_str)
        else {
            continue;
        };
        let wrong = Path::new(written).components().find_map(|c| match c {
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                Some("is an absolute path")
            }
            std::path::Component::ParentDir => Some("climbs out of the workspace root with `..`"),
            _ => None,
        });
        if let Some(wrong) = wrong {
            return Err(Error::Config(format!(
                "{}: key `run.{key}`: `{written}` {wrong}, and {who} may narrow the boundary and \
                 never widen it, because {because}. Every `*.md` under that directory reaches the \
                 model's system prompt. Write it relative to the workspace root, or name it from \
                 {USER_SCOPE} instead.",
                path.display()
            )));
        }
    }
    if let Some(profiles) = table.get("profile").and_then(toml::Value::as_table) {
        for body in profiles.values().filter_map(toml::Value::as_table) {
            refuse_widening(scope, body, path)?;
        }
    }
    Ok(())
}

/// A profile is an overlay, not a tree. Nesting one is rejected rather than ignored,
/// for the same reason an unknown key is: a section that silently does nothing is a
/// setting an operator believes in.
fn refuse_nested_profiles(file: &File, path: &Path) -> Result<()> {
    for (name, body) in &file.profile {
        if !body.profile.is_empty() {
            return Err(Error::Config(format!(
                "{}: key `profile.{name}.profile`: a profile may not contain profiles",
                path.display()
            )));
        }
    }
    Ok(())
}

/// Read what `[instructions]` names, relative to the discovery root, and word each
/// as a constraint.
///
/// A named file that does not exist is skipped, and one that holds only whitespace is
/// skipped: this is discovery, not substitution, and the "resolve or fail" rule that
/// governs `${...}` deliberately does not apply. The file name rides in the text so a
/// reader of the constraint — or of the trace — can see where it came from.
fn read_instructions(file: &File, root: &Path) -> Result<Vec<String>> {
    // 0.45.0 — an absent `[instructions]` table now means the defaults rather than
    // nothing. `AGENTS.md` has been the default name since 0.27.0, but discovery ran
    // only where the table was present, so a repository carrying the file every other
    // agent reads and no `io.toml` at all was read by none of it. An explicit
    // `files = []` is how a project says no, and it is distinct from an absent table
    // — the same distinction `Option<Vec<_>>` already carried and nothing read.
    let names = match file.instructions.as_ref().and_then(|s| s.files.as_ref()) {
        Some(files) => files.clone(),
        None => DEFAULT_INSTRUCTIONS.iter().map(PathBuf::from).collect(),
    };
    let mut out = Vec::new();
    for name in names {
        let at = root.join(&name);
        if !at.is_file() {
            continue;
        }
        let text = std::fs::read_to_string(&at)
            .map_err(|e| Error::Config(format!("{}: {e}", at.display())))?;
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        out.push(format!(
            "Project instructions from `{}`:\n{text}",
            name.display()
        ));
    }
    Ok(out)
}

fn deserialize(value: toml::Value, path: &Path) -> Result<File> {
    value
        .try_into()
        .map_err(|e: toml::de::Error| Error::Config(format!("{}: {}", path.display(), e.message())))
}

/// Expand `${env:...}`, `${file:...}` and `${cmd:...}` in every string this value
/// contains.
fn substitute(
    scope: Scope,
    value: &mut toml::Value,
    dir: &Path,
    key: &mut Vec<String>,
    path: &Path,
) -> Result<()> {
    match value {
        toml::Value::String(s) => *s = expand(scope, s, dir, key, path)?,
        toml::Value::Array(items) => {
            for (i, item) in items.iter_mut().enumerate() {
                key.push(format!("[{i}]"));
                substitute(scope, item, dir, key, path)?;
                key.pop();
            }
        }
        toml::Value::Table(table) => {
            for (k, v) in table.iter_mut() {
                key.push(k.clone());
                substitute(scope, v, dir, key, path)?;
                key.pop();
            }
        }
        _ => {}
    }
    Ok(())
}

/// Run `argv` with no shell and return its trimmed stdout.
///
/// Split on whitespace, `argv[0]` is the program: there is no shell between the
/// string and the process, so a value carrying `;` or `|` or a backtick is an
/// argument rather than a second command. A non-zero exit is a failure, because a
/// credential helper that failed did not produce a credential.
fn run_command(argv: &str) -> std::result::Result<String, String> {
    let mut parts = argv.split_whitespace();
    let Some(program) = parts.next() else {
        return Err("`${cmd:}` names no program".to_string());
    };
    let output = std::process::Command::new(program)
        .args(parts)
        .output()
        .map_err(|e| format!("cannot run `{program}`: {e}"))?;
    if !output.status.success() {
        return Err(format!("`{program}` exited with {}", output.status));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// One string's worth of substitution.
///
/// Every failure is an error naming the key. None of them is an empty string: a
/// config that silently disarms itself is the worst outcome this feature can
/// produce.
fn expand(scope: Scope, raw: &str, dir: &Path, key: &[String], path: &Path) -> Result<String> {
    let mut out = String::new();
    let mut rest = raw;
    while let Some(at) = rest.find("${") {
        out.push_str(&rest[..at]);
        let after = &rest[at + 2..];
        let Some(end) = after.find('}') else {
            return Err(bad_key(path, key, "unterminated `${` substitution"));
        };
        let inner = &after[..end];
        let Some((kind, arg)) = inner.split_once(':') else {
            return Err(bad_key(
                path,
                key,
                format!("`${{{inner}}}` names neither `env:` nor `file:`"),
            ));
        };
        let value = match kind {
            "env" => std::env::var(arg).map_err(|_| {
                bad_key(
                    path,
                    key,
                    format!("environment variable `{arg}` is not set"),
                )
            })?,
            // 0.74.0. Refused in the project scope for the reason `${cmd:}` is,
            // one step short of running a program: the argument is joined onto the
            // file's own directory, and `Path::join` lets an absolute argument
            // replace that directory outright while a relative one climbs out of
            // it with `..`. So `api_key = "${file:/home/you/.ssh/id_rsa}"` in a
            // cloned `io.toml` is an arbitrary read of this host, resolved at load
            // and then sent wherever the file's other keys point. The read happens
            // before any `Policy` exists, so `Act::Read` never sees it.
            "file" => {
                // 0.74.0, audit H2 — every scope inside the workspace, not just
                // the committed one. `io.local.toml` sits at the workspace root,
                // which is a path the run's own agent can write, so refusing this
                // for `io.toml` alone left the same arbitrary read one file over.
                if scope != Scope::User {
                    return Err(bad_key(
                        path,
                        key,
                        format!(
                            "`${{file:}}` is refused in a file inside the workspace, because \
                             `{PROJECT_FILE}` travels with a clone, `{LOCAL_FILE}` is a path this \
                             run's own agent can write, and the argument is joined onto the file's \
                             directory — an absolute one names any path on this machine. Write it \
                             in {USER_SCOPE} instead."
                        ),
                    ));
                }
                let at = dir.join(arg);
                // A `${file:}` target is a credential by construction — the
                // feature exists to keep one out of a committed file — so this
                // one is checked in every scope that reaches it.
                warn_if_exposed(&at);
                std::fs::read_to_string(&at)
                    .map_err(|e| {
                        bad_key(path, key, format!("cannot read `{}`: {e}", at.display()))
                    })?
                    .trim()
                    .to_string()
            }
            // 0.27.0. Parsing has never run anything before, and `io.toml` is the
            // file a `git clone` delivers — so the one scope that cannot use this is
            // the one an operator did not write.
            "cmd" => {
                // 0.74.0, audit H2 — and this one is the sharpest edge of that
                // finding rather than a tidy-up beside it. `${cmd:}` runs a
                // program at load: outside the `Policy`, outside the sandbox,
                // before the run has a first step. Refusing it for `io.toml`
                // alone left the whole of H2 open by a shorter route than the one
                // the finding describes — the agent writes `io.local.toml` with
                // any key at all carrying a `${cmd:}`, and the next
                // `Config::discover` runs it. No `[[hook]]` needed, and `[app]`
                // takes arbitrary keys.
                if scope != Scope::User {
                    return Err(bad_key(
                        path,
                        key,
                        format!(
                            "`${{cmd:}}` is refused in a file inside the workspace: it runs a \
                             program at load, before any policy or sandbox exists. `{PROJECT_FILE}` \
                             travels with a clone and `{LOCAL_FILE}` is a path this run's own agent \
                             can write. Write it in {USER_SCOPE} instead."
                        ),
                    ));
                }
                run_command(arg).map_err(|e| bad_key(path, key, e))?
            }
            other => {
                return Err(bad_key(
                    path,
                    key,
                    format!("`{other}:` is not a substitution this crate knows"),
                ))
            }
        };
        if value.is_empty() {
            return Err(bad_key(
                path,
                key,
                format!("`${{{inner}}}` resolved to nothing, and an empty value is never what a config meant"),
            ));
        }
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn bad_key(path: &Path, key: &[String], why: impl std::fmt::Display) -> Error {
    Error::Config(format!(
        "{}: key `{}`: {why}",
        path.display(),
        key.join(".")
    ))
}

/// The keys whose arrays *append* across scopes instead of being replaced.
///
/// Three, and each is a set every scope contributes to rather than a value one
/// scope owns: a later scope adds a policy layer, an agent or a plugin, it does
/// not rewrite the boundary, the roster or the bundle list. Everything else
/// replaces, because a half-merged MCP server definition is not a server.
const APPENDING: &[&[&str]] = &[&["policy", "layers"], &["agent"], &["plugin"]];

/// Keys a project-scoped file may only *lower* (0.55.0).
///
/// The scope rule stated in the module doc — a project file may narrow and may
/// never widen — has until now been enforced by refusing the widening *value* of
/// a key that has one (`exec = "allow"`, `force_floor = false`). A number has no
/// such value: whether `max_read_chars = 400000` widens depends on what the
/// scope below it said. So for these keys the lower number wins instead of the
/// later scope, and `io.toml` can tighten an operator's ceiling without being
/// able to loosen it.
///
/// Only the project scope is held to this, and 0.74.0 leaves that alone while
/// [`refuse_widening`] grows to cover `io.local.toml`. The two rules answer
/// different questions: a number a workspace file lowers is a ceiling an operator
/// still chose, where a section a workspace file declares is a program an operator
/// never saw. `io.local.toml` and the user scope set these three outright.
const NARROWING: &[&[&str]] = &[
    &["run", "max_read_chars"],
    // 0.60.0 — the ceiling on a blocking mailbox read. A number, like
    // `max_read_chars`, so it cannot be refused by its value the way `exec =
    // "allow"` is: the lower of the two wins when the incoming scope is `Project`.
    &["run", "max_wait_secs"],
    // 0.56.0 — the three memory caps. All three, not one representative: a rule
    // that covered two of them would be a boundary that depends on which cap a
    // repository chose to argue about.
    &["memory", "max_entries"],
    &["memory", "max_chars"],
    &["memory", "max_entry_chars"],
];

/// Record `origin` against every leaf key of `table`, walking it the way
/// [`merge`] walks it so the two cannot disagree about what a leaf is (0.30.0).
///
/// A table recurses; anything else — scalar, array, array of tables — is a leaf,
/// because that is exactly the granularity the merge replaces at. The one
/// exception is the [`APPENDING`] keys, where a later scope adds to the array
/// instead of replacing it, so the origin is pushed rather than replacing what is
/// there: `policy.layers` set in two files was genuinely built by two files.
///
/// Substitution needs no handling here and that is the point: `${env:}` and
/// `${cmd:}` are resolved by [`parse`] before this ever sees the table, so a
/// substituted value's origin is the file the substitution was written in without
/// anything having to arrange it.
fn record_origins(
    table: &toml::value::Table,
    at: &mut Vec<String>,
    origin: &Origin,
    into: &mut BTreeMap<String, Vec<Origin>>,
) {
    for (key, value) in table {
        at.push(key.clone());
        match value {
            toml::Value::Table(inner) => record_origins(inner, at, origin, into),
            _ => {
                let path = at.join(".");
                let appends = APPENDING.iter().any(|p| p == &at.as_slice());
                let entry = into.entry(path).or_default();
                if !appends {
                    entry.clear();
                }
                entry.push(origin.clone());
            }
        }
        at.pop();
    }
}

/// Deep-merge `over` onto `base`, later winning key by key.
///
/// `narrowing` is set when `over` is the project scope, where a [`NARROWING`]
/// key takes the lower of the two numbers instead of the later one (0.55.0).
fn merge(
    base: &mut toml::value::Table,
    over: toml::value::Table,
    at: &mut Vec<String>,
    narrowing: bool,
) {
    for (key, value) in over {
        at.push(key.clone());
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => merge(b, o, at, narrowing),
            (Some(toml::Value::Array(b)), toml::Value::Array(o))
                if APPENDING.iter().any(|p| p == &at.as_slice()) =>
            {
                b.extend(o);
            }
            (Some(toml::Value::Integer(b)), toml::Value::Integer(o))
                if narrowing && NARROWING.iter().any(|p| p == &at.as_slice()) =>
            {
                *b = (*b).min(o);
            }
            (_, value) => {
                base.insert(key.clone(), value);
            }
        }
        at.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(text: &str) -> toml::value::Table {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn a_later_scope_wins_one_key_and_leaves_its_siblings() {
        let mut base = table("[sandbox.limits]\nmax_wall_secs = 120\nmax_cpu_secs = 60\n");
        merge(
            &mut base,
            table("[sandbox.limits]\nmax_wall_secs = 5\n"),
            &mut Vec::new(),
            false,
        );
        let limits = base["sandbox"]["limits"].as_table().unwrap();
        assert_eq!(limits["max_wall_secs"].as_integer(), Some(5));
        assert_eq!(
            limits["max_cpu_secs"].as_integer(),
            Some(60),
            "a sibling key the later scope never named is not disturbed"
        );
    }

    #[test]
    fn policy_layers_append_and_every_other_array_replaces() {
        let mut base = table(
            "[[policy.layers]]\nname = \"ops\"\nrules = []\n\
             [toolchain.cargo]\ntest = [\"cargo\", \"test\"]\n",
        );
        merge(
            &mut base,
            table(
                "[[policy.layers]]\nname = \"mine\"\nrules = []\n\
                 [toolchain.cargo]\ntest = [\"cargo\", \"nextest\", \"run\"]\n",
            ),
            &mut Vec::new(),
            false,
        );
        let layers = base["policy"]["layers"].as_array().unwrap();
        assert_eq!(layers.len(), 2, "layers append");
        assert_eq!(layers[0]["name"].as_str(), Some("ops"));
        assert_eq!(layers[1]["name"].as_str(), Some("mine"));
        let test = base["toolchain"]["cargo"]["test"].as_array().unwrap();
        assert_eq!(test.len(), 3, "an ordinary array is replaced whole");
    }

    #[test]
    fn substitution_resolves_or_fails_and_never_empties() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("token"), "  s3cret\n").unwrap();
        std::env::set_var("IO_HARNESS_TEST_SET", "from-the-environment");
        std::env::set_var("IO_HARNESS_TEST_EMPTY", "");

        let key = ["run".to_string()];
        let path = Path::new("io.toml");
        // The user scope, because since 0.74.0 it is the only one `${file:}`
        // resolves in at all. The claim here is about what a substitution turns
        // into — a value, or an error naming the key, and never an empty string —
        // not about which file may carry one, so it is asserted in the scope where
        // every form still runs.
        let at = Scope::User;
        assert_eq!(
            expand(at, "${env:IO_HARNESS_TEST_SET}", dir.path(), &key, path).unwrap(),
            "from-the-environment"
        );
        assert_eq!(
            expand(at, "Bearer ${file:token}", dir.path(), &key, path).unwrap(),
            "Bearer s3cret",
            "a file's value is trimmed, and substitution is inside a larger string"
        );
        for (input, expect) in [
            ("${env:IO_HARNESS_TEST_UNSET}", "is not set"),
            ("${env:IO_HARNESS_TEST_EMPTY}", "resolved to nothing"),
            ("${file:absent}", "cannot read"),
            ("${nope:x}", "not a substitution"),
            ("${env:X", "unterminated"),
        ] {
            let err = expand(at, input, dir.path(), &key, path)
                .unwrap_err()
                .to_string();
            assert!(err.contains(expect), "{input}: {err}");
            assert!(err.contains("key `run`"), "the error names the key: {err}");
        }
        // The negative control for the whole set: a string with no substitution
        // in it is returned unchanged rather than being parsed at all.
        assert_eq!(
            expand(at, "plain $HOME value", dir.path(), &key, path).unwrap(),
            "plain $HOME value"
        );
    }

    /// 0.74.0, audit H2 — the boundary is the workspace, not the committed file.
    ///
    /// `io.local.toml` sits at the workspace root, which is a path the run's own
    /// agent writes to, so a rule that refused `${cmd:}` in `io.toml` alone left the
    /// same load-time execution one file over. Both workspace scopes are asserted
    /// here, and `${file:}` beside `${cmd:}`, because they are one rule with two
    /// arms and an arm with no assertion is an arm nothing holds.
    #[test]
    fn a_command_substitution_runs_in_the_user_scope_and_never_in_a_workspace_file() {
        // Each platform's own spelling of "print this", "succeed silently" and
        // "fail". Named rather than skipped: `${cmd:}` is a real feature on Windows
        // and a test that ran on two platforms out of three would prove it there.
        #[cfg(windows)]
        let (echo, quiet, fail) = ("cmd /c echo s3cret", "cmd /c rem", "cmd /c exit 1");
        #[cfg(not(windows))]
        let (echo, quiet, fail) = ("printf s3cret", "true", "false");

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("token"), "s3cret\n").unwrap();
        let key = ["mcp".to_string()];
        // The user-scope file, which is the only one either of these runs in now.
        let path = Path::new("io.toml");

        // A trailing newline is trimmed, and the value composes inside a larger string.
        assert_eq!(
            expand(
                Scope::User,
                &format!("Bearer ${{cmd:{echo}}}"),
                dir.path(),
                &key,
                path
            )
            .unwrap(),
            "Bearer s3cret"
        );

        // The three ways a helper fails, each named separately.
        for (input, expect) in [
            (format!("${{cmd:{fail}}}"), "exited with"),
            (
                "${cmd:io-harness-no-such-program}".to_string(),
                "cannot run",
            ),
            (format!("${{cmd:{quiet}}}"), "resolved to nothing"),
        ] {
            let input = input.as_str();
            let err = expand(Scope::User, input, dir.path(), &key, path)
                .unwrap_err()
                .to_string();
            assert!(err.contains(expect), "{input}: {err}");
        }

        // Both files inside the workspace refuse both substitutions outright, before
        // running or reading anything. The success above is what proves the refusal
        // is the scope and not a `${cmd:}` that stopped working.
        for (scope, file) in [(Scope::Project, PROJECT_FILE), (Scope::Local, LOCAL_FILE)] {
            for (input, needle) in [
                (format!("${{cmd:{echo}}}"), "`${cmd:}`"),
                ("${file:token}".to_string(), "`${file:}`"),
            ] {
                let err = expand(scope, &input, dir.path(), &key, Path::new(file))
                    .unwrap_err()
                    .to_string();
                assert!(
                    err.contains(&format!(
                        "{needle} is refused in a file inside the workspace"
                    )),
                    "{file}: {err}"
                );
                assert!(
                    err.contains(USER_SCOPE),
                    "the error says where to write it instead: {err}"
                );
            }

            // The negative control: it is `cmd:` and `file:` that a workspace file
            // refuses, not substitution. A rule that disarmed `${env:}` there would
            // be a much worse feature that this test would otherwise pass.
            std::env::set_var("IO_HARNESS_TEST_SET", "from-the-environment");
            assert_eq!(
                expand(
                    scope,
                    "${env:IO_HARNESS_TEST_SET}",
                    dir.path(),
                    &key,
                    Path::new(file),
                )
                .unwrap(),
                "from-the-environment"
            );
        }
    }
}
