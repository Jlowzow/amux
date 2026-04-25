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

    // Propagate --instance into the env so that:
    //   1. forked daemon children (which only see env, not argv) inherit it,
    //   2. nested `amux` calls in scripts see the same instance.
    if let Some(instance) = cli.instance.as_deref() {
        // SAFETY: set_var is unsafe in edition 2024+; we run before any
        // threads are spawned.
        unsafe {
            std::env::set_var(common::INSTANCE_ENV, instance);
        }
    }

    let command = cli.command.unwrap_or_else(|| {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        cli::Command::New {
            name: None,
            detached: false,
            env: Vec::new(),
            cwd: None,
            worktree: None,
            init_message: None,
            cmd: vec![shell],
        }
    });

    commands::dispatch(command)
}
