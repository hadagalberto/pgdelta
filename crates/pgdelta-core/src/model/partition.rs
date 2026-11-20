//! Partitioned tables and partitions.
//!
//! PostgreSQL partitioning (`PARTITION BY { RANGE | LIST | HASH }`,
//! `FOR VALUES ...`, `DEFAULT`) is documented in
//! <https://www.postgresql.org/docs/18/ddl-partitioning.html> and in the
//! `CREATE TABLE` page, <https://www.postgresql.org/docs/18/sql-createtable.html>.
//! The model keeps the partition key (columns or expressions) on the parent
//! table and each partition's bound expression on the child, which is the
//! minimum needed to plan safe creates, attaches and drops.

use serde::Serialize;

/// Partitioning strategy of a parent table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum PartitionStrategy {
    /// `RANGE`.
    Range,
    /// `LIST`.
    List,
    /// `HASH`.
    Hash,
}

impl PartitionStrategy {
    /// Renders the keyword.
    #[must_use]
    pub const fn as_sql(self) -> &'static str {
        match self {
            PartitionStrategy::Range => "RANGE",
            PartitionStrategy::List => "LIST",
            PartitionStrategy::Hash => "HASH",
        }
    }
}

/// One partition key element: a column or an expression.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum PartitionKey {
    /// A plain column name.
    Column(String),
    /// A parenthesised expression, normalised.
    Expression(String),
}

/// The `PARTITION BY` clause of a parent table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Partitioning {
    /// Strategy.
    pub strategy: PartitionStrategy,
    /// Key elements in declaration order.
    pub keys: Vec<PartitionKey>,
}

/// A partition and its bound.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Partition {
    /// Schema of the partition (always the parent's schema in pgdelta).
    pub schema: String,
    /// Partition name.
    pub name: String,
    /// Schema of the partitioned parent.
    pub parent_schema: String,
    /// Parent table name.
    pub parent_table: String,
    /// Bound clause, e.g. `FOR VALUES FROM ('a') TO ('b')`.
    /// Empty for a partition created without an explicit bound.
    pub bound: String,
    /// `DEFAULT` partition.
    pub is_default: bool,
}
