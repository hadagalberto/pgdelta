//! Migration safety analysis and the `--online` plan rewriting.
//!
//! The rules implemented here come from the official PostgreSQL
//! documentation (fetched 2026-11-20):
//!
//! * <https://www.postgresql.org/docs/current/sql-altertable.html>:
//!   "An ACCESS EXCLUSIVE lock is acquired unless explicitly noted";
//!   "Changing the type of an existing column will normally cause the entire
//!   table and its indexes to be rewritten"; "Normally, [ADD `table_constraint`]
//!   will cause a scan of the table ... But if the NOT VALID option is used,
//!   this potentially-lengthy scan is skipped ... until it is validated by
//!   using VALIDATE CONSTRAINT"; "SET NOT NULL ... is checked ... by scanning
//!   the entire table ... however, if a valid CHECK constraint exists ...
//!   which proves no NULL can exist, then the table scan is skipped";
//!   "VALIDATE CONSTRAINT ... acquires a SHARE UPDATE EXCLUSIVE lock".
//! * <https://www.postgresql.org/docs/current/sql-createindex.html>:
//!   "Normally PostgreSQL locks the table to be indexed against writes ...
//!   CONCURRENTLY ... without taking any locks that prevent concurrent
//!   inserts, updates, or deletes"; "a regular CREATE INDEX command can be
//!   performed within a transaction block, but CREATE INDEX CONCURRENTLY
//!   cannot"; "Concurrent builds for indexes on partitioned tables are
//!   currently not supported".
//! * <https://www.postgresql.org/docs/current/ddl-constraints.html>:
//!   "A not-null constraint is functionally equivalent to creating a check
//!   constraint `CHECK (column_name IS NOT NULL)`" — the basis of the online
//!   `SET NOT NULL` rewrite below.
//!
//! Analysis works on statement SQL text so the very same rules power
//! `pgdelta lint` on handwritten migrations and `--fail-on-hazard` on
//! generated plans. Names are recovered from the canonical SQL pgdelta
//! renders (always-quoted identifiers); keyword matching ignores comments
//! and string literals.
//!
//! Report assembly uses `push_str(&format!(...))` for the same reason as
//! `plan.rs`: `write!` would force the library to discard `fmt::Result`s.
#![allow(clippy::format_push_string)]

use std::collections::{BTreeMap, BTreeSet};
use std::io::IsTerminal;

use serde::Serialize;
use serde_json::Value;

use crate::diff::{AlterChange, Change};
use crate::error::Result;
use crate::model::Catalog;
use crate::plan::{Plan, PlanSection, PlanStatement};

/// Broad class of a hazard, used for summaries and colouring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HazardCategory {
    /// Irreversible data loss or removal of an integrity guarantee
    /// (drop table/schema/column/constraint, type narrowing).
    Destructive,
    /// An operation that blocks writers or scans/rewrites a whole table.
    Locking,
}

impl HazardCategory {
    /// Stable lowercase label for humans and JSON.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            HazardCategory::Destructive => "destructive",
            HazardCategory::Locking => "locking",
        }
    }
}

/// A single safety finding attached to a statement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hazard {
    /// Destructive or locking.
    pub category: HazardCategory,
    /// Stable machine-readable code (`drop-table`, `not-null-scan`, ...).
    pub code: &'static str,
    /// Human explanation, including how to make the step online-safe.
    pub message: String,
}

fn destructive(code: &'static str, message: impl Into<String>) -> Hazard {
    Hazard {
        category: HazardCategory::Destructive,
        code,
        message: message.into(),
    }
}

fn locking(code: &'static str, message: impl Into<String>) -> Hazard {
    Hazard {
        category: HazardCategory::Locking,
        code,
        message: message.into(),
    }
}

/// Whether ANSI colour should be emitted when rendering summaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorChoice {
    /// Colour only when stderr is a terminal and `NO_COLOR` is unset.
    #[default]
    Auto,
    /// Always emit ANSI colour (useful in tests and logs).
    Always,
    /// Never emit ANSI colour.
    Never,
}

impl ColorChoice {
    /// Resolves `Auto` against the current process environment.
    #[must_use]
    pub fn enabled(self) -> bool {
        match self {
            ColorChoice::Always => true,
            ColorChoice::Never => false,
            ColorChoice::Auto => {
                std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none()
            }
        }
    }
}

/// One statement handed to the analyser.
#[derive(Debug, Clone, Copy)]
pub struct StatementInput<'a> {
    /// The SQL text of a single statement.
    pub sql: &'a str,
    /// `Some(true/false)` when placement is known (generated plans);
    /// `None` to infer it from `BEGIN`/`COMMIT` (handwritten scripts).
    pub transactional: Option<bool>,
}

impl<'a> StatementInput<'a> {
    /// Builds an input with inferred transaction placement.
    #[must_use]
    pub fn sql(sql: &'a str) -> Self {
        StatementInput {
            sql,
            transactional: None,
        }
    }
}

/// Normalises SQL for keyword matching: comments are dropped, characters
/// lowercased and whitespace collapsed. Quoted identifier *contents* are
/// preserved so names can be recovered by the helpers below.
fn normalize(sql: &str) -> String {
    let chars: Vec<char> = sql.chars().collect();
    let mut out = String::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '-' if i + 1 < chars.len() && chars[i + 1] == '-' => {
                i += 2;
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if i + 1 < chars.len() && chars[i + 1] == '*' => {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i = (i + 2).min(chars.len());
                out.push(' ');
            }
            '\'' => {
                // String literals are data, not SQL keywords. Collapse them
                // to a placeholder so a comment like 'DROP TABLE x' cannot
                // produce a false hazard.
                if !out.ends_with(' ') {
                    out.push(' ');
                }
                out.push('_');
                i += 1;
                while i < chars.len() {
                    if chars[i] == '\'' {
                        i += 1;
                        if i < chars.len() && chars[i] == '\'' {
                            i += 1;
                        } else {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            c if c.is_whitespace() => {
                if !out.ends_with(' ') {
                    out.push(' ');
                }
                i += 1;
            }
            c => {
                out.push(c.to_ascii_lowercase());
                i += 1;
            }
        }
    }
    out.trim().to_owned()
}

/// Extracts the token after `keyword` from a normalised statement. The
/// token is a quoted (`"schema"."name"`) or bare identifier; trailing
/// punctuation is consumed but not returned.
fn token_after(normalized: &str, keyword: &str) -> Option<String> {
    let at = normalized.find(keyword)? + keyword.len();
    let rest = normalized[at..].trim_start();
    if rest.is_empty() {
        return None;
    }
    let mut token = String::new();
    let mut in_quotes = false;
    for c in rest.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                token.push(c);
            }
            '(' | ',' | ';' | ' ' | ')' if !in_quotes => break,
            _ => token.push(c),
        }
    }
    Some(token)
}

/// Last dotted part of a canonical identifier, with quotes removed.
fn bare_name(token: &str) -> String {
    token
        .rsplit('.')
        .next()
        .unwrap_or(token)
        .trim_matches('"')
        .to_owned()
}

/// `CHECK (` and `CHECK(` both occur: pgdelta renders the former, the SQL
/// tokeniser's canonical text the latter.
fn is_check(normalized: &str) -> bool {
    normalized.contains("check (") || normalized.contains("check(")
}

fn is_foreign_key(normalized: &str) -> bool {
    normalized.contains("foreign key")
}

/// Column of a `CHECK (col IS NOT NULL)` clause, if that is the check.
fn not_null_check_column(normalized: &str) -> Option<String> {
    let rest = normalized
        .split_once("check (")
        .or_else(|| normalized.split_once("check("))?
        .1;
    let (column, _) = rest.split_once(" is not null")?;
    Some(bare_name(column.trim()))
}

#[derive(Default)]
struct ScriptState {
    inside_transaction: bool,
    /// Constraint name -> (table, optional `IS NOT NULL` column).
    add_checks: BTreeMap<(String, String), (String, Option<String>)>,
    validated_not_null: BTreeSet<(String, String)>,
}

/// Analyses a sequence of statements, returning hazards per statement.
///
/// Context matters: a `SET NOT NULL` is safe when an earlier statement in
/// the same script added and validated a `CHECK (col IS NOT NULL)`; a
/// `CREATE INDEX CONCURRENTLY` is invalid inside an explicit transaction.
/// Both are tracked across the sequence (PostgreSQL docs cited in the
/// module header).
#[must_use]
pub fn analyze_statements(inputs: &[StatementInput<'_>]) -> Vec<Vec<Hazard>> {
    let mut state = ScriptState::default();
    let mut results = Vec::with_capacity(inputs.len());
    for input in inputs {
        let normalized = normalize(input.sql);
        let transactional = input.transactional.unwrap_or(state.inside_transaction);
        let hazards = analyze_one(&normalized, transactional, &mut state);
        if input.transactional.is_none() {
            if starts_with_word(&normalized, "begin") {
                state.inside_transaction = true;
            } else if starts_with_word(&normalized, "commit")
                || starts_with_word(&normalized, "rollback")
                || starts_with_word(&normalized, "end")
            {
                state.inside_transaction = false;
            }
        }
        results.push(hazards);
    }
    results
}

fn starts_with_word(normalized: &str, word: &str) -> bool {
    normalized == word
        || normalized
            .strip_prefix(word)
            .is_some_and(|rest| rest.starts_with(' ') || rest.starts_with(';'))
}

fn analyze_one(n: &str, transactional: bool, state: &mut ScriptState) -> Vec<Hazard> {
    let mut out = Vec::new();

    // Destructive statements: removed data or removed integrity guarantees.
    if n.contains("drop table ") || n.contains("drop table;") {
        out.push(destructive(
            "drop-table",
            "DROP TABLE removes every row in the table; the data cannot be recovered from the migration.",
        ));
    }
    if n.contains("drop schema ") || n.contains("drop schema;") {
        out.push(destructive(
            "drop-schema",
            "DROP SCHEMA removes all objects it contains (RESTRICT only refuses when dependencies are visible to PostgreSQL).",
        ));
    }
    if n.contains("drop column ") {
        out.push(destructive(
            "drop-column",
            "DROP COLUMN discards the data in that column for every row.",
        ));
    }
    if n.contains("drop constraint ") {
        out.push(destructive(
            "drop-constraint",
            "DROP CONSTRAINT removes an integrity guarantee and may drop a backing index.",
        ));
    }

    // Locking statements.
    if n.contains(" alter column ") && n.contains(" type ") {
        out.push(locking(
            "type-rewrite",
            "ALTER COLUMN ... TYPE normally rewrites the table and its indexes under ACCESS EXCLUSIVE (PostgreSQL ALTER TABLE, SET DATA TYPE).",
        ));
    }

    let adding_constraint = n.contains(" add constraint ");
    if adding_constraint && is_foreign_key(n) {
        let valid = !n.contains(" not valid");
        if valid {
            out.push(locking(
                "constraint-scan",
                "ADD FOREIGN KEY without NOT VALID scans the table under a lock that blocks updates until COMMIT (PostgreSQL ALTER TABLE, Notes).",
            ));
        }
    } else if adding_constraint && is_check(n) {
        let valid = !n.contains(" not valid");
        let table = token_after(n, "alter table ").map(|t| bare_name(&t));
        let constraint = token_after(n, "add constraint ").map(|c| bare_name(&c));
        let column = not_null_check_column(n);
        if let (Some(table), Some(constraint)) = (table.clone(), constraint.clone()) {
            state
                .add_checks
                .insert((table.clone(), constraint), (table.clone(), column.clone()));
        }
        if valid {
            if let (Some(table), Some(column)) = (table, column.clone()) {
                state.validated_not_null.insert((table, column));
            }
            out.push(locking(
                "constraint-scan",
                "ADD CHECK without NOT VALID scans the whole table under ACCESS EXCLUSIVE until COMMIT (PostgreSQL ALTER TABLE, Notes).",
            ));
        }
    } else if adding_constraint && (n.contains("primary key") || n.contains("unique")) {
        out.push(locking(
            "constraint-scan",
            "ADD PRIMARY KEY/UNIQUE builds a backing index and scans the table under ACCESS EXCLUSIVE.",
        ));
    }

    if n.contains(" validate constraint ") {
        let constraint =
            token_after(n, " validate constraint ").map_or_else(String::new, |c| bare_name(&c));
        let table = token_after(n, "alter table ").map_or_else(String::new, |t| bare_name(&t));
        if let Some((_, Some(column))) = state.add_checks.remove(&(table.clone(), constraint)) {
            state.validated_not_null.insert((table, column));
        }
    }

    if n.contains(" set not null") {
        let table = token_after(n, "alter table ").map(|t| bare_name(&t));
        let column = token_after(n, " alter column ").map(|c| bare_name(&c));
        if let (Some(table), Some(column)) = (table, column)
            && !state.validated_not_null.contains(&(table, column))
        {
            out.push(locking(
                "not-null-scan",
                "SET NOT NULL scans the whole table under ACCESS EXCLUSIVE unless a valid CHECK (col IS NOT NULL) already exists (PostgreSQL ALTER TABLE). Use --online to add and validate one first.",
            ));
        }
    }

    if n.starts_with("create index ") || n.starts_with("create unique index ") {
        let concurrent = n.contains(" concurrently ");
        if !concurrent {
            out.push(locking(
                "non-concurrent-index",
                "A standard CREATE INDEX locks out writes on the table until the build finishes (PostgreSQL CREATE INDEX, Building Indexes Concurrently). Use --online.",
            ));
        } else if transactional {
            out.push(locking(
                "concurrent-index-in-transaction",
                "CREATE INDEX CONCURRENTLY cannot run inside a transaction block (PostgreSQL CREATE INDEX); move it to a non-transactional section.",
            ));
        }
    }

    out
}

/// A statement paired with its hazards, for reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HazardLine {
    /// Where the statement came from (`statement 3`, `public.users`, ...).
    pub location: String,
    /// The statement SQL.
    pub sql: String,
    /// Hazards found on this statement, in detection order.
    pub hazards: Vec<Hazard>,
}

impl HazardLine {
    /// Builds a report line.
    #[must_use]
    pub fn new(location: impl Into<String>, sql: impl Into<String>, hazards: Vec<Hazard>) -> Self {
        HazardLine {
            location: location.into(),
            sql: sql.into(),
            hazards,
        }
    }
}

/// Collects the hazardous statements of a plan in order.
#[must_use]
pub fn plan_hazard_lines(plan: &Plan) -> Vec<HazardLine> {
    let mut out = Vec::new();
    let mut index = 0;
    for section in &plan.sections {
        for statement in &section.statements {
            index += 1;
            if !statement.hazards.is_empty() {
                out.push(HazardLine::new(
                    format!("statement {index}"),
                    statement.sql.clone(),
                    statement.hazards.clone(),
                ));
            }
        }
    }
    out
}

/// Total number of hazards across a plan.
#[must_use]
pub fn plan_hazard_count(plan: &Plan) -> usize {
    plan.statements().iter().map(|s| s.hazards.len()).sum()
}

/// Counts hazards by category.
#[must_use]
pub fn plan_hazard_counts(plan: &Plan) -> (usize, usize) {
    let mut destructive = 0;
    let mut locking = 0;
    for hazard in plan.statements().iter().flat_map(|s| s.hazards.iter()) {
        match hazard.category {
            HazardCategory::Destructive => destructive += 1,
            HazardCategory::Locking => locking += 1,
        }
    }
    (destructive, locking)
}

/// Fills a plan's per-statement hazard lists.
///
/// Called by the planner so `Plan`, `--json` and `--fail-on-hazard` all see
/// the same analysis. `from`/`to` are used only for the type-narrowing
/// check, which cannot be decided from SQL text alone.
pub(crate) fn analyze_plan(plan: &mut Plan, from: &Catalog, to: &Catalog) {
    let inputs: Vec<StatementInput<'_>> = plan
        .sections
        .iter()
        .flat_map(|section| {
            section
                .statements
                .iter()
                .map(move |statement| StatementInput {
                    sql: &statement.sql,
                    transactional: Some(section.transactional),
                })
        })
        .collect();
    let analysed = analyze_statements(&inputs);
    let mut cursor = 0;
    for section in &mut plan.sections {
        for statement in &mut section.statements {
            statement.hazards.clone_from(&analysed[cursor]);
            cursor += 1;
        }
    }

    // Type narrowing needs the source type, which is not in the statement
    // SQL. Source data types are from the PostgreSQL type catalogue; the
    // ranking below uses their documented storage widths.
    for change in crate::diff::diff(from, to) {
        let Change::AlterTable {
            schema,
            name,
            actions,
        } = change
        else {
            continue;
        };
        for action in actions {
            let AlterChange::AlterColumnType {
                column, data_type, ..
            } = action
            else {
                continue;
            };
            let Some(old) = from.table(&schema, &name).and_then(|t| t.column(&column)) else {
                continue;
            };
            if !is_narrowing(&old.data_type, &data_type) {
                continue;
            }
            let object = format!("{name}.{column}");
            for statement in plan.statements_mut() {
                if statement.object == object
                    && normalize(&statement.sql).contains(" type ")
                    && !statement.hazards.iter().any(|h| h.code == "type-narrowing")
                {
                    statement.hazards.push(destructive(
                        "type-narrowing",
                        format!(
                            "Narrowing {} to {data_type} can reject values that fit in the old type (and normally rewrites the table).",
                            old.data_type
                        ),
                    ));
                }
            }
        }
    }
}

/// True when `to` cannot represent every value of `from`. Only the builtin
/// families pgdelta renders canonically are ranked; unknown/custom types
/// are conservatively treated as *not* narrowing (no false alarms).
fn is_narrowing(from: &str, to: &str) -> bool {
    let width = |t: &str| -> Option<u32> {
        match t {
            "smallint" => Some(16),
            "integer" => Some(32),
            "bigint" => Some(64),
            "real" => Some(24),
            "double precision" => Some(53),
            "date" => Some(1),
            _ if t.starts_with("timestamp") => Some(2),
            _ => None,
        }
    };
    if let (Some(a), Some(b)) = (width(from), width(to)) {
        return b < a;
    }
    // character varying(n) -> character varying(m), m < n.
    if let (Some(a), Some(b)) = (varchar_len(from), varchar_len(to)) {
        return b < a;
    }
    // numeric(p[,s]) -> numeric(p'[,s']) with reduced precision or scale.
    if let (Some((ap, asc)), Some((bp, bs))) = (numeric_parts(from), numeric_parts(to)) {
        return bp < ap || bs < asc;
    }
    false
}

fn varchar_len(t: &str) -> Option<u32> {
    let inner = t
        .strip_prefix("character varying(")
        .and_then(|s| s.strip_suffix(')'))?;
    inner.parse().ok()
}

fn numeric_parts(t: &str) -> Option<(u32, u32)> {
    let inner = t.strip_prefix("numeric(")?.strip_suffix(')')?;
    let (p, s) = inner.split_once(',').unwrap_or((inner, "0"));
    Some((p.parse().ok()?, s.parse().ok()?))
}

/// Rewrites a plan into its online-safe form (the SPEC's `--online` mode):
///
/// * `ADD CONSTRAINT ... CHECK|FOREIGN KEY` becomes `NOT VALID` followed by
///   `VALIDATE CONSTRAINT` (PostgreSQL ALTER TABLE, Notes);
/// * `SET NOT NULL` is preceded by a `CHECK (col IS NOT NULL) NOT VALID`
///   and its `VALIDATE`, and followed by a `DROP CONSTRAINT` so the final
///   catalog converges to the target (ddl-constraints: not-null is
///   functionally equivalent to that check);
/// * `CREATE INDEX` becomes `CREATE INDEX CONCURRENTLY` and moves to the
///   non-transactional section. Partitioned tables are left alone because
///   concurrent builds there are unsupported (CREATE INDEX docs).
pub(crate) fn rewrite_online(plan: &mut Plan, from: &Catalog, to: &Catalog) {
    let mut statements = Vec::with_capacity(plan.statements().len());
    let mut concurrent = Vec::new();
    let mut used_names: BTreeSet<String> = plan
        .statements()
        .iter()
        .filter_map(|s| token_after(&normalize(&s.sql), " add constraint ").map(|t| bare_name(&t)))
        .collect();
    let mut validated_columns: BTreeSet<(String, String)> = BTreeSet::new();

    for statement in plan.statements() {
        let n = normalize(&statement.sql);
        let table = token_after(&n, "alter table ").map(|t| bare_name(&t));
        if let Some(constraint) = token_after(&n, " add constraint ").map(|c| bare_name(&c))
            && is_check(&n)
            && !n.contains(" not valid")
        {
            let mut add = statement.clone();
            add.sql = insert_not_valid(&statement.sql);
            add.hazards.clear();
            statements.push(add);
            let column = not_null_check_column(&n);
            let qual = token_after(&n, "alter table ").unwrap_or_default();
            statements.push(validate_statement(
                &format!(
                    "ALTER TABLE {qual} VALIDATE CONSTRAINT {};",
                    quote_ident(&constraint)
                ),
                &format!("{}.{}", table.clone().unwrap_or_default(), constraint),
            ));
            if let (Some(table), Some(column)) = (table.clone(), column) {
                validated_columns.insert((table, column));
            }
            continue;
        }
        if n.contains(" add constraint ") && is_foreign_key(&n) && !n.contains(" not valid") {
            let mut add = statement.clone();
            add.sql = insert_not_valid(&statement.sql);
            add.hazards.clear();
            statements.push(add);
            let constraint =
                token_after(&n, " add constraint ").map_or_else(String::new, |c| bare_name(&c));
            let qual = token_after(&n, "alter table ").unwrap_or_default();
            statements.push(validate_statement(
                &format!(
                    "ALTER TABLE {qual} VALIDATE CONSTRAINT {};",
                    quote_ident(&constraint)
                ),
                &format!("{}.{}", table.clone().unwrap_or_default(), constraint),
            ));
            continue;
        }
        if n.contains(" set not null") {
            let column = token_after(&n, " alter column ").map(|c| bare_name(&c));
            if let (Some(table), Some(column)) = (table.clone(), column.clone())
                && !validated_columns.contains(&(table.clone(), column.clone()))
            {
                let qual = token_after(&n, "alter table ").unwrap_or_default();
                let check = unique_check_name(from, to, &used_names, &table, &column);
                used_names.insert(check.clone());
                let check_sql = format!(
                    "ALTER TABLE {qual} ADD CONSTRAINT {} CHECK ({} IS NOT NULL) NOT VALID;",
                    quote_ident(&check),
                    quote_ident(&column)
                );
                statements.push(validate_statement(&check_sql, &format!("{table}.{check}")));
                statements.push(validate_statement(
                    &format!(
                        "ALTER TABLE {qual} VALIDATE CONSTRAINT {};",
                        quote_ident(&check)
                    ),
                    &format!("{table}.{check}"),
                ));
                statements.push(statement.clone());
                statements.push(validate_statement(
                    &format!(
                        "ALTER TABLE {qual} DROP CONSTRAINT {};",
                        quote_ident(&check)
                    ),
                    &format!("{table}.{check}"),
                ));
                continue;
            }
        }
        if (n.starts_with("create index ") || n.starts_with("create unique index "))
            && !n.contains(" concurrently ")
            && !partitioned_index(from, to, &statement.object)
        {
            let mut index = statement.clone();
            index.sql = insert_concurrently(&statement.sql);
            index.transactional = false;
            index.hazards.clear();
            concurrent.push(index);
            continue;
        }
        statements.push(statement.clone());
    }
    statements.extend(concurrent);

    let mut sections = Vec::new();
    let transactional: Vec<_> = statements
        .iter()
        .filter(|s| s.transactional)
        .cloned()
        .collect();
    let non_transactional: Vec<_> = statements
        .iter()
        .filter(|s| !s.transactional)
        .cloned()
        .collect();
    if !transactional.is_empty() {
        sections.push(PlanSection {
            transactional: true,
            statements: transactional,
        });
    }
    if !non_transactional.is_empty() {
        sections.push(PlanSection {
            transactional: false,
            statements: non_transactional,
        });
    }
    plan.sections = sections;
}

fn validate_statement(sql: &str, object: &str) -> PlanStatement {
    PlanStatement {
        sql: sql.to_owned(),
        action: "alter",
        object: object.to_owned(),
        transactional: true,
        hazards: Vec::new(),
    }
}

fn insert_not_valid(sql: &str) -> String {
    let trimmed = sql.trim_end();
    let Some(body) = trimmed.strip_suffix(';') else {
        return format!("{sql} NOT VALID;");
    };
    format!("{body} NOT VALID;")
}

fn insert_concurrently(sql: &str) -> String {
    sql.replacen("INDEX ", "INDEX CONCURRENTLY ", 1)
}

fn quote_ident(name: &str) -> String {
    crate::naming::quote_ident(name)
}

fn unique_check_name(
    from: &Catalog,
    to: &Catalog,
    used: &BTreeSet<String>,
    table: &str,
    column: &str,
) -> String {
    let base = format!("{table}_{column}_not_null_check");
    let mut candidate = base.clone();
    let mut n = 1;
    while used.contains(&candidate)
        || catalog_has_constraint(from, &candidate)
        || catalog_has_constraint(to, &candidate)
    {
        candidate = format!("{base}_{n}");
        n += 1;
    }
    candidate
}

fn catalog_has_constraint(catalog: &Catalog, name: &str) -> bool {
    catalog
        .tables
        .values()
        .any(|t| t.constraints.iter().any(|c| c.name == name))
        || catalog.types.values().any(|t| match &t.kind {
            crate::model::PgTypeKind::Domain { constraints, .. } => {
                constraints.iter().any(|c| c.name == name)
            }
            _ => false,
        })
}

fn partitioned_index(from: &Catalog, to: &Catalog, object: &str) -> bool {
    let name = bare_name(object);
    [from, to].iter().any(|catalog| {
        catalog.indexes.values().any(|index| {
            if index.name != name {
                return false;
            }
            // Concurrent index builds are unsupported on a partitioned
            // parent (CREATE INDEX docs); a leaf partition is a plain table
            // and can safely be built concurrently.
            catalog
                .tables
                .get(&(index.table_schema.clone(), index.table.clone()))
                .is_some_and(|t| t.partition.is_some() || t.partition_of.is_some())
        })
    })
}

/// Renders a compact, coloured summary of hazard lines.
///
/// Colour follows [`ColorChoice`] (`Auto` honours `NO_COLOR`); the text is
/// stable and testable with `Always`/`Never`.
#[must_use]
pub fn render_hazard_report(lines: &[HazardLine], color: ColorChoice) -> String {
    let total: usize = lines.iter().map(|l| l.hazards.len()).sum();
    let destructive: usize = lines
        .iter()
        .flat_map(|l| l.hazards.iter())
        .filter(|h| h.category == HazardCategory::Destructive)
        .count();
    let locking = total - destructive;
    let enabled = color.enabled();
    let paint = |text: &str, code: &str| -> String {
        if enabled {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    };

    let mut out = String::new();
    if total == 0 {
        out.push_str(&paint(
            "Hazards: none (all statements are online-safe).",
            "1;32",
        ));
        out.push('\n');
        return out;
    }
    out.push_str(&paint(
        &format!("Hazards: {total} ({destructive} destructive, {locking} locking)"),
        "1",
    ));
    out.push('\n');
    for line in lines {
        for hazard in &line.hazards {
            let (marker, ansi) = match hazard.category {
                HazardCategory::Destructive => ("!", "31"),
                HazardCategory::Locking => ("~", "33"),
            };
            out.push_str(&format!(
                "  {} {} [{}]{} {}\n",
                paint(&format!("[{marker}]"), ansi),
                line.location,
                hazard.category.as_str(),
                hazard.code,
                paint(&hazard.message, "2"),
            ));
            out.push_str(&format!("      {}\n", line.sql));
        }
    }
    out
}

/// Renders the hazard analysis of a handwritten migration as JSON.
///
/// # Errors
///
/// Returns [`crate::Error::Serialization`] only if a value cannot be
/// serialised (all fields are plain strings and numbers).
pub fn render_lint_json(
    source: &str,
    statements: &[String],
    hazards: &[Vec<Hazard>],
) -> Result<Value> {
    let mut total = 0;
    let mut destructive = 0;
    let mut items = Vec::with_capacity(statements.len());
    for (index, (sql, statement_hazards)) in statements.iter().zip(hazards).enumerate() {
        total += statement_hazards.len();
        destructive += statement_hazards
            .iter()
            .filter(|h| h.category == HazardCategory::Destructive)
            .count();
        items.push(serde_json::json!({
            "index": index + 1,
            "sql": sql,
            "hazards": statement_hazards,
        }));
    }
    Ok(serde_json::json!({
        "source": source,
        "summary": {
            "statements": statements.len(),
            "hazards": total,
            "destructive": destructive,
            "locking": total - destructive,
        },
        "statements": items,
    }))
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::assert_is_empty
    )]

    use super::*;

    fn h(sql: &str) -> Vec<Hazard> {
        analyze_statements(&[StatementInput::sql(sql)]).remove(0)
    }

    #[test]
    fn destructive_vectors_are_detected() {
        let vectors = [
            "DROP TABLE public.old;",
            "DROP SCHEMA legacy CASCADE;",
            "ALTER TABLE public.users DROP COLUMN email;",
            "ALTER TABLE public.users DROP CONSTRAINT users_pkey;",
        ];
        for sql in vectors {
            assert_eq!(h(sql).len(), 1, "{sql} should be one hazard");
            assert_eq!(h(sql)[0].category, HazardCategory::Destructive, "{sql}");
        }
    }

    #[test]
    fn safe_vectors_have_no_hazards() {
        let vectors = [
            "CREATE TABLE public.t (id integer NOT NULL);",
            "ALTER TABLE public.t ADD COLUMN note text;",
            "ALTER TABLE public.t ADD CONSTRAINT c CHECK (id >= 0) NOT VALID;",
            "ALTER TABLE public.t VALIDATE CONSTRAINT c;",
            "ALTER TABLE public.t ADD CONSTRAINT fk FOREIGN KEY (id) REFERENCES p (id) NOT VALID;",
            "CREATE INDEX CONCURRENTLY idx ON public.t (id);",
            "COMMENT ON TABLE public.t IS 'DROP TABLE is only a comment';",
        ];
        for sql in vectors {
            assert!(h(sql).is_empty(), "{sql} should be safe: {:?}", h(sql));
        }
    }

    #[test]
    fn locking_vectors_are_detected() {
        let vectors = [
            "ALTER TABLE public.t ALTER COLUMN id TYPE bigint;",
            "ALTER TABLE public.t ALTER COLUMN id SET NOT NULL;",
            "ALTER TABLE public.t ADD CONSTRAINT c CHECK (id >= 0);",
            "ALTER TABLE public.t ADD CONSTRAINT fk FOREIGN KEY (id) REFERENCES p (id);",
            "CREATE INDEX idx ON public.t (id);",
            "CREATE UNIQUE INDEX idx ON public.t (id);",
        ];
        for sql in vectors {
            assert!(
                h(sql).iter().any(|x| x.category == HazardCategory::Locking),
                "{sql}"
            );
        }
    }

    #[test]
    fn validated_check_suppresses_not_null_hazard() {
        let statements = [
            "ALTER TABLE public.t ADD CONSTRAINT nn CHECK (email IS NOT NULL) NOT VALID;",
            "ALTER TABLE public.t VALIDATE CONSTRAINT nn;",
            "ALTER TABLE public.t ALTER COLUMN email SET NOT NULL;",
        ];
        let inputs: Vec<_> = statements.iter().map(|s| StatementInput::sql(s)).collect();
        let found = analyze_statements(&inputs);
        assert!(found[0].is_empty());
        assert!(found[1].is_empty());
        assert!(
            found[2].is_empty(),
            "SET NOT NULL must be safe after VALIDATE"
        );
    }

    #[test]
    fn concurrent_index_inside_transaction_is_a_hazard() {
        let statements = [
            "BEGIN;",
            "CREATE INDEX CONCURRENTLY idx ON public.t (id);",
            "COMMIT;",
        ];
        let inputs: Vec<_> = statements.iter().map(|s| StatementInput::sql(s)).collect();
        let found = analyze_statements(&inputs);
        assert_eq!(found[1][0].code, "concurrent-index-in-transaction");
    }

    #[test]
    fn narrowing_vectors() {
        assert!(is_narrowing("bigint", "integer"));
        assert!(is_narrowing("integer", "smallint"));
        assert!(is_narrowing("double precision", "real"));
        assert!(is_narrowing(
            "character varying(50)",
            "character varying(10)"
        ));
        assert!(is_narrowing("numeric(8,2)", "numeric(6,2)"));
        assert!(is_narrowing("numeric(8,2)", "numeric(8,1)"));
        assert!(!is_narrowing("integer", "bigint"));
        assert!(!is_narrowing("text", "text"));
        assert!(!is_narrowing(
            "character varying(10)",
            "character varying(50)"
        ));
        assert!(!is_narrowing("numeric(6,2)", "numeric(8,2)"));
        assert!(!is_narrowing("public.custom", "public.other"));
    }

    #[test]
    fn normalization_ignores_comments_and_literals() {
        assert_eq!(
            normalize("-- drop table fake\nSELECT 'drop schema x';"),
            "select _;"
        );
        assert_eq!(
            normalize("/* DROP COLUMN x */ CREATE TABLE t (a int)"),
            "create table t (a int)"
        );
    }

    #[test]
    fn color_report_is_stable() {
        let lines = vec![HazardLine::new(
            "statement 1".to_owned(),
            "DROP TABLE old;".to_owned(),
            h("DROP TABLE old;"),
        )];
        let plain = render_hazard_report(&lines, ColorChoice::Never);
        assert!(plain.contains("[destructive]drop-table"));
        assert!(!plain.contains('\u{1b}'));
        let colored = render_hazard_report(&lines, ColorChoice::Always);
        assert!(colored.contains("\u{1b}[31m"));
    }
}
