//! The verification layer: deterministic checks that confirm the file meets the spec.
//!
//! v0.1 keeps this deterministic on purpose — a model-judged verifier can pass a
//! file that does not meet the spec. Substring and exact-match are checkable and
//! cannot lie.

/// How the harness decides a task is done.
#[derive(Debug, Clone)]
pub enum Verification {
    /// The file's contents must contain this text.
    FileContains(String),
    /// The file's contents must equal this text exactly.
    FileEquals(String),
}

impl Verification {
    /// Check the given file contents against the criterion.
    pub fn check(&self, contents: &str) -> bool {
        match self {
            Verification::FileContains(needle) => contents.contains(needle),
            Verification::FileEquals(expected) => contents == expected,
        }
    }

    /// Human-readable description fed to the model as the success criterion.
    pub fn describe(&self) -> String {
        match self {
            Verification::FileContains(needle) => {
                format!("the file must contain exactly this text: {needle:?}")
            }
            Verification::FileEquals(expected) => {
                format!("the file's entire contents must equal exactly: {expected:?}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_passes_and_fails() {
        let v = Verification::FileContains("fn hello".into());
        assert!(v.check("pub fn hello() {}"));
        assert!(!v.check("pub fn world() {}"));
    }

    #[test]
    fn equals_is_exact() {
        let v = Verification::FileEquals("a".into());
        assert!(v.check("a"));
        assert!(!v.check("a "));
    }
}
