//! Applies parsed statements to a catalog, mirroring PostgreSQL's
//! execution semantics for the modelled object kinds.
//!
//! Every rule enforced here cites its PostgreSQL counterpart in a comment
//! (error conditions, implied flags such as `PRIMARY KEY` implying
//! `NOT NULL`, auto-generated names, and the resolution of `REFERENCES
//! reftable` without a column list to the referenced table's primary
//! key).

use crate::error::{Error, Result};
use crate::model::{
    Catalog, Column, Constraint, ConstraintKind, Extension, Generated, Identity, Index,
    IndexElemKind, PUBLIC_SCHEMA, Table,
};
use crate::naming;
use crate::sql::{
    ParsedStatement, RawColumn, RawConstraint, RawConstraintKind, RawForeignKey, RawIndex,
    RawTable, alter_table::AlterAction, alter_table::SeqOption, normalize_sequence_options,
};

/// Applies a slice of parsed statements (with 1-based statement indices
/// starting at `index_base`) to `catalog`.
///
/// `source` names the origin for error messages. Every catalog produced
/// by this path carries the ambient `public` schema, mirroring a
/// PostgreSQL database.
pub(crate) fn apply_statements(
    catalog: &mut Catalog,
    source: &str,
    statements: &[ParsedStatement],
    index_base: usize,
) -> Result<()> {
    catalog.ensure_schema(PUBLIC_SCHEMA);
    for (offset, stmt) in statements.iter().enumerate() {
        apply_statement(catalog, source, index_base + offset, stmt)?;
    }
    Ok(())
}

/// Truncates an explicit identifier to PostgreSQL's 63-byte limit
/// (`NAMEDATALEN - 1`; PostgreSQL docs §4.1.1: "The system uses no more
/// than NAMEDATALEN-1 bytes of an identifier; longer names can be written
/// in commands, but they will be truncated").
fn trunc(name: &str) -> String {
    crate::naming::truncate_ident(name)
}

/// Splits parsed name parts into `(schema, name)`, defaulting the schema
/// to `public` (mirroring the default `search_path`).
fn resolve_name(parts: &[String]) -> (String, String) {
    match parts.len() {
        1 => (PUBLIC_SCHEMA.to_owned(), parts[0].clone()),
        _ => (parts[0].clone(), parts[1].clone()),
    }
}

/// Relation names occupied in a schema: tables, standalone indexes, and
/// the backing indexes of `PRIMARY KEY`/`UNIQUE` constraints (which are
/// catalog relations sharing the constraint's name — they participate in
/// PostgreSQL's `ChooseRelationName` collision scan of `pg_class`).
fn occupied_relation_names(catalog: &Catalog, schema: &str) -> Vec<String> {
    let mut names = catalog.relation_names(schema);
    for table in catalog.tables.values().filter(|t| t.schema == schema) {
        for c in &table.constraints {
            if matches!(
                c.kind,
                ConstraintKind::PrimaryKey { .. } | ConstraintKind::Unique { .. }
            ) {
                names.push(c.name.clone());
            }
        }
    }
    names
}

fn same_set(a: &[String], b: &[String]) -> bool {
    a.len() == b.len() && a.iter().all(|x| b.contains(x))
}

fn constraint_columns(constraint: &Constraint) -> &[String] {
    match &constraint.kind {
        ConstraintKind::PrimaryKey { columns } | ConstraintKind::Unique { columns, .. } => columns,
        ConstraintKind::ForeignKey { columns, .. } => columns,
        ConstraintKind::Check { .. } => &[],
    }
}

fn constraint_columns_mut(constraint: &mut Constraint) -> Option<&mut Vec<String>> {
    match &mut constraint.kind {
        ConstraintKind::PrimaryKey { columns } | ConstraintKind::Unique { columns, .. } => {
            Some(columns)
        }
        ConstraintKind::ForeignKey { columns, .. } => Some(columns),
        ConstraintKind::Check { .. } => None,
    }
}

fn apply_statement(
    catalog: &mut Catalog,
    source: &str,
    index: usize,
    stmt: &ParsedStatement,
) -> Result<()> {
    match stmt {
        ParsedStatement::CreateSchema {
            name,
            if_not_exists,
        } => apply_create_schema(catalog, source, index, &name[0], *if_not_exists),
        ParsedStatement::DropSchema { name, if_exists } => {
            apply_drop_schema(catalog, source, index, &name[0], *if_exists)
        }
        ParsedStatement::CreateExtension {
            name,
            if_not_exists,
            schema,
        } => apply_create_extension(
            catalog,
            source,
            index,
            name,
            *if_not_exists,
            schema.as_deref(),
        ),
        ParsedStatement::DropExtension { name, if_exists } => {
            apply_drop_extension(catalog, source, index, name, *if_exists)
        }
        ParsedStatement::CreateTable {
            name,
            if_not_exists,
            raw,
        } => apply_create_table(catalog, source, index, name, *if_not_exists, raw),
        ParsedStatement::DropTable { names, if_exists } => {
            apply_drop_table(catalog, source, index, names, *if_exists)
        }
        ParsedStatement::AlterTable {
            name,
            if_exists,
            actions,
        } => apply_alter_table(catalog, source, index, name, *if_exists, actions),
        ParsedStatement::CreateIndex { raw } => apply_create_index(catalog, source, index, raw),
        ParsedStatement::DropIndex { names, if_exists } => {
            apply_drop_index(catalog, source, index, names, *if_exists)
        }
        ParsedStatement::AlterIndexRename { name, new_name } => {
            apply_alter_index_rename(catalog, source, index, name, new_name)
        }
        ParsedStatement::SessionOp => Ok(()),
    }
}

fn apply_create_schema(
    catalog: &mut Catalog,
    source: &str,
    index: usize,
    name: &str,
    if_not_exists: bool,
) -> Result<()> {
    if catalog.schemas.contains_key(name) {
        if if_not_exists {
            return Ok(());
        }
        return Err(Error::invalid(
            source,
            index,
            format!("schema {name:?} already exists"),
            "CREATE SCHEMA",
        ));
    }
    catalog.ensure_schema(name);
    Ok(())
}

fn apply_drop_schema(
    catalog: &mut Catalog,
    source: &str,
    index: usize,
    name: &str,
    if_exists: bool,
) -> Result<()> {
    if name == PUBLIC_SCHEMA {
        return Err(Error::invalid(
            source,
            index,
            "cannot drop the ambient public schema",
            "DROP SCHEMA",
        ));
    }
    if !catalog.schemas.contains_key(name) {
        if if_exists {
            return Ok(());
        }
        return Err(Error::invalid(
            source,
            index,
            format!("schema {name:?} does not exist"),
            "DROP SCHEMA",
        ));
    }
    let occupied: Vec<String> = catalog
        .tables
        .keys()
        .chain(catalog.indexes.keys())
        .filter(|(s, _)| s == name)
        .map(|(_, n)| n.clone())
        .collect();
    if !occupied.is_empty() {
        return Err(Error::invalid(
            source,
            index,
            format!(
                "cannot drop schema {name:?} because other objects depend on it (first: {:?}); \
                 drop the contained objects explicitly",
                occupied[0]
            ),
            "DROP SCHEMA",
        ));
    }
    catalog.schemas.remove(name);
    Ok(())
}

fn apply_create_extension(
    catalog: &mut Catalog,
    source: &str,
    index: usize,
    name: &str,
    if_not_exists: bool,
    schema: Option<&str>,
) -> Result<()> {
    if catalog.extensions.contains_key(name) {
        if if_not_exists {
            return Ok(());
        }
        return Err(Error::invalid(
            source,
            index,
            format!("extension {name:?} already exists"),
            "CREATE EXTENSION",
        ));
    }
    if let Some(schema) = schema {
        catalog.ensure_schema(schema);
    }
    catalog.extensions.insert(
        name.to_owned(),
        Extension {
            name: name.to_owned(),
            schema: schema.map(str::to_owned),
        },
    );
    Ok(())
}

fn apply_drop_extension(
    catalog: &mut Catalog,
    source: &str,
    index: usize,
    name: &str,
    if_exists: bool,
) -> Result<()> {
    if catalog.extensions.remove(name).is_none() && !if_exists {
        return Err(Error::invalid(
            source,
            index,
            format!("extension {name:?} does not exist"),
            "DROP EXTENSION",
        ));
    }
    Ok(())
}

/// Builds a model column from a raw definition, normalising the identity
/// options against the column's type (defaults per the `CREATE SEQUENCE`
/// documentation, which `CREATE TABLE`'s `sequence_options` reference).
fn build_column(source: &str, index: usize, raw: &RawColumn, text: &str) -> Result<Column> {
    let identity = match &raw.identity {
        Some((kind, opts)) => Some(Identity {
            kind: *kind,
            options: normalize_sequence_options(source, index, &raw.data_type, opts, text)?,
        }),
        None => None,
    };
    Ok(Column {
        name: trunc(&raw.name),
        data_type: raw.data_type.clone(),
        not_null: raw.not_null,
        default: raw.default.clone(),
        generated: raw.generated.clone().map(|expr| Generated { expr }),
        identity,
        collation: raw.collation.clone(),
    })
}

/// Auto-names an unnamed constraint exactly as PostgreSQL does, per
/// `ChooseConstraintName` / `ChooseIndexName` (see `naming.rs` for the
/// cited sources).
fn auto_name(
    catalog: &Catalog,
    schema: &str,
    table: &str,
    constraint: &RawConstraint,
    others: &[String],
) -> String {
    let relations = occupied_relation_names(catalog, schema);
    let constraints = catalog.constraint_names(schema);
    let relations_taken = |c: &str| relations.iter().any(|r| r == c);
    let constraints_taken = |c: &str| constraints.iter().any(|r| r == c);
    match &constraint.kind {
        RawConstraintKind::PrimaryKey { .. } => naming::auto_index_name(
            table,
            &[],
            true,
            true,
            &relations_taken,
            &constraints_taken,
            others,
        ),
        RawConstraintKind::Unique { columns, .. } => naming::auto_index_name(
            table,
            columns,
            false,
            true,
            &relations_taken,
            &constraints_taken,
            others,
        ),
        RawConstraintKind::Check { .. } => {
            // Column-level checks name as table_column_check; table-level
            // as table_check (both via ChooseConstraintName).
            let name2 = constraint.column_level.as_deref();
            naming::auto_constraint_name(table, name2, "check", &constraints_taken, others)
        }
        RawConstraintKind::ForeignKey(fk) => naming::auto_constraint_name(
            table,
            Some(&naming::name_addition(&fk.columns)),
            "fkey",
            &constraints_taken,
            others,
        ),
    }
}

/// Resolves and validates a foreign key's referenced columns (PostgreSQL
/// docs, `CREATE TABLE`, `REFERENCES`: "If the refcolumn list is omitted,
/// the primary key of the reftable is used. Otherwise, the refcolumn list
/// must refer to the columns of a non-deferrable unique or primary key
/// constraint or be the columns of a non-partial unique index.").
fn resolve_fk(
    source: &str,
    index: usize,
    fk: &RawForeignKey,
    target: Option<&Table>,
    unique_indexes: &[&Index],
    text: &str,
) -> Result<(
    String,
    String,
    Vec<String>,
    crate::model::FkAction,
    crate::model::FkAction,
    crate::model::FkMatch,
)> {
    let Some(target) = target else {
        return Err(Error::invalid(
            source,
            index,
            format!(
                "referenced table {:?} does not exist (PostgreSQL requires the referenced \
                 table to exist when the constraint is created)",
                format!("{}.{}", fk.ref_schema, fk.ref_table)
            ),
            text,
        ));
    };
    let ref_columns = if let Some(explicit) = &fk.ref_columns {
        let deferrable_match = target.constraints.iter().find(|c| {
            matches!(
                &c.kind,
                ConstraintKind::PrimaryKey { .. } | ConstraintKind::Unique { .. }
            ) && same_set(constraint_columns(c), explicit)
        });
        if let Some(constraint) = deferrable_match {
            if constraint.deferrable.is_some() {
                return Err(Error::invalid(
                    source,
                    index,
                    format!(
                        "referenced constraint {:?} is deferrable; foreign keys may only \
                         reference non-deferrable unique or primary key constraints",
                        constraint.name
                    ),
                    text,
                ));
            }
        } else if !unique_indexes.iter().any(|idx| {
            idx.unique && idx.predicate.is_none() && index_key_set(idx) == explicit.as_slice()
        }) {
            return Err(Error::invalid(
                source,
                index,
                format!(
                    "referenced columns {:?} on table {:?} must match a non-deferrable \
                     unique or primary key constraint (PostgreSQL docs, CREATE TABLE, \
                     REFERENCES)",
                    explicit, target.name
                ),
                text,
            ));
        }
        explicit.clone()
    } else {
        let Some(pk) = target.primary_key_columns() else {
            return Err(Error::invalid(
                source,
                index,
                format!(
                    "referenced table {:?} has no primary key; specify the referenced \
                     columns explicitly",
                    target.name
                ),
                text,
            ));
        };
        pk.to_vec()
    };
    Ok((
        fk.ref_schema.clone(),
        fk.ref_table.clone(),
        ref_columns,
        fk.on_delete.clone(),
        fk.on_update.clone(),
        fk.match_type,
    ))
}

/// The key column names of an index (plain columns and expression text).
fn index_key_set(idx: &Index) -> Vec<String> {
    idx.columns
        .iter()
        .map(|e| match &e.kind {
            IndexElemKind::Column(c) => c.clone(),
            IndexElemKind::Expression(expr) => expr.clone(),
        })
        .collect()
}

/// Builds model constraints from raw (inline + table-level) definitions.
///
/// `in_progress` is the table under construction when this is called
/// from `CREATE TABLE` / `ADD COLUMN` (used to resolve self-referencing
/// foreign keys); when altering an existing table it is `None` and the
/// referenced table must already be in the catalog.
#[allow(clippy::too_many_arguments)]
fn build_constraints(
    source: &str,
    index: usize,
    catalog: &Catalog,
    schema: &str,
    table_name: &str,
    raws: &[RawConstraint],
    in_progress: Option<&Table>,
    text: &str,
) -> Result<Vec<Constraint>> {
    let mut constraints: Vec<Constraint> = Vec::new();
    let mut others: Vec<String> = Vec::new();
    for raw in raws {
        let name = if let Some(name) = &raw.name {
            crate::naming::truncate_ident(name)
        } else {
            let auto = auto_name(catalog, schema, table_name, raw, &others);
            others.push(auto.clone());
            auto
        };
        let kind = match &raw.kind {
            RawConstraintKind::PrimaryKey { columns } => ConstraintKind::PrimaryKey {
                columns: columns.iter().map(|c| trunc(c)).collect(),
            },
            RawConstraintKind::Unique {
                columns,
                nulls_distinct,
            } => ConstraintKind::Unique {
                columns: columns.iter().map(|c| trunc(c)).collect(),
                nulls_distinct: *nulls_distinct,
            },
            RawConstraintKind::Check { expr } => ConstraintKind::Check { expr: expr.clone() },
            RawConstraintKind::ForeignKey(fk) => {
                let self_reference = in_progress
                    .is_some_and(|t| t.schema == fk.ref_schema && t.name == fk.ref_table);
                let target = if self_reference {
                    in_progress
                } else {
                    catalog.table(&fk.ref_schema, &fk.ref_table)
                };
                let unique_indexes: Vec<&Index> = catalog
                    .indexes
                    .values()
                    .filter(|i| i.schema == fk.ref_schema && i.table == fk.ref_table && i.unique)
                    .collect();
                let (ref_schema, ref_table, ref_columns, on_delete, on_update, match_type) =
                    resolve_fk(source, index, fk, target, &unique_indexes, text)?;
                ConstraintKind::ForeignKey {
                    columns: fk.columns.iter().map(|c| trunc(c)).collect(),
                    ref_schema,
                    ref_table,
                    ref_columns,
                    on_delete,
                    on_update,
                    match_type,
                }
            }
        };
        constraints.push(Constraint {
            name,
            kind,
            deferrable: raw.deferrable.clone(),
            not_valid: false,
        });
    }
    // Constraint names must be unique per table (PostgreSQL:
    // "constraint ... for relation ... already exists").
    let mut seen = std::collections::BTreeSet::new();
    for c in &constraints {
        if !seen.insert(c.name.clone()) {
            return Err(Error::invalid(
                source,
                index,
                format!(
                    "constraint {:?} for relation {table_name:?} already exists",
                    c.name
                ),
                text,
            ));
        }
    }
    Ok(constraints)
}

/// Validates that every column referenced by a constraint exists
/// (PostgreSQL errors "column X named in constraint does not exist").
fn validate_constraint_columns(
    source: &str,
    index: usize,
    constraints: &[Constraint],
    table_columns: &[Column],
    text: &str,
) -> Result<()> {
    let names: Vec<&String> = table_columns.iter().map(|c| &c.name).collect();
    for constraint in constraints {
        for column in constraint_columns(constraint) {
            if !names.contains(&column) {
                return Err(Error::invalid(
                    source,
                    index,
                    format!(
                        "column {column:?} named in constraint {:?} does not exist",
                        constraint.name
                    ),
                    text,
                ));
            }
        }
    }
    Ok(())
}

/// Enforces the single-primary-key rule (PostgreSQL: "multiple primary
/// keys for table X are not allowed").
fn validate_single_primary_key(
    source: &str,
    index: usize,
    constraints: &[Constraint],
    text: &str,
) -> Result<()> {
    let pks = constraints
        .iter()
        .filter(|c| matches!(c.kind, ConstraintKind::PrimaryKey { .. }))
        .count();
    if pks > 1 {
        return Err(Error::invalid(
            source,
            index,
            "multiple primary keys are not allowed",
            text,
        ));
    }
    Ok(())
}

/// Marks the primary-key columns `NOT NULL` (PostgreSQL docs, `CREATE
/// TABLE`, `PRIMARY KEY`: "PRIMARY KEY enforces the same data constraints
/// as a combination of UNIQUE and NOT NULL").
fn imply_not_null(columns: &mut [Column], constraints: &[Constraint]) {
    for constraint in constraints {
        if let ConstraintKind::PrimaryKey { columns: pk } = &constraint.kind {
            for column in columns.iter_mut() {
                if pk.contains(&column.name) {
                    column.not_null = true;
                }
            }
        }
    }
}

/// Collects the full raw constraint list of a table body: table-level
/// constraints followed by every inline column constraint (annotated with
/// its column for auto-naming).
/// Validates that every identifier referenced by a `CHECK` expression is
/// a column of the owning table (PostgreSQL rejects unknown columns at
/// CREATE time: "column X does not exist").
fn validate_check_expr(
    source: &str,
    index: usize,
    expr: &str,
    columns: &[String],
    text: &str,
) -> Result<()> {
    for ident in expr_identifiers(expr, source, index, text)? {
        if !columns.contains(&ident) {
            return Err(Error::invalid(
                source,
                index,
                format!(
                    "column {ident:?} referenced in a CHECK expression does not exist on the \
                     table (PostgreSQL rejects unknown columns at CREATE time)"
                ),
                text,
            ));
        }
    }
    Ok(())
}

/// Validates a stored generated column's expression: identifiers must be
/// base columns — not the generated column itself and not other
/// generated columns (PostgreSQL docs, `CREATE TABLE`, `GENERATED ALWAYS
/// AS (generation_expr)`: "The generation expression can refer to other
/// columns in the table, but not other generated columns.").
fn validate_generated_expr(
    source: &str,
    index: usize,
    expr: &str,
    self_name: &str,
    columns: &[Column],
    text: &str,
) -> Result<()> {
    for ident in expr_identifiers(expr, source, index, text)? {
        if ident == self_name {
            return Err(Error::invalid(
                source,
                index,
                format!("generated column {self_name:?} cannot reference itself"),
                text,
            ));
        }
        match columns.iter().find(|c| c.name == ident) {
            None => {
                return Err(Error::invalid(
                    source,
                    index,
                    format!(
                        "column {ident:?} referenced in a generation expression does not \
                         exist on the table"
                    ),
                    text,
                ));
            }
            Some(referenced) if referenced.generated.is_some() => {
                return Err(Error::invalid(
                    source,
                    index,
                    format!(
                        "generated column {self_name:?} cannot reference generated column \
                         {ident:?} (PostgreSQL docs, CREATE TABLE, GENERATED ALWAYS AS)"
                    ),
                    text,
                ));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// Validates that a default expression references no columns: "The value
/// is any variable-free expression (in particular, cross-references to
/// other columns in the current table are not allowed)" (PostgreSQL
/// docs, `CREATE TABLE`, `DEFAULT`).
fn validate_default_expr(source: &str, index: usize, expr: &str, text: &str) -> Result<()> {
    let idents = expr_identifiers(expr, source, index, text)?;
    if let Some(ident) = idents.first() {
        return Err(Error::invalid(
            source,
            index,
            format!(
                "DEFAULT expressions may not reference columns (found {ident:?}; PostgreSQL \
                 docs, CREATE TABLE, DEFAULT)"
            ),
            text,
        ));
    }
    Ok(())
}

/// Collects the identifiers of a normalised expression. The normalised
/// form always quotes identifiers (see `sql/expr.rs`), so the walk is
/// exact; an unparsable expression is rejected.
fn expr_identifiers(
    normalised: &str,
    source: &str,
    index: usize,
    text: &str,
) -> Result<Vec<String>> {
    let parsed = crate::sql::expr::parse_expr_text(normalised).map_err(|e| {
        Error::invalid(
            source,
            index,
            format!("expression {normalised:?} is not parsable: {e}"),
            text,
        )
    })?;
    Ok(collect_idents(&parsed))
}

fn collect_idents(expr: &sqlparser::ast::Expr) -> Vec<String> {
    use sqlparser::ast::{Expr, FunctionArguments};
    let mut out = Vec::new();
    match expr {
        Expr::Identifier(ident) => out.push(ident.value.clone()),
        Expr::CompoundIdentifier(parts) => {
            if let Some(last) = parts.last() {
                out.push(last.value.clone());
            }
        }
        Expr::UnaryOp { expr, .. } => out.extend(collect_idents(expr)),
        Expr::BinaryOp { left, right, .. } => {
            out.extend(collect_idents(left));
            out.extend(collect_idents(right));
        }
        Expr::Nested(inner) => out.extend(collect_idents(inner)),
        Expr::Cast { expr, .. } => out.extend(collect_idents(expr)),
        // Literals (numbers, strings, booleans, NULL) are variable-free and
        // contribute no column references. Treating them as "unsupported"
        // wrongly rejected every normalised DEFAULT/GENERATED/CHECK
        // expression containing a literal.
        Expr::Value(_) => {}
        Expr::Function(f) => {
            if let FunctionArguments::List(list) = &f.args {
                for arg in &list.args {
                    if let sqlparser::ast::FunctionArg::Unnamed(
                        sqlparser::ast::FunctionArgExpr::Expr(e),
                    ) = arg
                    {
                        out.extend(collect_idents(e));
                    }
                }
            }
        }
        Expr::IsNull(inner) | Expr::IsNotNull(inner) => out.extend(collect_idents(inner)),
        Expr::Between {
            expr, low, high, ..
        } => {
            out.extend(collect_idents(expr));
            out.extend(collect_idents(low));
            out.extend(collect_idents(high));
        }
        Expr::InList { expr, list, .. } => {
            out.extend(collect_idents(expr));
            for item in list {
                out.extend(collect_idents(item));
            }
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(operand) = operand {
                out.extend(collect_idents(operand));
            }
            for when in conditions {
                out.extend(collect_idents(&when.condition));
                out.extend(collect_idents(&when.result));
            }
            if let Some(els) = else_result {
                out.extend(collect_idents(els));
            }
        }
        _ => {
            // Unsupported forms: record a sentinel that always fails
            // validation, so new expression support must extend this
            // walker consciously.
            out.push("\u{0}unsupported".to_owned());
        }
    }
    out
}

fn collect_constraints(raw: &RawTable) -> Vec<RawConstraint> {
    let mut out: Vec<RawConstraint> = raw.constraints.clone();
    for column in &raw.columns {
        for inline in &column.inline {
            out.push(RawConstraint {
                name: inline.name.clone(),
                kind: inline.kind.clone(),
                deferrable: inline.deferrable.clone(),
                column_level: Some(column.name.clone()),
            });
        }
    }
    out
}

/// Validates the expressions of a table body: defaults reference no
/// columns, generation expressions reference only base columns, and
/// `CHECK` expressions reference existing columns.
fn validate_table_expressions(
    source: &str,
    index: usize,
    columns: &[Column],
    constraints: &[Constraint],
    text: &str,
) -> Result<()> {
    for column in columns {
        if let Some(default) = &column.default {
            validate_default_expr(source, index, default, text)?;
        }
        if let Some(generated) = &column.generated {
            validate_generated_expr(source, index, &generated.expr, &column.name, columns, text)?;
        }
    }
    for constraint in constraints {
        if let ConstraintKind::Check { expr } = &constraint.kind {
            let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
            validate_check_expr(source, index, expr, &names, text)?;
        }
    }
    Ok(())
}

fn apply_create_table(
    catalog: &mut Catalog,
    source: &str,
    index: usize,
    name_parts: &[String],
    if_not_exists: bool,
    raw: &RawTable,
) -> Result<()> {
    let (schema, name) = resolve_name(name_parts);
    catalog.ensure_schema(&schema);
    if occupied_relation_names(catalog, &schema).contains(&name) {
        if if_not_exists {
            return Ok(());
        }
        return Err(Error::invalid(
            source,
            index,
            format!("relation {name:?} already exists"),
            "CREATE TABLE",
        ));
    }

    let mut columns: Vec<Column> = Vec::new();
    for raw_col in &raw.columns {
        let column = build_column(source, index, raw_col, "CREATE TABLE")?;
        if columns.iter().any(|c| c.name == column.name) {
            return Err(Error::invalid(
                source,
                index,
                format!("column {:?} specified more than once", column.name),
                "CREATE TABLE",
            ));
        }
        columns.push(column);
    }

    // Provisional table so self-referencing foreign keys resolve and the
    // constraint validators see the columns.
    let provisional = Table {
        schema: schema.clone(),
        name: name.clone(),
        columns: columns.clone(),
        constraints: Vec::new(),
    };
    let constraints = build_constraints(
        source,
        index,
        catalog,
        &schema,
        &name,
        &collect_constraints(raw),
        Some(&provisional),
        "CREATE TABLE",
    )?;
    validate_constraint_columns(source, index, &constraints, &columns, "CREATE TABLE")?;
    validate_single_primary_key(source, index, &constraints, "CREATE TABLE")?;
    validate_table_expressions(source, index, &columns, &constraints, "CREATE TABLE")?;
    imply_not_null(&mut columns, &constraints);

    catalog.tables.insert(
        (schema.clone(), name.clone()),
        Table {
            schema,
            name,
            columns,
            constraints,
        },
    );
    Ok(())
}

fn apply_drop_table(
    catalog: &mut Catalog,
    source: &str,
    index: usize,
    names: &[Vec<String>],
    if_exists: bool,
) -> Result<()> {
    for parts in names {
        let (schema, name) = resolve_name(parts);
        if catalog.table(&schema, &name).is_none() {
            if if_exists {
                continue;
            }
            return Err(Error::invalid(
                source,
                index,
                format!("table {schema}.{name:?} does not exist"),
                "DROP TABLE",
            ));
        }
        // PostgreSQL (RESTRICT, the default) refuses to drop a table that
        // other tables' foreign keys still reference.
        let referencing: Vec<String> = catalog
            .tables
            .values()
            .filter(|t| {
                t.constraints.iter().any(|c| match &c.kind {
                    ConstraintKind::ForeignKey {
                        ref_schema,
                        ref_table,
                        ..
                    } => ref_schema == &schema && ref_table == &name,
                    _ => false,
                })
            })
            .map(|t| format!("{}.{}", t.schema, t.name))
            .collect();
        if !referencing.is_empty() {
            return Err(Error::invalid(
                source,
                index,
                format!(
                    "cannot drop table {schema}.{name:?} because other objects depend on it \
                     (foreign keys from {referencing:?}); drop the referencing constraints explicitly"
                ),
                "DROP TABLE",
            ));
        }
        catalog.tables.remove(&(schema.clone(), name.clone()));
        catalog
            .indexes
            .retain(|_, idx| !(idx.schema == schema && idx.table == name));
    }
    Ok(())
}

fn apply_alter_table(
    catalog: &mut Catalog,
    source: &str,
    index: usize,
    name_parts: &[String],
    if_exists: bool,
    actions: &[AlterAction],
) -> Result<()> {
    let (schema, name) = resolve_name(name_parts);
    if catalog.table(&schema, &name).is_none() {
        if if_exists {
            return Ok(());
        }
        return Err(Error::invalid(
            source,
            index,
            format!("table {schema}.{name:?} does not exist"),
            "ALTER TABLE",
        ));
    }
    for action in actions {
        apply_alter_action(catalog, source, index, &schema, &name, action)?;
    }
    Ok(())
}

fn apply_alter_action(
    catalog: &mut Catalog,
    source: &str,
    index: usize,
    schema: &str,
    table_name: &str,
    action: &AlterAction,
) -> Result<()> {
    match action {
        AlterAction::AddColumn {
            if_not_exists,
            column: raw,
        } => apply_add_column(
            catalog,
            source,
            index,
            schema,
            table_name,
            raw,
            *if_not_exists,
        ),
        AlterAction::DropColumn { name, if_exists } => {
            apply_drop_column(catalog, source, index, schema, table_name, name, *if_exists)
        }
        AlterAction::AlterColumnType {
            column,
            data_type,
            collation,
        } => {
            let table = table_mut(catalog, schema, table_name, source, index)?;
            let col = column_mut(table, column, source, index, "ALTER COLUMN TYPE")?;
            col.data_type.clone_from(data_type);
            // "if omitted [COLLATE], the collation is the default for the
            // new column type" (PostgreSQL docs, ALTER TABLE, SET DATA
            // TYPE): the clause replaces the collation outright.
            col.collation.clone_from(collation);
            Ok(())
        }
        AlterAction::SetDefault { column, expr } => {
            let table = table_mut(catalog, schema, table_name, source, index)?;
            column_mut(table, column, source, index, "SET DEFAULT")?.default = Some(expr.clone());
            Ok(())
        }
        AlterAction::DropDefault { column } => {
            let table = table_mut(catalog, schema, table_name, source, index)?;
            column_mut(table, column, source, index, "DROP DEFAULT")?.default = None;
            Ok(())
        }
        AlterAction::SetNotNull { column } => {
            let table = table_mut(catalog, schema, table_name, source, index)?;
            column_mut(table, column, source, index, "SET NOT NULL")?.not_null = true;
            Ok(())
        }
        AlterAction::DropNotNull { column } => {
            let table = table_mut(catalog, schema, table_name, source, index)?;
            column_mut(table, column, source, index, "DROP NOT NULL")?.not_null = false;
            Ok(())
        }
        AlterAction::AddIdentity {
            column,
            kind,
            options,
        } => {
            let table = table_mut(catalog, schema, table_name, source, index)?;
            let col = column_mut(
                table,
                column,
                source,
                index,
                "ADD GENERATED ... AS IDENTITY",
            )?;
            if col.identity.is_some() {
                return Err(Error::invalid(
                    source,
                    index,
                    format!("column {column:?} is already an identity column"),
                    "ALTER TABLE",
                ));
            }
            let data_type = col.data_type.clone();
            col.identity = Some(Identity {
                kind: *kind,
                options: normalize_sequence_options(
                    source,
                    index,
                    &data_type,
                    options,
                    "ALTER TABLE",
                )?,
            });
            // Identity columns are implicitly NOT NULL.
            col.not_null = true;
            Ok(())
        }
        AlterAction::DropIdentity { column, if_exists } => {
            let table = table_mut(catalog, schema, table_name, source, index)?;
            let col = column_mut(table, column, source, index, "DROP IDENTITY")?;
            if col.identity.take().is_none() && !if_exists {
                return Err(Error::invalid(
                    source,
                    index,
                    format!("column {column:?} is not an identity column"),
                    "ALTER TABLE",
                ));
            }
            Ok(())
        }
        AlterAction::SetGenerated { column, kind } => {
            let table = table_mut(catalog, schema, table_name, source, index)?;
            let col = column_mut(table, column, source, index, "SET GENERATED")?;
            let Some(identity) = col.identity.as_mut() else {
                return Err(Error::invalid(
                    source,
                    index,
                    format!("column {column:?} is not an identity column"),
                    "ALTER TABLE",
                ));
            };
            identity.kind = *kind;
            Ok(())
        }
        AlterAction::SetSequenceOption { column, option } => {
            let table = table_mut(catalog, schema, table_name, source, index)?;
            let col = column_mut(table, column, source, index, "SET <sequence option>")?;
            let Some(identity) = col.identity.as_mut() else {
                return Err(Error::invalid(
                    source,
                    index,
                    format!("column {column:?} is not an identity column"),
                    "ALTER TABLE",
                ));
            };
            match option {
                SeqOption::Increment(v) => identity.options.increment = *v,
                SeqOption::MinValue(v) => identity.options.minvalue = *v,
                SeqOption::MaxValue(v) => identity.options.maxvalue = *v,
                SeqOption::Start(v) => identity.options.start = *v,
                SeqOption::Cache(v) => identity.options.cache = *v,
                SeqOption::Cycle(v) => identity.options.cycle = *v,
            }
            Ok(())
        }
        AlterAction::Restart => Ok(()),
        AlterAction::DropExpression { column, if_exists } => {
            let table = table_mut(catalog, schema, table_name, source, index)?;
            let col = column_mut(table, column, source, index, "DROP EXPRESSION")?;
            if col.generated.take().is_none() && !if_exists {
                return Err(Error::invalid(
                    source,
                    index,
                    format!("column {column:?} is not a stored generated column"),
                    "ALTER TABLE",
                ));
            }
            Ok(())
        }
        AlterAction::SetExpression { column, expr } => {
            let table = table_mut(catalog, schema, table_name, source, index)?;
            let col = column_mut(table, column, source, index, "SET EXPRESSION AS")?;
            if col.generated.is_none() {
                return Err(Error::invalid(
                    source,
                    index,
                    format!("column {column:?} is not a stored generated column"),
                    "ALTER TABLE",
                ));
            }
            col.generated = Some(Generated { expr: expr.clone() });
            Ok(())
        }
        AlterAction::AddConstraint {
            name,
            constraint: raw,
            not_valid,
        } => apply_add_constraint(
            catalog,
            source,
            index,
            schema,
            table_name,
            name.clone(),
            raw,
            *not_valid,
        ),
        AlterAction::DropConstraint { name, if_exists } => {
            let table = table_mut(catalog, schema, table_name, source, index)?;
            let before = table.constraints.len();
            table.constraints.retain(|c| c.name != *name);
            if table.constraints.len() == before && !if_exists {
                return Err(Error::invalid(
                    source,
                    index,
                    format!("constraint {name:?} for relation {table_name:?} does not exist"),
                    "ALTER TABLE",
                ));
            }
            Ok(())
        }
        AlterAction::ValidateConstraint { name } => {
            let table = table_mut(catalog, schema, table_name, source, index)?;
            let Some(constraint) = table.constraints.iter_mut().find(|c| c.name == *name) else {
                return Err(Error::invalid(
                    source,
                    index,
                    format!("constraint {name:?} for relation {table_name:?} does not exist"),
                    "ALTER TABLE",
                ));
            };
            match &constraint.kind {
                ConstraintKind::Check { .. } | ConstraintKind::ForeignKey { .. } => {
                    constraint.not_valid = false;
                    Ok(())
                }
                _ => Err(Error::invalid(
                    source,
                    index,
                    format!("constraint {name:?} is not a CHECK or foreign-key constraint"),
                    "ALTER TABLE",
                )),
            }
        }
        AlterAction::AlterConstraintDeferrable { name, deferrable } => {
            let table = table_mut(catalog, schema, table_name, source, index)?;
            let Some(constraint) = table.constraints.iter_mut().find(|c| c.name == *name) else {
                return Err(Error::invalid(
                    source,
                    index,
                    format!("constraint {name:?} for relation {table_name:?} does not exist"),
                    "ALTER TABLE",
                ));
            };
            match &constraint.kind {
                ConstraintKind::ForeignKey { .. } => {
                    constraint.deferrable.clone_from(deferrable);
                    Ok(())
                }
                _ => Err(Error::invalid(
                    source,
                    index,
                    format!(
                        "constraint {name:?} is not a foreign key; only foreign keys accept \
                         ALTER CONSTRAINT (PostgreSQL docs, ALTER TABLE, ALTER CONSTRAINT)"
                    ),
                    "ALTER TABLE",
                )),
            }
        }
        AlterAction::RenameColumn { from, to } => {
            apply_rename_column(catalog, source, index, schema, table_name, from, to)
        }
        AlterAction::RenameConstraint { from, to } => {
            let table = table_mut(catalog, schema, table_name, source, index)?;
            if table.constraints.iter().any(|c| c.name == *to) {
                return Err(Error::invalid(
                    source,
                    index,
                    format!("constraint {to:?} for relation {table_name:?} already exists"),
                    "ALTER TABLE",
                ));
            }
            let Some(constraint) = table.constraints.iter_mut().find(|c| c.name == *from) else {
                return Err(Error::invalid(
                    source,
                    index,
                    format!("constraint {from:?} for relation {table_name:?} does not exist"),
                    "ALTER TABLE",
                ));
            };
            constraint.name.clone_from(to);
            Ok(())
        }
        AlterAction::RenameTable { to } => {
            let new_name = trunc(to);
            if occupied_relation_names(catalog, schema).contains(&new_name) {
                return Err(Error::invalid(
                    source,
                    index,
                    format!("relation {new_name:?} already exists"),
                    "ALTER TABLE",
                ));
            }
            let mut table = catalog
                .tables
                .remove(&(schema.to_owned(), table_name.to_owned()))
                .ok_or_else(|| Error::internal(format!("table {schema}.{table_name} vanished")))?;
            table.name.clone_from(&new_name);
            for idx in catalog
                .indexes
                .values_mut()
                .filter(|idx| idx.schema == schema && idx.table == table_name)
            {
                idx.table.clone_from(&new_name);
            }
            catalog.tables.insert((schema.to_owned(), new_name), table);
            Ok(())
        }
    }
}

fn apply_add_column(
    catalog: &mut Catalog,
    source: &str,
    index: usize,
    schema: &str,
    table_name: &str,
    raw: &RawColumn,
    if_not_exists: bool,
) -> Result<()> {
    let existing = catalog
        .table(schema, table_name)
        .ok_or_else(|| Error::internal(format!("table {schema}.{table_name} vanished")))?
        .clone();
    if existing.columns.iter().any(|c| c.name == raw.name) {
        if if_not_exists {
            return Ok(());
        }
        return Err(Error::invalid(
            source,
            index,
            format!(
                "column {:?} of relation {table_name:?} already exists",
                raw.name
            ),
            "ALTER TABLE",
        ));
    }

    let mut column = build_column(source, index, raw, "ALTER TABLE")?;
    let raw_body = RawTable {
        columns: vec![raw.clone()],
        constraints: Vec::new(),
    };
    let constraint_raws = collect_constraints(&raw_body);
    let mut new_columns = existing.columns.clone();
    new_columns.push(column.clone());
    let provisional = Table {
        schema: schema.to_owned(),
        name: table_name.to_owned(),
        columns: new_columns.clone(),
        constraints: existing.constraints.clone(),
    };
    let constraints = build_constraints(
        source,
        index,
        catalog,
        schema,
        table_name,
        &constraint_raws,
        Some(&provisional),
        "ALTER TABLE",
    )?;
    validate_constraint_columns(source, index, &constraints, &new_columns, "ALTER TABLE")?;
    validate_table_expressions(source, index, &new_columns, &constraints, "ALTER TABLE")?;
    // PRIMARY KEY implies NOT NULL (PostgreSQL docs, CREATE TABLE,
    // PRIMARY KEY).
    for constraint in &constraints {
        if let ConstraintKind::PrimaryKey { columns } = &constraint.kind
            && columns.contains(&column.name)
        {
            column.not_null = true;
        }
    }

    let table = table_mut(catalog, schema, table_name, source, index)?;
    table.columns.push(column);
    table.constraints.extend(constraints);
    Ok(())
}

fn apply_drop_column(
    catalog: &mut Catalog,
    source: &str,
    index: usize,
    schema: &str,
    table_name: &str,
    name: &str,
    if_exists: bool,
) -> Result<()> {
    let table = table_mut(catalog, schema, table_name, source, index)?;
    let before = table.columns.len();
    table.columns.retain(|c| c.name != name);
    if table.columns.len() == before && !if_exists {
        return Err(Error::invalid(
            source,
            index,
            format!("column {name:?} of relation {table_name:?} does not exist"),
            "ALTER TABLE",
        ));
    }
    // "Indexes and table constraints involving the column will be
    // automatically dropped as well" (PostgreSQL docs, ALTER TABLE, DROP
    // COLUMN) — CASCADE would be needed for outside dependents, which the
    // parser rejects.
    table
        .constraints
        .retain(|c| !constraint_columns(c).iter().any(|col| col == name));
    catalog.indexes.retain(|_, idx| {
        !(idx.schema == schema
            && idx.table == table_name
            && (idx
                .columns
                .iter()
                .any(|e| matches!(&e.kind, IndexElemKind::Column(c) if c == name))
                || idx.include.iter().any(|c| c == name)))
    });
    Ok(())
}

fn apply_add_constraint(
    catalog: &mut Catalog,
    source: &str,
    index: usize,
    schema: &str,
    table_name: &str,
    name: Option<String>,
    raw: &RawConstraint,
    not_valid: bool,
) -> Result<()> {
    let existing = catalog
        .table(schema, table_name)
        .ok_or_else(|| Error::internal(format!("table {schema}.{table_name} vanished")))?
        .clone();
    let mut constraint = build_constraints(
        source,
        index,
        catalog,
        schema,
        table_name,
        std::slice::from_ref(raw),
        None,
        "ALTER TABLE",
    )?
    .into_iter()
    .next()
    .ok_or_else(|| Error::internal("ADD CONSTRAINT produced no constraint".to_owned()))?;
    if let Some(name) = name {
        constraint.name = trunc(&name);
        if existing
            .constraints
            .iter()
            .any(|c| c.name == constraint.name)
        {
            return Err(Error::invalid(
                source,
                index,
                format!(
                    "constraint {:?} for relation {table_name:?} already exists",
                    constraint.name
                ),
                "ALTER TABLE",
            ));
        }
    }
    if not_valid {
        match &constraint.kind {
            ConstraintKind::Check { .. } | ConstraintKind::ForeignKey { .. } => {}
            _ => {
                return Err(Error::invalid(
                    source,
                    index,
                    "NOT VALID is only allowed for CHECK and foreign-key constraints",
                    "ALTER TABLE",
                ));
            }
        }
    }
    constraint.not_valid = not_valid;
    validate_constraint_columns(
        source,
        index,
        std::slice::from_ref(&constraint),
        &existing.columns,
        "ALTER TABLE",
    )?;
    if let ConstraintKind::Check { expr } = &constraint.kind {
        let names: Vec<String> = existing.columns.iter().map(|c| c.name.clone()).collect();
        validate_check_expr(source, index, expr, &names, "ALTER TABLE")?;
    }
    let mut combined = existing.constraints.clone();
    combined.push(constraint.clone());
    validate_single_primary_key(source, index, &combined, "ALTER TABLE")?;
    let table = table_mut(catalog, schema, table_name, source, index)?;
    table.constraints.push(constraint);
    Ok(())
}

/// Applies `RENAME COLUMN`: updates plain references (constraint column
/// lists, foreign-key referenced columns in other tables, index elements)
/// and rejects the rename when an expression in the same table references
/// the column (expression rewriting is a later milestone; rejecting is
/// the honest option — see `docs/parser.md`).
fn apply_rename_column(
    catalog: &mut Catalog,
    source: &str,
    index: usize,
    schema: &str,
    table_name: &str,
    from: &String,
    to: &String,
) -> Result<()> {
    let table = catalog
        .table(schema, table_name)
        .ok_or_else(|| Error::internal(format!("table {schema}.{table_name} vanished")))?
        .clone();
    if !table.columns.iter().any(|c| c.name == *from) {
        return Err(Error::invalid(
            source,
            index,
            format!("column {from:?} of relation {table_name:?} does not exist"),
            "ALTER TABLE",
        ));
    }
    if table.columns.iter().any(|c| c.name == *to) {
        return Err(Error::invalid(
            source,
            index,
            format!("column {to:?} of relation {table_name:?} already exists"),
            "ALTER TABLE",
        ));
    }

    for column in &table.columns {
        if let Some(generated) = &column.generated
            && expr_references(&generated.expr, from.as_str())
        {
            return Err(Error::unsupported(
                source,
                index,
                format!(
                    "renaming column {from:?} would require rewriting the generation \
                         expression of column {:?}; expression rewriting is not modelled in \
                         milestone 1 (see docs/parser.md)",
                    column.name
                ),
                "ALTER TABLE",
            ));
        }
    }
    for constraint in &table.constraints {
        if let ConstraintKind::Check { expr } = &constraint.kind
            && expr_references(expr, from)
        {
            return Err(Error::unsupported(
                source,
                index,
                format!(
                    "renaming column {from:?} would require rewriting constraint \
                         {:?}'s expression; expression rewriting is not modelled in milestone 1 \
                         (see docs/parser.md)",
                    constraint.name
                ),
                "ALTER TABLE",
            ));
        }
    }
    let indexes: Vec<Index> = catalog
        .indexes
        .values()
        .filter(|i| i.schema == schema && i.table == table_name)
        .cloned()
        .collect();
    for idx in &indexes {
        for elem in &idx.columns {
            if let IndexElemKind::Expression(expr) = &elem.kind
                && expr_references(expr, from)
            {
                return Err(Error::unsupported(
                    source,
                    index,
                    format!(
                        "renaming column {from:?} would require rewriting the \
                             expression of index {:?}; expression rewriting is not modelled \
                             in milestone 1 (see docs/parser.md)",
                        idx.name
                    ),
                    "ALTER TABLE",
                ));
            }
        }
        if let Some(pred) = &idx.predicate
            && expr_references(pred, from)
        {
            return Err(Error::unsupported(
                source,
                index,
                format!(
                    "renaming column {from:?} would require rewriting the predicate of \
                         index {:?}; expression rewriting is not modelled in milestone 1 \
                         (see docs/parser.md)",
                    idx.name
                ),
                "ALTER TABLE",
            ));
        }
    }

    // Plain references follow the rename (PostgreSQL stores attnums, so
    // constraint/index references track the column automatically).
    let table_mut = table_mut(catalog, schema, table_name, source, index)?;
    for column in &mut table_mut.columns {
        if column.name == *from {
            column.name.clone_from(to);
        }
    }
    for constraint in &mut table_mut.constraints {
        if let Some(columns) = constraint_columns_mut(constraint) {
            for column in columns {
                if column == from {
                    column.clone_from(to);
                }
            }
        }
    }
    for idx in catalog
        .indexes
        .values_mut()
        .filter(|i| i.schema == schema && i.table == table_name)
    {
        for elem in &mut idx.columns {
            if let IndexElemKind::Column(c) = &mut elem.kind
                && c == from
            {
                c.clone_from(to);
            }
        }
        for inc in &mut idx.include {
            if inc == from {
                inc.clone_from(to);
            }
        }
    }
    for other in catalog
        .tables
        .values_mut()
        .filter(|t| t.schema == schema && t.name != table_name)
    {
        for constraint in &mut other.constraints {
            if let ConstraintKind::ForeignKey {
                ref_table,
                ref_columns,
                ..
            } = &mut constraint.kind
                && ref_table == table_name
            {
                for rc in ref_columns.iter_mut() {
                    if rc == from {
                        rc.clone_from(to);
                    }
                }
            }
        }
    }
    Ok(())
}

/// True when the normalised expression references identifier `name`.
/// Normalised expressions always quote identifiers (see `sql/expr.rs`),
/// so the check walks the parsed expression's identifier nodes.
fn expr_references(normalised: &str, name: &str) -> bool {
    match crate::sql::expr::parse_expr_text(normalised) {
        Ok(parsed) => expr_mentions_ident(&parsed, name),
        // Unparsable expression: conservatively block the rename.
        Err(_) => true,
    }
}

fn expr_mentions_ident(expr: &sqlparser::ast::Expr, name: &str) -> bool {
    use sqlparser::ast::{Expr, FunctionArguments};
    match expr {
        Expr::Identifier(ident) => ident.value == name,
        Expr::CompoundIdentifier(parts) => parts.last().is_some_and(|p| p.value == name),
        Expr::Value(_) => false,
        Expr::UnaryOp { expr, .. } => expr_mentions_ident(expr, name),
        Expr::BinaryOp { left, right, .. } => {
            expr_mentions_ident(left, name) || expr_mentions_ident(right, name)
        }
        Expr::Nested(inner) => expr_mentions_ident(inner, name),
        Expr::Cast { expr, .. } => expr_mentions_ident(expr, name),
        Expr::Function(f) => match &f.args {
            FunctionArguments::List(list) => list.args.iter().any(|arg| match arg {
                sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(e)) => {
                    expr_mentions_ident(e, name)
                }
                _ => false,
            }),
            _ => false,
        },
        Expr::IsNull(inner) | Expr::IsNotNull(inner) => expr_mentions_ident(inner, name),
        Expr::Between {
            expr, low, high, ..
        } => {
            expr_mentions_ident(expr, name)
                || expr_mentions_ident(low, name)
                || expr_mentions_ident(high, name)
        }
        Expr::InList { expr, list, .. } => {
            expr_mentions_ident(expr, name) || list.iter().any(|e| expr_mentions_ident(e, name))
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            operand
                .as_ref()
                .is_some_and(|e| expr_mentions_ident(e, name))
                || conditions.iter().any(|w| {
                    expr_mentions_ident(&w.condition, name) || expr_mentions_ident(&w.result, name)
                })
                || else_result
                    .as_ref()
                    .is_some_and(|e| expr_mentions_ident(e, name))
        }
        // Unsupported expression forms: conservatively block the rename.
        _ => true,
    }
}

fn apply_create_index(
    catalog: &mut Catalog,
    source: &str,
    index: usize,
    raw: &RawIndex,
) -> Result<()> {
    let (table_schema, table_name) = resolve_name(&raw.table);
    catalog.ensure_schema(&table_schema);
    let table = catalog
        .table(&table_schema, &table_name)
        .ok_or_else(|| {
            Error::invalid(
                source,
                index,
                format!(
                    "relation {:?} does not exist (CREATE INDEX requires an existing table)",
                    format!("{table_schema}.{table_name}")
                ),
                "CREATE INDEX",
            )
        })?
        .clone();
    let column_names: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();

    for elem in &raw.columns {
        if let IndexElemKind::Column(c) = &elem.kind
            && !column_names.contains(c)
        {
            return Err(Error::invalid(
                source,
                index,
                format!("column {c:?} does not exist"),
                "CREATE INDEX",
            ));
        }
    }
    for inc in &raw.include {
        if !column_names.contains(inc) {
            return Err(Error::invalid(
                source,
                index,
                format!("column {inc:?} does not exist"),
                "CREATE INDEX",
            ));
        }
    }

    let (name, schema) = if let Some(parts) = &raw.name {
        let (mut s, n) = resolve_name(parts);
        // "No schema name can be included here; the index is always
        // created in the same schema as its parent table" (PostgreSQL
        // docs, CREATE INDEX, `name`). An unqualified index name therefore
        // resolves to the table's schema, not to `search_path`/`public`.
        if parts.len() == 1 {
            s.clone_from(&table_schema);
        }
        if parts.len() == 2 && s != table_schema {
            return Err(Error::invalid(
                source,
                index,
                format!("index {n:?} must be created in the table's schema {table_schema:?}"),
                "CREATE INDEX",
            ));
        }
        (n, s)
    } else {
        let additions = index_key_set_from(&raw.columns);
        let relations = occupied_relation_names(catalog, &table_schema);
        let relations_taken = |c: &str| relations.iter().any(|r| r == c);
        let no_constraints: fn(&str) -> bool = |_| false;
        let auto = naming::auto_index_name(
            &table_name,
            &additions,
            false,
            false,
            &relations_taken,
            &no_constraints,
            &[],
        );
        (auto, table_schema.clone())
    };

    if occupied_relation_names(catalog, &schema).contains(&name) {
        if raw.if_not_exists {
            return Ok(());
        }
        return Err(Error::invalid(
            source,
            index,
            format!("relation {name:?} already exists"),
            "CREATE INDEX",
        ));
    }

    catalog.indexes.insert(
        (schema.clone(), name.clone()),
        Index {
            schema,
            name: name.clone(),
            table_schema,
            table: table_name,
            unique: raw.unique,
            concurrent: raw.concurrent,
            method: raw.method.clone(),
            columns: raw.columns.clone(),
            include: raw.include.clone(),
            nulls_distinct: raw.nulls_distinct,
            with: raw.with.clone(),
            tablespace: raw.tablespace.clone(),
            predicate: raw.predicate.clone(),
        },
    );
    Ok(())
}

/// The key column names of a set of index elements (plain columns and
/// expression text).
fn index_key_set_from(elems: &[crate::model::IndexElem]) -> Vec<String> {
    elems
        .iter()
        .map(|e| match &e.kind {
            IndexElemKind::Column(c) => c.clone(),
            IndexElemKind::Expression(expr) => expr.clone(),
        })
        .collect()
}

fn apply_drop_index(
    catalog: &mut Catalog,
    source: &str,
    index: usize,
    names: &[Vec<String>],
    if_exists: bool,
) -> Result<()> {
    for parts in names {
        let (schema, name) = resolve_name(parts);
        // A standalone DROP INDEX cannot remove a constraint's backing
        // index (PostgreSQL: "cannot drop index t_pkey because constraint
        // t_pkey on table t requires it").
        let backing = catalog.tables.values().find(|t| {
            t.schema == schema
                && t.constraints.iter().any(|c| {
                    c.name == name
                        && matches!(
                            c.kind,
                            ConstraintKind::PrimaryKey { .. } | ConstraintKind::Unique { .. }
                        )
                })
        });
        if let Some(table) = backing {
            return Err(Error::invalid(
                source,
                index,
                format!(
                    "cannot drop index {name:?} because constraint {name:?} on table {:?} \
                     requires it; use ALTER TABLE ... DROP CONSTRAINT instead",
                    table.name
                ),
                "DROP INDEX",
            ));
        }
        if catalog
            .indexes
            .remove(&(schema.clone(), name.clone()))
            .is_none()
            && !if_exists
        {
            return Err(Error::invalid(
                source,
                index,
                format!("index {schema}.{name:?} does not exist"),
                "DROP INDEX",
            ));
        }
    }
    Ok(())
}

fn apply_alter_index_rename(
    catalog: &mut Catalog,
    source: &str,
    index: usize,
    name_parts: &[String],
    new_name: &str,
) -> Result<()> {
    let (schema, old) = resolve_name(name_parts);
    let Some(mut idx) = catalog.indexes.remove(&(schema.clone(), old.clone())) else {
        return Err(Error::invalid(
            source,
            index,
            format!("index {schema}.{old:?} does not exist"),
            "ALTER INDEX",
        ));
    };
    let new_name = naming::truncate_ident(new_name);
    let relations = occupied_relation_names(catalog, &schema);
    if relations.contains(&new_name) && new_name != old {
        return Err(Error::invalid(
            source,
            index,
            format!("relation {new_name:?} already exists"),
            "ALTER INDEX",
        ));
    }
    idx.name.clone_from(&new_name);
    catalog.indexes.insert((schema, new_name), idx);
    Ok(())
}

fn table_mut<'a>(
    catalog: &'a mut Catalog,
    schema: &str,
    table: &str,
    _source: &str,
    _index: usize,
) -> Result<&'a mut Table> {
    catalog
        .table_mut(schema, table)
        .ok_or_else(|| Error::internal(format!("table {schema}.{table} vanished mid-alter")))
}

fn column_mut<'a>(
    table: &'a mut Table,
    column: &str,
    source: &str,
    index: usize,
    action: &str,
) -> Result<&'a mut Column> {
    table
        .columns
        .iter_mut()
        .find(|c| c.name == column)
        .ok_or_else(|| {
            Error::invalid(
                source,
                index,
                format!("column {column:?} does not exist (during {action})"),
                "ALTER TABLE",
            )
        })
}
