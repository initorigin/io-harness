//! A loaded configuration never prints the secrets it resolved (0.71.0, F1) —
//! and still prints enough to debug with (F1's control), while `Serialize` keeps
//! writing what the operator typed (F2).
//!
//! The defect this pins is that `Config::parse` resolves every `${env:}`,
//! `${file:}` and `${cmd:}` *before* the merged table is stored. From that point
//! a `Config` holds the plaintext twice — once in the typed sections and once in
//! the raw table `with_profile` overlays through — so the derived `Debug` on
//! `Config`, `File` and `ProviderSpec` printed a provider's `api_key` and an
//! `[[mcp]]` server's `Authorization` header, the second of which never touches
//! `ProviderSpec` at all. Both paths are asserted here, because fixing either one
//! alone leaves the credential in the first log line anyone writes.
//!
//! Hiding them on `Config` is not enough on its own, and that gap is asserted
//! here too. `File`'s impl omits the `[[mcp]]`, `[[lsp]]` and `[[hook]]` bodies
//! *because* each of them holds a substituted string — and `Config::mcp_servers`,
//! `Config::lsp_servers` and `Config::hooks` then hand those bodies straight back
//! to the caller. A redaction one call deep and a derived `Debug` one call
//! further on is not a redaction, so `McpTransport`, `McpServer`, `LspServer` and
//! `Hook` each have a hand-written impl of their own and each is asserted below.
//!
//! The same reasoning reaches three more accessors, and they are asserted here
//! too: `Config::toolchain` overlays a `[toolchain.<ecosystem>]` table's six
//! argvs onto a detection, `Config::browser` hands back a `[browser]` table whose
//! extra arguments are how a proxy credential is written, and `Config::agents`
//! hands back a roster whose `role` is a free-form prompt string. A `${env:}`
//! fills any of them exactly as it fills a hook's argv.
//!
//! Every absence assertion carries a positive control in the same pass:
//! `f.debug_struct("Config").finish()` hides the secret perfectly and tells an
//! operator nothing, and it would pass an absence-only test for ever. So the
//! model id, the MCP server id and the shape of the merged table must all still
//! be there.
//!
//! Both `{:?}` and `{:#?}`, because they are different code paths in `std`: a
//! hand-written impl written with `write!` rather than the `debug_struct`
//! builder can differ between them.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use io_harness::config::{Config, Scope};
use io_harness::ProviderSpec;

/// The process has one environment and `cargo test` runs these in parallel, so
/// every test that sets a variable takes this first — the same rule
/// `tests/config.rs` runs on, for the same reason.
static ENV: Mutex<()> = Mutex::new(());

/// The provider credential. Distinctive enough that a substring search cannot
/// match it by accident, and self-explaining in a failure message.
const KEY: &str = "sk-SENTINEL-PROVIDER-KEY-DO-NOT-PRINT";

/// The MCP `Authorization` value — a second secret on a second path, reached
/// through the raw table and the `[[mcp]]` array rather than through
/// `ProviderSpec`.
const TOKEN: &str = "SENTINEL-MCP-BEARER-DO-NOT-PRINT";

/// A language server's child environment — the `[[lsp]]` path, which reaches a
/// formatter through `Config::lsp_servers` and through nothing else.
const LSP_TOKEN: &str = "SENTINEL-LSP-ENV-DO-NOT-PRINT";

/// A hook's argv — the `[[hook]]` path. An argument is the ordinary way to hand a
/// child process a credential, and `Hook` became public in this release.
const HOOK_TOKEN: &str = "SENTINEL-HOOK-ARGV-DO-NOT-PRINT";

/// A `[toolchain.<ecosystem>]` override's argv — six of them per ecosystem, each
/// an argv the embedding application hands to `exec`.
const TOOLCHAIN_TOKEN: &str = "SENTINEL-TOOLCHAIN-ARGV-DO-NOT-PRINT";

/// An `[[agent]]` role — a free-form prompt string, which a `${env:}` fills like
/// every other string in the table.
const AGENT_TOKEN: &str = "SENTINEL-AGENT-ROLE-DO-NOT-PRINT";

/// A `[browser]` extra argument. `--proxy-server=https://user:pass@host` is the
/// ordinary way to point a browser at an authenticated proxy, so this is a live
/// credential path rather than a hypothetical one.
const BROWSER_TOKEN: &str = "SENTINEL-BROWSER-ARG-DO-NOT-PRINT";

/// Every sentinel, so one helper covers every rendering rather than each test
/// remembering which secrets its own value can reach.
const SECRETS: &[&str] = &[
    KEY,
    TOKEN,
    LSP_TOKEN,
    HOOK_TOKEN,
    TOOLCHAIN_TOKEN,
    AGENT_TOKEN,
    BROWSER_TOKEN,
];

const KEY_VAR: &str = "IO_HARNESS_SECRETS_TEST_KEY";
const TOKEN_VAR: &str = "IO_HARNESS_SECRETS_TEST_TOKEN";
const LSP_VAR: &str = "IO_HARNESS_SECRETS_TEST_LSP";
const HOOK_VAR: &str = "IO_HARNESS_SECRETS_TEST_HOOK";
const TOOLCHAIN_VAR: &str = "IO_HARNESS_SECRETS_TEST_TOOLCHAIN";
const AGENT_VAR: &str = "IO_HARNESS_SECRETS_TEST_AGENT";
const BROWSER_VAR: &str = "IO_HARNESS_SECRETS_TEST_BROWSER";

/// Hold the environment, and point the user scope at somewhere empty so a config
/// file on the developer's own machine cannot change what this measures.
fn env<'a>(user_dir: &Path) -> MutexGuard<'a, ()> {
    let guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("IO_CONFIG_HOME", user_dir);
    for (var, value) in [
        (KEY_VAR, KEY),
        (TOKEN_VAR, TOKEN),
        (LSP_VAR, LSP_TOKEN),
        (HOOK_VAR, HOOK_TOKEN),
        (TOOLCHAIN_VAR, TOOLCHAIN_TOKEN),
        (AGENT_VAR, AGENT_TOKEN),
        (BROWSER_VAR, BROWSER_TOKEN),
    ] {
        std::env::set_var(var, value);
    }
    guard
}

/// Both formats of one value.
fn both_forms<T: std::fmt::Debug>(value: &T) -> [String; 2] {
    [format!("{value:?}"), format!("{value:#?}")]
}

/// Neither secret is in the rendering, and everything in `expected` is.
///
/// The second half is the control, and it is not optional: an impl that printed
/// nothing at all would satisfy the first half for ever.
fn hides_secrets_shows<T: std::fmt::Debug>(value: &T, expected: &[&str]) {
    for rendered in both_forms(value) {
        for &secret in SECRETS {
            assert!(
                !rendered.contains(secret),
                "a resolved secret reached a formatter: {rendered}"
            );
        }
        for needle in expected {
            assert!(
                rendered.contains(needle),
                "{needle:?} is what an operator debugging a misconfiguration reads, and it is \
                 not in the rendering: {rendered}"
            );
        }
    }
}

/// A discovered config whose provider key, MCP header, LSP environment and hook
/// argv are all substituted — one secret per accessor that hands a caller a type
/// `File`'s own impl refuses to print.
///
/// Discovered rather than parsed, so `sources`, `origins` and the merged `raw`
/// table are all populated — the fields a derived `Debug` would have printed. The
/// hooks go in `io.local.toml` because a project-scoped file may not declare
/// them: a hook runs a program and `io.toml` arrives with a `git clone`.
///
/// The `Cargo.toml` is not configuration — it is the marker
/// [`io_harness::toolchain::detect`] needs, because `Config::toolchain` overlays
/// a `[toolchain.<ecosystem>]` table onto a detection rather than producing one.
fn loaded(project: &Path) -> Config {
    std::fs::write(project.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    std::fs::write(
        project.join("io.toml"),
        format!(
            "[[provider]]\nkind = \"anthropic\"\nmodel = \"claude-sonnet-4\"\n\
             api_key = \"${{env:{KEY_VAR}}}\"\n\n\
             [[mcp]]\nid = \"sentinel-server\"\ntransport = \"http\"\n\
             url = \"https://mcp.example.test/v1\"\n[mcp.headers]\n\
             Authorization = \"Bearer ${{env:{TOKEN_VAR}}}\"\n\n\
             [[mcp]]\nid = \"sentinel-stdio\"\ntransport = \"stdio\"\n\
             command = \"sentinel-mcp\"\nargs = [\"--token=${{env:{TOKEN_VAR}}}\"]\n\
             [mcp.env]\nSENTINEL_MCP_TOKEN = \"${{env:{TOKEN_VAR}}}\"\n\n\
             [[lsp]]\nid = \"sentinel-lsp\"\ncommand = \"sentinel-analyzer\"\n\
             args = [\"--log=${{env:{LSP_VAR}}}\"]\nextensions = [\".sentinel\"]\n\
             [lsp.env]\nSENTINEL_LSP_TOKEN = \"${{env:{LSP_VAR}}}\"\n\n\
             [[agent]]\nname = \"sentinel-agent\"\nmodel = \"sentinel-model\"\n\
             role = \"You are ${{env:{AGENT_VAR}}}\"\neffort = \"high\"\n\
             deny_write = true\n\n\
             [toolchain.cargo]\n\
             test = [\"sentinel-runner\", \"--token=${{env:{TOOLCHAIN_VAR}}}\"]\n\
             lint = []\n"
        ),
    )
    .unwrap();
    std::fs::write(
        project.join("io.local.toml"),
        format!(
            "[[hook]]\non = [\"refused\"]\n\
             run = [\"sentinel-gate\", \"--token=${{env:{HOOK_VAR}}}\"]\n\n\
             [[hook]]\non = [\"refused\"]\nappend = \"sentinel-audit.jsonl\"\n"
        ),
    )
    .unwrap();
    Config::discover(project).unwrap()
}

// ---------------------------------------------------------------------------
// F1 — the secrets are resolved, and none of the three impls prints them
// ---------------------------------------------------------------------------

#[test]
fn a_loaded_config_does_not_print_the_secrets_it_resolved() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());

    let config = loaded(project.path());

    // The substitution really happened — otherwise everything below is asserting
    // that a config with no secrets in it prints no secrets.
    assert_eq!(config.sources()[0].0, Scope::Project);
    let Some(ProviderSpec::Anthropic { model, api_key }) = config.provider_spec() else {
        panic!("the file named an anthropic provider");
    };
    assert_eq!(api_key.as_deref(), Some(KEY), "the key reached the field");
    assert_eq!(model, "claude-sonnet-4");
    let io_harness::McpTransport::Http { headers, .. } = &config.mcp_servers()[0].transport else {
        panic!("the file named an http server");
    };
    assert_eq!(
        headers["Authorization"],
        format!("Bearer {TOKEN}"),
        "the header reached the field"
    );

    // The spec on its own: the typed path, and the one an application hands
    // around after `provider_spec()`.
    hides_secrets_shows(
        config.provider_spec().unwrap(),
        &["claude-sonnet-4", "api_key: <redacted>"],
    );

    // And the whole config, which is where both paths meet: the typed sections
    // and the raw merged table the profile overlay is applied through. The MCP
    // header only ever existed on the second of those.
    hides_secrets_shows(
        &config,
        &[
            // The `File` behind it is private, so it is reachable only through
            // here — and this is the proof its own impl ran rather than a
            // derived one.
            "File {",
            "claude-sonnet-4",
            "sentinel-server",
            "sentinel-lsp",
            // The raw table is rendered as its shape. Key names and value kinds
            // stay, which is what makes the rendering worth reading; the leaf a
            // `${env:}` filled is gone.
            "\"api_key\": string",
        ],
    );
}

/// The gap `Config`'s own impl leaves: the three accessors that hand a caller
/// the very arrays `File`'s impl refuses to render.
///
/// `format!("{:?}", config)` being clean and `format!("{:?}", config.hooks())`
/// being a leak is a distinction no caller can be expected to hold, and a log
/// line is written from whichever one is in scope. Each arm asserts the absence
/// *and* what an operator still reads, because an impl that printed nothing would
/// satisfy the absence for ever.
#[test]
fn the_accessors_that_hand_back_those_arrays_do_not_print_them_either() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());

    let config = loaded(project.path());

    // Positive controls first: each secret reached the field it is asserted
    // absent from below. Without these, every assertion here would pass on a
    // configuration that simply never resolved anything.
    let io_harness::McpTransport::Http { headers, .. } = &config.mcp_servers()[0].transport else {
        panic!("the file named an http server");
    };
    assert_eq!(headers["Authorization"], format!("Bearer {TOKEN}"));
    let io_harness::McpTransport::Stdio {
        args, env: child, ..
    } = &config.mcp_servers()[1].transport
    else {
        panic!("the file named a stdio server");
    };
    assert_eq!(args[0], format!("--token={TOKEN}"), "the argv was filled");
    assert_eq!(
        child["SENTINEL_MCP_TOKEN"], TOKEN,
        "the child env was filled"
    );
    assert_eq!(
        config.lsp_servers()[0].args[0],
        format!("--log={LSP_TOKEN}")
    );
    assert_eq!(config.lsp_servers()[0].env["SENTINEL_LSP_TOKEN"], LSP_TOKEN);
    let hooks = config.hooks();
    let declarations = hooks.declarations();
    assert_eq!(
        declarations[0].run().unwrap()[1],
        format!("--token={HOOK_TOKEN}"),
        "the hook argv reached the field"
    );
    assert!(
        declarations[1].append().is_some(),
        "the second hook appends"
    );

    // `[[mcp]]`: the header names stay, their values go, and the endpoint goes
    // through the same redaction a provider's `base_url` does.
    hides_secrets_shows(
        &config.mcp_servers(),
        &[
            "sentinel-server",
            "mcp.example.test",
            "\"Authorization\": <redacted>",
            // The stdio half: a spawned server is handed its credential through
            // an argument or the child environment rather than a header, and
            // both of those are a different field on a different variant.
            "sentinel-stdio",
            "sentinel-mcp",
            "<1 redacted>",
            "\"SENTINEL_MCP_TOKEN\": <redacted>",
        ],
    );

    // `[[lsp]]`: the program and the suffixes it answers for are what an operator
    // selects a server by; the child environment is how it is handed a token.
    hides_secrets_shows(
        &config.lsp_servers(),
        &[
            "sentinel-lsp",
            "sentinel-analyzer",
            ".sentinel",
            "<1 redacted>",
            "\"SENTINEL_LSP_TOKEN\": <redacted>",
        ],
    );

    // `[[hook]]`: the events and the program stay, the arguments are counted.
    // `<1 redacted>` rather than a bare absence, so "this hook was given no
    // arguments at all" stays distinguishable from "its arguments are withheld".
    hides_secrets_shows(&hooks, &["refused", "sentinel-gate", "<1 redacted>"]);

    // The `append` path is a value the same substitution walked, so it is set-or-
    // not and nothing more. Asserted on the compact form alone: `{:#?}` breaks
    // `Some(_)` across three lines and this is a claim about one rendering, not
    // about `std`'s line breaking.
    assert!(
        format!("{hooks:?}").contains("append: Some(<redacted>)"),
        "an operator reads that the hook appends somewhere, and not where: {hooks:?}"
    );
}

/// The rest of the class: the three remaining accessors that hand a caller a
/// type built out of substituted strings.
///
/// `Config::toolchain` is the strongest of them — six argvs per ecosystem, each
/// one an argv the embedding application hands to `exec`, which is structurally
/// the `[[hook]]` hole one release-note bullet over. `Config::agents` carries a
/// free-form prompt string. Each arm asserts the absence *and* what an operator
/// still reads, because an impl that printed nothing would satisfy the absence
/// for ever.
#[test]
fn the_toolchain_overlay_and_the_agent_roster_do_not_print_them_either() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());

    let config = loaded(project.path());

    // `[toolchain.cargo]` — an override onto a detection, not a detection.
    let detected = io_harness::toolchain::detect(project.path()).expect("Cargo.toml is a marker");
    assert_eq!(
        detected.test,
        ["cargo", "test"],
        "the detection is the base"
    );
    let tuned = config.toolchain(detected);

    // Positive controls: the override replaced the detection, and the
    // substitution reached the argv it replaced it with.
    assert_eq!(
        tuned.test,
        vec![
            "sentinel-runner".to_string(),
            format!("--token={TOOLCHAIN_TOKEN}")
        ],
        "the override reached the argv"
    );
    assert_eq!(
        tuned.build,
        ["cargo", "build"],
        "what the file did not name is unchanged"
    );
    assert!(
        tuned.lint.is_empty(),
        "the file named an empty lint command"
    );

    hides_secrets_shows(
        &tuned,
        &[
            // What a toolchain is selected and debugged by, and what no file can
            // write.
            "cargo",
            "Cargo.toml",
            // Each argv keeps its program and counts the rest. `build` is the
            // untouched detection and is treated exactly as the override is: the
            // rule is about the field, not about where the value came from.
            "sentinel-runner",
            "<1 redacted>",
            // A job the ecosystem has no command for renders as `[]` rather than
            // as `[<0 redacted>]` — "there is no linter" and "the linter takes no
            // arguments" are different answers to the same question.
            "lint: []",
        ],
    );

    // `[[agent]]` — the roster, through the container `Config::agents` returns.
    let agents = config.agents();
    let def = agents.get("sentinel-agent").expect("the roster names it");
    assert_eq!(
        def.role,
        Some(format!("You are {AGENT_TOKEN}")),
        "the role reached the field"
    );

    hides_secrets_shows(
        &agents,
        &[
            // The name a spawn asks for, the model it asks for, and the boundary
            // it narrows to — which is the whole reason anyone formats a roster.
            "sentinel-agent",
            "sentinel-model",
            "High",
            "deny_write: true",
        ],
    );

    // Set-or-not and nothing more, the same treatment `Hook::append` gets.
    // Asserted on the compact form alone: `{:#?}` breaks `Some(_)` across three
    // lines, and this is a claim about one rendering rather than about `std`'s
    // line breaking.
    assert!(
        format!("{def:?}").contains("role: Some(<redacted>)"),
        "an operator reads that this agent has a role, and not what it says: {def:?}"
    );
}

/// `[browser]`, whose type exists only when the crate is built with the feature.
///
/// Gated on the test rather than on the file the way `tests/browser.rs` gates
/// itself: everything else here compiles without the feature, and a second file
/// for one test would be a second fixture to keep in step with this one.
#[cfg(feature = "browser")]
#[test]
fn a_credential_in_a_browser_argument_is_not_printed_either() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());

    // `io.local.toml`: a project-scoped file may not configure a browser, because
    // `[browser]` names a program to execute and `io.toml` arrives with a clone.
    std::fs::write(
        project.path().join("io.local.toml"),
        format!(
            "[browser]\nbinary = \"/usr/bin/sentinel-browser\"\n\
             args = [\"--proxy-server=https://user:${{env:{BROWSER_VAR}}}@proxy.example.test:8080\"]\n\
             headless = false\nwidth = 1440\n"
        ),
    )
    .unwrap();
    let config = Config::discover(project.path()).unwrap();

    // Positive control: an authenticated proxy is the ordinary way this field is
    // written, and the substitution really filled it.
    let browser = config.browser().expect("the file declared one");
    assert_eq!(
        browser.args,
        vec![format!(
            "--proxy-server=https://user:{BROWSER_TOKEN}@proxy.example.test:8080"
        )],
        "the proxy credential reached the field"
    );
    assert_eq!(browser.binary.as_deref(), Some("/usr/bin/sentinel-browser"));

    hides_secrets_shows(
        browser,
        &[
            // The binary this crate is about to spawn — "which browser did it
            // actually pick" is the first question anyone debugging `[browser]`
            // asks — and the operator's own numbers beside it.
            "/usr/bin/sentinel-browser",
            "<1 redacted>",
            "headless: false",
            "1440",
        ],
    );
}

/// The same hazard `ProviderSpec`'s `base_url` carries, on the other type that
/// takes a caller-written URL: an MCP endpoint behind a gateway is routinely
/// written with the credential in it, and it is not `headers` that would have
/// leaked it.
#[test]
fn a_credential_carried_in_an_mcp_url_is_not_printed_either() {
    let config = Config::from_toml(
        "[[mcp]]\nid = \"gateway\"\ntransport = \"http\"\n\
         url = \"https://user:sk-SENTINEL-IN-A-URL@mcp.example.test/v1?api-key=sk-SENTINEL-IN-A-URL\"\n",
    )
    .unwrap();
    for rendered in both_forms(&config.mcp_servers()) {
        assert!(
            !rendered.contains("sk-SENTINEL-IN-A-URL"),
            "a credential in the MCP URL reached a formatter: {rendered}"
        );
        // Host and id are the control: dropping the field would pass the
        // assertion above and leave an operator with nothing to look at.
        for needle in ["mcp.example.test", "gateway"] {
            assert!(rendered.contains(needle), "{rendered}");
        }
    }
}

/// The unset case is the other half of the operator-facing claim: "this file
/// supplied a key and it was still wrong" and "this file supplied none, so the
/// provider read its own environment variable" are different misconfigurations,
/// and a placeholder that could not tell them apart would hide the second one.
#[test]
fn an_unset_key_renders_as_none_rather_than_as_a_placeholder() {
    let config = Config::from_toml(
        "[[provider]]\nkind = \"openrouter\"\nmodel = \"anthropic/claude-sonnet-4\"\n",
    )
    .unwrap();
    let rendered = format!("{:?}", config.provider_spec().unwrap());
    assert_eq!(
        rendered, "OpenRouter { model: \"anthropic/claude-sonnet-4\", api_key: None }",
        "an unset key is `None`, and a set one is `<redacted>`"
    );
}

/// Withholding `api_key` is not enough on its own: a `compatible` provider's
/// base URL is written by the operator, and gateway and Azure-style deployments
/// routinely carry the credential *in the URL*. Printing it verbatim would put
/// the key in a log through the field next to the one being redacted.
#[test]
fn a_credential_carried_in_a_base_url_is_not_printed_either() {
    let config = Config::from_toml(
        "[[provider]]\nkind = \"compatible\"\n\
         base_url = \"https://user:sk-SENTINEL-IN-A-URL@gateway.example.test/v1\"\n\
         model = \"some-model\"\nauth = \"none\"\n",
    )
    .unwrap();
    for rendered in both_forms(config.provider_spec().unwrap()) {
        assert!(
            !rendered.contains("sk-SENTINEL-IN-A-URL"),
            "a credential in the base URL reached a formatter: {rendered}"
        );
        // The host and the model are the control: dropping the whole field would
        // pass the assertion above and tell an operator nothing.
        for needle in ["gateway.example.test", "some-model"] {
            assert!(rendered.contains(needle), "{rendered}");
        }
    }
}

// ---------------------------------------------------------------------------
// F2 — `Serialize` is untouched
// ---------------------------------------------------------------------------

/// An application layer persists the spec the operator typed — io-cli's first-run
/// wizard writes the key it just asked for straight back out of a `ProviderSpec`.
/// Redacting `Serialize` alongside `Debug` would write `<redacted>` into their
/// settings file and lose the credential, so the round trip is pinned here rather
/// than left to the downstream test that would have caught it a release later.
#[test]
fn serializing_a_spec_keeps_the_key_the_operator_wrote() {
    // Every variant that can carry a key, not one of them. This test asserted
    // `Anthropic` alone until a sabotage arm landed on `OpenRouter` by accident
    // and nothing went red — and `OpenRouter` is the variant the consumer this
    // criterion exists for actually writes. A per-variant `#[serde]` attribute is
    // exactly the kind of edit that lands on one arm of an enum, so covering one
    // arm and calling it covered is how the gap gets in.
    let specs = [
        ProviderSpec::OpenRouter {
            model: "anthropic/claude-sonnet-4".into(),
            api_key: Some(KEY.into()),
        },
        ProviderSpec::Anthropic {
            model: "claude-sonnet-4".into(),
            api_key: Some(KEY.into()),
        },
        ProviderSpec::OpenAi {
            model: "gpt-5".into(),
            api_key: Some(KEY.into()),
        },
        ProviderSpec::Compatible {
            model: "some-model".into(),
            preset: None,
            base_url: Some("https://gateway.example.test/v1".into()),
            api_key: Some(KEY.into()),
            auth: None,
            name: None,
            reference_prices: false,
        },
    ];

    for spec in specs {
        let json = serde_json::to_string(&spec).unwrap();
        assert!(
            json.contains(KEY),
            "the key an operator typed must survive being written back out: {json}"
        );

        let back: ProviderSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(back, spec, "and must come back equal");
    }
}
