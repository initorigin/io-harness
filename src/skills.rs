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

use std::path::{Path, PathBuf};

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

            skills.push(Skill {
                name,
                description: clamp(&description, DESCRIPTION_CAP),
                // Absolute so the path in the trace and the path the policy
                // decides on do not depend on the process's working directory.
                path: std::fs::canonicalize(&file).unwrap_or(file),
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

    #[test]
    fn skill_md_takes_its_directory_name_and_a_plain_file_its_stem() {
        assert_eq!(
            default_name(Path::new("/s/api-style/SKILL.md")),
            "api-style"
        );
        assert_eq!(default_name(Path::new("/s/migrations.md")), "migrations");
    }
}
