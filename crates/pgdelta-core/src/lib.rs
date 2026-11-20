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
pub mod apply_objects;
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
    ArgMode, Catalog, CheckOption, Column, CompositeAttribute, Constraint, ConstraintKind,
    Deferrable, DomainConstraint, DomainConstraintKind, Extension, FkAction, FkMatch, Generated,
    Grant, Identity, IdentityKind, Index, IndexElem, IndexElemKind, ObjectKind, ObjectRef,
    Partition, PartitionKey, PartitionStrategy, Partitioning, PgType, PgTypeKind, Policy,
    PolicyCommand, Routine, RoutineArg, RoutineKind, Schema, Security, Sequence, SequenceOptions,
    SequenceOwner, Table, Trigger, TriggerEvent, TriggerLevel, TriggerTiming, View, Volatility,
};
pub use plan::{Plan, PlanSection, PlanStatement, PlanSummary, plan};
pub use source::{SourceSpec, apply_sql_to, catalog_from_sql, load_catalog, split_sql_statements};
