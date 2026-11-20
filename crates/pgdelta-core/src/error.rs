//! Error types for pgdelta-core.

use std::fmt;

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors returned by pgdelta-core.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The SQL text could not be tokenized or parsed.
    #[error("parse error in {origin} (statement {index}):\n  {message}\n  statement: {statement}")]
    Parse {
        /// Name of the source (file path, directory entry, or literal).
        origin: String,
        /// 1-based statement index within the source.
        index: usize,
        /// What went wrong.
        message: String,
        /// Best-effort reconstruction of the offending statement.
        statement: String,
    },

    /// The statement parses, but is not something pgdelta models in this
    /// milestone.
    #[error("unsupported statement in {origin} (statement {index}): {message}\n  statement: {statement}")]
    Unsupported {
        /// Name of the source.
        origin: String,
        /// 1-based statement index within the source.
        index: usize,
        /// Why the statement is rejected and what to do instead.
        message: String,
        /// Best-effort reconstruction of the offending statement.
        statement: String,
    },

    /// The statement is well-formed but describes an invalid or
    /// inconsistent state, mirroring an error PostgreSQL itself would
    /// raise.
    #[error("invalid statement in {origin} (statement {index}): {message}\n  statement: {statement}")]
    Invalid {
        /// Name of the source.
        origin: String,
        /// 1-based statement index within the source.
        index: usize,
        /// What is inconsistent.
        message: String,
        /// Best-effort reconstruction of the offending statement.
        statement: String,
    },

    /// An internal invariant was violated (a bug in pgdelta).
    #[error("internal error: {message}")]
    Internal {
        /// Description of the invariant violation.
        message: String,
    },

    /// An I/O error while reading a source.
    #[error("{context}: {inner}")]
    Io {
        /// What pgdelta was doing when the I/O error happened.
        context: String,
        /// The underlying error.
        inner: std::io::Error,
    },

    /// An error while serializing the model or plan.
    #[error("serialization error: {0}")]
    Serialization(String),
}

impl Error {
    /// Builds an [`Error::Parse`] for the statement at `index` in `origin`.
    pub fn parse(origin: &str, index: usize, message: impl fmt::Display, statement: &str) -> Self {
        Error::Parse {
            origin: origin.to_owned(),
            index,
            message: message.to_string(),
            statement: statement.to_owned(),
        }
    }

    /// Builds an [`Error::Unsupported`] for the statement at `index` in
    /// `origin`.
    pub fn unsupported(
        origin: &str,
        index: usize,
        message: impl fmt::Display,
        statement: &str,
    ) -> Self {
        Error::Unsupported {
            origin: origin.to_owned(),
            index,
            message: message.to_string(),
            statement: statement.to_owned(),
        }
    }

    /// Builds an [`Error::Invalid`] for the statement at `index` in
    /// `origin`.
    pub fn invalid(
        origin: &str,
        index: usize,
        message: impl fmt::Display,
        statement: &str,
    ) -> Self {
        Error::Invalid {
            origin: origin.to_owned(),
            index,
            message: message.to_string(),
            statement: statement.to_owned(),
        }
    }

    /// Builds an [`Error::Internal`] with a message.
    pub fn internal(message: impl fmt::Display) -> Self {
        Error::Internal {
            message: message.to_string(),
        }
    }
}
