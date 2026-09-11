//! `rusta` binary entry point — a thin shim over [`rusta_cli::run`] so the
//! whole CLI is testable as a library (plan §9).

use clap::Parser;

#[tokio::main]
async fn main() {
    let cli = rusta_cli::Cli::parse();
    if let Err(err) = rusta_cli::run(cli).await {
        eprintln!("rusta: {err:#}");
        std::process::exit(1);
    }
}
