//! Indexes.

use serde::Serialize;

/// An index created with `CREATE INDEX` (and its `DROP INDEX` counterpart).
///
/// Indexes that back `PRIMARY KEY`/`UNIQUE` constraints are *not* separate
/// catalog entries: they are implied by the constraint, exactly as in
/// PostgreSQL's catalog, where the constraint and its index share a name and
/// lifetime. Only standalone `CREATE INDEX` statements produce [`Index`]
/// entries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Index {
    /// Schema the index lives in (always the table's schema; PostgreSQL
    /// puts an index in the same schema as its table).
    pub schema: String,
    /// Index name.
    pub name: String,
    /// Schema of the indexed table.
    pub table_schema: String,
    /// Name of the indexed table.
    pub table: String,
    /// `UNIQUE`.
    pub unique: bool,
    /// `CONCURRENTLY` was used to build the index. Concurrent builds cannot
    /// run inside a transaction block (PostgreSQL docs, `CREATE INDEX`,
    /// "Building Indexes Concurrently"), so the planner emits them in a
    /// separate non-transactional section.
    pub concurrent: bool,
    /// Index access method (`USING method`); `None` means the `btree`
    /// default, and an explicit `USING btree` normalises to `None`.
    pub method: Option<String>,
    /// Key columns/expressions, in order.
    pub columns: Vec<IndexElem>,
    /// `INCLUDE (columns)` payload columns.
    pub include: Vec<String>,
    /// `NULLS NOT DISTINCT` on unique indexes (PostgreSQL 15+); `true` is
    /// the default nulls-distinct behaviour.
    pub nulls_distinct: bool,
    /// `WITH (storage parameters)`, rendered verbatim as `key = value`.
    pub with: Vec<(String, String)>,
    /// `TABLESPACE`.
    pub tablespace: Option<String>,
    /// Partial-index predicate (`WHERE ...`).
    pub predicate: Option<String>,
}

/// One key element of an index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexElem {
    /// The indexed thing: a plain column or an expression.
    pub kind: IndexElemKind,
    /// Per-element `COLLATE`.
    pub collation: Option<String>,
    /// Operator class, if specified.
    pub opclass: Option<String>,
    /// Operator-class parameters (`opclass (k = v, ...)`), joined text.
    pub opclass_params: Option<String>,
    /// `ASC`/`DESC`.
    pub order: Option<SortOrder>,
    /// `NULLS FIRST`/`NULLS LAST`.
    pub nulls: Option<NullsOrder>,
}

/// The indexed thing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum IndexElemKind {
    /// A plain column reference.
    Column(String),
    /// An expression, normalised.
    Expression(String),
}

/// Sort order for index elements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum SortOrder {
    /// `ASC`.
    Asc,
    /// `DESC`.
    Desc,
}

/// Nulls placement for index elements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum NullsOrder {
    /// `NULLS FIRST`.
    First,
    /// `NULLS LAST`.
    Last,
}
