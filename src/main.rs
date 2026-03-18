mod cli;
mod client;
mod commands;
mod common;
mod daemon;
mod protocol;
mod util;

use clap::Parser;

fn main() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();

    let command = cli.command.unwrap_or_else(|| {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        cli::Command::New {
            name: None,
            detached: false,
            env: Vec::new(),
            cwd: None,
            worktree: None,
            cmd: vec![shell],
        }
    });

    commands::dispatch(command)
}
