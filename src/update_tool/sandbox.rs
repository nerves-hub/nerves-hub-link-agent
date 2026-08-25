//! An update tool that cannot break anything.
//!
//! Downloads the firmware, checks its SHA-256, writes it into a working
//! directory and records what it would now be running. It never invokes an
//! updater and never opens a block device, so an agent misconfigured while
//! someone is iterating on it damages a directory.
//!
//! That makes it the right tool for everything above the install itself: the
//! join payload, the update conversation, progress reporting, the IPC decision
//! path, reconnects, deployment targeting. All of that is identical whether the
//! bytes end up in a partition or in `/var/lib`.
//!
//! It reports itself to NervesHub as `fwup`, because it stands in for the fwup
//! path rather than being a format of its own.
//!
//! # What it deliberately does not test
//!
//! Anything a real updater decides: signature verification, whether the archive
//! matches the device, slot switching, rollback. A sandbox install that
//! succeeds says the agent did its part, not that the firmware was any good.

use std::path::PathBuf;

use crate::config::SandboxConfig;
use crate::error::Error;
use crate::update_tool::{BootState, Installed, UpdateTool};
use crate::{FirmwareMeta, Stage, UpdatePayload};

pub struct Sandbox {
    config: SandboxConfig,
    /// What a `current_firmware` call answers with. Persisted to the work dir so
    /// that restarting the agent looks like a device that stayed on the version
    /// it installed, rather than one that forgot.
    current: Option<FirmwareMeta>,
}

impl Sandbox {
    pub fn new(config: SandboxConfig) -> Result<Self, Error> {
        std::fs::create_dir_all(&config.work_dir).map_err(|e| Error::UpdateTool {
            tool: "sandbox",
            message: format!("creating {}: {e}", config.work_dir.display()),
        })?;

        let current = read_recorded(&state_path(&config.work_dir))
            .or_else(|| config.initial_firmware.as_ref().map(Into::into));

        Ok(Self { config, current })
    }

    fn record(&mut self, firmware: &FirmwareMeta) -> Result<(), Error> {
        let path = state_path(&self.config.work_dir);
        let json = serde_json::to_string_pretty(firmware)?;

        std::fs::write(&path, json).map_err(|e| Error::UpdateTool {
            tool: "sandbox",
            message: format!("writing {}: {e}", path.display()),
        })?;

        self.current = Some(firmware.clone());

        Ok(())
    }
}

fn state_path(work_dir: &std::path::Path) -> PathBuf {
    work_dir.join("installed.json")
}

fn read_recorded(path: &std::path::Path) -> Option<FirmwareMeta> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

impl From<&crate::config::SandboxFirmware> for FirmwareMeta {
    fn from(f: &crate::config::SandboxFirmware) -> Self {
        FirmwareMeta {
            uuid: Some(f.uuid.clone()),
            version: Some(f.version.clone()),
            product: Some(f.product.clone()),
            platform: Some(f.platform.clone()),
            architecture: Some(f.architecture.clone()),
        }
    }
}

impl UpdateTool for Sandbox {
    fn name(&self) -> &'static str {
        "fwup"
    }

    fn current_firmware(&self) -> Result<FirmwareMeta, Error> {
        self.current.clone().ok_or(Error::UpdateTool {
            tool: "sandbox",
            message: "nothing installed and no initial_firmware configured".into(),
        })
    }

    fn boot_state(&self) -> Result<BootState, Error> {
        Ok(BootState::Unknown)
    }

    fn install(
        &mut self,
        update: &UpdatePayload,
        progress: &mut dyn FnMut(Stage, u8),
    ) -> Result<Installed, Error> {
        // Implemented on the async side — see `install_async`. This exists so
        // that `Sandbox` still satisfies the trait every tool shares, and so
        // that the day a caller uses it synchronously it says so rather than
        // silently doing nothing.
        let _ = (update, progress);

        Err(Error::UpdateTool {
            tool: "sandbox",
            message: "sandbox installs run on the async path".into(),
        })
    }

    fn mark_valid(&mut self) -> Result<(), Error> {
        log::info!("sandbox: firmware marked valid");
        Ok(())
    }
}

impl Sandbox {
    /// Download, verify, write, record.
    ///
    /// Progress is reported against the download because that is the only phase
    /// with a real denominator here. A real tool splits the two, and a
    /// controller watching progress should not learn to expect this shape.
    pub async fn install_async(
        &mut self,
        update: &UpdatePayload,
        client: &crate::http::Client,
        mut progress: impl FnMut(Stage, u8),
    ) -> Result<Installed, Error> {
        use sha2::{Digest, Sha256};
        use tokio::io::AsyncWriteExt;

        let url = update
            .firmware_url
            .as_deref()
            .ok_or_else(|| Error::Download("update has no firmware url".into()))?;

        let meta = update
            .firmware_meta
            .clone()
            .ok_or_else(|| Error::Download("update has no firmware metadata".into()))?;

        if self.config.fail_installs {
            return Err(Error::UpdateTool {
                tool: "sandbox",
                message: "fail_installs is set".into(),
            });
        }

        let target = self
            .config
            .work_dir
            .join(format!("{}.fw", meta.uuid_or_unknown()));
        let mut file = tokio::fs::File::create(&target).await?;

        let mut response = client.get(url).await?;

        if !response.is_success() {
            return Err(Error::Download(format!(
                "{} returned {}",
                url,
                response.status()
            )));
        }

        // Content-Length rather than the update's `size`: they should agree, and
        // when they do not it is the transfer that decides how far along we are.
        let total = response.content_length().or(update.size).unwrap_or(0);
        let mut hasher = Sha256::new();
        let mut written: u64 = 0;
        let mut last_reported = 0u8;

        while let Some(chunk) = response.chunk().await? {
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
            written += chunk.len() as u64;

            // `total` is zero when the response carried no length, and a
            // percentage of an unknown total is not worth reporting.
            if let Some(percent) = (written * 100).checked_div(total) {
                let percent = percent.min(100) as u8;

                if percent >= last_reported.saturating_add(5) || percent == 100 {
                    last_reported = percent;
                    progress(Stage::Downloading, percent);
                }
            }
        }

        file.flush().await?;

        let digest = format!("{:X}", hasher.finalize());

        // Uppercase hex, matching what NervesHub sends. Compared
        // case-insensitively anyway, because a mismatch here should mean the
        // bytes were wrong and not that someone changed a formatter.
        if let Some(expected) = update.checksum.as_deref() {
            if !expected.eq_ignore_ascii_case(&digest) {
                let _ = tokio::fs::remove_file(&target).await;

                return Err(Error::Download(format!(
                    "checksum mismatch: expected {expected}, got {digest}"
                )));
            }
        }

        // A real install takes minutes on slow storage, and a controller that
        // wants to defer a reboot needs a window in which to be asked. An
        // install that finishes the instant the download does would never
        // exercise that.
        let steps = 10u8;
        let per_step = std::time::Duration::from_millis(
            (self.config.install_duration_secs * 1000 / steps as u64).max(1),
        );

        for step in 1..=steps {
            tokio::time::sleep(per_step).await;
            progress(Stage::Updating, step * (100 / steps));
        }

        self.record(&meta)?;

        log::info!(
            "sandbox: installed {} ({} bytes) to {}",
            meta.uuid_or_unknown(),
            written,
            target.display()
        );

        Ok(Installed {
            firmware: meta,
            bytes_transferred: written,
            reboot_required: true,
        })
    }
}
