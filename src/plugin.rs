//! A directory is a capability bundle (0.35.0).
//!
//! Six capabilities have a discovery path each and share nothing: skills come
//! from one directory, templates from another, agents, MCP servers and hooks
//! from arrays in `io.toml`, and policy layers from a stack the application
//! assembles. Distributing a coherent set of them means copying six things into
//! five places by hand, and once they are in place nothing records that any of
//! them came from somewhere other than the operator.
//!
//! A seventh kind has no discovery path at all and is why 0.73.0 added one. A
//! bundle that ships an executable contributed it to nothing: `which ultraship`
//! found nothing, and a model could not tell "not installed" from "installed
//! somewhere I am not allowed to look". A `[[bin]]` entry names one, and
//! [`Plugin::bin`] hands back its absolute path.
//!
//! A **plugin** is a directory with a [`PLUGIN_FILE`] at its root declaring what
//! it contributes. A `[[plugin]]` entry in any configuration scope names one by
//! path, [`Config::plugins`](crate::Config::plugins) loads every declared one,
//! and the result installs in three places — a [`TaskContract`] through
//! [`Plugins::apply_to`], a [`Policy`] through [`Plugins::apply_to_policy`], and
//! a [`Hooks`] through [`Plugins::apply_to_hooks`] — because those
//! are the three places this crate has ever installed anything.
//!
//! ```toml
//! # plugin.toml
//! name = "rust-review"
//! description = "Everything our Rust reviews need."
//!
//! skills = "skills"
//! templates = "templates"
//!
//! [[agent]]
//! name = "reviewer"
//! model = "cheap-model"
//! deny_write = true
//!
//! [[bin]]
//! name = "review"
//! path = "bin/review.mjs"
//!
//! [policy]
//! layers = [{ name = "no-secrets", rules = [
//!     { act = "write", effect = "deny", pattern = "secrets/**" },
//! ] }]
//! ```
//!
//! # Four rules, and each is the reason the format is usable at all
//!
//! **A bundle is a stranger's directory.** It arrives under the rule 0.28.0 wrote
//! for `[[hook]]`: a plugin whose manifest sits *inside the workspace* may
//! contribute skills, templates, agents and deny rules, and may not contribute a
//! `[[hook]]`, an `[[mcp]]` or a `[[bin]]` (0.73.0) — each names a program this
//! machine will run. Only a plugin whose manifest lives outside the workspace,
//! named from the **user-scope** file, contributes all seven.
//!
//! `io.local.toml` was on the trusted side of that line until 0.74.0, and audit
//! finding H2 is why it is not any more: it sits at the workspace root, which is
//! a path the run's own agent can write and a clone can ship. Declaring a bundle
//! is still permitted from either workspace file — a bundle contributing only
//! skills or templates is the ordinary case — so the refusal is on what the
//! *manifest* contributes, not on the declaration.
//!
//! A `${env:}`, `${file:}` or `${cmd:}` substitution is refused inside a
//! manifest in *every* scope (0.71.0), because a manifest is a third party's file
//! wherever it was named from: the first two read this machine's environment and
//! files, the third runs a program on it, and a downloaded directory gets none of
//! the three. For the same reason a `[[bin]]`'s `path` is refused if it is
//! absolute or climbs out of the bundle with `..` (0.73.0): a bundle contributes
//! an executable it ships, not one it points at somewhere else on this machine.
//! The check is lexical and nothing on disk is read — see [`Plugin::bin`].
//!
//! **The question is where the manifest is, not who named it** (0.74.0). A
//! `[[plugin]]`'s `path` is relative to the discovery root, so the operator's own
//! `~/.config/io/io.toml` writing `path = "bundles/tools"` names a directory
//! inside the very workspace the run is writing to — and a `plugin.toml` the
//! agent puts there is an agent-authored file whatever scope pointed at it. So
//! the user scope's trust is extended to the bundle only when the bundle is
//! outside the workspace, which is where the scope's own premise holds. Moving a
//! bundle out of the project is the migration for a declaration that stops
//! contributing a `[[hook]]`.
//!
//! **A bundle points only at itself, and a workspace file's declaration only
//! inside the workspace** (0.74.0, audit L13). `skills` and `templates` are held
//! to the `[[bin]]` rule above, and a `[[plugin]]`'s own `path` is resolved under
//! the discovery root rather than taken as written — an absolute one, a `..`, or
//! a symbolic link out of the workspace drops the declaration when the file that
//! wrote it is `io.toml` or `io.local.toml`. The user-scope file still points
//! wherever the operator wants, the way `run.skills` does. Both keys name
//! directories that are *read*: the frontmatter of every `*.md` under a skills
//! directory is composed into the model's system prompt on every turn, so a value
//! that escaped put a directory nobody agreed to into the context of every
//! request. Read-only, and still content from outside the workspace arriving
//! unasked.
//!
//! **A bundle may take capability away and may never hand it out.** A `[policy]`
//! block may carry layers of [`Effect::Deny`](crate::Effect) rules and nothing
//! else. An `allow` rule, an `ask` rule or a `defaults` block drops the plugin.
//!
//! **Every contribution carries its plugin.** A contributed skill, template,
//! agent, policy layer or MCP server id is namespaced `<plugin>__<name>` as it
//! loads, so the plugin is already inside the strings the trace has recorded
//! since 0.4.0: a refusal names `<plugin>__<layer>` in
//! [`PolicyEvent::layer`](crate::PolicyEvent), a call names `<plugin>__<server>`
//! in [`McpEvent::server`](crate::McpEvent), and a spawned child's tokens are
//! billed under `<plugin>__<agent>`. Nothing was added to the store to make that
//! true. A `[[hook]]` and a `[[bin]]` are the two exceptions and for one reason:
//! neither contributes a name for an id to prefix — a hook names events, a path
//! and an argv, and a `[[bin]]`'s `name` is the program an operator or a model
//! invokes, which `rust-review__review` is not.
//!
//! # Switched off is not absent
//!
//! A `[[plugin]]` entry carries an `enabled` flag defaulting to true (0.70.0),
//! so every file written before the key existed means exactly what it already
//! meant. An entry written `enabled = false` is still read, validated and held
//! to the same trust rule — switching a bundle on is a one-character edit, so a
//! manifest may not smuggle a `[[hook]]` past the workspace-scope refusal by
//! shipping it switched off — and then contributes nothing to any of the seven. It
//! is listed on [`Plugins::disabled`] rather than [`Plugins::iter`], because an
//! operator who turned a bundle off still has to be able to see that it is
//! declared: a capability missing from every listing reads the same as one
//! nobody ever wrote down.
//!
//! # Nothing here can fail
//!
//! [`Plugins`] has no error path. A plugin with no manifest, unparseable TOML, an
//! unknown key, a malformed or duplicate id, or a contribution its declaring
//! scope may not make is **dropped**: recorded on [`Plugins::dropped`] with the
//! reason and reported as
//! [`EventKind::PluginDropped`](crate::EventKind::PluginDropped), while every
//! plugin that did load is applied. One broken bundle costs exactly itself.
//!
//! That is a deliberate trade and it has a cost: a bundle an operator believes is
//! loaded can be silently absent for a week. Both report channels carry the
//! reason, and an application that wants a broken bundle to be fatal writes one
//! `if`:
//!
//! ```
//! # use io_harness::Config;
//! # fn demo(config: &Config) -> io_harness::Result<()> {
//! let plugins = config.plugins();
//! if let Some(bad) = plugins.dropped().first() {
//!     return Err(io_harness::Error::Config(format!("{}: {}", bad.id, bad.error)));
//! }
//! # Ok(()) }
//! ```

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::agent::AgentDef;
use crate::config::Scope;
use crate::contract::TaskContract;
use crate::error::Result;
use crate::hooks::{Hook, Hooks};
use crate::mcp::McpServer;
use crate::policy::{Effect, Layer, Policy};
use crate::template::Templates;

/// The manifest a plugin directory is recognised by.
///
/// ```
/// use io_harness::PLUGIN_FILE;
///
/// let bundle = std::path::Path::new("/repo/bundles/rust-review");
/// assert!(bundle.join(PLUGIN_FILE).ends_with("plugin.toml"));
/// ```
pub const PLUGIN_FILE: &str = "plugin.toml";

/// What separates a plugin's id from a name it contributed.
///
/// `__` rather than `/`, `:` or `.`, because a namespaced MCP server id ends up
/// inside a tool name (`mcp__<plugin>__<server>__<tool>`) and a vendor's
/// tool-name grammar is `[a-zA-Z0-9_-]`. It is also what
/// [`MCP_TOOL_PREFIX`](crate::MCP_TOOL_PREFIX) already uses, so the crate has one
/// separator rather than two.
///
/// ```
/// use io_harness::{MCP_TOOL_PREFIX, NAMESPACE};
///
/// // What the model is offered for a `search` tool on the `github` server of
/// // the `rust-review` bundle.
/// let tool = format!("{MCP_TOOL_PREFIX}rust-review{NAMESPACE}github{NAMESPACE}search");
/// assert_eq!(tool, "mcp__rust-review__github__search");
/// ```
pub const NAMESPACE: &str = "__";

/// The longest a plugin id may be.
///
/// A namespaced MCP tool name is `mcp__` + plugin + `__` + server + `__` + tool,
/// and vendors cap a tool name at 64 characters. Bounding the one part a bundle
/// author controls keeps that arithmetic from being discovered on the wire.
///
/// ```
/// use io_harness::MAX_ID;
///
/// assert!("rust-review".len() <= MAX_ID);
/// ```
pub const MAX_ID: usize = 32;

// ---------------------------------------------------------------------------
// The manifest
// ---------------------------------------------------------------------------

/// The file format, every contribution optional.
///
/// `deny_unknown_fields` for the reason `io.toml` has it: a section that silently
/// does nothing is a setting an operator believes in. The contribution types are
/// the ones `io.toml` already deserializes — a manifest is the configuration
/// file's vocabulary, not a second one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    /// The plugin's id. Namespaces every name it contributes.
    name: String,
    /// One line for a human. Never read by the crate.
    #[serde(default)]
    description: Option<String>,
    /// The bundle's own version. Documentation: nothing resolves it, and nothing
    /// compares two bundles that contribute the same thing.
    #[serde(default)]
    version: Option<String>,
    /// A directory of skills, relative to the plugin root.
    #[serde(default)]
    skills: Option<PathBuf>,
    /// A directory of prompt templates, relative to the plugin root.
    #[serde(default)]
    templates: Option<PathBuf>,
    #[serde(default)]
    agent: Vec<AgentDef>,
    #[serde(default)]
    mcp: Vec<McpServer>,
    #[serde(default)]
    hook: Vec<Hook>,
    /// (0.73.0) The executables this bundle ships. Read through [`Plugin::bin`].
    #[serde(default)]
    bin: Vec<Bin>,
    #[serde(default)]
    policy: Option<PluginPolicy>,
}

/// One executable a bundle ships (0.73.0).
///
/// `name` is what an operator or a model asks for and `path` is where the bundle
/// keeps it, relative to the plugin root — the two halves nothing could supply
/// for a directory that was never on `PATH`.
///
/// Private, where [`AgentDef`] and [`Hook`] are public: [`Plugin::bin`] hands
/// back `(&str, PathBuf)` pairs, so no caller ever names this type. Making it
/// public would publish its `Serialize`/`Deserialize` derives as API for nothing
/// — the `[[bin]]` table is already committed by the module docs above, which is
/// the argument [`Hook`] makes for the opposite conclusion only because
/// [`Plugin::hooks`] returns the type itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Bin {
    /// The program's name, as it would be invoked. Not namespaced — see
    /// [`Plugin::bin`].
    name: String,
    /// Where the bundle keeps it, relative to the plugin root. Never absolute
    /// and never climbing out with `..`; see `check_bins`.
    path: PathBuf,
}

/// A manifest's `[policy]`: layers, and deliberately no defaults.
///
/// `defaults` is accepted by the parser and refused by name rather than being
/// left to `deny_unknown_fields`, because "unknown field `defaults`" is a
/// confusing thing to read when the same key is legal three files away. The
/// refusal explains itself instead.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PluginPolicy {
    #[serde(default)]
    layers: Vec<Layer>,
    #[serde(default)]
    defaults: Option<toml::value::Table>,
}

// ---------------------------------------------------------------------------
// A loaded plugin, and one that was not
// ---------------------------------------------------------------------------

/// One loaded bundle: its id, its root, and what it contributed.
///
/// Every name in it is already namespaced — reading `agents()` gives back
/// `rust-review__reviewer`, not `reviewer` — because namespacing happens once, at
/// load, rather than at each of the four places a contribution is installed.
///
/// ```
/// use io_harness::Config;
///
/// # fn demo() -> io_harness::Result<()> {
/// let dir = tempfile::tempdir()?;
/// let root = dir.path();
/// std::fs::create_dir(root.join("bundle"))?;
/// std::fs::write(
///     root.join("bundle").join("plugin.toml"),
///     "name = \"rust-review\"\n\n[[agent]]\nname = \"reviewer\"\n",
/// )?;
/// std::fs::write(root.join("io.local.toml"), "[[plugin]]\npath = \"bundle\"\n")?;
///
/// let plugins = Config::discover(root)?.plugins();
/// let plugin = plugins.get("rust-review").unwrap();
/// assert_eq!(plugin.id(), "rust-review");
/// assert_eq!(plugin.agents()[0].name, "rust-review__reviewer");
/// assert_eq!(plugin.contributions(), vec!["agents"]);
/// # Ok(()) }
/// # demo().unwrap();
/// ```
#[derive(Debug, Clone)]
pub struct Plugin {
    id: String,
    root: PathBuf,
    manifest: Manifest,
}

impl Plugin {
    /// The plugin's id, as its manifest's `name` declared it.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The directory the manifest was read from.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The manifest's `description`, if it carried one.
    pub fn description(&self) -> Option<&str> {
        self.manifest.description.as_deref()
    }

    /// The manifest's `version`. Documentation only — see the module docs.
    pub fn version(&self) -> Option<&str> {
        self.manifest.version.as_deref()
    }

    /// The absolute skills directory this plugin contributes, if any.
    ///
    /// Guaranteed to be inside [`Plugin::root`] (0.74.0): a `skills` value that
    /// was absolute or climbed out with `..` was refused at load, for the reason
    /// a `[[bin]]`'s `path` is — with the frontmatter of every `*.md` under this
    /// directory reaching the model's system prompt as the added weight.
    pub fn skills_dir(&self) -> Option<PathBuf> {
        self.manifest.skills.as_ref().map(|d| self.root.join(d))
    }

    /// The absolute templates directory this plugin contributes, if any.
    ///
    /// Inside [`Plugin::root`] on the same rule as [`Plugin::skills_dir`].
    pub fn templates_dir(&self) -> Option<PathBuf> {
        self.manifest.templates.as_ref().map(|d| self.root.join(d))
    }

    /// The agent definitions it contributes, namespaced.
    pub fn agents(&self) -> &[AgentDef] {
        &self.manifest.agent
    }

    /// The MCP servers it contributes, namespaced.
    pub fn mcp_servers(&self) -> &[McpServer] {
        &self.manifest.mcp
    }

    /// The hooks it contributes, in declaration order.
    ///
    /// Not namespaced, and nothing was left out: a `[[hook]]` contributes no
    /// name for an id to prefix — it names events, a path and an argv, and all
    /// three belong to the operator's tree rather than to the bundle's.
    ///
    /// Empty unless the `[[plugin]]` entry was declared in a scope that may
    /// contribute one; a manifest carrying a `[[hook]]` and declared in the
    /// committed `io.toml` is refused whole rather than shortened here. See the
    /// module docs.
    pub fn hooks(&self) -> &[Hook] {
        &self.manifest.hook
    }

    /// The executables it contributes (0.73.0): each entry's `name`, with its
    /// `path` joined onto the plugin root and absolute.
    ///
    /// Nothing on disk is read, here or at load. An executable a bundle ships is
    /// ordinarily produced by the bundle's own build, so a manifest that checked
    /// out would be valid or not depending on whether that build had run — a
    /// property no other key here has. What comes back is what the manifest
    /// declared, resolved; what a missing file means is the caller's to decide.
    /// The path is still guaranteed to be *inside* [`Plugin::root`], because a
    /// `[[bin]]` that was absolute or climbed out with `..` was refused at load.
    ///
    /// Not namespaced, for the reason [`Plugin::hooks`] is not: the name is the
    /// program an operator or a model invokes, and `rust-review__review` is not a
    /// name anyone types.
    ///
    /// Empty unless the `[[plugin]]` entry was declared in a scope that may
    /// contribute one; a manifest carrying a `[[bin]]` and declared in the
    /// committed `io.toml` is refused whole rather than shortened here. See the
    /// module docs.
    pub fn bin(&self) -> Vec<(&str, PathBuf)> {
        self.manifest
            .bin
            .iter()
            .map(|b| (b.name.as_str(), self.root.join(&b.path)))
            .collect()
    }

    /// The policy layers it contributes, namespaced. Deny rules only.
    pub fn policy_layers(&self) -> &[Layer] {
        self.manifest
            .policy
            .as_ref()
            .map_or(&[], |p| p.layers.as_slice())
    }

    /// Which kinds of contribution this plugin actually declared, in a fixed
    /// order — what [`EventKind::PluginLoaded`](crate::EventKind::PluginLoaded)
    /// reports, so an operator reading the trace can see what a bundle brought
    /// without opening it.
    pub fn contributions(&self) -> Vec<&'static str> {
        let m = &self.manifest;
        let present = [
            ("skills", m.skills.is_some()),
            ("templates", m.templates.is_some()),
            ("agents", !m.agent.is_empty()),
            ("mcp", !m.mcp.is_empty()),
            ("hooks", !m.hook.is_empty()),
            // (0.73.0) After `hooks` and before `policy`, because `policy` reads
            // last: it is a constraint on what a run may do rather than
            // something the run is handed.
            ("bin", !m.bin.is_empty()),
            ("policy", !self.policy_layers().is_empty()),
        ];
        present
            .into_iter()
            .filter(|(_, yes)| *yes)
            .map(|(name, _)| name)
            .collect()
    }
}

/// A bundle that was not loaded, and why.
///
/// Never an error: a plugin that fails to load is dropped and reported, and the
/// run proceeds with every plugin that did load.
///
/// ```
/// use io_harness::Config;
///
/// # fn demo() -> io_harness::Result<()> {
/// let dir = tempfile::tempdir()?;
/// let root = dir.path();
/// std::fs::create_dir(root.join("empty"))?;
/// std::fs::write(root.join("io.local.toml"), "[[plugin]]\npath = \"empty\"\n")?;
///
/// let plugins = Config::discover(root)?.plugins();
/// assert!(plugins.is_empty(), "nothing loaded");
/// assert_eq!(plugins.dropped().len(), 1);
/// assert!(plugins.dropped()[0].error.contains("plugin.toml"));
/// # Ok(()) }
/// # demo().unwrap();
/// ```
#[derive(Debug, Clone)]
pub struct Dropped {
    /// The manifest's `name` when it could be read, and the directory's own name
    /// when it could not. A label for a report, not a key — [`Dropped::path`] is
    /// what identifies the directory.
    pub id: String,
    /// The directory the `[[plugin]]` entry named.
    pub path: PathBuf,
    /// What stopped it, worded for the operator who has to fix it.
    pub error: String,
}

// ---------------------------------------------------------------------------
// The set
// ---------------------------------------------------------------------------

/// Every plugin one configuration declared: the ones that loaded, the ones that
/// were switched off, and the ones that did not load at all.
///
/// Cheap to clone and carried by a [`TaskContract`], so a 0.5.0 tree's children
/// see the same bundles their parent did.
///
/// ```
/// use io_harness::{Config, TaskContract};
///
/// # fn demo() -> io_harness::Result<()> {
/// let dir = tempfile::tempdir()?;
/// let root = dir.path();
/// std::fs::create_dir_all(root.join("bundle").join("skills"))?;
/// std::fs::write(
///     root.join("bundle").join("plugin.toml"),
///     "name = \"rust-review\"\nskills = \"skills\"\n",
/// )?;
/// std::fs::write(
///     root.join("bundle").join("skills").join("review.md"),
///     "# review\n\nHow we review.\n",
/// )?;
/// std::fs::write(root.join("io.local.toml"), "[[plugin]]\npath = \"bundle\"\n")?;
///
/// let plugins = Config::discover(root)?.plugins();
/// assert_eq!(plugins.names(), vec!["rust-review"]);
///
/// let contract = plugins.apply_to(TaskContract::workspace("tidy the crate", root));
/// assert_eq!(contract.plugins.len(), 1);
/// # Ok(()) }
/// # demo().unwrap();
/// ```
#[derive(Debug, Clone, Default)]
pub struct Plugins {
    loaded: Vec<Plugin>,
    /// Read and valid, and switched off (0.70.0). A third bucket rather than a
    /// flag on `Plugin`, so every place that installs a contribution keeps
    /// reading `loaded` and none of them can forget the check.
    disabled: Vec<Plugin>,
    dropped: Vec<Dropped>,
}

impl Plugins {
    /// No plugins. What a configuration declaring none carries, and what a
    /// contract built without one holds.
    pub fn none() -> Self {
        Self::default()
    }

    /// The plugins that loaded, in the order they were declared.
    pub fn iter(&self) -> impl Iterator<Item = &Plugin> {
        self.loaded.iter()
    }

    /// One loaded plugin by id.
    pub fn get(&self, id: &str) -> Option<&Plugin> {
        self.loaded.iter().find(|p| p.id == id)
    }

    /// The ids that loaded.
    pub fn names(&self) -> Vec<&str> {
        self.loaded.iter().map(|p| p.id.as_str()).collect()
    }

    /// How many loaded. Says nothing about [`Plugins::disabled`] or
    /// [`Plugins::dropped`].
    pub fn len(&self) -> usize {
        self.loaded.len()
    }

    /// Whether none loaded. Says nothing about [`Plugins::disabled`] or
    /// [`Plugins::dropped`].
    pub fn is_empty(&self) -> bool {
        self.loaded.is_empty()
    }

    /// The bundles that were declared and not loaded, with the reason each was
    /// dropped.
    pub fn dropped(&self) -> &[Dropped] {
        &self.dropped
    }

    /// The bundles declared with `enabled = false` (0.70.0): read, valid, and
    /// contributing nothing.
    ///
    /// Not a second [`Plugins::dropped`]. That one is a failure report, and
    /// everything on it is something an operator has to fix; a bundle here is
    /// doing exactly what the file asked of it, and its id, description, version
    /// and [`Plugin::contributions`] are all readable so an operator can see what
    /// turning it back on would bring. A bundle that is switched off *and*
    /// broken is [`Dropped`], because a broken bundle is broken either way.
    pub fn disabled(&self) -> &[Plugin] {
        &self.disabled
    }

    /// `contract` with every plugin's agents, MCP servers and skills directories
    /// added.
    ///
    /// A method rather than something the run loop does implicitly, for the
    /// reason [`Config::apply_to`](crate::Config::apply_to) is one: what a file
    /// found and what a caller asked for stay separable.
    ///
    /// ```
    /// use io_harness::{Plugins, TaskContract};
    ///
    /// // Applying an empty set is the identity, so a caller need not branch.
    /// let contract = TaskContract::workspace("ship it", ".");
    /// let same = Plugins::none().apply_to(contract);
    /// assert!(same.plugins.is_empty());
    /// ```
    #[must_use]
    pub fn apply_to(&self, contract: TaskContract) -> TaskContract {
        let mut out = contract;
        let mut agents = out.agents.clone();
        for def in self.loaded.iter().flat_map(|p| p.agents()) {
            agents = agents.with(def.clone());
        }
        out.agents = agents;
        let servers: Vec<McpServer> = self
            .loaded
            .iter()
            .flat_map(|p| p.mcp_servers().iter().cloned())
            .collect();
        if !servers.is_empty() {
            let mut all = out.mcp.clone();
            all.extend(servers);
            out.mcp = all;
        }
        out.plugins = self.clone();
        out
    }

    /// `policy` with every plugin's layers stacked on top of it.
    ///
    /// Stacking can only ever narrow: evaluation is deny-first across the whole
    /// stack, and a plugin's rules are all [`Effect::Deny`] by the rule that
    /// admitted them.
    ///
    /// ```
    /// use io_harness::{Act, Effect, Plugins, Policy};
    ///
    /// let policy = Plugins::none().apply_to_policy(Policy::permissive());
    /// assert_eq!(policy.check(Act::Read, "src/lib.rs").effect, Effect::Allow);
    /// ```
    #[must_use]
    pub fn apply_to_policy(&self, policy: Policy) -> Policy {
        let mut out = policy;
        for layer in self.loaded.iter().flat_map(|p| p.policy_layers()) {
            out.layers.push(layer.clone());
        }
        out
    }

    /// `hooks` with every plugin's hooks added.
    ///
    /// Only a plugin declared in a scope that may contribute one has any — see
    /// the module docs. A relative `append` path resolves against `root`, the
    /// same discovery root a configuration's own hooks resolve against.
    ///
    /// ```
    /// use io_harness::{Config, Plugins};
    ///
    /// let config = Config::from_toml("").unwrap();
    /// let hooks = Plugins::none().apply_to_hooks(config.hooks(), ".");
    /// assert!(hooks.is_empty());
    /// ```
    #[must_use]
    pub fn apply_to_hooks(&self, hooks: Hooks, root: impl AsRef<Path>) -> Hooks {
        let mut all: Vec<Hook> = hooks.declarations().to_vec();
        for hook in self.loaded.iter().flat_map(|p| p.manifest.hook.iter()) {
            all.push(hook.clone());
        }
        Hooks::new(all, root.as_ref())
    }

    /// Every plugin's templates, discovered and merged, with each name
    /// namespaced.
    ///
    /// Fallible where the rest of this type is not, because it reads directories
    /// the manifest named: a `templates` key pointing at nothing is a mistake in
    /// a bundle that otherwise loaded, and reporting it as an empty catalogue
    /// would hide it. Rendering happens before a run exists, which is why this is
    /// the caller's to hold rather than the contract's.
    pub fn templates(&self) -> Result<Templates> {
        let mut out = Templates::none();
        for plugin in &self.loaded {
            let Some(dir) = plugin.templates_dir() else {
                continue;
            };
            out = out.merged(Templates::discover(dir)?.namespaced(&plugin.id))?;
        }
        Ok(out)
    }

    /// Read and validate one bundle directory without declaring it (0.71.0).
    ///
    /// The same loader [`Config::plugins`](crate::Config::plugins) runs, reached
    /// without the `[[plugin]]` entry — so an installer can show what a
    /// downloaded directory contributes, and *whether it would load at all*,
    /// before writing a line into an operator's configuration. Every check runs:
    /// the id grammar, the trust rule for `scope`, the narrowing rule, the
    /// `[[bin]]` containment rule, the
    /// `[[hook]]` validator, and every `${...}` substitution refused in a manifest
    /// wherever it came from. The error is the string that would have appeared on
    /// [`Plugins::dropped`], so a preflight and a load cannot disagree.
    ///
    /// Nothing about the host is read to answer: this call is what an installer
    /// makes *before* an operator has agreed to anything, so a manifest asking for
    /// `${env:AWS_SECRET_ACCESS_KEY}` or `${file:~/.ssh/id_rsa}` is refused rather
    /// than resolved into a string this call hands back to be displayed.
    ///
    /// Fallible where loading a declared set is not, and deliberately: a set that
    /// dropped a bundle still has the others, while a caller asking about one
    /// directory is asking a yes-or-no question.
    ///
    /// # `scope` is the answer, not a formality
    ///
    /// It is the scope the caller intends to *declare* the bundle from, and the
    /// result differs by it on purpose — this is the marketplace-install
    /// semantics of the module's first rule, not a quirk of the loader:
    ///
    /// - [`Scope::User`] is the operator's own file, outside every workspace, so
    ///   a manifest's `[[hook]]`, `[[mcp]]` and `[[bin]]` are returned like any
    ///   other contribution. What that scope does *not* carry is the bundle's
    ///   location: a user-scope `[[plugin]]` whose `path` resolves inside the
    ///   discovery root is graded at load time as the workspace file it is, and
    ///   the same manifest is refused there. An installer that writes a
    ///   bundle into the project it is working on wants that answer, not this
    ///   one.
    /// - [`Scope::Project`] is the committed `io.toml` that arrives with a
    ///   `git clone`, and as of 0.74.0 [`Scope::Local`] is held to the same rule
    ///   — `io.local.toml` sits at the workspace root, so the agent can write it
    ///   (audit H2). From either, the same manifest is **refused whole** — not
    ///   shortened.
    ///   A bundle that would only load from one of the two is exactly what an
    ///   installer has to tell an operator before it writes anything.
    ///
    /// ```
    /// use io_harness::config::Scope;
    /// use io_harness::Plugins;
    ///
    /// # fn demo() -> io_harness::Result<()> {
    /// let dir = tempfile::tempdir()?;
    /// let bundle = dir.path().join("rust-review");
    /// std::fs::create_dir(&bundle)?;
    /// std::fs::write(
    ///     bundle.join("plugin.toml"),
    ///     "name = \"rust-review\"\n\n\
    ///      [[hook]]\non = [\"finished\"]\nrun = [\"notify\"]\n",
    /// )?;
    ///
    /// // Nothing was declared and no configuration was discovered.
    /// let plugin = Plugins::inspect(Scope::User, &bundle)?;
    /// assert_eq!(plugin.id(), "rust-review");
    /// assert_eq!(plugin.hooks()[0].on().to_vec(), ["finished"]);
    ///
    /// // The same directory, named from the file a clone delivers.
    /// let err = Plugins::inspect(Scope::Project, &bundle).unwrap_err();
    /// assert!(err.to_string().contains("may not contribute"), "{err}");
    /// # Ok(()) }
    /// # demo().unwrap();
    /// ```
    pub fn inspect(scope: Scope, dir: impl AsRef<Path>) -> Result<Plugin> {
        // No discovery root reaches here, so `Scope::User` is answered as the
        // trusted case unconditionally. That is the honest answer for what this
        // call is — a preflight against a directory an operator is deciding
        // whether to declare at all — and `Plugins::load` is where the second
        // half of the rule lives: a user-scope declaration whose path lands
        // *inside* the workspace is graded as the workspace file it is, so a
        // bundle inspected here as loadable can still be refused there. The
        // directory is normally outside any workspace at install time, which is
        // when this is called.
        load_one(scope == Scope::User, dir.as_ref())
    }

    /// Load every declared plugin. Infallible by construction: see the module
    /// docs.
    ///
    /// `root` is the discovery root a relative `path` resolves against — the
    /// project the harness was pointed at, not the directory the declaring file
    /// happens to live in, which is the rule a `[[hook]]`'s `append` already
    /// follows. Since 0.74.0 it is also what decides trust, and it decides it two
    /// different ways depending on which file declared the bundle:
    ///
    /// - declared by a file **inside the workspace** (`io.toml`, `io.local.toml`),
    ///   a `path` that resolves outside `root` — absolute, `..`, or through a
    ///   symbolic link — is dropped rather than loaded;
    /// - declared by the **user-scope** file, a `path` may point anywhere, and it
    ///   is where the manifest lands that decides what the manifest may
    ///   contribute: inside `root` it is graded as the workspace file it is.
    ///
    /// A declaration switched off is loaded and validated like any other and
    /// then routed to [`Plugins::disabled`], because what an operator wants to
    /// read about a bundle they turned off is what it *is*, and a manifest that
    /// is refused while switched on is refused while switched off too.
    pub(crate) fn load(declarations: &[(Scope, Declaration)], root: &Path) -> Self {
        let mut out = Self::default();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for (scope, decl) in declarations {
            let dir = if decl.path.is_absolute() {
                decl.path.clone()
            } else {
                root.join(&decl.path)
            };
            let fallback = dir
                .file_name()
                .map_or_else(|| dir.display().to_string(), |n| n.to_string_lossy().into());
            // `contain_under_root` rather than the lexical rule `check_bins`
            // uses, because the directory this names is one that exists — it has
            // to, or there is no manifest to read — so a symbolic link inside the
            // workspace pointing out of it is a live route here in a way it is
            // not for an executable a bundle has not built yet.
            //
            // What is *checked* is canonical; what becomes `Plugin::root` is the
            // path as written. The two name the same directory once the check has
            // passed, and `Plugin::root` is a value operators read and strip
            // prefixes off — canonicalising it would rename every plugin path on
            // any host whose temporary or checkout directory is itself a link.
            let contained = crate::tools::workspace::contain_under_root(root, &decl.path);
            // 0.74.0, audit L13. `path` was taken as written: an absolute one
            // replaced the discovery root outright and a relative one climbed out
            // of it with `..`, so a workspace file could point a bundle at any
            // directory on this host and a manifest sitting there contributed its
            // `skills`, whose every `*.md` frontmatter is composed into the
            // model's system prompt on the next turn. Read-only, and still
            // content from outside the workspace entering the context unasked.
            //
            // Held against the scopes a workspace can supply and not against the
            // user scope, which is the split `refuse_widening` makes for
            // `run.skills` and makes for the same reason: the operator's own file
            // still points wherever the operator wants, and a shared bundle kept
            // outside the project is the case that would otherwise stop loading.
            if *scope != Scope::User {
                if let Err(error) = &contained {
                    out.dropped.push(Dropped {
                        id: fallback,
                        path: dir,
                        error: error.to_string(),
                    });
                    continue;
                }
            }
            // 0.74.0. Trust follows where the manifest *is*, not which file named
            // it, and a `[[plugin]]` is the one declaration where those two answers
            // differ: every other section is content in the file whose scope is
            // being graded, while this one is a pointer at a second file somewhere
            // else. `read_scope` states the premise the user scope's exemption
            // rests on — `$IO_CONFIG`, `$IO_CONFIG_HOME` and `~/.config/io` are
            // outside every workspace, so a run that can write its own root cannot
            // reach them — and that premise holds for the declaring file and does
            // not transfer to a directory it points at. A relative `path` resolves
            // against the discovery root, so `path = "bundles/tools"` in the
            // operator's own file names a directory the run's agent writes to: one
            // ordinary `write_file` of `bundles/tools/plugin.toml` carrying a
            // `[[hook]]` was a program to run, installed with no refusal anywhere
            // on the path.
            //
            // `contained.is_err()` is read as "outside", which is fail-closed
            // where it matters: the other way `contain_under_root` fails is a
            // component it cannot resolve, and a component this cannot resolve is
            // one `load_one` cannot read a manifest through either, so such a
            // declaration is dropped a few lines below rather than trusted.
            let trusted = *scope == Scope::User && contained.is_err();
            match load_one(trusted, &dir) {
                Ok(plugin) => {
                    // A switched-off bundle claims no id, and the ordering here
                    // is the whole of that rule (0.70.0).
                    //
                    // The id exists to namespace what a bundle contributes, and a
                    // disabled one contributes nothing — so reserving it would be
                    // holding a name against a bundle that uses none. It would
                    // also break the swap this feature exists to make easy:
                    // switching `tools-v1` off and declaring `tools-v2` beside it
                    // is a one-line edit, and if the disabled entry held the id
                    // the new bundle would be dropped as a duplicate and neither
                    // would contribute — with the failure reported against the
                    // entry the operator did not touch.
                    //
                    // Two ENABLED bundles sharing an id is still a mistake and is
                    // still reported, because that is a real collision. Two
                    // disabled ones are not: neither claims anything.
                    if decl.enabled {
                        if !seen.insert(plugin.id.clone()) {
                            out.dropped.push(Dropped {
                                id: plugin.id.clone(),
                                path: dir,
                                error: format!(
                                    "a plugin with id `{}` is already declared and switched on; \
                                     two bundles cannot share an id, because the id is what \
                                     every name they contribute is namespaced by",
                                    plugin.id
                                ),
                            });
                            continue;
                        }
                        out.loaded.push(plugin);
                    } else {
                        out.disabled.push(plugin);
                    }
                }
                Err(error) => out.dropped.push(Dropped {
                    id: fallback,
                    path: dir,
                    error: error.to_string(),
                }),
            }
        }
        out
    }
}

/// Read, validate, and namespace one bundle.
///
/// Every check runs **before** any contribution is namespaced or handed back, so
/// a refused manifest contributes nothing rather than contributing the half that
/// was read before the refusal.
fn load_one(trusted: bool, dir: &Path) -> Result<Plugin> {
    let file = dir.join(PLUGIN_FILE);
    if !file.is_file() {
        return Err(crate::Error::Config(format!(
            "{}: no {PLUGIN_FILE}; a plugin is a directory with a manifest at its root",
            dir.display()
        )));
    }
    let text = std::fs::read_to_string(&file)
        .map_err(|e| crate::Error::Config(format!("{}: {e}", file.display())))?;

    // Parsed and *not* substituted, in every scope: see `refuse_substitutions`.
    let value: toml::Value = toml::from_str(&text).map_err(|e: toml::de::Error| {
        crate::Error::Config(format!("{}: {}", file.display(), e.message()))
    })?;
    refuse_substitutions(&value, &mut Vec::new(), &file)?;
    let mut manifest: Manifest = value.try_into().map_err(|e: toml::de::Error| {
        crate::Error::Config(format!("{}: {}", file.display(), e.message()))
    })?;

    check_id(&manifest.name, &file)?;
    // 0.74.0, audit H2 — every scope but the user's, not just the project one.
    //
    // This read `scope == Scope::Project` until 0.74.0, and that left the whole
    // finding open one level down. `plugin` is deliberately not a refused
    // section, so a workspace file may still declare one; the agent therefore
    // wrote `io.local.toml` naming a bundle, wrote the bundle's own
    // `plugin.toml` carrying a `[[hook]]`, and the next `discover().plugins()`
    // carried it — two ordinary writes, no refusal anywhere on the path. The
    // check that closes it has to match `Config::read_scope`, which now
    // widening-checks every scope that is not the user's.
    //
    // `trusted` rather than the scope itself, because the scope answers who named
    // the bundle and this rule is about where the manifest is — see
    // `Plugins::load`, which is the caller that holds both answers. Here the
    // parameter is the whole of the decision, so nothing in this function has to
    // hold the two apart.
    if !trusted {
        refuse_executing_contributions(&manifest, &file)?;
    }
    check_narrowing(&manifest, &file)?;
    check_dirs(&manifest, &file)?;
    check_bins(&manifest, &file)?;
    Hooks::check(&manifest.hook, &file)?;

    let id = manifest.name.clone();
    for def in &mut manifest.agent {
        def.name = namespaced(&id, &def.name);
    }
    for server in &mut manifest.mcp {
        server.id = namespaced(&id, &server.id);
    }
    if let Some(policy) = manifest.policy.as_mut() {
        for layer in &mut policy.layers {
            layer.name = namespaced(&id, &layer.name);
        }
    }

    Ok(Plugin {
        id,
        root: dir.to_path_buf(),
        manifest,
    })
}

/// Refuse every `${...}` substitution inside a manifest, in every scope (0.71.0).
///
/// `${cmd:}` has been refused here since 0.35.0 because it runs a program on this
/// machine. `${env:}` and `${file:}` *read* one, which is the same class of act:
/// `${env:AWS_SECRET_ACCESS_KEY}` is whatever the resolving process was started
/// with, and `${file:}` resolves through `Path::join`, where an absolute argument
/// replaces the base and a relative one climbs out of the bundle with `..` — an
/// arbitrary read of the host. A manifest is the one file here nobody has agreed
/// to: [`Plugins::inspect`] resolves it precisely so an operator can decide
/// whether to declare the directory at all, and a value that reached
/// [`Plugin::description`] or [`Plugin::mcp_servers`] would be displayed and
/// logged before that decision was made. So a bundle is a third party's directory
/// even when the file naming it is the operator's own, and it does not get to read
/// this host.
///
/// Refused on the shared load path rather than inside `inspect`, so a preflight
/// and a load cannot disagree about what a bundle is.
fn refuse_substitutions(value: &toml::Value, key: &mut Vec<String>, file: &Path) -> Result<()> {
    match value {
        toml::Value::String(s) if s.contains("${") => Err(crate::Error::Config(format!(
            "{}: key `{}`: a `${{env:}}`, `${{file:}}` or `${{cmd:}}` substitution is refused \
             inside a {PLUGIN_FILE} in every scope, because a bundle is a third party's \
             directory even when the file naming it is the operator's own — resolving one \
             would read this machine's environment or its files, or run a program on it, for \
             a directory nobody has agreed to yet. Write the value out.",
            file.display(),
            key.join(".")
        ))),
        toml::Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                key.push(format!("[{i}]"));
                refuse_substitutions(item, key, file)?;
                key.pop();
            }
            Ok(())
        }
        toml::Value::Table(table) => {
            for (k, v) in table {
                key.push(k.clone());
                refuse_substitutions(v, key, file)?;
                key.pop();
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// `<plugin>__<name>`.
pub(crate) fn namespaced(plugin: &str, name: &str) -> String {
    format!("{plugin}{NAMESPACE}{name}")
}

/// A plugin id is `[a-z0-9][a-z0-9-]{0,31}`.
///
/// Lower-case and hyphenated so it is safe inside a vendor's tool-name grammar,
/// bounded so a namespaced tool name stays inside the 64 characters vendors
/// allow, and free of [`NAMESPACE`] so no two bundles can produce the same
/// namespaced name from different halves.
fn check_id(id: &str, at: &Path) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= MAX_ID
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && id.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit());
    if ok {
        return Ok(());
    }
    Err(crate::Error::Config(format!(
        "{}: key `name`: {id:?} is not a usable plugin id. It must be 1 to {MAX_ID} characters of \
         `a-z`, `0-9` and `-`, starting with a letter or a digit — it namespaces every name this \
         bundle contributes, and it ends up inside an MCP tool name where a vendor allows nothing \
         else.",
        at.display()
    )))
}

/// A plugin whose manifest sits inside the workspace contributes nothing that
/// runs a program.
///
/// The 0.28.0 argument applied to a new declaration site: `io.toml` is the file a
/// `git clone` delivers, a `[[hook]]` runs an argv or writes to a path the file
/// chose, an `[[mcp]]` names a command this process spawns, and a `[[bin]]`
/// (0.73.0) names a program for something to find and run. The refusal is
/// whole — a manifest that declares one contributes none of its other six kinds
/// either, because a half-applied stranger's manifest is the failure this rule
/// exists to prevent.
///
/// Whether it applies is [`Plugins::load`]'s answer and not this function's: the
/// caller is the one that knows the discovery root, and since 0.74.0 the question
/// is where the manifest lives rather than which file named it. The message
/// therefore states both halves of the way out — a bundle outside the workspace,
/// declared from the user scope — because either one alone leaves the refusal
/// standing.
fn refuse_executing_contributions(manifest: &Manifest, at: &Path) -> Result<()> {
    let offending = if !manifest.hook.is_empty() {
        "hook"
    } else if !manifest.mcp.is_empty() {
        "mcp"
    } else if !manifest.bin.is_empty() {
        // 0.73.0. A `[[bin]]` names a program on this machine and exists so that
        // something will go looking for it, which is the whole of the 0.28.0
        // argument with a third key attached.
        "bin"
    } else {
        return Ok(());
    };
    Err(crate::Error::Config(format!(
        "{}: key `{offending}`: a plugin whose manifest is inside the workspace may not contribute \
         `[[{offending}]]`, because it names a program this machine would run and a file inside the \
         workspace arrives with a `git clone` or is written by the agent itself. Keep this bundle \
         outside the workspace and name it from the user-scope file, or remove the \
         `[[{offending}]]` from its manifest.",
        at.display()
    )))
}

/// `skills` and `templates` stay inside the bundle, decided lexically (0.74.0,
/// audit L13).
///
/// [`Plugin::skills_dir`] and [`Plugin::templates_dir`] resolve their key through
/// `Path::join`, where an absolute value replaces the plugin root outright and a
/// relative one climbs out of it with `..` — the same two moves `check_bins`
/// refuses for a `[[bin]]`. The consequence is larger here than there: a
/// `[[bin]]` is a path handed back for a caller to decide about, while a skills
/// directory is *read* at run start and the frontmatter of every `*.md` under it
/// is composed into the model's system prompt on every turn. So `skills =
/// "/home/you/notes"` in a manifest put a directory nobody agreed to into the
/// context of every request, read-only and unannounced.
///
/// `skills::resolve_under` already confines each file to the skills directory.
/// What it cannot do is decide where that directory is, which is this.
///
/// Lexical, and for `check_bins`' reason: `load_one` performs no filesystem check
/// of any kind, and a manifest whose validity depended on whether a directory had
/// been checked out yet would be valid on Tuesday and dropped on Wednesday.
fn check_dirs(manifest: &Manifest, at: &Path) -> Result<()> {
    for (key, declared) in [
        ("skills", manifest.skills.as_ref()),
        ("templates", manifest.templates.as_ref()),
    ] {
        let Some(declared) = declared else { continue };
        let wrong = declared.components().find_map(|c| match c {
            Component::Prefix(_) | Component::RootDir => Some("is an absolute path"),
            Component::ParentDir => Some("climbs out of the plugin root with `..`"),
            _ => None,
        });
        let Some(wrong) = wrong else { continue };
        return Err(crate::Error::Config(format!(
            "{}: key `{key}`: {:?} {wrong}, and `{key}` is resolved by joining it onto the plugin \
             root, because a bundle contributes a directory it ships rather than one it points at \
             somewhere else on this machine — and every `*.md` under it reaches the model's system \
             prompt. Write the path relative to the plugin root.",
            at.display(),
            declared,
        )));
    }
    Ok(())
}

/// A `[[bin]]` path stays inside the bundle, decided lexically (0.73.0).
///
/// [`Plugin::bin`] resolves an entry through `Path::join`, where an absolute
/// argument replaces the base and a relative one climbs out with `..` — the same
/// two moves `refuse_substitutions` names about `${file:}`. A bundle contributes
/// an executable it *ships*, so an entry that could name `/usr/bin/env` or
/// `../../../ssh` is refused rather than resolved.
///
/// Lexical, and deliberately: `load_one` performs no filesystem check of any
/// kind, and an executable a bundle ships is ordinarily produced by the bundle's
/// own build — a manifest whose validity depended on whether that build had run
/// would be valid on Tuesday and dropped on Wednesday. Nothing here stats, opens
/// or canonicalizes the declared file.
///
/// `Component` rather than a string test on both counts, so a Windows prefix
/// (`C:\`), a bare root (`/bin/x`, which `is_absolute` calls relative on Windows)
/// and a `..` buried mid-path (`bin/../../x`) are all one rule.
fn check_bins(manifest: &Manifest, at: &Path) -> Result<()> {
    for (index, entry) in manifest.bin.iter().enumerate() {
        let wrong = entry.path.components().find_map(|c| match c {
            Component::Prefix(_) | Component::RootDir => Some("is an absolute path"),
            Component::ParentDir => Some("climbs out of the plugin root with `..`"),
            _ => None,
        });
        let Some(wrong) = wrong else { continue };
        return Err(crate::Error::Config(format!(
            "{}: key `bin[{index}]`: `{}` declares the path {:?}, which {wrong}, and a `[[bin]]` \
             path is resolved by joining it onto the plugin root, because a bundle contributes an \
             executable it ships rather than one it points at somewhere else on this machine. \
             Write the path relative to the plugin root.",
            at.display(),
            entry.name,
            entry.path,
        )));
    }
    Ok(())
}

/// Plugin-supplied policy may only narrow.
fn check_narrowing(manifest: &Manifest, at: &Path) -> Result<()> {
    let Some(policy) = &manifest.policy else {
        return Ok(());
    };
    if policy.defaults.is_some() {
        return Err(crate::Error::Config(format!(
            "{}: key `policy.defaults`: a plugin may narrow a boundary and may never widen one, \
             and a default decides every action no rule mentions. Contribute deny rules instead.",
            at.display()
        )));
    }
    for layer in &policy.layers {
        for rule in &layer.rules {
            if rule.effect != Effect::Deny {
                return Err(crate::Error::Config(format!(
                    "{}: key `policy.layers`: layer `{}` carries a `{:?}` rule for {:?}, and a \
                     plugin may only contribute `deny`. A bundle may take capability away and may \
                     never hand it out.",
                    at.display(),
                    layer.name,
                    rule.effect,
                    rule.pattern
                )));
            }
        }
    }
    Ok(())
}

/// What `[[plugin]]` deserializes to: one directory, named by path.
///
/// A separate declaration from the manifest itself so the *configuration* says
/// which bundles this project uses and the *bundle* says what it contains —
/// which is what lets the declaring scope decide the trust rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Declaration {
    /// The directory, relative to the discovery root or absolute.
    pub(crate) path: PathBuf,
    /// Whether this bundle is switched on (0.70.0).
    ///
    /// Absent means on, which is what makes the key additive: an `io.toml`
    /// written before 0.70.0 declares exactly the bundles it always declared. A
    /// misspelling cannot be mistaken for an operator turning something off,
    /// because `deny_unknown_fields` above refuses the entry outright.
    #[serde(default = "default_enabled")]
    pub(crate) enabled: bool,
}

/// The `enabled` default for a `[[plugin]]` entry that predates the field. See
/// [`Declaration::enabled`].
fn default_enabled() -> bool {
    true
}
