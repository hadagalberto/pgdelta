//! Applies milestone-2 DDL statements to the normalised catalog.
//!
//! Every rule mirrors PostgreSQL semantics documented in the statements'
//! pages (cited in `sql/types.rs` and `sql/objects.rs`). Names are resolved
//! and truncated during parsing; this layer enforces catalog consistency:
//! object existence, name collisions in the shared `pg_class` namespace,
//! and the fact that dependent objects must be dropped explicitly (never
//! silently cascaded).

use crate::error::{Error, Result};
use crate::model::{
    Catalog, DomainConstraint, DomainConstraintKind, Grant, PUBLIC_SCHEMA, Partition, PgType,
    PgTypeKind, Sequence, Table,
};
use crate::sql::{
    ParsedStatement,
    objects::{RoutineAction, SequenceAction, TriggerEnabled, ViewAction},
    types::TypeAction,
};

fn invalid(source: &str, index: usize, message: impl std::fmt::Display, statement: &str) -> Error {
    Error::invalid(source, index, message, statement)
}

fn resolve(parts: &[String]) -> (String, String) {
    match parts.len() {
        1 => (PUBLIC_SCHEMA.to_owned(), parts[0].clone()),
        _ => (parts[0].clone(), parts[parts.len() - 1].clone()),
    }
}

/// Rejects a relation name already used by any `pg_class` object in `schema`.
fn ensure_relation_free(
    catalog: &Catalog,
    schema: &str,
    name: &str,
    source: &str,
    index: usize,
    statement: &str,
) -> Result<()> {
    if catalog.relation_names(schema).iter().any(|n| n == name) {
        return Err(invalid(
            source,
            index,
            format!("relation {name:?} already exists in schema {schema:?}"),
            statement,
        ));
    }
    Ok(())
}

fn ensure_type_free(
    catalog: &Catalog,
    schema: &str,
    name: &str,
    source: &str,
    index: usize,
    statement: &str,
) -> Result<()> {
    if catalog
        .types
        .contains_key(&(schema.to_owned(), name.to_owned()))
    {
        return Err(invalid(
            source,
            index,
            format!("type {name:?} already exists in schema {schema:?}"),
            statement,
        ));
    }
    // A table's row type lives in the same namespace as standalone types.
    if catalog
        .tables
        .contains_key(&(schema.to_owned(), name.to_owned()))
    {
        return Err(invalid(
            source,
            index,
            format!("type {name:?} conflicts with a table of the same name"),
            statement,
        ));
    }
    Ok(())
}

fn require_type_mut<'a>(
    catalog: &'a mut Catalog,
    schema: &str,
    name: &str,
    source: &str,
    index: usize,
    statement: &str,
) -> Result<&'a mut PgType> {
    catalog
        .types
        .get_mut(&(schema.to_owned(), name.to_owned()))
        .ok_or_else(|| {
            invalid(
                source,
                index,
                format!("type {schema}.{name} does not exist"),
                statement,
            )
        })
}

fn drop_comments_for(catalog: &mut Catalog, prefix: &str) {
    catalog
        .comments
        .retain(|object, _| !object.name.starts_with(prefix));
    catalog
        .grants
        .retain(|(object, _), _| !object.name.starts_with(prefix));
}

/// Applies a parsed milestone-2 statement.
pub(crate) fn apply_statement(
    catalog: &mut Catalog,
    source: &str,
    index: usize,
    stmt: &ParsedStatement,
) -> Result<()> {
    match stmt {
        ParsedStatement::CreateType { pg_type } => {
            apply_create_type(catalog, source, index, pg_type)
        }
        ParsedStatement::DropType { names, if_exists } => {
            for parts in names {
                let (schema, name) = resolve(parts);
                if catalog
                    .types
                    .remove(&(schema.clone(), name.clone()))
                    .is_none()
                    && !if_exists
                {
                    return Err(invalid(
                        source,
                        index,
                        format!("type {schema}.{name} does not exist"),
                        "DROP TYPE",
                    ));
                }
                drop_comments_for(catalog, &format!("{schema}.{name}"));
            }
            Ok(())
        }
        ParsedStatement::AlterType {
            name,
            actions,
            domain,
        } => apply_alter_type(catalog, source, index, name, actions, *domain),
        ParsedStatement::CreateSequence { sequence } => {
            apply_create_sequence(catalog, source, index, sequence)
        }
        ParsedStatement::AlterSequence { name, actions } => {
            apply_alter_sequence(catalog, source, index, name, actions)
        }
        ParsedStatement::DropSequence { names, if_exists } => {
            for parts in names {
                let (schema, name) = resolve(parts);
                if catalog
                    .sequences
                    .remove(&(schema.clone(), name.clone()))
                    .is_none()
                    && !if_exists
                {
                    return Err(invalid(
                        source,
                        index,
                        format!("sequence {schema}.{name} does not exist"),
                        "DROP SEQUENCE",
                    ));
                }
                drop_comments_for(catalog, &format!("{schema}.{name}"));
            }
            Ok(())
        }
        ParsedStatement::CreateView { view, or_replace } => {
            apply_create_view(catalog, source, index, view, *or_replace)
        }
        ParsedStatement::DropView {
            names,
            if_exists,
            materialized,
        } => {
            for parts in names {
                let (schema, name) = resolve(parts);
                let key = (schema.clone(), name.clone());
                match catalog.views.get(&key) {
                    Some(existing) if existing.materialized != *materialized => {
                        return Err(invalid(
                            source,
                            index,
                            format!("{schema}.{name} is not the requested view kind"),
                            "DROP VIEW",
                        ));
                    }
                    Some(_) => {}
                    None if *if_exists => continue,
                    None => {
                        return Err(invalid(
                            source,
                            index,
                            format!("view {schema}.{name} does not exist"),
                            "DROP VIEW",
                        ));
                    }
                }
                catalog.views.remove(&key);
                catalog
                    .triggers
                    .retain(|(s, t, _), _| !(s == &schema && t == &name));
                catalog
                    .policies
                    .retain(|(s, t, _), _| !(s == &schema && t == &name));
                drop_comments_for(catalog, &format!("{schema}.{name}"));
            }
            Ok(())
        }
        ParsedStatement::AlterView { name, actions } => {
            apply_alter_view(catalog, source, index, name, actions)
        }
        ParsedStatement::CreateRoutine {
            routine,
            or_replace,
        } => {
            let key = (
                routine.schema.clone(),
                routine.name.clone(),
                routine.signature(),
            );
            if catalog.routines.contains_key(&key) && !or_replace {
                return Err(invalid(
                    source,
                    index,
                    format!(
                        "{} {}.{}({}) already exists",
                        routine.kind.as_sql(),
                        routine.schema,
                        routine.name,
                        routine.signature()
                    ),
                    "CREATE ROUTINE",
                ));
            }
            catalog.ensure_schema(&routine.schema);
            catalog.routines.insert(key, routine.clone());
            Ok(())
        }
        ParsedStatement::DropRoutine { names, if_exists } => {
            for (parts, types) in names {
                let (schema, name) = resolve(parts);
                let key = (schema.clone(), name.clone(), types.join(","));
                if catalog.routines.remove(&key).is_none() && !if_exists {
                    return Err(invalid(
                        source,
                        index,
                        format!(
                            "routine {schema}.{name}({}) does not exist",
                            types.join(",")
                        ),
                        "DROP ROUTINE",
                    ));
                }
                drop_comments_for(catalog, &format!("{schema}.{name}("));
            }
            Ok(())
        }
        ParsedStatement::AlterRoutine { name, actions } => {
            apply_alter_routine(catalog, source, index, name, actions)
        }
        ParsedStatement::CreateTrigger {
            trigger,
            or_replace,
        } => {
            let (schema, table) = (trigger.schema.clone(), trigger.table.clone());
            let is_relation = catalog.table(&schema, &table).is_some()
                || catalog.views.contains_key(&(schema.clone(), table.clone()));
            if !is_relation {
                return Err(invalid(
                    source,
                    index,
                    format!("relation {schema}.{table} does not exist"),
                    "CREATE TRIGGER",
                ));
            }
            let key = (schema.clone(), table.clone(), trigger.name.clone());
            if catalog.triggers.contains_key(&key) && !or_replace {
                return Err(invalid(
                    source,
                    index,
                    format!(
                        "trigger {:?} for relation {:?} already exists",
                        trigger.name, table
                    ),
                    "CREATE TRIGGER",
                ));
            }
            if !catalog
                .routines
                .values()
                .any(|r| r.schema == trigger.function.0 && r.name == trigger.function.1)
            {
                return Err(invalid(
                    source,
                    index,
                    format!(
                        "trigger function {}.{} does not exist",
                        trigger.function.0, trigger.function.1
                    ),
                    "CREATE TRIGGER",
                ));
            }
            catalog.triggers.insert(key, trigger.clone());
            Ok(())
        }
        ParsedStatement::DropTrigger {
            name,
            table,
            if_exists,
        } => {
            let (schema, table_name) = resolve(table);
            let key = (schema.clone(), table_name.clone(), name.clone());
            if catalog.triggers.remove(&key).is_none() && !if_exists {
                return Err(invalid(
                    source,
                    index,
                    format!("trigger {name:?} for relation {table_name:?} does not exist"),
                    "DROP TRIGGER",
                ));
            }
            drop_comments_for(catalog, &format!("{schema}.{table_name}.{name}"));
            Ok(())
        }
        ParsedStatement::AlterTrigger {
            name,
            table,
            rename_to,
            enabled,
        } => {
            let (schema, table_name) = resolve(table);
            let key = (schema.clone(), table_name.clone(), name.clone());
            let mut trigger = catalog.triggers.remove(&key).ok_or_else(|| {
                invalid(
                    source,
                    index,
                    format!("trigger {name:?} for relation {table_name:?} does not exist"),
                    "ALTER TRIGGER",
                )
            })?;
            if let Some(to) = rename_to {
                let new_key = (schema, table_name.clone(), to.clone());
                if catalog.triggers.contains_key(&new_key) {
                    return Err(invalid(
                        source,
                        index,
                        format!("trigger {to:?} for relation {table_name:?} already exists"),
                        "ALTER TRIGGER",
                    ));
                }
                trigger.name.clone_from(to);
                catalog.triggers.insert(new_key, trigger);
            } else {
                if let Some(state) = enabled {
                    trigger.enabled = !matches!(state, TriggerEnabled::Disable);
                }
                catalog.triggers.insert(key, trigger);
            }
            Ok(())
        }
        ParsedStatement::CreatePolicy { policy } => {
            if catalog.table(&policy.schema, &policy.table).is_none() {
                return Err(invalid(
                    source,
                    index,
                    format!("table {}.{} does not exist", policy.schema, policy.table),
                    "CREATE POLICY",
                ));
            }
            let key = (
                policy.schema.clone(),
                policy.table.clone(),
                policy.name.clone(),
            );
            if catalog.policies.contains_key(&key) {
                return Err(invalid(
                    source,
                    index,
                    format!(
                        "policy {:?} for table {:?} already exists",
                        policy.name, policy.table
                    ),
                    "CREATE POLICY",
                ));
            }
            catalog.policies.insert(key, policy.clone());
            Ok(())
        }
        ParsedStatement::DropPolicy {
            name,
            table,
            if_exists,
        } => {
            let (schema, table_name) = resolve(table);
            let key = (schema.clone(), table_name.clone(), name.clone());
            if catalog.policies.remove(&key).is_none() && !if_exists {
                return Err(invalid(
                    source,
                    index,
                    format!("policy {name:?} for table {table_name:?} does not exist"),
                    "DROP POLICY",
                ));
            }
            drop_comments_for(catalog, &format!("{schema}.{table_name}.{name}"));
            Ok(())
        }
        ParsedStatement::AlterPolicy {
            name,
            table,
            rename_to,
            roles,
            using,
            check,
        } => {
            let (schema, table_name) = resolve(table);
            let key = (schema.clone(), table_name.clone(), name.clone());
            let mut policy = catalog.policies.remove(&key).ok_or_else(|| {
                invalid(
                    source,
                    index,
                    format!("policy {name:?} for table {table_name:?} does not exist"),
                    "ALTER POLICY",
                )
            })?;
            if let Some(roles) = roles {
                policy.roles.clone_from(roles);
            }
            if let Some(using) = using {
                policy.using = Some(using.clone());
            }
            if let Some(check) = check {
                policy.check = Some(check.clone());
            }
            if let Some(to) = rename_to {
                let new_key = (schema, table_name.clone(), to.clone());
                if catalog.policies.contains_key(&new_key) {
                    return Err(invalid(
                        source,
                        index,
                        format!("policy {to:?} for table {table_name:?} already exists"),
                        "ALTER POLICY",
                    ));
                }
                policy.name.clone_from(to);
                catalog.policies.insert(new_key, policy);
            } else {
                catalog.policies.insert(key, policy);
            }
            Ok(())
        }
        ParsedStatement::AlterTableRls {
            table,
            enabled,
            forced,
        } => {
            let (schema, name) = resolve(table);
            let target = catalog.table_mut(&schema, &name).ok_or_else(|| {
                invalid(
                    source,
                    index,
                    format!("table {schema}.{name} does not exist"),
                    "ALTER TABLE ... ROW LEVEL SECURITY",
                )
            })?;
            if let Some(enabled) = enabled {
                target.rls_enabled = *enabled;
            }
            if let Some(forced) = forced {
                target.rls_forced = *forced;
            }
            Ok(())
        }
        ParsedStatement::Comment { object, text } => {
            if let Some(text) = text {
                catalog.comments.insert(object.clone(), text.clone());
            } else {
                catalog.comments.remove(object);
            }
            Ok(())
        }
        ParsedStatement::Grant { grants, revoke } => {
            apply_grants(catalog, source, index, grants, *revoke)
        }
        ParsedStatement::SetPartitioning { table, partition } => {
            let (schema, name) = resolve(table);
            let target = catalog.table_mut(&schema, &name).ok_or_else(|| {
                invalid(
                    source,
                    index,
                    format!("table {schema}.{name} does not exist"),
                    "ALTER TABLE ... PARTITION BY",
                )
            })?;
            validate_partition_keys(source, index, target, partition.as_ref(), "ALTER TABLE")?;
            target.partition.clone_from(partition);
            Ok(())
        }
        ParsedStatement::AttachPartition {
            table,
            partition,
            bound,
            is_default,
            detach,
        } => apply_attach_partition(
            catalog,
            source,
            index,
            table,
            partition,
            bound,
            *is_default,
            *detach,
        ),
        _ => Ok(()),
    }
}

fn apply_create_type(
    catalog: &mut Catalog,
    source: &str,
    index: usize,
    pg_type: &PgType,
) -> Result<()> {
    ensure_type_free(
        catalog,
        &pg_type.schema,
        &pg_type.name,
        source,
        index,
        "CREATE TYPE",
    )?;
    catalog.ensure_schema(&pg_type.schema);
    catalog.types.insert(
        (pg_type.schema.clone(), pg_type.name.clone()),
        pg_type.clone(),
    );
    Ok(())
}

fn apply_alter_type(
    catalog: &mut Catalog,
    source: &str,
    index: usize,
    name: &[String],
    actions: &[TypeAction],
    domain: bool,
) -> Result<()> {
    let (schema, type_name) = resolve(name);
    let key = (schema.clone(), type_name.clone());
    if !catalog.types.contains_key(&key) {
        return Err(invalid(
            source,
            index,
            format!(
                "{} {schema}.{type_name} does not exist",
                if domain { "domain" } else { "type" }
            ),
            "ALTER TYPE",
        ));
    }
    for action in actions {
        // Actions that move the object are applied by key replacement at the
        // end of the loop; all others mutate in place.
        let (move_to, move_schema) = match action {
            TypeAction::RenameType { to } => (Some(to.clone()), None),
            TypeAction::SetSchema { schema: s } => (None, Some(s.clone())),
            _ => (None, None),
        };
        if move_to.is_some() || move_schema.is_some() {
            let mut pg_type = catalog
                .types
                .remove(&key)
                .ok_or_else(|| invalid(source, index, "type vanished mid-alter", "ALTER TYPE"))?;
            if let Some(to) = move_to {
                ensure_type_free(catalog, &schema, &to, source, index, "ALTER TYPE")?;
                pg_type.name = to;
            }
            if let Some(s) = move_schema {
                ensure_type_free(catalog, &s, &pg_type.name, source, index, "ALTER TYPE")?;
                pg_type.schema.clone_from(&s);
                catalog.ensure_schema(&s);
            }
            catalog
                .types
                .insert((pg_type.schema.clone(), pg_type.name.clone()), pg_type);
            continue;
        }
        let pg_type = require_type_mut(catalog, &schema, &type_name, source, index, "ALTER TYPE")?;
        match (action, &mut pg_type.kind) {
            (
                TypeAction::AddEnumValue {
                    value,
                    position,
                    if_not_exists,
                },
                PgTypeKind::Enum { values },
            ) => {
                if values.contains(value) {
                    if *if_not_exists {
                        continue;
                    }
                    return Err(invalid(
                        source,
                        index,
                        format!("enum label {value:?} already exists"),
                        "ALTER TYPE",
                    ));
                }
                let at = match position {
                    None => values.len(),
                    Some((before, neighbour)) => {
                        let pos = values.iter().position(|v| v == neighbour).ok_or_else(|| {
                            invalid(
                                source,
                                index,
                                format!("enum label {neighbour:?} does not exist"),
                                "ALTER TYPE",
                            )
                        })?;
                        if *before { pos } else { pos + 1 }
                    }
                };
                values.insert(at, value.clone());
            }
            (TypeAction::RenameEnumValue { from, to }, PgTypeKind::Enum { values }) => {
                let pos = values.iter().position(|v| v == from).ok_or_else(|| {
                    invalid(
                        source,
                        index,
                        format!("enum label {from:?} does not exist"),
                        "ALTER TYPE",
                    )
                })?;
                if values.contains(to) {
                    return Err(invalid(
                        source,
                        index,
                        format!("enum label {to:?} already exists"),
                        "ALTER TYPE",
                    ));
                }
                values[pos].clone_from(to);
            }
            (TypeAction::AddAttribute { attribute }, PgTypeKind::Composite { attributes }) => {
                if attributes.iter().any(|a| a.name == attribute.name) {
                    return Err(invalid(
                        source,
                        index,
                        format!("attribute {:?} already exists", attribute.name),
                        "ALTER TYPE",
                    ));
                }
                attributes.push(attribute.clone());
            }
            (
                TypeAction::DropAttribute {
                    name: attr,
                    if_exists,
                },
                PgTypeKind::Composite { attributes },
            ) => {
                let before = attributes.len();
                attributes.retain(|a| a.name != *attr);
                if attributes.len() == before && !if_exists {
                    return Err(invalid(
                        source,
                        index,
                        format!("attribute {attr:?} does not exist"),
                        "ALTER TYPE",
                    ));
                }
            }
            (
                TypeAction::AlterAttribute {
                    name: attr,
                    data_type,
                    collation,
                },
                PgTypeKind::Composite { attributes },
            ) => {
                let target = attributes
                    .iter_mut()
                    .find(|a| a.name == *attr)
                    .ok_or_else(|| {
                        invalid(
                            source,
                            index,
                            format!("attribute {attr:?} does not exist"),
                            "ALTER TYPE",
                        )
                    })?;
                target.data_type.clone_from(data_type);
                target.collation.clone_from(collation);
            }
            (TypeAction::RenameAttribute { from, to }, PgTypeKind::Composite { attributes }) => {
                if attributes.iter().any(|a| a.name == *to) {
                    return Err(invalid(
                        source,
                        index,
                        format!("attribute {to:?} already exists"),
                        "ALTER TYPE",
                    ));
                }
                let target = attributes
                    .iter_mut()
                    .find(|a| a.name == *from)
                    .ok_or_else(|| {
                        invalid(
                            source,
                            index,
                            format!("attribute {from:?} does not exist"),
                            "ALTER TYPE",
                        )
                    })?;
                target.name.clone_from(to);
            }
            (TypeAction::SetNotNull, PgTypeKind::Domain { constraints, .. }) => {
                add_domain_not_null(constraints, source, index)?;
            }
            (TypeAction::DropNotNull, PgTypeKind::Domain { constraints, .. }) => {
                constraints.retain(|c| !matches!(c.kind, DomainConstraintKind::NotNull));
            }
            (TypeAction::SetDefault { expr: e }, PgTypeKind::Domain { default, .. }) => {
                *default = Some(e.clone());
            }
            (TypeAction::DropDefault, PgTypeKind::Domain { default, .. }) => {
                *default = None;
            }
            (
                TypeAction::AddDomainConstraint { constraint },
                PgTypeKind::Domain { constraints, .. },
            ) => {
                let mut constraint = constraint.clone();
                if constraint.name.is_empty() {
                    let label = match &constraint.kind {
                        DomainConstraintKind::NotNull => "not_null",
                        DomainConstraintKind::Check { .. } => "check",
                    };
                    constraint.name = crate::naming::auto_constraint_name(
                        &type_name,
                        None,
                        label,
                        &|c| constraints.iter().any(|existing| existing.name == c),
                        &[],
                    );
                }
                if constraints.iter().any(|c| c.name == constraint.name) {
                    return Err(invalid(
                        source,
                        index,
                        format!("constraint {:?} already exists", constraint.name),
                        "ALTER DOMAIN",
                    ));
                }
                constraints.push(constraint);
            }
            (
                TypeAction::DropDomainConstraint {
                    name: attr,
                    if_exists,
                },
                PgTypeKind::Domain { constraints, .. },
            ) => {
                let before = constraints.len();
                constraints.retain(|c| c.name != *attr);
                if constraints.len() == before && !if_exists {
                    return Err(invalid(
                        source,
                        index,
                        format!("constraint {attr:?} does not exist"),
                        "ALTER DOMAIN",
                    ));
                }
            }
            (
                TypeAction::RenameDomainConstraint { from, to },
                PgTypeKind::Domain { constraints, .. },
            ) => {
                let target = constraints
                    .iter_mut()
                    .find(|c| c.name == *from)
                    .ok_or_else(|| {
                        invalid(
                            source,
                            index,
                            format!("constraint {from:?} does not exist"),
                            "ALTER DOMAIN",
                        )
                    })?;
                target.name.clone_from(to);
            }
            (
                TypeAction::ValidateDomainConstraint { name: attr },
                PgTypeKind::Domain { constraints, .. },
            ) => {
                if !constraints.iter().any(|c| c.name == *attr) {
                    return Err(invalid(
                        source,
                        index,
                        format!("constraint {attr:?} does not exist"),
                        "ALTER DOMAIN",
                    ));
                }
            }
            _ => {
                return Err(invalid(
                    source,
                    index,
                    format!("action {action:?} is not valid for this type kind"),
                    "ALTER TYPE",
                ));
            }
        }
    }
    Ok(())
}

fn add_domain_not_null(
    constraints: &mut Vec<DomainConstraint>,
    source: &str,
    index: usize,
) -> Result<()> {
    if constraints
        .iter()
        .any(|c| matches!(c.kind, DomainConstraintKind::NotNull))
    {
        return Err(invalid(
            source,
            index,
            "domain already has a NOT NULL constraint",
            "ALTER DOMAIN",
        ));
    }
    constraints.push(DomainConstraint {
        name: crate::naming::auto_constraint_name(
            "domain",
            None,
            "not_null",
            &|c| constraints.iter().any(|existing| existing.name == c),
            &[],
        ),
        kind: DomainConstraintKind::NotNull,
    });
    Ok(())
}

fn apply_create_sequence(
    catalog: &mut Catalog,
    source: &str,
    index: usize,
    sequence: &Sequence,
) -> Result<()> {
    ensure_relation_free(
        catalog,
        &sequence.schema,
        &sequence.name,
        source,
        index,
        "CREATE SEQUENCE",
    )?;
    catalog.ensure_schema(&sequence.schema);
    catalog.sequences.insert(
        (sequence.schema.clone(), sequence.name.clone()),
        sequence.clone(),
    );
    Ok(())
}

fn apply_alter_sequence(
    catalog: &mut Catalog,
    source: &str,
    index: usize,
    name: &[String],
    actions: &[SequenceAction],
) -> Result<()> {
    let (schema, seq_name) = resolve(name);
    let key = (schema.clone(), seq_name.clone());
    for action in actions {
        let (move_to, move_schema) = match action {
            SequenceAction::Rename(to) => (Some(to.clone()), None),
            SequenceAction::SetSchema(s) => (None, Some(s.clone())),
            _ => (None, None),
        };
        if move_to.is_some() || move_schema.is_some() {
            let mut seq = catalog.sequences.remove(&key).ok_or_else(|| {
                invalid(
                    source,
                    index,
                    format!("sequence {schema}.{seq_name} does not exist"),
                    "ALTER SEQUENCE",
                )
            })?;
            if let Some(to) = move_to {
                ensure_relation_free(catalog, &schema, &to, source, index, "ALTER SEQUENCE")?;
                seq.name = to;
            }
            if let Some(s) = move_schema {
                ensure_relation_free(catalog, &s, &seq.name, source, index, "ALTER SEQUENCE")?;
                seq.schema.clone_from(&s);
                catalog.ensure_schema(&s);
            }
            catalog
                .sequences
                .insert((seq.schema.clone(), seq.name.clone()), seq);
            continue;
        }
        let seq = catalog.sequences.get_mut(&key).ok_or_else(|| {
            invalid(
                source,
                index,
                format!("sequence {schema}.{seq_name} does not exist"),
                "ALTER SEQUENCE",
            )
        })?;
        match action {
            SequenceAction::SetType(data_type) => {
                if !matches!(data_type.as_str(), "smallint" | "integer" | "bigint") {
                    return Err(invalid(
                        source,
                        index,
                        format!(
                            "sequence type must be smallint, integer or bigint, found {data_type:?}"
                        ),
                        "ALTER SEQUENCE",
                    ));
                }
                seq.data_type.clone_from(data_type);
            }
            SequenceAction::SetOptions(opts) => {
                let mut merged = crate::sql::ParsedSequenceOptions {
                    increment: Some(seq.options.increment),
                    minvalue: Some(seq.options.minvalue),
                    maxvalue: Some(seq.options.maxvalue),
                    start: Some(seq.options.start),
                    cache: Some(seq.options.cache),
                    cycle: Some(seq.options.cycle),
                };
                if let Some(v) = opts.increment {
                    merged.increment = Some(v);
                }
                if let Some(v) = opts.minvalue {
                    merged.minvalue = Some(v);
                }
                if let Some(v) = opts.maxvalue {
                    merged.maxvalue = Some(v);
                }
                if let Some(v) = opts.start {
                    merged.start = Some(v);
                }
                if let Some(v) = opts.cache {
                    merged.cache = Some(v);
                }
                if let Some(v) = opts.cycle {
                    merged.cycle = Some(v);
                }
                seq.options = crate::sql::normalize_sequence_options(
                    source,
                    index,
                    &seq.data_type,
                    &merged,
                    "ALTER SEQUENCE",
                )?;
            }
            SequenceAction::OwnedBy(owner) => seq.owned_by.clone_from(owner),
            SequenceAction::Rename(_) | SequenceAction::SetSchema(_) => unreachable!(),
        }
    }
    Ok(())
}

fn apply_create_view(
    catalog: &mut Catalog,
    source: &str,
    index: usize,
    view: &crate::model::View,
    or_replace: bool,
) -> Result<()> {
    let key = (view.schema.clone(), view.name.clone());
    match catalog.views.get(&key) {
        Some(existing) if existing.materialized != view.materialized => {
            return Err(invalid(
                source,
                index,
                format!(
                    "{}.{} is a different kind of relation",
                    view.schema, view.name
                ),
                "CREATE VIEW",
            ));
        }
        Some(_) if !or_replace => {
            return Err(invalid(
                source,
                index,
                format!("relation {:?} already exists", view.name),
                "CREATE VIEW",
            ));
        }
        _ => {}
    }
    ensure_relation_free(
        catalog,
        &view.schema,
        &view.name,
        source,
        index,
        "CREATE VIEW",
    )?;
    catalog.ensure_schema(&view.schema);
    catalog.views.insert(key, view.clone());
    Ok(())
}

fn apply_alter_view(
    catalog: &mut Catalog,
    source: &str,
    index: usize,
    name: &[String],
    actions: &[ViewAction],
) -> Result<()> {
    let (schema, view_name) = resolve(name);
    let key = (schema.clone(), view_name.clone());
    for action in actions {
        let (move_to, move_schema) = match action {
            ViewAction::Rename(to) => (Some(to.clone()), None),
            ViewAction::SetSchema(s) => (None, Some(s.clone())),
            _ => (None, None),
        };
        if move_to.is_some() || move_schema.is_some() {
            let mut view = catalog.views.remove(&key).ok_or_else(|| {
                invalid(
                    source,
                    index,
                    format!("view {schema}.{view_name} does not exist"),
                    "ALTER VIEW",
                )
            })?;
            if let Some(to) = move_to {
                ensure_relation_free(catalog, &schema, &to, source, index, "ALTER VIEW")?;
                view.name = to;
            }
            if let Some(s) = move_schema {
                ensure_relation_free(catalog, &s, &view.name, source, index, "ALTER VIEW")?;
                view.schema.clone_from(&s);
                catalog.ensure_schema(&s);
            }
            catalog
                .views
                .insert((view.schema.clone(), view.name.clone()), view);
            continue;
        }
        let view = catalog.views.get_mut(&key).ok_or_else(|| {
            invalid(
                source,
                index,
                format!("view {schema}.{view_name} does not exist"),
                "ALTER VIEW",
            )
        })?;
        match action {
            ViewAction::SetOption(option, value) => {
                if let Some(existing) = view.options.iter_mut().find(|(k, _)| k == option) {
                    existing.1.clone_from(value);
                } else {
                    view.options.push((option.clone(), value.clone()));
                }
            }
            ViewAction::DropOption(option) => {
                view.options.retain(|(k, _)| k != option);
            }
            ViewAction::Rename(_) | ViewAction::SetSchema(_) => unreachable!(),
        }
    }
    Ok(())
}

fn apply_alter_routine(
    catalog: &mut Catalog,
    source: &str,
    index: usize,
    name: &(Vec<String>, Vec<String>),
    actions: &[RoutineAction],
) -> Result<()> {
    let (schema, routine_name) = resolve(&name.0);
    let key = (schema.clone(), routine_name.clone(), name.1.join(","));
    for action in actions {
        let (move_to, move_schema) = match action {
            RoutineAction::Rename(to) => (Some(to.clone()), None),
            RoutineAction::SetSchema(s) => (None, Some(s.clone())),
            _ => (None, None),
        };
        if move_to.is_some() || move_schema.is_some() {
            let mut routine = catalog
                .routines
                .remove(&key)
                .ok_or_else(|| invalid(source, index, "routine does not exist", "ALTER ROUTINE"))?;
            if let Some(to) = move_to {
                let new_key = (schema.clone(), to.clone(), routine.signature());
                if catalog.routines.contains_key(&new_key) {
                    return Err(invalid(
                        source,
                        index,
                        format!("routine {to:?} with this signature already exists"),
                        "ALTER ROUTINE",
                    ));
                }
                routine.name = to;
            }
            if let Some(s) = move_schema {
                routine.schema.clone_from(&s);
                catalog.ensure_schema(&s);
            }
            let new_key = (
                routine.schema.clone(),
                routine.name.clone(),
                routine.signature(),
            );
            catalog.routines.insert(new_key, routine);
            continue;
        }
        let routine = catalog
            .routines
            .get_mut(&key)
            .ok_or_else(|| invalid(source, index, "routine does not exist", "ALTER ROUTINE"))?;
        match action {
            RoutineAction::SetOption(option, value) => {
                if let Some(existing) = routine.settings.iter_mut().find(|(k, _)| k == option) {
                    existing.1.clone_from(value);
                } else {
                    routine.settings.push((option.clone(), value.clone()));
                }
            }
            RoutineAction::ResetOption(option) if option.is_empty() => routine.settings.clear(),
            RoutineAction::ResetOption(option) => {
                routine.settings.retain(|(k, _)| k != option);
            }
            RoutineAction::Rename(_) | RoutineAction::SetSchema(_) => unreachable!(),
        }
    }
    Ok(())
}

fn apply_grants(
    catalog: &mut Catalog,
    source: &str,
    index: usize,
    grants: &[Grant],
    revoke: bool,
) -> Result<()> {
    for grant in grants {
        if grant.grantee.is_empty() {
            return Err(invalid(source, index, "empty grantee", "GRANT"));
        }
        let key = (grant.object.clone(), grant.grantee.clone());
        if revoke {
            if let Some(existing) = catalog.grants.get_mut(&key) {
                existing
                    .privileges
                    .retain(|p| !grant.privileges.contains(p));
                if existing.privileges.is_empty() {
                    catalog.grants.remove(&key);
                }
            }
        } else if let Some(existing) = catalog.grants.get_mut(&key) {
            existing.privileges.extend(grant.privileges.iter().cloned());
            existing.with_grant_option |= grant.with_grant_option;
        } else {
            catalog.grants.insert(key, grant.clone());
        }
    }
    Ok(())
}

fn validate_partition_keys(
    source: &str,
    index: usize,
    table: &Table,
    partition: Option<&crate::model::Partitioning>,
    statement: &str,
) -> Result<()> {
    let Some(partition) = partition else {
        return Ok(());
    };
    for key in &partition.keys {
        if let crate::model::PartitionKey::Column(column) = key
            && !table.columns.iter().any(|c| c.name == *column)
        {
            return Err(invalid(
                source,
                index,
                format!("partition key column {column:?} does not exist on the table"),
                statement,
            ));
        }
    }
    Ok(())
}

fn apply_attach_partition(
    catalog: &mut Catalog,
    source: &str,
    index: usize,
    parent: &[String],
    partition: &[String],
    bound: &str,
    is_default: bool,
    detach: bool,
) -> Result<()> {
    let (parent_schema, parent_name) = resolve(parent);
    let (schema, name) = resolve(partition);
    let parent_table = catalog
        .table(&parent_schema, &parent_name)
        .ok_or_else(|| {
            invalid(
                source,
                index,
                format!("parent table {parent_schema}.{parent_name} does not exist"),
                "ALTER TABLE ... PARTITION",
            )
        })?
        .clone();
    if detach {
        let table = catalog.table_mut(&schema, &name).ok_or_else(|| {
            invalid(
                source,
                index,
                format!("partition {schema}.{name} does not exist"),
                "DETACH PARTITION",
            )
        })?;
        if table.partition_of.as_ref() != Some(&(parent_schema.clone(), parent_name.clone())) {
            return Err(invalid(
                source,
                index,
                format!("{schema}.{name} is not a partition of {parent_schema}.{parent_name}"),
                "DETACH PARTITION",
            ));
        }
        table.partition_of = None;
        catalog.partitions.remove(&(schema, name));
        return Ok(());
    }
    if parent_table.partition.is_none() {
        return Err(invalid(
            source,
            index,
            format!("table {parent_schema}.{parent_name} is not partitioned"),
            "ATTACH PARTITION",
        ));
    }
    if !is_default && bound.trim().is_empty() {
        return Err(invalid(
            source,
            index,
            "a partition needs FOR VALUES ... or DEFAULT",
            "ATTACH PARTITION",
        ));
    }
    match catalog.table(&schema, &name).cloned() {
        None => {
            let mut columns = parent_table.columns.clone();
            // Generated columns are inherited, not re-declared on partitions.
            columns.retain(|c| c.generated.is_none());
            catalog.ensure_schema(&schema);
            catalog.tables.insert(
                (schema.clone(), name.clone()),
                Table {
                    schema: schema.clone(),
                    name: name.clone(),
                    columns,
                    constraints: Vec::new(),
                    partition: None,
                    partition_of: Some((parent_schema.clone(), parent_name.clone())),
                    rls_enabled: false,
                    rls_forced: false,
                },
            );
        }
        Some(existing) => {
            // `ATTACH PARTITION` needs an existing table whose columns match
            // the parent (CREATE TABLE docs, "Attaching/Detaching
            // Partitions"). Check the name/type pairs; physical order is not
            // significant to pgdelta's model.
            if existing.partition_of.is_some() {
                return Err(invalid(
                    source,
                    index,
                    format!("{schema}.{name} is already a partition"),
                    "ATTACH PARTITION",
                ));
            }
            let mut parent_columns: Vec<(&str, &str)> = parent_table
                .columns
                .iter()
                .map(|c| (c.name.as_str(), c.data_type.as_str()))
                .collect();
            let mut child_columns: Vec<(&str, &str)> = existing
                .columns
                .iter()
                .map(|c| (c.name.as_str(), c.data_type.as_str()))
                .collect();
            parent_columns.sort_unstable();
            child_columns.sort_unstable();
            if parent_columns != child_columns {
                return Err(invalid(
                    source,
                    index,
                    format!("partition {schema}.{name} does not match the parent's columns"),
                    "ATTACH PARTITION",
                ));
            }
            let table = catalog
                .table_mut(&schema, &name)
                .ok_or_else(|| invalid(source, index, "partition vanished", "ATTACH PARTITION"))?;
            table.partition_of = Some((parent_schema.clone(), parent_name.clone()));
        }
    }
    catalog.partitions.insert(
        (schema.clone(), name.clone()),
        Partition {
            schema,
            name,
            parent_schema,
            parent_table: parent_name,
            bound: bound.to_owned(),
            is_default,
        },
    );
    Ok(())
}
