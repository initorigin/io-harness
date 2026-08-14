//! Workspace-scoped tools: grep, find, list_dir, read_file, write_file — all
//! confined to one root directory.
//!
//! 0.1/0.2 scoped the agent to exactly one file. 0.3 gives it a repository: it
//! greps and finds to locate what to change, reads what it found, and writes
//! several files. Every path the model supplies is resolved relative to `root`
//! and refused if it escapes — an absolute path or a `..` climbing above the
//! root is an error, so the agent cannot touch files outside the workspace.

use std::fmt;
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

/// How a file's bytes were decoded into text (0.55.0).
///
/// Named in the observation whenever it is not [`TextEncoding::Utf8`], because a
/// model that cannot see which encoding produced a string cannot tell a decode
/// from a guess. Detection stops at the byte-order mark on purpose: a
/// statistical detector is a dependency this crate does not carry, and a guessed
/// Latin-1 is the same class of confident wrong answer as the empty string this
/// release removes.
///
/// ```
/// use io_harness::tools::TextEncoding;
///
/// assert_eq!(TextEncoding::Utf16Le.as_str(), "UTF-16LE");
/// // The ordinary case has a name too, so a trace never has to infer one.
/// assert_eq!(TextEncoding::Utf8.as_str(), "UTF-8");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextEncoding {
    /// Valid UTF-8 with no byte-order mark. The overwhelmingly common case.
    Utf8,
    /// Valid UTF-8 behind a byte-order mark, which is stripped from the text.
    Utf8Bom,
    /// UTF-16 little-endian, identified by its byte-order mark.
    Utf16Le,
    /// UTF-16 big-endian, identified by its byte-order mark.
    Utf16Be,
}

impl TextEncoding {
    /// The name written into the observation and the trace.
    pub fn as_str(self) -> &'static str {
        match self {
            TextEncoding::Utf8 => "UTF-8",
            TextEncoding::Utf8Bom => "UTF-8 with a byte-order mark",
            TextEncoding::Utf16Le => "UTF-16LE",
            TextEncoding::Utf16Be => "UTF-16BE",
        }
    }
}

/// What a path turned out to be when the harness looked at it (0.55.0).
///
/// The read path used to have exactly one answer — a `String` — so a file that
/// was not text arrived at the model as an empty document, indistinguishable
/// from a file that does not exist. This type is that missing distinction: text
/// carries the encoding it was decoded from, and everything else is named rather
/// than decoded, with the tool that *can* open it where there is one.
///
/// ```
/// use io_harness::tools::{FileContent, TextEncoding, Workspace};
///
/// # fn demo() -> io_harness::Result<()> {
/// let dir = tempfile::tempdir()?;
/// std::fs::write(dir.path().join("notes.md"), "hello\n")?;
/// std::fs::write(dir.path().join("blob.bin"), [0x7f, b'E', b'L', b'F', 0x02])?;
/// let ws = Workspace::new(dir.path());
///
/// assert_eq!(
///     ws.read_typed("notes.md")?,
///     FileContent::Text { text: "hello\n".into(), encoding: TextEncoding::Utf8 },
/// );
/// // Not decoded, and not empty either: named, with its size.
/// assert!(matches!(
///     ws.read_typed("blob.bin")?,
///     FileContent::Binary { bytes: 5, kind: "an ELF executable" },
/// ));
/// # Ok(()) }
/// # demo().unwrap();
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileContent {
    /// Text, and the encoding it was decoded from.
    Text {
        /// The decoded text. A missing file reads as empty, which is deliberate
        /// and unchanged from 0.1.0: it is what lets an agent create a file by
        /// reading it first.
        text: String,
        /// How those bytes became this string.
        encoding: TextEncoding,
    },
    /// An image, named rather than decoded. `view_image` is what looks at one.
    Image {
        /// A human-readable format name, e.g. `a PNG image`.
        format: &'static str,
    },
    /// A document with a decoder of its own behind a cargo feature.
    Document {
        /// A human-readable format name, e.g. `a spreadsheet`.
        format: &'static str,
        /// The tool that reads it, e.g. `xlsx_read`.
        tool: &'static str,
    },
    /// Bytes that are not text in any encoding this crate detects.
    Binary {
        /// The file's size on disk.
        bytes: u64,
        /// What the leading bytes look like, e.g. `a ZIP archive`. Falls back to
        /// `binary data` rather than guessing.
        kind: &'static str,
    },
}

impl FileContent {
    /// Why this is not text, phrased for the model that asked to read it, or
    /// `None` when it is text (0.55.0).
    ///
    /// One sentence, naming the file, what it is, and the tool that opens it
    /// where there is one — and saying so when that tool's cargo feature is not
    /// compiled into this build, because a model told nothing simply calls the
    /// same tool again.
    ///
    /// ```
    /// use io_harness::tools::FileContent;
    ///
    /// let png = FileContent::Image { format: "a PNG image" };
    /// let why = png.refusal("logo.png").unwrap();
    /// assert!(why.contains("logo.png") && why.contains("a PNG image"));
    ///
    /// // Text has no refusal, which is what makes this an `Option`.
    /// assert!(FileContent::Text {
    ///     text: "hi".into(),
    ///     encoding: io_harness::tools::TextEncoding::Utf8,
    /// }
    /// .refusal("notes.md")
    /// .is_none());
    /// ```
    pub fn refusal(&self, rel: &str) -> Option<String> {
        match self {
            FileContent::Text { .. } => None,
            FileContent::Image { format } => Some(if cfg!(feature = "media") {
                format!(
                    "{rel} is {format}, so nothing was decoded — call `{}` to look at it",
                    crate::tools::VIEW_IMAGE_TOOL
                )
            } else {
                format!(
                    "{rel} is {format}, so nothing was decoded, and this build cannot send \
                     images (the `media` cargo feature is off)"
                )
            }),
            FileContent::Document { format, tool } => {
                let compiled = match *tool {
                    crate::tools::XLSX_READ_TOOL => cfg!(feature = "xlsx"),
                    crate::tools::DOCX_READ_TOOL => cfg!(feature = "docx"),
                    crate::tools::PPTX_READ_TOOL => cfg!(feature = "pptx"),
                    _ => cfg!(feature = "pdf"),
                };
                Some(if compiled {
                    format!("{rel} is {format}, so nothing was decoded — `{tool}` is what reads one")
                } else {
                    format!(
                        "{rel} is {format}, so nothing was decoded; `{tool}` reads one and is \
                         not compiled into this build"
                    )
                })
            }
            FileContent::Binary { bytes, kind } => Some(format!(
                "{rel} is not text: {kind}, {bytes} bytes. Nothing was decoded — reading it as \
                 text would produce a document that is not what is in the file."
            )),
        }
    }
}

/// The format name for a path's extension, for the formats that are images
/// rather than text.
///
/// SVG is deliberately absent: it is XML, a model reading one wants the markup,
/// and calling it an image would make a readable file unreadable. HEIC and AVIF
/// are here because naming them is the point — refusing them as "not one of
/// image/jpeg, image/png, image/gif, image/webp" is the refusal 0.55.0 exists to
/// stop giving.
fn image_format_for(ext: &str) -> Option<&'static str> {
    Some(match ext {
        "png" => "a PNG image",
        "jpg" | "jpeg" => "a JPEG image",
        "gif" => "a GIF image",
        "webp" => "a WebP image",
        "bmp" => "a BMP image",
        "tif" | "tiff" => "a TIFF image",
        "ico" => "an ICO image",
        "tga" => "a TGA image",
        "pnm" | "pbm" | "pgm" | "ppm" => "a PNM image",
        "heic" | "heif" => "a HEIC image",
        "avif" => "an AVIF image",
        _ => return None,
    })
}

/// The format name and the tool for a path's extension, for the documents this
/// crate decodes behind a cargo feature.
fn document_for(ext: &str) -> Option<(&'static str, &'static str)> {
    Some(match ext {
        "xlsx" => ("a spreadsheet", crate::tools::XLSX_READ_TOOL),
        "docx" => ("a Word document", crate::tools::DOCX_READ_TOOL),
        "pptx" => ("a slide deck", crate::tools::PPTX_READ_TOOL),
        "pdf" => ("a PDF", crate::tools::PDF_READ_TOOL),
        _ => return None,
    })
}

/// What the leading bytes look like. Named for an operator reading a trace, and
/// `binary data` when nothing matches — a wrong guess would be worse than none.
fn sniff_binary(bytes: &[u8]) -> &'static str {
    match bytes {
        [0x7f, b'E', b'L', b'F', ..] => "an ELF executable",
        [0xfe, 0xed, 0xfa, ..] | [0xce | 0xcf, 0xfa, 0xed, 0xfe, ..] => "a Mach-O binary",
        [0xca, 0xfe, 0xba, 0xbe, ..] => "a Mach-O universal binary",
        [b'M', b'Z', ..] => "a Windows executable",
        [b'P', b'K', 0x03 | 0x05, ..] => "a ZIP archive",
        [0x1f, 0x8b, ..] => "a gzip stream",
        [b'%', b'P', b'D', b'F', ..] => "a PDF",
        [0x00, b'a', b's', b'm', ..] => "a WebAssembly module",
        [b'S', b'Q', b'L', b'i', b't', b'e', ..] => "a SQLite database",
        _ => "binary data",
    }
}

/// Decode UTF-16 from bytes that have already had their byte-order mark
/// removed. `None` for an odd byte count or an unpaired surrogate, both of which
/// mean the mark was a coincidence rather than a declaration.
fn decode_utf16(bytes: &[u8], little_endian: bool) -> Option<String> {
    if bytes.len() % 2 != 0 {
        return None;
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| {
            let pair = [pair[0], pair[1]];
            if little_endian {
                u16::from_le_bytes(pair)
            } else {
                u16::from_be_bytes(pair)
            }
        })
        .collect();
    String::from_utf16(&units).ok()
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

/// What one entry of a directory listing is (0.24.0).
///
/// The distinction a listing exists to draw. A name on its own does not tell the
/// agent whether the next call is [`Workspace::read_file`] or another
/// [`Workspace::list_dir`], and a tree is walked by making exactly that decision
/// at every level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file, which is the only kind that carries a size.
    File,
    /// A directory. Named, never descended into — see [`Workspace::list_dir`].
    Dir,
    /// A symbolic link, reported as the link itself rather than as whatever it
    /// points at.
    ///
    /// A listing that silently followed links would report a directory outside
    /// the workspace as if it were inside one, and would hang on a link that
    /// pointed at itself. What the link resolves to is decided where it matters —
    /// at the [`Act::Read`] check on the path — and not here.
    Symlink,
}

/// One entry of a directory listing: where it is, what it is, and how big it is
/// (0.24.0).
///
/// The path is relative to the workspace root and `/`-separated, exactly as
/// [`Workspace::find`] reports one, so what a listing returns can be handed
/// straight back to a read without the model having to join anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Path relative to the workspace root, `/`-separated.
    pub path: String,
    /// Whether this is a file, a directory or a link.
    pub kind: EntryKind,
    /// Size in bytes, for a [`EntryKind::File`] whose metadata could be read.
    ///
    /// `None` for a directory and a link, where the number would be a property
    /// of the entry rather than of anything the agent could read, and for a file
    /// whose metadata the platform refused — a size that could not be measured is
    /// absent rather than reported as zero.
    pub size: Option<u64>,
}

/// The one line the model reads for this entry.
///
/// The kind comes first and in a fixed width so a listing is a column a reader
/// scans rather than a sentence they parse, and the size is stated in bytes with
/// no unit-scaling: `1.2 KB` is a judgement about what matters, and the model is
/// better at that judgement than a formatter is.
impl fmt::Display for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            EntryKind::Dir => write!(f, "dir  {}/", self.path),
            EntryKind::Symlink => write!(f, "link {}", self.path),
            EntryKind::File => match self.size {
                Some(n) => write!(f, "file {} ({n} bytes)", self.path),
                None => write!(f, "file {}", self.path),
            },
        }
    }
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

    /// List the immediate contents of one directory under the root, one level
    /// deep and no deeper (0.24.0).
    ///
    /// [`Workspace::find`] globs the whole tree, which answers "where is the
    /// thing I can already name". This answers the question that comes before it
    /// and that the crate had no way to ask: *what is in here*. It is the first
    /// move in an unfamiliar repository, and an agent that could only glob had to
    /// guess a name to make it.
    ///
    /// One level is the whole point rather than a limitation to work around. A
    /// directory is reported as a directory and not descended into, so the cost
    /// of looking is proportional to the directory and not to the tree beneath
    /// it, and the agent decides which branch is worth another call. Recursion is
    /// spelled by calling this again.
    ///
    /// The bounds it shares with the rest of the layer:
    ///
    /// * The directory itself is an [`Act::Read`] on its path, refused through
    ///   the same [`Workspace::check_path`] that refuses a `read_file` — a
    ///   listing of a denied directory is a cheaper way to learn what is in it,
    ///   not a different act.
    /// * An entry whose own path is denied is left out entirely, exactly as
    ///   [`Workspace::grep`] and [`Workspace::find`] leave one out: a name the
    ///   policy will not let the agent read is a name it does not get told.
    /// * A path that escapes the root is refused by [`Workspace::resolve`]
    ///   before anything is opened.
    ///
    /// Sorted by path, so two runs over an unchanged directory produce the same
    /// listing — the filesystem's own order is arbitrary and platform-dependent,
    /// and a tool whose output reorders between runs makes two traces
    /// incomparable for no gain.
    ///
    /// Unlike `grep` and `find`, nothing here is hidden by this module's
    /// ignore list: `target/` and `.git/` are not searched, but they *are* in
    /// the directory, and a listing that omitted them would be answering a
    /// different question than the one asked.
    ///
    /// The whole listing is returned. Bounding what the *model* is shown is the
    /// run loop's job, done there for every tool at once against the turn's
    /// context budget, so an embedding program calling this directly gets the
    /// directory rather than a truncated view of it.
    ///
    /// ```
    /// use io_harness::tools::Workspace;
    /// use io_harness::tools::workspace::EntryKind;
    ///
    /// # fn demo() -> io_harness::Result<()> {
    /// let dir = tempfile::tempdir()?;
    /// std::fs::create_dir_all(dir.path().join("src/deep"))?;
    /// std::fs::write(dir.path().join("src/deep/buried.rs"), "fn f() {}\n")?;
    /// let ws = Workspace::new(dir.path());
    ///
    /// // One level: the subdirectory is named, what is inside it is not.
    /// let entries = ws.list_dir("src")?;
    /// assert_eq!(entries.len(), 1);
    /// assert_eq!(entries[0].path, "src/deep");
    /// assert_eq!(entries[0].kind, EntryKind::Dir);
    ///
    /// // And the level below is one more call away.
    /// assert_eq!(ws.list_dir("src/deep")?[0].path, "src/deep/buried.rs");
    /// # Ok(()) }
    /// # demo().unwrap();
    /// ```
    pub fn list_dir(&self, rel: &str) -> Result<Vec<Entry>> {
        let abs = self.resolve(rel)?;
        self.enforce(Act::Read, rel)?;
        // Every failure names the path. `read_dir` on a file reports "Not a
        // directory (os error 20)" and on a missing one "No such file or
        // directory", and neither says *which* — the model reads this text and
        // nothing else, and cannot correct a mistake it cannot locate.
        let read = std::fs::read_dir(&abs)
            .map_err(|e| Error::Config(format!("cannot list {rel}: {e}")))?;
        let mut out = Vec::new();
        for entry in read.flatten() {
            // An entry whose type or path cannot be read is skipped rather than
            // failing the listing: one unreadable name in a directory is not a
            // reason to tell the agent nothing about the other ninety-nine.
            let Ok(ft) = entry.file_type() else { continue };
            let Ok(path) = entry
                .path()
                .strip_prefix(&self.root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
            else {
                continue;
            };
            if self.check_path(Act::Read, &path).effect == Effect::Deny {
                continue;
            }
            // Order matters: a symlink to a directory answers true to `is_dir`
            // only after being followed, and `DirEntry::file_type` does not
            // follow, so the link test has to come first to stay honest.
            let kind = if ft.is_symlink() {
                EntryKind::Symlink
            } else if ft.is_dir() {
                EntryKind::Dir
            } else {
                EntryKind::File
            };
            let size = match kind {
                // `DirEntry::metadata` does not traverse links either, so this is
                // the file's own size in every case.
                EntryKind::File => entry.metadata().ok().map(|m| m.len()),
                EntryKind::Dir | EntryKind::Symlink => None,
            };
            out.push(Entry { path, kind, size });
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    /// Read a file under the root as text. A missing file reads as empty, so the
    /// agent can create it (matching the 0.1/0.2 `FsTool` behaviour). A path the
    /// policy denies is refused before anything is read.
    ///
    /// **A file that is not text is an error (0.55.0).** It used to be an empty
    /// string: the body of this method was
    /// `std::fs::read_to_string(abs).unwrap_or_default()`, so a JPEG, an
    /// executable and a UTF-16 log all arrived at the caller as `Ok("")` —
    /// indistinguishable from the missing file whose empty read is deliberate. A
    /// caller that wants the classification rather than the error wants
    /// [`Workspace::read_typed`]; a caller that wants the bytes wants
    /// [`Workspace::read_bytes`].
    pub fn read_file(&self, rel: &str) -> Result<String> {
        match self.read_typed(rel)? {
            FileContent::Text { text, .. } => Ok(text),
            other => Err(Error::Config(
                other
                    .refusal(rel)
                    .unwrap_or_else(|| format!("{rel} is not text")),
            )),
        }
    }

    /// Read a file under the root and say what it turned out to be (0.55.0).
    ///
    /// The classification order is extension first, then bytes: an extension the
    /// crate knows names the format without decoding anything, and everything
    /// else is decided by a byte-order mark, a UTF-8 check, and a look at the
    /// leading bytes. A missing file is [`FileContent::Text`] and empty, which is
    /// 0.1.0's deliberate behaviour and the one case where "nothing" is an
    /// answer rather than a failure.
    ///
    /// The policy gate runs before any byte is read, so nothing here can be used
    /// to learn what a file the policy denies contains — or whether it exists.
    pub fn read_typed(&self, rel: &str) -> Result<FileContent> {
        let abs = self.resolve(rel)?;
        self.enforce(Act::Read, rel)?;

        // Existence first, and only then the extension: a `.png` that is not
        // there is a file to create, not an image. That ordering is what keeps
        // 0.1.0's empty-on-missing behaviour exactly as it was for every path.
        match std::fs::metadata(&abs) {
            Ok(_) => {}
            // The one case where nothing is an answer: reading a file that is not
            // there yet is how an agent decides to create it.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(FileContent::Text {
                    text: String::new(),
                    encoding: TextEncoding::Utf8,
                })
            }
            Err(e) => return Err(Error::Io(e)),
        }

        let ext = Path::new(rel)
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if let Some(format) = image_format_for(&ext) {
            return Ok(FileContent::Image { format });
        }
        if let Some((format, tool)) = document_for(&ext) {
            return Ok(FileContent::Document { format, tool });
        }

        let bytes = std::fs::read(&abs).map_err(Error::Io)?;

        let text = |text: String, encoding| Ok(FileContent::Text { text, encoding });
        match bytes.as_slice() {
            [0xef, 0xbb, 0xbf, rest @ ..] => match std::str::from_utf8(rest) {
                Ok(s) => text(s.to_string(), TextEncoding::Utf8Bom),
                Err(_) => Ok(FileContent::Binary {
                    bytes: bytes.len() as u64,
                    kind: sniff_binary(&bytes),
                }),
            },
            [0xff, 0xfe, rest @ ..] if decode_utf16(rest, true).is_some() => text(
                decode_utf16(rest, true).unwrap_or_default(),
                TextEncoding::Utf16Le,
            ),
            [0xfe, 0xff, rest @ ..] if decode_utf16(rest, false).is_some() => text(
                decode_utf16(rest, false).unwrap_or_default(),
                TextEncoding::Utf16Be,
            ),
            _ => match String::from_utf8(bytes) {
                Ok(s) => text(s, TextEncoding::Utf8),
                Err(e) => {
                    let bytes = e.into_bytes();
                    Ok(FileContent::Binary {
                        bytes: bytes.len() as u64,
                        kind: sniff_binary(&bytes),
                    })
                }
            },
        }
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

    /// Replace one exact occurrence of `search` with `replace` in a file under
    /// the root, leaving everything else byte-identical (0.17.0).
    ///
    /// The same [`Act::Write`] gate as [`Workspace::write_file`], on the same
    /// path, because it is the same act — a partial edit is not a lesser one, and
    /// a policy that refuses a write to `secrets/*` refuses this too.
    ///
    /// **Exactly one match, or nothing happens.** A `search` that appears zero
    /// times or more than once is an [`Error::Config`] naming the file and the
    /// count, and the file is not touched. That is the whole capability: an edit
    /// that silently picks one of three occurrences is not a cheaper write, it is
    /// a corrupting one, and the agent cannot correct for a mistake it is not
    /// told about. The model's move when it sees the count is to lengthen
    /// `search` until it is unique.
    ///
    /// ```
    /// use io_harness::tools::Workspace;
    /// use io_harness::tools::workspace::Wrote;
    ///
    /// # fn demo() -> io_harness::Result<()> {
    /// let dir = tempfile::tempdir()?;
    /// std::fs::write(dir.path().join("a.rs"), "fn one() {}\nfn two() {}\n")?;
    /// let ws = Workspace::new(dir.path());
    ///
    /// // Unique: replaced, and the rest of the file is untouched.
    /// assert_eq!(ws.edit_file("a.rs", "fn two", "fn three")?, Wrote::Changed);
    /// assert_eq!(ws.read_file("a.rs")?, "fn one() {}\nfn three() {}\n");
    ///
    /// // Absent, and ambiguous, both refuse rather than guess.
    /// assert!(ws.edit_file("a.rs", "fn four", "x").is_err());
    /// assert!(ws.edit_file("a.rs", "fn ", "x").is_err());
    /// # Ok(()) }
    /// # demo().unwrap();
    /// ```
    ///
    /// [`Error::Config`]: crate::Error::Config
    pub fn edit_file(&self, rel: &str, search: &str, replace: &str) -> Result<Wrote> {
        let abs = self.resolve(rel)?;
        self.enforce(Act::Write, rel)?;
        if search.is_empty() {
            return Err(Error::Config(format!(
                "edit_file needs a non-empty search string; {rel} was not changed"
            )));
        }
        // Read through the same reader `read_file` uses, so a missing file is the
        // empty string and therefore a zero-match refusal — an edit cannot create
        // a file, and reporting "no such text" for a file that does not exist is
        // the same answer by a shorter route.
        let current = std::fs::read_to_string(&abs).unwrap_or_default();
        match current.matches(search).count() {
            1 => {}
            0 => {
                return Err(Error::Config(format!(
                    "edit_file found no occurrence of that text in {rel}; nothing was changed. \
                     Read the file and copy the text to replace from it exactly, whitespace \
                     included"
                )))
            }
            n => {
                return Err(Error::Config(format!(
                    "edit_file found {n} occurrences of that text in {rel}, and will not guess \
                     which one you meant; nothing was changed. Extend the search text with \
                     surrounding lines until it appears exactly once"
                )))
            }
        }
        let updated = current.replacen(search, replace, 1);
        let did = Wrote::classify(std::fs::read(&abs), updated.as_bytes());
        std::fs::write(abs, updated)?;
        Ok(did)
    }

    /// Apply a unified diff to a file under the root, or change nothing (0.51.0).
    ///
    /// [`Workspace::edit_file`] is one search-and-replace, so a change touching
    /// four places in a file is four calls — and after the second one the model
    /// is editing a file whose line numbers have moved under the text it read.
    /// This takes all four as one anchored patch. Same [`Act::Write`] check on
    /// the same path, because it is the same act.
    ///
    /// **All or nothing.** Every hunk is matched against the file as it stands,
    /// at its own recorded position, *before* anything is written; a patch whose
    /// third hunk does not fit leaves the file byte-identical and says which hunk
    /// and what it expected. A half-patched file is the outcome this ordering
    /// exists to make impossible, and it is not something a caller can inspect
    /// their way out of afterwards.
    ///
    /// It cannot create a file, for the reason [`Workspace::edit_file`] cannot:
    /// a patch is anchored to text that is already there, and creating is
    /// [`Workspace::write_file`]'s job.
    pub fn patch_file(&self, rel: &str, patch: &str) -> Result<Wrote> {
        let abs = self.resolve(rel)?;
        self.enforce(Act::Write, rel)?;
        if !abs.exists() {
            return Err(Error::Config(format!(
                "there is no {rel} to patch; a patch is anchored to text that is already there, \
                 so use write_file to create a file"
            )));
        }
        let current = std::fs::read_to_string(&abs).map_err(|e| {
            Error::Config(format!(
                "{rel} could not be read as text, so it cannot be patched: {e}"
            ))
        })?;
        let hunks = crate::diff::parse(patch)?;
        let updated = crate::diff::apply(&current, &hunks)?;
        let did = Wrote::classify(std::fs::read(&abs), updated.as_bytes());
        std::fs::write(abs, updated)?;
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
        // 0.55.0 — the text reader used to answer `Ok("")` here, which is what
        // made this pair necessary and also what made a binary read look like an
        // empty file. It now says what the file is instead.
        let err = ws.read_file("blob.bin").unwrap_err().to_string();
        assert!(
            err.contains("blob.bin") && err.contains("5 bytes"),
            "the text reader names what it will not decode, got {err}"
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
