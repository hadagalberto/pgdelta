//! Live-database convergence test (SPEC, Tests section).
//!
//! Runs only when `PGDELTA_TEST_DATABASE_URL` points at a PostgreSQL server
//! (the CI matrix sets it for each `postgres:13..18` service container).
//! For every table-driven case it creates two throwaway databases, builds
//! the `from` schema in one and the `to` schema in the other, applies the
//! generated migration to the first, and asserts that both databases have
//! the same schema fingerprint (via `pg_dump --schema-only`). Cases whose
//! `min_pg_version.txt` exceeds the server version are skipped — never
//! weakened.
//!
//! Comparing a *set* of dump lines (rather than raw text) is deliberate:
//! PostgreSQL cannot reorder columns without a rewrite, so pgdelta never
//! plans a reorder and the in-memory convergence oracle compares columns by
//! name. Physical column order is therefore not part of the contract.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr,
    clippy::print_stdout
)]

// Shared harness; this target uses only case loading and the version gate.
#[allow(dead_code)]
mod common;

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::task::JoinHandle;
use tokio_postgres::{Client, NoTls};

/// Replaces the database name in a `postgres://` URL while preserving the
/// authority and query string.
fn with_database(base: &str, database: &str) -> String {
    let (scheme, rest) = base.split_once("://").expect("URL has a scheme");
    let (authority, tail) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, ""),
    };
    let query = tail.find('?').map_or("", |index| &tail[index..]);
    format!("{scheme}://{authority}/{database}{query}")
}

/// Connects and keeps the connection driver task alive for the returned
/// client's lifetime.
async fn connect(url: &str) -> (Client, JoinHandle<()>) {
    let (client, connection) = tokio_postgres::connect(url, NoTls)
        .await
        .unwrap_or_else(|error| panic!("connecting to {url}: {error}"));
    let driver = tokio::spawn(async move {
        if let Err(error) = connection.await {
            eprintln!("connection driver ended: {error}");
        }
    });
    (client, driver)
}

/// Applies SQL text statement by statement. Splitting is required because a
/// multi-statement simple-query message runs in one implicit transaction,
/// which `CREATE INDEX CONCURRENTLY` forbids (PostgreSQL docs, CREATE INDEX).
async fn apply_sql(client: &Client, sql: &str) {
    for statement in pgdelta_core::split_sql_statements(sql).expect("splitting SQL") {
        client
            .batch_execute(&statement)
            .await
            .unwrap_or_else(|error| panic!("executing {statement:?}: {error}"));
    }
}

/// Canonical schema fingerprint: sorted, trimmed, meaningful dump lines.
/// Header comments, transaction settings and blank lines are irrelevant to
/// schema equivalence and are dropped.
fn fingerprint(database_url: &str) -> Vec<String> {
    let output = Command::new("pg_dump")
        .args([
            "--schema-only",
            "--no-owner",
            "--no-privileges",
            "--no-tablespaces",
            "--no-comments",
            "--database",
            database_url,
        ])
        .output()
        .expect("running pg_dump (install the postgresql-client package)");
    assert!(
        output.status.success(),
        "pg_dump failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let dump = String::from_utf8(output.stdout).expect("pg_dump emitted UTF-8");
    let mut lines: Vec<String> = dump
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("--"))
        .map(str::to_owned)
        .collect();
    lines.sort();
    lines
}

/// Identifier-safe, <=63-byte suffix shared by both databases of one case.
fn run_suffix(case: &common::Case) -> String {
    let stem: String = case
        .name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(28)
        .collect();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after the epoch")
        .as_nanos();
    format!("{stem}_{}_{nanos}", std::process::id())
}

#[tokio::test]
async fn generated_migration_converges_on_live_postgres() {
    let Some(base) = std::env::var("PGDELTA_TEST_DATABASE_URL").ok() else {
        eprintln!("PGDELTA_TEST_DATABASE_URL is unset: skipping live convergence test");
        return;
    };
    if Command::new("pg_dump").arg("--version").output().is_err() {
        eprintln!("pg_dump is unavailable: skipping live convergence test");
        return;
    }

    let (admin, admin_driver) = connect(&base).await;
    let version_text: String = admin
        .query_one("SHOW server_version_num", &[])
        .await
        .expect("reading server_version_num")
        .get(0);
    let major = version_text
        .parse::<i32>()
        .expect("numeric server_version_num")
        / 10_000;
    println!("PostgreSQL {major}: running live convergence suite");

    for case in common::load_cases() {
        if case.min_pg_version > major {
            eprintln!(
                "skipping {} (needs PostgreSQL {}+)",
                case.name, case.min_pg_version
            );
            continue;
        }
        let suffix = run_suffix(&case);
        let from_db = format!("pgdelta_from_{suffix}");
        let to_db = format!("pgdelta_to_{suffix}");
        admin
            .execute(&format!("CREATE DATABASE \"{from_db}\""), &[])
            .await
            .expect("creating the from database");
        admin
            .execute(&format!("CREATE DATABASE \"{to_db}\""), &[])
            .await
            .expect("creating the to database");

        let from_url = with_database(&base, &from_db);
        let to_url = with_database(&base, &to_db);

        let (from_client, from_driver) = connect(&from_url).await;
        apply_sql(&from_client, &case.from_sql).await;

        let from_catalog =
            pgdelta_core::catalog_from_sql(&case.from_sql, "from.sql").expect("parsing from.sql");
        let to_catalog =
            pgdelta_core::catalog_from_sql(&case.to_sql, "to.sql").expect("parsing to.sql");
        let planned = pgdelta_core::plan(
            &from_catalog,
            &to_catalog,
            &format!("<{}/from.sql>", case.name),
            &format!("<{}/to.sql>", case.name),
        )
        .expect("planning the migration");
        let script = pgdelta_core::plan::render_sql(&planned);
        apply_sql(&from_client, &script).await;
        drop(from_client);
        from_driver.abort();

        let (to_client, to_driver) = connect(&to_url).await;
        apply_sql(&to_client, &case.to_sql).await;
        drop(to_client);
        to_driver.abort();

        assert_eq!(
            fingerprint(&from_url),
            fingerprint(&to_url),
            "{}: live schemas diverge after applying the generated migration",
            case.name
        );

        admin
            .execute(
                &format!("DROP DATABASE IF EXISTS \"{from_db}\" WITH (FORCE)"),
                &[],
            )
            .await
            .expect("dropping the from database");
        admin
            .execute(
                &format!("DROP DATABASE IF EXISTS \"{to_db}\" WITH (FORCE)"),
                &[],
            )
            .await
            .expect("dropping the to database");
    }
    admin_driver.abort();
}
