//! Canonical object references, comments and privileges.
//!
//! PostgreSQL stores comments and access privileges against catalog object
//! identities, not against the SQL text that created them. pgdelta models
//! both with a single [`ObjectRef`] key so that a `COMMENT ON`/`GRANT` in a
//! declarative source and the same state read from a live database compare
//! equal. The `COMMENT ON` grammar and its `IS { string | NULL }` form are
//! documented in the PostgreSQL 18 docs,
//! <https://www.postgresql.org/docs/18/sql-comment.html>; `GRANT`/`REVOKE`
//! privilege names follow <https://www.postgresql.org/docs/18/sql-grant.html>.

use std::collections::BTreeSet;

use serde::Serialize;

use crate::naming::quote_parts;

/// The kind of catalog object an [`ObjectRef`] points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum ObjectKind {
    /// A table (including a partitioned table or a partition).
    Table,
    /// A plain view.
    View,
    /// A materialized view.
    MaterializedView,
    /// A column of a table, view or composite type.
    Column,
    /// A standalone sequence.
    Sequence,
    /// A user-defined type (enum, composite or range).
    Type,
    /// A domain.
    Domain,
    /// A schema.
    Schema,
    /// A function (identity includes its input signature).
    Function,
    /// A procedure (identity includes its input signature).
    Procedure,
    /// A standalone index.
    Index,
    /// A trigger on a relation.
    Trigger,
    /// A row-level-security policy on a table.
    Policy,
    /// A constraint on a table or domain.
    Constraint,
}

impl ObjectKind {
    /// The keyword used by `COMMENT ON`/`GRANT ... ON` for this kind.
    #[must_use]
    pub const fn as_sql(self) -> &'static str {
        match self {
            ObjectKind::Table => "TABLE",
            ObjectKind::View => "VIEW",
            ObjectKind::MaterializedView => "MATERIALIZED VIEW",
            ObjectKind::Column => "COLUMN",
            ObjectKind::Sequence => "SEQUENCE",
            ObjectKind::Type => "TYPE",
            ObjectKind::Domain => "DOMAIN",
            ObjectKind::Schema => "SCHEMA",
            ObjectKind::Function => "FUNCTION",
            ObjectKind::Procedure => "PROCEDURE",
            ObjectKind::Index => "INDEX",
            ObjectKind::Trigger => "TRIGGER",
            ObjectKind::Policy => "POLICY",
            ObjectKind::Constraint => "CONSTRAINT",
        }
    }
}

/// A stable identity for an object that can carry a comment or privileges.
///
/// `name` is the fully-resolved catalog name: a dotted name for relations
/// (`"public.users"`), a three-part name for columns and triggers
/// (`"public.users.email"`), and, for routines, a dotted name followed by
/// the parenthesised input-type signature (`"public.add(integer,integer)"`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct ObjectRef {
    /// Object kind.
    pub kind: ObjectKind,
    /// Canonical identity of the object.
    pub name: String,
}

impl ObjectRef {
    /// Builds a reference.
    #[must_use]
    pub fn new(kind: ObjectKind, name: impl Into<String>) -> Self {
        ObjectRef {
            kind,
            name: name.into(),
        }
    }

    /// Renders the object for `COMMENT ON`/`GRANT ... ON`.
    ///
    /// Every identifier part is quoted (the `pg_dump` convention). A
    /// routine's signature carries SQL type names, which are rendered
    /// verbatim after the quoted function name.
    #[must_use]
    pub fn render(&self) -> String {
        if let Some(open) = self.name.find('(') {
            let (base, tail) = self.name.split_at(open);
            format!("{} {}{tail}", self.kind.as_sql(), quote_parts(base))
        } else {
            format!("{} {}", self.kind.as_sql(), quote_parts(&self.name))
        }
    }
}

/// A privilege grant on one object to one grantee.
///
/// PostgreSQL adds privileges rather than replacing them, so the model keeps
/// the accumulated set per `(object, grantee)` pair. `GRANT OPTION` is a
/// property of the whole grant, as in `WITH GRANT OPTION` (GRANT synopsis).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Grant {
    /// The object the privilege applies to.
    pub object: ObjectRef,
    /// Role receiving the privilege (`PUBLIC` is stored literally).
    pub grantee: String,
    /// Sorted, de-duplicated privilege names in canonical upper case.
    pub privileges: BTreeSet<String>,
    /// Whether the privileges were granted `WITH GRANT OPTION`.
    pub with_grant_option: bool,
}

/// Builds a canonical routine identity: `schema.name(type1,type2)`.
///
/// Only input-like argument modes participate in PostgreSQL's function
/// identity (CREATE FUNCTION, "Overloading": OUT parameters are ignored).
#[must_use]
pub fn routine_ref_name(schema: &str, name: &str, input_types: &[String]) -> String {
    format!("{schema}.{name}({})", input_types.join(","))
}
