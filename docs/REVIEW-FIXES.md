# Independent review findings — fixes

Milestone 6, 2026-11-20. Every item was reproduced by a regression test
written **before** the corresponding code change, with a hand-derived
expected value (PostgreSQL documentation / SPEC / explicit reasoning in the
test comments). Tests were observed failing before each fix.

Live verification used a dedicated `postgres:18` container and
`pg_dump --schema-only` as the convergence oracle (not pgdelta introspection).

| # | Item | Regression test | Fix summary | Status |
|---|------|-----------------|-------------|--------|
| 1 | CRITICAL: index definition changes ignored | `review_fixes::index_expression_change_is_drop_and_create`, `index_predicate_change_is_drop_and_create`, `same_index_definition_is_no_change`, `changed_index_is_concurrent_in_online_mode` | `diff.rs` compares the normalized index definition (everything except the build-only `concurrent` flag); a same-named index with a different definition now emits DROP INDEX + CREATE INDEX. `--online` converts the CREATE to CONCURRENTLY via the existing rewrite. Case 78 added. | Fixed |
| 2 | CRITICAL: unquoted function bodies | `review_fixes::single_quoted_function_body_is_dollar_quoted`, `equivalent_function_body_quotings_do_not_diff`, `function_body_tag_avoids_collision` | Bodies are normalized to raw inner text at ingestion (parser and `pg_proc.prosrc` now agree), so `'..'` and `$$..$$` compare equal. `plan.rs` always dollar-quotes the body, choosing the first tag (`$$`, `$pgdelta$`, `$pgdelta0$`, …) absent from the text. Case 79 added. | Fixed |
| 3 | HIGH: mutual-FK table drops hit a dependency cycle | `review_fixes::mutual_fk_drops_remove_constraints_first`, `plan::tests::drop_table_waits_for_views_in_every_schema` (case 80) | Dropped tables' FKs that reference other dropped tables are emitted as `ALTER TABLE ... DROP CONSTRAINT` before any table drop; the cyclic table-to-table edges were removed. Self-references die with the table. | Fixed |
| 4 | HIGH: enum value removal / insertion | `review_fixes::enum_insert_before_uses_add_value_before`, `enum_removal_rewrites_columns_and_is_hazardous` (case 81) | Subsequence insertions use `ALTER TYPE ... ADD VALUE ... BEFORE/AFTER` anchored to an original label. Removals/reorders with dependent columns rename the old type, create the replacement, rewrite each column with `USING col::text::new`, drop the old type, and attach a destructive `enum-value-removal` hazard. | Fixed |
| 5 | HIGH: transitive view / matview chains | `review_fixes::view_chain_is_dropped_and_recreated_transitively` (case 82) | `diff.rs` seeds the recreate set with definition changes and changed-table readers, then takes the transitive closure over view-on-view reads. `plan.rs` adds explicit drop edges (dependents first) and create edges (dependencies first). | Fixed |
| 6 | MEDIUM: `STORAGE` inside CREATE TABLE invalid on PG13–15 | `review_fixes::storage_is_emitted_after_create_table` | `render_column_def` no longer emits inline `STORAGE`; the planner emits portable `ALTER TABLE ... ALTER COLUMN ... SET STORAGE` after CREATE TABLE / ADD COLUMN. | Fixed |
| 7 | MEDIUM: cross-schema dependent views | `plan::tests::drop_table_waits_for_views_in_every_schema`, `review_fixes::cross_schema_view_is_dropped_before_its_table` (case 84) | Drop-table edges now consider views in **all** schemas that actually read the table, not just the table's own schema. | Fixed |
| 8 | LOW: `serial` rejected / silent live skips | `review_fixes::serial_is_a_sequence_with_default_and_ownership` (case 83) | `serial`/`bigserial`/`smallserial` expand to the base integer type, a `<table>_<column>_seq` sequence, the canonical `nextval(...)` default, `NOT NULL`, and `OWNED BY` after the table exists. `live_postgres.rs` prints a visible SKIP on both streams and fails in CI when `PGDELTA_TEST_DATABASE_URL` is unset. | Fixed |
| 9 | TEST ORACLE: circular introspection convergence | `live_postgres::generated_migration_converges_on_live_postgres`, `live_postgres::rich_generated_migration_converges_with_pgdump_oracle`, `props::rich_plan_applied_to_a_converges_to_b` | Restored byte-for-byte `pg_dump --schema-only` comparison as the live convergence oracle (configurable via `PGDELTA_TEST_PGDUMP`). Added `common::any_rich_catalog()` producing same-named indexes with different definitions, function body changes, enum removals and view-on-matview chains; a live property test applies generated pairs and compares pg_dump. This immediately exposed an extra representation bug: `current_user` was rendered as `"current_user"()` — fixed in `sql/expr.rs` to emit the bare `CURRENT_USER` keyword (case 70/71 snapshots regenerated and read). | Fixed |

## Additional corrections exposed by the independent oracle

* `current_user` / `session_user` are SQL special values, not quoted function
  calls: `review_fixes::current_user_is_not_a_quoted_function_call`.
* Regenerated and read snapshots: `56_sequence_create` (redundant
  `OWNED BY NONE` removed), `70_rls_enable_policy` and `71_policy_change`
  (bare `CURRENT_USER` / `SESSION_USER`), plus the seven new case snapshots
  78–84. Snapshot 76 needed no change: it only alters an existing table, so
  no CREATE TABLE is rendered.

## Verification

* `cargo fmt --all -- --check` — clean.
* `cargo clippy --workspace --all-targets -- -D warnings` — clean.
* `cargo test --workspace` — all tests pass (live tests print an explicit
  SKIP when no database URL is configured).
* `cargo test --workspace` with `PGDELTA_TEST_DATABASE_URL` and
  `PGDELTA_TEST_PGDUMP` set against PostgreSQL 18 — all tests pass,
  including the pg_dump convergence oracle for all 84 cases and the rich
  property test.
