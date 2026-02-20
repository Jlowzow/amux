use serde::{Deserialize, Serialize};

/// Requests from client to daemon.
#[derive(Serialize, Deserialize, Debug)]
pub enum ClientMessage {
    Ping,
    KillServer,
    CreateSession {
        name: Option<String>,
        command: Vec<String>,
    },
    ListSessions,
    KillSession {
        name: String,
    },
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
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SessionInfo {
    pub name: String,
    pub command: String,
    pub pid: u32,
    pub alive: bool,
}
