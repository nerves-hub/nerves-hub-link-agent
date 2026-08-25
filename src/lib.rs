//! NervesHub device agent for Linux.
//!
//! A long-running daemon that connects a Linux device to NervesHub over
//! Phoenix Channels, reports what firmware it is running, and applies
//! over-the-air updates through whichever update tool the device uses.
//!
//! # Two boundaries
//!
//! Almost every design decision in this crate falls out of one of two seams.
//!
//! The first is [`ipc`]. The application running on the device is a separate
//! process, often not Rust, and it is the only thing that knows whether now is
//! a good moment to install an update or to reboot into one. So those
//! questions have to cross a process boundary, in both directions: the agent
//! asks and waits, and it needs a defined answer for every way that can fail.
//!
//! The second is [`update_tool::UpdateTool`]. The thing that writes firmware is
//! a separate program with its own idea of where bytes should come from —
//! `fwup` reads them from stdin, `rauc` wants a URL so it can stream a bundle
//! and skip blocks the target slot already has. So an implementation is handed
//! the update and decides how the transfer happens, rather than being given a
//! byte sink to fill.
//!
//! # Shape of a session
//!
//! ```text
//!   ┌ application (separate process) ┐
//!   │        unix socket, ndjson     │
//!   └──────────────┬─────────────────┘
//!                  │  update_available? ──► apply / ignore / reschedule
//!                  │  reboot now? ───────► reboot / defer
//!                  │  ◄── connection, progress, result events
//!   ┌──────────────┴─────────────────┐
//!   │            agent               │
//!   │  link  ─ phoenix channel ──────┼──► NervesHub
//!   │  update_tool ─ fwup | rauc     │
//!   │  extensions ─ health, geo,     │
//!   │               logging, shell   │
//!   └────────────────────────────────┘
//! ```
//!
//! # Status
//!
//! Working, and exercised end to end against a real NervesHub: both update
//! tools install, roll back and validate on a QEMU rig with a real bootloader.
//! Not yet run on production hardware.
//!
//! `docs/fwup.md` and `docs/rauc.md` are the per-tool guides, `docs/ipc.md` is
//! the protocol, and `examples/agent.toml` is an annotated configuration.

pub mod agent;
pub mod config;
pub mod error;
pub mod extensions;
pub mod http;
pub mod identity;
pub mod ipc;
pub mod link;
pub mod message;
pub mod scripts;
pub mod shared_secret;
pub mod tls;
pub mod transport;
pub mod update_tool;

pub use config::Config;
pub use error::Error;
pub use ipc::protocol::{Event, Frame, Method, Request, Response, Role};
pub use update_tool::{BootState, Installed, UpdateTool};

/// What NervesHub sends on the `update` event and in the reply to `phx_join`.
///
/// Only `update_available` is guaranteed; every other field is absent when
/// there is nothing to do.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct UpdatePayload {
    #[serde(default)]
    pub update_available: bool,
    #[serde(default)]
    pub firmware_url: Option<String>,
    #[serde(default)]
    pub firmware_meta: Option<FirmwareMeta>,
    #[serde(default)]
    pub size: Option<u64>,
    /// SHA-256 of the whole archive, uppercase hex.
    #[serde(default)]
    pub checksum: Option<String>,
    #[serde(default)]
    pub deployment_id: Option<i64>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct FirmwareMeta {
    /// Which firmware this is, when that is knowable.
    ///
    /// `None` for a device whose slot was written by something other than its
    /// update tool -- a RAUC slot flashed at the factory with UUU or dd has no
    /// bundle recorded against it. Such a device still knows its version,
    /// platform and architecture, which is what NervesHub matches deployments
    /// on, so it can be enrolled and updated. Refusing to report at all left it
    /// unable to receive the update that would give it a uuid.
    #[serde(default)]
    pub uuid: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub product: Option<String>,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub architecture: Option<String>,
}

impl FirmwareMeta {
    /// The uuid for logs, environment variables and progress messages, where
    /// there has to be *something* to print.
    pub fn uuid_or_unknown(&self) -> &str {
        self.uuid.as_deref().unwrap_or("unknown")
    }
}

/// Where an update is in its lifecycle. Sent as the `stage` of an
/// `update_progress` message, both to NervesHub and to IPC subscribers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Downloading,
    Updating,
}
