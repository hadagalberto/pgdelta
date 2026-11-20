# pgdelta

A single-binary Rust CLI + library that diffs two PostgreSQL schemas and
generates a safe, dependency-ordered migration script. Sources are `.sql`
files or directories of `.sql` files ("declarative schema"); live database
introspection is planned for a later milestone.

MIT licensed. [Leia em português](README.pt-BR.md).

## Status

Milestone 2: normalised catalog model and diff/plan for schemas, extensions,
tables, columns, constraints, indexes, user-defined types (enum, composite,
domain, range), sequences, views/materialized views, overloaded functions and
procedures, triggers, RLS policies, partitioned tables, comments and grants.
Plans are dependency-ordered, drop and recreate views on referenced column
type changes, and are covered by 75 insta snapshot/convergence cases plus
known-valid/known-invalid and property tests.

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

Line endings are LF (`.gitattributes`). The repository is not published to
crates.io.

## Layout

* `crates/pgdelta-core` — model, SQL parsing/applying, diff, planner;
* `crates/pgdelta` — `clap` command line;
* `crates/pgdelta-core/tests/cases` — table-driven `(from.sql, to.sql)`
  fixtures with insta snapshots and a convergence oracle;
* `.github/workflows/ci.yml` — unit, lint and PostgreSQL 13–18 matrix.
