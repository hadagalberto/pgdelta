//! Hostile-input regression tests (milestone 5).
//!
//! These drive the real CLI binary with malformed text, BOM/CRLF input,
//! Windows-style paths, empty and oversized files, and deeply nested
//! expressions. The contract is: never panic, never silently accept bad
//! DDL, and report a source-located error with exit code 1 (or a clap usage
//! error with exit code 2).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn work_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("pgdelta-hostile-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("creating the hostile-input workspace");
    dir
}

fn write(name: &str, bytes: &[u8]) -> PathBuf {
    let path = work_dir().join(name);
    std::fs::write(&path, bytes).expect("writing a fixture");
    path
}

fn text(name: &str, content: &str) -> PathBuf {
    write(name, content.as_bytes())
}

fn path_str(path: &Path) -> &str {
    path.to_str()
        .expect("fixture paths are UTF-8 on the test host")
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_pgdelta"))
        .args(args)
        .output()
        .expect("running the pgdelta binary")
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn bom_crlf_and_missing_semicolon_are_accepted() {
    // A real UTF-8 BOM (EF BB BF) plus CRLF line endings and no trailing
    // semicolon: all legal at a schema-file boundary.
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(b"CREATE TABLE public.t (id integer)\r\n");
    let source = write("bom_crlf.sql", &bytes);
    let output = run(&["inspect", path_str(&source)]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["tables"].as_array().unwrap().len(), 1);
    assert_eq!(json["tables"][0]["name"], "t");
}

#[test]
fn empty_sources_diff_to_no_changes() {
    let empty = text("empty.sql", "");
    let output = run(&[
        "diff",
        "--from",
        path_str(&empty),
        "--to",
        path_str(&empty),
        "--color",
        "never",
    ]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout.len(), 0);
    assert!(stderr(&output).contains("no changes"));
}

#[test]
fn unterminated_string_and_comment_are_reported_not_panics() {
    let string = text(
        "unterminated_string.sql",
        "CREATE TABLE t (a text DEFAULT 'oops);",
    );
    let output = run(&["inspect", path_str(&string)]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("parse error"),
        "{}",
        stderr(&output)
    );

    let comment = text(
        "unterminated_comment.sql",
        "CREATE TABLE t (a text); /* never closed",
    );
    let output = run(&["inspect", path_str(&comment)]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("parse error"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn nul_byte_input_is_rejected_cleanly() {
    let mut bytes = b"CREATE TABLE ".to_vec();
    bytes.push(0);
    bytes.extend_from_slice(b"t (a integer);");
    let source = write("nul.sql", &bytes);
    let output = run(&["inspect", path_str(&source)]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("pgdelta:"));
    assert!(!stderr(&output).contains("panic"), "{}", stderr(&output));
}

#[test]
fn deeply_nested_expression_is_rejected_without_stack_overflow() {
    let depth = 2_000;
    let sql = format!(
        "CREATE TABLE t (a integer CHECK {} a > 0 {});",
        "(".repeat(depth),
        ")".repeat(depth)
    );
    let source = text("deep.sql", &sql);
    let output = run(&["inspect", path_str(&source)]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("nesting depth"),
        "{}",
        stderr(&output)
    );
    // The same guard protects `lint`, which uses the shared tokenizer.
    let output = run(&["lint", path_str(&source)]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("nesting depth"));
}

#[test]
fn wide_schema_with_thousands_of_columns_is_handled() {
    let columns = (1..=5_000)
        .map(|i| format!("c{i} integer"))
        .collect::<Vec<_>>()
        .join(", ");
    let source = text(
        "wide.sql",
        &format!("CREATE TABLE public.wide ({columns});"),
    );
    let output = run(&["inspect", path_str(&source)]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        json["tables"][0]["columns"].as_array().unwrap().len(),
        5_000
    );
}

#[test]
fn missing_sources_and_bad_output_paths_report_errors() {
    let missing = work_dir().join("does-not-exist.sql");
    let output = run(&["inspect", path_str(&missing)]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("neither an existing file nor a directory"));

    let source = text("simple.sql", "CREATE TABLE t (a integer);");
    let empty = text("target-empty.sql", "");
    let dir = work_dir();
    let output = run(&[
        "diff",
        "--from",
        path_str(&source),
        "--to",
        path_str(&empty),
        "--output",
        path_str(&dir),
    ]);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stderr(&output).contains("writing"));

    let output = run(&[
        "diff",
        "--from",
        path_str(&source),
        "--to",
        path_str(&empty),
        "--output",
        path_str(&dir.join("absent").join("out.sql")),
    ]);
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn malformed_rename_hints_are_rejected() {
    let empty = text("rename-empty.sql", "");
    for hint in ["garbage", "table.", "column.t.a", "table.a="] {
        let output = run(&[
            "diff",
            "--from",
            path_str(&empty),
            "--to",
            path_str(&empty),
            "--rename",
            hint,
            "--color",
            "never",
        ]);
        assert_eq!(output.status.code(), Some(1), "hint {hint:?} was accepted");
        assert!(
            stderr(&output).contains("--rename hints"),
            "hint {hint:?}: {}",
            stderr(&output)
        );
    }
    // A well-formed hint naming a missing object is a planning error, not
    // an ignored hint.
    let output = run(&[
        "diff",
        "--from",
        path_str(&empty),
        "--to",
        path_str(&empty),
        "--rename",
        "view.a=b",
        "--color",
        "never",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("does not exist"));
}

#[test]
fn directory_sources_sort_and_accept_uppercase_sql_extension() {
    let dir = work_dir().join("declarative");
    let nested = dir.join("nested");
    std::fs::create_dir_all(&nested).expect("creating a nested source directory");
    // Creation order is deliberately the reverse of the required apply
    // order; directory sources sort by relative path.
    std::fs::write(
        nested.join("b.SQL"),
        "ALTER TABLE public.t ADD COLUMN b text;",
    )
    .expect("writing an uppercase-extension file");
    std::fs::write(dir.join("a.sql"), "CREATE TABLE public.t (a integer);")
        .expect("writing a source file");
    std::fs::write(dir.join("notes.txt"), "CREATE TABLE ignored (x integer);")
        .expect("writing a non-SQL file");

    let output = run(&["inspect", path_str(&dir)]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let columns = json["tables"][0]["columns"].as_array().unwrap();
    assert_eq!(columns.len(), 2);
    assert_eq!(columns[0]["name"], "a");
    assert_eq!(columns[1]["name"], "b");
}

#[test]
fn empty_directory_is_a_valid_empty_source() {
    let dir = work_dir().join("empty-dir");
    std::fs::create_dir_all(&dir).expect("creating an empty directory");
    let output = run(&["inspect", path_str(&dir)]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["tables"].as_array().unwrap().len(), 0);
}

#[test]
fn lint_is_keyword_based_and_ignores_comments_and_literals() {
    let source = text(
        "lint-hostile.sql",
        "-- DROP TABLE imaginary;\n\
         /* ALTER TABLE t ALTER COLUMN a SET NOT NULL; */\n\
         INSERT INTO t VALUES ('DROP TABLE nope');\n\
         DROP TABLE real;",
    );
    let output = run(&["lint", "--json", path_str(&source)]);
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["summary"]["statements"], 2);
    assert_eq!(json["summary"]["destructive"], 1);
    assert_eq!(
        json["statements"][0]["hazards"].as_array().unwrap().len(),
        0
    );
    assert_eq!(json["statements"][1]["hazards"][0]["code"], "drop-table");
}

#[test]
fn invalid_flag_values_are_clap_usage_errors() {
    let source = text("flag.sql", "DROP TABLE t;");
    let output = run(&["lint", "--color", "rainbow", path_str(&source)]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("invalid value"));

    let output = run(&["diff", "--from", path_str(&source)]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("--to"));
}

#[test]
fn unreachable_database_is_an_operational_error_not_a_panic() {
    let output = run(&["inspect", "postgres://postgres:postgres@127.0.0.1:1/nope"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("database error"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn fail_on_hazard_exit_code_two_is_stable_with_json_output() {
    let from = text("hazard-from.sql", "CREATE TABLE public.old (id integer);");
    let to = text("hazard-to.sql", "-- nothing");
    let output = run(&[
        "diff",
        "--from",
        path_str(&from),
        "--to",
        path_str(&to),
        "--json",
        "--fail-on-hazard",
        "--color",
        "never",
    ]);
    assert_eq!(output.status.code(), Some(2));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["summary"]["hazards"], 1);
}
