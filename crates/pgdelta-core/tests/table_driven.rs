//! Table-driven snapshot tests: every case directory under `tests/cases`
//! produces one deterministic insta snapshot of the generated migration
//! script, plus the convergence oracle (applying the script to the `from`
//! model reproduces the `to` model).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

// Shared harness; each test target uses a different subset of its helpers.
#[allow(dead_code)]
mod common;

use insta::assert_snapshot;

#[test]
fn every_case_snapshots_and_converges() {
    let cases = common::load_cases();
    assert!(
        cases.len() >= 45,
        "the case corpus must stay complete (found {})",
        cases.len()
    );
    for case in &cases {
        let script = common::run_case(case);
        // One snapshot per case, named after the directory. Using the
        // case name (not a line number) keeps review diffs readable.
        assert_snapshot!(case.name.as_str(), script);
    }
}

#[test]
fn case_names_are_stable_and_unique() {
    let cases = common::load_cases();
    let mut names: Vec<&str> = cases.iter().map(|c| c.name.as_str()).collect();
    let total = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), total, "case names must be unique");
}

#[test]
fn identical_models_produce_empty_plans() {
    let cases = common::load_cases();
    for case in &cases {
        let from = pgdelta_core::catalog_from_sql(&case.from_sql, "from.sql").unwrap();
        let planned = pgdelta_core::plan(
            &from,
            &from,
            &format!("<{}/from.sql>", case.name),
            &format!("<{}/from.sql>", case.name),
        )
        .unwrap();
        assert!(
            planned.is_empty(),
            "{}: diff(a, a) must be empty",
            case.name
        );
    }
}
