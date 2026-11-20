//! Statement kinds handled through sqlparser's own AST, plus the
//! session-statement policy.
//!
//! These grammars are simple enough that sqlparser-rs parses them
//! completely on all PostgreSQL versions pgdelta targets (verified by the
//! parser support tests); pgdelta maps the AST to
//! [`ParsedStatement`](super::ParsedStatement) entries. Anything that is
//! not a catalog statement and not an accepted-and-ignored session
//! statement is rejected with an explicit message.

use sqlparser::ast::Statement;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

use super::tokens::{Cursor, StatementTokens};
use super::ParsedStatement;
use crate::error::{Error, Result};

/// Parses `CREATE SCHEMA [IF NOT EXISTS] name`.
pub(crate) fn parse_create_schema(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedStatement> {
    if !cursor.eat_word("SCHEMA") {
        return Err(Error::parse(source, index, "expected SCHEMA after CREATE", text));
    }
    let if_not_exists = cursor.eat_words2("IF", "NOT") && cursor.eat_word("EXISTS");
    let Some(mut name) = cursor.parse_object_name() else {
        return Err(Error::parse(source, index, "expected a schema name", text));
    };
    if name.len() != 1 {
        return Err(Error::invalid(
            source,
            index,
            "schema names are not qualified",
            text,
        ));
    }
    if !cursor.is_empty() {
        return Err(Error::unsupported(
            source,
            index,
            "CREATE SCHEMA ... AUTHORIZATION and schema elements are not modelled in \
             milestone 1 (ownership arrives with the grants milestone)",
            text,
        ));
    }
    name[0] = crate::naming::truncate_ident(&name[0]);
    Ok(ParsedStatement::CreateSchema {
        name,
        if_not_exists,
    })
}

/// Parses `CREATE EXTENSION [IF NOT EXISTS] name [WITH] [SCHEMA s]`.
///
/// `VERSION` and `CASCADE` are accepted and ignored: the model records
/// the extension's presence and schema, and version migration is managed
/// by `ALTER EXTENSION UPDATE` (out of milestone-1 scope, documented in
/// `docs/parser.md`).
pub(crate) fn parse_create_extension(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedStatement> {
    if !cursor.eat_word("EXTENSION") {
        return Err(Error::parse(source, index, "expected EXTENSION after CREATE", text));
    }
    let if_not_exists = cursor.eat_words2("IF", "NOT") && cursor.eat_word("EXISTS");
    let Some(mut name) = cursor.parse_object_name() else {
        return Err(Error::parse(source, index, "expected an extension name", text));
    };
    if name.len() != 1 {
        return Err(Error::invalid(source, index, "extension names are not qualified", text));
    }
    truncate_parts(&mut name);
    let mut schema = None;
    cursor.eat_word("WITH");
    while !cursor.is_empty() {
        if cursor.eat_word("SCHEMA") {
            let Some(parts) = cursor.parse_object_name() else {
                return Err(Error::parse(source, index, "expected a schema name after SCHEMA", text));
            };
            schema = Some(parts.join("."));
        } else if cursor.eat_word("VERSION") {
            // Accepted and ignored (version state is ALTER EXTENSION
            // UPDATE territory).
            if cursor.next().is_none() {
                return Err(Error::parse(source, index, "expected a version after VERSION", text));
            }
        } else if cursor.eat_word("CASCADE") {
            // Accepted and ignored: CASCADE pulls in dependency
            // extensions, which PostgreSQL resolves at execution time;
            // the source should still declare them explicitly.
        } else {
            return Err(Error::parse(
                source,
                index,
                "expected SCHEMA, VERSION or CASCADE in CREATE EXTENSION options",
                text,
            ));
        }
    }
    Ok(ParsedStatement::CreateExtension {
        name: name[0].clone(),
        if_not_exists,
        schema,
    })
}

/// Parses `DROP SCHEMA [IF EXISTS] name [RESTRICT|CASCADE]`.
pub(crate) fn parse_drop_schema(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedStatement> {
    let if_exists = cursor.eat_words2("IF", "EXISTS");
    let Some(mut name) = cursor.parse_object_name() else {
        return Err(Error::parse(source, index, "expected a schema name", text));
    };
    if name.len() != 1 {
        return Err(Error::invalid(source, index, "schema names are not qualified", text));
    }
    truncate_parts(&mut name);
    let cascade = if cursor.eat_word("CASCADE") {
        true
    } else {
        cursor.eat_word("RESTRICT");
        false
    };
    if cascade {
        return Err(Error::unsupported(
            source,
            index,
            "DROP SCHEMA ... CASCADE drops contained objects implicitly, which pgdelta will \
             never do silently; drop the contained objects explicitly",
            text,
        ));
    }
    Ok(ParsedStatement::DropSchema {
        name,
        if_exists,
    })
}

/// Parses `DROP EXTENSION [IF EXISTS] name [RESTRICT|CASCADE]`.
pub(crate) fn parse_drop_extension(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedStatement> {
    let if_exists = cursor.eat_words2("IF", "EXISTS");
    let Some(mut name) = cursor.parse_object_name() else {
        return Err(Error::parse(source, index, "expected an extension name", text));
    };
    if name.len() != 1 {
        return Err(Error::invalid(source, index, "extension names are not qualified", text));
    }
    truncate_parts(&mut name);
    let cascade = if cursor.eat_word("CASCADE") {
        true
    } else {
        cursor.eat_word("RESTRICT");
        false
    };
    if cascade {
        return Err(Error::unsupported(
            source,
            index,
            "DROP EXTENSION ... CASCADE drops dependent objects implicitly, which pgdelta \
             will never do silently; drop the dependents explicitly",
            text,
        ));
    }
    Ok(ParsedStatement::DropExtension {
        name: name[0].clone(),
        if_exists,
    })
}

/// Parses `DROP TABLE [IF EXISTS] name [, ...] [RESTRICT|CASCADE]`.
pub(crate) fn parse_drop_table(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedStatement> {
    let if_exists = cursor.eat_words2("IF", "EXISTS");
    let mut names = Vec::new();
    loop {
        let Some(mut parts) = cursor.parse_object_name() else {
            return Err(Error::parse(source, index, "expected a table name", text));
        };
        truncate_parts(&mut parts);
        names.push(parts);
        if !cursor.eat_comma() {
            break;
        }
    }
    let cascade = if cursor.eat_word("CASCADE") {
        true
    } else {
        cursor.eat_word("RESTRICT");
        false
    };
    if cascade {
        return Err(Error::unsupported(
            source,
            index,
            "DROP TABLE ... CASCADE drops dependent objects implicitly, which pgdelta will \
             never do silently; drop the dependents (foreign keys, views) explicitly",
            text,
        ));
    }
    Ok(ParsedStatement::DropTable {
        names,
        if_exists,
    })
}

/// Handles statements sqlparser parses well and that either configure no
/// catalog state (session statements — accepted and ignored, per the
/// policy documented in `docs/parser.md`) or are out of milestone-1 scope
/// (rejected with explicit messages).
pub(crate) fn parse_misc(
    source: &str,
    index: usize,
    stmt: &StatementTokens,
    text: &str,
) -> Result<ParsedStatement> {
    let statements = Parser::parse_sql(&PostgreSqlDialect {}, &stmt.text())
        .map_err(|e| Error::parse(source, index, e, text))?;
    let Some(parsed) = statements.into_iter().next() else {
        return Err(Error::parse(source, index, "statement did not parse", text));
    };
    match parsed {
        Statement::Set(_) => Ok(ParsedStatement::SessionOp),
        Statement::StartTransaction { .. } => Ok(ParsedStatement::SessionOp),
        Statement::Commit { .. } => Ok(ParsedStatement::SessionOp),
        Statement::LockTables { .. } => Ok(ParsedStatement::SessionOp),
        other => {
            let kind = statement_kind_name(&other);
            Err(Error::unsupported(
                source,
                index,
                format!(
                    "{kind} statements are not part of pgdelta's catalog model in milestone 1 \
                     (views, sequences, types, functions, triggers, policies, grants and \
                     comments are later milestones; see docs/parser.md)"
                ),
                text,
            ))
        }
    }
}

fn statement_kind_name(stmt: &Statement) -> &'static str {
    match stmt {
        Statement::CreateView { .. } => "CREATE VIEW",
        Statement::CreateSequence { .. } => "CREATE SEQUENCE",
        Statement::CreateType { .. } => "CREATE TYPE",
        Statement::CreateFunction { .. } => "CREATE FUNCTION",
        Statement::CreateTrigger { .. } => "CREATE TRIGGER",
        Statement::CreatePolicy { .. } => "CREATE POLICY",
        Statement::Grant { .. } => "GRANT",
        Statement::Revoke { .. } => "REVOKE",
        Statement::Comment { .. } => "COMMENT ON",
        Statement::Insert { .. } => "INSERT",
        Statement::Update { .. } => "UPDATE",
        Statement::Delete { .. } => "DELETE",
        Statement::Query(_) => "SELECT",
        Statement::Truncate { .. } => "TRUNCATE",
        _ => "this",
    }
}

fn truncate_parts(parts: &mut [String]) {
    for part in parts.iter_mut() {
        if part.len() > crate::naming::MAX_IDENTIFIER_BYTES {
            let mut end = crate::naming::MAX_IDENTIFIER_BYTES;
            while end > 0 && !part.is_char_boundary(end) {
                end -= 1;
            }
            *part = part[..end].to_owned();
        }
    }
}
