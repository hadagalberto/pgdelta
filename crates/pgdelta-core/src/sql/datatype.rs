//! Data-type capture and canonicalisation.
//! Column types are captured as raw token ranges by pgdelta's own parsers,
//! re-parsed with sqlparser through a `CREATE TABLE __pgdelta_lift__
//! (__c__ <type>)` lift, and canonicalised to a single string per type.
//!
//! The canonical strings mirror what PostgreSQL's `pg_catalog.format_type`
//! reports for the built-in types (`bigint`, `character varying(255)`,
//! `timestamp with time zone`, ...). That is the shape a live-database
//! source reports for the same column, so a SQL-file source and a live
//! source produce equal models (the parity requirement in the SPEC). Alias
//! folding (`int8` → `bigint`, `timestamptz` → `timestamp with time zone`,
//! ...) makes semantically-equal spellings compare equal.
//!
//! Types that sqlparser does not model as builtins arrive as
//! `Custom(ObjectName, ...)`; those are folded (unquoted parts to lower
//! case, quoted parts preserved), qualified as written, and keep their
//! array suffixes. `serial`-family pseudo-types are rejected: they imply a
//! sequence object, which is modelled in a later milestone — identity
//! columns are the portable replacement.

use sqlparser::ast::{
    ArrayElemTypeDef, DataType, ExactNumberInfo, ObjectName, ObjectNamePart, TimezoneInfo,
};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

use crate::error::{Error, Result};

/// Re-parses raw type text through sqlparser's `CREATE TABLE` lift.
pub(crate) fn parse_type_text(text: &str) -> Result<DataType> {
    let sql = format!("CREATE TABLE __pgdelta_lift__ (__c__ {text})");
    let statements = Parser::parse_sql(&PostgreSqlDialect {}, &sql).map_err(|e| {
        Error::Serialization(format!("internal type lift failed for {text:?}: {e}"))
    })?;
    match statements.into_iter().next() {
        Some(sqlparser::ast::Statement::CreateTable(create)) => create
            .columns
            .into_iter()
            .next()
            .map(|c| c.data_type)
            .ok_or_else(|| {
                Error::Serialization(format!(
                    "internal type lift produced no column for {text:?}"
                ))
            }),
        _ => Err(Error::Serialization(format!(
            "internal type lift produced no CREATE TABLE for {text:?}"
        ))),
    }
}

/// Captures raw type text, lifts it and renders the canonical string.
pub(crate) fn normalize_type_text(text: &str) -> Result<String> {
    let data_type = parse_type_text(text)?;
    canonical_type(&data_type)
}

/// Renders a `sqlparser` data type for use inside `CAST(x AS <type>)`.
pub(crate) fn render_type_for_expr(data_type: &DataType) -> Result<String> {
    canonical_type(data_type)
}

/// Canonical string for a parsed type.
pub(crate) fn canonical_type(data_type: &DataType) -> Result<String> {
    let out = match data_type {
        // Integers and aliases: PostgreSQL docs Chapter 8; format_type
        // reports the canonical spelling on the right.
        DataType::SmallInt(_) => "smallint".to_owned(),
        DataType::Int(_) | DataType::Integer(_) => "integer".to_owned(),
        DataType::BigInt(_) => "bigint".to_owned(),
        DataType::Boolean | DataType::Bool => "boolean".to_owned(),
        DataType::Real => "real".to_owned(),
        // PG float(n): 1-24 → real, 25-53 → double precision.
        DataType::Float(info) => match info {
            ExactNumberInfo::Precision(n) if *n <= 24 => "real".to_owned(),
            ExactNumberInfo::Precision(_) => "double precision".to_owned(),
            _ => {
                return Err(Error::Serialization(
                    "bare FLOAT is deprecated and ambiguous; use real or double precision"
                        .to_owned(),
                ));
            }
        },
        DataType::Double(_) | DataType::DoublePrecision => "double precision".to_owned(),
        DataType::Numeric(info) | DataType::Decimal(info) => numeric_string(info),
        DataType::Text => "text".to_owned(),
        // `String` (SQL-standard CHARACTER?) is PG-unrecognized; reject.
        DataType::String(_) => {
            return Err(Error::Serialization(
                "the STRING type is not a PostgreSQL type".to_owned(),
            ));
        }
        DataType::Varchar(len) | DataType::CharacterVarying(len) => match length(len.as_ref()) {
            Some(n) => format!("character varying({n})"),
            None => "character varying".to_owned(),
        },
        DataType::Char(len) | DataType::Character(len) => match length(len.as_ref()) {
            Some(n) => format!("character({n})"),
            None => "character(1)".to_owned(),
        },
        DataType::Date => "date".to_owned(),
        DataType::Time(prec, tz) => tz_string("time", prec.as_ref().copied(), *tz),
        DataType::Timestamp(prec, tz) => tz_string("timestamp", prec.as_ref().copied(), *tz),
        DataType::Bytea => "bytea".to_owned(),
        // PostgreSQL Chapter 8 (JSON Types): `json` stores an exact text
        // copy; `jsonb` stores a decomposed binary representation. Both are
        // first-class built-in column types with these catalog spellings.
        DataType::JSON => "json".to_owned(),
        DataType::JSONB => "jsonb".to_owned(),
        DataType::Bit(len) => match len {
            Some(n) => format!("bit({n})"),
            None => "bit(1)".to_owned(),
        },
        DataType::BitVarying(len) => match len {
            Some(n) => format!("bit varying({n})"),
            None => "bit varying".to_owned(),
        },
        DataType::Interval { .. } => "interval".to_owned(),
        // Pseudo-types used by routine signatures (CREATE FUNCTION docs,
        // `rettype`/`argtype`): a trigger function must return `trigger`.
        DataType::Trigger => "trigger".to_owned(),
        DataType::Regclass => "regclass".to_owned(),
        DataType::Array(elem) => array_string(elem)?,
        DataType::Custom(name, _) => custom_canonical(name)?,
        other => {
            return Err(Error::Serialization(format!(
                "unsupported data type {other:?}"
            )));
        }
    };
    Ok(out)
}

fn numeric_string(info: &ExactNumberInfo) -> String {
    match info {
        ExactNumberInfo::Precision(p) => format!("numeric({p})"),
        ExactNumberInfo::PrecisionAndScale(p, s) => format!("numeric({p},{s})"),
        ExactNumberInfo::None => "numeric".to_owned(),
    }
}

fn length(len: Option<&sqlparser::ast::CharacterLength>) -> Option<String> {
    match len {
        Some(sqlparser::ast::CharacterLength::IntegerLength { length, .. }) => {
            Some(length.to_string())
        }
        // `VARCHAR(MAX)` is T-SQL syntax, not a PostgreSQL type.
        Some(sqlparser::ast::CharacterLength::Max) => None,
        None => None,
    }
}

fn array_string(elem: &ArrayElemTypeDef) -> Result<String> {
    let inner = match elem {
        ArrayElemTypeDef::SquareBracket(dt, _) | ArrayElemTypeDef::Parenthesis(dt) => {
            canonical_type(dt)?
        }
        ArrayElemTypeDef::AngleBracket(dt) => canonical_type(dt)?,
        ArrayElemTypeDef::None => {
            return Err(Error::Serialization(
                "ARRAY without an element type is not a PostgreSQL column type".to_owned(),
            ));
        }
    };
    Ok(format!("{inner}[]"))
}

fn tz_string(base: &str, precision: Option<u64>, tz: TimezoneInfo) -> String {
    let p = precision.map_or_default(|p| format!("({p})"));
    let tz = match tz {
        TimezoneInfo::WithTimeZone | TimezoneInfo::Tz => " with time zone",
        TimezoneInfo::WithoutTimeZone | TimezoneInfo::None => " without time zone",
    };
    format!("{base}{p}{tz}")
}

/// Canonical form for names sqlparser passes through as `Custom`: apply
/// the PostgreSQL alias map for well-known catalog names, fold unquoted
/// parts, and keep qualification as written.
fn custom_canonical(name: &ObjectName) -> Result<String> {
    let mut rendered: Vec<String> = Vec::with_capacity(name.0.len());
    for part in &name.0 {
        match part {
            ObjectNamePart::Identifier(i) => {
                if i.quote_style.is_some() {
                    rendered.push(i.value.clone());
                } else {
                    rendered.push(i.value.to_ascii_lowercase());
                }
            }
            ObjectNamePart::Function(function) => {
                return Err(Error::Serialization(format!(
                    "unsupported type name part {function:?}"
                )));
            }
        }
    }
    let joined = rendered.join(".");
    if rendered.len() == 1
        && let Some(al) = alias(&joined)
    {
        return Ok(al);
    }
    Ok(joined)
}

/// PostgreSQL type aliases that canonicalise to the catalog name
/// (`pg_type` spelling), as documented in Chapter 8. Returns `None` for
/// names that need no rewriting; the `serial` family is rejected by
/// [`is_serial_family`] before the model is built.
fn alias(name: &str) -> Option<String> {
    match name {
        "bool" => Some("boolean".to_owned()),
        "int2" => Some("smallint".to_owned()),
        "int4" | "int" => Some("integer".to_owned()),
        "int8" => Some("bigint".to_owned()),
        "float4" => Some("real".to_owned()),
        "float8" => Some("double precision".to_owned()),
        "decimal" | "dec" => Some("numeric".to_owned()),
        "varbit" => Some("bit varying".to_owned()),
        "timestamptz" => Some("timestamp with time zone".to_owned()),
        "timetz" => Some("time with time zone".to_owned()),
        "bpchar" => Some("character".to_owned()),
        _ => None,
    }
}

/// True when the canonical string is a `serial`-family pseudo-type; these
/// imply a sequence object, which the model does not cover in milestone 1.
pub(crate) fn is_serial_family(canonical: &str) -> bool {
    matches!(
        canonical,
        "serial" | "bigserial" | "smallserial" | "serial2" | "serial4" | "serial8"
    )
}
