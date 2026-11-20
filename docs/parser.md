# SQL parser decision and normalisation rules (milestone 1)

## Decision: `sqlparser-rs`, not `pg_query`

The SPEC asks for `pg_query` (libpg_query bindings) to be tried first and, if
it cannot be built on Windows MSVC, a fallback to `sqlparser-rs` with the
PostgreSQL dialect, documented. That is what this repository does.

### The `pg_query` attempt (evidence)

The probe lived in `tmp-probe/` (a throwaway crate, deliberately **not** part
of the workspace and not committed). It declared `pg_query = "6.1.0"` and
called `pg_query::parse("CREATE TABLE t (id bigint PRIMARY KEY);")`.

On this development host (`x86_64-pc-windows-msvc`, Rust 1.100.0) the build
fails in `pg_query`'s build script before any pgdelta code is compiled:

```
thread 'main' panicked at .../bindgen-0.66.1/lib.rs:604:31:
Unable to find libclang: "couldn't find any valid shared libraries matching:
['clang.dll', 'libclang.dll'], set the `LIBCLANG_PATH` environment variable
to a path where one of these files can be found (invalid: [])"
  cargo:rustc-link-lib=static=pg_query
```

`pg_query` generates Rust bindings over libpg_query's C headers with
`bindgen`, which requires a `libclang` shared library. The MSVC toolchain
installed here does not provide one. Requiring developers and CI to install
LLVM just to parse SQL would break the "single static binary, builds
everywhere" goal, so the fallback was taken.

### Fallback: `sqlparser-rs` (PostgreSQL dialect)

`pgdelta-core` depends on `sqlparser = "0.59"`. The crate's PostgreSQL
dialect tokenises and parses DDL; pgdelta adds a thin, explicit layer on top:

* `sql/tokens.rs` — statement splitting that is comment-, string- and
  dollar-quote-aware (PostgreSQL docs §4.1.5, "Comments": `--` and nestable
  `/* ... */`; §4.1.2.4, "Dollar-Quoted String Constants"), with UTF-8 BOM
  stripping;
* `sql/datatype.rs`, `sql/expr.rs` — canonical type and expression rendering;
* `sql/create_table.rs`, `sql/alter_table.rs`, `sql/index.rs`, `sql/misc.rs`
  — statement parsers that map the production to the normalised model;
* `apply.rs` — enforces PostgreSQL's semantic rules (no stubs: unknown or
  unsupported forms are rejected with a source-quoted error, never silently
  dropped).

Every normalisation rule below was checked against the official PostgreSQL
18 documentation; nothing was guessed.

## Official sources consulted

All fetched from `postgresql.org` during milestone 1 (PostgreSQL 18 stable
docs, the oldest supported server is 13; the rules cited here are stable
across 13–18 unless marked otherwise):

* §4.1.1 "Identifiers and Key Words" —
  <https://www.postgresql.org/docs/18/sql-syntax-lexical.html#SQL-SYNTAX-IDENTIFIERS>
* `CREATE TABLE` —
  <https://www.postgresql.org/docs/18/sql-createtable.html>
* `CREATE SEQUENCE` —
  <https://www.postgresql.org/docs/18/sql-createsequence.html>
* `CREATE INDEX` (including "Building Indexes Concurrently") —
  <https://www.postgresql.org/docs/18/sql-createindex.html>
* `ALTER TABLE` —
  <https://www.postgresql.org/docs/18/sql-altertable.html>

PostgreSQL C sources for the auto-generated-name algorithms are cited inline
in `crates/pgdelta-core/src/naming.rs` with `REL_18_STABLE` GitHub URLs.

## Normalisation rules implemented

### Identifiers

* Unquoted identifiers fold to lower case; quoted identifiers keep their
  case exactly (§4.1.1: "Quoting an identifier also makes it case-sensitive,
  whereas unquoted names are always folded to lower case").
* Identifiers are truncated to 63 bytes on a UTF-8 character boundary
  (§4.1.1: "The system uses no more than NAMEDATALEN-1 bytes of an
  identifier ... By default, NAMEDATALEN is 64").
* Unqualified relation names resolve to `public`, matching a fresh
  database's default `search_path`.
* Output always quotes identifiers, as `pg_dump` does: always valid, and
  immune to keyword collisions.

### Columns

* `COLLATE` requires a collatable type; omitted collation is `None` in the
  model, meaning "the type's default collation" (`CREATE TABLE`, `COLLATE`).
* `DEFAULT` expressions must be variable-free (`CREATE TABLE`, `DEFAULT`:
  "any variable-free expression ... cross-references to other columns in the
  current table are not allowed").
* Stored generated columns are modelled. The generation expression may refer
  to other base columns, never to itself or to another generated column
  (`CREATE TABLE`, `GENERATED ALWAYS AS`). PostgreSQL 18 added `VIRTUAL`
  generated columns and made it the default; because generated SQL targets
  13–18, sources must spell `STORED` explicitly (13–17 require it anyway).
* Identity columns are restricted to integer types, are implicitly `NOT
  NULL`, and cannot carry a `DEFAULT` (`CREATE TABLE`, `GENERATED ... AS
  IDENTITY`). Sequence options are normalised with the `CREATE SEQUENCE`
  defaults: ascending `MINVALUE` 1, `MAXVALUE` = type maximum, `START` =
  minvalue, `CACHE` 1, `NO CYCLE`.

### Constraints and indexes

* `PRIMARY KEY` implies `NOT NULL`; at most one primary key per table
  (`CREATE TABLE`, `PRIMARY KEY`).
* A `REFERENCES` clause without a column list resolves to the referenced
  table's primary key; an explicit list must match a non-deferrable unique
  or primary-key constraint, or a non-partial unique index (`CREATE TABLE`,
  `REFERENCES`).
* `NULLS NOT DISTINCT` on `UNIQUE` constraints and unique indexes is
  PostgreSQL 15+; cases using it carry a `min_pg_version.txt` gate so the
  live-database suite skips them on 13/14 instead of weakening an assertion.
* `NOT VALID` is accepted only for `CHECK` and foreign-key constraints
  (`ALTER TABLE`, `ADD ... NOT VALID`).
* Indexes model method, key elements (column/expression, collation, opclass
  and parameters, sort/nulls order), `INCLUDE`, `WITH`, `TABLESPACE` and
  partial predicates (`CREATE INDEX`). An index backing a `PRIMARY KEY` or
  `UNIQUE` constraint is *not* a separate model entry — it shares the
  constraint's name and lifetime.
* `CREATE INDEX CONCURRENTLY` cannot run inside a transaction block
  (`CREATE INDEX`, "Building Indexes Concurrently"), so those statements are
  emitted in a separate, clearly marked non-transactional section.

### Ordering and apply semantics

* Drops: constraints → columns → tables → extensions → schemas; a dropped
  table's indexes and constraints die with it.
* Creates: schemas → extensions → tables (non-FK constraints inline) →
  column/constraint alterations → foreign keys (after every referenced
  object exists) → indexes.
* `DROP COLUMN` automatically drops indexes and table constraints involving
  the column (`ALTER TABLE`, `DROP COLUMN`); outside dependents (e.g. FKs
  from other tables) are rejected rather than silently cascaded.
* `SET DATA TYPE ... COLLATE` replaces the collation outright; if omitted,
  the collation is the new type's default (`ALTER TABLE`, `SET DATA TYPE`).
  A type change with a default uses the `pg_dump` pattern: `DROP DEFAULT`,
  `SET DATA TYPE`, `SET DEFAULT` (`ALTER TABLE`, Notes).
* `NOT VALID` → valid is planned as `VALIDATE CONSTRAINT` rather than a
  drop/recreate (`ALTER TABLE`, `VALIDATE CONSTRAINT`).

## Deliberate milestone-1 limits

These were absent from the milestone-1 model and **rejected with a
source-located error** rather than silently ignored: partitioned tables,
views/materialized views, functions/procedures, triggers, policies/RLS,
grants, comments, enum/composite/domain/range types and standalone
sequences. Extension `VERSION` and `CASCADE` clauses parse but are not
modelled (version state is managed with `ALTER EXTENSION UPDATE`).

Renames are never guessed: a rename diffs as a drop plus a create until the
opt-in `--rename` hints from the SPEC are implemented in a later milestone.

## Milestone 2: extended object coverage

Milestone 2 adds the object kinds above to the same normalised catalog, so
all three sources (SQL file, SQL directory, live database) continue to
compare equal. The following official PostgreSQL 18 pages were fetched with
web tools and are cited from the relevant modules:

* `CREATE TYPE` — <https://www.postgresql.org/docs/18/sql-createtype.html>
* `ALTER TYPE` — <https://www.postgresql.org/docs/18/sql-altertype.html>
* `CREATE DOMAIN` — <https://www.postgresql.org/docs/18/sql-createdomain.html>
* `CREATE SEQUENCE` — <https://www.postgresql.org/docs/18/sql-createsequence.html>
* `CREATE VIEW` — <https://www.postgresql.org/docs/18/sql-createview.html>
* `CREATE MATERIALIZED VIEW` — <https://www.postgresql.org/docs/18/sql-creatematerializedview.html>
* `CREATE FUNCTION` (overloads, pseudo-types) — <https://www.postgresql.org/docs/18/sql-createfunction.html>
* `CREATE TRIGGER` — <https://www.postgresql.org/docs/18/sql-createtrigger.html>
* `CREATE POLICY` — <https://www.postgresql.org/docs/18/sql-createpolicy.html>
* `COMMENT` — <https://www.postgresql.org/docs/18/sql-comment.html>
* `GRANT`/`REVOKE` — <https://www.postgresql.org/docs/18/sql-grant.html>
* partitioning — <https://www.postgresql.org/docs/18/sql-createtable.html>,
  <https://www.postgresql.org/docs/18/ddl-partitioning.html>

### Normalisation rules implemented

* **Types.** Enum labels keep their sort order; `ADD VALUE` appends or
  inserts `BEFORE`/`AFTER` a neighbour, `RENAME VALUE` keeps the position.
  Only appends and in-place renames are planned as `ALTER TYPE`; removals or
  reordering drop and recreate the type. Composite attributes are ordered;
  domain `CHECK`/`NOT NULL` constraints carry generated names; range types
  keep all `SUBTYPE`-family options. All named type properties are part of
  the model identity.
* **Sequences.** Options are fully normalised with the documented defaults,
  including negative increments (descending `MINVALUE` = type minimum,
  `MAXVALUE` = -1, `START` = maxvalue). `OWNED BY` is a first-class field.
* **Views.** The defining query and output columns are stored verbatim. A
  view that reads a table whose column type changes is dropped **before**
  the `ALTER COLUMN ... TYPE` and recreated **after** it, because a view's
  stored target list caches the column type.
* **Routines.** Identity is `(schema, name, input signature)`; OUT
  parameters are ignored exactly as in PostgreSQL's overload resolution.
  `CREATE OR REPLACE` updates a body without dropping dependants; a changed
  signature drops and recreates.
* **Triggers/policies.** Constraint triggers must be `AFTER ... FOR EACH
  ROW` and `INSTEAD OF` triggers must be `FOR EACH ROW`; `INSERT` policies
  cannot carry `USING` and `SELECT`/`DELETE` policies cannot carry `WITH
  CHECK` (CREATE POLICY docs). These rules are validated at apply time.
* **Comments/grants.** `''` and `NULL` both remove a comment. Privileges are
  accumulated per `(object, grantee)`; `REVOKE` removes only the named
  privileges, and an empty grant disappears from the model.
* **Partitioning.** A parent stores `PARTITION BY`; children store their
  bound and parent link. `ATTACH PARTITION` validates that an existing
  table's columns match the parent's; `CREATE TABLE ... PARTITION OF`
  materialises the child.

### Still out of scope (explicitly rejected)

Publication membership, exclusion constraints, column storage settings,
base/shell types, role membership grants, `GRANT ... ALL ... IN SCHEMA`,
column-level grants, transition relations in triggers, extension ownership,
and generated-expression rewriting on rename. Unsupported forms are
rejected with a source-located error rather than silently dropped.

## Live introspection and shadow sources (milestone 3)

A `postgres://` source is introspected through `pg_catalog` with
`tokio-postgres` (`src/introspect.rs`). No `information_schema` views and no
`pg_dump` subprocess are used; the queries are valid on PostgreSQL 13–18 and
gate version-specific columns (`indnullsnotdistinct`, the unique-constraint
nulls flag read via its backing index) behind a `SHOW server_version_num`
check.

Parity with a SQL file is achieved by feeding the catalog's deparsed values
through the *same* normalisers used by the SQL parser:

* `format_type(...)` → `sql::datatype::normalize_type_text`;
* `pg_get_expr(...)` → `sql::expr::normalize_expr_text` (with one balanced
  pair of parentheses removed from CHECK expressions, which `pg_get_expr`
  always adds);
* `pg_get_viewdef(...)` → the same token join used for view bodies;
* index expressions and predicates → `normalize_expr_text`.

Catalog columns and deparsing helpers are cited in the module documentation
at the top of `src/introspect.rs` (PostgreSQL 18 catalog and functions-info
reference pages). TLS uses `tokio-postgres-rustls` with the bundled Mozilla
root store when `sslmode=require|verify-ca|verify-full` (or `ssl=true`) is
present in the URL; local/CI service containers use `NoTls`.

`--shadow postgres://...` applies a SQL source statement-by-statement to a
dedicated database (`pgdelta_shadow`) on that server: it is created on first
use and emptied before every load (user schemas and non-default extensions
are dropped). Because PostgreSQL performs its own expression
deparsing, name resolution and constraint inference, this is the most
accurate SQL source mode. The integration tests assert that the shadow model
equals the in-process SQL model for every snapshot case.

The live tests (in `tests/live_postgres.rs`) require
`PGDELTA_TEST_DATABASE_URL`; when it is unset they print a skip reason and
return. CI sets it for a `postgres:13` … `postgres:18` service-container
matrix.
