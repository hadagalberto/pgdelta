# Safety rules and citations

`pgdelta` classifies each planned (or linted) statement as **destructive**
or **locking**, and `--online` rewrites the locking cases that can be made
safe without changing the final catalog. The rules below were taken from the
official PostgreSQL documentation, fetched 2026-11-20 (PostgreSQL 19 docs,
the current version; the same statements apply on the supported 13–18 range
unless noted).

## Sources

| Rule | Official source |
| ---- | --------------- |
| Lock levels: `ALTER TABLE` takes `ACCESS EXCLUSIVE` unless noted; `ADD FOREIGN KEY` takes `SHARE ROW EXCLUSIVE`; `VALIDATE CONSTRAINT` takes `SHARE UPDATE EXCLUSIVE` | <https://www.postgresql.org/docs/current/sql-altertable.html> |
| `ALTER COLUMN ... TYPE` normally rewrites the table and its indexes; rewriting forms are not MVCC-safe and can need double disk space | <https://www.postgresql.org/docs/current/sql-altertable.html> (Notes) |
| `ADD CONSTRAINT` scans the table unless `NOT VALID`; `NOT VALID` skips the scan and `VALIDATE CONSTRAINT` checks existing rows without blocking updates | <https://www.postgresql.org/docs/current/sql-altertable.html> (Notes) |
| `SET NOT NULL` scans the table unless a valid `CHECK` proves no NULL exists | <https://www.postgresql.org/docs/current/sql-altertable.html> (`SET/DROP NOT NULL`) |
| A not-null constraint is functionally equivalent to `CHECK (column IS NOT NULL)` | <https://www.postgresql.org/docs/current/ddl-constraints.html> (Not-Null Constraints) |
| A standard `CREATE INDEX` locks out writes; `CONCURRENTLY` does not, but cannot run in a transaction block; concurrent builds on partitioned tables are unsupported | <https://www.postgresql.org/docs/current/sql-createindex.html> (Building Indexes Concurrently) |
| `ALTER TABLE ... RENAME TO` / `RENAME [COLUMN] ... TO` do not affect stored data; `ALTER INDEX ... RENAME TO`, `ALTER TYPE ... RENAME TO`, `ALTER VIEW ... RENAME TO`, `ALTER SEQUENCE ... RENAME TO` follow their respective command pages | <https://www.postgresql.org/docs/current/sql-altertable.html>, <https://www.postgresql.org/docs/current/sql-alterindex.html>, <https://www.postgresql.org/docs/current/sql-altertype.html>, <https://www.postgresql.org/docs/current/sql-alterview.html> |

## Hazard codes

| Code | Category | Trigger |
| ---- | -------- | ------- |
| `drop-table`, `drop-schema`, `drop-column`, `drop-constraint` | destructive | The statement removes data or an integrity guarantee. |
| `type-narrowing` | destructive | The target type cannot represent all source values (builtin ranks only; custom types are not guessed). |
| `type-rewrite` | locking | `ALTER COLUMN ... TYPE` (normally rewrites the table). |
| `constraint-scan` | locking | `ADD CHECK`/`FOREIGN KEY`/`PRIMARY KEY`/`UNIQUE` without `NOT VALID`. |
| `not-null-scan` | locking | `SET NOT NULL` without a previously validated `CHECK (col IS NOT NULL)` in the same script. |
| `non-concurrent-index` | locking | Standard `CREATE INDEX` (locks writers during the build). |
| `concurrent-index-in-transaction` | locking | `CREATE INDEX CONCURRENTLY` inside an explicit transaction block (invalid SQL on PostgreSQL). |

## Online rewrites

1. `ADD CONSTRAINT ... CHECK ...` and `ADD CONSTRAINT ... FOREIGN KEY ...`
   gain `NOT VALID` and are followed by `ALTER TABLE ... VALIDATE CONSTRAINT`.
2. `SET NOT NULL` is preceded by `ADD CONSTRAINT <unique name> CHECK (col IS
   NOT NULL) NOT VALID` plus its `VALIDATE CONSTRAINT`, and followed by
   `DROP CONSTRAINT <name>` so the migrated catalog converges exactly to the
   target (the helper constraint is build scaffolding, not schema state).
3. `CREATE INDEX` becomes `CREATE INDEX CONCURRENTLY` and moves to the
   non-transactional section. Partitioned-table indexes are exempt because
   concurrent builds are unsupported there; the standard build is still
   reported as a hazard.

`--online` never rewrites destructive steps. Even with rewriting, the final
human summary reports the hazards that remain so a reviewer can decide.

## Linting handwritten migrations

`pgdelta lint migration.sql` splits the script into statements with the same
tokenizer used for SQL sources and applies the exact same analyser. It is
keyword-based and deliberately conservative: it does not execute or fully
parse DDL, so it can inspect migrations pgdelta cannot model. String literals
and comments are ignored. Type narrowing cannot be inferred from a single
statement's text and is therefore only reported on generated plans, where
the source and target models are both available.

Machine-readable output: `pgdelta lint --json migration.sql` emits
`summary.statements`, `summary.hazards`, per-category counts, and a
`hazards` array on each statement.
