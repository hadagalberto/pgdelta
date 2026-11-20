# Contributing to pgdelta

Thanks for helping. The project is developed in milestones defined in
[SPEC.md](SPEC.md); please keep changes inside the current milestone's scope.

## Ground rules

* No stubs: no `todo!()`, `unimplemented!()`, ignored or placeholder tests,
  and no features that silently do nothing. Unsupported SQL must fail with a
  source-located error.
* Tests assert real behaviour with known-valid and known-invalid vectors.
  Never weaken or delete a test to make a change pass — fix the root cause.
* Production rules must be backed by the official PostgreSQL documentation
  (or source) and cited in a code comment or `docs/parser.md`. Never invent a
  grammar position or a semantic rule.
* Line endings are LF in the repository (`.gitattributes`).

## Before opening a change

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The live-database suite additionally needs a PostgreSQL connection:

```sh
PGDELTA_TEST_DATABASE_URL=postgres://user:pass@localhost:5432/postgres \
  cargo test -p pgdelta-core --test live_postgres
```

It creates and drops its own temporary databases and skips itself when the
variable is absent. Version-gated cases are skipped on older servers via
their `min_pg_version.txt` files.

## Adding a table-driven case

Create `crates/pgdelta-core/tests/cases/<NN>_<name>/from.sql` and `to.sql`.
Run the snapshot test once with `INSTA_UPDATE=always`, review the new `.snap`
file under `tests/snapshots/`, and commit it. The convergence oracle runs
automatically for every case.
