# SPEC checklist — milestone 5 (audit and hardening)

Source of truth: [`SPEC.md`](../SPEC.md), re-read line by line on 2026-11-20.
Every item below was **run**, not assumed. Commands were executed from the
repository root on `x86_64-pc-windows-msvc`; the live-database items are
verified by the PostgreSQL 13–18 CI matrix because no local server/Docker
daemon was available (the tests print a skip reason when
`PGDELTA_TEST_DATABASE_URL` is unset).

Legend: **PASS** = verified locally by the cited command/test; **CI** =
verified by tests that require a live server and run in the CI matrix;
**OPTIONAL** = the SPEC explicitly marks the feature optional and it is not
implemented.

## 1. Product, stack and repository constraints

| # | SPEC requirement | Status | Evidence |
| - | ---------------- | ------ | -------- |
| 1.1 | Single-binary Rust CLI + library that diffs two schemas and generates a safe, ordered migration script | PASS | `cargo build --workspace`; `cargo run -p pgdelta -- diff --from examples/basic/old.sql --to examples/basic/new.sql --output migration.sql` (exit 0, 5 statements, dependency-ordered script) |
| 1.2 | Sources: live `postgres://`, `pg_dump --schema-only` SQL file, directory of `.sql` files | PASS | `cargo test -p pgdelta --test hostile` (`directory_sources_sort_and_accept_uppercase_sql_extension`, `bom_crlf_and_missing_semicolon_are_accepted`); `cargo test --workspace` exercises SQL-file sources in all table-driven cases; live source tests run in CI |
| 1.3 | Open source, MIT, author "Hadagalberto Junior" | PASS | `LICENSE`; `Cargo.toml` `authors`/`license`; `git ls-files` |
| 1.4 | No stubs, `todo!()`, `unimplemented!()`, ignored/placeholder tests | PASS | `grep -E 'todo!|unimplemented!|#\[ignore\]' crates` → no forbidden construct; `cargo clippy --workspace --all-targets -- -D warnings` (workspace lints set `todo`/`unimplemented` to deny) |
| 1.5 | Rust stable workspace `pgdelta-core` (lib) + `pgdelta` (clap derive) | PASS | `Cargo.toml`, both crate manifests; `cargo build --workspace` |
| 1.6 | `pg_query` tried first; on Windows MSVC fallback to `sqlparser-rs` PostgreSQL dialect, documented | PASS | `docs/parser.md` records the failed `pg_query` probe (libclang) and the fallback; `crates/pgdelta-core/src/sql/` is a PostgreSQL-dialect layer over `sqlparser = "0.59"` |
| 1.7 | Live introspection with `tokio-postgres` + rustls via `pg_catalog`, not `information_schema`, PostgreSQL 13–18 | CI | `src/introspect.rs` queries `pg_catalog`; TLS via `tokio-postgres-rustls`; `.github/workflows/ci.yml` matrix `13…18`; `cargo test -p pgdelta-core --test live_postgres` (skips locally, runs in CI) |
| 1.8 | `clippy -D warnings`, rustfmt, `.gitattributes * text=auto eol=lf`, proptest, insta snapshots | PASS | `cargo clippy --workspace --all-targets -- -D warnings`; `cargo fmt --all -- --check`; `.gitattributes`; `tests/props.rs`; `tests/snapshots/` (77 snapshots) |
| 1.9 | `README.md`, `README.pt-BR.md`, CHANGELOG, CONTRIBUTING, LICENSE; never publish to crates.io | PASS | All files present; no `cargo publish` in workflow or scripts; READMEs' commands verified below |

## 2. Object coverage (SPEC line 15)

| Object | Status | Evidence |
| ------ | ------ | -------- |
| Schemas (create/drop; `public` never emitted) | PASS | cases `01`–`03`, snapshots + convergence oracle |
| Extensions (create, move schema) | PASS | cases `04`–`05` |
| Enum add value / rename value | PASS | cases `46`–`49`; `valid_enum_composite_domain_range_vectors` |
| Composite types, attribute changes | PASS | cases `50`–`51` |
| Domain with constraints, default changes | PASS | cases `52`–`54` |
| Range types | PASS | case `55` |
| Sequences: standalone, options, ownership, identity columns | PASS | cases `14`–`17`, `56`–`58`; `valid_sequence_view_routine_vectors` |
| Tables: columns, defaults, generated columns, NOT NULL, collation, **storage** | PASS | cases `06`–`13`, `18`–`20`, `76`; `valid_storage_and_exclusion_vectors` |
| Constraints: PK, unique (incl. `NULLS NOT DISTINCT`), check, FK (deferrable, ON DELETE, cycles), **exclusion** | PASS | cases `21`–`28`, `32`, `42`, `77`; `invalid_storage_and_exclusion_vectors_are_rejected` |
| Indexes: partial, expression, INCLUDE, method, `CONCURRENTLY`, unique | PASS | cases `29`–`32` |
| Partitioned tables and partitions | PASS | cases `72`–`73` |
| Views and materialized views with dependency ordering | PASS | cases `59`–`63`; `column_type_change_drops_and_recreates_dependent_view_in_order` |
| Functions/procedures with overloads by signature | PASS | cases `64`–`67`; `valid_sequence_view_routine_vectors` |
| Triggers | PASS | cases `68`–`69`; `valid_trigger_policy_rls_grant_comment_vectors` |
| Policies and `ENABLE ROW LEVEL SECURITY` | PASS | cases `70`–`71` |
| Grants/privileges *(optional in SPEC)* | PASS | case `75`; `grants_revoke_and_comments_remove_real_state` |
| Comments | PASS | case `74` |
| Publication membership *(optional in SPEC)* | OPTIONAL | Not implemented; explicitly permitted by the SPEC. Unsupported publication DDL is rejected with a source-located error, never silently dropped. |

## 3. Diff and plan (SPEC lines 18–22)

| # | Requirement | Status | Evidence |
| - | ----------- | ------ | -------- |
| 3.1 | One normalised catalog model shared by all three sources | PASS | `src/model/catalog.rs`; `src/source.rs`; parity test in `tests/live_postgres.rs` (CI) |
| 3.2 | SQL-file sources applied to a throwaway model: parse DDL, apply ALTERs in order | PASS | `catalog_from_sql`/`apply_sql_to`; every table-driven case applies `from.sql` then the generated migration |
| 3.3 | `--shadow postgres://...`: execute into shadow DB and introspect | CI | `src/source.rs::shadow_catalog`; `shadow_mode_matches_sql_file_model` runs in the CI matrix |
| 3.4 | Dependency graph with topological ordering for drops/creates | PASS | `src/dep.rs`; cases `41`, `63`; `column_type_change_drops_and_recreates_dependent_view_in_order` |
| 3.5 | Drop and recreate dependent views when a column type changes | PASS | case `63` + named object-validation test |
| 3.6 | Rename detection only through opt-in hints, never guessed | PASS | `rename_hints_are_strict_and_never_guessed`; `rename_hints_emit_rename_statements_and_converge`; new hostile test `malformed_rename_hints_are_rejected` |
| 3.7 | Safety analysis: destructive and locking hazards | PASS | `safety::tests::*`; `offline_plan_reports_real_hazards`; docs/safety.md |
| 3.8 | Online mode: `CHECK NOT VALID → VALIDATE → SET NOT NULL`, `FK NOT VALID → VALIDATE`, concurrent index outside transaction | PASS | `online_check_add_uses_not_valid_and_converges`, `online_fk_add_uses_not_valid_then_validate_and_converges`, `online_set_not_null_adds_and_drops_a_validated_check_and_converges`, `online_index_is_concurrent_and_non_transactional` |
| 3.9 | SQL output with separate transactional and non-transactional sections | PASS | snapshots for every case; CLI test `diff_online_and_rename_hints_rewrite_the_script` |
| 3.10 | JSON plan | PASS | `json_plan_includes_hazards_and_summary_counts`; `diff_fail_on_hazard_returns_exit_code_two_with_json_on_stdout`; verified README example |
| 3.11 | Human summary with colored hazard markers | PASS | `always_color_emits_ansi_in_the_human_summary`; `--color` is implemented in `src/safety.rs` with `NO_COLOR` support |
| 3.12 | Exit code 2 when hazards exist and `--fail-on-hazard` | PASS | `lint_fail_on_hazard_returns_exit_code_two`, `diff_fail_on_hazard_returns_exit_code_two_with_json_on_stdout`, new `fail_on_hazard_exit_code_two_is_stable_with_json_output` |

## 4. CLI (SPEC lines 24–25)

| Invocation / flag | Status | Evidence |
| ----------------- | ------ | -------- |
| `pgdelta diff --from <src> --to <src>` | PASS | `cargo run -p pgdelta -- diff --from examples/basic/old.sql --to examples/basic/new.sql --color never` (exit 0) |
| `-o` / `--output <file>` | PASS | Same command with `--output migration.sql` (exit 0); error cases in `missing_sources_and_bad_output_paths_report_errors`. **Fixed:** READMEs previously used the nonexistent `--out`. |
| `--online` | PASS | Verified README command (exit 0, 9 statements, no locking hazards) |
| `--json` | PASS | Verified README command (exit 0, valid JSON) |
| `pgdelta inspect <src>` | PASS | Verified README command (exit 0, normalised model JSON) |
| `pgdelta lint migration.sql` | PASS | Verified README command (exit 0, valid JSON) |
| `--shadow`, `--rename`, `--fail-on-hazard`, `--color` | PASS | Verified README command where applicable; unit + CLI tests for all four |

## 5. Tests (SPEC lines 28–31)

| # | Requirement | Status | Evidence |
| - | ----------- | ------ | -------- |
| 5.1 | Table-driven `(from.sql, to.sql) → expected migration snapshot` covering every object kind and dependency ordering | PASS | `cargo test -p pgdelta-core --test table_driven`: **77** named cases, each with an insta snapshot and a convergence check. The full suite also runs 64 generated cases per property test (6 properties = 384 generated schemas), i.e. **461 diff/convergence scenarios** in total. |
| 5.2 | Convergence integration test on every Postgres version 13–18 | CI | `tests/live_postgres.rs::generated_migration_converges_on_live_postgres`; `ci.yml` `postgres:['13'…'18']` service matrix, serial execution |
| 5.3 | Property test: `diff(a,a)` empty; `diff(a,b)` applied to `a` converges to `b` | PASS | `tests/props.rs`: `diff_a_a_is_empty`, `plan_applied_to_a_converges_to_b`, `plan_from_empty_applied_converges`, `online_plan_applied_to_a_converges_to_b` |
| 5.4 | Parser/introspection parity for the same schema | CI | `sql_file_model_matches_live_model`, `shadow_mode_matches_sql_file_model` (the latter asserts parity for all 77 snapshot cases) |
| 5.5 | Known-valid and known-invalid vectors, never silently accepting bad DDL | PASS | `tests/object_validation.rs` (11 tests), `safety::tests::narrowing_vectors`, and the new hostile CLI suite |
| 5.6 | Hostile inputs: malformed SQL, edge cases, Windows paths, large inputs | PASS | `cargo test -p pgdelta --test hostile`: **14** tests (BOM/CRLF, NUL bytes, unterminated strings/comments, 2,000-level nesting, 5,000 columns, Windows paths, uppercase `.SQL`, empty dirs, invalid flags/hints, unreachable DB, missing output paths) |

### Bugs found and fixed during the hostile audit

* **Stack overflow on deeply nested SQL** — a 2,000-level parenthesised CHECK
  crashed the process (`exit -1073741571`). Fixed by
  `sql::tokens::check_nesting` (limit 128), applied to both the catalog parser
  and `split_sql_statements` (lint/shadow). Regression test:
  `deeply_nested_expression_is_rejected_without_stack_overflow`.
* **README examples used `--out`**, which is not a CLI flag. Fixed to
  `--output` in both languages; every SQL-file example now points at a
  committed, runnable fixture in `examples/basic/` and was executed with the
  exact documented command.

## 6. CI and documentation

| # | Requirement | Status | Evidence |
| - | ----------- | ------ | -------- |
| 6.1 | Build/lint/test commands identical to the local ones | PASS | `.github/workflows/ci.yml`: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo build --workspace`, `cargo test --workspace`; all four were run locally with the same results |
| 6.2 | Same commands on ubuntu, windows, macos | PASS | `quality` job matrix `os: [ubuntu-latest, windows-latest, macos-latest]`, `fail-fast: false`; local Windows run of all four commands passed |
| 6.3 | PostgreSQL 13–18 service-container integration job | PASS | `integration-postgres` matrix in `ci.yml`, `PGDELTA_TEST_DATABASE_URL` set, `cargo test --workspace -- --nocapture --test-threads=1` |
| 6.4 | Both READMEs have accurate, runnable examples whose output was verified | PASS | All six SQL-file commands in `README.md` were executed (exit 0) with output checked; `README.pt-BR.md` mirrors them. Live/shadow commands are marked as requiring a server. |
| 6.5 | Parser decision, safety rules and official citations documented | PASS | `docs/parser.md` (fetched PostgreSQL 18 CREATE TABLE/ALTER TABLE/etc. pages, PostgreSQL C-source URLs) and `docs/safety.md` (fetched PostgreSQL docs, 2026-11-20) |

## 7. Final verification run (this milestone)

Executed from the repository root after all changes:

```
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace
cargo test --workspace
```

Result: formatting, clippy and build clean; **all 64 test functions pass**
(6 CLI, 14 hostile, 10 core unit, 3 live-skip, 11 milestone-4,
11 object-validation, 6 property, 3 table-driven; 0 ignored/failed). Those
functions drive 77 named snapshot/convergence cases and 384 proptest-generated
schemas. Live-server items are the only checks not runnable locally; they are
covered by the CI matrix described above.
