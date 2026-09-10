//! `rusta` command-line entry point — development plan §6.9 (R3).
//!
//! Milestone M0 ships the binary surface (`rusta --version`, `rusta --help`).
//! The Aider-style interactive REPL, `/commands`, config loading, and the
//! non-interactive `-c` mode arrive with milestones M1–M8 per the plan.

use clap::Parser;

/// Rusta: a lean AI coding-agent harness for small, locally hosted LLMs.
#[derive(Parser)]
#[command(
    name = "rusta",
    version,
    about = "Lean AI coding-agent harness for small, locally hosted LLMs (8B–35B)",
    long_about = None
)]
struct Cli;

fn main() {
    // M0: parse and acknowledge. The agent REPL (plan §6.9) is wired in later
    // milestones; until then the binary exposes exactly `--version` / `--help`.
    let _cli = Cli::parse();
    println!(
        "rusta {} — lean AI coding-agent harness for small local LLMs",
        env!("CARGO_PKG_VERSION")
    );
}
