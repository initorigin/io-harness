//! Workspace-scoped tools: grep, find, read_file, write_file — all confined to
//! one root directory.
//!
//! 0.1/0.2 scoped the agent to exactly one file. 0.3 gives it a repository: it
//! greps and finds to locate what to change, reads what it found, and writes
//! several files. Every path the model supplies is resolved relative to `root`
//! and refused if it escapes — an absolute path or a `..` climbing above the
//! root is an error, so the agent cannot touch files outside the workspace.

use std::path::{Component, Path, PathBuf};

use regex::Regex;

use crate::error::{Error, Result};
use crate::policy::{Act, Effect, Policy, Verdict};

/// Directory names never walked by grep/find — build output and VCS metadata,
/// which the agent should never search or edit.
// ponytail: fixed ignore list; honor .gitignore instead if the agent starts
// searching real build trees (open question in the 0.3.0 contract).
const IGNORE_DIRS: &[&str] = &[".git", "target", "node_modules"];

/// A workspace rooted at one directory. All operations stay under `root`, and
/// every path is additionally checked against a [`Policy`] before it is read or
/// written — in this layer, not in the system prompt, so a model that ignores
/// its instructions still cannot act outside the policy.
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
    policy: Policy,
}

/// What a write did to the file it targeted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wrote {
    /// The file did not exist and now does.
    Created,
    /// The contents differ from what was there.
    Changed,
    /// The bytes written are identical to the bytes that were already there.
    /// Recorded as the no-op it is: a step that only rewrites a file unchanged
    /// has not moved the workspace.
    Unchanged,
}

impl Wrote {
    /// Whether this write actually moved the workspace.
    ///
    /// The question stall detection asks each step: an agent that only rewrites
    /// files with what they already contained has not made progress, however many
    /// writes it performed.
    pub fn moved_the_workspace(self) -> bool {
        !matches!(self, Wrote::Unchanged)
    }

    /// Classify a write from the result of reading the target beforehand.
    ///
    /// Content is compared, never metadata: a same-length different-content
    /// write is [`Wrote::Changed`]. A file that exists but cannot be read
    /// (permissions) is not a reason to fail the write — the old content is
    /// unknown, so it reports `Changed`, the conservative answer.
    pub(crate) fn classify(old: std::io::Result<Vec<u8>>, content: &[u8]) -> Self {
        match old {
            Ok(old) if old == content => Wrote::Unchanged,
            Ok(_) => Wrote::Changed,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Wrote::Created,
            Err(_) => Wrote::Changed,
        }
    }
}

/// One grep hit: file relative to the root, 1-based line number, and the line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// Path relative to the workspace root, `/`-separated.
    pub path: String,
    /// 1-based line number.
    pub line: u32,
    /// The matching line's text.
    pub text: String,
}

impl Workspace {
    /// Root the workspace at `root`, enforcing nothing beyond the root itself.
    /// This is the 0.3.0 behaviour and what a caller who passes no policy gets.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            policy: Policy::permissive(),
        }
    }

    /// Root the workspace at `root` and enforce `policy` on every path.
    pub fn with_policy(root: impl Into<PathBuf>, policy: Policy) -> Self {
        Self {
            root: root.into(),
            policy,
        }
    }

    /// The workspace root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The policy this workspace enforces.
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// Evaluate `act` against a workspace-relative path, returning the strictest
    /// verdict across every form that path can take.
    ///
    /// A symlink is checked by its own path *and* by its resolved target, so a
    /// link sitting inside an allowed directory but pointing at a denied file is
    /// refused — the target fails even though the link's own path passes.
    pub fn check_path(&self, act: Act, rel: &str) -> Verdict {
        let mut worst = self.policy.check(act, &normalize(rel));

        // The canonical form, when it differs and still lands inside the root.
        if let Ok(abs) = self.resolve(rel) {
            if let Ok(canon) = abs.canonicalize() {
                let root_canon = self
                    .root
                    .canonicalize()
                    .unwrap_or_else(|_| self.root.clone());
                if let Ok(rel_canon) = canon.strip_prefix(&root_canon) {
                    let rel_canon = rel_canon.to_string_lossy().replace('\\', "/");
                    let v = self.policy.check(act, &rel_canon);
                    if v.effect > worst.effect {
                        worst = v;
                    }
                } else {
                    // Resolves outside the root: a symlink escape, refused
                    // regardless of what the policy says about the link itself.
                    return Verdict {
                        effect: Effect::Deny,
                        rule: Some("<resolves outside workspace root>".into()),
                        layer: None,
                    };
                }
            }
        }
        worst
    }

    /// Refuse the action if the policy denies it, as a typed [`Error::Refused`].
    fn enforce(&self, act: Act, rel: &str) -> Result<()> {
        let v = self.check_path(act, rel);
        if v.effect == Effect::Deny {
            return Err(Error::Refused {
                act: format!("{act:?}").to_lowercase(),
                target: rel.to_string(),
                rule: v.rule,
                layer: v.layer,
            });
        }
        Ok(())
    }

    /// Resolve a model-supplied relative path under the root, refusing absolute
    /// paths and any `..` that climbs above the root.
    pub fn resolve(&self, rel: &str) -> Result<PathBuf> {
        let p = Path::new(rel);
        if p.is_absolute() {
            return Err(escape(rel));
        }
        let mut out = self.root.clone();
        for comp in p.components() {
            match comp {
                Component::Normal(c) => out.push(c),
                Component::CurDir => {}
                Component::ParentDir => {
                    // Pop, then require we are still inside the root.
                    if !out.pop() || !out.starts_with(&self.root) {
                        return Err(escape(rel));
                    }
                }
                Component::RootDir | Component::Prefix(_) => return Err(escape(rel)),
            }
        }
        Ok(out)
    }

    /// Search every text file under the root for `pattern` (a regex; a plain
    /// substring is a valid regex). `path_glob`, if given, limits the search to
    /// files whose relative path matches the glob.
    pub fn grep(&self, pattern: &str, path_glob: Option<&str>) -> Result<Vec<Match>> {
        let re = Regex::new(pattern).map_err(|e| Error::Config(format!("bad grep regex: {e}")))?;
        let glob = path_glob.map(glob_to_regex).transpose()?;
        let mut out = Vec::new();
        for file in self.walk() {
            if let Some(g) = &glob {
                if !g.is_match(&file) {
                    continue;
                }
            }
            // A denied file contributes no matches, so its contents cannot be
            // exfiltrated into the model's context through a search.
            if self.check_path(Act::Read, &file).effect == Effect::Deny {
                continue;
            }
            // Non-UTF-8 / binary files just don't match; skip quietly.
            let Ok(content) = std::fs::read_to_string(self.root.join(&file)) else {
                continue;
            };
            for (i, line) in content.lines().enumerate() {
                if re.is_match(line) {
                    out.push(Match {
                        path: file.clone(),
                        line: (i + 1) as u32,
                        text: line.to_string(),
                    });
                }
            }
        }
        Ok(out)
    }

    /// List files under the root whose name or relative path matches the glob
    /// (`*` any run, `?` one char). `*.rs` matches by basename; `src/*.rs` by
    /// relative path.
    pub fn find(&self, name_glob: &str) -> Result<Vec<String>> {
        let re = glob_to_regex(name_glob)?;
        Ok(self
            .walk()
            .into_iter()
            .filter(|file| {
                let base = Path::new(file)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(file);
                (re.is_match(base) || re.is_match(file))
                    // A denied path is not even named back to the model.
                    && self.check_path(Act::Read, file).effect != Effect::Deny
            })
            .collect())
    }

    /// Read a file under the root. A missing file reads as empty, so the agent
    /// can create it (matching the 0.1/0.2 `FsTool` behaviour). A path the
    /// policy denies is refused before anything is read.
    pub fn read_file(&self, rel: &str) -> Result<String> {
        let abs = self.resolve(rel)?;
        self.enforce(Act::Read, rel)?;
        Ok(std::fs::read_to_string(abs).unwrap_or_default())
    }

    /// Write a file under the root, creating parent directories, reporting
    /// whether it changed anything. A path the policy denies is refused before
    /// anything is read or written.
    ///
    /// The write happens in every case, [`Wrote::Unchanged`] included: this
    /// reports, it does not skip work, so nothing about the file's state depends
    /// on the comparison.
    pub fn write_file(&self, rel: &str, content: &str) -> Result<Wrote> {
        let abs = self.resolve(rel)?;
        self.enforce(Act::Write, rel)?;
        let did = Wrote::classify(std::fs::read(&abs), content.as_bytes());
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(abs, content)?;
        Ok(did)
    }

    /// Read a file under the root as bytes, for a format that is not text.
    ///
    /// A path the policy denies is refused before anything is read, exactly as
    /// [`Workspace::read_file`] refuses it — this is the same gate, not a second
    /// one, which is what lets a document capability be governed by the rules
    /// that already govern source.
    ///
    /// Unlike [`Workspace::read_file`], a missing file is an error rather than an
    /// empty buffer. The text case reads empty so an agent can create a file it
    /// is about to write; a byte read is always "parse this document", and
    /// handing a parser zero bytes turns "there is no such file" into "this file
    /// is corrupt", which is the wrong thing to tell the model.
    pub fn read_bytes(&self, rel: &str) -> Result<Vec<u8>> {
        let abs = self.resolve(rel)?;
        self.enforce(Act::Read, rel)?;
        std::fs::read(abs).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => Error::Config(format!("no such file: {rel}")),
            _ => Error::Io(e),
        })
    }

    /// Write a file under the root as bytes, creating parent directories and
    /// reporting whether it changed anything.
    ///
    /// The byte twin of [`Workspace::write_file`], through the same policy gate
    /// and with the same semantics: the write happens in every case,
    /// [`Wrote::Unchanged`] included, so nothing about the file's state depends
    /// on the comparison.
    pub fn write_bytes(&self, rel: &str, content: &[u8]) -> Result<Wrote> {
        let abs = self.resolve(rel)?;
        self.enforce(Act::Write, rel)?;
        let did = Wrote::classify(std::fs::read(&abs), content);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(abs, content)?;
        Ok(did)
    }

    /// All files under the root, relative and `/`-separated, sorted, skipping
    /// [`IGNORE_DIRS`]. Synchronous walk — fine for local repos.
    // ponytail: blocking std::fs walk on the async runtime; wrap in
    // spawn_blocking if it is ever pointed at a huge tree.
    fn walk(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(ft) = entry.file_type() else { continue };
                let name = entry.file_name();
                if ft.is_dir() {
                    if !IGNORE_DIRS.contains(&name.to_string_lossy().as_ref()) {
                        stack.push(entry.path());
                    }
                } else if ft.is_file() {
                    if let Ok(rel) = entry.path().strip_prefix(&self.root) {
                        out.push(rel.to_string_lossy().replace('\\', "/"));
                    }
                }
            }
        }
        out.sort();
        out
    }
}

fn escape(rel: &str) -> Error {
    Error::Config(format!("path escapes workspace: {rel}"))
}

/// A model-supplied path in the `/`-separated, `.`-free form policy globs match
/// against, so `./src/a.rs` and `src/a.rs` are the same target to a rule.
fn normalize(rel: &str) -> String {
    let s = rel.replace('\\', "/");
    let mut out: Vec<&str> = Vec::new();
    for part in s.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            p => out.push(p),
        }
    }
    out.join("/")
}

/// Compile a glob (`*` any run including `/`, `?` one char) to a regex.
fn glob_to_regex(glob: &str) -> Result<Regex> {
    let mut re = String::from("(?s)^");
    for ch in glob.chars() {
        match ch {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            c => re.push_str(&regex::escape(&c.to_string())),
        }
    }
    re.push('$');
    Regex::new(&re).map_err(|e| Error::Config(format!("bad glob: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small fixture repo: two Rust files under src/, one doc, and an
    /// ignored target/ build artifact.
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("src/a.rs"), "pub fn alpha() -> u32 { 1 }\n").unwrap();
        std::fs::write(
            root.join("src/b.rs"),
            "pub fn beta() -> u32 { 2 }\n// alpha ref\n",
        )
        .unwrap();
        std::fs::write(root.join("README.md"), "# alpha and beta\n").unwrap();
        std::fs::write(root.join("target/junk.rs"), "fn alpha() {}\n").unwrap();
        dir
    }

    #[test]
    fn grep_finds_matches_by_regex_across_files_skipping_ignored() {
        let dir = fixture();
        let ws = Workspace::new(dir.path());
        let hits = ws.grep(r"alpha", None).unwrap();
        // src/a.rs:1, src/b.rs:2, README.md:1 — but NOT target/junk.rs.
        let paths: Vec<_> = hits.iter().map(|m| m.path.as_str()).collect();
        assert!(paths.contains(&"src/a.rs"));
        assert!(paths.contains(&"src/b.rs"));
        assert!(paths.contains(&"README.md"));
        assert!(!paths.iter().any(|p| p.starts_with("target/")));
        // line numbers are 1-based and correct.
        let b = hits.iter().find(|m| m.path == "src/b.rs").unwrap();
        assert_eq!(b.line, 2);
    }

    #[test]
    fn grep_path_glob_restricts_to_matching_files() {
        let dir = fixture();
        let ws = Workspace::new(dir.path());
        let hits = ws.grep("alpha", Some("src/*.rs")).unwrap();
        assert!(hits.iter().all(|m| m.path.starts_with("src/")));
        assert!(!hits.iter().any(|m| m.path == "README.md"));
    }

    #[test]
    fn find_matches_by_basename_and_path_glob() {
        let dir = fixture();
        let ws = Workspace::new(dir.path());
        let rs = ws.find("*.rs").unwrap();
        assert!(rs.contains(&"src/a.rs".to_string()));
        assert!(rs.contains(&"src/b.rs".to_string()));
        assert!(!rs.iter().any(|p| p.starts_with("target/"))); // ignored dir
        let only_a = ws.find("a.rs").unwrap();
        assert_eq!(only_a, vec!["src/a.rs".to_string()]);
    }

    #[test]
    fn resolve_refuses_escapes_but_allows_inner_paths() {
        let dir = fixture();
        let ws = Workspace::new(dir.path());
        assert!(ws.resolve("src/a.rs").is_ok());
        assert!(ws.resolve("src/../README.md").is_ok()); // stays inside
        assert!(ws.resolve("../secret").is_err()); // climbs out
        assert!(ws.resolve("src/../../etc/passwd").is_err()); // climbs out
        #[cfg(unix)]
        assert!(ws.resolve("/etc/passwd").is_err()); // absolute
    }

    /// The policy used across the enforcement tests: src/ is readable and
    /// writable, secrets/ is denied outright.
    fn guarded(root: &Path) -> Workspace {
        Workspace::with_policy(
            root,
            Policy::default()
                .layer("base")
                .allow_read("*")
                .allow_write("src/*")
                .deny_read("secrets/*")
                .deny_write("secrets/*"),
        )
    }

    #[test]
    fn a_denied_write_is_refused_and_the_file_is_untouched() {
        let dir = fixture();
        std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
        std::fs::write(dir.path().join("secrets/key.txt"), "original").unwrap();
        let ws = guarded(dir.path());

        let err = ws.write_file("secrets/key.txt", "stolen").unwrap_err();
        assert!(
            matches!(&err, Error::Refused { rule, layer, .. }
                if rule.as_deref() == Some("secrets/*") && layer.as_deref() == Some("base")),
            "expected an attributable refusal, got {err:?}"
        );
        // Nothing was written.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("secrets/key.txt")).unwrap(),
            "original"
        );
        // An in-policy write still succeeds.
        assert!(ws
            .write_file("src/a.rs", "pub fn alpha() -> u32 { 9 }\n")
            .is_ok());
    }

    #[test]
    fn denied_paths_are_invisible_to_grep_and_find() {
        let dir = fixture();
        std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
        std::fs::write(dir.path().join("secrets/creds.rs"), "alpha token\n").unwrap();
        let ws = guarded(dir.path());

        // grep would otherwise match secrets/creds.rs — it must not appear.
        let hits = ws.grep("alpha", None).unwrap();
        assert!(!hits.iter().any(|m| m.path.starts_with("secrets/")));
        assert!(hits.iter().any(|m| m.path == "src/a.rs"));

        // find must not even name it.
        let found = ws.find("*.rs").unwrap();
        assert!(!found.iter().any(|p| p.starts_with("secrets/")));

        // and a direct read is refused, not silently empty.
        assert!(matches!(
            ws.read_file("secrets/creds.rs"),
            Err(Error::Refused { .. })
        ));
    }

    #[test]
    fn traversal_is_evaluated_on_the_resolved_path_not_the_literal_one() {
        let dir = fixture();
        std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
        std::fs::write(dir.path().join("secrets/key.txt"), "original").unwrap();
        let ws = guarded(dir.path());

        // Lands inside secrets/ after resolution, so the deny still applies.
        assert!(matches!(
            ws.write_file("src/../secrets/key.txt", "stolen"),
            Err(Error::Refused { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("secrets/key.txt")).unwrap(),
            "original"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_is_denied_by_its_target_even_when_its_own_path_is_allowed() {
        let dir = fixture();
        std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
        std::fs::write(dir.path().join("secrets/key.txt"), "secret").unwrap();
        // A link that lives in the allowed tree but points into the denied one.
        std::os::unix::fs::symlink(
            dir.path().join("secrets/key.txt"),
            dir.path().join("src/link.rs"),
        )
        .unwrap();
        let ws = guarded(dir.path());

        // src/link.rs passes on its own path; its target does not.
        assert_eq!(
            ws.check_path(Act::Read, "src/link.rs").effect,
            Effect::Deny,
            "a link into a denied path must be refused"
        );
        assert!(matches!(
            ws.read_file("src/link.rs"),
            Err(Error::Refused { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_pointing_outside_the_root_is_refused() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("passwd"), "root:x:0:0").unwrap();
        let dir = fixture();
        std::os::unix::fs::symlink(outside.path().join("passwd"), dir.path().join("src/out.rs"))
            .unwrap();
        let ws = guarded(dir.path());

        assert_eq!(ws.check_path(Act::Read, "src/out.rs").effect, Effect::Deny);
    }

    #[test]
    fn a_workspace_without_a_policy_behaves_exactly_as_0_3_0_did() {
        let dir = fixture();
        std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
        std::fs::write(dir.path().join("secrets/key.txt"), "x").unwrap();
        let ws = Workspace::new(dir.path());

        // No policy means no enforcement — the boundary is opt-in.
        assert!(ws.write_file("secrets/key.txt", "y").is_ok());
        assert!(ws.read_file("secrets/key.txt").is_ok());
        assert!(ws
            .find("*.txt")
            .unwrap()
            .iter()
            .any(|p| p.starts_with("secrets/")));
    }

    #[test]
    fn check_path_agrees_with_what_read_and_write_actually_enforce() {
        let dir = fixture();
        std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
        std::fs::write(dir.path().join("secrets/key.txt"), "x").unwrap();
        let ws = guarded(dir.path());

        for (act, path) in [
            (Act::Read, "src/a.rs"),
            (Act::Read, "secrets/key.txt"),
            (Act::Write, "src/a.rs"),
            (Act::Write, "secrets/key.txt"),
        ] {
            let denied = ws.check_path(act, path).effect == Effect::Deny;
            let refused = match act {
                Act::Read => matches!(ws.read_file(path), Err(Error::Refused { .. })),
                Act::Write => matches!(ws.write_file(path, "x"), Err(Error::Refused { .. })),
                // The workspace only ever performs reads and writes; exec is the
                // verify gate's and net is the connection point's.
                Act::Exec | Act::Net => unreachable!(),
            };
            assert_eq!(denied, refused, "{act:?} {path}");
        }
    }

    #[test]
    fn read_missing_is_empty_then_write_roundtrips_within_root() {
        let dir = fixture();
        let ws = Workspace::new(dir.path());
        assert_eq!(ws.read_file("src/new.rs").unwrap(), "");
        ws.write_file("src/new.rs", "fn n() {}").unwrap();
        assert_eq!(ws.read_file("src/new.rs").unwrap(), "fn n() {}");
        // an escaping write is refused, nothing written outside root.
        assert!(ws.write_file("../evil.rs", "x").is_err());
    }

    #[test]
    fn a_write_to_a_path_that_does_not_exist_reports_created() {
        let dir = fixture();
        let ws = Workspace::new(dir.path());
        assert_eq!(
            ws.write_file("src/new.rs", "fn n() {}").unwrap(),
            Wrote::Created
        );
    }

    #[test]
    fn a_write_of_different_content_reports_changed_and_lands_on_disk() {
        let dir = fixture();
        let ws = Workspace::new(dir.path());
        assert_eq!(
            ws.write_file("src/a.rs", "pub fn alpha() -> u32 { 9 }\n")
                .unwrap(),
            Wrote::Changed
        );
        assert_eq!(
            ws.read_file("src/a.rs").unwrap(),
            "pub fn alpha() -> u32 { 9 }\n"
        );
    }

    #[test]
    fn a_write_of_byte_identical_content_reports_unchanged() {
        let dir = fixture();
        let ws = Workspace::new(dir.path());
        let same = "pub fn alpha() -> u32 { 1 }\n";
        assert_eq!(ws.write_file("src/a.rs", same).unwrap(), Wrote::Unchanged);
        // Written anyway: the file is still exactly what was asked for.
        assert_eq!(ws.read_file("src/a.rs").unwrap(), same);
    }

    #[test]
    fn a_write_of_the_same_length_but_different_bytes_reports_changed() {
        let dir = fixture();
        let ws = Workspace::new(dir.path());
        // Same byte length as the fixture's src/a.rs, one digit apart — a size
        // or mtime shortcut would call this unchanged.
        assert_eq!(
            ws.write_file("src/a.rs", "pub fn alpha() -> u32 { 2 }\n")
                .unwrap(),
            Wrote::Changed
        );
    }

    #[test]
    fn multibyte_content_round_trips_and_compares_correctly() {
        let dir = fixture();
        let ws = Workspace::new(dir.path());
        let text = "// héllo — 日本語 🌍\n";
        assert_eq!(ws.write_file("src/uni.rs", text).unwrap(), Wrote::Created);
        assert_eq!(ws.read_file("src/uni.rs").unwrap(), text);
        assert_eq!(ws.write_file("src/uni.rs", text).unwrap(), Wrote::Unchanged);
        // Same char count, different bytes.
        assert_eq!(
            ws.write_file("src/uni.rs", "// héllo — 日本語 🌎\n")
                .unwrap(),
            Wrote::Changed
        );
    }

    #[test]
    fn a_denied_write_is_refused_before_the_change_signal_is_computed() {
        let dir = fixture();
        std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
        std::fs::write(dir.path().join("secrets/key.txt"), "original").unwrap();
        let ws = guarded(dir.path());

        // No Wrote at all — the refusal still comes first, even for content
        // identical to what is already there.
        assert!(matches!(
            ws.write_file("secrets/key.txt", "original"),
            Err(Error::Refused { .. })
        ));
    }

    // 0.14.0 — byte IO under the same gate as text IO. These are the foundation
    // every document capability routes through, so the boundary tests come with
    // their negative controls: an assertion that a denied path is refused proves
    // nothing unless the same operation demonstrably succeeds when allowed.

    #[test]
    fn a_denied_byte_write_is_refused_and_the_file_is_untouched() {
        let dir = fixture();
        std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
        std::fs::write(dir.path().join("secrets/blob.bin"), b"original").unwrap();
        let ws = guarded(dir.path());

        let refused = ws.write_bytes("secrets/blob.bin", b"\x00\x01\x02");

        assert!(
            matches!(&refused, Err(Error::Refused { act, target, .. })
                if act == "write" && target == "secrets/blob.bin"),
            "the refusal names the act and the real path, got {refused:?}"
        );
        assert_eq!(
            std::fs::read(dir.path().join("secrets/blob.bin")).unwrap(),
            b"original",
            "refused before anything was written"
        );
    }

    #[test]
    fn a_denied_byte_read_is_refused_before_anything_is_read() {
        let dir = fixture();
        std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
        std::fs::write(dir.path().join("secrets/blob.bin"), b"original").unwrap();
        let ws = guarded(dir.path());

        assert!(
            matches!(ws.read_bytes("secrets/blob.bin"),
                Err(Error::Refused { act, .. }) if act == "read"),
            "a document read of a denied path is the same hole facing the other way"
        );
    }

    /// The negative control for both tests above. The same bytes, the same
    /// operations, a policy that allows them — so the refusals are measuring the
    /// boundary rather than an operation that would have failed regardless.
    #[test]
    fn the_same_byte_io_succeeds_where_the_policy_allows_it() {
        let dir = fixture();
        let ws = guarded(dir.path());
        let payload = b"\x50\x4b\x03\x04binary-not-utf8\xff\xfe";

        assert_eq!(
            ws.write_bytes("src/doc.bin", payload).unwrap(),
            Wrote::Created
        );
        assert_eq!(ws.read_bytes("src/doc.bin").unwrap(), payload);
    }

    #[test]
    fn byte_io_round_trips_content_that_is_not_valid_utf8() {
        let dir = fixture();
        let ws = Workspace::new(dir.path());
        // A lone 0xFF is not valid UTF-8: read_file would lose it, which is the
        // whole reason this pair exists.
        let payload = &[0x00u8, 0xFF, 0x10, 0x80, b'z'];

        ws.write_bytes("blob.bin", payload).unwrap();
        assert_eq!(ws.read_bytes("blob.bin").unwrap(), payload);
        assert_ne!(
            ws.read_file("blob.bin").unwrap().as_bytes(),
            payload,
            "the text reader cannot represent these bytes — that is what this pair is for"
        );
    }

    #[test]
    fn a_missing_byte_read_is_an_error_not_an_empty_document() {
        let dir = fixture();
        let ws = Workspace::new(dir.path());

        let err = ws.read_bytes("nope.xlsx").unwrap_err();
        assert!(
            err.to_string().contains("nope.xlsx"),
            "the error names the missing file, got {err}"
        );
        // Contrast with the text reader, whose empty-on-missing behaviour is
        // deliberate and unchanged.
        assert_eq!(ws.read_file("nope.xlsx").unwrap(), "");
    }

    #[test]
    fn a_byte_write_reports_whether_it_changed_anything() {
        let dir = fixture();
        let ws = Workspace::new(dir.path());

        assert_eq!(ws.write_bytes("b.bin", b"one").unwrap(), Wrote::Created);
        assert_eq!(ws.write_bytes("b.bin", b"one").unwrap(), Wrote::Unchanged);
        assert_eq!(ws.write_bytes("b.bin", b"two").unwrap(), Wrote::Changed);
    }

    #[test]
    fn byte_io_cannot_escape_the_workspace_root() {
        let dir = fixture();
        let ws = Workspace::new(dir.path());
        assert!(ws.read_bytes("../outside.bin").is_err());
        assert!(ws.write_bytes("../outside.bin", b"x").is_err());
    }
}
