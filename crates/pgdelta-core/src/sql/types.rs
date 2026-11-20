//! Parsers for user-defined types and domains.
//!
//! Grammar sources (PostgreSQL 18 stable docs, fetched with web tools):
//! * `CREATE TYPE` — <https://www.postgresql.org/docs/18/sql-createtype.html>
//! * `ALTER TYPE` — <https://www.postgresql.org/docs/18/sql-altertype.html>
//! * `CREATE DOMAIN` — <https://www.postgresql.org/docs/18/sql-createdomain.html>
//!
//! Enum values are sorted labels whose order is significant: `ADD VALUE
//! [BEFORE|AFTER neighbour]` inserts at a chosen position while `RENAME
//! VALUE` keeps the position (ALTER TYPE docs). Domains carry a base type,
//! an optional default and named `NOT NULL`/`CHECK` constraints (CREATE
//! DOMAIN docs).

use crate::error::{Error, Result};
use crate::model::{
    CompositeAttribute, DomainConstraint, DomainConstraintKind, PgType, PgTypeKind,
};

use super::tokens::Cursor;
use super::{ParsedStatement, expr, take_type, tokens};

/// An `ALTER TYPE`/`ALTER DOMAIN` action.
#[derive(Debug, Clone)]
pub(crate) enum TypeAction {
    /// `RENAME TO`.
    RenameType { to: String },
    /// `SET SCHEMA`.
    SetSchema { schema: String },
    /// `ADD VALUE` (enum). `position` is `(before, neighbour)`.
    AddEnumValue {
        value: String,
        position: Option<(bool, String)>,
        if_not_exists: bool,
    },
    /// `RENAME VALUE` (enum); the position is unchanged.
    RenameEnumValue { from: String, to: String },
    /// `ADD ATTRIBUTE` (composite).
    AddAttribute { attribute: CompositeAttribute },
    /// `DROP ATTRIBUTE` (composite).
    DropAttribute { name: String, if_exists: bool },
    /// `ALTER ATTRIBUTE ... SET DATA TYPE` (composite).
    AlterAttribute {
        name: String,
        data_type: String,
        collation: Option<String>,
    },
    /// `RENAME ATTRIBUTE` (composite).
    RenameAttribute { from: String, to: String },
    /// `SET NOT NULL` (domain).
    SetNotNull,
    /// `DROP NOT NULL` (domain).
    DropNotNull,
    /// `ADD CONSTRAINT ... CHECK` (domain).
    AddDomainConstraint { constraint: DomainConstraint },
    /// `DROP CONSTRAINT` (domain).
    DropDomainConstraint { name: String, if_exists: bool },
    /// `RENAME CONSTRAINT` (domain).
    RenameDomainConstraint { from: String, to: String },
    /// `VALIDATE CONSTRAINT` (domain): 9.5+ no-op on already-valid CHECKs.
    ValidateDomainConstraint { name: String },
    /// `SET DEFAULT` (domain).
    SetDefault { expr: String },
    /// `DROP DEFAULT` (domain).
    DropDefault,
}

fn fail(source: &str, index: usize, message: &str, text: &str) -> Result<ParsedStatement> {
    Err(Error::parse(source, index, message, text))
}

/// Parses `CREATE TYPE name ...` (the cursor starts at `TYPE`).
pub(crate) fn parse_create_type(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedStatement> {
    if !cursor.eat_word("TYPE") {
        return fail(source, index, "expected TYPE after CREATE", text);
    }
    let if_not_exists = cursor.eat_words2("IF", "NOT") && cursor.eat_word("EXISTS");
    let Some(mut name) = cursor.parse_object_name() else {
        return fail(source, index, "expected a type name", text);
    };
    super::truncate_parts(&mut name);
    let (schema, type_name) = super::resolve_name(&name);

    let kind = if cursor.eat_words2("AS", "ENUM") {
        PgTypeKind::Enum {
            values: parse_labels(source, index, cursor, text)?,
        }
    } else if cursor.eat_words2("AS", "RANGE") {
        parse_range(source, index, cursor, text)?
    } else if cursor.eat_word("AS") && cursor.at_lparen() {
        PgTypeKind::Composite {
            attributes: parse_attributes(source, index, cursor, text)?,
        }
    } else if cursor.at_lparen() {
        return Err(Error::unsupported(
            source,
            index,
            "base and shell types require C support functions and are not part of pgdelta's \
             catalog model (use an enum, composite, domain or range type instead)",
            text,
        ));
    } else {
        return Err(Error::unsupported(
            source,
            index,
            "CREATE TYPE without a definition creates a shell type; pgdelta models only \
             complete enum, composite, range and domain types",
            text,
        ));
    };
    if !cursor.is_empty() {
        return Err(Error::unsupported(
            source,
            index,
            "unsupported trailing CREATE TYPE options",
            text,
        ));
    }
    if if_not_exists {
        // Harmless in a declarative source; the apply layer uses it only to
        // tolerate a pre-existing ambient type.
    }
    Ok(ParsedStatement::CreateType {
        pg_type: PgType {
            schema,
            name: type_name,
            kind,
        },
    })
}

/// Parses `CREATE DOMAIN name [AS] type [COLLATE c] [DEFAULT e] [constraints]`.
pub(crate) fn parse_create_domain(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedStatement> {
    if !cursor.eat_word("DOMAIN") {
        return fail(source, index, "expected DOMAIN after CREATE", text);
    }
    let Some(mut name) = cursor.parse_object_name() else {
        return fail(source, index, "expected a domain name", text);
    };
    super::truncate_parts(&mut name);
    let (schema, domain_name) = super::resolve_name(&name);
    cursor.eat_word("AS");
    let base_type = take_type(
        source,
        index,
        cursor,
        &["COLLATE", "DEFAULT", "CONSTRAINT", "NOT", "NULL", "CHECK"],
        text,
    )?;
    let collation = if cursor.eat_word("COLLATE") {
        cursor.parse_object_name().map(|p| p.join("."))
    } else {
        None
    };
    let default = if cursor.eat_word("DEFAULT") {
        let expr_text = cursor.capture_until_words(&["CONSTRAINT", "NOT", "NULL", "CHECK"]);
        if expr_text.trim().is_empty() {
            return fail(source, index, "expected a DEFAULT expression", text);
        }
        Some(expr::normalize_expr_text(&expr_text).map_err(|e| {
            Error::parse(
                source,
                index,
                format!("invalid DEFAULT expression: {e}"),
                text,
            )
        })?)
    } else {
        None
    };
    let constraints = parse_domain_constraints(source, index, cursor, &domain_name, text)?;
    if !cursor.is_empty() {
        return Err(Error::unsupported(
            source,
            index,
            "unsupported trailing CREATE DOMAIN options",
            text,
        ));
    }
    Ok(ParsedStatement::CreateType {
        pg_type: PgType {
            schema,
            name: domain_name,
            kind: PgTypeKind::Domain {
                base_type,
                collation,
                default,
                constraints,
            },
        },
    })
}

/// Parses `DROP TYPE`/`DROP DOMAIN [IF EXISTS] name [, ...]`.
pub(crate) fn parse_drop_type(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedStatement> {
    let if_exists = cursor.eat_words2("IF", "EXISTS");
    let mut names = Vec::new();
    loop {
        let Some(mut parts) = cursor.parse_object_name() else {
            return fail(source, index, "expected a type name", text);
        };
        super::truncate_parts(&mut parts);
        names.push(parts);
        if !cursor.eat_comma() {
            break;
        }
    }
    if cursor.eat_word("CASCADE") {
        return Err(Error::unsupported(
            source,
            index,
            "DROP TYPE ... CASCADE drops dependent objects implicitly, which pgdelta will never \
             do silently; drop the dependents explicitly",
            text,
        ));
    }
    cursor.eat_word("RESTRICT");
    Ok(ParsedStatement::DropType { names, if_exists })
}

/// Parses `ALTER TYPE`/`ALTER DOMAIN name action [, ...]`.
pub(crate) fn parse_alter_type(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    domain: bool,
    text: &str,
) -> Result<ParsedStatement> {
    let Some(mut name) = cursor.parse_object_name() else {
        return fail(source, index, "expected a type name", text);
    };
    super::truncate_parts(&mut name);
    let mut actions = Vec::new();
    loop {
        actions.push(parse_type_action(source, index, cursor, domain, text)?);
        if cursor.eat_comma() {
            continue;
        }
        break;
    }
    if !cursor.is_empty() {
        return Err(Error::unsupported(
            source,
            index,
            "unsupported trailing ALTER TYPE option (OWNER TO is not modelled)",
            text,
        ));
    }
    Ok(ParsedStatement::AlterType {
        name,
        actions,
        domain,
    })
}

fn parse_type_action(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    domain: bool,
    text: &str,
) -> Result<TypeAction> {
    if cursor.eat_words2("RENAME", "TO") {
        let to = cursor.parse_ident().ok_or_else(|| {
            Error::parse(
                source,
                index,
                "expected a new type name after RENAME TO",
                text,
            )
        })?;
        return Ok(TypeAction::RenameType {
            to: crate::naming::truncate_ident(&to),
        });
    }
    if cursor.eat_words2("SET", "SCHEMA") {
        let schema = cursor.parse_ident().ok_or_else(|| {
            Error::parse(
                source,
                index,
                "expected a schema name after SET SCHEMA",
                text,
            )
        })?;
        return Ok(TypeAction::SetSchema {
            schema: crate::naming::truncate_ident(&schema),
        });
    }
    if !domain && cursor.eat_words2("ADD", "VALUE") {
        let if_not_exists = cursor.eat_words2("IF", "NOT") && cursor.eat_word("EXISTS");
        let Some(value) = tokens::eat_string_literal(cursor) else {
            return Err(Error::parse(
                source,
                index,
                "expected an enum label after ADD VALUE",
                text,
            ));
        };
        let position = if cursor.eat_word("BEFORE") {
            let neighbour = tokens::eat_string_literal(cursor).ok_or_else(|| {
                Error::parse(
                    source,
                    index,
                    "expected a neighbour label after BEFORE",
                    text,
                )
            })?;
            Some((true, neighbour))
        } else if cursor.eat_word("AFTER") {
            let neighbour = tokens::eat_string_literal(cursor).ok_or_else(|| {
                Error::parse(
                    source,
                    index,
                    "expected a neighbour label after AFTER",
                    text,
                )
            })?;
            Some((false, neighbour))
        } else {
            None
        };
        return Ok(TypeAction::AddEnumValue {
            value,
            position,
            if_not_exists,
        });
    }
    if !domain && cursor.eat_words2("RENAME", "VALUE") {
        let Some(from) = tokens::eat_string_literal(cursor) else {
            return Err(Error::parse(
                source,
                index,
                "expected an enum label after RENAME VALUE",
                text,
            ));
        };
        if !cursor.eat_word("TO") {
            return Err(Error::parse(
                source,
                index,
                "expected TO after RENAME VALUE",
                text,
            ));
        }
        let Some(to) = tokens::eat_string_literal(cursor) else {
            return Err(Error::parse(
                source,
                index,
                "expected a new enum label after TO",
                text,
            ));
        };
        return Ok(TypeAction::RenameEnumValue { from, to });
    }
    if !domain && cursor.eat_words2("RENAME", "ATTRIBUTE") {
        let Some(from) = cursor.parse_ident() else {
            return Err(Error::parse(
                source,
                index,
                "expected an attribute name",
                text,
            ));
        };
        if !cursor.eat_word("TO") {
            return Err(Error::parse(
                source,
                index,
                "expected TO after RENAME ATTRIBUTE",
                text,
            ));
        }
        let to = cursor
            .parse_ident()
            .ok_or_else(|| Error::parse(source, index, "expected a new attribute name", text))?;
        eat_cascade_restrict(cursor);
        return Ok(TypeAction::RenameAttribute {
            from,
            to: crate::naming::truncate_ident(&to),
        });
    }
    if !domain && cursor.eat_words2("ADD", "ATTRIBUTE") {
        let Some(name) = cursor.parse_ident() else {
            return Err(Error::parse(
                source,
                index,
                "expected an attribute name",
                text,
            ));
        };
        let data_type = take_type(
            source,
            index,
            cursor,
            &["COLLATE", "CASCADE", "RESTRICT"],
            text,
        )?;
        let collation = if cursor.eat_word("COLLATE") {
            cursor.parse_object_name().map(|p| p.join("."))
        } else {
            None
        };
        eat_cascade_restrict(cursor);
        return Ok(TypeAction::AddAttribute {
            attribute: CompositeAttribute {
                name: crate::naming::truncate_ident(&name),
                data_type,
                collation,
            },
        });
    }
    if !domain && cursor.eat_words2("DROP", "ATTRIBUTE") {
        let if_exists = cursor.eat_words2("IF", "EXISTS");
        let Some(name) = cursor.parse_ident() else {
            return Err(Error::parse(
                source,
                index,
                "expected an attribute name",
                text,
            ));
        };
        eat_cascade_restrict(cursor);
        return Ok(TypeAction::DropAttribute { name, if_exists });
    }
    if !domain && cursor.eat_words2("ALTER", "ATTRIBUTE") {
        let Some(name) = cursor.parse_ident() else {
            return Err(Error::parse(
                source,
                index,
                "expected an attribute name",
                text,
            ));
        };
        cursor.eat_words2("SET", "DATA");
        if !cursor.eat_word("TYPE") {
            return Err(Error::parse(
                source,
                index,
                "expected TYPE after ALTER ATTRIBUTE",
                text,
            ));
        }
        let data_type = take_type(
            source,
            index,
            cursor,
            &["COLLATE", "CASCADE", "RESTRICT"],
            text,
        )?;
        let collation = if cursor.eat_word("COLLATE") {
            cursor.parse_object_name().map(|p| p.join("."))
        } else {
            None
        };
        eat_cascade_restrict(cursor);
        return Ok(TypeAction::AlterAttribute {
            name,
            data_type,
            collation,
        });
    }
    if domain {
        parse_domain_action(source, index, cursor, text)
    } else {
        Err(Error::unsupported(
            source,
            index,
            "this ALTER TYPE action is not part of pgdelta's catalog model",
            text,
        ))
    }
}

fn parse_domain_action(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<TypeAction> {
    if cursor.eat_words2("SET", "NOT") && cursor.eat_word("NULL") {
        return Ok(TypeAction::SetNotNull);
    }
    if cursor.eat_words2("DROP", "NOT") && cursor.eat_word("NULL") {
        return Ok(TypeAction::DropNotNull);
    }
    if cursor.eat_words2("SET", "DEFAULT") {
        let expr_text = cursor.capture_until_words(&[]);
        if expr_text.trim().is_empty() {
            return Err(Error::parse(
                source,
                index,
                "expected a DEFAULT expression",
                text,
            ));
        }
        return Ok(TypeAction::SetDefault {
            expr: expr::normalize_expr_text(&expr_text).map_err(|e| {
                Error::parse(
                    source,
                    index,
                    format!("invalid DEFAULT expression: {e}"),
                    text,
                )
            })?,
        });
    }
    if cursor.eat_words2("DROP", "DEFAULT") {
        return Ok(TypeAction::DropDefault);
    }
    if cursor.eat_words2("RENAME", "CONSTRAINT") {
        let from = cursor
            .parse_ident()
            .ok_or_else(|| Error::parse(source, index, "expected a constraint name", text))?;
        if !cursor.eat_word("TO") {
            return Err(Error::parse(
                source,
                index,
                "expected TO after RENAME CONSTRAINT",
                text,
            ));
        }
        let to = cursor
            .parse_ident()
            .ok_or_else(|| Error::parse(source, index, "expected a new constraint name", text))?;
        return Ok(TypeAction::RenameDomainConstraint { from, to });
    }
    if cursor.eat_words2("DROP", "CONSTRAINT") {
        let if_exists = cursor.eat_words2("IF", "EXISTS");
        let name = cursor
            .parse_ident()
            .ok_or_else(|| Error::parse(source, index, "expected a constraint name", text))?;
        return Ok(TypeAction::DropDomainConstraint { name, if_exists });
    }
    if cursor.eat_words2("VALIDATE", "CONSTRAINT") {
        let name = cursor
            .parse_ident()
            .ok_or_else(|| Error::parse(source, index, "expected a constraint name", text))?;
        return Ok(TypeAction::ValidateDomainConstraint { name });
    }
    if cursor.eat_words2("ADD", "CONSTRAINT") {
        let name = cursor
            .parse_ident()
            .ok_or_else(|| Error::parse(source, index, "expected a constraint name", text))?;
        let constraint = parse_domain_check_body(source, index, cursor, name, text)?;
        return Ok(TypeAction::AddDomainConstraint { constraint });
    }
    if cursor.eat_word("CHECK") {
        let constraint = parse_domain_check_body(source, index, cursor, String::new(), text)?;
        return Ok(TypeAction::AddDomainConstraint { constraint });
    }
    if cursor.eat_word("NOT") && cursor.eat_word("NULL") {
        return Ok(TypeAction::SetNotNull);
    }
    if cursor.eat_word("NULL") {
        return Ok(TypeAction::DropNotNull);
    }
    Err(Error::unsupported(
        source,
        index,
        "this ALTER DOMAIN action is not part of pgdelta's catalog model",
        text,
    ))
}

fn parse_domain_check_body(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    name: String,
    text: &str,
) -> Result<DomainConstraint> {
    if !cursor.eat_word("CHECK") {
        return Err(Error::parse(
            source,
            index,
            "expected CHECK after constraint name",
            text,
        ));
    }
    let raw = cursor.capture_paren_group().ok_or_else(|| {
        Error::parse(
            source,
            index,
            "expected a parenthesised CHECK expression",
            text,
        )
    })?;
    let expr = expr::normalize_expr_text(&raw).map_err(|e| {
        Error::parse(
            source,
            index,
            format!("invalid CHECK expression: {e}"),
            text,
        )
    })?;
    Ok(DomainConstraint {
        name,
        kind: DomainConstraintKind::Check { expr },
    })
}

fn eat_cascade_restrict(cursor: &mut Cursor) {
    if cursor.eat_word("CASCADE") || cursor.eat_word("RESTRICT") {
        // Both are accepted and ignored: pgdelta models the logical type
        // state, not the propagation policy of an individual statement.
    }
}

fn parse_labels(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<Vec<String>> {
    if !cursor.eat_lparen() {
        return Err(Error::parse(
            source,
            index,
            "expected '(' after AS ENUM",
            text,
        ));
    }
    let mut labels = Vec::new();
    loop {
        let Some(label) = tokens::eat_string_literal(cursor) else {
            return Err(Error::parse(
                source,
                index,
                "expected a quoted enum label",
                text,
            ));
        };
        // CREATE TYPE: enum labels are limited to NAMEDATALEN-1 bytes.
        labels.push(crate::naming::truncate_ident(&label));
        if cursor.eat_comma() {
            continue;
        }
        if cursor.eat_rparen() {
            break;
        }
        return Err(Error::parse(
            source,
            index,
            "expected ',' or ')' after enum label",
            text,
        ));
    }
    if labels.iter().any(String::is_empty) {
        return Err(Error::invalid(
            source,
            index,
            "enum labels cannot be empty strings",
            text,
        ));
    }
    if let Some(dup) = duplicate(&labels) {
        return Err(Error::invalid(
            source,
            index,
            format!("enum label {dup:?} is duplicated"),
            text,
        ));
    }
    Ok(labels)
}

fn duplicate(items: &[String]) -> Option<&String> {
    let mut seen = std::collections::BTreeSet::new();
    items.iter().find(|item| !seen.insert(*item))
}

fn parse_attributes(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<Vec<CompositeAttribute>> {
    if !cursor.eat_lparen() {
        return Err(Error::parse(source, index, "expected '(' after AS", text));
    }
    let mut attrs = Vec::new();
    loop {
        let Some(name) = cursor.parse_ident() else {
            if cursor.eat_rparen() {
                break;
            }
            return Err(Error::parse(
                source,
                index,
                "expected a composite attribute name",
                text,
            ));
        };
        let data_type = take_type(source, index, cursor, &["COLLATE"], text)?;
        let collation = if cursor.eat_word("COLLATE") {
            cursor.parse_object_name().map(|p| p.join("."))
        } else {
            None
        };
        attrs.push(CompositeAttribute {
            name: crate::naming::truncate_ident(&name),
            data_type,
            collation,
        });
        if cursor.eat_comma() {
            continue;
        }
        if cursor.eat_rparen() {
            break;
        }
        return Err(Error::parse(
            source,
            index,
            "expected ',' or ')' after attribute",
            text,
        ));
    }
    if let Some(dup) = duplicate(&attrs.iter().map(|a| a.name.clone()).collect::<Vec<_>>()) {
        return Err(Error::invalid(
            source,
            index,
            format!("composite attribute {dup:?} is duplicated"),
            text,
        ));
    }
    Ok(attrs)
}

fn parse_range(source: &str, index: usize, cursor: &mut Cursor, text: &str) -> Result<PgTypeKind> {
    if !cursor.eat_lparen() {
        return Err(Error::parse(
            source,
            index,
            "expected '(' after AS RANGE",
            text,
        ));
    }
    let mut subtype = None;
    let mut subtype_opclass = None;
    let mut collation = None;
    let mut canonical = None;
    let mut subtype_diff = None;
    let mut multirange_type_name = None;
    loop {
        let Some(key) = cursor.parse_ident() else {
            return Err(Error::parse(
                source,
                index,
                "expected a RANGE option name",
                text,
            ));
        };
        if !cursor.eat_equals() {
            return Err(Error::parse(
                source,
                index,
                "expected '=' after RANGE option",
                text,
            ));
        }
        match key.to_ascii_uppercase().as_str() {
            "SUBTYPE" => subtype = Some(take_type(source, index, cursor, &[], text)?),
            "SUBTYPE_OPCLASS" => {
                subtype_opclass = cursor.parse_object_name().map(|p| p.join("."));
            }
            "COLLATION" => collation = cursor.parse_object_name().map(|p| p.join(".")),
            "CANONICAL" => canonical = cursor.parse_object_name().map(|p| p.join(".")),
            "SUBTYPE_DIFF" => subtype_diff = cursor.parse_object_name().map(|p| p.join(".")),
            "MULTIRANGE_TYPE_NAME" => {
                multirange_type_name = cursor.parse_object_name().map(|p| p.join("."));
            }
            other => {
                return Err(Error::unsupported(
                    source,
                    index,
                    format!("unknown RANGE option {other}"),
                    text,
                ));
            }
        }
        if cursor.eat_comma() {
            continue;
        }
        if cursor.eat_rparen() {
            break;
        }
        return Err(Error::parse(
            source,
            index,
            "expected ',' or ')' after RANGE option",
            text,
        ));
    }
    let Some(subtype) = subtype else {
        return Err(Error::invalid(
            source,
            index,
            "RANGE types require SUBTYPE (CREATE TYPE, Range Types)",
            text,
        ));
    };
    Ok(PgTypeKind::Range {
        subtype,
        subtype_opclass,
        collation,
        canonical,
        subtype_diff,
        multirange_type_name,
    })
}

/// Parses the domain constraints of a `CREATE DOMAIN`, auto-naming unnamed
/// `CHECK` constraints like PostgreSQL (`ChooseConstraintName`, cited in
/// `naming.rs`).
fn parse_domain_constraints(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    domain: &str,
    text: &str,
) -> Result<Vec<DomainConstraint>> {
    let mut out = Vec::new();
    loop {
        let mut name =
            if cursor.eat_word("CONSTRAINT") {
                Some(cursor.parse_ident().ok_or_else(|| {
                    Error::parse(source, index, "expected a constraint name", text)
                })?)
            } else {
                None
            };
        if cursor.eat_words2("NOT", "NULL") {
            let name = name.take().unwrap_or_else(|| {
                crate::naming::auto_constraint_name(domain, None, "not_null", &|_| false, &[])
            });
            out.push(DomainConstraint {
                name,
                kind: DomainConstraintKind::NotNull,
            });
        } else if cursor.eat_word("NULL") {
            // A `NULL` constraint is the default and imposes no restriction.
        } else if cursor.eat_word("CHECK") {
            let raw = cursor.capture_paren_group().ok_or_else(|| {
                Error::parse(
                    source,
                    index,
                    "expected a parenthesised CHECK expression",
                    text,
                )
            })?;
            let expr = expr::normalize_expr_text(&raw).map_err(|e| {
                Error::parse(
                    source,
                    index,
                    format!("invalid CHECK expression: {e}"),
                    text,
                )
            })?;
            let name = name.take().unwrap_or_else(|| {
                crate::naming::auto_constraint_name(domain, None, "check", &|_| false, &[])
            });
            out.push(DomainConstraint {
                name,
                kind: DomainConstraintKind::Check { expr },
            });
        } else if cursor.is_empty() {
            break;
        } else {
            return Err(Error::parse(
                source,
                index,
                "expected a domain constraint (NOT NULL, NULL or CHECK)",
                text,
            ));
        }
    }
    if let Some(dup) = duplicate(&out.iter().map(|c| c.name.clone()).collect::<Vec<_>>()) {
        return Err(Error::invalid(
            source,
            index,
            format!("domain constraint {dup:?} is duplicated"),
            text,
        ));
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}
