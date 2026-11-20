//! The normalised catalog, plus its schema and extension entries.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use super::index::Index;
use super::table::Table;

/// The schema that exists in every database and is never created or dropped.
pub const PUBLIC_SCHEMA: &str = "public";

/// A schema in the catalog.
///
/// `public` is ambient: it exists in every database and is never created
/// or dropped by generated plans. All other schemas are first-class
/// model entries that the planner creates/drops as needed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Schema {
    /// Schema name, folded per PostgreSQL rules.
    pub name: String,
}

/// An installed extension.
///
/// The model records the extension name and its optional `WITH SCHEMA`
/// target. `VERSION` and `CASCADE` clauses are accepted by the parser but
/// not modelled: version state is managed with `ALTER EXTENSION UPDATE`,
/// which is out of scope for milestone 1 (see `docs/parser.md`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Extension {
    /// Extension name.
    pub name: String,
    /// Schema the extension is installed into (`WITH SCHEMA`), if specified.
    pub schema: Option<String>,
}

/// The normalised in-memory catalog model shared by all sources.
///
/// Lookups are keyed by `(schema, name)` pairs; iteration order follows the
/// `BTreeMap` ordering, which keeps every downstream output (plans, JSON
/// dumps, snapshots) deterministic.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalog {
    /// All schemas referenced by the model (explicitly created, referenced
    /// by objects, or the ambient `public`).
    pub schemas: BTreeMap<String, Schema>,
    /// Installed extensions, by name.
    pub extensions: BTreeMap<String, Extension>,
    /// Tables, keyed by `(schema, table)`.
    pub tables: BTreeMap<(String, String), Table>,
    /// Standalone indexes, keyed by `(schema, index)`.
    pub indexes: BTreeMap<(String, String), Index>,
}

impl Catalog {
    /// Creates an empty catalog.
    #[must_use]
    pub fn new() -> Self {
        Catalog::default()
    }

    /// Registers a schema entry if it is not present yet. Used when a
    /// statement references a schema that was never explicitly created.
    pub fn ensure_schema(&mut self, schema: &str) {
        self.schemas.entry(schema.to_owned()).or_insert(Schema {
            name: schema.to_owned(),
        });
    }

    /// Returns the table `(schema, name)`, if present.
    #[must_use]
    pub fn table(&self, schema: &str, name: &str) -> Option<&Table> {
        self.tables.get(&(schema.to_owned(), name.to_owned()))
    }

    /// Returns a mutable reference to the table `(schema, name)`, if present.
    pub fn table_mut(&mut self, schema: &str, name: &str) -> Option<&mut Table> {
        self.tables.get_mut(&(schema.to_owned(), name.to_owned()))
    }

    /// Returns the index `(schema, name)`, if present.
    #[must_use]
    pub fn index(&self, schema: &str, name: &str) -> Option<&Index> {
        self.indexes.get(&(schema.to_owned(), name.to_owned()))
    }

    /// Returns every relation name (table or index) currently in `schema`.
    #[must_use]
    pub fn relation_names(&self, schema: &str) -> Vec<String> {
        let mut names: Vec<String> = self
            .tables
            .keys()
            .filter(|(s, _)| s == schema)
            .map(|(_, n)| n.clone())
            .collect();
        names.extend(
            self.indexes
                .keys()
                .filter(|(s, _)| s == schema)
                .map(|(_, n)| n.clone()),
        );
        names
    }

    /// Returns every constraint name currently in `schema` (PostgreSQL
    /// auto-generated constraint names are unique per schema namespace, and
    /// `ChooseConstraintName` checks them schema-wide — see `naming.rs`).
    #[must_use]
    pub fn constraint_names(&self, schema: &str) -> Vec<String> {
        self.tables
            .iter()
            .filter(|((s, _), _)| s == schema)
            .flat_map(|(_, t)| t.constraints.iter().map(|c| c.name.clone()))
            .collect()
    }

    /// Serialises the catalog to a deterministic JSON document (used by
    /// `pgdelta inspect`).
    #[must_use]
    pub fn to_json(&self) -> Value {
        fn schemas(c: &Catalog) -> Value {
            serde_json::json!(c
                .schemas
                .values()
                .map(|s| &s.name)
                .collect::<Vec<_>>())
        }
        fn extensions(c: &Catalog) -> Value {
            serde_json::json!(c.extensions.values().collect::<Vec<_>>())
        }
        fn tables(c: &Catalog) -> Value {
            serde_json::json!(c.tables.values().collect::<Vec<_>>())
        }
        fn indexes(c: &Catalog) -> Value {
            serde_json::json!(c.indexes.values().collect::<Vec<_>>())
        }
        serde_json::json!({
            "schemas": schemas(self),
            "extensions": extensions(self),
            "tables": tables(self),
            "indexes": indexes(self),
        })
    }
}
