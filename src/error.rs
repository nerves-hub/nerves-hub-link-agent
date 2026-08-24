//! One error type for the whole agent.

use thiserror::Error as ThisError;

#[derive(Debug, ThisError)]
pub enum Error {
    #[error("config: {0}")]
    Config(String),

    #[error("identity: {0}")]
    Identity(String),

    #[error("connection: {0}")]
    Connection(String),

    #[error("download: {0}")]
    Download(String),

    /// The update tool refused, failed, or was not there. Carries the tool's
    /// own output — a failed install is nearly always diagnosed from what fwup
    /// or rauc said, not from where the agent noticed.
    #[error("{tool}: {message}")]
    UpdateTool { tool: &'static str, message: String },

    #[error("ipc: {0}")]
    Ipc(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}
