//! pgdelta's `CREATE INDEX` / `DROP INDEX` / `ALTER INDEX ... RENAME`
//! grammars (PostgreSQL dialect).
//!
//! Follows the synopsis in the PostgreSQL 18 documentation, `CREATE
//! INDEX` (<https://www.postgresql.org/docs/18/sql-createindex.html)>:
//! key columns or expressions with optional `COLLATE`, operator class
//! (with parameters), `ASC`/`DESC`, `NULLS FIRST/LAST`; plus `INCLUDE`,
//! `NULLS [NOT] DISTINCT`, `WITH`, `TABLESPACE`, `WHERE`.

use super::tokens::Cursor;
use super::{ParsedStatement, RawIndex};
use crate::error::{Error, Result};
use crate::model::{IndexElem, IndexElemKind, NullsOrder, SortOrder};

/// Parses `CREATE [UNIQUE] INDEX [CONCURRENTLY] [IF NOT EXISTS] [name] ON
/// [ONLY] table_name [USING method] ( elems ) [INCLUDE (...)] [NULLS [NOT]
/// DISTINCT] [WITH (...)] [TABLESPACE x] [WHERE pred]`.
pub(crate) fn parse_create_index(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedStatement> {
    let unique = cursor.eat_word("UNIQUE");
    if !cursor.eat_word("INDEX") {
        return Err(Error::unsupported(
            source,
            index,
            "expected INDEX after CREATE",
            text,
        ));
    }
    let concurrent = cursor.eat_word("CONCURRENTLY");
    let if_not_exists = cursor.eat_words2("IF", "NOT") && cursor.eat_word("EXISTS");
    let name = if cursor.at_word("ON") {
        None
    } else {
        Some(
            cursor
                .parse_object_name()
                .ok_or_else(|| Error::parse(source, index, "expected an index name", text))?,
        )
    };
    if !cursor.eat_word("ON") {
        return Err(Error::parse(source, index, "expected ON before the table name", text));
    }
    if cursor.eat_word("ONLY") {
        return Err(Error::unsupported(
            source,
            index,
            "CREATE INDEX ON ONLY (partitioned parents, no recursion) is not modelled in \
             milestone 1; partitioning is a later milestone",
            text,
        ));
    }
    let Some(mut table) = cursor.parse_object_name() else {
        return Err(Error::parse(source, index, "expected a table name after ON", text));
    };
    truncate_parts(&mut table);

    let mut method = None;
    if cursor.eat_word("USING") {
        let Some(parts) = cursor.parse_object_name() else {
            return Err(Error::parse(source, index, "expected an index method after USING", text));
        };
        let m = parts.join(".");
        // Explicit USING btree is the default; normalise it away so that
        // `... USING btree` and no-`USING` compare equal.
        if m != "btree" {
            method = Some(m);
        }
    }

    let Some(columns) = parse_index_elems(source, index, cursor, text)? else {
        return Err(Error::parse(source, index, "expected an index element list", text));
    };

    let mut include = Vec::new();
    if cursor.eat_word("INCLUDE") {
        let Some(cols) = cursor.parse_ident_list_paren() else {
            return Err(Error::parse(source, index, "expected a column list after INCLUDE", text));
        };
        include = cols;
    }

    let mut nulls_distinct = true;
    if cursor.eat_word("NULLS") {
        if cursor.eat_word("NOT") {
            if !cursor.eat_word("DISTINCT") {
                return Err(Error::parse(source, index, "expected DISTINCT after NULLS NOT", text));
            }
            nulls_distinct = false;
        } else if !cursor.eat_word("DISTINCT") {
            return Err(Error::parse(source, index, "expected [NOT] DISTINCT after NULLS", text));
        }
    }

    let mut with = Vec::new();
    if cursor.at_word("WITH") {
        cursor.advance(1);
        if !cursor.eat_lparen() {
            return Err(Error::parse(source, index, "expected '(' after WITH", text));
        }
        loop {
            let Some(key) = cursor.parse_ident() else {
                return Err(Error::parse(source, index, "expected a storage parameter name in WITH (...)", text));
            };
            let mut value = "on".to_owned();
            if matches!(cursor.peek(), Some(sqlparser::tokenizer::Token::Eq)) {
                cursor.advance(1);
                // capture_until_words stops at ',' or ')' at depth 0,
                // leaving the cursor on the separator.
                value = cursor.capture_until_words(&[]);
            }
            with.push((key, value));
            if cursor.eat_rparen() {
                break;
            }
            if !cursor.eat_comma() {
                return Err(Error::parse(source, index, "expected ',' or ')' in WITH (...)", text));
            }
        }
    }

    let mut tablespace = None;
    if cursor.eat_word("TABLESPACE") {
        let Some(ts) = cursor.parse_ident() else {
            return Err(Error::parse(source, index, "expected a tablespace name", text));
        };
        tablespace = Some(ts);
    }

    let mut predicate = None;
    if cursor.eat_word("WHERE") {
        let pred_text = cursor.capture_until_words(&[]);
        if pred_text.trim().is_empty() {
            return Err(Error::parse(source, index, "expected a predicate after WHERE", text));
        }
        predicate = Some(
            super::expr::normalize_expr_text(&pred_text)
                .map_err(|e| Error::parse(source, index, format!("bad index predicate: {e}"), text))?,
        );
    }

    if !cursor.is_empty() {
        return Err(Error::parse(source, index, "unexpected trailing tokens after the index definition", text));
    }
    if !unique && !nulls_distinct {
        return Err(Error::invalid(
            source,
            index,
            "NULLS NOT DISTINCT is only valid on unique indexes (PostgreSQL docs, CREATE \
             INDEX, NULLS DISTINCT / NULLS NOT DISTINCT)",
            text,
        ));
    }

    Ok(ParsedStatement::CreateIndex {
        raw: RawIndex {
            name,
            table,
            unique,
            concurrent,
            if_not_exists,
            method,
            columns,
            include,
            nulls_distinct,
            with,
            tablespace,
            predicate,
        },
    })
}

/// Parses the parenthesised index element list. Returns `None` when the
/// shape does not match `( elem [, ...] )`.
fn parse_index_elems(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<Option<Vec<IndexElem>>> {
    if !cursor.eat_lparen() {
        return Ok(None);
    }
    let mut out = Vec::new();
    loop {
        let Some(elem) = parse_index_elem(source, index, cursor, text)? else {
            return Ok(None);
        };
        out.push(elem);
        if cursor.eat_rparen() {
            break;
        }
        if !cursor.eat_comma() {
            return Ok(None);
        }
    }
    Ok(Some(out))
}

/// Parses one index element:
/// `{ column_name | ( expression ) } [COLLATE c] [opclass [(params)]]
/// [ASC|DESC] [NULLS {FIRST|LAST}]` (PostgreSQL docs, `CREATE INDEX`,
/// synopsis).
fn parse_index_elem(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<Option<IndexElem>> {
    let kind = if cursor.at_lparen() {
        let Some(expr_text) = cursor.capture_paren_group() else {
            return Ok(None);
        };
        let expr = super::expr::normalize_expr_text(&expr_text)
            .map_err(|e| Error::parse(source, index, format!("bad index expression: {e}"), text))?;
        IndexElemKind::Expression(expr)
    } else {
        let Some(col) = cursor.parse_ident() else {
            return Ok(None);
        };
        IndexElemKind::Column(col)
    };

    let mut collation = None;
    if cursor.eat_word("COLLATE") {
        let Some(parts) = cursor.parse_object_name() else {
            return Ok(None);
        };
        collation = Some(parts.join("."));
    }

    // Operator class — a bare word at this position; a following `(`
    // opens its parameters (PostgreSQL docs, `CREATE INDEX`, `opclass
    // [ ( opclass_parameter = value [, ... ] ) ]`).
    let mut opclass = None;
    let mut opclass_params = None;
    if !cursor.at_word("ASC") && !cursor.at_word("DESC") && !cursor.at_word("NULLS")
        && let Some(parts) = cursor.parse_object_name() {
            opclass = Some(parts.join("."));
            if cursor.at_lparen() {
                let Some(params) = cursor.capture_paren_group() else {
                    return Ok(None);
                };
                opclass_params = Some(params);
            }
        }

    let mut order = None;
    if cursor.eat_word("ASC") {
        order = Some(SortOrder::Asc);
    } else if cursor.eat_word("DESC") {
        order = Some(SortOrder::Desc);
    }
    let mut nulls = None;
    if cursor.eat_word("NULLS") {
        if cursor.eat_word("FIRST") {
            nulls = Some(NullsOrder::First);
        } else if cursor.eat_word("LAST") {
            nulls = Some(NullsOrder::Last);
        } else {
            return Ok(None);
        }
    }

    Ok(Some(IndexElem {
        kind,
        collation,
        opclass,
        opclass_params,
        order,
        nulls,
    }))
}

/// Parses `DROP INDEX [CONCURRENTLY] [IF EXISTS] name [, ...]
/// [RESTRICT|CASCADE]`.
///
/// `CONCURRENTLY` is accepted and ignored: the planner always emits plain
/// (transactional) `DROP INDEX`, which takes only a brief lock; flipping
/// drops to concurrent is part of the online mode in a later milestone.
pub(crate) fn parse_drop_index(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedStatement> {
    let _concurrent = cursor.eat_word("CONCURRENTLY");
    let if_exists = cursor.eat_words2("IF", "EXISTS");
    let mut names = Vec::new();
    loop {
        let Some(mut parts) = cursor.parse_object_name() else {
            return Err(Error::parse(source, index, "expected an index name", text));
        };
        truncate_parts(&mut parts);
        names.push(parts);
        if !cursor.eat_comma() {
            break;
        }
    }
    if cursor.eat_word("CASCADE") {
        return Err(Error::unsupported(
            source,
            index,
            "DROP INDEX ... CASCADE drops dependent objects implicitly, which pgdelta will \
             never do silently; drop the dependents explicitly",
            text,
        ));
    }
    cursor.eat_word("RESTRICT");
    Ok(ParsedStatement::DropIndex {
        names,
        if_exists,
    })
}

/// Parses `ALTER INDEX [IF EXISTS] name RENAME TO new_name` (the only
/// index alteration that changes modelled state in milestone 1).
pub(crate) fn parse_alter_index(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedStatement> {
    let if_exists = cursor.eat_words2("IF", "EXISTS");
    let Some(mut name) = cursor.parse_object_name() else {
        return Err(Error::parse(source, index, "expected an index name", text));
    };
    truncate_parts(&mut name);
    if !cursor.eat_word("RENAME") {
        return Err(Error::unsupported(
            source,
            index,
            "only ALTER INDEX ... RENAME TO is modelled in milestone 1 (ALTER INDEX SET \
             TABLESPACE / ATTACH PARTITION / alters are later milestones)",
            text,
        ));
    }
    if !cursor.eat_word("TO") {
        return Err(Error::parse(source, index, "expected TO after RENAME", text));
    }
    let Some(new_name) = cursor.parse_ident() else {
        return Err(Error::parse(source, index, "expected a new index name", text));
    };
    let _ = if_exists;
    Ok(ParsedStatement::AlterIndexRename { name, new_name })
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
