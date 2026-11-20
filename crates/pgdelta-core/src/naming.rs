//! PostgreSQL's auto-generated object-name algorithms, ported faithfully.
//!
//! Sources (PostgreSQL 18 stable branch, files as of the `REL_18_STABLE`
//! tags, MIT-compatible PostgreSQL Licence):
//!
//! * `makeObjectName` — `src/backend/commands/indexcmds.c`
//!   <https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/commands/indexcmds.c>
//! * `ChooseRelationName`, `ChooseIndexName`, `ChooseIndexNameAddition` —
//!   same file;
//! * `ChooseConstraintName` —
//!   `src/backend/catalog/pg_constraint.c`
//!   <https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/catalog/pg_constraint.c>
//! * `ChooseForeignKeyConstraintNameAddition` —
//!   `src/backend/commands/tablecmds.c`
//!   <https://github.com/postgres/postgres/blob/REL_18_STABLE/src/backend/commands/tablecmds.c>
//! * `NAMEDATALEN` (64, i.e. 63 usable identifier bytes) —
//!   `src/include/pg_config_manual.h`
//!   <https://github.com/postgres/postgres/blob/REL_18_STABLE/src/include/pg_config_manual.h>
//!
//! The call graph is reproduced exactly as the PostgreSQL sources use it:
//!
//! * unnamed `PRIMARY KEY` (column or table form) →
//!   `ChooseIndexName(primary=true)` → `table_pkey` — no column names;
//! * unnamed `UNIQUE` → `ChooseIndexName(isconstraint=true)` →
//!   `table_cols_key`;
//! * unnamed index → `ChooseIndexName` → `table_cols_idx`;
//! * unnamed column-level `CHECK` → `ChooseConstraintName(table, column,
//!   "check")` → `table_column_check`; table-level →
//!   `ChooseConstraintName(table, NULL, "check")` → `table_check`;
//! * unnamed `FOREIGN KEY` → `ChooseConstraintName(table, cols, "fkey")`
//!   → `table_cols_fkey`.
//!
//! Collision handling differs between the two families exactly as in the
//! sources: constraint names (`pkey` comes via the index path, `key`,
//! `check`, `fkey`) retry by appending digits to the *label*
//! (`t_check`, `t_check1`, `t_check2`, ...) while re-checking conflicts
//! within the schema namespace (and against names already chosen within the
//! same command); plain index names check conflicts against relation names
//! only.

/// Maximum identifier length in bytes: `NAMEDATALEN - 1` (64 - 1 = 63).
///
/// PostgreSQL truncates identifiers that exceed this; auto-generated names
/// are constructed to fit, mirroring `makeObjectName`.
pub const MAX_IDENTIFIER_BYTES: usize = 63;

/// Ports `makeObjectName(name1, name2, label)` from
/// `src/backend/commands/indexcmds.c`: builds `name1_name2_label`,
/// omitting absent parts, truncating `name1`/`name2` (never the label) so
/// the result fits in 63 bytes, shrinking the longer name first, and
/// clipping on a character boundary (`pg_mbcliplen` semantics).
pub fn make_object_name(name1: &str, name2: Option<&str>, label: Option<&str>) -> String {
    let mut name1chars = name1.len() as i64;
    let mut name2chars = name2.map_or(0, str::len) as i64;
    // overhead = chars needed for the label and the separating underscores.
    let mut overhead = 0usize;
    if name2.is_some() {
        overhead += 1;
    }
    if let Some(l) = label {
        overhead += l.len() + 1;
    }
    let availchars = MAX_IDENTIFIER_BYTES as i64 - overhead as i64;

    // If we must truncate, preferentially truncate the longer name.
    while name1chars + name2chars > availchars {
        if name1chars > name2chars {
            name1chars -= 1;
        } else {
            name2chars -= 1;
        }
    }

    let n1 = mb_clip(name1, name1chars.max(0) as usize);
    let mut out = String::from(n1);
    if let (Some(n2), true) = (name2, name2chars > 0) {
        out.push('_');
        out.push_str(mb_clip(n2, name2chars as usize));
    }
    if let Some(l) = label {
        out.push('_');
        out.push_str(l);
    }
    out
}

/// Clips `s` to at most `limit` bytes without splitting a multi-byte UTF-8
/// character (`pg_mbcliplen`).
fn mb_clip(s: &str, limit: usize) -> &str {
    if s.len() <= limit {
        return s;
    }
    let mut end = limit;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Ports `ChooseIndexNameAddition` /
/// `ChooseForeignKeyConstraintNameAddition`: joins column names with `_`,
/// truncating the result once it reaches 63 bytes.
#[must_use]
pub fn name_addition(names: &[String]) -> String {
    let mut buf = String::new();
    for name in names {
        if !buf.is_empty() {
            buf.push('_');
        }
        buf.push_str(name);
        if buf.len() >= MAX_IDENTIFIER_BYTES {
            // strlcpy(buf + buflen, name, NAMEDATALEN) in the original clips
            // the whole buffer at 63 bytes; mirror that.
            buf.truncate(MAX_IDENTIFIER_BYTES);
            break;
        }
    }
    buf
}

/// Collision-check used by the choose_* helpers: `taken` reports whether a
/// candidate name is unavailable (existing relation/constraint names plus
/// names already chosen within the same command).
type Taken<'a> = &'a dyn Fn(&str) -> bool;

/// Ports `ChooseRelationName` / `ChooseConstraintName`: tries the label
/// first, then `label1`, `label2`, ... until `taken` accepts the candidate.
fn choose_unique(name1: &str, name2: Option<&str>, label: &str, taken: Taken) -> String {
    let mut pass = 0u32;
    loop {
        let modlabel = if pass == 0 {
            label.to_owned()
        } else {
            format!("{label}{pass}")
        };
        let candidate = make_object_name(name1, name2, Some(&modlabel));
        if !taken(&candidate) {
            return candidate;
        }
        pass += 1;
    }
}

/// Auto-generates the name for an unnamed `PRIMARY KEY`, `UNIQUE`
/// constraint, or standalone index, mirroring `ChooseIndexName`:
/// `primary` → `table_pkey` (no columns); `isconstraint` (unique
/// constraints) → `table_cols_key`; plain indexes → `table_cols_idx`.
///
/// `taken_relations` and `taken_constraints` correspond to the two catalogs
/// the original scans: `ChooseRelationName` always checks `pg_class`
/// (relations) and, when `isconstraint`, also `pg_constraint`;
/// `others` carries names already chosen within the same command.
pub fn auto_index_name(
    table: &str,
    columns: &[String],
    primary: bool,
    is_constraint: bool,
    taken_relations: Taken,
    taken_constraints: Taken,
    others: &[String],
) -> String {
    let addition = name_addition(columns);
    let (name2, label) = if primary {
        (None, "pkey")
    } else if is_constraint {
        (Some(addition.as_str()), "key")
    } else {
        (Some(addition.as_str()), "idx")
    };
    choose_unique(table, name2, label, &|candidate| {
        taken_relations(candidate)
            || (is_constraint && taken_constraints(candidate))
            || others.contains(&candidate.to_owned())
    })
}

/// Auto-generates the name for an unnamed `CHECK` or `FOREIGN KEY`
/// constraint, mirroring `ChooseConstraintName`: column-level checks pass
/// the column name, table-level checks and foreign keys pass the
/// column-name addition. Conflicts are checked against constraint names in
/// the schema and against `others` (names already chosen in the same
/// command).
pub fn auto_constraint_name(
    table: &str,
    name2: Option<&str>,
    label: &str,
    taken_constraints: Taken,
    others: &[String],
) -> String {
    choose_unique(table, name2, label, &|candidate| {
        taken_constraints(candidate) || others.contains(&candidate.to_owned())
    })
}

/// Renders an identifier with PostgreSQL's always-quote style used by
/// `pg_dump`: wrapped in double quotes with embedded quotes doubled.
///
/// This is always valid and side-steps keyword collisions (a column named
/// `"order"` renders as `"order"`, never `order`).
#[must_use]
pub fn quote_ident(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    out.push('"');
    for ch in name.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// Folds an identifier to its PostgreSQL-stored form: unquoted identifiers
/// are case-insensitive and folded to lower case (PostgreSQL docs §4.1.1),
/// quoted identifiers keep their case.
#[must_use]
pub fn fold_ident(value: &str, was_quoted: bool) -> String {
    if was_quoted {
        value.to_owned()
    } else {
        value.to_ascii_lowercase()
    }
}

/// Truncates an identifier to PostgreSQL's 63-byte limit
/// (`NAMEDATALEN - 1`; PostgreSQL docs §4.1.1: "The system uses no more
/// than NAMEDATALEN-1 bytes of an identifier; longer names can be written
/// in commands, but they will be truncated"). The clip keeps UTF-8
/// characters intact.
#[must_use]
pub fn truncate_ident(name: &str) -> String {
    if name.len() <= MAX_IDENTIFIER_BYTES {
        name.to_owned()
    } else {
        let mut end = MAX_IDENTIFIER_BYTES;
        while end > 0 && !name.is_char_boundary(end) {
            end -= 1;
        }
        name[..end].to_owned()
    }
}

/// Renders a `schema.name` pair, always quoted.
#[must_use]
pub fn qualified(schema: &str, name: &str) -> String {
    format!("{}.{}", quote_ident(schema), quote_ident(name))
}

/// Renders a dotted name with every part quoted.
#[must_use]
pub fn quote_parts(name: &str) -> String {
    name.split('.')
        .map(quote_ident)
        .collect::<Vec<_>>()
        .join(".")
}
