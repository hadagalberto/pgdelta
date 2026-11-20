# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0] — milestone 4

### Added

- Safety analysis for every planned and linted statement: destructive hazards
  (`DROP TABLE/SCHEMA/COLUMN/CONSTRAINT`, type narrowing) and locking hazards
  (`ALTER COLUMN ... TYPE` rewrites, `SET NOT NULL` without a validated check,
  `ADD CONSTRAINT` without `NOT VALID`, standard index builds, concurrent
  index builds inside a transaction). Rules and official PostgreSQL citations
  live in `docs/safety.md` and in `src/safety.rs`.
- `--online`: rewrites `CHECK`/`FOREIGN KEY` additions to `NOT VALID` +
  `VALIDATE CONSTRAINT`, wraps `SET NOT NULL` in a validated-check sequence
  (with the helper constraint dropped afterwards so the schema converges), and
  moves `CREATE INDEX CONCURRENTLY` to the non-transactional section
  (partitioned-table indexes are exempt — concurrent builds are unsupported).
- `pgdelta lint migration.sql` for handwritten migrations, with human and
  `--json` output using the same analyser as generated plans.
- Opt-in `--rename table.old=new` / `--rename column.table.old=new` hints
  (also `index`, `view`, `type`, `sequence`): emitted as `ALTER ... RENAME`
  before the rest of the plan, never guessed silently, and validated against
  both catalogs.
- Colored human hazard summary on stderr with `--color auto|always|never`
  (`auto` honours `NO_COLOR`); JSON plans now carry per-statement hazards and
  per-category summary counts.
- `--fail-on-hazard` returns exit code 2 for `diff` and `lint`.
- Property test for online-plan convergence, plus milestone-4 integration and
  CLI tests covering known-valid/known-invalid vectors, JSON, colour, rename
  hints and exit codes.

### Fixed

- `ALTER TABLE ... RENAME TO` in the in-memory applier now propagates the new
  name to foreign keys, indexes, triggers, policies, owned sequences,
  partitions, view queries and comments/grants, matching PostgreSQL's OID
  references.
- Hazard keyword matching handles both `CHECK (` and the tokeniser's
  canonical `CHECK(` form, and ignores SQL comments and string literals.

## [0.3.0] — milestone 3

### Added

- Live source support: `postgres://` sources are introspected from
  `pg_catalog` with `tokio-postgres` (rustls for TLS connections), covering
  every modelled object kind on PostgreSQL 13–18.
- `--shadow postgres://...` for `diff` and `inspect`: SQL sources are applied
  to a dedicated shadow database (created on first use, emptied on every
  load) and introspected, which is the SPEC's most accurate source mode.
- Live convergence tests for all 75 snapshot cases: apply `from`, run the
  generated migration, introspect, and verify the diff against the introspected
  `to` is empty. Parity tests assert the SQL-file model equals the live model;
  shadow-mode tests assert the shadow model matches the SQL-file model. No
  `pg_dump` binary is required. Tests skip with a printed reason when
  `PGDELTA_TEST_DATABASE_URL` is unset.

### Fixed

- `ALTER COLUMN ... TYPE` now re-applies a column default *after* the type
  change (the lexicographic statement key previously put `SET DEFAULT` first,
  leaving a stale cast on the live column).
- Identity drops are ordered before `DROP NOT NULL`, as PostgreSQL requires.
- `CREATE INDEX ... USING <method> (<columns>)` now includes the required space
  before the column list.

## [0.2.0] — milestone 2

### Added

- Model, parser, diff and planner coverage for enum (including `ADD VALUE`),
  composite, domain and range types, standalone sequences, plain and
  materialized views, overloaded functions/procedures, triggers, RLS
  policies, partitioned tables, comments and grants.
- Full catalog dependency graph: views depending on a changed column are
  dropped before `ALTER COLUMN ... TYPE` and recreated afterwards.
- Known-valid and known-invalid vector tests for every new object kind; 30
  new snapshot/convergence cases (75 total).
- Parser normalisation rules and official PostgreSQL documentation citations
  in `docs/parser.md`.

### Fixed

- Dropping a table with a self-referencing foreign key no longer reports a
  dependency error (PostgreSQL drops the constraint with the table).

## [0.1.0] — milestone 1

### Added

- Cargo workspace with `pgdelta-core` (library) and `pgdelta` (`clap` CLI).
- Normalised catalog model: schemas, extensions, tables, columns (defaults,
  generated, identity, collation), constraints and indexes.
- SQL file and SQL directory sources, applied in statement order to the model.
- Diff and dependency-ordered migration plans, with transactional and
  non-transactional sections.
- `pgdelta diff` (SQL/JSON output) and `pgdelta inspect` subcommands.
- Parser decision (`sqlparser-rs` fallback, with the reasons recorded in
  `docs/parser.md`).
- Table-driven insta snapshot fixtures, property tests and a live-database
  convergence suite wired into the PostgreSQL 13–18 CI matrix.
- MIT license, bilingual READMEs, contributing guide, `.gitattributes` (LF).
