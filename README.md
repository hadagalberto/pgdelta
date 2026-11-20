# pgdelta

A single-binary Rust CLI + library that diffs two PostgreSQL schemas and
generates a safe, dependency-ordered migration script. Sources can be `.sql`
files, directories of `.sql` files ("declarative schema"), or live
`postgres://` databases introspected through `pg_catalog` (PostgreSQL 13–18).
A `--shadow` source applies SQL to a throwaway database and introspects it —
the most accurate mode, because PostgreSQL validates and normalises the DDL.

MIT licensed. [Leia em português](README.pt-BR.md).

## Status

Milestone 5: on top of the object coverage, live introspection and safety
analysis of milestones 1–4, pgdelta now also models **column `STORAGE`
modes** and **exclusion constraints**, and has been audited requirement by
requirement (see [docs/SPEC-CHECKLIST.md](docs/SPEC-CHECKLIST.md)). Hostile
inputs are covered by regression tests: malformed SQL, NUL bytes, BOM/CRLF,
Windows paths, empty and 5,000-column sources, and deeply nested expressions
(which now fail cleanly instead of overflowing the stack). The 77 table-driven
snapshot/convergence cases, live parity, property tests and known-valid /
known-invalid vectors all pass.

See [SPEC.md](SPEC.md) for the design, [docs/parser.md](docs/parser.md) for
the parser decision and [docs/safety.md](docs/safety.md) for the hazard rules
and their official PostgreSQL citations.

## Usage

All SQL-file examples below use the runnable fixtures in `examples/basic/`.
Their output was verified with the exact commands shown; the `postgres://`
examples additionally need a reachable server.

```sh
# Generate a migration script (writes migration.sql)
cargo run -p pgdelta -- diff --from examples/basic/old.sql \
  --to examples/basic/new.sql --output migration.sql

# JSON plan (sections, statements, hazards, summary)
cargo run -p pgdelta -- diff --from examples/basic/old.sql \
  --to examples/basic/new.sql --json

# Online-safe plan: NOT VALID → VALIDATE → SET NOT NULL,
# FK NOT VALID → VALIDATE, CREATE INDEX CONCURRENTLY outside a transaction
cargo run -p pgdelta -- diff --from examples/basic/old.sql \
  --to examples/basic/new.sql --online

# Opt-in rename hints (never guessed silently); may be repeated
cargo run -p pgdelta -- diff --from examples/basic/old.sql \
  --to examples/basic/new.sql \
  --rename table.users=people --rename column.people.email=contact

# Fail CI when the plan contains hazards (exit code 2)
cargo run -p pgdelta -- diff --from examples/basic/old.sql \
  --to examples/basic/new.sql --fail-on-hazard

# Analyse a handwritten migration with the same rules
cargo run -p pgdelta -- lint migration.sql --json

# Dump the normalised model of a source
cargo run -p pgdelta -- inspect examples/basic/old.sql

# Diff a live database against a declarative schema
cargo run -p pgdelta -- diff --from postgres://user:pass@host/db \
  --to examples/basic/new.sql

# Let PostgreSQL validate and normalise SQL sources in a shadow database
cargo run -p pgdelta -- diff --from examples/basic/old.sql \
  --to examples/basic/new.sql --shadow postgres://user:pass@host/postgres
```

The script has an explicit transactional `BEGIN; ... COMMIT;` block
containing rename statements, drops and ordinary alterations. Non-transactional
steps (`CREATE INDEX CONCURRENTLY`) follow in a clearly marked section. The
human summary and hazard markers go to **stderr**; SQL and JSON go to
**stdout**, so either can be piped safely. `--color auto|always|never`
controls ANSI colour (`auto` honours `NO_COLOR`).

### Exit codes

| Code | Meaning |
| ---- | ------- |
| 0 | Success (including "no changes"). |
| 1 | Operational error (bad source, invalid SQL, invalid rename hint). |
| 2 | Hazards found and `--fail-on-hazard` was requested. |

### Hazard analysis

Every statement is classified as **destructive** (`DROP TABLE/SCHEMA/COLUMN`,
`DROP CONSTRAINT`, narrowing a column type) or **locking** (`ALTER COLUMN
TYPE` rewrites, `SET NOT NULL` without a validated `CHECK`, `ADD
CHECK/FOREIGN KEY` without `NOT VALID`, standard `CREATE INDEX`,
`CREATE INDEX CONCURRENTLY` inside a transaction). `--online` removes the
locking hazards that can be made safe without changing the final schema;
destructive steps are still reported because they discard data.

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

* `crates/pgdelta-core` — model, SQL parsing/applying, diff, planner,
  safety analysis, rename hints and reporting;
* `crates/pgdelta` — `clap` command line (`diff`, `inspect`, `lint`);
* `crates/pgdelta-core/tests/cases` — table-driven `(from.sql, to.sql)`
  fixtures with insta snapshots and a convergence oracle;
* `docs/` — parser decision, safety rules, SPEC checklist and citations;
* `examples/basic/` — runnable source fixtures used by the README examples;
* `.github/workflows/ci.yml` — build, lint, tests on Linux/macOS/Windows and
  a PostgreSQL 13–18 matrix.
