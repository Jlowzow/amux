use crate::client;
use crate::protocol::messages::{ClientMessage, DaemonMessage, SessionInfo};
use crate::util::ensure_daemon_running;

use std::collections::HashMap;
use std::io::{self, Write};
use std::time::Duration;

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    style::{self, Attribute, Color, SetAttribute, SetForegroundColor, ResetColor},
    terminal::{self, ClearType},
};

/// Sparkline characters ordered by intensity (lowest to highest).
const SPARKLINE_CHARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
/// Number of samples in the activity ring buffer.
const ACTIVITY_SAMPLES: usize = 10;

/// Tracks output byte deltas per poll interval for a single session.
struct ActivityTracker {
    /// Ring buffer of byte-count deltas (one per poll tick).
    samples: [u64; ACTIVITY_SAMPLES],
    /// Index of the next sample to write.
    next: usize,
    /// Last observed `output_bytes` value from SessionInfo.
    last_bytes: u64,
}

impl ActivityTracker {
    fn new(initial_bytes: u64) -> Self {
        Self {
            samples: [0; ACTIVITY_SAMPLES],
            next: 0,
            last_bytes: initial_bytes,
        }
    }

    /// Record a new observation of total output bytes.
    fn record(&mut self, current_bytes: u64) {
        let delta = current_bytes.saturating_sub(self.last_bytes);
        self.samples[self.next] = delta;
        self.next = (self.next + 1) % ACTIVITY_SAMPLES;
        self.last_bytes = current_bytes;
    }

    /// Render the sparkline string from the ring buffer.
    fn sparkline(&self) -> String {
        let max = self.samples.iter().copied().max().unwrap_or(0);
        self.samples
            .iter()
            .cycle()
            .skip(self.next) // start from oldest sample
            .take(ACTIVITY_SAMPLES)
            .map(|&val| {
                if max == 0 {
                    SPARKLINE_CHARS[0]
                } else {
                    let idx = ((val as f64 / max as f64) * 7.0).round() as usize;
                    SPARKLINE_CHARS[idx.min(7)]
                }
            })
            .collect()
    }
}

/// Format seconds into a human-readable duration string like "5m32s" or "2h15m".
fn format_duration(secs: u64) -> String {
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        let m = secs / 60;
        let s = secs % 60;
        format!("{}m{:02}s", m, s)
    } else if secs < 86400 {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        format!("{}h{:02}m", h, m)
    } else {
        let d = secs / 86400;
        let h = (secs % 86400) / 3600;
        format!("{}d{:02}h", d, h)
    }
}

/// Sort sessions: alive first, then by name.
fn sort_sessions(sessions: &mut [SessionInfo]) {
    sessions.sort_by(|a, b| {
        b.alive.cmp(&a.alive).then_with(|| a.name.cmp(&b.name))
    });
}

/// Truncate a string to fit within `max_width`, adding "..." if truncated.
fn truncate(s: &str, max_width: usize) -> String {
    if s.len() <= max_width {
        s.to_string()
    } else if max_width <= 3 {
        s[..max_width].to_string()
    } else {
        format!("{}...", &s[..max_width - 3])
    }
}

/// Build the summary line, e.g. "7 sessions (5 alive, 2 dead)".
fn summary_line(sessions: &[SessionInfo]) -> String {
    let total = sessions.len();
    let alive = sessions.iter().filter(|s| s.alive).count();
    let dead = total - alive;
    format!("{} sessions ({} alive, {} dead)", total, alive, dead)
}

/// Render one frame of the dashboard to a buffer.
fn render_frame(sessions: &[SessionInfo], term_cols: u16, trackers: &HashMap<String, ActivityTracker>) -> Vec<String> {
    let mut lines = Vec::new();

    // Header
    // Fixed columns: name(16) + status(8) + pid(8) + uptime(8) + idle(8) + exit(6) + activity(11) + spaces(7) = 72
    let header = format!(
        "{:<16} {:<8} {:>8} {:>8} {:>8} {:>6} {:<11} {}",
        "NAME", "STATUS", "PID", "UPTIME", "IDLE", "EXIT", "ACTIVITY", "COMMAND"
    );
    lines.push(header);

    // Rows
    for s in sessions {
        let status = if s.alive { "alive" } else { "dead" };
        let exit_str = match s.exit_code {
            Some(c) => c.to_string(),
            None => "-".to_string(),
        };
        let uptime = format_duration(s.uptime_secs);
        let idle = format_duration(s.idle_secs);

        let sparkline = trackers
            .get(&s.name)
            .map(|t| t.sparkline())
            .unwrap_or_else(|| SPARKLINE_CHARS[0].to_string().repeat(ACTIVITY_SAMPLES));

        // Calculate remaining space for command column
        // Fixed columns: name(16) + status(8) + pid(8) + uptime(8) + idle(8) + exit(6) + activity(11) + spaces(7) = 72
        let fixed_width = 72;
        let cmd_width = if (term_cols as usize) > fixed_width + 4 {
            (term_cols as usize) - fixed_width
        } else {
            20
        };
        let cmd = truncate(&s.command, cmd_width);

        let row = format!(
            "{:<16} {:<8} {:>8} {:>8} {:>8} {:>6} {:<11} {}",
            truncate(&s.name, 16),
            status,
            s.pid,
            uptime,
            idle,
            exit_str,
            sparkline,
            cmd
        );
        lines.push(row);
    }

    lines
}

/// Run the live TUI dashboard.
pub fn do_top() -> anyhow::Result<()> {
    ensure_daemon_running()?;

    let mut stdout = io::stdout();

    // Enter alternate screen, enable raw mode
    execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;
    terminal::enable_raw_mode()?;

    let result = top_loop(&mut stdout);

    // Restore terminal
    terminal::disable_raw_mode()?;
    execute!(stdout, cursor::Show, terminal::LeaveAlternateScreen)?;

    result
}

fn top_loop(stdout: &mut io::Stdout) -> anyhow::Result<()> {
    let mut trackers: HashMap<String, ActivityTracker> = HashMap::new();

    loop {
        // Poll sessions from daemon
        let sessions = match fetch_sessions() {
            Ok(s) => s,
            Err(_) => Vec::new(), // Show empty if daemon unreachable
        };

        // Update activity trackers for each session
        for s in &sessions {
            trackers
                .entry(s.name.clone())
                .and_modify(|t| t.record(s.output_bytes))
                .or_insert_with(|| ActivityTracker::new(s.output_bytes));
        }

        // Remove trackers for sessions that no longer exist
        trackers.retain(|name, _| sessions.iter().any(|s| &s.name == name));

        let mut sorted = sessions;
        sort_sessions(&mut sorted);

        // Get terminal size
        let (cols, rows) = terminal::size().unwrap_or((80, 24));

        // Clear screen and render
        execute!(
            stdout,
            terminal::Clear(ClearType::All),
            cursor::MoveTo(0, 0)
        )?;

        // Title
        execute!(
            stdout,
            SetAttribute(Attribute::Bold),
            SetForegroundColor(Color::Cyan)
        )?;
        write!(stdout, "amux top")?;
        execute!(stdout, ResetColor, SetAttribute(Attribute::Reset))?;
        write!(stdout, "\r\n\r\n")?;

        // Header row
        let frame = render_frame(&sorted, cols, &trackers);
        if let Some(header) = frame.first() {
            execute!(stdout, SetAttribute(Attribute::Bold))?;
            write!(stdout, "{}\r\n", header)?;
            execute!(stdout, SetAttribute(Attribute::Reset))?;
        }

        // Data rows
        for (i, line) in frame.iter().skip(1).enumerate() {
            let session = &sorted[i];
            if session.alive {
                execute!(stdout, SetForegroundColor(Color::Green))?;
            } else {
                execute!(stdout, SetForegroundColor(Color::DarkRed))?;
            }
            write!(stdout, "{}\r\n", line)?;
            execute!(stdout, ResetColor)?;
        }

        // Summary line at bottom
        let summary = summary_line(&sorted);
        let summary_row = rows.saturating_sub(2);
        execute!(stdout, cursor::MoveTo(0, summary_row))?;
        execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
        write!(stdout, "{}", summary)?;
        execute!(stdout, ResetColor)?;

        // Help line
        execute!(stdout, cursor::MoveTo(0, rows.saturating_sub(1)))?;
        execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
        write!(stdout, "Press q or Ctrl+C to exit")?;
        execute!(stdout, ResetColor)?;

        stdout.flush()?;

        // Wait for input or timeout (poll every 2 seconds)
        if event::poll(Duration::from_secs(2))? {
            if let Event::Key(KeyEvent { code, modifiers, .. }) = event::read()? {
                match code {
                    KeyCode::Char('q') | KeyCode::Char('Q') => break,
                    KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => break,
                    _ => {}
                }
            }
        }
    }

    Ok(())
}

fn fetch_sessions() -> anyhow::Result<Vec<SessionInfo>> {
    let resp = client::request(&ClientMessage::ListSessions)?;
    match resp {
        DaemonMessage::SessionList(sessions) => Ok(sessions),
        DaemonMessage::Error(e) => anyhow::bail!(e),
        _ => anyhow::bail!("unexpected response"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::messages::SessionInfo;

    fn make_session(name: &str, alive: bool, uptime: u64, idle: u64, exit_code: Option<i32>) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            command: "bash".to_string(),
            pid: 1234,
            alive,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            uptime_secs: uptime,
            last_activity: "2026-01-01T00:00:00Z".to_string(),
            idle_secs: idle,
            exit_code,
            output_bytes: 0,
        }
    }

    #[test]
    fn test_format_duration_seconds() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(45), "45s");
        assert_eq!(format_duration(59), "59s");
    }

    #[test]
    fn test_format_duration_minutes() {
        assert_eq!(format_duration(60), "1m00s");
        assert_eq!(format_duration(332), "5m32s");
        assert_eq!(format_duration(3599), "59m59s");
    }

    #[test]
    fn test_format_duration_hours() {
        assert_eq!(format_duration(3600), "1h00m");
        assert_eq!(format_duration(8100), "2h15m");
        assert_eq!(format_duration(86399), "23h59m");
    }

    #[test]
    fn test_format_duration_days() {
        assert_eq!(format_duration(86400), "1d00h");
        assert_eq!(format_duration(90000), "1d01h");
    }

    #[test]
    fn test_sort_sessions_alive_first() {
        let mut sessions = vec![
            make_session("dead-b", false, 100, 50, Some(1)),
            make_session("alive-a", true, 200, 10, None),
            make_session("dead-a", false, 300, 100, Some(0)),
            make_session("alive-b", true, 50, 5, None),
        ];
        sort_sessions(&mut sessions);
        assert_eq!(sessions[0].name, "alive-a");
        assert_eq!(sessions[1].name, "alive-b");
        assert_eq!(sessions[2].name, "dead-a");
        assert_eq!(sessions[3].name, "dead-b");
    }

    #[test]
    fn test_sort_sessions_empty() {
        let mut sessions: Vec<SessionInfo> = vec![];
        sort_sessions(&mut sessions);
        assert!(sessions.is_empty());
    }

    #[test]
    fn test_truncate_short() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_exact() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn test_truncate_long() {
        assert_eq!(truncate("hello world foo", 10), "hello w...");
    }

    #[test]
    fn test_truncate_very_short_max() {
        assert_eq!(truncate("hello", 3), "hel");
    }

    #[test]
    fn test_summary_line_all_alive() {
        let sessions = vec![
            make_session("a", true, 10, 1, None),
            make_session("b", true, 20, 2, None),
        ];
        assert_eq!(summary_line(&sessions), "2 sessions (2 alive, 0 dead)");
    }

    #[test]
    fn test_summary_line_mixed() {
        let sessions = vec![
            make_session("a", true, 10, 1, None),
            make_session("b", false, 20, 2, Some(0)),
            make_session("c", true, 30, 3, None),
        ];
        assert_eq!(summary_line(&sessions), "3 sessions (2 alive, 1 dead)");
    }

    #[test]
    fn test_summary_line_empty() {
        let sessions: Vec<SessionInfo> = vec![];
        assert_eq!(summary_line(&sessions), "0 sessions (0 alive, 0 dead)");
    }

    #[test]
    fn test_render_frame_header() {
        let sessions: Vec<SessionInfo> = vec![];
        let trackers = HashMap::new();
        let frame = render_frame(&sessions, 120, &trackers);
        assert_eq!(frame.len(), 1); // just header
        assert!(frame[0].contains("NAME"));
        assert!(frame[0].contains("STATUS"));
        assert!(frame[0].contains("PID"));
        assert!(frame[0].contains("UPTIME"));
        assert!(frame[0].contains("IDLE"));
        assert!(frame[0].contains("ACTIVITY"));
        assert!(frame[0].contains("COMMAND"));
    }

    #[test]
    fn test_render_frame_rows() {
        let sessions = vec![
            make_session("worker-1", true, 332, 12, None),
            make_session("builder", false, 600, 300, Some(1)),
        ];
        let trackers = HashMap::new();
        let frame = render_frame(&sessions, 120, &trackers);
        assert_eq!(frame.len(), 3); // header + 2 rows
        assert!(frame[1].contains("worker-1"));
        assert!(frame[1].contains("alive"));
        assert!(frame[1].contains("5m32s"));
        assert!(frame[2].contains("builder"));
        assert!(frame[2].contains("dead"));
        assert!(frame[2].contains("1")); // exit code
    }

    #[test]
    fn test_activity_tracker_idle() {
        let tracker = ActivityTracker::new(0);
        let sparkline = tracker.sparkline();
        assert_eq!(sparkline, "▁▁▁▁▁▁▁▁▁▁");
    }

    #[test]
    fn test_activity_tracker_single_burst() {
        let mut tracker = ActivityTracker::new(0);
        tracker.record(1000);
        // After one record: samples = [1000, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        // Oldest starts at index 1, so we get: [0,0,0,0,0,0,0,0,0,1000]
        let sparkline = tracker.sparkline();
        assert_eq!(sparkline, "▁▁▁▁▁▁▁▁▁█");
    }

    #[test]
    fn test_activity_tracker_uniform() {
        let mut tracker = ActivityTracker::new(0);
        for i in 1..=10 {
            tracker.record(i * 100);
        }
        // All deltas are 100, all should map to max (█)
        let sparkline = tracker.sparkline();
        assert_eq!(sparkline, "██████████");
    }

    #[test]
    fn test_activity_tracker_ramp() {
        let mut tracker = ActivityTracker::new(0);
        // Create increasing deltas: 100, 200, 300, ..., 1000
        let mut total = 0u64;
        for i in 1..=10 {
            total += i * 100;
            tracker.record(total);
        }
        let sparkline = tracker.sparkline();
        let chars: Vec<char> = sparkline.chars().collect();
        // Should be monotonically non-decreasing
        for i in 1..chars.len() {
            assert!(chars[i] >= chars[i - 1], "sparkline should be non-decreasing");
        }
        // Last should be highest
        assert_eq!(*chars.last().unwrap(), '█');
    }

    #[test]
    fn test_activity_tracker_wraps_around() {
        let mut tracker = ActivityTracker::new(0);
        // Record 12 samples (wraps around the 10-element buffer)
        for i in 1..=12 {
            tracker.record(i * 50);
        }
        // Should still produce 10-char sparkline
        assert_eq!(tracker.sparkline().chars().count(), 10);
    }

    #[test]
    fn test_sparkline_in_render_frame() {
        let mut sessions = vec![make_session("active", true, 100, 2, None)];
        sessions[0].output_bytes = 5000;
        let mut trackers = HashMap::new();
        let mut t = ActivityTracker::new(0);
        t.record(1000);
        t.record(2000);
        t.record(5000);
        trackers.insert("active".to_string(), t);
        let frame = render_frame(&sessions, 120, &trackers);
        // Row should contain sparkline characters
        assert!(frame[1].contains('▁') || frame[1].contains('█') || frame[1].contains('▃'));
    }

    #[test]
    fn test_cli_top_parses() {
        use clap::Parser;
        let cli = crate::cli::Cli::try_parse_from(["amux", "top"]).unwrap();
        assert!(matches!(cli.command.unwrap(), crate::cli::Command::Top));
    }
}
