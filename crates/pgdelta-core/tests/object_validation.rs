//! Known-valid and known-invalid vectors for the milestone-2 object kinds.
//!
//! Each invalid vector asserts that pgdelta reports a source-located error,
//! never that it silently ignores or coerces bad DDL. Valid vectors assert
//! the normalised model fields, not just that parsing succeeded.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use pgdelta_core::model::{
    ObjectKind, PgTypeKind, PolicyCommand, RoutineKind, TriggerTiming, Volatility,
};
use pgdelta_core::{apply_sql_to, catalog_from_sql, plan};

fn valid(sql: &str) -> pgdelta_core::Catalog {
    catalog_from_sql(sql, "valid.sql").expect("valid vector must parse")
}

fn invalid(sql: &str, must_mention: &str) {
    let error = catalog_from_sql(sql, "invalid.sql")
        .expect_err("invalid vector must be rejected, never silently accepted");
    let rendered = error.to_string();
    assert!(
        rendered.contains(must_mention),
        "error must explain the violation ({must_mention:?}): {rendered}"
    );
}

#[test]
fn valid_enum_composite_domain_range_vectors() {
    let catalog = valid(
        "CREATE TYPE mood AS ENUM ('sad', 'ok');\
         ALTER TYPE mood ADD VALUE 'happy' AFTER 'ok';\
         CREATE TYPE addr AS (street text, zip integer);\
         ALTER TYPE addr ADD ATTRIBUTE city text;\
         CREATE DOMAIN posint AS integer CHECK (VALUE > 0);\
         ALTER DOMAIN posint SET NOT NULL;\
         CREATE TYPE numrange2 AS RANGE (SUBTYPE = integer);",
    );
    assert_eq!(catalog.types.len(), 4);
    let mood = catalog
        .types
        .get(&("public".into(), "mood".into()))
        .unwrap();
    match &mood.kind {
        PgTypeKind::Enum { values } => assert_eq!(values, &["sad", "ok", "happy"]),
        other => panic!("expected enum, got {other:?}"),
    }
    let addr = catalog
        .types
        .get(&("public".into(), "addr".into()))
        .unwrap();
    match &addr.kind {
        PgTypeKind::Composite { attributes } => {
            assert_eq!(
                attributes
                    .iter()
                    .map(|a| a.name.as_str())
                    .collect::<Vec<_>>(),
                ["street", "zip", "city"]
            );
        }
        other => panic!("expected composite, got {other:?}"),
    }
    let posint = catalog
        .types
        .get(&("public".into(), "posint".into()))
        .unwrap();
    match &posint.kind {
        PgTypeKind::Domain { constraints, .. } => assert_eq!(constraints.len(), 2),
        other => panic!("expected domain, got {other:?}"),
    }
}

#[test]
fn invalid_type_vectors_are_rejected() {
    invalid("CREATE TYPE mood AS ENUM ('a', 'a');", "duplicated");
    invalid("CREATE TYPE mood AS ENUM ('');", "cannot be empty");
    invalid("CREATE TYPE bad AS (a text, a integer);", "duplicated");
    invalid(
        "CREATE TYPE bad_range AS RANGE (SUBTYPE_OPCLASS = int4_ops);",
        "SUBTYPE",
    );
    invalid("ALTER TYPE missing ADD VALUE 'x';", "does not exist");
    invalid(
        "CREATE DOMAIN d AS integer CHECK VALUE > 0;",
        "parenthesised CHECK",
    );
    invalid(
        "ALTER TYPE mood ADD VALUE 'x' BEFORE 'absent';",
        "does not exist",
    );
}

#[test]
fn valid_sequence_view_routine_vectors() {
    let catalog = valid(
        "CREATE SEQUENCE s AS integer INCREMENT BY -2 MINVALUE -100 MAXVALUE 0 START WITH 0 CACHE 2 CYCLE;\
         CREATE TABLE t (id integer PRIMARY KEY, label text);\
         CREATE VIEW v AS SELECT id, label FROM t;\
         CREATE MATERIALIZED VIEW mv AS SELECT count(*) AS n FROM t WITH NO DATA;\
         CREATE FUNCTION f(a integer) RETURNS integer LANGUAGE SQL IMMUTABLE STRICT AS 'select $1';\
         CREATE PROCEDURE p() LANGUAGE SQL AS 'select 1';",
    );
    let seq = catalog
        .sequences
        .get(&("public".into(), "s".into()))
        .unwrap();
    assert_eq!(seq.options.increment, -2);
    assert_eq!(seq.options.minvalue, -100);
    assert_eq!(seq.options.maxvalue, 0);
    assert!(seq.options.cycle);
    assert_eq!(catalog.views.len(), 2);
    let view = catalog.views.get(&("public".into(), "v".into())).unwrap();
    assert_eq!(
        view.referenced_relations(),
        vec![("public".into(), "t".into())]
    );
    let routine = catalog
        .routines
        .get(&("public".into(), "f".into(), "integer".into()))
        .unwrap();
    assert_eq!(routine.kind, RoutineKind::Function);
    assert_eq!(routine.volatility, Volatility::Immutable);
    assert!(routine.strict);
    assert!(
        catalog
            .routines
            .contains_key(&("public".into(), "p".into(), String::new()))
    );
}

#[test]
fn invalid_sequence_view_routine_vectors_are_rejected() {
    invalid("CREATE SEQUENCE s AS text;", "smallint, integer or bigint");
    invalid("CREATE SEQUENCE s INCREMENT BY 0;", "nonzero");
    invalid("CREATE VIEW v AS ;", "expected a query");
    invalid(
        "CREATE RECURSIVE VIEW v AS SELECT 1;",
        "explicit column list",
    );
    invalid("CREATE FUNCTION f(a integer) AS 'select 1';", "LANGUAGE");
    invalid(
        "CREATE FUNCTION f() RETURNS integer LANGUAGE SQL;",
        "needs a body",
    );
    invalid("DROP VIEW v;", "does not exist");
}

#[test]
fn valid_trigger_policy_rls_grant_comment_vectors() {
    let catalog = valid(
        "CREATE TABLE t (id integer PRIMARY KEY, owner text);\
         CREATE FUNCTION tf() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END; $$;\
         CREATE TRIGGER trg BEFORE UPDATE ON t FOR EACH ROW EXECUTE FUNCTION tf();\
         ALTER TABLE t ENABLE ROW LEVEL SECURITY;\
         CREATE POLICY p ON t FOR SELECT TO public USING (owner = current_user);\
         COMMENT ON TABLE t IS 'table';\
         COMMENT ON COLUMN t.owner IS 'owner';\
         GRANT SELECT, UPDATE ON t TO readers WITH GRANT OPTION;",
    );
    let trigger = catalog
        .triggers
        .get(&("public".into(), "t".into(), "trg".into()))
        .unwrap();
    assert_eq!(trigger.timing, TriggerTiming::Before);
    assert!(trigger.enabled);
    let policy = catalog
        .policies
        .get(&("public".into(), "t".into(), "p".into()))
        .unwrap();
    assert_eq!(policy.command, PolicyCommand::Select);
    assert!(catalog.table("public", "t").unwrap().rls_enabled);
    assert_eq!(catalog.comments.len(), 2);
    assert!(
        catalog
            .comments
            .contains_key(&pgdelta_core::ObjectRef::new(ObjectKind::Table, "public.t"))
    );
    let grant = catalog
        .grants
        .get(&(
            pgdelta_core::ObjectRef::new(ObjectKind::Table, "public.t"),
            "readers".to_owned(),
        ))
        .unwrap();
    assert!(grant.with_grant_option);
    assert!(grant.privileges.contains("SELECT"));
    assert!(grant.privileges.contains("UPDATE"));
}

#[test]
fn invalid_trigger_policy_grant_vectors_are_rejected() {
    invalid(
        "CREATE TABLE t (id integer);\
         CREATE FUNCTION tf() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END; $$;\
         CREATE CONSTRAINT TRIGGER c BEFORE UPDATE ON t FOR EACH ROW EXECUTE FUNCTION tf();",
        "AFTER ... FOR EACH ROW",
    );
    invalid(
        "CREATE TABLE t (id integer);\
         CREATE FUNCTION tf() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END; $$;\
         CREATE TRIGGER c INSTEAD OF INSERT ON t FOR EACH STATEMENT EXECUTE FUNCTION tf();",
        "FOR EACH ROW",
    );
    invalid(
        "CREATE TABLE t (id integer);\
         CREATE POLICY p ON t FOR INSERT USING (id > 0);",
        "cannot have a USING expression",
    );
    invalid(
        "CREATE TABLE t (id integer);\
         GRANT SELECT ON t TO;",
        "expected a grantee role",
    );
    invalid(
        "CREATE TABLE t (id integer);\
         CREATE TRIGGER trg BEFORE UPDATE ON t FOR EACH ROW EXECUTE FUNCTION missing();",
        "does not exist",
    );
}

#[test]
fn enum_add_value_is_planned_incrementally() {
    let from = valid("CREATE TYPE mood AS ENUM ('sad', 'ok');");
    let to = valid("CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');");
    let planned = plan(&from, &to, "from", "to").unwrap();
    let sql = pgdelta_core::plan::render_sql(&planned);
    assert!(
        sql.contains("ALTER TYPE \"public\".\"mood\" ADD VALUE 'happy'"),
        "enum append must use ADD VALUE, not a drop/recreate: {sql}"
    );
    assert!(
        !sql.contains("DROP TYPE"),
        "no destructive type replacement: {sql}"
    );
}

#[test]
fn column_type_change_drops_and_recreates_dependent_view_in_order() {
    let from = valid(
        "CREATE TABLE events (id integer, payload text);\
         CREATE VIEW event_view AS SELECT id, payload FROM events;",
    );
    let to = valid(
        "CREATE TABLE events (id integer, payload varchar(80));\
         CREATE VIEW event_view AS SELECT id, payload FROM events;",
    );
    let planned = plan(&from, &to, "from", "to").unwrap();
    let statements: Vec<&str> = planned
        .statements()
        .iter()
        .map(|statement| statement.sql.as_str())
        .collect();
    let drop_at = statements
        .iter()
        .position(|sql| sql.starts_with("DROP VIEW"))
        .expect("dependent view must be dropped");
    let alter_at = statements
        .iter()
        .position(|sql| sql.contains("ALTER COLUMN \"payload\" TYPE"))
        .expect("column type change must be planned");
    let create_at = statements
        .iter()
        .position(|sql| sql.starts_with("CREATE VIEW"))
        .expect("dependent view must be recreated");
    assert!(drop_at < alter_at, "DROP VIEW must precede the type change");
    assert!(
        alter_at < create_at,
        "CREATE VIEW must follow the type change"
    );

    let mut converged = from.clone();
    apply_sql_to(
        &mut converged,
        &pgdelta_core::plan::render_sql(&planned),
        "migration.sql",
    )
    .expect("migration must apply");
}

#[test]
fn grants_revoke_and_comments_remove_real_state() {
    let from = valid(
        "CREATE TABLE t (id integer);\
         COMMENT ON TABLE t IS 'old';\
         GRANT SELECT, INSERT ON t TO r;",
    );
    let to = valid("CREATE TABLE t (id integer);");
    let planned = plan(&from, &to, "from", "to").unwrap();
    let sql = pgdelta_core::plan::render_sql(&planned);
    assert!(
        sql.contains("COMMENT ON TABLE \"public\".\"t\" IS NULL"),
        "{sql}"
    );
    assert!(sql.contains("REVOKE"), "{sql}");
    let mut converged = from.clone();
    apply_sql_to(&mut converged, &sql, "migration.sql").expect("migration applies");
    assert!(converged.comments.is_empty());
    assert!(converged.grants.is_empty());
}
