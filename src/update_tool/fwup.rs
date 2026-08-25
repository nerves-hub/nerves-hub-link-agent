//! `fwup`.
//!
//! The archive is streamed into `fwup`'s stdin as it downloads, so nothing is
//! staged on disk and a device needs no free space beyond the slot it is
//! writing into. That also means the download and the write are one operation
//! rather than two phases.
//!
//! # Integrity
//!
//! `fwup` verifies the archive's signature against `public_key` before writing
//! anything, and checks each resource's hash as it streams. A corrupted or
//! tampered archive therefore fails inside fwup rather than after the fact —
//! and because the write goes to the *inactive* slot, a failure leaves the
//! running system exactly as it was.
//!
//! The agent also hashes the bytes it forwards and compares them with the
//! checksum NervesHub sent. That is a weaker check than fwup's and it is not
//! redundant: it distinguishes "the transfer went wrong" from "the archive is
//! not what this device will accept", which are different problems with
//! different fixes.
//!
//! # Deltas
//!
//! Nothing here. NervesHub generates a delta archive that `fwup` applies
//! exactly like a full one, gated server-side on the fwup version this device
//! reports at join. So the only obligation to deltas is to report that version
//! honestly.

use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::config::{FwupConfig, FwupMetadataSource};
use crate::error::Error;
use crate::update_tool::{BootState, Installed, UpdateTool};
use crate::{FirmwareMeta, Stage, UpdatePayload};

pub struct Fwup {
    config: FwupConfig,
    /// Reported at join as `fwup_version`. NervesHub uses it to decide whether
    /// this device may be sent a delta, so a wrong answer here either loses
    /// deltas or sends one the device cannot apply.
    version: String,
}

impl Fwup {
    /// Probe the binary and check the target, both at startup.
    ///
    /// Checked here rather than at install time: a missing binary should stop
    /// the agent while someone is looking at it, not surface as a failed
    /// deployment weeks later.
    pub fn new(config: FwupConfig) -> Result<Self, Error> {
        guard_target(&config)?;

        let output = std::process::Command::new(&config.binary)
            .arg("--version")
            .output()
            .map_err(|e| Error::UpdateTool {
                tool: "fwup",
                message: format!("running {}: {e}", config.binary.display()),
            })?;

        if !output.status.success() {
            return Err(Error::UpdateTool {
                tool: "fwup",
                message: format!("{} --version failed", config.binary.display()),
            });
        }

        let version = String::from_utf8_lossy(&output.stdout).trim().to_string();

        if config.public_key.is_none() {
            log::warn!(
                "fwup: no public_key configured — archives will be applied without \
                 verifying their signature on this device"
            );
        }

        log::info!("fwup {version} writing to {}", config.device.display());

        let tool = Self { config, version };
        tool.reconcile_active_slot();

        Ok(tool)
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    /// Make the environment agree with the slot that actually booted.
    ///
    /// After a rollback the bootloader is running the previous slot but cannot
    /// say so on disk — only Linux can write that environment. Left alone, the
    /// disagreement is not merely cosmetic: fwup chooses which slot to write by
    /// reading `nerves_fw_active`, so the next update would target the slot
    /// currently running and overwrite the working system from under itself.
    ///
    /// Failure here is logged and not fatal. A device that cannot correct its
    /// environment still runs, still reports accurately — `current_firmware`
    /// trusts the command line — and still refuses nothing except a safe
    /// update, which is better than refusing to start.
    fn reconcile_active_slot(&self) {
        let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();

        let Some(booted) = kernel_cmdline_value(&cmdline, "fw_slot")
            .or_else(|| kernel_cmdline_value(&cmdline, "nerves_fw_slot"))
        else {
            return;
        };

        let env = read_metadata(&self.config.metadata)
            .ok()
            .map(|raw| parse_env(&raw));

        // Correct whichever name this image uses; writing the other would leave
        // fwup reading the stale one.
        let variable = match &env {
            Some(env) if env.contains_key("nerves_fw_active") && !env.contains_key("fw_active") => {
                "nerves_fw_active"
            }
            _ => "fw_active",
        };

        let recorded = env.as_ref().and_then(|env| env.get(variable).cloned());

        if recorded.as_deref() == Some(booted.as_str()) {
            return;
        }

        log::warn!(
            "fwup: booted slot {booted} but the environment says {}; correcting it. \
             A rollback happened, and the next update would otherwise overwrite \
             the running slot.",
            recorded.as_deref().unwrap_or("nothing")
        );

        match std::process::Command::new("fw_setenv")
            .args([variable, &booted])
            .output()
        {
            Ok(output) if output.status.success() => {
                log::info!("fwup: {variable} corrected to {booted}")
            }
            Ok(output) => log::error!(
                "fwup: could not correct {variable}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            Err(e) => log::error!("fwup: could not run fw_setenv: {e}"),
        }
    }

    /// Fetch and apply, streaming the archive straight into fwup.
    pub async fn install_async(
        &mut self,
        update: &UpdatePayload,
        client: &crate::http::Client,
        mut progress: impl FnMut(Stage, u8),
    ) -> Result<Installed, Error> {
        let url = update
            .firmware_url
            .as_deref()
            .ok_or_else(|| Error::Download("update has no firmware url".into()))?;

        let meta = update
            .firmware_meta
            .clone()
            .ok_or_else(|| Error::Download("update has no firmware metadata".into()))?;

        let response = client.get(url).await?;

        if !response.is_success() {
            return Err(Error::Download(format!(
                "{url} returned {}",
                response.status()
            )));
        }

        let mut child = self.spawn()?;

        let stdin = child.stdin.take().ok_or_else(|| Error::UpdateTool {
            tool: "fwup",
            message: "fwup produced no stdin".into(),
        })?;

        let stdout = child.stdout.take().ok_or_else(|| Error::UpdateTool {
            tool: "fwup",
            message: "fwup produced no stdout".into(),
        })?;

        let stderr = child.stderr.take().ok_or_else(|| Error::UpdateTool {
            tool: "fwup",
            message: "fwup produced no stderr".into(),
        })?;

        // Feeding fwup and reading its progress have to happen at once: fwup
        // consumes stdin as it writes, so waiting for the transfer to finish
        // before reading stdout deadlocks as soon as the pipe buffer fills.
        let feeder = tokio::spawn(feed(response, stdin));
        let collector = tokio::spawn(collect(stderr));

        let mut lines = BufReader::new(stdout).lines();
        let mut last = 0u8;

        // fwup does not put everything on stderr. With `-n` its progress goes
        // to stdout, and so does the reason it gave up — so a reader that keeps
        // only the integers throws away the one sentence that explains a failed
        // update. Anything unparseable is kept and reported.
        let mut said = String::new();

        while let Ok(Some(line)) = lines.next_line().await {
            match line.trim().parse::<u8>() {
                // `-n` prints one integer per line, 0 to 100.
                Ok(percent) => {
                    if percent != last {
                        last = percent;
                        progress(Stage::Updating, percent.min(100));
                    }
                }
                Err(_) if line.trim().is_empty() => {}
                Err(_) => {
                    said.push_str(line.trim());
                    said.push('\n');
                }
            }
        }

        let status = child.wait().await?;

        let transferred = feeder.await.map_err(|e| Error::UpdateTool {
            tool: "fwup",
            message: format!("the transfer task failed: {e}"),
        })?;

        let complaints = collector.await.unwrap_or_default();

        if !status.success() {
            // fwup's own words, from wherever it chose to put them. A failed
            // update is nearly always diagnosed from what fwup said, not from
            // where the agent noticed it stopped.
            let mut reason = String::new();
            reason.push_str(said.trim());

            if !complaints.trim().is_empty() {
                if !reason.is_empty() {
                    reason.push('\n');
                }
                reason.push_str(complaints.trim());
            }

            if reason.is_empty() {
                reason.push_str("said nothing");
            }

            return Err(Error::UpdateTool {
                tool: "fwup",
                message: format!("exited with {status}: {}", reason.replace('\n', "; ")),
            });
        }

        // Checked after the fact on purpose. fwup has already refused a corrupt
        // archive by here, so reaching this and failing means something subtler:
        // the bytes were right for fwup and wrong for what NervesHub recorded.
        let transferred = transferred?;

        if let Some(expected) = update.checksum.as_deref() {
            if !expected.eq_ignore_ascii_case(&transferred.digest) {
                return Err(Error::Download(format!(
                    "fwup accepted the archive but its checksum was {}, not {expected}",
                    transferred.digest
                )));
            }
        }

        log::info!(
            "fwup applied {} ({} bytes)",
            meta.uuid_or_unknown(),
            transferred.bytes
        );

        Ok(Installed {
            firmware: meta,
            bytes_transferred: transferred.bytes,
            reboot_required: true,
        })
    }

    fn spawn(&self) -> Result<tokio::process::Child, Error> {
        let mut command = tokio::process::Command::new(&self.config.binary);

        command
            .arg("--apply")
            .arg("--task")
            .arg(&self.config.task)
            .arg("-d")
            .arg(&self.config.device)
            // Read the archive from stdin.
            .arg("-i")
            .arg("-")
            // Numeric progress: one integer per line rather than a redrawn bar,
            // which is the difference between parsing output and scraping it.
            .arg("-n")
            // The agent has no idea what else is mounted and no business
            // unmounting it. On a device the target slot is not mounted anyway.
            .arg("--no-unmount");

        if let Some(key) = &self.config.public_key {
            command.arg("--public-key").arg(key);
        }

        command.args(&self.config.extra_args);

        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        command.spawn().map_err(|e| Error::UpdateTool {
            tool: "fwup",
            message: format!("running {}: {e}", self.config.binary.display()),
        })
    }
}

struct Transferred {
    bytes: u64,
    digest: String,
}

/// Stream the response body into fwup, hashing on the way past.
async fn feed(
    mut response: crate::http::Response,
    mut stdin: tokio::process::ChildStdin,
) -> Result<Transferred, Error> {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    let mut bytes = 0u64;

    while let Some(chunk) = response.chunk().await? {
        hasher.update(&chunk);
        bytes += chunk.len() as u64;

        if let Err(e) = stdin.write_all(&chunk).await {
            // fwup exited early — a bad signature, a full device, a task that
            // matches nothing. The broken pipe is the symptom; the reason is on
            // stderr, and the caller reports that instead of this.
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                log::debug!("fwup closed its stdin after {bytes} bytes");
                break;
            }

            return Err(Error::UpdateTool {
                tool: "fwup",
                message: format!("writing to fwup: {e}"),
            });
        }
    }

    // Closing stdin is what tells fwup the archive is complete. Without it fwup
    // waits for more and the agent waits for fwup.
    let _ = stdin.shutdown().await;
    drop(stdin);

    Ok(Transferred {
        bytes,
        digest: format!("{:X}", hasher.finalize()),
    })
}

async fn collect(stderr: tokio::process::ChildStderr) -> String {
    let mut lines = BufReader::new(stderr).lines();
    let mut collected = String::new();

    while let Ok(Some(line)) = lines.next_line().await {
        collected.push_str(&line);
        collected.push('\n');
    }

    collected
}

/// Refuse to run against a block device unless told to in as many words.
///
/// fwup writes to whatever `-d` names, immediately and without confirmation.
/// The difference between a test rig and the disk holding someone's work is one
/// typo in a config file, and the failure is not recoverable. So the default is
/// that the target must already exist as a regular file — which is also exactly
/// what a container or a CI job wants.
fn guard_target(config: &FwupConfig) -> Result<(), Error> {
    use std::os::unix::fs::FileTypeExt;

    let refuse = |message: String| Error::UpdateTool {
        tool: "fwup",
        message,
    };

    let metadata = match std::fs::metadata(&config.device) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(refuse(format!(
                "{} does not exist. Create the image file first (`fwup -a -t complete`), \
                 or set allow_block_device if this really is a device node.",
                config.device.display()
            )))
        }
        Err(e) => return Err(refuse(format!("{}: {e}", config.device.display()))),
    };

    let file_type = metadata.file_type();

    if file_type.is_file() {
        return Ok(());
    }

    if (file_type.is_block_device() || file_type.is_char_device()) && config.allow_block_device {
        log::warn!(
            "fwup: writing to {}, a real device — an update will overwrite it",
            config.device.display()
        );

        return Ok(());
    }

    Err(refuse(format!(
        "{} is not a regular file and allow_block_device is not set",
        config.device.display()
    )))
}

/// Parse `key=value` lines, ignoring blanks and `#` comments.
///
/// The shape `fw_printenv` prints and a reasonable thing for a build to write
/// into a rootfs, so one parser serves a u-boot device and an image with no
/// u-boot at all.
fn parse_env(raw: &str) -> std::collections::BTreeMap<String, String> {
    raw.lines()
        .filter_map(|line| {
            let line = line.trim();

            if line.is_empty() || line.starts_with('#') {
                return None;
            }

            let (key, value) = line.split_once('=')?;

            Some((
                key.trim().to_string(),
                value.trim().trim_matches('"').to_string(),
            ))
        })
        .collect()
}

fn read_metadata(source: &FwupMetadataSource) -> Result<String, Error> {
    let refuse = |message: String| Error::UpdateTool {
        tool: "fwup",
        message,
    };

    match source {
        FwupMetadataSource::File(path) => std::fs::read_to_string(path)
            .map_err(|e| refuse(format!("reading {}: {e}", path.display()))),

        FwupMetadataSource::Command(command) => {
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(command)
                .output()
                .map_err(|e| refuse(format!("running {command:?}: {e}")))?;

            if !output.status.success() {
                return Err(refuse(format!(
                    "{command:?} exited with {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }

            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        }
    }
}

/// A value from the kernel command line.
///
/// This exists because the environment on disk can be wrong. After a rollback
/// the bootloader boots the previous slot, and it cannot rewrite the disk to
/// say so — the environment still names the slot that failed. A device
/// trusting it reports the firmware that just broke as the firmware it is
/// running, which is the one answer worse than reporting nothing.
///
/// Linux can see which partition it is rooted on, but not which *slot* that is
/// meant to be, so the bootloader passes it.
fn kernel_cmdline_value(cmdline: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");

    cmdline
        .split_whitespace()
        .find_map(|token| token.strip_prefix(&prefix))
        .map(str::to_string)
}

/// The metadata for one slot.
///
/// `slot` selects the `a.`/`b.` prefixed variables. `None` reads the unprefixed
/// ones, which is what an image built without per-slot metadata writes — those
/// cannot survive a rollback correctly, but they should still report.
/// The validated flag, under either naming.
///
/// `fw_validated` first and `nerves_fw_validated` second, matching
/// [`metadata_from_env`]. Reading only the prefixed name is what this function
/// exists to prevent: every other reader here learned to accept both when the
/// project dropped the prefix, this one did not, and on a device writing the
/// bare name it reported `Unknown` forever -- so nothing ever knew it owed a
/// validation, on exactly the boot where that matters.
fn validated_flag(env: &std::collections::BTreeMap<String, String>) -> BootState {
    match ["fw_validated", "nerves_fw_validated"]
        .iter()
        .find_map(|key| env.get(*key))
        .map(String::as_str)
    {
        Some("1") => BootState::Confirmed,
        Some("0") => BootState::PendingValidation,
        // Absent entirely: an image built without the flag, which is not the
        // same as one that has failed to validate.
        _ => BootState::Unknown,
    }
}

fn metadata_from_env(
    env: &std::collections::BTreeMap<String, String>,
    slot: Option<&str>,
) -> Result<FirmwareMeta, Error> {
    // `fw_*` first, `nerves_fw_*` second. The second is not this project's
    // naming — it is what a fwup.conf copied from a Nerves system writes, and
    // that is the most likely source of a real one. Reading both means such a
    // config works unchanged rather than reporting nothing at all.
    //
    // Within each, the slot-prefixed value wins over the bare one.
    let get = |suffix: &str| -> Option<String> {
        ["fw_", "nerves_fw_"].iter().find_map(|prefix| {
            let key = format!("{prefix}{suffix}");

            slot.and_then(|slot| env.get(&format!("{slot}.{key}")))
                .or_else(|| env.get(&key))
                .cloned()
        })
    };

    // Still fatal here, unlike RAUC.
    //
    // fwup's metadata all comes from the same U-Boot environment, so an absent
    // uuid means the environment was not written by fwup and version, platform
    // and architecture are absent with it -- there is nothing to report and
    // nothing a deployment could match. RAUC is the opposite case: its
    // `compatible` comes from system.conf and is always there, and the
    // architecture comes from the binary, so a slot with no recorded bundle
    // can still be matched and updated.
    let uuid = get("uuid").ok_or_else(|| Error::UpdateTool {
        tool: "fwup",
        message: match slot {
            Some(slot) => format!(
                "no firmware uuid: looked for {slot}.fw_uuid, fw_uuid, and the nerves_ equivalents"
            ),
            None => "no firmware uuid: looked for fw_uuid and nerves_fw_uuid".into(),
        },
    })?;

    Ok(FirmwareMeta {
        uuid: Some(uuid),
        version: get("version"),
        product: get("product"),
        platform: get("platform"),
        architecture: get("architecture"),
    })
}

impl UpdateTool for Fwup {
    fn name(&self) -> &'static str {
        "fwup"
    }

    fn current_firmware(&self) -> Result<FirmwareMeta, Error> {
        let raw = read_metadata(&self.config.metadata)?;
        let env = parse_env(&raw);

        // The bootloader's answer first, the environment's second. They differ
        // exactly when it matters — after a rollback.
        let cmdline = std::fs::read_to_string("/proc/cmdline").unwrap_or_default();
        let slot = kernel_cmdline_value(&cmdline, "fw_slot")
            .or_else(|| kernel_cmdline_value(&cmdline, "nerves_fw_slot"))
            .or_else(|| env.get("fw_active").cloned())
            .or_else(|| env.get("nerves_fw_active").cloned());

        metadata_from_env(&env, slot.as_deref())
    }

    /// Whether this boot has been validated, from `nerves_fw_validated`.
    ///
    /// The same variable in the same place Nerves keeps it: the u-boot
    /// environment. An upgrade writes `0`, `mark_valid` writes `1`, and a
    /// bootloader configured to watch it reverts to the other slot if a device
    /// keeps booting without ever reaching `1`.
    ///
    /// Reading it does not require u-boot to be running — the environment is a
    /// formatted block on the disk, and `fw_printenv` reads it directly. What
    /// *does* require u-boot is anything acting on it. So a device can report
    /// its state accurately long before anything enforces it, and knowing which
    /// of those two you have is the difference between a rollback story and a
    /// variable nobody reads.
    fn boot_state(&self) -> Result<BootState, Error> {
        let raw = read_metadata(&self.config.metadata)?;

        Ok(validated_flag(&parse_env(&raw)))
    }

    fn install(
        &mut self,
        update: &UpdatePayload,
        progress: &mut dyn FnMut(Stage, u8),
    ) -> Result<Installed, Error> {
        let _ = (update, progress);

        Err(Error::UpdateTool {
            tool: "fwup",
            message: "fwup installs run on the async path".into(),
        })
    }

    fn mark_valid(&mut self) -> Result<(), Error> {
        // Unreachable through `Config::from_toml`, which rejects an fwup
        // configuration without one. Kept as a failure rather than a success
        // because this is the call whose return value becomes
        // `firmware_validated` on the wire: reporting a no-op as done is how
        // the server and the device end up disagreeing about a slot that is
        // about to revert.
        let Some(command) = &self.config.confirm_command else {
            return Err(Error::UpdateTool {
                tool: "fwup",
                message: "no confirm_command configured, so this boot cannot be marked valid"
                    .into(),
            });
        };

        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .output()
            .map_err(|e| Error::UpdateTool {
                tool: "fwup",
                message: format!("running {command:?}: {e}"),
            })?;

        if !output.status.success() {
            return Err(Error::UpdateTool {
                tool: "fwup",
                message: format!(
                    "{command:?} exited with {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            });
        }

        log::info!("fwup: firmware marked valid");

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn env_lines_become_metadata() {
        let env = parse_env(
            "# written by the build\n\
             fw_uuid=abc-123\n\
             fw_version=\"1.2.3\"\n\
             fw_product=gateway\n\
             fw_platform=rpi4\n\
             fw_architecture=arm\n\
             \n",
        );

        let meta = metadata_from_env(&env, None).unwrap();

        assert_eq!(meta.uuid, Some("abc-123".to_string()));
        // Quoted values are unwrapped — fw_printenv does not quote, but a file
        // written by a shell script very often does.
        assert_eq!(meta.version.as_deref(), Some("1.2.3"));
        assert_eq!(meta.platform.as_deref(), Some("rpi4"));
    }

    #[test]
    fn a_missing_uuid_is_an_error_rather_than_a_blank() {
        let env = parse_env("fw_version=1.2.3\n");

        // A device reporting an empty uuid would join and then never match a
        // deployment, which looks like a server problem rather than a device
        // that cannot say what it is running.
        assert!(metadata_from_env(&env, None).is_err());
    }

    #[test]
    fn values_containing_equals_survive() {
        let env = parse_env("fw_misc=a=b=c\n");

        assert_eq!(env["fw_misc"], "a=b=c");
    }

    #[test]
    fn a_config_copied_from_a_nerves_system_still_reads() {
        // Not this project's naming, but the most likely source of a real
        // fwup.conf is someone adapting one from a Nerves system. Reporting
        // nothing at all would be a poor welcome.
        let env = parse_env("nerves_fw_uuid=abc\nnerves_fw_version=1.0.0\n");
        let meta = metadata_from_env(&env, None).unwrap();

        assert_eq!(meta.uuid, Some("abc".to_string()));
        assert_eq!(meta.version.as_deref(), Some("1.0.0"));
    }

    #[test]
    fn our_own_naming_wins_when_both_are_present() {
        let env = parse_env("fw_uuid=ours\nnerves_fw_uuid=theirs\n");

        assert_eq!(metadata_from_env(&env, None).unwrap().uuid, Some("ours".to_string()));
    }

    /// Calls the production function rather than restating it. The previous
    /// version of this helper was a copy of the match in `boot_state`, so it
    /// went on passing while that match read a variable no device wrote.
    fn boot_state_from(env: &str) -> BootState {
        validated_flag(&parse_env(env))
    }

    #[test]
    fn the_running_slot_decides_which_metadata_is_reported() {
        // The state after a rollback: the environment still names slot b as
        // active, but the bootloader booted a.
        let env = parse_env(
            "fw_active=b\n\
             a.fw_uuid=aaaa\n\
             a.fw_version=1.0.0\n\
             b.fw_uuid=bbbb\n\
             b.fw_version=2.0.0\n",
        );

        let rolled_back = metadata_from_env(&env, Some("a")).unwrap();

        assert_eq!(rolled_back.uuid, Some("aaaa".to_string()));
        assert_eq!(rolled_back.version.as_deref(), Some("1.0.0"));

        // And trusting the environment instead would report the firmware that
        // just failed as the one running.
        let believed = metadata_from_env(&env, Some("b")).unwrap();

        assert_eq!(believed.uuid, Some("bbbb".to_string()));
    }

    #[test]
    fn unprefixed_metadata_still_reads() {
        let env = parse_env("fw_uuid=abc\nfw_version=1.0.0\n");

        // An image from before per-slot metadata. Asking for slot a falls back
        // rather than failing.
        assert_eq!(metadata_from_env(&env, Some("a")).unwrap().uuid, Some("abc".to_string()));
    }

    #[test]
    fn the_kernel_command_line_is_where_the_truth_is() {
        let cmdline = "root=/dev/vda1 rw nerves_fw_slot=a nerves_fw_validated=1 console=ttyAMA0";

        assert_eq!(
            kernel_cmdline_value(cmdline, "nerves_fw_slot").as_deref(),
            Some("a")
        );
        assert_eq!(
            kernel_cmdline_value(cmdline, "nerves_fw_validated").as_deref(),
            Some("1")
        );
        assert_eq!(kernel_cmdline_value(cmdline, "nerves_fw_previous"), None);
        // A key that is a suffix of another must not match.
        assert_eq!(
            kernel_cmdline_value("xnerves_fw_slot=b", "nerves_fw_slot"),
            None
        );
    }

    #[test]
    fn the_bare_flag_is_read_as_well_as_the_prefixed_one() {
        // What this project's own fwup.conf writes.
        assert_eq!(
            boot_state_from("fw_validated=0\n"),
            BootState::PendingValidation
        );
        assert_eq!(boot_state_from("fw_validated=1\n"), BootState::Confirmed);
    }

    #[test]
    fn our_own_naming_wins_for_the_flag_too() {
        assert_eq!(
            boot_state_from("fw_validated=1\nnerves_fw_validated=0\n"),
            BootState::Confirmed
        );
    }

    #[test]
    fn the_validated_flag_decides_the_boot_state() {
        assert_eq!(
            boot_state_from("nerves_fw_validated=1\n"),
            BootState::Confirmed
        );
        assert_eq!(
            boot_state_from("nerves_fw_validated=0\n"),
            BootState::PendingValidation
        );
    }

    #[test]
    fn a_missing_flag_is_unknown_rather_than_unvalidated() {
        // An image built without the flag has not failed to validate — it has
        // nothing to say. Reporting PendingValidation would have an operator
        // chasing a rollback that was never armed.
        assert_eq!(boot_state_from("fw_uuid=abc\n"), BootState::Unknown);
    }

    #[test]
    fn a_target_that_does_not_exist_is_refused_with_advice() {
        let config = FwupConfig {
            device: PathBuf::from("/nonexistent/disk.img"),
            task: "upgrade".into(),
            binary: PathBuf::from("fwup"),
            public_key: None,
            extra_args: vec![],
            allow_block_device: false,
            confirm_command: None,
            metadata: FwupMetadataSource::default(),
        };

        let message = guard_target(&config).unwrap_err().to_string();

        assert!(message.contains("does not exist"), "{message}");
        assert!(message.contains("allow_block_device"), "{message}");
    }
}
