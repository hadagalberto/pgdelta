# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] — milestone 1

### Added

- Cargo workspace with `pgdelta-core` (library) and `pgdelta` (`clap` CLI).
- Normalised catalog model: schemas, extensions, tables, columns (defaults,
  generated, identity, collation), constraints and indexes.
- SQL file and SQL directory sources, applied in statement order to the model.
- Diff and dependency-ordered migration plans, with transactional and
  non-transactional sections.
- `pgdelta diff` (SQL/JSON output) and `pgdelta inspect` subcommands.
- Parser decision (`sqlparser-rs` fallback, with the reasons recorded in
  `docs/parser.md`).
- Table-driven insta snapshot fixtures, property tests and a live-database
  convergence suite wired into the PostgreSQL 13–18 CI matrix.
- MIT license, bilingual READMEs, contributing guide, `.gitattributes` (LF).
