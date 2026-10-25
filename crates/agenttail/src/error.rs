//! The library's one error type: a few plain cases, no boxed causes.

use std::fmt;

/// What can go wrong when you use the library. Reading a transcript never fails: lines that
/// don't parse are skipped and counted (see [`Diagnostics`](crate::Diagnostics)).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// A harness id that isn't one of [`Harness::ALL`](crate::Harness::ALL)'s.
    UnknownHarness { id: String },
    /// A chunk that doesn't carry on from where the reducer is: bytes were lost or sent
    /// twice. Start a new reducer, or resend from [`SessionReducer::received`] (or
    /// [`consumed`](crate::SessionReducer::consumed), or 0).
    ///
    /// [`SessionReducer::received`]: crate::SessionReducer::received
    OffsetMismatch { expected: u64, got: u64 },
    /// Cursor's `state.vscdb` could not be copied, opened read-only, or read.
    #[cfg(feature = "sqlite")]
    Sqlite { path: std::path::PathBuf, message: String },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::UnknownHarness { id } => {
                let ids: Vec<_> = crate::Harness::ALL.iter().map(|h| h.id()).collect();
                write!(f, "no agent \"{id}\" (there are: {})", ids.join(", "))
            }
            Error::OffsetMismatch { expected, got } => write!(f, "expected offset {expected}, got {got}"),
            #[cfg(feature = "sqlite")]
            Error::Sqlite { path, message } => write!(f, "{}: {message}", path.display()),
        }
    }
}

impl std::error::Error for Error {}
