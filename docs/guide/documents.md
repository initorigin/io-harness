# Documents

Documents let the agent read and write spreadsheets, Word files, PowerPoint
decks, PDFs and barcodes — the file types that were a byte blob it could neither
read nor produce when every file it could open was text.

```toml
io-harness = { version = "0.15", features = ["documents"] }
```

`documents` is an umbrella over five per-format features — `xlsx`, `docx`,
`pptx`, `pdf`, `barcode` — so a consumer who wants spreadsheets does not compile
a PDF stack. The default build is unchanged: `default = []`, every document
dependency optional, nothing paid for by a consumer who does not ask. Nothing
here binds a C or C++ library, so no CI runner needs a system package.

## What the agent gains

Twelve tools, offered beside `read_file` and `write_file` when the feature is on:
`xlsx_read`, `xlsx_sheets`, `xlsx_write`, `xlsx_set_cell`, `docx_read`,
`docx_write`, `pptx_read`, `pdf_read`, `pdf_write`, `pdf_watermark`,
`pdf_fill_form`, `barcode_decode`.

They are **built-ins, not registered `Tool` implementations**, and that is
deliberate. A registered tool is authorised once by an `Act::Exec` check on its
name, and the [permission model](permissions.md) states plainly that the policy
governs whether a tool is *called* and not what it does once running. A document
tool's whole job is reading and writing files in the user's workspace, so
dispatch gates each call on `Act::Read` or `Act::Write` against the path the
model named: `deny_write("secrets/*")` stops `xlsx_set_cell` in the same place
and for the same reason it stops `write_file`. Every byte in and out goes through
`Workspace::read_bytes` / `write_bytes` — nothing here opens a path itself.

A sub-agent's narrowed policy therefore applies to documents exactly as it
applies to source; see [Agent composition](composition.md).

## Verifying a document

```rust
use io_harness::{TaskContract, Verification};

let contract = TaskContract::workspace(
    "Add the Q3 revenue row to the summary workbook.",
    "/path/to/repo",
    Verification::DocumentContains {
        file: "reports/summary.xlsx".into(),
        needle: "Q3".into(),
    },
);
```

It gates on the document's *extracted* text, and it exists because the criterion
you would otherwise reach for cannot work here.
`Verification::WorkspaceFileContains` reads with
`read_to_string(..).unwrap_or_default()`; every format above is a binary
container, so that read yields the empty string and the criterion reports "does
not contain" for every document — including one whose visible text plainly does
contain the needle. That is a silent, permanent false FAIL, not a false pass: a
gate that can never say yes, ending the run as `StepCapReached` so it looks like
an agent that could not do the work.

The reader is chosen by extension — `.xlsx`, `.docx`, `.pptx`, `.pdf`. Anything
else is an error, not a fallback to matching raw bytes: a criterion that silently
degrades into a weaker check is the failure mode this exists to remove. The
variant exists in every build; one compiled without the matching feature returns
a typed `Error::Config` saying so rather than vanishing from the enum.

Like the other content criteria it is gameable and does not prove the document is
*correct* — see [what a passing gate proves](verification.md#what-a-passing-gate-proves).

## The limits, stated plainly

**Word is generate-and-read, with no in-place edit.** `docx-rs`'s reader models
the OOXML it knows and drops what it does not, so read-then-write is a lossy
rewrite rather than an edit — on a user's real document it silently deletes the
comment thread, content control, field or vendor extension the reader could not
name. The capability is not claimed rather than claimed and unsafe.

**PowerPoint is read-only and deliberately has no writer.** `pptx_read` pulls the
slide text out; there is no `pptx_write`.

**PDF text extraction is best-effort on reading order.** A PDF stores placed
glyphs, not a document: columns can interleave, tables lose their shape, and a
scanned page returns empty text rather than an error. Treat the output as
evidence, not as the document. `pdf-extract` is also written to panic on input it
does not understand, so the call catches the unwind and reports a typed error —
which does nothing under a `panic = "abort"` profile.

**The spreadsheet preserving-edit is preservation in practice, not a guarantee.**
`xlsx_set_cell` is a real `umya-spreadsheet` round trip: what the library models
it keeps, what it does not model it can drop or normalise on the way out. It is
not guaranteed for chart-, drawing-, pivot- or macro-heavy workbooks — that is
the case to check before trusting an edit, not the case to assume.

**OCR and PowerPoint authoring were cut from the roadmap, not deferred.** Barcode
generation, the in-place Word edit and document delete were on the old capability
line and are not here either; none of the three is on a later release. See
[CHANGELOG.md](../../CHANGELOG.md) for what each cut cost and why it was made.

## The decisions behind those cuts

These are not gaps waiting for a later release. Each is a decision with its
reasoning, and none appears in any planned version.

### OCR is off the roadmap

Every viable Rust path breaks a standing constraint that no CI runner installs a
system package. The Tesseract binding needs the system library on all three
runners — worst on Windows, where it means vcpkg and a from-source C++ build plus
libclang for bindgen, with language data distributed to every user. The pure-Rust
alternative needs MSRV 1.89 against this crate's 1.88 floor, fetches its models
over the network on first use, and recognises Latin script only.

A capability that only works on machines that were prepared for it is not a
capability this crate can claim.

### PowerPoint authoring is off the roadmap

Generating a deck means writing slide layouts, masters, theme parts and the
relationship graph that ties them together. Hand-rolled on top of a zip writer,
that produces a file PowerPoint may or may not open — and "may or may not" is not
a capability. The one credible Rust crate is a 46-star, single-maintainer,
pre-0.3 project.

Reading a deck is a projection and cannot corrupt anything, so the read half
stays: `.pptx` is read-only here, and no write path for it exists in the public
surface. The reader is a short piece of code over `zip` and `quick-xml` — a
`.pptx` is a zip of XML and the text lives in `ppt/slides/*.xml` — rather than a
ten-day-old extraction crate, because the vetting rule that removed OCR applies
to convenience too.

### Editing a Word document in place is not claimed

`docx-rs`'s round trip silently drops the OOXML it does not model. On a document
this harness generated that costs nothing; on a user's real one — a comment
thread, a content control, a field, a shape, a vendor extension — it deletes the
parts the reader could not name, which is **data loss presented as an edit**, and
that is precisely what an agent editing someone's document must not do.
`xlsx_set_cell` exists because `umya-spreadsheet` genuinely round-trips a workbook
it did not create; nothing in the Word ecosystem earns the same call yet.

### Video is off the roadmap

Named here because it is the neighbouring question and gets asked alongside OCR.
The Anthropic Messages API and the OpenAI Chat Completions API accept images and
**no video at all**, and OpenRouter — the only one of the three carrying a
`video_url` content part — states that support varies by model and offers no way
to ask which. That resolves to one provider out of three and an unanswerable
question. Audio is likewise absent. If video returns it will be a new roadmap
entry argued on its own merits. See [Images and git](images-and-git.md) for what
*is* supported.

### High-fidelity PDF rendering and rasterisation is out of scope

It means binding Pdfium, a per-OS binary the crate would have to tell users to
install — the same constraint that removed OCR, plus a redistribution question.

### Barcode generation and document deletion are absent

The barcode half decodes and does not encode. Deleting a document is not a
document gap: the harness has no delete for any file — there is no `delete_file`
built-in for source text either — so it is an absent file operation, and the
document work did not introduce one.

## Why spreadsheets take three crates

The three operations are genuinely separate: `calamine` reads and cannot write,
`rust_xlsxwriter` writes new files and explicitly cannot modify an existing one,
and `umya-spreadsheet` is the only one that round-trips an existing workbook. One
crate for three jobs would have meant claiming one of them badly.

## See also

- [Permissions and approval](permissions.md) — the per-path `Act::Read`/`Act::Write` gate every document call passes
- [Verification](verification.md) — `DocumentContains` and what a passing criterion proves
- [Tools and skills](tools-and-skills.md) — why these are built-ins rather than registered tools
- [Images and git](images-and-git.md) — the other optional-feature capability, and the video decision
- [Agent composition](composition.md) — how a narrowed child policy reaches documents
- [CHANGELOG.md](../../CHANGELOG.md) — what each cut cost and why it was made
- [The contract](../CONTRACT.md) — the crate's stability and API promises
- [README](../../README.md)
