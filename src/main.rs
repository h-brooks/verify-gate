use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    name = "verify-gate",
    version,
    about = "Claude Code Stop hook that blocks finishing a turn when files were edited but never verified"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Read a Stop-hook JSON payload from stdin; print a block decision if the rule fires.
    Hook {
        /// Skip the rule entirely and allow (also triggered by VERIFY_GATE_DISABLE=1).
        #[arg(long)]
        dry_run: bool,
    },
    /// Evaluate a recorded transcript file directly. The test/CI seam for the rule.
    Check {
        transcript: PathBuf,
        #[arg(long, value_enum, default_value = "text")]
        format: Format,
    },
    /// Write .verify-gate.toml with defaults and print the settings.json hooks snippet.
    Init,
}

#[derive(Copy, Clone, ValueEnum)]
enum Format {
    Text,
    Json,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Hook { dry_run } => {
            // The harness runs this on every Stop; a panic here must never
            // take the agent down with it.
            if std::panic::catch_unwind(|| verify_gate::hook::run_hook(dry_run)).is_err() {
                eprintln!("verify-gate: internal panic, allowing");
            }
            ExitCode::SUCCESS
        }
        Command::Check { transcript, format } => {
            let config = verify_gate::config::Config::load(std::env::current_dir().ok().as_deref());
            match verify_gate::transcript::evaluate_transcript(&transcript, &config) {
                Ok(report) => {
                    let out = match format {
                        Format::Text => verify_gate::report::format_text(&report),
                        Format::Json => verify_gate::report::format_json(&report),
                    };
                    print!("{out}");
                    if !out.ends_with('\n') {
                        println!();
                    }
                    if report.is_block() {
                        ExitCode::FAILURE
                    } else {
                        ExitCode::SUCCESS
                    }
                }
                Err(e) => {
                    eprintln!("verify-gate: error: {e:#}");
                    ExitCode::from(2)
                }
            }
        }
        Command::Init => match verify_gate::init::run_init() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("verify-gate: error: {e:#}");
                ExitCode::from(2)
            }
        },
    }
}
