use crate::client;
use crate::protocol::messages::{ClientMessage, DaemonMessage};
use crate::util::parse_env_vars;

pub fn do_respawn(
    name: &str,
    cwd: Option<String>,
    env: Vec<String>,
    cmd: Vec<String>,
) -> anyhow::Result<()> {
    let env_map = parse_env_vars(&env)?;
    let resp = client::request(&ClientMessage::RespawnSession {
        name: name.to_string(),
        command: cmd,
        cwd,
        env: env_map,
    })?;
    match resp {
        DaemonMessage::Ok => Ok(()),
        DaemonMessage::Error(e) => {
            eprintln!("amux: error: {}", e);
            std::process::exit(1);
        }
        other => {
            eprintln!("amux: unexpected: {:?}", other);
            std::process::exit(1);
        }
    }
}
