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

/// The largest file [`Workspace::read_bytes`] will pull into memory (0.74.0).
///
/// `read_bytes` is the one entry every document parser starts at — pdf, docx,
/// xlsx, pptx, barcode all begin with the whole file as a `Vec<u8>` — and it had
/// no ceiling, so a single oversized file in the workspace exhausted memory
/// before any parser saw a byte, and an archive bomb got its input side for
/// free. 64 MiB is the number because it is an order of magnitude above the
/// largest office document a coding agent legitimately opens and an order of
/// magnitude below what a host can lose to one allocation. A caller that
/// genuinely needs more is not reading a document: it wants a shell command that
/// streams.
pub const MAX_DOCUMENT_BYTES: u64 = 64 * 1024 * 1024;

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
/// // A lone 0x80 is not valid UTF-8 in any position, and the bytes before it
/// // say what the file is.
/// std::fs::write(dir.path().join("blob.bin"), [0x7f, b'E', b'L', b'F', 0x80])?;
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
                    format!(
                        "{rel} is {format}, so nothing was decoded — `{tool}` is what reads one"
                    )
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
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&pair| {
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
    /// refused — the target fails even though the link's own path passes. That
    /// holds for a link whose target does not exist yet as well (0.74.0): a
    /// *dangling* link used to be indistinguishable from a leaf about to be
    /// created, so `src/a.rs -> ../io.local.toml` was graded as `src/a.rs` and a
    /// hostile clone could ship one. `read_link` tells the two apart now, where
    /// `canonicalize` reports the same "not found" for both.
    ///
    /// **A path that leaves the root is denied outright, whether or not it exists
    /// yet (0.74.0).** Two holes made that untrue before. The escape test sat
    /// inside `if let Ok(canon) = abs.canonicalize()`, which fails unless every
    /// component already exists, so a file about to be *created* skipped the test
    /// entirely — and the leaf of a write is exactly what usually does not exist.
    /// Separately, a path [`Workspace::resolve`] refused fell through to grading
    /// the lexical form alone, and that form collapsed a `..` instead of refusing
    /// it, so `../../outside` graded as `outside` and came back
    /// [`Effect::Allow`]. Containment is now decided by `contain_under_root`,
    /// which resolves the deepest component that *does* exist; either escape is
    /// [`Effect::Deny`] with no layer attributed, because no layer can permit it.
    pub fn check_path(&self, act: Act, rel: &str) -> Verdict {
        // Anything `resolve` refuses is refused here too. Grading the lexical
        // form of an escaping path grades a path that is not the one that would
        // be opened, and the answer to a path with no in-workspace meaning is a
        // refusal, not the verdict for some other path.
        let (Ok(abs), Some(lexical)) = (self.resolve(rel), normalize(rel)) else {
            return denied("<path escapes workspace root>");
        };
        let mut worst = self.policy.check(act, &lexical);

        // The contained form: the deepest existing ancestor canonicalized, with
        // whatever does not exist yet joined back on. Grading it too is what
        // judges a symlink by where it lands as well as by its own name, and it
        // can only ever make the verdict stricter.
        let (Ok(contained), Ok(root_real)) = (
            contain_under_root(&self.root, &abs),
            deepest_existing(&self.root),
        ) else {
            // Resolves outside the root: a symlink escape, refused regardless of
            // what the policy says about the link itself.
            return denied("<resolves outside workspace root>");
        };
        if let Ok(rel_canon) = contained.strip_prefix(&root_real) {
            let rel_canon = rel_canon.to_string_lossy().replace('\\', "/");
            let v = self.policy.check(act, &rel_canon);
            if v.effect > worst.effect {
                worst = v;
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

    /// Refuse the action against an already-contained *absolute* path, graded by
    /// the workspace-relative name a policy rule matches (0.74.0).
    ///
    /// [`Workspace::write_leaf`]'s retry is the caller and the reason this exists.
    /// Everything else arrives holding the relative path the model wrote; a
    /// symbolic link's destination is discovered from the link, so it has no such
    /// name until one is derived from it — and without one it was never graded at
    /// all, which is how a link at the leaf reached a denied file.
    ///
    /// The refusal that comes back names the destination rather than the link, so
    /// the model reads which file was actually refused.
    ///
    /// Unix only, because the retry it serves is: Windows has no `O_NOFOLLOW`, so
    /// there is no second open there to re-decide.
    #[cfg(unix)]
    fn enforce_contained(&self, act: Act, abs: &Path) -> Result<()> {
        let root_real = deepest_existing(&self.root)?;
        let rel = abs
            .strip_prefix(&root_real)
            .map_err(|_| outside_root(abs, &self.root))?;
        self.enforce(act, &rel.to_string_lossy().replace('\\', "/"))
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
    ///
    /// **A file over [`MAX_DOCUMENT_BYTES`] is refused rather than read
    /// (0.80.0),** in the same words [`Workspace::read_bytes`] refuses it. The
    /// ceiling arrived on the byte read alone and this is the same allocation
    /// reached through the door an agent uses first: every [`Workspace::read_file`]
    /// is a `read_typed`. An extension the crate classifies without decoding —
    /// an image, a document — is answered before the limit applies, because
    /// naming a format costs no bytes.
    pub fn read_typed(&self, rel: &str) -> Result<FileContent> {
        use std::io::Read as _;

        let abs = self.resolve(rel)?;
        self.enforce(Act::Read, rel)?;

        // Existence first, and only then the extension: a `.png` that is not
        // there is a file to create, not an image. That ordering is what keeps
        // 0.1.0's empty-on-missing behaviour exactly as it was for every path.
        let len = match std::fs::metadata(&abs) {
            Ok(meta) => meta.len(),
            // The one case where nothing is an answer: reading a file that is not
            // there yet is how an agent decides to create it.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(FileContent::Text {
                    text: String::new(),
                    encoding: TextEncoding::Utf8,
                })
            }
            Err(e) => return Err(Error::Io(e)),
        };

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

        // The same ceiling `read_bytes` holds, on the same limit and with the
        // same refusal. M15 put it on the byte read alone, and this is the second
        // door into the same allocation: every `read_file` is a call to this
        // method, so an oversized file exhausted memory through an ordinary text
        // read while the byte read beside it refused — and the text read is the
        // one an agent reaches for first. All three entry points answer alike.
        //
        // The size is the `metadata` above rather than a second `stat`, and the
        // read is capped as well for the reason `read_bytes` caps its own: a path
        // that reports zero bytes can still stream without end, which is what a
        // character device or a `/proc` entry inside the root does. Refusing is
        // the whole answer — handing the model the front of a file it believes it
        // has read is the confident wrong reading 0.55.0 removed the empty string
        // to prevent.
        if len > MAX_DOCUMENT_BYTES {
            return Err(too_large(rel, &format!("{len} bytes")));
        }
        let mut bytes = Vec::new();
        std::fs::File::open(&abs)
            .map_err(|e| read_error(rel, e))?
            .take(MAX_DOCUMENT_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| read_error(rel, e))?;
        if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
            return Err(too_large(rel, "longer than the size it reports"));
        }

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
    ///
    /// The bytes go through one opener that on unix refuses to follow a symbolic
    /// link at the final component, so the path the gate graded is the path that
    /// receives the bytes.
    pub fn write_file(&self, rel: &str, content: &str) -> Result<Wrote> {
        let abs = self.resolve(rel)?;
        self.enforce(Act::Write, rel)?;
        let did = Wrote::classify(std::fs::read(&abs), content.as_bytes());
        self.write_leaf(&abs, content.as_bytes())?;
        Ok(did)
    }

    /// Write `content` to `abs`, refusing to follow a symlink at the leaf
    /// (0.74.0).
    ///
    /// The gate canonicalizes to decide and the write used to open the
    /// un-canonicalized path, which leaves a window: a live `shell_start` handle
    /// looping on `ln -sfn /etc/cron.d/x src/a.rs` can replace a gated-allowed
    /// file with a link to anywhere between the check and the write, and turn
    /// write-inside-the-workspace into write-anywhere. Opening the leaf with
    /// `O_NOFOLLOW` closes it: the descriptor written through is either the file
    /// that was checked or nothing at all.
    ///
    /// A link that stays *inside* the root is still writable, because that is a
    /// capability 0.73.0 had and nothing here is meant to remove: the open is
    /// retried once against where the link points, and that destination goes
    /// through [`contain_under_root`] **and through the policy** before it is
    /// opened. Both, because they answer different questions and only the pair of
    /// them makes "following a link lands where an ordinary write to its
    /// destination would have" true. Containment alone was the claim this
    /// docstring used to make, and it was false: a link is a different path from
    /// its destination — that is the whole argument for re-deciding containment —
    /// and a different path gets a different verdict. A link planted between the
    /// gate and this open, which is the race `O_NOFOLLOW` exists to close, reached
    /// `io.local.toml` and `.git/hooks/*` through a name the gate had allowed
    /// while the deny that names those files was never consulted.
    ///
    /// **Windows** has no `O_NOFOLLOW` and no `OpenOptions` equivalent, so the
    /// write there is an ordinary one and containment rests on
    /// [`contain_under_root`] alone. The race this closes needs the attacker to
    /// create a symbolic link, which on Windows needs
    /// `SeCreateSymbolicLinkPrivilege` or developer mode — not something an
    /// unprivileged agent has.
    fn write_leaf(&self, abs: &Path, content: &[u8]) -> Result<()> {
        #[cfg(not(unix))]
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent)?;
        }
        #[cfg(unix)]
        {
            use std::io::Write as _;

            // 0.80.0 — [`walk_open`] creates every intermediate directory and
            // opens the leaf, each step from a descriptor rather than from a
            // path, so the directories are not created here and no component is
            // resolved twice.
            let mut file = match walk_open(&self.root, abs) {
                Ok(f) => f,
                // The leaf is a symbolic link, which is a different path from
                // its destination and gets its own verdict — the re-decision
                // this arm exists for.
                Err(NoFollow::LeafIsLink) => {
                    let dest = std::fs::read_link(abs).map_err(Error::Io)?;
                    let dest = match abs.parent() {
                        Some(parent) if dest.is_relative() => parent.join(dest),
                        _ => dest,
                    };
                    let dest = contain_under_root(&self.root, &dest)?;
                    self.enforce_contained(Act::Write, &dest)?;
                    match walk_open(&self.root, &dest) {
                        Ok(f) => f,
                        // A link to a link, or a link whose destination sits
                        // under a swapped component. One re-decision is the
                        // claim this arm makes; a chain of them is a walk with
                        // no fixed point, so it is refused.
                        Err(NoFollow::LeafIsLink | NoFollow::ComponentIsLink { .. }) => {
                            return Err(link_chain(abs, &dest))
                        }
                        Err(NoFollow::Io(e)) => return Err(Error::Io(e)),
                    }
                }
                Err(NoFollow::ComponentIsLink { component }) => {
                    return Err(swapped_component(abs, &component))
                }
                Err(NoFollow::Io(e)) => return Err(Error::Io(e)),
            };
            file.write_all(content).map_err(Error::Io)
        }
        #[cfg(not(unix))]
        {
            std::fs::write(abs, content).map_err(Error::Io)
        }
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
        self.write_leaf(&abs, updated.as_bytes())?;
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
        self.write_leaf(&abs, updated.as_bytes())?;
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
    ///
    /// **A file over [`MAX_DOCUMENT_BYTES`] is refused rather than read
    /// (0.74.0).** The size is asked for before any byte is, so a file far too
    /// large costs one `stat` and not an allocation; the read is then capped as
    /// well, because a path whose reported size is zero can still stream without
    /// end — a character device or a `/proc` entry inside the root does exactly
    /// that. Both limbs are the same limit, and refusing is the whole answer:
    /// truncating a document and handing the front of it to a parser produces a
    /// confident wrong reading of a file the model believes it has seen.
    pub fn read_bytes(&self, rel: &str) -> Result<Vec<u8>> {
        use std::io::Read as _;

        let abs = self.resolve(rel)?;
        self.enforce(Act::Read, rel)?;
        let len = std::fs::metadata(&abs)
            .map_err(|e| read_error(rel, e))?
            .len();
        if len > MAX_DOCUMENT_BYTES {
            return Err(too_large(rel, &format!("{len} bytes")));
        }
        let mut bytes = Vec::new();
        std::fs::File::open(&abs)
            .map_err(|e| read_error(rel, e))?
            // One byte past the limit, so a file that reports one size and
            // streams another is caught by the length of what actually arrived.
            .take(MAX_DOCUMENT_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| read_error(rel, e))?;
        if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
            return Err(too_large(rel, "longer than the size it reports"));
        }
        Ok(bytes)
    }

    /// Write a file under the root as bytes, creating parent directories and
    /// reporting whether it changed anything.
    ///
    /// The byte twin of [`Workspace::write_file`], through the same policy gate
    /// and the same symlink-refusing opener, with the same semantics: the write
    /// happens in every case, [`Wrote::Unchanged`] included, so nothing about the
    /// file's state depends on the comparison.
    pub fn write_bytes(&self, rel: &str, content: &[u8]) -> Result<Wrote> {
        let abs = self.resolve(rel)?;
        self.enforce(Act::Write, rel)?;
        let did = Wrote::classify(std::fs::read(&abs), content);
        self.write_leaf(&abs, content)?;
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

/// The refusal a path that leaves the root gets: what it was, why, and the one
/// thing that would work instead.
/// What stopped [`walk_open`], told apart because the three answers are three
/// different things to do (0.80.0).
#[cfg(unix)]
enum NoFollow {
    /// The final component is a symbolic link. Not a refusal: the caller reads
    /// where it points and re-decides containment against that path, which is
    /// the behaviour `write_file` has documented since 0.74.0.
    LeafIsLink,
    /// A *directory* component is a symbolic link, or is not a directory at all.
    /// Always a refusal — nothing above the leaf is ever re-decided, because a
    /// component that changed under the walk is the race itself.
    ComponentIsLink {
        component: std::ffi::OsString,
    },
    Io(std::io::Error),
}

/// Open `abs` for writing without ever resolving a component by path.
///
/// **The parent-directory race, closed (0.80.0.)** Until this release the parent
/// chain was checked by [`contain_under_root`] and then handed to
/// `create_dir_all` and an `O_NOFOLLOW` open, both of which resolve the whole
/// path again — and `O_NOFOLLOW` covers the final component only. A directory
/// swapped for a symbolic link in the window between the check and the write was
/// followed by both: `root/a/b/x` with `a` replaced by a link to `/etc` created
/// `/etc/b` and wrote `/etc/b/x`, past a gate that had graded a path inside the
/// root. Every writing entry point in this file routes through `write_leaf`, so
/// that was the whole write surface. Winning the window needs a second writer —
/// a live `shell_start` handle, or a process the run spawned — which is exactly
/// what this crate's threat model gives an agent.
///
/// So each component is opened from the descriptor of the one above it, with
/// `O_DIRECTORY | O_NOFOLLOW`, and the descriptor is what the next step names.
/// A component swapped after it was opened is a component the walk is no longer
/// looking at: the descriptor refers to the directory that was there, not to the
/// name. There is no window left to win.
///
/// **The root itself is opened by path and its own links are followed**, which is
/// deliberate rather than an omission. It is the operator's own directory,
/// resolved before the run started and not reachable by anything the run does —
/// and on macOS every temporary workspace is reached through `/tmp`, which is a
/// link to `/private/tmp`. Refusing links there would refuse the ordinary case
/// while closing nothing.
///
/// The `unsafe` is three `libc` calls, and it is the first in `src/tools/`. There
/// is no safe `openat` in `std` and no way to hold a directory descriptor across
/// a resolution without one; the alternative is a dependency, which this crate's
/// contract does not allow for something this small.
#[cfg(unix)]
fn walk_open(root: &Path, abs: &Path) -> std::result::Result<std::fs::File, NoFollow> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt as _;

    let bad_input = || NoFollow::Io(std::io::Error::from(std::io::ErrorKind::InvalidInput));

    // Two callers reach here with paths built two ways, and both have to work:
    // [`Workspace::resolve`] joins onto the root as the operator wrote it, while
    // [`contain_under_root`] answers with the canonicalised form. Whichever the
    // path is relative to is the directory the walk starts from — the base is
    // the descriptor everything below is opened from, so the two only have to
    // name the same directory, not the same string.
    let (base, rel) = match abs.strip_prefix(root) {
        Ok(rel) => (root.to_path_buf(), rel.to_path_buf()),
        Err(_) => {
            let real = deepest_existing(root).map_err(|_| bad_input())?;
            let rel = abs
                .strip_prefix(&real)
                .map_err(|_| bad_input())?
                .to_path_buf();
            (real, rel)
        }
    };

    let mut names: Vec<&std::ffi::OsStr> = Vec::new();
    for c in rel.components() {
        match c {
            std::path::Component::Normal(name) => names.push(name),
            // `contain_under_root` resolved every `.`, `..` and link before this,
            // so anything else here is a caller that skipped it.
            _ => return Err(bad_input()),
        }
    }
    let Some((leaf, dirs)) = names.split_last() else {
        return Err(bad_input());
    };

    let mut dir: OwnedFd = std::fs::File::open(&base).map_err(NoFollow::Io)?.into();
    for name in dirs {
        let c = CString::new(name.as_bytes()).map_err(|_| bad_input())?;
        // SAFETY: `dir` is an open directory descriptor owned by this function
        // and `c` is a NUL-terminated component name. Neither call retains a
        // pointer, and the mode is masked by the process umask as it is for
        // `create_dir_all`.
        unsafe {
            if libc::mkdirat(dir.as_raw_fd(), c.as_ptr(), 0o777) != 0 {
                let e = std::io::Error::last_os_error();
                // An existing directory is the ordinary case. An existing
                // *link* is caught by the open below, not here, because
                // `mkdirat` answers `EEXIST` for both.
                if e.raw_os_error() != Some(libc::EEXIST) {
                    return Err(NoFollow::Io(e));
                }
            }
        }
        // SAFETY: as above. The descriptor returned is fresh and unowned until
        // `OwnedFd` takes it on the next line.
        let fd = unsafe {
            libc::openat(
                dir.as_raw_fd(),
                c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            return Err(match e.raw_os_error() {
                // `ELOOP` is a link; `EMLINK` is the same answer on FreeBSD,
                // NetBSD and DragonFly, which chose the other errno where POSIX
                // left it unspecified; `ENOTDIR` is a component replaced by a
                // file. All three mean the path is no longer the one that was
                // graded.
                Some(libc::ELOOP | libc::EMLINK | libc::ENOTDIR) => NoFollow::ComponentIsLink {
                    component: name.to_os_string(),
                },
                _ => NoFollow::Io(e),
            });
        }
        // SAFETY: `fd` is a descriptor this function has just been given and
        // nothing else holds. The previous `dir` is closed by the assignment.
        dir = unsafe { OwnedFd::from_raw_fd(fd) };
    }

    let c = CString::new(leaf.as_bytes()).map_err(|_| bad_input())?;
    // SAFETY: as above. `openat` is variadic and takes the mode when `O_CREAT`
    // is set; it is masked by the umask, matching `File::create`.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            c.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o666 as libc::c_uint,
        )
    };
    if fd < 0 {
        let e = std::io::Error::last_os_error();
        return Err(match e.raw_os_error() {
            Some(libc::ELOOP | libc::EMLINK) => NoFollow::LeafIsLink,
            _ => NoFollow::Io(e),
        });
    }
    // SAFETY: `fd` is a fresh descriptor nothing else holds.
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

/// The refusal for a directory component that is a link, or is not a directory.
#[cfg(unix)]
fn swapped_component(path: &Path, component: &std::ffi::OsStr) -> Error {
    Error::Config(format!(
        "{} was not written: the directory {} on the way to it is a symbolic link or is not a \
         directory, so the path no longer names what it named when it was allowed. Nothing was \
         created and nothing was written",
        path.display(),
        component.to_string_lossy()
    ))
}

/// The refusal for a link whose destination is itself a link, or sits under a
/// component that is one.
#[cfg(unix)]
fn link_chain(path: &Path, dest: &Path) -> Error {
    Error::Config(format!(
        "{} is a symbolic link to {}, which is itself a link or lies under one. One redirection is \
         re-decided against the policy; a chain is refused. Name the file you mean",
        path.display(),
        dest.display()
    ))
}

fn outside_root(path: &Path, root: &Path) -> Error {
    Error::Config(format!(
        "{} is outside the workspace root {}, so nothing was opened — a `..` or a symbolic link \
         in that path leaves the workspace root. Name a path inside it instead",
        path.display(),
        root.display()
    ))
}

/// The refusal for a document read over [`MAX_DOCUMENT_BYTES`].
fn too_large(rel: &str, size: &str) -> Error {
    Error::Config(format!(
        "{rel} is {size}, over the {MAX_DOCUMENT_BYTES}-byte limit on one document read, so \
         nothing was read. Read the part you need with a command that streams, or point this at \
         a smaller file"
    ))
}

/// A byte read's io failure, with a missing file named as such rather than as an
/// operating-system error the model has to decode.
fn read_error(rel: &str, e: std::io::Error) -> Error {
    match e.kind() {
        std::io::ErrorKind::NotFound => Error::Config(format!("no such file: {rel}")),
        _ => Error::Io(e),
    }
}

/// A refusal no layer wrote and no layer can override: the path is not a path in
/// this workspace at all.
fn denied(rule: &str) -> Verdict {
    Verdict {
        effect: Effect::Deny,
        rule: Some(rule.into()),
        layer: None,
    }
}

/// Resolve `path` under `root` so the result cannot name a file outside the
/// canonical root — whether or not that file exists yet (0.74.0).
///
/// `canonicalize` alone cannot decide this. It fails unless every component
/// already exists, so a path whose leaf is about to be *created* skipped the
/// check entirely, and the leaf of a write is exactly what usually does not
/// exist: `~/.bashrc`, `authorized_keys`, `.config/autostart/*` and
/// `.git/hooks/*` are all absent until something writes them. This resolves the
/// deepest component that *does* exist, canonicalizes that, requires it under
/// the canonical root, and joins back the components that do not exist yet.
///
/// `path` is joined onto the root when it is relative and taken as given when it
/// is absolute; an absolute path outside the root is refused like any other
/// escape. A `..` is resolved by canonicalizing the existing prefix, so it
/// cannot climb out; a `..` among the components that do not exist is refused
/// rather than collapsed, because there is nothing on disk to resolve it against
/// and collapsing it lexically is the bug this exists to remove.
///
/// What a caller may rely on:
/// - the returned path starts with the canonical root, component by component;
/// - no symbolic link is followed *out* of the root at any depth, because the
///   whole existing prefix is canonical;
/// - a link that stays inside the root still resolves and is still usable;
/// - a *dangling* link resolves to what it names rather than to itself (0.74.0),
///   so a link checked into a repository is graded by its destination whether or
///   not that destination exists yet — see [`deepest_existing`];
/// - a root that does not exist yet is resolved the same way, so a workspace
///   whose directory is about to be created behaves as it did before this check.
///
/// What it does not promise: that the path *stays* what this returned. Every link
/// on it is resolved at the moment of the call, and a component swapped for a link
/// after this returns is outside its knowledge. A writer that needs the checked
/// path to be the written path opens the leaf with `O_NOFOLLOW` and re-decides
/// about the destination — see [`Workspace::write_leaf`].
///
/// The error is an [`Error::Config`] naming the path, the root, and what to
/// write instead.
pub(crate) fn contain_under_root(root: &Path, path: &Path) -> Result<PathBuf> {
    let root_real = deepest_existing(root).map_err(|_| outside_root(path, root))?;
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root_real.join(path)
    };
    let target = deepest_existing(&joined).map_err(|_| outside_root(path, root))?;
    if !target.starts_with(&root_real) {
        return Err(outside_root(path, root));
    }
    Ok(target)
}

/// The deepest ancestor of `path` that exists, canonicalized, with the
/// components that do not exist joined back on.
///
/// Only a "not found" stops the walk. Any other failure — a component that is a
/// file rather than a directory, a link that loops, a directory that cannot be
/// searched — is refused rather than walked past, because a component this
/// cannot read is a component whose target it cannot vouch for.
///
/// **A dangling symbolic link is not an absent path (0.74.0).** `canonicalize`
/// answers `NotFound` for both, so a link whose target does not exist *yet* was
/// graded as a leaf about to be created and answered with the link's own name —
/// while every writer that followed it landed on the destination. That is
/// precisely the case a hostile clone ships: git stores a symbolic link as a blob
/// holding its target string and never requires the target to exist, so
/// `src/a.rs -> ../io.local.toml` arrives with a checkout and needs no `exec`
/// permission to plant. A link whose target *does* exist was never affected —
/// `canonicalize` resolves it and the destination is what gets graded — which is
/// why the dangling half was the only one that escaped.
///
/// `read_link` tells the two apart where `canonicalize` cannot: it succeeds on a
/// link and fails with `EINVAL` on anything else, so no second `stat` is needed.
/// The walk then goes on resolving from where the link points, which is the
/// destination's own answer: outside the root it is refused by
/// [`contain_under_root`] like any other escape, inside the root it resolves to
/// the file it names and is graded as that file.
///
/// The hop ceiling is not there for cycles — `canonicalize` reports a cycle as
/// `ELOOP`, which is already refused above — but so that a chain no bound is known
/// for cannot spin here.
fn deepest_existing(path: &Path) -> Result<PathBuf> {
    // Links followed before the path is refused instead. `SYMLOOP_MAX` is 8 on
    // the hosts this runs on; 40 is what Linux's own resolver allows.
    const MAX_LINK_HOPS: u32 = 40;

    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut at = path.to_path_buf();
    let mut hops = 0u32;
    loop {
        match at.canonicalize() {
            Ok(canon) => {
                let mut out = canon;
                out.extend(tail.iter().rev().map(|c| c.as_os_str()));
                return Ok(out);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Ok(dest) = std::fs::read_link(&at) {
                    hops += 1;
                    if hops > MAX_LINK_HOPS {
                        return Err(escape(&path.to_string_lossy()));
                    }
                    // A relative target is relative to the link's own directory,
                    // not to the process's; resolving it any other way is how a
                    // `../` in a link target stops meaning what it says.
                    let next = match at.parent() {
                        Some(parent) if dest.is_relative() => parent.join(dest),
                        _ => dest,
                    };
                    at = next;
                    continue;
                }
            }
            Err(_) => return Err(escape(&path.to_string_lossy())),
        }
        // `file_name` is `None` for `..`, for `.` and for a bare root. None of
        // those can be resolved without a directory on disk to resolve them
        // against, and guessing is what let `../../outside` through.
        let (Some(name), Some(parent)) = (at.file_name(), at.parent()) else {
            return Err(escape(&path.to_string_lossy()));
        };
        let (name, parent) = (name.to_os_string(), parent.to_path_buf());
        tail.push(name);
        at = parent;
    }
}

/// A model-supplied path in the `/`-separated, `.`-free form policy globs match
/// against, so `./src/a.rs` and `src/a.rs` are the same target to a rule.
///
/// `None` when the path climbs above its own start. That used to be a silent
/// `Vec::pop` on an empty vector — a no-op — so `../../outside` normalized to
/// `outside` and was graded as though it named a file in the workspace. A path
/// with no in-workspace form has no verdict to give, and the caller's only
/// correct answer to one is a refusal.
fn normalize(rel: &str) -> Option<String> {
    let s = rel.replace('\\', "/");
    let mut out: Vec<&str> = Vec::new();
    for part in s.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                out.pop()?;
            }
            p => out.push(p),
        }
    }
    Some(out.join("/"))
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

    /// 0.80.0 — the parent-directory race, asserted at the only layer that can
    /// see it.
    ///
    /// The race is a directory swapped for a symbolic link *after*
    /// `contain_under_root` graded the path and *before* the write. A test
    /// cannot win a real race deterministically, so it does the equivalent: it
    /// grades the path while the directory is real, plants the link, and then
    /// calls the writing half with the graded path — which is exactly the state
    /// the second writer leaves behind. Against 0.79.1 this wrote through the
    /// link and created a file outside the workspace.
    #[cfg(unix)]
    #[test]
    fn a_directory_swapped_for_a_link_after_the_check_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ws = Workspace::new(root);

        // Graded while `a` is an ordinary directory, as the gate would.
        std::fs::create_dir_all(root.join("a")).unwrap();
        let abs = contain_under_root(root, Path::new("a/b/x")).unwrap();

        // The window: `a` becomes a link out of the workspace.
        std::fs::remove_dir(root.join("a")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("a")).unwrap();

        let err = ws
            .write_leaf(&abs, b"escaped")
            .expect_err("a swapped component must refuse the write");
        assert!(
            err.to_string().contains("symbolic link"),
            "the refusal says why: {err}"
        );
        assert!(
            !outside.path().join("b").exists(),
            "and nothing is created on the far side of the link"
        );
    }

    /// The control. Without it the test above passes against a build whose
    /// writes are broken outright, which is the failure mode a one-armed
    /// containment test always has.
    #[cfg(unix)]
    #[test]
    fn an_ordinary_nested_write_still_creates_its_directories() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path());

        ws.write_file("a/b/x.txt", "kept").unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join("a/b/x.txt")).unwrap(),
            "kept"
        );
    }

    /// And a link *inside* the root still gets its re-decision rather than a
    /// refusal — the behaviour `write_file` has documented since 0.74.0, which
    /// the walk must not have taken away.
    #[cfg(unix)]
    #[test]
    fn a_leaf_link_inside_the_root_is_still_followed_after_re_deciding() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let ws = Workspace::new(root);

        std::fs::write(root.join("real.txt"), "before").unwrap();
        std::os::unix::fs::symlink(root.join("real.txt"), root.join("link.txt")).unwrap();

        ws.write_file("link.txt", "after").unwrap();

        assert_eq!(
            std::fs::read_to_string(root.join("real.txt")).unwrap(),
            "after",
            "the write lands on the destination, which is what was re-decided"
        );
    }
}
