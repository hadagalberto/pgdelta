//! Standalone sequence objects.
//!
//! `CREATE SEQUENCE` and its options follow the PostgreSQL 18 synopsis at
//! <https://www.postgresql.org/docs/18/sql-createsequence.html>. Options are
//! fully normalised with the documented defaults (increment 1, ascending
//! `MINVALUE` 1, `MAXVALUE` the type maximum, `START` = minvalue, `CACHE`
//! 1, `NO CYCLE`) so that differently-spelled but equivalent sources
//! produce equal models. Identity columns reuse the same
//! [`SequenceOptions`](super::SequenceOptions) shape.

use serde::Serialize;

use super::SequenceOptions;

/// A sequence's `OWNED BY` column, if any.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SequenceOwner {
    /// Schema of the owning table.
    pub schema: String,
    /// Owning table name.
    pub table: String,
    /// Owning column name.
    pub column: String,
}

/// A standalone sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Sequence {
    /// Schema owning the sequence.
    pub schema: String,
    /// Sequence name.
    pub name: String,
    /// Arithmetic type (`smallint`, `integer` or `bigint`).
    pub data_type: String,
    /// Fully-normalised sequence parameters.
    pub options: SequenceOptions,
    /// Column this sequence is `OWNED BY`, if any.
    pub owned_by: Option<SequenceOwner>,
}
