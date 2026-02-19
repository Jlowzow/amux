mod client;
mod common;
mod server;

use clap::{Parser, Subcommand};

use crate::common::{Request, Response};

#[derive(Parser)]
#[command(name = "amux", about = "A terminal multiplexer")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the daemon server.
    StartServer,
    /// Stop the daemon server.
    KillServer,
    /// Ping the server (health check).
    Ping,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::StartServer => {
            if common::server_running() {
                eprintln!("amux: server is already running");
                return Ok(());
            }
            server::fork_daemon()?;
        }
        Command::KillServer => {
            let resp = client::request(&Request::KillServer)?;
            match resp {
                Response::Ok => eprintln!("amux: server stopped"),
                Response::Error(e) => eprintln!("amux: error: {}", e),
                other => eprintln!("amux: unexpected response: {:?}", other),
            }
        }
        Command::Ping => {
            let resp = client::request(&Request::Ping)?;
            match resp {
                Response::Pong => println!("pong"),
                other => eprintln!("amux: unexpected response: {:?}", other),
            }
        }
    }

    Ok(())
}
