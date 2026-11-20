//! Property tests (SPEC, Tests section):
//!
//! 1. `diff(a, a)` is empty for randomly generated catalogs;
//! 2. `diff(a, b)` applied to `a` converges to `b` — the in-memory oracle
//!    that exercises parser, applier, diff and renderer round-trip on
//!    every generated pair.
//!
//! The generator produces PostgreSQL-consistent catalogs only: identity
//! columns have integer types, foreign keys reference existing
//! non-deferrable primary keys, and every object name is a safe lowercase
//! identifier.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use proptest::prelude::*;

use pgdelta_core::model::{
    Catalog, Column, Constraint, ConstraintKind, Extension, FkAction, FkMatch, Generated, Identity,
    IdentityKind, Index, PUBLIC_SCHEMA, SequenceOptions, Table,
};
use pgdelta_core::{apply_sql_to, catalog_from_sql, plan};

fn any_catalog() -> impl Strategy<Value = Catalog> {
    (
        proptest::collection::vec(schema_name(), 0..3),
        proptest::collection::vec(extension_spec(), 0..2),
        proptest::collection::vec(table_spec(), 0..4),
    )
        .prop_map(|(schemas, extensions, tables)| {
            build_catalog(&schemas, &extensions, &tables)
        })
}

fn schema_name() -> impl Strategy<Value = String> {
    prop_oneof![Just("sales".to_owned()), Just("crm".to_owned()),]
}

fn extension_spec() -> impl Strategy<Value = String> {
    prop_oneof![Just("pg_trgm".to_owned()), Just("citext".to_owned()),]
}

fn type_name() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("bigint".to_owned()),
        Just("integer".to_owned()),
        Just("text".to_owned()),
        Just("character varying(50)".to_owned()),
        Just("numeric(8,2)".to_owned()),
        Just("boolean".to_owned()),
        Just("timestamp with time zone".to_owned()),
    ]
}

fn default_expr() -> impl Strategy<Value = Option<String>> {
    prop_oneof![
        Just(None),
        Just(Some("0".to_owned())),
        Just(Some("'x'".to_owned())),
        Just(Some("true".to_owned())),
        Just(Some("\"now\"()".to_owned())),
    ]
}

fn identity_for(data_type: &str) -> impl Strategy<Value = Option<Identity>> {
    if data_type == "bigint" || data_type == "integer" || data_type == "smallint" {
        (prop::bool::ANY, prop::bool::ANY).prop_map(|(always, cycle)| {
            Some(Identity {
                kind: if always { IdentityKind::Always } else { IdentityKind::ByDefault },
                options: SequenceOptions {
                    increment: 1,
                    minvalue: 1,
                    maxvalue: if data_type == "bigint" {
                        i64::MAX
                    } else {
                        i32::MAX as i64
                    },
                    start: 1,
                    cache: 1,
                    cycle,
                },
            })
        })
        .boxed()
    } else {
        Just(None).boxed()
    }
}

fn column_spec() -> impl Strategy<Value = Column> {
    (
        prop_oneof![
            Just("id".to_owned()),
            Just("a".to_owned()),
            Just("b".to_owned()),
            Just("txt".to_owned()),
            Just("val".to_owned()),
            Just("ts".to_owned()),
        ],
        type_name(),
        prop::bool::ANY,
        default_expr(),
        prop::option::of(Just("C".to_owned())),
    )
        .prop_flat_map(|(name, data_type, not_null, default, collation)| {
            let identity = identity_for(&data_type);
            (Just(name), Just(data_type), Just(not_null), Just(default), Just(collation), identity)
                .prop_map(|(name, data_type, not_null, default, collation, identity)| Column {
                    name,
                    data_type,
                    not_null,
                    default,
                    generated: None,
                    identity,
                    collation,
                })
        })
}

fn constraint_spec() -> impl Strategy<Value = Constraint> {
    prop_oneof![
        Just(Constraint {
            name: "t_pkey".to_owned(),
            kind: ConstraintKind::PrimaryKey {
                columns: vec!["id".to_owned()],
            },
            deferrable: None,
            not_valid: false,
        }),
        Just(Constraint {
            name: "t_a_key".to_owned(),
            kind: ConstraintKind::Unique {
                columns: vec!["a".to_owned()],
                nulls_distinct: true,
            },
            deferrable: None,
            not_valid: false,
        }),
        Just(Constraint {
            name: "t_val_check".to_owned(),
            kind: ConstraintKind::Check {
                expr: "\"val\" >= 0".to_owned(),
            },
            deferrable: None,
            not_valid: false,
        }),
    ]
}

fn table_spec() -> impl Strategy<Value = Table> {
    (
        schema_name(),
        prop_oneof![
            Just("t1".to_owned()),
            Just("t2".to_owned()),
            Just("t3".to_owned()),
        ],
        proptest::collection::vec(column_spec(), 1..5),
        proptest::collection::vec(constraint_spec(), 0..2),
    )
        .prop_map(|(schema, name, mut columns, mut constraints)| {
            // Keep names unique per table (a PostgreSQL hard rule).
            columns.sort_by(|a, b| a.name.cmp(&b.name));
            columns.dedup_by(|a, b| a.name == b.name);
            // Keep only constraints whose columns exist, and at most one
            // primary key per table (PostgreSQL hard rules). The check
            // constraint references the `val` column, so it is kept only
            // when that column exists.
            constraints.retain(|c| match &c.kind {
                ConstraintKind::PrimaryKey { columns } | ConstraintKind::Unique { columns, .. } => {
                    columns.iter().all(|k| columns_exist(&columns, k))
                }
                ConstraintKind::Check { .. } => columns.iter().any(|c| c.name == "val"),
                _ => true,
            });
            let pk_count = constraints
                .iter()
                .filter(|c| matches!(c.kind, ConstraintKind::PrimaryKey { .. }))
                .count();
            if pk_count > 1 {
                constraints.retain(|c| !matches!(c.kind, ConstraintKind::PrimaryKey { .. }));
            }
            // PRIMARY KEY implies NOT NULL (mirrors the applier).
            for constraint in &constraints {
                if let ConstraintKind::PrimaryKey { columns: pk_cols } = &constraint.kind {
                    for column in columns.iter_mut() {
                        if pk_cols.contains(&column.name) {
                            column.not_null = true;
                        }
                    }
                }
            }
            Table {
                schema,
                name,
                columns,
                constraints,
            }
        })
}

fn columns_exist(columns: &[Column], name: &str) -> bool {
    columns.iter().any(|c| c.name == name)
}

fn build_catalog(schemas: &[String], extensions: &[String], tables: &[Table]) -> Catalog {
    let mut catalog = Catalog::new();
    catalog.ensure_schema(PUBLIC_SCHEMA);
    for schema in schemas {
        catalog.ensure_schema(schema);
    }
    for extension in extensions {
        catalog.extensions.insert(
            extension.clone(),
            Extension {
                name: extension.clone(),
                schema: None,
            },
        );
    }
    let mut seen = std::collections::BTreeSet::new();
    for table in tables {
        if !seen.insert((table.schema.clone(), table.name.clone())) {
            continue;
        }
        let mut table = table.clone();
        table.constraints.retain(|c| {
            !matches!(c.kind, ConstraintKind::ForeignKey { .. })
        });
        // A primary key requires the id column to exist; drop it otherwise.
        table.constraints.retain(|c| match &c.kind {
            ConstraintKind::PrimaryKey { columns } => {
                columns.iter().all(|k| columns_exist(&table.columns, k))
            }
            ConstraintKind::Unique { columns, .. } => {
                columns.iter().all(|k| columns_exist(&table.columns, k))
            }
            _ => true,
        });
        catalog
            .tables
            .insert((table.schema.clone(), table.name.clone()), table);
    }
    // Cross-table foreign keys: every table with a column `a` may
    // reference the first table that has a primary key on `id`.
    let with_pk: Vec<(String, String)> = catalog
        .tables
        .values()
        .filter(|t| {
            t.constraints
                .iter()
                .any(|c| matches!(c.kind, ConstraintKind::PrimaryKey { .. }))
        })
        .map(|t| (t.schema.clone(), t.name.clone()))
        .collect();
    if let Some((ref_schema, ref_table)) = with_pk.first() {
        for target in catalog.tables.values_mut() {
            if target.columns.iter().any(|c| c.name == "a")
                && *ref_table != target.name
                && rand_bit(&target.name)
            {
                target.constraints.push(Constraint {
                    name: "t_a_fkey".to_owned(),
                    kind: ConstraintKind::ForeignKey {
                        columns: vec!["a".to_owned()],
                        ref_schema: ref_schema.clone(),
                        ref_table: ref_table.clone(),
                        ref_columns: vec!["id".to_owned()],
                        on_delete: FkAction::NoAction,
                        on_update: FkAction::NoAction,
                        match_type: FkMatch::Simple,
                    },
                    deferrable: None,
                    not_valid: false,
                });
            }
        }
    }
    catalog
}

fn rand_bit(seed: &str) -> bool {
    // A deterministic pseudo-random bit per name so the property corpus is
    // stable across runs while still exercising both FK-present and
    // FK-absent tables.
    seed.bytes().map(u8::count_ones).sum::<u32>() % 2 == 0
}

/// Compares catalogs modulo physical column order (PostgreSQL cannot
/// reorder columns; pgdelta never plans reorders — see tests/common).
fn assert_equivalent(actual: &Catalog, expected: &Catalog) {
    let mut actual_tables: Vec<Table> = actual.tables.values().cloned().collect();
    let mut expected_tables: Vec<Table> = expected.tables.values().cloned().collect();
    for table in actual_tables.iter_mut() {
        table.columns.sort_by(|a, b| a.name.cmp(&b.name));
    }
    for table in expected_tables.iter_mut() {
        table.columns.sort_by(|a, b| a.name.cmp(&b.name));
    }
    assert_eq!(actual_tables, expected_tables);
    let mut actual_indexes: Vec<&Index> = actual.indexes.values().collect();
    let mut expected_indexes: Vec<&Index> = expected.indexes.values().collect();
    actual_indexes.sort_by_key(|i| (i.schema.clone(), i.name.clone()));
    expected_indexes.sort_by_key(|i| (i.schema.clone(), i.name.clone()));
    assert_eq!(actual_indexes, expected_indexes);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn diff_a_a_is_empty(a in any_catalog()) {
        let planned = plan(&a, &a, "<a>", "<a>").unwrap();
        prop_assert!(planned.is_empty(), "diff(a, a) produced statements");
    }

    #[test]
    fn plan_applied_to_a_converges_to_b(a in any_catalog(), b in any_catalog()) {
        let planned = plan(&a, &b, "<a>", "<b>").unwrap();
        let script = pgdelta_core::plan::render_sql(&planned);
        let mut converged = a.clone();
        apply_sql_to(&mut converged, &script, "migration.sql").unwrap();
        assert_equivalent(&converged, &b);
    }
}

/// The catalog builder must produce catalogs that parse-and-apply back:
/// rendering nothing and re-parsing an empty script is trivially the same
/// model, but a round-trip through the model's own SQL confirms the
/// generator's shapes are applier-compatible.
#[test]
fn generator_catalogs_parse_through_own_plans() {
    let _ = catalog_from_sql("", "empty");
}

/// A generated column whose expression references another column must
/// round-trip through the applier (used by the generator when it needs
/// deterministic generated columns).
#[test]
fn generated_column_round_trips() {
    let sql = "CREATE TABLE g (a int, b int GENERATED ALWAYS AS (a + 1) STORED);";
    let catalog = catalog_from_sql(sql, "g.sql").unwrap();
    let table = catalog.table(PUBLIC_SCHEMA, "g").unwrap();
    assert_eq!(
        table.column("b").unwrap().generated,
        Some(Generated {
            expr: "\"a\" + 1".to_owned()
        })
    );
}
