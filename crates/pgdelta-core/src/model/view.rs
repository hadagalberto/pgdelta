//! Views and materialized views.
//!
//! Syntax and properties follow the PostgreSQL 18 documentation:
//! <https://www.postgresql.org/docs/18/sql-createview.html> and
//! <https://www.postgresql.org/docs/18/sql-creatematerializedview.html>.
//! The defining query is stored verbatim (tokens are re-joined with the
//! canonical spacing from `sql/tokens.rs`) so that a plan can emit a
//! dependency-safe `DROP VIEW`/`CREATE VIEW` pair when a referenced column
//! changes type.

use serde::Serialize;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::tokenizer::{Token, Tokenizer};

/// A view's `WITH [CASCADED|LOCAL] CHECK OPTION` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum CheckOption {
    /// No check option.
    None,
    /// `WITH LOCAL CHECK OPTION`.
    Local,
    /// `WITH CASCADED CHECK OPTION` (the default when unspecified).
    Cascaded,
}

/// A view or materialized view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct View {
    /// Schema owning the relation.
    pub schema: String,
    /// View name.
    pub name: String,
    /// Explicit output column names (`None`-equivalent is an empty list).
    pub columns: Vec<String>,
    /// Normalised defining query text (without a trailing semicolon).
    pub query: String,
    /// `WITH (...)` view/storage options, in declaration order.
    pub options: Vec<(String, String)>,
    /// `TABLESPACE` (materialized views only).
    pub tablespace: Option<String>,
    /// `USING method` access method (materialized views only).
    pub access_method: Option<String>,
    /// Check option (plain views only).
    pub check_option: CheckOption,
    /// Whether this is a materialized view.
    pub materialized: bool,
    /// `WITH DATA` (materialized views only); `WITH NO DATA` is the opposite.
    pub with_data: bool,
}

impl View {
    /// Relations the defining query reads from, resolved to
    /// `(schema, name)` with unqualified names in `public`.
    ///
    /// A token-level walk is used rather than a full query AST: DDL sources
    /// routinely contain dialect-specific constructs, and pgdelta only
    /// needs the relation identities for ordering and for deciding when a
    /// view must be dropped and recreated. Keywords in subqueries are
    /// handled because the walk is linear over the token stream.
    #[must_use]
    pub fn referenced_relations(&self) -> Vec<(String, String)> {
        let Ok(all_tokens) = Tokenizer::new(&PostgreSqlDialect {}, &self.query).tokenize() else {
            return Vec::new();
        };
        // The tokenizer keeps whitespace tokens; the walk only cares about
        // words and punctuation.
        let tokens: Vec<Token> = all_tokens
            .into_iter()
            .filter(|token| !matches!(token, Token::Whitespace(_)))
            .collect();
        let mut out = Vec::new();
        let mut i = 0;
        while i < tokens.len() {
            let is_source = matches!(&tokens[i], Token::Word(w) if w.quote_style.is_none()
                && matches!(w.value.to_ascii_uppercase().as_str(),
                    "FROM" | "JOIN" | "UPDATE" | "INTO"));
            if !is_source {
                i += 1;
                continue;
            }
            i += 1;
            // `FROM ONLY tbl`, `FROM LATERAL (...)`, etc.
            while matches!(&tokens.get(i), Some(Token::Word(w)) if w.quote_style.is_none()
                && matches!(w.value.to_ascii_uppercase().as_str(), "ONLY" | "LATERAL"))
            {
                i += 1;
            }
            let Some((parts, used)) = object_name_at(&tokens, i) else {
                continue;
            };
            i += used;
            if parts.is_empty() {
                continue;
            }
            let (schema, name) = if parts.len() >= 2 {
                (parts[0].clone(), parts[parts.len() - 1].clone())
            } else {
                (crate::model::PUBLIC_SCHEMA.to_owned(), parts[0].clone())
            };
            if !out.contains(&(schema.clone(), name.clone())) {
                out.push((schema, name));
            }
        }
        out
    }
}

/// Reads a possibly-qualified identifier starting at `i`; returns its
/// folded parts and the number of tokens consumed.
fn object_name_at(tokens: &[Token], i: usize) -> Option<(Vec<String>, usize)> {
    let mut parts = Vec::new();
    let mut at = i;
    loop {
        match tokens.get(at) {
            Some(Token::Word(w)) => {
                parts.push(crate::naming::fold_ident(&w.value, w.quote_style.is_some()));
                at += 1;
            }
            _ if parts.is_empty() => return None,
            _ => break,
        }
        if matches!(tokens.get(at), Some(Token::Period)) {
            at += 1;
        } else {
            break;
        }
    }
    Some((parts, at - i))
}
