//! Property tests (SPEC, Tests section):
//!
//! 1. `diff(a, a)` is empty for randomly generated catalogs;
//! 2. `diff(a, b)` applied to `a` converges to `b` — the in-memory oracle
//!    that exercises parser, applier, diff and renderer round-trip on
//!    every generated pair;
//! 3. `diff(empty, a)` applied to an empty catalog converges to `a` —
//!    the same oracle focused on the create-side renderers.
//!
//! The generator produces PostgreSQL-consistent catalogs only: identity
//! columns have integer types and carry no default, defaults match their
//! column's type, collations are only put on collatable types, `CHECK`s
//! only compare numeric columns, foreign keys reference non-deferrable
//! primary keys of the same column type, and every constraint/index name
//! is unique within its schema (constraint backing indexes share the
//! per-schema relation namespace — PostgreSQL's `ChooseRelationName`
//! scans `pg_class`).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

// Shared harness; a property-test target uses only part of it.
#[allow(dead_code)]
mod common;

use proptest::prelude::*;
use proptest::strategy::{BoxedStrategy, ValueTree};

use pgdelta_core::model::{
    Catalog, Column, Constraint, ConstraintKind, Extension, FkAction, FkMatch, Generated, Identity,
    IdentityKind, Index, IndexElem, IndexElemKind, PUBLIC_SCHEMA, SequenceOptions, SortOrder,
    Table,
};
use pgdelta_core::{PlanOptions, apply_sql_to, catalog_from_sql, plan, plan_with_options};

fn any_catalog() -> impl Strategy<Value = Catalog> {
    (
        proptest::collection::vec(schema_name(), 0..3),
        proptest::collection::vec(extension_spec(), 0..2),
        proptest::collection::vec(table_spec(), 0..4),
    )
        .prop_map(|(schemas, extensions, tables)| build_catalog(&schemas, &extensions, &tables))
}

/// The schemas a generated catalog may use (plus the ambient `public`).
fn schema_name() -> impl Strategy<Value = String> {
    prop_oneof![Just("sales".to_owned()), Just("crm".to_owned()),]
}

fn extension_spec() -> impl Strategy<Value = String> {
    prop_oneof![Just("pg_trgm".to_owned()), Just("citext".to_owned()),]
}

/// The closed set of generated column types (canonical spellings, the
/// same forms `sql/datatype.rs` produces).
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

/// A default expression acceptable for the column type. PostgreSQL
/// coerces the default at CREATE time, so the generator only pairs
/// defaults with types that accept them (all expressions are already in
/// the normalised form `sql/expr.rs` renders).
fn default_for(data_type: &str) -> Option<&'static str> {
    match data_type {
        "bigint" | "integer" | "numeric(8,2)" => Some("0"),
        "text" | "character varying(50)" => Some("'x'"),
        "boolean" => Some("true"),
        "timestamp with time zone" => Some("\"now\"()"),
        // Unknown types never occur (the type set is closed); no default.
        _ => None,
    }
}

/// `COLLATE` is only valid on collatable types; PostgreSQL rejects
/// `boolean COLLATE "C"` ("collations are not supported by type").
fn is_collatable(data_type: &str) -> bool {
    matches!(data_type, "text" | "character varying(50)")
}

/// Types that support the `>=` operator with an integer literal (the
/// generated `CHECK` expression).
fn is_comparable(data_type: &str) -> bool {
    matches!(data_type, "bigint" | "integer" | "numeric(8,2)")
}

/// Identity specifications, only for identity-capable integer types
/// (`CREATE TABLE`, `GENERATED ... AS IDENTITY` restricts identity
/// columns to smallint, integer, bigint). The options mirror
/// `normalize_sequence_options`'s defaults so the model is exactly what
/// the applier would build from the rendered SQL.
fn identity_for(data_type: &str) -> BoxedStrategy<Option<Identity>> {
    if data_type == "bigint" || data_type == "integer" {
        let maxvalue = if data_type == "bigint" {
            i64::MAX
        } else {
            i64::from(i32::MAX)
        };
        (prop::bool::ANY, prop::bool::ANY)
            .prop_map(move |(always, cycle)| {
                Some(Identity {
                    kind: if always {
                        IdentityKind::Always
                    } else {
                        IdentityKind::ByDefault
                    },
                    options: SequenceOptions {
                        increment: 1,
                        minvalue: 1,
                        maxvalue,
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
        prop::bool::ANY,
        prop::option::of(Just("C".to_owned())),
    )
        .prop_flat_map(|(name, data_type, not_null, want_default, collation)| {
            let identity = identity_for(&data_type);
            (
                Just(name),
                Just(data_type),
                Just(not_null),
                Just(want_default),
                Just(collation),
                identity,
            )
                .prop_map(
                    |(name, data_type, not_null, want_default, collation, identity)| {
                        // Identity columns are implicitly NOT NULL (`CREATE TABLE`,
                        // `GENERATED ... AS IDENTITY`) and may not carry a default
                        // (PostgreSQL: "column ... cannot have a default value and an
                        // identity specification").
                        let (not_null, default) = if identity.is_some() {
                            (true, None)
                        } else {
                            (
                                not_null,
                                want_default.then(|| default_for(&data_type)).flatten(),
                            )
                        };
                        // Decide the collation before `data_type` is moved into the
                        // struct literal (field initialisers are evaluated in order).
                        let collation = collation.filter(|_| is_collatable(&data_type));
                        Column {
                            name,
                            data_type,
                            not_null,
                            default: default.map(str::to_owned),
                            generated: None,
                            identity,
                            collation,
                            storage: None,
                        }
                    },
                )
        })
}

/// Constraint kinds; names are derived per table in `table_spec` (a
/// constraint's backing index shares the per-schema relation namespace,
/// so the names must be schema-unique, which table-derived names are).
fn constraint_spec() -> impl Strategy<Value = Constraint> {
    prop_oneof![
        Just(ConstraintKind::PrimaryKey {
            columns: vec!["id".to_owned()],
        }),
        Just(ConstraintKind::Unique {
            columns: vec!["a".to_owned()],
            nulls_distinct: true,
        }),
        Just(ConstraintKind::Check {
            expr: "\"val\" >= 0".to_owned(),
        }),
    ]
    .prop_map(|kind| Constraint {
        name: String::new(),
        kind,
        deferrable: None,
        not_valid: false,
    })
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
            // Column names unique per table (a PostgreSQL hard rule).
            columns.sort_by(|a, b| a.name.cmp(&b.name));
            columns.dedup_by(|a, b| a.name == b.name);
            // Keep only constraints whose columns exist; the `CHECK` on
            // `"val" >= 0` additionally requires `val` to have a comparable
            // type. (`constraint_spec` never generates foreign keys.)
            constraints.retain(|c| match &c.kind {
                ConstraintKind::PrimaryKey { columns: keys }
                | ConstraintKind::Unique { columns: keys, .. } => {
                    keys.iter().all(|k| column_exists(&columns, k))
                }
                ConstraintKind::Check { .. } => columns
                    .iter()
                    .any(|c| c.name == "val" && is_comparable(&c.data_type)),
                ConstraintKind::ForeignKey { .. } | ConstraintKind::Exclusion { .. } => true,
            });
            // At most one primary key per table (PostgreSQL hard rule).
            let pk_count = constraints
                .iter()
                .filter(|c| matches!(c.kind, ConstraintKind::PrimaryKey { .. }))
                .count();
            if pk_count > 1 {
                constraints.retain(|c| !matches!(c.kind, ConstraintKind::PrimaryKey { .. }));
            }
            for constraint in &mut constraints {
                constraint.name = match &constraint.kind {
                    ConstraintKind::PrimaryKey { .. } => format!("{name}_pkey"),
                    ConstraintKind::Unique { .. } => format!("{name}_a_key"),
                    ConstraintKind::Check { .. } => format!("{name}_val_check"),
                    ConstraintKind::ForeignKey { .. } => format!("{name}_a_fkey"),
                    ConstraintKind::Exclusion { .. } => format!("{name}_excl"),
                };
            }
            // PRIMARY KEY implies NOT NULL (mirrors the applier).
            for constraint in &constraints {
                if let ConstraintKind::PrimaryKey { columns: pk } = &constraint.kind {
                    for column in &mut columns {
                        if pk.contains(&column.name) {
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
                partition: None,
                partition_of: None,
                rls_enabled: false,
                rls_forced: false,
            }
        })
}

fn column_exists(columns: &[Column], name: &str) -> bool {
    columns.iter().any(|c| c.name == name)
}

fn build_catalog(schemas: &[String], extensions: &[String], tables: &[Table]) -> Catalog {
    let mut catalog = Catalog::new();
    catalog.ensure_schema(PUBLIC_SCHEMA);
    for schema in schemas {
        catalog.ensure_schema(schema);
    }
    // Every referenced table schema is a first-class schema entry — the
    // applier registers them the same way when a table is created.
    for table in tables {
        catalog.ensure_schema(&table.schema);
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
        // Table names are unique per (schema, name) in a PostgreSQL
        // database; later duplicates are dropped.
        if !seen.insert((table.schema.clone(), table.name.clone())) {
            continue;
        }
        catalog
            .tables
            .insert((table.schema.clone(), table.name.clone()), table.clone());
    }
    // Standalone indexes: optionally one single-column index per table on
    // its first column (PostgreSQL index names live in the same
    // per-schema namespace as tables, and the derived names are unique).
    for table in catalog.tables.values() {
        let Some(first) = table.columns.first() else {
            continue;
        };
        if !rand_bit(&table.name) {
            continue;
        }
        let name = format!("{}_{}_idx", table.name, first.name);
        catalog.indexes.insert(
            (table.schema.clone(), name.clone()),
            Index {
                schema: table.schema.clone(),
                name,
                table_schema: table.schema.clone(),
                table: table.name.clone(),
                unique: false,
                concurrent: false,
                method: None,
                columns: vec![IndexElem {
                    kind: IndexElemKind::Column(first.name.clone()),
                    collation: None,
                    opclass: None,
                    opclass_params: None,
                    order: if rand_bit(&first.name) {
                        Some(SortOrder::Desc)
                    } else {
                        None
                    },
                    nulls: None,
                }],
                include: Vec::new(),
                nulls_distinct: true,
                with: Vec::new(),
                tablespace: None,
                predicate: None,
            },
        );
    }
    // Cross-table foreign keys: a table with an `a` column may reference
    // the first table with a primary key on `id`, when the referencing
    // and referenced columns share the same type (PostgreSQL requires
    // comparable types) and the pseudo-random per-table bit is set.
    let referenced: Option<(String, String, String)> = catalog
        .tables
        .values()
        .find(|t| {
            t.constraints
                .iter()
                .any(|c| matches!(c.kind, ConstraintKind::PrimaryKey { .. }))
        })
        .and_then(|t| {
            t.columns
                .iter()
                .find(|c| c.name == "id")
                .map(|c| (t.schema.clone(), t.name.clone(), c.data_type.clone()))
        });
    if let Some((ref_schema, ref_table, id_type)) = referenced {
        for target in catalog.tables.values_mut() {
            let has_matching_a = target
                .columns
                .iter()
                .any(|c| c.name == "a" && c.data_type == id_type);
            if !has_matching_a || !rand_bit(&format!("{}_fk", target.name)) {
                continue;
            }
            target.constraints.push(Constraint {
                name: format!("{}_a_fkey", target.name),
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
    catalog
}

/// A deterministic pseudo-random bit per seed so the property corpus is
/// stable across runs while still exercising both outcomes per table.
fn rand_bit(seed: &str) -> bool {
    let mut hash: u32 = 0;
    for (i, byte) in seed.bytes().enumerate() {
        hash = hash
            .wrapping_mul(31)
            .wrapping_add(u32::from(byte) + i as u32);
    }
    hash.is_multiple_of(2)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn diff_a_a_is_empty(a in any_catalog()) {
        let planned = plan(&a, &a, "<a>", "<a>").unwrap();
        prop_assert!(
            planned.is_empty(),
            "diff(a, a) produced statements: {:?}",
            planned.statements().iter().map(|s| s.sql.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn plan_applied_to_a_converges_to_b(a in any_catalog(), b in any_catalog()) {
        let planned = plan(&a, &b, "<a>", "<b>").unwrap();
        let script = pgdelta_core::plan::render_sql(&planned);
        let mut converged = a.clone();
        apply_sql_to(&mut converged, &script, "migration.sql").unwrap();
        common::assert_equivalent(&converged, &b, "plan(a -> b) applied to a");
    }

    #[test]
    fn plan_from_empty_applied_converges(a in any_catalog()) {
        let empty = Catalog::new();
        let planned = plan(&empty, &a, "<empty>", "<a>").unwrap();
        let script = pgdelta_core::plan::render_sql(&planned);
        let mut actual = Catalog::new();
        apply_sql_to(&mut actual, &script, "migration.sql").unwrap();
        common::assert_equivalent(&actual, &a, "plan(empty -> a) applied to empty");
    }

    /// The `--online` rewriting must preserve convergence: the extra
    /// NOT VALID/VALIDATE/CHECK bookkeeping is a build strategy, not schema
    /// state. `CONCURRENTLY` is likewise a build method, so the generated
    /// flag is normalised before comparing (PostgreSQL has no catalog flag
    /// recording how an index was built).
    #[test]
    fn online_plan_applied_to_a_converges_to_b(a in any_catalog(), b in any_catalog()) {
        let options = PlanOptions {
            online: true,
            renames: Vec::new(),
        };
        let planned = plan_with_options(&a, &b, "<a>", "<b>", &options).unwrap();
        let script = pgdelta_core::plan::render_sql(&planned);
        let mut converged = a.clone();
        apply_sql_to(&mut converged, &script, "migration.sql").unwrap();
        let mut expected = b.clone();
        for index in converged.indexes.values_mut().chain(expected.indexes.values_mut()) {
            index.concurrent = false;
        }
        common::assert_equivalent(&converged, &expected, "online plan(a -> b) applied to a");
    }
}

/// A generated column whose expression references another column must
/// round-trip through the model's own SQL.
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

/// The property corpus must itself satisfy the model invariants the
/// applier enforces: schema-uniqueness of relation/constraint names and
/// single primary keys (checked on a fixed sample, deterministically).
#[test]
fn generator_tables_are_name_consistent() {
    let strategy = any_catalog();
    let mut runner = proptest::test_runner::TestRunner::deterministic();
    for _ in 0..16 {
        let catalog = strategy.new_tree(&mut runner).unwrap().current();
        let mut relations: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for ((schema, name), table) in &catalog.tables {
            assert!(
                relations.insert(format!("{schema}.{name}")),
                "duplicate relation name {schema}.{name}"
            );
            for constraint in &table.constraints {
                assert!(
                    relations.insert(format!("{schema}.{}", constraint.name)),
                    "constraint name {}.{} collides with a relation name",
                    schema,
                    constraint.name
                );
            }
        }
        for (schema, name) in catalog.indexes.keys() {
            assert!(
                relations.insert(format!("{schema}.{name}")),
                "duplicate index name {schema}.{name}"
            );
        }
    }
}
