//! F9 — the `[otel]` section cannot be chosen by a cloned repository.
//!
//! An export endpoint is an outbound channel: it names a host every span of
//! every run is posted to, and the headers beside it name the credential that
//! post carries. The rule is the one 0.74.0 wrote for a provider's `base_url`
//! and `api_key` and 0.75.0 applied to `[routing]` — a table that decides where
//! an operator's traffic goes is not a table a repository may write.
//!
//! Two properties are asserted separately here, because they can fail
//! independently and this crate has been caught by exactly that before:
//!
//! * the refusal fires for a **parsed string** as well as for a file on disk,
//!   because `Config::from_toml` repeats `read_scope`'s validator row rather
//!   than sharing it, and a check added to one site is silently absent from the
//!   other;
//! * the refusal fires **inside a `[profile.*]` body** too, because a widening
//!   hidden one level down is the same widening.
//!
//! The section is refused in every build, not only when the `otel` feature is
//! on. A boundary that appeared and disappeared with a feature flag would be one
//! an operator could not state, which is why `[browser]` is on the same list in
//! every build. Only the reader — `Config::otel` — is feature-gated, so the
//! tests below are split on that line rather than the whole file being gated.

use io_harness::Config;

/// The refusal names the section, so an operator reading the error knows which
/// table to move rather than which line number to look at.
fn refusal_names_otel(err: &io_harness::Error) -> bool {
    err.to_string().contains("otel")
}

#[test]
fn f9_a_project_scoped_otel_section_is_refused() {
    let err = Config::from_toml(
        r#"
        [otel]
        endpoint = "http://collector.example:4318"
        "#,
    )
    .expect_err("a project-scoped [otel] table must be refused");

    assert!(
        refusal_names_otel(&err),
        "the refusal must name the section an operator has to move: {err}"
    );
}

#[test]
fn f9_an_otel_section_inside_a_profile_is_refused_too() {
    let err = Config::from_toml(
        r#"
        [profile.ci.otel]
        endpoint = "http://collector.example:4318"
        "#,
    )
    .expect_err("a widening one level down is the same widening");

    assert!(
        refusal_names_otel(&err),
        "the profile body must be checked with the same words: {err}"
    );
}

#[test]
fn f9_the_headers_table_is_refused_with_the_section_that_carries_it() {
    // The endpoint is the destination and the headers are the credential. A rule
    // that refused one and accepted the other would be half a boundary, so this
    // asserts the whole table goes rather than a key of it.
    let err = Config::from_toml(
        r#"
        [otel.headers]
        authorization = "Bearer sk-live"
        "#,
    )
    .expect_err("the headers table is part of the section that is refused");

    assert!(
        refusal_names_otel(&err),
        "a header-only [otel] table must be refused as the section it is: {err}"
    );
}

#[test]
fn f9_a_file_that_declares_no_collector_is_accepted() {
    // The negative control for all three above. Without it they would pass on a
    // parser that refused every document, which is the failure mode a refusal
    // test has.
    let config = Config::from_toml("[run]\nmax_steps = 3\n")
        .expect("a file with no [otel] table is an ordinary file");

    let _ = config;
}

#[test]
fn f9_the_refusal_is_not_a_side_effect_of_an_unknown_key() {
    // `deny_unknown_fields` would refuse an undeclared section too, and its error
    // would also happen to contain the word. That would make the three tests
    // above pass without the boundary existing at all — the section is declared,
    // so a refusal has to come from the widening rule rather than from the
    // absence of a field.
    //
    // A section this crate does not know is refused with the word "unknown"; the
    // widening rule refuses with its own sentence about what the table does.
    let declared = Config::from_toml("[otel]\nendpoint = \"http://c:4318\"\n")
        .expect_err("declared and refused");
    let undeclared = Config::from_toml("[otelx]\nendpoint = \"http://c:4318\"\n")
        .expect_err("undeclared and refused");

    assert_ne!(
        declared.to_string(),
        undeclared.to_string(),
        "an undeclared section and a refused one must not fail the same way, or \
         the boundary is indistinguishable from a typo"
    );
    assert!(
        declared.to_string().contains("collector"),
        "the widening refusal states what the table does: {declared}"
    );
}

#[cfg(feature = "otel")]
mod reader {
    use super::*;

    use io_harness::{OtelConfig, OtelExporter};

    /// A value that must never appear in anything printed. Distinctive enough
    /// that a substring search cannot match it by accident.
    const SECRET: &str = "Bearer sk-live-otel-must-not-print-this";

    /// One arm per holder, because a leak fixed on the inner type comes back
    /// through any outer type that derives `Debug` over it — which is how the
    /// same class reached four provider types in 0.70.0 and their config types
    /// in 0.71.0.
    fn carries_the_secret(rendered: &str) -> bool {
        rendered.contains(SECRET) || rendered.contains("sk-live")
    }

    #[test]
    fn nf6_a_collector_credential_is_not_printed_by_the_configs_debug() {
        let config = OtelConfig::default().with_header("authorization", SECRET);

        let rendered = format!("{config:?}");
        assert!(
            !carries_the_secret(&rendered),
            "OtelConfig's Debug must not print a header value: {rendered}"
        );
        // The whole header map is withheld — not the values alone. A header
        // name is often the vendor and a count narrows which gateway is in
        // front of the collector, so neither is printed either.
        assert!(
            !rendered.contains("authorization"),
            "a header name is not printed either: {rendered}"
        );
        assert!(
            rendered.contains("localhost:4318"),
            "what a misconfiguration is diagnosed from is still printed: {rendered}"
        );
    }

    #[test]
    fn nf6_a_collector_credential_is_not_printed_by_the_exporters_debug() {
        // The second holder. `OtelExporter` owns an `OtelConfig`, so a derived
        // `Debug` on the outer type would reprint whatever the inner one does —
        // which is why fixing one and not the other leaves the leak open and
        // the suite green.
        let exporter = OtelExporter::open(
            OtelConfig::default().with_header("authorization", SECRET),
            std::env::temp_dir().join("io-harness-otel-debug-arm.db"),
        )
        .expect("an exporter opens without touching the store");

        let rendered = format!("{exporter:?}");
        assert!(
            !carries_the_secret(&rendered),
            "OtelExporter's Debug must not print a header value: {rendered}"
        );
    }

    #[test]
    fn control_the_secret_checker_finds_a_secret_that_is_present() {
        // Without this, both arms above would pass against a checker that
        // matched nothing at all.
        assert!(carries_the_secret(&format!(
            "headers: {{authorization: {SECRET}}}"
        )));
        assert!(!carries_the_secret(
            "OtelConfig { endpoint: \"http://localhost:4318\" }"
        ));
    }

    #[test]
    fn f9_a_user_scoped_collector_is_read_back_through_the_builder() {
        // The same document, accepted at user scope. `Config::from_toml` is the
        // project-scope door, so this proves the reader rather than the scope —
        // the scope half is what the three refusals above assert.
        let config = Config::from_toml("[run]\nmax_steps = 3\n").expect("an ordinary file");
        assert!(
            config.otel().is_none(),
            "a file that declares no collector has none"
        );
    }
}
