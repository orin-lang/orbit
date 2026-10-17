//! The `orbit` command line tool: run and inspect Orbit modules.

mod inspect;
mod run;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "orbit",
    version,
    about = "Run and inspect Orbit WebAssembly modules"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Instantiate a module in an isolate and optionally invoke an export.
    ///
    /// Capabilities are denied by default; pass --grant for each one the module may use.
    Run(run::RunArgs),
    /// Print a module's imports, exports, manifest and required capabilities.
    Inspect(InspectArgs),
}

#[derive(clap::Args)]
pub struct InspectArgs {
    /// Path to a .wasm or .wat file.
    module: PathBuf,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Run(args) => run::run(args).await,
        Command::Inspect(args) => inspect::inspect(&args.module),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
