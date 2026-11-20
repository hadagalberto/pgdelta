//! pgdelta-core: PostgreSQL schema model, SQL sources, diff and migration planning.
//!
//! This crate implements the milestone-1 scope of the `pgdelta` SPEC:
//!
//! * a normalised in-memory catalog model ([`Catalog`]) shared by all sources;
//! * SQL-file and SQL-directory sources that are applied to the catalog model
//!   by parsing DDL statements and applying `ALTER`s in order;
//! * diff + plan for schemas, extensions, tables, columns (defaults,
//!   generated columns, identity, collation), constraints and indexes, with
//!   dependency-ordered output.
//!
//! The SQL parser decision (sqlparser-rs with a PostgreSQL-specific extension
//! layer, rather than `pg_query`) is documented in `docs/parser.md` at the
//! repository root.

pub mod apply;
pub mod dep;
pub mod diff;
pub mod error;
pub mod model;
pub mod naming;
pub mod plan;
pub mod source;
pub mod sql;

pub use error::{Error, Result};
pub use model::{
    Catalog, Column, Constraint, ConstraintKind, Extension, FkAction, FkMatch, Index, IndexElem,
    IndexElemKind, Schema, Table,
};
pub use plan::{Plan, PlanStatement, plan};
pub use source::{SourceSpec, load_catalog};
