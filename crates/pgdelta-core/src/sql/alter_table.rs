//! pgdelta's `ALTER TABLE` grammar (PostgreSQL dialect).
//!
//! Follows the synopsis in the PostgreSQL 18 documentation, `ALTER TABLE`
//! (<https://www.postgresql.org/docs/18/sql-altertable.html>). Every action
//! that mutates modelled catalog state is parsed; the parser rejects
//! storage/statistics options, trigger toggles, inheritance, partitioning
//! and ownership actions with explicit messages (they configure
//! unmodelled state; see `docs/parser.md`).

use sqlparser::tokenizer::Token;

use super::create_table::{parse_column_def, parse_deferrable, parse_table_constraint};
use super::tokens::Cursor;
use super::{
    ParsedSequenceOptions, ParsedStatement, RawColumn, RawConstraint, RawTable,
    parse_sequence_options_paren,
};
use crate::error::{Error, Result};
use crate::model::IdentityKind;

/// One parsed `ALTER TABLE` action.
#[derive(Debug, Clone)]
pub(crate) enum AlterAction {
    /// `ADD [COLUMN] [IF NOT EXISTS] column_def`.
    AddColumn {
        /// `IF NOT EXISTS`.
        if_not_exists: bool,
        /// The column.
        column: RawColumn,
    },
    /// `DROP [COLUMN] [IF EXISTS] name [RESTRICT|CASCADE]`.
    DropColumn {
        /// Column name.
        name: String,
        /// `IF EXISTS`.
        if_exists: bool,
    },
    /// `ALTER COLUMN name [SET DATA] TYPE type [COLLATE c] [USING expr]`.
    AlterColumnType {
        /// Column name.
        column: String,
        /// Canonical new type.
        data_type: String,
        /// New collation, when the clause carried one.
        collation: Option<String>,
    },
    /// `ALTER COLUMN name SET DEFAULT expr`.
    SetDefault {
        /// Column name.
        column: String,
        /// Normalised expression.
        expr: String,
    },
    /// `ALTER COLUMN name DROP DEFAULT`.
    DropDefault {
        /// Column name.
        column: String,
    },
    /// `ALTER COLUMN name SET NOT NULL`.
    SetNotNull {
        /// Column name.
        column: String,
    },
    /// `ALTER COLUMN name DROP NOT NULL`.
    DropNotNull {
        /// Column name.
        column: String,
    },
    /// `ALTER COLUMN name SET STORAGE { PLAIN | EXTERNAL | EXTENDED | MAIN |
    /// DEFAULT }` (PostgreSQL docs, `ALTER TABLE`).
    SetStorage {
        /// Column name.
        column: String,
        /// `None` is `SET STORAGE DEFAULT` (restore the type's default).
        storage: Option<crate::model::ColumnStorage>,
    },
    /// `ALTER COLUMN name ADD GENERATED {ALWAYS|BY DEFAULT} AS IDENTITY
    /// [(options)]` (PostgreSQL 10+).
    AddIdentity {
        /// Column name.
        column: String,
        /// Identity kind.
        kind: IdentityKind,
        /// Parsed sequence options.
        options: ParsedSequenceOptions,
    },
    /// `ALTER COLUMN name DROP IDENTITY [IF EXISTS]`.
    DropIdentity {
        /// Column name.
        column: String,
        /// `IF EXISTS`.
        if_exists: bool,
    },
    /// `ALTER COLUMN name SET GENERATED {ALWAYS|BY DEFAULT}`.
    SetGenerated {
        /// Column name.
        column: String,
        /// New identity kind.
        kind: IdentityKind,
    },
    /// `ALTER COLUMN name SET <sequence_option>` — a single
    /// `SeqOptElem` per the `alter_identity_column_option` production in
    /// PostgreSQL's `src/backend/parser/gram.y` (`REL_18_STABLE)`:
    /// `RESTART`, `SET SeqOptElem`, `SET GENERATED ...`.
    SetSequenceOption {
        /// Column name.
        column: String,
        /// Which option to change.
        option: SeqOption,
    },
    /// `RESTART [WITH n]` — a runtime operation on the backing sequence's
    /// current value, not a schema change; accepted and ignored
    /// (documented in `docs/parser.md`).
    Restart,
    /// `ALTER COLUMN name DROP EXPRESSION [IF EXISTS]` (PostgreSQL 17+):
    /// turns a stored generated column into a plain column.
    DropExpression {
        /// Column name.
        column: String,
        /// `IF EXISTS`.
        if_exists: bool,
    },
    /// `ALTER COLUMN name SET EXPRESSION AS (expr)` (PostgreSQL 17+):
    /// replaces the generation expression.
    SetExpression {
        /// Column name.
        column: String,
        /// Normalised new expression.
        expr: String,
    },
    /// `ADD [CONSTRAINT [name]] table_constraint [NOT VALID]`.
    AddConstraint {
        /// Explicit name, if given.
        name: Option<String>,
        /// The constraint.
        constraint: RawConstraint,
        /// `NOT VALID` (legal only for CHECK and foreign keys; PostgreSQL
        /// docs, `ALTER TABLE`, `ADD table_constraint [NOT VALID]`).
        not_valid: bool,
    },
    /// `DROP CONSTRAINT [IF EXISTS] name`.
    DropConstraint {
        /// Constraint name.
        name: String,
        /// `IF EXISTS`.
        if_exists: bool,
    },
    /// `VALIDATE CONSTRAINT name`.
    ValidateConstraint {
        /// Constraint name.
        name: String,
    },
    /// `ALTER CONSTRAINT name [NOT] DEFERRABLE [INITIALLY ...]`
    /// (foreign keys only).
    AlterConstraintDeferrable {
        /// Constraint name.
        name: String,
        /// New deferrability.
        deferrable: Option<crate::model::Deferrable>,
    },
    /// `RENAME COLUMN a TO b`.
    RenameColumn {
        /// Old name.
        from: String,
        /// New name.
        to: String,
    },
    /// `RENAME CONSTRAINT a TO b`.
    RenameConstraint {
        /// Old name.
        from: String,
        /// New name.
        to: String,
    },
    /// `RENAME TO new_name`.
    RenameTable {
        /// New table name.
        to: String,
    },
}

/// A single `SET <sequence_option>` change.
#[derive(Debug, Clone)]
pub(crate) enum SeqOption {
    /// `SET INCREMENT BY n`.
    Increment(i64),
    /// `SET MINVALUE n`.
    MinValue(i64),
    /// `SET MAXVALUE n`.
    MaxValue(i64),
    /// `SET START WITH n`.
    Start(i64),
    /// `SET CACHE n`.
    Cache(i64),
    /// `SET CYCLE` / `SET NO CYCLE`.
    Cycle(bool),
}

/// Truncates every identifier part to PostgreSQL's 63-byte limit
/// (`NAMEDATALEN - 1`; PostgreSQL docs §4.1.1: "The system uses no more
/// than NAMEDATALEN-1 bytes of an identifier; longer names can be written
/// in commands, but they will be truncated").
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

/// Parses `ALTER TABLE [IF EXISTS] [ONLY] name [*] action [, ...]`.
pub(crate) fn parse_alter_table(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedStatement> {
    let if_exists = cursor.eat_words2("IF", "EXISTS");
    // ONLY (and the trailing *) only matter for inheritance/partition
    // recursion, which milestone 1 does not model; both are no-ops on
    // plain tables and are accepted and ignored.
    cursor.eat_word("ONLY");
    let Some(mut name) = cursor.parse_object_name() else {
        return Err(Error::parse(source, index, "expected a table name", text));
    };
    cursor.eat_word("*");
    truncate_parts(&mut name);

    // Partitioning and row-level-security statements share the ALTER TABLE
    // prefix but change object-level state, so they are handled before the
    // per-column action loop.
    if let Some(stmt) =
        super::objects::parse_alter_partition_action(source, index, cursor, &name, text)?
    {
        return Ok(stmt);
    }
    if cursor.eat_words2("ENABLE", "ROW") {
        if !cursor.eat_words2("LEVEL", "SECURITY") {
            return Err(Error::parse(source, index, "expected LEVEL SECURITY", text));
        }
        return Ok(ParsedStatement::AlterTableRls {
            table: name,
            enabled: Some(true),
            forced: None,
        });
    }
    if cursor.eat_words2("DISABLE", "ROW") {
        if !cursor.eat_words2("LEVEL", "SECURITY") {
            return Err(Error::parse(source, index, "expected LEVEL SECURITY", text));
        }
        return Ok(ParsedStatement::AlterTableRls {
            table: name,
            enabled: Some(false),
            forced: None,
        });
    }
    let force = if cursor.eat_word("FORCE") {
        Some(true)
    } else if cursor.eat_words2("NO", "FORCE") {
        Some(false)
    } else {
        None
    };
    if force.is_some() {
        if !cursor.eat_words2("ROW", "LEVEL") || !cursor.eat_word("SECURITY") {
            return Err(Error::parse(
                source,
                index,
                "expected ROW LEVEL SECURITY",
                text,
            ));
        }
        return Ok(ParsedStatement::AlterTableRls {
            table: name,
            enabled: None,
            forced: force,
        });
    }

    let mut actions = Vec::new();
    loop {
        actions.push(parse_action(source, index, cursor, text)?);
        if cursor.is_empty() {
            break;
        }
        if !cursor.eat_comma() {
            return Err(Error::parse(
                source,
                index,
                "expected ',' between ALTER TABLE actions or end of statement",
                text,
            ));
        }
    }

    Ok(ParsedStatement::AlterTable {
        name,
        if_exists,
        actions,
    })
}

/// Parses a single `ALTER TABLE` action (PostgreSQL docs, `ALTER TABLE`,
/// synopsis "where action is one of").
fn parse_action(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<AlterAction> {
    if cursor.eat_word("ADD") {
        // ADD [COLUMN] [IF NOT EXISTS] column_def, or
        // ADD [CONSTRAINT [name]] table_constraint [NOT VALID].
        if cursor.eat_word("CONSTRAINT") {
            let Some(name) = cursor.parse_ident() else {
                return Err(Error::parse(
                    source,
                    index,
                    "expected a constraint name after ADD CONSTRAINT",
                    text,
                ));
            };
            return parse_add_constraint(source, index, cursor, text, Some(name));
        }
        if at_constraint_start(cursor) {
            return parse_add_constraint(source, index, cursor, text, None);
        }
        let if_not_exists = cursor.eat_words2("IF", "NOT") && cursor.eat_word("EXISTS");
        cursor.eat_word("COLUMN");
        let parsed = parse_column_def(source, index, cursor, text)?;
        return Ok(AlterAction::AddColumn {
            if_not_exists,
            column: RawColumn {
                name: parsed.name,
                data_type: parsed.data_type,
                not_null: parsed.not_null,
                default: parsed.default,
                generated: parsed.generated,
                identity: parsed.identity,
                collation: parsed.collation,
                storage: parsed.storage,
                inline: parsed.inline,
            },
        });
    }
    if cursor.eat_word("DROP") {
        if cursor.eat_word("CONSTRAINT") {
            let if_exists = cursor.eat_words2("IF", "EXISTS");
            let Some(name) = cursor.parse_ident() else {
                return Err(Error::parse(
                    source,
                    index,
                    "expected a constraint name",
                    text,
                ));
            };
            parse_drop_behavior(source, index, cursor, text, "DROP CONSTRAINT")?;
            return Ok(AlterAction::DropConstraint { name, if_exists });
        }
        let if_exists = cursor.eat_words2("IF", "EXISTS");
        if !cursor.eat_word("COLUMN") {
            return Err(Error::parse(
                source,
                index,
                "expected COLUMN after DROP in an ALTER TABLE action",
                text,
            ));
        }
        let Some(name) = cursor.parse_ident() else {
            return Err(Error::parse(
                source,
                index,
                "expected a column name after DROP COLUMN",
                text,
            ));
        };
        parse_drop_behavior(source, index, cursor, text, "DROP COLUMN")?;
        return Ok(AlterAction::DropColumn { name, if_exists });
    }
    if cursor.eat_word("ALTER") {
        return parse_alter_column(source, index, cursor, text);
    }
    if cursor.eat_word("VALIDATE") {
        if !cursor.eat_word("CONSTRAINT") {
            return Err(Error::parse(
                source,
                index,
                "expected CONSTRAINT after VALIDATE",
                text,
            ));
        }
        let Some(name) = cursor.parse_ident() else {
            return Err(Error::parse(
                source,
                index,
                "expected a constraint name",
                text,
            ));
        };
        return Ok(AlterAction::ValidateConstraint { name });
    }
    if cursor.eat_word("RENAME") {
        if cursor.eat_word("COLUMN") {
            let Some(from) = cursor.parse_ident() else {
                return Err(Error::parse(source, index, "expected a column name", text));
            };
            if !cursor.eat_word("TO") {
                return Err(Error::parse(
                    source,
                    index,
                    "expected TO in RENAME COLUMN",
                    text,
                ));
            }
            let Some(to) = cursor.parse_ident() else {
                return Err(Error::parse(
                    source,
                    index,
                    "expected a new column name",
                    text,
                ));
            };
            return Ok(AlterAction::RenameColumn { from, to });
        }
        if cursor.eat_word("CONSTRAINT") {
            let Some(from) = cursor.parse_ident() else {
                return Err(Error::parse(
                    source,
                    index,
                    "expected a constraint name",
                    text,
                ));
            };
            if !cursor.eat_word("TO") {
                return Err(Error::parse(
                    source,
                    index,
                    "expected TO in RENAME CONSTRAINT",
                    text,
                ));
            }
            let Some(to) = cursor.parse_ident() else {
                return Err(Error::parse(
                    source,
                    index,
                    "expected a new constraint name",
                    text,
                ));
            };
            return Ok(AlterAction::RenameConstraint { from, to });
        }
        if cursor.eat_word("TO") {
            let Some(to) = cursor.parse_ident() else {
                return Err(Error::parse(
                    source,
                    index,
                    "expected a new table name",
                    text,
                ));
            };
            return Ok(AlterAction::RenameTable { to });
        }
        return Err(Error::parse(
            source,
            index,
            "expected COLUMN, CONSTRAINT or TO after RENAME",
            text,
        ));
    }
    let head: Vec<String> = cursor.tokens()[cursor.pos()..]
        .iter()
        .take(3)
        .map(std::string::ToString::to_string)
        .collect();
    Err(Error::unsupported(
        source,
        index,
        format!(
            "ALTER TABLE action {:?} is not modelled in milestone 1 (storage, \
             statistics, triggers, policies, inheritance, partitioning and ownership are \
             later milestones; see docs/parser.md)",
            head.join(" ")
        ),
        text,
    ))
}

/// True when the cursor sits on the start of an unnamed table constraint
/// (`PRIMARY`, `UNIQUE`, `CHECK`, `FOREIGN`, `EXCLUDE`, table-level
/// `NOT NULL`).
fn at_constraint_start(cursor: &Cursor) -> bool {
    matches!(cursor.peek(), Some(Token::Word(w)) if w.quote_style.is_none() && matches!(
        w.value.to_ascii_uppercase().as_str(),
        "PRIMARY" | "UNIQUE" | "CHECK" | "FOREIGN" | "EXCLUDE" | "NOT"
    ))
}

fn parse_drop_behavior(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
    what: &str,
) -> Result<()> {
    if cursor.eat_word("CASCADE") {
        return Err(Error::unsupported(
            source,
            index,
            format!(
                "{what} ... CASCADE drops dependent objects implicitly, which pgdelta will \
                 never do silently; drop the dependents explicitly"
            ),
            text,
        ));
    }
    // RESTRICT is the default and always accepted.
    cursor.eat_word("RESTRICT");
    Ok(())
}

/// Parses the constraint body of `ADD [CONSTRAINT [name]] <constraint>
/// [NOT VALID]` by delegating to the `CREATE TABLE` table-constraint
/// grammar.
fn parse_add_constraint(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
    name: Option<String>,
) -> Result<AlterAction> {
    let mut raw = RawTable::default();
    parse_table_constraint(source, index, cursor, text, &mut raw, None)?;
    let mut constraint = raw
        .constraints
        .into_iter()
        .next()
        .ok_or_else(|| Error::parse(source, index, "expected a constraint body", text))?;
    constraint.name = name;

    let mut not_valid = false;
    if cursor.eat_words2("NOT", "VALID") {
        not_valid = true;
    }
    if not_valid {
        match constraint.kind {
            super::RawConstraintKind::Check { .. } | super::RawConstraintKind::ForeignKey(_) => {}
            _ => {
                return Err(Error::invalid(
                    source,
                    index,
                    "NOT VALID is only allowed for CHECK and foreign-key constraints \
                     (PostgreSQL docs, ALTER TABLE, ADD table_constraint [NOT VALID])",
                    text,
                ));
            }
        }
    }

    Ok(AlterAction::AddConstraint {
        name: constraint.name.clone(),
        constraint,
        not_valid,
    })
}

/// Parses `ALTER [COLUMN] col ...` — the column-focused actions
/// (`ALTER COLUMN` synopsis block of the PostgreSQL docs).
fn parse_alter_column(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<AlterAction> {
    if cursor.eat_word("CONSTRAINT") {
        // ALTER CONSTRAINT name [DEFERRABLE|NOT DEFERRABLE] [INITIALLY ...]
        let Some(name) = cursor.parse_ident() else {
            return Err(Error::parse(
                source,
                index,
                "expected a constraint name",
                text,
            ));
        };
        let deferrable = parse_deferrable(source, index, cursor, text)?;
        return Ok(AlterAction::AlterConstraintDeferrable { name, deferrable });
    }
    cursor.eat_word("COLUMN");
    let Some(column) = cursor.parse_ident() else {
        return Err(Error::parse(
            source,
            index,
            "expected a column name after ALTER COLUMN",
            text,
        ));
    };

    if cursor.eat_words2("SET", "DATA") {
        if !cursor.eat_word("TYPE") {
            return Err(Error::parse(
                source,
                index,
                "expected TYPE after SET DATA",
                text,
            ));
        }
        return parse_set_type(source, index, cursor, text, column);
    }
    if cursor.eat_word("TYPE") {
        return parse_set_type(source, index, cursor, text, column);
    }
    if cursor.eat_words2("SET", "DEFAULT") {
        let expr_text = cursor.capture_until_words(&[]);
        let expr = super::expr::normalize_expr_text(&expr_text).map_err(|e| {
            Error::parse(source, index, format!("bad DEFAULT expression: {e}"), text)
        })?;
        return Ok(AlterAction::SetDefault { column, expr });
    }
    if cursor.eat_words2("DROP", "DEFAULT") {
        return Ok(AlterAction::DropDefault { column });
    }
    if cursor.eat_words2("SET", "NOT") {
        if !cursor.eat_word("NULL") {
            return Err(Error::parse(
                source,
                index,
                "expected NULL after SET NOT",
                text,
            ));
        }
        return Ok(AlterAction::SetNotNull { column });
    }
    if cursor.eat_words2("DROP", "NOT") {
        if !cursor.eat_word("NULL") {
            return Err(Error::parse(
                source,
                index,
                "expected NULL after DROP NOT",
                text,
            ));
        }
        return Ok(AlterAction::DropNotNull { column });
    }
    if cursor.eat_word("ADD") {
        if !cursor.eat_word("GENERATED") {
            return Err(Error::parse(
                source,
                index,
                "expected GENERATED after ADD",
                text,
            ));
        }
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
        if !cursor.eat_word("AS") || !cursor.eat_word("IDENTITY") {
            return Err(Error::parse(
                source,
                index,
                "expected 'AS IDENTITY' after GENERATED ALWAYS/BY DEFAULT",
                text,
            ));
        }
        let options = parse_sequence_options_paren(source, index, cursor, text)?;
        return Ok(AlterAction::AddIdentity {
            column,
            kind,
            options,
        });
    }
    if cursor.eat_word("DROP") {
        if cursor.eat_word("IDENTITY") {
            let if_exists = cursor.eat_words2("IF", "EXISTS");
            return Ok(AlterAction::DropIdentity { column, if_exists });
        }
        if cursor.eat_word("EXPRESSION") {
            let if_exists = cursor.eat_words2("IF", "EXISTS");
            return Ok(AlterAction::DropExpression { column, if_exists });
        }
        return Err(Error::parse(
            source,
            index,
            "expected IDENTITY or EXPRESSION after ALTER COLUMN ... DROP",
            text,
        ));
    }
    if cursor.eat_word("SET") {
        if cursor.eat_word("STORAGE") {
            let Some(word) = cursor.parse_ident() else {
                return Err(Error::parse(
                    source,
                    index,
                    "expected PLAIN, EXTERNAL, EXTENDED, MAIN or DEFAULT after SET STORAGE",
                    text,
                ));
            };
            let storage = if word.eq_ignore_ascii_case("DEFAULT") {
                None
            } else if let Some(mode) = crate::model::ColumnStorage::from_word(&word) {
                Some(mode)
            } else {
                return Err(Error::invalid(
                    source,
                    index,
                    format!(
                        "invalid storage mode {word:?}: expected PLAIN, EXTERNAL, EXTENDED, MAIN \
                         or DEFAULT (PostgreSQL docs, ALTER TABLE, SET STORAGE)"
                    ),
                    text,
                ));
            };
            return Ok(AlterAction::SetStorage { column, storage });
        }
        if cursor.eat_word("GENERATED") {
            let kind = if cursor.eat_word("ALWAYS") {
                IdentityKind::Always
            } else if cursor.eat_words2("BY", "DEFAULT") {
                IdentityKind::ByDefault
            } else {
                return Err(Error::parse(
                    source,
                    index,
                    "expected ALWAYS or BY DEFAULT after SET GENERATED",
                    text,
                ));
            };
            return Ok(AlterAction::SetGenerated { column, kind });
        }
        // SET <sequence_option>: a single SeqOptElem per the
        // alter_identity_column_option production in PostgreSQL's gram.y.
        if cursor.eat_word("INCREMENT") {
            cursor.eat_word("BY");
            let Some(v) = super::tokens::eat_i64(cursor) else {
                return Err(Error::parse(
                    source,
                    index,
                    "expected a number after INCREMENT BY",
                    text,
                ));
            };
            return Ok(AlterAction::SetSequenceOption {
                column,
                option: SeqOption::Increment(v),
            });
        }
        if cursor.eat_word("MINVALUE") {
            let Some(v) = super::tokens::eat_i64(cursor) else {
                return Err(Error::parse(
                    source,
                    index,
                    "expected a number after MINVALUE",
                    text,
                ));
            };
            return Ok(AlterAction::SetSequenceOption {
                column,
                option: SeqOption::MinValue(v),
            });
        }
        if cursor.eat_words2("NO", "MINVALUE") {
            return Err(Error::unsupported(
                source,
                index,
                "SET NO MINVALUE on an identity column is not modelled in milestone 1: \
                 NO MINVALUE selects a type-dependent default that pgdelta must know \
                 statically; use SET MINVALUE <value> instead",
                text,
            ));
        }
        if cursor.eat_word("MAXVALUE") {
            let Some(v) = super::tokens::eat_i64(cursor) else {
                return Err(Error::parse(
                    source,
                    index,
                    "expected a number after MAXVALUE",
                    text,
                ));
            };
            return Ok(AlterAction::SetSequenceOption {
                column,
                option: SeqOption::MaxValue(v),
            });
        }
        if cursor.eat_words2("NO", "MAXVALUE") {
            return Err(Error::unsupported(
                source,
                index,
                "SET NO MAXVALUE on an identity column is not modelled in milestone 1: \
                 NO MAXVALUE selects a type-dependent default that pgdelta must know \
                 statically; use SET MAXVALUE <value> instead",
                text,
            ));
        }
        if cursor.eat_word("START") {
            cursor.eat_word("WITH");
            let Some(v) = super::tokens::eat_i64(cursor) else {
                return Err(Error::parse(
                    source,
                    index,
                    "expected a number after START WITH",
                    text,
                ));
            };
            return Ok(AlterAction::SetSequenceOption {
                column,
                option: SeqOption::Start(v),
            });
        }
        if cursor.eat_word("CACHE") {
            let Some(v) = super::tokens::eat_i64(cursor) else {
                return Err(Error::parse(
                    source,
                    index,
                    "expected a number after CACHE",
                    text,
                ));
            };
            return Ok(AlterAction::SetSequenceOption {
                column,
                option: SeqOption::Cache(v),
            });
        }
        if cursor.eat_word("CYCLE") {
            return Ok(AlterAction::SetSequenceOption {
                column,
                option: SeqOption::Cycle(true),
            });
        }
        if cursor.eat_words2("NO", "CYCLE") {
            return Ok(AlterAction::SetSequenceOption {
                column,
                option: SeqOption::Cycle(false),
            });
        }
        return Err(Error::unsupported(
            source,
            index,
            "ALTER COLUMN ... SET <option> is only modelled for identity sequence options \
             (GENERATED, INCREMENT BY, MINVALUE, MAXVALUE, START WITH, CACHE, CYCLE) and SET \
             STORAGE; SET STATISTICS/(attribute options) are not modelled",
            text,
        ));
    }
    if cursor.eat_word("RESTART") {
        // Runtime operation on the backing sequence's current value, not
        // catalog state: accepted and ignored.
        if cursor.eat_word("WITH") {
            if super::tokens::eat_i64(cursor).is_none() {
                return Err(Error::parse(
                    source,
                    index,
                    "expected a number after RESTART WITH",
                    text,
                ));
            }
        } else {
            let _ = super::tokens::eat_i64(cursor);
        }
        return Ok(AlterAction::Restart);
    }
    if cursor.eat_words2("SET", "EXPRESSION") {
        if !cursor.eat_word("AS") {
            return Err(Error::parse(
                source,
                index,
                "expected AS in SET EXPRESSION AS",
                text,
            ));
        }
        let Some(expr_text) = cursor.capture_paren_group() else {
            return Err(Error::parse(
                source,
                index,
                "expected '(expression)' after SET EXPRESSION AS",
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
        return Ok(AlterAction::SetExpression { column, expr });
    }
    Err(Error::parse(
        source,
        index,
        "expected an ALTER COLUMN action (SET DATA TYPE, TYPE, SET/DROP DEFAULT, \
         SET/DROP NOT NULL, ADD/DROP IDENTITY, SET GENERATED, SET <sequence option>, \
         RESTART, DROP/SET EXPRESSION)",
        text,
    ))
}

/// Parses the tail of `... TYPE <data_type> [COLLATE c] [USING expr]`.
/// `USING` is a one-time conversion expression (a migration detail, not
/// catalog state) and is accepted and ignored — documented in
/// `docs/parser.md`.
fn parse_set_type(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
    column: String,
) -> Result<AlterAction> {
    let type_text = cursor.capture_until_words(&["COLLATE", "USING"]);
    if type_text.trim().is_empty() {
        return Err(Error::parse(
            source,
            index,
            "expected a data type after TYPE",
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
            "changing a column to a serial pseudo-type is not supported; use GENERATED ... \
             AS IDENTITY (serial implies a sequence object; see docs/parser.md)",
            text,
        ));
    }
    let mut collation = None;
    if cursor.eat_word("COLLATE") {
        let Some(parts) = cursor.parse_object_name() else {
            return Err(Error::parse(
                source,
                index,
                "expected a collation name after COLLATE",
                text,
            ));
        };
        collation = Some(parts.join("."));
    }
    if cursor.eat_word("USING") {
        let _ = cursor.capture_until_words(&[]);
    }
    Ok(AlterAction::AlterColumnType {
        column,
        data_type,
        collation,
    })
}
