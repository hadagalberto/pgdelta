//! SQL parsing: tokenize, split statements, and parse each into a
//! [`ParsedStatement`].
//!
//! pgdelta drives sqlparser-rs wherever it is sufficient, and its own
//! token-level grammars for the PostgreSQL-specific forms sqlparser-rs
//! cannot parse (identity `SET` options, `MATCH` clauses in `CREATE
//! TABLE`, index expressions with operator-class parameters, `DROP INDEX
//! CONCURRENTLY`, `NOT VALID` deferrability clauses, ...). The verified
//! support matrix and the decision record live in `docs/parser.md`.

pub mod alter_table;
pub mod create_table;
pub mod datatype;
pub mod expr;
pub mod index;
pub mod misc;
pub mod objects;
pub mod tokens;
pub mod types;

use crate::error::{Error, Result};
use crate::model::{Deferrable, FkAction, FkMatch, IdentityKind, SequenceOptions};
use tokens::{Cursor, StatementTokens};

/// Tokenizes and parses a whole source's SQL text.
///
/// Statement indices in errors are 1-based across the whole text. A
/// leading UTF-8 BOM is stripped: editors emit it and PostgreSQL's own
/// lexer skips non-ASCII whitespace.
pub(crate) fn parse_source(source: &str, sql_text: &str) -> Result<Vec<ParsedStatement>> {
    let sql_text = sql_text.strip_prefix('\u{feff}').unwrap_or(sql_text);
    let statements = super::sql::tokens::split_statements(sql_text)
        .map_err(|e| Error::parse(source, 0, format!("tokenization failed: {e}"), sql_text))?;
    if let Err(depth) = tokens::check_nesting(&statements) {
        return Err(Error::parse(
            source,
            1,
            format!(
                "SQL nesting depth {depth} exceeds the supported limit of {}",
                tokens::MAX_NESTING_DEPTH
            ),
            sql_text,
        ));
    }
    parse_statements(source, &statements, 1)
}

/// Raw (parsed, not yet applied) sequence options for identity columns.
#[derive(Debug, Clone, Default)]
pub(crate) struct ParsedSequenceOptions {
    /// `INCREMENT [BY]`.
    pub increment: Option<i64>,
    /// `MINVALUE n` (explicit value only; `NO MINVALUE` folds to the
    /// default, per the `CREATE SEQUENCE` documentation).
    pub minvalue: Option<i64>,
    /// `MAXVALUE n`.
    pub maxvalue: Option<i64>,
    /// `START [WITH]`.
    pub start: Option<i64>,
    /// `CACHE`.
    pub cache: Option<i64>,
    /// `CYCLE` (true) / `NO CYCLE` (false).
    pub cycle: Option<bool>,
}

/// One parsed SQL statement, ready to be applied to a catalog.
#[derive(Debug, Clone)]
pub(crate) enum ParsedStatement {
    /// `CREATE SCHEMA`.
    CreateSchema {
        /// Schema name.
        name: Vec<String>,
        /// `IF NOT EXISTS`.
        if_not_exists: bool,
    },
    /// `DROP SCHEMA`.
    ///
    /// `CASCADE` is rejected at parse time: pgdelta never drops contained
    /// objects implicitly.
    DropSchema {
        /// Schema name.
        name: Vec<String>,
        /// `IF EXISTS`.
        if_exists: bool,
    },
    /// `CREATE EXTENSION`.
    CreateExtension {
        /// Extension name.
        name: String,
        /// `IF NOT EXISTS`.
        if_not_exists: bool,
        /// `WITH SCHEMA`.
        schema: Option<String>,
    },
    /// `DROP EXTENSION`.
    DropExtension {
        /// Extension name.
        name: String,
        /// `IF EXISTS`.
        if_exists: bool,
    },
    /// `CREATE TABLE`.
    CreateTable {
        /// Table name parts (possibly schema-qualified).
        name: Vec<String>,
        /// `IF NOT EXISTS`.
        if_not_exists: bool,
        /// The table body.
        raw: RawTable,
    },
    /// `DROP TABLE`.
    ///
    /// `CASCADE` is rejected at parse time: pgdelta never drops dependent
    /// objects implicitly.
    DropTable {
        /// Table name parts, one per table.
        names: Vec<Vec<String>>,
        /// `IF EXISTS`.
        if_exists: bool,
    },
    /// `ALTER TABLE`.
    AlterTable {
        /// Table name parts.
        name: Vec<String>,
        /// `IF EXISTS`.
        if_exists: bool,
        /// Actions in statement order.
        actions: Vec<alter_table::AlterAction>,
    },
    /// `CREATE INDEX`.
    CreateIndex {
        /// The index body.
        raw: RawIndex,
    },
    /// `DROP INDEX`.
    ///
    /// `CONCURRENTLY` is accepted and ignored at parse time: the planner
    /// always emits plain (transactional) `DROP INDEX`, which takes only a
    /// brief lock; the online mode of a later milestone may flip this.
    DropIndex {
        /// Index name parts, one per index.
        names: Vec<Vec<String>>,
        /// `IF EXISTS`.
        if_exists: bool,
    },
    /// `ALTER INDEX ... RENAME TO`.
    AlterIndexRename {
        /// Index name parts.
        name: Vec<String>,
        /// New name.
        new_name: String,
    },
    /// A session-level statement that configures no catalog state and is
    /// accepted and ignored (`SET`, `RESET`, `SHOW`, `BEGIN`, `COMMIT`,
    /// `START TRANSACTION`, `END` — the policy is documented in
    /// `docs/parser.md`).
    SessionOp,

    // ---- Milestone 2 object kinds -------------------------------------
    /// `CREATE TYPE` / `CREATE DOMAIN` (a domain is a [`PgType`] variant).
    CreateType {
        /// The new type, ready for the catalog.
        pg_type: crate::model::PgType,
    },
    /// `DROP TYPE` / `DROP DOMAIN`.
    DropType {
        /// Names to drop.
        names: Vec<Vec<String>>,
        /// `IF EXISTS`.
        if_exists: bool,
    },
    /// `ALTER TYPE` / `ALTER DOMAIN`.
    AlterType {
        /// Target type name.
        name: Vec<String>,
        /// Actions in statement order.
        actions: Vec<types::TypeAction>,
        /// Whether the target is a domain.
        domain: bool,
    },
    /// `CREATE SEQUENCE`.
    CreateSequence {
        /// The new sequence, ready for the catalog.
        sequence: crate::model::Sequence,
    },
    /// `ALTER SEQUENCE`.
    AlterSequence {
        /// Target sequence name.
        name: Vec<String>,
        /// Actions in statement order.
        actions: Vec<objects::SequenceAction>,
    },
    /// `DROP SEQUENCE`.
    DropSequence {
        /// Names to drop.
        names: Vec<Vec<String>>,
        /// `IF EXISTS`.
        if_exists: bool,
    },
    /// `CREATE VIEW` / `CREATE MATERIALIZED VIEW`.
    CreateView {
        /// The new view, ready for the catalog.
        view: crate::model::View,
        /// `CREATE OR REPLACE` / no-op when the view already exists.
        or_replace: bool,
    },
    /// `DROP VIEW` / `DROP MATERIALIZED VIEW`.
    DropView {
        /// Names to drop.
        names: Vec<Vec<String>>,
        /// `IF EXISTS`.
        if_exists: bool,
        /// Materialized rather than plain.
        materialized: bool,
    },
    /// `ALTER VIEW` / `ALTER MATERIALIZED VIEW`.
    AlterView {
        /// Target view name.
        name: Vec<String>,
        /// Actions in statement order.
        actions: Vec<objects::ViewAction>,
    },
    /// `CREATE FUNCTION` / `CREATE PROCEDURE`.
    CreateRoutine {
        /// The new routine, ready for the catalog.
        routine: crate::model::Routine,
        /// `CREATE OR REPLACE`.
        or_replace: bool,
    },
    /// `DROP FUNCTION` / `DROP PROCEDURE`.
    DropRoutine {
        /// Signature references: `(name parts, input types)`.
        names: Vec<(Vec<String>, Vec<String>)>,
        /// `IF EXISTS`.
        if_exists: bool,
    },
    /// `ALTER FUNCTION` / `ALTER PROCEDURE`.
    AlterRoutine {
        /// Signature reference: `(name parts, input types)`.
        name: (Vec<String>, Vec<String>),
        /// Actions in statement order.
        actions: Vec<objects::RoutineAction>,
    },
    /// `CREATE TRIGGER`.
    CreateTrigger {
        /// The new trigger, ready for the catalog.
        trigger: crate::model::Trigger,
        /// `CREATE OR REPLACE`.
        or_replace: bool,
    },
    /// `DROP TRIGGER`.
    DropTrigger {
        /// Trigger name.
        name: String,
        /// Owning table name parts.
        table: Vec<String>,
        /// `IF EXISTS`.
        if_exists: bool,
    },
    /// `ALTER TRIGGER`.
    AlterTrigger {
        /// Trigger name.
        name: String,
        /// Owning table name parts.
        table: Vec<String>,
        /// New name when renaming, otherwise `None`.
        rename_to: Option<String>,
        /// `ENABLE` / `DISABLE` / `ENABLE ALWAYS` / `ENABLE REPLICA`.
        enabled: Option<objects::TriggerEnabled>,
    },
    /// `CREATE POLICY`.
    CreatePolicy {
        /// The new policy, ready for the catalog.
        policy: crate::model::Policy,
    },
    /// `DROP POLICY`.
    DropPolicy {
        /// Policy name.
        name: String,
        /// Owning table name parts.
        table: Vec<String>,
        /// `IF EXISTS`.
        if_exists: bool,
    },
    /// `ALTER POLICY`.
    AlterPolicy {
        /// Policy name.
        name: String,
        /// Owning table name parts.
        table: Vec<String>,
        /// `RENAME TO` target, if given.
        rename_to: Option<String>,
        /// Replacement roles (`TO ...`), if given.
        roles: Option<Vec<String>>,
        /// Replacement `USING` expression, if given.
        using: Option<String>,
        /// Replacement `WITH CHECK` expression, if given.
        check: Option<String>,
    },
    /// `ALTER TABLE ... ENABLE/DISABLE/FORCE ROW LEVEL SECURITY`.
    AlterTableRls {
        /// Table name parts.
        table: Vec<String>,
        /// `ENABLE` (true) or `DISABLE` (false), when given.
        enabled: Option<bool>,
        /// `FORCE` (true) or `NO FORCE` (false), when given.
        forced: Option<bool>,
    },
    /// `COMMENT ON ... IS { 'text' | NULL }`.
    Comment {
        /// Canonical object reference.
        object: crate::model::ObjectRef,
        /// Comment text; `None` removes the comment.
        text: Option<String>,
    },
    /// `GRANT` / `REVOKE` on catalog objects.
    Grant {
        /// Grants, one per object/privilege-set.
        grants: Vec<crate::model::Grant>,
        /// `REVOKE` rather than `GRANT`.
        revoke: bool,
    },
    /// `ALTER TABLE ... PARTITION BY` (or `REMOVE PARTITIONING`).
    SetPartitioning {
        /// Table name parts.
        table: Vec<String>,
        /// New partitioning, `None` for `REMOVE PARTITIONING`.
        partition: Option<crate::model::Partitioning>,
    },
    /// `ALTER TABLE ... ATTACH/DETACH PARTITION`.
    AttachPartition {
        /// Parent table name parts.
        table: Vec<String>,
        /// Partition name parts.
        partition: Vec<String>,
        /// Bound clause text, empty for `DEFAULT`.
        bound: String,
        /// `DEFAULT` partition.
        is_default: bool,
        /// `DETACH` rather than `ATTACH`.
        detach: bool,
    },
}

/// A parsed table body.
#[derive(Debug, Clone, Default)]
pub(crate) struct RawTable {
    /// Column definitions in declaration order.
    pub columns: Vec<RawColumn>,
    /// Table constraints, in declaration order.
    pub constraints: Vec<RawConstraint>,
    /// `PARTITION BY` clause, when the table is partitioned.
    pub partition: Option<crate::model::Partitioning>,
}

/// A parsed column definition.
#[derive(Debug, Clone)]
pub(crate) struct RawColumn {
    /// `serial`-family pseudo-type: expand into a backing sequence.
    pub serial: bool,
    /// Column name.
    pub name: String,
    /// Canonical data type.
    pub data_type: String,
    /// `NOT NULL` (explicit, or implied by `PRIMARY KEY`/identity).
    pub not_null: bool,
    /// Normalised default expression.
    pub default: Option<String>,
    /// Stored generated expression.
    pub generated: Option<String>,
    /// Identity specification (kind + raw options).
    pub identity: Option<(IdentityKind, ParsedSequenceOptions)>,
    /// Explicit collation.
    pub collation: Option<String>,
    /// Explicit TOAST storage mode (`STORAGE ...`), if any.
    pub storage: Option<crate::model::ColumnStorage>,
    /// Constraints declared inline on this column (PK/UNIQUE/CHECK/FK).
    pub inline: Vec<RawConstraint>,
}

/// A parsed constraint.
#[derive(Debug, Clone)]
pub(crate) struct RawConstraint {
    /// Explicit name, if given.
    pub name: Option<String>,
    /// The constraint body.
    pub kind: RawConstraintKind,
    /// Deferrability.
    pub deferrable: Option<Deferrable>,
    /// The column this constraint was declared on, when declared inline.
    /// Needed for auto-naming (column-level `CHECK` names use the column;
    /// see `naming.rs`).
    pub column_level: Option<String>,
}

/// The body of a parsed constraint.
#[derive(Debug, Clone)]
pub(crate) enum RawConstraintKind {
    /// `PRIMARY KEY`.
    PrimaryKey {
        /// Constrained columns.
        columns: Vec<String>,
    },
    /// `UNIQUE`.
    Unique {
        /// Constrained columns.
        columns: Vec<String>,
        /// `NULLS NOT DISTINCT`.
        nulls_distinct: bool,
    },
    /// `CHECK`.
    Check {
        /// Normalised expression.
        expr: String,
    },
    /// `EXCLUDE`: the canonical body after the keyword.
    Exclusion {
        /// Normalised `USING ... (...) [WHERE (...)]` text.
        spec: String,
    },
    /// `FOREIGN KEY` / `REFERENCES`.
    ForeignKey(RawForeignKey),
}

/// A parsed foreign key (sans the constraint name).
#[derive(Debug, Clone)]
pub(crate) struct RawForeignKey {
    /// Referencing columns.
    pub columns: Vec<String>,
    /// Referenced schema (resolved to `public` when unqualified).
    pub ref_schema: String,
    /// Referenced table.
    pub ref_table: String,
    /// Referenced columns, `None` when the clause omitted them (resolved
    /// to the referenced table's primary key at apply time).
    pub ref_columns: Option<Vec<String>>,
    /// `ON DELETE`.
    pub on_delete: FkAction,
    /// `ON UPDATE`.
    pub on_update: FkAction,
    /// `MATCH`.
    pub match_type: FkMatch,
}

/// A parsed `CREATE INDEX` body.
///
/// The four boolean flags (`unique`, `concurrent`, `if_not_exists`,
/// `nulls_distinct`) mirror one-to-one the options of the `CREATE INDEX`
/// synopsis (PostgreSQL docs, `CREATE INDEX`); grouping them into an
/// options struct would obscure that mapping.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone)]
pub(crate) struct RawIndex {
    /// Index name, `None` when omitted (auto-named at apply time).
    pub name: Option<Vec<String>>,
    /// Schema-qualified table name.
    pub table: Vec<String>,
    /// `UNIQUE`.
    pub unique: bool,
    /// `CONCURRENTLY`.
    pub concurrent: bool,
    /// `IF NOT EXISTS`.
    pub if_not_exists: bool,
    /// `USING method`.
    pub method: Option<String>,
    /// Key elements.
    pub columns: Vec<crate::model::IndexElem>,
    /// `INCLUDE` columns.
    pub include: Vec<String>,
    /// `NULLS NOT DISTINCT` (unique only).
    pub nulls_distinct: bool,
    /// `WITH` storage parameters.
    pub with: Vec<(String, String)>,
    /// `TABLESPACE`.
    pub tablespace: Option<String>,
    /// Partial-index predicate.
    pub predicate: Option<String>,
}

/// Parses all statements in `sql` (already tokenized and split).
///
/// `source` names the origin for error messages; `index_base` is the
/// 1-based index of the first statement in the slice (directory sources
/// concatenate files, so indices stay honest across the concatenation).
pub(crate) fn parse_statements(
    source: &str,
    statements: &[StatementTokens],
    index_base: usize,
) -> Result<Vec<ParsedStatement>> {
    let mut out = Vec::new();
    for (offset, stmt) in statements.iter().enumerate() {
        let index = index_base + offset;
        out.push(parse_statement(source, index, stmt)?);
    }
    Ok(out)
}

fn parse_statement(source: &str, index: usize, stmt: &StatementTokens) -> Result<ParsedStatement> {
    let text = stmt.text();
    let mut cursor = Cursor::new(&stmt.tokens);
    // First word dispatches the statement kind.
    let Some(first) = cursor.peek() else {
        return Err(Error::parse(source, index, "empty statement", &text));
    };
    let keyword = match first {
        sqlparser::tokenizer::Token::Word(w) => w.value.to_ascii_uppercase(),
        _ => {
            return Err(Error::unsupported(
                source,
                index,
                "expected a SQL keyword to start the statement",
                &text,
            ));
        }
    };
    // Consume the verb so every sub-parser starts on its own grammar.
    cursor.advance(1);
    match keyword.as_str() {
        "CREATE" => parse_create_dispatch(source, index, &mut cursor, &text),
        "DROP" => parse_drop(source, index, &mut cursor, &text),
        "ALTER" => parse_alter(source, index, &mut cursor, &text),
        // Session statements that configure nothing in the catalog.
        "RESET" => Ok(ParsedStatement::SessionOp),
        "SHOW" => Ok(ParsedStatement::SessionOp),
        "BEGIN" => Ok(ParsedStatement::SessionOp),
        "END" => Ok(ParsedStatement::SessionOp),
        "COMMENT" => objects::parse_comment(source, index, &mut cursor, &text),
        "GRANT" => objects::parse_grant(source, index, &mut cursor, false, &text),
        "REVOKE" => objects::parse_grant(source, index, &mut cursor, true, &text),
        "ROLLBACK" => Err(Error::unsupported(
            source,
            index,
            "ROLLBACK in a declarative source would undo earlier statements, and pgdelta's \
             model applies statements unconditionally; remove the ROLLBACK (see \
             docs/parser.md)",
            &text,
        )),
        _ => misc::parse_misc(source, index, stmt, &text),
    }
}

/// Dispatches `CREATE` statements: `INDEX` (with optional `UNIQUE`
/// prefix), `SCHEMA`, `EXTENSION` go to their own parsers; `TABLE` (with
/// its optional prefixes) goes to the CREATE TABLE parser; everything
/// else is rejected there with a precise message.
fn parse_create_dispatch(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedStatement> {
    use sqlparser::tokenizer::Token;
    let word_at = |n: usize| -> String {
        match cursor.peek_nth(n) {
            Some(Token::Word(w)) => w.value.to_ascii_uppercase(),
            _ => String::new(),
        }
    };
    let (second, third, fourth) = (word_at(0), word_at(1), word_at(2));
    match (second.as_str(), third.as_str(), fourth.as_str()) {
        ("INDEX", _, _) | ("UNIQUE", "INDEX", _) => {
            index::parse_create_index(source, index, cursor, text)
        }
        ("SCHEMA", _, _) => misc::parse_create_schema(source, index, cursor, text),
        ("EXTENSION", _, _) => misc::parse_create_extension(source, index, cursor, text),
        ("TYPE", _, _) => types::parse_create_type(source, index, cursor, text),
        ("DOMAIN", _, _) => types::parse_create_domain(source, index, cursor, text),
        ("SEQUENCE", _, _) => objects::parse_create_sequence(source, index, cursor, text),
        ("MATERIALIZED", "VIEW", _) => objects::parse_create_matview(source, index, cursor, text),
        ("VIEW", _, _) | ("RECURSIVE", "VIEW", _) | ("OR", "REPLACE", "VIEW") => {
            objects::parse_create_view(source, index, cursor, text)
        }
        ("FUNCTION" | "PROCEDURE", _, _) | ("OR", "REPLACE", "FUNCTION" | "PROCEDURE") => {
            objects::parse_create_routine(source, index, cursor, text)
        }
        ("TRIGGER", _, _) | ("CONSTRAINT", "TRIGGER", _) => {
            objects::parse_create_trigger(source, index, cursor, text)
        }
        ("POLICY", _, _) => objects::parse_create_policy(source, index, cursor, text),
        _ => create_table::parse_create(source, index, cursor, text),
    }
}

fn parse_drop(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedStatement> {
    let object = cursor.next();
    let object_name = match object {
        Some(sqlparser::tokenizer::Token::Word(w)) => w.value.to_ascii_uppercase(),
        _ => {
            return Err(Error::unsupported(
                source,
                index,
                "expected SCHEMA, EXTENSION, TABLE or INDEX after DROP",
                text,
            ));
        }
    };
    match object_name.as_str() {
        "SCHEMA" => misc::parse_drop_schema(source, index, cursor, text),
        "EXTENSION" => misc::parse_drop_extension(source, index, cursor, text),
        "TABLE" => misc::parse_drop_table(source, index, cursor, text),
        "INDEX" => index::parse_drop_index(source, index, cursor, text),
        "TYPE" | "DOMAIN" => types::parse_drop_type(source, index, cursor, text),
        "SEQUENCE" => objects::parse_drop_sequence(source, index, cursor, text),
        "VIEW" => objects::parse_drop_view(source, index, cursor, false, text),
        "MATERIALIZED" => objects::parse_drop_view(source, index, cursor, true, text),
        "FUNCTION" | "PROCEDURE" => objects::parse_drop_routine(source, index, cursor, text),
        "TRIGGER" => objects::parse_drop_trigger(source, index, cursor, text),
        "POLICY" => objects::parse_drop_policy(source, index, cursor, text),
        other => Err(Error::unsupported(
            source,
            index,
            format!("DROP {other} is not supported by pgdelta's catalog model"),
            text,
        )),
    }
}

fn parse_alter(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedStatement> {
    let object = cursor.next();
    let object_name = match object {
        Some(sqlparser::tokenizer::Token::Word(w)) => w.value.to_ascii_uppercase(),
        _ => {
            return Err(Error::unsupported(
                source,
                index,
                "expected TABLE or INDEX after ALTER",
                text,
            ));
        }
    };
    match object_name.as_str() {
        "TABLE" => alter_table::parse_alter_table(source, index, cursor, text),
        "INDEX" => index::parse_alter_index(source, index, cursor, text),
        "TYPE" => types::parse_alter_type(source, index, cursor, false, text),
        "DOMAIN" => types::parse_alter_type(source, index, cursor, true, text),
        "SEQUENCE" => objects::parse_alter_sequence(source, index, cursor, text),
        "VIEW" => objects::parse_alter_view(source, index, cursor, false, text),
        "MATERIALIZED" => objects::parse_alter_view(source, index, cursor, true, text),
        "FUNCTION" | "PROCEDURE" => objects::parse_alter_routine(source, index, cursor, text),
        "TRIGGER" => objects::parse_alter_trigger(source, index, cursor, text),
        "POLICY" => objects::parse_alter_policy(source, index, cursor, text),
        other => Err(Error::unsupported(
            source,
            index,
            format!("ALTER {other} is not supported by pgdelta's catalog model"),
            text,
        )),
    }
}

/// Parses a `CREATE SEQUENCE`-style option list (the parenthesised
/// `sequence_options` of an identity column).
///
/// Options follow the `CREATE SEQUENCE` synopsis (PostgreSQL docs,
/// `CREATE SEQUENCE`): `INCREMENT [BY]`, `MINVALUE`/`NO MINVALUE`,
/// `MAXVALUE`/`NO MAXVALUE`, `START [WITH]`, `CACHE`, `[NO] CYCLE`. The
/// identity-specific `SEQUENCE NAME`, `AS`, `OWNED BY`, `LOGGED` and
/// `UNLOGGED` options are rejected with a clear message: `SEQUENCE NAME`
/// and persistence are sequence-object concerns, modelled with sequences
/// in a later milestone (see `docs/parser.md`).
pub(crate) fn parse_sequence_options_paren(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<ParsedSequenceOptions> {
    let mut opts = ParsedSequenceOptions::default();
    if !cursor.eat_lparen() {
        return Ok(opts);
    }
    loop {
        if cursor.eat_words(&["INCREMENT"]) {
            cursor.eat_word("BY");
            let Some(v) = tokens::eat_i64(cursor) else {
                return Err(Error::parse(
                    source,
                    index,
                    "expected a number after INCREMENT BY",
                    text,
                ));
            };
            opts.increment = Some(v);
        } else if cursor.eat_words(&["MINVALUE"]) {
            let Some(v) = tokens::eat_i64(cursor) else {
                return Err(Error::parse(
                    source,
                    index,
                    "expected a number after MINVALUE",
                    text,
                ));
            };
            opts.minvalue = Some(v);
        } else if cursor.eat_words2("NO", "MINVALUE") {
            // NO MINVALUE selects the default, which is what `None` models.
        } else if cursor.eat_words(&["MAXVALUE"]) {
            let Some(v) = tokens::eat_i64(cursor) else {
                return Err(Error::parse(
                    source,
                    index,
                    "expected a number after MAXVALUE",
                    text,
                ));
            };
            opts.maxvalue = Some(v);
        } else if cursor.eat_words2("NO", "MAXVALUE") {
            // NO MAXVALUE selects the default.
        } else if cursor.eat_word("START") {
            cursor.eat_word("WITH");
            let Some(v) = tokens::eat_i64(cursor) else {
                return Err(Error::parse(
                    source,
                    index,
                    "expected a number after START WITH",
                    text,
                ));
            };
            opts.start = Some(v);
        } else if cursor.eat_word("CACHE") {
            let Some(v) = tokens::eat_i64(cursor) else {
                return Err(Error::parse(
                    source,
                    index,
                    "expected a number after CACHE",
                    text,
                ));
            };
            opts.cache = Some(v);
        } else if cursor.eat_word("CYCLE") {
            opts.cycle = Some(true);
        } else if cursor.eat_words2("NO", "CYCLE") {
            opts.cycle = Some(false);
        } else if cursor.at_word("SEQUENCE")
            || cursor.at_word("AS")
            || cursor.at_word("OWNED")
            || cursor.at_word("LOGGED")
            || cursor.at_word("UNLOGGED")
        {
            return Err(Error::unsupported(
                source,
                index,
                "SEQUENCE NAME / AS / OWNED BY / LOGGED / UNLOGGED identity options are not \
                 modelled in milestone 1 (they configure the backing sequence object; see \
                 docs/parser.md)",
                text,
            ));
        } else {
            return Err(Error::parse(
                source,
                index,
                "expected a sequence option (INCREMENT BY, MINVALUE, MAXVALUE, START WITH, \
                 CACHE, CYCLE)",
                text,
            ));
        }
        if cursor.eat_rparen() {
            break;
        }
        // PostgreSQL's `seqopt_list` permits options separated by whitespace
        // alone as well as by commas (CREATE SEQUENCE synopsis and grammar).
        // The canonical renderer emits the whitespace form, so both must be
        // accepted here: a comma is optional between options. The loop head
        // still rejects any token that is not a real option, so progress is
        // always made and malformed input is never silently skipped.
        let _ = cursor.eat_comma();
    }
    Ok(opts)
}

/// Normalises parsed sequence options against the identity column's data
/// type, filling PostgreSQL's documented defaults (`CREATE SEQUENCE`):
/// increment 1; ascending sequences default `MINVALUE` to 1, `MAXVALUE` to
/// the type maximum and `START` to the minvalue; descending sequences
/// default `MINVALUE` to the type minimum, `MAXVALUE` to -1 and `START` to
/// the maxvalue; `CACHE` 1; `NO CYCLE`.
pub(crate) fn normalize_sequence_options(
    source: &str,
    index: usize,
    data_type: &str,
    opts: &ParsedSequenceOptions,
    text: &str,
) -> Result<SequenceOptions> {
    let Some((type_min, type_max)) = crate::model::sequence_type_range(data_type) else {
        return Err(Error::invalid(
            source,
            index,
            format!(
                "identity columns require smallint, integer or bigint, found {data_type:?} \
                 (the identity sequence inherits the column's arithmetic type)"
            ),
            text,
        ));
    };
    let increment = opts.increment.unwrap_or(1);
    if increment == 0 {
        return Err(Error::invalid(
            source,
            index,
            "sequence INCREMENT must be nonzero",
            text,
        ));
    }
    let ascending = increment > 0;
    let minvalue = opts
        .minvalue
        .unwrap_or(if ascending { 1 } else { type_min });
    let maxvalue = opts
        .maxvalue
        .unwrap_or(if ascending { type_max } else { -1 });
    let start = opts
        .start
        .unwrap_or(if ascending { minvalue } else { maxvalue });
    Ok(SequenceOptions {
        increment,
        minvalue,
        maxvalue,
        start,
        cache: opts.cache.unwrap_or(1),
        cycle: opts.cycle.unwrap_or(false),
    })
}

/// Parses the referential-action part of a `REFERENCES` clause:
/// `[ON DELETE action] [ON UPDATE action]` where an action is `NO ACTION`,
/// `RESTRICT`, `CASCADE`, `SET NULL [(cols)]` or `SET DEFAULT [(cols)]`
/// (PostgreSQL docs, `CREATE TABLE`, "`referential_action`").
pub(crate) fn parse_fk_actions(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<(FkAction, FkAction, FkMatch)> {
    let mut on_delete = FkAction::NoAction;
    let mut on_update = FkAction::NoAction;
    let mut match_type = FkMatch::Simple;
    if cursor.eat_word("MATCH") {
        match_type = if cursor.eat_word("FULL") {
            FkMatch::Full
        } else if cursor.eat_word("SIMPLE") {
            FkMatch::Simple
        } else if cursor.eat_word("PARTIAL") {
            return Err(Error::invalid(
                source,
                index,
                "MATCH PARTIAL is not implemented by PostgreSQL",
                text,
            ));
        } else {
            return Err(Error::parse(
                source,
                index,
                "expected FULL, PARTIAL or SIMPLE after MATCH",
                text,
            ));
        };
    }
    loop {
        let (which, action) = if cursor.eat_words2("ON", "DELETE") {
            (
                &mut on_delete,
                parse_one_fk_action(source, index, cursor, text)?,
            )
        } else if cursor.eat_words2("ON", "UPDATE") {
            (
                &mut on_update,
                parse_one_fk_action(source, index, cursor, text)?,
            )
        } else {
            break;
        };
        *which = action;
    }
    Ok((on_delete, on_update, match_type))
}

fn parse_one_fk_action(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    text: &str,
) -> Result<FkAction> {
    if cursor.eat_words2("NO", "ACTION") {
        Ok(FkAction::NoAction)
    } else if cursor.eat_word("RESTRICT") {
        Ok(FkAction::Restrict)
    } else if cursor.eat_word("CASCADE") {
        Ok(FkAction::Cascade)
    } else if cursor.eat_words2("SET", "NULL") {
        Ok(FkAction::SetNull(parse_optional_fk_columns(cursor)))
    } else if cursor.eat_words2("SET", "DEFAULT") {
        Ok(FkAction::SetDefault(parse_optional_fk_columns(cursor)))
    } else {
        Err(Error::parse(
            source,
            index,
            "expected NO ACTION, RESTRICT, CASCADE, SET NULL or SET DEFAULT",
            text,
        ))
    }
}

fn parse_optional_fk_columns(cursor: &mut Cursor) -> Vec<String> {
    cursor.parse_ident_list_paren().unwrap_or_default()
}

/// Captures a canonical data type from the cursor until one of `boundary`
/// words, a top-level comma or a closing paren, then normalises it through
/// the `CREATE TABLE` lift in `datatype.rs`.
pub(crate) fn take_type(
    source: &str,
    index: usize,
    cursor: &mut Cursor,
    boundary: &[&str],
    text: &str,
) -> Result<String> {
    let raw = cursor.capture_until_words(boundary);
    if raw.trim().is_empty() {
        return Err(Error::parse(source, index, "expected a data type", text));
    }
    datatype::normalize_type_text(&raw)
        .map_err(|e| Error::parse(source, index, format!("invalid data type: {e}"), text))
}

/// Parses `[(name = value | name value), ...]`, consuming the parentheses.
/// Values that are keywords or contain spaces are captured until a comma or
/// closing paren, so `SET search_path = admin, pg_temp` style lists need
/// their own parser and are not misread here.
pub(crate) fn take_option_pairs(cursor: &mut Cursor) -> Option<Vec<(String, String)>> {
    if !cursor.eat_lparen() {
        return None;
    }
    let mut out = Vec::new();
    loop {
        let name = cursor.parse_ident()?;
        cursor.eat_equals();
        let value = cursor.capture_until_words(&[]);
        let value = value.trim().trim_start_matches('=').trim().to_owned();
        out.push((name.to_ascii_uppercase(), value));
        if cursor.eat_comma() {
            continue;
        }
        if cursor.eat_rparen() {
            break;
        }
        return None;
    }
    Some(out)
}

/// A parsed `IS { 'text' | NULL }` literal.
#[derive(Debug, Clone)]
pub(crate) enum ParsedLiteral {
    /// A single-quoted string.
    Text(String),
    /// The `NULL` keyword (which removes the comment).
    Null,
}

/// Parses a single-quoted literal or the `NULL` keyword.
pub(crate) fn take_literal_or_null(cursor: &mut Cursor) -> Option<ParsedLiteral> {
    if cursor.eat_word("NULL") {
        return Some(ParsedLiteral::Null);
    }
    tokens::eat_string_literal(cursor).map(ParsedLiteral::Text)
}

/// Truncates every part of a parsed object name to PostgreSQL's 63 bytes.
pub(crate) fn truncate_parts(parts: &mut [String]) {
    for part in parts.iter_mut() {
        *part = crate::naming::truncate_ident(part);
    }
}

/// Resolves a parsed name to `(schema, name)` with `public` as default.
pub(crate) fn resolve_name(parts: &[String]) -> (String, String) {
    match parts.len() {
        1 => (crate::model::PUBLIC_SCHEMA.to_owned(), parts[0].clone()),
        _ => (parts[0].clone(), parts[parts.len() - 1].clone()),
    }
}

/// Renders sequence options for `GENERATED ... AS IDENTITY (...)`.
pub(crate) fn sequence_options_to_sql(opts: &SequenceOptions) -> String {
    format!(
        "INCREMENT BY {} MINVALUE {} MAXVALUE {} START WITH {} CACHE {} {}",
        opts.increment,
        opts.minvalue,
        opts.maxvalue,
        opts.start,
        opts.cache,
        if opts.cycle { "CYCLE" } else { "NO CYCLE" }
    )
}
