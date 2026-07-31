//! Parsing a command line so the boundary can be applied to what it actually runs.
//!
//! [`exec`](super::exec) takes an argv array and hands it to the operating system
//! as an argv array, which is what makes its permission check mean something: `;`,
//! `&&` and `$( )` are ordinary bytes inside one argument because nothing on that
//! path ever parses one. The cost of that guarantee is that half of real work is
//! unreachable. `cd infra && kubectl get pods | grep CrashLoop` is not a thing a
//! model can express through a single argv, so a pipeline, a redirect or a
//! sequence has to be smuggled in as a script the harness cannot see inside of —
//! and a boundary applied to `sh -c <opaque string>` is a boundary applied to
//! nothing.
//!
//! This module parses the line itself. Every sub-command it finds is checked
//! against [`Act::Exec`](crate::Act::Exec) and every redirect target against
//! [`Act::Write`](crate::Act::Write) or [`Act::Read`](crate::Act::Read), by the
//! same [`gate`] every other tool routes through, before anything is spawned. What
//! it produces is an argv per stage, spawned exactly the way `exec` spawns one.
//! **There is no host shell anywhere after the parse** — no `sh -c`, no `cmd /c`.
//! Handing the original string to a shell after parsing it would make the parse
//! decorative.
//!
//! [`gate`]: crate::run
//!
//! ## Conservative, not complete
//!
//! The grammar here is a deliberately small subset of POSIX: quoted and escaped
//! words, `|`, `;`, `&&`, `||`, and the `>` `>>` `<` `2>` `2>>` `2>&1` redirects.
//! Everything else — command substitution, parameter expansion, arithmetic,
//! subshells, heredocs, background, functions, `if`/`for`/`while`/`case` — is
//! refused by name, with a reason a human can read.
//!
//! That is not a first cut to be grown later. A parser this tool depends on has to
//! get exactly one thing right: never producing a set of sub-commands that differs
//! from what gets executed. A parser that is *complete* has to get that right for
//! the whole language; a parser that is *conservative* has to get it right only
//! for the fragment it admits, and may fail closed on everything else. The second
//! is a much smaller thing to be sure of, and being sure of it is the entire
//! product.
//!
//! It is also why this is written here rather than taken from a crate. Three were
//! evaluated when the release was planned. `yash-syntax` is GPL-3.0-or-later and
//! this crate is Apache-2.0 and designed to be linked into someone else's program,
//! so a copyleft parser would propagate to every consumer — a bar, not a friction.
//! `brush-parser` is MIT and maintained but measured at 37 new crates on the
//! default feature set, including a snapshot-testing library and two parser
//! generators in a *runtime* tree. `conch-parser` has not been published since
//! 2019, and an unmaintained dependency is a poor place to put a security
//! boundary. The deciding argument is none of those, though: a complete bash
//! parser accepts a superset of what this tool permits, so its accept-set becomes
//! a second surface that has to be audited against the permit-set for as long as
//! it is depended on. The set this module admits and the set it executes are the
//! same set because they are the same code.
//!
//! ## The allowlist is the mechanism
//!
//! The refusals are not a blocklist of known-bad constructs. The lexer permits an
//! enumerated set of characters per state and **refuses every byte outside it**,
//! so a construct nobody anticipated is refused rather than falling through into a
//! word. A blocklist fails open on the case its author did not think of, which is
//! precisely the case that matters; this fails closed on it.
//!
//! The named refusals below exist on top of that, and only to produce a better
//! message: `$(` reaching the catch-all would still be refused, but it would be
//! refused as "unsupported character" rather than as "command substitution", and
//! a model that cannot tell which construct offended cannot rewrite its line.
//!
//! One consequence worth stating because it is a real limitation rather than an
//! oversight: **glob metacharacters are refused outside quotes.** `ls *.rs` does
//! not run. Expanding a glob would make the argv depend on the state of the
//! filesystem at expansion time, so the argv the policy checked and the argv that
//! ran could differ by anything created in between — a time-of-check to
//! time-of-use gap opened inside the one code path whose entire job is checking.
//! Not expanding it but passing it through would hand the model a `*` that a real
//! shell would have expanded, which silently means something else. Refusing is the
//! only one of the three that cannot be wrong, and the refusal message points at
//! [`find`](super::workspace) and `list_dir`, which answer the same question
//! through a checked path.
//!
//! ## Nesting
//!
//! There is no depth cap here because there is no depth. Every construct that
//! nests — `$( )`, `` ` ` ``, `( )`, `{ }`, `<( )` — is refused, so the grammar is
//! flat by construction and a recursive-descent parser over it cannot recurse
//! deeply enough to matter. What is capped is the input's length and the number of
//! stages, so a pathological line is refused rather than allocated for.

use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command as TokioCommand;

use crate::error::{Error, Result};

/// The longest command line that will be parsed at all.
///
/// A model that emits more than this has not written a command, and refusing
/// early means the lexer never allocates against an adversarial input. Generous
/// against real use: a long `find` invocation with a dozen predicates is a few
/// hundred characters.
pub(crate) const MAX_LINE_CHARS: usize = 8192;

/// The most sub-commands one line may contain, across every stage and sequence.
///
/// Bounds the number of policy checks a single tool call can demand, and the
/// number of children one line can spawn. Thirty is far past the point where a
/// line is still something a human would review.
pub(crate) const MAX_COMMANDS: usize = 30;

/// Why a line was refused, in terms of the thing the model wrote.
///
/// `construct` is the name of the offending syntax, which is the part a model can
/// act on; `reason` says why this tool will not run it and, where there is one,
/// what to use instead. `at` is the character offset, so a caller can point at it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refusal {
    /// The construct's name, e.g. `command substitution`.
    pub(crate) construct: &'static str,
    /// Why it is refused, and the alternative where one exists.
    pub(crate) reason: String,
    /// Character offset into the original line.
    pub(crate) at: usize,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "refused at character {}: {} is not supported by the `shell` tool. {}",
            self.at, self.construct, self.reason
        )
    }
}

impl Refusal {
    fn new(construct: &'static str, at: usize, reason: impl Into<String>) -> Self {
        Self {
            construct,
            reason: reason.into(),
            at,
        }
    }
}

/// A parse either produces a line or refuses it. There is no third outcome, and in
/// particular no "parsed with warnings" — a line this module is unsure about is a
/// line it refuses.
pub(crate) type ParseResult<T> = std::result::Result<T, Refusal>;

/// Which stream a redirect moves, and where.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RedirectKind {
    /// `> file` — truncate.
    Stdout,
    /// `>> file` — append.
    StdoutAppend,
    /// `< file`.
    Stdin,
    /// `2> file` — truncate.
    Stderr,
    /// `2>> file` — append.
    StderrAppend,
    /// `2>&1`. Carries no path, so nothing is checked for it.
    StderrToStdout,
}

impl RedirectKind {
    /// Whether the target is opened for writing, which decides whether the path is
    /// checked against [`Act::Write`](crate::Act::Write) or
    /// [`Act::Read`](crate::Act::Read).
    pub(crate) fn is_write(self) -> bool {
        matches!(
            self,
            Self::Stdout | Self::StdoutAppend | Self::Stderr | Self::StderrAppend
        )
    }
}

/// One redirect on one sub-command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Redirect {
    /// Which stream, and whether it truncates or appends.
    pub(crate) kind: RedirectKind,
    /// The path, exactly as written after quote removal. `None` only for
    /// [`RedirectKind::StderrToStdout`], which names no file.
    pub(crate) target: Option<String>,
}

/// One sub-command: the argv that will be spawned, plus its redirects.
///
/// `argv[0]` is the program. The parser guarantees `argv` is non-empty — a
/// redirect with no command (`> out.txt`) is refused rather than represented,
/// because it has no program to check against
/// [`Act::Exec`](crate::Act::Exec) and a permission model with a hole in it is
/// worse than one that says no.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Command {
    /// Program and arguments, quotes removed, no expansion of any kind performed.
    pub(crate) argv: Vec<String>,
    /// Redirects in the order written.
    pub(crate) redirects: Vec<Redirect>,
}

/// Sub-commands joined by `|`, all spawned together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pipeline {
    /// At least one. Stage *n*'s stdout becomes stage *n+1*'s stdin.
    pub(crate) stages: Vec<Command>,
}

/// How a pipeline's exit status decides whether the next one runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AndOr {
    /// `&&` — run only if the previous pipeline succeeded.
    And,
    /// `||` — run only if the previous pipeline failed.
    Or,
}

/// Pipelines joined by `&&` and `||`, evaluated left to right.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AndOrList {
    /// Always runs.
    pub(crate) first: Pipeline,
    /// Each runs or is skipped depending on the operator and what came before.
    pub(crate) rest: Vec<(AndOr, Pipeline)>,
}

/// A whole parsed line: and-or lists separated by `;` or a newline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Line {
    /// Each runs in order regardless of the previous one's status.
    pub(crate) seq: Vec<AndOrList>,
}

impl Line {
    /// Every sub-command in the line, in written order.
    ///
    /// This is what the caller checks against the policy. It deliberately flattens
    /// away the control structure: a stage that `&&` may skip at runtime is still
    /// checked before anything runs, because a line is authorised as a whole. The
    /// alternative — checking each stage as control flow reaches it — would run the
    /// first half of a line whose second half was never permitted, which is exactly
    /// the partial execution the contract forbids.
    pub(crate) fn commands(&self) -> impl Iterator<Item = &Command> {
        self.seq.iter().flat_map(|ao| {
            ao.first
                .stages
                .iter()
                .chain(ao.rest.iter().flat_map(|(_, p)| p.stages.iter()))
        })
    }
}

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

/// What the lexer produces. Deliberately small: the grammar has no construct that
/// needs more, and every token that would need more is refused before it is built.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    /// A word after quote removal, with whether any part of it was quoted.
    ///
    /// The flag matters for exactly one rule: `2>` is a redirect on file
    /// descriptor 2, but `"2">` is the word `2` followed by a redirect, because
    /// POSIX only treats an *unquoted* digit string as a file-descriptor prefix.
    /// Dropping the distinction would let a quoted argument silently become a
    /// redirect.
    Word {
        text: String,
        quoted: bool,
        at: usize,
    },
    /// `|`
    Pipe(usize),
    /// `&&`
    AndIf(usize),
    /// `||`
    OrIf(usize),
    /// `;` or a newline, which POSIX makes equivalent for a simple list.
    Semi(usize),
    /// A redirect operator. The target word, if it needs one, is the next token.
    Redirect { kind: RedirectKind, at: usize },
}

impl Token {
    fn at(&self) -> usize {
        match self {
            Token::Word { at, .. }
            | Token::Pipe(at)
            | Token::AndIf(at)
            | Token::OrIf(at)
            | Token::Semi(at)
            | Token::Redirect { at, .. } => *at,
        }
    }
}

/// Characters that may appear unquoted inside a word.
///
/// This is the allowlist, and it is the security property of the module. Every
/// character here is inert: none of them is an operator, a quote, an expansion
/// sigil or a glob metacharacter in any shell, so a word built only from these
/// means the same thing to this parser as it would to `sh`. Anything not here and
/// not handled explicitly as syntax reaches the catch-all and is refused.
///
/// Non-ASCII is permitted wholesale, which is not a hole: no shell assigns
/// syntactic meaning to any scalar above U+007F, so such a character is a literal
/// wherever it appears. A filename or a `grep` pattern in any language therefore
/// works. Control characters below U+0020 are *not* permitted — a NUL or a stray
/// carriage return in an argv is a portability and injection hazard with no
/// legitimate use here.
fn is_bare_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(c, '_' | '-' | '.' | '/' | '=' | '+' | ':' | ',' | '@' | '%')
        || (c as u32) > 0x7f
}

/// What may appear inside a quoted string, on top of the bare-word set.
///
/// Quotes widen the permitted set — that is what they are for, and it is what
/// keeps the refusals from being a wall: a literal `$`, `*` or space is reachable
/// through them. What quotes do *not* widen is control characters, because a NUL
/// or a carriage return in an argv is a portability and injection hazard with no
/// legitimate use, and a quote around one does not make it one.
///
/// Tab and newline are the two exceptions, and they are exceptions because a
/// multi-line or tab-bearing string really is something a `grep` pattern or a
/// commit message needs. They are inert here for the same reason every other
/// character is: the word goes into an argv array, and nothing downstream ever
/// re-parses it.
fn is_permitted_in_quotes(c: char) -> bool {
    (c as u32) >= 0x20 || c == '\t' || c == '\n'
}

/// Words that a shell reads as syntax rather than as a program name.
///
/// Refused in command position, and only there — `find . -name do` is an ordinary
/// argument and stays one. Without this the compound commands fail in a way that
/// teaches nothing: `if [ -f x ]; then ls; fi` parses cleanly as five separate
/// commands named `if`, `then`, `fi` and so on, none of which exists, so the model
/// gets three "no such program" errors instead of one refusal telling it that
/// control flow is not available. Nothing unsafe runs either way; this is about
/// the difference between a boundary that explains itself and one that does not.
///
/// Quoting escapes the keyword-ness, exactly as it does in a shell: `'if'` in
/// command position names a program called `if`, and if one exists it is checked
/// against the policy like any other.
const RESERVED_WORDS: &[&str] = &[
    "if", "then", "elif", "else", "fi", "for", "while", "until", "do", "done", "case", "esac",
    "function", "select",
];

/// Turn a line into tokens, refusing anything outside the admitted grammar.
fn lex(src: &str) -> ParseResult<Vec<Token>> {
    if src.chars().count() > MAX_LINE_CHARS {
        return Err(Refusal::new(
            "an over-long command line",
            0,
            format!("this tool parses at most {MAX_LINE_CHARS} characters; split the work across several calls."),
        ));
    }

    let chars: Vec<char> = src.chars().collect();
    let mut out: Vec<Token> = Vec::new();
    let mut i = 0usize;
    // The word being accumulated, and where it started. `None` means "between
    // words", which is what makes `a b` two words and `a"b"` one.
    let mut word: Option<(String, bool, usize)> = None;

    // Close off the pending word, if any, and push it.
    macro_rules! flush {
        ($out:expr, $word:expr) => {
            if let Some((text, quoted, at)) = $word.take() {
                $out.push(Token::Word { text, quoted, at });
            }
        };
    }

    while i < chars.len() {
        let c = chars[i];
        match c {
            // -- whitespace ------------------------------------------------
            ' ' | '\t' => {
                flush!(out, word);
                i += 1;
            }
            '\n' => {
                flush!(out, word);
                out.push(Token::Semi(i));
                i += 1;
            }

            // -- quoting ---------------------------------------------------
            '\'' => {
                // Single quotes are the one context with no escapes at all: every
                // byte until the next `'` is literal, which is what makes them the
                // safe way to pass a string this parser would otherwise refuse.
                let start = i;
                i += 1;
                let (text, _, _) = word.get_or_insert((String::new(), false, start));
                loop {
                    let Some(&ch) = chars.get(i) else {
                        return Err(Refusal::new(
                            "an unterminated single quote",
                            start,
                            "the line ended inside a quoted string; close the quote.",
                        ));
                    };
                    i += 1;
                    if ch == '\'' {
                        break;
                    }
                    if !is_permitted_in_quotes(ch) {
                        return Err(Refusal::new(
                            "a control character",
                            i - 1,
                            "control characters are refused even inside quotes.",
                        ));
                    }
                    text.push(ch);
                }
                word.as_mut().expect("just inserted").1 = true;
            }
            '"' => {
                let start = i;
                i += 1;
                let (text, _, _) = word.get_or_insert((String::new(), false, start));
                loop {
                    let Some(&ch) = chars.get(i) else {
                        return Err(Refusal::new(
                            "an unterminated double quote",
                            start,
                            "the line ended inside a quoted string; close the quote.",
                        ));
                    };
                    match ch {
                        '"' => {
                            i += 1;
                            break;
                        }
                        // Inside double quotes these still expand in a real shell,
                        // so quoting is not a way to smuggle them past the parser.
                        '$' => {
                            return Err(Refusal::new(
                                "parameter or command expansion",
                                i,
                                "`$` expands inside double quotes too. Use single quotes for a literal `$`, and pass values as arguments rather than through the environment.",
                            ))
                        }
                        '`' => {
                            return Err(Refusal::new(
                                "command substitution",
                                i,
                                "backticks expand inside double quotes too. Run the inner command as its own step and use its output.",
                            ))
                        }
                        '\\' => {
                            // POSIX: inside double quotes a backslash escapes only
                            // these four. Before anything else it is a literal
                            // backslash, and copying that rule exactly is what
                            // keeps this parser's reading identical to a shell's.
                            match chars.get(i + 1) {
                                Some(&n @ ('"' | '\\' | '$' | '`')) => {
                                    text.push(n);
                                    i += 2;
                                }
                                Some(&'\n') => i += 2,
                                _ => {
                                    text.push('\\');
                                    i += 1;
                                }
                            }
                        }
                        _ => {
                            if !is_permitted_in_quotes(ch) {
                                return Err(Refusal::new(
                                    "a control character",
                                    i,
                                    "control characters are refused even inside quotes.",
                                ));
                            }
                            text.push(ch);
                            i += 1;
                        }
                    }
                }
                word.as_mut().expect("just inserted").1 = true;
            }
            '\\' => {
                // Outside quotes a backslash escapes anything, including a
                // character this parser would otherwise refuse. That is correct and
                // safe: the escaped character becomes a literal in the argv and
                // never reaches a shell, so `grep \$HOME` searches for the text
                // `$HOME` exactly as a shell would make it.
                match chars.get(i + 1) {
                    None => {
                        return Err(Refusal::new(
                            "a trailing backslash",
                            i,
                            "the line ended with an escape that has nothing to escape.",
                        ))
                    }
                    Some(&'\n') => i += 2,
                    Some(&n) => {
                        if (n as u32) < 0x20 && n != '\t' {
                            return Err(Refusal::new(
                                "a control character",
                                i + 1,
                                "control characters are refused even when escaped.",
                            ));
                        }
                        word.get_or_insert((String::new(), false, i)).0.push(n);
                        i += 2;
                    }
                }
            }

            // -- operators -------------------------------------------------
            '|' => {
                flush!(out, word);
                if chars.get(i + 1) == Some(&'|') {
                    out.push(Token::OrIf(i));
                    i += 2;
                } else {
                    out.push(Token::Pipe(i));
                    i += 1;
                }
            }
            '&' => {
                if chars.get(i + 1) == Some(&'&') {
                    flush!(out, word);
                    out.push(Token::AndIf(i));
                    i += 2;
                } else {
                    return Err(Refusal::new(
                        "background execution",
                        i,
                        "a single `&` detaches a process this tool would then not be able to bound, so the run could end leaving it alive. Run the command in the foreground instead; if it is long, raise the contract's exec timeout rather than backgrounding it.",
                    ));
                }
            }
            ';' => {
                flush!(out, word);
                if chars.get(i + 1) == Some(&';') {
                    return Err(Refusal::new(
                        "a `case` clause terminator",
                        i,
                        "`case` and the other compound commands are not supported; express the branch as separate calls.",
                    ));
                }
                out.push(Token::Semi(i));
                i += 1;
            }
            '>' | '<' => {
                // A digit string written immediately before the operator, and not
                // quoted, is a file-descriptor prefix rather than a word. This is
                // the one place the lexer has to look backwards, and getting it
                // wrong in either direction is a real bug: treating `echo 2>x` as
                // fd 2 would silently drop an argument, and treating `2>x` as a
                // word would redirect nothing.
                let fd: Option<u32> = match &word {
                    Some((text, false, _)) if !text.is_empty() && text.chars().all(|c| c.is_ascii_digit()) => {
                        text.parse().ok()
                    }
                    _ => None,
                };
                if fd.is_some() {
                    word = None;
                } else {
                    flush!(out, word);
                }
                let at = i;
                let kind = if c == '<' {
                    match chars.get(i + 1) {
                        Some(&'<') => {
                            return Err(Refusal::new(
                                "a here-document",
                                i,
                                "`<<` reads inline input this tool cannot bound. Write the text to a file with `write_file` and redirect from it.",
                            ))
                        }
                        Some(&'(') => {
                            return Err(Refusal::new(
                                "process substitution",
                                i,
                                "`<( )` starts a hidden sub-command that would never be checked. Run it as its own stage and redirect from a file.",
                            ))
                        }
                        Some(&'>') => {
                            return Err(Refusal::new(
                                "a read-write redirect",
                                i,
                                "`<>` opens a file for both reading and writing, which this tool cannot express as a single permission check.",
                            ))
                        }
                        Some(&'&') => {
                            return Err(Refusal::new(
                                "an input descriptor duplication",
                                i,
                                "only `2>&1` is supported among the descriptor duplications.",
                            ))
                        }
                        _ => {
                            if fd.is_some_and(|n| n != 0) {
                                return Err(Refusal::new(
                                    "a redirect of an unsupported file descriptor",
                                    at,
                                    "input may be redirected only for descriptor 0.",
                                ));
                            }
                            i += 1;
                            RedirectKind::Stdin
                        }
                    }
                } else {
                    match chars.get(i + 1) {
                        Some(&'>') => {
                            i += 2;
                            match fd {
                                None | Some(1) => RedirectKind::StdoutAppend,
                                Some(2) => RedirectKind::StderrAppend,
                                Some(_) => {
                                    return Err(Refusal::new(
                                        "a redirect of an unsupported file descriptor",
                                        at,
                                        "only descriptors 1 and 2 may be redirected.",
                                    ))
                                }
                            }
                        }
                        Some(&'|') => {
                            return Err(Refusal::new(
                                "a clobbering redirect",
                                i,
                                "`>|` overrides a shell option this tool does not have; use `>`.",
                            ))
                        }
                        Some(&'&') => {
                            // Only `2>&1`, spelled exactly. `&>` never reaches here
                            // because a leading `&` is refused as background above.
                            if fd != Some(2) || chars.get(i + 2) != Some(&'1') {
                                return Err(Refusal::new(
                                    "a descriptor duplication",
                                    at,
                                    "`2>&1` is the only descriptor duplication supported.",
                                ));
                            }
                            // Whatever follows must end the word, or this was
                            // something like `2>&12` that only looks like `2>&1`.
                            if chars.get(i + 3).is_some_and(|&n| {
                                is_bare_word_char(n) || n == '\'' || n == '"' || n == '\\'
                            }) {
                                return Err(Refusal::new(
                                    "a descriptor duplication",
                                    at,
                                    "`2>&1` is the only descriptor duplication supported.",
                                ));
                            }
                            i += 3;
                            RedirectKind::StderrToStdout
                        }
                        _ => {
                            i += 1;
                            match fd {
                                None | Some(1) => RedirectKind::Stdout,
                                Some(2) => RedirectKind::Stderr,
                                Some(_) => {
                                    return Err(Refusal::new(
                                        "a redirect of an unsupported file descriptor",
                                        at,
                                        "only descriptors 1 and 2 may be redirected.",
                                    ))
                                }
                            }
                        }
                    }
                };
                out.push(Token::Redirect { kind, at });
            }

            // -- named refusals --------------------------------------------
            //
            // Each of these would be refused by the catch-all below anyway. They
            // are enumerated only so the message names the construct, because a
            // model told "unsupported character" cannot rewrite its line and a
            // model told "command substitution" can.
            '$' => {
                let what = if chars.get(i + 1) == Some(&'(') {
                    if chars.get(i + 2) == Some(&'(') {
                        ("arithmetic expansion", "`$(( ))` is not evaluated by this tool.")
                    } else {
                        ("command substitution", "`$( )` would run a command this tool never sees and therefore never checks. Run it as its own step and use the output.")
                    }
                } else {
                    ("parameter expansion", "this tool does not expand variables: the argv it checks must be the argv it runs, and a value read from the environment is neither. Pass the value literally.")
                };
                return Err(Refusal::new(what.0, i, what.1));
            }
            '`' => {
                return Err(Refusal::new(
                    "command substitution",
                    i,
                    "backticks would run a command this tool never sees and therefore never checks. Run it as its own step and use the output.",
                ))
            }
            '(' | ')' => {
                return Err(Refusal::new(
                    "a subshell",
                    i,
                    "`( )` groups commands in a child shell this tool does not start. Write the commands as a sequence with `;` or `&&`.",
                ))
            }
            '{' | '}' => {
                return Err(Refusal::new(
                    "a brace group or brace expansion",
                    i,
                    "`{ }` is either a command group or an expansion, and this tool performs neither. Quote it if you meant the literal character.",
                ))
            }
            '*' | '?' | '[' | ']' => {
                return Err(Refusal::new(
                    "a glob pattern",
                    i,
                    "expanding a glob would let the argv that runs differ from the argv that was checked, since the filesystem can change in between. Use `find` or `list_dir` to select paths, then name them; or quote the character if you meant it literally.",
                ))
            }
            '~' => {
                return Err(Refusal::new(
                    "tilde expansion",
                    i,
                    "this tool does not expand `~`. Write the path out, or quote the character if you meant it literally.",
                ))
            }
            '#' => {
                return Err(Refusal::new(
                    "a comment",
                    i,
                    "`#` begins a comment, which has no meaning in a single command line. Quote it if you meant the literal character.",
                ))
            }
            '!' => {
                return Err(Refusal::new(
                    "pipeline negation or history expansion",
                    i,
                    "`!` is not supported. Check the exit status through `&&` and `||` instead.",
                ))
            }

            // -- words and the catch-all -----------------------------------
            _ if (c as u32) < 0x20 => {
                // Would reach the catch-all below and be refused there anyway;
                // named separately only so the message says which hazard it is.
                return Err(Refusal::new(
                    "a control character",
                    i,
                    "control characters are not permitted in a command line. Quote a tab or a newline if a literal one is genuinely needed.",
                ));
            }
            _ if is_bare_word_char(c) => {
                word.get_or_insert((String::new(), false, i)).0.push(c);
                i += 1;
            }
            _ => {
                // The property the whole module rests on: an unrecognised character
                // is refused, never absorbed into a word.
                return Err(Refusal::new(
                    "an unsupported character",
                    i,
                    format!(
                        "`{}` (U+{:04X}) is not permitted outside quotes. Quote it if you meant it literally.",
                        c.escape_debug(),
                        c as u32
                    ),
                ));
            }
        }
    }

    flush!(out, word);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Parse one command line into the sub-commands it would run.
///
/// Every error is a [`Refusal`] naming a construct. Nothing is executed, nothing
/// is resolved against the filesystem, and no expansion of any kind is performed:
/// the argv this returns is the argv that will be spawned, byte for byte, which is
/// what makes checking it meaningful.
pub(crate) fn parse(src: &str) -> ParseResult<Line> {
    let tokens = lex(src)?;
    let mut p = Parser {
        tokens,
        pos: 0,
        commands: 0,
    };
    let line = p.line()?;
    if line.seq.is_empty() {
        return Err(Refusal::new(
            "an empty command line",
            0,
            "there is nothing here to run.",
        ));
    }
    Ok(line)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    commands: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn line(&mut self) -> ParseResult<Line> {
        let mut seq = Vec::new();
        loop {
            while matches!(self.peek(), Some(Token::Semi(_))) {
                self.pos += 1;
            }
            if self.peek().is_none() {
                break;
            }
            seq.push(self.and_or()?);
        }
        Ok(Line { seq })
    }

    fn and_or(&mut self) -> ParseResult<AndOrList> {
        let first = self.pipeline()?;
        let mut rest = Vec::new();
        loop {
            let op = match self.peek() {
                Some(Token::AndIf(_)) => AndOr::And,
                Some(Token::OrIf(_)) => AndOr::Or,
                _ => break,
            };
            let at = self.peek().expect("matched above").at();
            self.pos += 1;
            if matches!(self.peek(), None | Some(Token::Semi(_))) {
                return Err(Refusal::new(
                    "a dangling operator",
                    at,
                    "`&&` and `||` must be followed by a command.",
                ));
            }
            rest.push((op, self.pipeline()?));
        }
        Ok(AndOrList { first, rest })
    }

    fn pipeline(&mut self) -> ParseResult<Pipeline> {
        let mut stages = vec![self.command()?];
        while let Some(Token::Pipe(at)) = self.peek() {
            let at = *at;
            self.pos += 1;
            if matches!(self.peek(), None | Some(Token::Semi(_))) {
                return Err(Refusal::new(
                    "a dangling pipe",
                    at,
                    "`|` must be followed by a command.",
                ));
            }
            stages.push(self.command()?);
        }
        Ok(Pipeline { stages })
    }

    fn command(&mut self) -> ParseResult<Command> {
        self.commands += 1;
        if self.commands > MAX_COMMANDS {
            let at = self.peek().map_or(0, Token::at);
            return Err(Refusal::new(
                "an over-long command line",
                at,
                format!("this tool runs at most {MAX_COMMANDS} sub-commands in one line; split the work across several calls."),
            ));
        }

        let start = self.peek().map_or(0, Token::at);
        let mut argv: Vec<String> = Vec::new();
        let mut redirects: Vec<Redirect> = Vec::new();
        // Whether the word in command position arrived quoted, which is what
        // decides between a reserved word and a program that happens to share its
        // spelling. Only the first word can be in command position, so this is
        // settled on the first iteration and never revisited.
        let mut program_quoted = false;

        loop {
            match self.peek() {
                Some(Token::Word { .. }) => {
                    let Some(Token::Word { text, quoted, .. }) = self.tokens.get(self.pos) else {
                        unreachable!("matched Word above")
                    };
                    if argv.is_empty() {
                        program_quoted = *quoted;
                    }
                    argv.push(text.clone());
                    self.pos += 1;
                }
                Some(&Token::Redirect { kind, at }) => {
                    self.pos += 1;
                    let target = if kind == RedirectKind::StderrToStdout {
                        None
                    } else {
                        match self.tokens.get(self.pos) {
                            Some(Token::Word { text, .. }) => {
                                let t = text.clone();
                                self.pos += 1;
                                Some(t)
                            }
                            _ => {
                                return Err(Refusal::new(
                                    "a redirect with no target",
                                    at,
                                    "a redirect must name the file it reads from or writes to.",
                                ))
                            }
                        }
                    };
                    redirects.push(Redirect { kind, target });
                }
                _ => break,
            }
        }

        if argv.is_empty() {
            return Err(Refusal::new(
                "a redirect with no command",
                start,
                "there is no program here to run, so there is nothing to check against the execute policy. Name the command that produces or consumes the file.",
            ));
        }
        if !program_quoted && RESERVED_WORDS.contains(&argv[0].as_str()) {
            return Err(Refusal::new(
                "a shell keyword",
                start,
                format!(
                    "`{}` introduces a compound command, and this tool has no control flow: it runs a sequence of checked commands and nothing else. Express the branch with `&&` and `||`, or as separate steps that read each other's results.",
                    argv[0]
                ),
            ));
        }
        Ok(Command { argv, redirects })
    }
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/// The one builtin. See [`plan`] for why it is the only one.
pub(crate) const CD: &str = "cd";

/// One sub-command with every path it touches already resolved.
///
/// This type is the reason the tool's central claim holds. The policy is checked
/// against these paths and the runner opens *these* paths — not the text in the
/// [`Line`], re-resolved later against wherever the run happened to get to. A
/// redirect written as `> out.txt` after a `cd src` means `src/out.txt`, and if
/// the check resolved it one way and the open resolved it the other, the boundary
/// would be applied to a file that was never written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Planned {
    /// Where this sub-command runs. Absolute.
    pub(crate) cwd: PathBuf,
    /// Its redirects, targets absolute. `None` for `2>&1`, which names no file.
    pub(crate) redirects: Vec<(RedirectKind, Option<PathBuf>)>,
    /// Where a `cd` moves to, absolute, or `None` for every other command.
    ///
    /// Recorded here so the caller can check the destination against
    /// [`Act::Read`](crate::Act::Read) without resolving the path a second time.
    /// A second resolution is a second chance to disagree, and the disagreement
    /// would be between the directory that was approved and the directory the
    /// commands after it actually ran in.
    pub(crate) cd_target: Option<PathBuf>,
}

/// Resolve every path in `line` against `root`, in written order.
///
/// The result is parallel to [`Line::commands`] and is indexed by the same walk,
/// so the caller checks entry *n* and the runner runs entry *n*.
///
/// ## `cd` is applied unconditionally, and that is deliberate
///
/// `&&` and `||` mean a `cd` may or may not execute, so where a later redirect
/// points is not knowable before the line runs. Three answers were possible and
/// only one of them is safe. Resolving at run time makes the checked path and the
/// opened path different objects, which is the whole hazard. Refusing any line
/// that mixes `cd` with a redirect would refuse most real uses of both. So the
/// plan assumes every `cd` happens, resolves against that, and the runner uses
/// the resolved path whether or not the `cd` ran.
///
/// The cost is one stated difference from a real shell: in
/// `cd nope && ls > out.txt`, a shell that failed the `cd` would write `out.txt`
/// in the original directory, and this writes it in `nope/`. Both were checked,
/// neither escapes the root, and the model is told where the file went. That is a
/// smaller surprise than a path the policy approved and the process did not use.
///
/// `cd` is the only builtin for a related reason: every other word in command
/// position is a program looked up on `PATH`, which is what keeps "checked
/// against [`Act::Exec`](crate::Act::Exec)" a complete statement about what may
/// run. Adding `echo` or `test` would each be one more word that no longer means
/// an external program was authorised.
pub(crate) fn plan(line: &Line, root: &Path) -> Result<Vec<Planned>> {
    let mut cwd = root.to_path_buf();
    let mut out = Vec::new();
    for cmd in line.commands() {
        let here = cwd.clone();
        let mut redirects = Vec::with_capacity(cmd.redirects.len());
        for r in &cmd.redirects {
            let target = match r.target.as_deref() {
                None => None,
                Some(t) => Some(resolve(&here, t, root)?),
            };
            redirects.push((r.kind, target));
        }
        let cd_target = if cmd.argv[0] == CD {
            Some(match cmd.argv.get(1) {
                None => root.to_path_buf(),
                Some(t) => resolve(&here, t, root)?,
            })
        } else {
            None
        };
        out.push(Planned {
            cwd: here.clone(),
            redirects,
            cd_target: cd_target.clone(),
        });
        // A `cd` moves the directory for everything after it, including the
        // stages of a later pipeline. Applied after this command is planned, so
        // `cd x > log` — which is odd but legal — resolves `log` where it was
        // written rather than where it is going.
        if let Some(t) = cd_target {
            cwd = t;
        }
    }
    Ok(out)
}

/// What one `shell` invocation produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ShellOutcome {
    /// The line ran to completion, or stopped early because `&&` or `||` said so.
    Ran {
        /// The status of the last pipeline that actually ran.
        code: Option<i32>,
        /// Everything the line wrote to stdout that was not redirected away.
        stdout: String,
        /// Everything it wrote to stderr that was not redirected away.
        stderr: String,
        /// How many characters the output bounds elided, or 0.
        elided: usize,
        /// How many sub-commands ran, which is not the number parsed: `&&` and
        /// `||` skip. Reported so a reader of the trace can tell a line that
        /// short-circuited from one that ran everything.
        ran: usize,
    },
    /// The line outlived the wall-clock timeout and was killed.
    TimedOut {
        /// The ceiling it crossed.
        after: Duration,
    },
    /// A program in the line does not exist on this machine. Not a failed run:
    /// the model is told and carries on, exactly as `exec` does.
    Unavailable {
        /// What was looked for.
        reason: String,
    },
}

/// Runs a parsed [`Line`] against its [`plan`], wiring the pipes itself.
///
/// Holds no [`Policy`](crate::Policy) for the same reason [`Exec`](super::exec)
/// holds none: every sub-command and every path in the plan has already been
/// through `dispatch`'s gate by the time this is reached, and a runner that
/// re-checked would be a second boundary that could disagree with the first.
/// What this type guarantees instead is that it runs *exactly* the [`Line`] and
/// *exactly* the [`Planned`] paths it was handed — no re-parse, no re-resolve, no
/// shell, no expansion.
pub(crate) struct Shell {
    timeout: Duration,
    cap: usize,
}

impl Shell {
    /// Kill the whole line after `timeout`, bounding each captured stream at
    /// `cap` chars.
    pub(crate) fn new(timeout: Duration, cap: usize) -> Self {
        Self { timeout, cap }
    }

    /// Execute an authorised line.
    ///
    /// The whole line shares one wall-clock ceiling rather than one per stage,
    /// because a line is what the model asked for and a per-stage timeout would
    /// let a ten-stage line run ten times longer than the contract's ceiling
    /// implies.
    pub(crate) async fn run(&self, line: &Line, plan: &[Planned]) -> Result<ShellOutcome> {
        match tokio::time::timeout(self.timeout, self.run_line(line, plan)).await {
            Err(_elapsed) => Ok(ShellOutcome::TimedOut {
                after: self.timeout,
            }),
            Ok(r) => r,
        }
    }

    async fn run_line(&self, line: &Line, plan: &[Planned]) -> Result<ShellOutcome> {
        let mut out = String::new();
        let mut err = String::new();
        let mut code = Some(0);
        let mut ran = 0usize;
        // Walks in step with `Line::commands`, which is the order `plan` was
        // built in. Advanced even for a skipped pipeline, so a `&&` that does not
        // fire cannot slide every later stage onto the wrong plan entry.
        let mut at = 0usize;

        for list in &line.seq {
            let mut status = match self
                .run_pipeline(&list.first, plan, &mut at, &mut out, &mut err)
                .await?
            {
                Stage::Ran(c) => {
                    ran += list.first.stages.len();
                    c
                }
                Stage::Unavailable(reason) => return Ok(ShellOutcome::Unavailable { reason }),
            };
            for (op, pipeline) in &list.rest {
                if (*op == AndOr::And) != (status == Some(0)) {
                    at += pipeline.stages.len();
                    continue;
                }
                status = match self
                    .run_pipeline(pipeline, plan, &mut at, &mut out, &mut err)
                    .await?
                {
                    Stage::Ran(c) => {
                        ran += pipeline.stages.len();
                        c
                    }
                    Stage::Unavailable(reason) => return Ok(ShellOutcome::Unavailable { reason }),
                };
            }
            code = status;
        }

        let (stdout, cut_o) = super::exec::head_and_tail(&out, self.cap);
        let (stderr, cut_e) = super::exec::head_and_tail(&err, self.cap);
        Ok(ShellOutcome::Ran {
            code,
            stdout,
            stderr,
            elided: cut_o + cut_e,
            ran,
        })
    }

    async fn run_pipeline(
        &self,
        pipeline: &Pipeline,
        plan: &[Planned],
        at: &mut usize,
        out: &mut String,
        err: &mut String,
    ) -> Result<Stage> {
        let base = *at;
        *at += pipeline.stages.len();

        // `cd` only means anything as a whole pipeline. `cd x | y` is a directory
        // change in a subshell that a real shell throws away, so representing it
        // would mean implementing a semantic whose only correct behaviour is to
        // do nothing visible. The move itself already happened in `plan`; all
        // that is left is to report a target that turned out not to be there.
        if pipeline.stages.len() == 1 && pipeline.stages[0].argv[0] == CD {
            let next = plan.get(base + 1).map(|p| p.cwd.clone());
            return Ok(match next {
                Some(d) if !d.is_dir() => {
                    err.push_str(&format!("[shell] cd: no such directory: {}\n", d.display()));
                    Stage::Ran(Some(1))
                }
                _ => Stage::Ran(Some(0)),
            });
        }

        let mut children = Vec::with_capacity(pipeline.stages.len());
        let last = pipeline.stages.len() - 1;
        let mut prev_stdout: Option<Stdio> = None;

        for (i, stage) in pipeline.stages.iter().enumerate() {
            let planned = &plan[base + i];
            if stage.argv[0] == CD {
                err.push_str(
                    "[shell] `cd` inside a pipeline changes nothing and was skipped; run it as \
                     its own stage.\n",
                );
                continue;
            }
            let mut cmd = TokioCommand::new(&stage.argv[0]);
            cmd.args(&stage.argv[1..])
                .current_dir(&planned.cwd)
                .kill_on_drop(true);

            match prev_stdout.take() {
                Some(s) => cmd.stdin(s),
                None => cmd.stdin(Stdio::null()),
            };
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

            apply_redirects(&mut cmd, planned, i != last)?;

            let mut child = match cmd.spawn() {
                Ok(c) => c,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(Stage::Unavailable(format!(
                        "no `{}` on PATH",
                        stage.argv[0]
                    )))
                }
                Err(e) => return Err(Error::Io(e)),
            };
            if i != last {
                if let Some(so) = child.stdout.take() {
                    // `TryInto`, not `TryFrom`: tokio implements the conversion in
                    // that direction only, so `Stdio::try_from(..)` does not
                    // resolve and the blanket impl does not bridge it.
                    let piped: Stdio = so.try_into().map_err(Error::Io)?;
                    prev_stdout = Some(piped);
                }
            }
            children.push(child);
        }

        // Wait in order. The last child's status is the pipeline's, which is what
        // POSIX says without `pipefail` and what `&&` will read.
        let mut code = Some(0);
        let merge_stderr = pipeline.stages.last().is_some_and(|s| {
            s.redirects
                .iter()
                .any(|r| r.kind == RedirectKind::StderrToStdout)
        });
        let n = children.len();
        for (i, child) in children.into_iter().enumerate() {
            let output = child.wait_with_output().await.map_err(Error::Io)?;
            let so = String::from_utf8_lossy(&output.stdout);
            let se = String::from_utf8_lossy(&output.stderr);
            if i + 1 == n {
                code = output.status.code();
                out.push_str(&so);
                if merge_stderr {
                    out.push_str(&se);
                } else {
                    err.push_str(&se);
                }
            } else {
                // An intermediate stage's stdout went down the pipe; what it wrote
                // to stderr is all that is left to report, and it is reported
                // rather than dropped because a failing middle stage is otherwise
                // invisible.
                err.push_str(&se);
            }
        }
        Ok(Stage::Ran(code))
    }
}

enum Stage {
    Ran(Option<i32>),
    Unavailable(String),
}

/// Open the files a stage redirects, at the paths [`plan`] already resolved.
///
/// Every one of these paths was checked against
/// [`Act::Write`](crate::Act::Write) or [`Act::Read`](crate::Act::Read) before
/// this ran. Opening happens here rather than in the caller because a file has to
/// be opened at spawn time to be a redirect at all.
fn apply_redirects(cmd: &mut TokioCommand, planned: &Planned, piped_out: bool) -> Result<()> {
    for (kind, target) in &planned.redirects {
        let Some(path) = target else {
            // `2>&1`, which names no file. Merging the two streams into a *pipe*
            // needs a descriptor duplication this crate does not perform, so it is
            // refused on a stage whose stdout is piped rather than silently
            // dropped. On a final stage the merge happens in the captured output,
            // which is where the model reads it from anyway.
            if piped_out {
                return Err(Error::Config(
                    "`2>&1` on a stage whose stdout is piped is not supported by this tool: \
                     merging the two streams into a pipe needs a descriptor duplication this \
                     crate does not perform. Put the redirect on the last stage of the pipeline."
                        .into(),
                ));
            }
            continue;
        };
        match kind {
            RedirectKind::Stdin => {
                cmd.stdin(Stdio::from(std::fs::File::open(path).map_err(Error::Io)?));
            }
            RedirectKind::Stdout | RedirectKind::StdoutAppend => {
                cmd.stdout(Stdio::from(open_for_write(
                    path,
                    *kind == RedirectKind::StdoutAppend,
                )?));
            }
            RedirectKind::Stderr | RedirectKind::StderrAppend => {
                cmd.stderr(Stdio::from(open_for_write(
                    path,
                    *kind == RedirectKind::StderrAppend,
                )?));
            }
            RedirectKind::StderrToStdout => unreachable!("handled above: it has no target"),
        }
    }
    Ok(())
}

fn open_for_write(path: &Path, append: bool) -> Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .append(append)
        .truncate(!append)
        .open(path)
        .map_err(Error::Io)
}

/// Join `rel` onto `base` and refuse anything that leaves `root`.
///
/// Lexical rather than `canonicalize`, and deliberately: `canonicalize` requires
/// the path to exist, and a redirect target usually does not — it is about to be
/// created. So `..` is resolved by walking components, and a walk that would rise
/// above the root is refused rather than clamped. Clamping would silently
/// redirect `> ../../etc/passwd` into the workspace, which is a surprise in the
/// one direction that matters.
///
/// This is a second line rather than the first: `dispatch` checks each of these
/// paths against the policy through `Workspace::check_path`. It is here because
/// this module is what actually opens the file, and a boundary enforced only by
/// its caller is one refactor away from being gone.
fn resolve(base: &Path, rel: &str, root: &Path) -> Result<PathBuf> {
    let joined = if Path::new(rel).is_absolute() {
        PathBuf::from(rel)
    } else {
        base.join(rel)
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                if !out.pop() {
                    return Err(Error::Config(format!("`{rel}` leaves the workspace root")));
                }
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    if !out.starts_with(root) {
        return Err(Error::Config(format!("`{rel}` leaves the workspace root")));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(src: &str) -> Line {
        parse(src).unwrap_or_else(|e| panic!("expected `{src}` to parse, got: {e}"))
    }

    fn refused(src: &str) -> Refusal {
        parse(src).expect_err(&format!("expected `{src}` to be refused, but it parsed"))
    }

    fn argvs(line: &Line) -> Vec<Vec<String>> {
        line.commands().map(|c| c.argv.clone()).collect()
    }

    #[test]
    fn a_bare_command_is_one_stage() {
        assert_eq!(argvs(&ok("ls")), vec![vec!["ls".to_string()]]);
    }

    #[test]
    fn words_split_on_whitespace_and_join_across_quotes() {
        assert_eq!(
            argvs(&ok("grep -n 'two words' file.txt")),
            vec![vec!["grep", "-n", "two words", "file.txt"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()]
        );
        // Adjacent quoted and bare segments are one word, as in any shell.
        assert_eq!(
            argvs(&ok("echo a'b'c")),
            vec![vec!["echo".to_string(), "abc".to_string()]]
        );
    }

    #[test]
    fn the_motivating_line_parses_into_its_three_stages() {
        let line = ok("cd infra && kubectl get pods | grep CrashLoop");
        assert_eq!(
            argvs(&line),
            vec![
                vec!["cd".to_string(), "infra".to_string()],
                vec!["kubectl".to_string(), "get".to_string(), "pods".to_string()],
                vec!["grep".to_string(), "CrashLoop".to_string()],
            ]
        );
        // One and-or list holding two pipelines, the second of two stages.
        assert_eq!(line.seq.len(), 1);
        assert_eq!(line.seq[0].rest.len(), 1);
        assert_eq!(line.seq[0].rest[0].0, AndOr::And);
        assert_eq!(line.seq[0].rest[0].1.stages.len(), 2);
    }

    #[test]
    fn sequences_and_newlines_are_the_same_separator() {
        assert_eq!(argvs(&ok("a; b")), argvs(&ok("a\nb")));
        assert_eq!(ok("a; b").seq.len(), 2);
        // A trailing separator is not an error, as in any shell.
        assert_eq!(ok("a; ").seq.len(), 1);
        assert_eq!(ok("a; ; b").seq.len(), 2);
        // But `;;` is a `case` terminator, and a syntax error outside one in a
        // real shell too — so it is refused rather than read as two separators.
        assert_eq!(refused("a;; b").construct, "a `case` clause terminator");
    }

    #[test]
    fn every_supported_redirect_is_recognised() {
        let cases: &[(&str, RedirectKind, Option<&str>)] = &[
            ("c > o", RedirectKind::Stdout, Some("o")),
            ("c 1> o", RedirectKind::Stdout, Some("o")),
            ("c >> o", RedirectKind::StdoutAppend, Some("o")),
            ("c < i", RedirectKind::Stdin, Some("i")),
            ("c 0< i", RedirectKind::Stdin, Some("i")),
            ("c 2> e", RedirectKind::Stderr, Some("e")),
            ("c 2>> e", RedirectKind::StderrAppend, Some("e")),
            ("c 2>&1", RedirectKind::StderrToStdout, None),
        ];
        for (src, kind, target) in cases {
            let line = ok(src);
            let cmd = line.commands().next().expect("one command");
            assert_eq!(cmd.argv, vec!["c".to_string()], "argv for `{src}`");
            assert_eq!(cmd.redirects.len(), 1, "redirect count for `{src}`");
            assert_eq!(cmd.redirects[0].kind, *kind, "kind for `{src}`");
            assert_eq!(
                cmd.redirects[0].target.as_deref(),
                *target,
                "target for `{src}`"
            );
        }
    }

    #[test]
    fn write_redirects_are_distinguished_from_read_ones() {
        assert!(RedirectKind::Stdout.is_write());
        assert!(RedirectKind::StdoutAppend.is_write());
        assert!(RedirectKind::Stderr.is_write());
        assert!(RedirectKind::StderrAppend.is_write());
        assert!(!RedirectKind::Stdin.is_write());
        // Names no file, so there is nothing to check and nothing to claim.
        assert!(!RedirectKind::StderrToStdout.is_write());
    }

    #[test]
    fn a_quoted_digit_is_an_argument_not_a_descriptor() {
        // The rule that keeps `echo "2" > out` from redirecting stderr.
        let line = ok("echo \"2\"> out");
        let cmd = line.commands().next().expect("one command");
        assert_eq!(cmd.argv, vec!["echo".to_string(), "2".to_string()]);
        assert_eq!(cmd.redirects[0].kind, RedirectKind::Stdout);
    }

    #[test]
    fn an_unquoted_digit_immediately_before_an_operator_is_a_descriptor() {
        let line = ok("cmd 2> err");
        let cmd = line.commands().next().expect("one command");
        assert_eq!(cmd.argv, vec!["cmd".to_string()]);
        assert_eq!(cmd.redirects[0].kind, RedirectKind::Stderr);
        // But separated by a space it is an ordinary argument.
        let line = ok("cmd 2 > err");
        let cmd = line.commands().next().expect("one command");
        assert_eq!(cmd.argv, vec!["cmd".to_string(), "2".to_string()]);
        assert_eq!(cmd.redirects[0].kind, RedirectKind::Stdout);
    }

    /// The enumerated half of the refusal set. Every construct the contract
    /// excludes appears here with the name the model is told, and each is asserted
    /// to be refused rather than merely to fail somehow.
    #[test]
    fn every_excluded_construct_is_refused_by_name() {
        let cases: &[(&str, &str)] = &[
            ("echo $(date)", "command substitution"),
            ("echo `date`", "command substitution"),
            ("echo \"`date`\"", "command substitution"),
            ("echo $HOME", "parameter expansion"),
            ("echo ${HOME}", "parameter expansion"),
            ("echo \"$HOME\"", "parameter or command expansion"),
            ("echo $((1+1))", "arithmetic expansion"),
            ("diff <(a) <(b)", "process substitution"),
            ("(cd x && ls)", "a subshell"),
            ("{ ls; }", "a brace group or brace expansion"),
            ("cat <<EOF", "a here-document"),
            ("sleep 10 &", "background execution"),
            ("ls *.rs", "a glob pattern"),
            ("ls file?.txt", "a glob pattern"),
            ("ls [ab].txt", "a glob pattern"),
            ("cd ~/src", "tilde expansion"),
            ("ls # comment", "a comment"),
            ("! ls", "pipeline negation or history expansion"),
            ("x;; y", "a `case` clause terminator"),
            ("if test -f x", "a shell keyword"),
            ("for f in a b", "a shell keyword"),
            ("while true", "a shell keyword"),
            ("case x in", "a shell keyword"),
            ("ls && then ls", "a shell keyword"),
            ("function f", "a shell keyword"),
            ("cmd >| out", "a clobbering redirect"),
            ("cmd <> f", "a read-write redirect"),
            ("cmd 3> f", "a redirect of an unsupported file descriptor"),
            ("cmd 1>&2", "a descriptor duplication"),
            ("cmd 2>&12", "a descriptor duplication"),
            ("cmd <&0", "an input descriptor duplication"),
            ("echo 'unterminated", "an unterminated single quote"),
            ("echo \"unterminated", "an unterminated double quote"),
            ("echo trailing\\", "a trailing backslash"),
            ("> out", "a redirect with no command"),
            ("ls |", "a dangling pipe"),
            ("ls &&", "a dangling operator"),
            ("ls > ", "a redirect with no target"),
            ("", "an empty command line"),
        ];
        for (src, construct) in cases {
            let r = refused(src);
            assert_eq!(&r.construct, construct, "wrong construct for `{src}`: {r}");
        }
    }

    #[test]
    fn a_refused_line_names_a_character_offset_inside_it() {
        let r = refused("ls && echo $HOME");
        assert_eq!(r.construct, "parameter expansion");
        assert_eq!(r.at, 11, "offset should point at the `$`");
    }

    #[test]
    fn quoting_makes_a_refused_character_an_ordinary_argument() {
        // The escape hatch that keeps the refusals from being a wall: a literal
        // `$` or `*` is reachable, it just cannot be an expansion.
        assert_eq!(
            argvs(&ok("grep '$HOME' f")),
            vec![vec![
                "grep".to_string(),
                "$HOME".to_string(),
                "f".to_string()
            ]]
        );
        assert_eq!(
            argvs(&ok("grep \\$HOME f")),
            vec![vec![
                "grep".to_string(),
                "$HOME".to_string(),
                "f".to_string()
            ]]
        );
        assert_eq!(
            argvs(&ok("grep '*.rs' f")),
            vec![vec![
                "grep".to_string(),
                "*.rs".to_string(),
                "f".to_string()
            ]]
        );
    }

    #[test]
    fn double_quotes_follow_the_posix_escape_rule_exactly() {
        // Only `" \ $ ` are escapable inside double quotes; before anything else a
        // backslash is a literal backslash. Copying the rule is what keeps this
        // parser's reading of a word identical to a shell's.
        assert_eq!(
            argvs(&ok("echo \"a\\\"b\"")),
            vec![vec!["echo".to_string(), "a\"b".to_string()]]
        );
        assert_eq!(
            argvs(&ok("echo \"a\\\\b\"")),
            vec![vec!["echo".to_string(), "a\\b".to_string()]]
        );
        assert_eq!(
            argvs(&ok("echo \"a\\nb\"")),
            vec![vec!["echo".to_string(), "a\\nb".to_string()]]
        );
    }

    #[test]
    fn non_ascii_is_a_literal_and_control_characters_are_not() {
        assert_eq!(
            argvs(&ok("grep café ./notes")),
            vec![vec![
                "grep".to_string(),
                "café".to_string(),
                "./notes".to_string()
            ]]
        );
        for src in [
            "echo a\u{0}b",
            "echo 'a\u{0}b'",
            "echo \"a\u{0}b\"",
            "echo a\rb",
        ] {
            assert_eq!(refused(src).construct, "a control character", "for {src:?}");
        }
    }

    #[test]
    fn the_length_and_stage_caps_refuse_rather_than_allocate() {
        let long = "a".repeat(MAX_LINE_CHARS + 1);
        assert_eq!(refused(&long).construct, "an over-long command line");

        let many = vec!["true"; MAX_COMMANDS + 1].join(" | ");
        assert_eq!(refused(&many).construct, "an over-long command line");

        // And the line just inside each cap still parses, so the cap is a bound
        // rather than an off-by-one that refuses legitimate work.
        assert_eq!(ok(&"a".repeat(MAX_LINE_CHARS)).seq.len(), 1);
        let many = vec!["true"; MAX_COMMANDS].join(" | ");
        assert_eq!(ok(&many).commands().count(), MAX_COMMANDS);
    }

    /// The invariant the module's security rests on, asserted directly rather than
    /// through the enumerated cases: **any input containing a character outside the
    /// permitted set, outside quotes, is refused.**
    ///
    /// The generator is a deterministic LCG rather than a property-testing crate,
    /// because this release adds no dependency to the macOS and Linux trees and a
    /// seeded loop is what that leaves. Deterministic is also the better trade
    /// here: a failure is reproducible from the seed alone.
    #[test]
    fn no_input_containing_an_unpermitted_character_is_ever_accepted() {
        // Every ASCII character the lexer treats as syntax or refuses by name.
        // Anything here, unquoted and unescaped, must not end up inside an argv.
        const ALPHABET: &[char] = &[
            'a', 'b', '1', '2', ' ', '\t', '\n', '|', '&', ';', '<', '>', '(', ')', '{', '}', '$',
            '`', '*', '?', '[', ']', '~', '#', '!', '\'', '"', '\\', '=', '-', '.', '/', '\u{0}',
            '\u{7}', '\u{1b}', 'é',
        ];

        let mut seed: u64 = 0x5eed_1234_dead_beef;
        let mut next = move || {
            // Numerical Recipes LCG. Any full-period generator does; this one is
            // four lines and needs no crate.
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };

        let mut accepted = 0usize;
        for _ in 0..20_000 {
            let len = 1 + next() % 12;
            let src: String = (0..len)
                .map(|_| ALPHABET[next() % ALPHABET.len()])
                .collect();

            let Ok(line) = parse(&src) else { continue };
            accepted += 1;

            // Whatever was accepted must be built only from permitted characters,
            // and must contain no control character anywhere.
            for cmd in line.commands() {
                for word in cmd
                    .argv
                    .iter()
                    .chain(cmd.redirects.iter().filter_map(|r| r.target.as_ref()))
                {
                    for c in word.chars() {
                        assert!(
                            is_permitted_in_quotes(c),
                            "control character U+{:04X} survived from {src:?}",
                            c as u32
                        );
                    }
                }
            }

            // And the strong form: if the source contained no quote and no escape,
            // then no character with syntactic meaning to a shell may appear in any
            // produced word.
            //
            // NEVER_UNQUOTED is written out here as a literal rather than expressed
            // as `!is_bare_word_char(c)`, and that is the whole point of it. An
            // earlier version of this test used the predicate, which made the
            // assertion a tautology: widening the allowlist widened the oracle with
            // it, so the sabotage run that added `$` to the permitted set passed
            // every test. A test whose expectation is derived from the code under
            // test cannot fail when that code is wrong. This list is the
            // independent statement of what a shell would have done with these
            // characters, and it does not move when the parser does.
            const NEVER_UNQUOTED: &[char] = &[
                '$', '`', '(', ')', '{', '}', '*', '?', '[', ']', '~', '#', '!', '|', '&', ';',
                '<', '>', '\'', '"', '\\', ' ', '\t', '\n',
            ];
            if !src.contains(['\'', '"', '\\']) {
                for cmd in line.commands() {
                    for word in cmd
                        .argv
                        .iter()
                        .chain(cmd.redirects.iter().filter_map(|r| r.target.as_ref()))
                    {
                        for c in word.chars() {
                            assert!(
                                !NEVER_UNQUOTED.contains(&c),
                                "shell metacharacter {c:?} reached an argv from unquoted {src:?}"
                            );
                        }
                    }
                }
            }
        }

        // Non-vacuity: if the generator produced nothing parseable, the assertions
        // above never ran and this test proves nothing at all.
        assert!(
            accepted > 200,
            "only {accepted} inputs parsed; the property was barely exercised"
        );
    }

    /// The characters this module permits inside a bare word, written out.
    ///
    /// This duplicates [`is_bare_word_char`] on purpose, and the duplication is
    /// the test. An oracle computed from the code under test cannot fail when that
    /// code is wrong — the first version of the sweep below asked
    /// `is_bare_word_char(c)` and therefore passed happily against a sabotaged
    /// parser that had `$` added to its allowlist. This list does not move when
    /// the parser moves, so widening the allowlist by one byte fails here.
    const PERMITTED_PUNCTUATION: &[char] = &['_', '-', '.', '/', '=', '+', ':', ',', '@', '%'];

    /// The grammar's own syntax, exempt from the sweeps because creating structure
    /// is exactly what it is for. Each has a test of its own above.
    const ADMITTED_SYNTAX: &[char] = &[' ', '\t', '\n', ';', '|', '<', '>', '&', '\'', '"', '\\'];

    /// The module's central claim, swept exhaustively rather than sampled:
    /// **every character outside the permitted set is refused.**
    ///
    /// Not "refused or harmless" — refused. The doc comment on
    /// [`is_bare_word_char`] says the lexer permits an enumerated set and refuses
    /// every byte outside it, and this is that sentence as an assertion. Stating
    /// it as the weaker "refused or structurally inert" was tried and rejected: it
    /// passes against a parser that quietly absorbs characters it does not
    /// understand, which is the exact habit the allowlist exists to prevent.
    ///
    /// Sweeping the whole ASCII range is cheaper and stronger than sampling it.
    /// There is no character in it this test has not tried.
    #[test]
    fn every_ascii_character_outside_the_permitted_set_is_refused() {
        for byte in 0u8..=127 {
            let c = byte as char;
            if c.is_ascii_alphanumeric() || ADMITTED_SYNTAX.contains(&c) {
                continue;
            }
            let src = format!("echo A{c}B");
            let parsed = parse(&src);

            if PERMITTED_PUNCTUATION.contains(&c) {
                let line = parsed.unwrap_or_else(|e| {
                    panic!("U+{byte:04X} ({c:?}) is permitted but {src:?} was refused: {e}")
                });
                let cmds: Vec<_> = line.commands().collect();
                assert_eq!(cmds.len(), 1, "U+{byte:04X} ({c:?}) split {src:?}");
                assert_eq!(
                    cmds[0].argv,
                    vec!["echo".to_string(), format!("A{c}B")],
                    "U+{byte:04X} ({c:?}) changed the argv of {src:?}"
                );
            } else {
                assert!(
                    parsed.is_err(),
                    "U+{byte:04X} ({c:?}) is outside the permitted set but {src:?} parsed as {:?}",
                    parsed.map(|l| l.commands().map(|c| c.argv.clone()).collect::<Vec<_>>())
                );
            }
        }
    }

    /// The structural half, kept separate because it is a different claim: even
    /// among characters that *are* accepted, none may silently create a
    /// sub-command. The set of commands the policy checks is the set this parser
    /// reports, so a stage that appears only at execution time was never checked.
    #[test]
    fn every_ascii_character_is_either_refused_or_structurally_inert() {
        for byte in 0u8..=127 {
            let c = byte as char;
            if c.is_ascii_alphanumeric() {
                continue;
            }
            if ADMITTED_SYNTAX.contains(&c) {
                continue;
            }
            let src = format!("echo A{c}B");
            let Ok(line) = parse(&src) else { continue };

            let cmds: Vec<_> = line.commands().collect();
            assert_eq!(
                cmds.len(),
                1,
                "U+{byte:04X} ({c:?}) created {} sub-commands from {src:?}",
                cmds.len()
            );
            assert!(
                cmds[0].redirects.is_empty(),
                "U+{byte:04X} ({c:?}) created a redirect from {src:?}"
            );
            assert_eq!(
                cmds[0].argv,
                vec!["echo".to_string(), format!("A{c}B")],
                "U+{byte:04X} ({c:?}) changed the argv of {src:?}"
            );
        }
    }

    /// The same sweep with the character in command position, where a mistake is
    /// worse: the word that lands in `argv[0]` is what gets checked against
    /// [`Act::Exec`](crate::Act::Exec) and then spawned.
    #[test]
    fn every_ascii_character_in_command_position_is_refused_or_inert() {
        for byte in 0u8..=127 {
            let c = byte as char;
            if c.is_ascii_alphanumeric() {
                continue;
            }
            if ADMITTED_SYNTAX.contains(&c) {
                continue;
            }
            let src = format!("A{c}B arg");
            let Ok(line) = parse(&src) else { continue };

            let cmds: Vec<_> = line.commands().collect();
            assert_eq!(
                cmds.len(),
                1,
                "U+{byte:04X} ({c:?}) created {} sub-commands from {src:?}",
                cmds.len()
            );
            assert_eq!(
                cmds[0].argv,
                vec![format!("A{c}B"), "arg".to_string()],
                "U+{byte:04X} ({c:?}) changed the argv of {src:?}"
            );
        }
    }

    /// The other half of non-vacuity: the parser must not accept a line that a
    /// shell would read as more commands than it reports. Every accepted line is
    /// re-checked by counting the operators the lexer found against the structure
    /// the parser built.
    #[test]
    fn the_reported_stage_count_matches_the_operators_in_the_line() {
        let cases = [
            ("a", 1),
            ("a | b", 2),
            ("a && b", 2),
            ("a || b", 2),
            ("a; b", 2),
            ("a | b | c", 3),
            ("a && b | c; d", 4),
            ("a > f && b < g | c", 3),
        ];
        for (src, want) in cases {
            assert_eq!(ok(src).commands().count(), want, "for `{src}`");
        }
    }
}
