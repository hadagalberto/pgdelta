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
    Catalog, Column, Constraint, ConstraintKind, Deferrable, Extension, Index, PUBLIC_SCHEMA,
    PgTypeKind, Table,
};
use std::collections::BTreeSet;

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
    // ---- Milestone 2 object changes ------------------------------------
    /// A user-defined type exists only in the target.
    CreateType {
        /// The new type.
        pg_type: crate::model::PgType,
    },
    /// A user-defined type exists only in the source.
    DropType {
        /// Schema name.
        schema: String,
        /// Type name.
        name: String,
    },
    /// An enum gained values at the end (or had values renamed in place).
    AlterEnum {
        /// Schema name.
        schema: String,
        /// Type name.
        name: String,
        /// Values appended in target order.
        append: Vec<String>,
        /// `(old, new)` renames at unchanged positions.
        renames: Vec<(String, String)>,
    },
    /// A type changed in a way that requires drop + recreate.
    ReplaceType {
        /// The replacement type.
        pg_type: crate::model::PgType,
    },
    /// A standalone sequence was created or differs and must be altered.
    UpsertSequence {
        /// The desired sequence state.
        sequence: crate::model::Sequence,
        /// Whether the sequence is new.
        create: bool,
    },
    /// A standalone sequence exists only in the source.
    DropSequence {
        /// Schema name.
        schema: String,
        /// Sequence name.
        name: String,
    },
    /// A view/materialized view exists only in the target.
    CreateView {
        /// The new view.
        view: crate::model::View,
    },
    /// A view/materialized view must be dropped (and possibly recreated).
    DropView {
        /// Schema name.
        schema: String,
        /// View name.
        name: String,
        /// Whether it is materialized.
        materialized: bool,
    },
    /// A routine was created, replaced or changed signature.
    UpsertRoutine {
        /// The desired routine.
        routine: crate::model::Routine,
        /// Whether a `CREATE` (rather than `CREATE OR REPLACE`) is required.
        create: bool,
    },
    /// A routine exists only in the source.
    DropRoutine {
        /// Schema name.
        schema: String,
        /// Routine name.
        name: String,
        /// Input signature.
        signature: String,
        /// Function or procedure.
        kind: crate::model::RoutineKind,
    },
    /// A trigger was created or changed.
    CreateTrigger {
        /// The new trigger.
        trigger: crate::model::Trigger,
    },
    /// A trigger exists only in the source or must be recreated.
    DropTrigger {
        /// Schema name.
        schema: String,
        /// Relation name.
        table: String,
        /// Trigger name.
        name: String,
    },
    /// A policy was created or changed.
    CreatePolicy {
        /// The new policy.
        policy: crate::model::Policy,
    },
    /// A policy exists only in the source or must be recreated.
    DropPolicy {
        /// Schema name.
        schema: String,
        /// Table name.
        table: String,
        /// Policy name.
        name: String,
    },
    /// Table RLS flags changed.
    SetTableRls {
        /// Schema name.
        schema: String,
        /// Table name.
        table: String,
        /// Desired `ENABLE`/`DISABLE` state.
        enabled: bool,
        /// Desired `FORCE` state.
        forced: bool,
    },
    /// A comment must be set.
    SetComment {
        /// Object reference.
        object: crate::model::ObjectRef,
        /// Comment text.
        text: String,
    },
    /// A comment must be removed.
    DropComment {
        /// Object reference.
        object: crate::model::ObjectRef,
    },
    /// A privilege grant must be applied.
    SetGrant {
        /// Desired grant.
        grant: crate::model::Grant,
    },
    /// A privilege grant must be revoked.
    DropGrant {
        /// Object reference.
        object: crate::model::ObjectRef,
        /// Grantee.
        grantee: String,
        /// Privileges to revoke.
        privileges: std::collections::BTreeSet<String>,
    },
    /// A partition was created or changed.
    CreatePartition {
        /// The partition.
        partition: crate::model::Partition,
    },
    /// A partition exists only in the source or must be recreated.
    DropPartition {
        /// Schema name.
        schema: String,
        /// Partition name.
        name: String,
    },
    /// A table became (or stopped being) partitioned.
    SetPartitioning {
        /// Schema name.
        schema: String,
        /// Table name.
        table: String,
        /// Desired partitioning (`None` = `REMOVE PARTITIONING`).
        partition: Option<crate::model::Partitioning>,
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
        // A partition's table entry dies with its Partition change.
        if from
            .tables
            .get(&(schema.clone(), name.clone()))
            .is_some_and(|t| t.partition_of.is_some())
        {
            continue;
        }
        changes.push(Change::DropTable {
            schema: schema.clone(),
            name: name.clone(),
        });
    }
    for table in to.tables.values() {
        // Partitions are created by their Partition change, not as plain
        // tables (PostgreSQL's `CREATE TABLE ... PARTITION OF`).
        if table.partition_of.is_some() {
            continue;
        }
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

    diff_objects(from, to, &mut changes);

    changes
}

/// Diffs the milestone-2 object collections.
fn diff_objects(from: &Catalog, to: &Catalog, changes: &mut Vec<Change>) {
    // Tables whose column type/collation changed. A view reading such a
    // table caches the column's type in its stored target list, so it must
    // be dropped and recreated around the ALTER COLUMN ... TYPE statement.
    let mut changed_tables: BTreeSet<(String, String)> = BTreeSet::new();
    for ((schema, name), target) in &to.tables {
        let Some(source) = from.tables.get(&(schema.clone(), name.clone())) else {
            continue;
        };
        for column in &target.columns {
            if let Some(old) = source.column(&column.name)
                && (old.data_type != column.data_type || old.collation != column.collation)
            {
                changed_tables.insert((schema.clone(), name.clone()));
                break;
            }
        }
    }
    let recreate_views: BTreeSet<(String, String)> = from
        .views
        .iter()
        .filter(|(_, view)| {
            view.referenced_relations()
                .iter()
                .any(|relation| changed_tables.contains(relation))
        })
        .map(|(key, _)| key.clone())
        .collect();

    // ---- Types -----------------------------------------------------------
    for (key, target) in &to.types {
        match from.types.get(key) {
            None => changes.push(Change::CreateType {
                pg_type: target.clone(),
            }),
            Some(source) => diff_type(key, source, target, changes),
        }
    }
    for key in from.types.keys() {
        if !to.types.contains_key(key) {
            changes.push(Change::DropType {
                schema: key.0.clone(),
                name: key.1.clone(),
            });
        }
    }

    // ---- Sequences -------------------------------------------------------
    for (key, target) in &to.sequences {
        match from.sequences.get(key) {
            None => changes.push(Change::UpsertSequence {
                sequence: target.clone(),
                create: true,
            }),
            Some(source) if source != target => changes.push(Change::UpsertSequence {
                sequence: target.clone(),
                create: false,
            }),
            Some(_) => {}
        }
    }
    for key in from.sequences.keys() {
        if !to.sequences.contains_key(key) {
            changes.push(Change::DropSequence {
                schema: key.0.clone(),
                name: key.1.clone(),
            });
        }
    }

    // ---- Views -----------------------------------------------------------
    diff_views(from, to, &recreate_views, changes);

    // ---- Routines --------------------------------------------------------
    for (key, target) in &to.routines {
        match from.routines.get(key) {
            None => changes.push(Change::UpsertRoutine {
                routine: target.clone(),
                create: true,
            }),
            Some(source) if source != target => changes.push(Change::UpsertRoutine {
                routine: target.clone(),
                create: false,
            }),
            Some(_) => {}
        }
    }
    for (key, source) in &from.routines {
        if !to.routines.contains_key(key) {
            changes.push(Change::DropRoutine {
                schema: key.0.clone(),
                name: key.1.clone(),
                signature: key.2.clone(),
                kind: source.kind,
            });
        }
    }

    // ---- Triggers --------------------------------------------------------
    for (key, target) in &to.triggers {
        match from.triggers.get(key) {
            None => changes.push(Change::CreateTrigger {
                trigger: target.clone(),
            }),
            Some(source) if source != target => {
                changes.push(Change::DropTrigger {
                    schema: key.0.clone(),
                    table: key.1.clone(),
                    name: key.2.clone(),
                });
                changes.push(Change::CreateTrigger {
                    trigger: target.clone(),
                });
            }
            Some(_) => {}
        }
    }
    for key in from.triggers.keys() {
        if !to.triggers.contains_key(key) {
            changes.push(Change::DropTrigger {
                schema: key.0.clone(),
                table: key.1.clone(),
                name: key.2.clone(),
            });
        }
    }

    // ---- Policies + RLS --------------------------------------------------
    for (key, target) in &to.policies {
        match from.policies.get(key) {
            None => changes.push(Change::CreatePolicy {
                policy: target.clone(),
            }),
            Some(source) if source != target => {
                changes.push(Change::DropPolicy {
                    schema: key.0.clone(),
                    table: key.1.clone(),
                    name: key.2.clone(),
                });
                changes.push(Change::CreatePolicy {
                    policy: target.clone(),
                });
            }
            Some(_) => {}
        }
    }
    for key in from.policies.keys() {
        if !to.policies.contains_key(key) {
            changes.push(Change::DropPolicy {
                schema: key.0.clone(),
                table: key.1.clone(),
                name: key.2.clone(),
            });
        }
    }
    for ((schema, name), target) in &to.tables {
        if let Some(source) = from.tables.get(&(schema.clone(), name.clone()))
            && (source.rls_enabled != target.rls_enabled || source.rls_forced != target.rls_forced)
        {
            changes.push(Change::SetTableRls {
                schema: schema.clone(),
                table: name.clone(),
                enabled: target.rls_enabled,
                forced: target.rls_forced,
            });
        }
    }

    // ---- Comments --------------------------------------------------------
    for (object, text) in &to.comments {
        match from.comments.get(object) {
            Some(old) if old == text => {}
            _ => changes.push(Change::SetComment {
                object: object.clone(),
                text: text.clone(),
            }),
        }
    }
    for object in from.comments.keys() {
        if !to.comments.contains_key(object) {
            changes.push(Change::DropComment {
                object: object.clone(),
            });
        }
    }

    // ---- Grants ----------------------------------------------------------
    for (key, target) in &to.grants {
        match from.grants.get(key) {
            None => changes.push(Change::SetGrant {
                grant: target.clone(),
            }),
            Some(source) if source != target => {
                let removed: std::collections::BTreeSet<String> = source
                    .privileges
                    .difference(&target.privileges)
                    .cloned()
                    .collect();
                if !removed.is_empty() {
                    changes.push(Change::DropGrant {
                        object: key.0.clone(),
                        grantee: key.1.clone(),
                        privileges: removed,
                    });
                }
                changes.push(Change::SetGrant {
                    grant: target.clone(),
                });
            }
            Some(_) => {}
        }
    }
    for (key, source) in &from.grants {
        if !to.grants.contains_key(key) {
            changes.push(Change::DropGrant {
                object: key.0.clone(),
                grantee: key.1.clone(),
                privileges: source.privileges.clone(),
            });
        }
    }

    // ---- Partitioning ----------------------------------------------------
    for ((schema, name), target) in &to.tables {
        if let Some(source) = from.tables.get(&(schema.clone(), name.clone()))
            && source.partition != target.partition
        {
            changes.push(Change::SetPartitioning {
                schema: schema.clone(),
                table: name.clone(),
                partition: target.partition.clone(),
            });
        }
    }
    for (key, target) in &to.partitions {
        match from.partitions.get(key) {
            None => changes.push(Change::CreatePartition {
                partition: target.clone(),
            }),
            Some(source) if source != target => {
                changes.push(Change::DropPartition {
                    schema: key.0.clone(),
                    name: key.1.clone(),
                });
                changes.push(Change::CreatePartition {
                    partition: target.clone(),
                });
            }
            Some(_) => {}
        }
    }
    for key in from.partitions.keys() {
        if !to.partitions.contains_key(key) {
            changes.push(Change::DropPartition {
                schema: key.0.clone(),
                name: key.1.clone(),
            });
        }
    }
}

fn diff_type(
    key: &(String, String),
    source: &crate::model::PgType,
    target: &crate::model::PgType,
    changes: &mut Vec<Change>,
) {
    if source.kind == target.kind {
        return;
    }
    if let (PgTypeKind::Enum { values: from }, PgTypeKind::Enum { values: to }) =
        (&source.kind, &target.kind)
    {
        // Appends and in-place renames are the only ALTER-safe changes;
        // removals/reordering require dropping the whole enum type.
        let prefix = from.len() <= to.len() && from.iter().zip(to).all(|(a, b)| a == b);
        if prefix {
            changes.push(Change::AlterEnum {
                schema: key.0.clone(),
                name: key.1.clone(),
                append: to[from.len()..].to_vec(),
                renames: Vec::new(),
            });
            return;
        }
        let same_len = from.len() == to.len() && from.iter().zip(to).all(|(a, b)| a != b) && {
            let mut unique = std::collections::BTreeSet::new();
            to.iter().all(|v| unique.insert(v))
        };
        if same_len {
            changes.push(Change::AlterEnum {
                schema: key.0.clone(),
                name: key.1.clone(),
                append: Vec::new(),
                renames: from
                    .iter()
                    .zip(to)
                    .map(|(a, b)| (a.clone(), b.clone()))
                    .collect(),
            });
            return;
        }
    }
    changes.push(Change::ReplaceType {
        pg_type: target.clone(),
    });
}

fn diff_views(
    from: &Catalog,
    to: &Catalog,
    recreate: &BTreeSet<(String, String)>,
    changes: &mut Vec<Change>,
) {
    for (key, target) in &to.views {
        match from.views.get(key) {
            None => changes.push(Change::CreateView {
                view: target.clone(),
            }),
            Some(source) if source != target || recreate.contains(key) => {
                changes.push(Change::DropView {
                    schema: key.0.clone(),
                    name: key.1.clone(),
                    materialized: source.materialized,
                });
                changes.push(Change::CreateView {
                    view: target.clone(),
                });
            }
            Some(_) => {}
        }
    }
    for (key, source) in &from.views {
        if !to.views.contains_key(key) {
            changes.push(Change::DropView {
                schema: key.0.clone(),
                name: key.1.clone(),
                materialized: source.materialized,
            });
        }
    }
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
    // PostgreSQL requires the column to be NOT NULL before identity can be
    // added, but forbids DROP NOT NULL while the column is still an identity
    // column. Order the two changes accordingly so both directions execute
    // on PostgreSQL 13–18.
    let dropping_identity = from.identity.is_some() && to.identity.is_none();
    let not_null_change = (from.not_null != to.not_null).then(|| {
        if to.not_null {
            AlterChange::SetNotNull {
                column: to.name.clone(),
            }
        } else {
            AlterChange::DropNotNull {
                column: to.name.clone(),
            }
        }
    });
    if dropping_identity {
        diff_identity(from, to, alters);
        if let Some(change) = not_null_change {
            alters.push(change);
        }
    } else {
        if let Some(change) = not_null_change {
            alters.push(change);
        }
        diff_identity(from, to, alters);
    }
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
