//! Catalog diff: compares two catalogs and produces the changes the
//! planner renders.
//!
//! Renames are **not** detected: per the SPEC, rename detection is an
//! opt-in hint (`--rename table.a=b`) in a later milestone, never guessed
//! silently. A renamed object therefore diffs as a drop plus a create —
//! the honest, explicit output.
//!
//! Column comparisons are by name: PostgreSQL cannot reorder columns
//! without a destructive rewrite, so pgdelta never plans a reorder.

use crate::model::{
    Catalog, Column, Constraint, ConstraintKind, Deferrable, Extension, Index, PUBLIC_SCHEMA, Table,
};

/// One modelled difference between two catalogs.
#[derive(Debug, Clone)]
pub(crate) enum Change {
    /// A schema exists only in the target.
    CreateSchema {
        /// Schema name.
        name: String,
    },
    /// A schema exists only in the source.
    DropSchema {
        /// Schema name.
        name: String,
    },
    /// An extension exists only in the target.
    CreateExtension {
        /// The extension.
        extension: Extension,
    },
    /// An extension exists only in the source.
    DropExtension {
        /// Extension name.
        name: String,
    },
    /// A table exists only in the target.
    CreateTable {
        /// The table.
        table: Table,
    },
    /// A table exists only in the source.
    DropTable {
        /// Schema name.
        schema: String,
        /// Table name.
        name: String,
    },
    /// A table exists in both but differs.
    AlterTable {
        /// Schema name.
        schema: String,
        /// Table name.
        name: String,
        /// Granular changes, in plan order.
        actions: Vec<AlterChange>,
    },
    /// An index exists only in the target.
    CreateIndex {
        /// The index.
        index: Index,
    },
    /// An index exists only in the source.
    DropIndex {
        /// Schema name.
        schema: String,
        /// Index name.
        name: String,
    },
}

/// One granular table alteration.
#[derive(Debug, Clone)]
pub(crate) enum AlterChange {
    /// `DROP CONSTRAINT`.
    DropConstraint {
        /// Constraint name.
        name: String,
    },
    /// `DROP COLUMN`.
    DropColumn {
        /// Column name.
        name: String,
    },
    /// `ALTER COLUMN ... TYPE` (with the source-side default, so the
    /// planner can drop it first and re-apply it after, the `pg_dump`
    /// pattern).
    AlterColumnType {
        /// Column name.
        column: String,
        /// New canonical type.
        data_type: String,
        /// New collation (replaces the old outright; `None` = type
        /// default).
        collation: Option<String>,
        /// Default on the source side, if any (re-applied afterwards).
        from_default: Option<String>,
        /// Default on the target side, if any.
        to_default: Option<String>,
    },
    /// `ALTER COLUMN ... SET DEFAULT` / `DROP DEFAULT` (type unchanged).
    SetDefault {
        /// Column name.
        column: String,
        /// Normalised expression.
        expr: String,
    },
    /// See [`AlterChange::SetDefault`].
    DropDefault {
        /// Column name.
        column: String,
    },
    /// `ALTER COLUMN ... SET NOT NULL`.
    SetNotNull {
        /// Column name.
        column: String,
    },
    /// `ALTER COLUMN ... DROP NOT NULL`.
    DropNotNull {
        /// Column name.
        column: String,
    },
    /// `ALTER COLUMN ... ADD GENERATED ... AS IDENTITY`.
    AddIdentity {
        /// Column name.
        column: String,
        /// Identity specification.
        identity: crate::model::Identity,
    },
    /// `ALTER COLUMN ... DROP IDENTITY`.
    DropIdentity {
        /// Column name.
        column: String,
    },
    /// `ALTER COLUMN ... SET GENERATED {ALWAYS|BY DEFAULT}`.
    SetGeneratedKind {
        /// Column name.
        column: String,
        /// New identity kind.
        kind: crate::model::IdentityKind,
    },
    /// `ALTER COLUMN ... SET <option>` for each changed sequence option.
    SetSequenceOption {
        /// Column name.
        column: String,
        /// The changed option.
        option: crate::sql::alter_table::SeqOption,
    },
    /// `ADD COLUMN` (including the re-added side of a generated-column
    /// expression change, which PostgreSQL below 17 cannot alter in
    /// place).
    AddColumn {
        /// The column.
        column: Column,
    },
    /// `ADD CONSTRAINT`.
    AddConstraint {
        /// The constraint.
        constraint: Constraint,
    },
    /// `VALIDATE CONSTRAINT` (target is valid, source is `NOT VALID`).
    ValidateConstraint {
        /// Constraint name.
        name: String,
    },
    /// `ALTER CONSTRAINT ... DEFERRABLE ...` (foreign keys only).
    AlterConstraintDeferrable {
        /// Constraint name.
        name: String,
        /// New deferrability.
        deferrable: Option<Deferrable>,
    },
}

/// Diffs two catalogs.
pub(crate) fn diff(from: &Catalog, to: &Catalog) -> Vec<Change> {
    let mut changes = Vec::new();

    // Schemas (the ambient public schema is never created or dropped).
    for schema in to.schemas.keys() {
        if schema != PUBLIC_SCHEMA && !from.schemas.contains_key(schema) {
            changes.push(Change::CreateSchema {
                name: schema.clone(),
            });
        }
    }
    for schema in from.schemas.keys() {
        if schema != PUBLIC_SCHEMA && !to.schemas.contains_key(schema) {
            changes.push(Change::DropSchema {
                name: schema.clone(),
            });
        }
    }

    // Extensions: keyed by name; a WITH SCHEMA difference recreates the
    // extension.
    for (name, extension) in &to.extensions {
        match from.extensions.get(name) {
            None => changes.push(Change::CreateExtension {
                extension: extension.clone(),
            }),
            Some(old) if old != extension => {
                changes.push(Change::DropExtension { name: name.clone() });
                changes.push(Change::CreateExtension {
                    extension: extension.clone(),
                });
            }
            Some(_) => {}
        }
    }
    for name in from.extensions.keys() {
        if !to.extensions.contains_key(name) {
            changes.push(Change::DropExtension { name: name.clone() });
        }
    }

    // Tables (and, folded in, their columns and constraints).
    let dropped_tables: Vec<(String, String)> = from
        .tables
        .keys()
        .filter(|k| !to.tables.contains_key(k))
        .cloned()
        .collect();
    for (schema, name) in &dropped_tables {
        changes.push(Change::DropTable {
            schema: schema.clone(),
            name: name.clone(),
        });
    }
    for table in to.tables.values() {
        match from.table(&table.schema, &table.name) {
            None => changes.push(Change::CreateTable {
                table: table.clone(),
            }),
            Some(old) => {
                let actions = diff_table(old, table);
                if !actions.is_empty() {
                    changes.push(Change::AlterTable {
                        schema: table.schema.clone(),
                        name: table.name.clone(),
                        actions,
                    });
                }
            }
        }
    }

    // Indexes: an index of a dropped table dies with the table (a
    // separate DROP INDEX would be redundant and even invalid after the
    // table is gone).
    for index in to.indexes.values() {
        if from
            .indexes
            .contains_key(&(index.schema.clone(), index.name.clone()))
        {
            continue;
        }
        changes.push(Change::CreateIndex {
            index: index.clone(),
        });
    }
    for (schema, name) in from.indexes.keys() {
        let table = &from.indexes[&(schema.clone(), name.clone())].table;
        let table_dropped = dropped_tables.contains(&(schema.clone(), table.clone()));
        if table_dropped || to.indexes.contains_key(&(schema.clone(), name.clone())) {
            continue;
        }
        changes.push(Change::DropIndex {
            schema: schema.clone(),
            name: name.clone(),
        });
    }

    changes
}

/// Diffs two versions of the same table, producing granular changes.
fn diff_table(from: &Table, to: &Table) -> Vec<AlterChange> {
    let mut drops: Vec<AlterChange> = Vec::new();
    let mut alters: Vec<AlterChange> = Vec::new();
    let mut adds: Vec<AlterChange> = Vec::new();
    let mut validates: Vec<AlterChange> = Vec::new();
    let mut deferrables: Vec<AlterChange> = Vec::new();

    // Columns by name.
    let to_names: Vec<String> = to.columns.iter().map(|c| c.name.clone()).collect();

    for column in &from.columns {
        if !to_names.contains(&column.name) {
            drops.push(AlterChange::DropColumn {
                name: column.name.clone(),
            });
        }
    }
    for column in &to.columns {
        let Some(old) = from.columns.iter().find(|c| c.name == column.name) else {
            // A generated-expression change replaces the column (Postgres
            // below 17 has no in-place expression replacement; dropping
            // and re-adding is the portable sequence). The drop is
            // recorded on the old side, the add here.
            adds.push(AlterChange::AddColumn {
                column: column.clone(),
            });
            continue;
        };
        diff_column(old, column, &mut drops, &mut alters, &mut adds);
    }

    // Constraints by name.
    for constraint in &from.constraints {
        if !to.constraints.iter().any(|c| c.name == constraint.name) {
            drops.push(AlterChange::DropConstraint {
                name: constraint.name.clone(),
            });
        }
    }
    for constraint in &to.constraints {
        let Some(old) = from.constraints.iter().find(|c| c.name == constraint.name) else {
            adds.push(AlterChange::AddConstraint {
                constraint: constraint.clone(),
            });
            continue;
        };
        diff_constraint(
            old,
            constraint,
            &mut drops,
            &mut adds,
            &mut validates,
            &mut deferrables,
        );
    }

    // Fixed sub-order per table: constraint drops, column drops, column
    // alterations (name-sorted), column adds (name-sorted), constraint
    // adds (name-sorted), validations, deferrability changes.
    let mut out: Vec<AlterChange> = Vec::new();
    out.append(&mut drops_by_name(drops));
    alters.sort_by_key(alter_key);
    out.extend(alters);
    adds.sort_by_key(alter_key);
    out.extend(adds);
    out.extend(validates);
    out.extend(deferrables);
    out
}

/// Stable sort keys for the fixed sub-order inside a table's alterations.
fn alter_key(change: &AlterChange) -> (u8, String) {
    match change {
        AlterChange::DropConstraint { name } => (0, name.clone()),
        AlterChange::DropColumn { name } => (1, name.clone()),
        AlterChange::AddConstraint { constraint } => (5, constraint.name.clone()),
        AlterChange::AddColumn { column } => (4, column.name.clone()),
        AlterChange::AlterColumnType { column, .. } => (2, column.clone()),
        AlterChange::SetDefault { column, .. } | AlterChange::DropDefault { column } => {
            (2, column.clone())
        }
        AlterChange::SetNotNull { column } | AlterChange::DropNotNull { column } => {
            (2, column.clone())
        }
        AlterChange::AddIdentity { column, .. }
        | AlterChange::DropIdentity { column }
        | AlterChange::SetGeneratedKind { column, .. }
        | AlterChange::SetSequenceOption { column, .. } => (2, column.clone()),
        AlterChange::ValidateConstraint { name } => (6, name.clone()),
        AlterChange::AlterConstraintDeferrable { name, .. } => (7, name.clone()),
    }
}

fn drops_by_name(drops: Vec<AlterChange>) -> Vec<AlterChange> {
    let mut sorted = drops;
    sorted.sort_by_key(alter_key);
    sorted
}

/// Diffs one column (both sides exist).
fn diff_column(
    from: &Column,
    to: &Column,
    drops: &mut Vec<AlterChange>,
    alters: &mut Vec<AlterChange>,
    adds: &mut Vec<AlterChange>,
) {
    // Generated-expression change: replace the column (portable across
    // PostgreSQL 13-18; see the planner notes in docs/parser.md).
    let generated_changed = match (&from.generated, &to.generated) {
        (Some(a), Some(b)) => a.expr != b.expr,
        (None, None) => false,
        _ => true,
    };
    if generated_changed {
        drops.push(AlterChange::DropColumn {
            name: from.name.clone(),
        });
        adds.push(AlterChange::AddColumn { column: to.clone() });
        return;
    }

    if from.data_type != to.data_type || from.collation != to.collation {
        alters.push(AlterChange::AlterColumnType {
            column: to.name.clone(),
            data_type: to.data_type.clone(),
            collation: to.collation.clone(),
            from_default: from.default.clone(),
            to_default: to.default.clone(),
        });
    } else if from.default != to.default {
        match &to.default {
            Some(expr) => alters.push(AlterChange::SetDefault {
                column: to.name.clone(),
                expr: expr.clone(),
            }),
            None => alters.push(AlterChange::DropDefault {
                column: to.name.clone(),
            }),
        }
    }
    if from.not_null != to.not_null {
        alters.push(if to.not_null {
            AlterChange::SetNotNull {
                column: to.name.clone(),
            }
        } else {
            AlterChange::DropNotNull {
                column: to.name.clone(),
            }
        });
    }
    diff_identity(from, to, alters);
}

/// Diffs the identity specification of a column (both sides exist).
fn diff_identity(from: &Column, to: &Column, alters: &mut Vec<AlterChange>) {
    match (&from.identity, &to.identity) {
        (None, None) => {}
        (Some(_), None) => alters.push(AlterChange::DropIdentity {
            column: to.name.clone(),
        }),
        (None, Some(identity)) => alters.push(AlterChange::AddIdentity {
            column: to.name.clone(),
            identity: identity.clone(),
        }),
        (Some(old), Some(new)) => {
            use crate::sql::alter_table::SeqOption;
            if old.kind != new.kind {
                alters.push(AlterChange::SetGeneratedKind {
                    column: to.name.clone(),
                    kind: new.kind,
                });
            }
            let o = &old.options;
            let n = &new.options;
            if o.increment != n.increment {
                alters.push(AlterChange::SetSequenceOption {
                    column: to.name.clone(),
                    option: SeqOption::Increment(n.increment),
                });
            }
            if o.minvalue != n.minvalue {
                alters.push(AlterChange::SetSequenceOption {
                    column: to.name.clone(),
                    option: SeqOption::MinValue(n.minvalue),
                });
            }
            if o.maxvalue != n.maxvalue {
                alters.push(AlterChange::SetSequenceOption {
                    column: to.name.clone(),
                    option: SeqOption::MaxValue(n.maxvalue),
                });
            }
            if o.start != n.start {
                alters.push(AlterChange::SetSequenceOption {
                    column: to.name.clone(),
                    option: SeqOption::Start(n.start),
                });
            }
            if o.cache != n.cache {
                alters.push(AlterChange::SetSequenceOption {
                    column: to.name.clone(),
                    option: SeqOption::Cache(n.cache),
                });
            }
            if o.cycle != n.cycle {
                alters.push(AlterChange::SetSequenceOption {
                    column: to.name.clone(),
                    option: SeqOption::Cycle(n.cycle),
                });
            }
        }
    }
}

/// Diffs one constraint (both sides exist).
fn diff_constraint(
    from: &Constraint,
    to: &Constraint,
    drops: &mut Vec<AlterChange>,
    adds: &mut Vec<AlterChange>,
    validates: &mut Vec<AlterChange>,
    deferrables: &mut Vec<AlterChange>,
) {
    // NOT VALID -> VALID is a pure validation pass.
    if from.not_valid && !to.not_valid && from.kind == to.kind && from.deferrable == to.deferrable {
        validates.push(AlterChange::ValidateConstraint {
            name: to.name.clone(),
        });
        return;
    }
    // A deferrability change on a foreign key uses ALTER CONSTRAINT
    // (PostgreSQL docs, ALTER TABLE, ALTER CONSTRAINT).
    if from.kind == to.kind
        && !from.not_valid
        && !to.not_valid
        && from.deferrable != to.deferrable
        && matches!(to.kind, ConstraintKind::ForeignKey { .. })
    {
        deferrables.push(AlterChange::AlterConstraintDeferrable {
            name: to.name.clone(),
            deferrable: to.deferrable.clone(),
        });
        return;
    }
    if from.kind == to.kind && from.deferrable == to.deferrable && from.not_valid == to.not_valid {
        return;
    }
    // Everything else: drop and re-add (the re-add may carry NOT VALID,
    // which cannot be reached any other way).
    drops.push(AlterChange::DropConstraint {
        name: to.name.clone(),
    });
    adds.push(AlterChange::AddConstraint {
        constraint: to.clone(),
    });
}
