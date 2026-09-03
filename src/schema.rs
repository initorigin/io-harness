//! A stated, closed subset of JSON Schema for an agent's final output (0.77.0).
//!
//! A run can end with "and the answer must be this shape". The obvious way to
//! honour that is to take whatever JSON Schema the caller wrote, check the parts
//! this crate happens to implement, and pass the rest. That is the version of
//! this feature worth refusing to ship: every test passes, every instance
//! validates, and a schema whose real constraint lived in an `oneOf` was never
//! checked at all. The caller believes in a boundary that does not exist — the
//! same failure `web.rs` names when it refuses to quietly drop the half of a
//! [`WebAccess`](crate::WebAccess) declaration a vendor cannot carry, and the
//! same one `ensure_web_supported` prevents by refusing before anything is sent.
//!
//! So the subset here is **closed and stated**. [`OutputSchema::new`] walks the
//! whole schema document once, at the moment the caller declares it, and refuses
//! any keyword outside the list below by name and by JSON pointer. What survives
//! that walk has been understood in full, which is what makes a later `Ok(())`
//! mean something.
//!
//! Validated:
//!
//! - `type` — `object`, `array`, `string`, `number`, `integer`, `boolean`, `null`
//! - `properties`, `required`, `additionalProperties` (the `false` form)
//! - `items` (the single-subschema form), `minItems`, `maxItems`
//! - `enum`
//! - `minimum`, `maximum`
//! - `minLength`, `maxLength`
//!
//! Accepted and ignored, because they describe the schema rather than constrain
//! the instance, and a caller pasting a schema from elsewhere should not have to
//! strip them: `$schema`, `title`, `description`.
//!
//! Everything else is refused: `oneOf`, `anyOf`, `allOf`, `not`, `$ref`,
//! `$defs`, `patternProperties`, `if`/`then`/`else`, `format`, `pattern`,
//! `const`, `uniqueItems`, and every keyword a future draft invents. Refusing an
//! unknown name is the whole mechanism — an accepted-but-unchecked keyword is
//! indistinguishable, from the outside, from one this crate enforces.
//!
//! The failure messages are written for the model, in the register `parse_plan`
//! in `src/run/gate.rs` established: one sentence per failure, naming where and
//! what, because a model re-prompted with one error at a time takes one attempt
//! per error. [`OutputSchema::validate`] therefore collects every failure rather
//! than stopping at the first.

use crate::error::{Error, Result};
use serde_json::Value;

/// One keyword this crate implements.
///
/// The variants exist so that [`check_instance`] can match on them
/// *exhaustively*, with no wildcard arm. That is the structural half of the
/// guarantee this module sells: a keyword cannot be added to [`KEYWORDS`]
/// without adding a variant here, and a variant cannot be added without the
/// validator failing to compile until it is checked. "Accepted but not
/// validated" is not a state this module can be left in by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keyword {
    /// `$schema` — which draft the author had in mind. Annotation.
    Schema,
    /// `title` — a human label. Annotation.
    Title,
    /// `description` — prose, usually written for the model. Annotation.
    Description,
    /// `type` — the JSON type the instance must have.
    Type,
    /// `properties` — a subschema per named field.
    Properties,
    /// `required` — the fields that must be present.
    Required,
    /// `additionalProperties` — `false` closes the object to undeclared fields.
    AdditionalProperties,
    /// `items` — one subschema every element must satisfy.
    Items,
    /// `minItems` / `maxItems` — array length bounds.
    MinItems,
    /// See [`Keyword::MinItems`].
    MaxItems,
    /// `enum` — the closed set of values allowed here.
    Enum,
    /// `minimum` / `maximum` — inclusive numeric bounds.
    Minimum,
    /// See [`Keyword::Minimum`].
    Maximum,
    /// `minLength` / `maxLength` — string length bounds, in characters.
    MinLength,
    /// See [`Keyword::MinLength`].
    MaxLength,
}

/// The subset, as data, in one place.
///
/// Both halves read this table: the constructor to decide what may appear in a
/// schema, and the refusal message to tell the caller what the alternatives are.
/// Scattering the membership across `match` arms is how a keyword ends up
/// accepted in one half and unknown in the other.
const KEYWORDS: &[(&str, Keyword)] = &[
    ("$schema", Keyword::Schema),
    ("title", Keyword::Title),
    ("description", Keyword::Description),
    ("type", Keyword::Type),
    ("properties", Keyword::Properties),
    ("required", Keyword::Required),
    ("additionalProperties", Keyword::AdditionalProperties),
    ("items", Keyword::Items),
    ("minItems", Keyword::MinItems),
    ("maxItems", Keyword::MaxItems),
    ("enum", Keyword::Enum),
    ("minimum", Keyword::Minimum),
    ("maximum", Keyword::Maximum),
    ("minLength", Keyword::MinLength),
    ("maxLength", Keyword::MaxLength),
];

/// The type names `type` may carry, which are the seven JSON types and nothing
/// else — no `any`, and no union array, both of which would be a second way to
/// say something the subset already says once.
const TYPES: &[&str] = &[
    "object", "array", "string", "number", "integer", "boolean", "null",
];

impl Keyword {
    /// The keyword a schema key names, or `None` when it names none of them —
    /// which is a refusal, not a shrug.
    fn parse(name: &str) -> Option<Self> {
        KEYWORDS
            .iter()
            .find(|(known, _)| *known == name)
            .map(|(_, keyword)| *keyword)
    }
}

/// A JSON Schema this crate has read in full and will validate in full.
///
/// **There is no way to obtain an `OutputSchema` that was not checked in full**
/// — not by construction, not by deserialization. [`OutputSchema::new`] is the
/// only constructor, and `Deserialize` is routed through it by
/// `#[serde(try_from = "serde_json::Value")]` rather than derived structurally,
/// because the path an operator is most likely to use is a TOML or JSON config
/// file, and a derived `Deserialize` would let a schema built on `oneOf` or
/// `$ref` enter the crate along exactly that path with nothing having looked at
/// it. A future reader who replaces that attribute with a plain derive removes
/// the guarantee this type exists to make while leaving every test passing.
///
/// The check happens when the schema is *declared*, not when output arrives.
/// That ordering is the point: by the time there is a model reply to validate,
/// the only keywords in the document are ones with a validation arm, so
/// validation cannot silently skip a constraint it does not implement. The
/// alternative — accept any schema, check what you recognise — passes the same
/// tests and guarantees nothing.
///
/// The original [`serde_json::Value`] is kept verbatim, because the provider is
/// sent this document as written and a re-serialised approximation of it is a
/// second thing that can drift.
///
/// ```
/// use io_harness::OutputSchema;
/// use serde_json::json;
///
/// let schema = OutputSchema::new(json!({
///     "type": "object",
///     "properties": {
///         "title": { "type": "string", "minLength": 1 },
///         "score": { "type": "integer", "minimum": 0, "maximum": 10 }
///     },
///     "required": ["title", "score"],
///     "additionalProperties": false
/// }))?;
///
/// // A conforming reply, however the model wrapped it. (`\u{60}` is a backtick:
/// // a doc example cannot spell a code fence inside one.)
/// let fence = "\u{60}\u{60}\u{60}";
/// let reply = format!("Here you go:\n{fence}json\n{{\"title\": \"ok\", \"score\": 7}}\n{fence}");
/// let value = schema.validate_text(&reply).expect("this reply conforms");
/// assert_eq!(value["score"], 7);
///
/// // A schema the crate cannot validate in full is refused where it is written,
/// // by name, rather than accepted and half-checked.
/// let refused = OutputSchema::new(json!({ "oneOf": [{ "type": "string" }] }))
///     .unwrap_err()
///     .to_string();
/// assert!(refused.contains("`oneOf`"), "{refused}");
/// # Ok::<(), io_harness::Error>(())
/// ```
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "serde_json::Value", into = "serde_json::Value")]
pub struct OutputSchema {
    /// The document as the caller wrote it. Private: handing out `&mut` to it
    /// would be a way to put a refused keyword back in after the check.
    document: Value,
}

impl OutputSchema {
    /// Read a schema document, refusing anything outside the stated subset.
    ///
    /// Walks the whole document — every subschema under `properties` and
    /// `items`, to any depth — and returns [`Error::Config`] naming the first
    /// keyword it cannot validate and the JSON pointer where it sits.
    ///
    /// [`Error::Config`] rather than a provider error, for the reason
    /// `ensure_web_supported` is one: nothing went wrong on the wire, a
    /// declaration was paired with a crate that cannot carry it, and that is a
    /// decision the caller made and can fix.
    ///
    /// The shape of each keyword's *value* is checked here too — `type` must
    /// name one of the seven types, `required` must be a list of names,
    /// `additionalProperties` must be a boolean. A malformed schema found at
    /// declaration costs one message; found at validation time it costs a run.
    pub fn new(document: Value) -> Result<Self> {
        check_subschema(&document, "")?;
        Ok(Self { document })
    }

    /// The document as written, for sending to the provider verbatim.
    pub fn as_value(&self) -> &Value {
        &self.document
    }

    /// Take the document back out.
    pub fn into_value(self) -> Value {
        self.document
    }

    /// Check an instance, collecting every failure.
    ///
    /// `Err` carries one sentence per failure, each naming the JSON pointer it
    /// applies to and what was wrong with the value there — written to be handed
    /// to the model unedited. Every failure is reported rather than only the
    /// first: a model re-prompted with one error at a time spends one attempt
    /// per error, and the attempts are the budget.
    ///
    /// ```
    /// use io_harness::OutputSchema;
    /// use serde_json::json;
    ///
    /// let schema = OutputSchema::new(json!({
    ///     "type": "object",
    ///     "properties": { "n": { "type": "integer" } },
    ///     "required": ["n", "label"]
    /// }))?;
    ///
    /// let failures = schema.validate(&json!({ "n": "seven" })).unwrap_err();
    /// assert_eq!(failures.len(), 2, "{failures:?}");
    /// # Ok::<(), io_harness::Error>(())
    /// ```
    pub fn validate(&self, instance: &Value) -> std::result::Result<(), Vec<String>> {
        let mut failures = Vec::new();
        check_instance(&self.document, instance, "", &mut failures);
        match failures.is_empty() {
            true => Ok(()),
            false => Err(failures),
        }
    }

    /// Read the model's final text and check what it produced.
    ///
    /// The one call a run loop wants: [`extract_json`] to get a document out of
    /// whatever the model wrapped it in, then [`validate`](Self::validate). Text
    /// that carries no JSON at all is itself a validation failure, with a message
    /// in the same register as the rest, so a caller has one list of sentences to
    /// hand back either way.
    pub fn validate_text(&self, text: &str) -> std::result::Result<Value, Vec<String>> {
        let value = extract_json(text).map_err(|failure| vec![failure])?;
        self.validate(&value)?;
        Ok(value)
    }
}

impl TryFrom<Value> for OutputSchema {
    type Error = Error;

    /// The seam `#[serde(try_from)]` uses. Deserialization is a construction
    /// like any other and gets the same walk — an operator whose config names a
    /// refused keyword is told which one, at load, rather than discovering at
    /// the end of a run that half their schema was decorative.
    fn try_from(document: Value) -> Result<Self> {
        Self::new(document)
    }
}

impl From<OutputSchema> for Value {
    fn from(schema: OutputSchema) -> Self {
        schema.document
    }
}

/// Pull the JSON document out of a model's final message.
///
/// Models wrap their answer. The three wrappings worth tolerating, in the order
/// they are tried: a fenced block (with or without a language tag), the whole
/// text as written, and a single balanced object or array with prose around it.
///
/// Deliberately not a repair engine. It does not close brackets, strip trailing
/// commas, or unescape anything: a document this cannot parse is one the model
/// should be told to send again, and a repaired document is a guess about intent
/// that then validates cleanly and lies about it.
///
/// ```
/// use io_harness::schema::extract_json;
///
/// // `\u{60}` is a backtick: a doc example cannot spell a code fence inside one.
/// let fence = "\u{60}\u{60}\u{60}";
/// let fenced = extract_json(&format!("sure:\n{fence}json\n{{\"a\": 1}}\n{fence}\nhope that helps"))?;
/// let bare = extract_json("  {\"a\": 1}  ")?;
/// let inline = extract_json("The answer is {\"a\": 1} — let me know.")?;
/// assert_eq!(fenced, bare);
/// assert_eq!(bare, inline);
/// # Ok::<(), String>(())
/// ```
pub fn extract_json(text: &str) -> std::result::Result<Value, String> {
    let trimmed = text.trim();
    let candidates = [fenced_body(trimmed), Some(trimmed), balanced_span(trimmed)];
    for candidate in candidates.into_iter().flatten() {
        if let Ok(value) = serde_json::from_str::<Value>(candidate) {
            return Ok(value);
        }
    }
    Err(
        "your reply does not contain a JSON document. Reply with the JSON alone — \
         no explanation before or after it, and nothing after the closing brace."
            .to_string(),
    )
}

/// The body of the first fenced block, with its language tag line dropped.
fn fenced_body(text: &str) -> Option<&str> {
    let open = text.find("```")?;
    let after = &text[open + 3..];
    // Everything up to the first newline is the language tag, if any. A fence
    // with no newline at all has no body worth reading; `balanced_span` picks
    // that shape up instead.
    let body = &after[after.find('\n')? + 1..];
    Some(body[..body.find("```")?].trim())
}

/// The first balanced `{...}` or `[...]` span, ignoring brackets inside strings.
///
/// Byte-indexed, and safe to slice: `start` and the closing index both land on
/// ASCII punctuation, so neither can fall inside a multi-byte character.
fn balanced_span(text: &str) -> Option<&str> {
    let start = text.find(['{', '['])?;
    let bytes = text.as_bytes();
    let open = bytes[start];
    let close = if open == b'{' { b'}' } else { b']' };
    let (mut depth, mut in_string, mut escaped) = (0usize, false, false);
    for (i, byte) in bytes.iter().enumerate().skip(start) {
        if in_string {
            match (escaped, *byte) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match *byte {
            b'"' => in_string = true,
            b if b == open => depth += 1,
            b if b == close => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Walk one subschema, refusing every key the subset does not name.
///
/// Recurses through `properties` values and `items`, and nowhere else: `enum`
/// values and `required` names are data, not subschemas, and a property
/// legitimately named `oneOf` is a property name rather than a keyword.
fn check_subschema(schema: &Value, at: &str) -> Result<()> {
    let Some(object) = schema.as_object() else {
        return Err(Error::Config(format!(
            "output schema: {} must be a JSON object naming the keywords the value has to \
             satisfy; found {}. The `true`/`false` schema form is not implemented.",
            pointer(at),
            kind_of(schema)
        )));
    };
    for (key, value) in object {
        let here = child(at, key);
        let Some(keyword) = Keyword::parse(key) else {
            return Err(Error::Config(format!(
                "output schema: `{key}` at `{here}` is not a keyword this crate validates. \
                 The accepted keywords are {}. A keyword accepted here and then skipped when \
                 output arrives would make the whole schema a guarantee that is not enforced, \
                 so it is refused where it is written instead. Rewrite the schema using only \
                 those keywords, or check this constraint yourself. No schema was accepted.",
                accepted()
            )));
        };
        match keyword {
            Keyword::Schema | Keyword::Title | Keyword::Description => {
                if !value.is_string() {
                    return Err(malformed(&here, key, "a string", value));
                }
            }
            Keyword::Type => {
                let Some(name) = value.as_str() else {
                    return Err(malformed(&here, key, "a string", value));
                };
                if !TYPES.contains(&name) {
                    return Err(Error::Config(format!(
                        "output schema: `type` at `{here}` names `{name}`, which is not one of \
                         {}. A union of types is not implemented either — state one.",
                        TYPES.join(", ")
                    )));
                }
            }
            Keyword::Properties => {
                let Some(properties) = value.as_object() else {
                    return Err(malformed(
                        &here,
                        key,
                        "an object mapping property names to subschemas",
                        value,
                    ));
                };
                for (name, subschema) in properties {
                    check_subschema(subschema, &child(&here, name))?;
                }
            }
            Keyword::Required => {
                let names = value.as_array();
                if !names.is_some_and(|list| list.iter().all(Value::is_string)) {
                    return Err(malformed(&here, key, "an array of property names", value));
                }
            }
            Keyword::AdditionalProperties => {
                if !value.is_boolean() {
                    return Err(malformed(
                        &here,
                        key,
                        "`true` or `false` — the subschema form is not implemented",
                        value,
                    ));
                }
            }
            // The single-subschema form only. A tuple (`items: [...]`) is a
            // different rule with different index semantics, and this is the
            // one that is implemented.
            Keyword::Items => check_subschema(value, &here)?,
            Keyword::Enum => {
                let Some(choices) = value.as_array() else {
                    return Err(malformed(&here, key, "an array of allowed values", value));
                };
                if choices.is_empty() {
                    return Err(Error::Config(format!(
                        "output schema: `enum` at `{here}` is empty, so no value could ever \
                         satisfy it."
                    )));
                }
            }
            Keyword::Minimum | Keyword::Maximum => {
                if !value.is_number() {
                    return Err(malformed(&here, key, "a number", value));
                }
            }
            Keyword::MinItems | Keyword::MaxItems | Keyword::MinLength | Keyword::MaxLength => {
                if value.as_u64().is_none() {
                    return Err(malformed(&here, key, "a non-negative whole number", value));
                }
            }
        }
    }
    Ok(())
}

/// Check one instance against one subschema, appending a sentence per failure.
///
/// The `match` below is exhaustive on purpose and must stay that way: it is what
/// makes a [`Keyword`] variant with no validation arm a compile error rather
/// than a silently unchecked constraint.
///
/// Each arm is a no-op when the instance is of the wrong JSON type for it —
/// `minimum` says nothing about a string. `type` is the arm that reports that,
/// once, so a single wrong value does not produce a paragraph of failures that
/// all say the same thing.
fn check_instance(schema: &Value, instance: &Value, at: &str, out: &mut Vec<String>) {
    // The constructor guaranteed both of these; neither `else` branch is
    // reachable through the public API.
    let Some(object) = schema.as_object() else {
        return;
    };
    for (key, value) in object {
        let Some(keyword) = Keyword::parse(key) else {
            continue;
        };
        match keyword {
            // Annotations describe the schema. There is nothing to check, and
            // saying so in an arm is what keeps the match exhaustive.
            Keyword::Schema | Keyword::Title | Keyword::Description => {}
            Keyword::Type => {
                let want = value.as_str().unwrap_or_default();
                if !type_matches(want, instance) {
                    out.push(format!(
                        "{} is {}; the schema requires {}.",
                        pointer(at),
                        kind_of(instance),
                        named(want)
                    ));
                }
            }
            Keyword::Properties => {
                let (Some(properties), Some(fields)) = (value.as_object(), instance.as_object())
                else {
                    continue;
                };
                for (name, subschema) in properties {
                    // A field that is absent is `required`'s business, not this
                    // one's — an optional field is allowed to be missing.
                    if let Some(found) = fields.get(name) {
                        check_instance(subschema, found, &child(at, name), out);
                    }
                }
            }
            Keyword::Required => {
                let (Some(names), Some(fields)) = (value.as_array(), instance.as_object()) else {
                    continue;
                };
                for name in names.iter().filter_map(Value::as_str) {
                    if !fields.contains_key(name) {
                        out.push(format!(
                            "{} has no `{name}`; the schema requires it.",
                            pointer(at)
                        ));
                    }
                }
            }
            Keyword::AdditionalProperties => {
                let (Some(false), Some(fields)) = (value.as_bool(), instance.as_object()) else {
                    continue;
                };
                let declared = object.get("properties").and_then(Value::as_object);
                for name in fields.keys() {
                    if !declared.is_some_and(|known| known.contains_key(name)) {
                        out.push(format!(
                            "{} has `{name}`, which the schema does not declare; \
                             `additionalProperties` is false, so remove it.",
                            pointer(at)
                        ));
                    }
                }
            }
            Keyword::Items => {
                let Some(elements) = instance.as_array() else {
                    continue;
                };
                for (i, element) in elements.iter().enumerate() {
                    check_instance(value, element, &child(at, &i.to_string()), out);
                }
            }
            Keyword::Enum => {
                let Some(choices) = value.as_array() else {
                    continue;
                };
                if !choices.contains(instance) {
                    out.push(format!(
                        "{} is {instance}; the schema allows only {}.",
                        pointer(at),
                        choices
                            .iter()
                            .map(Value::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
            Keyword::Minimum => {
                let (Some(bound), Some(found)) = (value.as_f64(), instance.as_f64()) else {
                    continue;
                };
                if found < bound {
                    out.push(format!(
                        "{} is {instance}; the schema requires at least {value}.",
                        pointer(at)
                    ));
                }
            }
            Keyword::Maximum => {
                let (Some(bound), Some(found)) = (value.as_f64(), instance.as_f64()) else {
                    continue;
                };
                if found > bound {
                    out.push(format!(
                        "{} is {instance}; the schema allows at most {value}.",
                        pointer(at)
                    ));
                }
            }
            // Characters, not UTF-16 code units as the JSON Schema drafts
            // specify. The number goes into a sentence a model reads, and "12
            // characters" is the count it can act on; the two differ only for
            // astral-plane text, where neither number means much to a reader.
            Keyword::MinLength => {
                let (Some(bound), Some(text)) = (value.as_u64(), instance.as_str()) else {
                    continue;
                };
                let found = text.chars().count() as u64;
                if found < bound {
                    out.push(format!(
                        "{} is {found} characters long; the schema requires at least {value}.",
                        pointer(at)
                    ));
                }
            }
            Keyword::MaxLength => {
                let (Some(bound), Some(text)) = (value.as_u64(), instance.as_str()) else {
                    continue;
                };
                let found = text.chars().count() as u64;
                if found > bound {
                    out.push(format!(
                        "{} is {found} characters long; the schema allows at most {value}.",
                        pointer(at)
                    ));
                }
            }
            Keyword::MinItems => {
                let (Some(bound), Some(elements)) = (value.as_u64(), instance.as_array()) else {
                    continue;
                };
                if (elements.len() as u64) < bound {
                    out.push(format!(
                        "{} has {} items; the schema requires at least {value}.",
                        pointer(at),
                        elements.len()
                    ));
                }
            }
            Keyword::MaxItems => {
                let (Some(bound), Some(elements)) = (value.as_u64(), instance.as_array()) else {
                    continue;
                };
                if elements.len() as u64 > bound {
                    out.push(format!(
                        "{} has {} items; the schema allows at most {value}.",
                        pointer(at),
                        elements.len()
                    ));
                }
            }
        }
    }
}

/// Whether an instance has the named JSON type.
///
/// `integer` accepts `3.0` as well as `3`, because JSON has one number type and
/// a model that wrote a trailing `.0` has not made a different claim.
fn type_matches(name: &str, instance: &Value) -> bool {
    match name {
        "object" => instance.is_object(),
        "array" => instance.is_array(),
        "string" => instance.is_string(),
        "number" => instance.is_number(),
        "integer" => matches!(instance.as_f64(), Some(n) if n.fract() == 0.0),
        "boolean" => instance.is_boolean(),
        "null" => instance.is_null(),
        // Unreachable: the constructor refused any other name.
        _ => false,
    }
}

/// The accepted keywords, quoted, for a refusal message.
fn accepted() -> String {
    KEYWORDS
        .iter()
        .map(|(name, _)| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A malformed keyword value, named the same way everywhere.
fn malformed(at: &str, key: &str, expected: &str, found: &Value) -> Error {
    Error::Config(format!(
        "output schema: `{key}` at `{at}` must be {expected}; found {}.",
        kind_of(found)
    ))
}

/// How a location reads in a sentence. The root has no pointer to quote, and
/// "`` `` is a string" would be worse than useless to the model reading it.
fn pointer(at: &str) -> String {
    match at.is_empty() {
        true => "the document root".to_string(),
        false => format!("`{at}`"),
    }
}

/// One step deeper, with RFC 6901's escaping — a property genuinely named
/// `a/b` must not read as two path segments.
fn child(at: &str, token: &str) -> String {
    format!("{at}/{}", token.replace('~', "~0").replace('/', "~1"))
}

/// What a value is, for a sentence that says what it should have been.
fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// The same phrasing as [`kind_of`], for a `type` name rather than a value, so
/// the two halves of "is a string; the schema requires a number" agree.
fn named(type_name: &str) -> String {
    match type_name {
        "null" => "null".to_string(),
        "object" | "array" | "integer" => format!("an {type_name}"),
        other => format!("a {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema(document: Value) -> OutputSchema {
        OutputSchema::new(document).expect("this schema is inside the subset")
    }

    #[test]
    fn every_accepted_keyword_takes_a_conforming_instance_and_names_a_violating_one() {
        // One row per validated keyword: the schema, an instance that satisfies
        // it, and one that does not together with the words the model must see.
        let cases: &[(Value, Value, Value, &str)] = &[
            (
                json!({ "type": "object" }),
                json!({}),
                json!([]),
                "is an array; the schema requires an object",
            ),
            (
                json!({ "type": "array" }),
                json!([1]),
                json!({}),
                "requires an array",
            ),
            (
                json!({ "type": "string" }),
                json!("x"),
                json!(1),
                "is a number; the schema requires a string",
            ),
            (
                json!({ "type": "number" }),
                json!(1.5),
                json!("1.5"),
                "requires a number",
            ),
            (
                json!({ "type": "integer" }),
                json!(3),
                json!(3.5),
                "requires an integer",
            ),
            (
                json!({ "type": "boolean" }),
                json!(false),
                json!("false"),
                "requires a boolean",
            ),
            (
                json!({ "type": "null" }),
                json!(null),
                json!(0),
                "requires null",
            ),
            (
                json!({ "properties": { "n": { "type": "integer" } } }),
                json!({ "n": 1 }),
                json!({ "n": "one" }),
                "`/n` is a string",
            ),
            (
                json!({ "required": ["n"] }),
                json!({ "n": 1 }),
                json!({}),
                "has no `n`; the schema requires it",
            ),
            (
                json!({ "properties": { "n": {} }, "additionalProperties": false }),
                json!({ "n": 1 }),
                json!({ "n": 1, "extra": true }),
                "has `extra`, which the schema does not declare",
            ),
            (
                json!({ "items": { "type": "integer" } }),
                json!([1, 2]),
                json!([1, "2"]),
                "`/1` is a string",
            ),
            (
                json!({ "enum": ["open", "closed"] }),
                json!("open"),
                json!("done"),
                "the schema allows only \"open\", \"closed\"",
            ),
            (
                json!({ "minimum": 5 }),
                json!(5),
                json!(4),
                "is 4; the schema requires at least 5",
            ),
            (
                json!({ "maximum": 5 }),
                json!(5),
                json!(6),
                "is 6; the schema allows at most 5",
            ),
            (
                json!({ "minLength": 3 }),
                json!("abc"),
                json!("ab"),
                "is 2 characters long; the schema requires at least 3",
            ),
            (
                json!({ "maxLength": 3 }),
                json!("abc"),
                json!("abcd"),
                "is 4 characters long; the schema allows at most 3",
            ),
            (
                json!({ "minItems": 2 }),
                json!([1, 2]),
                json!([1]),
                "has 1 items; the schema requires at least 2",
            ),
            (
                json!({ "maxItems": 2 }),
                json!([1, 2]),
                json!([1, 2, 3]),
                "has 3 items; the schema allows at most 2",
            ),
        ];
        for (document, good, bad, expected) in cases {
            let schema = schema(document.clone());
            assert!(
                schema.validate(good).is_ok(),
                "{document} should accept {good}: {:?}",
                schema.validate(good)
            );
            let failures = schema
                .validate(bad)
                .expect_err(&format!("{document} should reject {bad}"));
            assert!(
                failures.iter().any(|f| f.contains(expected)),
                "{document} rejecting {bad} should say {expected:?}, said {failures:?}"
            );
        }
    }

    #[test]
    fn a_keyword_outside_the_subset_is_refused_by_name_when_the_schema_is_declared() {
        for keyword in [
            "oneOf",
            "anyOf",
            "allOf",
            "not",
            "$ref",
            "$defs",
            "patternProperties",
            "if",
            "then",
            "else",
            "format",
            "pattern",
            "const",
            "uniqueItems",
            "definitions",
        ] {
            // Built by hand rather than through `json!`, so the keyword under
            // test is a value this loop supplies and not a literal in a macro.
            let mut document = serde_json::Map::new();
            document.insert("type".to_string(), json!("object"));
            document.insert(keyword.to_string(), json!({}));
            let refusal = OutputSchema::new(Value::Object(document))
                .expect_err(&format!("`{keyword}` is outside the subset"))
                .to_string();
            assert!(
                refusal.contains(&format!("`{keyword}`")),
                "the refusal must name `{keyword}`: {refusal}"
            );
            assert!(
                refusal.contains("not a keyword this crate validates"),
                "{refusal}"
            );
        }
    }

    #[test]
    fn a_refused_keyword_nested_in_a_subschema_is_named_with_the_pointer_that_reached_it() {
        let refusal = OutputSchema::new(json!({
            "type": "object",
            "properties": {
                "rows": {
                    "type": "array",
                    "items": { "type": "object", "properties": { "price": { "format": "money" } } }
                }
            }
        }))
        .expect_err("`format` is outside the subset")
        .to_string();
        assert!(
            refusal.contains("/properties/rows/items/properties/price/format"),
            "{refusal}"
        );
    }

    #[test]
    fn the_annotation_keywords_are_accepted_and_constrain_nothing() {
        let schema = schema(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "Report",
            "description": "what the run produced",
            "type": "object"
        }));
        assert!(schema.validate(&json!({ "anything": [1, 2, 3] })).is_ok());
        // And they are still checked for shape, because a `title` that is an
        // object is a schema its author did not mean to write.
        let refusal = OutputSchema::new(json!({ "title": 7 }))
            .expect_err("a numeric title is malformed")
            .to_string();
        assert!(refusal.contains("`title`"), "{refusal}");
        assert!(refusal.contains("must be a string"), "{refusal}");
    }

    #[test]
    fn a_nested_failure_names_the_full_pointer_to_the_value_that_failed() {
        let schema = schema(json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": { "price": { "type": "number" } }
                    }
                }
            }
        }));
        let failures = schema
            .validate(&json!({ "items": [{ "price": 1 }, { "price": 2 }, { "price": "3" }] }))
            .expect_err("the third price is a string");
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert_eq!(
            failures[0],
            "`/items/2/price` is a string; the schema requires a number."
        );
    }

    #[test]
    fn a_property_name_with_a_slash_in_it_is_escaped_rather_than_read_as_two_segments() {
        let schema = schema(json!({ "properties": { "a/b": { "type": "integer" } } }));
        let failures = schema
            .validate(&json!({ "a/b": "no" }))
            .expect_err("a/b is a string");
        assert!(failures[0].starts_with("`/a~1b`"), "{failures:?}");
    }

    #[test]
    fn every_failure_is_collected_rather_than_only_the_first() {
        let schema = schema(json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "minLength": 3 },
                "score": { "type": "integer", "maximum": 10 }
            },
            "required": ["name", "score", "owner"],
            "additionalProperties": false
        }));
        let failures = schema
            .validate(&json!({ "name": "ab", "score": 99, "stray": 1 }))
            .expect_err("four separate things are wrong");
        // Too short, too large, a missing required field, and an undeclared one.
        assert_eq!(failures.len(), 4, "{failures:?}");
        for expected in [
            "`/name` is 2 characters long",
            "`/score` is 99; the schema allows at most 10",
            "the document root has no `owner`",
            "has `stray`, which the schema does not declare",
        ] {
            assert!(
                failures.iter().any(|f| f.contains(expected)),
                "missing {expected:?} in {failures:?}"
            );
        }
    }

    #[test]
    fn a_wrongly_typed_value_is_reported_once_rather_than_by_every_keyword_that_touches_it() {
        let schema = schema(json!({ "type": "number", "minimum": 5, "maximum": 9 }));
        let failures = schema.validate(&json!("seven")).expect_err("not a number");
        assert_eq!(failures.len(), 1, "{failures:?}");
    }

    #[test]
    fn a_schema_whose_keyword_value_is_malformed_is_refused_when_it_is_declared() {
        for (document, expected) in [
            (json!({ "type": "date" }), "names `date`"),
            (json!({ "type": ["string", "null"] }), "must be a string"),
            (json!({ "required": "name" }), "array of property names"),
            (json!({ "required": [1] }), "array of property names"),
            (
                json!({ "additionalProperties": { "type": "string" } }),
                "subschema form is not implemented",
            ),
            (
                json!({ "items": [{ "type": "string" }] }),
                "must be a JSON object",
            ),
            (json!({ "enum": [] }), "is empty"),
            (json!({ "minimum": "5" }), "must be a number"),
            (json!({ "maxItems": -1 }), "non-negative whole number"),
            (json!(true), "must be a JSON object"),
        ] {
            let refusal = OutputSchema::new(document.clone())
                .expect_err(&format!("{document} is malformed"))
                .to_string();
            assert!(refusal.contains(expected), "{document}: {refusal}");
        }
    }

    #[test]
    fn a_config_carrying_a_refused_keyword_cannot_be_deserialized_and_the_error_names_it() {
        // The shape an operator writes: a config section with the schema on it.
        // `Debug` because `expect_err` renders the Ok side, which is the whole
        // point of the assertion — a deserialization that succeeded here is the
        // hole this test exists to prove is closed.
        #[derive(Debug, serde::Deserialize)]
        struct Section {
            output_schema: OutputSchema,
        }

        let refusal = serde_json::from_str::<Section>(
            r#"{ "output_schema": { "type": "object", "oneOf": [{ "type": "string" }] } }"#,
        )
        .expect_err("deserialization must go through the constructor")
        .to_string();
        assert!(refusal.contains("`oneOf`"), "{refusal}");

        let config = r#"{ "output_schema": { "type": "object", "required": ["a"] } }"#;
        let accepted: Section =
            serde_json::from_str(config).expect("a schema inside the subset deserializes");
        assert_eq!(accepted.output_schema.as_value()["type"], "object");
    }

    #[test]
    fn the_document_survives_a_serialize_deserialize_round_trip_unchanged() {
        let document = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": { "n": { "type": "integer", "minimum": 0 } },
            "required": ["n"]
        });
        let schema = schema(document.clone());
        // Kept verbatim, because a later task sends this to the provider as
        // written rather than a re-rendered approximation of it.
        assert_eq!(schema.as_value(), &document);
        let wire = serde_json::to_string(&schema).expect("serializes");
        let back: OutputSchema = serde_json::from_str(&wire).expect("round trips");
        assert_eq!(back, schema);
        assert_eq!(back.into_value(), document);
    }

    #[test]
    fn the_model_text_helper_reads_a_fence_a_bare_document_and_prose_around_one() {
        let want = json!({ "title": "ok", "n": 2 });
        for text in [
            "```json\n{\"title\": \"ok\", \"n\": 2}\n```",
            "```\n{\"title\": \"ok\", \"n\": 2}\n```",
            "{\"title\": \"ok\", \"n\": 2}",
            "  \n {\"title\": \"ok\", \"n\": 2}\n  ",
            "Here is the report you asked for:\n\n```json\n{\"title\": \"ok\", \"n\": 2}\n```\n\nLet me know if you want it shorter.",
            "Sure — {\"title\": \"ok\", \"n\": 2} — that covers it.",
        ] {
            assert_eq!(extract_json(text).expect("this shape parses"), want, "{text:?}");
        }
    }

    #[test]
    fn a_brace_inside_a_string_does_not_end_the_document_early() {
        let value = extract_json("answer: {\"note\": \"a } and a \\\" in here\"} done")
            .expect("the closing brace inside the string is not the end");
        assert_eq!(value["note"], "a } and a \" in here");
    }

    #[test]
    fn a_top_level_array_is_read_the_same_way_an_object_is() {
        let value = extract_json("the list:\n```json\n[1, 2, 3]\n```").expect("an array parses");
        assert_eq!(value, json!([1, 2, 3]));
    }

    #[test]
    fn a_reply_with_no_json_in_it_is_a_failure_that_says_what_to_send_instead() {
        let failure = extract_json("I could not complete the task.").expect_err("no JSON at all");
        assert!(failure.contains("JSON alone"), "{failure}");
        // And it arrives through `validate_text` as one failure like any other,
        // so a caller has a single list of sentences to hand back to the model.
        let schema = schema(json!({ "type": "object" }));
        let failures = schema
            .validate_text("I could not complete the task.")
            .expect_err("no JSON at all");
        assert_eq!(failures, vec![failure]);
    }

    #[test]
    fn a_property_named_like_a_refused_keyword_is_a_property_name_not_a_keyword() {
        // `oneOf` sits under `properties`, so it is a field name and nothing
        // this module has an opinion about.
        let schema = schema(json!({
            "type": "object",
            "properties": { "oneOf": { "type": "string" } },
            "required": ["oneOf"]
        }));
        assert!(schema.validate(&json!({ "oneOf": "fine" })).is_ok());
        assert!(schema.validate(&json!({})).is_err());
    }

    #[test]
    fn enum_values_are_data_and_are_not_walked_as_subschemas() {
        // A refused keyword *inside* an `enum` value is a value the model may
        // legitimately produce, not a schema keyword.
        let schema = schema(json!({ "enum": [{ "$ref": "#/x" }, "plain"] }));
        assert!(schema.validate(&json!({ "$ref": "#/x" })).is_ok());
        assert!(schema.validate(&json!("other")).is_err());
    }

    #[test]
    fn every_table_entry_has_a_distinct_keyword_and_the_table_is_the_only_membership_test() {
        for (name, keyword) in KEYWORDS {
            assert_eq!(Keyword::parse(name), Some(*keyword), "{name}");
        }
        assert_eq!(Keyword::parse("oneOf"), None);
        assert_eq!(Keyword::parse("Type"), None, "matching is case-sensitive");
        assert!(
            accepted().contains("`additionalProperties`"),
            "the refusal message lists the table"
        );
    }
}
