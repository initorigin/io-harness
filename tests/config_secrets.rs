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

const KEY_VAR: &str = "IO_HARNESS_SECRETS_TEST_KEY";
const TOKEN_VAR: &str = "IO_HARNESS_SECRETS_TEST_TOKEN";

/// Hold the environment, and point the user scope at somewhere empty so a config
/// file on the developer's own machine cannot change what this measures.
fn env<'a>(user_dir: &Path) -> MutexGuard<'a, ()> {
    let guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("IO_CONFIG_HOME", user_dir);
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
        for secret in [KEY, TOKEN] {
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

/// A discovered config whose provider key and MCP header are both substituted.
///
/// Discovered rather than parsed, so `sources`, `origins` and the merged `raw`
/// table are all populated — the fields a derived `Debug` would have printed.
fn loaded(project: &Path) -> Config {
    std::fs::write(
        project.join("io.toml"),
        format!(
            "[[provider]]\nkind = \"anthropic\"\nmodel = \"claude-sonnet-4\"\n\
             api_key = \"${{env:{KEY_VAR}}}\"\n\n\
             [[mcp]]\nid = \"sentinel-server\"\ntransport = \"http\"\n\
             url = \"https://mcp.example.test/v1\"\n[mcp.headers]\n\
             Authorization = \"Bearer ${{env:{TOKEN_VAR}}}\"\n"
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
    std::env::set_var(KEY_VAR, KEY);
    std::env::set_var(TOKEN_VAR, TOKEN);

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
            // The raw table is rendered as its shape. Key names and value kinds
            // stay, which is what makes the rendering worth reading; the leaf a
            // `${env:}` filled is gone.
            "\"api_key\": string",
        ],
    );
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
    let spec = ProviderSpec::Anthropic {
        model: "claude-sonnet-4".into(),
        api_key: Some(KEY.into()),
    };

    let json = serde_json::to_string(&spec).unwrap();
    assert!(
        json.contains(KEY),
        "the key an operator typed must survive being written back out: {json}"
    );

    let back: ProviderSpec = serde_json::from_str(&json).unwrap();
    assert_eq!(back, spec, "and must come back equal");
}
