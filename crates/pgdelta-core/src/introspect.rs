//! Live introspection of a PostgreSQL 13–18 server through `pg_catalog`.
//!
//! Every object kind modelled by [`Catalog`] is read back from the system
//! catalogs, not from `information_schema`, so that a live source and a SQL
//! file describing the same schema produce the same normalised model (the
//! SPEC's parity requirement). All expressions and type names returned by
//! `pg_catalog` are fed through the very same normalisers used by the SQL
//! parser (`sql::expr` and `sql::datatype`), which is what makes the two
//! representations comparable.
//!
//! Catalog columns and the deparsing functions used here are documented in
//! the PostgreSQL 18 reference:
//!
//! * <https://www.postgresql.org/docs/18/catalog-pg-class.html>
//! * <https://www.postgresql.org/docs/18/catalog-pg-attribute.html>
//! * <https://www.postgresql.org/docs/18/catalog-pg-constraint.html>
//! * <https://www.postgresql.org/docs/18/catalog-pg-index.html>
//! * <https://www.postgresql.org/docs/18/catalog-pg-proc.html>
//! * <https://www.postgresql.org/docs/18/functions-info.html> (`pg_get_expr`,
//!   `pg_get_viewdef`, `pg_get_indexdef`, `format_type`)
//!
//! The queries are written to work unchanged on PostgreSQL 13–18: columns
//! added in later releases (`connullsnotdistinct`, `indnullsnotdistinct`)
//! are read behind a server-version check, never referenced on older
//! servers.

use std::collections::{BTreeMap, BTreeSet};

use tokio::task::JoinHandle;
use tokio_postgres::{Client, NoTls, Row, Socket, types::ToSql};

use crate::error::{Error, Result};
use crate::model::{
    ArgMode, Catalog, CheckOption, Column, ColumnStorage, CompositeAttribute, Constraint,
    ConstraintKind, Deferrable, DomainConstraint, DomainConstraintKind, Extension, FkAction,
    FkMatch, Generated, Grant, Identity, IdentityKind, Index, IndexElem, IndexElemKind, NullsOrder,
    ObjectKind, ObjectRef, PUBLIC_SCHEMA, Partition, PartitionKey, PartitionStrategy, Partitioning,
    PgType, PgTypeKind, Policy, PolicyCommand, Routine, RoutineArg, RoutineKind, Schema, Security,
    Sequence, SequenceOptions, SequenceOwner, SortOrder, Table, Trigger, TriggerEvent,
    TriggerLevel, TriggerTiming, View, Volatility,
};

/// Connects to `url`. TLS is used when the connection string asks for it
/// (`sslmode=require`/`verify-full`/`verify-ca`, or `ssl=true`); otherwise
/// `NoTls` is used, which is what local development and the CI service
/// containers need. The rustls connector uses Mozilla's bundled root store.
///
/// # Errors
///
/// Returns a [`Error::Database`] when the connection cannot be established.
pub async fn connect(url: &str) -> Result<(Client, JoinHandle<()>)> {
    if tls_requested(url) {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let tls = tokio_postgres_rustls::MakeRustlsConnect::new(config);
        connect_with(url, tls).await
    } else {
        connect_with(url, NoTls).await
    }
}

fn tls_requested(url: &str) -> bool {
    let query = url.split_once('?').map_or("", |(_, q)| q);
    query.split('&').any(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        key.eq_ignore_ascii_case("sslmode")
            && matches!(
                value.to_ascii_lowercase().as_str(),
                "require" | "verify-ca" | "verify-full"
            )
    }) || query
        .split('&')
        .any(|pair| pair.eq_ignore_ascii_case("ssl=true"))
}

async fn connect_with<T>(url: &str, tls: T) -> Result<(Client, JoinHandle<()>)>
where
    T: tokio_postgres::tls::MakeTlsConnect<Socket> + Send + 'static,
    T::Stream: Send,
    T::TlsConnect: Send,
    <T::TlsConnect as tokio_postgres::tls::TlsConnect<Socket>>::Future: Send,
{
    let (client, connection) =
        tokio_postgres::connect(url, tls)
            .await
            .map_err(|inner| Error::Database {
                context: format!("connecting to {url}"),
                inner,
            })?;
    let driver = tokio::spawn(async move {
        if let Err(error) = connection.await {
            eprintln!("pgdelta: connection driver ended: {error}");
        }
    });
    Ok((client, driver))
}

/// Introspects an already-connected client. The connection must be to a
/// PostgreSQL server (13–18); no other database is supported.
///
/// # Errors
///
/// Returns a [`Error::Database`] for query failures and a
/// [`Error::Serialization`] when a catalog value cannot be normalised into
/// pgdelta's model.
pub async fn introspect(client: &Client) -> Result<Catalog> {
    let mut catalog = Catalog::new();
    catalog.ensure_schema(PUBLIC_SCHEMA);
    let major = server_major(client).await?;

    introspect_schemas(client, &mut catalog).await?;
    introspect_extensions(client, &mut catalog).await?;
    introspect_tables(client, &mut catalog).await?;
    let column_numbers = introspect_columns(client, &mut catalog).await?;
    introspect_identity_options(client, &mut catalog).await?;
    introspect_constraints(client, &mut catalog, major, &column_numbers).await?;
    introspect_indexes(client, &mut catalog, major).await?;
    introspect_types(client, &mut catalog).await?;
    introspect_sequences(client, &mut catalog).await?;
    introspect_views(client, &mut catalog).await?;
    introspect_routines(client, &mut catalog).await?;
    introspect_triggers(client, &mut catalog).await?;
    introspect_policies(client, &mut catalog).await?;
    introspect_comments(client, &mut catalog).await?;
    introspect_grants(client, &mut catalog).await?;
    Ok(catalog)
}

/// Connects to `url`, introspects it and closes the connection.
///
/// # Errors
///
/// See [`connect`] and [`introspect`].
pub async fn catalog_from_url(url: &str) -> Result<Catalog> {
    let (client, driver) = connect(url).await?;
    let result = introspect(&client).await;
    drop(client);
    driver.abort();
    result
}

/// Blocking wrapper around [`catalog_from_url`] for the synchronous library
/// and CLI APIs. Must not be called from inside a Tokio runtime; the CLI and
/// the synchronous `load_catalog` path always call it from outside one.
///
/// # Errors
///
/// See [`catalog_from_url`].
pub fn catalog_from_url_blocking(url: &str) -> Result<Catalog> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|inner| Error::Internal {
            message: format!("building the introspection runtime: {inner}"),
        })?;
    runtime.block_on(catalog_from_url(url))
}

/// Server major version from `server_version_num` (e.g. `170004` → 17).
async fn server_major(client: &Client) -> Result<i32> {
    let text: String = query_one(client, "SHOW server_version_num", &[]).await?;
    let number: i32 = text.parse().map_err(|_| Error::Internal {
        message: format!("unrecognised server_version_num {text:?}"),
    })?;
    Ok(number / 10_000)
}

async fn query(client: &Client, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Vec<Row>> {
    client
        .query(sql, params)
        .await
        .map_err(|inner| Error::Database {
            context: db_context(sql, &inner),
            inner,
        })
}

async fn query_one<T>(client: &Client, sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<T>
where
    T: tokio_postgres::types::FromSqlOwned,
{
    let row = client
        .query_one(sql, params)
        .await
        .map_err(|inner| Error::Database {
            context: db_context(sql, &inner),
            inner,
        })?;
    Ok(row.get(0))
}

// ---------------------------------------------------------------------------
// Normalisation shared with the SQL parser.
// ---------------------------------------------------------------------------

/// Normalises a deparsed expression with the parser's own normaliser. If the
/// server emits an expression form the parser does not model, the raw text is
/// kept: introspection must never fail on a valid PostgreSQL schema because
/// of an expression spelling, and the convergence tests compare two live
/// models (which use the same fallback).
fn expr(text: &str) -> String {
    let normalized =
        crate::sql::expr::normalize_expr_text(text).unwrap_or_else(|_| text.to_owned());
    strip_simple_casts(&normalized)
}

/// PostgreSQL's deparser adds casts around bare literals (`score >= 0`
/// becomes `score >= (0)::numeric`, `DEFAULT 'x'` becomes `'x'::text`). The
/// SQL-file model keeps the literal as written, so drop casts whose operand
/// is already a self-describing literal. Casts of columns or function calls
/// are never removed.
fn strip_simple_casts(text: &str) -> String {
    let mut current = text.to_owned();
    loop {
        let next = strip_simple_casts_once(&current);
        if next == current {
            return current;
        }
        current = next;
    }
}

fn strip_simple_casts_once(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut index = 0usize;
    while index < bytes.len() {
        if text[index..].starts_with("CAST(")
            && let Some((end, inner)) = parse_simple_cast(text, index)
            && is_simple_literal(inner)
        {
            out.push_str(inner);
            index = end;
            continue;
        }
        let Some(ch) = text[index..].chars().next() else {
            break;
        };
        out.push(ch);
        index += ch.len_utf8();
    }
    out
}

/// Parses `CAST(<inner> AS <type>)` at `start`; returns the index just after
/// the closing parenthesis and the inner text when the cast has the shape.
fn parse_simple_cast(text: &str, start: usize) -> Option<(usize, &str)> {
    let inner_start = start + "CAST(".len();
    let mut depth = 1usize;
    let mut split = None;
    let mut end = None;
    for (offset, ch) in text[inner_start..].char_indices() {
        let absolute = inner_start + offset;
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(absolute);
                    break;
                }
            }
            'A' | 'a' if depth == 1 && text[absolute..].starts_with("AS ") => {
                split = Some(absolute);
            }
            _ => {}
        }
    }
    let split = split?;
    let end = end?;
    let inner = strip_parens(text[inner_start..split].trim());
    Some((end + 1, inner))
}

fn strip_parens(mut text: &str) -> &str {
    loop {
        if text.starts_with('(') && text.ends_with(')') {
            text = text[1..text.len() - 1].trim();
        } else {
            return text;
        }
    }
}

fn is_simple_literal(text: &str) -> bool {
    let text = strip_parens(text);
    if text.is_empty() {
        return false;
    }
    text.starts_with('\'')
        || text.starts_with('$')
        || matches!(text, "true" | "false" | "null")
        || text.parse::<f64>().is_ok()
        || (text.starts_with('-') && text[1..].parse::<f64>().is_ok())
}

/// `format_type` already reports the canonical catalog spelling; running it
/// through the parser's type normaliser makes aliases and arrays byte-for-byte
/// equal to a SQL-file source.
fn type_name(text: &str) -> String {
    crate::sql::datatype::normalize_type_text(text).unwrap_or_else(|_| text.to_owned())
}

/// Strips one balanced pair of enclosing parentheses. `pg_get_expr` wraps
/// CHECK expressions in parentheses that are not part of the source text.
fn strip_outer_parens(mut text: &str) -> &str {
    loop {
        let trimmed = text.trim();
        if !(trimmed.starts_with('(') && trimmed.ends_with(')')) {
            return trimmed;
        }
        let mut depth = 0i32;
        let mut closes_at_end = false;
        for (index, ch) in trimmed.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        closes_at_end = index + 1 == trimmed.len();
                        break;
                    }
                }
                _ => {}
            }
        }
        if !closes_at_end {
            return trimmed;
        }
        text = &trimmed[1..trimmed.len() - 1];
    }
}

/// Parses `key=value` entries from `reloptions`/`proconfig`-style arrays.
fn option_pairs(values: &[String]) -> Vec<(String, String)> {
    values
        .iter()
        .filter_map(|item| item.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

/// Splits the NUL-separated byte string used by `pg_trigger.tgargs`.
fn nul_strings(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect()
}

fn db_context(sql: &str, error: &tokio_postgres::Error) -> String {
    match error.as_db_error() {
        Some(db) => format!(
            "querying pg_catalog ({sql}): {} [{}]",
            db.message(),
            db.code().code()
        ),
        None => format!("querying pg_catalog ({sql}): {error}"),
    }
}

fn role_name(oid: u32) -> String {
    if oid == 0 {
        "PUBLIC".to_owned()
    } else {
        format!("pg_catalog role {oid}")
    }
}

/// Resolves role OIDs to names with one catalog query, avoiding a round trip
/// per grant/policy. OID 0 is `PUBLIC`.
async fn role_names(client: &Client, oids: &[u32]) -> Result<BTreeMap<u32, String>> {
    if oids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let rows = query(
        client,
        "SELECT oid, rolname::text FROM pg_roles WHERE oid = ANY ($1::oid[])",
        &[&oids],
    )
    .await?;
    Ok(rows
        .iter()
        .map(|row| (row.get::<_, u32>(0), row.get::<_, String>(1)))
        .collect())
}

// ---------------------------------------------------------------------------
// Schemas and extensions.
// ---------------------------------------------------------------------------

async fn introspect_schemas(client: &Client, catalog: &mut Catalog) -> Result<()> {
    // System schemas are implementation details; `public` is ambient and is
    // seeded by `introspect`. `information_schema` is a standard view schema.
    let rows = query(
        client,
        "SELECT nspname FROM pg_namespace \
         WHERE nspname <> 'public' AND nspname <> 'information_schema' \
           AND nspname NOT LIKE 'pg\\_%'",
        &[],
    )
    .await?;
    for row in &rows {
        let name: String = row.get(0);
        catalog
            .schemas
            .insert(name.clone(), Schema { name: name.clone() });
    }
    Ok(())
}

async fn introspect_extensions(client: &Client, catalog: &mut Catalog) -> Result<()> {
    let rows = query(
        client,
        "SELECT e.extname, n.nspname FROM pg_extension e \
         JOIN pg_namespace n ON n.oid = e.extnamespace \
         WHERE e.extname <> 'plpgsql'",
        &[],
    )
    .await?;
    for row in &rows {
        let name: String = row.get(0);
        let schema: String = row.get(1);
        catalog.extensions.insert(
            name.clone(),
            Extension {
                name,
                schema: (schema != PUBLIC_SCHEMA).then_some(schema),
            },
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tables, columns, constraints and indexes.
// ---------------------------------------------------------------------------

async fn introspect_tables(client: &Client, catalog: &mut Catalog) -> Result<()> {
    let rows = query(
        client,
        "SELECT n.nspname, c.relname, c.relrowsecurity, c.relforcerowsecurity, \
                c.relkind::text, p.nspname AS parent_schema, pc.relname AS parent_table, \
                pg_get_expr(c.relpartbound, c.oid) AS bound \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         LEFT JOIN pg_inherits i ON i.inhrelid = c.oid \
         LEFT JOIN pg_class pc ON pc.oid = i.inhparent \
         LEFT JOIN pg_namespace p ON p.oid = pc.relnamespace \
         WHERE c.relkind IN ('r', 'p', 'f') \
           AND n.nspname <> 'pg_catalog' AND n.nspname <> 'information_schema' \
           AND n.nspname NOT LIKE 'pg\\_%'",
        &[],
    )
    .await?;
    for row in &rows {
        let schema: String = row.get(0);
        let name: String = row.get(1);
        let rls_enabled: bool = row.get(2);
        let rls_forced: bool = row.get(3);
        let relkind: String = row.get(4);
        let parent_schema: Option<String> = row.get(5);
        let parent_table: Option<String> = row.get(6);
        let bound: Option<String> = row.get(7);

        catalog.ensure_schema(&schema);
        let partition_of = parent_schema.zip(parent_table);
        catalog.tables.insert(
            (schema.clone(), name.clone()),
            Table {
                schema: schema.clone(),
                name: name.clone(),
                columns: Vec::new(),
                constraints: Vec::new(),
                partition: None,
                partition_of: partition_of.clone(),
                rls_enabled,
                rls_forced,
            },
        );
        if let (Some((ps, pt)), Some(bound_text)) = (&partition_of, &bound) {
            let is_default = bound_text.contains("DEFAULT");
            catalog.partitions.insert(
                (schema.clone(), name.clone()),
                Partition {
                    schema: schema.clone(),
                    name: name.clone(),
                    parent_schema: ps.clone(),
                    parent_table: pt.clone(),
                    bound: if is_default {
                        String::new()
                    } else {
                        normalize_bound(bound_text)
                    },
                    is_default,
                },
            );
        }
        let _ = relkind; // relkind distinguishes partitioned parents below.
    }

    // Partitioned parents: one query, keyed by relation name.
    let parent_rows = query(
        client,
        "SELECT n.nspname, c.relname, pt.partstrat::text, pt.partattrs::int2[], \
                pg_get_expr(pt.partexprs, pt.partrelid) AS exprs \
         FROM pg_partitioned_table pt \
         JOIN pg_class c ON c.oid = pt.partrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace",
        &[],
    )
    .await?;
    for row in &parent_rows {
        let schema: String = row.get(0);
        let name: String = row.get(1);
        let strategy: String = row.get(2);
        let attrs: Vec<i16> = row.get(3);
        let exprs: Option<String> = row.get(4);
        let keys = if attrs.iter().any(|a| *a != 0) {
            // Column keys: resolve attnums to names in a second query below.
            Vec::new()
        } else {
            exprs.map_or_else(Vec::new, |text| {
                text.split(", ")
                    .map(|piece| PartitionKey::Expression(expr(piece)))
                    .collect()
            })
        };
        let strategy = match strategy.as_str() {
            "r" => PartitionStrategy::Range,
            "l" => PartitionStrategy::List,
            "h" => PartitionStrategy::Hash,
            other => {
                return Err(Error::Internal {
                    message: format!("unknown partition strategy {other:?}"),
                });
            }
        };
        if let Some(table) = catalog.tables.get_mut(&(schema, name)) {
            table.partition = Some(Partitioning { strategy, keys });
        }
    }

    // Resolve partition key column names by attnum.
    let key_rows = query(
        client,
        "SELECT n.nspname, c.relname, pt.partattrs::int2[], a.attname \
         FROM pg_partitioned_table pt \
         JOIN pg_class c ON c.oid = pt.partrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_attribute a ON a.attrelid = c.oid \
           AND a.attnum = ANY (pt.partattrs::int2[]) \
         ORDER BY a.attnum",
        &[],
    )
    .await?;
    for row in &key_rows {
        let schema: String = row.get(0);
        let name: String = row.get(1);
        let column: String = row.get(3);
        if let Some(table) = catalog.tables.get_mut(&(schema, name))
            && let Some(partition) = table.partition.as_mut()
        {
            partition.keys.push(PartitionKey::Column(column));
        }
    }
    Ok(())
}

/// Normalises `pg_get_expr(relpartbound, ...)` to the source spelling:
/// `FOR VALUES IN ('Paris', 'Berlin')` (dropping the implicit literal casts
/// PostgreSQL adds when deparsing).
fn normalize_bound(text: &str) -> String {
    // `pg_get_expr` renders the bound as `FOR VALUES ...`; the SQL parser
    // stores only the text after `FOR VALUES`.
    let text = text
        .trim()
        .strip_prefix("FOR VALUES ")
        .unwrap_or(text.trim());
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == ':' && chars.peek() == Some(&':') {
            chars.next();
            // Drop the cast type that follows `::`: a bare word, possibly
            // schema-qualified, possibly with precision.
            while let Some(next) = chars.peek() {
                if next.is_alphanumeric() || *next == '_' || *next == '.' || *next == '(' {
                    chars.next();
                } else {
                    break;
                }
            }
            // A parenthesised precision may be present.
            if chars.peek() == Some(&')') {
                chars.next();
            }
            continue;
        }
        out.push(ch);
    }
    out.trim().to_owned()
}

type ColumnNumbers = BTreeMap<(String, String), BTreeMap<i16, String>>;

async fn introspect_columns(client: &Client, catalog: &mut Catalog) -> Result<ColumnNumbers> {
    let mut numbers = ColumnNumbers::new();
    let rows = query(
        client,
        "SELECT n.nspname, c.relname, a.attnum, a.attname, \
                format_type(a.atttypid, a.atttypmod) AS data_type, \
                a.attnotnull, a.attidentity::text, a.attgenerated::text, \
                pg_get_expr(d.adbin, d.adrelid) AS default_expr, \
                col.collname, a.attstorage::text, t.typstorage::text \
         FROM pg_attribute a \
         JOIN pg_class c ON c.oid = a.attrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_type t ON t.oid = a.atttypid \
         LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
         LEFT JOIN pg_collation col ON col.oid = a.attcollation \
         WHERE c.relkind IN ('r', 'p', 'f') AND a.attnum > 0 AND NOT a.attisdropped \
           AND n.nspname <> 'pg_catalog' AND n.nspname <> 'information_schema' \
           AND n.nspname NOT LIKE 'pg\\_%' \
         ORDER BY n.nspname, c.relname, a.attnum",
        &[],
    )
    .await?;
    for row in &rows {
        let schema: String = row.get(0);
        let table: String = row.get(1);
        let attnum: i16 = row.get(2);
        let name: String = row.get(3);
        let data_type: String = row.get(4);
        let not_null: bool = row.get(5);
        let identity: String = row.get(6);
        let generated: String = row.get(7);
        let default: Option<String> = row.get(8);
        let collation: Option<String> = row.get(9);
        let attstorage: String = row.get(10);
        let typstorage: String = row.get(11);
        // `STORAGE DEFAULT` sets attstorage to the type's typstorage. Both
        // sides normalise that case to `None` so a live database and an
        // equivalent SQL source produce equal models.
        let storage = if attstorage == typstorage {
            None
        } else {
            ColumnStorage::from_pg_char(&attstorage)
        };
        let column = Column {
            name,
            data_type: type_name(&data_type),
            not_null,
            default: default.as_deref().map(expr),
            generated: if generated == "s" {
                // pg_get_expr wraps a generation expression in parentheses
                // that are not part of the source's `AS (...)` clause.
                default.as_deref().map(|text| Generated {
                    expr: expr(strip_outer_parens(text)),
                })
            } else {
                None
            },
            identity: match identity.as_str() {
                "a" => Some(Identity {
                    kind: IdentityKind::Always,
                    options: SequenceOptions {
                        increment: 1,
                        minvalue: 1,
                        maxvalue: i64::MAX,
                        start: 1,
                        cache: 1,
                        cycle: false,
                    },
                }),
                "d" => Some(Identity {
                    kind: IdentityKind::ByDefault,
                    options: SequenceOptions {
                        increment: 1,
                        minvalue: 1,
                        maxvalue: i64::MAX,
                        start: 1,
                        cache: 1,
                        cycle: false,
                    },
                }),
                _ => None,
            },
            collation: collation.filter(|c| c != "default" && c != "-" && c != "???"),
            storage,
        };
        // Generated columns store their expression in pg_attrdef just like
        // defaults; distinguish them by `attgenerated` (done above) and keep
        // their default slot empty.
        let mut column = column;
        if generated == "s" {
            column.default = None;
        }
        numbers
            .entry((schema.clone(), table.clone()))
            .or_default()
            .insert(attnum, column.name.clone());
        if let Some(table) = catalog.table_mut(&schema, &table) {
            table.columns.push(column);
        }
    }
    Ok(numbers)
}

async fn introspect_identity_options(client: &Client, catalog: &mut Catalog) -> Result<()> {
    let rows = query(
        client,
        "SELECT n.nspname, c.relname, a.attname, \
                s.seqstart, s.seqincrement, s.seqmax, s.seqmin, s.seqcache, s.seqcycle \
         FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_attribute a ON a.attrelid = c.oid AND a.attidentity <> '' \
         JOIN pg_depend d ON d.refclassid = 'pg_class'::regclass AND d.refobjid = c.oid \
           AND d.refobjsubid = a.attnum AND d.classid = 'pg_class'::regclass AND d.deptype = 'i' \
         JOIN pg_sequence s ON s.seqrelid = d.objid",
        &[],
    )
    .await?;
    for row in &rows {
        let schema: String = row.get(0);
        let table: String = row.get(1);
        let column: String = row.get(2);
        let options = SequenceOptions {
            start: row.get(3),
            increment: row.get(4),
            maxvalue: row.get(5),
            minvalue: row.get(6),
            cache: row.get(7),
            cycle: row.get(8),
        };
        if let Some(table) = catalog.table_mut(&schema, &table)
            && let Some(column) = table.columns.iter_mut().find(|c| c.name == column)
            && let Some(identity) = column.identity.as_mut()
        {
            identity.options = options;
        }
    }
    Ok(())
}

async fn introspect_constraints(
    client: &Client,
    catalog: &mut Catalog,
    major: i32,
    numbers: &ColumnNumbers,
) -> Result<()> {
    // UNIQUE NULLS NOT DISTINCT is stored on the constraint's backing index
    // (pg_index.indnullsnotdistinct, PostgreSQL 15+), not on pg_constraint.
    let nulls_column = if major >= 15 {
        "COALESCE((SELECT i.indnullsnotdistinct FROM pg_index i \
                   WHERE i.indexrelid = k.conindid), FALSE) AS connullsnotdistinct"
    } else {
        "FALSE AS connullsnotdistinct"
    };
    let sql = format!(
        "SELECT n.nspname, c.relname, k.conname, k.contype::text, k.condeferrable, k.condeferred, \
                k.convalidated, k.conkey::int2[], rs.rs_schema, rs.rs_table, \
                k.confkey::int2[], k.confdeltype::text, k.confupdtype::text, \
                k.confmatchtype::text, \
                pg_get_expr(k.conbin, k.conrelid) AS check_expr, {nulls_column}, \
                pg_get_constraintdef(k.oid) AS constraint_def \
         FROM pg_constraint k \
         JOIN pg_class c ON c.oid = k.conrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         LEFT JOIN LATERAL ( \
              SELECT rn.nspname AS rs_schema, rc.relname AS rs_table \
              FROM pg_class rc JOIN pg_namespace rn ON rn.oid = rc.relnamespace \
              WHERE rc.oid = k.confrelid \
         ) rs ON TRUE \
         WHERE k.conrelid <> 0 AND k.contype IN ('p', 'u', 'c', 'f', 'x') \
           AND n.nspname <> 'pg_catalog' AND n.nspname <> 'information_schema' \
           AND n.nspname NOT LIKE 'pg\\_%' \
         ORDER BY n.nspname, c.relname, k.conname"
    );
    let rows = query(client, &sql, &[]).await?;
    for row in &rows {
        let schema: String = row.get(0);
        let table: String = row.get(1);
        let name: String = row.get(2);
        let contype: String = row.get(3);
        let deferrable: bool = row.get(4);
        let initially_deferred: bool = row.get(5);
        let validated: bool = row.get(6);
        let constraint_columns: Vec<i16> = row.get::<_, Option<Vec<i16>>>(7).unwrap_or_default();
        let ref_schema: Option<String> = row.get(8);
        let ref_table: Option<String> = row.get(9);
        let referenced_columns: Vec<i16> = row.get::<_, Option<Vec<i16>>>(10).unwrap_or_default();
        let on_delete: String = row.get(11);
        let on_update: String = row.get(12);
        let match_type: String = row.get(13);
        let check_expr: Option<String> = row.get(14);
        let nulls_not_distinct: bool = row.get(15);
        let constraint_def: Option<String> = row.get(16);

        let columns = column_names_with(numbers, &schema, &table, &constraint_columns);
        let kind = match contype.as_str() {
            "p" => ConstraintKind::PrimaryKey { columns },
            "u" => ConstraintKind::Unique {
                columns,
                nulls_distinct: !nulls_not_distinct,
            },
            "c" => ConstraintKind::Check {
                expr: check_expr.map_or_else(String::new, |text| expr(strip_outer_parens(&text))),
            },
            "x" => {
                // `pg_get_constraintdef` returns `EXCLUDE ...` with the
                // same clause grammar the SQL parser normalises. Strip the
                // keyword and run both through the same token canonicaliser.
                let def = constraint_def.unwrap_or_default();
                let body = if def.len() >= 8 && def[..8].eq_ignore_ascii_case("EXCLUDE ") {
                    &def[8..]
                } else {
                    def.as_str()
                };
                ConstraintKind::Exclusion {
                    spec: crate::sql::tokens::normalize_sql_text(body),
                }
            }
            "f" => {
                let (Some(ref_schema), Some(ref_table)) = (ref_schema, ref_table) else {
                    continue;
                };
                let ref_columns =
                    column_names_with(numbers, &ref_schema, &ref_table, &referenced_columns);
                ConstraintKind::ForeignKey {
                    columns,
                    ref_schema,
                    ref_table,
                    ref_columns,
                    on_delete: fk_action(&on_delete, &[]),
                    on_update: fk_action(&on_update, &[]),
                    match_type: if match_type == "f" {
                        FkMatch::Full
                    } else {
                        FkMatch::Simple
                    },
                }
            }
            _ => continue,
        };
        let constraint = Constraint {
            name,
            kind,
            deferrable: deferrable.then_some(Deferrable {
                deferrable: true,
                initially_deferred,
            }),
            not_valid: !validated,
        };
        if let Some(table) = catalog.table_mut(&schema, &table) {
            table.constraints.push(constraint);
        }
    }
    Ok(())
}

/// Resolves `pg_constraint.conkey`/`confkey` attribute numbers to column
/// names. The model deliberately does not store attribute numbers, so the
/// mapping is kept alongside the columns while introspecting.
fn column_names_with(
    numbers: &ColumnNumbers,
    schema: &str,
    table: &str,
    attnums: &[i16],
) -> Vec<String> {
    let names = numbers.get(&(schema.to_owned(), table.to_owned()));
    attnums
        .iter()
        .filter_map(|num| names.and_then(|map| map.get(num)).cloned())
        .collect()
}

fn fk_action(code: &str, columns: &[String]) -> FkAction {
    match code {
        "a" => FkAction::NoAction,
        "r" => FkAction::Restrict,
        "c" => FkAction::Cascade,
        "n" => FkAction::SetNull(columns.to_vec()),
        "d" => FkAction::SetDefault(columns.to_vec()),
        _ => FkAction::NoAction,
    }
}

async fn introspect_indexes(client: &Client, catalog: &mut Catalog, major: i32) -> Result<()> {
    let nulls_column = if major >= 15 {
        "i.indnullsnotdistinct"
    } else {
        "FALSE AS indnullsnotdistinct"
    };
    let sql = format!(
        "SELECT n.nspname, ic.relname, tc.relname, i.indisunique, i.indkey::int2[], \
                i.indnkeyatts, i.indclass::oid[], i.indoption::int2[], \
                pg_get_expr(i.indexprs, i.indrelid) AS exprs, \
                pg_get_expr(i.indpred, i.indrelid) AS predicate, am.amname, \
                {nulls_column}, rel.reloptions::text[], ts.nspname AS tablespace \
         FROM pg_index i \
         JOIN pg_class ic ON ic.oid = i.indexrelid \
         JOIN pg_class tc ON tc.oid = i.indrelid \
         JOIN pg_namespace n ON n.oid = ic.relnamespace \
         JOIN pg_am am ON am.oid = ic.relam \
         LEFT JOIN pg_namespace ts ON ts.oid = ic.reltablespace \
         LEFT JOIN LATERAL (SELECT reloptions FROM pg_class WHERE oid = i.indexrelid) rel ON TRUE \
         WHERE NOT EXISTS (SELECT 1 FROM pg_constraint k WHERE k.conindid = i.indexrelid) \
           AND tc.relkind IN ('r', 'p', 'f') \
           AND n.nspname <> 'pg_catalog' AND n.nspname <> 'information_schema' \
           AND n.nspname NOT LIKE 'pg\\_%' \
         ORDER BY n.nspname, ic.relname"
    );
    let rows = query(client, &sql, &[]).await?;
    // Resolve operator classes in one round trip. Default operator classes
    // are not part of the source spelling and stay `None` in the model.
    let mut class_oids = BTreeSet::new();
    for row in &rows {
        class_oids.extend(row.get::<_, Vec<u32>>(6));
    }
    let oids: Vec<u32> = class_oids.into_iter().collect();
    let opclass_rows = query(
        client,
        "SELECT o.oid, o.opcname::text, o.opcdefault FROM pg_opclass o \
         WHERE o.oid = ANY ($1::oid[])",
        &[&oids],
    )
    .await?;
    let opclasses: BTreeMap<u32, (String, bool)> = opclass_rows
        .iter()
        .map(|row| (row.get(0), (row.get(1), row.get(2))))
        .collect();

    for row in &rows {
        let schema: String = row.get(0);
        let name: String = row.get(1);
        let table: String = row.get(2);
        let unique: bool = row.get(3);
        let indkey: Vec<i16> = row.get(4);
        let nkeyatts: i16 = row.get(5);
        let indclass: Vec<u32> = row.get(6);
        let indoption: Vec<i16> = row.get(7);
        let exprs: Option<String> = row.get(8);
        let predicate: Option<String> = row.get(9);
        let method: String = row.get(10);
        let nulls_not_distinct: bool = row.get(11);
        let with: Option<Vec<String>> = row.get(12);
        let with = with.unwrap_or_default();
        let tablespace: Option<String> = row.get(13);

        let table_columns = catalog.table(&schema, &table).map(|t| {
            t.columns
                .iter()
                .enumerate()
                .map(|(index, column)| ((index + 1) as i16, column.name.clone()))
                .collect::<BTreeMap<i16, String>>()
        });
        let Some(table_columns) = table_columns else {
            continue;
        };
        let mut columns = Vec::new();
        let expression_texts: Vec<String> = exprs.map_or_else(Vec::new, |text| {
            // pg_get_indexdef's expression list is comma-separated at the
            // top level; simple cases never nest a comma outside parentheses.
            split_top_level(&text)
        });
        let mut expression_index = 0usize;
        for (position, attnum) in indkey.iter().enumerate() {
            if position >= nkeyatts as usize {
                break;
            }
            let kind = if *attnum == 0 {
                let text = expression_texts
                    .get(expression_index)
                    .cloned()
                    .unwrap_or_default();
                expression_index += 1;
                IndexElemKind::Expression(expr(&text))
            } else {
                IndexElemKind::Column(table_columns.get(attnum).cloned().unwrap_or_default())
            };
            // indoption bit 0 = DESC, bit 1 = NULLS FIRST. NULLS FIRST is
            // the default for DESC and NULLS LAST for ASC; only an explicit
            // clause that differs from that default is stored in the model.
            let option = indoption.get(position).copied().unwrap_or(0);
            let descending = option & 1 != 0;
            let nulls_first = option & 2 != 0;
            let opclass = indclass
                .get(position)
                .and_then(|oid| opclasses.get(oid))
                .filter(|(_, is_default)| !*is_default)
                .map(|(name, _)| name.clone());
            columns.push(IndexElem {
                kind,
                collation: None,
                opclass,
                opclass_params: None,
                order: descending.then_some(SortOrder::Desc),
                nulls: if nulls_first {
                    Some(NullsOrder::First)
                } else if descending {
                    Some(NullsOrder::Last)
                } else {
                    None
                },
            });
        }
        let include = indkey
            .iter()
            .skip(nkeyatts as usize)
            .filter_map(|num| table_columns.get(num).cloned())
            .collect();
        catalog.indexes.insert(
            (schema.clone(), name.clone()),
            Index {
                schema: schema.clone(),
                name,
                // PostgreSQL always puts an index in its table's schema.
                table_schema: schema,
                table,
                unique,
                // The catalog does not remember CONCURRENTLY; a SQL-file
                // source and a live source therefore normalise to false.
                concurrent: false,
                method: (method != "btree").then_some(method),
                columns,
                include,
                nulls_distinct: !nulls_not_distinct,
                with: option_pairs(&with),
                tablespace,
                // pg_get_expr parenthesises index predicates; the SQL
                // parser stores the predicate without that outer pair.
                predicate: predicate.map(|text| expr(strip_outer_parens(&text))),
            },
        );
    }
    Ok(())
}

/// Splits a comma-separated expression list at top-level commas only.
fn split_top_level(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (index, ch) in text.char_indices() {
        match ch {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            ',' if depth == 0 => {
                out.push(text[start..index].trim().to_owned());
                start = index + 1;
            }
            _ => {}
        }
    }
    if start < text.len() {
        out.push(text[start..].trim().to_owned());
    }
    out
}

// ---------------------------------------------------------------------------
// Types, sequences, views, routines, triggers, policies.
// ---------------------------------------------------------------------------

async fn introspect_types(client: &Client, catalog: &mut Catalog) -> Result<()> {
    let rows = query(
        client,
        "SELECT t.oid, n.nspname, t.typname, t.typtype::text, \
                format_type(t.typbasetype, t.typtypmod) AS base_type, \
                t.typdefault, format_type(t.typcollation, NULL) AS collation, \
                t.typnotnull \
         FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace \
         WHERE t.typtype IN ('e', 'c', 'd', 'r') \
           AND (t.typtype <> 'c' OR EXISTS ( \
                SELECT 1 FROM pg_class c WHERE c.oid = t.typrelid AND c.relkind = 'c')) \
           AND n.nspname <> 'pg_catalog' AND n.nspname <> 'information_schema' \
           AND n.nspname NOT LIKE 'pg\\_%' \
           AND NOT EXISTS (SELECT 1 FROM pg_depend d \
                           WHERE d.classid = 'pg_type'::regclass AND d.objid = t.oid \
                             AND d.deptype = 'e') \
         ORDER BY n.nspname, t.typname",
        &[],
    )
    .await?;
    for row in &rows {
        let oid: u32 = row.get(0);
        let schema: String = row.get(1);
        let name: String = row.get(2);
        let typtype: String = row.get(3);
        let base_type: String = row.get(4);
        let default: Option<String> = row.get(5);
        let collation: Option<String> = row.get(6);
        let not_null: bool = row.get(7);

        let kind = match typtype.as_str() {
            "e" => {
                let labels = query(
                    client,
                    "SELECT enumlabel FROM pg_enum WHERE enumtypid = $1 ORDER BY enumsortorder",
                    &[&oid],
                )
                .await?;
                PgTypeKind::Enum {
                    values: labels.iter().map(|r| r.get::<_, String>(0)).collect(),
                }
            }
            "c" => {
                let attrs = query(
                    client,
                    "SELECT a.attname, format_type(a.atttypid, a.atttypmod) AS data_type, \
                            format_type(a.attcollation, NULL) AS collation \
                     FROM pg_attribute a WHERE a.attrelid = (SELECT typrelid FROM pg_type WHERE oid = $1) \
                       AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum",
                    &[&oid],
                )
                .await?;
                PgTypeKind::Composite {
                    attributes: attrs
                        .iter()
                        .map(|r| CompositeAttribute {
                            name: r.get(0),
                            data_type: type_name(&r.get::<_, String>(1)),
                            collation: r
                                .get::<_, Option<String>>(2)
                                .filter(|c| c != "default" && c != "-" && c != "???"),
                        })
                        .collect(),
                }
            }
            "d" => {
                let constraints = query(
                    client,
                    "SELECT conname, contype::text, pg_get_expr(conbin, 0) AS expr \
                     FROM pg_constraint WHERE contypid = $1 AND contype IN ('c', 'n') \
                     ORDER BY conname",
                    &[&oid],
                )
                .await?;
                let mut domain_constraints = Vec::new();
                for con in &constraints {
                    let conname: String = con.get(0);
                    let contype: String = con.get(1);
                    let expression: Option<String> = con.get(2);
                    domain_constraints.push(DomainConstraint {
                        name: conname,
                        kind: if contype == "n" {
                            DomainConstraintKind::NotNull
                        } else {
                            DomainConstraintKind::Check {
                                expr: expr(strip_outer_parens(&expression.unwrap_or_default())),
                            }
                        },
                    });
                }
                if not_null
                    && !domain_constraints
                        .iter()
                        .any(|c| matches!(c.kind, DomainConstraintKind::NotNull))
                {
                    domain_constraints.push(DomainConstraint {
                        // CREATE DOMAIN ... NOT NULL produces
                        // `<domain>_not_null` (the inline naming path in
                        // sql/types.rs); older servers expose it only through
                        // `pg_type.typnotnull`.
                        name: format!("{name}_not_null"),
                        kind: DomainConstraintKind::NotNull,
                    });
                }
                domain_constraints.sort_by(|a, b| a.name.cmp(&b.name));
                PgTypeKind::Domain {
                    base_type: type_name(&base_type),
                    collation: collation.filter(|c| c != "default" && c != "-" && c != "???"),
                    default: default.map(|text| expr(&text)),
                    constraints: domain_constraints,
                }
            }
            "r" => {
                // `pg_range` stores function OIDs; `format_type` cannot
                // render them, so resolve the names through pg_proc. The
                // optional multirange type is not modelled by SQL sources.
                let ranges = query(
                    client,
                    "SELECT format_type(r.rngsubtype, NULL), \
                            op.opcname::text, op.opcdefault, \
                            (SELECT p.proname::text FROM pg_proc p WHERE p.oid = r.rngcanonical), \
                            (SELECT p.proname::text FROM pg_proc p WHERE p.oid = r.rngsubdiff), \
                            format_type(r.rngcollation, NULL) \
                     FROM pg_range r \
                     LEFT JOIN pg_opclass op ON op.oid = r.rngsubopc \
                     WHERE r.rngtypid = $1",
                    &[&oid],
                )
                .await?;
                let Some(range) = ranges.first() else {
                    continue;
                };
                let opclass: Option<String> = range.get(1);
                let default_opclass: Option<bool> = range.get(2);
                PgTypeKind::Range {
                    subtype: type_name(&range.get::<_, String>(0)),
                    subtype_opclass: opclass.filter(|_| default_opclass == Some(false)),
                    collation: range
                        .get::<_, Option<String>>(5)
                        .filter(|c| c != "default" && c != "-" && c != "???"),
                    canonical: range.get(3),
                    subtype_diff: range.get(4),
                    multirange_type_name: None,
                }
            }
            _ => continue,
        };
        catalog.types.insert(
            (schema.clone(), name.clone()),
            PgType { schema, name, kind },
        );
    }
    Ok(())
}

async fn introspect_sequences(client: &Client, catalog: &mut Catalog) -> Result<()> {
    let rows = query(
        client,
        "SELECT n.nspname, c.relname, format_type(s.seqtypid, NULL) AS data_type, \
                s.seqstart, s.seqincrement, s.seqmax, s.seqmin, s.seqcache, s.seqcycle, \
                tn.nspname, tc.relname, ta.attname \
         FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_sequence s ON s.seqrelid = c.oid \
         LEFT JOIN pg_depend d ON d.classid = 'pg_class'::regclass AND d.objid = c.oid \
           AND d.refclassid = 'pg_class'::regclass AND d.deptype = 'a' \
         LEFT JOIN pg_class tc ON tc.oid = d.refobjid \
         LEFT JOIN pg_namespace tn ON tn.oid = tc.relnamespace \
         LEFT JOIN pg_attribute ta ON ta.attrelid = tc.oid AND ta.attnum = d.refobjsubid \
         WHERE c.relkind = 'S' AND NOT EXISTS ( \
              SELECT 1 FROM pg_depend di WHERE di.classid = 'pg_class'::regclass \
                AND di.objid = c.oid AND di.deptype = 'i') \
           AND n.nspname <> 'pg_catalog' AND n.nspname <> 'information_schema' \
           AND n.nspname NOT LIKE 'pg\\_%' \
         ORDER BY n.nspname, c.relname",
        &[],
    )
    .await?;
    for row in &rows {
        let schema: String = row.get(0);
        let name: String = row.get(1);
        let data_type: String = row.get(2);
        let owned_by = row
            .get::<_, Option<String>>(9)
            .zip(row.get::<_, Option<String>>(10))
            .zip(row.get::<_, Option<String>>(11))
            .map(|((s, t), c)| SequenceOwner {
                schema: s,
                table: t,
                column: c,
            });
        catalog.sequences.insert(
            (schema.clone(), name.clone()),
            Sequence {
                schema: schema.clone(),
                name,
                data_type: type_name(&data_type),
                options: SequenceOptions {
                    increment: row.get(4),
                    minvalue: row.get(6),
                    maxvalue: row.get(5),
                    start: row.get(3),
                    cache: row.get(7),
                    cycle: row.get(8),
                },
                owned_by,
            },
        );
    }
    Ok(())
}

async fn introspect_views(client: &Client, catalog: &mut Catalog) -> Result<()> {
    let rows = query(
        client,
        "SELECT n.nspname, c.relname, c.relkind::text, \
                pg_get_viewdef(c.oid, false) AS definition, \
                c.reloptions::text[], ts.nspname AS tablespace, am.amname, \
                c.relispopulated \
         FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         LEFT JOIN pg_namespace ts ON ts.oid = c.reltablespace \
         LEFT JOIN pg_am am ON am.oid = c.relam \
         WHERE c.relkind IN ('v', 'm') \
           AND n.nspname <> 'pg_catalog' AND n.nspname <> 'information_schema' \
           AND n.nspname NOT LIKE 'pg\\_%' \
         ORDER BY n.nspname, c.relname",
        &[],
    )
    .await?;
    for row in &rows {
        let schema: String = row.get(0);
        let name: String = row.get(1);
        let relkind: String = row.get(2);
        let definition: String = row.get(3);
        let options: Option<Vec<String>> = row.get(4);
        let options = options.unwrap_or_default();
        let tablespace: Option<String> = row.get(5);
        let access_method: Option<String> = row.get(6);
        let with_data: bool = row.get(7);
        let (query_text, check_option) = split_check_option(&definition);
        catalog.views.insert(
            (schema.clone(), name.clone()),
            View {
                schema: schema.clone(),
                name,
                // The model's `columns` is the *explicit* view column
                // list, not the derived output names stored in pg_attribute.
                columns: Vec::new(),
                query: normalize_view_query(&query_text),
                options: option_pairs(&options),
                tablespace,
                // `heap` is PostgreSQL's default and is not spelled out in
                // a SQL source, so the model keeps it as `None`.
                access_method: access_method.filter(|method| method != "heap"),
                check_option,
                materialized: relkind == "m",
                with_data,
            },
        );
    }
    Ok(())
}

async fn view_oid(client: &Client, schema: &str, name: &str) -> Result<u32> {
    query_one(
        client,
        "SELECT c.oid FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = $1 AND c.relname = $2",
        &[&schema, &name],
    )
    .await
}

fn split_check_option(definition: &str) -> (String, CheckOption) {
    let upper = definition.to_ascii_uppercase();
    if let Some(index) = upper.rfind("WITH CASCADED CHECK OPTION") {
        return (definition[..index].trim().to_owned(), CheckOption::Cascaded);
    }
    if let Some(index) = upper.rfind("WITH LOCAL CHECK OPTION") {
        return (definition[..index].trim().to_owned(), CheckOption::Local);
    }
    if let Some(index) = upper.rfind("WITH CHECK OPTION") {
        return (definition[..index].trim().to_owned(), CheckOption::Cascaded);
    }
    (definition.trim().to_owned(), CheckOption::None)
}

/// Token-joins a deparsed view query with the same spacing rules as the SQL
/// parser, so whitespace and comments never affect model equality.
fn normalize_view_query(text: &str) -> String {
    let trimmed = text.trim().trim_end_matches(';');
    crate::sql::tokens::split_statements(trimmed)
        .ok()
        .and_then(|statements| {
            statements
                .first()
                .map(|statement| crate::sql::tokens::join_tokens(&statement.tokens))
        })
        .unwrap_or_else(|| trimmed.to_owned())
}

async fn introspect_routines(client: &Client, catalog: &mut Catalog) -> Result<()> {
    let rows = query(
        client,
        "SELECT n.nspname, p.proname, p.prokind::text, p.proargtypes::oid[], \
                p.proallargtypes::oid[], p.proargmodes::text[], p.proargnames::text[], \
                pg_get_expr(p.proargdefaults, 0) AS defaults, \
                format_type(p.prorettype, NULL) AS return_type, p.proretset, \
                l.lanname, p.prosrc, p.provolatile::text, p.prosecdef, p.proisstrict, \
                p.proleakproof, p.proparallel::text, p.procost, p.prorows, \
                p.proconfig::text[] \
         FROM pg_proc p \
         JOIN pg_namespace n ON n.oid = p.pronamespace \
         JOIN pg_language l ON l.oid = p.prolang \
         WHERE n.nspname NOT LIKE 'pg\\_%' AND n.nspname <> 'information_schema' \
           AND l.lanname <> 'internal' \
           AND NOT EXISTS (SELECT 1 FROM pg_depend d \
                           WHERE d.classid = 'pg_proc'::regclass AND d.objid = p.oid \
                             AND d.deptype = 'e') \
         ORDER BY n.nspname, p.proname",
        &[],
    )
    .await?;
    for row in &rows {
        let schema: String = row.get(0);
        let name: String = row.get(1);
        let prokind: String = row.get(2);
        let argtypes: Vec<u32> = row.get(3);
        let all_types: Option<Vec<u32>> = row.get(4);
        let modes: Option<Vec<String>> = row.get(5);
        let argnames: Option<Vec<String>> = row.get(6);
        let defaults: Option<String> = row.get(7);
        let return_type: String = row.get(8);
        let returns_setof: bool = row.get(9);
        let language: String = row.get(10);
        let body: String = row.get(11);
        let volatility: String = row.get(12);
        let security: bool = row.get(13);
        let strict: bool = row.get(14);
        let leakproof: bool = row.get(15);
        // `proiswindow` was folded into `prokind = 'w'` in PostgreSQL 11.
        let window = prokind == "w";
        let parallel: String = row.get(16);
        let cost: f32 = row.get(17);
        let rows_estimate: f32 = row.get(18);
        let config: Option<Vec<String>> = row.get(19);
        let config = config.unwrap_or_default();

        let default_texts: Vec<String> =
            defaults.map_or_else(Vec::new, |text| split_top_level(&text));
        let mut args = Vec::new();
        let total = all_types.as_ref().map_or(argtypes.len(), Vec::len);
        for index in 0..total {
            let oid = all_types
                .as_ref()
                .and_then(|types| types.get(index))
                .or_else(|| argtypes.get(index));
            let Some(oid) = oid else {
                continue;
            };
            let mode = match modes
                .as_ref()
                .and_then(|m| m.get(index))
                .map(String::as_str)
            {
                Some("o") => ArgMode::Out,
                Some("b") => ArgMode::InOut,
                Some("v") => ArgMode::Variadic,
                _ => ArgMode::In,
            };
            let data_type = type_name(&format_type_oid(client, *oid).await?);
            let is_input = matches!(mode, ArgMode::In | ArgMode::InOut | ArgMode::Variadic);
            let input_index = argtypes
                .iter()
                .take_while(|candidate| **candidate != *oid || !is_input)
                .count();
            let default = if is_input {
                default_texts.get(input_index).map(|text| expr(text))
            } else {
                None
            };
            args.push(RoutineArg {
                mode,
                name: argnames
                    .as_ref()
                    .and_then(|names| names.get(index))
                    .cloned(),
                data_type,
                default,
            });
        }
        let routine = Routine {
            schema: schema.clone(),
            name: name.clone(),
            kind: if prokind == "p" {
                RoutineKind::Procedure
            } else {
                RoutineKind::Function
            },
            args,
            // Procedures have an implicit void return type in pg_proc;
            // SQL sources omit `RETURNS`, so the model stores None.
            return_type: (prokind != "p").then(|| return_type.clone()),
            returns_setof,
            return_table: Vec::new(),
            language,
            body,
            volatility: match volatility.as_str() {
                "i" => Volatility::Immutable,
                "s" => Volatility::Stable,
                _ => Volatility::Volatile,
            },
            security: if security {
                Security::Definer
            } else {
                Security::Invoker
            },
            strict,
            leakproof,
            window,
            parallel: match parallel.as_str() {
                "s" => Some("safe".to_owned()),
                // PARALLEL UNSAFE is PostgreSQL's default and is stored as
                // None by the SQL-file model when the clause is omitted.
                "u" => None,
                _ => None,
            },
            // The catalog materialises CREATE FUNCTION's defaults (COST 1
            // for most languages, 100 for SQL functions; ROWS 1000); the
            // model stores None unless they were spelled out.
            // 1 and 100 are language-specific PostgreSQL defaults; the
            // model stores None unless COST was spelled out. Compare the
            // integral values to avoid floating-point equality lints.
            cost: (![0, 1, 100].contains(&(cost as u64))).then_some(cost as u64),
            rows: (![0, 1000].contains(&(rows_estimate as u64))).then_some(rows_estimate as u64),
            settings: option_pairs(&config),
        };
        let signature = routine.signature();
        catalog.routines.insert((schema, name, signature), routine);
    }
    Ok(())
}

async fn format_type_oid(client: &Client, oid: u32) -> Result<String> {
    query_one(client, "SELECT format_type($1::oid, NULL)", &[&oid]).await
}

/// Extracts a trigger's `WHEN (...)` clause from `pg_get_triggerdef`.
/// `pg_get_expr(tgqual, tgrelid)` cannot be used here: a WHEN expression may
/// reference both OLD and NEW (two pseudo-relations), which deparsing a bare
/// expression rejects (PostgreSQL docs, CREATE TRIGGER).
fn trigger_when_condition(definition: &str) -> Option<String> {
    let upper = definition.to_ascii_uppercase();
    let start = upper.find(" WHEN ")? + " WHEN ".len();
    let rest = &definition[start..];
    let upper_rest = upper[start..].to_ascii_uppercase();
    let end = upper_rest.find(" EXECUTE ")?;
    Some(rest[..end].trim().to_owned())
}

async fn introspect_triggers(client: &Client, catalog: &mut Catalog) -> Result<()> {
    let rows = query(
        client,
        "SELECT n.nspname, c.relname, t.tgname, t.tgtype, t.tgenabled::text, \
                t.tgdeferrable, t.tginitdeferred, t.tgconstraint <> 0 AS is_constraint, \
                fn_n.nspname, fn.proname, t.tgargs, \
                pg_get_triggerdef(t.oid) AS definition, t.tgattr::int2[] \
         FROM pg_trigger t \
         JOIN pg_class c ON c.oid = t.tgrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_proc fn ON fn.oid = t.tgfoid \
         JOIN pg_namespace fn_n ON fn_n.oid = fn.pronamespace \
         WHERE NOT t.tgisinternal \
           AND n.nspname <> 'pg_catalog' AND n.nspname <> 'information_schema' \
           AND n.nspname NOT LIKE 'pg\\_%' \
         ORDER BY n.nspname, c.relname, t.tgname",
        &[],
    )
    .await?;
    for row in &rows {
        let schema: String = row.get(0);
        let table: String = row.get(1);
        let name: String = row.get(2);
        let tgtype: i16 = row.get(3);
        let enabled_flag: String = row.get(4);
        let deferrable: bool = row.get(5);
        let deferred: bool = row.get(6);
        let constraint: bool = row.get(7);
        let fn_schema: String = row.get(8);
        let fn_name: String = row.get(9);
        let args: Vec<u8> = row.get(10);
        let definition: String = row.get(11);
        let condition = trigger_when_condition(&definition);
        let cols: Vec<i16> = row.get(12);

        let timing = if tgtype & 64 != 0 {
            TriggerTiming::InsteadOf
        } else if tgtype & 2 != 0 {
            TriggerTiming::Before
        } else {
            TriggerTiming::After
        };
        let mut events = Vec::new();
        if tgtype & 4 != 0 {
            events.push(TriggerEvent::Insert);
        }
        if tgtype & 16 != 0 {
            let names = columns_from_attnums(client, &schema, &table, &cols).await?;
            events.push(TriggerEvent::Update(names));
        }
        if tgtype & 8 != 0 {
            events.push(TriggerEvent::Delete);
        }
        if tgtype & 32 != 0 {
            events.push(TriggerEvent::Truncate);
        }
        catalog.triggers.insert(
            (schema.clone(), table, name),
            Trigger {
                schema,
                table: row.get(1),
                name: row.get(2),
                timing,
                events,
                level: if tgtype & 1 != 0 {
                    TriggerLevel::Row
                } else {
                    TriggerLevel::Statement
                },
                function: (fn_schema, fn_name),
                function_args: nul_strings(&args),
                condition: condition.map(|text| expr(strip_outer_parens(&text))),
                constraint,
                deferrable: deferrable.then_some(Deferrable {
                    deferrable: true,
                    initially_deferred: deferred,
                }),
                enabled: enabled_flag != "D",
            },
        );
    }
    Ok(())
}

async fn columns_from_attnums(
    client: &Client,
    schema: &str,
    table: &str,
    attnums: &[i16],
) -> Result<Vec<String>> {
    if attnums.is_empty() {
        return Ok(Vec::new());
    }
    let oid = view_oid(client, schema, table).await?;
    let rows = query(
        client,
        "SELECT attname FROM pg_attribute WHERE attrelid = $1 \
         AND attnum = ANY ($2::int2[]) ORDER BY attnum",
        &[&oid, &attnums],
    )
    .await?;
    Ok(rows.iter().map(|row| row.get::<_, String>(0)).collect())
}

async fn introspect_policies(client: &Client, catalog: &mut Catalog) -> Result<()> {
    let rows = query(
        client,
        "SELECT n.nspname, c.relname, p.polname, p.polcmd::text, p.polpermissive, \
                p.polroles::oid[], pg_get_expr(p.polqual, p.polrelid) AS using_expr, \
                pg_get_expr(p.polwithcheck, p.polrelid) AS check_expr \
         FROM pg_policy p \
         JOIN pg_class c ON c.oid = p.polrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname <> 'pg_catalog' AND n.nspname <> 'information_schema' \
           AND n.nspname NOT LIKE 'pg\\_%' \
         ORDER BY n.nspname, c.relname, p.polname",
        &[],
    )
    .await?;
    for row in &rows {
        let schema: String = row.get(0);
        let table: String = row.get(1);
        let name: String = row.get(2);
        let command: String = row.get(3);
        let permissive: bool = row.get(4);
        let roles: Vec<u32> = row.get(5);
        let using: Option<String> = row.get(6);
        let check: Option<String> = row.get(7);
        let lookup = role_names(client, &roles).await?;
        let role_names: Vec<String> = roles
            .iter()
            .map(|oid| {
                if *oid == 0 {
                    "public".to_owned()
                } else {
                    lookup.get(oid).cloned().unwrap_or_else(|| role_name(*oid))
                }
            })
            .collect();
        catalog.policies.insert(
            (schema.clone(), table, name),
            Policy {
                schema,
                table: row.get(1),
                name: row.get(2),
                permissive,
                command: match command.as_str() {
                    "r" => PolicyCommand::Select,
                    "a" => PolicyCommand::Insert,
                    "w" => PolicyCommand::Update,
                    "d" => PolicyCommand::Delete,
                    _ => PolicyCommand::All,
                },
                roles: role_names,
                using: using.map(|text| expr(strip_outer_parens(&text))),
                check: check.map(|text| expr(strip_outer_parens(&text))),
            },
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Comments and privileges.
// ---------------------------------------------------------------------------

async fn introspect_comments(client: &Client, catalog: &mut Catalog) -> Result<()> {
    let rows = query(
        client,
        // User objects have OIDs >= FirstNormalObjectId (16384). Filtering
        // stock comments on system objects here avoids hundreds of per-row
        // catalog lookups on every introspection.
        "SELECT d.classoid::oid, d.objoid, d.objsubid, d.description \
         FROM pg_description d \
         WHERE d.description IS NOT NULL AND d.objoid >= 16384",
        &[],
    )
    .await?;
    let pg_class = class_oid(client, "pg_class").await?;
    let pg_proc = class_oid(client, "pg_proc").await?;
    let pg_type = class_oid(client, "pg_type").await?;
    let pg_namespace = class_oid(client, "pg_namespace").await?;
    let pg_constraint = class_oid(client, "pg_constraint").await?;
    let pg_trigger = class_oid(client, "pg_trigger").await?;
    let pg_policy = class_oid(client, "pg_policy").await?;
    let pg_attribute = class_oid(client, "pg_attribute").await?;

    for row in &rows {
        let classid: u32 = row.get(0);
        let objoid: u32 = row.get(1);
        let objsubid: i32 = row.get(2);
        let description: String = row.get(3);
        let object = if classid == pg_class {
            // PostgreSQL records column comments against the table's
            // pg_class OID with a non-zero objsubid (the attribute number).
            if objsubid > 0 {
                attribute_object_ref(client, objoid, objsubid).await?
            } else {
                class_object_ref(client, objoid).await?
            }
        } else if classid == pg_attribute {
            attribute_object_ref(client, objoid, objsubid).await?
        } else if classid == pg_proc {
            routine_object_ref(client, objoid).await?
        } else if classid == pg_type {
            type_object_ref(client, objoid).await?
        } else if classid == pg_namespace {
            Some(ObjectRef::new(
                ObjectKind::Schema,
                schema_name(client, objoid).await?,
            ))
        } else if classid == pg_constraint {
            constraint_object_ref(client, objoid).await?
        } else if classid == pg_trigger {
            trigger_object_ref(client, objoid).await?
        } else if classid == pg_policy {
            policy_object_ref(client, objoid).await?
        } else {
            // Comments on extensions and other non-modelled catalog objects
            // have no ObjectRef in the model.
            None
        };
        if let Some(mut object) = object {
            if object.name.starts_with("pg_catalog.")
                || object.name.starts_with("information_schema.")
                || matches!(object.kind, ObjectKind::Function | ObjectKind::Procedure)
                    && object.name.contains("pg_catalog.")
                // System schemas carry stock comments that no declarative
                // source created; ambient `public` is not modelled either.
                || object.kind == ObjectKind::Schema
                    && (object.name == PUBLIC_SCHEMA
                        || !catalog.schemas.contains_key(&object.name))
            {
                continue;
            }
            // A declarative source that did not qualify a public object
            // stores the bare name; `public.` is ambient in every database.
            let bare = if object.kind == ObjectKind::Column {
                object.name.strip_prefix("public.").map(str::to_owned)
            } else {
                None
            };
            if let Some(bare) = bare {
                object.name = bare;
            }
            catalog.comments.insert(object, description);
        }
    }
    Ok(())
}

async fn class_oid(client: &Client, name: &str) -> Result<u32> {
    query_one(client, "SELECT $1::text::regclass::oid", &[&name]).await
}

async fn schema_name(client: &Client, oid: u32) -> Result<String> {
    query_one(
        client,
        "SELECT nspname FROM pg_namespace WHERE oid = $1",
        &[&oid],
    )
    .await
}

async fn class_object_ref(client: &Client, oid: u32) -> Result<Option<ObjectRef>> {
    let rows = query(
        client,
        "SELECT n.nspname, c.relname, c.relkind::text \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE c.oid = $1",
        &[&oid],
    )
    .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let schema: String = row.get(0);
    let name: String = row.get(1);
    let kind: String = row.get(2);
    let object_kind = match kind.as_str() {
        "r" | "p" | "f" => ObjectKind::Table,
        "v" => ObjectKind::View,
        "m" => ObjectKind::MaterializedView,
        "S" => ObjectKind::Sequence,
        "i" => ObjectKind::Index,
        _ => return Ok(None),
    };
    Ok(Some(ObjectRef::new(
        object_kind,
        format!("{schema}.{name}"),
    )))
}

async fn attribute_object_ref(
    client: &Client,
    reloid: u32,
    attnum: i32,
) -> Result<Option<ObjectRef>> {
    let base = class_object_ref(client, reloid).await?;
    let Some(base) = base else {
        return Ok(None);
    };
    let rows = query(
        client,
        "SELECT attname FROM pg_attribute WHERE attrelid = $1 AND attnum = $2",
        &[&reloid, &(i16::try_from(attnum).unwrap_or(0))],
    )
    .await?;
    let Some(column) = rows.first().map(|row| row.get::<_, String>(0)) else {
        return Ok(None);
    };
    Ok(Some(ObjectRef::new(
        ObjectKind::Column,
        format!("{}.{}", base.name, column),
    )))
}

async fn routine_object_ref(client: &Client, oid: u32) -> Result<Option<ObjectRef>> {
    let rows = query(
        client,
        "SELECT n.nspname, p.proname, p.prokind::text, p.proargtypes::oid[] \
         FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace WHERE p.oid = $1",
        &[&oid],
    )
    .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let schema: String = row.get(0);
    let name: String = row.get(1);
    let prokind: String = row.get(2);
    let types: Vec<u32> = row.get(3);
    let mut rendered = Vec::new();
    for oid in types {
        rendered.push(type_name(&format_type_oid(client, oid).await?));
    }
    let kind = if prokind == "p" {
        ObjectKind::Procedure
    } else {
        ObjectKind::Function
    };
    Ok(Some(ObjectRef::new(
        kind,
        crate::model::routine_ref_name(&schema, &name, &rendered),
    )))
}

async fn type_object_ref(client: &Client, oid: u32) -> Result<Option<ObjectRef>> {
    let rows = query(
        client,
        "SELECT n.nspname, t.typname, t.typtype::text FROM pg_type t \
         JOIN pg_namespace n ON n.oid = t.typnamespace WHERE t.oid = $1",
        &[&oid],
    )
    .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let schema: String = row.get(0);
    let name: String = row.get(1);
    let typtype: String = row.get(2);
    let kind = if typtype == "d" {
        ObjectKind::Domain
    } else {
        ObjectKind::Type
    };
    Ok(Some(ObjectRef::new(kind, format!("{schema}.{name}"))))
}

async fn constraint_object_ref(client: &Client, oid: u32) -> Result<Option<ObjectRef>> {
    let rows = query(
        client,
        "SELECT conname FROM pg_constraint WHERE oid = $1",
        &[&oid],
    )
    .await?;
    Ok(rows
        .first()
        .map(|row| ObjectRef::new(ObjectKind::Constraint, row.get::<_, String>(0))))
}

async fn trigger_object_ref(client: &Client, oid: u32) -> Result<Option<ObjectRef>> {
    let rows = query(
        client,
        "SELECT n.nspname, c.relname, t.tgname FROM pg_trigger t \
         JOIN pg_class c ON c.oid = t.tgrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace WHERE t.oid = $1",
        &[&oid],
    )
    .await?;
    Ok(rows.first().map(|row| {
        ObjectRef::new(
            ObjectKind::Trigger,
            format!(
                "{}.{}.{}",
                row.get::<_, String>(0),
                row.get::<_, String>(1),
                row.get::<_, String>(2)
            ),
        )
    }))
}

async fn policy_object_ref(client: &Client, oid: u32) -> Result<Option<ObjectRef>> {
    let rows = query(
        client,
        "SELECT n.nspname, c.relname, p.polname FROM pg_policy p \
         JOIN pg_class c ON c.oid = p.polrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace WHERE p.oid = $1",
        &[&oid],
    )
    .await?;
    Ok(rows.first().map(|row| {
        ObjectRef::new(
            ObjectKind::Policy,
            format!(
                "{}.{}.{}",
                row.get::<_, String>(0),
                row.get::<_, String>(1),
                row.get::<_, String>(2)
            ),
        )
    }))
}

async fn introspect_grants(client: &Client, catalog: &mut Catalog) -> Result<()> {
    // Default privileges that PostgreSQL implicitly gives the owner are not
    // part of a declarative schema; only explicit ACL entries are modelled.
    let rows = query(
        client,
        "SELECT 'TABLE'::text AS kind, n.nspname, c.relname, NULL::text AS column_name, \
                c.relowner AS owner, a.grantee, a.privilege_type, a.is_grantable \
         FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         CROSS JOIN LATERAL aclexplode(c.relacl) a \
         WHERE c.relkind IN ('r', 'p', 'f', 'v', 'm', 'S') \
           AND n.nspname <> 'pg_catalog' AND n.nspname <> 'information_schema' \
           AND n.nspname NOT LIKE 'pg\\_%' \
         UNION ALL \
         SELECT 'COLUMN', n.nspname, c.relname, att.attname, c.relowner, \
                a.grantee, a.privilege_type, a.is_grantable \
         FROM pg_class c \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_attribute att ON att.attrelid = c.oid AND att.attnum > 0 AND att.attacl IS NOT NULL \
         CROSS JOIN LATERAL aclexplode(att.attacl) a \
         WHERE c.relkind IN ('r', 'p', 'f', 'v', 'm') \
           AND n.nspname <> 'pg_catalog' AND n.nspname <> 'information_schema' \
           AND n.nspname NOT LIKE 'pg\\_%' \
         ORDER BY 1, 2, 3, 4, 6, 7",
        &[],
    )
    .await?;
    let grantees: Vec<u32> = rows.iter().map(|row| row.get::<_, u32>(5)).collect();
    let role_lookup = role_names(client, &grantees).await?;
    for row in &rows {
        let kind: String = row.get(0);
        let schema: String = row.get(1);
        let name: String = row.get(2);
        let column: Option<String> = row.get(3);
        let owner: u32 = row.get(4);
        let grantee_oid: u32 = row.get(5);
        let privilege: String = row.get(6);
        let grantable: bool = row.get(7);
        if owner == grantee_oid {
            continue;
        }
        let grantee = if grantee_oid == 0 {
            "public".to_owned()
        } else {
            role_lookup
                .get(&grantee_oid)
                .cloned()
                .unwrap_or_else(|| role_name(grantee_oid))
        };
        let object_kind = match (kind.as_str(), column.is_some()) {
            ("TABLE", false) => ObjectKind::Table,
            ("TABLE", true) => ObjectKind::Column,
            _ => continue,
        };
        let object_name = match &column {
            Some(column) => format!("{schema}.{name}.{column}"),
            None => format!("{schema}.{name}"),
        };
        let key = (
            ObjectRef::new(object_kind, object_name.clone()),
            grantee.clone(),
        );
        let entry = catalog.grants.entry(key).or_insert_with(|| Grant {
            object: ObjectRef::new(object_kind, object_name),
            grantee,
            privileges: BTreeSet::new(),
            with_grant_option: false,
        });
        entry.privileges.insert(privilege.to_ascii_uppercase());
        entry.with_grant_option |= grantable;
    }
    Ok(())
}
