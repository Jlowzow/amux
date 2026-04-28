use std::io::Write;
use std::path::{Path, PathBuf};

use crate::client;
use crate::common::runtime_dir;
use crate::protocol::messages::{ClientMessage, DaemonMessage};
use crate::util::parse_env_vars;

/// `amux handoff` — stage a handoff message, then atomically cycle the
/// session's child via the `RespawnSession` primitive (bd-wh4).
///
/// Two consumers (per bd-uhp):
///   1. Orchestrator `/handoff` — when the orchestrator runs inside an
///      amux session, this swaps its claude in place with fresh context.
///   2. Worker→agent pipelines — a finishing worker can chain a follow-up
///      agent inside its own session without the orchestrator spawning a
///      fresh session.
pub fn do_handoff(
    name: Option<String>,
    message: Option<String>,
    prime: Option<String>,
    cwd: Option<String>,
    env: Vec<String>,
    cmd: Vec<String>,
) -> anyhow::Result<()> {
    // 1. Resolve the target session.
    let name = match name {
        Some(n) if !n.is_empty() => n,
        _ => match std::env::var("AMUX_SESSION") {
            Ok(v) if !v.is_empty() => v,
            _ => anyhow::bail!(
                "no target session — pass -n <name> or run inside an amux session \
                 (AMUX_SESSION must be set)"
            ),
        },
    };

    // 2. Stage the handoff message, if any.
    let mut staged_path: Option<PathBuf> = None;
    if let Some(text) = message.as_deref() {
        let path = handoff_message_path(&name);
        write_handoff_message(&path, text)?;
        staged_path = Some(path);
    }

    // 3. Build the restart command. Default to `claude`; with `--prime`
    //    forward the prompt as claude's first message arg. An explicit
    //    positional `-- <cmd...>` always wins.
    let cmd = if cmd.is_empty() {
        let mut v = vec!["claude".to_string()];
        if let Some(p) = prime {
            v.push(p);
        }
        v
    } else {
        cmd
    };

    let env_map = parse_env_vars(&env)?;

    // 4. Hand off to the daemon's respawn machinery.
    let resp = client::request(&ClientMessage::RespawnSession {
        name: name.clone(),
        command: cmd,
        cwd,
        env: env_map,
    })?;
    match resp {
        DaemonMessage::Ok => {
            match staged_path {
                Some(p) => println!(
                    "Session {} handed off (handoff message at {}).",
                    name,
                    p.display()
                ),
                None => println!("Session {} handed off.", name),
            }
            Ok(())
        }
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

/// Path where `amux handoff --message` stages the message for `<name>`.
/// Per-instance (runtime_dir is instance-aware after bd-qz6).
pub fn handoff_message_path(name: &str) -> PathBuf {
    runtime_dir().join("handoff").join(format!("{}.msg", name))
}

/// Atomically write `text` to `path`: tempfile beside `path`, fsync, then
/// rename into place. The next session reads (and clears) this file on
/// startup; we never want a half-written message read.
fn write_handoff_message(path: &Path, text: &str) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("handoff path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent)?;

    let pid = std::process::id();
    let tmp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("handoff"),
        pid
    ));

    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    // rename(2) on the same filesystem is atomic — readers see either the
    // old contents or the new, never a torn write.
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handoff_message_path_uses_runtime_dir_and_name() {
        // The path must be inside runtime_dir/handoff/, named "<name>.msg".
        let p = handoff_message_path("orch");
        assert_eq!(
            p.file_name().and_then(|s| s.to_str()),
            Some("orch.msg"),
            "filename must be <name>.msg, got {}",
            p.display()
        );
        let parent = p.parent().expect("has parent");
        assert_eq!(parent.file_name().and_then(|s| s.to_str()), Some("handoff"));
        // And that parent is itself inside runtime_dir.
        let grandparent = parent.parent().expect("has grandparent");
        assert_eq!(grandparent, runtime_dir());
    }

    #[test]
    fn write_handoff_message_creates_dir_and_writes_atomically() {
        // Use an isolated subdir of /tmp so we don't fight runtime_dir
        // (which might or might not exist depending on whether the test
        // runner has ever started a daemon).
        let dir = std::env::temp_dir()
            .join(format!("amux-handoff-test-{}-{}", std::process::id(), line!()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("handoff").join("orch.msg");

        write_handoff_message(&path, "hello handoff").expect("write should succeed");

        let read = std::fs::read_to_string(&path).expect("file must exist");
        assert_eq!(read, "hello handoff");

        // No leftover tempfile.
        let parent = path.parent().unwrap();
        let leftover: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .map(|s| s.starts_with('.') && s.ends_with(".tmp"))
                    .unwrap_or(false)
            })
            .collect();
        assert!(
            leftover.is_empty(),
            "tempfile must be renamed away, found: {:?}",
            leftover
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_handoff_message_overwrites_existing() {
        let dir = std::env::temp_dir()
            .join(format!("amux-handoff-overwrite-{}-{}", std::process::id(), line!()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("handoff").join("orch.msg");

        write_handoff_message(&path, "first").unwrap();
        write_handoff_message(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
