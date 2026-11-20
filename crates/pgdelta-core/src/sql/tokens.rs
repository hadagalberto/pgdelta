//! Token-level SQL utilities built on sqlparser's tokenizer.
//!
//! pgdelta tokenizes every source with sqlparser's PostgreSQL tokenizer
//! (so quoting rules, dollar-quoting and comments behave exactly like the
//! parser's view of the text), then splits statements and drives its own
//! small grammars for the PostgreSQL-specific forms sqlparser-rs cannot
//! parse. See `docs/parser.md` for the full rationale and the verified
//! support matrix.

use crate::naming;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::tokenizer::Token;

/// A single SQL statement: its tokens (whitespace stripped) plus
/// helpers.
#[derive(Debug, Clone)]
pub(crate) struct StatementTokens {
    /// The statement's tokens.
    pub tokens: Vec<Token>,
}

impl StatementTokens {
    /// Best-effort reconstruction of the statement text for diagnostics.
    pub fn text(&self) -> String {
        join_tokens(&self.tokens)
    }

    /// True when the statement has no tokens (blank).
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }
}

/// Tokenizes `sql` and splits it into statements at top-level semicolons.
///
/// Semicolons inside string literals, quoted identifiers or dollar-quoted
/// strings are single tokens and never split statements. Whitespace is
/// dropped (comments are not semantically relevant to the catalog model).
pub(crate) fn split_statements(
    sql: &str,
) -> Result<Vec<StatementTokens>, sqlparser::tokenizer::TokenizerError> {
    let mut tokens = Vec::new();
    for tok in
        sqlparser::tokenizer::Tokenizer::new(&PostgreSqlDialect {}, sql).tokenize_with_location()?
    {
        if !matches!(tok.token, Token::Whitespace(_)) {
            tokens.push(tok.token);
        }
    }

    let mut statements = Vec::new();
    let mut current = Vec::new();
    for tok in tokens {
        if tok == Token::SemiColon {
            statements.push(StatementTokens { tokens: current });
            current = Vec::new();
        } else {
            current.push(tok);
        }
    }
    if !current.is_empty() {
        statements.push(StatementTokens { tokens: current });
    }
    Ok(statements.into_iter().filter(|s| !s.is_empty()).collect())
}

/// Joins tokens back into SQL text that sqlparser can re-parse.
///
/// Spacing follows adjacency rules (no space around `.`/`::`, none before
/// `,`/`)`/`]`, none after `(`/`[`). The output is not meant to be
/// pretty — it feeds the type/expression lifts, which re-parse it with
/// sqlparser itself.
pub(crate) fn join_tokens(tokens: &[Token]) -> String {
    use std::fmt::Write;

    let mut out = String::new();
    let mut prev_needs_space = false;
    for tok in tokens {
        let no_space_before = matches!(
            tok,
            Token::Comma | Token::RParen | Token::RBracket | Token::Period | Token::DoubleColon
        );
        let no_space_after_prev = matches!(
            tok,
            Token::LParen | Token::LBracket | Token::Period | Token::DoubleColon
        );
        if prev_needs_space && !no_space_before && !no_space_after_prev {
            out.push(' ');
        }
        let _ = write!(out, "{tok}");
        prev_needs_space = !matches!(
            tok,
            Token::LParen | Token::LBracket | Token::Period | Token::DoubleColon
        );
    }
    out
}

/// A simple token cursor over a statement.
pub(crate) struct Cursor<'a> {
    tokens: &'a [Token],
    pos: usize,
}

impl<'a> Cursor<'a> {
    /// Creates a cursor over `tokens`.
    pub fn new(tokens: &'a [Token]) -> Self {
        Cursor { tokens, pos: 0 }
    }

    /// Current position.
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// Advances `n` tokens without inspecting them.
    pub fn advance(&mut self, n: usize) {
        self.pos = (self.pos + n).min(self.tokens.len());
    }

    /// Peeks at the current token without consuming it.
    pub fn peek(&self) -> Option<&'a Token> {
        self.tokens.get(self.pos)
    }

    /// Peeks `n` tokens ahead.
    pub fn peek_nth(&self, n: usize) -> Option<&'a Token> {
        self.tokens.get(self.pos + n)
    }

    /// Consumes and returns the current token.
    pub fn next(&mut self) -> Option<&'a Token> {
        let tok = self.tokens.get(self.pos);
        if tok.is_some() {
            self.pos += 1;
        }
        tok
    }

    /// True when the current token is the (case-insensitive) unquoted
    /// word `word`.
    pub fn at_word(&self, word: &str) -> bool {
        matches!(self.peek(), Some(Token::Word(w)) if w.quote_style.is_none() && w.value.eq_ignore_ascii_case(word))
    }

    /// If the current token is the word `word`, consumes it and returns
    /// true.
    pub fn eat_word(&mut self, word: &str) -> bool {
        if self.at_word(word) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Consumes `w1 w2` if both match; returns false (position unchanged)
    /// otherwise.
    pub fn eat_words2(&mut self, w1: &str, w2: &str) -> bool {
        if self.at_word(w1)
            && matches!(self.peek_nth(1), Some(Token::Word(w)) if w.quote_style.is_none() && w.value.eq_ignore_ascii_case(w2))
        {
            self.pos += 2;
            true
        } else {
            false
        }
    }

    /// Consumes all of `words` if they match consecutively.
    pub fn eat_words(&mut self, words: &[&str]) -> bool {
        let start = self.pos;
        for w in words {
            if !self.eat_word(w) {
                self.pos = start;
                return false;
            }
        }
        true
    }

    /// True when the current token is `(`.
    pub fn at_lparen(&self) -> bool {
        matches!(self.peek(), Some(Token::LParen))
    }

    /// Consumes a `(` if present.
    pub fn eat_lparen(&mut self) -> bool {
        if self.at_lparen() {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Consumes a `)` if present.
    pub fn eat_rparen(&mut self) -> bool {
        if matches!(self.peek(), Some(Token::RParen)) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Consumes a `,` if present.
    pub fn eat_comma(&mut self) -> bool {
        if matches!(self.peek(), Some(Token::Comma)) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Parses a possibly-qualified identifier (parts joined with `.`).
    ///
    /// Unquoted parts fold to lower case; quoted parts keep their case
    /// (PostgreSQL docs §4.1.1). Returns the folded parts.
    pub fn parse_object_name(&mut self) -> Option<Vec<String>> {
        let mut parts = Vec::new();
        if let Some(Token::Word(w)) = self.peek() {
            parts.push(naming::fold_ident(&w.value, w.quote_style.is_some()));
            self.pos += 1;
        } else {
            return None;
        }
        while matches!(self.peek(), Some(Token::Period)) {
            self.pos += 1;
            match self.next() {
                Some(Token::Word(w)) => {
                    parts.push(naming::fold_ident(&w.value, w.quote_style.is_some()));
                }
                // `t.*` never appears in DDL positions we parse.
                _ => return None,
            }
        }
        Some(parts)
    }

    /// Parses a single (unqualified) identifier.
    pub fn parse_ident(&mut self) -> Option<String> {
        match self.peek() {
            Some(Token::Word(w)) => {
                let folded = naming::fold_ident(&w.value, w.quote_style.is_some());
                self.pos += 1;
                Some(folded)
            }
            _ => None,
        }
    }

    /// Consumes tokens until one of the boundary words (matched
    /// case-insensitively at depth 0), a top-level `,`/`)`, or the end,
    /// returning the smart-joined text. The boundary token is not
    /// consumed.
    ///
    /// Depth counts parentheses and brackets. A boundary word inside
    /// grouping does not terminate the capture (needed for expressions
    /// that contain keywords, e.g. `CAST(x AS integer)`). Only unquoted
    /// words act as boundaries.
    pub fn capture_until_words(&mut self, boundaries: &[&str]) -> String {
        let start = self.pos;
        let mut depth = 0usize;
        loop {
            match self.peek() {
                None => break,
                Some(Token::LParen | Token::LBracket) => {
                    depth += 1;
                    self.pos += 1;
                }
                Some(Token::RParen | Token::RBracket) => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                    self.pos += 1;
                }
                Some(Token::Word(w))
                    if depth == 0
                        && w.quote_style.is_none()
                        && boundaries.iter().any(|b| w.value.eq_ignore_ascii_case(b)) =>
                {
                    break;
                }
                Some(Token::Comma) if depth == 0 => break,
                _ => self.pos += 1,
            }
        }
        join_tokens(&self.tokens[start..self.pos])
    }

    /// True when the remaining tokens are exhausted.
    pub fn is_empty(&self) -> bool {
        self.pos >= self.tokens.len()
    }

    /// The statement's tokens (for diagnostics and lifts).
    pub fn tokens(&self) -> &'a [Token] {
        self.tokens
    }

    /// Captures a parenthesised list of identifiers: consumes `(`, then
    /// comma-separated identifiers, then `)`. Returns `None` on a shape
    /// mismatch (consuming nothing).
    pub fn parse_ident_list_paren(&mut self) -> Option<Vec<String>> {
        let start = self.pos;
        if !self.eat_lparen() {
            return None;
        }
        let mut out = Vec::new();
        loop {
            let Some(ident) = self.parse_ident() else {
                self.pos = start;
                return None;
            };
            out.push(ident);
            if self.eat_comma() {
                continue;
            }
            if self.eat_rparen() {
                break;
            }
            self.pos = start;
            return None;
        }
        Some(out)
    }

    /// Captures a balanced `(...)` group (both parens consumed) and
    /// returns the smart-joined inner text.
    pub fn capture_paren_group(&mut self) -> Option<String> {
        if !self.at_lparen() {
            return None;
        }
        let start = self.pos;
        self.pos += 1;
        let mut depth = 1usize;
        while let Some(tok) = self.peek() {
            if matches!(tok, Token::LParen | Token::LBracket) {
                depth += 1;
            } else if matches!(tok, Token::RParen | Token::RBracket) {
                depth -= 1;
                if depth == 0 {
                    let inner = join_tokens(&self.tokens[start + 1..self.pos]);
                    self.pos += 1;
                    return Some(inner);
                }
            }
            self.pos += 1;
        }
        None
    }
}

/// Parses a signed integer literal from the current token.
pub(crate) fn eat_i64(cursor: &mut Cursor) -> Option<i64> {
    let sign = if matches!(cursor.peek(), Some(Token::Plus)) {
        cursor.pos += 1;
        1i64
    } else if matches!(cursor.peek(), Some(Token::Minus)) {
        cursor.pos += 1;
        -1i64
    } else {
        1i64
    };
    match cursor.peek() {
        Some(Token::Number(n, _)) => {
            let value: i64 = n.parse().ok()?;
            cursor.pos += 1;
            Some(sign * value)
        }
        _ => None,
    }
}

/// Parses a string literal (single-quoted) token value.
#[allow(dead_code)]
pub(crate) fn eat_string_literal(cursor: &mut Cursor) -> Option<String> {
    match cursor.peek() {
        Some(Token::SingleQuotedString(s)) => {
            let s = s.clone();
            cursor.pos += 1;
            Some(s)
        }
        _ => None,
    }
}
