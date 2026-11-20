//! Parsers for the milestone-2 object kinds: sequences, views, routines,
//! triggers, RLS policies, comments, grants and partitioning.
//!
//! All grammar rules are cited from the PostgreSQL 18 stable
//! documentation, fetched with web tools during this milestone:
//!
//! * sequences — <https://www.postgresql.org/docs/18/sql-createsequence.html>
//! * views — <https://www.postgresql.org/docs/18/sql-createview.html>,
//!   <https://www.postgresql.org/docs/18/sql-creatematerializedview.html>
//! * routines — <https://www.postgresql.org/docs/18/sql-createfunction.html>
//! * triggers — <https://www.postgresql.org/docs/18/sql-createtrigger.html>
//! * policies — <https://www.postgresql.org/docs/18/sql-createpolicy.html>
//! * comments — <https://www.postgresql.org/docs/18/sql-comment.html>
//! * grants — <https://www.postgresql.org/docs/18/sql-grant.html>
//! * partitioning — <https://www.postgresql.org/docs/18/sql-createtable.html>

use std::collections::BTreeSet;

use sqlparser::ast::DollarQuotedString;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::tokenizer::{Token, Tokenizer};

use crate::error::{Error, Result};
use crate::model::{
    ArgMode, CheckOption, Grant, ObjectKind, ObjectRef, PUBLIC_SCHEMA, PartitionKey,
    PartitionStrategy, Partitioning, Policy, PolicyCommand, Routine, RoutineArg, RoutineKind,
    Security, Sequence, SequenceOwner, Trigger, TriggerEvent, TriggerLevel, TriggerTiming, View,
    Volatility, routine_ref_name,
};

use super::tokens::{Cursor, eat_i64};
use super::{
    ParsedSequenceOptions, ParsedStatement, expr, resolve_name, take_literal_or_null,
    take_option_pairs, take_type, tokens, truncate_parts,
};

/// An `ALTER SEQUENCE` action.
#[derive(Debug, Clone)]
pub(crate) enum SequenceAction {
    /// `AS data_type`.
    SetType(String),
    /// One or more sequence parameters (normalised at apply time).
    SetOptions(ParsedSequenceOptions),
    /// `OWNED BY ...` (`None` means `OWNED BY NONE`).
    OwnedBy(Option<SequenceOwner>),
    /// `RENAME TO`.
    Rename(String),
    /// `SET SCHEMA`.
    SetSchema(String),
}

/// An `ALTER VIEW`/`ALTER MATERIALIZED VIEW` action.
#[derive(Debug, Clone)]
pub(crate) enum ViewAction {
    /// `RENAME TO`.
    Rename(String),
    /// `SET SCHEMA`.
    SetSchema(String),
    /// `SET (option = value)`.
    SetOption(String, String),
    /// `RESET (option)`.
    DropOption(String),
}

/// An `ALTER FUNCTION`/`ALTER PROCEDURE` action.
#[derive(Debug, Clone)]
pub(crate) enum RoutineAction {
    /// `RENAME TO`.
    Rename(String),
    /// `SET SCHEMA`.
    SetSchema(String),
    /// `SET parameter = value`.
    SetOption(String, String),
    /// `RESET parameter`.
    ResetOption(String),
}

/// Trigger enablement state for `ALTER TRIGGER`.
#[derive(Debug, Clone, Copy)]
pub(crate) enum TriggerEnabled {
    /// `ENABLE`.
    Enable,
    /// `ENABLE ALWAYS`.
    EnableAlways,
    /// `ENABLE REPLICA`.
    EnableReplica,
    /// `DISABLE`.
    Disable,
}

fn parse_err(source: &str, index: usize, message: &str, text: &str) -> Error {
    Error::parse(source, index, message, text)
}

/// Tokenises a fragment with the same PostgreSQL dialect used for sources.
fn lex(text: &str, _source: &str) -> Result<Vec<Token>> {
    Tokenizer::new(&PostgreSqlDialect {}, text)
        .tokenize()
        .map(|tokens| {
            tokens
                .into_iter()
                .filter(|token| !matches!(token, Token::Whitespace(_)))
                .collect()
        })
        .map_err(|e| Error::parse("<fragment>", 0, e, text))
}

// ---------------------------------------------------------------------------
// CREATE SEQUENCE / ALTER SEQUENCE / DROP SEQUENCE
// ---------------------------------------------------------------------------

/// Parses `CREATE SEQUENCE [IF NOT EXISTS] name [AS type] [options]`.
pub(crate) fn parse_create_sequence(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedStatement> {
    if !cursor.eat_word("SEQUENCE") {
        return Err(parse_err(
            source,
            index,
            "expected SEQUENCE after CREATE",
            text,
        ));
    }
    let _if_not_exists = cursor.eat_words2("IF", "NOT") && cursor.eat_word("EXISTS");
    let Some(mut name) = cursor.parse_object_name() else {
        return Err(parse_err(source, index, "expected a sequence name", text));
    };
    truncate_parts(&mut name);
    let (schema, seq_name) = resolve_name(&name);
    let mut data_type = "bigint".to_owned();
    if cursor.eat_word("AS") {
        data_type = take_type(source, index, cursor, SEQ_BOUNDARIES, text)?;
    }
    let mut opts = ParsedSequenceOptions::default();
    let mut owned_by = None;
    parse_sequence_tail(
        source,
        index,
        cursor,
        &mut data_type,
        &mut opts,
        &mut owned_by,
        text,
    )?;
    let options = super::normalize_sequence_options(source, index, &data_type, &opts, text)?;
    Ok(ParsedStatement::CreateSequence {
        sequence: Sequence {
            schema,
            name: seq_name,
            data_type,
            options,
            owned_by,
        },
    })
}

const SEQ_BOUNDARIES: &[&str] = &[
    "INCREMENT",
    "MINVALUE",
    "NO",
    "MAXVALUE",
    "START",
    "CACHE",
    "CYCLE",
    "OWNED",
    "RESTART",
    "RENAME",
    "SET",
];

fn parse_sequence_tail(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    data_type: &mut String,
    opts: &mut ParsedSequenceOptions,
    owned_by: &mut Option<SequenceOwner>,
    text: &str,
) -> Result<()> {
    loop {
        if cursor.eat_word("INCREMENT") {
            cursor.eat_word("BY");
            opts.increment = Some(
                eat_i64(cursor)
                    .ok_or_else(|| parse_err(source, index, "expected INCREMENT value", text))?,
            );
        } else if cursor.eat_word("MINVALUE") {
            opts.minvalue = Some(
                eat_i64(cursor)
                    .ok_or_else(|| parse_err(source, index, "expected MINVALUE", text))?,
            );
        } else if cursor.eat_words2("NO", "MINVALUE") {
            // Default: handled by normalisation.
        } else if cursor.eat_word("MAXVALUE") {
            opts.maxvalue = Some(
                eat_i64(cursor)
                    .ok_or_else(|| parse_err(source, index, "expected MAXVALUE", text))?,
            );
        } else if cursor.eat_words2("NO", "MAXVALUE") {
            // Default: handled by normalisation.
        } else if cursor.eat_word("START") {
            cursor.eat_word("WITH");
            opts.start = Some(
                eat_i64(cursor)
                    .ok_or_else(|| parse_err(source, index, "expected START value", text))?,
            );
        } else if cursor.eat_word("CACHE") {
            opts.cache = Some(
                eat_i64(cursor)
                    .ok_or_else(|| parse_err(source, index, "expected CACHE value", text))?,
            );
        } else if cursor.eat_word("CYCLE") {
            opts.cycle = Some(true);
        } else if cursor.eat_words2("NO", "CYCLE") {
            opts.cycle = Some(false);
        } else if cursor.eat_words2("OWNED", "BY") {
            if cursor.eat_word("NONE") {
                *owned_by = None;
            } else {
                let Some(mut parts) = cursor.parse_object_name() else {
                    return Err(parse_err(
                        source,
                        index,
                        "expected a table.column after OWNED BY",
                        text,
                    ));
                };
                truncate_parts(&mut parts);
                *owned_by = Some(parse_owned_target(source, index, cursor, &mut parts, text)?);
            }
        } else if cursor.eat_word("AS") {
            *data_type = take_type(source, index, cursor, SEQ_BOUNDARIES, text)?;
        } else if cursor.eat_word("RESTART") {
            cursor.eat_word("WITH");
            let _ = eat_i64(cursor);
        } else {
            break;
        }
    }
    if !cursor.is_empty() {
        return Err(Error::unsupported(
            source,
            index,
            "unsupported SEQUENCE option",
            text,
        ));
    }
    Ok(())
}

/// Parses `ALTER SEQUENCE name action [, ...]`.
pub(crate) fn parse_alter_sequence(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedStatement> {
    let Some(mut name) = cursor.parse_object_name() else {
        return Err(parse_err(source, index, "expected a sequence name", text));
    };
    truncate_parts(&mut name);
    let mut actions = Vec::new();
    loop {
        if cursor.eat_words2("RENAME", "TO") {
            let to = cursor
                .parse_ident()
                .ok_or_else(|| parse_err(source, index, "expected a new sequence name", text))?;
            actions.push(SequenceAction::Rename(crate::naming::truncate_ident(&to)));
        } else if cursor.eat_words2("SET", "SCHEMA") {
            let schema = cursor
                .parse_ident()
                .ok_or_else(|| parse_err(source, index, "expected a schema name", text))?;
            actions.push(SequenceAction::SetSchema(crate::naming::truncate_ident(
                &schema,
            )));
        } else if cursor.eat_word("AS") {
            let data_type = take_type(source, index, cursor, SEQ_BOUNDARIES, text)?;
            actions.push(SequenceAction::SetType(data_type));
        } else if cursor.eat_words2("OWNED", "BY") {
            if cursor.eat_word("NONE") {
                actions.push(SequenceAction::OwnedBy(None));
            } else {
                let Some(mut parts) = cursor.parse_object_name() else {
                    return Err(parse_err(
                        source,
                        index,
                        "expected table.column after OWNED BY",
                        text,
                    ));
                };
                truncate_parts(&mut parts);
                actions.push(SequenceAction::OwnedBy(Some(parse_owned_target(
                    source, index, cursor, &mut parts, text,
                )?)));
            }
        } else {
            let opts = parse_seq_param_options(source, index, cursor, text)?;
            actions.push(SequenceAction::SetOptions(opts));
            // The canonical renderer emits `OWNED BY NONE` after the option
            // list; accept it here as part of the same ALTER action.
            if cursor.eat_words2("OWNED", "BY") {
                if cursor.eat_word("NONE") {
                    actions.push(SequenceAction::OwnedBy(None));
                } else {
                    let Some(mut parts) = cursor.parse_object_name() else {
                        return Err(parse_err(
                            source,
                            index,
                            "expected table.column after OWNED BY",
                            text,
                        ));
                    };
                    truncate_parts(&mut parts);
                    actions.push(SequenceAction::OwnedBy(Some(parse_owned_target(
                        source, index, cursor, &mut parts, text,
                    )?)));
                }
            }
        }
        // PostgreSQL's `ALTER SEQUENCE` options are whitespace-separated;
        // commas are tolerated but not required between actions.
        let _ = cursor.eat_comma();
        if cursor.is_empty() {
            break;
        }
    }
    if !cursor.is_empty() {
        return Err(Error::unsupported(
            source,
            index,
            "unsupported ALTER SEQUENCE action",
            text,
        ));
    }
    Ok(ParsedStatement::AlterSequence { name, actions })
}

/// Resolves the target of `OWNED BY`: either `table.column` (one object
/// name token stream) or `schema.table.column`, folding unqualified table
/// names into `public`.
fn parse_owned_target(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    parts: &mut [String],
    text: &str,
) -> Result<SequenceOwner> {
    let (schema, table, column) = if parts.len() >= 3 {
        (parts[0].clone(), parts[1].clone(), parts[2].clone())
    } else if parts.len() == 2 {
        (PUBLIC_SCHEMA.to_owned(), parts[0].clone(), parts[1].clone())
    } else if parts.len() == 1 {
        let column = cursor.parse_ident().ok_or_else(|| {
            parse_err(
                source,
                index,
                "expected a column after OWNED BY table",
                text,
            )
        })?;
        (PUBLIC_SCHEMA.to_owned(), parts[0].clone(), column)
    } else {
        return Err(Error::invalid(
            source,
            index,
            "OWNED BY requires table.column",
            text,
        ));
    };
    Ok(SequenceOwner {
        schema,
        table,
        column,
    })
}

/// Parses the parameter options of `ALTER SEQUENCE` (not `AS`/`OWNED BY`).
fn parse_seq_param_options(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedSequenceOptions> {
    let mut opts = ParsedSequenceOptions::default();
    let mut any = false;
    loop {
        if cursor.eat_word("INCREMENT") {
            cursor.eat_word("BY");
            opts.increment = Some(
                eat_i64(cursor)
                    .ok_or_else(|| parse_err(source, index, "expected INCREMENT value", text))?,
            );
        } else if cursor.eat_word("MINVALUE") {
            opts.minvalue = Some(
                eat_i64(cursor)
                    .ok_or_else(|| parse_err(source, index, "expected MINVALUE", text))?,
            );
        } else if cursor.eat_words2("NO", "MINVALUE") {
            opts.minvalue = None;
        } else if cursor.eat_word("MAXVALUE") {
            opts.maxvalue = Some(
                eat_i64(cursor)
                    .ok_or_else(|| parse_err(source, index, "expected MAXVALUE", text))?,
            );
        } else if cursor.eat_words2("NO", "MAXVALUE") {
            opts.maxvalue = None;
        } else if cursor.eat_word("START") {
            cursor.eat_word("WITH");
            opts.start = Some(
                eat_i64(cursor)
                    .ok_or_else(|| parse_err(source, index, "expected START value", text))?,
            );
        } else if cursor.eat_word("CACHE") {
            opts.cache = Some(
                eat_i64(cursor)
                    .ok_or_else(|| parse_err(source, index, "expected CACHE value", text))?,
            );
        } else if cursor.eat_word("CYCLE") {
            opts.cycle = Some(true);
        } else if cursor.eat_words2("NO", "CYCLE") {
            opts.cycle = Some(false);
        } else if cursor.eat_word("RESTART") {
            cursor.eat_word("WITH");
            let _ = eat_i64(cursor);
        } else {
            break;
        }
        any = true;
    }
    if !any {
        return Err(Error::unsupported(
            source,
            index,
            "unsupported ALTER SEQUENCE action",
            text,
        ));
    }
    Ok(opts)
}

/// Parses `DROP SEQUENCE [IF EXISTS] name [, ...]`.
pub(crate) fn parse_drop_sequence(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedStatement> {
    let if_exists = cursor.eat_words2("IF", "EXISTS");
    let mut names = Vec::new();
    loop {
        let Some(mut parts) = cursor.parse_object_name() else {
            return Err(parse_err(source, index, "expected a sequence name", text));
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
            "DROP SEQUENCE ... CASCADE drops dependent objects implicitly, which pgdelta will \
             never do silently",
            text,
        ));
    }
    cursor.eat_word("RESTRICT");
    Ok(ParsedStatement::DropSequence { names, if_exists })
}

// ---------------------------------------------------------------------------
// CREATE VIEW / CREATE MATERIALIZED VIEW
// ---------------------------------------------------------------------------

/// Parses `CREATE [OR REPLACE] VIEW ... AS query [WITH ... CHECK OPTION]`.
pub(crate) fn parse_create_view(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedStatement> {
    let or_replace = cursor.eat_words2("OR", "REPLACE");
    if cursor.eat_word("TEMPORARY") || cursor.eat_word("TEMP") {
        return Err(Error::unsupported(
            source,
            index,
            "temporary views are session state and are not modelled",
            text,
        ));
    }
    let recursive = cursor.eat_word("RECURSIVE");
    if !cursor.eat_word("VIEW") {
        return Err(parse_err(source, index, "expected VIEW after CREATE", text));
    }
    let Some(mut name) = cursor.parse_object_name() else {
        return Err(parse_err(source, index, "expected a view name", text));
    };
    truncate_parts(&mut name);
    let (schema, view_name) = resolve_name(&name);
    let columns = cursor.parse_ident_list_paren().unwrap_or_default();
    if recursive && columns.is_empty() {
        return Err(Error::invalid(
            source,
            index,
            "a recursive view requires an explicit column list",
            text,
        ));
    }
    let options = take_option_pairs(cursor).unwrap_or_default();
    if !cursor.eat_word("AS") {
        return Err(parse_err(
            source,
            index,
            "expected AS before the view query",
            text,
        ));
    }
    let (query, check_option) = take_query_and_check(cursor);
    if query.trim().is_empty() {
        return Err(parse_err(source, index, "expected a query after AS", text));
    }
    if recursive && check_option != CheckOption::None {
        return Err(Error::invalid(
            source,
            index,
            "WITH CHECK OPTION is not allowed on a recursive view",
            text,
        ));
    }
    Ok(ParsedStatement::CreateView {
        view: View {
            schema,
            name: view_name,
            columns,
            query,
            options,
            tablespace: None,
            access_method: None,
            check_option,
            materialized: false,
            with_data: true,
        },
        or_replace,
    })
}

/// Parses `CREATE MATERIALIZED VIEW [IF NOT EXISTS] ... AS query [WITH NO DATA]`.
pub(crate) fn parse_create_matview(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedStatement> {
    if !cursor.eat_word("MATERIALIZED") || !cursor.eat_word("VIEW") {
        return Err(parse_err(source, index, "expected MATERIALIZED VIEW", text));
    }
    let _if_not_exists = cursor.eat_words2("IF", "NOT") && cursor.eat_word("EXISTS");
    let Some(mut name) = cursor.parse_object_name() else {
        return Err(parse_err(
            source,
            index,
            "expected a materialized view name",
            text,
        ));
    };
    truncate_parts(&mut name);
    let (schema, view_name) = resolve_name(&name);
    let columns = cursor.parse_ident_list_paren().unwrap_or_default();
    let access_method = if cursor.eat_word("USING") {
        cursor.parse_ident()
    } else {
        None
    };
    let options = take_option_pairs(cursor).unwrap_or_default();
    let tablespace = if cursor.eat_word("TABLESPACE") {
        cursor
            .parse_ident()
            .map(|t| crate::naming::truncate_ident(&t))
    } else {
        None
    };
    if !cursor.eat_word("AS") {
        return Err(parse_err(
            source,
            index,
            "expected AS before the materialized view query",
            text,
        ));
    }
    let mut with_data = true;
    let mut query_tokens = Vec::new();
    let toks = cursor.tokens();
    let mut i = cursor.pos();
    while i < toks.len() {
        if matches!(&toks[i], Token::Word(w) if w.quote_style.is_none() && w.value.eq_ignore_ascii_case("WITH"))
            && matches!(toks.get(i + 1), Some(Token::Word(w)) if w.quote_style.is_none() && w.value.eq_ignore_ascii_case("DATA"))
        {
            with_data = true;
            i += 2;
            break;
        }
        if matches!(&toks[i], Token::Word(w) if w.quote_style.is_none() && w.value.eq_ignore_ascii_case("WITH"))
            && matches!(toks.get(i + 1), Some(Token::Word(w)) if w.quote_style.is_none() && w.value.eq_ignore_ascii_case("NO"))
            && matches!(toks.get(i + 2), Some(Token::Word(w)) if w.quote_style.is_none() && w.value.eq_ignore_ascii_case("DATA"))
        {
            with_data = false;
            i += 3;
            break;
        }
        query_tokens.push(toks[i].clone());
        i += 1;
    }
    cursor.advance(i.saturating_sub(cursor.pos()));
    let query = tokens::join_tokens(&query_tokens);
    if query.trim().is_empty() {
        return Err(parse_err(source, index, "expected a query after AS", text));
    }
    if !cursor.is_empty() {
        return Err(Error::unsupported(
            source,
            index,
            "unsupported materialized view option",
            text,
        ));
    }
    Ok(ParsedStatement::CreateView {
        view: View {
            schema,
            name: view_name,
            columns,
            query,
            options,
            tablespace,
            access_method,
            check_option: CheckOption::None,
            materialized: true,
            with_data,
        },
        or_replace: false,
    })
}

/// Splits a trailing `WITH [CASCADED|LOCAL] CHECK OPTION` from the query.
fn take_query_and_check(cursor: &mut Cursor) -> (String, CheckOption) {
    let toks = cursor.tokens();
    let mut with_at = None;
    let mut depth = 0i32;
    for (i, tok) in toks.iter().enumerate().skip(cursor.pos()) {
        match tok {
            Token::LParen => depth += 1,
            Token::RParen => depth -= 1,
            Token::Word(w)
                if depth == 0
                    && w.quote_style.is_none()
                    && w.value.eq_ignore_ascii_case("WITH") =>
            {
                with_at = Some(i);
            }
            _ => {}
        }
    }
    let (end, check) = if let Some(i) = with_at {
        let mut j = i + 1;
        let mut check = CheckOption::Cascaded;
        if matches!(toks.get(j), Some(Token::Word(w)) if w.value.eq_ignore_ascii_case("CASCADED")) {
            j += 1;
        } else if matches!(toks.get(j), Some(Token::Word(w)) if w.value.eq_ignore_ascii_case("LOCAL"))
        {
            check = CheckOption::Local;
            j += 1;
        }
        if matches!(toks.get(j), Some(Token::Word(w)) if w.value.eq_ignore_ascii_case("CHECK"))
            && matches!(toks.get(j + 1), Some(Token::Word(w)) if w.value.eq_ignore_ascii_case("OPTION"))
            && j + 2 == toks.len()
        {
            (i, check)
        } else {
            (toks.len(), CheckOption::None)
        }
    } else {
        (toks.len(), CheckOption::None)
    };
    let query = tokens::join_tokens(&toks[cursor.pos()..end]);
    cursor.advance(end.saturating_sub(cursor.pos()));
    (query, check)
}

/// Parses `DROP VIEW`/`DROP MATERIALIZED VIEW [IF EXISTS] name [, ...]`.
pub(crate) fn parse_drop_view(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    materialized: bool,
    text: &str,
) -> Result<ParsedStatement> {
    // The dispatcher consumed DROP; for materialized views it stopped at
    // MATERIALIZED, so VIEW is still pending.
    if materialized && !cursor.eat_word("VIEW") {
        return Err(parse_err(
            source,
            index,
            "expected VIEW after MATERIALIZED",
            text,
        ));
    }
    let if_exists = cursor.eat_words2("IF", "EXISTS");
    let mut names = Vec::new();
    loop {
        let Some(mut parts) = cursor.parse_object_name() else {
            return Err(parse_err(source, index, "expected a view name", text));
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
            "DROP VIEW ... CASCADE drops dependent views implicitly, which pgdelta will never \
             do silently; drop the dependents explicitly",
            text,
        ));
    }
    cursor.eat_word("RESTRICT");
    Ok(ParsedStatement::DropView {
        names,
        if_exists,
        materialized,
    })
}

/// Parses `ALTER VIEW`/`ALTER MATERIALIZED VIEW name action [, ...]`.
pub(crate) fn parse_alter_view(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    materialized: bool,
    text: &str,
) -> Result<ParsedStatement> {
    // The dispatcher consumed ALTER; for materialized views VIEW is still
    // pending, for plain views the name follows directly.
    if materialized && !cursor.eat_word("VIEW") {
        return Err(parse_err(
            source,
            index,
            "expected VIEW after MATERIALIZED",
            text,
        ));
    }
    let Some(mut name) = cursor.parse_object_name() else {
        return Err(parse_err(source, index, "expected a view name", text));
    };
    truncate_parts(&mut name);
    let mut actions = Vec::new();
    loop {
        if cursor.eat_words2("RENAME", "TO") {
            let to = cursor
                .parse_ident()
                .ok_or_else(|| parse_err(source, index, "expected a new view name", text))?;
            actions.push(ViewAction::Rename(crate::naming::truncate_ident(&to)));
        } else if cursor.eat_words2("SET", "SCHEMA") {
            let schema = cursor
                .parse_ident()
                .ok_or_else(|| parse_err(source, index, "expected a schema name", text))?;
            actions.push(ViewAction::SetSchema(crate::naming::truncate_ident(
                &schema,
            )));
        } else if cursor.eat_word("SET") {
            let Some(pairs) = take_option_pairs(cursor) else {
                return Err(parse_err(
                    source,
                    index,
                    "expected SET (option = value)",
                    text,
                ));
            };
            actions.extend(pairs.into_iter().map(|(k, v)| ViewAction::SetOption(k, v)));
        } else if cursor.eat_word("RESET") {
            if !cursor.at_lparen() {
                return Err(parse_err(source, index, "expected RESET (option)", text));
            }
            let inner = cursor.capture_paren_group().unwrap_or_default();
            for opt in inner.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                actions.push(ViewAction::DropOption(opt.to_ascii_uppercase()));
            }
        } else {
            return Err(Error::unsupported(
                source,
                index,
                "this ALTER VIEW action is not part of pgdelta's catalog model",
                text,
            ));
        }
        if cursor.eat_comma() {
            continue;
        }
        break;
    }
    if !cursor.is_empty() {
        return Err(Error::unsupported(
            source,
            index,
            "unsupported trailing ALTER VIEW option",
            text,
        ));
    }
    Ok(ParsedStatement::AlterView { name, actions })
}

// ---------------------------------------------------------------------------
// CREATE FUNCTION / CREATE PROCEDURE
// ---------------------------------------------------------------------------

/// Parses `CREATE [OR REPLACE] FUNCTION|PROCEDURE name(args) ...`.
pub(crate) fn parse_create_routine(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedStatement> {
    let or_replace = cursor.eat_words2("OR", "REPLACE");
    let kind = if cursor.eat_word("FUNCTION") {
        RoutineKind::Function
    } else if cursor.eat_word("PROCEDURE") {
        RoutineKind::Procedure
    } else {
        return Err(parse_err(
            source,
            index,
            "expected FUNCTION or PROCEDURE after CREATE",
            text,
        ));
    };
    let Some(mut name) = cursor.parse_object_name() else {
        return Err(parse_err(source, index, "expected a routine name", text));
    };
    truncate_parts(&mut name);
    let (schema, routine_name) = resolve_name(&name);
    let args = parse_routine_args(source, index, cursor, text)?;
    let mut return_type = None;
    let mut returns_setof = false;
    let mut return_table = Vec::new();
    if cursor.eat_words2("RETURNS", "TABLE") {
        return_table = parse_return_table(source, index, cursor, text)?;
    } else if cursor.eat_word("RETURNS") {
        returns_setof = cursor.eat_word("SETOF");
        return_type = Some(take_type(source, index, cursor, ROUTINE_BOUNDARIES, text)?);
    }

    let mut language = String::new();
    let mut body = String::new();
    let mut volatility = Volatility::Volatile;
    let mut security = Security::Invoker;
    let mut strict = false;
    let mut leakproof = false;
    let mut window = false;
    let mut parallel = None;
    let mut cost = None;
    let mut rows = None;
    let mut settings = Vec::new();

    while !cursor.is_empty() {
        if cursor.eat_word("LANGUAGE") {
            language = cursor
                .parse_ident()
                .ok_or_else(|| parse_err(source, index, "expected a language name", text))?
                .to_ascii_lowercase();
        } else if cursor.eat_word("IMMUTABLE") {
            volatility = Volatility::Immutable;
        } else if cursor.eat_word("STABLE") {
            volatility = Volatility::Stable;
        } else if cursor.eat_word("VOLATILE") {
            volatility = Volatility::Volatile;
        } else if cursor.eat_word("STRICT")
            || (cursor.eat_words2("RETURNS", "NULL")
                && cursor.eat_words2("ON", "NULL")
                && cursor.eat_word("INPUT"))
        {
            strict = true;
        } else if cursor.eat_words2("CALLED", "ON")
            && cursor.eat_word("NULL")
            && cursor.eat_word("INPUT")
        {
            strict = false;
        } else if cursor.eat_word("SECURITY") {
            if cursor.eat_word("DEFINER") {
                security = Security::Definer;
            } else {
                cursor.eat_word("INVOKER");
                security = Security::Invoker;
            }
        } else if cursor.eat_word("LEAKPROOF") {
            leakproof = true;
        } else if cursor.eat_word("WINDOW") {
            window = true;
        } else if cursor.eat_word("PARALLEL") {
            parallel = cursor.parse_ident().map(|s| s.to_ascii_uppercase());
        } else if cursor.eat_word("COST") {
            cost = eat_i64(cursor).map(|v| v.max(0) as u64);
        } else if cursor.eat_word("ROWS") {
            rows = eat_i64(cursor).map(|v| v.max(0) as u64);
        } else if cursor.eat_word("SET") {
            let name = cursor.parse_object_name().map_or_default(|p| p.join("."));
            cursor.eat_word("TO");
            cursor.eat_equals();
            let value = cursor.capture_until_words(ROUTINE_BOUNDARIES);
            if value.trim().is_empty() {
                return Err(parse_err(source, index, "expected a SET value", text));
            }
            settings.push((name, value.trim().to_owned()));
        } else if cursor.eat_word("AS") {
            body = take_routine_body(cursor).ok_or_else(|| {
                parse_err(
                    source,
                    index,
                    "expected a quoted routine body after AS",
                    text,
                )
            })?;
        } else if cursor.at_word("RETURN") || cursor.at_word("BEGIN") {
            // SQL-standard body: capture the remaining tokens verbatim.
            body = cursor.capture_rest();
            break;
        } else if cursor.eat_word("EXTERNAL") {
            // Noise word before SECURITY.
        } else if cursor.eat_word("TRANSFORM") {
            return Err(Error::unsupported(
                source,
                index,
                "TRANSFORMS are not part of pgdelta's routine model",
                text,
            ));
        } else if cursor.eat_word("SUPPORT") {
            return Err(Error::unsupported(
                source,
                index,
                "planner SUPPORT functions are not part of pgdelta's routine model",
                text,
            ));
        } else {
            return Err(Error::unsupported(
                source,
                index,
                "unsupported CREATE FUNCTION option",
                text,
            ));
        }
    }
    if language.is_empty() {
        return Err(Error::invalid(
            source,
            index,
            "a routine must declare LANGUAGE (or use a SQL-standard body, which implies SQL)",
            text,
        ));
    }
    if body.trim().is_empty() {
        return Err(Error::invalid(
            source,
            index,
            "a routine needs a body",
            text,
        ));
    }
    if kind == RoutineKind::Procedure && return_type.is_some() {
        return Err(Error::invalid(
            source,
            index,
            "procedures do not have a return type (use CALL, not SELECT)",
            text,
        ));
    }
    Ok(ParsedStatement::CreateRoutine {
        routine: Routine {
            schema,
            name: routine_name,
            kind,
            args,
            return_type,
            returns_setof,
            return_table,
            language,
            body,
            volatility,
            security,
            strict,
            leakproof,
            window,
            parallel,
            cost,
            rows,
            settings,
        },
        or_replace,
    })
}

const ROUTINE_BOUNDARIES: &[&str] = &[
    "LANGUAGE",
    "IMMUTABLE",
    "STABLE",
    "VOLATILE",
    "STRICT",
    "SECURITY",
    "LEAKPROOF",
    "WINDOW",
    "PARALLEL",
    "COST",
    "ROWS",
    "SUPPORT",
    "SET",
    "AS",
    "RETURN",
    "BEGIN",
    "EXTERNAL",
    "TRANSFORM",
];

fn parse_routine_args(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<Vec<RoutineArg>> {
    if !cursor.at_lparen() {
        return Err(parse_err(
            source,
            index,
            "expected '(' after routine name",
            text,
        ));
    }
    let inner = cursor
        .capture_paren_group()
        .ok_or_else(|| parse_err(source, index, "unbalanced argument list", text))?;
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    let toks = lex(&inner, source)?;
    let mut args = Vec::new();
    for item in split_top_level(&toks, &Token::Comma) {
        if item.is_empty() {
            continue;
        }
        args.push(parse_one_arg(source, index, &item, text)?);
    }
    Ok(args)
}

fn parse_one_arg(source: &str, index: usize, toks: &[Token], text: &str) -> Result<RoutineArg> {
    let mut pos = 0;
    let mode = if is_word_at(toks, pos, "IN") {
        pos += 1;
        ArgMode::In
    } else if is_word_at(toks, pos, "OUT") {
        pos += 1;
        ArgMode::Out
    } else if is_word_at(toks, pos, "INOUT") {
        pos += 1;
        ArgMode::InOut
    } else if is_word_at(toks, pos, "VARIADIC") {
        pos += 1;
        ArgMode::Variadic
    } else {
        ArgMode::In
    };
    // An optional argument name is a word followed by a *type* token. A
    // bare one-word item, or a multi-word built-in type (`character
    // varying`, `timestamp with time zone`), has no argument name.
    let mut name = None;
    if matches!(toks.get(pos), Some(Token::Word(w)) if !is_type_word(w))
        && matches!(toks.get(pos + 1), Some(Token::Word(_)))
    {
        name = Some(crate::naming::fold_ident(
            word_value(&toks[pos]),
            word_quoted(&toks[pos]),
        ));
        pos += 1;
    }
    let mut default_at = None;
    let mut depth = 0i32;
    for (i, tok) in toks.iter().enumerate().skip(pos) {
        match tok {
            Token::LParen => depth += 1,
            Token::RParen => depth -= 1,
            Token::Word(w) if depth == 0 && w.value.eq_ignore_ascii_case("DEFAULT") => {
                default_at = Some(i);
                break;
            }
            Token::Eq if depth == 0 => {
                default_at = Some(i);
                break;
            }
            _ => {}
        }
    }
    let type_toks = &toks[pos..default_at.unwrap_or(toks.len())];
    let type_text = tokens::join_tokens(type_toks);
    let data_type = super::datatype::normalize_type_text(&type_text).map_err(|e| {
        parse_err(
            source,
            index,
            &format!("invalid argument type {type_text:?}: {e}"),
            text,
        )
    })?;
    let default = default_at.map(|i| {
        let raw = tokens::join_tokens(&toks[i + 1..]);
        expr::normalize_expr_text(&raw).unwrap_or(raw)
    });
    Ok(RoutineArg {
        mode,
        name,
        data_type,
        default,
    })
}

fn parse_return_table(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<Vec<(String, String)>> {
    let inner = cursor
        .capture_paren_group()
        .ok_or_else(|| parse_err(source, index, "expected (...) after RETURNS TABLE", text))?;
    let toks = lex(&inner, source)?;
    let mut out = Vec::new();
    for item in split_top_level(&toks, &Token::Comma) {
        if item.len() < 2 {
            return Err(parse_err(
                source,
                index,
                "expected column name and type in RETURNS TABLE",
                text,
            ));
        }
        let name = crate::naming::fold_ident(word_value(&item[0]), word_quoted(&item[0]));
        let data_type = super::datatype::normalize_type_text(&tokens::join_tokens(&item[1..]))
            .map_err(|e| {
                parse_err(
                    source,
                    index,
                    &format!("invalid RETURNS TABLE type: {e}"),
                    text,
                )
            })?;
        out.push((name, data_type));
    }
    Ok(out)
}

fn take_routine_body(cursor: &mut Cursor) -> Option<String> {
    match cursor.peek()? {
        Token::SingleQuotedString(s) => {
            let out = s.clone();
            cursor.advance(1);
            Some(out)
        }
        Token::DollarQuotedString(DollarQuotedString { tag, value }) => {
            let out = value.clone();
            let tag = tag.clone().unwrap_or_default();
            cursor.advance(1);
            Some(if tag.is_empty() {
                format!("$${out}$$")
            } else {
                format!("${tag}${out}${tag}$")
            })
        }
        _ => None,
    }
}

/// True when a word starts a built-in PostgreSQL type name (so it cannot
/// be an argument name in a routine signature).
fn is_type_word(word: &sqlparser::tokenizer::Word) -> bool {
    if word.quote_style.is_some() {
        return false;
    }
    matches!(
        word.value.to_ascii_lowercase().as_str(),
        "integer"
            | "int"
            | "int2"
            | "int4"
            | "int8"
            | "smallint"
            | "bigint"
            | "boolean"
            | "bool"
            | "text"
            | "varchar"
            | "character"
            | "char"
            | "numeric"
            | "decimal"
            | "real"
            | "float"
            | "float4"
            | "float8"
            | "double"
            | "precision"
            | "date"
            | "time"
            | "timestamp"
            | "interval"
            | "bytea"
            | "json"
            | "jsonb"
            | "uuid"
            | "serial"
            | "bigserial"
            | "smallserial"
            | "bit"
            | "varbit"
            | "trigger"
            | "record"
            | "void"
            | "anyelement"
            | "anyarray"
            | "cstring"
            | "internal"
            | "with"
            | "without"
            | "varying"
            | "zone"
            | "setof"
    )
}

fn is_word_at(toks: &[Token], i: usize, word: &str) -> bool {
    matches!(toks.get(i), Some(Token::Word(w)) if w.quote_style.is_none() && w.value.eq_ignore_ascii_case(word))
}

fn word_value(tok: &Token) -> &str {
    match tok {
        Token::Word(w) => &w.value,
        _ => "",
    }
}

fn word_quoted(tok: &Token) -> bool {
    matches!(tok, Token::Word(w) if w.quote_style.is_some())
}

fn split_top_level(toks: &[Token], separator: &Token) -> Vec<Vec<Token>> {
    let mut out = Vec::new();
    let mut current = Vec::new();
    let mut depth = 0i32;
    for tok in toks {
        match tok {
            Token::LParen => depth += 1,
            Token::RParen => depth -= 1,
            _ if depth == 0 && tok == separator => {
                out.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(tok.clone());
    }
    out.push(current);
    out
}

/// Parses `DROP FUNCTION|PROCEDURE [IF EXISTS] name(types) [, ...]`.
pub(crate) fn parse_drop_routine(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedStatement> {
    let if_exists = cursor.eat_words2("IF", "EXISTS");
    let mut names = Vec::new();
    loop {
        let Some(mut parts) = cursor.parse_object_name() else {
            return Err(parse_err(source, index, "expected a routine name", text));
        };
        truncate_parts(&mut parts);
        let raw = cursor.capture_paren_group().unwrap_or_default();
        let mut types = Vec::new();
        if !raw.trim().is_empty() {
            let toks = lex(&raw, source)?;
            for item in split_top_level(&toks, &Token::Comma) {
                let text_item = tokens::join_tokens(&item);
                if text_item.trim().is_empty() {
                    continue;
                }
                types.push(
                    super::datatype::normalize_type_text(&text_item).map_err(|e| {
                        parse_err(
                            source,
                            index,
                            &format!("invalid routine signature type: {e}"),
                            text,
                        )
                    })?,
                );
            }
        }
        names.push((parts, types));
        if !cursor.eat_comma() {
            break;
        }
    }
    if cursor.eat_word("CASCADE") {
        return Err(Error::unsupported(
            source,
            index,
            "DROP ROUTINE ... CASCADE drops dependent objects implicitly, which pgdelta will \
             never do silently",
            text,
        ));
    }
    cursor.eat_word("RESTRICT");
    Ok(ParsedStatement::DropRoutine { names, if_exists })
}

/// Parses `ALTER FUNCTION|PROCEDURE name(types) action [, ...]`.
pub(crate) fn parse_alter_routine(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedStatement> {
    let Some(mut parts) = cursor.parse_object_name() else {
        return Err(parse_err(source, index, "expected a routine name", text));
    };
    truncate_parts(&mut parts);
    let raw = cursor.capture_paren_group().unwrap_or_default();
    let mut types = Vec::new();
    if !raw.trim().is_empty() {
        let toks = lex(&raw, source)?;
        for item in split_top_level(&toks, &Token::Comma) {
            let item_text = tokens::join_tokens(&item);
            if !item_text.trim().is_empty() {
                types.push(
                    super::datatype::normalize_type_text(&item_text).map_err(|e| {
                        parse_err(source, index, &format!("invalid signature type: {e}"), text)
                    })?,
                );
            }
        }
    }
    let mut actions = Vec::new();
    loop {
        if cursor.eat_words2("RENAME", "TO") {
            let to = cursor
                .parse_ident()
                .ok_or_else(|| parse_err(source, index, "expected a new routine name", text))?;
            actions.push(RoutineAction::Rename(crate::naming::truncate_ident(&to)));
        } else if cursor.eat_words2("SET", "SCHEMA") {
            let schema = cursor
                .parse_ident()
                .ok_or_else(|| parse_err(source, index, "expected a schema name", text))?;
            actions.push(RoutineAction::SetSchema(crate::naming::truncate_ident(
                &schema,
            )));
        } else if cursor.eat_word("SET") {
            let name = cursor.parse_object_name().map_or_default(|p| p.join("."));
            cursor.eat_word("TO");
            cursor.eat_equals();
            let value = cursor.capture_until_words(&[]);
            actions.push(RoutineAction::SetOption(name, value.trim().to_owned()));
        } else if cursor.eat_word("RESET") {
            if cursor.eat_word("ALL") {
                actions.push(RoutineAction::ResetOption(String::new()));
            } else {
                let name = cursor.parse_object_name().map_or_default(|p| p.join("."));
                actions.push(RoutineAction::ResetOption(name));
            }
        } else if cursor.eat_words2("OWNER", "TO") {
            return Err(Error::unsupported(
                source,
                index,
                "routine ownership is not part of pgdelta's catalog model",
                text,
            ));
        } else {
            return Err(Error::unsupported(
                source,
                index,
                "unsupported ALTER ROUTINE action",
                text,
            ));
        }
        if cursor.eat_comma() {
            continue;
        }
        break;
    }
    if !cursor.is_empty() {
        return Err(Error::unsupported(
            source,
            index,
            "unsupported trailing ALTER ROUTINE option",
            text,
        ));
    }
    Ok(ParsedStatement::AlterRoutine {
        name: (parts, types),
        actions,
    })
}

// ---------------------------------------------------------------------------
// TRIGGERS
// ---------------------------------------------------------------------------

/// Parses `CREATE [OR REPLACE] [CONSTRAINT] TRIGGER name ...`.
pub(crate) fn parse_create_trigger(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedStatement> {
    let or_replace = cursor.eat_words2("OR", "REPLACE");
    let constraint = cursor.eat_word("CONSTRAINT");
    if !cursor.eat_word("TRIGGER") {
        return Err(parse_err(
            source,
            index,
            "expected TRIGGER after CREATE",
            text,
        ));
    }
    let name = cursor
        .parse_ident()
        .ok_or_else(|| parse_err(source, index, "expected a trigger name", text))?;
    let timing = if cursor.eat_word("BEFORE") {
        TriggerTiming::Before
    } else if cursor.eat_words2("INSTEAD", "OF") {
        TriggerTiming::InsteadOf
    } else if cursor.eat_word("AFTER") {
        TriggerTiming::After
    } else {
        return Err(parse_err(
            source,
            index,
            "expected BEFORE, AFTER or INSTEAD OF",
            text,
        ));
    };
    let mut events = Vec::new();
    loop {
        if cursor.eat_word("INSERT") {
            events.push(TriggerEvent::Insert);
        } else if cursor.eat_word("UPDATE") {
            let mut cols = Vec::new();
            if cursor.eat_word("OF") {
                loop {
                    let Some(col) = cursor.parse_ident() else {
                        return Err(parse_err(
                            source,
                            index,
                            "expected a column after UPDATE OF",
                            text,
                        ));
                    };
                    cols.push(col);
                    if !cursor.eat_comma() {
                        break;
                    }
                }
            }
            events.push(TriggerEvent::Update(cols));
        } else if cursor.eat_word("DELETE") {
            events.push(TriggerEvent::Delete);
        } else if cursor.eat_word("TRUNCATE") {
            events.push(TriggerEvent::Truncate);
        } else {
            return Err(parse_err(source, index, "expected a trigger event", text));
        }
        if !cursor.eat_word("OR") {
            break;
        }
    }
    if !cursor.eat_word("ON") {
        return Err(parse_err(
            source,
            index,
            "expected ON after trigger event",
            text,
        ));
    }
    let Some(mut table) = cursor.parse_object_name() else {
        return Err(parse_err(
            source,
            index,
            "expected a table name after ON",
            text,
        ));
    };
    truncate_parts(&mut table);
    let (table_schema, table_name) = resolve_name(&table);

    let deferrable = if cursor.eat_words2("NOT", "DEFERRABLE") {
        None
    } else if cursor.eat_word("DEFERRABLE") {
        let initially_deferred = cursor.eat_words2("INITIALLY", "DEFERRED");
        if !initially_deferred {
            cursor.eat_words2("INITIALLY", "IMMEDIATE");
        }
        Some(crate::model::Deferrable {
            deferrable: true,
            initially_deferred,
        })
    } else {
        None
    };
    // `FROM referenced_table` and `REFERENCING ...` are constraint/transition
    // details; they configure trigger behaviour beyond pgdelta's model and
    // are explicitly rejected rather than silently forgotten.
    if cursor.eat_word("FROM") || cursor.eat_word("REFERENCING") {
        return Err(Error::unsupported(
            source,
            index,
            "trigger transition relations and FROM referenced_table are not part of pgdelta's \
             trigger model",
            text,
        ));
    }
    let level = if cursor.eat_words2("FOR", "EACH") {
        if cursor.eat_word("ROW") {
            TriggerLevel::Row
        } else if cursor.eat_word("STATEMENT") {
            TriggerLevel::Statement
        } else {
            return Err(parse_err(
                source,
                index,
                "expected ROW or STATEMENT after FOR EACH",
                text,
            ));
        }
    } else {
        TriggerLevel::Statement
    };
    let condition = if cursor.eat_word("WHEN") {
        let raw = cursor.capture_paren_group().ok_or_else(|| {
            parse_err(
                source,
                index,
                "expected a parenthesised WHEN condition",
                text,
            )
        })?;
        Some(expr::normalize_expr_text(&raw).map_err(|e| {
            parse_err(
                source,
                index,
                &format!("invalid WHEN expression: {e}"),
                text,
            )
        })?)
    } else {
        None
    };
    if !cursor.eat_word("EXECUTE") {
        return Err(parse_err(source, index, "expected EXECUTE FUNCTION", text));
    }
    cursor.eat_word("FUNCTION");
    cursor.eat_word("PROCEDURE");
    let Some(mut func) = cursor.parse_object_name() else {
        return Err(parse_err(
            source,
            index,
            "expected a trigger function name",
            text,
        ));
    };
    truncate_parts(&mut func);
    let (func_schema, func_name) = resolve_name(&func);
    let function_args = if cursor.at_lparen() {
        let inner = cursor.capture_paren_group().unwrap_or_default();
        let toks = lex(&inner, source)?;
        toks.iter()
            .filter(|t| !matches!(t, Token::Comma))
            .map(|t| match t {
                Token::SingleQuotedString(s) => s.clone(),
                Token::Number(n, _) => n.clone(),
                Token::Word(w) => w.value.clone(),
                _ => t.to_string(),
            })
            .collect()
    } else {
        Vec::new()
    };
    if !cursor.is_empty() {
        return Err(Error::unsupported(
            source,
            index,
            "unsupported trailing TRIGGER option",
            text,
        ));
    }
    if constraint && (timing != TriggerTiming::After || level != TriggerLevel::Row) {
        return Err(Error::invalid(
            source,
            index,
            "constraint triggers must be AFTER ... FOR EACH ROW (CREATE TRIGGER docs)",
            text,
        ));
    }
    if timing == TriggerTiming::InsteadOf && level != TriggerLevel::Row {
        return Err(Error::invalid(
            source,
            index,
            "INSTEAD OF triggers must be FOR EACH ROW (CREATE TRIGGER docs)",
            text,
        ));
    }
    Ok(ParsedStatement::CreateTrigger {
        trigger: Trigger {
            schema: table_schema,
            table: table_name,
            name: crate::naming::truncate_ident(&name),
            timing,
            events,
            level,
            function: (func_schema, func_name),
            function_args,
            condition,
            constraint,
            deferrable,
            enabled: true,
        },
        or_replace,
    })
}

/// Parses `DROP TRIGGER [IF EXISTS] name ON table`.
pub(crate) fn parse_drop_trigger(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedStatement> {
    let if_exists = cursor.eat_words2("IF", "EXISTS");
    let name = cursor
        .parse_ident()
        .ok_or_else(|| parse_err(source, index, "expected a trigger name", text))?;
    if !cursor.eat_word("ON") {
        return Err(parse_err(
            source,
            index,
            "expected ON after trigger name",
            text,
        ));
    }
    let Some(mut table) = cursor.parse_object_name() else {
        return Err(parse_err(source, index, "expected a table name", text));
    };
    truncate_parts(&mut table);
    if cursor.eat_word("CASCADE") {
        return Err(Error::unsupported(
            source,
            index,
            "DROP TRIGGER ... CASCADE is not needed in a declarative model",
            text,
        ));
    }
    cursor.eat_word("RESTRICT");
    Ok(ParsedStatement::DropTrigger {
        name,
        table,
        if_exists,
    })
}

/// Parses `ALTER TRIGGER name ON table RENAME TO|ENABLE|DISABLE`.
pub(crate) fn parse_alter_trigger(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedStatement> {
    let name = cursor
        .parse_ident()
        .ok_or_else(|| parse_err(source, index, "expected a trigger name", text))?;
    if !cursor.eat_word("ON") {
        return Err(parse_err(
            source,
            index,
            "expected ON after trigger name",
            text,
        ));
    }
    let Some(mut table) = cursor.parse_object_name() else {
        return Err(parse_err(source, index, "expected a table name", text));
    };
    truncate_parts(&mut table);
    let rename_to = if cursor.eat_words2("RENAME", "TO") {
        Some(crate::naming::truncate_ident(
            &cursor
                .parse_ident()
                .ok_or_else(|| parse_err(source, index, "expected a new name", text))?,
        ))
    } else {
        None
    };
    let enabled = if cursor.eat_word("ENABLE") {
        if cursor.eat_word("ALWAYS") {
            Some(TriggerEnabled::EnableAlways)
        } else if cursor.eat_word("REPLICA") {
            Some(TriggerEnabled::EnableReplica)
        } else {
            Some(TriggerEnabled::Enable)
        }
    } else if cursor.eat_word("DISABLE") {
        Some(TriggerEnabled::Disable)
    } else {
        None
    };
    if rename_to.is_none() && enabled.is_none() {
        return Err(Error::unsupported(
            source,
            index,
            "unsupported ALTER TRIGGER action",
            text,
        ));
    }
    if !cursor.is_empty() {
        return Err(Error::unsupported(
            source,
            index,
            "unsupported trailing ALTER TRIGGER option",
            text,
        ));
    }
    Ok(ParsedStatement::AlterTrigger {
        name,
        table,
        rename_to,
        enabled,
    })
}

// ---------------------------------------------------------------------------
// POLICIES
// ---------------------------------------------------------------------------

/// Parses `CREATE POLICY name ON table [AS ...] [FOR ...] [TO ...] ...`.
pub(crate) fn parse_create_policy(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedStatement> {
    if !cursor.eat_word("POLICY") {
        return Err(parse_err(
            source,
            index,
            "expected POLICY after CREATE",
            text,
        ));
    }
    let name = cursor
        .parse_ident()
        .ok_or_else(|| parse_err(source, index, "expected a policy name", text))?;
    if !cursor.eat_word("ON") {
        return Err(parse_err(
            source,
            index,
            "expected ON after policy name",
            text,
        ));
    }
    let Some(mut table) = cursor.parse_object_name() else {
        return Err(parse_err(source, index, "expected a table name", text));
    };
    truncate_parts(&mut table);
    let (schema, table_name) = resolve_name(&table);
    let mut permissive = true;
    if cursor.eat_word("AS") {
        if cursor.eat_word("RESTRICTIVE") {
            permissive = false;
        } else if !cursor.eat_word("PERMISSIVE") {
            return Err(parse_err(
                source,
                index,
                "expected PERMISSIVE or RESTRICTIVE",
                text,
            ));
        }
    }
    let command = if cursor.eat_word("FOR") {
        parse_policy_command(source, index, cursor, text)?
    } else {
        PolicyCommand::All
    };
    let roles = if cursor.eat_word("TO") {
        parse_roles(source, index, cursor, text)?
    } else {
        vec!["PUBLIC".to_owned()]
    };
    let mut using = None;
    let mut check = None;
    loop {
        if cursor.eat_word("USING") {
            using = Some(parse_policy_expr(source, index, cursor, text, "USING")?);
        } else if cursor.eat_words2("WITH", "CHECK") {
            check = Some(parse_policy_expr(
                source,
                index,
                cursor,
                text,
                "WITH CHECK",
            )?);
        } else {
            break;
        }
    }
    if !cursor.is_empty() {
        return Err(Error::unsupported(
            source,
            index,
            "unsupported trailing CREATE POLICY option",
            text,
        ));
    }
    if command == PolicyCommand::Insert && using.is_some() {
        return Err(Error::invalid(
            source,
            index,
            "an INSERT policy cannot have a USING expression (CREATE POLICY docs)",
            text,
        ));
    }
    if command == PolicyCommand::Select && check.is_some() {
        return Err(Error::invalid(
            source,
            index,
            "a SELECT policy cannot have a WITH CHECK expression (CREATE POLICY docs)",
            text,
        ));
    }
    if command == PolicyCommand::Delete && check.is_some() {
        return Err(Error::invalid(
            source,
            index,
            "a DELETE policy cannot have a WITH CHECK expression (CREATE POLICY docs)",
            text,
        ));
    }
    Ok(ParsedStatement::CreatePolicy {
        policy: Policy {
            schema,
            table: table_name,
            name: crate::naming::truncate_ident(&name),
            permissive,
            command,
            roles,
            using,
            check,
        },
    })
}

fn parse_policy_command(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<PolicyCommand> {
    if cursor.eat_word("ALL") {
        Ok(PolicyCommand::All)
    } else if cursor.eat_word("SELECT") {
        Ok(PolicyCommand::Select)
    } else if cursor.eat_word("INSERT") {
        Ok(PolicyCommand::Insert)
    } else if cursor.eat_word("UPDATE") {
        Ok(PolicyCommand::Update)
    } else if cursor.eat_word("DELETE") {
        Ok(PolicyCommand::Delete)
    } else {
        Err(parse_err(
            source,
            index,
            "expected ALL, SELECT, INSERT, UPDATE or DELETE",
            text,
        ))
    }
}

fn parse_roles(source: &str, index: usize, cursor: &mut Cursor, text: &str) -> Result<Vec<String>> {
    let mut roles = Vec::new();
    loop {
        if cursor.eat_word("PUBLIC")
            || cursor.eat_word("CURRENT_ROLE")
            || cursor.eat_word("CURRENT_USER")
        {
            roles.push("PUBLIC".to_owned());
        } else if cursor.eat_word("SESSION_USER") {
            roles.push("SESSION_USER".to_owned());
        } else if let Some(role) = cursor.parse_ident() {
            roles.push(role);
        } else {
            return Err(parse_err(source, index, "expected a role name", text));
        }
        if !cursor.eat_comma() {
            break;
        }
    }
    roles.sort();
    roles.dedup();
    Ok(roles)
}

fn parse_policy_expr(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
    label: &str,
) -> Result<String> {
    let raw = cursor
        .capture_paren_group()
        .ok_or_else(|| parse_err(source, index, &format!("expected '(' after {label}"), text))?;
    expr::normalize_expr_text(&raw).map_err(|e| {
        parse_err(
            source,
            index,
            &format!("invalid {label} expression: {e}"),
            text,
        )
    })
}

/// Parses `DROP POLICY [IF EXISTS] name ON table`.
pub(crate) fn parse_drop_policy(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedStatement> {
    let if_exists = cursor.eat_words2("IF", "EXISTS");
    let name = cursor
        .parse_ident()
        .ok_or_else(|| parse_err(source, index, "expected a policy name", text))?;
    if !cursor.eat_word("ON") {
        return Err(parse_err(
            source,
            index,
            "expected ON after policy name",
            text,
        ));
    }
    let Some(mut table) = cursor.parse_object_name() else {
        return Err(parse_err(source, index, "expected a table name", text));
    };
    truncate_parts(&mut table);
    if !cursor.is_empty() {
        return Err(Error::unsupported(
            source,
            index,
            "unsupported trailing DROP POLICY option",
            text,
        ));
    }
    Ok(ParsedStatement::DropPolicy {
        name,
        table,
        if_exists,
    })
}

/// Parses `ALTER POLICY name ON table ...`.
pub(crate) fn parse_alter_policy(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedStatement> {
    let name = cursor
        .parse_ident()
        .ok_or_else(|| parse_err(source, index, "expected a policy name", text))?;
    if !cursor.eat_word("ON") {
        return Err(parse_err(
            source,
            index,
            "expected ON after policy name",
            text,
        ));
    }
    let Some(mut table) = cursor.parse_object_name() else {
        return Err(parse_err(source, index, "expected a table name", text));
    };
    truncate_parts(&mut table);
    let mut rename_to = None;
    let mut roles = None;
    let mut using = None;
    let mut check = None;
    let mut saw_action = false;
    loop {
        if cursor.eat_words2("RENAME", "TO") {
            rename_to = Some(
                cursor
                    .parse_ident()
                    .ok_or_else(|| parse_err(source, index, "expected a new policy name", text))?,
            );
        } else if cursor.eat_word("TO") {
            roles = Some(parse_roles(source, index, cursor, text)?);
        } else if cursor.eat_word("USING") {
            using = Some(parse_policy_expr(source, index, cursor, text, "USING")?);
        } else if cursor.eat_words2("WITH", "CHECK") {
            check = Some(parse_policy_expr(
                source,
                index,
                cursor,
                text,
                "WITH CHECK",
            )?);
        } else {
            break;
        }
        saw_action = true;
    }
    if !saw_action || !cursor.is_empty() {
        return Err(Error::unsupported(
            source,
            index,
            "unsupported ALTER POLICY action",
            text,
        ));
    }
    Ok(ParsedStatement::AlterPolicy {
        name,
        table,
        rename_to,
        roles,
        using,
        check,
    })
}

// ---------------------------------------------------------------------------
// COMMENTS
// ---------------------------------------------------------------------------

/// Parses `COMMENT ON <object> IS { 'text' | NULL }`.
pub(crate) fn parse_comment(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedStatement> {
    // The dispatcher consumed COMMENT; ON is still pending.
    if !cursor.eat_word("ON") {
        return Err(parse_err(source, index, "expected ON after COMMENT", text));
    }
    let object = parse_comment_object(source, index, cursor, text)?;
    if !cursor.eat_word("IS") {
        return Err(parse_err(
            source,
            index,
            "expected IS after COMMENT ON object",
            text,
        ));
    }
    let text_value = match take_literal_or_null(cursor) {
        Some(crate::sql::ParsedLiteral::Text(value)) if !value.is_empty() => Some(value),
        Some(_) => None,
        None => {
            return Err(parse_err(
                source,
                index,
                "expected a quoted comment or NULL",
                text,
            ));
        }
    };
    if !cursor.is_empty() {
        return Err(Error::unsupported(
            source,
            index,
            "unsupported trailing COMMENT option",
            text,
        ));
    }
    Ok(ParsedStatement::Comment {
        object,
        text: text_value,
    })
}

fn parse_comment_object(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ObjectRef> {
    if cursor.eat_word("MATERIALIZED") {
        if !cursor.eat_word("VIEW") {
            return Err(parse_err(
                source,
                index,
                "expected VIEW after MATERIALIZED",
                text,
            ));
        }
        return qualified_ref(ObjectKind::MaterializedView, cursor, text);
    }
    if cursor.eat_word("TABLE") {
        return qualified_ref(ObjectKind::Table, cursor, text);
    }
    if cursor.eat_word("VIEW") {
        return qualified_ref(ObjectKind::View, cursor, text);
    }
    if cursor.eat_word("SEQUENCE") {
        return qualified_ref(ObjectKind::Sequence, cursor, text);
    }
    if cursor.eat_word("TYPE") {
        return qualified_ref(ObjectKind::Type, cursor, text);
    }
    if cursor.eat_word("DOMAIN") {
        return qualified_ref(ObjectKind::Domain, cursor, text);
    }
    if cursor.eat_word("SCHEMA") {
        return qualified_ref(ObjectKind::Schema, cursor, text);
    }
    if cursor.eat_word("INDEX") {
        return qualified_ref(ObjectKind::Index, cursor, text);
    }
    if cursor.eat_word("COLUMN") {
        let Some(parts) = cursor.parse_object_name() else {
            return Err(parse_err(source, index, "expected relation.column", text));
        };
        let mut parts = parts;
        truncate_parts(&mut parts);
        if parts.len() != 2 {
            return Err(Error::invalid(
                source,
                index,
                "a column reference needs relation.column",
                text,
            ));
        }
        return Ok(ObjectRef::new(ObjectKind::Column, parts.join(".")));
    }
    if cursor.eat_word("CONSTRAINT") {
        let constraint = cursor
            .parse_ident()
            .ok_or_else(|| parse_err(source, index, "expected a constraint name", text))?;
        if !cursor.eat_word("ON") {
            return Err(parse_err(
                source,
                index,
                "expected ON after constraint name",
                text,
            ));
        }
        cursor.eat_word("DOMAIN");
        let Some(mut table) = cursor.parse_object_name() else {
            return Err(parse_err(
                source,
                index,
                "expected a table or domain name",
                text,
            ));
        };
        truncate_parts(&mut table);
        let (schema, name) = resolve_name(&table);
        return Ok(ObjectRef::new(
            ObjectKind::Constraint,
            format!("{schema}.{name}.{constraint}"),
        ));
    }
    let is_trigger = cursor.eat_word("TRIGGER");
    let is_policy = !is_trigger && cursor.eat_word("POLICY");
    if is_trigger || is_policy {
        let kind = if is_trigger {
            ObjectKind::Trigger
        } else {
            ObjectKind::Policy
        };
        let name = cursor
            .parse_ident()
            .ok_or_else(|| parse_err(source, index, "expected an object name", text))?;
        if !cursor.eat_word("ON") {
            return Err(parse_err(
                source,
                index,
                "expected ON after object name",
                text,
            ));
        }
        let Some(mut table) = cursor.parse_object_name() else {
            return Err(parse_err(source, index, "expected a table name", text));
        };
        truncate_parts(&mut table);
        let (schema, table_name) = resolve_name(&table);
        return Ok(ObjectRef::new(
            kind,
            format!("{schema}.{table_name}.{name}"),
        ));
    }
    let is_function = cursor.eat_word("FUNCTION");
    let is_procedure = !is_function && cursor.eat_word("PROCEDURE");
    let is_routine = !is_function && !is_procedure && cursor.eat_word("ROUTINE");
    if is_function || is_procedure || is_routine {
        let kind = if is_procedure {
            ObjectKind::Procedure
        } else {
            ObjectKind::Function
        };
        let Some(mut name) = cursor.parse_object_name() else {
            return Err(parse_err(source, index, "expected a routine name", text));
        };
        truncate_parts(&mut name);
        let (schema, routine) = resolve_name(&name);
        let mut types = Vec::new();
        if cursor.at_lparen() {
            let raw = cursor.capture_paren_group().unwrap_or_default();
            let toks = lex(&raw, source)?;
            for item in split_top_level(&toks, &Token::Comma) {
                let item_text = tokens::join_tokens(&item);
                if item_text.trim().is_empty() {
                    continue;
                }
                types.push(
                    super::datatype::normalize_type_text(&item_text).map_err(|e| {
                        parse_err(source, index, &format!("invalid signature type: {e}"), text)
                    })?,
                );
            }
        }
        return Ok(ObjectRef::new(
            kind,
            routine_ref_name(&schema, &routine, &types),
        ));
    }
    Err(Error::unsupported(
        source,
        index,
        "this COMMENT ON object kind is not part of pgdelta's catalog model",
        text,
    ))
}

fn qualified_ref(kind: ObjectKind, cursor: &mut Cursor, text: &str) -> Result<ObjectRef> {
    let Some(mut parts) = cursor.parse_object_name() else {
        return Err(parse_err(text, 0, "expected an object name", text));
    };
    truncate_parts(&mut parts);
    // Schemas are named directly; every other object resolves unqualified
    // names to `public` so that `COMMENT ON TABLE t` and `... ON public.t`
    // produce the same catalog identity (default `search_path`).
    let name = if kind == ObjectKind::Schema {
        parts.join(".")
    } else {
        let (schema, name) = resolve_name(&parts);
        format!("{schema}.{name}")
    };
    Ok(ObjectRef::new(kind, name))
}

// ---------------------------------------------------------------------------
// GRANT / REVOKE
// ---------------------------------------------------------------------------

/// Parses `GRANT ... ON ... TO ...` / `REVOKE ... ON ... FROM ...`.
pub(crate) fn parse_grant(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    revoke: bool,
    text: &str,
) -> Result<ParsedStatement> {
    let grant_option_only =
        revoke && cursor.eat_words2("GRANT", "OPTION") && cursor.eat_word("FOR");
    let mut privileges = Vec::new();
    loop {
        let word = cursor
            .parse_ident()
            .ok_or_else(|| parse_err(source, index, "expected a privilege name", text))?;
        let up = word.to_ascii_uppercase();
        if up == "ALL" {
            cursor.eat_word("PRIVILEGES");
            privileges.push("ALL".to_owned());
        } else {
            privileges.push(up);
        }
        if !cursor.eat_comma() {
            break;
        }
    }
    if cursor.at_lparen() {
        return Err(Error::unsupported(
            source,
            index,
            "column-level GRANT/REVOKE is not part of pgdelta's privilege model",
            text,
        ));
    }
    if !cursor.eat_word("ON") {
        return Err(parse_err(
            source,
            index,
            "expected ON after privilege list",
            text,
        ));
    }
    let object = parse_grant_object(source, index, cursor, text)?;
    if revoke {
        if !cursor.eat_word("FROM") {
            return Err(parse_err(
                source,
                index,
                "expected FROM after REVOKE ... ON",
                text,
            ));
        }
    } else if !cursor.eat_word("TO") {
        return Err(parse_err(
            source,
            index,
            "expected TO after GRANT ... ON",
            text,
        ));
    }
    let mut grantees = Vec::new();
    loop {
        if cursor.eat_word("PUBLIC") {
            grantees.push("PUBLIC".to_owned());
        } else if cursor.eat_word("GROUP") {
            let role = cursor
                .parse_ident()
                .ok_or_else(|| parse_err(source, index, "expected a role after GROUP", text))?;
            grantees.push(role);
        } else if let Some(role) = cursor.parse_ident() {
            grantees.push(role);
        } else {
            return Err(parse_err(source, index, "expected a grantee role", text));
        }
        if !cursor.eat_comma() {
            break;
        }
    }
    let with_grant_option = if revoke {
        grant_option_only
    } else {
        cursor.eat_words2("WITH", "GRANT") && cursor.eat_word("OPTION")
    };
    if revoke && (cursor.eat_word("CASCADE") || cursor.eat_word("RESTRICT")) {
        // Behavior on dependent grants; not part of the privilege state.
    }
    if !cursor.is_empty() {
        return Err(Error::unsupported(
            source,
            index,
            "unsupported trailing GRANT/REVOKE option",
            text,
        ));
    }
    let all_privileges = privileges.iter().any(|p| p == "ALL");
    let concrete: BTreeSet<String> = if all_privileges {
        privileges_for_kind(object.kind)
    } else {
        privileges.into_iter().collect()
    };
    let grants = grantees
        .into_iter()
        .map(|grantee| Grant {
            object: object.clone(),
            grantee,
            privileges: concrete.clone(),
            with_grant_option,
        })
        .collect();
    Ok(ParsedStatement::Grant { grants, revoke })
}

fn parse_grant_object(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ObjectRef> {
    let mut kind = ObjectKind::Table;
    if cursor.eat_word("TABLE") {
        kind = ObjectKind::Table;
    } else if cursor.eat_word("SEQUENCE") {
        kind = ObjectKind::Sequence;
    } else if cursor.eat_word("SCHEMA") {
        kind = ObjectKind::Schema;
    } else if cursor.eat_word("TYPE") {
        kind = ObjectKind::Type;
    } else if cursor.eat_word("DOMAIN") {
        kind = ObjectKind::Domain;
    } else if cursor.eat_word("MATERIALIZED") && cursor.eat_word("VIEW") {
        kind = ObjectKind::MaterializedView;
    } else if cursor.eat_word("VIEW") {
        kind = ObjectKind::View;
    } else {
        let is_function = cursor.eat_word("FUNCTION");
        let is_procedure = !is_function && cursor.eat_word("PROCEDURE");
        let is_routine = !is_function && !is_procedure && cursor.eat_word("ROUTINE");
        if is_function || is_procedure || is_routine {
            kind = if is_procedure {
                ObjectKind::Procedure
            } else {
                ObjectKind::Function
            };
        } else if cursor.eat_word("ALL") {
            return Err(Error::unsupported(
                source,
                index,
                "GRANT ... ALL TABLES/SEQUENCES/FUNCTIONS IN SCHEMA grants privileges on objects \
                 that may not exist yet; list the objects explicitly instead",
                text,
            ));
        }
    }
    let Some(mut parts) = cursor.parse_object_name() else {
        return Err(parse_err(
            source,
            index,
            "expected an object name after ON",
            text,
        ));
    };
    truncate_parts(&mut parts);
    let (schema, name) = resolve_name(&parts);
    if matches!(kind, ObjectKind::Function | ObjectKind::Procedure) && cursor.at_lparen() {
        let raw = cursor.capture_paren_group().unwrap_or_default();
        let toks = lex(&raw, source)?;
        let mut types = Vec::new();
        for item in split_top_level(&toks, &Token::Comma) {
            let item_text = tokens::join_tokens(&item);
            if item_text.trim().is_empty() {
                continue;
            }
            types.push(
                super::datatype::normalize_type_text(&item_text).map_err(|e| {
                    parse_err(source, index, &format!("invalid signature type: {e}"), text)
                })?,
            );
        }
        return Ok(ObjectRef::new(
            kind,
            routine_ref_name(&schema, &name, &types),
        ));
    }
    let name = if kind == ObjectKind::Schema {
        parts[0].clone()
    } else {
        format!("{schema}.{name}")
    };
    Ok(ObjectRef::new(kind, name))
}

fn privileges_for_kind(kind: ObjectKind) -> BTreeSet<String> {
    let list: &[&str] = match kind {
        ObjectKind::Table | ObjectKind::View | ObjectKind::MaterializedView => &[
            "SELECT",
            "INSERT",
            "UPDATE",
            "DELETE",
            "TRUNCATE",
            "REFERENCES",
            "TRIGGER",
        ],
        ObjectKind::Sequence => &["USAGE", "SELECT", "UPDATE"],
        ObjectKind::Schema => &["CREATE", "USAGE"],
        ObjectKind::Function | ObjectKind::Procedure => &["EXECUTE"],
        ObjectKind::Type | ObjectKind::Domain => &["USAGE"],
        _ => &["USAGE"],
    };
    list.iter().map(|s| (*s).to_owned()).collect()
}

// ---------------------------------------------------------------------------
// PARTITIONING
// ---------------------------------------------------------------------------

/// Parses `PARTITION BY {RANGE|LIST|HASH} (keys)` (cursor at `PARTITION`).
pub(crate) fn parse_partition_by(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<Partitioning> {
    if !cursor.eat_word("PARTITION") || !cursor.eat_word("BY") {
        return Err(parse_err(source, index, "expected PARTITION BY", text));
    }
    let strategy = if cursor.eat_word("RANGE") {
        PartitionStrategy::Range
    } else if cursor.eat_word("LIST") {
        PartitionStrategy::List
    } else if cursor.eat_word("HASH") {
        PartitionStrategy::Hash
    } else {
        return Err(parse_err(
            source,
            index,
            "expected RANGE, LIST or HASH",
            text,
        ));
    };
    let inner = cursor.capture_paren_group().ok_or_else(|| {
        parse_err(
            source,
            index,
            "expected a parenthesised partition key list",
            text,
        )
    })?;
    let toks = lex(&inner, source)?;
    let mut keys = Vec::new();
    for item in split_top_level(&toks, &Token::Comma) {
        if item.len() == 1 && matches!(item[0], Token::Word(_)) {
            keys.push(PartitionKey::Column(crate::naming::fold_ident(
                word_value(&item[0]),
                word_quoted(&item[0]),
            )));
        } else {
            let raw = tokens::join_tokens(&item);
            let expr = expr::normalize_expr_text(&raw).map_err(|e| {
                parse_err(
                    source,
                    index,
                    &format!("invalid partition expression: {e}"),
                    text,
                )
            })?;
            keys.push(PartitionKey::Expression(expr));
        }
    }
    if keys.is_empty() {
        return Err(Error::invalid(
            source,
            index,
            "a partition key list cannot be empty",
            text,
        ));
    }
    Ok(Partitioning { strategy, keys })
}

/// Parses `CREATE TABLE name PARTITION OF parent FOR VALUES ...`.
pub(crate) fn parse_create_partition(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    mut name: Vec<String>,
    text: &str,
) -> Result<ParsedStatement> {
    if !cursor.eat_word("PARTITION") || !cursor.eat_word("OF") {
        return Err(parse_err(source, index, "expected PARTITION OF", text));
    }
    let Some(mut parent) = cursor.parse_object_name() else {
        return Err(parse_err(
            source,
            index,
            "expected a parent table name",
            text,
        ));
    };
    truncate_parts(&mut parent);
    truncate_parts(&mut name);
    let (schema, part_name) = resolve_name(&name);
    let (parent_schema, parent_table) = resolve_name(&parent);
    let (bound, is_default) = parse_bound(source, index, cursor, text)?;
    if !cursor.is_empty() {
        return Err(Error::unsupported(
            source,
            index,
            "unsupported trailing partition option",
            text,
        ));
    }
    Ok(ParsedStatement::AttachPartition {
        table: vec![parent_schema, parent_table],
        partition: vec![schema, part_name],
        bound,
        is_default,
        detach: false,
    })
}

fn parse_bound(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<(String, bool)> {
    if cursor.eat_word("DEFAULT") {
        return Ok((String::new(), true));
    }
    if cursor.eat_words2("FOR", "VALUES") {
        let rest = cursor.capture_rest();
        if rest.trim().is_empty() {
            return Err(parse_err(
                source,
                index,
                "expected a partition bound after FOR VALUES",
                text,
            ));
        }
        return Ok((rest, false));
    }
    Ok((String::new(), false))
}

/// Parses `ALTER TABLE name {PARTITION BY | REMOVE PARTITIONING |
/// ATTACH PARTITION | DETACH PARTITION} ...`, called right after the table
/// name. Returns `Ok(None)` when the next action is an ordinary column or
/// constraint action handled by `alter_table.rs`.
pub(crate) fn parse_alter_partition_action(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    table: &[String],
    text: &str,
) -> Result<Option<ParsedStatement>> {
    if cursor.at_word("PARTITION")
        && matches!(cursor.peek_nth(1), Some(Token::Word(w)) if w.quote_style.is_none() && w.value.eq_ignore_ascii_case("BY"))
    {
        let partition = parse_partition_by(source, index, cursor, text)?;
        return Ok(Some(ParsedStatement::SetPartitioning {
            table: table.to_vec(),
            partition: Some(partition),
        }));
    }
    if cursor.eat_words2("REMOVE", "PARTITIONING") {
        return Ok(Some(ParsedStatement::SetPartitioning {
            table: table.to_vec(),
            partition: None,
        }));
    }
    if cursor.eat_word("ATTACH") {
        if !cursor.eat_word("PARTITION") {
            return Err(parse_err(
                source,
                index,
                "expected PARTITION after ATTACH",
                text,
            ));
        }
        let Some(mut parts) = cursor.parse_object_name() else {
            return Err(parse_err(source, index, "expected a partition name", text));
        };
        truncate_parts(&mut parts);
        let (bound, is_default) = parse_bound(source, index, cursor, text)?;
        return Ok(Some(ParsedStatement::AttachPartition {
            table: table.to_vec(),
            partition: parts,
            bound,
            is_default,
            detach: false,
        }));
    }
    if cursor.eat_word("DETACH") {
        if !cursor.eat_word("PARTITION") {
            return Err(parse_err(
                source,
                index,
                "expected PARTITION after DETACH",
                text,
            ));
        }
        let Some(mut parts) = cursor.parse_object_name() else {
            return Err(parse_err(source, index, "expected a partition name", text));
        };
        truncate_parts(&mut parts);
        cursor.eat_word("CONCURRENTLY");
        cursor.eat_word("FINALIZE");
        return Ok(Some(ParsedStatement::AttachPartition {
            table: table.to_vec(),
            partition: parts,
            bound: String::new(),
            is_default: false,
            detach: true,
        }));
    }
    Ok(None)
}
