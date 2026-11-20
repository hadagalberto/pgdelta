//! Regression tests for the independent review findings (2026-11-20).
//!
//! Every expected value in this file is hand-derived from the PostgreSQL
//! documentation / the SPEC and written next to the assertion. None of the
//! oracles call pgdelta's own renderer, applier or introspection to produce
//! the value being asserted.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use pgdelta_core::{PlanOptions, catalog_from_sql, plan, plan_with_options};

fn statements(from_sql: &str, to_sql: &str) -> Vec<String> {
    let from = catalog_from_sql(from_sql, "from.sql").expect("parsing from.sql");
    let to = catalog_from_sql(to_sql, "to.sql").expect("parsing to.sql");
    plan(&from, &to, "<from>", "<to>")
        .expect("planning")
        .statements()
        .into_iter()
        .map(|s| s.sql.clone())
        .collect()
}

fn online_statements(from_sql: &str, to_sql: &str) -> Vec<String> {
    let from = catalog_from_sql(from_sql, "from.sql").expect("parsing from.sql");
    let to = catalog_from_sql(to_sql, "to.sql").expect("parsing to.sql");
    plan_with_options(
        &from,
        &to,
        "<from>",
        "<to>",
        &PlanOptions {
            online: true,
            renames: Vec::new(),
        },
    )
    .expect("planning online")
    .statements()
    .into_iter()
    .map(|s| s.sql.clone())
    .collect()
}

// ---------------------------------------------------------------------------
// Finding 1: index definition changes are ignored.
// ---------------------------------------------------------------------------

/// PostgreSQL indexes are identified by *definition*, not by name alone:
/// `CREATE INDEX i ON t (lower(note))` and `CREATE INDEX i ON t (upper(note))`
/// are different objects (PostgreSQL, CREATE INDEX). When the definition
/// changes, the only portable migration is DROP + CREATE.
#[test]
fn index_expression_change_is_drop_and_create() {
    let from = "CREATE TABLE t (note text); CREATE INDEX i ON t (lower(note));";
    let to = "CREATE TABLE t (note text); CREATE INDEX i ON t (upper(note));";
    let sql = statements(from, to);
    assert!(
        sql.iter().any(|s| s == "DROP INDEX \"public\".\"i\";"),
        "expected DROP INDEX \"public\".\"i\"; got {sql:?}"
    );
    assert!(
        sql.iter()
            .any(|s| s == "CREATE INDEX \"i\" ON \"public\".\"t\" ((\"upper\"(\"note\")));"),
        "expected CREATE INDEX with upper(note); got {sql:?}"
    );
    assert!(
        !sql.iter().any(|s| s.contains("lower")),
        "the obsolete lower(note) definition must not survive; got {sql:?}"
    );
}

/// A partial-index predicate is part of the definition (PostgreSQL,
/// CREATE INDEX, "Partial Indexes"); changing it must also drop+create.
#[test]
fn index_predicate_change_is_drop_and_create() {
    let from = "CREATE TABLE t (note text); CREATE INDEX i ON t (note) WHERE note IS NOT NULL;";
    let to = "CREATE TABLE t (note text); CREATE INDEX i ON t (note) WHERE note IS NULL;";
    let sql = statements(from, to);
    assert!(
        sql.iter().any(|s| s == "DROP INDEX \"public\".\"i\";"),
        "got {sql:?}"
    );
    assert!(
        sql.iter().any(|s| s.contains("WHERE \"note\" is null")),
        "got {sql:?}"
    );
}

/// A definition that only differs in the build method (CONCURRENTLY) is not
/// a catalog change; PostgreSQL stores no such flag. Guard against emitting
/// spurious drop+create for it.
#[test]
fn same_index_definition_is_no_change() {
    let from = "CREATE TABLE t (note text); CREATE INDEX i ON t (lower(note));";
    let to = "CREATE TABLE t (note text); CREATE INDEX CONCURRENTLY i ON t (lower(note));";
    let sql = statements(from, to);
    assert!(sql.is_empty(), "got {sql:?}");
}

/// The online rewrite must turn the replacement CREATE INDEX into a
/// concurrent, non-transactional build (PostgreSQL CREATE INDEX docs).
#[test]
fn changed_index_is_concurrent_in_online_mode() {
    let from = "CREATE TABLE t (note text); CREATE INDEX i ON t (lower(note));";
    let to = "CREATE TABLE t (note text); CREATE INDEX i ON t (upper(note));";
    let sql = online_statements(from, to);
    assert!(
        sql.iter()
            .any(|s| s
                == "CREATE INDEX CONCURRENTLY \"i\" ON \"public\".\"t\" ((\"upper\"(\"note\")));"),
        "got {sql:?}"
    );
}

// ---------------------------------------------------------------------------
// Finding 2: function bodies emitted unquoted.
// ---------------------------------------------------------------------------

/// PostgreSQL requires a function body after `AS` to be a string literal or
/// dollar-quoted text (CREATE FUNCTION synopsis). A body coming from
/// `pg_proc.prosrc` has no delimiters, so the planner must supply them.
#[test]
fn single_quoted_function_body_is_dollar_quoted() {
    let to = "CREATE FUNCTION f() RETURNS integer LANGUAGE SQL AS 'select 42';";
    let sql = statements("", to);
    assert!(
        sql.iter().any(|s| s.contains("AS $$select 42$$;")),
        "expected a dollar-quoted body; got {sql:?}"
    );
}

/// `'..'` and `$$..$$` with the same inner text denote the same function
/// body (PostgreSQL CREATE FUNCTION); they must not produce a diff.
#[test]
fn equivalent_function_body_quotings_do_not_diff() {
    let from = "CREATE FUNCTION f() RETURNS integer LANGUAGE SQL AS 'select 42';";
    let to = "CREATE FUNCTION f() RETURNS integer LANGUAGE SQL AS $$select 42$$;";
    let sql = statements(from, to);
    assert!(sql.is_empty(), "expected no statements; got {sql:?}");
}

/// The dollar-quote tag must not appear in the body, otherwise the script
/// terminates early (PostgreSQL, "Dollar-Quoted String Constants").
#[test]
fn function_body_tag_avoids_collision() {
    let to = "CREATE FUNCTION f() RETURNS text LANGUAGE SQL AS $f$select '$$'::text$f$;";
    let sql = statements("", to);
    assert!(
        sql.iter()
            .any(|s| s.contains("AS $pgdelta$select '$$'::text$pgdelta$;")),
        "expected a non-colliding dollar-quote tag; got {sql:?}"
    );
}

// ---------------------------------------------------------------------------
// Finding 3: mutually-referencing dropped tables.
// ---------------------------------------------------------------------------

/// PostgreSQL rejects `DROP TABLE a, b` (or sequential drops) with
/// "dependency cycle" while foreign keys between them still exist. The
/// constraints must be dropped first (PostgreSQL ALTER TABLE, DROP CONSTRAINT;
/// CREATE TABLE, DROP TABLE RESTRICT).
#[test]
fn mutual_fk_drops_remove_constraints_first() {
    let from = concat!(
        "CREATE TABLE a (id int PRIMARY KEY, b_id int); ",
        "CREATE TABLE b (id int PRIMARY KEY, a_id int); ",
        "ALTER TABLE a ADD CONSTRAINT a_b_fkey FOREIGN KEY (b_id) REFERENCES b(id); ",
        "ALTER TABLE b ADD CONSTRAINT b_a_fkey FOREIGN KEY (a_id) REFERENCES a(id);"
    );
    let planned = {
        let from_catalog = catalog_from_sql(from, "from.sql").unwrap();
        let to_catalog = catalog_from_sql("", "to.sql").unwrap();
        plan(&from_catalog, &to_catalog, "<from>", "<to>").expect("planning mutual drops")
    };
    let sql: Vec<&str> = planned
        .statements()
        .iter()
        .map(|s| s.sql.as_str())
        .collect();
    let fk_on_a = sql
        .iter()
        .position(|s| *s == "ALTER TABLE \"public\".\"a\" DROP CONSTRAINT \"a_b_fkey\";");
    let fk_on_b = sql
        .iter()
        .position(|s| *s == "ALTER TABLE \"public\".\"b\" DROP CONSTRAINT \"b_a_fkey\";");
    let table_a = sql
        .iter()
        .position(|s| *s == "DROP TABLE \"public\".\"a\";");
    let table_b = sql
        .iter()
        .position(|s| *s == "DROP TABLE \"public\".\"b\";");
    assert!(fk_on_a.is_some() && fk_on_b.is_some(), "got {sql:?}");
    assert!(table_a.is_some() && table_b.is_some(), "got {sql:?}");
    assert!(fk_on_a < table_a && fk_on_b < table_b, "got {sql:?}");
}

// ---------------------------------------------------------------------------
// Finding 4: enum changes.
// ---------------------------------------------------------------------------

/// `ALTER TYPE ... ADD VALUE ... BEFORE neighbour` inserts an enum label at
/// an arbitrary position (PostgreSQL ALTER TYPE, ADD VALUE). No drop/recreate
/// is needed.
#[test]
fn enum_insert_before_uses_add_value_before() {
    let from = concat!(
        "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy'); ",
        "CREATE TABLE t (m mood);"
    );
    let to = concat!(
        "CREATE TYPE mood AS ENUM ('sad', 'meh', 'ok', 'happy'); ",
        "CREATE TABLE t (m mood);"
    );
    let sql = statements(from, to);
    assert!(
        sql.iter()
            .any(|s| s == "ALTER TYPE \"public\".\"mood\" ADD VALUE 'meh' BEFORE 'ok';"),
        "got {sql:?}"
    );
    assert!(
        !sql.iter().any(|s| s.contains("DROP TYPE")),
        "no type may be dropped; got {sql:?}"
    );
}

/// PostgreSQL refuses `DROP TYPE` while a column still uses it. Removing a
/// label therefore needs: rename the old type, create the new one, rewrite
/// every dependent column via `USING col::text::new`, then drop the old
/// type. The rewrite is destructive and must be reported as a hazard.
#[test]
fn enum_removal_rewrites_columns_and_is_hazardous() {
    let from = concat!(
        "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy'); ",
        "CREATE TABLE t (m mood);"
    );
    let to = concat!(
        "CREATE TYPE mood AS ENUM ('sad', 'happy'); ",
        "CREATE TABLE t (m mood);"
    );
    let from_catalog = catalog_from_sql(from, "from.sql").unwrap();
    let to_catalog = catalog_from_sql(to, "to.sql").unwrap();
    let planned = plan(&from_catalog, &to_catalog, "<from>", "<to>").unwrap();
    let sql: Vec<String> = planned.statements().iter().map(|s| s.sql.clone()).collect();
    let find = |needle: &str| sql.iter().position(|s| s.contains(needle));
    assert!(find("RENAME TO \"mood_old\"").is_some(), "got {sql:?}");
    assert!(
        find("CREATE TYPE \"public\".\"mood\" AS ENUM ('sad', 'happy');").is_some(),
        "got {sql:?}"
    );
    assert!(find("USING \"m\"::text::\"mood\"").is_some(), "got {sql:?}");
    assert!(
        find("DROP TYPE \"public\".\"mood_old\";").is_some(),
        "got {sql:?}"
    );
    assert!(
        planned.summary().hazards > 0
            && planned
                .statements()
                .iter()
                .any(|s| s.hazards.iter().any(|h| h.code == "enum-value-removal")),
        "enum rewrite must be flagged as a hazard; got {sql:?}"
    );
}

// ---------------------------------------------------------------------------
// Finding 5: transitive view / materialized-view chains.
// ---------------------------------------------------------------------------

/// A view reading a materialized view is a real PostgreSQL dependency: the
/// view must be dropped before the materialized view, and recreated after
/// it. The chain must be followed transitively.
#[test]
fn view_chain_is_dropped_and_recreated_transitively() {
    let from = concat!(
        "CREATE TABLE t (c int); ",
        "CREATE MATERIALIZED VIEW mv AS SELECT c FROM t; ",
        "CREATE VIEW v2 AS SELECT c FROM mv;"
    );
    let to = concat!(
        "CREATE TABLE t (c bigint); ",
        "CREATE MATERIALIZED VIEW mv AS SELECT c FROM t; ",
        "CREATE VIEW v2 AS SELECT c FROM mv;"
    );
    let sql = statements(from, to);
    let pos = |needle: &str| sql.iter().position(|s| s == needle);
    let drop_v2 = pos("DROP VIEW \"public\".\"v2\";");
    let drop_mv = pos("DROP MATERIALIZED VIEW \"public\".\"mv\";");
    let create_mv = pos("CREATE MATERIALIZED VIEW \"public\".\"mv\" AS SELECT c FROM t;");
    let create_v2 = pos("CREATE VIEW \"public\".\"v2\" AS SELECT c FROM mv;");
    assert!(drop_v2.is_some() && drop_mv.is_some(), "got {sql:?}");
    assert!(create_mv.is_some() && create_v2.is_some(), "got {sql:?}");
    assert!(
        drop_v2 < drop_mv,
        "dependent view must drop first; got {sql:?}"
    );
    assert!(
        create_mv < create_v2,
        "dependency must be recreated first; got {sql:?}"
    );
}

// ---------------------------------------------------------------------------
// Finding 6: STORAGE inside CREATE TABLE.
// ---------------------------------------------------------------------------

/// `STORAGE` is an `ALTER COLUMN` option on every supported PostgreSQL
/// version (13-18); putting it inside CREATE TABLE is invalid before 16. The
/// portable form is a separate ALTER after the CREATE.
#[test]
fn storage_is_emitted_after_create_table() {
    let to = "CREATE TABLE t (a text STORAGE PLAIN);";
    let sql = statements("", to);
    assert!(
        sql.iter()
            .any(|s| s
                .contains("ALTER TABLE \"public\".\"t\" ALTER COLUMN \"a\" SET STORAGE PLAIN;")),
        "got {sql:?}"
    );
    assert!(
        sql.iter()
            .find(|s| s.starts_with("CREATE TABLE"))
            .is_some_and(|s| !s.contains("STORAGE")),
        "CREATE TABLE must not contain STORAGE; got {sql:?}"
    );
}

// ---------------------------------------------------------------------------
// Finding 7: cross-schema dependent views.
// ---------------------------------------------------------------------------

/// PostgreSQL refuses to drop a table while a view in *any* schema reads it
/// (DROP TABLE ... RESTRICT). A view in another schema must be dropped first.
#[test]
fn cross_schema_view_is_dropped_before_its_table() {
    let from = concat!(
        "CREATE SCHEMA s; ",
        "CREATE TABLE t (c int); ",
        "CREATE VIEW s.v AS SELECT c FROM public.t;"
    );
    let sql = statements(from, "");
    let drop_view = sql.iter().position(|s| s == "DROP VIEW \"s\".\"v\";");
    let drop_table = sql.iter().position(|s| s == "DROP TABLE \"public\".\"t\";");
    assert!(drop_view.is_some() && drop_table.is_some(), "got {sql:?}");
    assert!(drop_view < drop_table, "got {sql:?}");
}

/// `current_user` is a SQL special value, not a callable function
/// (PostgreSQL, "Current User"). `pg_dump` renders it as `CURRENT_USER`;
/// re-writing it as `"current_user"()` produces a different schema dump.
/// The independent `pg_dump` oracle in the live suite exposed this.
#[test]
fn current_user_is_not_a_quoted_function_call() {
    let to = concat!(
        "CREATE TABLE documents (id integer PRIMARY KEY, owner text); ",
        "ALTER TABLE documents ENABLE ROW LEVEL SECURITY; ",
        "CREATE POLICY owner_read ON documents FOR SELECT USING (owner = current_user);"
    );
    let sql = statements("", to);
    assert!(
        sql.iter()
            .any(|s| s.contains("USING (\"owner\" = CURRENT_USER)")),
        "expected the SQL special value CURRENT_USER; got {sql:?}"
    );
    assert!(
        !sql.iter().any(|s| s.contains("\"current_user\"()")),
        "got {sql:?}"
    );
}

// ---------------------------------------------------------------------------
// Finding 8: `serial` in SQL files.
// ---------------------------------------------------------------------------

/// `serial` is shorthand (PostgreSQL, "Serial Types"): an integer column,
/// a sequence named `<table>_<column>_seq`, `DEFAULT nextval(...)`, and
/// `OWNED BY` the column. All three pieces must be modelled.
#[test]
fn serial_is_a_sequence_with_default_and_ownership() {
    let catalog = catalog_from_sql("CREATE TABLE t (id serial);", "serial.sql").unwrap();
    let table = catalog.table("public", "t").unwrap();
    let column = table.column("id").unwrap();
    assert_eq!(
        column.data_type, "integer",
        "serial is shorthand for integer"
    );
    assert_eq!(column.default.as_deref(), Some("\"nextval\"('t_id_seq')"));
    let sequence = catalog
        .sequences
        .get(&("public".to_owned(), "t_id_seq".to_owned()))
        .unwrap();
    assert_eq!(sequence.data_type, "integer");
    assert_eq!(sequence.owned_by.as_ref().unwrap().table, "t");
    assert_eq!(sequence.owned_by.as_ref().unwrap().column, "id");
}
