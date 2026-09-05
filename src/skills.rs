//! Skills — instructions that shape how the agent approaches a class of task.
//!
//! A skill is a markdown file. Its body is guidance, not code: "when editing a
//! migration, always write the down-migration first", "our JSON responses use
//! snake_case", "prefer the repository's existing test fixtures". The agent is
//! told what skills exist and reads the one it judges relevant.
//!
//! # Why not just put it in the system prompt
//!
//! Because a caller with twenty of them would pay for all twenty on every turn
//! of every run. Only each skill's *name and one-line description* go into the
//! prompt; a body is loaded on demand through
//! [`READ_SKILL_TOOL`](crate::tools::READ_SKILL_TOOL) and enters the
//! observations once. Which skill is relevant is the model's judgement — the
//! harness does not rank, match, or auto-inject. Automatic relevance selection
//! is a context-construction question and is deliberately not here.
//!
//! # Instructions, never execution
//!
//! Nothing in a skill runs. A skill that says "run `rm -rf /`" is a sentence the
//! model reads, and any action it then takes goes through the same policy every
//! other action does. Anything that should actually *do* something is a
//! [`Tool`](crate::tools::Tool), where the permission layer can see it.
//!
//! # Layout
//!
//! Both conventions in common use are accepted, so a directory written for
//! another agent tool usually works unchanged:
//!
//! ```text
//! skills/
//!   migrations.md          -> skill "migrations"
//!   api-style/
//!     SKILL.md             -> skill "api-style"
//! ```
//!
//! Optional YAML frontmatter names the skill and describes it. Without
//! frontmatter the name comes from the filename (or its directory, for
//! `SKILL.md`) and the description from the first prose line.
//!
//! ```text
//! ---
//! name: migrations
//! description: How to write a reversible database migration in this repo.
//! ---
//!
//! Always write the down-migration first...
//! ```

use std::path::{Component, Path, PathBuf};

use crate::error::{Error, Result};

/// How many skills one directory may hold.
///
/// A hard ceiling rather than a truncation: a caller who drops 500 files in a
/// directory should hear that the set was rejected, not run for an hour on a
/// silently chosen 64 of them and wonder why the agent ignored the rest. Every
/// skill's name and description is in the system prompt on every turn, so the
/// count is a real cost, not a bookkeeping limit.
pub const MAX_SKILLS: usize = 64;

/// How much of one description is kept for the prompt catalogue.
///
/// A description is a line, and a caller who writes an essay in the
/// `description:` field would otherwise put that essay in every request.
const DESCRIPTION_CAP: usize = 240;

/// One discovered skill. The body is not held here — it is read when the agent
/// asks for it, so the read passes the permission policy at the moment it
/// happens rather than at discovery.
///
/// The split matters for what a run costs. `name` and `description` are what go
/// into the system prompt, on every turn, for every skill; the file at `path` is
/// loaded only if the model asks for that one. Twenty skills therefore cost
/// twenty lines a turn, not twenty bodies.
///
/// ```no_run
/// use io_harness::Skills;
///
/// # fn demo() -> io_harness::Result<()> {
/// for skill in Skills::discover("./skills")?.iter() {
///     // `path` is absolute, and it is the path the policy decides on when
///     // the agent asks to read it — so `deny_read` over a subdirectory of
///     // skills is a skill the model can see listed and cannot open.
///     println!("{} — {}\n  {}", skill.name, skill.description, skill.path.display());
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Skill {
    /// How the agent refers to it: the frontmatter `name`, else the file stem,
    /// else the containing directory for a `SKILL.md`.
    pub name: String,
    /// One line for the prompt catalogue: the frontmatter `description`, else
    /// the first prose line of the body.
    pub description: String,
    /// The file the body lives in. Absolute, and what the policy decides on when
    /// the agent asks to read it.
    pub path: PathBuf,
    /// The directory a companion file asked for by name resolves beneath, and
    /// may not leave (0.73.0).
    ///
    /// A skill file is usually not the whole skill: it points at a checklist, a
    /// worked example, a longer reference sitting beside it. This is the root
    /// those siblings are looked up under, and the boundary the resolver behind
    /// `read_skill` refuses to cross.
    ///
    /// - **Contributed by a plugin**: the *bundle's* root — the directory its
    ///   manifest was read from, not its `skills/` directory. A bundle that
    ///   keeps `shared/` beside `skills/` is a normal layout, so the whole
    ///   bundle is in reach of any skill it contributes.
    /// - **Discovered through [`TaskContract::with_skills`](crate::TaskContract::with_skills)**:
    ///   the skill's own directory — the subdirectory holding its `SKILL.md`,
    ///   or, for a top-level `*.md` skill, the skills directory itself, which is
    ///   the only directory such a skill has.
    ///
    /// Canonical for a discovered skill, since it is [`Skill::path`]'s parent;
    /// for a contributed one it is the bundle root as the manifest was loaded
    /// from, which is why the resolver canonicalises both sides rather than
    /// trusting this to already be canonical.
    pub root: PathBuf,
}

/// What a companion path asked for by name turned out to be — the outcome of
/// [`resolve_under`] (0.73.0).
///
/// Three outcomes rather than an `Option`, because a file that is not there and
/// a file that is somewhere it may not be are different events and must read
/// differently in the trace: one is a typo, the other is the skill reaching out
/// of its bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SkillFile {
    /// Inside the root. The absolute, canonical path to open.
    Resolved(PathBuf),
    /// Refused: absolute, or climbing out with `..`, or a symlink whose target
    /// canonicalises outside the root. Carries what was asked for, verbatim, so
    /// the observation can name it — never the resolved location, which is what
    /// the refusal exists to keep from being disclosed.
    Outside(String),
    /// Nothing is there. Carries what was asked for. Not an escape: a skill
    /// pointing at a file it no longer ships is a mistake, not an attack, and
    /// saying "refused" would send the operator hunting for a breach.
    Missing(String),
}

/// Resolve `rel` beneath `root`, refusing anything that leaves it.
///
/// The order matters, and each step catches what the one before it cannot:
///
/// 1. An absolute path is refused by its [`Component`]s, not by
///    [`Path::is_absolute`] — on Windows `is_absolute` is false for `/etc/passwd`,
///    which would make it a *relative* join and let it through.
/// 2. Any `..` component is refused lexically, wherever it sits — `a/../../x`
///    included. Refused, never clamped: clamping silently rewrites the request
///    into something the caller did not ask for.
/// 3. Both the joined path and the root are then **canonicalised** and compared.
///    This is the actual defence. Steps 1 and 2 cannot see through a symlink
///    sitting inside the bundle that points at the operator's home directory;
///    canonicalising both sides is what does. It also keeps a symlink that stays
///    *inside* the root legal, which a bundle is entitled to use.
///
/// A path that does not exist comes back [`SkillFile::Missing`], never
/// [`SkillFile::Outside`] — `canonicalize` fails on both, and collapsing them
/// would make every typo look like an attempted escape.
pub(crate) fn resolve_under(root: &Path, rel: &str) -> SkillFile {
    let mut joined = root.to_path_buf();
    for component in Path::new(rel).components() {
        match component {
            // Absolute, or climbing out. Both are refused before any filesystem
            // call, so a refused path is never even stat'ed.
            Component::Prefix(_) | Component::RootDir | Component::ParentDir => {
                return SkillFile::Outside(rel.to_string());
            }
            Component::CurDir => {}
            Component::Normal(part) => joined.push(part),
        }
    }

    // The root has to be canonical too, or a root reached through a symlink
    // (`/tmp` on macOS, a bind-mounted checkout) would make every legal file
    // look like an escape.
    let (Ok(root), Ok(resolved)) = (root.canonicalize(), joined.canonicalize()) else {
        return SkillFile::Missing(rel.to_string());
    };
    if resolved.starts_with(&root) {
        SkillFile::Resolved(resolved)
    } else {
        SkillFile::Outside(rel.to_string())
    }
}

/// Resolve a companion path for one skill, trying the skill's own directory
/// before its root (0.73.0).
///
/// Both conventions are in use and a bundle usually carries both at once:
/// per-skill prose in a `references/` directory beside the skill's own
/// `SKILL.md`, and bundle-wide prose in a `shared/` directory beside the whole
/// `skills/` tree. A skill saying *read `references/tools.md`* means the one
/// next to itself; a skill saying *read `shared/state-model.md`* means the
/// bundle's. Resolving under a single root can serve one of those and not the
/// other, so both are tried, nearest first.
///
/// **The boundary does not widen.** Each candidate goes through
/// [`resolve_under`], and a skill's own directory is inside its root — for a
/// contributed skill the bundle root, for a standalone one the skill's own
/// directory, where the two candidates coincide and the second try is skipped.
/// So every outcome is still a path inside the root, and an absolute path, a
/// `..` or an escaping symlink is refused by whichever candidate answers.
///
/// Nearest-first also means a skill cannot be shadowed by a bundle-level file of
/// the same name: its own copy wins.
pub(crate) fn resolve_companion(skill: &Skill, rel: &str) -> SkillFile {
    let own = skill.path.parent().unwrap_or(skill.root.as_path());
    if own != skill.root {
        if let found @ SkillFile::Resolved(_) = resolve_under(own, rel) {
            return found;
        }
    }
    resolve_under(&skill.root, rel)
}

/// Whether an entry sitting in a skills directory leads out of it (0.80.0).
///
/// L13 confined the paths a workspace *declares* — `run.skills`,
/// `run.templates`, a plugin's `path` — and stopped at the declaration. The walk
/// *under* an accepted root was never confined, so a subdirectory that is a
/// symbolic link to somewhere else was descended, and the `SKILL.md` it holds
/// became a name and a description in the system prompt on every turn of every
/// run. That is the whole of what a skill has to reach to be worth planting:
/// the catalogue is prompt text, and it is sent before the model has done
/// anything a policy could refuse.
///
/// A top-level `*.md` entry is refused on the same test, because it is the same
/// hole with one character changed — `evil.md -> /elsewhere/notes.md` — and it
/// is the worse half: [`Skills::discover`] canonicalises what it finds, so such
/// a skill's [`Skill::root`] would be a directory *outside* the accepted one,
/// and every companion file it then names resolves under that root rather than
/// under the operator's. The cost is that a skill symlinked in from a dotfiles
/// checkout no longer loads; it is named in the warning, and pointing
/// `with_skills` at the directory the links come from loads it.
///
/// Only a symbolic link pays for a `canonicalize` — an ordinary file or
/// directory is answered by the `symlink_metadata` alone, so a full directory
/// costs one `lstat` per entry and nothing else. A link whose target will not
/// canonicalise — dangling, or unreadable — counts as leaving: there is nothing
/// to read through it either way, and one boolean is the whole answer the caller
/// wants.
fn escapes_root(root: &Path, entry: &Path) -> bool {
    if !entry
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
    {
        return false;
    }
    // The root is canonicalised too, or a skills directory reached through a
    // link — `/tmp` on macOS, a bind-mounted checkout — would make every entry
    // under it look like an escape.
    let (Ok(root), Ok(target)) = (root.canonicalize(), entry.canonicalize()) else {
        return true;
    };
    !target.starts_with(&root)
}

/// The skills discovered for one run.
///
/// Both layout conventions in common use are discovered, so a directory
/// written for another agent tool usually works unchanged. Given:
///
/// ```text
/// skills/
///   migrations.md          -> skill "migrations"
///   api-style/
///     SKILL.md             -> skill "api-style"
///   assets/                -> ignored: a directory with no SKILL.md in it
///   notes.txt              -> ignored: not markdown
/// ```
///
/// every top-level `*.md` file is a skill, and every subdirectory holding a
/// `SKILL.md` is one too. A subdirectory without a `SKILL.md` is skipped, as is
/// any file that is not markdown — but note that a top-level `README.md` *is*
/// discovered as a skill, because the rule is the extension and nothing else.
/// Keep prose that is not a skill out of the directory or one directory down.
///
/// The name comes from the frontmatter, else the file stem, else the containing
/// directory for a `SKILL.md`:
///
/// ```no_run
/// use io_harness::Skills;
///
/// # fn demo() -> io_harness::Result<()> {
/// let skills = Skills::discover("./skills")?;
/// assert_eq!(skills.names(), vec!["api-style", "migrations"]); // sorted by name
///
/// // The catalogue is the whole prompt cost: one line per skill, no bodies.
/// // Byte-identical across runs on the same directory, because discovery
/// // sorts and `read_dir` order does not.
/// println!("{}", skills.catalog());
/// # Ok(())
/// # }
/// ```
///
/// In a run you point the contract at the directory rather than discovering it
/// yourself. Discovery then happens at run start, so a path that does not
/// exist, is not a directory, holds more than [`MAX_SKILLS`], or holds two
/// skills of the same name fails the run with
/// [`Error::Config`] naming it — a rejected set, never a silently truncated
/// one.
///
/// ```
/// use io_harness::{TaskContract, Verification};
///
/// let contract = TaskContract::workspace(
///     "add a migration for the new column",
///     "/path/to/repo",
/// )
/// .with_verification(Verification::WorkspaceFileContains {
///     file: "migrations/latest.sql".into(),
///     needle: "DROP".into(),
/// })
/// .with_skills("./skills");
/// # let _ = contract;
/// ```
///
/// Cheap to clone and shared by a whole 0.5.0 tree, so a child is offered the
/// same catalogue its parent was.
#[derive(Debug, Clone, Default)]
pub struct Skills {
    skills: Vec<Skill>,
}

impl Skills {
    /// No skills. What a contract that configures none carries, and what makes
    /// [`READ_SKILL_TOOL`](crate::tools::READ_SKILL_TOOL) not be offered at all.
    pub fn none() -> Self {
        Self::default()
    }

    /// Discover every skill under `dir`, sorted by name.
    ///
    /// Fails with [`Error::Config`] when `dir` does not exist, is not a
    /// directory, holds more than [`MAX_SKILLS`], or holds two skills with the
    /// same name — an ambiguous catalogue is a configuration mistake, and
    /// resolving it by picking one silently is how an operator ends up debugging
    /// why their agent read the wrong instructions.
    ///
    /// **The walk stays inside `dir` (0.80.0).** An entry that is a symbolic
    /// link whose target sits outside the directory is skipped and warned about
    /// through `tracing`, file and subdirectory alike. Confining the *declared*
    /// path is not enough on its own: a `skills/notes -> ../../elsewhere` inside
    /// an accepted directory put a stranger's `SKILL.md` into the system prompt,
    /// which is prompt text sent before the model has done anything a policy
    /// could refuse. Skipped rather than fatal, so one stray link does not cost
    /// the operator every other skill in the directory.
    pub fn discover(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        if !dir.exists() {
            return Err(Error::Config(format!(
                "skills directory {} does not exist",
                dir.display()
            )));
        }
        if !dir.is_dir() {
            return Err(Error::Config(format!(
                "skills path {} is not a directory; point with_skills at a directory of markdown \
                 files, not at one file",
                dir.display()
            )));
        }

        // Sorted so the catalogue — and therefore the prompt — is byte-identical
        // across runs on the same directory. `read_dir` order is not.
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        entries.sort();

        let mut skills: Vec<Skill> = Vec::new();
        for path in entries {
            // The walk stays inside the directory the caller accepted. Skipped
            // rather than an error: a stray link in a skills directory is an
            // operator's own layout far more often than an attack, and failing
            // the run would take the other sixty-three skills down with it. The
            // trace names the link's own path and never its target — the same
            // rule `SkillFile::Outside` holds, for the same reason.
            if escapes_root(dir, &path) {
                tracing::warn!(
                    "skills directory {}: {} leads outside it and was skipped. Every skill's \
                     name and description is sent on every turn, so discovery does not follow a \
                     link out of the directory it was pointed at — point `with_skills` at the \
                     directory the link comes from instead",
                    dir.display(),
                    path.display()
                );
                continue;
            }
            let file = if path.is_dir() {
                let candidate = path.join("SKILL.md");
                if !candidate.is_file() {
                    continue;
                }
                candidate
            } else if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("md"))
            {
                path.clone()
            } else {
                // Not a skill: a .gitkeep, a .txt, an editor's leftovers. The
                // test is the extension and nothing else, so a `README.md`
                // sitting here *is* discovered as one — documented on `Skills`
                // rather than special-cased, because a name-based exception
                // would be the start of a list nobody can predict.
                continue;
            };

            let text = std::fs::read_to_string(&file)?;
            let (front_name, front_desc, body) = split_front_matter(&text);
            let fallback_name = default_name(&file);
            let name = front_name.unwrap_or(fallback_name);
            let description = front_desc
                .or_else(|| first_prose_line(body))
                .unwrap_or_else(|| "(no description)".to_string());

            // Absolute so the path in the trace and the path the policy decides
            // on do not depend on the process's working directory.
            let path = std::fs::canonicalize(&file).unwrap_or(file);
            skills.push(Skill {
                name,
                description: clamp(&description, DESCRIPTION_CAP),
                // A standalone skill reaches beneath its own directory: the one
                // holding its `SKILL.md`, or — for a top-level `*.md` skill —
                // the skills directory, the only directory it has. A plugin's
                // skills get this overwritten with the bundle root by
                // `namespaced`.
                root: path.parent().unwrap_or(&path).to_path_buf(),
                path,
            });

            if skills.len() > MAX_SKILLS {
                return Err(Error::Config(format!(
                    "skills directory {} holds more than {MAX_SKILLS} skills. Every skill's name \
                     and description is sent on every turn, so the set is rejected rather than \
                     silently reduced — split the directory or point at a smaller one",
                    dir.display()
                )));
            }
        }

        skills.sort_by(|a, b| a.name.cmp(&b.name));
        for pair in skills.windows(2) {
            if pair[0].name == pair[1].name {
                return Err(Error::Config(format!(
                    "two skills are both named {:?} ({} and {}); the agent addresses a skill by \
                     name, so the set is ambiguous",
                    pair[0].name,
                    pair[0].path.display(),
                    pair[1].path.display()
                )));
            }
        }
        Ok(Self { skills })
    }

    /// Every name prefixed with the plugin that contributed it, and every root
    /// widened to the bundle's (0.35.0, root 0.73.0).
    ///
    /// Applied once, as a bundle loads, so a contributed skill cannot occupy a
    /// name the operator already uses and the catalogue the model reads says
    /// which bundle each skill came from.
    ///
    /// `root` is the bundle's own directory rather than its `skills/` — this is
    /// the one place that knows a set of skills came from a plugin *and* knows
    /// which, so it is where the widening belongs. A bundle that keeps `shared/`
    /// beside `skills/` is a normal layout, and a skill that cannot reach it
    /// could only point at files it duplicates. See [`Skill::root`].
    #[must_use]
    pub(crate) fn namespaced(mut self, plugin: &str, root: &Path) -> Self {
        for skill in &mut self.skills {
            skill.name = crate::plugin::namespaced(plugin, &skill.name);
            skill.root = root.to_path_buf();
        }
        self
    }

    /// This catalogue and `other`, sorted by name (0.35.0).
    ///
    /// Refuses a duplicate name for the reason [`Skills::discover`] does: an
    /// ambiguous catalogue resolved by picking one silently is how an operator
    /// ends up debugging why their agent read the wrong instructions. Namespacing
    /// is what keeps two bundles from ever reaching this.
    pub(crate) fn merged(mut self, other: Self) -> Result<Self> {
        self.skills.extend(other.skills);
        self.skills.sort_by(|a, b| a.name.cmp(&b.name));
        for pair in self.skills.windows(2) {
            if pair[0].name == pair[1].name {
                return Err(Error::Config(format!(
                    "two skills are both named {:?}: {} and {}",
                    pair[0].name,
                    pair[0].path.display(),
                    pair[1].path.display()
                )));
            }
        }
        Ok(self)
    }

    /// True if nothing was discovered or configured.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// How many skills are available.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// The skill the agent named, if it exists.
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// Every discovered skill, sorted by name.
    pub fn iter(&self) -> impl Iterator<Item = &Skill> {
        self.skills.iter()
    }

    /// The available names, for telling the model what it can ask for.
    pub fn names(&self) -> Vec<&str> {
        self.skills.iter().map(|s| s.name.as_str()).collect()
    }

    /// The catalogue that goes into the system prompt: one line per skill, name
    /// and description, no bodies.
    pub fn catalog(&self) -> String {
        self.skills
            .iter()
            .map(|s| format!("- {}: {}", s.name, s.description))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The name a file implies when its frontmatter does not give one: the directory
/// for a `SKILL.md`, otherwise the file stem.
fn default_name(file: &Path) -> String {
    let stem = file
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if stem.eq_ignore_ascii_case("SKILL") {
        if let Some(parent) = file.parent().and_then(|p| p.file_name()) {
            return parent.to_string_lossy().to_string();
        }
    }
    stem
}

/// Split optional YAML frontmatter off the front of a skill file, returning the
/// `name`, the `description`, and the body that follows.
///
/// Deliberately not a YAML parser. Two scalar keys is the whole contract, and a
/// dependency that can evaluate anchors and merge keys is a large surface to
/// take on for that. A file whose frontmatter is unterminated is treated as
/// having none — its whole text is body — because guessing where an operator
/// meant the fence to close is worse than falling back to the filename.
///
/// Supports `key: value`, YAML block scalars (`key: >` / `key: |`), and plain
/// continuation lines, since a `description:` long enough to wrap is common.
pub(crate) fn split_front_matter(text: &str) -> (Option<String>, Option<String>, &str) {
    let stripped = text
        .strip_prefix("---\n")
        .or_else(|| text.strip_prefix("---\r\n"));
    let Some(rest) = stripped else {
        return (None, None, text);
    };

    // Find the closing fence. Byte offsets are tracked so the body can be
    // returned as a borrowed slice of the original.
    let mut offset = 0usize;
    let mut front_end = None;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']).trim() == "---" {
            front_end = Some((offset, offset + line.len()));
            break;
        }
        offset += line.len();
    }
    let Some((front_len, body_start)) = front_end else {
        return (None, None, text);
    };

    let front = &rest[..front_len];
    let body = &rest[body_start..];

    let mut name = None;
    let mut description = None;
    // Which key the current continuation lines belong to.
    let mut open: Option<&str> = None;
    let mut buffer = String::new();

    let flush = |key: Option<&str>,
                 buffer: &mut String,
                 name: &mut Option<String>,
                 description: &mut Option<String>| {
        let value = buffer.trim().to_string();
        buffer.clear();
        if value.is_empty() {
            return;
        }
        match key {
            Some("name") => *name = Some(value),
            Some("description") => *description = Some(value),
            _ => {}
        }
    };

    for raw in front.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        let indented = line.starts_with(' ') || line.starts_with('\t');
        // A continuation of the key above: an indented line, or an unindented
        // one with no `key:` of its own.
        let starts_key = line
            .find(':')
            .is_some_and(|i| !line[..i].trim().is_empty() && !line[..i].contains(' '));
        if indented || !starts_key {
            if open.is_some() {
                if !buffer.is_empty() {
                    buffer.push(' ');
                }
                buffer.push_str(line.trim());
            }
            continue;
        }

        flush(open, &mut buffer, &mut name, &mut description);
        let (key, value) = line.split_once(':').expect("starts_key found one");
        let key = key.trim();
        let value = value.trim();
        open = match key {
            "name" => Some("name"),
            "description" => Some("description"),
            // A key this reader does not care about (`metadata:`, `allowed-tools:`)
            // still has to be tracked, so its own continuation lines are not
            // appended to whichever key came before it.
            _ => Some("other"),
        };
        // `>` and `|` say "the value is the indented block below"; anything else
        // is the value itself.
        if value != ">" && value != "|" && value != ">-" && value != "|-" {
            buffer.push_str(value);
        }
    }
    flush(open, &mut buffer, &mut name, &mut description);

    (name, description, body)
}

/// The first line of a body that reads as prose, for a file with no
/// `description`. Markdown heading markers and blockquote markers are stripped
/// so a body opening with `# Migrations` describes itself as "Migrations".
pub(crate) fn first_prose_line(body: &str) -> Option<String> {
    body.lines()
        .map(|l| l.trim().trim_start_matches(['#', '>']).trim())
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// Keep a description to one line's worth of characters, at a char boundary.
pub(crate) fn clamp(s: &str, cap: usize) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.len() <= cap {
        return one_line;
    }
    let mut end = cap;
    while end > 0 && !one_line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &one_line[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn front(text: &str) -> (Option<String>, Option<String>, String) {
        let (n, d, b) = split_front_matter(text);
        (n, d, b.to_string())
    }

    #[test]
    fn plain_frontmatter_is_read_and_the_body_follows() {
        let (name, desc, body) = front("---\nname: alpha\ndescription: does a thing\n---\nBODY\n");
        assert_eq!(name.as_deref(), Some("alpha"));
        assert_eq!(desc.as_deref(), Some("does a thing"));
        assert_eq!(body, "BODY\n");
    }

    #[test]
    fn crlf_line_endings_parse() {
        let (name, desc, body) =
            front("---\r\nname: alpha\r\ndescription: does a thing\r\n---\r\nBODY\r\n");
        assert_eq!(name.as_deref(), Some("alpha"));
        assert_eq!(desc.as_deref(), Some("does a thing"));
        assert_eq!(body, "BODY\r\n");
    }

    #[test]
    fn a_description_spanning_lines_is_joined() {
        let (_, desc, _) =
            front("---\nname: alpha\ndescription: >\n  first part\n  second part\n---\nBODY\n");
        assert_eq!(desc.as_deref(), Some("first part second part"));
    }

    #[test]
    fn a_plain_continuation_line_is_joined_too() {
        let (_, desc, _) = front("---\ndescription: first part\n  second part\n---\nBODY\n");
        assert_eq!(desc.as_deref(), Some("first part second part"));
    }

    #[test]
    fn a_later_key_does_not_absorb_the_description() {
        let (name, desc, _) =
            front("---\nname: alpha\ndescription: the real one\nmetadata:\n  type: x\n---\nBODY\n");
        assert_eq!(name.as_deref(), Some("alpha"));
        assert_eq!(desc.as_deref(), Some("the real one"));
    }

    #[test]
    fn absent_frontmatter_leaves_the_whole_file_as_body() {
        let (name, desc, body) = front("# Heading\n\nprose\n");
        assert!(name.is_none());
        assert!(desc.is_none());
        assert_eq!(body, "# Heading\n\nprose\n");
    }

    #[test]
    fn unterminated_frontmatter_is_treated_as_no_frontmatter() {
        let text = "---\nname: alpha\ndescription: never closed\n\nBODY\n";
        let (name, desc, body) = front(text);
        assert!(name.is_none(), "an unclosed fence must not be trusted");
        assert!(desc.is_none());
        assert_eq!(body, text, "the whole file is the body");
    }

    #[test]
    fn a_heading_becomes_the_fallback_description() {
        assert_eq!(
            first_prose_line("# Migrations\n\nbody").as_deref(),
            Some("Migrations")
        );
        assert_eq!(
            first_prose_line("\n\nplain line\n").as_deref(),
            Some("plain line")
        );
        assert_eq!(first_prose_line("   \n"), None);
    }

    #[test]
    fn a_non_ascii_body_survives_and_a_long_description_clamps_at_a_char_boundary() {
        let (_, _, body) = front("---\nname: a\n---\n中文の本文 — é\n");
        assert_eq!(body, "中文の本文 — é\n");
        let long = "é".repeat(400);
        let clamped = clamp(&long, DESCRIPTION_CAP);
        assert!(clamped.ends_with('…'));
        assert!(clamped.len() <= DESCRIPTION_CAP + 4);
    }

    /// A bundle-shaped root: a skill file, a `shared/` beside it, and a
    /// `references/` under it. Every resolver test resolves against this.
    fn bundle() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("shared")).unwrap();
        std::fs::create_dir_all(root.join("references")).unwrap();
        std::fs::write(root.join("notes.md"), "notes").unwrap();
        std::fs::write(root.join("shared/state-model.md"), "model").unwrap();
        std::fs::write(root.join("references/codex-tools.md"), "tools").unwrap();
        dir
    }

    fn canon(p: impl AsRef<Path>) -> PathBuf {
        p.as_ref().canonicalize().expect("canonicalize")
    }

    #[test]
    fn a_plain_relative_file_resolves() {
        let dir = bundle();
        assert_eq!(
            resolve_under(dir.path(), "notes.md"),
            SkillFile::Resolved(canon(dir.path().join("notes.md")))
        );
    }

    #[test]
    fn a_nested_file_resolves() {
        let dir = bundle();
        assert_eq!(
            resolve_under(dir.path(), "shared/state-model.md"),
            SkillFile::Resolved(canon(dir.path().join("shared/state-model.md")))
        );
    }

    #[test]
    fn a_file_in_the_skills_own_references_directory_resolves() {
        let dir = bundle();
        assert_eq!(
            resolve_under(dir.path(), "references/codex-tools.md"),
            SkillFile::Resolved(canon(dir.path().join("references/codex-tools.md")))
        );
    }

    /// The two conventions coexist in one bundle, so both have to resolve for
    /// the same skill: `references/` beside the skill itself, `shared/` beside
    /// the whole `skills/` tree. Resolving under a single root serves one and
    /// not the other, which is what `resolve_companion` exists to fix.
    #[test]
    fn a_companion_resolves_beside_the_skill_and_beside_the_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("skills/codex/references")).unwrap();
        std::fs::create_dir_all(root.join("shared")).unwrap();
        std::fs::write(root.join("skills/codex/SKILL.md"), "body").unwrap();
        std::fs::write(root.join("skills/codex/references/tools.md"), "OWN").unwrap();
        std::fs::write(root.join("shared/state-model.md"), "SHARED").unwrap();
        // Same name in both places: the skill's own copy must win, so a
        // bundle-level file can never shadow one a skill ships beside itself.
        std::fs::write(root.join("skills/codex/dup.md"), "OWN DUP").unwrap();
        std::fs::write(root.join("dup.md"), "BUNDLE DUP").unwrap();

        let skill = Skill {
            name: "codex".into(),
            description: "d".into(),
            path: canon(root.join("skills/codex/SKILL.md")),
            root: canon(root),
        };

        assert_eq!(
            resolve_companion(&skill, "references/tools.md"),
            SkillFile::Resolved(canon(root.join("skills/codex/references/tools.md"))),
            "a reference beside the skill resolves"
        );
        assert_eq!(
            resolve_companion(&skill, "shared/state-model.md"),
            SkillFile::Resolved(canon(root.join("shared/state-model.md"))),
            "and so does bundle-wide prose beside the skills tree"
        );
        assert_eq!(
            resolve_companion(&skill, "dup.md"),
            SkillFile::Resolved(canon(root.join("skills/codex/dup.md"))),
            "nearest wins: the skill's own copy, not the bundle's"
        );
        assert_eq!(
            resolve_companion(&skill, "../secrets"),
            SkillFile::Outside("../secrets".into()),
            "and the boundary does not widen by trying two candidates"
        );
        assert_eq!(
            resolve_companion(&skill, "nope.md"),
            SkillFile::Missing("nope.md".into()),
            "absent from both is missing, not refused"
        );
    }

    #[test]
    fn an_absolute_path_is_refused() {
        let dir = bundle();
        assert_eq!(
            resolve_under(dir.path(), "/etc/passwd"),
            SkillFile::Outside("/etc/passwd".into())
        );
    }

    #[test]
    fn a_parent_path_is_refused() {
        let dir = bundle();
        assert_eq!(
            resolve_under(dir.path(), "../../secrets"),
            SkillFile::Outside("../../secrets".into())
        );
    }

    #[test]
    fn a_parent_buried_mid_path_is_refused() {
        let dir = bundle();
        // Lands back inside the root only by arithmetic; refused anyway, because
        // clamping a `..` is how `a/../../x` becomes a surprise.
        assert_eq!(
            resolve_under(dir.path(), "shared/../../x"),
            SkillFile::Outside("shared/../../x".into())
        );
    }

    #[test]
    fn a_missing_file_is_missing_not_an_escape() {
        let dir = bundle();
        assert_eq!(
            resolve_under(dir.path(), "shared/gone.md"),
            SkillFile::Missing("shared/gone.md".into())
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_that_stays_inside_the_root_resolves() {
        let dir = bundle();
        let link = dir.path().join("link.md");
        std::os::unix::fs::symlink(dir.path().join("notes.md"), &link).unwrap();
        assert_eq!(
            resolve_under(dir.path(), "link.md"),
            SkillFile::Resolved(canon(dir.path().join("notes.md")))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_pointing_outside_the_root_is_refused() {
        let dir = bundle();
        let elsewhere = tempfile::tempdir().expect("tempdir");
        let secret = elsewhere.path().join("secret.md");
        std::fs::write(&secret, "ssh-key").unwrap();
        // Nothing lexical sees this: the asked-for path is one plain component
        // inside the root. Canonicalising both sides is the whole defence.
        std::os::unix::fs::symlink(&secret, dir.path().join("escape.md")).unwrap();
        assert_eq!(
            resolve_under(dir.path(), "escape.md"),
            SkillFile::Outside("escape.md".into())
        );
    }

    #[test]
    fn discovery_roots_a_standalone_skill_at_its_own_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("migrations.md"), "how to migrate\n").unwrap();
        std::fs::create_dir_all(dir.path().join("api-style")).unwrap();
        std::fs::write(dir.path().join("api-style/SKILL.md"), "snake_case\n").unwrap();

        let skills = Skills::discover(dir.path()).expect("discover");
        // A `SKILL.md` skill owns its subdirectory; a top-level `*.md` skill has
        // no directory of its own but the skills directory.
        assert_eq!(
            skills.get("api-style").unwrap().root,
            canon(dir.path().join("api-style"))
        );
        assert_eq!(skills.get("migrations").unwrap().root, canon(dir.path()));
    }

    #[test]
    fn namespacing_rewrites_the_root_to_the_bundle() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bundle_root = dir.path();
        std::fs::create_dir_all(bundle_root.join("skills")).unwrap();
        std::fs::write(bundle_root.join("skills/migrations.md"), "how to migrate\n").unwrap();

        let skills = Skills::discover(bundle_root.join("skills"))
            .expect("discover")
            .namespaced("acme", bundle_root);
        let skill = skills.iter().next().expect("one skill");
        assert_eq!(skill.name, "acme__migrations");
        // The bundle root, not `skills/` — so `shared/` beside it is reachable.
        assert_eq!(skill.root, bundle_root.to_path_buf());
    }

    #[test]
    fn skill_md_takes_its_directory_name_and_a_plain_file_its_stem() {
        assert_eq!(
            default_name(Path::new("/s/api-style/SKILL.md")),
            "api-style"
        );
        assert_eq!(default_name(Path::new("/s/migrations.md")), "migrations");
    }
}
