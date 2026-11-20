# pgdelta

A single-binary Rust CLI + library that diffs two PostgreSQL schemas and
generates a safe, dependency-ordered migration script. Sources can be `.sql`
files, directories of `.sql` files ("declarative schema"), or live
`postgres://` databases introspected through `pg_catalog` (PostgreSQL 13–18).
A `--shadow` source applies SQL to a throwaway database and introspects it —
the most accurate mode, because PostgreSQL validates and normalises the DDL.

MIT licensed. [Leia em português](README.pt-BR.md).

## Status

Milestone 3: normalised catalog model and diff/plan for schemas, extensions,
tables, columns, constraints, indexes, user-defined types (enum, composite,
domain, range), sequences, views/materialized views, overloaded functions and
procedures, triggers, RLS policies, partitioned tables, comments and grants.
Live introspection reads every modelled object from `pg_catalog` and feeds
deparsed expressions through the SQL parser's own normalisers, so a live
source and a SQL file describing the same schema compare equal (parity).
Plans are dependency-ordered, drop and recreate views on referenced column
type changes, and are covered by 75 insta snapshot/convergence cases plus
live convergence, parity, shadow-mode, known-valid/known-invalid and property
tests.

See [SPEC.md](SPEC.md) for the complete design and
[docs/parser.md](docs/parser.md) for the parser decision and normalisation
rules.

## Usage

```sh
# Generate a migration script
cargo run -p pgdelta -- diff --from schema.sql --to wanted/ --out migration.sql

# JSON plan instead of SQL
cargo run -p pgdelta -- diff --from old.sql --to new.sql --json

# Dump the normalised model of a source
cargo run -p pgdelta -- inspect schema/

# Diff a live database against a declarative schema
cargo run -p pgdelta -- diff --from postgres://user:pass@host/db --to wanted/

# Let PostgreSQL validate and normalise SQL sources in a shadow database
cargo run -p pgdelta -- diff --from old.sql --to new.sql \
  --shadow postgres://user:pass@host/postgres
```

The script has an explicit transactional `BEGIN; ... COMMIT;` block; any
`CREATE INDEX CONCURRENTLY` statements follow in a separate, clearly marked
non-transactional section.

## Development

```sh
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
cargo test --workspace
```

The live integration tests need `PGDELTA_TEST_DATABASE_URL` pointing at a
PostgreSQL 13–18 server. With no URL set they print a skip reason and return;
the CI matrix runs them against `postgres:13` … `postgres:18` service
containers. Locally, Docker works:

```sh
docker run -d --name pgdelta-pg -e POSTGRES_PASSWORD=postgres \
  -p 55432:5432 postgres:17
PGDELTA_TEST_DATABASE_URL=postgres://postgres:postgres@localhost:55432/postgres \
  cargo test --workspace -- --test-threads=1
```

Line endings are LF (`.gitattributes`). The repository is not published to
crates.io.

## Layout

* `crates/pgdelta-core` — model, SQL parsing/applying, diff, planner;
* `crates/pgdelta` — `clap` command line;
* `crates/pgdelta-core/tests/cases` — table-driven `(from.sql, to.sql)`
  fixtures with insta snapshots and a convergence oracle;
* `.github/workflows/ci.yml` — unit, lint and PostgreSQL 13–18 matrix.
