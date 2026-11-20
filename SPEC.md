# pgdelta — SPEC

**pgdelta**: a fast single-binary Rust CLI + library that diffs two PostgreSQL schemas and generates a safe, ordered migration script.
Sources can be a live database (`postgres://...`), a `pg_dump --schema-only` SQL file, or a directory of `.sql` files ("declarative schema").
Think `migra`/`atlas schema diff`, but a single static binary with first-class safety analysis. Open source, MIT, "Hadagalberto Junior".
NOT a toy: no stubs, no `todo!()`, `unimplemented!()`, ignored or placeholder tests.

## Stack
Rust stable, workspace `pgdelta-core` (lib) + `pgdelta` (CLI, clap derive). SQL parsing with `pg_query` (libpg_query bindings) is allowed; if it does not build on Windows MSVC, use `sqlparser-rs` with the PostgreSQL dialect and document the limits.
Live introspection with `tokio-postgres` (+ rustls) querying `pg_catalog` (not information_schema) for PostgreSQL 13–18.
clippy -D warnings, rustfmt, `.gitattributes` `* text=auto eol=lf`, proptest, insta snapshots. CI ubuntu/windows/macos for unit tests + an ubuntu job matrix with `postgres:13..18` service containers for integration tests.
README.md + README.pt-BR.md, CHANGELOG, CONTRIBUTING, LICENSE. Do not publish to crates.io.

## Object coverage
Schemas, extensions, types (enum add/rename value, composite, domain with constraints, range), sequences (ownership, identity columns), tables (columns, defaults, generated columns, NOT NULL, collation, storage), constraints (PK, unique, check, FK with deferrable/on delete, exclusion), indexes (partial, expression, INCLUDE, method, `CONCURRENTLY` option), partitioned tables and partitions, views and materialized views (with dependency ordering), functions/procedures (overloads by signature), triggers, policies (RLS) and `ENABLE ROW LEVEL SECURITY`, grants/privileges (optional), comments, publication membership (optional).

## Diff and plan
- Normalised in-memory catalog model shared by all three sources; SQL-file sources are applied to a throwaway in-memory model (parse DDL statements, apply ALTERs in order) — and, when `--shadow postgres://...` is given, by executing into a shadow database and introspecting (most accurate mode).
- Dependency graph (views → tables/functions, FKs, sequences, types) with topological ordering for drops and creates; drop-and-recreate of dependent views when a column type changes.
- Rename detection heuristics as opt-in hints (`--rename table.a=b`) — never guessed silently.
- Safety analysis per statement: destructive (drop column/table, type narrowing), locking (ACCESS EXCLUSIVE rewrites, non-concurrent index on large table, adding NOT NULL without a validated CHECK, FK without NOT VALID), with an "online" mode that rewrites plans into safe sequences (CHECK NOT VALID → VALIDATE → SET NOT NULL; FK NOT VALID → VALIDATE; CREATE INDEX CONCURRENTLY outside transaction).
- Output: SQL script (transactional sections and non-transactional sections clearly separated), JSON plan, human summary with colored hazard markers. Exit code 2 when hazards exist and `--fail-on-hazard`.

## CLI
`pgdelta diff --from <src> --to <src> [-o migration.sql] [--online] [--json]`, `pgdelta inspect <src>` (dump normalised model), `pgdelta lint migration.sql` (hazard analysis of a handwritten migration).

## Tests
- Hundreds of table-driven cases: (from.sql, to.sql) → expected migration snapshot (insta), covering every object kind and dependency ordering.
- Convergence integration test (CI with Postgres): apply `from`, run generated migration, introspect, assert the result's diff against `to` is empty — for every case, on every Postgres version in the matrix.
- Property test: random schema generator → diff(a, a) is empty; diff(a, b) applied to a converges to b.
- Parser/introspection parity: model from SQL file == model from live DB for the same schema.

## Hard constraints
- Never run `git push`, `gh`, `cargo publish`, or publish anything. Commit locally after each milestone. Stay inside this repository.
