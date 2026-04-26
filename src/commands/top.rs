use crate::client;
use crate::common::resolved_instance;
use crate::protocol::messages::{CaptureMode, ClientMessage, DaemonMessage, SessionInfo};
use crate::util::{ensure_daemon_running, truncate, truncate_preserving_ansi};

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

/// Format the title-row instance label.
///
/// Default instance (`None` or `Some("")`) returns an empty string so the
/// title is unchanged. A named instance returns `"  [instance: <name>]"` so
/// it can be appended to the existing title text on the same row.
fn instance_suffix(instance: Option<&str>) -> String {
    match instance {
        Some(name) if !name.is_empty() => format!("  [instance: {}]", name),
        _ => String::new(),
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

/// Render preview lines from vterm-rendered scrollback.
///
/// The daemon returns the virtual-terminal screen with SGR color codes
/// preserved but cursor positioning stripped — bytes can be written to any
/// column on the terminal and will appear correctly colored. Truncation is
/// ANSI-aware so escape sequences are kept intact while the visible width
/// is clamped to `max_cols`. A trailing SGR reset is appended to each line
/// so a colored background does not bleed past the preview pane.
fn render_preview(raw: &[u8], max_lines: usize, max_cols: usize) -> Vec<String> {
    let text = String::from_utf8_lossy(raw);
    let all_lines: Vec<&str> = text.lines().collect();
    let start = all_lines.len().saturating_sub(max_lines);
    all_lines[start..]
        .iter()
        .map(|l| {
            let mut truncated = truncate_preserving_ansi(l, max_cols);
            // Ensure each preview line ends clean so selection/summary rows
            // below aren't rendered in a left-over fg/bg.
            truncated.push_str("\x1b[0m");
            truncated
        })
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

/// Absolute row positions for each section of the top TUI.
///
/// All rendering uses `MoveTo(0, row)` — we never rely on sequential writes
/// advancing the cursor into the right place. This guarantees no section can
/// overwrite another regardless of preview length or content filtering.
#[derive(Debug, Clone, PartialEq, Eq)]
struct TopLayout {
    title_row: u16,
    header_row: u16,
    data_row_start: u16,
    data_row_count: u16,
    separator_row: Option<u16>,
    preview_header_row: Option<u16>,
    preview_row_start: Option<u16>,
    preview_row_count: u16,
    summary_row: u16,
    help_row: u16,
}

// No artificial cap — preview uses all available terminal rows.

/// Compute non-overlapping absolute row positions for every section.
///
/// Layout shape (top to bottom):
///   title(1) + blank(1) + header(1) + N data rows + blank(1) + separator(1)
///   + preview_header(1) + preview_content(K) + ... + summary(1) + help(1)
///
/// Data rows and preview content are bounded so that their ranges stay
/// strictly above `summary_row = rows - 2`. If the terminal is too short to
/// fit a preview section, the preview fields are `None`.
fn compute_layout(session_count: u16, rows: u16) -> TopLayout {
    let title_row: u16 = 0;
    let header_row: u16 = 1;
    let data_row_start: u16 = 2;
    let summary_row = rows.saturating_sub(1);
    let help_row = rows.saturating_sub(1); // combined with summary

    // Reserve 3 rows at the bottom of the middle area for a minimal preview
    // section: separator + header + at least one content line.
    // Whatever's left after the reservation is available for the table.
    let middle_budget = summary_row.saturating_sub(data_row_start);
    let preview_reservation: u16 = 3;
    let max_data = middle_budget.saturating_sub(preview_reservation);
    let data_row_count = session_count.min(max_data);
    let data_row_end = data_row_start.saturating_add(data_row_count);

    // Preview starts immediately after the table.
    let separator_row = data_row_end;
    let preview_header_row = separator_row.saturating_add(1);
    let preview_row_start = preview_header_row.saturating_add(1);

    let preview_row_count = summary_row
        .saturating_sub(preview_row_start);

    let has_preview = preview_row_count > 0 && preview_row_start < summary_row;

    TopLayout {
        title_row,
        header_row,
        data_row_start,
        data_row_count,
        separator_row: if has_preview { Some(separator_row) } else { None },
        preview_header_row: if has_preview { Some(preview_header_row) } else { None },
        preview_row_start: if has_preview { Some(preview_row_start) } else { None },
        preview_row_count,
        summary_row,
        help_row,
    }
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

    let resolved = resolved_instance();
    let suffix = instance_suffix(resolved.as_deref());
    if !suffix.is_empty() {
        println!("amux top{}", suffix);
    }
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

        // Get terminal size first so we know how much preview the layout has.
        let (cols, rows) = terminal::size().unwrap_or((80, 24));
        let layout = compute_layout(sorted.len() as u16, rows);

        // Ask the daemon for enough lines to fill the preview pane. The
        // daemon replays raw scrollback through a tall vt100 parser to
        // recover history past the agent's PTY size (bd-pmk). A small
        // margin guards against trailing-blank filtering shrinking the
        // result below the available rows.
        let requested_lines = (layout.preview_row_count as usize).saturating_add(8).max(30);
        let preview_raw = if !sorted.is_empty() {
            fetch_scrollback(&sorted[selected].name, requested_lines).unwrap_or_default()
        } else {
            Vec::new()
        };

        let preview = render_preview(
            &preview_raw,
            layout.preview_row_count.max(1) as usize,
            cols as usize,
        );

        // Clear the full screen once, then render every section at an
        // absolute row. No section relies on sequential cursor movement, so
        // short preview content or filtered blank lines can never cause one
        // section to spill into another's rows.
        execute!(stdout, terminal::Clear(ClearType::All))?;

        // Title (with optional [instance: <name>] suffix from resolved_instance)
        let resolved = resolved_instance();
        let suffix = instance_suffix(resolved.as_deref());
        execute!(
            stdout,
            cursor::MoveTo(0, layout.title_row),
            SetAttribute(Attribute::Bold),
            SetForegroundColor(Color::Cyan)
        )?;
        write!(stdout, "amux top{}", suffix)?;
        execute!(stdout, ResetColor, SetAttribute(Attribute::Reset))?;

        // Header row
        let frame = render_frame(&sorted, cols, &trackers);
        if let Some(header) = frame.first() {
            execute!(
                stdout,
                cursor::MoveTo(0, layout.header_row),
                SetAttribute(Attribute::Bold)
            )?;
            write!(stdout, "{}", header)?;
            execute!(stdout, SetAttribute(Attribute::Reset))?;
        }

        // Data rows with selection highlight (bounded by layout.data_row_count)
        for i in 0..layout.data_row_count as usize {
            let line = match frame.get(i + 1) {
                Some(l) => l,
                None => break,
            };
            let session = &sorted[i];
            execute!(stdout, cursor::MoveTo(0, layout.data_row_start + i as u16))?;
            if i == selected {
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
            let padded = if i == selected {
                format!("{:<width$}", line, width = cols as usize)
            } else {
                line.to_string()
            };
            write!(stdout, "{}", padded)?;
            execute!(stdout, ResetColor, SetAttribute(Attribute::Reset))?;
        }

        // Preview pane
        if let (Some(sep_row), Some(ph_row), Some(pr_start)) = (
            layout.separator_row,
            layout.preview_header_row,
            layout.preview_row_start,
        ) {
            // Separator
            let separator: String = "─".repeat(cols as usize);
            execute!(
                stdout,
                cursor::MoveTo(0, sep_row),
                SetForegroundColor(Color::DarkGrey)
            )?;
            write!(stdout, "{}", separator)?;
            execute!(stdout, ResetColor)?;

            // Preview header
            let preview_title = if !sorted.is_empty() {
                format!(" Preview: {} ", sorted[selected].name)
            } else {
                " Preview: (no sessions) ".to_string()
            };
            execute!(
                stdout,
                cursor::MoveTo(0, ph_row),
                SetAttribute(Attribute::Bold),
                SetForegroundColor(Color::Yellow)
            )?;
            write!(stdout, "{}", preview_title)?;
            execute!(stdout, ResetColor, SetAttribute(Attribute::Reset))?;

            // Preview content
            if preview.is_empty() || (preview.len() == 1 && preview[0].is_empty()) {
                execute!(
                    stdout,
                    cursor::MoveTo(0, pr_start),
                    SetForegroundColor(Color::DarkGrey)
                )?;
                write!(stdout, "  (no output)")?;
                execute!(stdout, ResetColor)?;
            } else {
                for (i, line) in preview.iter().enumerate() {
                    if i >= layout.preview_row_count as usize {
                        break;
                    }
                    execute!(stdout, cursor::MoveTo(0, pr_start + i as u16))?;
                    write!(stdout, "  {}", line)?;
                }
            }
        }

        // Status bar (summary + help combined on one line)
        let summary = summary_line(&sorted);
        execute!(
            stdout,
            cursor::MoveTo(0, layout.summary_row),
            SetForegroundColor(Color::DarkGrey)
        )?;
        write!(stdout, "{}  │  j/k:select  Enter:attach  f:follow  q:quit", summary)?;
        execute!(stdout, ResetColor)?;

        stdout.flush()?;

        // Wait for input or timeout. Poll at 100ms so key presses feel
        // responsive and the session/activity data refreshes at ~10Hz.
        if event::poll(Duration::from_millis(100))? {
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
    // Formatted mode: rendered screen with SGR color codes preserved, cursor
    // positioning stripped. This lets the preview show the target's colors
    // (e.g. claude's UI) rather than monochrome text.
    let resp = client::request(&ClientMessage::CaptureScrollback {
        name: name.to_string(),
        lines,
        mode: CaptureMode::Formatted,
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

    /// Every preview line ends with an SGR reset so a colored background
    /// from the previous line cannot leak into the surrounding pane.
    const RESET: &str = "\x1b[0m";

    #[test]
    fn test_render_preview_plain_text() {
        let raw = b"line one\nline two\nline three\n";
        let result = render_preview(raw, 10, 80);
        assert_eq!(
            result,
            vec![
                format!("line one{}", RESET),
                format!("line two{}", RESET),
                format!("line three{}", RESET),
            ]
        );
    }

    #[test]
    fn test_render_preview_limits_lines() {
        let raw = b"a\nb\nc\nd\ne\n";
        let result = render_preview(raw, 3, 80);
        assert_eq!(
            result,
            vec![format!("c{}", RESET), format!("d{}", RESET), format!("e{}", RESET)]
        );
    }

    #[test]
    fn test_render_preview_truncates_wide_lines() {
        let raw = b"this is a very long line that should be truncated\n";
        let result = render_preview(raw, 10, 20);
        assert_eq!(result, vec![format!("this is a very long\u{2026}{}", RESET)]);
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
            vec![
                format!("│ Opus 4.7 │{}", RESET),
                format!("────────{}", RESET),
                format!("█░░░░ 10%{}", RESET),
            ]
        );
    }

    #[test]
    fn test_render_preview_preserves_sgr_colors() {
        // Input with SGR color codes should have them preserved in the
        // rendered preview output — that's the whole point.
        let raw = b"\x1b[31mred text\x1b[0m normal\n";
        let result = render_preview(raw, 10, 80);
        assert_eq!(result.len(), 1);
        let line = &result[0];
        assert!(line.contains("\x1b[31m"), "missing red SGR: {:?}", line);
        assert!(line.contains("red text"), "missing text: {:?}", line);
        assert!(line.ends_with(RESET), "must end with reset: {:?}", line);
    }

    #[test]
    fn test_render_preview_truncates_past_sgr_codes() {
        // SGR codes don't consume visible width; truncation must still
        // clamp the visible chars and keep the escapes intact.
        let raw = b"\x1b[31mabcdefghij\x1b[0m\n";
        let result = render_preview(raw, 10, 5);
        assert_eq!(result.len(), 1);
        let line = &result[0];
        // 4 visible chars + ellipsis = 5 visible chars.
        assert!(line.contains("\x1b[31m"));
        assert!(line.contains("abcd"));
        assert!(line.contains('…'));
        assert!(!line.contains("efgh"), "must not contain dropped content: {:?}", line);
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

    // --- Layout tests: guard the fix for bd-9ep (preview overwritten by summary) ---

    /// Walk every row position the layout reports and check that no two
    /// ranges overlap and nothing escapes the terminal.
    fn assert_layout_consistent(layout: &TopLayout, rows: u16) {
        assert!(layout.title_row < rows, "title_row out of bounds");
        assert!(layout.header_row < rows, "header_row out of bounds");
        assert!(layout.summary_row < rows, "summary_row out of bounds");
        assert!(layout.help_row < rows, "help_row out of bounds");

        // Summary and help are on the same row (combined status bar)
        assert_eq!(
            layout.summary_row, layout.help_row,
            "summary and help should share the same row"
        );

        // Data rows sit between header and summary
        let data_end = layout.data_row_start + layout.data_row_count; // exclusive
        assert!(
            layout.data_row_start > layout.header_row,
            "data must be below header"
        );
        assert!(
            data_end <= layout.summary_row,
            "data rows overflow into summary (data_end={}, summary_row={})",
            data_end, layout.summary_row
        );

        // Preview (if present) sits entirely between data rows and summary
        if let (Some(sep), Some(ph), Some(pr)) = (
            layout.separator_row,
            layout.preview_header_row,
            layout.preview_row_start,
        ) {
            assert!(sep >= data_end, "separator overlaps data rows");
            assert_eq!(ph, sep + 1, "preview header must follow separator");
            assert_eq!(pr, ph + 1, "preview content must follow its header");
            let preview_end = pr + layout.preview_row_count; // exclusive
            assert!(
                preview_end <= layout.summary_row,
                "preview content overflows into summary (preview_end={}, summary_row={})",
                preview_end,
                layout.summary_row
            );
        }
    }

    #[test]
    fn test_layout_no_overlap_across_sizes() {
        // Sweep plausible terminal sizes and session counts; every layout
        // must be self-consistent (no section overlaps another).
        for &rows in &[12u16, 18, 20, 24, 30, 40, 60, 100] {
            for &n in &[0u16, 1, 2, 3, 5, 8, 12, 20, 50] {
                let layout = compute_layout(n, rows);
                assert_layout_consistent(&layout, rows);
            }
        }
    }

    #[test]
    fn test_layout_reserves_status_bar() {
        let layout = compute_layout(3, 24);
        assert_eq!(layout.summary_row, 23);
        assert_eq!(layout.help_row, 23);
    }

    #[test]
    fn test_layout_preview_never_overruns_summary_standard_terminal() {
        // This is the exact shape the bug manifested on: default 80x24 PTY
        // with a small session table. The old layout math let the preview
        // end on the same row the summary's MoveTo later clobbered.
        let layout = compute_layout(1, 24);
        let pr = layout.preview_row_start.expect("preview should fit");
        let end = pr + layout.preview_row_count;
        assert!(
            end <= layout.summary_row,
            "preview ends at row {} but summary is at {}",
            end, layout.summary_row
        );
    }

    #[test]
    fn test_layout_preview_uses_available_space() {
        // Preview should use all available rows between header and summary.
        let layout = compute_layout(0, 100);
        assert!(layout.preview_row_count > 15, "tall terminal should give more than 15 preview lines, got {}", layout.preview_row_count);
    }

    #[test]
    fn test_layout_drops_preview_on_tiny_terminal() {
        // On a 5-row terminal there's no room for preview after
        // title/header/data/status — layout should report no preview section.
        let layout = compute_layout(2, 5);
        assert_layout_consistent(&layout, 7);
        assert!(
            layout.preview_row_start.is_none() && layout.preview_row_count == 0,
            "expected no preview on tiny terminal, got {:?}",
            layout
        );
    }

    // --- Instance suffix helper (bd-3ri) ---

    #[test]
    fn test_instance_suffix_default_is_empty() {
        assert_eq!(instance_suffix(None), "");
        assert_eq!(instance_suffix(Some("")), "");
    }

    #[test]
    fn test_instance_suffix_named_contains_label() {
        let s = instance_suffix(Some("foo"));
        assert!(s.contains("instance: foo"), "expected label, got {:?}", s);
        // Default invocation must not accidentally produce this string.
        assert!(!instance_suffix(None).contains("instance:"));
    }

    #[test]
    fn test_instance_suffix_appends_cleanly_to_title() {
        // The suffix is meant to sit on the same line as the title without
        // colliding with the existing text.
        let title = format!("amux top{}", instance_suffix(Some("projA")));
        assert!(title.starts_with("amux top"));
        assert!(title.contains("instance: projA"));
    }

    #[test]
    fn test_layout_caps_data_rows_when_sessions_exceed_space() {
        // 50 sessions can't fit on a 24-row terminal; the table must be
        // clipped so that summary/help rows stay reachable.
        let layout = compute_layout(50, 24);
        assert_layout_consistent(&layout, 24);
        let data_end = layout.data_row_start + layout.data_row_count;
        assert!(data_end < layout.summary_row);
    }
}
