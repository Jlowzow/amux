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
}
