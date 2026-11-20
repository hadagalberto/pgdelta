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

    /// True when the plan has no statements (the schemas already
    /// converge).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.statements().is_empty()
    }

    /// Counts statements per action category.
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
}

/// Ranks (broad phases) for the dependency sort.
mod rank {
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

/// Plans a migration from `from` to `to`.
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
    let changes = crate::diff::diff(from, to);
    let mut nodes: Vec<PlanNode<PlanStatement>> = Vec::new();

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
                nodes.push(node(
                    rank::DROP_TABLE,
                    format!("drop-table:{schema}.{name}"),
                    "drop",
                    qualified(schema, name),
                    format!("DROP TABLE {};", qualified(schema, name)),
                ));
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
                    },
                    after: Vec::new(),
                });
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
                    format!("type-drop-default:{column}"),
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
                format!("type:{column}"),
                format!("{table}.{column}"),
                sql,
            ));
            if let Some(default) = to_default {
                out.push((
                    rank::ALTER,
                    format!("type-set-default:{column}"),
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
    format!(
        "CREATE TABLE {} (\n{}\n);",
        qualified(&table.schema, &table.name),
        lines.join(",\n")
    )
}

/// Renders a column definition: name, type, then the canonical option
/// order COLLATE → DEFAULT → GENERATED (stored/identity) → NOT NULL.
fn render_column_def(column: &Column) -> String {
    let mut out = format!(
        "{} {}",
        quote_ident(&column.name),
        render_type(&column.data_type)
    );
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
    out.push_str(&qualified(&index.schema, &index.name));
    out.push_str(" ON ");
    out.push_str(&qualified(&index.table_schema, &index.table));
    if let Some(method) = &index.method {
        out.push_str(&format!(" USING {}", quote_ident(method)));
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
fn render_type(canonical: &str) -> String {
    if !canonical.contains('.') {
        return canonical.to_owned();
    }
    quote_parts(canonical)
}

/// Renders a dotted name with every part quoted.
fn quote_parts(name: &str) -> String {
    name.split('.')
        .map(quote_ident)
        .collect::<Vec<_>>()
        .join(".")
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
    out.push_str("-- pgdelta 0.1.0 migration\n");
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
