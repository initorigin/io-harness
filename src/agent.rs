//! Named agent definitions — a role, a model, and a narrower boundary, by name.
//!
//! Before 0.21.0 a spawned sub-agent was identical to its parent apart from a goal
//! string: same model, same system prompt, same boundary minus whatever the parent's
//! own call happened to deny. So "search with the cheap model, write with the strong
//! one" — the largest cost lever this crate has — was unexpressible, and a role was
//! something you had to smuggle into the goal text.
//!
//! An [`AgentDef`] is that missing thing. Register a few in an [`Agents`] roster,
//! hand it to a run with
//! [`TaskContract::with_agents`](crate::TaskContract::with_agents), and the agent
//! spawns one by name.
//!
//! # A definition can only narrow
//!
//! This is the property the whole feature rests on, and it is not enforced by a new
//! check: `deny_write` and `deny_net` are composed through
//! [`Policy::contain`](crate::Policy::contain), the same function that has bounded
//! every child since 0.5.0 — allows intersect, denies union, at any depth. So a
//! definition has no way to express a grant. There is deliberately no `allow_write`
//! and no `allow_net`: "give the writer agent write access" is the natural thing to
//! reach for and it must be impossible, or a roster in a config file becomes a
//! privilege-escalation path.
//!
//! A definition silent about a path its parent denies still yields a child that is
//! refused it.
//!
//! # A model is a request, not a fact
//!
//! [`AgentDef::model`] travels as
//! [`CompletionRequest::model`](crate::provider::CompletionRequest::model), which a
//! vendor may substitute or alias. What actually served a call is
//! [`CompletionResponse::model`](crate::provider::CompletionResponse::model), and
//! that is what the trace keeps.
//!
//! ```
//! use io_harness::{AgentDef, Agents};
//!
//! let roster = Agents::new()
//!     .with(
//!         AgentDef::new("searcher")
//!             .with_role("You find things. You report paths and line numbers and never edit.")
//!             .with_model("anthropic/claude-haiku-4.5")
//!             // It reads. It does not write, and it does not dial out.
//!             .deny_write()
//!             .deny_net()
//!             .with_max_steps(8),
//!     )
//!     .with(
//!         AgentDef::new("author")
//!             .with_role("You make the edit the searcher located, and only that edit.")
//!             .with_model("anthropic/claude-opus-4.5"),
//!     );
//!
//! assert_eq!(roster.names(), vec!["author", "searcher"]);
//! assert_eq!(roster.get("searcher").unwrap().model.as_deref(), Some("anthropic/claude-haiku-4.5"));
//! assert!(roster.get("searcher").unwrap().deny_write);
//! assert!(roster.get("nobody").is_none());
//! ```

use std::collections::BTreeMap;

/// One named agent: who it is, what serves it, and how much narrower than its parent
/// it runs.
///
/// Every field past `name` is optional, and an `AgentDef::new(name)` with nothing else
/// set produces exactly the child a bare `spawn_agent` produced before 0.21.0 — which
/// is what makes the roster additive rather than a second spawn path.
///
/// ```
/// use io_harness::AgentDef;
///
/// let bare = AgentDef::new("worker");
/// assert!(bare.role.is_none() && bare.model.is_none());
/// assert!(!bare.deny_write && !bare.deny_net);
/// assert_eq!(bare.max_steps, None);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentDef {
    /// How the parent asks for it, in `spawn_agent`'s `agent` argument.
    pub name: String,
    /// Prepended to the child's system prompt. Its role, in the second person.
    ///
    /// Prepended rather than replacing: the tree's own system prompt is what tells a
    /// child how to use its tools and that its result composes back, and a role that
    /// replaced it would produce an agent that did not know how to be one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// The model to ask for, or `None` for whatever the run's provider was built
    /// with. A request, not a guarantee — see the module docs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// A step cap for this agent, or `None` to take the spawn call's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_steps: Option<u32>,
    /// Refuse every write for this agent.
    #[serde(default)]
    pub deny_write: bool,
    /// Refuse every outbound connection for this agent.
    #[serde(default)]
    pub deny_net: bool,
}

impl AgentDef {
    /// A definition with a name and nothing narrowed.
    ///
    /// ```
    /// use io_harness::AgentDef;
    ///
    /// assert_eq!(AgentDef::new("reviewer").name, "reviewer");
    /// ```
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Default::default()
        }
    }

    /// Set the role prepended to this agent's system prompt.
    ///
    /// ```
    /// use io_harness::AgentDef;
    ///
    /// let d = AgentDef::new("critic").with_role("You look for what is missing.");
    /// assert_eq!(d.role.as_deref(), Some("You look for what is missing."));
    /// ```
    pub fn with_role(mut self, role: impl Into<String>) -> Self {
        self.role = Some(role.into());
        self
    }

    /// Ask for a specific model for this agent.
    ///
    /// ```
    /// use io_harness::AgentDef;
    ///
    /// let d = AgentDef::new("searcher").with_model("openai/gpt-5-mini");
    /// assert_eq!(d.model.as_deref(), Some("openai/gpt-5-mini"));
    /// ```
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Cap this agent's steps regardless of what the spawn call asks for.
    ///
    /// ```
    /// use io_harness::AgentDef;
    ///
    /// assert_eq!(AgentDef::new("s").with_max_steps(4).max_steps, Some(4));
    /// ```
    pub fn with_max_steps(mut self, steps: u32) -> Self {
        self.max_steps = Some(steps);
        self
    }

    /// Refuse every write for this agent.
    ///
    /// There is no `allow_write`, and that is the point — see the module docs.
    ///
    /// ```
    /// use io_harness::AgentDef;
    ///
    /// assert!(AgentDef::new("reader").deny_write().deny_write);
    /// ```
    pub fn deny_write(mut self) -> Self {
        self.deny_write = true;
        self
    }

    /// Refuse every outbound connection for this agent.
    ///
    /// ```
    /// use io_harness::AgentDef;
    ///
    /// assert!(AgentDef::new("offline").deny_net().deny_net);
    /// ```
    pub fn deny_net(mut self) -> Self {
        self.deny_net = true;
        self
    }
}

/// The roster of definitions one run may spawn by name.
///
/// Sorted and addressed by name, so a duplicate registration replaces rather than
/// shadows — a roster in which the same name means two things depending on
/// registration order is a roster nobody can read.
///
/// ```
/// use io_harness::{AgentDef, Agents};
///
/// let roster = Agents::new()
///     .with(AgentDef::new("worker").with_model("first"))
///     .with(AgentDef::new("worker").with_model("second"));
///
/// // One `worker`, and it is the last one registered.
/// assert_eq!(roster.len(), 1);
/// assert_eq!(roster.get("worker").unwrap().model.as_deref(), Some("second"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Agents {
    defs: BTreeMap<String, AgentDef>,
}

impl Agents {
    /// An empty roster. A run with this spawns exactly as 0.20.0 did.
    ///
    /// ```
    /// use io_harness::Agents;
    ///
    /// assert!(Agents::new().is_empty());
    /// ```
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one definition, replacing any of the same name.
    ///
    /// ```
    /// use io_harness::{AgentDef, Agents};
    ///
    /// let roster = Agents::new().with(AgentDef::new("scout"));
    /// assert!(roster.get("scout").is_some());
    /// ```
    pub fn with(mut self, def: AgentDef) -> Self {
        self.defs.insert(def.name.clone(), def);
        self
    }

    /// The definition under `name`, or `None`.
    ///
    /// A caller asking for a name nobody registered gets `None` rather than a
    /// permissive default: a spawn that silently became an unnarrowed agent because
    /// its definition was misspelled is the failure this prevents.
    ///
    /// ```
    /// use io_harness::{AgentDef, Agents};
    ///
    /// let roster = Agents::new().with(AgentDef::new("scout"));
    /// assert!(roster.get("scout").is_some());
    /// assert!(roster.get("Scout").is_none(), "names are exact");
    /// ```
    pub fn get(&self, name: &str) -> Option<&AgentDef> {
        self.defs.get(name)
    }

    /// Every registered name, sorted.
    ///
    /// ```
    /// use io_harness::{AgentDef, Agents};
    ///
    /// let roster = Agents::new()
    ///     .with(AgentDef::new("zeta"))
    ///     .with(AgentDef::new("alpha"));
    /// assert_eq!(roster.names(), vec!["alpha", "zeta"]);
    /// ```
    pub fn names(&self) -> Vec<&str> {
        self.defs.keys().map(String::as_str).collect()
    }

    /// How many definitions are registered.
    ///
    /// ```
    /// use io_harness::{AgentDef, Agents};
    ///
    /// assert_eq!(Agents::new().with(AgentDef::new("a")).len(), 1);
    /// ```
    pub fn len(&self) -> usize {
        self.defs.len()
    }

    /// Whether the roster is empty.
    ///
    /// ```
    /// use io_harness::Agents;
    ///
    /// assert!(Agents::new().is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }

    /// The roster as the agent is told about it: one `- name: role` line each.
    ///
    /// Offered in the spawn tool's description so the model knows what it may ask
    /// for. A definition with no role is listed by name alone — there is nothing
    /// honest to say about it beyond that it exists.
    ///
    /// ```
    /// use io_harness::{AgentDef, Agents};
    ///
    /// let roster = Agents::new()
    ///     .with(AgentDef::new("searcher").with_role("Finds things; never edits."))
    ///     .with(AgentDef::new("plain"));
    /// let catalog = roster.catalog();
    /// assert!(catalog.contains("- searcher: Finds things; never edits."));
    /// assert!(catalog.contains("- plain\n"));
    /// ```
    pub fn catalog(&self) -> String {
        let mut out = String::new();
        for def in self.defs.values() {
            match def.role.as_deref() {
                Some(role) => out.push_str(&format!("- {}: {}\n", def.name, role.trim())),
                None => out.push_str(&format!("- {}\n", def.name)),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_definition_narrows_nothing() {
        let d = AgentDef::new("worker");
        assert!(!d.deny_write && !d.deny_net);
        assert!(d.role.is_none() && d.model.is_none() && d.max_steps.is_none());
    }

    #[test]
    fn registering_the_same_name_twice_replaces_rather_than_shadows() {
        let roster = Agents::new()
            .with(AgentDef::new("w").with_model("a"))
            .with(AgentDef::new("w").with_model("b"));
        assert_eq!(roster.len(), 1);
        assert_eq!(roster.get("w").unwrap().model.as_deref(), Some("b"));
    }

    #[test]
    fn the_catalog_lists_names_in_sorted_order() {
        let roster = Agents::new()
            .with(AgentDef::new("zeta").with_role("last"))
            .with(AgentDef::new("alpha").with_role("first"));
        let catalog = roster.catalog();
        assert!(catalog.find("alpha").unwrap() < catalog.find("zeta").unwrap());
    }

    #[test]
    fn a_definition_has_no_way_to_express_a_grant() {
        // Not a behavioural test — a surface test. If someone ever adds an
        // `allow_write` to `AgentDef`, this is the assertion that should stop them:
        // narrowing is composed through `Policy::contain`, which cannot widen, and a
        // grant field would need a second mechanism that can.
        let json = serde_json::to_string(&AgentDef::new("x").deny_write()).unwrap();
        assert!(json.contains("deny_write"));
        assert!(!json.contains("allow"));
    }

    #[test]
    fn an_unknown_key_in_a_definition_is_rejected() {
        // `deny_unknown_fields`, so a misspelled `deny_writes` in an `io.toml` is an
        // error naming it rather than a boundary that silently did not narrow.
        let err = serde_json::from_str::<AgentDef>(r#"{"name":"x","deny_writes":true}"#);
        assert!(err.is_err(), "a misspelled narrowing must not be ignored");
    }
}
