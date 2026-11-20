//! Migration planning: catalog changes become an ordered, dependency-
//! sorted plan rendered as SQL (or JSON).
//!
//! Statement ordering (ranks in `plan_statements`, deterministic
//! tie-breaks by key; see `dep.rs`):
//!
//! * drops: foreign keys on surviving tables that reference dropped
//!   tables are unnecessary as a special case — constraint drops rank
//!   below column drops, which rank below table drops. The full drop
//!   order is constraints → columns → tables → extensions → schemas
//!   (a dropped table's indexes/constraints die with it and are never
//!   emitted separately);
//! * creates: schemas → extensions → tables (with inline non-FK
//!   constraints) → column/constraint alterations → foreign-key
//!   additions (last, after every referenced table and column exists) →
//!   indexes;
//! * `CREATE INDEX CONCURRENTLY` builds cannot run inside a transaction
//!   block (PostgreSQL docs, `CREATE INDEX`, "Building Indexes
//!   Concurrently"), so they land in a separate non-transactional
//!   section after `COMMIT`.
//!
//! Every statement renders with always-quoted identifiers (`pg_dump`'s
//! convention; unconditionally valid and immune to keyword collisions).
//!
//! Statement SQL is assembled with `push_str(&format!(...))` throughout:
//! the lint around that pattern is disabled for this module because
//! `write!`-based assembly would require discarding `fmt::Result`s (the
//! library forbids `unwrap`/`expect`), and the render path is not
//! performance-critical.
#![allow(clippy::format_push_string)]

use serde::Serialize;
use serde_json::Value;

use crate::dep::{self, PlanNode};
use crate::diff::{AlterChange, Change};
use crate::error::Result;
use crate::model::{
    Catalog, Column, Constraint, ConstraintKind, Deferrable, FkAction, FkMatch, Index,
    IndexElemKind, NullsOrder, SortOrder, Table,
};
use crate::naming::{qualified, quote_ident};
use crate::rename::{RenameHint, RenameOp};
use crate::safety::Hazard;
use crate::sql::sequence_options_to_sql;

/// A single planned statement.
#[derive(Debug, Clone)]
pub struct PlanStatement {
    /// The SQL text: exactly one statement, `;`-terminated.
    pub sql: String,
    /// Broad action category (for summaries and `--json`).
    pub action: &'static str,
    /// The affected object, for `--json`.
    pub object: String,
    /// Whether the statement may run inside a transaction.
    pub transactional: bool,
    /// Safety hazards attached by [`crate::safety::analyze_plan`]. Empty
    /// when the statement is online-safe.
    pub hazards: Vec<Hazard>,
}

/// A section of the migration script.
#[derive(Debug, Clone)]
pub struct PlanSection {
    /// `true` for the transactional block (`BEGIN; ... COMMIT;`).
    pub transactional: bool,
    /// The statements, in order.
    pub statements: Vec<PlanStatement>,
}

/// The migration plan.
#[derive(Debug, Clone)]
pub struct Plan {
    /// All sections, transactional first.
    pub sections: Vec<PlanSection>,
    /// Description of the source (`--from`).
    pub from_desc: String,
    /// Description of the target (`--to`).
    pub to_desc: String,
}

impl Plan {
    /// Every statement in order.
    #[must_use]
    pub fn statements(&self) -> Vec<&PlanStatement> {
        self.sections
            .iter()
            .flat_map(|s| s.statements.iter())
            .collect()
    }

    /// Every statement in order, mutably.
    pub(crate) fn statements_mut(&mut self) -> impl Iterator<Item = &mut PlanStatement> {
        self.sections
            .iter_mut()
            .flat_map(|s| s.statements.iter_mut())
    }

    /// True when the plan has no statements (the schemas already
    /// converge).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.statements().is_empty()
    }

    /// Counts statements per action category and hazards per kind.
    #[must_use]
    pub fn summary(&self) -> PlanSummary {
        let mut summary = PlanSummary::default();
        for statement in self.statements() {
            match statement.action {
                "create" => summary.creates += 1,
                "alter" => summary.alters += 1,
                "drop" => summary.drops += 1,
                _ => {}
            }
            for hazard in &statement.hazards {
                summary.hazards += 1;
                match hazard.category {
                    crate::safety::HazardCategory::Destructive => summary.destructive += 1,
                    crate::safety::HazardCategory::Locking => summary.locking += 1,
                }
            }
        }
        summary.total = self.statements().len();
        summary
    }
}

/// Plan summary counts (used by the CLI and `--json`).
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct PlanSummary {
    /// Total statements.
    pub total: usize,
    /// `CREATE ...` statements.
    pub creates: usize,
    /// `ALTER ...` statements.
    pub alters: usize,
    /// `DROP ...` statements.
    pub drops: usize,
    /// Total safety hazards across all statements.
    pub hazards: usize,
    /// Destructive hazards.
    pub destructive: usize,
    /// Locking hazards.
    pub locking: usize,
}

/// Ranks (broad phases) for the dependency sort.
mod rank {
    /// `DROP VIEW` of a view depending on a changing/dropped relation.
    pub(crate) const DROP_VIEW: u32 = 5;
    /// `DROP TRIGGER`.
    pub(crate) const DROP_TRIGGER: u32 = 6;
    /// `DROP POLICY`.
    pub(crate) const DROP_POLICY: u32 = 7;
    /// `REVOKE`.
    pub(crate) const DROP_GRANT: u32 = 8;
    /// `COMMENT ... IS NULL`.
    pub(crate) const DROP_COMMENT: u32 = 9;
    /// `DROP PARTITION`.
    pub(crate) const DROP_PARTITION: u32 = 10;
    /// `DROP FUNCTION`/`DROP PROCEDURE`.
    pub(crate) const DROP_ROUTINE: u32 = 35;
    /// `DROP TYPE`/`DROP DOMAIN`.
    pub(crate) const DROP_TYPE: u32 = 36;
    /// `DROP SEQUENCE`.
    pub(crate) const DROP_SEQUENCE: u32 = 37;
    /// `CREATE TYPE`/`CREATE DOMAIN`.
    pub(crate) const CREATE_TYPE: u32 = 105;
    /// `CREATE SEQUENCE`.
    pub(crate) const CREATE_SEQUENCE: u32 = 106;
    /// `ALTER TYPE ... ADD VALUE` / `RENAME VALUE`.
    pub(crate) const ALTER_TYPE: u32 = 115;
    /// `ALTER SEQUENCE`.
    pub(crate) const ALTER_SEQUENCE: u32 = 116;
    /// `CREATE FUNCTION`/`CREATE PROCEDURE`.
    pub(crate) const CREATE_ROUTINE: u32 = 125;
    /// `CREATE VIEW`/`CREATE MATERIALIZED VIEW` (after altering tables).
    pub(crate) const CREATE_VIEW: u32 = 140;
    /// `CREATE TRIGGER`.
    pub(crate) const CREATE_TRIGGER: u32 = 172;
    /// `CREATE POLICY` / RLS flags.
    pub(crate) const CREATE_POLICY: u32 = 174;
    /// `CREATE TABLE ... PARTITION OF` / `ATTACH PARTITION`.
    pub(crate) const CREATE_PARTITION: u32 = 176;
    /// `COMMENT ON`.
    pub(crate) const SET_COMMENT: u32 = 180;
    /// `GRANT`.
    pub(crate) const SET_GRANT: u32 = 182;
    /// `ALTER TABLE ... DROP CONSTRAINT` on surviving tables.
    pub(crate) const DROP_CONSTRAINT: u32 = 10;
    /// `ALTER TABLE ... DROP COLUMN`.
    pub(crate) const DROP_COLUMN: u32 = 20;
    /// `DROP TABLE`.
    pub(crate) const DROP_TABLE: u32 = 30;
    /// `DROP EXTENSION`.
    pub(crate) const DROP_EXTENSION: u32 = 40;
    /// `DROP SCHEMA`.
    pub(crate) const DROP_SCHEMA: u32 = 50;
    /// `CREATE SCHEMA`.
    pub(crate) const CREATE_SCHEMA: u32 = 100;
    /// `CREATE EXTENSION`.
    pub(crate) const CREATE_EXTENSION: u32 = 110;
    /// `CREATE TABLE`.
    pub(crate) const CREATE_TABLE: u32 = 120;
    /// Column/constraint alterations on surviving tables.
    pub(crate) const ALTER: u32 = 130;
    /// `ALTER TABLE ... ADD CONSTRAINT ... FOREIGN KEY` (after all tables
    /// exist and all columns are added).
    pub(crate) const ADD_FOREIGN_KEY: u32 = 160;
    /// `CREATE INDEX` (transactional).
    pub(crate) const CREATE_INDEX: u32 = 170;
    /// `CREATE INDEX CONCURRENTLY` (non-transactional section).
    pub(crate) const CREATE_INDEX_CONCURRENTLY: u32 = 180;
}

/// Options controlling planning (`--online` and `--rename`).
#[derive(Debug, Clone, Default)]
pub struct PlanOptions {
    /// Rewrite the plan into online-safe statement sequences.
    pub online: bool,
    /// Opt-in rename hints. Never guessed by the planner itself.
    pub renames: Vec<RenameHint>,
}

/// Plans a migration with explicit options.
///
/// # Errors
///
/// Returns [`Error::Invalid`] for a rename hint that cannot be applied,
/// and [`Error::Internal`] if the dependency graph contains a cycle.
pub fn plan_with_options(
    from: &Catalog,
    to: &Catalog,
    from_desc: &str,
    to_desc: &str,
    options: &PlanOptions,
) -> Result<Plan> {
    let (renamed_from, renames) = crate::rename::apply_rename_hints(from, to, &options.renames)?;
    let mut planned = plan_impl(&renamed_from, to, from_desc, to_desc, renames)?;
    if options.online {
        crate::safety::rewrite_online(&mut planned, from, to);
    }
    crate::safety::analyze_plan(&mut planned, &renamed_from, to);
    Ok(planned)
}

/// Plans a migration from `from` to `to` with the default (conservative)
/// options: no renames and no online rewriting.
///
/// Returns an error only on an internal planning bug (a dependency
/// cycle); model-level inconsistencies were already rejected when the
/// catalogs were built.
///
/// # Errors
///
/// Returns [`Error::Internal`] if the dependency graph contains a cycle
/// (a pgdelta bug, not a user condition).
pub fn plan(from: &Catalog, to: &Catalog, from_desc: &str, to_desc: &str) -> Result<Plan> {
    plan_with_options(from, to, from_desc, to_desc, &PlanOptions::default())
}

fn plan_impl(
    from: &Catalog,
    to: &Catalog,
    from_desc: &str,
    to_desc: &str,
    renames: Vec<RenameOp>,
) -> Result<Plan> {
    let changes = crate::diff::diff(from, to);
    let mut nodes: Vec<PlanNode<PlanStatement>> = Vec::new();

    // Rename hints run before every drop/create: later statements already
    // reference the new names (the hints were applied to the source model).
    for rename in renames {
        nodes.push(PlanNode {
            rank: rename.rank,
            key: rename.key,
            payload: PlanStatement {
                sql: rename.sql,
                action: "alter",
                object: rename.object,
                transactional: true,
                hazards: Vec::new(),
            },
            after: Vec::new(),
        });
    }

    // Drop phase.
    for change in &changes {
        match change {
            Change::DropSchema { name } => {
                nodes.push(node(
                    rank::DROP_SCHEMA,
                    format!("drop-schema:{name}"),
                    "drop",
                    name.clone(),
                    format!("DROP SCHEMA {};", quote_ident(name)),
                ));
            }
            Change::DropExtension { name } => {
                nodes.push(node(
                    rank::DROP_EXTENSION,
                    format!("drop-extension:{name}"),
                    "drop",
                    name.clone(),
                    format!("DROP EXTENSION {};", quote_ident(name)),
                ));
            }
            Change::DropTable { schema, name } => {
                // Views reading this table must disappear first: PostgreSQL
                // refuses DROP TABLE while a view depends on it (RESTRICT).
                let mut after = Vec::new();
                for (view_schema, view_name) in from.views.keys() {
                    if view_schema == schema {
                        // Cheap conservative edge: any view in the same
                        // schema is dropped before the table; only views
                        // that actually read it are emitted by the diff.
                        let key = format!("drop-view:{view_schema}.{view_name}");
                        if changes.iter().any(|c| matches!(c, Change::DropView { schema: s, name: n, .. } if s == view_schema && n == view_name)) {
                            after.push(key);
                        }
                    }
                }
                nodes.push(PlanNode {
                    rank: rank::DROP_TABLE,
                    key: format!("drop-table:{schema}.{name}"),
                    payload: PlanStatement {
                        sql: format!("DROP TABLE {};", qualified(schema, name)),
                        action: "drop",
                        object: qualified(schema, name),
                        transactional: true,
                        hazards: Vec::new(),
                    },
                    after,
                });
            }
            Change::DropIndex { schema, name } => {
                nodes.push(node(
                    rank::DROP_CONSTRAINT,
                    format!("drop-index:{schema}.{name}"),
                    "drop",
                    qualified(schema, name),
                    format!("DROP INDEX {};", qualified(schema, name)),
                ));
            }
            Change::DropView {
                schema,
                name,
                materialized,
            } => {
                let object = qualified(schema, name);
                nodes.push(node(
                    rank::DROP_VIEW,
                    format!("drop-view:{schema}.{name}"),
                    "drop",
                    object.clone(),
                    format!(
                        "DROP {} {};",
                        if *materialized {
                            "MATERIALIZED VIEW"
                        } else {
                            "VIEW"
                        },
                        object
                    ),
                ));
            }
            Change::DropTrigger {
                schema,
                table,
                name,
            } => {
                nodes.push(node(
                    rank::DROP_TRIGGER,
                    format!("drop-trigger:{schema}.{table}.{name}"),
                    "drop",
                    format!("{table}.{name}"),
                    format!(
                        "DROP TRIGGER {} ON {};",
                        quote_ident(name),
                        qualified(schema, table)
                    ),
                ));
            }
            Change::DropPolicy {
                schema,
                table,
                name,
            } => {
                nodes.push(node(
                    rank::DROP_POLICY,
                    format!("drop-policy:{schema}.{table}.{name}"),
                    "drop",
                    format!("{table}.{name}"),
                    format!(
                        "DROP POLICY {} ON {};",
                        quote_ident(name),
                        qualified(schema, table)
                    ),
                ));
            }
            Change::DropType { schema, name } => {
                nodes.push(node(
                    rank::DROP_TYPE,
                    format!("drop-type:{schema}.{name}"),
                    "drop",
                    qualified(schema, name),
                    format!("DROP TYPE {};", qualified(schema, name)),
                ));
            }
            Change::DropSequence { schema, name } => {
                nodes.push(node(
                    rank::DROP_SEQUENCE,
                    format!("drop-sequence:{schema}.{name}"),
                    "drop",
                    qualified(schema, name),
                    format!("DROP SEQUENCE {};", qualified(schema, name)),
                ));
            }
            Change::DropRoutine {
                schema,
                name,
                signature,
                kind,
            } => {
                nodes.push(node(
                    rank::DROP_ROUTINE,
                    format!("drop-routine:{schema}.{name}.{signature}"),
                    "drop",
                    qualified(schema, name),
                    format!(
                        "DROP {} {}({});",
                        kind.as_sql(),
                        qualified(schema, name),
                        signature
                    ),
                ));
            }
            Change::DropPartition { schema, name } => {
                nodes.push(node(
                    rank::DROP_PARTITION,
                    format!("drop-partition:{schema}.{name}"),
                    "drop",
                    qualified(schema, name),
                    format!("DROP TABLE {};", qualified(schema, name)),
                ));
            }
            Change::DropComment { object } => {
                nodes.push(node(
                    rank::DROP_COMMENT,
                    format!("drop-comment:{}:{}", object.kind.as_sql(), object.name),
                    "drop",
                    object.name.clone(),
                    format!("COMMENT ON {} IS NULL;", object.render()),
                ));
            }
            Change::DropGrant {
                object,
                grantee,
                privileges,
            } => {
                let list = privileges.iter().cloned().collect::<Vec<_>>().join(", ");
                nodes.push(node(
                    rank::DROP_GRANT,
                    format!(
                        "revoke:{}:{}:{}",
                        object.kind.as_sql(),
                        object.name,
                        grantee
                    ),
                    "drop",
                    object.name.clone(),
                    format!(
                        "REVOKE {list} ON {} FROM {};",
                        object.render(),
                        quote_ident(grantee)
                    ),
                ));
            }
            Change::ReplaceType { pg_type } => {
                let key = format!("replace-type:{}:{}", pg_type.schema, pg_type.name);
                nodes.push(node(
                    rank::DROP_TYPE,
                    format!("{key}:drop"),
                    "drop",
                    qualified(&pg_type.schema, &pg_type.name),
                    format!("DROP TYPE {};", qualified(&pg_type.schema, &pg_type.name)),
                ));
                nodes.push(node(
                    rank::CREATE_TYPE,
                    format!("{key}:create"),
                    "create",
                    qualified(&pg_type.schema, &pg_type.name),
                    render_create_type(pg_type),
                ));
            }
            Change::AlterTable {
                schema,
                name,
                actions,
            } => {
                for action in actions {
                    match action {
                        AlterChange::DropConstraint { name: constraint } => {
                            nodes.push(node(
                                rank::DROP_CONSTRAINT,
                                format!("drop-constraint:{schema}.{name}.{constraint}"),
                                "drop",
                                format!("{name}.{constraint}"),
                                format!(
                                    "ALTER TABLE {} DROP CONSTRAINT {};",
                                    qualified(schema, name),
                                    quote_ident(constraint)
                                ),
                            ));
                        }
                        AlterChange::DropColumn { name: column } => {
                            nodes.push(node(
                                rank::DROP_COLUMN,
                                format!("drop-column:{schema}.{name}.{column}"),
                                "drop",
                                format!("{name}.{column}"),
                                format!(
                                    "ALTER TABLE {} DROP COLUMN {};",
                                    qualified(schema, name),
                                    quote_ident(column)
                                ),
                            ));
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    // Create phase.
    for change in &changes {
        match change {
            Change::CreateSchema { name } => {
                nodes.push(node(
                    rank::CREATE_SCHEMA,
                    format!("create-schema:{name}"),
                    "create",
                    name.clone(),
                    format!("CREATE SCHEMA {};", quote_ident(name)),
                ));
            }
            Change::CreateExtension { extension } => {
                let schema = extension
                    .schema
                    .as_deref()
                    .map_or_default(|s| format!(" WITH SCHEMA {}", quote_ident(s)));
                nodes.push(node(
                    rank::CREATE_EXTENSION,
                    format!("create-extension:{}", extension.name),
                    "create",
                    extension.name.clone(),
                    format!("CREATE EXTENSION {}{schema};", quote_ident(&extension.name)),
                ));
            }
            Change::CreateTable { table } => {
                nodes.push(node(
                    rank::CREATE_TABLE,
                    format!("create-table:{}:{}", table.schema, table.name),
                    "create",
                    qualified(&table.schema, &table.name),
                    render_create_table(table),
                ));
                // Inline non-FK, valid constraints ship with the table;
                // FKs are added afterwards so that forward references and
                // mutual references never fail. `NOT VALID` constraints
                // cannot appear in CREATE TABLE (PostgreSQL docs, ALTER
                // TABLE, ADD ... NOT VALID), so they are added separately.
                for constraint in &table.constraints {
                    if matches!(constraint.kind, ConstraintKind::ForeignKey { .. }) {
                        nodes.push(node(
                            rank::ADD_FOREIGN_KEY,
                            format!("add-fk:{}:{}:{}", table.schema, table.name, constraint.name),
                            "alter",
                            format!("{}.{}", table.name, constraint.name),
                            render_add_fk(&table.schema, &table.name, constraint),
                        ));
                    } else if constraint.not_valid {
                        nodes.push(node(
                            rank::ALTER,
                            format!(
                                "add-not-valid:{}:{}:{}",
                                table.schema, table.name, constraint.name
                            ),
                            "alter",
                            format!("{}.{}", table.name, constraint.name),
                            format!(
                                "ALTER TABLE {} ADD CONSTRAINT {} {};",
                                qualified(&table.schema, &table.name),
                                quote_ident(&constraint.name),
                                render_constraint_body(constraint)
                            ),
                        ));
                    }
                }
            }
            Change::AlterTable {
                schema,
                name,
                actions,
            } => {
                for (position, action) in actions.iter().enumerate() {
                    for (rank_value, key_suffix, object, sql) in
                        render_alter_statements(schema, name, action)
                    {
                        let key = format!("alter:{schema}:{name}:{position:03}:{key_suffix}");
                        nodes.push(node(rank_value, key, "alter", object, sql));
                    }
                }
            }
            Change::CreateIndex { index } => {
                let (rank_value, key, transactional) = if index.concurrent {
                    (
                        rank::CREATE_INDEX_CONCURRENTLY,
                        format!(
                            "create-index-c:{}:{}:{}",
                            index.schema, index.table, index.name
                        ),
                        false,
                    )
                } else {
                    (
                        rank::CREATE_INDEX,
                        format!(
                            "create-index:{}:{}:{}",
                            index.schema, index.table, index.name
                        ),
                        true,
                    )
                };
                nodes.push(PlanNode {
                    rank: rank_value,
                    key,
                    payload: PlanStatement {
                        sql: render_create_index(index),
                        action: "create",
                        object: qualified(&index.schema, &index.name),
                        transactional,
                        hazards: Vec::new(),
                    },
                    after: Vec::new(),
                });
            }
            Change::CreateType { pg_type } => {
                nodes.push(node(
                    rank::CREATE_TYPE,
                    format!("create-type:{}:{}", pg_type.schema, pg_type.name),
                    "create",
                    qualified(&pg_type.schema, &pg_type.name),
                    render_create_type(pg_type),
                ));
            }
            Change::AlterEnum {
                schema,
                name,
                append,
                renames,
            } => {
                let object = qualified(schema, name);
                for (position, value) in append.iter().enumerate() {
                    nodes.push(node(
                        rank::ALTER_TYPE,
                        format!("enum-add:{schema}:{name}:{position}"),
                        "alter",
                        object.clone(),
                        format!(
                            "ALTER TYPE {object} ADD VALUE {};",
                            crate::sql::expr::render_string_literal(value)
                        ),
                    ));
                }
                for (from, to) in renames {
                    nodes.push(node(
                        rank::ALTER_TYPE,
                        format!("enum-rename:{schema}:{name}:{from}"),
                        "alter",
                        object.clone(),
                        format!(
                            "ALTER TYPE {object} RENAME VALUE {} TO {};",
                            crate::sql::expr::render_string_literal(from),
                            crate::sql::expr::render_string_literal(to)
                        ),
                    ));
                }
            }
            Change::UpsertSequence { sequence, create } => {
                let sql = if *create {
                    render_create_sequence(sequence)
                } else {
                    render_alter_sequence(sequence)
                };
                nodes.push(node(
                    if *create {
                        rank::CREATE_SEQUENCE
                    } else {
                        rank::ALTER_SEQUENCE
                    },
                    format!("sequence:{}:{}", sequence.schema, sequence.name),
                    if *create { "create" } else { "alter" },
                    qualified(&sequence.schema, &sequence.name),
                    sql,
                ));
            }
            Change::CreateView { view } => {
                let object = qualified(&view.schema, &view.name);
                // A view depends on every table it reads: when those tables
                // are created in the same plan, the view follows them.
                let mut after = Vec::new();
                for (schema, table) in view.referenced_relations() {
                    let key = format!("create-table:{schema}.{table}");
                    if nodes.iter().any(|n| n.key == key) {
                        after.push(key);
                    }
                }
                nodes.push(PlanNode {
                    rank: rank::CREATE_VIEW,
                    key: format!("create-view:{}:{}", view.schema, view.name),
                    payload: PlanStatement {
                        sql: render_create_view(view),
                        action: "create",
                        object: object.clone(),
                        transactional: true,
                        hazards: Vec::new(),
                    },
                    after,
                });
            }
            Change::UpsertRoutine { routine, create } => {
                nodes.push(node(
                    rank::CREATE_ROUTINE,
                    format!(
                        "routine:{}:{}:{}",
                        routine.schema,
                        routine.name,
                        routine.signature()
                    ),
                    if *create { "create" } else { "alter" },
                    format!("{}.{}", routine.name, routine.signature()),
                    render_create_routine(routine),
                ));
            }
            Change::CreateTrigger { trigger } => {
                let key = format!(
                    "create-trigger:{}:{}:{}",
                    trigger.schema, trigger.table, trigger.name
                );
                nodes.push(node(
                    rank::CREATE_TRIGGER,
                    key.clone(),
                    "create",
                    format!("{}.{}", trigger.table, trigger.name),
                    render_create_trigger(trigger),
                ));
                if !trigger.enabled {
                    nodes.push(node(
                        rank::CREATE_TRIGGER + 1,
                        format!("{key}:disable"),
                        "alter",
                        format!("{}.{}", trigger.table, trigger.name),
                        format!(
                            "ALTER TRIGGER {} ON {} DISABLE;",
                            quote_ident(&trigger.name),
                            qualified(&trigger.schema, &trigger.table)
                        ),
                    ));
                }
            }
            Change::CreatePolicy { policy } => {
                nodes.push(node(
                    rank::CREATE_POLICY,
                    format!(
                        "create-policy:{}:{}:{}",
                        policy.schema, policy.table, policy.name
                    ),
                    "create",
                    format!("{}.{}", policy.table, policy.name),
                    render_create_policy(policy),
                ));
            }
            Change::SetTableRls {
                schema,
                table,
                enabled,
                forced,
            } => {
                let object = qualified(schema, table);
                let mut statements = Vec::new();
                if *enabled {
                    statements.push(format!("ALTER TABLE {object} ENABLE ROW LEVEL SECURITY;"));
                } else {
                    statements.push(format!("ALTER TABLE {object} DISABLE ROW LEVEL SECURITY;"));
                }
                if *forced {
                    statements.push(format!("ALTER TABLE {object} FORCE ROW LEVEL SECURITY;"));
                } else {
                    statements.push(format!("ALTER TABLE {object} NO FORCE ROW LEVEL SECURITY;"));
                }
                for (i, sql) in statements.into_iter().enumerate() {
                    nodes.push(node(
                        rank::CREATE_POLICY,
                        format!("rls:{schema}:{table}:{i}"),
                        "alter",
                        object.clone(),
                        sql,
                    ));
                }
            }
            Change::SetComment { object, text } => {
                nodes.push(node(
                    rank::SET_COMMENT,
                    format!("comment:{}:{}", object.kind.as_sql(), object.name),
                    "create",
                    object.name.clone(),
                    format!(
                        "COMMENT ON {} IS {};",
                        object.render(),
                        crate::sql::expr::render_string_literal(text)
                    ),
                ));
            }
            Change::SetGrant { grant } => {
                let list = grant
                    .privileges
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ");
                let option = if grant.with_grant_option {
                    " WITH GRANT OPTION"
                } else {
                    ""
                };
                nodes.push(node(
                    rank::SET_GRANT,
                    format!(
                        "grant:{}:{}:{}",
                        grant.object.kind.as_sql(),
                        grant.object.name,
                        grant.grantee
                    ),
                    "create",
                    grant.object.name.clone(),
                    format!(
                        "GRANT {list} ON {} TO {}{option};",
                        grant.object.render(),
                        quote_ident(&grant.grantee)
                    ),
                ));
            }
            Change::SetPartitioning {
                schema,
                table,
                partition,
            } => {
                let object = qualified(schema, table);
                let sql = match partition {
                    Some(partition) => {
                        format!("ALTER TABLE {object} {}", render_partition_by(partition))
                    }
                    None => format!("ALTER TABLE {object} REMOVE PARTITIONING;"),
                };
                nodes.push(node(
                    rank::ALTER,
                    format!("partitioning:{schema}:{table}"),
                    "alter",
                    object,
                    sql,
                ));
            }
            Change::CreatePartition { partition } => {
                let object = qualified(&partition.schema, &partition.name);
                let parent = qualified(&partition.parent_schema, &partition.parent_table);
                let bound = if partition.is_default {
                    "DEFAULT".to_owned()
                } else {
                    format!("FOR VALUES {}", partition.bound)
                };
                let sql = if from
                    .tables
                    .contains_key(&(partition.schema.clone(), partition.name.clone()))
                {
                    // The table already exists as a standalone table: use
                    // ATTACH PARTITION rather than trying to create it again.
                    format!("ALTER TABLE {parent} ATTACH PARTITION {object} {bound};")
                } else {
                    format!("CREATE TABLE {object} PARTITION OF {parent} {bound};")
                };
                nodes.push(node(
                    rank::CREATE_PARTITION,
                    format!("create-partition:{}:{}", partition.schema, partition.name),
                    "create",
                    object.clone(),
                    sql,
                ));
            }
            _ => {}
        }
    }

    let sorted = dep::toposort(nodes)?;

    let mut sections = Vec::new();
    let transactional: Vec<PlanStatement> =
        sorted.iter().filter(|s| s.transactional).cloned().collect();
    let non_transactional: Vec<PlanStatement> = sorted
        .iter()
        .filter(|s| !s.transactional)
        .cloned()
        .collect();
    if !transactional.is_empty() {
        sections.push(PlanSection {
            transactional: true,
            statements: transactional,
        });
    }
    if !non_transactional.is_empty() {
        sections.push(PlanSection {
            transactional: false,
            statements: non_transactional,
        });
    }
    Ok(Plan {
        sections,
        from_desc: from_desc.to_owned(),
        to_desc: to_desc.to_owned(),
    })
}

fn node(
    rank: u32,
    key: String,
    action: &'static str,
    object: String,
    sql: String,
) -> PlanNode<PlanStatement> {
    PlanNode {
        rank,
        key,
        payload: PlanStatement {
            sql,
            action,
            object,
            transactional: true,
            hazards: Vec::new(),
        },
        after: Vec::new(),
    }
}

/// Renders one alter action as one or more statements
/// `(rank, key_suffix, object, sql)`.
///
/// `ALTER COLUMN ... TYPE` with a default expands into the `pg_dump`
/// pattern — drop the default, change the type, re-apply the default —
/// as three separate statements, each valid on every supported
/// PostgreSQL version.
fn render_alter_statements(
    schema: &str,
    table: &str,
    action: &AlterChange,
) -> Vec<(u32, String, String, String)> {
    let target = qualified(schema, table);
    match action {
        AlterChange::DropConstraint { .. } | AlterChange::DropColumn { .. } => Vec::new(),
        AlterChange::AlterColumnType {
            column,
            data_type,
            collation,
            from_default,
            to_default,
        } => {
            let mut out = Vec::new();
            if from_default.is_some() {
                out.push((
                    rank::ALTER,
                    format!("type-0-drop-default:{column}"),
                    format!("{table}.{column}"),
                    format!(
                        "ALTER TABLE {target} ALTER COLUMN {} DROP DEFAULT;",
                        quote_ident(column)
                    ),
                ));
            }
            let mut sql = format!(
                "ALTER TABLE {target} ALTER COLUMN {} TYPE {}",
                quote_ident(column),
                render_type(data_type)
            );
            if let Some(collation) = collation {
                sql.push_str(&format!(" COLLATE {}", render_collation(collation)));
            }
            sql.push(';');
            out.push((
                rank::ALTER,
                format!("type-1-change:{column}"),
                format!("{table}.{column}"),
                sql,
            ));
            if let Some(default) = to_default {
                out.push((
                    rank::ALTER,
                    format!("type-2-set-default:{column}"),
                    format!("{table}.{column}"),
                    format!(
                        "ALTER TABLE {target} ALTER COLUMN {} SET DEFAULT {};",
                        quote_ident(column),
                        default
                    ),
                ));
            }
            out
        }
        AlterChange::SetDefault { column, expr } => vec![(
            rank::ALTER,
            format!("default:{column}"),
            format!("{table}.{column}"),
            format!(
                "ALTER TABLE {target} ALTER COLUMN {} SET DEFAULT {};",
                quote_ident(column),
                expr
            ),
        )],
        AlterChange::DropDefault { column } => vec![(
            rank::ALTER,
            format!("default:{column}"),
            format!("{table}.{column}"),
            format!(
                "ALTER TABLE {target} ALTER COLUMN {} DROP DEFAULT;",
                quote_ident(column)
            ),
        )],
        AlterChange::SetStorage { column, storage } => vec![(
            rank::ALTER,
            format!("storage:{column}"),
            format!("{table}.{column}"),
            format!(
                "ALTER TABLE {target} ALTER COLUMN {} SET STORAGE {};",
                quote_ident(column),
                storage.map_or("DEFAULT", crate::model::ColumnStorage::as_sql)
            ),
        )],
        AlterChange::SetNotNull { column } => vec![(
            rank::ALTER,
            format!("notnull:{column}"),
            format!("{table}.{column}"),
            format!(
                "ALTER TABLE {target} ALTER COLUMN {} SET NOT NULL;",
                quote_ident(column)
            ),
        )],
        AlterChange::DropNotNull { column } => vec![(
            rank::ALTER,
            format!("notnull:{column}"),
            format!("{table}.{column}"),
            format!(
                "ALTER TABLE {target} ALTER COLUMN {} DROP NOT NULL;",
                quote_ident(column)
            ),
        )],
        AlterChange::AddIdentity { column, identity } => vec![(
            rank::ALTER,
            format!("identity:{column}"),
            format!("{table}.{column}"),
            format!(
                "ALTER TABLE {target} ALTER COLUMN {} ADD {};",
                quote_ident(column),
                render_identity_spec(identity)
            ),
        )],
        AlterChange::DropIdentity { column } => vec![(
            rank::ALTER,
            format!("identity:{column}"),
            format!("{table}.{column}"),
            format!(
                "ALTER TABLE {target} ALTER COLUMN {} DROP IDENTITY;",
                quote_ident(column)
            ),
        )],
        AlterChange::SetGeneratedKind { column, kind } => {
            let kind = match kind {
                crate::model::IdentityKind::Always => "ALWAYS",
                crate::model::IdentityKind::ByDefault => "BY DEFAULT",
            };
            vec![(
                rank::ALTER,
                format!("identity:{column}"),
                format!("{table}.{column}"),
                format!(
                    "ALTER TABLE {target} ALTER COLUMN {} SET GENERATED {kind};",
                    quote_ident(column)
                ),
            )]
        }
        AlterChange::SetSequenceOption { column, option } => {
            use crate::sql::alter_table::SeqOption;
            let option_sql = match option {
                SeqOption::Increment(v) => format!("INCREMENT BY {v}"),
                SeqOption::MinValue(v) => format!("MINVALUE {v}"),
                SeqOption::MaxValue(v) => format!("MAXVALUE {v}"),
                SeqOption::Start(v) => format!("START WITH {v}"),
                SeqOption::Cache(v) => format!("CACHE {v}"),
                SeqOption::Cycle(true) => "CYCLE".to_owned(),
                SeqOption::Cycle(false) => "NO CYCLE".to_owned(),
            };
            vec![(
                rank::ALTER,
                format!("identity:{column}"),
                format!("{table}.{column}"),
                format!(
                    "ALTER TABLE {target} ALTER COLUMN {} SET {option_sql};",
                    quote_ident(column)
                ),
            )]
        }
        AlterChange::AddColumn { column } => vec![(
            rank::ALTER,
            format!("add-column:{}", column.name),
            format!("{table}.{}", column.name),
            format!(
                "ALTER TABLE {target} ADD COLUMN {};",
                render_column_def(column)
            ),
        )],
        AlterChange::AddConstraint { constraint } => {
            let body = render_constraint_body(constraint);
            if matches!(constraint.kind, ConstraintKind::ForeignKey { .. }) {
                vec![(
                    rank::ADD_FOREIGN_KEY,
                    format!("add-fk:{}", constraint.name),
                    format!("{table}.{}", constraint.name),
                    render_add_fk(schema, table, constraint),
                )]
            } else {
                vec![(
                    rank::ALTER,
                    format!("add-constraint:{}", constraint.name),
                    format!("{table}.{}", constraint.name),
                    format!(
                        "ALTER TABLE {target} ADD CONSTRAINT {} {body};",
                        quote_ident(&constraint.name)
                    ),
                )]
            }
        }
        AlterChange::ValidateConstraint { name } => vec![(
            rank::ALTER,
            format!("validate:{name}"),
            format!("{table}.{name}"),
            format!(
                "ALTER TABLE {target} VALIDATE CONSTRAINT {};",
                quote_ident(name)
            ),
        )],
        AlterChange::AlterConstraintDeferrable { name, deferrable } => {
            let tail = match deferrable {
                None => "NOT DEFERRABLE",
                Some(Deferrable {
                    initially_deferred: true,
                    ..
                }) => "DEFERRABLE INITIALLY DEFERRED",
                Some(_) => "DEFERRABLE INITIALLY IMMEDIATE",
            };
            vec![(
                rank::ALTER,
                format!("alter-constraint:{name}"),
                format!("{table}.{name}"),
                format!(
                    "ALTER TABLE {target} ALTER CONSTRAINT {} {tail};",
                    quote_ident(name)
                ),
            )]
        }
    }
}

/// Renders `GENERATED {ALWAYS|BY DEFAULT} AS IDENTITY (...)`.
fn render_identity_spec(identity: &crate::model::Identity) -> String {
    let kind = match identity.kind {
        crate::model::IdentityKind::Always => "ALWAYS",
        crate::model::IdentityKind::ByDefault => "BY DEFAULT",
    };
    format!(
        "GENERATED {kind} AS IDENTITY ({})",
        sequence_options_to_sql(&identity.options)
    )
}

/// Renders `CREATE TABLE` with its non-FK constraints inline.
fn render_create_table(table: &Table) -> String {
    let mut lines: Vec<String> = Vec::new();
    for column in &table.columns {
        lines.push(format!("    {}", render_column_def(column)));
    }
    for constraint in &table.constraints {
        // PostgreSQL forbids `NOT VALID` inside CREATE TABLE; the planner
        // emits those constraints as separate ALTER statements.
        if !matches!(constraint.kind, ConstraintKind::ForeignKey { .. }) && !constraint.not_valid {
            lines.push(format!(
                "    CONSTRAINT {} {}",
                quote_ident(&constraint.name),
                render_constraint_body(constraint)
            ));
        }
    }
    let mut out = format!(
        "CREATE TABLE {} (\n{}\n)",
        qualified(&table.schema, &table.name),
        lines.join(",\n")
    );
    if let Some(partition) = &table.partition {
        out.push(' ');
        out.push_str(render_partition_by(partition).trim_end_matches(';'));
    }
    out.push(';');
    out
}

/// Renders a column definition: name, type, then the canonical option
/// order STORAGE → COLLATE → DEFAULT → GENERATED (stored/identity) → NOT NULL.
fn render_column_def(column: &Column) -> String {
    let mut out = format!(
        "{} {}",
        quote_ident(&column.name),
        render_type(&column.data_type)
    );
    if let Some(storage) = &column.storage {
        out.push_str(&format!(" STORAGE {}", storage.as_sql()));
    }
    if let Some(collation) = &column.collation {
        out.push_str(&format!(" COLLATE {}", render_collation(collation)));
    }
    if let Some(default) = &column.default {
        out.push_str(&format!(" DEFAULT {default}"));
    }
    if let Some(generated) = &column.generated {
        out.push_str(&format!(" GENERATED ALWAYS AS ({}) STORED", generated.expr));
    }
    if let Some(identity) = &column.identity {
        out.push(' ');
        out.push_str(&render_identity_spec(identity));
    }
    if column.not_null {
        out.push_str(" NOT NULL");
    }
    out
}

/// Renders a constraint body (without the `CONSTRAINT name` prefix).
fn render_constraint_body(constraint: &Constraint) -> String {
    let mut out = match &constraint.kind {
        ConstraintKind::PrimaryKey { columns } => {
            format!("PRIMARY KEY ({})", render_ident_list(columns))
        }
        ConstraintKind::Unique {
            columns,
            nulls_distinct,
        } => {
            let nulls: &str = if *nulls_distinct {
                ""
            } else {
                "NULLS NOT DISTINCT "
            };
            format!("UNIQUE {nulls}({})", render_ident_list(columns))
        }
        ConstraintKind::Check { expr } => format!("CHECK ({expr})"),
        ConstraintKind::Exclusion { spec } => format!("EXCLUDE {spec}"),
        ConstraintKind::ForeignKey { .. } => render_fk_body(constraint),
    };
    if let Some(Deferrable {
        initially_deferred, ..
    }) = &constraint.deferrable
    {
        out.push_str(" DEFERRABLE");
        out.push_str(if *initially_deferred {
            " INITIALLY DEFERRED"
        } else {
            " INITIALLY IMMEDIATE"
        });
    }
    if constraint.not_valid {
        out.push_str(" NOT VALID");
    }
    out
}

/// Renders `ALTER TABLE ... ADD CONSTRAINT ... FOREIGN KEY ...`.
fn render_add_fk(schema: &str, table: &str, constraint: &Constraint) -> String {
    format!(
        "ALTER TABLE {} ADD CONSTRAINT {} {};",
        qualified(schema, table),
        quote_ident(&constraint.name),
        render_fk_body(constraint)
    )
}

/// Renders the FOREIGN KEY clause.
fn render_fk_body(constraint: &Constraint) -> String {
    let ConstraintKind::ForeignKey {
        columns,
        ref_schema,
        ref_table,
        ref_columns,
        on_delete,
        on_update,
        match_type,
    } = &constraint.kind
    else {
        unreachable!("render_fk_body called on a non-FK constraint")
    };
    let mut out = format!(
        "FOREIGN KEY ({}) REFERENCES {} ({})",
        render_ident_list(columns),
        qualified(ref_schema, ref_table),
        render_ident_list(ref_columns)
    );
    if *match_type != FkMatch::Simple {
        out.push(' ');
        out.push_str(match_type.to_sql());
    }
    if *on_delete != FkAction::NoAction {
        out.push_str(&format!(" ON DELETE {}", on_delete.to_sql()));
    }
    if *on_update != FkAction::NoAction {
        out.push_str(&format!(" ON UPDATE {}", on_update.to_sql()));
    }
    if let Some(Deferrable {
        initially_deferred, ..
    }) = &constraint.deferrable
    {
        out.push_str(" DEFERRABLE");
        out.push_str(if *initially_deferred {
            " INITIALLY DEFERRED"
        } else {
            " INITIALLY IMMEDIATE"
        });
    }
    if constraint.not_valid {
        out.push_str(" NOT VALID");
    }
    out
}

/// Renders `CREATE [UNIQUE] INDEX [CONCURRENTLY] ...`.
fn render_create_index(index: &Index) -> String {
    let mut out = String::from("CREATE ");
    if index.unique {
        out.push_str("UNIQUE ");
    }
    out.push_str("INDEX ");
    if index.concurrent {
        out.push_str("CONCURRENTLY ");
    }
    // CREATE INDEX does not accept a schema-qualified index name: the index
    // is always created in the table's schema. DROP INDEX, in contrast, may
    // be qualified (PostgreSQL docs, CREATE INDEX / DROP INDEX).
    out.push_str(&quote_ident(&index.name));
    out.push_str(" ON ");
    out.push_str(&qualified(&index.table_schema, &index.table));
    if let Some(method) = &index.method {
        out.push_str(&format!(" USING {} ", quote_ident(method)));
    }
    let elems: Vec<String> = index
        .columns
        .iter()
        .map(|elem| {
            let mut e = match &elem.kind {
                IndexElemKind::Column(c) => quote_ident(c),
                IndexElemKind::Expression(expr) => format!("({expr})"),
            };
            if let Some(collation) = &elem.collation {
                e.push_str(&format!(" COLLATE {}", render_collation(collation)));
            }
            if let Some(opclass) = &elem.opclass {
                e.push(' ');
                e.push_str(&quote_parts(opclass));
                if let Some(params) = &elem.opclass_params {
                    e.push_str(&format!(" ({params})"));
                }
            }
            if let Some(order) = &elem.order {
                e.push(' ');
                e.push_str(match order {
                    SortOrder::Asc => "ASC",
                    SortOrder::Desc => "DESC",
                });
            }
            if let Some(nulls) = &elem.nulls {
                e.push(' ');
                e.push_str(match nulls {
                    NullsOrder::First => "NULLS FIRST",
                    NullsOrder::Last => "NULLS LAST",
                });
            }
            e
        })
        .collect();
    out.push_str(&format!(" ({})", elems.join(", ")));
    if !index.include.is_empty() {
        out.push_str(&format!(" INCLUDE ({})", render_ident_list(&index.include)));
    }
    if index.unique && !index.nulls_distinct {
        out.push_str(" NULLS NOT DISTINCT");
    }
    if !index.with.is_empty() {
        let params: Vec<String> = index
            .with
            .iter()
            .map(|(k, v)| format!("{} = {}", quote_ident(k), v))
            .collect();
        out.push_str(&format!(" WITH ({})", params.join(", ")));
    }
    if let Some(tablespace) = &index.tablespace {
        out.push_str(&format!(" TABLESPACE {}", quote_ident(tablespace)));
    }
    if let Some(predicate) = &index.predicate {
        out.push_str(&format!(" WHERE {predicate}"));
    }
    out.push(';');
    out
}

fn render_ident_list(idents: &[String]) -> String {
    idents
        .iter()
        .map(|i| quote_ident(i))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Renders a canonical type string. Builtins are display-ready;
/// qualified custom names quote each part.
pub(crate) fn render_type(canonical: &str) -> String {
    if !canonical.contains('.') {
        return canonical.to_owned();
    }
    quote_parts(canonical)
}

/// Renders a dotted name with every part quoted.
fn quote_parts(name: &str) -> String {
    crate::naming::quote_parts(name)
}

/// Renders a collation name (parts quoted individually).
fn render_collation(name: &str) -> String {
    quote_parts(name)
}

/// Renders the plan as a SQL script.
///
/// The transactional section is wrapped in an explicit `BEGIN; ...
/// COMMIT;` block; a non-transactional section (concurrent index
/// builds) follows with a clear separator comment. An empty plan
/// renders as an empty string — nothing to do.
#[must_use]
pub fn render_sql(plan: &Plan) -> String {
    if plan.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str(concat!(
        "-- pgdelta ",
        env!("CARGO_PKG_VERSION"),
        " migration\n"
    ));
    out.push_str(&format!("-- from: {}\n", plan.from_desc));
    out.push_str(&format!("-- to:   {}\n", plan.to_desc));
    out.push_str("--\n");
    out.push_str("-- The first block runs inside a single transaction.\n");
    out.push_str("-- Any following block must run outside a transaction block.\n\n");
    for section in &plan.sections {
        if section.transactional {
            out.push_str("BEGIN;\n\n");
            for statement in &section.statements {
                out.push_str(&statement.sql);
                out.push_str("\n\n");
            }
            out.push_str("COMMIT;\n");
        } else {
            out.push_str(
                "\n-- Non-transactional section (cannot run inside a transaction block):\n\n",
            );
            for statement in &section.statements {
                out.push_str(&statement.sql);
                out.push_str("\n\n");
            }
        }
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

/// Renders the plan as JSON (`--json`).
///
/// # Errors
///
/// Returns [`Error::Serialization`] if the plan cannot be serialized
/// (every statement renders plain strings, so this is a pgdelta bug).
pub fn render_json(plan: &Plan) -> Result<Value> {
    #[derive(Serialize)]
    struct JsonStatement<'a> {
        action: &'a str,
        object: &'a str,
        sql: &'a str,
        transactional: bool,
        hazards: &'a [Hazard],
    }
    let mut sections = Vec::new();
    for section in &plan.sections {
        let statements: Vec<JsonStatement> = section
            .statements
            .iter()
            .map(|s| JsonStatement {
                action: s.action,
                object: &s.object,
                sql: &s.sql,
                transactional: section.transactional,
                hazards: &s.hazards,
            })
            .collect();
        sections.push(serde_json::json!({
            "transactional": section.transactional,
            "statements": statements,
        }));
    }
    Ok(serde_json::json!({
        "from": plan.from_desc,
        "to": plan.to_desc,
        "summary": plan.summary(),
        "sections": sections,
    }))
}

// ---------------------------------------------------------------------------
// Rendering helpers for milestone-2 object kinds.
// ---------------------------------------------------------------------------

/// Renders `CREATE TYPE` / `CREATE DOMAIN`.
fn render_create_type(pg_type: &crate::model::PgType) -> String {
    use crate::model::PgTypeKind;
    let name = qualified(&pg_type.schema, &pg_type.name);
    match &pg_type.kind {
        PgTypeKind::Enum { values } => {
            let labels = values
                .iter()
                .map(|v| crate::sql::expr::render_string_literal(v))
                .collect::<Vec<_>>()
                .join(", ");
            format!("CREATE TYPE {name} AS ENUM ({labels});")
        }
        PgTypeKind::Composite { attributes } => {
            let attrs = attributes
                .iter()
                .map(|a| {
                    let mut out = format!("{} {}", quote_ident(&a.name), render_type(&a.data_type));
                    if let Some(collation) = &a.collation {
                        out.push_str(&format!(" COLLATE {}", render_collation(collation)));
                    }
                    out
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("CREATE TYPE {name} AS ({attrs});")
        }
        PgTypeKind::Domain {
            base_type,
            collation,
            default,
            constraints,
        } => {
            let mut out = format!("CREATE DOMAIN {name} AS {}", render_type(base_type));
            if let Some(collation) = collation {
                out.push_str(&format!(" COLLATE {}", render_collation(collation)));
            }
            if let Some(default) = default {
                out.push_str(&format!(" DEFAULT {default}"));
            }
            for constraint in constraints {
                out.push_str(" CONSTRAINT ");
                out.push_str(&quote_ident(&constraint.name));
                match &constraint.kind {
                    crate::model::DomainConstraintKind::NotNull => out.push_str(" NOT NULL"),
                    crate::model::DomainConstraintKind::Check { expr } => {
                        out.push_str(&format!(" CHECK ({expr})"));
                    }
                }
            }
            out.push(';');
            out
        }
        PgTypeKind::Range {
            subtype,
            subtype_opclass,
            collation,
            canonical,
            subtype_diff,
            multirange_type_name,
        } => {
            let mut options = vec![format!("SUBTYPE = {}", render_type(subtype))];
            if let Some(v) = subtype_opclass {
                options.push(format!("SUBTYPE_OPCLASS = {v}"));
            }
            if let Some(v) = collation {
                options.push(format!("COLLATION = {v}"));
            }
            if let Some(v) = canonical {
                options.push(format!("CANONICAL = {v}"));
            }
            if let Some(v) = subtype_diff {
                options.push(format!("SUBTYPE_DIFF = {v}"));
            }
            if let Some(v) = multirange_type_name {
                options.push(format!("MULTIRANGE_TYPE_NAME = {v}"));
            }
            format!("CREATE TYPE {name} AS RANGE ({});", options.join(", "))
        }
    }
}

/// Renders `CREATE SEQUENCE` with all normalised options.
fn render_create_sequence(sequence: &crate::model::Sequence) -> String {
    let mut out = format!(
        "CREATE SEQUENCE {}",
        qualified(&sequence.schema, &sequence.name)
    );
    out.push_str(&render_sequence_options(sequence));
    out.push(';');
    out
}

/// Renders `ALTER SEQUENCE ...` with all normalised options.
fn render_alter_sequence(sequence: &crate::model::Sequence) -> String {
    let mut out = format!(
        "ALTER SEQUENCE {}",
        qualified(&sequence.schema, &sequence.name)
    );
    out.push_str(&render_sequence_options(sequence));
    out.push(';');
    out
}

fn render_sequence_options(sequence: &crate::model::Sequence) -> String {
    let o = &sequence.options;
    let mut out = format!(
        " AS {} INCREMENT BY {} MINVALUE {} MAXVALUE {} START WITH {} CACHE {} {}",
        sequence.data_type,
        o.increment,
        o.minvalue,
        o.maxvalue,
        o.start,
        o.cache,
        if o.cycle { "CYCLE" } else { "NO CYCLE" }
    );
    match &sequence.owned_by {
        Some(owner) => out.push_str(&format!(
            " OWNED BY {}.{}",
            qualified(&owner.schema, &owner.table),
            quote_ident(&owner.column)
        )),
        None => out.push_str(" OWNED BY NONE"),
    }
    out
}

/// Renders `CREATE [MATERIALIZED] VIEW`.
fn render_create_view(view: &crate::model::View) -> String {
    use crate::model::CheckOption;
    let name = qualified(&view.schema, &view.name);
    let mut out = String::new();
    if view.materialized {
        out.push_str("CREATE MATERIALIZED VIEW ");
    } else {
        out.push_str("CREATE VIEW ");
    }
    out.push_str(&name);
    if !view.columns.is_empty() {
        out.push_str(&format!(" ({})", render_ident_list(&view.columns)));
    }
    if view.materialized
        && let Some(method) = &view.access_method
    {
        out.push_str(&format!(" USING {}", quote_ident(method)));
    }
    if !view.options.is_empty() {
        let options = view
            .options
            .iter()
            .map(|(k, v)| format!("{} = {}", quote_ident(&k.to_ascii_lowercase()), v))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(" WITH ({options})"));
    }
    if let Some(tablespace) = &view.tablespace {
        out.push_str(&format!(" TABLESPACE {}", quote_ident(tablespace)));
    }
    out.push_str(&format!(" AS {}", view.query));
    if !view.materialized {
        match view.check_option {
            CheckOption::None => {}
            CheckOption::Local => out.push_str(" WITH LOCAL CHECK OPTION"),
            CheckOption::Cascaded => out.push_str(" WITH CASCADED CHECK OPTION"),
        }
    } else if !view.with_data {
        out.push_str(" WITH NO DATA");
    }
    out.push(';');
    out
}

/// Renders `CREATE [OR REPLACE] FUNCTION|PROCEDURE`.
fn render_create_routine(routine: &crate::model::Routine) -> String {
    let mut out = format!(
        "CREATE OR REPLACE {} {} ({})",
        routine.kind.as_sql(),
        routine.qualified_name(),
        routine.render_args()
    );
    if !routine.return_table.is_empty() {
        let cols = routine
            .return_table
            .iter()
            .map(|(name, data_type)| format!("{} {}", quote_ident(name), render_type(data_type)))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(" RETURNS TABLE ({cols})"));
    } else if let Some(return_type) = &routine.return_type {
        out.push_str(&format!(
            " RETURNS {}{}",
            if routine.returns_setof { "SETOF " } else { "" },
            render_type(return_type)
        ));
    }
    out.push_str(&format!(" LANGUAGE {}", quote_ident(&routine.language)));
    if routine.volatility != crate::model::Volatility::Volatile {
        out.push(' ');
        out.push_str(routine.volatility.as_sql());
    }
    if routine.strict {
        out.push_str(" STRICT");
    }
    if routine.leakproof {
        out.push_str(" LEAKPROOF");
    }
    if routine.window {
        out.push_str(" WINDOW");
    }
    if routine.security == crate::model::Security::Definer {
        out.push_str(" SECURITY DEFINER");
    }
    if let Some(parallel) = &routine.parallel {
        out.push_str(&format!(" PARALLEL {parallel}"));
    }
    if let Some(cost) = routine.cost {
        out.push_str(&format!(" COST {cost}"));
    }
    if let Some(rows) = routine.rows {
        out.push_str(&format!(" ROWS {rows}"));
    }
    for (name, value) in &routine.settings {
        out.push_str(&format!(" SET {name} = {value}"));
    }
    out.push_str(&format!(" AS {}", routine.body));
    out.push(';');
    out
}

/// Renders `CREATE TRIGGER`.
fn render_create_trigger(trigger: &crate::model::Trigger) -> String {
    let mut out = String::from("CREATE ");
    if trigger.constraint {
        out.push_str("CONSTRAINT ");
    }
    out.push_str(&format!(
        "TRIGGER {} {} ",
        quote_ident(&trigger.name),
        trigger.timing.as_sql()
    ));
    out.push_str(
        &trigger
            .events
            .iter()
            .map(crate::model::TriggerEvent::as_sql)
            .collect::<Vec<_>>()
            .join(" OR "),
    );
    out.push_str(&format!(
        " ON {} ",
        qualified(&trigger.schema, &trigger.table)
    ));
    if let Some(deferrable) = &trigger.deferrable {
        out.push_str(if deferrable.initially_deferred {
            "DEFERRABLE INITIALLY DEFERRED "
        } else {
            "DEFERRABLE INITIALLY IMMEDIATE "
        });
    }
    out.push_str(match trigger.level {
        crate::model::TriggerLevel::Row => "FOR EACH ROW ",
        crate::model::TriggerLevel::Statement => "FOR EACH STATEMENT ",
    });
    if let Some(condition) = &trigger.condition {
        out.push_str(&format!("WHEN ({condition}) "));
    }
    out.push_str(&format!(
        "EXECUTE FUNCTION {}({})",
        qualified(&trigger.function.0, &trigger.function.1),
        trigger
            .function_args
            .iter()
            .map(|a| crate::sql::expr::render_string_literal(a))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    out.push(';');
    out
}

/// Renders `CREATE POLICY`.
fn render_create_policy(policy: &crate::model::Policy) -> String {
    let mut out = format!(
        "CREATE POLICY {} ON {}",
        quote_ident(&policy.name),
        qualified(&policy.schema, &policy.table)
    );
    if !policy.permissive {
        out.push_str(" AS RESTRICTIVE");
    }
    if policy.command != crate::model::PolicyCommand::All {
        out.push_str(&format!(" FOR {}", policy.command.as_sql()));
    }
    if policy.roles != ["PUBLIC"] {
        out.push_str(&format!(" TO {}", policy.roles.join(", ")));
    }
    if let Some(using) = &policy.using {
        out.push_str(&format!(" USING ({using})"));
    }
    if let Some(check) = &policy.check {
        out.push_str(&format!(" WITH CHECK ({check})"));
    }
    out.push(';');
    out
}

/// Renders `PARTITION BY ...`.
fn render_partition_by(partition: &crate::model::Partitioning) -> String {
    let keys = partition
        .keys
        .iter()
        .map(|key| match key {
            crate::model::PartitionKey::Column(column) => quote_ident(column),
            crate::model::PartitionKey::Expression(expr) => format!("({expr})"),
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("PARTITION BY {} ({keys});", partition.strategy.as_sql())
}
