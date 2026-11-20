//! End-to-end CLI tests: output modes, coloured summaries, rename hints,
//! `--online` rewriting and the `--fail-on-hazard` exit code.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn temp_sql(name: &str, content: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("pgdelta-cli-{}-{name}.sql", std::process::id()));
    std::fs::write(&path, content).unwrap();
    path
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pgdelta"))
        .args(args)
        .output()
        .expect("running the pgdelta binary")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).expect("UTF-8 CLI output")
}

fn remove(path: &Path) {
    let _ = std::fs::remove_file(path);
}

#[test]
fn lint_fail_on_hazard_returns_exit_code_two() {
    let file = temp_sql(
        "lint-hazard",
        "DROP TABLE public.old; ALTER TABLE public.t ALTER COLUMN id SET NOT NULL;",
    );
    let path = file.to_string_lossy();
    let failing = run(&["lint", "--color", "never", "--fail-on-hazard", &path]);
    assert_eq!(failing.status.code(), Some(2));
    let stderr = text(&failing.stderr);
    assert!(stderr.contains("drop-table"), "{stderr}");
    assert!(stderr.contains("not-null-scan"), "{stderr}");
    assert!(stderr.contains("Hazards: 2"), "{stderr}");

    let passing = run(&["lint", "--color", "never", &path]);
    assert_eq!(passing.status.code(), Some(0));
    remove(&file);
}

#[test]
fn lint_json_is_machine_readable_without_human_noise() {
    let file = temp_sql(
        "lint-json",
        "ALTER TABLE public.t ADD CONSTRAINT c CHECK (id > 0);",
    );
    let path = file.to_string_lossy();
    let output = run(&["lint", "--json", &path]);
    assert_eq!(output.status.code(), Some(0));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["summary"]["hazards"], 1);
    assert_eq!(json["summary"]["destructive"], 0);
    assert_eq!(
        json["statements"][0]["hazards"][0]["code"],
        "constraint-scan"
    );
    assert!(text(&output.stdout).contains("\"source\""));
    remove(&file);
}

#[test]
fn diff_fail_on_hazard_returns_exit_code_two_with_json_on_stdout() {
    let from = temp_sql("diff-from", "CREATE TABLE public.old (id integer);");
    let to = temp_sql("diff-to", "-- nothing to keep");
    let output = run(&[
        "diff",
        "--from",
        &from.to_string_lossy(),
        "--to",
        &to.to_string_lossy(),
        "--json",
        "--color",
        "never",
        "--fail-on-hazard",
    ]);
    assert_eq!(output.status.code(), Some(2));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["summary"]["hazards"], 1);
    assert_eq!(
        json["sections"][0]["statements"][0]["hazards"][0]["code"],
        "drop-table"
    );
    assert!(text(&output.stderr).contains("Hazards: 1"));
    remove(&from);
    remove(&to);
}

#[test]
fn diff_online_and_rename_hints_rewrite_the_script() {
    let from = temp_sql(
        "online-from",
        "CREATE TABLE public.users (id integer PRIMARY KEY, email text); \
         CREATE TABLE public.posts (id integer, user_id integer, email text);",
    );
    let to = temp_sql(
        "online-to",
        "CREATE TABLE public.people (id integer PRIMARY KEY, contact text); \
         CREATE TABLE public.posts (id integer, user_id integer NOT NULL, email text); \
         ALTER TABLE public.posts ADD CONSTRAINT posts_user_id_fkey \
           FOREIGN KEY (user_id) REFERENCES public.people(id); \
         CREATE INDEX posts_email_idx ON public.posts (email);",
    );
    let output = run(&[
        "diff",
        "--from",
        &from.to_string_lossy(),
        "--to",
        &to.to_string_lossy(),
        "--online",
        "--rename",
        "table.users=people",
        "--rename",
        "column.people.email=contact",
        "--color",
        "never",
    ]);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    let sql = text(&output.stdout);
    assert!(sql.contains("RENAME TO \"people\""), "{sql}");
    assert!(
        sql.contains("RENAME COLUMN \"email\" TO \"contact\""),
        "{sql}"
    );
    assert!(sql.contains("FOREIGN KEY"), "{sql}");
    assert!(sql.contains("NOT VALID"), "{sql}");
    assert!(sql.contains("VALIDATE CONSTRAINT"), "{sql}");
    assert!(sql.contains("CREATE INDEX CONCURRENTLY"), "{sql}");
    assert!(sql.contains("Non-transactional section"), "{sql}");
    remove(&from);
    remove(&to);
}

#[test]
fn no_changes_returns_success_with_empty_output() {
    let file = temp_sql("no-changes", "CREATE TABLE public.t (id integer);");
    let path = file.to_string_lossy();
    let output = run(&["diff", "--from", &path, "--to", &path, "--color", "never"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout.len(), 0);
    assert!(text(&output.stderr).contains("no changes"));
    remove(&file);
}

#[test]
fn always_color_emits_ansi_in_the_human_summary() {
    let file = temp_sql("color", "DROP TABLE public.old;");
    let path = file.to_string_lossy();
    let output = run(&["lint", "--color", "always", &path]);
    assert_eq!(output.status.code(), Some(0));
    assert!(
        text(&output.stderr).contains("\u{1b}[31m"),
        "{}",
        text(&output.stderr)
    );
    remove(&file);
}
