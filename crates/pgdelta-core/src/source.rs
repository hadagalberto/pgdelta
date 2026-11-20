//! Sources: how a `--from`/`--to` argument becomes a catalog.
//!
//! SQL-file and SQL-directory sources are applied to a throwaway in-memory
//! model exactly as the SPEC describes — parse DDL statements, apply `ALTER`s
//! in order. A directory source walks recursively, reads every `.sql` file in
//! deterministic (sorted by relative path) order, and concatenates them;
//! statement indices in error messages stay honest across the concatenation.
//!
//! A `postgres://` source is introspected live through `pg_catalog`. When
//! `--shadow postgres://...` is supplied for a SQL source, the SQL is instead
//! executed against a fresh throwaway database and the resulting catalog is
//! introspected — the SPEC's most accurate mode, because PostgreSQL itself
//! validates and normalises every statement (including expression deparsing,
//! generated names and constraint inference).

use std::path::{Path, PathBuf};

use crate::apply::apply_statements;
use crate::error::{Error, Result};
use crate::introspect;
use crate::model::Catalog;
use crate::sql;

/// A source specification from the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceSpec {
    /// A live database connection string, introspected through `pg_catalog`.
    Postgres(String),
    /// A single `.sql` file.
    SqlFile(PathBuf),
    /// A directory of `.sql` files (the "declarative schema" source).
    SqlDir(PathBuf),
}

impl SourceSpec {
    /// Parses a `--from`/`--to` argument.
    ///
    /// `postgres://`/`postgresql://` URLs select the live source; an
    /// existing directory selects the directory source; anything else is
    /// treated as a file path.
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
/// Returns an error for I/O failures, invalid SQL, or a failed live
/// introspection (see [`introspect`]).
pub fn load_catalog(spec: &SourceSpec) -> Result<Catalog> {
    match spec {
        SourceSpec::Postgres(url) => introspect::catalog_from_url_blocking(url),
        SourceSpec::SqlFile(path) => {
            let text = read_file(path)?;
            let name = path.display().to_string();
            catalog_from_sql(&text, &name)
        }
        SourceSpec::SqlDir(dir) => {
            catalog_from_sql(&sql_dir_text(dir)?, &dir.display().to_string())
        }
    }
}

/// Async counterpart of [`load_catalog_with_shadow`], for callers that
/// already run inside a Tokio runtime (notably the integration tests).
///
/// # Errors
///
/// See [`load_catalog_with_shadow`].
pub async fn load_catalog_with_shadow_async(
    spec: &SourceSpec,
    shadow_url: Option<&str>,
) -> Result<Catalog> {
    match (spec, shadow_url) {
        (SourceSpec::Postgres(url), _) => introspect::catalog_from_url(url).await,
        (_, None) => load_catalog(spec),
        (SourceSpec::SqlFile(_) | SourceSpec::SqlDir(_), Some(url)) => {
            shadow_catalog(spec, url).await
        }
    }
}

/// Loads a catalog from a source, using a shadow PostgreSQL database for SQL
/// sources when `shadow_url` is `Some`. Live sources are introspected
/// directly; the shadow URL is ignored for them.
///
/// The shadow URL must point to a database whose role can `CREATE DATABASE`.
/// A dedicated shadow database is created on first use and emptied before
/// every load, so no state leaks between sources.
///
/// # Errors
///
/// See [`load_catalog`] and [`shadow_catalog`].
pub fn load_catalog_with_shadow(spec: &SourceSpec, shadow_url: Option<&str>) -> Result<Catalog> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|inner| Error::Internal {
            message: format!("building the source runtime: {inner}"),
        })?;
    runtime.block_on(load_catalog_with_shadow_async(spec, shadow_url))
}

fn sql_dir_text(dir: &Path) -> Result<String> {
    let files = collect_sql_files(dir)?;
    let mut full = String::new();
    for file in &files {
        let text = read_file(file)?;
        if !full.is_empty() {
            full.push_str("\n\n");
        }
        full.push_str(&text);
    }
    Ok(full)
}

/// Name of the dedicated database used for shadow-mode loads. It is created
/// on first use and emptied before every load, so no state leaks between
/// sources without the cost of creating a database per source.
pub const SHADOW_DATABASE: &str = "pgdelta_shadow";

fn with_database(base: &str, database: &str) -> String {
    let (scheme, rest) = base.split_once("://").unwrap_or(("postgres", base));
    let (authority, tail) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    let query = tail.find('?').map_or("", |index| &tail[index..]);
    format!("{scheme}://{authority}/{database}{query}")
}

/// Applies a SQL source to the dedicated shadow database, introspects it,
/// and returns the model. The database is emptied before every load: user
/// schemas (with all their objects) and non-default extensions are dropped.
async fn shadow_catalog(spec: &SourceSpec, shadow_url: &str) -> Result<Catalog> {
    let sql_text = match spec {
        SourceSpec::SqlFile(path) => read_file(path)?,
        SourceSpec::SqlDir(dir) => sql_dir_text(dir)?,
        SourceSpec::Postgres(url) => return introspect::catalog_from_url(url).await,
    };
    let admin_url = shadow_url.to_owned();
    let (admin, admin_driver) = introspect::connect(&admin_url).await?;
    let check = format!("SELECT 1 FROM pg_database WHERE datname = '{SHADOW_DATABASE}'");
    let exists = admin
        .query_opt(&check, &[])
        .await
        .map_err(|inner| Error::Database {
            context: "checking for the shadow database".to_owned(),
            inner,
        })?;
    if exists.is_none() {
        let create = format!("CREATE DATABASE \"{SHADOW_DATABASE}\"");
        admin
            .execute(&create, &[])
            .await
            .map_err(|inner| Error::Database {
                context: format!("creating shadow database {SHADOW_DATABASE}"),
                inner,
            })?;
    }
    drop(admin);
    admin_driver.abort();

    let shadow_db_url = with_database(&admin_url, SHADOW_DATABASE);
    let description = spec.describe();
    let (client, driver) = introspect::connect(&shadow_db_url).await?;
    let result = async {
        reset_database(&client).await?;
        for statement in split_sql_statements(&sql_text)? {
            client
                .batch_execute(&statement)
                .await
                .map_err(|inner| Error::Database {
                    context: format!("applying {description} to the shadow database"),
                    inner,
                })?;
        }
        introspect::introspect(&client).await
    }
    .await;
    drop(client);
    driver.abort();
    result
}

/// Empties all user objects from a database. System schemas and `public`
/// (which is recreated empty) are left in place.
async fn reset_database(client: &tokio_postgres::Client) -> Result<()> {
    let extensions = client
        .query(
            "SELECT extname FROM pg_extension WHERE extname <> 'plpgsql'",
            &[],
        )
        .await
        .map_err(|inner| Error::Database {
            context: "listing shadow extensions".to_owned(),
            inner,
        })?;
    for row in &extensions {
        let name: String = row.get(0);
        let sql = format!("DROP EXTENSION IF EXISTS \"{name}\" CASCADE");
        client
            .batch_execute(&sql)
            .await
            .map_err(|inner| Error::Database {
                context: format!("dropping shadow extension {name}"),
                inner,
            })?;
    }
    let schemas = client
        .query(
            "SELECT nspname FROM pg_namespace \
             WHERE nspname NOT IN ('pg_catalog', 'information_schema', 'public') \
               AND nspname NOT LIKE 'pg\\_%'",
            &[],
        )
        .await
        .map_err(|inner| Error::Database {
            context: "listing shadow schemas".to_owned(),
            inner,
        })?;
    for row in &schemas {
        let name: String = row.get(0);
        let sql = format!("DROP SCHEMA IF EXISTS \"{name}\" CASCADE");
        client
            .batch_execute(&sql)
            .await
            .map_err(|inner| Error::Database {
                context: format!("dropping shadow schema {name}"),
                inner,
            })?;
    }
    client
        .batch_execute("DROP SCHEMA IF EXISTS public CASCADE; CREATE SCHEMA public")
        .await
        .map_err(|inner| Error::Database {
            context: "resetting the public schema".to_owned(),
            inner,
        })
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
    let statements = sql::tokens::split_statements(sql_text).map_err(|e| Error::Parse {
        origin: "<sql>".to_owned(),
        index: 0,
        message: format!("tokenization failed: {e}"),
        statement: String::new(),
    })?;
    Ok(statements
        .iter()
        .map(crate::sql::tokens::StatementTokens::text)
        .collect())
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
            } else if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("sql"))
            {
                let rel = path.strip_prefix(dir).map_or_else(
                    |_| path.to_string_lossy().into_owned(),
                    |p| p.to_string_lossy().replace('\\', "/"),
                );
                files.push((rel, path));
            }
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files.into_iter().map(|(_, p)| p).collect())
}
