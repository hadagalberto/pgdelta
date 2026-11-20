//! Expression capture and normalisation.
//!
//! Column defaults, `CHECK` expressions, generated-column expressions
//! and index predicates/expressions are captured as raw token ranges by
//! pgdelta's own parsers, then re-parsed with sqlparser itself through
//! the `SELECT <expr>` lift, and finally rendered back to a canonical,
//! deterministic string by this module.
//!
//! Normalisation guarantees:
//!
//! * unquoted identifiers and function names are folded to lower case
//!   (PostgreSQL docs §4.1.1), quoted identifiers keep their case; all
//!   identifiers render always-quoted, which is unconditionally valid and
//!   sidesteps keyword collisions (see `docs/parser.md` for the
//!   trade-off);
//! * equivalent spellings of the same expression parse to the same AST
//!   and therefore render to the same string (`NOW()` == `now()`,
//!   `x::int` == `CAST(x AS integer)`);
//! * binary operators re-parenthesise according to PostgreSQL's
//!   documented operator precedence (PostgreSQL docs §4.1.6, "Operator
//!   Precedence") so the rendered string re-parses to the identical tree.

use sqlparser::ast::{
    BinaryOperator, CaseWhen, DataType, DuplicateTreatment, Expr, Function, FunctionArg,
    FunctionArgExpr, FunctionArguments, ObjectNamePart, UnaryOperator,
};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

use crate::error::Result;
use crate::naming::quote_ident;

/// Re-parses raw expression text through sqlparser's `SELECT <expr>` lift.
pub(crate) fn parse_expr_text(text: &str) -> Result<Expr> {
    let sql = format!("SELECT {text}");
    let statements = Parser::parse_sql(&PostgreSqlDialect {}, &sql).map_err(|e| {
        crate::Error::Serialization(format!("internal expr lift failed for {text:?}: {e}"))
    })?;
    let expr = match statements.into_iter().next() {
        Some(sqlparser::ast::Statement::Query(query)) => match *query.body {
            sqlparser::ast::SetExpr::Select(select) => {
                match select.projection.into_iter().next() {
                    Some(sqlparser::ast::SelectItem::UnnamedExpr(expr)) => Some(expr),
                    _ => None,
                }
            }
            _ => None,
        },
        _ => None,
    };
    expr.ok_or_else(|| {
        crate::Error::Serialization(format!(
            "internal expr lift produced no expression for {text:?}"
        ))
    })
}

/// Captures raw expression text, lifts it and renders the normalised form.
pub(crate) fn normalize_expr_text(text: &str) -> Result<String> {
    let expr = parse_expr_text(text)?;
    render_expr(&expr)
}

/// Operator precedence for re-parenthesisation, following the PostgreSQL
/// documentation table "Operator Precedence (highest to lowest)"
/// (PostgreSQL docs §4.1.6). All binary operators are left-associative.
fn binary_precedence(op: &BinaryOperator) -> u8 {
    use BinaryOperator::{Or, And, Eq, NotEq, Lt, LtEq, Gt, GtEq, Spaceship};
    match op {
        Or => 1,
        And => 2,
        Eq | NotEq | Lt | LtEq | Gt | GtEq | Spaceship => 3,
        // The "any other operator" band, which includes `||` and every
        // PostgreSQL operator that is not listed above by name.
        _ => 4,
    }
}

/// Whether `child` needs wrapping in parentheses when rendered as the
/// `right` (true) or `left` (false) operand of `parent`.
fn needs_parens(parent: &BinaryOperator, child: &Expr, is_right: bool) -> bool {
    match child {
        Expr::BinaryOp { op, .. } => {
            let p = binary_precedence(parent);
            let c = binary_precedence(op);
            // Same-precedence left-associative operators need parens only
            // on the right side; lower-precedence children need them on
            // both sides so the tree re-parses identically.
            c < p || (c == p && is_right)
        }
        _ => false,
    }
}

/// Renders an expression to its canonical pgdelta form.
pub(crate) fn render_expr(expr: &Expr) -> Result<String> {
    render_inner(expr)
}

fn render_inner(expr: &Expr) -> Result<String> {
    match expr {
        Expr::Identifier(ident) => {
            Ok(quote_ident(&folded(&ident.value, ident.quote_style.is_some())))
        }
        Expr::CompoundIdentifier(idents) => Ok(idents
            .iter()
            .map(|i| quote_ident(&folded(&i.value, i.quote_style.is_some())))
            .collect::<Vec<_>>()
            .join(".")),
        Expr::Value(v) => render_value(&v.value),
        Expr::UnaryOp { op, expr } => {
            let inner = render_inner(expr)?;
            match op {
                UnaryOperator::Not => Ok(format!("not ({inner})")),
                UnaryOperator::Minus => Ok(format!("-{inner}")),
                UnaryOperator::Plus => Ok(format!("+{inner}")),
                other => Err(crate::Error::Serialization(format!(
                    "unsupported unary operator {other:?} in expression"
                ))),
            }
        }
        Expr::BinaryOp { left, op, right } => {
            let l = render_inner(left)?;
            let r = render_inner(right)?;
            let lp = if needs_parens(op, left, false) {
                format!("({l})")
            } else {
                l
            };
            let rp = if needs_parens(op, right, true) {
                format!("({r})")
            } else {
                r
            };
            let op_str = render_binary_op(op)?;
            Ok(format!("{lp} {op_str} {rp}"))
        }
        Expr::Nested(inner) => {
            let s = render_inner(inner)?;
            Ok(format!("({s})"))
        }
        Expr::Function(Function {
            name,
            uses_odbc_syntax,
            parameters,
            args,
            filter,
            null_treatment,
            over,
            within_group,
            ..
        }) => {
            if *uses_odbc_syntax
                || filter.is_some()
                || null_treatment.is_some()
                || over.is_some()
                || !within_group.is_empty()
            {
                return Err(crate::Error::Serialization(
                    "unsupported function modifier (FILTER/OVER/WITHIN GROUP) in expression"
                        .to_owned(),
                ));
            }
            let name_rendered = render_object_name(name)?;
            let args = match args {
                FunctionArguments::List(list) => {
                    if list.duplicate_treatment == Some(DuplicateTreatment::Distinct) {
                        return Err(crate::Error::Serialization(
                            "DISTINCT function arguments are not supported in modelled expressions"
                                .to_owned(),
                        ));
                    }
                    if !list.clauses.is_empty() {
                        return Err(crate::Error::Serialization(
                            "function-argument clauses (ORDER BY/LIMIT) are not supported in \
                             modelled expressions"
                                .to_owned(),
                        ));
                    }
                    list.args.clone()
                }
                FunctionArguments::None => Vec::new(),
                FunctionArguments::Subquery(_) => {
                    return Err(crate::Error::Serialization(
                        "subquery function arguments are not supported in modelled expressions"
                            .to_owned(),
                    ))
                }
            };
            let _ = parameters;
            let mut parts = Vec::new();
            for arg in &args {
                match arg {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                        parts.push(render_inner(e)?);
                    }
                    other => {
                        return Err(crate::Error::Serialization(format!(
                            "unsupported function argument shape {other:?}"
                        )))
                    }
                }
            }
            Ok(format!("{name_rendered}({})", parts.join(", ")))
        }
        Expr::Cast { expr, data_type, .. } => {
            let inner = render_inner(expr)?;
            let ty = render_cast_type(data_type)?;
            Ok(format!("CAST({inner} AS {ty})"))
        }
        Expr::IsNull(inner) => {
            let s = render_inner(inner)?;
            Ok(format!("{s} is null"))
        }
        Expr::IsNotNull(inner) => {
            let s = render_inner(inner)?;
            Ok(format!("{s} is not null"))
        }
        Expr::IsTrue(inner) => {
            let s = render_inner(inner)?;
            Ok(format!("{s} is true"))
        }
        Expr::IsFalse(inner) => {
            let s = render_inner(inner)?;
            Ok(format!("{s} is false"))
        }
        Expr::Between {
            expr,
            low,
            high,
            negated,
            ..
        } => {
            let e = render_inner(expr)?;
            let l = render_inner(low)?;
            let h = render_inner(high)?;
            let neg = if *negated { "not " } else { "" };
            Ok(format!("{e} {neg}between {l} and {h}"))
        }
        Expr::InList {
            expr, list, negated, ..
        } => {
            let e = render_inner(expr)?;
            let items = list.iter().map(render_inner).collect::<Result<Vec<_>>>()?;
            let neg = if *negated { "not " } else { "" };
            Ok(format!("{e} {neg}in ({})", items.join(", ")))
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            let mut out = String::from("case");
            if let Some(operand) = operand {
                out.push(' ');
                out.push_str(&render_inner(operand)?);
            }
            for CaseWhen { condition, result } in conditions {
                out.push_str(" when ");
                out.push_str(&render_inner(condition)?);
                out.push_str(" then ");
                out.push_str(&render_inner(result)?);
            }
            if let Some(els) = else_result {
                out.push_str(" else ");
                out.push_str(&render_inner(els)?);
            }
            out.push_str(" end");
            Ok(out)
        }
        other => Err(crate::Error::Serialization(format!(
            "unsupported expression form {other:?}"
        ))),
    }
}

fn folded(value: &str, was_quoted: bool) -> String {
    if was_quoted {
        value.to_owned()
    } else {
        value.to_ascii_lowercase()
    }
}

fn render_object_name(name: &sqlparser::ast::ObjectName) -> Result<String> {
    let parts: Vec<&sqlparser::ast::Ident> = name
        .0
        .iter()
        .filter_map(|p| match p {
            ObjectNamePart::Identifier(i) => Some(i),
            ObjectNamePart::Function(_) => None,
        })
        .collect();
    if parts.len() != name.0.len() {
        return Err(crate::Error::Serialization(
            "unsupported function name shape in expression".to_owned(),
        ));
    }
    Ok(parts
        .iter()
        .map(|i| quote_ident(&folded(&i.value, i.quote_style.is_some())))
        .collect::<Vec<_>>()
        .join("."))
}

fn render_binary_op(op: &BinaryOperator) -> Result<&'static str> {
    use BinaryOperator::{Plus, Minus, Multiply, Divide, Modulo, Gt, Lt, GtEq, LtEq, Eq, NotEq, And, Or, StringConcat};
    Ok(match op {
        Plus => "+",
        Minus => "-",
        Multiply => "*",
        Divide => "/",
        Modulo => "%",
        Gt => ">",
        Lt => "<",
        GtEq => ">=",
        LtEq => "<=",
        Eq => "=",
        NotEq => "<>",
        And => "and",
        Or => "or",
        StringConcat => "||",
        other => {
            return Err(crate::Error::Serialization(format!(
                "unsupported binary operator {other:?} in expression"
            )))
        }
    })
}

fn render_value(value: &sqlparser::ast::Value) -> Result<String> {
    use sqlparser::ast::Value::{Number, SingleQuotedString, EscapedStringLiteral, UnicodeStringLiteral, DollarQuotedString, SingleQuotedByteStringLiteral, Boolean, Null};
    match value {
        Number(n, _) => Ok(n.clone()),
        SingleQuotedString(s) | EscapedStringLiteral(s) => Ok(render_string_literal(s)),
        UnicodeStringLiteral(s) => Ok(render_string_literal(s)),
        DollarQuotedString(dq) => {
            let raw = &dq.value;
            if raw.contains('$') || raw.is_empty() {
                Ok(render_string_literal(raw))
            } else {
                Ok(format!("$pgd${raw}$pgd$"))
            }
        }
        SingleQuotedByteStringLiteral(s) => Ok(render_string_literal(s)),
        Boolean(b) => Ok(if *b { "true" } else { "false" }.to_owned()),
        Null => Ok("null".to_owned()),
        other => Err(crate::Error::Serialization(format!(
            "unsupported literal {other:?} in expression"
        ))),
    }
}

/// Renders a string literal with single quotes, doubling embedded quotes
/// (PostgreSQL docs §4.1.2.1).
pub(crate) fn render_string_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Formats a type for use inside `CAST(x AS <type>)`.
pub(crate) fn render_cast_type(data_type: &DataType) -> Result<String> {
    crate::sql::datatype::render_type_for_expr(data_type)
}
