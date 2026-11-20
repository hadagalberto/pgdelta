//! User-defined types: enums, composites, domains and ranges.
//!
//! The four forms modelled here are the PostgreSQL `CREATE TYPE` forms for
//! composite, enum and range types and the separate `CREATE DOMAIN`
//! statement. Base/shell types (which require C support functions) are
//! deliberately not modelled. See the PostgreSQL 18 documentation:
//!
//! * <https://www.postgresql.org/docs/18/sql-createtype.html>
//! * <https://www.postgresql.org/docs/18/sql-altertype.html>
//! * <https://www.postgresql.org/docs/18/sql-createdomain.html>

use serde::Serialize;

/// One attribute of a composite type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompositeAttribute {
    /// Attribute name.
    pub name: String,
    /// Canonical attribute type.
    pub data_type: String,
    /// Explicit collation, if any.
    pub collation: Option<String>,
}

/// The payload of a domain constraint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum DomainConstraintKind {
    /// `NOT NULL`.
    NotNull,
    /// `CHECK (expression)`; the expression uses `VALUE` for the input.
    Check {
        /// Normalised check expression.
        expr: String,
    },
}

/// A named domain constraint (unnamed constraints receive generated names).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DomainConstraint {
    /// Constraint name.
    pub name: String,
    /// Constraint body.
    pub kind: DomainConstraintKind,
}

/// The body of a user-defined type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum PgTypeKind {
    /// `CREATE TYPE name AS ENUM ('label', ...)`.
    Enum {
        /// Enum labels in sort order (PostgreSQL docs, `CREATE TYPE`,
        /// "Enumerated Types": `ADD VALUE` may insert before/after a
        /// neighbour, so order is part of the model).
        values: Vec<String>,
    },
    /// `CREATE TYPE name AS (attribute type, ...)`.
    Composite {
        /// Attributes in declaration order.
        attributes: Vec<CompositeAttribute>,
    },
    /// `CREATE DOMAIN name AS type [DEFAULT ...] [constraints]`.
    Domain {
        /// Underlying (base) type, canonical string.
        base_type: String,
        /// Explicit collation, if any.
        collation: Option<String>,
        /// Domain default expression, if any.
        default: Option<String>,
        /// Domain constraints, sorted by name in the model.
        constraints: Vec<DomainConstraint>,
    },
    /// `CREATE TYPE name AS RANGE (SUBTYPE = ..., ...)`.
    Range {
        /// Range element type.
        subtype: String,
        /// Non-default b-tree operator class for the subtype.
        subtype_opclass: Option<String>,
        /// Collation used in the range's ordering.
        collation: Option<String>,
        /// Canonicalisation function name.
        canonical: Option<String>,
        /// Subtype difference function name.
        subtype_diff: Option<String>,
        /// Name of the automatically created multirange type.
        multirange_type_name: Option<String>,
    },
}

impl PgTypeKind {
    /// Short name used in diagnostics and plan object labels.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            PgTypeKind::Enum { .. } => "enum",
            PgTypeKind::Composite { .. } => "composite type",
            PgTypeKind::Domain { .. } => "domain",
            PgTypeKind::Range { .. } => "range type",
        }
    }
}

/// A user-defined type in the normalised catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PgType {
    /// Schema owning the type.
    pub schema: String,
    /// Type name.
    pub name: String,
    /// Type form and its properties.
    pub kind: PgTypeKind,
}
