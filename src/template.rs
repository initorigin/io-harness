//! Prompt templates — one task description written once and filled in per run.
//!
//! A template is a markdown file holding the text of a goal, with the parts that
//! change marked. The operator writes "fix the failing test {{test}} in {{file}}"
//! once; a run supplies the two values and gets a goal. What comes out is an
//! ordinary `String` and goes to [`TaskContract::workspace`](crate::TaskContract)
//! like any other, so nothing downstream knows a template was involved.
//!
//! # What it is not
//!
//! Not a template *language*. There are no conditionals, no loops, no includes,
//! no partials, and no nesting: substitution and `$ARGUMENTS`, and that is the
//! whole grammar. A value that itself contains `{{x}}` is emitted literally
//! rather than re-read, because a prompt builder that can recurse is a prompt
//! builder whose output nobody can predict from its input.
//!
//! Rendering is a pure function of the template and the arguments. It reads no
//! file, consults no [`Policy`](crate::Policy), draws on no budget, and reaches
//! no model — [`Templates::render`] returns a `String` and that is the entire
//! effect.
//!
//! # A substitution resolves or fails; it never empties
//!
//! The rule 0.19.0 set for `${env:}` and `${file:}` in
//! [`Config`](crate::Config), for the same reason. A `{{placeholder}}` nobody
//! passed an argument for is [`Error::Config`], not an empty string: a goal with
//! a hole in it still reads like a goal, so the run proceeds and pursues
//! something the operator never asked for. `$ARGUMENTS` is the one exception,
//! and it is not an exception to the rule — it names a *remainder*, and a
//! remainder is legitimately empty.
//!
//! # Layout
//!
//! The same two conventions [`Skills`](crate::Skills) accepts:
//!
//! ```text
//! templates/
//!   bugfix.md              -> template "bugfix"
//!   review/
//!     TEMPLATE.md          -> template "review"
//! ```
//!
//! Optional YAML frontmatter names the template and describes it. Without
//! frontmatter the name comes from the filename (or its directory, for a
//! `TEMPLATE.md`) and the description from the first prose line.
//!
//! ```text
//! ---
//! name: bugfix
//! description: Fix one failing test and change nothing else.
//! ---
//!
//! Fix the failing test {{test}} in {{file}}. $ARGUMENTS
//! ```

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
// The frontmatter reader is `skills`', not a second one. Two scalar keys off the
// front of a `.md` file is the same job in both places, and a lesser second parser
// would disagree with the first about some file an operator wrote once.
use crate::skills::{clamp, first_prose_line, split_front_matter};

/// How many templates one directory may hold.
///
/// A hard ceiling rather than a truncation, for the reason
/// [`MAX_SKILLS`](crate::skills::MAX_SKILLS) has one: a caller who drops 500
/// files in a directory should hear that the set was rejected, not discover
/// later that the template they meant to render was one of the ones dropped.
///
/// ```
/// assert!(io_harness::template::MAX_TEMPLATES >= 1);
/// ```
pub const MAX_TEMPLATES: usize = 64;

/// How much of one description is kept.
const DESCRIPTION_CAP: usize = 240;

/// The marker that collects every argument no `{{placeholder}}` claimed.
const ARGUMENTS: &str = "$ARGUMENTS";

/// One discovered template: a name, a line describing it, and the text to render.
///
/// Unlike [`Skill`](crate::Skill), which holds only a path so the body's read
/// passes the permission policy at the moment it happens, a `Template` holds its
/// body. Rendering needs the text, and rendering is a pure string operation that
/// happens before a run exists — there is no step for a policy to sit in front
/// of, and re-reading the file per render would make the same call return
/// different prompts.
///
/// ```no_run
/// use io_harness::Templates;
///
/// # fn demo() -> io_harness::Result<()> {
/// for template in Templates::discover("./templates")?.iter() {
///     println!("{} — {}", template.name, template.description);
///     println!("{} chars of body", template.body.len());
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    /// How a caller asks for it: the frontmatter `name`, else the file stem, else
    /// the containing directory for a `TEMPLATE.md`.
    pub name: String,
    /// One line describing it: the frontmatter `description`, else the first
    /// prose line of the body.
    pub description: String,
    /// The text to render: the file with its frontmatter stripped and the
    /// surrounding whitespace trimmed. Trimmed because what this renders into is
    /// a goal rather than a file, and the blank line the closing `---` leaves
    /// behind would otherwise open every rendered prompt.
    pub body: String,
}

/// The templates discovered for one caller, and the renderer over them.
///
/// Discovery mirrors [`Skills`](crate::Skills) exactly — both layouts, sorted by
/// name, and a path that does not exist, is not a directory, holds more than
/// [`MAX_TEMPLATES`], or holds two templates of the same name is
/// [`Error::Config`] naming it. A rejected set, never a silently truncated one.
///
/// ```
/// use io_harness::Templates;
///
/// # fn demo() -> io_harness::Result<()> {
/// let dir = tempfile::tempdir()?;
/// std::fs::write(
///     dir.path().join("bugfix.md"),
///     "---\nname: bugfix\ndescription: Fix one test.\n---\n\
///      Fix {{test}} in {{file}}. $ARGUMENTS\n",
/// )?;
///
/// let templates = Templates::discover(dir.path())?;
/// assert_eq!(templates.names(), vec!["bugfix"]);
///
/// // `{{test}}` and `{{file}}` are named; `hint` matched no placeholder, so it
/// // is what `$ARGUMENTS` collects.
/// let goal = templates.render("bugfix", &[
///     ("test", "parses_a_crlf_header"),
///     ("file", "src/parse.rs"),
///     ("hint", "it only fails on CI"),
/// ])?;
/// assert_eq!(goal, "Fix parses_a_crlf_header in src/parse.rs. it only fails on CI");
///
/// // A placeholder with no argument fails; it never renders as nothing.
/// assert!(templates.render("bugfix", &[("test", "t")]).is_err());
/// # Ok(())
/// # }
/// # demo().unwrap();
/// ```
///
/// What comes out is an ordinary goal:
///
/// ```no_run
/// use io_harness::{TaskContract, Templates, Verification};
///
/// # fn demo() -> io_harness::Result<()> {
/// let goal = Templates::discover("./templates")?.render("bugfix", &[
///     ("test", "parses_a_crlf_header"),
///     ("file", "src/parse.rs"),
/// ])?;
/// let contract = TaskContract::workspace(goal, "/path/to/repo")
///     .with_verification(Verification::Command {
///         argv: vec!["cargo".into(), "test".into()],
///         expect_exit: 0,
///     });
/// # let _ = contract;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Default)]
pub struct Templates {
    templates: Vec<Template>,
}

impl Templates {
    /// No templates.
    ///
    /// ```
    /// use io_harness::Templates;
    ///
    /// let templates = Templates::none();
    /// assert!(templates.is_empty());
    /// assert!(templates.render("bugfix", &[]).is_err());
    /// ```
    pub fn none() -> Self {
        Self::default()
    }

    /// Discover every template under `dir`, sorted by name.
    ///
    /// Fails with [`Error::Config`] when `dir` does not exist, is not a
    /// directory, holds more than [`MAX_TEMPLATES`], or holds two templates with
    /// the same name — a caller addresses a template by name, so an ambiguous
    /// set is a configuration mistake, and picking one silently is how an
    /// operator ends up debugging why their run got the wrong prompt.
    ///
    /// ```
    /// use io_harness::Templates;
    ///
    /// # fn demo() -> io_harness::Result<()> {
    /// let dir = tempfile::tempdir()?;
    /// std::fs::create_dir(dir.path().join("review"))?;
    /// std::fs::write(dir.path().join("review/TEMPLATE.md"), "Read every changed line.\n")?;
    /// std::fs::write(dir.path().join("notes.txt"), "not a template\n")?;
    ///
    /// let templates = Templates::discover(dir.path())?;
    /// assert_eq!(templates.names(), vec!["review"]);
    /// // No frontmatter: the description is the first prose line.
    /// assert_eq!(templates.get("review").unwrap().description, "Read every changed line.");
    ///
    /// assert!(Templates::discover(dir.path().join("nope")).is_err());
    /// # Ok(())
    /// # }
    /// # demo().unwrap();
    /// ```
    pub fn discover(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        if !dir.exists() {
            return Err(Error::Config(format!(
                "templates directory {} does not exist",
                dir.display()
            )));
        }
        if !dir.is_dir() {
            return Err(Error::Config(format!(
                "templates path {} is not a directory; point it at a directory of markdown files, \
                 not at one file",
                dir.display()
            )));
        }

        // Sorted, so the set — and any catalogue built from it — is identical
        // across runs on the same directory. `read_dir` order is not.
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        entries.sort();

        let mut templates: Vec<(Template, PathBuf)> = Vec::new();
        for path in entries {
            let file = if path.is_dir() {
                let candidate = path.join("TEMPLATE.md");
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
                // Not a template. The test is the extension and nothing else, so
                // a `README.md` sitting here *is* discovered as one — documented
                // rather than special-cased, because a name-based exception is
                // the start of a list nobody can predict.
                continue;
            };

            let text = std::fs::read_to_string(&file)?;
            let (front_name, front_desc, body) = split_front_matter(&text);
            let name = front_name.unwrap_or_else(|| default_name(&file));
            let description = front_desc
                .or_else(|| first_prose_line(body))
                .unwrap_or_else(|| "(no description)".to_string());

            templates.push((
                Template {
                    name,
                    description: clamp(&description, DESCRIPTION_CAP),
                    body: body.trim().to_string(),
                },
                file,
            ));

            if templates.len() > MAX_TEMPLATES {
                return Err(Error::Config(format!(
                    "templates directory {} holds more than {MAX_TEMPLATES} templates. The set is \
                     rejected rather than silently reduced — split the directory or point at a \
                     smaller one",
                    dir.display()
                )));
            }
        }

        templates.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        for pair in templates.windows(2) {
            if pair[0].0.name == pair[1].0.name {
                return Err(Error::Config(format!(
                    "two templates are both named {:?} ({} and {}); a template is addressed by \
                     name, so the set is ambiguous",
                    pair[0].0.name,
                    pair[0].1.display(),
                    pair[1].1.display()
                )));
            }
        }
        Ok(Self {
            templates: templates.into_iter().map(|(t, _)| t).collect(),
        })
    }

    /// True if nothing was discovered or configured.
    ///
    /// ```
    /// assert!(io_harness::Templates::none().is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.templates.is_empty()
    }

    /// How many templates are available.
    ///
    /// ```
    /// assert_eq!(io_harness::Templates::none().len(), 0);
    /// ```
    pub fn len(&self) -> usize {
        self.templates.len()
    }

    /// The template of that name, if it exists.
    ///
    /// ```
    /// assert!(io_harness::Templates::none().get("bugfix").is_none());
    /// ```
    pub fn get(&self, name: &str) -> Option<&Template> {
        self.templates.iter().find(|t| t.name == name)
    }

    /// Every discovered template, sorted by name.
    ///
    /// ```
    /// assert_eq!(io_harness::Templates::none().iter().count(), 0);
    /// ```
    pub fn iter(&self) -> impl Iterator<Item = &Template> {
        self.templates.iter()
    }

    /// The available names, for telling a caller what it can ask for.
    ///
    /// ```
    /// assert!(io_harness::Templates::none().names().is_empty());
    /// ```
    pub fn names(&self) -> Vec<&str> {
        self.templates.iter().map(|t| t.name.as_str()).collect()
    }

    /// One line per template — name and description, no bodies.
    ///
    /// ```
    /// assert_eq!(io_harness::Templates::none().catalog(), "");
    /// ```
    pub fn catalog(&self) -> String {
        self.templates
            .iter()
            .map(|t| format!("- {}: {}", t.name, t.description))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Render the named template against `args`, returning the goal.
    ///
    /// Every `{{placeholder}}` is replaced by the argument of that name, and
    /// `$ARGUMENTS` by every argument no placeholder claimed, joined by one space
    /// in the order given. Substitution is single-pass: an inserted value that
    /// itself contains `{{x}}` or `$ARGUMENTS` is emitted literally.
    ///
    /// Fails with [`Error::Config`] when `name` is not a template, when a
    /// `{{placeholder}}` has no argument, or when a `{{` is never closed. A
    /// placeholder resolves or fails; it never renders as an empty string.
    ///
    /// ```
    /// use io_harness::Templates;
    ///
    /// # fn demo() -> io_harness::Result<()> {
    /// let dir = tempfile::tempdir()?;
    /// std::fs::write(
    ///     dir.path().join("ship.md"),
    ///     "Write {{file}} containing {{needle}}. $ARGUMENTS\n",
    /// )?;
    /// let templates = Templates::discover(dir.path())?;
    ///
    /// assert_eq!(
    ///     templates.render("ship", &[
    ///         ("file", "out.txt"),
    ///         ("needle", "SHIPPED"),
    ///         ("aside", "and stop there"),
    ///     ])?,
    ///     "Write out.txt containing SHIPPED. and stop there",
    /// );
    ///
    /// // An unknown name, and an unfilled placeholder, are both errors.
    /// assert!(templates.render("shipp", &[]).is_err());
    /// assert!(templates.render("ship", &[("file", "out.txt")]).is_err());
    /// # Ok(())
    /// # }
    /// # demo().unwrap();
    /// ```
    pub fn render(&self, name: &str, args: &[(&str, &str)]) -> Result<String> {
        let template = self.get(name).ok_or_else(|| {
            Error::Config(format!(
                "there is no template named {name:?}; the templates are: {}",
                if self.is_empty() {
                    "(none)".to_string()
                } else {
                    self.names().join(", ")
                }
            ))
        })?;

        // Which arguments a placeholder claims has to be known before the walk,
        // because `$ARGUMENTS` may appear before the placeholder that claims one.
        let named = placeholders(&template.body);
        let remainder = args
            .iter()
            .filter(|(k, _)| !named.contains(k))
            .map(|(_, v)| *v)
            .collect::<Vec<_>>()
            .join(" ");

        let mut out = String::new();
        let mut rest = template.body.as_str();
        loop {
            let open = rest.find("{{");
            let dollar = rest.find(ARGUMENTS);
            // The earlier of the two markers, so neither can shadow the other.
            let (at, is_placeholder) = match (open, dollar) {
                (None, None) => {
                    out.push_str(rest);
                    return Ok(out);
                }
                (Some(o), None) => (o, true),
                (None, Some(d)) => (d, false),
                (Some(o), Some(d)) => (o.min(d), o < d),
            };
            out.push_str(&rest[..at]);
            if !is_placeholder {
                out.push_str(&remainder);
                rest = &rest[at + ARGUMENTS.len()..];
                continue;
            }
            let after = &rest[at + 2..];
            let Some(end) = after.find("}}") else {
                return Err(Error::Config(format!(
                    "template {name:?} has a `{{{{` that is never closed; a placeholder is \
                     `{{{{name}}}}` and there is no other syntax"
                )));
            };
            let key = after[..end].trim();
            let value = args
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| *v)
                .ok_or_else(|| {
                    Error::Config(format!(
                        "template {name:?} has no argument for `{{{{{key}}}}}`; a placeholder \
                         resolves or fails and is never empty, because a goal with a hole in it \
                         still reads like a goal — pass {key:?} or take it out of the template"
                    ))
                })?;
            out.push_str(value);
            rest = &after[end + 2..];
        }
    }
}

/// Every `{{name}}` in a body, in order. A `{{` that is never closed ends the
/// scan; [`Templates::render`] is what reports it, so this stays infallible.
fn placeholders(body: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(at) = rest.find("{{") {
        let after = &rest[at + 2..];
        let Some(end) = after.find("}}") else { break };
        out.push(after[..end].trim());
        rest = &after[end + 2..];
    }
    out
}

/// The name a file implies when its frontmatter does not give one: the directory
/// for a `TEMPLATE.md`, otherwise the file stem.
fn default_name(file: &Path) -> String {
    let stem = file
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if stem.eq_ignore_ascii_case("TEMPLATE") {
        if let Some(parent) = file.parent().and_then(|p| p.file_name()) {
            return parent.to_string_lossy().to_string();
        }
    }
    stem
}

// ponytail: `split_front_matter`, `first_prose_line` and `clamp` below are copies
// of the ones in `skills.rs`, which are private to that module. They are proven
// and unit-tested there, and a lesser second parser would be worse than a
// duplicate one. The fix is to lift all three into a shared crate-private
// `front_matter` module (or make them `pub(crate)`) and delete these — a change
// to `skills.rs`, which this task was told not to touch.

#[cfg(test)]
mod tests {
    use super::*;

    fn one(body: &str) -> Templates {
        Templates {
            templates: vec![Template {
                name: "t".into(),
                description: "d".into(),
                body: body.into(),
            }],
        }
    }

    #[test]
    fn a_placeholder_and_arguments_are_the_whole_grammar() {
        let t = one("a {{x}} b $ARGUMENTS c\n");
        assert_eq!(
            t.render("t", &[("x", "X"), ("y", "Y"), ("z", "Z")])
                .unwrap(),
            "a X b Y Z c\n"
        );
    }

    #[test]
    fn arguments_before_the_placeholder_that_claims_one_still_excludes_it() {
        // `$ARGUMENTS` is reached first, but `{{x}}` later in the body still
        // claims the `x` argument, so it must not also land in the remainder.
        let t = one("$ARGUMENTS then {{x}}");
        assert_eq!(
            t.render("t", &[("x", "X"), ("y", "Y")]).unwrap(),
            "Y then X"
        );
    }

    #[test]
    fn a_repeated_placeholder_is_substituted_every_time() {
        let t = one("{{x}} and {{x}}");
        assert_eq!(t.render("t", &[("x", "X")]).unwrap(), "X and X");
    }

    #[test]
    fn spaces_inside_the_braces_are_tolerated() {
        let t = one("{{ x }}");
        assert_eq!(t.render("t", &[("x", "X")]).unwrap(), "X");
    }

    #[test]
    fn a_value_is_not_re_substituted() {
        let t = one("{{x}} {{y}}");
        // The `{{y}}` inside x's value is text, not a placeholder; the real
        // `{{y}}` after it is the one substituted.
        assert_eq!(
            t.render("t", &[("x", "{{y}}"), ("y", "Y")]).unwrap(),
            "{{y}} Y"
        );
    }

    #[test]
    fn an_unfilled_placeholder_and_an_unclosed_brace_are_both_errors() {
        let t = one("{{x}}");
        assert!(matches!(t.render("t", &[]), Err(Error::Config(m)) if m.contains("x")));
        let t = one("{{x");
        assert!(
            matches!(t.render("t", &[("x", "X")]), Err(Error::Config(m)) if m.contains("closed"))
        );
    }

    #[test]
    fn an_unknown_name_lists_what_exists() {
        let t = one("body");
        assert!(matches!(t.render("nope", &[]), Err(Error::Config(m)) if m.contains("t")));
        assert!(
            matches!(Templates::none().render("nope", &[]), Err(Error::Config(m)) if m.contains("(none)"))
        );
    }

    #[test]
    fn a_body_with_no_marker_renders_to_itself() {
        let t = one("plain prose, {braces} and $ARGS but no marker\n");
        assert_eq!(
            t.render("t", &[("x", "X")]).unwrap(),
            "plain prose, {braces} and $ARGS but no marker\n",
            "an argument nobody claimed and no $ARGUMENTS to collect it is dropped"
        );
    }

    #[test]
    fn template_md_takes_its_directory_name_and_a_plain_file_its_stem() {
        assert_eq!(default_name(Path::new("/t/review/TEMPLATE.md")), "review");
        assert_eq!(default_name(Path::new("/t/bugfix.md")), "bugfix");
    }

    #[test]
    fn frontmatter_is_stripped_from_the_body() {
        let (name, desc, body) = split_front_matter("---\nname: a\ndescription: d\n---\nB {{x}}\n");
        assert_eq!(name.as_deref(), Some("a"));
        assert_eq!(desc.as_deref(), Some("d"));
        assert_eq!(body, "B {{x}}\n");
        assert_eq!(first_prose_line("# H\n\nx").as_deref(), Some("H"));
        assert_eq!(clamp("  a   b ", DESCRIPTION_CAP), "a b");
    }
}
