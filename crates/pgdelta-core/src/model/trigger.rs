//! Triggers on tables and views.
//!
//! Grammar and rules follow the PostgreSQL 18 documentation,
//! <https://www.postgresql.org/docs/18/sql-createtrigger.html>:
//! trigger names are per-table, `INSTEAD OF` triggers must be `FOR EACH
//! ROW` on views, and constraint triggers must be `AFTER ... FOR EACH
//! ROW`. The trigger function is referenced by name; pgdelta records it so
//! dependency ordering can place routine creation first.

use serde::Serialize;

use super::Deferrable;

/// When a trigger fires (`BEFORE`, `AFTER`, `INSTEAD OF`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum TriggerTiming {
    /// `BEFORE`.
    Before,
    /// `AFTER`.
    After,
    /// `INSTEAD OF`.
    InsteadOf,
}

impl TriggerTiming {
    /// Renders the keyword.
    #[must_use]
    pub const fn as_sql(self) -> &'static str {
        match self {
            TriggerTiming::Before => "BEFORE",
            TriggerTiming::After => "AFTER",
            TriggerTiming::InsteadOf => "INSTEAD OF",
        }
    }
}

/// A trigger event; `UPDATE` may carry a column list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum TriggerEvent {
    /// `INSERT`.
    Insert,
    /// `UPDATE` or `UPDATE OF (columns)`.
    Update(Vec<String>),
    /// `DELETE`.
    Delete,
    /// `TRUNCATE`.
    Truncate,
}

impl TriggerEvent {
    /// Renders the event clause.
    #[must_use]
    pub fn as_sql(&self) -> String {
        match self {
            TriggerEvent::Insert => "INSERT".to_owned(),
            TriggerEvent::Update(columns) if columns.is_empty() => "UPDATE".to_owned(),
            TriggerEvent::Update(columns) => {
                let cols = columns
                    .iter()
                    .map(|c| crate::naming::quote_ident(c))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("UPDATE OF {cols}")
            }
            TriggerEvent::Delete => "DELETE".to_owned(),
            TriggerEvent::Truncate => "TRUNCATE".to_owned(),
        }
    }
}

/// Row- or statement-level trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum TriggerLevel {
    /// `FOR EACH ROW`.
    Row,
    /// `FOR EACH STATEMENT` (the default).
    Statement,
}

/// A trigger attached to a table or view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Trigger {
    /// Schema of the relation (and of the trigger).
    pub schema: String,
    /// Name of the relation the trigger is on.
    pub table: String,
    /// Trigger name (unique per relation).
    pub name: String,
    /// Firing time.
    pub timing: TriggerTiming,
    /// Events, always in canonical order.
    pub events: Vec<TriggerEvent>,
    /// Row or statement level.
    pub level: TriggerLevel,
    /// Trigger function, `(schema, name)`.
    pub function: (String, String),
    /// Literal arguments passed to the function.
    pub function_args: Vec<String>,
    /// `WHEN (condition)`, if given.
    pub condition: Option<String>,
    /// Whether this is a `CONSTRAINT` trigger.
    pub constraint: bool,
    /// Deferrability (constraint triggers only).
    pub deferrable: Option<Deferrable>,
    /// `ALTER TRIGGER ... ENABLE/DISABLE` state (`ENABLE` by default).
    pub enabled: bool,
}
