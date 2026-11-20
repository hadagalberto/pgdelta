# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
