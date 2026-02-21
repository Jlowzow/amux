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
    CaptureScrollback {
        name: String,
        lines: usize,
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
    /// Block until a session exits or a timeout elapses.
    WaitSession {
        name: String,
        /// Timeout in seconds (0 = wait forever).
        timeout_secs: u64,
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
}
