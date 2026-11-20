//! pgdelta: diff two PostgreSQL schemas and generate a safe, ordered
//! migration script.
//!
//! `diff` (SQL/JSON output) and `inspect` support SQL files, SQL directories
//! and live `postgres://` sources. `--shadow postgres://...` applies a SQL
//! source to a throwaway database and introspects it, which is the most
//! accurate source mode because PostgreSQL itself validates and normalises
//! the DDL. The `--online` rewrite mode and `lint` land with their milestones
//! (see `docs/parser.md` and the roadmap in `README.md`).

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use clap::{Parser, Subcommand};
use pgdelta_core::{SourceSpec, load_catalog_with_shadow, plan};

/// Exit code used when an operational error occurred.
const EXIT_ERROR: u8 = 1;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match &cli.command {
        Commands::Diff(diff) => run_diff(diff),
        Commands::Inspect(inspect) => run_inspect(inspect),
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("pgdelta: {error:#}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

/// The pgdelta command line.
#[derive(Parser)]
#[command(
    name = "pgdelta",
    version,
    about = "Diff two PostgreSQL schemas and generate a safe, ordered migration script",
    long_about = "pgdelta diffs two PostgreSQL schemas — SQL files or directories of \
                  .sql files — and generates a dependency-ordered migration script with \
                  clearly separated transactional and non-transactional sections."
)]
struct Cli {
    /// The subcommand to run.
    #[command(subcommand)]
    command: Commands,
}

/// Subcommands.
#[derive(Subcommand)]
enum Commands {
    /// Generate the migration script (or JSON plan) from one schema to
    /// another.
    Diff(DiffArgs),
    /// Dump the normalised catalog model of a single source as JSON.
    Inspect(InspectArgs),
}

/// Arguments of `pgdelta diff`.
#[derive(clap::Args)]
struct DiffArgs {
    /// Source the migration starts from: a `.sql` file, a directory of
    /// `.sql` files, or a `postgres://` URL.
    #[arg(long)]
    from: String,
    /// Source the migration converges to: same forms as `--from`.
    #[arg(long)]
    to: String,
    /// Apply SQL sources to a fresh database on this server and introspect
    /// the result instead of parsing them in-process.
    #[arg(long, value_name = "URL")]
    shadow: Option<String>,
    /// Write the script to this file instead of stdout.
    #[arg(short, long, value_name = "FILE")]
    output: Option<PathBuf>,
    /// Print the plan as JSON (sections, statements, summary) instead of
    /// SQL.
    #[arg(long)]
    json: bool,
}

/// Arguments of `pgdelta inspect`.
#[derive(clap::Args)]
struct InspectArgs {
    /// The source to inspect: a `.sql` file, a directory of `.sql`
    /// files, or a `postgres://` URL.
    source: String,
    /// Apply a SQL source to a fresh database on this server and introspect
    /// the result instead of parsing it in-process.
    #[arg(long, value_name = "URL")]
    shadow: Option<String>,
}

fn run_diff(args: &DiffArgs) -> Result<ExitCode, anyhow::Error> {
    let from_spec =
        SourceSpec::parse(&args.from).with_context(|| format!("parsing --from {:?}", args.from))?;
    let to_spec =
        SourceSpec::parse(&args.to).with_context(|| format!("parsing --to {:?}", args.to))?;

    let from = load_catalog_with_shadow(&from_spec, args.shadow.as_deref())
        .with_context(|| format!("loading --from ({})", from_spec.describe()))?;
    let to = load_catalog_with_shadow(&to_spec, args.shadow.as_deref())
        .with_context(|| format!("loading --to ({})", to_spec.describe()))?;

    let planned = plan(&from, &to, &from_spec.describe(), &to_spec.describe())
        .context("planning the migration")?;

    let summary = planned.summary();
    if summary.total == 0 {
        eprintln!("no changes: the schemas already converge");
        return Ok(ExitCode::SUCCESS);
    }
    eprintln!(
        "{} statement(s): {} create, {} alter, {} drop",
        summary.total, summary.creates, summary.alters, summary.drops
    );

    if args.json {
        let json = pgdelta_core::plan::render_json(&planned).context("rendering the JSON plan")?;
        write_output(args, serde_json::to_string_pretty(&json)?.as_bytes())?;
    } else {
        let sql = pgdelta_core::plan::render_sql(&planned);
        write_output(args, sql.as_bytes())?;
    }
    Ok(ExitCode::SUCCESS)
}

fn write_output(args: &DiffArgs, bytes: &[u8]) -> Result<(), anyhow::Error> {
    if let Some(path) = &args.output {
        std::fs::write(path, bytes).with_context(|| format!("writing {}", path.display()))?;
    } else {
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        handle
            .write_all(bytes)
            .and_then(|()| handle.flush())
            .context("writing the migration script to stdout")?;
    }
    Ok(())
}

fn run_inspect(args: &InspectArgs) -> Result<ExitCode, anyhow::Error> {
    let spec = SourceSpec::parse(&args.source)
        .with_context(|| format!("parsing source {:?}", args.source))?;
    let catalog = load_catalog_with_shadow(&spec, args.shadow.as_deref())
        .with_context(|| format!("loading source ({})", spec.describe()))?;
    let json = catalog.to_json();
    println!("{}", serde_json::to_string_pretty(&json)?);
    Ok(ExitCode::SUCCESS)
}
