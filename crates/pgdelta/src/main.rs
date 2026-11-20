//! pgdelta: diff two PostgreSQL schemas and generate a safe, ordered
//! migration script.
//!
//! `diff` (SQL/JSON output) and `inspect` support SQL files, SQL directories
//! and live `postgres://` sources. `--shadow postgres://...` applies a SQL
//! source to a throwaway database and introspects it, which is the most
//! accurate source mode because PostgreSQL itself validates and normalises
//! the DDL. `--online` rewrites risky steps into safe sequences, `--rename`
//! supplies opt-in rename hints, and `lint` analyses a handwritten migration
//! with the same safety rules (see `docs/parser.md` and the SPEC).

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use clap::{Parser, Subcommand, ValueEnum};
use pgdelta_core::{
    ColorChoice, HazardLine, PlanOptions, SourceSpec, StatementInput, analyze_statements,
    load_catalog_with_shadow, parse_rename_hint, plan_with_options, render_hazard_report,
    render_lint_json,
};

/// Exit code used when an operational error occurred.
const EXIT_ERROR: u8 = 1;
/// Exit code used when hazards exist and `--fail-on-hazard` was requested.
const EXIT_HAZARD: u8 = 2;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match &cli.command {
        Commands::Diff(diff) => run_diff(diff),
        Commands::Inspect(inspect) => run_inspect(inspect),
        Commands::Lint(lint) => run_lint(lint),
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("pgdelta: {error:#}");
            ExitCode::from(EXIT_ERROR)
        }
    }
}

/// Colour mode accepted on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ColorArg {
    /// Colour when stderr is a terminal.
    Auto,
    /// Always emit ANSI colour.
    Always,
    /// Never emit ANSI colour.
    Never,
}

impl From<ColorArg> for ColorChoice {
    fn from(value: ColorArg) -> Self {
        match value {
            ColorArg::Auto => ColorChoice::Auto,
            ColorArg::Always => ColorChoice::Always,
            ColorArg::Never => ColorChoice::Never,
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
    /// Analyse a handwritten migration for destructive and locking hazards.
    Lint(LintArgs),
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
    /// Rewrite risky steps into online-safe sequences (NOT VALID →
    /// VALIDATE → SET NOT NULL, FK NOT VALID → VALIDATE, CREATE INDEX
    /// CONCURRENTLY outside a transaction).
    #[arg(long)]
    online: bool,
    /// Opt-in rename hint, e.g. `--rename table.old=new` or
    /// `--rename column.users.email=contact`. May be repeated.
    #[arg(long = "rename", value_name = "HINT")]
    renames: Vec<String>,
    /// Exit with code 2 instead of 0 when the plan contains hazards.
    #[arg(long)]
    fail_on_hazard: bool,
    /// Colour in the human summary on stderr.
    #[arg(long, value_enum, default_value = "auto")]
    color: ColorArg,
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

/// Arguments of `pgdelta lint`.
#[derive(clap::Args)]
struct LintArgs {
    /// Handwritten migration script to analyse.
    file: PathBuf,
    /// Print the analysis as JSON instead of a human summary.
    #[arg(long)]
    json: bool,
    /// Exit with code 2 instead of 0 when the script contains hazards.
    #[arg(long)]
    fail_on_hazard: bool,
    /// Colour in the human summary on stderr.
    #[arg(long, value_enum, default_value = "auto")]
    color: ColorArg,
}

fn run_diff(args: &DiffArgs) -> Result<ExitCode, anyhow::Error> {
    let from_spec =
        SourceSpec::parse(&args.from).with_context(|| format!("parsing --from {:?}", args.from))?;
    let to_spec =
        SourceSpec::parse(&args.to).with_context(|| format!("parsing --to {:?}", args.to))?;

    let renames = args
        .renames
        .iter()
        .map(|hint| parse_rename_hint(hint))
        .collect::<pgdelta_core::Result<Vec<_>>>()
        .context("parsing --rename hints")?;
    let options = PlanOptions {
        online: args.online,
        renames,
    };

    let from = load_catalog_with_shadow(&from_spec, args.shadow.as_deref())
        .with_context(|| format!("loading --from ({})", from_spec.describe()))?;
    let to = load_catalog_with_shadow(&to_spec, args.shadow.as_deref())
        .with_context(|| format!("loading --to ({})", to_spec.describe()))?;

    let planned = plan_with_options(
        &from,
        &to,
        &from_spec.describe(),
        &to_spec.describe(),
        &options,
    )
    .context("planning the migration")?;

    let summary = planned.summary();
    eprintln!(
        "{} statement(s): {} create, {} alter, {} drop",
        summary.total, summary.creates, summary.alters, summary.drops
    );
    eprint!(
        "{}",
        render_hazard_report(
            &pgdelta_core::plan_hazard_lines(&planned),
            args.color.into()
        )
    );

    if planned.is_empty() {
        eprintln!("no changes: the schemas already converge");
        return Ok(ExitCode::SUCCESS);
    }

    if args.json {
        let json = pgdelta_core::plan::render_json(&planned).context("rendering the JSON plan")?;
        write_output(args, serde_json::to_string_pretty(&json)?.as_bytes())?;
    } else {
        let sql = pgdelta_core::plan::render_sql(&planned);
        write_output(args, sql.as_bytes())?;
    }

    if args.fail_on_hazard && summary.hazards > 0 {
        return Ok(ExitCode::from(EXIT_HAZARD));
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

fn run_lint(args: &LintArgs) -> Result<ExitCode, anyhow::Error> {
    let text = std::fs::read_to_string(&args.file)
        .with_context(|| format!("reading {}", args.file.display()))?;
    let statements = pgdelta_core::split_sql_statements(&text)
        .with_context(|| format!("tokenizing {}", args.file.display()))?;
    let inputs: Vec<StatementInput<'_>> =
        statements.iter().map(|s| StatementInput::sql(s)).collect();
    let hazards = analyze_statements(&inputs);

    if args.json {
        let json = render_lint_json(&args.file.display().to_string(), &statements, &hazards)
            .context("rendering the lint JSON")?;
        println!("{}", serde_json::to_string_pretty(&json)?);
    } else {
        let lines: Vec<HazardLine> = statements
            .iter()
            .zip(&hazards)
            .enumerate()
            .filter(|(_, (_, statement_hazards))| !statement_hazards.is_empty())
            .map(|(index, (sql, statement_hazards))| {
                HazardLine::new(
                    format!("statement {}", index + 1),
                    sql.clone(),
                    statement_hazards.clone(),
                )
            })
            .collect();
        eprint!("{}", render_hazard_report(&lines, args.color.into()));
        eprintln!(
            "{} statement(s) analysed in {}",
            statements.len(),
            args.file.display()
        );
    }

    let total: usize = hazards.iter().map(Vec::len).sum();
    if args.fail_on_hazard && total > 0 {
        return Ok(ExitCode::from(EXIT_HAZARD));
    }
    Ok(ExitCode::SUCCESS)
}
