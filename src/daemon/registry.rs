use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::daemon::session::Session;
use crate::protocol::SessionInfo;

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct Registry {
    sessions: HashMap<String, Session>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::new(),
        }
    }

    /// Allocate a session name if none provided.
    pub fn allocate_name(&self, requested: Option<String>) -> String {
        if let Some(name) = requested {
            return name;
        }
        loop {
            let n = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
            let name = n.to_string();
            if !self.sessions.contains_key(&name) {
                return name;
            }
        }
    }

    /// Create a new session.
    pub fn create(
        &mut self,
        name: Option<String>,
        cmd: &[String],
        cols: u16,
        rows: u16,
    ) -> anyhow::Result<String> {
        let name = self.allocate_name(name);
        if self.sessions.contains_key(&name) {
            anyhow::bail!("session '{}' already exists", name);
        }
        let session = Session::spawn(name.clone(), cmd, cols, rows)?;
        self.sessions.insert(name.clone(), session);
        Ok(name)
    }

    /// List all sessions.
    pub fn list(&self) -> Vec<SessionInfo> {
        self.sessions
            .values()
            .map(|s| SessionInfo {
                name: s.name.clone(),
                command: s.command.clone(),
                pid: s.child_pid.as_raw() as u32,
                alive: s.is_alive(),
            })
            .collect()
    }

    /// Kill a session by name.
    pub fn kill(&mut self, name: &str) -> anyhow::Result<()> {
        let mut session = self
            .sessions
            .remove(name)
            .ok_or_else(|| anyhow::anyhow!("session '{}' not found", name))?;
        if let Some(kill_tx) = session.kill_tx.take() {
            let _ = kill_tx.send(());
        }
        Ok(())
    }

    /// Get a session by name.
    pub fn get(&self, name: &str) -> Option<&Session> {
        self.sessions.get(name)
    }

    /// Get a mutable session by name.
    pub fn get_mut(&mut self, name: &str) -> Option<&mut Session> {
        self.sessions.get_mut(name)
    }

    /// Remove dead sessions (reaper).
    pub fn reap_dead(&mut self) -> Vec<String> {
        let dead: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, s)| !s.is_alive())
            .map(|(k, _)| k.clone())
            .collect();
        for name in &dead {
            self.sessions.remove(name);
        }
        dead
    }
}
