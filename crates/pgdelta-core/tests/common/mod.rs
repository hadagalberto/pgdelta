//! Shared test harness: table-driven cases, the convergence oracle and
//! PostgreSQL-version gates.
//!
//! A *case* is a directory under `tests/cases/` containing `from.sql`
//! and `to.sql`. Two properties are checked for every case:
//!
//! 1. the rendered migration script snapshots deterministically (insta);
//! 2. applying the rendered script to the `from` model reproduces the
//!    `to` model, modulo column and constraint order — PostgreSQL cannot
//!    reorder columns without a destructive rewrite, so pgdelta never plans
//!    a reorder, and a foreign key added after `CREATE TABLE` has no
//!    meaningful catalog position relative to inline constraints. Neither
//!    order is part of the schema's semantics, so the oracle compares both
//!    as name-sorted sets.
//!
//! A case may carry a `min_pg_version.txt` file (e.g. `15`) when its SQL
//! uses syntax newer than PostgreSQL 13: the live-database test skips it
//! on older servers instead of weakening the assertion.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

use pgdelta_core::model::{
    Catalog, CheckOption, Column, Index, IndexElem, IndexElemKind, PUBLIC_SCHEMA, PgType,
    PgTypeKind, Routine, RoutineKind, Table, View, Volatility,
};
use pgdelta_core::{apply_sql_to, catalog_from_sql, plan};
use proptest::prelude::*;

/// One table-driven case.
pub struct Case {
    /// Directory name (used as the snapshot name).
    pub name: String,
    /// `from.sql` contents.
    pub from_sql: String,
    /// `to.sql` contents.
    pub to_sql: String,
    /// Minimum PostgreSQL major version the SQL is valid on (1 when the
    /// case carries no gate).
    pub min_pg_version: i32,
}

/// The catalog of every case, sorted by name.
pub fn load_cases() -> Vec<Case> {
    let root = cases_root();
    let mut names: Vec<PathBuf> = std::fs::read_dir(&root)
        .expect("reading the cases directory")
        .filter_map(|entry| {
            let path = entry.expect("reading a case entry").path();
            (path.is_dir() && path.join("from.sql").is_file() && path.join("to.sql").is_file())
                .then_some(path)
        })
        .collect();
    names.sort();
    names
        .into_iter()
        .map(|dir| Case {
            name: dir.file_name().unwrap().to_string_lossy().into_owned(),
            from_sql: std::fs::read_to_string(dir.join("from.sql")).unwrap(),
            to_sql: std::fs::read_to_string(dir.join("to.sql")).unwrap(),
            min_pg_version: read_min_version(&dir),
        })
        .collect()
}

pub fn cases_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/cases")
}

fn read_min_version(dir: &Path) -> i32 {
    let file = dir.join("min_pg_version.txt");
    if !file.is_file() {
        return 1;
    }
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .trim()
        .parse()
        .unwrap_or(1)
}

/// Runs the two case properties and returns the rendered script.
#[allow(dead_code)]
pub fn run_case(case: &Case) -> String {
    let from = catalog_from_sql(&case.from_sql, "from.sql").expect("parsing from.sql");
    let to = catalog_from_sql(&case.to_sql, "to.sql").expect("parsing to.sql");

    let planned = plan(
        &from,
        &to,
        &format!("<{}/from.sql>", case.name),
        &format!("<{}/to.sql>", case.name),
    )
    .expect("planning");
    let script = pgdelta_core::plan::render_sql(&planned);

    // Convergence oracle: applying the script to the `from` model must
    // reproduce the `to` model (modulo physical column order).
    let mut converged = from.clone();
    apply_sql_to(&mut converged, &script, "migration.sql")
        .expect("applying the generated migration");
    assert_equivalent(&converged, &to, &case.name);

    script
}

/// An extended generator that adds the object shapes the review found were
/// never exercised: same-named indexes with different definitions, routine
/// body changes, enum value removals and a view reading a materialized view.
/// Every variant is a plain `bool`, so two independently generated catalogs
/// regularly disagree on exactly one of those objects.
#[allow(dead_code)]
pub fn any_rich_catalog() -> impl Strategy<Value = Catalog> {
    use pgdelta_core::model::RoutineArg;

    (
        Just(Catalog::new()),
        proptest::bool::ANY,
        proptest::bool::ANY,
        proptest::bool::ANY,
        proptest::bool::ANY,
    )
        .prop_map(
            |(mut catalog, upper_index, alternate_body, short_enum, alternate_mv)| {
                catalog.ensure_schema(PUBLIC_SCHEMA);
                let public = PUBLIC_SCHEMA.to_owned();
                catalog.tables.insert(
                    (public.clone(), "rich".to_owned()),
                    Table {
                        schema: public.clone(),
                        name: "rich".to_owned(),
                        columns: vec![
                            Column {
                                name: "id".to_owned(),
                                data_type: "integer".to_owned(),
                                not_null: true,
                                default: None,
                                generated: None,
                                identity: None,
                                collation: None,
                                storage: None,
                            },
                            Column {
                                name: "txt".to_owned(),
                                data_type: "text".to_owned(),
                                not_null: false,
                                default: None,
                                generated: None,
                                identity: None,
                                collation: None,
                                storage: None,
                            },
                        ],
                        constraints: Vec::new(),
                        partition: None,
                        partition_of: None,
                        rls_enabled: false,
                        rls_forced: false,
                    },
                );
                let expression = if upper_index {
                    "\"upper\"(\"txt\")"
                } else {
                    "\"lower\"(\"txt\")"
                };
                catalog.indexes.insert(
                    (public.clone(), "rich_expr_idx".to_owned()),
                    Index {
                        schema: public.clone(),
                        name: "rich_expr_idx".to_owned(),
                        table_schema: public.clone(),
                        table: "rich".to_owned(),
                        unique: false,
                        concurrent: false,
                        method: None,
                        columns: vec![IndexElem {
                            kind: IndexElemKind::Expression(expression.to_owned()),
                            collation: None,
                            opclass: None,
                            opclass_params: None,
                            order: None,
                            nulls: None,
                        }],
                        include: Vec::new(),
                        nulls_distinct: true,
                        with: Vec::new(),
                        tablespace: None,
                        predicate: None,
                    },
                );
                catalog.routines.insert(
                    (public.clone(), "rich_fn".to_owned(), String::new()),
                    Routine {
                        schema: public.clone(),
                        name: "rich_fn".to_owned(),
                        kind: RoutineKind::Function,
                        args: Vec::<RoutineArg>::new(),
                        return_type: Some("integer".to_owned()),
                        returns_setof: false,
                        return_table: Vec::new(),
                        language: "sql".to_owned(),
                        body: if alternate_body {
                            "select 2".to_owned()
                        } else {
                            "select 1".to_owned()
                        },
                        volatility: Volatility::Stable,
                        security: pgdelta_core::model::Security::Invoker,
                        strict: false,
                        leakproof: false,
                        window: false,
                        parallel: None,
                        cost: None,
                        rows: None,
                        settings: Vec::new(),
                    },
                );
                catalog.types.insert(
                    (public.clone(), "rich_enum".to_owned()),
                    PgType {
                        schema: public.clone(),
                        name: "rich_enum".to_owned(),
                        kind: PgTypeKind::Enum {
                            values: if short_enum {
                                vec!["a".to_owned()]
                            } else {
                                vec!["a".to_owned(), "b".to_owned()]
                            },
                        },
                    },
                );
                catalog.views.insert(
                    (public.clone(), "rich_mv".to_owned()),
                    View {
                        schema: public.clone(),
                        name: "rich_mv".to_owned(),
                        columns: Vec::new(),
                        query: if alternate_mv {
                            "SELECT id, txt FROM rich".to_owned()
                        } else {
                            "SELECT id FROM rich".to_owned()
                        },
                        options: Vec::new(),
                        tablespace: None,
                        access_method: None,
                        check_option: CheckOption::None,
                        materialized: true,
                        with_data: true,
                    },
                );
                catalog.views.insert(
                    (public.clone(), "rich_v".to_owned()),
                    View {
                        schema: public,
                        name: "rich_v".to_owned(),
                        columns: Vec::new(),
                        query: "SELECT id FROM rich_mv".to_owned(),
                        options: Vec::new(),
                        tablespace: None,
                        access_method: None,
                        check_option: CheckOption::None,
                        materialized: false,
                        with_data: true,
                    },
                );
                catalog
            },
        )
}

/// Model equality modulo physical column order (see the module docs).
#[allow(dead_code)]
pub fn assert_equivalent(actual: &Catalog, expected: &Catalog, context: &str) {
    // Schemas: every non-ambient schema matches.
    let actual_schemas: Vec<&String> = actual
        .schemas
        .keys()
        .filter(|s| *s != PUBLIC_SCHEMA)
        .collect();
    let expected_schemas: Vec<&String> = expected
        .schemas
        .keys()
        .filter(|s| *s != PUBLIC_SCHEMA)
        .collect();
    assert_eq!(
        actual_schemas, expected_schemas,
        "{context}: schemas diverge"
    );

    assert_eq!(
        actual.extensions, expected.extensions,
        "{context}: extensions diverge"
    );

    let mut actual_tables: Vec<Table> = actual.tables.values().cloned().collect();
    let mut expected_tables: Vec<Table> = expected.tables.values().cloned().collect();
    for table in &mut actual_tables {
        table.columns.sort_by(|a, b| a.name.cmp(&b.name));
        table.constraints.sort_by(|a, b| a.name.cmp(&b.name));
    }
    for table in &mut expected_tables {
        table.columns.sort_by(|a, b| a.name.cmp(&b.name));
        table.constraints.sort_by(|a, b| a.name.cmp(&b.name));
    }
    assert_eq!(
        actual_tables, expected_tables,
        "{context}: tables diverge (by-name column/constraint comparison)"
    );

    let mut actual_indexes: Vec<_> = actual.indexes.values().cloned().collect();
    let mut expected_indexes: Vec<_> = expected.indexes.values().cloned().collect();
    actual_indexes.sort_by_key(|i| (i.schema.clone(), i.name.clone()));
    expected_indexes.sort_by_key(|i| (i.schema.clone(), i.name.clone()));
    assert_eq!(
        actual_indexes, expected_indexes,
        "{context}: indexes diverge"
    );

    // Milestone-2 object collections are compared as ordered-by-key vectors
    // so that HashMap-style iteration never makes a snapshot flaky.
    assert_eq!(
        actual.types.iter().collect::<Vec<_>>(),
        expected.types.iter().collect::<Vec<_>>(),
        "{context}: types diverge"
    );
    assert_eq!(
        actual.sequences.iter().collect::<Vec<_>>(),
        expected.sequences.iter().collect::<Vec<_>>(),
        "{context}: sequences diverge"
    );
    assert_eq!(
        actual.views.iter().collect::<Vec<_>>(),
        expected.views.iter().collect::<Vec<_>>(),
        "{context}: views diverge"
    );
    assert_eq!(
        actual.routines.iter().collect::<Vec<_>>(),
        expected.routines.iter().collect::<Vec<_>>(),
        "{context}: routines diverge"
    );
    assert_eq!(
        actual.triggers.iter().collect::<Vec<_>>(),
        expected.triggers.iter().collect::<Vec<_>>(),
        "{context}: triggers diverge"
    );
    assert_eq!(
        actual.policies.iter().collect::<Vec<_>>(),
        expected.policies.iter().collect::<Vec<_>>(),
        "{context}: policies diverge"
    );
    assert_eq!(
        actual.partitions.iter().collect::<Vec<_>>(),
        expected.partitions.iter().collect::<Vec<_>>(),
        "{context}: partitions diverge"
    );
    assert_eq!(
        actual.comments, expected.comments,
        "{context}: comments diverge"
    );
    assert_eq!(actual.grants, expected.grants, "{context}: grants diverge");
}
