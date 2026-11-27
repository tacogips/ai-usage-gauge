mod cli;
mod client;
mod rendering;
mod runtime;

use clap::Parser;

use ccusage_core::paths::AppPaths;

use crate::cli::{Cli, Commands};

fn main() {
    let parsed = match Cli::try_parse() {
        Ok(c) => c,
        Err(err) => {
            let code = err.exit_code();
            let _ = err.print();
            std::process::exit(code);
        }
    };

    let paths = AppPaths::production();
    let result = match parsed.command {
        Commands::ConfigCheck => runtime::run_config_check(&paths),
        Commands::UsageSnapshot { json } => runtime::run_usage_snapshot(&paths, json),
        Commands::Dashboard => runtime::run_dashboard(&paths),
        Commands::Serve { port, assets } => {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            rt.block_on(runtime::run_serve(&paths, port, assets))
        }
        Commands::Client {
            port,
            json,
            subcommand,
        } => {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            rt.block_on(client::run_client(&paths, port, json, subcommand))
        }
    };

    if let Err(code) = result {
        std::process::exit(code);
    }
}
