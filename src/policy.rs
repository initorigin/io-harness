//! The permission policy: what the agent may read, write, and execute.
//!
//! A [`Policy`] is a stack of named [`Layer`]s plus a per-action default. It is
//! evaluated deny-first: a deny in *any* layer wins over an allow in any other,
//! so an overlay can add capability but can never re-allow what a layer beneath
//! it denied. That single rule is what makes a shared base policy trustworthy
//! when io-cli and io-studio each stack their own config over it.
//!
//! [`Policy::check`] and [`Policy::explain`] are the same function — `check` is
//! `explain` — so an explanation can never describe a boundary different from
//! the one enforced.

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// What an action wants to do. Search tools (`grep`, `find`) filter their
/// results with [`Act::Read`], so a denied path cannot reach the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Act {
    /// Read a file's contents into context.
    Read,
    /// Create or overwrite a file.
    Write,
    /// Spawn a binary (the verification layer's compile/test commands).
    Exec,
    /// Open an outbound connection. The target is a host, normally in
    /// `host:port` form — see [`Policy::check`] for how a rule matches one.
    Net,
}

/// What a rule does. Ordered by strictness: `Allow` < `Ask` < `Deny`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    /// Proceed without asking.
    Allow,
    /// Proceed only after a human approves.
    Ask,
    /// Never proceed. Absolute across layers.
    Deny,
}

/// One rule: an effect applied to an action whose target matches `pattern`.
///
/// `pattern` is a glob (`*` any run including `/`, `?` one character) matched
/// against the target's full relative path *or* its basename, the same way the
/// `find` tool matches. That is what lets `.env` deny `config/.env`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    /// Which kind of action this rule governs.
    pub act: Act,
    /// What to do when it matches.
    pub effect: Effect,
    /// The glob matched against the path (or binary name, for [`Act::Exec`]).
    pub pattern: String,
}

/// A named group of rules. The name is what [`Policy::explain`] and the trace
/// report, so a refusal from a shared base is attributable to that base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layer {
    /// Human-readable name, surfaced in explanations and the trace.
    pub name: String,
    /// The rules in this layer.
    pub rules: Vec<Rule>,
}

/// The default effect for an action no rule mentions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Defaults {
    /// Default for reads. Reads inside the allow list are the permissive tier.
    pub read: Effect,
    /// Default for writes. Every write asks, including an in-policy overwrite.
    pub write: Effect,
    /// Default for spawning a binary.
    pub exec: Effect,
    /// Default for opening an outbound connection.
    ///
    /// `#[serde(default)]` is load-bearing: a policy serialized by 0.7.0 or
    /// earlier has no `net` field at all, and it deserializes here to `Deny`.
    /// That is a deliberate behaviour change — an old config that made outbound
    /// calls stops making them until it carries a `net` allow — chosen because
    /// the alternative silently leaves egress ungoverned for exactly the callers
    /// who upgraded to govern it.
    #[serde(default = "deny")]
    pub net: Effect,
}

/// The `net` default for a policy that predates the field. See [`Defaults::net`].
fn deny() -> Effect {
    Effect::Deny
}

/// The outcome of evaluating a policy, with the rule and layer that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// What to do.
    pub effect: Effect,
    /// The glob that decided, or `None` when the tier default decided.
    pub rule: Option<String>,
    /// The layer the deciding rule came from, or `None` for the tier default.
    pub layer: Option<String>,
}

/// Secret-bearing paths denied out of the box by [`Policy::default`], following
/// OpenCode's `.env` default. A read allow list alone does not protect secrets
/// that live *inside* the workspace. Writes are denied too — nothing the agent
/// legitimately does rewrites a private key.
const SECRET_PATTERNS: &[&str] = &[".env", "*.pem", "id_rsa", "id_ed25519", "*.key"];

/// A permission policy: a stack of layers plus per-action defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// Layers in stacking order. Later layers may add capability, never
    /// re-allow a deny from an earlier one.
    pub layers: Vec<Layer>,
    /// What applies when no rule matches.
    pub defaults: Defaults,
}

impl Default for Policy {
    /// The tiered default a caller gets when they construct a policy without
    /// specifying tiers — reads allowed, writes and execs gated on approval,
    /// and the secret paths denied outright.
    ///
    /// This is *not* what applies when a caller passes no policy at all; that
    /// is [`Policy::permissive`], which enforces nothing.
    fn default() -> Self {
        let mut rules = Vec::new();
        for pat in SECRET_PATTERNS {
            for act in [Act::Read, Act::Write] {
                rules.push(Rule {
                    act,
                    effect: Effect::Deny,
                    pattern: (*pat).to_string(),
                });
            }
        }
        // Verification's own spawns are allowed by name so a constructed policy
        // does not break the verify gate; anything else must be allowed
        // explicitly, since verification has no approver to prompt.
        let exec = Layer {
            name: "builtin-exec".into(),
            rules: ["rustc", "<test-binary>"]
                .iter()
                .map(|p| Rule {
                    act: Act::Exec,
                    effect: Effect::Allow,
                    pattern: (*p).to_string(),
                })
                .collect(),
        };
        Self {
            layers: vec![
                Layer {
                    name: "builtin-secrets".into(),
                    rules,
                },
                exec,
            ],
            defaults: Defaults {
                read: Effect::Allow,
                write: Effect::Ask,
                exec: Effect::Ask,
                // Deny, not Ask: an outbound host is not a thing a human can
                // meaningfully approve on sight mid-run without knowing why it
                // is being dialled, and the caller naming its hosts up front is
                // the whole point of the act.
                net: Effect::Deny,
            },
        }
    }
}

impl Policy {
    /// A policy that enforces nothing — what [`crate::run`] applies when the
    /// caller passes none, preserving 0.3.0 behaviour. The boundary is opt-in.
    pub fn permissive() -> Self {
        Self {
            layers: Vec::new(),
            defaults: Defaults {
                read: Effect::Allow,
                write: Effect::Allow,
                exec: Effect::Allow,
                net: Effect::Allow,
            },
        }
    }

    /// Does this policy enforce nothing? True for [`Policy::permissive`] — no
    /// rules and every default `Allow`.
    pub fn is_permissive(&self) -> bool {
        self.layers.iter().all(|l| l.rules.is_empty())
            && self.defaults.read == Effect::Allow
            && self.defaults.write == Effect::Allow
            && self.defaults.exec == Effect::Allow
            && self.defaults.net == Effect::Allow
    }

    /// Start a new named layer. Subsequent rule builders append to it.
    pub fn layer(mut self, name: impl Into<String>) -> Self {
        self.layers.push(Layer {
            name: name.into(),
            rules: Vec::new(),
        });
        self
    }

    /// Append a rule to the current layer, starting one if none exists.
    pub fn rule(mut self, act: Act, effect: Effect, pattern: impl Into<String>) -> Self {
        if self.layers.is_empty() {
            self = self.layer("policy");
        }
        let pattern = pattern.into();
        let layer = self.layers.last_mut().expect("a layer exists");
        layer.rules.push(Rule {
            act,
            effect,
            pattern,
        });
        self
    }

    /// Allow reads matching `pattern`.
    pub fn allow_read(self, pattern: impl Into<String>) -> Self {
        self.rule(Act::Read, Effect::Allow, pattern)
    }
    /// Deny reads matching `pattern`.
    pub fn deny_read(self, pattern: impl Into<String>) -> Self {
        self.rule(Act::Read, Effect::Deny, pattern)
    }
    /// Allow writes matching `pattern` without asking.
    pub fn allow_write(self, pattern: impl Into<String>) -> Self {
        self.rule(Act::Write, Effect::Allow, pattern)
    }
    /// Deny writes matching `pattern`.
    pub fn deny_write(self, pattern: impl Into<String>) -> Self {
        self.rule(Act::Write, Effect::Deny, pattern)
    }
    /// Require approval for writes matching `pattern`.
    pub fn ask_write(self, pattern: impl Into<String>) -> Self {
        self.rule(Act::Write, Effect::Ask, pattern)
    }
    /// Allow spawning a binary matching `pattern`.
    pub fn allow_exec(self, pattern: impl Into<String>) -> Self {
        self.rule(Act::Exec, Effect::Allow, pattern)
    }
    /// Deny spawning a binary matching `pattern`.
    pub fn deny_exec(self, pattern: impl Into<String>) -> Self {
        self.rule(Act::Exec, Effect::Deny, pattern)
    }
    /// Allow outbound connections to hosts matching `pattern`.
    pub fn allow_net(self, pattern: impl Into<String>) -> Self {
        self.rule(Act::Net, Effect::Allow, pattern)
    }
    /// Deny outbound connections to hosts matching `pattern`.
    pub fn deny_net(self, pattern: impl Into<String>) -> Self {
        self.rule(Act::Net, Effect::Deny, pattern)
    }
    /// Require approval for outbound connections to hosts matching `pattern`.
    pub fn ask_net(self, pattern: impl Into<String>) -> Self {
        self.rule(Act::Net, Effect::Ask, pattern)
    }

    /// Stack `overlay` on top of this policy.
    ///
    /// Layers concatenate, so every deny in either policy still applies —
    /// evaluation is deny-first across the whole stack, which is what makes an
    /// overlay unable to re-allow a base deny. Defaults tighten only: the
    /// stricter of the two wins per action.
    pub fn merge(mut self, overlay: Policy) -> Self {
        self.layers.extend(overlay.layers);
        self.defaults = Defaults {
            read: self.defaults.read.max(overlay.defaults.read),
            write: self.defaults.write.max(overlay.defaults.write),
            exec: self.defaults.exec.max(overlay.defaults.exec),
            net: self.defaults.net.max(overlay.defaults.net),
        };
        self
    }

    /// Derive a child agent's effective policy from this parent policy under
    /// *containment*: the child inherits every parent rule and may only narrow.
    ///
    /// The child's own deny/ask rules are added (denies union downward), but its
    /// allow rules are dropped — allows *intersect* downward, so a child can
    /// never grant itself read, write, or execute the parent lacked. Defaults
    /// tighten to the stricter of the two per action.
    ///
    /// This is deliberately *not* [`Policy::merge`], where an overlay may add
    /// allows to widen a base. Containment flows one way: no descendant, at any
    /// depth, can hold an effective allow the root did not — because `contain`
    /// only ever appends denies and tightens defaults, applying it again for a
    /// grandchild preserves the invariant.
    pub fn contain(&self, child: &Policy) -> Policy {
        let mut layers = self.layers.clone();
        for l in &child.layers {
            // Keep only the child's tightening rules; its allows grant nothing.
            let rules: Vec<Rule> = l
                .rules
                .iter()
                .filter(|r| r.effect != Effect::Allow)
                .cloned()
                .collect();
            if !rules.is_empty() {
                layers.push(Layer {
                    name: l.name.clone(),
                    rules,
                });
            }
        }
        Policy {
            layers,
            defaults: Defaults {
                read: self.defaults.read.max(child.defaults.read),
                write: self.defaults.write.max(child.defaults.write),
                exec: self.defaults.exec.max(child.defaults.exec),
                net: self.defaults.net.max(child.defaults.net),
            },
        }
    }

    /// Evaluate `act` against `target`, returning the effect with the rule and
    /// layer that produced it.
    ///
    /// Deny-first across all layers, then ask, then allow, then the tier
    /// default. Specificity does not matter — a broad deny beats a narrow
    /// allow, matching Claude Code's precedence.
    pub fn explain(&self, act: Act, target: &str) -> Verdict {
        // A network target arrives as `host:port`; a rule may name either form.
        // Trying both is what lets `allow_net("api.example.com")` cover whatever
        // port the URL resolved to, while `allow_net("api.example.com:443")`
        // still means that port and no other.
        let forms: Vec<&str> = match (act, target.rsplit_once(':')) {
            (Act::Net, Some((host, port)))
                if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) =>
            {
                vec![target, host]
            }
            _ => vec![target],
        };
        for effect in [Effect::Deny, Effect::Ask, Effect::Allow] {
            for layer in &self.layers {
                for rule in &layer.rules {
                    if rule.act == act
                        && rule.effect == effect
                        && forms.iter().any(|t| matches(&rule.pattern, t))
                    {
                        return Verdict {
                            effect,
                            rule: Some(rule.pattern.clone()),
                            layer: Some(layer.name.clone()),
                        };
                    }
                }
            }
        }
        Verdict {
            effect: match act {
                Act::Read => self.defaults.read,
                Act::Write => self.defaults.write,
                Act::Exec => self.defaults.exec,
                Act::Net => self.defaults.net,
            },
            rule: None,
            layer: None,
        }
    }

    /// Evaluate `act` against `target`. This *is* [`Policy::explain`] — the
    /// enforcement path and the explanation path are one function, so they
    /// cannot drift apart.
    pub fn check(&self, act: Act, target: &str) -> Verdict {
        self.explain(act, target)
    }
}

/// Does `pattern` match `target`, by full relative path or by basename?
///
/// Basename matching is what lets a bare `.env` deny `config/.env`, and mirrors
/// how the `find` tool already matches globs.
// ponytail: compiles the glob per check. Cache compiled regexes if a policy ever
// carries hundreds of rules; a handful per tool call is not worth the machinery.
fn matches(pattern: &str, target: &str) -> bool {
    let target = target.replace('\\', "/");
    let Ok(re) = glob_to_regex(pattern) else {
        return false; // a malformed glob matches nothing rather than everything
    };
    if re.is_match(&target) {
        return true;
    }
    match target.rsplit('/').next() {
        Some(base) if base != target => re.is_match(base),
        _ => false,
    }
}

/// Compile a glob (`*` any run including `/`, `?` one char) to an anchored regex.
fn glob_to_regex(glob: &str) -> Result<regex::Regex> {
    let mut re = String::from("(?s)^");
    for ch in glob.chars() {
        match ch {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
    }
    re.push('$');
    regex::Regex::new(&re).map_err(|e| Error::Config(format!("bad policy glob: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A base layer that allows the workspace but denies a secrets tree.
    fn base() -> Policy {
        Policy::default()
            .layer("base")
            .allow_read("src/*")
            .allow_write("src/*")
            .deny_read("secrets/*")
            .deny_write("secrets/*")
    }

    #[test]
    fn deny_beats_allow_even_when_both_match() {
        let p = Policy::default()
            .layer("l")
            .allow_write("src/*")
            .deny_write("src/generated/*");
        assert_eq!(p.check(Act::Write, "src/a.rs").effect, Effect::Allow);
        assert_eq!(
            p.check(Act::Write, "src/generated/x.rs").effect,
            Effect::Deny
        );
    }

    #[test]
    fn default_policy_denies_dotenv_read_inside_a_readable_tree() {
        let p = Policy::default().layer("l").allow_read("*");
        assert_eq!(p.check(Act::Read, "src/a.rs").effect, Effect::Allow);
        assert_eq!(p.check(Act::Read, ".env").effect, Effect::Deny);
        assert_eq!(p.check(Act::Read, "config/.env").effect, Effect::Deny);
        assert_eq!(p.check(Act::Read, "keys/id_rsa").effect, Effect::Deny);
    }

    #[test]
    fn serde_roundtrip_enforces_identically() {
        let p = base();
        let json = serde_json::to_string(&p).unwrap();
        let back: Policy = serde_json::from_str(&json).unwrap();
        for (act, path) in [
            (Act::Write, "src/a.rs"),
            (Act::Write, "secrets/key.txt"),
            (Act::Read, "secrets/key.txt"),
            (Act::Read, ".env"),
            (Act::Exec, "rustc"),
        ] {
            assert_eq!(
                p.check(act, path).effect,
                back.check(act, path).effect,
                "{act:?} {path}"
            );
        }
    }

    #[test]
    fn an_overlay_cannot_reallow_what_the_base_denied() {
        let overlay = Policy::default().layer("app").allow_write("secrets/*");
        let merged = base().merge(overlay);
        // The overlay allows it, the base denies it — deny is absolute.
        assert_eq!(
            merged.check(Act::Write, "secrets/key.txt").effect,
            Effect::Deny
        );
    }

    #[test]
    fn an_overlay_only_allow_grants_capability_the_base_lacked() {
        let overlay = Policy::default().layer("app").allow_write("docs/*");
        let merged = base().merge(overlay);
        assert_eq!(merged.check(Act::Write, "docs/x.md").effect, Effect::Allow);
        // and the base's own rules survive the merge
        assert_eq!(merged.check(Act::Write, "src/a.rs").effect, Effect::Allow);
    }

    #[test]
    fn merging_is_deterministic_and_merging_a_layer_with_itself_changes_nothing() {
        let a = base();
        let probes = [
            (Act::Write, "src/a.rs"),
            (Act::Write, "secrets/k"),
            (Act::Read, "docs/x"),
        ];
        let once = a.clone().merge(base());
        let twice = a.clone().merge(base()).merge(base());
        for (act, path) in probes {
            assert_eq!(a.check(act, path).effect, once.check(act, path).effect);
            assert_eq!(once.check(act, path).effect, twice.check(act, path).effect);
        }
    }

    #[test]
    fn an_empty_overlay_leaves_the_base_unchanged() {
        let merged = base().merge(Policy::default());
        for (act, path) in [(Act::Write, "src/a.rs"), (Act::Write, "secrets/k")] {
            assert_eq!(
                base().check(act, path).effect,
                merged.check(act, path).effect
            );
        }
    }

    #[test]
    fn explain_names_the_rule_and_the_layer_that_decided() {
        let merged = base().merge(Policy::default().layer("app").allow_write("docs/*"));

        let denied = merged.explain(Act::Write, "secrets/key.txt");
        assert_eq!(denied.effect, Effect::Deny);
        assert_eq!(denied.layer.as_deref(), Some("base"));
        assert_eq!(denied.rule.as_deref(), Some("secrets/*"));

        let allowed = merged.explain(Act::Write, "docs/x.md");
        assert_eq!(allowed.effect, Effect::Allow);
        assert_eq!(allowed.layer.as_deref(), Some("app"));

        // A path no rule mentions falls to the tier default, attributed to no layer.
        let fallback = merged.explain(Act::Write, "elsewhere/x");
        assert_eq!(fallback.effect, Effect::Ask);
        assert_eq!(fallback.layer, None);
    }

    // --- 0.5.0 containment merge: inherit-and-narrow only, downward. ---

    #[test]
    fn a_child_overlay_allow_cannot_reach_a_path_the_parent_denies() {
        // Parent denies secrets. A child that tries to allow it gains nothing —
        // allows intersect downward, so the deny still stands.
        let parent = base(); // denies secrets/*
        let child = Policy::permissive()
            .layer("child")
            .allow_read("secrets/*")
            .allow_write("secrets/*");
        let contained = parent.contain(&child);
        assert_eq!(
            contained.check(Act::Write, "secrets/key.txt").effect,
            Effect::Deny
        );
        assert_eq!(
            contained.check(Act::Read, "secrets/key.txt").effect,
            Effect::Deny
        );
    }

    #[test]
    fn a_child_overlay_allow_cannot_widen_a_parent_default() {
        // The real teeth of containment vs merge: the parent never allowed
        // docs/* writes (they fall to the Ask default). merge() would let a
        // child allow widen it to Allow; contain() must not.
        let parent = Policy::default().layer("parent").allow_write("src/*");
        let child = Policy::permissive().layer("child").allow_write("docs/*");
        assert_eq!(
            parent
                .clone()
                .merge(child.clone())
                .check(Act::Write, "docs/x.md")
                .effect,
            Effect::Allow,
            "merge widens (0.4.0 behaviour)"
        );
        assert_eq!(
            parent.contain(&child).check(Act::Write, "docs/x.md").effect,
            Effect::Ask,
            "contain does not widen"
        );
    }

    #[test]
    fn a_child_overlay_deny_narrows_the_parent() {
        // A child adds a deny the parent lacked; denies union downward, and the
        // child may narrow. Paths the child did not deny still follow the parent.
        let parent = Policy::default().layer("parent").allow_write("src/*");
        let child = Policy::permissive()
            .layer("child")
            .deny_write("src/generated/*");
        let contained = parent.contain(&child);
        assert_eq!(
            contained.check(Act::Write, "src/a.rs").effect,
            Effect::Allow
        );
        assert_eq!(
            contained.check(Act::Write, "src/generated/x.rs").effect,
            Effect::Deny
        );
    }

    #[test]
    fn containment_holds_downward_through_depth() {
        // A grandchild cannot re-open what the root denied, nor widen a root
        // default — the invariant holds at depth > 1, not just parent->child.
        let root = base(); // denies secrets/*, allows src/*
        let child = Policy::permissive()
            .layer("child")
            .deny_write("src/vendor/*");
        let grandchild = Policy::permissive()
            .layer("grandchild")
            .allow_write("secrets/*") // try to re-allow a root deny
            .allow_write("docs/*"); // try to widen past the root default
        let effective = root.contain(&child).contain(&grandchild);
        assert_eq!(
            effective.check(Act::Write, "secrets/key.txt").effect,
            Effect::Deny
        );
        assert_eq!(effective.check(Act::Write, "docs/x.md").effect, Effect::Ask);
        assert_eq!(
            effective.check(Act::Write, "src/vendor/x.rs").effect,
            Effect::Deny
        );
        assert_eq!(
            effective.check(Act::Write, "src/a.rs").effect,
            Effect::Allow
        );
    }

    #[test]
    fn net_is_denied_by_default_and_allowed_only_where_a_rule_says_so() {
        let p = Policy::default()
            .layer("egress")
            .allow_net("api.example.com");
        assert_eq!(
            p.check(Act::Net, "api.example.com:443").effect,
            Effect::Allow
        );
        assert_eq!(
            p.check(Act::Net, "evil.example.com:443").effect,
            Effect::Deny
        );
        // Deny is absolute across layers, network included.
        let tighter = p.layer("lockdown").deny_net("api.example.com");
        assert_eq!(
            tighter.check(Act::Net, "api.example.com:443").effect,
            Effect::Deny
        );
    }

    #[test]
    fn a_net_rule_matches_with_or_without_the_port() {
        let bare = Policy::default().layer("l").allow_net("api.example.com");
        assert_eq!(
            bare.check(Act::Net, "api.example.com:443").effect,
            Effect::Allow
        );
        assert_eq!(
            bare.check(Act::Net, "api.example.com:8080").effect,
            Effect::Allow
        );

        // A rule that names a port is honoured as written: that port only.
        let ported = Policy::default()
            .layer("l")
            .allow_net("api.example.com:443");
        assert_eq!(
            ported.check(Act::Net, "api.example.com:443").effect,
            Effect::Allow
        );
        assert_eq!(
            ported.check(Act::Net, "api.example.com:8080").effect,
            Effect::Deny
        );

        // Wildcards work on hosts the way they work on paths.
        let wild = Policy::default().layer("l").allow_net("*.example.com");
        assert_eq!(
            wild.check(Act::Net, "api.example.com:443").effect,
            Effect::Allow
        );
        assert_eq!(wild.check(Act::Net, "example.org:443").effect, Effect::Deny);
    }

    #[test]
    fn net_narrows_downward_and_never_widens() {
        let root = Policy::default().layer("root").allow_net("api.example.com");
        let child = Policy::permissive()
            .layer("child")
            .allow_net("evil.example.com") // a child allow grants nothing
            .deny_net("api.example.com"); // a child deny binds
        let effective = root.contain(&child);
        assert_eq!(
            effective.check(Act::Net, "evil.example.com:443").effect,
            Effect::Deny
        );
        assert_eq!(
            effective.check(Act::Net, "api.example.com:443").effect,
            Effect::Deny
        );
    }

    #[test]
    fn a_pre_0_8_policy_deserialises_with_network_denied() {
        // Exactly what 0.7.0 wrote: defaults with no `net` field at all.
        let old = r#"{"layers":[],"defaults":{"read":"allow","write":"ask","exec":"ask"}}"#;
        let p: Policy = serde_json::from_str(old).unwrap();
        assert_eq!(p.defaults.net, Effect::Deny);
        assert_eq!(
            p.check(Act::Net, "anywhere.example.com:443").effect,
            Effect::Deny
        );
    }

    #[test]
    fn net_rules_survive_a_serde_roundtrip() {
        let p = Policy::default()
            .layer("egress")
            .allow_net("api.example.com")
            .deny_net("evil.example.com")
            .ask_net("maybe.example.com");
        let back: Policy = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        for host in [
            "api.example.com:443",
            "evil.example.com:443",
            "maybe.example.com:443",
        ] {
            assert_eq!(p.check(Act::Net, host), back.check(Act::Net, host));
        }
    }

    #[test]
    fn permissive_still_enforces_nothing_including_the_network() {
        let p = Policy::permissive();
        assert!(p.is_permissive());
        assert_eq!(
            p.check(Act::Net, "anywhere.example.com:443").effect,
            Effect::Allow
        );
    }

    #[test]
    fn explain_and_check_never_disagree() {
        let p = base();
        for (act, path) in [
            (Act::Read, "src/a.rs"),
            (Act::Write, "secrets/k"),
            (Act::Write, "elsewhere/x"),
            (Act::Read, ".env"),
            (Act::Exec, "rustc"),
        ] {
            assert_eq!(p.check(act, path).effect, p.explain(act, path).effect);
        }
    }
}
