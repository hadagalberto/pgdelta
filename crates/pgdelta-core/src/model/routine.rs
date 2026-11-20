//! Functions and procedures, including overloads.
//!
//! PostgreSQL identifies a routine by `(schema, name, input argument
//! types)`; OUT parameters do not participate in the identity. This is
//! documented under "Overloading" in the PostgreSQL 18 `CREATE FUNCTION`
//! page, <https://www.postgresql.org/docs/18/sql-createfunction.html>.
//! `CREATE PROCEDURE` shares the same synopsis with a different verb.

use serde::Serialize;

/// Argument mode (`CREATE FUNCTION`, `argmode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum ArgMode {
    /// `IN` (the default).
    In,
    /// `OUT`.
    Out,
    /// `INOUT`.
    InOut,
    /// `VARIADIC`.
    Variadic,
}

impl ArgMode {
    /// Renders the mode, omitting the default `IN`.
    #[must_use]
    pub const fn as_sql(self) -> &'static str {
        match self {
            ArgMode::In => "",
            ArgMode::Out => "OUT ",
            ArgMode::InOut => "INOUT ",
            ArgMode::Variadic => "VARIADIC ",
        }
    }

    /// True when this argument participates in the routine's identity.
    #[must_use]
    pub const fn is_input(self) -> bool {
        matches!(self, ArgMode::In | ArgMode::InOut | ArgMode::Variadic)
    }
}

/// One routine argument.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RoutineArg {
    /// Argument mode.
    pub mode: ArgMode,
    /// Argument name, if given.
    pub name: Option<String>,
    /// Canonical argument type.
    pub data_type: String,
    /// Default expression, if given.
    pub default: Option<String>,
}

/// Function volatility (`CREATE FUNCTION`, `IMMUTABLE|STABLE|VOLATILE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Volatility {
    /// `VOLATILE` (the default).
    Volatile,
    /// `STABLE`.
    Stable,
    /// `IMMUTABLE`.
    Immutable,
}

impl Volatility {
    /// Renders the keyword.
    #[must_use]
    pub const fn as_sql(self) -> &'static str {
        match self {
            Volatility::Volatile => "VOLATILE",
            Volatility::Stable => "STABLE",
            Volatility::Immutable => "IMMUTABLE",
        }
    }
}

/// Security context of a routine (`SECURITY INVOKER|DEFINER`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Security {
    /// `SECURITY INVOKER` (the default).
    Invoker,
    /// `SECURITY DEFINER`.
    Definer,
}

/// A function or procedure.
///
/// The boolean fields mirror independent `CREATE FUNCTION` attributes; a
/// state machine would obscure the one-to-one mapping to the SQL synopsis.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Routine {
    /// Schema owning the routine.
    pub schema: String,
    /// Routine name.
    pub name: String,
    /// Function or procedure (they share names but are distinct objects).
    pub kind: RoutineKind,
    /// Arguments in declaration order.
    pub args: Vec<RoutineArg>,
    /// Explicit return type; `None` when implied by OUT parameters.
    pub return_type: Option<String>,
    /// `RETURNS SETOF`.
    pub returns_setof: bool,
    /// `RETURNS TABLE (column type, ...)` columns.
    pub return_table: Vec<(String, String)>,
    /// Implementation language, lower case.
    pub language: String,
    /// Function body source (string literal or dollar-quoted text).
    pub body: String,
    /// Volatility.
    pub volatility: Volatility,
    /// Security context.
    pub security: Security,
    /// `STRICT` / `RETURNS NULL ON NULL INPUT`.
    pub strict: bool,
    /// `LEAKPROOF`.
    pub leakproof: bool,
    /// `WINDOW`.
    pub window: bool,
    /// `PARALLEL` setting, if specified.
    pub parallel: Option<String>,
    /// `COST`, if specified.
    pub cost: Option<u64>,
    /// `ROWS`, if specified.
    pub rows: Option<u64>,
    /// `SET parameter = value` clauses, in declaration order.
    pub settings: Vec<(String, String)>,
}

/// Function vs procedure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum RoutineKind {
    /// `CREATE FUNCTION`.
    Function,
    /// `CREATE PROCEDURE`.
    Procedure,
}

impl RoutineKind {
    /// SQL keyword for the kind.
    #[must_use]
    pub const fn as_sql(self) -> &'static str {
        match self {
            RoutineKind::Function => "FUNCTION",
            RoutineKind::Procedure => "PROCEDURE",
        }
    }
}

impl Routine {
    /// The canonical input signature (comma-joined argument types),
    /// excluding OUT parameters — PostgreSQL's overload identity.
    #[must_use]
    pub fn signature(&self) -> String {
        self.args
            .iter()
            .filter(|arg| arg.mode.is_input())
            .map(|arg| arg.data_type.as_str())
            .collect::<Vec<_>>()
            .join(",")
    }

    /// Renders the argument list for CREATE/DROP statements.
    #[must_use]
    pub fn render_args(&self) -> String {
        self.args
            .iter()
            .map(|arg| {
                let mut out = String::from(arg.mode.as_sql());
                if let Some(name) = &arg.name {
                    out.push_str(&crate::naming::quote_ident(name));
                    out.push(' ');
                }
                out.push_str(&crate::plan::render_type(&arg.data_type));
                if let Some(default) = &arg.default {
                    out.push_str(" DEFAULT ");
                    out.push_str(default);
                }
                out
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Fully-qualified, quoted name.
    #[must_use]
    pub fn qualified_name(&self) -> String {
        crate::naming::qualified(&self.schema, &self.name)
    }
}
