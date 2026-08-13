//! The six things a run may do with a browser, and what it gets back (0.53.0).
//!
//! Each action returns text the model reads, and every one of them ends with
//! whatever the page said while it ran — console output and uncaught errors,
//! attached to the action that produced them rather than to a tool the model has
//! to remember to call. That is 0.52.0's decision about diagnostics applied
//! again, for the same reason: a run that clicks a button and gets a page that
//! looks unchanged has learned nothing, and the same run reading
//! `Uncaught TypeError` from that click has learned the whole answer.

use serde_json::{json, Value};

use crate::browser::{fail, Browser, BrowserConfig, Line};
use crate::error::Result;

/// What a run asked the browser to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    /// Go to a URL. Gated at the paused request, like every other navigation.
    Navigate { url: String },
    /// The page's text, or one element's.
    Read { selector: Option<String> },
    /// A PNG of the viewport, which the model looks at.
    Screenshot,
    /// A trusted click at the element a selector resolves to.
    Click { selector: String },
    /// Focus an element and type into it.
    Type { selector: String, text: String },
    /// Scroll the page by a number of pixels.
    Scroll { dy: i64 },
}

/// What an action produced.
pub(crate) struct Outcome {
    /// What the model reads.
    pub(crate) text: String,
    /// A screenshot, base64-encoded PNG, staged for the next turn.
    pub(crate) image: Option<String>,
}

/// How long a page is given to finish loading before the answer is returned
/// anyway, expressed as attempts rather than as a deadline the suite asserts on.
///
/// A page that polls never goes idle, so waiting for quiet is waiting forever.
/// The bound expiring is a **normal outcome** that still returns the page, and
/// the observation says the page had not finished — never an error, and never a
/// clock any test reads.
const SETTLE_TRIES: u32 = 20;
const SETTLE_STEP: std::time::Duration = std::time::Duration::from_millis(100);

/// Run one action against a live browser.
pub(crate) async fn act(browser: &Browser, action: Action) -> Result<Outcome> {
    let mut image = None;
    let body = match action {
        Action::Navigate { url } => {
            browser.page("Page.navigate", json!({"url": url})).await?;
            let settled = settle(browser).await;
            let mut out = format!("navigated to {url}");
            if !settled {
                out.push_str("\nthe page had not finished loading when this returned");
            }
            out
        }
        Action::Read { selector } => {
            let expression = match &selector {
                Some(s) => format!(
                    "(document.querySelector({}) || {{}}).innerText || ''",
                    json!(s)
                ),
                None => "document.body ? document.body.innerText : ''".to_string(),
            };
            let value = evaluate(browser, &expression).await?;
            let text = value.as_str().unwrap_or_default().to_string();
            if text.trim().is_empty() {
                match selector {
                    Some(s) => format!("no text at {s}"),
                    None => "the page has no text".to_string(),
                }
            } else {
                text
            }
        }
        Action::Screenshot => {
            let shot = browser
                .page("Page.captureScreenshot", json!({"format": "png"}))
                .await?;
            let data = shot
                .get("data")
                .and_then(Value::as_str)
                .ok_or_else(|| fail("the browser returned no screenshot"))?
                .to_string();
            let (width, height) = browser.viewport();
            image = Some(data);
            format!("a screenshot of the page at {width}x{height}")
        }
        Action::Click { selector } => {
            let (x, y) = locate(browser, &selector).await?;
            for kind in ["mousePressed", "mouseReleased"] {
                browser
                    .page(
                        "Input.dispatchMouseEvent",
                        json!({"type": kind, "x": x, "y": y,
                               "button": "left", "clickCount": 1}),
                    )
                    .await?;
            }
            // A click can navigate, and that navigation is gated at the request
            // like any other. Settling here is what lets the caller see it.
            settle(browser).await;
            format!("clicked {selector}")
        }
        Action::Type { selector, text } => {
            let node = node_id(browser, &selector).await?;
            browser.page("DOM.focus", json!({"nodeId": node})).await?;
            browser
                .page("Input.insertText", json!({"text": text}))
                .await?;
            format!("typed into {selector}")
        }
        Action::Scroll { dy } => {
            browser
                .page(
                    "Input.dispatchMouseEvent",
                    json!({"type": "mouseWheel", "x": 0, "y": 0,
                           "deltaX": 0, "deltaY": dy}),
                )
                .await?;
            format!("scrolled {dy} pixels")
        }
    };

    Ok(Outcome {
        text: with_console(body, browser.drain_console()),
        image,
    })
}

/// Evaluate one expression in the page and return its value.
async fn evaluate(browser: &Browser, expression: &str) -> Result<Value> {
    let answer = browser
        .page(
            "Runtime.evaluate",
            json!({"expression": expression, "returnByValue": true}),
        )
        .await?;
    Ok(answer
        .get("result")
        .and_then(|r| r.get("value"))
        .cloned()
        .unwrap_or(Value::Null))
}

/// Wait for the page to finish loading, bounded. Returns whether it did.
async fn settle(browser: &Browser) -> bool {
    for _ in 0..SETTLE_TRIES {
        if let Ok(state) = evaluate(browser, "document.readyState").await {
            if state.as_str() == Some("complete") {
                return true;
            }
        }
        tokio::time::sleep(SETTLE_STEP).await;
    }
    false
}

/// The node a selector resolves to, or a failure naming the selector.
///
/// A selector that matches nothing must fail loudly. The alternative — treating
/// it as a no-op that succeeded — is the classic browser-automation defect and
/// the one a model cannot detect: it reads a successful result, believes it
/// clicked, and reasons forward from a state that never happened.
async fn node_id(browser: &Browser, selector: &str) -> Result<i64> {
    let document = browser.page("DOM.getDocument", json!({})).await?;
    let root = document
        .get("root")
        .and_then(|r| r.get("nodeId"))
        .and_then(Value::as_i64)
        .ok_or_else(|| fail("the browser returned no document"))?;
    let found = browser
        .page(
            "DOM.querySelector",
            json!({"nodeId": root, "selector": selector}),
        )
        .await?;
    match found.get("nodeId").and_then(Value::as_i64) {
        Some(node) if node != 0 => Ok(node),
        _ => Err(fail(format!("nothing on the page matches `{selector}`"))),
    }
}

/// The centre of the element a selector resolves to.
async fn locate(browser: &Browser, selector: &str) -> Result<(f64, f64)> {
    let node = node_id(browser, selector).await?;
    let box_model = browser
        .page("DOM.getBoxModel", json!({"nodeId": node}))
        .await?;
    let content = box_model
        .get("model")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
        .ok_or_else(|| fail(format!("`{selector}` has no box on the page")))?;
    let number = |i: usize| content.get(i).and_then(Value::as_f64).unwrap_or_default();
    // The quad is four corners; its centre is the midpoint of two opposite ones.
    Ok(((number(0) + number(4)) / 2.0, (number(1) + number(5)) / 2.0))
}

/// Append what the page said, or say plainly that it said nothing.
///
/// The empty case is stated rather than omitted: a section that disappears when
/// there is nothing to report is indistinguishable from one that was never
/// collected, and a model reading a short answer needs to know which.
fn with_console(body: String, lines: Vec<Line>) -> String {
    if lines.is_empty() {
        return format!("{body}\n\n[console] nothing");
    }
    let mut out = format!("{body}\n\n[console]");
    for line in &lines {
        out.push_str(&format!("\n{}: {}", line.kind, line.text));
    }
    out
}

/// A browser held for the life of a run, started on first use.
///
/// Lazy rather than eager, unlike the language-server session: a browser has no
/// index to warm, so a run that configures one and never browses should not pay
/// for a process it does not use.
pub(crate) struct BrowserSession {
    config: Option<BrowserConfig>,
    /// The proxy a contained run owns, which the browser is pointed at so its
    /// traffic takes the same path every other contained command's traffic takes.
    ///
    /// Set after construction rather than passed to it, because the run starts
    /// its proxy after it builds this session and the browser does not launch
    /// until the first action — so the address is always in place before it is
    /// needed, and a session for an uncontained run never gets one.
    proxy: std::sync::Mutex<Option<String>>,
    started: tokio::sync::Mutex<Option<Browser>>,
}

impl BrowserSession {
    /// A session for a run that configured a browser, or one that did nothing.
    pub(crate) fn new(config: Option<BrowserConfig>) -> Self {
        Self {
            config,
            proxy: std::sync::Mutex::new(None),
            started: tokio::sync::Mutex::new(None),
        }
    }

    /// Point this run's browser at the loopback proxy the run owns (0.48.0).
    pub(crate) fn route_through(&self, addr: std::net::SocketAddr) {
        *self.proxy.lock().expect("browser proxy is not poisoned") = Some(addr.to_string());
    }

    /// Whether this run configured a browser at all.
    ///
    /// What decides the tool catalogue: a run that configured none is offered no
    /// browser schema, which is what keeps its composed prompt byte-identical to
    /// the previous release's.
    pub(crate) fn configured(&self) -> bool {
        self.config.is_some()
    }

    /// Run one action, starting the browser if this is the first.
    pub(crate) async fn act(
        &self,
        action: Action,
        policy: &crate::Policy,
        store: &crate::state::Store,
        run_id: i64,
        watch: &crate::run::Watch<'_>,
    ) -> Result<Acted> {
        let config = self
            .config
            .clone()
            .ok_or_else(|| fail("no browser is configured for this run"))?;
        let mut held = self.started.lock().await;
        let mut started = None;
        if held.is_none() {
            let proxy = self
                .proxy
                .lock()
                .expect("browser proxy is not poisoned")
                .clone();
            let browser =
                crate::browser::launch(&config, policy, store, run_id, watch, proxy.as_deref())
                    .await?;
            started = Some(Started {
                binary: browser.binary().to_string(),
                headless: config.headless,
                ready_ms: browser.ready_ms(),
            });
            *held = Some(browser);
        }
        let browser = held.as_ref().expect("the browser was just started");
        let outcome = act(browser, action).await;
        // The decisions are drained whether the action succeeded or not, and the
        // action's own failure is carried rather than propagated. A **refused
        // navigation is exactly the case that fails and has a decision worth
        // recording** — the browser reports the blocked load as an error, and a
        // `?` here would throw away the one event that proves the boundary held.
        Acted {
            outcome,
            decisions: browser.gate().drain(),
            started,
        }
        .into()
    }

    /// Close the browser if one was started. Called where the run ends.
    pub(crate) async fn shutdown(&self) {
        if let Some(browser) = self.started.lock().await.take() {
            browser.close().await;
        }
    }
}

/// What one action produced, including the parts that survive its failure.
///
/// The outer [`Result`] is "could this run have a browser at all"; the inner one
/// is "did this action work". They are genuinely different questions: a browser
/// that will not start ends the tool call, while a navigation the policy refused
/// is the boundary working and comes back as an observation the model can adapt
/// to.
pub(crate) struct Acted {
    pub(crate) outcome: Result<Outcome>,
    pub(crate) decisions: Vec<crate::browser::Decision>,
    pub(crate) started: Option<Started>,
}

impl From<Acted> for Result<Acted> {
    fn from(acted: Acted) -> Self {
        Ok(acted)
    }
}

/// What the event naming a started browser carries.
pub(crate) struct Started {
    pub(crate) binary: String,
    pub(crate) headless: bool,
    pub(crate) ready_ms: u128,
}

/// The six schemas, offered only to a run that configured a browser.
pub(crate) fn browser_tools() -> Vec<crate::provider::ToolSpec> {
    use crate::provider::ToolSpec;
    let selector = |what: &str| {
        json!({
            "type": "object",
            "properties": {
                "selector": { "type": "string", "description": format!("CSS selector for {what}.") }
            },
            "required": ["selector"]
        })
    };
    vec![
        ToolSpec {
            name: super::BROWSER_NAVIGATE_TOOL.to_string(),
            description: "Open a URL in the browser. The run's network policy decides whether it \
                          is reached, and so does every navigation a click or a redirect causes."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "The URL to open." }
                },
                "required": ["url"]
            }),
        },
        ToolSpec {
            name: super::BROWSER_READ_TOOL.to_string(),
            description: "The text the page actually renders, after its scripts have run — not \
                          the HTML the server sent."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "selector": { "type": "string", "description": "One element's text, by CSS selector. Omit for the whole page." }
                }
            }),
        },
        ToolSpec {
            name: super::BROWSER_SCREENSHOT_TOOL.to_string(),
            description: "A picture of the page, which you will be shown. Use it when how the \
                          page looks is the question — text says a heading exists, a screenshot \
                          says it is off-screen or behind a dialog."
                .to_string(),
            parameters: json!({"type": "object", "properties": {}}),
        },
        ToolSpec {
            name: super::BROWSER_CLICK_TOOL.to_string(),
            description: "Click an element. Fails naming the selector if nothing matches, so a \
                          click that did not happen is never reported as one that did."
                .to_string(),
            parameters: selector("the element to click"),
        },
        ToolSpec {
            name: super::BROWSER_TYPE_TOOL.to_string(),
            description: "Focus an element and type into it.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "selector": { "type": "string", "description": "CSS selector for the field." },
                    "text": { "type": "string", "description": "The text to type." }
                },
                "required": ["selector", "text"]
            }),
        },
        ToolSpec {
            name: super::BROWSER_SCROLL_TOOL.to_string(),
            description: "Scroll the page vertically by a number of pixels; negative scrolls up."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "dy": { "type": "integer", "description": "Pixels to scroll, negative for up." }
                },
                "required": ["dy"]
            }),
        },
    ]
}

/// Shared so the run loop and the tests read one list rather than two.
pub(crate) fn is_browser_tool(name: &str) -> bool {
    matches!(
        name,
        super::BROWSER_NAVIGATE_TOOL
            | super::BROWSER_READ_TOOL
            | super::BROWSER_SCREENSHOT_TOOL
            | super::BROWSER_CLICK_TOOL
            | super::BROWSER_TYPE_TOOL
            | super::BROWSER_SCROLL_TOOL
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_action_with_no_console_output_says_so_rather_than_omitting_the_section() {
        // A section that disappears when empty is indistinguishable from one that
        // was never collected, and the model cannot tell which it is reading.
        let out = with_console("navigated to https://example.com".into(), Vec::new());
        assert!(out.ends_with("[console] nothing"), "{out}");
    }

    #[test]
    fn console_lines_are_labelled_by_kind() {
        let out = with_console(
            "clicked #go".into(),
            vec![
                Line {
                    kind: "log".into(),
                    text: "hello".into(),
                },
                Line {
                    kind: "page error".into(),
                    text: "TypeError: x is not a function".into(),
                },
            ],
        );
        assert!(out.contains("log: hello"), "{out}");
        assert!(
            out.contains("page error: TypeError: x is not a function"),
            "{out}"
        );
    }

    #[test]
    fn the_six_schemas_are_the_whole_surface_and_are_named_once() {
        let names: Vec<String> = browser_tools().into_iter().map(|t| t.name).collect();
        assert_eq!(names.len(), 6);
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 6, "a schema is declared twice: {names:?}");
        assert!(names.iter().all(|n| is_browser_tool(n)));
    }
}
