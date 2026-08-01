//! The OpenAI-compatible provider (0.29.0).
//!
//! Placeholder: filled by US-IO-HARNESS-0.29.0-T03.

/// How a request authenticates.
///
/// ```
/// # use io_harness::Auth;
/// let _ = Auth::None;
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Auth {
    /// `Authorization: Bearer <key>`.
    Bearer,
    /// No credential at all.
    None,
}

/// An OpenAI-compatible provider.
///
/// ```
/// # use io_harness::Compatible;
/// let _ = Compatible::default();
/// ```
#[derive(Debug, Clone, Default)]
pub struct Compatible;
