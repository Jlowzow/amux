use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Requests from client to daemon.
#[derive(Serialize, Deserialize, Debug)]
pub enum ClientMessage {
    Ping,
    KillServer,
    CreateSession {
        name: Option<String>,
        command: Vec<String>,
        env: Option<HashMap<String, String>>,
        cwd: Option<String>,
        cols: Option<u16>,
        rows: Option<u16>,
    },
    ListSessions,
    /// Get detailed info for a single session.
    GetSessionInfo {
        name: String,
    },
    KillSession {
        name: String,
    },
    KillAllSessions,
    Attach {
        name: String,
        cols: u16,
        rows: u16,
    },
    AttachInput(Vec<u8>),
    AttachResize {
        cols: u16,
        rows: u16,
    },
    Detach,
    SendInput {
        name: String,
        data: Vec<u8>,
        newline: bool,
    },
    HasSession {
        name: String,
    },
    /// Capture session scrollback in one of three modes (see `CaptureMode`).
    /// Plain and Formatted both return the rendered virtual-terminal screen
    /// (correct for TUI apps that use cursor movement to redraw in place);
    /// Formatted additionally preserves SGR color/attribute codes, while
    /// Plain strips them. Raw returns the raw PTY byte stream.
    CaptureScrollback {
        name: String,
        lines: usize,
        mode: CaptureMode,
    },
    SetEnv {
        name: String,
        key: String,
        value: String,
    },
    GetEnv {
        name: String,
        key: String,
    },
    GetAllEnv {
        name: String,
    },
    /// Subscribe to session output without interactive attach (read-only streaming).
    Follow {
        name: String,
    },
    /// Block until a session exits or a timeout elapses.
    WaitSession {
        name: String,
        /// Timeout in seconds (0 = wait forever).
        timeout_secs: u64,
    },
    /// Get the exit code of a (finished) session.
    GetExitCode {
        name: String,
    },
    /// Watch multiple sessions for exit events.
    WatchSessions {
        sessions: Vec<String>,
    },
    /// Block until any of the given sessions exits (or timeout).
    WaitAny {
        sessions: Vec<String>,
        /// Timeout in seconds (0 = wait forever).
        timeout_secs: u64,
    },
    /// Resize a session's PTY without attaching. Used by `amux top` to
    /// match the agent's canvas to its viewer's terminal when no client
    /// is attached. AttachResize is for active attach connections; this
    /// is for stateless one-shot resizes (bd-is4).
    ResizeSession {
        name: String,
        cols: u16,
        rows: u16,
    },
    /// Atomically replace a session's child process with a new command,
    /// preserving the session name, registry slot, attached clients'
    /// output stream, and current PTY size. Equivalent to
    /// `tmux respawn-pane -k`. See bd-wh4.
    RespawnSession {
        name: String,
        command: Vec<String>,
        cwd: Option<String>,
        env: Option<HashMap<String, String>>,
    },
}

/// Responses from daemon to client.
#[derive(Serialize, Deserialize, Debug)]
pub enum DaemonMessage {
    Pong,
    Ok,
    Error(String),
    SessionCreated {
        name: String,
    },
    SessionList(Vec<SessionInfo>),
    /// Detailed info for a single session.
    SessionDetail(SessionInfo),
    /// Output data streamed during attach.
    Output(Vec<u8>),
    /// Session ended while attached.
    SessionEnded,
    /// Whether a session exists.
    SessionExists(bool),
    /// Count of sessions killed in a bulk operation.
    KilledSessions {
        count: usize,
    },
    /// Captured scrollback output.
    CaptureOutput(Vec<u8>),
    /// Acknowledgement that input was sent to a session.
    InputSent,
    /// Value of a single environment variable (None if not set).
    EnvValue(Option<String>),
    /// All environment variables for a session.
    EnvVars(HashMap<String, String>),
    /// Session exited (response to WaitSession).
    SessionExited,
    /// Exit code of a session (None if still running or unknown).
    ExitCode(Option<i32>),
    /// A watched session exited (streamed during WatchSessions).
    WatchSessionExited {
        session: String,
        exit_code: Option<i32>,
    },
    /// All watched sessions have exited.
    WatchDone,
    /// A session exited (response to WaitAny).
    WaitAnyExited {
        session: String,
        exit_code: Option<i32>,
    },
}

/// Scrollback capture mode. See `ClientMessage::CaptureScrollback`.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureMode {
    /// Raw PTY byte stream — ANSI and cursor sequences included verbatim.
    Raw,
    /// Rendered virtual-terminal screen as plain UTF-8 text; no escape codes.
    Plain,
    /// Rendered virtual-terminal screen with SGR (color/attribute) codes
    /// preserved but cursor positioning stripped. Intended for callers that
    /// want colored output but place the cursor themselves.
    Formatted,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SessionInfo {
    pub name: String,
    pub command: String,
    pub pid: u32,
    pub alive: bool,
    /// ISO 8601 timestamp of session creation.
    pub created_at: String,
    /// Seconds since session was created.
    pub uptime_secs: u64,
    /// ISO 8601 timestamp of last PTY output activity.
    pub last_activity: String,
    /// Seconds since last PTY output activity.
    pub idle_secs: u64,
    /// Exit code of the session process (None if still running).
    pub exit_code: Option<i32>,
    /// Total bytes of PTY output produced by this session.
    pub output_bytes: u64,
    /// Current PTY rows.
    pub rows: u16,
    /// Current PTY cols.
    pub cols: u16,
    /// Number of clients currently attached. `amux top` checks this
    /// before resizing the PTY to its viewer's terminal — when a client
    /// is attached, the attacher owns the size (bd-is4).
    pub attach_count: u32,
    /// Number of times this session has been atomically replaced via
    /// `RespawnSession`. Bumped each time `amux respawn` swaps the
    /// child in place; surfaced for telemetry (bd-wh4).
    pub respawn_count: u32,
}
