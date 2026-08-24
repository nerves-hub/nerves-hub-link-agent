//! The wire format applications speak to the agent.
//!
//! Newline-delimited JSON over a Unix domain socket: one object per line, no
//! framing header, no length prefix. Any language with a JSON library and a
//! socket can talk to the agent in a few lines, which matters more here than
//! compactness — the traffic is a handful of messages an hour.
//!
//! # Why requests go both ways
//!
//! The interesting question is not "what can the application ask the agent",
//! it is "how does the agent ask the application whether to install". That
//! makes this a peer protocol rather than a client/server one: both sides send
//! [`Frame::Request`] and both sides answer with [`Frame::Response`].
//!
//! Request ids are per-direction. The agent's `1` and the application's `1` are
//! different requests, and neither side should try to be clever about it.
//!
//! # Why not D-Bus
//!
//! D-Bus is the idiomatic answer on Yocto, and RAUC itself is D-Bus-native, so
//! an adapter is worth having later. It is not the primary interface because it
//! needs a bus daemon that a minimal Buildroot or single-purpose image often
//! does not run, and because a socket keeps the protocol legible in a log.
//!
//! See `docs/ipc.md` for worked exchanges.

use serde::{Deserialize, Serialize};

use crate::{FirmwareMeta, Stage};

/// The version of this protocol. Sent in [`Frame::Hello`] and checked by the
/// agent, so an application built against a later agent fails at connect with
/// something readable rather than at the first unrecognised method.
pub const API_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    /// First line an application sends. Nothing else is accepted before it.
    Hello {
        /// Free-form, for logs and for telling two connections apart.
        name: String,
        #[serde(default)]
        role: Role,
        api: u32,
        /// Events this connection wants. Empty means none — an application
        /// that only answers `update_available` should not have to read
        /// progress it does not use.
        #[serde(default)]
        subscribe: Vec<String>,
    },
    /// The agent's answer to `Hello`.
    Welcome {
        agent_version: String,
        api: u32,
        role: Role,
        /// Which update tool this device is configured for, so an application
        /// can refuse to run somewhere it does not understand.
        update_tool: String,
    },
    Request {
        id: String,
        #[serde(flatten)]
        method: Method,
    },
    Response {
        id: String,
        #[serde(flatten)]
        result: Response,
    },
    Event {
        #[serde(flatten)]
        event: Event,
    },
}

/// What a connection is for.
///
/// At most one controller may be connected. A second is refused rather than
/// replacing the first: two processes each believing they decide whether the
/// device updates is a bug, and it should surface at connect time on a bench
/// rather than as a fleet that updates when it was told not to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Receives events and may call methods. Never asked to decide anything.
    #[default]
    Observer,
    /// Answers `update_available` and `reboot_request`.
    Controller,
}

/// Everything either side can ask for. One enum rather than two so that a
/// reader can see the whole vocabulary in one place; which side may send which
/// is documented per variant and enforced by the agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Method {
    // ---- application → agent ----
    /// Connection state, identity, running firmware, any update in flight.
    Status,
    /// Mark the running firmware good, so the bootloader stops holding a
    /// rollback. The agent runs whatever the configured tool needs —
    /// `rauc status mark-good`, or `fwup.confirm_command`.
    ///
    /// This is the application's call and not the agent's: the agent knows the
    /// download succeeded and the system booted, which is not the same as
    /// knowing the device works.
    MarkValid,
    /// Reboot now, through the agent, so it can tell NervesHub first and
    /// release a deferred update.
    Reboot { reason: Option<String> },
    /// Change what this connection receives after `Hello`.
    Subscribe { events: Vec<String> },
    /// Application-supplied readings, merged into the health extension report.
    /// Lets an application publish what it knows — queue depth, sensor state —
    /// without opening its own connection to NervesHub.
    Metrics {
        values: std::collections::BTreeMap<String, f64>,
    },

    // ---- agent → application (controller only) ----
    /// NervesHub has an update. Answered with [`Response::Update`].
    UpdateAvailable {
        firmware: FirmwareMeta,
        size: Option<u64>,
        deployment_id: Option<i64>,
    },
    /// The update is installed and needs a reboot to take effect. Answered
    /// with [`Response::Reboot`].
    RebootRequest { firmware: FirmwareMeta },
    /// An operator pressed Identify in the web UI. Blink something.
    Identify,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum Response {
    Ok { result: ResponseBody },
    Err { error: ErrorBody },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ResponseBody {
    Status(Status),
    Update(UpdateDecision),
    Reboot(RebootDecision),
    /// For methods with nothing to say. Serializes as `{}`.
    Empty {},
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ErrorBody {
    /// Stable, matchable. e.g. `not_connected`, `unknown_method`,
    /// `controller_taken`, `unsupported`.
    pub code: String,
    pub message: String,
}

/// What the controller wants done about an available update.
///
/// The three arms map onto statuses NervesHub already understands — a
/// rescheduled device goes into the penalty box for the delay rather than
/// simply going quiet, which is the difference between a deliberate deferral
/// and a device that looks broken.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum UpdateDecision {
    Apply,
    Ignore { reason: String },
    Reschedule { delay_ms: u64, reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RebootDecision {
    Reboot,
    /// Ask again in `delay_ms`. Bounded by `reboot.max_defer_secs`.
    Defer {
        delay_ms: u64,
        reason: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Status {
    pub connection: ConnectionState,
    pub identifier: String,
    pub update_tool: String,
    pub firmware: Option<FirmwareMeta>,
    /// Present while an update is in flight.
    pub update: Option<UpdateStatus>,
    /// Whether the running firmware still needs [`Method::MarkValid`].
    pub pending_validation: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    Connecting,
    Connected,
    Disconnected,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UpdateStatus {
    pub uuid: String,
    pub stage: Stage,
    pub percent: u8,
}

/// Fire-and-forget, agent → subscribers. Never answered.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event", content = "payload", rename_all = "snake_case")]
pub enum Event {
    Connection {
        state: ConnectionState,
    },
    UpdateProgress {
        stage: Stage,
        percent: u8,
    },
    UpdateInstalled {
        firmware: FirmwareMeta,
    },
    UpdateFailed {
        reason: String,
    },
    /// An update is installed and waiting for a reboot that was deferred.
    RebootPending {
        deferred_until_ms: Option<u64>,
    },
}

/// The name used in `Hello.subscribe` and [`Method::Subscribe`].
impl Event {
    pub fn name(&self) -> &'static str {
        match self {
            Event::Connection { .. } => "connection",
            Event::UpdateProgress { .. } => "update_progress",
            Event::UpdateInstalled { .. } => "update_installed",
            Event::UpdateFailed { .. } => "update_failed",
            Event::RebootPending { .. } => "reboot_pending",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Request {
    pub id: String,
    #[serde(flatten)]
    pub method: Method,
}
