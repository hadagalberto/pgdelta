//! Opt-in rename hints for the planner.
//!
//! The SPEC is explicit: renames are never guessed silently. A rename only
//! happens when the caller supplies `--rename <kind>.<old>=<new>`; otherwise
//! the object diffs as a drop plus a create, which is honest and explicit.
//!
//! Hint grammar (all identifiers are simple, unquoted names):
//!
//! * `table.a=b` / `table.public.a=b` — table `a` → `b`;
//! * `column.users.a=b` / `column.public.users.a=b` — column `a` → `b`;
//! * `index.a=b`, `view.a=b`, `type.a=b`, `sequence.a=b` — same as `table`.
//!
//! The SQL emitted by the planner follows the official PostgreSQL
//! documentation (fetched 2026-11-20):
//! `ALTER TABLE ... RENAME TO` / `RENAME [COLUMN] ... TO` and
//! <https://www.postgresql.org/docs/current/sql-altertable.html>;
//! `ALTER INDEX ... RENAME TO`
//! <https://www.postgresql.org/docs/current/sql-alterindex.html>;
//! `ALTER TYPE ... RENAME TO`
//! <https://www.postgresql.org/docs/current/sql-altertype.html>;
//! `ALTER VIEW ... RENAME TO`
//! <https://www.postgresql.org/docs/current/sql-alterview.html>;
//! `ALTER SEQUENCE ... RENAME TO` is the sequence counterpart of the
//! `ALTER TABLE` RENAME form ("the name of a table (or an index, sequence,
//! view, materialized view, or foreign table)").
//!
//! Rewriting catalog keys and their embedded names is inherently repetitive
//! (one field per object kind); the style lints below would only add noise
//! to that mechanical code.
#![allow(
    clippy::assigning_clones,
    clippy::collapsible_if,
    clippy::collapsible_match
)]

use crate::error::{Error, Result};
use crate::model::{
    Catalog, ConstraintKind, IndexElemKind, ObjectKind, ObjectRef, PUBLIC_SCHEMA, PartitionKey,
    PgTypeKind, SequenceOwner,
};
use crate::naming::{qualified, quote_ident};

/// Kind of object a hint renames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameKind {
    /// A table (including a partition).
    Table,
    /// A column of a table.
    Column,
    /// A standalone index.
    Index,
    /// A plain or materialized view.
    View,
    /// A user-defined type, domain or range.
    Type,
    /// A standalone sequence.
    Sequence,
}

impl RenameKind {
    fn parse(word: &str) -> Option<Self> {
        match word {
            "table" => Some(RenameKind::Table),
            "column" => Some(RenameKind::Column),
            "index" => Some(RenameKind::Index),
            "view" => Some(RenameKind::View),
            "type" | "domain" => Some(RenameKind::Type),
            "sequence" => Some(RenameKind::Sequence),
            _ => None,
        }
    }
}

/// A parsed `--rename` hint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameHint {
    /// Object kind.
    pub kind: RenameKind,
    /// Schema of the object (defaults to `public`).
    pub schema: String,
    /// Table owning a renamed column (`None` for other kinds).
    pub table: Option<String>,
    /// Current name.
    pub old: String,
    /// Desired name, which must already exist in the target catalog.
    pub new: String,
}

fn hint_error(text: &str, message: impl std::fmt::Display) -> Error {
    Error::Invalid {
        origin: "--rename hint".to_owned(),
        index: 0,
        message: message.to_string(),
        statement: text.to_owned(),
    }
}

fn clean(part: &str) -> String {
    part.trim().trim_matches('"').trim().to_owned()
}

/// Parses one `--rename` value.
///
/// # Errors
///
/// Returns [`Error::Invalid`] for a missing `=`, an unknown kind, a wrong
/// number of path components, or an empty identifier.
pub fn parse_rename_hint(text: &str) -> Result<RenameHint> {
    let (left, right) = text
        .split_once('=')
        .ok_or_else(|| hint_error(text, "expected <kind>.<path>=<new-name>"))?;
    let new = clean(right);
    let parts: Vec<String> = left.split('.').map(clean).collect();
    let kind = parts
        .first()
        .and_then(|p| RenameKind::parse(p))
        .ok_or_else(|| {
            hint_error(
                text,
                "unknown object kind (use table, column, index, view, type or sequence)",
            )
        })?;
    if new.is_empty() || new.contains('.') {
        return Err(hint_error(
            text,
            "new name must be a single non-empty identifier",
        ));
    }
    let (schema, table, old) = match (kind, parts.len()) {
        (RenameKind::Column, 3) => (
            PUBLIC_SCHEMA.to_owned(),
            parts.get(1).cloned(),
            parts[2].clone(),
        ),
        (RenameKind::Column, 4) => (parts[1].clone(), parts.get(2).cloned(), parts[3].clone()),
        (RenameKind::Column, _) => {
            return Err(hint_error(
                text,
                "column hints need a table: column.<table>.<old>=<new> or column.<schema>.<table>.<old>=<new>",
            ));
        }
        (_, 2) => (PUBLIC_SCHEMA.to_owned(), None, parts[1].clone()),
        (_, 3) => (parts[1].clone(), None, parts[2].clone()),
        (_, _) => {
            return Err(hint_error(
                text,
                "expected <kind>.<old>=<new> or <kind>.<schema>.<old>=<new>",
            ));
        }
    };
    if old.is_empty() || schema.is_empty() || table.as_deref().is_some_and(str::is_empty) {
        return Err(hint_error(text, "identifiers must be non-empty"));
    }
    Ok(RenameHint {
        kind,
        schema,
        table,
        old,
        new,
    })
}

/// A rename statement the planner places before all drops and creates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RenameOp {
    /// Phase: table renames precede renames of their columns/indexes.
    pub(crate) rank: u32,
    /// Deterministic ordering key.
    pub(crate) key: String,
    /// Affected object, for JSON summaries.
    pub(crate) object: String,
    /// The `ALTER ... RENAME ...` statement.
    pub(crate) sql: String,
}

fn invalid(hint: &RenameHint, message: impl std::fmt::Display) -> Error {
    Error::Invalid {
        origin: "--rename hint".to_owned(),
        index: 0,
        message: message.to_string(),
        statement: format!("{}={}", hint.old, hint.new),
    }
}

/// Applies hints to a clone of `from` and returns the matching rename
/// statements.
///
/// A hint is rejected unless the old object exists in the source, the new
/// name exists in the target, and the old name is gone from the target — a
/// typo must never silently degrade into a destructive drop+create.
///
/// # Errors
///
/// Returns [`Error::Invalid`] when a hint cannot be applied consistently.
pub(crate) fn apply_rename_hints(
    from: &Catalog,
    to: &Catalog,
    hints: &[RenameHint],
) -> Result<(Catalog, Vec<RenameOp>)> {
    let mut working = from.clone();
    let mut ops = Vec::new();
    for hint in hints {
        let op = match hint.kind {
            RenameKind::Table => apply_table(&mut working, to, hint)?,
            RenameKind::Column => apply_column(&mut working, to, hint)?,
            RenameKind::Index => apply_index(&mut working, to, hint)?,
            RenameKind::View => apply_view(&mut working, to, hint)?,
            RenameKind::Type => apply_type(&mut working, to, hint)?,
            RenameKind::Sequence => apply_sequence(&mut working, to, hint)?,
        };
        ops.push(op);
    }
    Ok((working, ops))
}

fn apply_table(working: &mut Catalog, to: &Catalog, hint: &RenameHint) -> Result<RenameOp> {
    let old_key = (hint.schema.clone(), hint.old.clone());
    let new_key = (hint.schema.clone(), hint.new.clone());
    let Some(mut table) = working.tables.remove(&old_key) else {
        return Err(invalid(
            hint,
            format!(
                "table {} does not exist in the source",
                qualified(&hint.schema, &hint.old)
            ),
        ));
    };
    if !to.tables.contains_key(&new_key) {
        return Err(invalid(
            hint,
            format!("target has no table {}", qualified(&hint.schema, &hint.new)),
        ));
    }
    if to.tables.contains_key(&old_key) || working.tables.contains_key(&new_key) {
        return Err(invalid(
            hint,
            "the rename would collide with an existing table",
        ));
    }
    table.name = hint.new.clone();
    working.tables.insert(new_key, table);
    rename_table_refs(working, &hint.schema, &hint.old, &hint.new);
    let qual = qualified(&hint.schema, &hint.old);
    Ok(RenameOp {
        rank: 1,
        key: format!("rename-table:{qual}"),
        object: qualified(&hint.schema, &hint.new),
        sql: format!("ALTER TABLE {qual} RENAME TO {};", quote_ident(&hint.new)),
    })
}

fn apply_column(working: &mut Catalog, to: &Catalog, hint: &RenameHint) -> Result<RenameOp> {
    let table_name = hint.table.as_deref().unwrap_or_default();
    let old_key = (hint.schema.clone(), table_name.to_owned());
    let Some(mut table) = working.tables.remove(&old_key) else {
        return Err(invalid(
            hint,
            format!(
                "table {} does not exist in the source",
                qualified(&hint.schema, table_name)
            ),
        ));
    };
    if !table.columns.iter().any(|c| c.name == hint.old) {
        working.tables.insert(old_key, table);
        return Err(invalid(
            hint,
            format!("column {} does not exist in the source", hint.old),
        ));
    }
    let target = to.table(&hint.schema, table_name);
    if target.is_none_or(|t| t.column(&hint.new).is_none()) {
        working.tables.insert(old_key, table);
        return Err(invalid(hint, format!("target has no column {}", hint.new)));
    }
    if target.is_some_and(|t| t.column(&hint.old).is_some())
        || table.columns.iter().any(|c| c.name == hint.new)
    {
        working.tables.insert(old_key, table);
        return Err(invalid(hint, "the column rename would collide"));
    }
    for c in &mut table.columns {
        if c.name == hint.old {
            c.name = hint.new.clone();
        }
    }
    working.tables.insert(old_key, table);
    rename_column_refs(working, &hint.schema, table_name, &hint.old, &hint.new);
    let qual = qualified(&hint.schema, table_name);
    Ok(RenameOp {
        rank: 2,
        key: format!("rename-column:{qual}:{}", hint.old),
        object: format!("{table_name}.{}", hint.new),
        sql: format!(
            "ALTER TABLE {qual} RENAME COLUMN {} TO {};",
            quote_ident(&hint.old),
            quote_ident(&hint.new)
        ),
    })
}

fn apply_index(working: &mut Catalog, to: &Catalog, hint: &RenameHint) -> Result<RenameOp> {
    let old_key = (hint.schema.clone(), hint.old.clone());
    let new_key = (hint.schema.clone(), hint.new.clone());
    let Some(mut index) = working.indexes.remove(&old_key) else {
        return Err(invalid(
            hint,
            format!(
                "index {} does not exist in the source",
                qualified(&hint.schema, &hint.old)
            ),
        ));
    };
    if !to.indexes.contains_key(&new_key) || working.indexes.contains_key(&new_key) {
        return Err(invalid(
            hint,
            "target has no such index (or the name is taken)",
        ));
    }
    index.name = hint.new.clone();
    working.indexes.insert(new_key, index);
    rename_object_ref(
        working,
        ObjectKind::Index,
        &hint.schema,
        &hint.old,
        &hint.new,
    );
    let qual = qualified(&hint.schema, &hint.old);
    Ok(RenameOp {
        rank: 2,
        key: format!("rename-index:{qual}"),
        object: qualified(&hint.schema, &hint.new),
        sql: format!("ALTER INDEX {qual} RENAME TO {};", quote_ident(&hint.new)),
    })
}

fn apply_view(working: &mut Catalog, to: &Catalog, hint: &RenameHint) -> Result<RenameOp> {
    let old_key = (hint.schema.clone(), hint.old.clone());
    let new_key = (hint.schema.clone(), hint.new.clone());
    let Some(mut view) = working.views.remove(&old_key) else {
        return Err(invalid(
            hint,
            format!(
                "view {} does not exist in the source",
                qualified(&hint.schema, &hint.old)
            ),
        ));
    };
    if !to.views.contains_key(&new_key) || working.views.contains_key(&new_key) {
        return Err(invalid(
            hint,
            "target has no such view (or the name is taken)",
        ));
    }
    let materialized = view.materialized;
    view.name = hint.new.clone();
    working.views.insert(new_key, view);
    let kind = if materialized {
        ObjectKind::MaterializedView
    } else {
        ObjectKind::View
    };
    rename_object_ref(working, kind, &hint.schema, &hint.old, &hint.new);
    let qual = qualified(&hint.schema, &hint.old);
    let verb = if materialized {
        "ALTER MATERIALIZED VIEW"
    } else {
        "ALTER VIEW"
    };
    Ok(RenameOp {
        rank: 2,
        key: format!("rename-view:{qual}"),
        object: qualified(&hint.schema, &hint.new),
        sql: format!("{verb} {qual} RENAME TO {};", quote_ident(&hint.new)),
    })
}

fn apply_type(working: &mut Catalog, to: &Catalog, hint: &RenameHint) -> Result<RenameOp> {
    let old_key = (hint.schema.clone(), hint.old.clone());
    let new_key = (hint.schema.clone(), hint.new.clone());
    let Some(mut pg_type) = working.types.remove(&old_key) else {
        return Err(invalid(hint, "type does not exist in the source"));
    };
    if !to.types.contains_key(&new_key) {
        return Err(invalid(hint, "target has no such type"));
    }
    pg_type.name = hint.new.clone();
    working.types.insert(new_key, pg_type);
    // Column/attribute/domain types reference the old name verbatim.
    let old_qual = qualified(&hint.schema, &hint.old);
    for table in working.tables.values_mut() {
        for column in &mut table.columns {
            if column.data_type == old_qual {
                column.data_type = qualified(&hint.schema, &hint.new);
            }
        }
    }
    for pg_type in working.types.values_mut() {
        match &mut pg_type.kind {
            PgTypeKind::Composite { attributes } => {
                for attr in attributes {
                    if attr.data_type == old_qual {
                        attr.data_type = qualified(&hint.schema, &hint.new);
                    }
                }
            }
            PgTypeKind::Domain { base_type, .. } => {
                if *base_type == old_qual {
                    *base_type = qualified(&hint.schema, &hint.new);
                }
            }
            PgTypeKind::Range { subtype, .. } => {
                if *subtype == old_qual {
                    *subtype = qualified(&hint.schema, &hint.new);
                }
            }
            PgTypeKind::Enum { .. } => {}
        }
    }
    rename_object_ref(
        working,
        ObjectKind::Type,
        &hint.schema,
        &hint.old,
        &hint.new,
    );
    let qual = qualified(&hint.schema, &hint.old);
    Ok(RenameOp {
        rank: 2,
        key: format!("rename-type:{qual}"),
        object: qualified(&hint.schema, &hint.new),
        sql: format!("ALTER TYPE {qual} RENAME TO {};", quote_ident(&hint.new)),
    })
}

fn apply_sequence(working: &mut Catalog, to: &Catalog, hint: &RenameHint) -> Result<RenameOp> {
    let old_key = (hint.schema.clone(), hint.old.clone());
    let new_key = (hint.schema.clone(), hint.new.clone());
    let Some(mut sequence) = working.sequences.remove(&old_key) else {
        return Err(invalid(hint, "sequence does not exist in the source"));
    };
    if !to.sequences.contains_key(&new_key) {
        return Err(invalid(hint, "target has no such sequence"));
    }
    sequence.name = hint.new.clone();
    working.sequences.insert(new_key, sequence);
    rename_object_ref(
        working,
        ObjectKind::Sequence,
        &hint.schema,
        &hint.old,
        &hint.new,
    );
    let qual = qualified(&hint.schema, &hint.old);
    Ok(RenameOp {
        rank: 2,
        key: format!("rename-sequence:{qual}"),
        object: qualified(&hint.schema, &hint.new),
        sql: format!(
            "ALTER SEQUENCE {qual} RENAME TO {};",
            quote_ident(&hint.new)
        ),
    })
}

pub(crate) fn rename_table_refs(catalog: &mut Catalog, schema: &str, old: &str, new: &str) {
    for index in catalog.indexes.values_mut() {
        if index.table_schema == schema && index.table == old {
            index.table = new.to_owned();
        }
    }
    let trigger_keys: Vec<_> = catalog
        .triggers
        .keys()
        .filter(|k| k.0 == schema && k.1 == old)
        .cloned()
        .collect();
    for key in trigger_keys {
        if let Some(mut trigger) = catalog.triggers.remove(&key) {
            trigger.table = new.to_owned();
            catalog
                .triggers
                .insert((schema.to_owned(), new.to_owned(), key.2), trigger);
        }
    }
    let policy_keys: Vec<_> = catalog
        .policies
        .keys()
        .filter(|k| k.0 == schema && k.1 == old)
        .cloned()
        .collect();
    for key in policy_keys {
        if let Some(mut policy) = catalog.policies.remove(&key) {
            policy.table = new.to_owned();
            catalog
                .policies
                .insert((schema.to_owned(), new.to_owned(), key.2), policy);
        }
    }
    for sequence in catalog.sequences.values_mut() {
        if let Some(SequenceOwner {
            schema: s, table, ..
        }) = &mut sequence.owned_by
        {
            if s == schema && table == old {
                *table = new.to_owned();
            }
        }
    }
    for partition in catalog.partitions.values_mut() {
        if partition.parent_schema == schema && partition.parent_table == old {
            partition.parent_table = new.to_owned();
        }
    }
    for table in catalog.tables.values_mut() {
        for constraint in &mut table.constraints {
            if let ConstraintKind::ForeignKey {
                ref_schema,
                ref_table,
                ..
            } = &mut constraint.kind
            {
                if ref_schema == schema && ref_table == old {
                    *ref_table = new.to_owned();
                }
            }
        }
    }
    for view in catalog.views.values_mut() {
        view.query = replace_ident(&view.query, old, new);
    }
    rename_object_ref(catalog, ObjectKind::Table, schema, old, new);
}

fn rename_column_refs(catalog: &mut Catalog, schema: &str, table: &str, old: &str, new: &str) {
    if let Some(target) = catalog
        .tables
        .get_mut(&(schema.to_owned(), table.to_owned()))
    {
        for constraint in &mut target.constraints {
            match &mut constraint.kind {
                ConstraintKind::PrimaryKey { columns } | ConstraintKind::Unique { columns, .. } => {
                    replace_in_list(columns, old, new);
                }
                ConstraintKind::Check { expr } => *expr = replace_ident(expr, old, new),
                ConstraintKind::Exclusion { spec } => *spec = replace_ident(spec, old, new),
                ConstraintKind::ForeignKey {
                    columns,
                    ref_schema,
                    ref_table,
                    ref_columns,
                    ..
                } => {
                    replace_in_list(columns, old, new);
                    if ref_schema == schema && ref_table == table {
                        replace_in_list(ref_columns, old, new);
                    }
                }
            }
        }
        if let Some(partition) = &mut target.partition {
            for key in &mut partition.keys {
                if let PartitionKey::Column(column) = key
                    && column == old
                {
                    *column = new.to_owned();
                }
                if let PartitionKey::Expression(expr) = key {
                    *expr = replace_ident(expr, old, new);
                }
            }
        }
    }
    for table_ref in catalog.tables.values_mut() {
        for constraint in &mut table_ref.constraints {
            if let ConstraintKind::ForeignKey {
                ref_schema,
                ref_table,
                ref_columns,
                ..
            } = &mut constraint.kind
                && ref_schema == schema
                && ref_table == table
            {
                replace_in_list(ref_columns, old, new);
            }
        }
    }
    for index in catalog.indexes.values_mut() {
        if index.table_schema == schema && index.table == table {
            for elem in &mut index.columns {
                match &mut elem.kind {
                    IndexElemKind::Column(column) => {
                        if column == old {
                            *column = new.to_owned();
                        }
                    }
                    IndexElemKind::Expression(expr) => *expr = replace_ident(expr, old, new),
                }
            }
            replace_in_list(&mut index.include, old, new);
            if let Some(predicate) = &mut index.predicate {
                *predicate = replace_ident(predicate, old, new);
            }
        }
    }
    for trigger in catalog.triggers.values_mut() {
        if trigger.schema == schema && trigger.table == table {
            if let Some(condition) = &mut trigger.condition {
                *condition = replace_ident(condition, old, new);
            }
        }
    }
    for sequence in catalog.sequences.values_mut() {
        if let Some(owner) = &mut sequence.owned_by
            && owner.schema == schema
            && owner.table == table
            && owner.column == old
        {
            owner.column = new.to_owned();
        }
    }
    for view in catalog.views.values_mut() {
        view.query = replace_ident(&view.query, old, new);
    }
    rename_object_ref(
        catalog,
        ObjectKind::Column,
        schema,
        &format!("{table}.{old}"),
        &format!("{table}.{new}"),
    );
    rename_grants(
        catalog,
        ObjectKind::Column,
        schema,
        &format!("{table}.{old}"),
        &format!("{table}.{new}"),
    );
}

fn replace_in_list(list: &mut Vec<String>, old: &str, new: &str) {
    for item in list {
        if item == old {
            *item = new.to_owned();
        }
    }
}

/// Replaces identifier tokens (quoted or bare) in a normalised SQL
/// expression. String literals are left untouched: they are data, not
/// references to the renamed object.
fn replace_ident(text: &str, old: &str, new: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '"' => {
                let start = i + 1;
                i = start;
                while i < chars.len() && chars[i] != '"' {
                    i += 1;
                }
                let name: String = chars[start..i].iter().collect();
                if name == old {
                    out.push('"');
                    out.push_str(new);
                    out.push('"');
                } else {
                    out.push('"');
                    out.push_str(&name);
                    out.push('"');
                }
                if i < chars.len() {
                    i += 1;
                }
            }
            '\'' => {
                out.push('\'');
                i += 1;
                while i < chars.len() {
                    out.push(chars[i]);
                    if chars[i] == '\'' {
                        i += 1;
                        if i < chars.len() && chars[i] == '\'' {
                            out.push('\'');
                            i += 1;
                        } else {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            c if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                if word == old {
                    out.push_str(new);
                } else {
                    out.push_str(&word);
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

fn rename_object_ref(catalog: &mut Catalog, kind: ObjectKind, schema: &str, old: &str, new: &str) {
    let old_name = format!(
        "{}.{}",
        quote_ident(schema),
        old.split('.')
            .map(quote_ident)
            .collect::<Vec<_>>()
            .join(".")
    );
    let new_name = format!(
        "{}.{}",
        quote_ident(schema),
        new.split('.')
            .map(quote_ident)
            .collect::<Vec<_>>()
            .join(".")
    );
    let keys: Vec<_> = catalog
        .comments
        .keys()
        .filter(|o| o.kind == kind && o.name == old_name)
        .cloned()
        .collect();
    for key in keys {
        if let Some(value) = catalog.comments.remove(&key) {
            catalog
                .comments
                .insert(ObjectRef::new(kind, new_name.clone()), value);
        }
    }
    rename_grants(catalog, kind, schema, old, new);
}

fn rename_grants(catalog: &mut Catalog, kind: ObjectKind, schema: &str, old: &str, new: &str) {
    let old_name = format!(
        "{}.{}",
        quote_ident(schema),
        old.split('.')
            .map(quote_ident)
            .collect::<Vec<_>>()
            .join(".")
    );
    let new_name = format!(
        "{}.{}",
        quote_ident(schema),
        new.split('.')
            .map(quote_ident)
            .collect::<Vec<_>>()
            .join(".")
    );
    let keys: Vec<_> = catalog
        .grants
        .keys()
        .filter(|(o, _)| o.kind == kind && o.name == old_name)
        .cloned()
        .collect();
    for (object, grantee) in keys {
        if let Some(mut grant) = catalog.grants.remove(&(object, grantee.clone())) {
            grant.object = ObjectRef::new(kind, new_name.clone());
            catalog
                .grants
                .insert((ObjectRef::new(kind, new_name.clone()), grantee), grant);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn parses_all_hint_forms() {
        assert_eq!(
            parse_rename_hint("table.users=new_users").unwrap(),
            RenameHint {
                kind: RenameKind::Table,
                schema: "public".into(),
                table: None,
                old: "users".into(),
                new: "new_users".into()
            }
        );
        assert_eq!(
            parse_rename_hint("table.sales.a=b").unwrap(),
            RenameHint {
                kind: RenameKind::Table,
                schema: "sales".into(),
                table: None,
                old: "a".into(),
                new: "b".into()
            }
        );
        assert_eq!(
            parse_rename_hint("column.users.email=contact").unwrap(),
            RenameHint {
                kind: RenameKind::Column,
                schema: "public".into(),
                table: Some("users".into()),
                old: "email".into(),
                new: "contact".into()
            }
        );
        assert_eq!(
            parse_rename_hint("column.sales.users.email=contact").unwrap(),
            RenameHint {
                kind: RenameKind::Column,
                schema: "sales".into(),
                table: Some("users".into()),
                old: "email".into(),
                new: "contact".into()
            }
        );
        for bad in [
            "users=b",
            "table.=b",
            "column.email=b",
            "table.a=",
            "table.a=b.c",
            "widget.a=b",
        ] {
            assert!(parse_rename_hint(bad).is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn applies_table_and_column_renames() {
        let from = crate::catalog_from_sql(
            "CREATE TABLE public.users (id integer PRIMARY KEY, email text); \
             CREATE TABLE public.posts (id integer, user_id integer REFERENCES public.users(id)); \
             CREATE INDEX users_email_idx ON public.users (email);",
            "from.sql",
        )
        .unwrap();
        let to = crate::catalog_from_sql(
            "CREATE TABLE public.people (id integer PRIMARY KEY, email text); \
             CREATE TABLE public.posts (id integer, user_id integer REFERENCES public.people(id)); \
             CREATE INDEX people_email_idx ON public.people (email); \
             ALTER TABLE public.people RENAME COLUMN email TO contact;",
            "to.sql",
        )
        .unwrap();
        let hints = [
            parse_rename_hint("table.users=people").unwrap(),
            parse_rename_hint("column.people.email=contact").unwrap(),
            parse_rename_hint("index.users_email_idx=people_email_idx").unwrap(),
        ];
        let (renamed, ops) = apply_rename_hints(&from, &to, &hints).unwrap();
        assert_eq!(ops.len(), 3);
        assert!(renamed.table("public", "people").is_some());
        assert!(renamed.table("public", "users").is_none());
        // The FK reference followed the table; the index followed both the
        // table and the column rename.
        let posts = renamed.table("public", "posts").unwrap();
        assert!(posts.constraints.iter().any(|c| matches!(&c.kind, ConstraintKind::ForeignKey { ref_table, .. } if ref_table == "people")));
        let index = renamed.index("public", "people_email_idx").unwrap();
        assert_eq!(index.table, "people");
        assert!(
            index
                .columns
                .iter()
                .any(|e| matches!(&e.kind, IndexElemKind::Column(c) if c == "contact"))
        );
        assert_eq!(
            renamed
                .table("public", "people")
                .unwrap()
                .column("contact")
                .unwrap()
                .data_type,
            "text"
        );
    }
}
