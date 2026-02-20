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
        let now = std::time::SystemTime::now();
        self.sessions
            .values()
            .map(|s| {
                let uptime_secs = now
                    .duration_since(s.created_at)
                    .unwrap_or_default()
                    .as_secs();
                let created_at = format_system_time(s.created_at);
                SessionInfo {
                    name: s.name.clone(),
                    command: s.command.clone(),
                    pid: s.child_pid.as_raw() as u32,
                    alive: s.is_alive(),
                    created_at,
                    uptime_secs,
                }
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

    /// Kill all sessions. Returns the number killed.
    pub fn kill_all(&mut self) -> usize {
        let names: Vec<String> = self.sessions.keys().cloned().collect();
        let count = names.len();
        for name in &names {
            if let Some(mut session) = self.sessions.remove(name) {
                if let Some(kill_tx) = session.kill_tx.take() {
                    let _ = kill_tx.send(());
                }
            }
        }
        count
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

/// Format a SystemTime as an ISO 8601 UTC string (no external deps).
fn format_system_time(t: std::time::SystemTime) -> String {
    let dur = t
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();

    // Simple UTC calendar calculation.
    let days = secs / 86400;
    let time_secs = secs % 86400;
    let hours = time_secs / 3600;
    let minutes = (time_secs % 3600) / 60;
    let seconds = time_secs % 60;

    // Days since epoch to Y-M-D (civil calendar from days).
    let (year, month, day) = days_to_date(days as i64);

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hours, minutes, seconds
    )
}

/// Convert days since Unix epoch to (year, month, day).
fn days_to_date(days: i64) -> (i64, u32, u32) {
    // Algorithm from Howard Hinnant's chrono-compatible date library.
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn test_format_unix_epoch() {
        let t = UNIX_EPOCH;
        assert_eq!(format_system_time(t), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn test_format_known_timestamp() {
        // 2026-02-20T14:30:00Z = 1771597800 seconds since epoch
        let t = UNIX_EPOCH + Duration::from_secs(1771597800);
        assert_eq!(format_system_time(t), "2026-02-20T14:30:00Z");
    }

    #[test]
    fn test_format_system_time_now() {
        let t = std::time::SystemTime::now();
        let s = format_system_time(t);
        // Should be a valid ISO 8601 string starting with "20"
        assert!(s.starts_with("20"));
        assert!(s.ends_with('Z'));
        assert_eq!(s.len(), 20);
    }
}
