use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

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

    /// Validate a session name: must be non-empty and contain only [a-zA-Z0-9_-].
    fn validate_name(name: &str) -> anyhow::Result<()> {
        if name.is_empty() {
            anyhow::bail!("session name must not be empty");
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            anyhow::bail!(
                "invalid session name '{}': only [a-zA-Z0-9_-] allowed",
                name
            );
        }
        Ok(())
    }

    /// Allocate a session name if none provided, and validate it.
    pub fn allocate_name(&self, requested: Option<String>) -> anyhow::Result<String> {
        if let Some(name) = requested {
            Self::validate_name(&name)?;
            if self.sessions.contains_key(&name) {
                anyhow::bail!("session '{}' already exists", name);
            }
            return Ok(name);
        }
        loop {
            let n = SESSION_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
            let name = n.to_string();
            if !self.sessions.contains_key(&name) {
                return Ok(name);
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
        env: Option<HashMap<String, String>>,
        cwd: Option<String>,
    ) -> anyhow::Result<String> {
        let name = self.allocate_name(name)?;
        let session = Session::spawn(name.clone(), cmd, cols, rows, env, cwd)?;
        self.sessions.insert(name.clone(), session);
        Ok(name)
    }

    /// Build a SessionInfo from a Session.
    fn session_info(s: &Session, now: std::time::SystemTime) -> SessionInfo {
        let uptime_secs = now
            .duration_since(s.created_at)
            .unwrap_or_default()
            .as_secs();
        let created_at = format_system_time(s.created_at);
        let last_activity_time = s
            .last_activity
            .lock()
            .map(|ts| *ts)
            .unwrap_or(s.created_at);
        let idle_secs = now
            .duration_since(last_activity_time)
            .unwrap_or_default()
            .as_secs();
        let last_activity = format_system_time(last_activity_time);
        let exit_code = s.exit_code.lock().ok().and_then(|ec| *ec);
        let output_bytes = s.total_output_bytes.load(std::sync::atomic::Ordering::Relaxed);
        SessionInfo {
            name: s.name.clone(),
            command: s.command.clone(),
            pid: s.child_pid.as_raw() as u32,
            alive: s.is_alive(),
            created_at,
            uptime_secs,
            last_activity,
            idle_secs,
            exit_code,
            output_bytes,
        }
    }

    /// List all sessions.
    pub fn list(&self) -> Vec<SessionInfo> {
        let now = std::time::SystemTime::now();
        self.sessions
            .values()
            .map(|s| Self::session_info(s, now))
            .collect()
    }

    /// Get detailed info for a single session.
    pub fn info(&self, name: &str) -> Option<SessionInfo> {
        let now = std::time::SystemTime::now();
        self.sessions.get(name).map(|s| Self::session_info(s, now))
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

    /// Re-probe each session's child via `waitpid(WNOHANG)` and reap any
    /// whose child has exited. This is intended to be called after the
    /// daemon detects it has been suspended (macOS App Nap, system sleep)
    /// and wants to catch up on child-death events that may have been
    /// missed while the runtime wasn't scheduled.
    ///
    /// For each child found exited, records the exit code and `died_at`
    /// (only if not already set, to avoid clobbering values written by the
    /// session's own io_loop). Returns the names of sessions whose
    /// children were reaped here.
    pub fn probe_after_resume(&self) -> Vec<String> {
        use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
        let now = std::time::SystemTime::now();
        let mut reaped = Vec::new();
        for (name, session) in &self.sessions {
            let code = match waitpid(session.child_pid, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(_, code)) => Some(code),
                Ok(WaitStatus::Signaled(_, sig, _)) => Some(128 + sig as i32),
                _ => continue, // Still alive, already reaped, or error.
            };
            if let Ok(mut ec) = session.exit_code.lock() {
                if ec.is_none() {
                    *ec = code;
                }
            }
            if let Ok(mut da) = session.died_at.lock() {
                if da.is_none() {
                    *da = Some(now);
                }
            }
            reaped.push(name.clone());
        }
        reaped
    }

    /// Remove dead sessions that have been dead for longer than the retention period.
    /// Dead sessions are kept for 5 minutes so their exit codes remain visible in `ls`.
    pub fn reap_dead(&mut self) -> Vec<String> {
        let now = std::time::SystemTime::now();
        let retention = std::time::Duration::from_secs(300); // 5 minutes
        let dead: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, s)| {
                if s.is_alive() {
                    return false;
                }
                // Only reap if died_at is set and older than retention period.
                match s.died_at.lock().ok().and_then(|da| *da) {
                    Some(died) => now.duration_since(died).unwrap_or_default() > retention,
                    None => false, // Keep if died_at not yet set (race with io_loop).
                }
            })
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

    #[test]
    fn test_allocate_name_with_provided_name() {
        let reg = Registry::new();
        let name = reg.allocate_name(Some("my-session".to_string())).unwrap();
        assert_eq!(name, "my-session");
    }

    #[test]
    fn test_allocate_name_auto_generates_when_none() {
        let reg = Registry::new();
        let name = reg.allocate_name(None).unwrap();
        // Auto-generated names are numeric strings.
        assert!(name.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn test_allocate_name_rejects_empty() {
        let reg = Registry::new();
        let err = reg.allocate_name(Some("".to_string())).unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn test_allocate_name_rejects_invalid_chars() {
        let reg = Registry::new();
        let err = reg.allocate_name(Some("my session!".to_string())).unwrap_err();
        assert!(err.to_string().contains("only [a-zA-Z0-9_-] allowed"));
    }

    #[test]
    fn test_allocate_name_accepts_underscores_and_hyphens() {
        let reg = Registry::new();
        let name = reg.allocate_name(Some("my_session-1".to_string())).unwrap();
        assert_eq!(name, "my_session-1");
    }

    #[tokio::test]
    async fn test_create_named_session() {
        let mut reg = Registry::new();
        let name = reg
            .create(
                Some("test-named".to_string()),
                &["echo".to_string(), "hello".to_string()],
                80,
                24,
                None,
                None,
            )
            .unwrap();
        assert_eq!(name, "test-named");

        // Session should be findable by name.
        assert!(reg.get("test-named").is_some());
        assert!(reg.get("nonexistent").is_none());
    }

    #[tokio::test]
    async fn test_create_duplicate_name_fails() {
        let mut reg = Registry::new();
        reg.create(
            Some("dup-test".to_string()),
            &["echo".to_string()],
            80,
            24,
            None,
            None,
        )
        .unwrap();

        let err = reg
            .create(
                Some("dup-test".to_string()),
                &["echo".to_string()],
                80,
                24,
                None,
                None,
            )
            .unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[tokio::test]
    async fn test_named_session_appears_in_list() {
        let mut reg = Registry::new();
        reg.create(
            Some("listed-session".to_string()),
            &["echo".to_string()],
            80,
            24,
            None,
            None,
        )
        .unwrap();

        let list = reg.list();
        assert!(list.iter().any(|s| s.name == "listed-session"));
    }

    #[tokio::test]
    async fn test_named_session_info_lookup() {
        let mut reg = Registry::new();
        reg.create(
            Some("info-test".to_string()),
            &["echo".to_string()],
            80,
            24,
            None,
            None,
        )
        .unwrap();

        let info = reg.info("info-test").unwrap();
        assert_eq!(info.name, "info-test");
        assert!(info.alive);
    }

    #[tokio::test]
    async fn test_probe_after_resume_reaps_exited_child() {
        let mut reg = Registry::new();
        reg.create(
            Some("probe-test".to_string()),
            // `true` exits with status 0 immediately.
            &["true".to_string()],
            80,
            24,
            None,
            None,
        )
        .unwrap();

        // Wait for the child to actually exit. Without a yield the
        // watchdog probe might race the child's own setup.
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            // is_alive() uses kill -0, which returns OK for zombies, so
            // it isn't a reliable signal — break unconditionally after
            // a short wait and let probe_after_resume handle it.
            break;
        }
        // Give the child time to exit and become a zombie.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let reaped = reg.probe_after_resume();
        // Either we reaped it here, or the io_loop already did — both are
        // legal outcomes. What matters is that exit_code is populated.
        let session = reg.get("probe-test").expect("session still present");
        // Wait briefly for either path to populate exit_code.
        let mut got_code = None;
        for _ in 0..50 {
            if let Ok(ec) = session.exit_code.lock() {
                if ec.is_some() {
                    got_code = *ec;
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(got_code, Some(0), "expected exit code 0, reaped={:?}", reaped);
        // died_at should be populated whichever path reaped the child.
        assert!(
            session.died_at.lock().unwrap().is_some(),
            "expected died_at to be populated"
        );
    }

    #[tokio::test]
    async fn test_probe_after_resume_preserves_existing_exit_code() {
        // Invariant: probe_after_resume must not clobber an exit_code
        // already set by the session's io_loop. Without this guard, the
        // watchdog would race io_loop and either could overwrite the
        // other with stale data — see the matching `if ec.is_none()`
        // checks in registry::probe_after_resume and session::io_loop.
        let mut reg = Registry::new();
        reg.create(
            Some("preserve".to_string()),
            &["true".to_string()],
            80,
            24,
            None,
            None,
        )
        .unwrap();

        // Wait for the child to exit and io_loop to settle.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Inject a sentinel value as if io_loop had already recorded it.
        let sentinel = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(42);
        {
            let session = reg.get("preserve").expect("session present");
            *session.exit_code.lock().unwrap() = Some(99);
            *session.died_at.lock().unwrap() = Some(sentinel);
        }

        let _ = reg.probe_after_resume();

        let session = reg.get("preserve").expect("session still present");
        assert_eq!(
            *session.exit_code.lock().unwrap(),
            Some(99),
            "probe_after_resume must not overwrite a pre-set exit_code"
        );
        assert_eq!(
            *session.died_at.lock().unwrap(),
            Some(sentinel),
            "probe_after_resume must not overwrite a pre-set died_at"
        );
    }

    #[tokio::test]
    async fn test_probe_after_resume_skips_alive_child() {
        let mut reg = Registry::new();
        reg.create(
            Some("probe-alive".to_string()),
            &["sleep".to_string(), "60".to_string()],
            80,
            24,
            None,
            None,
        )
        .unwrap();
        let reaped = reg.probe_after_resume();
        assert!(
            reaped.is_empty(),
            "alive child must not be reaped: {:?}",
            reaped
        );
        // Cleanup.
        let _ = reg.kill("probe-alive");
    }

    #[tokio::test]
    async fn test_kill_named_session() {
        let mut reg = Registry::new();
        reg.create(
            Some("kill-me".to_string()),
            &["sleep".to_string(), "60".to_string()],
            80,
            24,
            None,
            None,
        )
        .unwrap();

        assert!(reg.get("kill-me").is_some());
        reg.kill("kill-me").unwrap();
        assert!(reg.get("kill-me").is_none());
    }
}
