//! Milestone 4 behaviour: safety analysis, `--online` rewriting, JSON plans,
//! opt-in rename hints and the lint analyser. Every test applies the
//! generated script back to the source model (the same convergence oracle as
//! the table-driven suite) and asserts known-valid / known-invalid vectors.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[allow(dead_code)]
mod common;

use pgdelta_core::model::Catalog;
use pgdelta_core::{
    HazardCategory, PlanOptions, RenameHint, analyze_statements, apply_sql_to, catalog_from_sql,
    parse_rename_hint, plan, plan_with_options, render_hazard_report, render_lint_json,
    split_sql_statements,
};

fn make(
    from_sql: &str,
    to_sql: &str,
    online: bool,
    renames: Vec<RenameHint>,
) -> (pgdelta_core::Plan, String, Catalog, Catalog) {
    let from = catalog_from_sql(from_sql, "<from>").unwrap();
    let to = catalog_from_sql(to_sql, "<to>").unwrap();
    let options = PlanOptions { online, renames };
    let planned = plan_with_options(&from, &to, "<from>", "<to>", &options).unwrap();
    let sql = pgdelta_core::plan::render_sql(&planned);
    (planned, sql, from, to)
}

#[test]
fn offline_plan_reports_real_hazards() {
    let from =
        "CREATE TABLE public.old (id integer); CREATE TABLE public.t (a bigint, email text);";
    let to = "CREATE TABLE public.t (a integer, email text NOT NULL);";
    let (planned, _, _, _) = make(from, to, false, Vec::new());
    let summary = planned.summary();
    assert!(summary.hazards >= 3, "summary: {summary:?}");
    assert!(summary.destructive >= 1, "drop table must be destructive");
    assert!(summary.locking >= 1, "SET NOT NULL must be locking");
    let codes: Vec<_> = planned
        .statements()
        .iter()
        .flat_map(|s| s.hazards.iter())
        .map(|h| h.code)
        .collect();
    assert!(codes.contains(&"drop-table"), "{codes:?}");
    assert!(codes.contains(&"not-null-scan"), "{codes:?}");
    assert!(codes.contains(&"type-narrowing"), "{codes:?}");
}

#[test]
fn online_fk_add_uses_not_valid_then_validate_and_converges() {
    let from = "CREATE TABLE public.p (id integer PRIMARY KEY); CREATE TABLE public.c (id integer, pid integer);";
    let to = "CREATE TABLE public.p (id integer PRIMARY KEY); CREATE TABLE public.c (id integer, pid integer); \
              ALTER TABLE public.c ADD CONSTRAINT c_pid_fkey FOREIGN KEY (pid) REFERENCES public.p(id);";
    let (planned, sql, from_cat, to_cat) = make(from, to, true, Vec::new());
    let other = plan(&from_cat, &to_cat, "<from>", "<to>").unwrap();
    let other_sql = pgdelta_core::plan::render_sql(&other);
    assert!(other_sql.contains("FOREIGN KEY"), "{other_sql}");
    assert!(
        !other_sql.contains("NOT VALID"),
        "offline FK add must be validated inline: {other_sql}"
    );

    assert!(sql.contains("NOT VALID"), "{sql}");
    assert!(sql.contains("VALIDATE CONSTRAINT"), "{sql}");
    let mut converged = from_cat.clone();
    apply_sql_to(&mut converged, &sql, "migration.sql").unwrap();
    common::assert_equivalent(&converged, &to_cat, "online FK add");
    assert!(
        planned
            .statements()
            .iter()
            .all(|s| !s.hazards.iter().any(|h| h.code == "constraint-scan")),
        "rewritten FK must have no constraint-scan hazard"
    );
}

#[test]
fn online_set_not_null_adds_and_drops_a_validated_check_and_converges() {
    let from = "CREATE TABLE public.t (id integer, email text);";
    let to = "CREATE TABLE public.t (id integer, email text NOT NULL);";
    let (_, sql, from_cat, to_cat) = make(from, to, true, Vec::new());
    assert!(
        sql.contains("CHECK (\"email\" IS NOT NULL) NOT VALID"),
        "{sql}"
    );
    assert!(sql.contains("VALIDATE CONSTRAINT"), "{sql}");
    assert!(sql.contains("SET NOT NULL"), "{sql}");
    assert!(sql.contains("DROP CONSTRAINT"), "{sql}");
    let mut converged = from_cat.clone();
    apply_sql_to(&mut converged, &sql, "migration.sql").unwrap();
    common::assert_equivalent(&converged, &to_cat, "online SET NOT NULL");
    // No leftover helper constraint: the final catalog must equal `to`.
}

#[test]
fn online_check_add_uses_not_valid_and_converges() {
    let from = "CREATE TABLE public.t (id integer);";
    let to = "CREATE TABLE public.t (id integer); ALTER TABLE public.t ADD CONSTRAINT t_id_check CHECK (id >= 0);";
    let (_, sql, from_cat, to_cat) = make(from, to, true, Vec::new());
    assert!(sql.contains("CHECK (\"id\" >= 0) NOT VALID"), "{sql}");
    assert!(sql.contains("VALIDATE CONSTRAINT"), "{sql}");
    let mut converged = from_cat.clone();
    apply_sql_to(&mut converged, &sql, "migration.sql").unwrap();
    common::assert_equivalent(&converged, &to_cat, "online CHECK add");
}

#[test]
fn online_index_is_concurrent_and_non_transactional() {
    let from = "CREATE TABLE public.t (id integer);";
    let to = "CREATE TABLE public.t (id integer); CREATE INDEX t_id_idx ON public.t (id);";
    let (planned, sql, from_cat, to_cat) = make(from, to, true, Vec::new());
    assert!(sql.contains("CREATE INDEX CONCURRENTLY"), "{sql}");
    assert!(sql.contains("Non-transactional section"), "{sql}");
    let last = planned.sections.last().unwrap();
    assert!(!last.transactional);
    assert!(last.statements.iter().all(|s| !s.transactional));
    assert!(
        planned
            .statements()
            .iter()
            .all(|s| !s.hazards.iter().any(|h| h.code == "non-concurrent-index"))
    );
    let mut converged = from_cat.clone();
    apply_sql_to(&mut converged, &sql, "migration.sql").unwrap();
    // CONCURRENTLY is a build method, not persistent catalog state: the
    // model records it, but a concurrent build yields the same index as a
    // standard one. Normalise it before comparing schema semantics.
    for index in converged.indexes.values_mut() {
        index.concurrent = false;
    }
    common::assert_equivalent(&converged, &to_cat, "online index build");
}

#[test]
fn online_keeps_transactional_index_build_on_partitioned_tables() {
    // PostgreSQL does not support CREATE INDEX CONCURRENTLY on a
    // partitioned parent (CREATE INDEX docs). The standard build stays and
    // keeps its locking hazard so the operator sees it.
    let base = "CREATE TABLE public.p (id integer) PARTITION BY RANGE (id); \
                CREATE TABLE public.p1 PARTITION OF public.p FOR VALUES FROM (0) TO (10);";
    let to = format!("{base} CREATE INDEX p_id_idx ON public.p (id);");
    let (planned, sql, _, _) = make(base, &to, true, Vec::new());
    assert!(sql.contains("CREATE INDEX \"p_id_idx\""), "{sql}");
    assert!(!sql.contains("CONCURRENTLY"), "{sql}");
    assert!(
        planned
            .statements()
            .iter()
            .any(|s| s.hazards.iter().any(|h| h.code == "non-concurrent-index"))
    );
}

#[test]
fn json_plan_includes_hazards_and_summary_counts() {
    let from = "CREATE TABLE public.old (id integer);";
    let to = "";
    let (planned, _, _, _) = make(from, to, false, Vec::new());
    let json = pgdelta_core::plan::render_json(&planned).unwrap();
    assert_eq!(json["summary"]["hazards"], 1);
    assert_eq!(json["summary"]["destructive"], 1);
    assert_eq!(json["summary"]["locking"], 0);
    let hazards = &json["sections"][0]["statements"][0]["hazards"];
    assert_eq!(hazards[0]["code"], "drop-table");
    assert_eq!(hazards[0]["category"], "destructive");
    assert!(
        hazards[0]["message"]
            .as_str()
            .unwrap()
            .contains("DROP TABLE")
    );
}

#[test]
fn rename_hints_emit_rename_statements_and_converge() {
    let from = "CREATE TABLE public.users (id integer PRIMARY KEY, email text); \
                CREATE TABLE public.posts (id integer, user_id integer REFERENCES public.users(id)); \
                CREATE INDEX users_email_idx ON public.users (email);";
    let to = "CREATE TABLE public.people (id integer PRIMARY KEY, contact text); \
              CREATE TABLE public.posts (id integer, user_id integer REFERENCES public.people(id)); \
              CREATE INDEX people_email_idx ON public.people (contact);";
    let hints = [
        parse_rename_hint("table.users=people").unwrap(),
        parse_rename_hint("column.people.email=contact").unwrap(),
        parse_rename_hint("index.users_email_idx=people_email_idx").unwrap(),
    ];
    let (_, sql, from_cat, to_cat) = make(from, to, false, hints.to_vec());
    assert!(sql.contains("RENAME TO \"people\""), "{sql}");
    assert!(
        sql.contains("RENAME COLUMN \"email\" TO \"contact\""),
        "{sql}"
    );
    assert!(sql.contains("RENAME TO \"people_email_idx\""), "{sql}");
    assert!(!sql.contains("DROP TABLE \"public\".\"users\""), "{sql}");
    let mut converged = from_cat.clone();
    apply_sql_to(&mut converged, &sql, "migration.sql").unwrap();
    common::assert_equivalent(&converged, &to_cat, "rename hints");
}

#[test]
fn rename_hints_are_strict_and_never_guessed() {
    let from = "CREATE TABLE public.a (id integer);";
    let to = "CREATE TABLE public.b (id integer);";
    // Without a hint the honest plan is drop + create.
    let (_, sql, _, _) = make(from, to, false, Vec::new());
    assert!(sql.contains("DROP TABLE \"public\".\"a\""), "{sql}");
    assert!(!sql.contains("RENAME TO"), "{sql}");
    // A hint pointing at a missing source object is rejected, not ignored.
    let from_cat = catalog_from_sql(from, "<from>").unwrap();
    let to_cat = catalog_from_sql(to, "<to>").unwrap();
    let options = PlanOptions {
        online: false,
        renames: vec![parse_rename_hint("table.missing=b").unwrap()],
    };
    assert!(plan_with_options(&from_cat, &to_cat, "<from>", "<to>", &options).is_err());
    assert!(parse_rename_hint("column.email=contact").is_err());
}

#[test]
fn lint_analyses_handwritten_migrations_with_known_vectors() {
    let safe = "CREATE TABLE public.t (id integer);\n\
                ALTER TABLE public.t ADD CONSTRAINT c CHECK (id >= 0) NOT VALID;\n\
                ALTER TABLE public.t VALIDATE CONSTRAINT c;\n\
                CREATE INDEX CONCURRENTLY t_id_idx ON public.t (id);";
    let unsafe_sql = "BEGIN;\n\
                      DROP TABLE public.old;\n\
                      ALTER TABLE public.t ALTER COLUMN id SET NOT NULL;\n\
                      CREATE INDEX CONCURRENTLY t_id_idx ON public.t (id);\n\
                      COMMIT;";

    for (sql, expected_hazards) in [(safe, 0), (unsafe_sql, 3)] {
        let statements = split_sql_statements(sql).unwrap();
        let inputs: Vec<_> = statements
            .iter()
            .map(|s| pgdelta_core::StatementInput::sql(s))
            .collect();
        let hazards = analyze_statements(&inputs);
        let total: usize = hazards.iter().map(Vec::len).sum();
        assert_eq!(total, expected_hazards, "{sql}: {hazards:?}");
        let json = render_lint_json("<script>", &statements, &hazards).unwrap();
        assert_eq!(json["summary"]["statements"], statements.len() as u64);
        assert_eq!(json["summary"]["hazards"], total as u64);
    }

    let statements = split_sql_statements(unsafe_sql).unwrap();
    let inputs: Vec<_> = statements
        .iter()
        .map(|s| pgdelta_core::StatementInput::sql(s))
        .collect();
    let hazards = analyze_statements(&inputs);
    let codes: Vec<_> = hazards.iter().flatten().map(|h| h.code).collect();
    assert!(codes.contains(&"drop-table"), "{codes:?}");
    assert!(codes.contains(&"not-null-scan"), "{codes:?}");
    assert!(
        codes.contains(&"concurrent-index-in-transaction"),
        "{codes:?}"
    );
    assert!(
        hazards
            .iter()
            .flatten()
            .any(|h| h.category == HazardCategory::Destructive)
    );

    let lines: Vec<_> = statements
        .iter()
        .zip(&hazards)
        .enumerate()
        .filter(|(_, (_, h))| !h.is_empty())
        .map(|(i, (sql, h))| {
            pgdelta_core::HazardLine::new(format!("statement {}", i + 1), sql.clone(), h.clone())
        })
        .collect();
    let report = render_hazard_report(&lines, pgdelta_core::ColorChoice::Never);
    assert!(report.contains("Hazards: 3"));
    assert!(!report.contains('\u{1b}'));
}

#[test]
fn online_rewrite_preserves_empty_plans() {
    let from = "CREATE TABLE public.t (id integer);";
    let (planned, sql, from_cat, _) = make(from, from, true, Vec::new());
    assert!(planned.is_empty());
    assert_eq!(sql, "");
    assert_eq!(planned.summary().hazards, 0);
    let mut unchanged = from_cat.clone();
    apply_sql_to(&mut unchanged, &sql, "migration.sql").unwrap();
    common::assert_equivalent(&unchanged, &from_cat, "empty online plan");
}
