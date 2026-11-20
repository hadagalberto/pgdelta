//! Row-level-security policies and table RLS flags.
//!
//! Grammar and semantics follow the PostgreSQL 18 documentation,
//! <https://www.postgresql.org/docs/18/sql-createpolicy.html>: policies are
//! named per table, `PERMISSIVE` is the default, `ALL` is the default
//! command, `PUBLIC` is the default role list, `USING` filters existing
//! rows and `WITH CHECK` validates new rows. RLS must be enabled with
//! `ALTER TABLE ... ENABLE ROW LEVEL SECURITY` for policies to apply.

use serde::Serialize;

/// Command a policy applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum PolicyCommand {
    /// `ALL` (the default).
    All,
    /// `SELECT`.
    Select,
    /// `INSERT`.
    Insert,
    /// `UPDATE`.
    Update,
    /// `DELETE`.
    Delete,
}

impl PolicyCommand {
    /// Renders the keyword.
    #[must_use]
    pub const fn as_sql(self) -> &'static str {
        match self {
            PolicyCommand::All => "ALL",
            PolicyCommand::Select => "SELECT",
            PolicyCommand::Insert => "INSERT",
            PolicyCommand::Update => "UPDATE",
            PolicyCommand::Delete => "DELETE",
        }
    }
}

/// A row-level-security policy on a table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Policy {
    /// Schema owning the table.
    pub schema: String,
    /// Table the policy applies to.
    pub table: String,
    /// Policy name (unique per table).
    pub name: String,
    /// `PERMISSIVE` (true, the default) vs `RESTRICTIVE`.
    pub permissive: bool,
    /// Command the policy applies to.
    pub command: PolicyCommand,
    /// Roles the policy applies to (`PUBLIC` is stored literally).
    pub roles: Vec<String>,
    /// `USING (expression)`.
    pub using: Option<String>,
    /// `WITH CHECK (expression)`.
    pub check: Option<String>,
}
