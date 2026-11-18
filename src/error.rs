//! The single error type used across strata.
//!
//! Keeping one enum (instead of one per module) makes the public API easy to
//! use: every fallible call returns `strata::Result<T>`.

/// Everything that can go wrong inside strata.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The operating system reported an I/O failure (disk full, permission, ...).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Bytes on disk did not match what we expect (bad checksum, bad format).
    /// Corruption is always reported and never silently ignored.
    #[error("corruption: {0}")]
    Corruption(String),

    /// The caller passed something we refuse to store (e.g. an empty key).
    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    /// The database was closed; no further operations are allowed.
    #[error("database is closed")]
    Closed,
}

/// Shorthand used by every fallible strata function.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_is_readable() {
        let msg = Error::Corruption("bad crc".into()).to_string();
        assert!(msg.contains("bad crc"), "got: {msg}");
    }
}
