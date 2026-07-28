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

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{OnceLock, PoisonError, RwLock};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// What an action wants to do. Search tools (`grep`, `find`) filter their
/// results with [`Act::Read`], so a denied path cannot reach the model.
///
/// Four acts, and every checked thing in the crate maps onto exactly one of
/// them. Knowing which is how you write a rule for something whose name never
/// appears in the policy — a registered [`Tool`](crate::Tool) and an MCP tool are
/// both [`Act::Exec`] on their *name*, and a spreadsheet tool is
/// [`Act::Read`]/[`Act::Write`] on the path the model gave it:
///
/// ```
/// use io_harness::{Act, Effect, Policy};
///
/// let policy = Policy::permissive()
///     .layer("app")
///     // Files: the path, relative to the workspace root.
///     .deny_read("secrets/*")
///     .allow_write("src/*")
///     // Binaries the verification gate spawns, by name — and the same act
///     // decides whether a registered or MCP tool may be *called*.
///     .allow_exec("rustc")
///     .deny_exec("charge_credit_card")
///     // Outbound connections, by host or `host:port`. Naming the host alone
///     // covers whichever port a URL resolved to.
///     .allow_net("api.example.com");
///
/// assert_eq!(policy.check(Act::Read, "secrets/prod.env").effect, Effect::Deny);
/// assert_eq!(policy.check(Act::Exec, "charge_credit_card").effect, Effect::Deny);
/// assert_eq!(policy.check(Act::Net, "api.example.com:443").effect, Effect::Allow);
/// ```
///
/// What the exec act does *not* govern is what a thing does once it is running:
/// a registered tool runs in the harness's process with the embedding program's
/// privileges, and a stdio MCP server is a separate process that then dials what
/// it likes. The policy decides what starts, not what a started thing does.
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
///
/// The ordering is not decoration — it is how stacking two policies is defined.
/// [`Policy::merge`] and [`Policy::contain`] take the `max` of the two defaults
/// per act, so combining policies can only ever tighten them:
///
/// ```
/// use io_harness::{Act, Effect, Policy};
///
/// assert!(Effect::Allow < Effect::Ask && Effect::Ask < Effect::Deny);
///
/// // A permissive overlay cannot loosen a base's asking default.
/// let combined = Policy::default().merge(Policy::permissive());
/// assert_eq!(combined.check(Act::Write, "anything-unmatched").effect, Effect::Ask);
/// ```
///
/// The three effects also mean three different things at run time, and the
/// middle one is the only one a human ever sees: `Allow` proceeds silently,
/// `Ask` routes to the [`Approver`](crate::Approver), and `Deny` refuses without
/// consulting anyone — a denied action never reaches an approver, so an
/// [`ApproveAll`](crate::ApproveAll) cannot wave it through.
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
///
/// The basename half is why a bare filename is a *recursive* deny, and it is the
/// reason to construct a `Rule` deliberately rather than reaching for the
/// nearest-looking builder:
///
/// ```
/// use io_harness::{Act, Effect, Policy, Rule};
///
/// // `*` spans `/`, so this is not "one directory down".
/// let deny_anywhere = Rule { act: Act::Read, effect: Effect::Deny, pattern: ".env".into() };
/// let deny_one_tree = Rule { act: Act::Read, effect: Effect::Deny, pattern: "vendor/*".into() };
///
/// let policy = Policy::permissive()
///     .layer("secrets")
///     .rule(deny_anywhere.act, deny_anywhere.effect, deny_anywhere.pattern.clone())
///     .rule(deny_one_tree.act, deny_one_tree.effect, deny_one_tree.pattern.clone());
///
/// // Matched on the basename: every `.env` at every depth.
/// assert_eq!(policy.check(Act::Read, "deploy/staging/.env").effect, Effect::Deny);
/// // Matched on the full relative path, and `*` crosses directories.
/// assert_eq!(policy.check(Act::Read, "vendor/lib/src/main.rs").effect, Effect::Deny);
/// // Neither form matches, so the tier default decides.
/// assert_eq!(policy.check(Act::Read, "src/main.rs").effect, Effect::Allow);
/// ```
///
/// A `Rule` is also what an approver hands back in
/// [`Decision::Approve`](crate::Decision::Approve)`::remember`, and what a
/// deserialized operator config is made of — it is `Serialize`/`Deserialize` for
/// exactly that.
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
///
/// Name layers after *who wrote them*, not after what they do. When a run is
/// refused six weeks later, the trace names the layer, and "ops-baseline" sends
/// the reader to the right file while "denies" sends them nowhere:
///
/// ```
/// use io_harness::{Act, Effect, Layer, Policy, Rule};
///
/// // A layer built directly — what deserializing an operator's config file
/// // produces, as opposed to the builder methods a program writes inline.
/// let baseline = Layer {
///     name: "ops-baseline".into(),
///     rules: vec![
///         Rule { act: Act::Read, effect: Effect::Deny, pattern: "infra/*".into() },
///         Rule { act: Act::Write, effect: Effect::Deny, pattern: "infra/*".into() },
///     ],
/// };
///
/// let mut policy = Policy::permissive();
/// policy.layers.push(baseline);
/// let policy = policy.layer("app").allow_read("*").allow_write("*");
///
/// // The app's blanket allows do not lift the baseline's denies, and the verdict
/// // says whose rule stopped it.
/// let verdict = policy.explain(Act::Write, "infra/main.tf");
/// assert_eq!(verdict.effect, Effect::Deny);
/// assert_eq!(verdict.layer.as_deref(), Some("ops-baseline"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layer {
    /// Human-readable name, surfaced in explanations and the trace.
    pub name: String,
    /// The rules in this layer.
    pub rules: Vec<Rule>,
}

/// The default effect for an action no rule mentions.
///
/// This is the part of a policy that decides what happens to everything you did
/// *not* think of, so it is the part worth setting on purpose. Deny-by-default
/// makes the rule list exhaustive — anything unnamed is refused — which is the
/// shape an unattended run wants:
///
/// ```
/// use io_harness::{Act, Defaults, Effect, Policy};
///
/// let mut policy = Policy::permissive().layer("job").allow_read("src/*").allow_write("out/*");
/// policy.defaults = Defaults {
///     read: Effect::Deny,
///     write: Effect::Deny,
///     exec: Effect::Deny,
///     net: Effect::Deny,
/// };
///
/// // Named: allowed. Unnamed: refused, without ever asking a human.
/// assert_eq!(policy.check(Act::Read, "src/lib.rs").effect, Effect::Allow);
/// assert_eq!(policy.check(Act::Read, "/etc/passwd").effect, Effect::Deny);
/// ```
///
/// Two defaults it is easy to be surprised by. `Policy::default()` sets `write`
/// and `exec` to [`Effect::Ask`], so a run with no approver behind it stalls on
/// its first write unless a rule allows it outright. And `net` defaults to
/// [`Effect::Deny`] everywhere, including for a policy deserialized from a 0.7.0
/// config that has no `net` field — the harness contributes the configured
/// provider's host as its own layer, so the model is still reachable and nothing
/// else is.
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
///
/// The two `Option`s are the useful part: they distinguish "a rule someone wrote
/// stopped this" from "nothing matched, so the tier default did". Those need
/// different fixes — one edits a line of config, the other adds one — and a
/// refusal message that cannot tell them apart sends the reader to the wrong
/// file:
///
/// ```
/// use io_harness::{Act, Effect, Policy, Verdict};
///
/// fn explain_refusal(v: &Verdict, target: &str) -> String {
///     match (&v.rule, &v.layer) {
///         (Some(rule), Some(layer)) => format!("{target}: refused by `{rule}` in layer {layer}"),
///         // No rule mentioned it at all — the answer is in `Defaults`.
///         _ => format!("{target}: no rule matched; the tier default is {:?}", v.effect),
///     }
/// }
///
/// let policy = Policy::default().layer("app").deny_read("vendor/*");
///
/// assert_eq!(
///     explain_refusal(&policy.explain(Act::Read, "vendor/x.rs"), "vendor/x.rs"),
///     "vendor/x.rs: refused by `vendor/*` in layer app",
/// );
/// assert_eq!(
///     explain_refusal(&policy.explain(Act::Write, "src/x.rs"), "src/x.rs"),
///     "src/x.rs: no rule matched; the tier default is Ask",
/// );
/// ```
///
/// The same `Verdict` is what enforcement acts on — [`Policy::check`] *is*
/// [`Policy::explain`] — so an explanation can never describe a boundary
/// different from the one enforced. These fields are also what the trace records
/// on a refusal, so an audit six weeks later reads the same attribution.
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
///
/// The reason it is a *stack* rather than one rule list is that the rules
/// normally come from two people. An operator writes a base, an application
/// stacks its own needs on top, and the base has to keep holding — which it
/// does, because evaluation is deny-first across the whole stack and specificity
/// does not enter into it:
///
/// ```
/// use io_harness::{Act, Effect, Policy};
///
/// // Whoever runs the fleet. Shipped once, reused by every job.
/// let ops = Policy::permissive()
///     .layer("ops")
///     .deny_read("infra/*")
///     .deny_write("infra/*")
///     .deny_exec("kubectl");
///
/// // Whoever wrote this job. It asks for everything, as applications do.
/// let app = Policy::permissive()
///     .layer("app")
///     .allow_read("*")
///     .allow_write("*")
///     .allow_exec("rustc");
///
/// let policy = ops.merge(app);
///
/// // The app's `allow_read("*")` is broader and later, and still loses. A deny
/// // in any layer beats an allow in any other, so handing out `ops` is safe:
/// // nothing stacked on top can take it back.
/// assert_eq!(policy.check(Act::Read, "infra/prod.tf").effect, Effect::Deny);
/// assert_eq!(policy.check(Act::Exec, "kubectl").effect, Effect::Deny);
/// assert_eq!(policy.check(Act::Write, "src/lib.rs").effect, Effect::Allow);
/// ```
///
/// Which constructor to start from is a real decision, not a default:
///
/// * [`Policy::default`] — reads allowed, writes and execs [`Effect::Ask`], the
///   secret paths (`.env`, `*.pem`, `id_rsa`, `id_ed25519`, `*.key`) denied
///   outright, egress denied. The tiered starting point for an interactive run.
/// * [`Policy::permissive`] — enforces nothing, and is what
///   [`run`](crate::run) applies when the caller passes no policy at all. Start
///   here when you intend to write the whole boundary yourself.
///
/// For a [`run_tree`](crate::run_tree), [`Policy::contain`] is the one that
/// matters: a child inherits the parent's rules and may only narrow them, so no
/// descendant at any depth holds an allow the root did not.
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
                        && forms.iter().any(|t| matches(act, &rule.pattern, t))
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

/// Fold a path-valued pattern or target into the one form both sides are
/// compared in. Windows only, and applied identically to pattern and target so
/// there is a single definition of "the same path":
///
/// * a `\\?\` verbatim prefix is stripped (`\\?\UNC\srv\share` becomes
///   `\\srv\share`), because that prefix is something [`std::fs::canonicalize`]
///   adds and no human writes in a rule; then
/// * `\` becomes `/`, because on Windows both are directory separators.
///
/// Without this a deny built from a canonicalized [`std::path::Path`] — which is
/// what the harness itself and any caller doing `deny_read(format!("{}/*", p))`
/// produce — never matched the backslash target it was meant to cover, and the
/// read was *allowed*. A permission rule that misses fails open, so this is the
/// security-relevant half of matching, not a cosmetic tidy-up.
///
/// Deliberately a no-op on unix, where `\` is an ordinary character in a file
/// name: folding it to `/` there would make two genuinely different paths match,
/// which is the same fail-open bug pointed the other way.
#[cfg(windows)]
fn normalise_path(s: &str) -> Cow<'_, str> {
    let stripped = match s.strip_prefix(r"\\?\UNC\") {
        Some(rest) => Cow::Owned(format!(r"\\{rest}")),
        None => Cow::Borrowed(s.strip_prefix(r"\\?\").unwrap_or(s)),
    };
    match stripped.contains('\\') {
        true => Cow::Owned(stripped.replace('\\', "/")),
        false => stripped,
    }
}

/// See the Windows definition. On unix `\` is a legal filename character, so
/// normalising it would silently merge distinct paths — matching stays literal.
#[cfg(not(windows))]
fn normalise_path(s: &str) -> Cow<'_, str> {
    Cow::Borrowed(s)
}

/// Compiled globs, keyed by the whole (normalised) pattern text.
///
/// The key is whatever text [`matches`] hands over — already run through
/// [`normalise_path`] for a path act — so the key is always exactly the string
/// that was compiled. Two raw patterns that normalise to one form share an
/// entry, which is correct: they denote the same location.
///
/// Safe as a process-wide cache because compilation is a pure function of that
/// text and nothing else: no act, no effect, no layer, no target enters
/// [`glob_to_regex`], and the cache stores the regex, never a verdict. So a hit
/// can only ever hand back the regex that same pattern would have compiled to,
/// and a pattern appearing in two layers — or in a parent and the child that
/// [`Policy::contain`] narrows — still decides through its own rule's act,
/// effect, and layer position. A failed compile is cached as `None`, keeping a
/// malformed glob matching nothing exactly as before.
// ponytail: unbounded — one entry per distinct pattern text ever checked.
// Patterns come from policy configs, not from the model, so the set is small and
// fixed; bound it (LRU, or a per-Policy cache) only if a caller ever generates
// pattern text at runtime.
static GLOBS: OnceLock<RwLock<HashMap<String, Option<regex::Regex>>>> = OnceLock::new();

/// [`glob_to_regex`] memoised on the pattern text. `None` is a malformed glob.
fn compiled(pattern: &str) -> Option<regex::Regex> {
    let cache = GLOBS.get_or_init(Default::default);
    if let Some(hit) = cache
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .get(pattern)
    {
        return hit.clone();
    }
    // The error is dropped here as it always was — a bad glob is silent and
    // matches nothing; surfacing it would be a behaviour change, not a fix.
    let re = glob_to_regex(pattern).ok();
    cache
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(pattern.to_string(), re.clone());
    re
}

/// Does `pattern` match `target`, by full relative path or by basename?
///
/// Basename matching is what lets a bare `.env` deny `config/.env`, and mirrors
/// how the `find` tool already matches globs.
///
/// `act` decides whether the two sides are paths. [`Act::Read`] and
/// [`Act::Write`] target paths and are normalised (see [`normalise_path`]);
/// [`Act::Exec`] targets a binary *name* and [`Act::Net`] a host, where `\` is
/// not a separator and rewriting it would change what a rule means.
fn matches(act: Act, pattern: &str, target: &str) -> bool {
    let (pattern, target) = match act {
        Act::Read | Act::Write => (normalise_path(pattern), normalise_path(target)),
        Act::Exec | Act::Net => (Cow::Borrowed(pattern), Cow::Borrowed(target)),
    };
    let Some(re) = compiled(&pattern) else {
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
    fn the_glob_cache_cannot_make_one_pattern_answer_another_pattern_s_question() {
        // Patterns a sloppy key — a prefix, a basename, a normalised form, an
        // invented hash — would conflate. Each pair must keep deciding apart,
        // and must decide the same on the second pass, which runs off the cache.
        let p = Policy::permissive()
            .layer("l")
            .deny_read("secrets/*")
            .allow_read("secrets-public/*")
            .deny_read("a?.txt")
            .deny_read("build/*/out")
            .allow_read("build/x/out.bak");
        let probes = [
            ("secrets/k", Effect::Deny),
            ("secrets-public/k", Effect::Allow),
            ("ab.txt", Effect::Deny),
            ("abc.txt", Effect::Allow), // `?` is one char, `*` is any run
            ("build/x/out", Effect::Deny),
            ("build/x/out.bak", Effect::Allow),
        ];
        for pass in 0..2 {
            for (path, want) in probes {
                assert_eq!(p.check(Act::Read, path).effect, want, "pass {pass}: {path}");
            }
        }

        // The same pattern text in two layers with opposite effects: the cache
        // holds a regex, not a verdict, so deny-first still decides regardless
        // of which layer compiled the text first, in either stacking order.
        let allow_first = Policy::permissive()
            .layer("a")
            .allow_exec("tool")
            .layer("b")
            .deny_exec("tool");
        let deny_first = Policy::permissive()
            .layer("b")
            .deny_exec("tool")
            .layer("a")
            .allow_exec("tool");
        for p in [&allow_first, &deny_first] {
            let v = p.check(Act::Exec, "tool");
            assert_eq!(v.effect, Effect::Deny);
            assert_eq!(v.layer.as_deref(), Some("b"));
        }
        // And a layer whose only rule allows that same text still allows it —
        // it did not inherit the other stack's deny along with the regex.
        let alone = Policy::permissive().layer("a").allow_exec("tool");
        assert_eq!(alone.check(Act::Exec, "tool").effect, Effect::Allow);

        // Same text again across a containment boundary: the child's allow is
        // dropped, the parent's deny stands, and the parent alone is unchanged.
        let parent = Policy::permissive().layer("parent").deny_write("shared/*");
        let child = Policy::permissive().layer("child").allow_write("shared/*");
        assert_eq!(
            parent.contain(&child).check(Act::Write, "shared/f").effect,
            Effect::Deny
        );
        assert_eq!(child.check(Act::Write, "shared/f").effect, Effect::Allow);
    }

    // --- 0.9.1: separator-agnostic path matching on Windows (fail-open fix). ---

    #[cfg(windows)]
    #[test]
    fn a_path_rule_matches_whichever_separator_either_side_spelled() {
        // The shape that failed open: a pattern built from a canonicalized Path
        // (backslashes, verbatim prefix) with a `/*` suffix appended by the
        // caller, against the backslash target canonicalize hands back.
        let p = Policy::permissive()
            .layer("base")
            .deny_read(r"\\?\C:\Users\me\skills/*");
        assert_eq!(
            p.check(Act::Read, r"\\?\C:\Users\me\skills\beta\SKILL.md")
                .effect,
            Effect::Deny
        );
        // A human writing the same rule in the plain, forward-slash form covers
        // that verbatim target too.
        let plain = Policy::permissive().layer("base").deny_read("C:/secrets/*");
        assert_eq!(
            plain.check(Act::Read, r"\\?\C:\secrets\token.txt").effect,
            Effect::Deny
        );
        // And backslash-for-backslash, the form 0.4.0 through 0.9.0 missed.
        let backslash = Policy::permissive()
            .layer("base")
            .deny_write(r"C:\secrets\*");
        assert_eq!(
            backslash.check(Act::Write, r"C:\secrets\token.txt").effect,
            Effect::Deny
        );
        // Symmetric: the same rule catches the forward-slash spelling too.
        assert_eq!(
            backslash.check(Act::Write, "C:/secrets/token.txt").effect,
            Effect::Deny
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_unc_share_matches_with_or_without_the_verbatim_prefix() {
        let p = Policy::permissive()
            .layer("base")
            .deny_read(r"\\srv\share\*");
        assert_eq!(
            p.check(Act::Read, r"\\?\UNC\srv\share\secret.txt").effect,
            Effect::Deny
        );
    }

    #[cfg(windows)]
    #[test]
    fn normalising_separators_does_not_make_two_different_paths_match() {
        // The negative half: folding `\` to `/` must not widen a rule onto a
        // path it never covered. If any of these turned Deny, normalisation is
        // over-matching and the fix is worse than the bug.
        let p = Policy::permissive()
            .layer("base")
            .deny_read(r"C:\secrets\*")
            .deny_read(r"C:\a\b.txt");
        for allowed in [
            r"C:\secrets-public\k",  // prefix, not the same directory
            r"D:\secrets\k",         // different volume
            r"C:\other\secrets.txt", // basename is not the directory
            r"C:\a\b.txt.bak",       // suffix past the pattern
            r"C:\ab.txt",            // the separator is not nothing
        ] {
            assert_eq!(
                p.check(Act::Read, allowed).effect,
                Effect::Allow,
                "{allowed} must not be caught by a rule for a different path"
            );
        }
        // Exec and net are names, not paths: a `\` in one stays a literal
        // character, so 0.9.0's routing of them through check() is untouched.
        let names = Policy::permissive()
            .layer("base")
            .deny_exec(r"tools\build.exe")
            .deny_net(r"a\b.example.com");
        assert_eq!(
            names.check(Act::Exec, "tools/build.exe").effect,
            Effect::Allow
        );
        assert_eq!(
            names.check(Act::Exec, r"tools\build.exe").effect,
            Effect::Deny
        );
        assert_eq!(
            names.check(Act::Net, "a/b.example.com:443").effect,
            Effect::Allow
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn a_backslash_is_an_ordinary_filename_character_on_unix() {
        // `a\b.txt` is one file here, not `a/b.txt`. Normalising would merge
        // two distinct paths — a fail-open bug in the other direction — so the
        // Windows folding is cfg'd out and matching stays literal.
        let p = Policy::permissive().layer("base").deny_read(r"a/b.txt");
        assert_eq!(p.check(Act::Read, r"a\b.txt").effect, Effect::Allow);
        assert_eq!(p.check(Act::Read, "a/b.txt").effect, Effect::Deny);

        // And a rule naming the literal backslash catches only it.
        let literal = Policy::permissive().layer("base").deny_read(r"a\b.txt");
        assert_eq!(literal.check(Act::Read, r"a\b.txt").effect, Effect::Deny);
        assert_eq!(literal.check(Act::Read, "a/b.txt").effect, Effect::Allow);
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
