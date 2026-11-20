//! Sources: how a `--from`/`--to` argument becomes a catalog.
//!
//! Milestone 1 implements the SQL-file and SQL-directory sources: the
//! files are applied to a throwaway in-memory model exactly as the SPEC
//! describes — parse DDL statements, apply `ALTER`s in order. A
//! directory source walks recursively, reads every `.sql` file in
//! deterministic (sorted by relative path) order, and concatenates them;
//! statement indices in error messages stay honest across the
//! concatenation.

use std::path::{Path, PathBuf};

use crate::apply::apply_statements;
use crate::error::{Error, Result};
use crate::model::Catalog;
use crate::sql;

/// A source specification from the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceSpec {
    /// A live database connection string. Parsing is implemented in
    /// milestone 1; loading is milestone 2 (live introspection).
    Postgres(String),
    /// A single `.sql` file.
    SqlFile(PathBuf),
    /// A directory of `.sql` files (the "declarative schema" source).
    SqlDir(PathBuf),
}

impl SourceSpec {
    /// Parses a `--from`/`--to` argument.
    ///
    /// `postgres://`/`postgresql://` URLs select the (future) live
    /// source; an existing directory selects the directory source;
    /// anything else is treated as a file path.
    ///
    /// # Errors
    ///
    /// Returns an error when the path exists as neither a file nor a
    /// directory.
    pub fn parse(spec: &str) -> Result<Self> {
        let trimmed = spec.trim();
        if trimmed.starts_with("postgres://") || trimmed.starts_with("postgresql://") {
            return Ok(SourceSpec::Postgres(trimmed.to_owned()));
        }
        let path = Path::new(trimmed);
        if path.is_dir() {
            return Ok(SourceSpec::SqlDir(path.to_owned()));
        }
        if path.is_file() {
            return Ok(SourceSpec::SqlFile(path.to_owned()));
        }
        Err(Error::Io {
            context: format!("source {spec:?} is neither an existing file nor a directory"),
            inner: std::io::Error::new(std::io::ErrorKind::NotFound, "no such file or directory"),
        })
    }

    /// Human-readable description for plan headers and diagnostics.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            SourceSpec::Postgres(url) => url.clone(),
            SourceSpec::SqlFile(p) => p.display().to_string(),
            SourceSpec::SqlDir(p) => format!("{}/ (directory)", p.display()),
        }
    }
}

/// Loads a catalog from a source.
///
/// # Errors
///
/// Returns an error for the not-yet-implemented live source, for I/O
/// failures, and when the SQL is invalid, unsupported or inconsistent
/// (mirroring PostgreSQL's own rejections — see `docs/parser.md`).
pub fn load_catalog(spec: &SourceSpec) -> Result<Catalog> {
    match spec {
        SourceSpec::Postgres(url) => Err(Error::Unsupported {
            origin: url.clone(),
            index: 0,
            message: "live database sources are planned for milestone 2 (tokio-postgres \
                      introspection over pg_catalog); milestone 1 supports .sql files and \
                      directories"
                .to_owned(),
            statement: String::new(),
        }),
        SourceSpec::SqlFile(path) => {
            let text = read_file(path)?;
            let name = path.display().to_string();
            catalog_from_sql(&text, &name)
        }
        SourceSpec::SqlDir(dir) => {
            let files = collect_sql_files(dir)?;
            let mut full = String::new();
            for file in &files {
                let text = read_file(file)?;
                if !full.is_empty() {
                    full.push_str("\n\n");
                }
                full.push_str(&text);
            }
            catalog_from_sql(&full, &dir.display().to_string())
        }
    }
}

/// Applies SQL text to a fresh catalog.
///
/// # Errors
///
/// Returns an error when the SQL is invalid, unsupported or inconsistent
/// (mirroring PostgreSQL's own rejections — see `docs/parser.md`).
pub fn catalog_from_sql(sql_text: &str, source_name: &str) -> Result<Catalog> {
    let mut catalog = Catalog::new();
    apply_sql_to(&mut catalog, sql_text, source_name)?;
    Ok(catalog)
}

/// Applies SQL text to an existing catalog (the convergence oracle:
/// applying a generated migration script to the source model must
/// reproduce the target model).
///
/// # Errors
///
/// Returns an error when the SQL is invalid, unsupported or inconsistent
/// (mirroring PostgreSQL's own rejections — see `docs/parser.md`).
pub fn apply_sql_to(catalog: &mut Catalog, sql_text: &str, source_name: &str) -> Result<()> {
    let statements = sql::parse_source(source_name, sql_text)?;
    apply_statements(catalog, source_name, &statements, 1)
}

/// Splits SQL text into individual statement texts, stripping a leading
/// UTF-8 BOM.
///
/// The live-database integration tests (and, later, `pgdelta lint`) use
/// this to execute a source statement by statement: a multi-statement
/// simple-query message runs inside one implicit transaction on the
/// server (PostgreSQL docs, `CREATE INDEX`: `CREATE INDEX CONCURRENTLY`
/// "cannot run inside a transaction block"), so sources that mix session
/// statements and concurrent index builds must be split first.
///
/// # Errors
///
/// Returns an error when the text cannot be tokenized.
pub fn split_sql_statements(sql_text: &str) -> Result<Vec<String>> {
    let sql_text = sql_text.strip_prefix('\u{feff}').unwrap_or(sql_text);
    let statements = sql::tokens::split_statements(sql_text).map_err(|e| {
        Error::Parse {
            origin: "<sql>".to_owned(),
            index: 0,
            message: format!("tokenization failed: {e}"),
            statement: String::new(),
        }
    })?;
    Ok(statements.iter().map(|s| s.text()).collect())
}

fn read_file(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|inner| Error::Io {
        context: format!("reading {}", path.display()),
        inner,
    })
}

/// Every `.sql` file under `dir`, sorted by relative path (the
/// deterministic order in which a directory source applies them).
fn collect_sql_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(current) = stack.pop() {
        let entries = std::fs::read_dir(&current).map_err(|inner| Error::Io {
            context: format!("reading directory {}", current.display()),
            inner,
        })?;
        for entry in entries {
            let entry = entry.map_err(|inner| Error::Io {
                context: format!("reading directory {}", current.display()),
                inner,
            })?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("sql")) {
                let rel = path
                    .strip_prefix(dir).map_or_else(|_| path.to_string_lossy().into_owned(), |p| p.to_string_lossy().replace('\\', "/"));
                files.push((rel, path));
            }
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files.into_iter().map(|(_, p)| p).collect())
}
