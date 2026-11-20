//! pgdelta's `CREATE TABLE` grammar (PostgreSQL dialect).
//!
//! Follows the synopsis in the PostgreSQL 18 documentation, `CREATE TABLE`
//! (<https://www.postgresql.org/docs/18/sql-createtable.html)>: column
//! definitions with their `column_constraint`s and table constraints.
//! Milestone-1 scope covers plain permanent tables; the parser rejects
//! `TEMPORARY`/`UNLOGGED` persistence, partitioning, `LIKE`, typed tables
//! and inheritance with explicit messages (tracked in `docs/parser.md`).

use sqlparser::tokenizer::Token;

use super::tokens::{Cursor, join_tokens};
use super::{
    ParsedSequenceOptions, ParsedStatement, RawColumn, RawConstraint, RawConstraintKind,
    RawForeignKey, RawTable, parse_fk_actions, parse_sequence_options_paren,
};
use crate::error::{Error, Result};
use crate::model::{Deferrable, IdentityKind};

/// Words that may follow a column's data type (and therefore terminate the
/// type-capture). All of them start a column option or the next
/// column/constraint, so an unquoted occurrence at depth 0 ends the type.
const COLUMN_TYPE_BOUNDARIES: &[&str] = &[
    "collate",
    "constraint",
    "not",
    "null",
    "default",
    "generated",
    "unique",
    "primary",
    "references",
    "check",
    "exclude",
    "deferrable",
    "initially",
    "enforced",
    "storage",
    "compression",
];

/// Words that may follow a column option's expression.
const EXPR_BOUNDARIES: &[&str] = &[
    "collate",
    "constraint",
    "not",
    "null",
    "default",
    "generated",
    "unique",
    "primary",
    "references",
    "check",
    "exclude",
    "deferrable",
    "initially",
    "enforced",
    "storage",
    "compression",
];

/// Parses `CREATE TABLE` (and rejects other `CREATE` forms with precise
/// messages).
pub(crate) fn parse_create(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedStatement> {
    // Reject persistence/type/other prefixes before TABLE.
    if cursor.at_word("OR") {
        return Err(Error::unsupported(
            source,
            index,
            "CREATE OR REPLACE TABLE is not a PostgreSQL statement",
            text,
        ));
    }
    if cursor.at_word("TEMPORARY")
        || cursor.at_word("TEMP")
        || cursor.at_word("GLOBAL")
        || cursor.at_word("LOCAL")
    {
        return Err(Error::unsupported(
            source,
            index,
            "temporary tables are not modelled in milestone 1 (declarative schema sources \
             describe persistent database state)",
            text,
        ));
    }
    if cursor.at_word("UNLOGGED") {
        return Err(Error::unsupported(
            source,
            index,
            "UNLOGGED tables are not modelled in milestone 1 (persistence differences are \
             tracked with the storage milestone; see docs/parser.md)",
            text,
        ));
    }
    if !cursor.eat_word("TABLE") {
        return Err(Error::unsupported(
            source,
            index,
            "expected TABLE after CREATE (other CREATE forms are rejected with explicit \
             messages by their own parsers or the misc dispatcher)",
            text,
        ));
    }
    let if_not_exists = cursor.eat_words2("IF", "NOT") && cursor.eat_word("EXISTS");
    let Some(mut name) = cursor.parse_object_name() else {
        return Err(Error::parse(source, index, "expected a table name", text));
    };
    if cursor.at_word("PARTITION") {
        return super::objects::parse_create_partition(source, index, cursor, name, text);
    }
    if cursor.at_word("OF") {
        return Err(Error::unsupported(
            source,
            index,
            "typed tables (OF type_name) are not modelled in milestone 1",
            text,
        ));
    }
    if !cursor.eat_lparen() {
        // CREATE TABLE ... AS SELECT / any other tail-less form.
        return Err(Error::unsupported(
            source,
            index,
            "expected '(' to start the column list (CREATE TABLE ... AS SELECT and \
             CREATE TABLE ... LIKE are not modelled)",
            text,
        ));
    }
    name = truncate_object_name(name);

    let mut raw = RawTable::default();
    loop {
        parse_table_member(source, index, cursor, text, &mut raw)?;
        if cursor.eat_rparen() {
            break;
        }
        if !cursor.eat_comma() {
            return Err(Error::parse(
                source,
                index,
                "expected ',' or ')' after a column or constraint definition",
                text,
            ));
        }
    }

    // `PARTITION BY` is part of the table's definition; other trailing
    // options (INHERITS, USING, WITH, ...) configure unmodelled state.
    if cursor.at_word("PARTITION") {
        raw.partition = Some(super::objects::parse_partition_by(
            source, index, cursor, text,
        )?);
    }
    if !cursor.is_empty() {
        let tail = join_tokens(&cursor.tokens()[cursor.pos()..]);
        return Err(Error::unsupported(
            source,
            index,
            format!("unsupported table options after the column list: {tail}"),
            text,
        ));
    }

    Ok(ParsedStatement::CreateTable {
        name,
        if_not_exists,
        raw,
    })
}

/// Truncates every identifier part to PostgreSQL's 63-byte limit
/// (`NAMEDATALEN - 1`; PostgreSQL docs §4.1.1: "The system uses no more
/// than NAMEDATALEN-1 bytes of an identifier; longer names can be written
/// in commands, but they will be truncated"). The clip keeps UTF-8
/// characters intact.
fn truncate_object_name(parts: Vec<String>) -> Vec<String> {
    parts
        .into_iter()
        .map(|p| {
            if p.len() <= crate::naming::MAX_IDENTIFIER_BYTES {
                p
            } else {
                let mut end = crate::naming::MAX_IDENTIFIER_BYTES;
                while end > 0 && !p.is_char_boundary(end) {
                    end -= 1;
                }
                p[..end].to_owned()
            }
        })
        .collect()
}

/// Parses one member of the parenthesised table body: a column definition
/// or a table constraint (PostgreSQL docs, `CREATE TABLE`, synopsis).
fn parse_table_member(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
    raw: &mut RawTable,
) -> Result<()> {
    // Table constraints start with these unquoted keywords.
    let starts_constraint = matches!(cursor.peek(), Some(Token::Word(w)) if w.quote_style.is_none() && matches!(
        w.value.to_ascii_uppercase().as_str(),
        "CONSTRAINT" | "PRIMARY" | "UNIQUE" | "CHECK" | "FOREIGN" | "EXCLUDE" | "NOT"
    ));
    if starts_constraint {
        parse_table_constraint(source, index, cursor, text, raw, None)?;
        return Ok(());
    }
    let column = parse_column_def(source, index, cursor, text)?;
    for inline in column.inline {
        raw.constraints.push(inline);
    }
    raw.columns.push(RawColumn {
        name: column.name,
        data_type: column.data_type,
        not_null: column.not_null,
        default: column.default,
        generated: column.generated,
        identity: column.identity,
        collation: column.collation,
        inline: Vec::new(),
    });
    Ok(())
}

/// Intermediate column definition (inline constraints still attached).
pub(crate) struct ColumnDefParsed {
    /// Column name.
    pub(crate) name: String,
    /// Canonical data type.
    pub(crate) data_type: String,
    /// `NOT NULL`.
    pub(crate) not_null: bool,
    /// Normalised default expression.
    pub(crate) default: Option<String>,
    /// Stored generated expression.
    pub(crate) generated: Option<String>,
    /// Identity kind + parsed sequence options.
    pub(crate) identity: Option<(IdentityKind, ParsedSequenceOptions)>,
    /// Explicit collation.
    pub(crate) collation: Option<String>,
    /// Inline constraints.
    pub(crate) inline: Vec<RawConstraint>,
}

/// Parses `column_name data_type [column_constraint ...]` (PostgreSQL
/// docs, `CREATE TABLE`, synopsis).
///
/// # Errors
///
/// Returns an error when the column name or data type is missing, the
/// type is unsupported (including the `serial` family), or a constraint
/// clause is malformed.
pub(crate) fn parse_column_def(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ColumnDefParsed> {
    let Some(name) = cursor.parse_ident() else {
        return Err(Error::parse(source, index, "expected a column name", text));
    };
    let type_text = cursor.capture_until_words(COLUMN_TYPE_BOUNDARIES);
    if type_text.trim().is_empty() {
        return Err(Error::parse(
            source,
            index,
            "expected a data type after the column name",
            text,
        ));
    }
    let data_type = super::datatype::normalize_type_text(&type_text).map_err(|e| {
        Error::parse(
            source,
            index,
            format!("bad data type {type_text:?}: {e}"),
            text,
        )
    })?;
    if super::datatype::is_serial_family(&data_type) {
        return Err(Error::unsupported(
            source,
            index,
            format!(
                "{data_type} pseudo-types imply a sequence object; sequences are modelled in a \
                 later milestone — use GENERATED ... AS IDENTITY instead (see docs/parser.md)"
            ),
            text,
        ));
    }

    let mut out = ColumnDefParsed {
        name,
        data_type,
        not_null: false,
        default: None,
        generated: None,
        identity: None,
        collation: None,
        inline: Vec::new(),
    };

    // Optional collation (per the synopsis, COLLATE precedes constraints).
    if cursor.eat_word("COLLATE") {
        let Some(parts) = cursor.parse_object_name() else {
            return Err(Error::parse(
                source,
                index,
                "expected a collation name after COLLATE",
                text,
            ));
        };
        out.collation = Some(parts.join("."));
    }

    loop {
        if cursor.is_empty() || matches!(cursor.peek(), Some(Token::Comma | Token::RParen)) {
            break;
        }
        let column_name = out.name.clone();
        if cursor.at_word("CONSTRAINT") {
            // Named column constraint.
            cursor.advance(1);
            let Some(cname) = cursor.parse_ident() else {
                return Err(Error::parse(
                    source,
                    index,
                    "expected a constraint name after CONSTRAINT",
                    text,
                ));
            };
            parse_column_constraint(
                source,
                index,
                cursor,
                text,
                &column_name,
                Some(cname),
                &mut out,
            )?;
            continue;
        }
        parse_column_constraint(source, index, cursor, text, &column_name, None, &mut out)?;
    }
    Ok(out)
}

/// Parses one `column_constraint` (PostgreSQL docs, `CREATE TABLE`,
/// synopsis: `[CONSTRAINT name] { NOT NULL | NULL | CHECK | DEFAULT |
/// GENERATED ... | UNIQUE | PRIMARY KEY | REFERENCES ... } [DEFERRABLE |
/// NOT DEFERRABLE] [INITIALLY ...]`).
fn parse_column_constraint(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
    column: &str,
    name: Option<String>,
    out: &mut ColumnDefParsed,
) -> Result<()> {
    if cursor.eat_words2("NOT", "NULL") {
        if cursor.at_word("NO") {
            return Err(Error::unsupported(
                source,
                index,
                "NOT NULL NO INHERIT requires inheritance modelling (out of milestone-1 scope)",
                text,
            ));
        }
        out.not_null = true;
        return Ok(());
    }
    if cursor.eat_word("NULL") {
        out.not_null = false;
        return Ok(());
    }
    if cursor.eat_word("DEFAULT") {
        let expr_text = cursor.capture_until_words(EXPR_BOUNDARIES);
        out.default = Some(super::expr::normalize_expr_text(&expr_text).map_err(|e| {
            Error::parse(source, index, format!("bad DEFAULT expression: {e}"), text)
        })?);
        return Ok(());
    }
    if cursor.at_word("GENERATED") {
        cursor.advance(1);
        let kind = if cursor.eat_word("ALWAYS") {
            IdentityKind::Always
        } else if cursor.eat_words2("BY", "DEFAULT") {
            IdentityKind::ByDefault
        } else {
            return Err(Error::parse(
                source,
                index,
                "expected ALWAYS or BY DEFAULT after GENERATED",
                text,
            ));
        };
        if !cursor.eat_word("AS") {
            return Err(Error::parse(
                source,
                index,
                "expected AS after GENERATED ALWAYS/BY DEFAULT",
                text,
            ));
        }
        if cursor.eat_word("IDENTITY") {
            let opts = parse_sequence_options_paren(source, index, cursor, text)?;
            out.identity = Some((kind, opts));
            // Identity columns are implicitly NOT NULL (PostgreSQL docs,
            // CREATE TABLE, GENERATED ... AS IDENTITY).
            out.not_null = true;
            return Ok(());
        }
        // Generated column: GENERATED ALWAYS AS (expr) STORED.
        let Some(expr_text) = cursor.capture_paren_group() else {
            return Err(Error::parse(
                source,
                index,
                "expected '(expression)' after GENERATED ALWAYS AS",
                text,
            ));
        };
        let expr = super::expr::normalize_expr_text(&expr_text).map_err(|e| {
            Error::parse(
                source,
                index,
                format!("bad generation expression: {e}"),
                text,
            )
        })?;
        if cursor.eat_word("VIRTUAL") {
            return Err(Error::unsupported(
                source,
                index,
                "VIRTUAL generated columns are new in PostgreSQL 18 and not modelled in \
                 milestone 1; use STORED (supported by PostgreSQL 13+)",
                text,
            ));
        }
        if !cursor.eat_word("STORED") {
            return Err(Error::unsupported(
                source,
                index,
                "generated columns must spell STORED explicitly (PostgreSQL 13-17 require it; \
                 omitting it means VIRTUAL on PostgreSQL 18, which pgdelta does not model)",
                text,
            ));
        }
        out.generated = Some(expr);
        return Ok(());
    }
    if cursor.eat_word("UNIQUE") {
        let nulls_distinct = parse_nulls_distinct(source, index, cursor, text)?;
        let deferrable = parse_deferrable(source, index, cursor, text)?;
        out.inline.push(RawConstraint {
            name,
            kind: RawConstraintKind::Unique {
                columns: vec![column.to_owned()],
                nulls_distinct,
            },
            deferrable,
            column_level: Some(column.to_owned()),
        });
        return Ok(());
    }
    if cursor.eat_words2("PRIMARY", "KEY") {
        let deferrable = parse_deferrable(source, index, cursor, text)?;
        out.inline.push(RawConstraint {
            name,
            kind: RawConstraintKind::PrimaryKey {
                columns: vec![column.to_owned()],
            },
            deferrable,
            column_level: Some(column.to_owned()),
        });
        // PRIMARY KEY implies NOT NULL.
        out.not_null = true;
        return Ok(());
    }
    if cursor.eat_word("REFERENCES") {
        let fk = parse_reference_tail(source, index, cursor, text, vec![column.to_owned()])?;
        let deferrable = parse_deferrable(source, index, cursor, text)?;
        out.inline.push(RawConstraint {
            name,
            kind: RawConstraintKind::ForeignKey(fk),
            deferrable,
            column_level: Some(column.to_owned()),
        });
        return Ok(());
    }
    if cursor.eat_word("CHECK") {
        let Some(expr_text) = cursor.capture_paren_group() else {
            return Err(Error::parse(
                source,
                index,
                "expected '(expression)' after CHECK",
                text,
            ));
        };
        if cursor.at_word("NO") {
            return Err(Error::unsupported(
                source,
                index,
                "CHECK ... NO INHERIT requires inheritance modelling (out of milestone-1 scope)",
                text,
            ));
        }
        let expr = super::expr::normalize_expr_text(&expr_text)
            .map_err(|e| Error::parse(source, index, format!("bad CHECK expression: {e}"), text))?;
        // CHECK constraints cannot be deferrable (PostgreSQL docs, CREATE
        // TABLE, DEFERRABLE); a stray DEFERRABLE token falls through to
        // the generic constraint error below.
        out.inline.push(RawConstraint {
            name,
            kind: RawConstraintKind::Check { expr },
            deferrable: None,
            column_level: Some(column.to_owned()),
        });
        return Ok(());
    }
    if cursor.at_word("STORAGE") || cursor.at_word("COMPRESSION") {
        return Err(Error::unsupported(
            source,
            index,
            "column STORAGE/COMPRESSION clauses are not modelled in milestone 1",
            text,
        ));
    }
    if cursor.at_word("ENFORCED")
        || (cursor.at_word("NOT")
            && matches!(cursor.peek_nth(1), Some(Token::Word(w)) if w.value.eq_ignore_ascii_case("ENFORCED")))
    {
        return Err(Error::unsupported(
            source,
            index,
            "ENFORCED/NOT ENFORCED constraints (PostgreSQL 18) are not modelled in milestone 1",
            text,
        ));
    }
    if cursor.at_word("DEFERRABLE")
        || (cursor.at_word("NOT")
            && matches!(cursor.peek_nth(1), Some(Token::Word(w)) if w.value.eq_ignore_ascii_case("DEFERRABLE")))
    {
        return Err(Error::invalid(
            source,
            index,
            "DEFERRABLE is only accepted on UNIQUE, PRIMARY KEY and foreign-key constraints \
             (PostgreSQL docs, CREATE TABLE, DEFERRABLE)",
            text,
        ));
    }
    let found = cursor.peek().map_or_else(
        || "end of statement".to_owned(),
        std::string::ToString::to_string,
    );
    Err(Error::parse(
        source,
        index,
        format!(
            "expected a column constraint (NOT NULL, DEFAULT, UNIQUE, PRIMARY KEY, REFERENCES, CHECK, GENERATED, COLLATE), found {found}"
        ),
        text,
    ))
}

/// Parses `NULLS NOT DISTINCT` / `NULLS DISTINCT` after `UNIQUE`
/// (PostgreSQL 15+); returns `true` (nulls distinct) unless `NULLS NOT
/// DISTINCT` was written.
fn parse_nulls_distinct(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<bool> {
    if !cursor.eat_word("NULLS") {
        return Ok(true);
    }
    if cursor.eat_word("NOT") {
        if !cursor.eat_word("DISTINCT") {
            return Err(Error::parse(
                source,
                index,
                "expected DISTINCT after NULLS NOT",
                text,
            ));
        }
        Ok(false)
    } else if cursor.eat_word("DISTINCT") {
        Ok(true)
    } else {
        Err(Error::parse(
            source,
            index,
            "expected [NOT] DISTINCT after NULLS",
            text,
        ))
    }
}

/// Parses `[DEFERRABLE | NOT DEFERRABLE] [INITIALLY DEFERRED | INITIALLY
/// IMMEDIATE]` (PostgreSQL docs, `CREATE TABLE`, "DEFERRABLE").
pub(crate) fn parse_deferrable(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<Option<Deferrable>> {
    if cursor.eat_word("DEFERRABLE") {
        let initially_deferred = if cursor.eat_word("INITIALLY") {
            if cursor.eat_word("DEFERRED") {
                true
            } else if cursor.eat_word("IMMEDIATE") {
                false
            } else {
                return Err(Error::parse(
                    source,
                    index,
                    "expected DEFERRED or IMMEDIATE after INITIALLY",
                    text,
                ));
            }
        } else {
            false
        };
        return Ok(Some(Deferrable {
            deferrable: true,
            initially_deferred,
        }));
    }
    if cursor.eat_words2("NOT", "DEFERRABLE") {
        return Ok(None);
    }
    Ok(None)
}

/// Parses the tail of a `REFERENCES reftable [(refcolumns)] [MATCH ...]
/// [ON DELETE ...] [ON UPDATE ...]` clause (shared by column-level and
/// table-level foreign keys).
pub(crate) fn parse_reference_tail(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
    referencing: Vec<String>,
) -> Result<RawForeignKey> {
    let Some(parts) = cursor.parse_object_name() else {
        return Err(Error::parse(
            source,
            index,
            "expected a table name after REFERENCES",
            text,
        ));
    };
    let (ref_schema, ref_table) = match parts.len() {
        1 => ("public".to_owned(), parts[0].clone()),
        2 => (parts[0].clone(), parts[1].clone()),
        _ => {
            return Err(Error::invalid(
                source,
                index,
                "expected an unqualified or schema-qualified table name after REFERENCES",
                text,
            ));
        }
    };
    let ref_columns = cursor.parse_ident_list_paren();
    let (on_delete, on_update, match_type) = parse_fk_actions(source, index, cursor, text)?;
    Ok(RawForeignKey {
        columns: referencing,
        ref_schema,
        ref_table,
        ref_columns,
        on_delete,
        on_update,
        match_type,
    })
}

/// Parses a table constraint (PostgreSQL docs, `CREATE TABLE`,
/// "`table_constraint`"). `column_level` is set for constraints parsed as
/// part of a column definition (used for auto-naming).
pub(crate) fn parse_table_constraint(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
    raw: &mut RawTable,
    column_level: Option<String>,
) -> Result<()> {
    let mut name = None;
    if cursor.eat_word("CONSTRAINT") {
        let Some(cname) = cursor.parse_ident() else {
            return Err(Error::parse(
                source,
                index,
                "expected a constraint name after CONSTRAINT",
                text,
            ));
        };
        name = Some(cname);
    }

    if cursor.at_word("NOT") {
        // PostgreSQL 18's table-level NOT NULL constraint.
        return Err(Error::unsupported(
            source,
            index,
            "table-level NOT NULL constraints are new in PostgreSQL 18 and not modelled in \
             milestone 1 (use column-level NOT NULL)",
            text,
        ));
    }
    if cursor.at_word("EXCLUDE") {
        return Err(Error::unsupported(
            source,
            index,
            "exclusion constraints are not modelled in milestone 1; sqlparser-rs cannot parse \
             them either (see docs/parser.md). Track them with the exclusion-constraint \
             milestone",
            text,
        ));
    }
    if cursor.eat_words2("PRIMARY", "KEY") {
        let Some(columns) = cursor.parse_ident_list_paren() else {
            return Err(Error::parse(
                source,
                index,
                "expected a column list after PRIMARY KEY",
                text,
            ));
        };
        reject_index_parameters(source, index, cursor, text, "PRIMARY KEY")?;
        let deferrable = parse_deferrable(source, index, cursor, text)?;
        raw.constraints.push(RawConstraint {
            name,
            kind: RawConstraintKind::PrimaryKey { columns },
            deferrable,
            column_level,
        });
        return Ok(());
    }
    if cursor.eat_word("UNIQUE") {
        let nulls_distinct = parse_nulls_distinct(source, index, cursor, text)?;
        let Some(columns) = cursor.parse_ident_list_paren() else {
            return Err(Error::parse(
                source,
                index,
                "expected a column list after UNIQUE",
                text,
            ));
        };
        if cursor.at_word("WITHOUT") {
            return Err(Error::unsupported(
                source,
                index,
                "WITHOUT OVERLAPS temporal keys (PostgreSQL 18) are not modelled in milestone 1",
                text,
            ));
        }
        reject_index_parameters(source, index, cursor, text, "UNIQUE")?;
        let deferrable = parse_deferrable(source, index, cursor, text)?;
        raw.constraints.push(RawConstraint {
            name,
            kind: RawConstraintKind::Unique {
                columns,
                nulls_distinct,
            },
            deferrable,
            column_level,
        });
        return Ok(());
    }
    if cursor.eat_word("CHECK") {
        let Some(expr_text) = cursor.capture_paren_group() else {
            return Err(Error::parse(
                source,
                index,
                "expected '(expression)' after CHECK",
                text,
            ));
        };
        if cursor.at_word("NO") {
            return Err(Error::unsupported(
                source,
                index,
                "CHECK ... NO INHERIT requires inheritance modelling (out of milestone-1 scope)",
                text,
            ));
        }
        let expr = super::expr::normalize_expr_text(&expr_text)
            .map_err(|e| Error::parse(source, index, format!("bad CHECK expression: {e}"), text))?;
        raw.constraints.push(RawConstraint {
            name,
            kind: RawConstraintKind::Check { expr },
            deferrable: None,
            column_level,
        });
        return Ok(());
    }
    if cursor.eat_words2("FOREIGN", "KEY") {
        let Some(columns) = cursor.parse_ident_list_paren() else {
            return Err(Error::parse(
                source,
                index,
                "expected a column list after FOREIGN KEY",
                text,
            ));
        };
        if cursor.at_word("PERIOD") {
            return Err(Error::unsupported(
                source,
                index,
                "PERIOD foreign keys (PostgreSQL 18) are not modelled in milestone 1",
                text,
            ));
        }
        if !cursor.eat_word("REFERENCES") {
            return Err(Error::parse(
                source,
                index,
                "expected REFERENCES after FOREIGN KEY (...)",
                text,
            ));
        }
        let fk = parse_reference_tail(source, index, cursor, text, columns)?;
        if cursor.at_word("PERIOD") {
            return Err(Error::unsupported(
                source,
                index,
                "PERIOD foreign keys (PostgreSQL 18) are not modelled in milestone 1",
                text,
            ));
        }
        let deferrable = parse_deferrable(source, index, cursor, text)?;
        raw.constraints.push(RawConstraint {
            name,
            kind: RawConstraintKind::ForeignKey(fk),
            deferrable,
            column_level,
        });
        return Ok(());
    }
    Err(Error::parse(
        source,
        index,
        "expected PRIMARY KEY, UNIQUE, CHECK, FOREIGN KEY or EXCLUDE to start a table constraint",
        text,
    ))
}

/// Rejects `INCLUDE`/`WITH`/`USING INDEX TABLESPACE` on `PRIMARY KEY` /
/// `UNIQUE` constraints (the `index_parameters` of the PostgreSQL synopsis).
/// They shape the constraint's backing index; milestone 1 models the
/// constraint itself (the index is implied), so these are documented
/// unsupported.
fn reject_index_parameters(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
    what: &str,
) -> Result<()> {
    if cursor.at_word("INCLUDE") || cursor.at_word("WITH") || cursor.at_word("USING") {
        return Err(Error::unsupported(
            source,
            index,
            format!(
                "{what} constraint index parameters (INCLUDE/WITH/USING INDEX TABLESPACE) \
                 are not modelled in milestone 1 (the backing index is implied by the \
                 constraint; see docs/parser.md)"
            ),
            text,
        ));
    }
    Ok(())
}
