//! How a run ended, and the exit code it reports.
//!
//! Shared by both binaries so `hledger-xfmt` and `hledger-xadd` cannot drift
//! apart on what an exit code means.

/// How the run ended. Ordered worst-last: a run that both finds unformatted
/// files and hits an error reports the error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Status {
    /// Everything succeeded.
    #[default]
    Ok,
    /// `--check` found files that need formatting.
    Unformatted,
    /// The invocation itself did not make sense.
    Usage,
    /// A file could not be read, written, or walked.
    Error,
}

impl Status {
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::Unformatted => 1,
            Self::Usage => 2,
            Self::Error => 3,
        }
    }

    /// Keep the worse of the two.
    pub fn merge(&mut self, other: Self) {
        if other > *self {
            *self = other;
        }
    }
}
