//! Normalised in-memory catalog model.
//!
//! Every source (SQL file, SQL directory, and later a live database) resolves
//! into the same [`Catalog`] shape, and all diffing happens on the model.
//!
//! Normalisation rules (documented per field below):
//!
//! * unquoted identifiers are folded to lower case, quoted identifiers keep
//!   their case exactly — PostgreSQL folds unquoted names to lower case
//!   (PostgreSQL docs §4.1.1, "Identifiers and Key Words");
//! * unqualified table/index names resolve to the `public` schema, matching
//!   the default `search_path` behaviour of a fresh database;
//! * the model always stores fully-resolved state: identity columns carry
//!   normalised sequence options with PostgreSQL's defaults filled in
//!   (see [`Column::identity`]), and a `REFERENCES` clause without an
//!   explicit column list is resolved to the referenced table's primary key
//!   (PostgreSQL docs, `CREATE TABLE`, `REFERENCES`: "If the refcolumn list
//!   is omitted, the primary key of the reftable is used").

pub mod catalog;
pub mod constraint;
pub mod index;
pub mod table;

pub use catalog::{Catalog, Extension, PUBLIC_SCHEMA, Schema};
pub use constraint::{Constraint, ConstraintKind, Deferrable, FkAction, FkMatch};
pub use index::{Index, IndexElem, IndexElemKind, NullsOrder, SortOrder};
pub(crate) use table::sequence_type_range;
pub use table::{Column, Generated, Identity, IdentityKind, SequenceOptions, Table};
