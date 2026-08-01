//! One config file, layered, projected onto the typed API (0.19.0).
//!
//! # What this is
//!
//! An operator writes `io.toml` and gets a permission boundary, sandbox caps,
//! run budgets, per-ecosystem toolchain commands, MCP servers, a price table and
//! — since 0.27.0 — which provider and model to run with what standing behind it,
//! without compiling anything. io-cli and io-studio read the same file rather
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
//! **A project-scoped file may narrow the boundary and may never widen it**
//! (0.27.0). `io.toml` arrives with a `git clone`, so four keys are refused there
//! when the value written is the widening one — `policy.defaults.exec = "allow"`,
//! `policy.defaults.net = "allow"`, `sandbox.allow_network = true`,
//! `sandbox.force_floor = false` — and so is `${cmd:}` anywhere in that file. Each
//! is accepted in `io.local.toml` and in the user scope. This does not claim that a
//! cloned repository is safe: `[[mcp]]` still names a command and the boundary
//! against the agent is still the [`Policy`] the caller loaded.
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

// ---------------------------------------------------------------------------
// The file format
// ---------------------------------------------------------------------------

/// The whole file, every section optional.
///
/// `deny_unknown_fields` is on every section: an unknown key is the failure this
/// module exists to make loud.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    policy: Option<PolicySection>,
    #[serde(default)]
    sandbox: Option<SandboxSection>,
    #[serde(default)]
    run: Option<RunSection>,
    #[serde(default)]
    toolchain: BTreeMap<String, ToolchainSection>,
    #[serde(default)]
    prices: Option<PricesSection>,
    // `McpServer` is `#[serde(flatten)]`-based and serde refuses `flatten`
    // beside `deny_unknown_fields`, so an unknown key *inside* one of these
    // tables is not rejected. Stated in the guide rather than papered over.
    #[serde(default)]
    mcp: Vec<McpServer>,
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
    #[serde(default)]
    provider: Vec<ProviderSpec>,
    // 0.27.0. The one section this crate stores and never validates, so io-cli and
    // io-studio keep their own settings in the same file. A `toml::value::Table`
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
}

/// Which provider a run uses, as a value a configuration can carry (0.27.0).
///
/// A **spec**, never a constructed provider: [`Provider::complete`](crate::Provider::complete)
/// returns `impl Future`, so the trait is not dyn-compatible and there is no
/// `Box<dyn Provider>` for an accessor to return. The application reads the spec and
/// builds from it, which is three lines and keeps every entry point generic.
///
/// ```
/// use io_harness::{Config, ProviderSpec};
///
/// let config = Config::from_toml(r#"
///     [[provider]]
///     kind = "openrouter"
///     model = "anthropic/claude-sonnet-4"
///
///     [[provider]]
///     kind = "anthropic"
///     model = "claude-sonnet-4"
/// "#).unwrap();
///
/// // The first entry is the provider; the rest are the chain behind it, in order.
/// let ProviderSpec::OpenRouter { model, api_key } = config.provider_spec().unwrap() else {
///     panic!("the file named openrouter");
/// };
/// assert_eq!(model, "anthropic/claude-sonnet-4");
/// assert_eq!(*api_key, None, "no key written means the provider's own environment variable");
/// assert_eq!(config.fallback_specs().len(), 1);
/// ```
///
/// It is `#[non_exhaustive]` from the first release it exists, because a later one
/// adds a variant: a consumer matching it needs a `_ =>` arm, and paying that once
/// now is what keeps the addition from being a break.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// let config = Config::from_toml(r#"
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
    /// "#).unwrap();
    ///
    /// let ProviderSpec::Compatible { preset, model, .. } = config.provider_spec().unwrap() else {
    ///     panic!("the file named a compatible provider");
    /// };
    /// assert_eq!(preset.as_deref(), Some("groq"));
    /// assert_eq!(model, "llama-3.3-70b-versatile");
    ///
    /// // The fallback is a model on this machine, which costs nothing to run.
    /// assert_eq!(config.fallback_specs().len(), 1);
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
    commit_identity: Option<Identity>,
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
#[derive(Debug, Clone, Default)]
pub struct Config {
    file: File,
    sources: Vec<(Scope, PathBuf)>,
    /// The merged table the typed `file` was deserialized from, kept so
    /// [`Config::with_profile`] can overlay through the same [`merge`] the scopes
    /// use rather than inventing a second set of merge semantics.
    raw: toml::value::Table,
    /// What `[instructions]` found, already worded as constraints. Read once, in
    /// [`Config::discover`], which is the caller's own call — the run loop never
    /// reads a file.
    instructions: Vec<String>,
    /// The root a `[[hook]]`'s relative `append` path resolves against (0.28.0).
    ///
    /// The *discovery root*, not the declaring file's directory, and the two are the
    /// same thing for a local-scope file. It matters for a user-scope one, where an
    /// operator writing `append = "audit.jsonl"` means the project they are pointing
    /// the harness at rather than their own home directory.
    dir: PathBuf,
}

impl Config {
    /// Read every scope that exists for `root` and merge them.
    ///
    /// A scope whose file is absent is skipped; a workspace with no files at all
    /// is not an error, it is the crate's defaults.
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
        for (scope, path) in candidates {
            if !path.is_file() {
                continue;
            }
            let table = read_scope(scope, &path)?;
            merge(&mut merged, table, &mut Vec::new());
            sources.push((scope, path));
        }

        let file: File = deserialize(toml::Value::Table(merged.clone()), Path::new("<merged>"))?;
        let instructions = read_instructions(&file, root)?;
        Ok(Self {
            file,
            sources,
            raw: merged,
            instructions,
            dir: root.to_path_buf(),
        })
    }

    /// Parse one config from text, as the project scope.
    ///
    /// For a caller that holds its configuration somewhere this crate does not
    /// know about, and for tests. `${file:...}` resolves against the current
    /// directory, since there is no file to resolve against.
    ///
    /// **It is the project scope**, so the 0.27.0 rules that bound that scope apply:
    /// `${cmd:...}` is refused here, and a key whose value would widen a boundary is
    /// refused here. `[instructions]` finds nothing, because there is no root to
    /// discover against.
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
    /// ```
    pub fn from_toml(text: &str) -> Result<Self> {
        let path = Path::new(PROJECT_FILE);
        let table = parse(Scope::Project, text, path)?;
        refuse_widening(&table, path)?;
        let file: File = deserialize(toml::Value::Table(table.clone()), path)?;
        refuse_nested_profiles(&file, path)?;
        crate::hooks::Hooks::check(&file.hook, path)?;
        check_providers(&file.provider)?;
        Ok(Self {
            file,
            sources: Vec::new(),
            raw: table,
            instructions: Vec::new(),
            dir: PathBuf::from("."),
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
        merge(&mut merged, overlay, &mut Vec::new());

        Ok(Self {
            file: deserialize(toml::Value::Table(merged.clone()), Path::new("<profile>"))?,
            sources: self.sources.clone(),
            raw: merged,
            instructions: self.instructions.clone(),
            dir: self.dir.clone(),
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
    /// let config = Config::from_toml(r#"
    ///     [[provider]]
    ///     kind = "anthropic"
    ///     model = "claude-sonnet-4"
    ///     api_key = "sk-written-here-or-left-out-for-ANTHROPIC_API_KEY"
    /// "#).unwrap();
    ///
    /// assert!(matches!(config.provider_spec(), Some(ProviderSpec::Anthropic { .. })));
    /// assert!(Config::from_toml("").unwrap().provider_spec().is_none());
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
    /// let config = Config::from_toml(r#"
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
    /// "#).unwrap();
    ///
    /// // Order is the configuration, not a detail of it.
    /// let models: Vec<&str> = config.fallback_specs().iter().map(|s| match s {
    ///     ProviderSpec::Anthropic { model, .. }
    ///     | ProviderSpec::OpenAi { model, .. }
    ///     | ProviderSpec::OpenRouter { model, .. } => model.as_str(),
    ///     _ => "unknown",
    /// }).collect();
    /// assert_eq!(models, ["second", "third"]);
    /// ```
    pub fn fallback_specs(&self) -> &[ProviderSpec] {
        self.file.provider.get(1..).unwrap_or_default()
    }

    /// The application's own settings under `[app.<key>]`, deserialized into the
    /// application's own type (0.27.0).
    ///
    /// This crate stores `[app]` and **never validates it**. That is the whole
    /// feature: io-cli and io-studio keep their settings in the same file without the
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

    /// What `[instructions]` discovered, worded as constraints (0.27.0).
    ///
    /// One entry per file that existed and carried text, each naming the file it came
    /// from, and applied to a contract by [`Config::apply_to`]. Empty where the file
    /// has no `[instructions]` section, where no named file exists, and always for
    /// [`Config::from_toml`], which has no root to discover against.
    ///
    /// The files are read inside [`Config::discover`] — the caller's own call, before
    /// the run — so "nothing is loaded implicitly" still holds exactly.
    ///
    /// **They are untrusted text.** A discovered `AGENTS.md` reaches the model
    /// verbatim and grants nothing: the boundary is still the [`Policy`] the caller
    /// loaded.
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
    /// ```
    /// use io_harness::Config;
    ///
    /// let config = Config::from_toml(r#"
    ///     [[mcp]]
    ///     id = "github"
    ///     transport = "stdio"
    ///     command = "github-mcp-server"
    ///     args = ["stdio"]
    /// "#).unwrap();
    ///
    /// assert_eq!(config.mcp_servers()[0].id, "github");
    /// ```
    pub fn mcp_servers(&self) -> &[McpServer] {
        &self.file.mcp
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
    /// let dir = tempfile::tempdir()?;
    /// std::fs::write(
    ///     dir.path().join("io.local.toml"),
    ///     "[[hook]]\non = [\"finished\"]\nrun = [\"cargo\", \"fmt\"]\n",
    /// )?;
    ///
    /// let config = Config::discover(dir.path())?;
    /// assert!(!config.hooks().is_empty());
    ///
    /// // The same table in the committed file is refused: `io.toml` arrives with a clone.
    /// let err = Config::from_toml("[[hook]]\nrun = [\"cargo\", \"fmt\"]\n").unwrap_err();
    /// assert!(err.to_string().contains("may not declare hooks"), "{err}");
    /// # Ok(()) }
    /// # demo().unwrap();
    /// ```
    pub fn hooks(&self) -> crate::hooks::Hooks {
        crate::hooks::Hooks::new(self.file.hook.clone(), &self.dir)
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
        // 0.27.0 — discovered project instructions land in `constraints`, the field
        // that already exists to carry "extra rules the agent must respect, surfaced
        // to the model verbatim". A new `TaskContract` field would be a break, and
        // this release carries none.
        for instruction in &self.instructions {
            out = out.with_constraint(instruction.clone());
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

/// Read one scope: parse it, substitute against its own directory, and validate
/// it on its own so an error can name the file it came from.
fn read_scope(scope: Scope, path: &Path) -> Result<toml::value::Table> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
    let table = parse(scope, &text, path)?;
    if scope == Scope::Project {
        refuse_widening(&table, path)?;
    }
    // Validated here and discarded: the value that is kept is the merged one,
    // but an error found now can name this file rather than "<merged>".
    let file: File = deserialize(toml::Value::Table(table.clone()), path)?;
    refuse_nested_profiles(&file, path)?;
    crate::hooks::Hooks::check(&file.hook, path)?;
    check_providers(&file.provider)?;
    Ok(table)
}

/// Parse and substitute, in that order — a substitution is a value, not syntax.
///
/// `scope` reaches all the way down to [`expand`] because one substitution —
/// `${cmd:...}` — is refused in the project scope, and the project scope is the one
/// a `git clone` delivers.
fn parse(scope: Scope, text: &str, path: &Path) -> Result<toml::value::Table> {
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

/// The keys a project-scoped file may not set to the value that *widens* a boundary,
/// paired with that value (0.27.0).
///
/// `io.toml` is committed and arrives with a `git clone`. These four are the keys
/// that turn reading a stranger's configuration into a risk: two that default an act
/// to `allow`, one that re-opens egress inside the sandbox, and one that switches the
/// portable floor off. The *narrowing* value of each is still legal in a project
/// file, because a project file denying `exec` is exactly what the scope is for.
const PROJECT_WIDENING: &[(&[&str], &str)] = &[
    (&["policy", "defaults", "exec"], "allow"),
    (&["policy", "defaults", "net"], "allow"),
    (&["sandbox", "allow_network"], "true"),
    (&["sandbox", "force_floor"], "false"),
];

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

/// A project-scoped file may narrow the boundary and may never widen it.
///
/// What this does **not** claim: that a cloned repository is safe. `[[mcp]]` still
/// names a command and `[toolchain]` still names an argv, and the boundary against
/// the agent is still the [`Policy`] the caller loaded. This is a specific narrowing
/// of a specific hazard — four keys, `${cmd:}` and `[[hook]]`, no more.
///
/// Profile bodies are checked too. A widening key hidden in `[profile.x.sandbox]`
/// would otherwise reach the same place by a different path.
fn refuse_widening(table: &toml::value::Table, path: &Path) -> Result<()> {
    // 0.28.0. The whole array, not its executing half: `run` is the `${cmd:}`
    // primitive by another name, and `append` is a write to a path the file chose,
    // which is the same hazard by a shorter route. Refusing one and allowing the
    // other would be a rule a reader has to hold two halves of.
    if table.contains_key("hook") {
        return Err(Error::Config(format!(
            "{}: key `hook`: a project-scoped file may not declare hooks, because a hook \
             runs or writes on this machine and `{PROJECT_FILE}` arrives with a `git clone`. \
             Write it in `{LOCAL_FILE}` or the user-scope file instead.",
            path.display()
        )));
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
                "{}: key `{}`: `{written}` widens the boundary, and a project-scoped \
                 file may narrow it and never widen it. Write it in `{LOCAL_FILE}` or \
                 the user-scope file instead.",
                path.display(),
                keys.join(".")
            )));
        }
    }
    if let Some(profiles) = table.get("profile").and_then(toml::Value::as_table) {
        for body in profiles.values().filter_map(toml::Value::as_table) {
            refuse_widening(body, path)?;
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
    let Some(section) = &file.instructions else {
        return Ok(Vec::new());
    };
    let names = match &section.files {
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
            "file" => {
                let at = dir.join(arg);
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
                if scope == Scope::Project {
                    return Err(bad_key(
                        path,
                        key,
                        format!(
                            "`${{cmd:}}` is refused in the project scope, because `{PROJECT_FILE}` \
                             travels with a clone. Write it in `{LOCAL_FILE}` or the user-scope \
                             file instead."
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
/// Only one, and it is the one the [`Policy`] type's own semantics call for: a
/// later scope adds a layer, it does not rewrite the boundary. Everything else
/// replaces, because a half-merged MCP server definition is not a server.
const APPENDING: &[&[&str]] = &[&["policy", "layers"], &["agent"]];

/// Deep-merge `over` onto `base`, later winning key by key.
fn merge(base: &mut toml::value::Table, over: toml::value::Table, at: &mut Vec<String>) {
    for (key, value) in over {
        at.push(key.clone());
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => merge(b, o, at),
            (Some(toml::Value::Array(b)), toml::Value::Array(o))
                if APPENDING.iter().any(|p| p == &at.as_slice()) =>
            {
                b.extend(o);
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
        let at = Scope::Local;
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

    #[test]
    fn a_command_substitution_runs_outside_the_project_scope_and_never_inside_it() {
        // Each platform's own spelling of "print this", "succeed silently" and
        // "fail". Named rather than skipped: `${cmd:}` is a real feature on Windows
        // and a test that ran on two platforms out of three would prove it there.
        #[cfg(windows)]
        let (echo, quiet, fail) = ("cmd /c echo s3cret", "cmd /c rem", "cmd /c exit 1");
        #[cfg(not(windows))]
        let (echo, quiet, fail) = ("printf s3cret", "true", "false");

        let dir = tempfile::tempdir().unwrap();
        let key = ["mcp".to_string()];
        let path = Path::new("io.local.toml");

        // A trailing newline is trimmed, and the value composes inside a larger string.
        assert_eq!(
            expand(
                Scope::Local,
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
            let err = expand(Scope::Local, input, dir.path(), &key, path)
                .unwrap_err()
                .to_string();
            assert!(err.contains(expect), "{input}: {err}");
        }

        // The project scope refuses it outright, before running anything.
        let err = expand(
            Scope::Project,
            &format!("${{cmd:{echo}}}"),
            dir.path(),
            &key,
            Path::new(PROJECT_FILE),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("refused in the project scope"), "{err}");
        // The negative control: it is `cmd:` that is refused there, not substitution.
        // A rule that disarmed `${env:}` in the project scope would be a much worse
        // feature that this test would otherwise pass.
        std::env::set_var("IO_HARNESS_TEST_SET", "from-the-environment");
        assert_eq!(
            expand(
                Scope::Project,
                "${env:IO_HARNESS_TEST_SET}",
                dir.path(),
                &key,
                Path::new(PROJECT_FILE),
            )
            .unwrap(),
            "from-the-environment"
        );
    }
}
