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

/// Directory names never walked by grep/find — build output and VCS metadata,
/// which the agent should never search or edit.
// ponytail: fixed ignore list; honor .gitignore instead if the agent starts
// searching real build trees (open question in the 0.3.0 contract).
const IGNORE_DIRS: &[&str] = &[".git", "target", "node_modules"];

/// A workspace rooted at one directory. All operations stay under `root`.
#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
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
    /// Root the workspace at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The workspace root.
    pub fn root(&self) -> &Path {
        &self.root
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
                re.is_match(base) || re.is_match(file)
            })
            .collect())
    }

    /// Read a file under the root. A missing file reads as empty, so the agent
    /// can create it (matching the 0.1/0.2 `FsTool` behaviour).
    pub fn read_file(&self, rel: &str) -> Result<String> {
        let abs = self.resolve(rel)?;
        Ok(std::fs::read_to_string(abs).unwrap_or_default())
    }

    /// Write a file under the root, creating parent directories.
    pub fn write_file(&self, rel: &str, content: &str) -> Result<()> {
        let abs = self.resolve(rel)?;
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(abs, content)?;
        Ok(())
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
        std::fs::write(root.join("src/b.rs"), "pub fn beta() -> u32 { 2 }\n// alpha ref\n").unwrap();
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
}
