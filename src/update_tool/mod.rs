//! Installing firmware.
//!
//! One trait, one implementation per format, chosen once at startup from
//! `[update_tool]` in the config. The name is also what the agent declares in
//! its join params, so the server knows which format it is talking to rather
//! than inferring it from the metadata the device reports.
//!
//! # What an implementation is responsible for
//!
//! Getting the bytes. That is deliberate, and it is the reason this trait takes
//! an [`UpdatePayload`] rather than writing into a sink the agent provides:
//!
//!   * `fwup` reads an archive from stdin, so the agent streams the download
//!     into a child process and the transfer is the agent's HTTP client.
//!   * `rauc` is handed the URL and does its own HTTP, because that is how its
//!     adaptive install works — it fetches ranges of the bundle and skips
//!     blocks the target slot already holds. Downloading the bundle first to
//!     hand it a local file gives up the entire benefit.
//!
//! An implementation that owns the transfer can also own resumption, which the
//! agent has no way to do generically.

use crate::error::Error;
use crate::{FirmwareMeta, Stage, UpdatePayload};

#[cfg(feature = "fwup")]
pub mod fwup;
#[cfg(feature = "rauc")]
pub mod rauc;
#[cfg(feature = "sandbox")]
pub mod sandbox;

/// Whether the running firmware is confirmed, or still on probation.
///
/// A device that reboots into a new image and never confirms it will be rolled
/// back by the bootloader. The agent reports this on join so an operator can
/// see a device stuck in the loop, and exposes it over IPC so an application
/// can tell whether it still owes a `mark_valid`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootState {
    /// Running firmware has been marked good.
    Confirmed,
    /// Running firmware will be rolled back unless confirmed.
    PendingValidation,
    /// The tool has no notion of confirmation, or none is configured.
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Installed {
    /// What is now in the inactive slot and will run after a reboot.
    pub firmware: FirmwareMeta,
    /// Bytes actually transferred. Worth reporting for its own sake: with a
    /// streaming or delta-capable tool this is the number that shows whether
    /// the mechanism is doing anything, and it will not match the firmware
    /// size.
    pub bytes_transferred: u64,
    /// False when the tool applied the update to the running system in place.
    pub reboot_required: bool,
}

pub trait UpdateTool {
    /// Recorded by NervesHub as the tool that produced this device's firmware,
    /// and sent as `update_tool` in the join params.
    fn name(&self) -> &'static str;

    /// What this device is running, for the join payload.
    ///
    /// Read from the system rather than remembered across restarts: an agent
    /// that reports what it last installed will lie after a rollback, and a
    /// rollback is exactly when an accurate answer matters.
    fn current_firmware(&self) -> Result<FirmwareMeta, Error>;

    fn boot_state(&self) -> Result<BootState, Error> {
        Ok(BootState::Unknown)
    }

    /// Fetch and install, leaving the result staged but not yet running.
    ///
    /// `progress` is called as the tool reports it and is throttled by the
    /// caller, not here. Must leave the currently running firmware bootable on
    /// any failure — a partial write that bricks on the next power cut is worse
    /// than a failed update.
    fn install(
        &mut self,
        update: &UpdatePayload,
        progress: &mut dyn FnMut(Stage, u8),
    ) -> Result<Installed, Error>;

    /// Mark the running firmware good so the bootloader stops holding a
    /// rollback. Driven by the application over IPC, never by the agent on its
    /// own — see [`crate::ipc::protocol::Method::MarkValid`].
    fn mark_valid(&mut self) -> Result<(), Error>;
}
