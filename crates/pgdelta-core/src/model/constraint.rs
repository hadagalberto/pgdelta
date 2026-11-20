//! Table constraints: primary keys, unique, check and foreign keys.

use serde::Serialize;

/// A named table constraint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Constraint {
    /// Constraint name. Unnamed source constraints receive PostgreSQL's
    /// auto-generated names (`naming.rs` reproduces
    /// `ChooseConstraintName`/`ChooseIndexName` from the PostgreSQL
    /// sources, cited there).
    pub name: String,
    /// The constraint itself.
    pub kind: ConstraintKind,
    /// Deferrability. `None` means `NOT DEFERRABLE` (the default);
    /// `Some(Deferrable { initially_deferred: true })` models
    /// `DEFERRABLE INITIALLY DEFERRED`, and so on.
    pub deferrable: Option<Deferrable>,
    /// `NOT VALID` flag for `CHECK` and foreign-key constraints
    /// (`ALTER TABLE ... ADD CONSTRAINT ... NOT VALID`; PostgreSQL allows
    /// `NOT VALID` only when adding such constraints, never in
    /// `CREATE TABLE`).
    pub not_valid: bool,
}

/// Deferrability specification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Deferrable {
    /// `DEFERRABLE` (always true when this struct is present;
    /// `NOT DEFERRABLE` collapses to `None`).
    pub deferrable: bool,
    /// `INITIALLY DEFERRED` vs the default `INITIALLY IMMEDIATE`.
    pub initially_deferred: bool,
}

/// The payload of a constraint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum ConstraintKind {
    /// `PRIMARY KEY (columns)`.
    PrimaryKey {
        /// Constrained columns.
        columns: Vec<String>,
    },
    /// `UNIQUE (columns)`.
    Unique {
        /// Constrained columns.
        columns: Vec<String>,
        /// `NULLS NOT DISTINCT` (PostgreSQL 15+). `true` = the default
        /// nulls-distinct behaviour; `false` = `NULLS NOT DISTINCT`.
        nulls_distinct: bool,
    },
    /// `CHECK (expression)`.
    Check {
        /// Normalised check expression.
        expr: String,
    },
    /// `FOREIGN KEY (columns) REFERENCES ref_table (ref_columns) ...`.
    ForeignKey {
        /// Referencing columns.
        columns: Vec<String>,
        /// Referenced table, schema-qualified in the model (unqualified
        /// names resolve to `public`).
        ref_schema: String,
        /// Referenced table name.
        ref_table: String,
        /// Referenced columns. Always concrete: a `REFERENCES reftable`
        /// clause without a column list is resolved to the referenced
        /// table's primary key at apply time, mirroring PostgreSQL
        /// (`CREATE TABLE`, `REFERENCES`: "If the refcolumn list is
        /// omitted, the primary key of the reftable is used").
        ref_columns: Vec<String>,
        /// `ON DELETE` action.
        on_delete: FkAction,
        /// `ON UPDATE` action.
        on_update: FkAction,
        /// `MATCH FULL|PARTIAL|SIMPLE`.
        match_type: FkMatch,
    },
}

/// Foreign-key referential actions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum FkAction {
    /// `NO ACTION` (the default).
    NoAction,
    /// `RESTRICT`.
    Restrict,
    /// `CASCADE`.
    Cascade,
    /// `SET NULL`, optionally restricted to a column list
    /// (PostgreSQL 15+).
    SetNull(Vec<String>),
    /// `SET DEFAULT`, optionally restricted to a column list
    /// (PostgreSQL 15+).
    SetDefault(Vec<String>),
}

/// Foreign-key match types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum FkMatch {
    /// `MATCH SIMPLE` (the default).
    Simple,
    /// `MATCH FULL`.
    Full,
}

impl FkAction {
    /// Renders the action for a plan statement (no leading space).
    pub(crate) fn to_sql(&self) -> String {
        match self {
            FkAction::NoAction => "NO ACTION".to_owned(),
            FkAction::Restrict => "RESTRICT".to_owned(),
            FkAction::Cascade => "CASCADE".to_owned(),
            FkAction::SetNull(cols) => with_columns("SET NULL", cols),
            FkAction::SetDefault(cols) => with_columns("SET DEFAULT", cols),
        }
    }
}

fn with_columns(action: &str, columns: &[String]) -> String {
    if columns.is_empty() {
        action.to_owned()
    } else {
        let cols: Vec<String> = columns.iter().map(|c| crate::naming::quote_ident(c)).collect();
        format!("{action} ({})", cols.join(", "))
    }
}

impl FkMatch {
    /// Renders the match clause for a plan statement.
    pub(crate) fn to_sql(self) -> &'static str {
        match self {
            FkMatch::Simple => "MATCH SIMPLE",
            FkMatch::Full => "MATCH FULL",
        }
    }
}
