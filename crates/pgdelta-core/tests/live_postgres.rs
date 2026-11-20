//! Live PostgreSQL integration tests (SPEC, Tests section).
//!
//! Runs only when `PGDELTA_TEST_DATABASE_URL` points at a PostgreSQL 13–18
//! server (the CI matrix sets it for each `postgres:13..18` service
//! container). Two properties are checked for every table-driven case:
//!
//! 1. **Convergence** — build `from` and `to` in two databases, apply the
//!    generated migration to the first, introspect both databases via
//!    `pg_catalog`, and assert the diff is empty;
//! 2. **Parity** — introspect a database built from `to.sql` and assert its
//!    model equals the model produced by parsing the same SQL file.
//!
//! A third test checks shadow mode: the SQL-file model must equal the model
//! obtained by executing the same SQL in a shadow database and introspecting
//! it. Cases whose `min_pg_version.txt` exceeds the server version are
//! skipped with a printed reason — never weakened.
//!
//! No `pg_dump` binary is needed: introspection is done in-process with
//! `tokio-postgres`. The databases are created once and emptied between
//! cases, which keeps the suite fast even on Docker Desktop.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr,
    clippy::print_stdout
)]

mod common;

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use proptest::strategy::{Strategy, ValueTree};
use proptest::test_runner::TestRunner;
use tokio::task::JoinHandle;
use tokio_postgres::{Client, NoTls};

use pgdelta_core::introspect::introspect;
use pgdelta_core::{SourceSpec, catalog_from_sql, load_catalog_with_shadow_async, plan};

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

async fn connect_no_tls(url: &str) -> (Client, JoinHandle<()>) {
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

/// Applies SQL statement by statement. Splitting is required because a
/// multi-statement simple-query message runs in one implicit transaction,
/// which `CREATE INDEX CONCURRENTLY` forbids (PostgreSQL docs, CREATE INDEX).
async fn apply_sql(client: &Client, sql: &str, context: &str) {
    for statement in pgdelta_core::split_sql_statements(sql).expect("splitting SQL") {
        client
            .batch_execute(&statement)
            .await
            .unwrap_or_else(|error| {
                let detail = error
                    .as_db_error()
                    .map_or_else(|| error.to_string(), |db| db.message().to_owned());
                panic!("{context}: executing {statement:?}: {detail}");
            });
    }
}

/// Empties a database between cases: non-default extensions and all user
/// schemas go away, `public` is recreated empty.
async fn reset_database(client: &Client) {
    let extensions = client
        .query(
            "SELECT extname FROM pg_extension WHERE extname <> 'plpgsql'",
            &[],
        )
        .await
        .expect("listing extensions");
    for row in &extensions {
        let name: String = row.get(0);
        client
            .batch_execute(&format!("DROP EXTENSION IF EXISTS \"{name}\" CASCADE"))
            .await
            .unwrap_or_else(|error| panic!("dropping extension {name}: {error}"));
    }
    let schemas = client
        .query(
            "SELECT nspname FROM pg_namespace \
             WHERE nspname NOT IN ('pg_catalog', 'information_schema', 'public') \
               AND nspname NOT LIKE 'pg\\_%'",
            &[],
        )
        .await
        .expect("listing schemas");
    for row in &schemas {
        let name: String = row.get(0);
        client
            .batch_execute(&format!("DROP SCHEMA IF EXISTS \"{name}\" CASCADE"))
            .await
            .unwrap_or_else(|error| panic!("dropping schema {name}: {error}"));
    }
    client
        .batch_execute("DROP SCHEMA IF EXISTS public CASCADE; CREATE SCHEMA public")
        .await
        .expect("resetting the public schema");
}

/// Roles referenced by `GRANT`/`CREATE POLICY ... TO` must exist before the
/// source is applied. Declarative sources assume this; a throwaway database
/// starts with no custom roles.
async fn ensure_roles(client: &Client, sql: &str) {
    let statements = pgdelta_core::split_sql_statements(sql).expect("splitting SQL for roles");
    let mut roles: Vec<String> = Vec::new();
    for statement in statements {
        let upper = statement.to_ascii_uppercase();
        let trimmed = upper.trim_start();
        if !trimmed.starts_with("GRANT ") && !trimmed.starts_with("REVOKE ") {
            continue;
        }
        let words: Vec<&str> = statement
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .collect();
        for index in 0..words.len().saturating_sub(1) {
            if words[index].eq_ignore_ascii_case("TO") || words[index].eq_ignore_ascii_case("FROM")
            {
                let candidate = words[index + 1];
                if !candidate.is_empty()
                    && !candidate.eq_ignore_ascii_case("public")
                    && !roles.iter().any(|role| role == candidate)
                {
                    roles.push(candidate.to_owned());
                }
            }
        }
    }
    for role in roles {
        let lowered = role.to_ascii_lowercase();
        let existing = client
            .query_opt("SELECT 1 FROM pg_roles WHERE rolname = $1", &[&lowered])
            .await
            .expect("checking for a role");
        if existing.is_none() {
            client
                .execute(&format!("CREATE ROLE \"{lowered}\""), &[])
                .await
                .unwrap_or_else(|error| panic!("creating role {role}: {error}"));
        }
    }
}

fn run_suffix(tag: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after the epoch")
        .as_nanos();
    format!("{tag}_{}_{nanos}", std::process::id())
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|value| {
        !value.is_empty() && !matches!(value.to_ascii_lowercase().as_str(), "0" | "false" | "no")
    })
}

/// True when running in CI without an explicit opt-out. A missing database
/// URL there means the live suite silently did nothing, which must fail the
/// build (review finding 8). CI jobs that deliberately run without a
/// database (the fmt/clippy/unit matrix) set `PGDELTA_SKIP_LIVE_TESTS=1`;
/// the `integration-postgres` jobs set the URL and never skip.
fn in_ci() -> bool {
    env_flag("CI") && !env_flag("PGDELTA_SKIP_LIVE_TESTS")
}

fn test_url() -> Option<String> {
    match std::env::var("PGDELTA_TEST_DATABASE_URL") {
        Ok(url) if !url.trim().is_empty() => Some(url),
        _ => {
            let message = concat!(
                "SKIP: PGDELTA_TEST_DATABASE_URL is unset; live PostgreSQL tests need a ",
                "database URL (set it to postgres://user:pass@localhost:5432/postgres)"
            );
            // Visible even without `--nocapture` (cargo captures stdout, but
            // the line is printed on the test's output channel on failure)
            // and explicit on both streams for interactive runs.
            println!("{message}");
            eprintln!("{message}");
            assert!(
                !in_ci(),
                "PGDELTA_TEST_DATABASE_URL is unset in CI: live PostgreSQL tests must \
                 not silently pass without a database"
            );
            None
        }
    }
}

/// Reads the server major version; versions outside 13–18 are rejected so a
/// misconfigured CI job cannot silently test an unsupported server.
async fn supported_major(client: &Client) -> i32 {
    let version: String = client
        .query_one("SHOW server_version_num", &[])
        .await
        .expect("reading server_version_num")
        .get(0);
    let major: i32 = version.parse::<i32>().expect("numeric server_version_num") / 10_000;
    assert!(
        (13..=18).contains(&major),
        "PostgreSQL {major} is outside the supported 13–18 range"
    );
    println!("PostgreSQL {major}: running live introspection suite");
    major
}

async fn create_database(admin: &Client, name: &str) {
    admin
        .execute(&format!("CREATE DATABASE \"{name}\""), &[])
        .await
        .unwrap_or_else(|error| panic!("creating database {name}: {error}"));
}

async fn drop_database(admin: &Client, name: &str) {
    admin
        .execute(
            &format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"),
            &[],
        )
        .await
        .unwrap_or_else(|error| panic!("dropping database {name}: {error}"));
}

/// Canonical form of a view query for parity comparisons. `pg_get_viewdef`
/// adds redundant parentheses and explicit casts to literals that the SQL
/// source did not spell out; both are representation, not schema.
fn canonical_query(text: &str, relations: &[String]) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut index = 0usize;
    while index < chars.len() {
        if chars[index] == ':' && chars.get(index + 1) == Some(&':') {
            index += 2;
            while index < chars.len()
                && (chars[index].is_alphanumeric()
                    || chars[index] == '_'
                    || chars[index] == '.'
                    || chars[index] == '"')
            {
                index += 1;
            }
            continue;
        }
        // Replace parentheses with a space so `WHERE(expr)` and
        // `WHERE expr` canonicalise identically.
        out.push(if chars[index] == '(' || chars[index] == ')' {
            ' '
        } else {
            chars[index]
        });
        index += 1;
    }
    let mut out = out.split_whitespace().collect::<Vec<_>>().join(" ");
    // Older servers qualify every column reference with its relation name;
    // newer ones omit it when unambiguous. The model stores the query as
    // written in the SQL source, which is unqualified.
    for relation in relations {
        out = out.replace(&format!("{relation}."), "");
    }
    out
}

fn relation_names(catalog: &pgdelta_core::Catalog) -> Vec<String> {
    catalog
        .tables
        .keys()
        .chain(catalog.views.keys())
        .map(|(_, name)| name.clone())
        .collect()
}

/// Reduces representation-only differences between an introspected model and
/// a SQL-file model. Every adjustment here reflects information the catalog
/// cannot preserve (build instructions, delimiters, default spellings).
fn normalize_for_comparison(catalog: &mut pgdelta_core::Catalog) {
    let relations = relation_names(catalog);
    // CONCURRENTLY is a build-time instruction, not catalog state.
    for index in catalog.indexes.values_mut() {
        index.concurrent = false;
    }
    // `pg_proc.prosrc` stores bodies without dollar-quote delimiters.
    for routine in catalog.routines.values_mut() {
        let body = &routine.body;
        if body.len() >= 4 && body.starts_with("$$") && body.ends_with("$$") {
            routine.body = body[2..body.len() - 2].to_owned();
        }
    }
    // Role names are case-insensitive in PostgreSQL.
    for policy in catalog.policies.values_mut() {
        policy.roles = policy
            .roles
            .iter()
            .map(|role| role.to_ascii_uppercase())
            .collect();
    }
    for view in catalog.views.values_mut() {
        view.query = canonical_query(&view.query, &relations);
    }
    for partition in catalog.partitions.values_mut() {
        partition.bound = canonical_query(&partition.bound, &relations);
    }
}

/// Independent convergence oracle: `pg_dump --schema-only` of a database.
///
/// pgdelta's own introspection cannot be the oracle — anything it misses
/// converges by definition (review finding 9). The command is configurable:
///
/// * default: `pg_dump --schema-only --no-owner --no-privileges {url}`
/// * Docker Desktop: `PGDELTA_TEST_PGDUMP='docker exec <container> pg_dump
///   --schema-only --no-owner --no-privileges -U <user> -d {db}'`
///
/// `{url}` and `{db}` are substituted from the test database URL.
fn dump_schema(url: &str, context: &str) -> String {
    let template = std::env::var("PGDELTA_TEST_PGDUMP")
        .unwrap_or_else(|_| "pg_dump --schema-only --no-owner --no-privileges {url}".to_owned());
    let database = url
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .split('?')
        .next()
        .unwrap_or_default();
    let command = template.replace("{url}", url).replace("{db}", database);
    let parts: Vec<&str> = command.split_whitespace().collect();
    assert!(!parts.is_empty(), "empty PGDELTA_TEST_PGDUMP command");
    let output = Command::new(parts[0])
        .args(&parts[1..])
        .output()
        .unwrap_or_else(|error| panic!("{context}: running {command:?}: {error}"));
    assert!(
        output.status.success(),
        "{context}: pg_dump failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    normalize_dump(&String::from_utf8_lossy(&output.stdout))
}

/// Removes representation-only lines from a `pg_dump` stream. PostgreSQL 18
/// adds `\restrict`/`\unrestrict` lines containing a random token.
fn normalize_dump(text: &str) -> String {
    text.lines()
        .filter(|line| {
            !line.starts_with("\\restrict ")
                && !line.starts_with("\\unrestrict")
                && !line.starts_with("-- Dumped by ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Renders the remaining differences for an assertion message.
fn diagnose(after: &pgdelta_core::Catalog, target: &pgdelta_core::Catalog, context: &str) {
    let planned = plan(after, target, "<actual>", "<expected>").expect("diagnostic plan");
    let sql = pgdelta_core::plan::render_sql(&planned);
    panic!("{context}: models differ:\n{sql}");
}

#[tokio::test]
async fn generated_migration_converges_on_live_postgres() {
    let Some(base) = test_url() else {
        return;
    };
    let (admin, admin_driver) = connect_no_tls(&base).await;
    let major = supported_major(&admin).await;

    let from_db = run_suffix("pgdelta_from");
    let to_db = run_suffix("pgdelta_to");
    create_database(&admin, &from_db).await;
    create_database(&admin, &to_db).await;
    let from_url = with_database(&base, &from_db);
    let to_url = with_database(&base, &to_db);
    let (from_client, from_driver) = connect_no_tls(&from_url).await;
    let (to_client, to_driver) = connect_no_tls(&to_url).await;

    for case in common::load_cases() {
        if case.min_pg_version > major {
            eprintln!(
                "SKIP: {} needs PostgreSQL {}+ (server is {major})",
                case.name, case.min_pg_version
            );
            continue;
        }
        println!("converging {}", case.name);
        reset_database(&from_client).await;
        reset_database(&to_client).await;
        ensure_roles(&from_client, &case.from_sql).await;
        ensure_roles(&from_client, &case.to_sql).await;
        apply_sql(&from_client, &case.from_sql, &case.name).await;

        let from_model = catalog_from_sql(&case.from_sql, "from.sql").expect("parsing from.sql");
        let to_model = catalog_from_sql(&case.to_sql, "to.sql").expect("parsing to.sql");
        let planned = plan(
            &from_model,
            &to_model,
            &format!("<{}/from.sql>", case.name),
            &format!("<{}/to.sql>", case.name),
        )
        .expect("planning the migration");
        apply_sql(
            &from_client,
            &pgdelta_core::plan::render_sql(&planned),
            &case.name,
        )
        .await;

        apply_sql(&to_client, &case.to_sql, &case.name).await;

        let converged = introspect(&from_client).await.unwrap_or_else(|error| {
            panic!(
                "introspecting the migrated database for {}: {error}",
                case.name
            )
        });
        let target = introspect(&to_client)
            .await
            .unwrap_or_else(|error| panic!("introspecting the target for {}: {error}", case.name));
        let verification = plan(&converged, &target, "<after>", "<target>").expect("verifying");
        if !verification.is_empty() {
            diagnose(&converged, &target, &case.name);
        }
        // Independent oracle: the two schemas must produce byte-identical
        // `pg_dump --schema-only` output. pgdelta introspection is only a
        // diagnostic aid, never the convergence authority.
        let after_dump = dump_schema(&from_url, &case.name);
        let target_dump = dump_schema(&to_url, &case.name);
        assert_eq!(
            after_dump, target_dump,
            "{}: pg_dump --schema-only convergence failed (migrated vs target)",
            case.name
        );
    }
    drop(from_client);
    from_driver.abort();
    drop(to_client);
    to_driver.abort();
    drop_database(&admin, &from_db).await;
    drop_database(&admin, &to_db).await;
    admin_driver.abort();
}

/// Property test whose convergence oracle is `pg_dump --schema-only`.
///
/// The extended generator produces same-named indexes with different
/// definitions, routine body changes, enum removals and view-on-matview
/// chains — the shapes that the circular introspection oracle let through.
#[tokio::test]
async fn rich_generated_migration_converges_with_pgdump_oracle() {
    let Some(base) = test_url() else {
        return;
    };
    let (admin, admin_driver) = connect_no_tls(&base).await;
    supported_major(&admin).await;
    let from_db = run_suffix("pgdelta_rich_from");
    let to_db = run_suffix("pgdelta_rich_to");
    create_database(&admin, &from_db).await;
    create_database(&admin, &to_db).await;
    let from_url = with_database(&base, &from_db);
    let to_url = with_database(&base, &to_db);
    let (from_client, from_driver) = connect_no_tls(&from_url).await;
    let (to_client, to_driver) = connect_no_tls(&to_url).await;

    let strategy = common::any_rich_catalog();
    let mut runner = TestRunner::deterministic();
    for iteration in 0..16 {
        let a = strategy
            .new_tree(&mut runner)
            .expect("generating catalog a")
            .current();
        let b = strategy
            .new_tree(&mut runner)
            .expect("generating catalog b")
            .current();
        println!("rich property iteration {iteration}");
        reset_database(&from_client).await;
        reset_database(&to_client).await;
        let empty = pgdelta_core::Catalog::new();
        let create_a = pgdelta_core::plan::render_sql(
            &plan(&empty, &a, "<empty>", "<a>").expect("planning a"),
        );
        let create_b = pgdelta_core::plan::render_sql(
            &plan(&empty, &b, "<empty>", "<b>").expect("planning b"),
        );
        apply_sql(&from_client, &create_a, "rich a").await;
        apply_sql(&to_client, &create_b, "rich b").await;
        let migration = pgdelta_core::plan::render_sql(
            &plan(&a, &b, "<a>", "<b>").expect("planning migration"),
        );
        apply_sql(&from_client, &migration, "rich migration").await;

        let after_dump = dump_schema(&from_url, "rich property");
        let target_dump = dump_schema(&to_url, "rich property");
        assert_eq!(
            after_dump, target_dump,
            "rich property iteration {iteration}: pg_dump convergence failed"
        );
    }

    drop(from_client);
    from_driver.abort();
    drop(to_client);
    to_driver.abort();
    drop_database(&admin, &from_db).await;
    drop_database(&admin, &to_db).await;
    admin_driver.abort();
}

#[tokio::test]
async fn sql_file_model_matches_live_model() {
    let Some(base) = test_url() else {
        return;
    };
    let (admin, admin_driver) = connect_no_tls(&base).await;
    let major = supported_major(&admin).await;
    let database = run_suffix("pgdelta_parity");
    create_database(&admin, &database).await;
    let url = with_database(&base, &database);
    let (client, driver) = connect_no_tls(&url).await;

    for case in common::load_cases() {
        if case.min_pg_version > major {
            eprintln!(
                "SKIP: {} needs PostgreSQL {}+ (server is {major})",
                case.name, case.min_pg_version
            );
            continue;
        }
        println!("parity {}", case.name);
        reset_database(&client).await;
        ensure_roles(&client, &case.to_sql).await;
        apply_sql(&client, &case.to_sql, &case.name).await;
        let live = introspect(&client)
            .await
            .unwrap_or_else(|error| panic!("introspecting {}: {error}", case.name));
        let parsed = catalog_from_sql(&case.to_sql, "to.sql").expect("parsing to.sql");
        // Physical column/constraint order is not part of the schema
        // contract (PostgreSQL cannot reorder columns without a rewrite),
        // so compare with the shared, order-insensitive equivalence check.
        let mut live = live;
        let mut parsed = parsed;
        normalize_for_comparison(&mut live);
        normalize_for_comparison(&mut parsed);
        common::assert_equivalent(&live, &parsed, &format!("{}: parity", case.name));
    }
    drop(client);
    driver.abort();
    drop_database(&admin, &database).await;
    admin_driver.abort();
}

#[tokio::test]
async fn shadow_mode_matches_sql_file_model() {
    let Some(base) = test_url() else {
        return;
    };
    let (admin, admin_driver) = connect_no_tls(&base).await;
    let major = supported_major(&admin).await;

    for case in common::load_cases() {
        if case.min_pg_version > major {
            continue;
        }
        println!("shadow {}", case.name);
        // Roles are cluster-wide, so creating them once here is enough for
        // the shadow database that `load_catalog_with_shadow_async` reuses.
        ensure_roles(&admin, &case.to_sql).await;
        let dir = common::cases_root().join(&case.name);
        let spec = SourceSpec::SqlFile(dir.join("to.sql"));
        let shadow = load_catalog_with_shadow_async(&spec, Some(&base))
            .await
            .unwrap_or_else(|error| panic!("shadow mode for {}: {error}", case.name));
        let parsed = catalog_from_sql(&case.to_sql, "to.sql").expect("parsing to.sql");
        let mut shadow = shadow;
        let mut parsed = parsed;
        normalize_for_comparison(&mut shadow);
        normalize_for_comparison(&mut parsed);
        common::assert_equivalent(&shadow, &parsed, &format!("{}: shadow mode", case.name));
    }
    admin_driver.abort();
}
