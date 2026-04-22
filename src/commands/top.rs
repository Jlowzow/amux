use crate::client;
use crate::protocol::messages::{ClientMessage, DaemonMessage, SessionInfo};
use crate::util::{ensure_daemon_running, truncate};

use std::collections::HashMap;
use std::io::{self, Write};
use std::time::Duration;

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    style::{Attribute, Color, SetAttribute, SetBackgroundColor, SetForegroundColor, ResetColor},
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

/// Render preview lines from vterm-rendered scrollback.
///
/// The daemon returns the virtual-terminal screen (rendered mode), which is
/// already clean UTF-8 with ANSI sequences and control chars consumed by the
/// emulator. We preserve the bytes verbatim so unicode glyphs used by TUI
/// apps (box-drawing, progress bars, spinners) survive — an aggressive
/// ASCII-only sanitize would blank most of the preview.
fn render_preview(raw: &[u8], max_lines: usize, max_cols: usize) -> Vec<String> {
    let text = String::from_utf8_lossy(raw);
    let all_lines: Vec<&str> = text.lines().collect();
    let start = all_lines.len().saturating_sub(max_lines);
    all_lines[start..]
        .iter()
        .map(|l| truncate(l, max_cols))
        .collect()
}

/// Clamp a selection index to be within [0, count-1], or 0 if count is 0.
fn clamp_selection(selected: usize, count: usize) -> usize {
    if count == 0 {
        0
    } else if selected >= count {
        count - 1
    } else {
        selected
    }
}

/// Action returned from key handling to drive attach/follow.
enum TopAction {
    Continue,
    Quit,
    Attach(String),
    Follow(String),
}

/// Render a plain-text snapshot of the session table (no TUI, no ANSI).
fn render_snapshot(sessions: &[SessionInfo], term_cols: u16, trackers: &HashMap<String, ActivityTracker>) -> String {
    let frame = render_frame(sessions, term_cols, trackers);
    let summary = summary_line(sessions);
    let mut lines = frame;
    lines.push(summary);
    lines.join("\n")
}

/// Print a single snapshot of the dashboard to stdout and exit.
pub fn do_top_once() -> anyhow::Result<()> {
    ensure_daemon_running()?;

    let mut sessions = fetch_sessions()?;
    sort_sessions(&mut sessions);

    // Build trackers from current state (no history, so sparklines will be flat)
    let mut trackers: HashMap<String, ActivityTracker> = HashMap::new();
    for s in &sessions {
        trackers.insert(s.name.clone(), ActivityTracker::new(s.output_bytes));
    }

    // Use 80 columns as default when not in a terminal
    let cols = terminal::size().map(|(c, _)| c).unwrap_or(80);

    let output = render_snapshot(&sessions, cols, &trackers);
    println!("{}", output);
    Ok(())
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
    let mut selected: usize = 0;

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

        // Clamp selection
        selected = clamp_selection(selected, sorted.len());

        // Fetch scrollback for the selected session
        let preview_raw = if !sorted.is_empty() {
            fetch_scrollback(&sorted[selected].name, 30).unwrap_or_default()
        } else {
            Vec::new()
        };

        // Get terminal size
        let (cols, rows) = terminal::size().unwrap_or((80, 24));

        // Layout: title(1) + blank(1) + header(1) + session_rows + blank(1) + preview_header(1) + preview_lines + summary(1) + help(1)
        // We allocate roughly half the terminal for table, half for preview
        let table_rows = sorted.len() as u16;
        // Preview pane gets remaining space between table area and bottom status lines
        // Top area: title(1) + blank(1) + header(1) + table_rows + blank(1) = 4 + table_rows
        let top_area = 4 + table_rows;
        // Bottom area: summary(1) + help(1) = 2
        let bottom_area: u16 = 2;
        // Preview area: preview_header(1) + separator(1) + preview_lines
        let preview_total = rows.saturating_sub(top_area + bottom_area);
        let preview_lines_count = preview_total.saturating_sub(2) as usize; // subtract header + separator
        let preview_lines_count = if preview_lines_count > 15 { 15 } else { preview_lines_count };

        let preview = render_preview(&preview_raw, preview_lines_count.max(1), cols as usize);

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

        // Data rows with selection highlight
        for (i, line) in frame.iter().skip(1).enumerate() {
            let session = &sorted[i];
            if i == selected {
                // Highlighted row: reverse video
                execute!(
                    stdout,
                    SetBackgroundColor(Color::DarkGrey),
                    SetForegroundColor(Color::White),
                    SetAttribute(Attribute::Bold)
                )?;
            } else if session.alive {
                execute!(stdout, SetForegroundColor(Color::Green))?;
            } else {
                execute!(stdout, SetForegroundColor(Color::DarkRed))?;
            }
            // Pad line to full width to fill the highlight background
            let padded = if i == selected {
                format!("{:<width$}", line, width = cols as usize)
            } else {
                line.to_string()
            };
            write!(stdout, "{}\r\n", padded)?;
            execute!(stdout, ResetColor, SetAttribute(Attribute::Reset))?;
        }

        // Preview pane
        if preview_lines_count > 0 {
            write!(stdout, "\r\n")?;

            // Separator line
            let separator: String = "─".repeat(cols as usize);
            execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
            write!(stdout, "{}\r\n", separator)?;
            execute!(stdout, ResetColor)?;

            // Preview header
            let preview_title = if !sorted.is_empty() {
                format!(" Preview: {} ", sorted[selected].name)
            } else {
                " Preview: (no sessions) ".to_string()
            };
            execute!(
                stdout,
                SetAttribute(Attribute::Bold),
                SetForegroundColor(Color::Yellow)
            )?;
            write!(stdout, "{}\r\n", preview_title)?;
            execute!(stdout, ResetColor, SetAttribute(Attribute::Reset))?;

            // Preview content
            if preview.is_empty() || (preview.len() == 1 && preview[0].is_empty()) {
                execute!(stdout, SetForegroundColor(Color::DarkGrey))?;
                write!(stdout, "  (no output)\r\n")?;
                execute!(stdout, ResetColor)?;
            } else {
                for line in &preview {
                    write!(stdout, "  {}\r\n", line)?;
                }
            }
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
        write!(stdout, "j/k:select  Enter:attach  f:follow  q:quit")?;
        execute!(stdout, ResetColor)?;

        stdout.flush()?;

        // Wait for input or timeout (poll every 2 seconds)
        if event::poll(Duration::from_secs(2))? {
            if let Event::Key(KeyEvent { code, modifiers, .. }) = event::read()? {
                let action = handle_key(code, modifiers, &sorted, &mut selected);
                match action {
                    TopAction::Quit => break,
                    TopAction::Attach(name) => {
                        // Restore terminal, attach, then re-enter alternate screen
                        terminal::disable_raw_mode()?;
                        execute!(stdout, cursor::Show, terminal::LeaveAlternateScreen)?;
                        // Use the attach command
                        let _ = super::attach::do_attach(&name);
                        execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;
                        terminal::enable_raw_mode()?;
                    }
                    TopAction::Follow(name) => {
                        terminal::disable_raw_mode()?;
                        execute!(stdout, cursor::Show, terminal::LeaveAlternateScreen)?;
                        let _ = super::attach::do_follow(&name, false);
                        execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)?;
                        terminal::enable_raw_mode()?;
                    }
                    TopAction::Continue => {}
                }
            }
        }
    }

    Ok(())
}

/// Handle a key event and return the action to take.
fn handle_key(
    code: KeyCode,
    modifiers: KeyModifiers,
    sessions: &[SessionInfo],
    selected: &mut usize,
) -> TopAction {
    match code {
        KeyCode::Char('q') | KeyCode::Char('Q') => TopAction::Quit,
        KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => TopAction::Quit,
        KeyCode::Char('j') | KeyCode::Down => {
            if !sessions.is_empty() {
                *selected = (*selected + 1).min(sessions.len() - 1);
            }
            TopAction::Continue
        }
        KeyCode::Char('k') | KeyCode::Up => {
            *selected = selected.saturating_sub(1);
            TopAction::Continue
        }
        KeyCode::Enter => {
            if !sessions.is_empty() {
                TopAction::Attach(sessions[*selected].name.clone())
            } else {
                TopAction::Continue
            }
        }
        KeyCode::Char('f') => {
            if !sessions.is_empty() {
                TopAction::Follow(sessions[*selected].name.clone())
            } else {
                TopAction::Continue
            }
        }
        _ => TopAction::Continue,
    }
}

fn fetch_sessions() -> anyhow::Result<Vec<SessionInfo>> {
    let resp = client::request(&ClientMessage::ListSessions)?;
    match resp {
        DaemonMessage::SessionList(sessions) => Ok(sessions),
        DaemonMessage::Error(e) => anyhow::bail!(e),
        _ => anyhow::bail!("unexpected response"),
    }
}

fn fetch_scrollback(name: &str, lines: usize) -> anyhow::Result<Vec<u8>> {
    // Ask for the rendered virtual-terminal screen so TUI apps (which draw
    // with cursor-addressed escape sequences) preview correctly. The raw
    // byte stream would yield garbled fragments after naive ANSI stripping.
    let resp = client::request(&ClientMessage::CaptureScrollback {
        name: name.to_string(),
        lines,
        raw: false,
    })?;
    match resp {
        DaemonMessage::CaptureOutput(data) => Ok(data),
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
        assert_eq!(truncate("hello world foo", 10), "hello wor…");
    }

    #[test]
    fn test_truncate_very_short_max() {
        assert_eq!(truncate("hello", 3), "he…");
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
        match cli.command.unwrap() {
            crate::cli::Command::Top { once } => assert!(!once),
            other => panic!("expected Top, got {:?}", other),
        }
    }

    #[test]
    fn test_cli_top_once_parses() {
        use clap::Parser;
        let cli = crate::cli::Cli::try_parse_from(["amux", "top", "--once"]).unwrap();
        match cli.command.unwrap() {
            crate::cli::Command::Top { once } => assert!(once),
            other => panic!("expected Top, got {:?}", other),
        }
    }

    #[test]
    fn test_render_snapshot_empty() {
        let sessions: Vec<SessionInfo> = vec![];
        let trackers = HashMap::new();
        let output = render_snapshot(&sessions, 80, &trackers);
        assert!(output.contains("NAME"));
        assert!(output.contains("0 sessions (0 alive, 0 dead)"));
    }

    #[test]
    fn test_render_snapshot_with_sessions() {
        let sessions = vec![
            make_session("worker-1", true, 332, 12, None),
            make_session("builder", false, 600, 300, Some(1)),
        ];
        let trackers = HashMap::new();
        let output = render_snapshot(&sessions, 120, &trackers);
        assert!(output.contains("worker-1"));
        assert!(output.contains("alive"));
        assert!(output.contains("builder"));
        assert!(output.contains("dead"));
        assert!(output.contains("2 sessions (1 alive, 1 dead)"));
    }

    // --- New tests for preview pane and selection ---

    #[test]
    fn test_clamp_selection_within_bounds() {
        assert_eq!(clamp_selection(0, 5), 0);
        assert_eq!(clamp_selection(2, 5), 2);
        assert_eq!(clamp_selection(4, 5), 4);
    }

    #[test]
    fn test_clamp_selection_overflow() {
        assert_eq!(clamp_selection(10, 5), 4);
        assert_eq!(clamp_selection(100, 1), 0);
    }

    #[test]
    fn test_clamp_selection_empty() {
        assert_eq!(clamp_selection(0, 0), 0);
        assert_eq!(clamp_selection(5, 0), 0);
    }

    #[test]
    fn test_render_preview_plain_text() {
        let raw = b"line one\nline two\nline three\n";
        let result = render_preview(raw, 10, 80);
        assert_eq!(result, vec!["line one", "line two", "line three"]);
    }

    #[test]
    fn test_render_preview_limits_lines() {
        let raw = b"a\nb\nc\nd\ne\n";
        let result = render_preview(raw, 3, 80);
        assert_eq!(result, vec!["c", "d", "e"]);
    }

    #[test]
    fn test_render_preview_truncates_wide_lines() {
        let raw = b"this is a very long line that should be truncated\n";
        let result = render_preview(raw, 10, 20);
        assert_eq!(result, vec!["this is a very long\u{2026}"]);
    }

    #[test]
    fn test_render_preview_empty() {
        let raw = b"";
        let result = render_preview(raw, 10, 80);
        assert!(result.is_empty());
    }

    #[test]
    fn test_render_preview_preserves_unicode() {
        // vterm-rendered output for TUIs contains unicode box-drawing,
        // progress bars, and glyphs. Preview must preserve them verbatim.
        let raw = "│ Opus 4.7 │\n────────\n█░░░░ 10%\n".as_bytes();
        let result = render_preview(raw, 10, 80);
        assert_eq!(
            result,
            vec!["│ Opus 4.7 │", "────────", "█░░░░ 10%"]
        );
    }

    #[test]
    fn test_handle_key_quit_q() {
        let sessions = vec![make_session("a", true, 10, 1, None)];
        let mut sel = 0;
        assert!(matches!(
            handle_key(KeyCode::Char('q'), KeyModifiers::NONE, &sessions, &mut sel),
            TopAction::Quit
        ));
    }

    #[test]
    fn test_handle_key_quit_ctrl_c() {
        let sessions = vec![make_session("a", true, 10, 1, None)];
        let mut sel = 0;
        assert!(matches!(
            handle_key(KeyCode::Char('c'), KeyModifiers::CONTROL, &sessions, &mut sel),
            TopAction::Quit
        ));
    }

    #[test]
    fn test_handle_key_move_down_j() {
        let sessions = vec![
            make_session("a", true, 10, 1, None),
            make_session("b", true, 20, 2, None),
            make_session("c", true, 30, 3, None),
        ];
        let mut sel = 0;
        handle_key(KeyCode::Char('j'), KeyModifiers::NONE, &sessions, &mut sel);
        assert_eq!(sel, 1);
        handle_key(KeyCode::Char('j'), KeyModifiers::NONE, &sessions, &mut sel);
        assert_eq!(sel, 2);
        // Should not go past end
        handle_key(KeyCode::Char('j'), KeyModifiers::NONE, &sessions, &mut sel);
        assert_eq!(sel, 2);
    }

    #[test]
    fn test_handle_key_move_up_k() {
        let sessions = vec![
            make_session("a", true, 10, 1, None),
            make_session("b", true, 20, 2, None),
        ];
        let mut sel = 1;
        handle_key(KeyCode::Char('k'), KeyModifiers::NONE, &sessions, &mut sel);
        assert_eq!(sel, 0);
        // Should not go below 0
        handle_key(KeyCode::Char('k'), KeyModifiers::NONE, &sessions, &mut sel);
        assert_eq!(sel, 0);
    }

    #[test]
    fn test_handle_key_arrow_keys() {
        let sessions = vec![
            make_session("a", true, 10, 1, None),
            make_session("b", true, 20, 2, None),
        ];
        let mut sel = 0;
        handle_key(KeyCode::Down, KeyModifiers::NONE, &sessions, &mut sel);
        assert_eq!(sel, 1);
        handle_key(KeyCode::Up, KeyModifiers::NONE, &sessions, &mut sel);
        assert_eq!(sel, 0);
    }

    #[test]
    fn test_handle_key_enter_attach() {
        let sessions = vec![
            make_session("worker-1", true, 10, 1, None),
            make_session("worker-2", true, 20, 2, None),
        ];
        let mut sel = 1;
        match handle_key(KeyCode::Enter, KeyModifiers::NONE, &sessions, &mut sel) {
            TopAction::Attach(name) => assert_eq!(name, "worker-2"),
            _ => panic!("expected Attach action"),
        }
    }

    #[test]
    fn test_handle_key_f_follow() {
        let sessions = vec![
            make_session("worker-1", true, 10, 1, None),
        ];
        let mut sel = 0;
        match handle_key(KeyCode::Char('f'), KeyModifiers::NONE, &sessions, &mut sel) {
            TopAction::Follow(name) => assert_eq!(name, "worker-1"),
            _ => panic!("expected Follow action"),
        }
    }

    #[test]
    fn test_handle_key_enter_empty_sessions() {
        let sessions: Vec<SessionInfo> = vec![];
        let mut sel = 0;
        assert!(matches!(
            handle_key(KeyCode::Enter, KeyModifiers::NONE, &sessions, &mut sel),
            TopAction::Continue
        ));
    }

    #[test]
    fn test_handle_key_f_empty_sessions() {
        let sessions: Vec<SessionInfo> = vec![];
        let mut sel = 0;
        assert!(matches!(
            handle_key(KeyCode::Char('f'), KeyModifiers::NONE, &sessions, &mut sel),
            TopAction::Continue
        ));
    }

    #[test]
    fn test_handle_key_move_on_empty() {
        let sessions: Vec<SessionInfo> = vec![];
        let mut sel = 0;
        handle_key(KeyCode::Char('j'), KeyModifiers::NONE, &sessions, &mut sel);
        assert_eq!(sel, 0);
    }
}
