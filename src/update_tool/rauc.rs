//! RAUC.
//!
//! `rauc install <url>` — the bundle is never downloaded first. RAUC streams it
//! and, with a verity bundle, fetches only the blocks the target slot lacks. So
//! the presigned URL must honour `Range` and must outlive the install, which on
//! a slow link is considerably longer than a download of the same bundle.
//!
//! There is no delta artifact and NervesHub should never build one for this
//! format — the saving is in what the device declines to fetch.
//!
//! # `rauc install` needs the RAUC service
//!
//! The command is a D-Bus client; the work happens in `rauc service`. A device
//! without that service running gets `Error creating proxy: Could not connect`,
//! which reads like a broken bundle rather than a missing daemon.
//!
//! So the service is probed at startup rather than at install time — a device
//! that cannot install should say so while someone is looking at it, not when a
//! deployment reaches it.
//!
//! # Identity
//!
//! RAUC has no notion of a UUID, and NervesHub requires one. It is derived from
//! a SHA-256 over the bundle's manifest, which is the digest RAUC itself
//! computes: `rauc info` reports it as `hash`, and RAUC records it against the
//! slot it installed into as `bundle.hash`. That is what makes it recoverable
//! here — the device reads back the same value NervesHub derived at upload,
//! without having been told it.
//!
//! # Signatures
//!
//! RAUC verifies against its own keyring from `system.conf` and refuses an
//! unsigned bundle outright, so there is no signature code here. It also means
//! the trust anchor is provisioned by the image build rather than by NervesHub.

use std::process::Stdio;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::config::RaucConfig;
use crate::error::Error;
use crate::update_tool::{BootState, Installed, UpdateTool};
use crate::{FirmwareMeta, Stage, UpdatePayload};

/// The oldest RAUC that can report which firmware a device is running.
///
/// `bundle.hash` was added to slot status in 1.9. Before that a device installs
/// and reboots perfectly well and then has no way to say what it is running,
/// which NervesHub reads as a device whose firmware is unknown.
const MINIMUM_VERSION: (u32, u32) = (1, 9);

pub struct Rauc {
    config: RaucConfig,
    version: String,
}

impl Rauc {
    /// Probe the binary *and* the service.
    ///
    /// Two checks rather than one because they fail for different reasons and
    /// only one of them is a missing package.
    pub fn new(config: RaucConfig) -> Result<Self, Error> {
        let version = Self::probe(&config)?;

        // Checked here because the symptom otherwise appears somewhere else
        // entirely: the install succeeds, the device reboots, and the join
        // reports no firmware at all.
        match parse_version(&version) {
            Some(found) if found < MINIMUM_VERSION => {
                return Err(Error::UpdateTool {
                    tool: "rauc",
                    message: format!(
                        "rauc {version} is too old: {}.{} or newer is needed for slot status \
                         to record the bundle hash, which is how this device says which \
                         firmware it is running. Bundles would install and then be \
                         unidentifiable.",
                        MINIMUM_VERSION.0, MINIMUM_VERSION.1
                    ),
                })
            }
            // An unparseable version is not grounds for refusing to start. A
            // distribution patch suffix should not stop a device booting, and
            // the failure it would otherwise cause is visible and specific.
            Some(_) | None => {}
        }

        // `status` is the cheapest command that goes through D-Bus, so it
        // answers "is the service there" without changing anything.
        match Self::run(&config, &["status"]) {
            Ok(_) => log::info!("rauc {version}, service reachable"),
            Err(e) => {
                return Err(Error::UpdateTool {
                    tool: "rauc",
                    message: format!(
                        "rauc {version} is installed but its service is not reachable ({e}). \
                         `rauc install` is a D-Bus client — the device needs `rauc service` \
                         running, usually as rauc.service."
                    ),
                })
            }
        }

        Ok(Self { config, version })
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    /// The system's `compatible`, which is how RAUC decides whether a bundle
    /// belongs on this device at all.
    ///
    /// Reported at join so NervesHub can recognise the metadata as RAUC's
    /// without sniffing it — see `recognises_device_metadata?/1` on the server.
    /// Read from the running system rather than remembered, so it stays true
    /// across an update that changes it.
    pub fn compatible(&self) -> String {
        self.status()
            .ok()
            .and_then(|status| {
                status
                    .get("compatible")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_default()
    }

    fn probe(config: &RaucConfig) -> Result<String, Error> {
        let output = std::process::Command::new(&config.binary)
            .arg("--version")
            .output()
            .map_err(|e| Error::UpdateTool {
                tool: "rauc",
                message: format!("running {}: {e}", config.binary.display()),
            })?;

        Ok(String::from_utf8_lossy(&output.stdout)
            .trim()
            .trim_start_matches("rauc ")
            .to_string())
    }

    fn run(config: &RaucConfig, args: &[&str]) -> Result<String, Error> {
        let output = std::process::Command::new(&config.binary)
            .args(args)
            .output()
            .map_err(|e| Error::UpdateTool {
                tool: "rauc",
                message: format!("running rauc {}: {e}", args.join(" ")),
            })?;

        if !output.status.success() {
            return Err(Error::UpdateTool {
                tool: "rauc",
                message: format!(
                    "rauc {} exited with {}: {}",
                    args.join(" "),
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            });
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn status(&self) -> Result<Value, Error> {
        // `--detailed` is not optional. Without it `rauc status --output-format=json`
        // reports a slot's class, device, bootname and state and stops there —
        // no `slot_status`, and therefore no bundle hash. The plain form looks
        // complete enough that its absence reads as a device that has never had
        // a bundle installed.
        let raw = Self::run(
            &self.config,
            &["status", "--detailed", "--output-format=json"],
        )?;

        // rauc writes progress and warnings to stderr, but a stray line on
        // stdout would still break a strict parse — so the JSON object is
        // located rather than assumed to be the whole of it.
        let json = raw
            .find('{')
            .map(|start| &raw[start..])
            .ok_or_else(|| Error::UpdateTool {
                tool: "rauc",
                message: "rauc status produced no JSON".into(),
            })?;

        serde_json::from_str(json).map_err(|e| Error::UpdateTool {
            tool: "rauc",
            message: format!("could not read rauc status: {e}"),
        })
    }

    /// Fetch and install, letting RAUC do the transfer.
    pub async fn install_async(
        &mut self,
        update: &UpdatePayload,
        _client: &crate::http::Client,
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

        if !self.config.stream_from_url {
            return Err(Error::UpdateTool {
                tool: "rauc",
                message: "staging the bundle to disk is not implemented; \
                          streaming is the reason to use RAUC"
                    .into(),
            });
        }

        let mut command = tokio::process::Command::new(&self.config.binary);
        command.arg("install").arg("--progress").arg(url);

        if self.config.tls_no_verify {
            log::warn!(
                "rauc: TLS verification is off for the bundle download — for a NervesHub \
                 with a self-signed certificate, and nothing else"
            );
            command.arg("--tls-no-verify");
        }

        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = command.spawn().map_err(|e| Error::UpdateTool {
            tool: "rauc",
            message: format!("running {}: {e}", self.config.binary.display()),
        })?;

        let stdout = child.stdout.take().ok_or_else(|| Error::UpdateTool {
            tool: "rauc",
            message: "rauc produced no stdout".into(),
        })?;

        let stderr = child.stderr.take().ok_or_else(|| Error::UpdateTool {
            tool: "rauc",
            message: "rauc produced no stderr".into(),
        })?;

        let collector = tokio::spawn(collect(stderr));

        let mut lines = BufReader::new(stdout).lines();
        let mut last = 0u8;
        let mut said = String::new();

        while let Ok(Some(line)) = lines.next_line().await {
            match percentage(&line) {
                Some(percent) if percent != last => {
                    last = percent;
                    // One phase, not two: RAUC downloads and writes at once,
                    // and reporting a download percentage separately would be
                    // inventing a number it does not have.
                    progress(Stage::Updating, percent);
                }
                Some(_) => {}
                None => {
                    let trimmed = line.trim();

                    if !trimmed.is_empty() {
                        said.push_str(trimmed);
                        said.push('\n');
                    }
                }
            }
        }

        let status = child.wait().await?;
        let complaints = collector.await.unwrap_or_default();

        if !status.success() {
            let reason = [said.trim(), complaints.trim()]
                .iter()
                .filter(|part| !part.is_empty())
                .cloned()
                .collect::<Vec<_>>()
                .join("; ");

            return Err(Error::UpdateTool {
                tool: "rauc",
                message: format!(
                    "exited with {status}: {}",
                    if reason.is_empty() {
                        "said nothing"
                    } else {
                        &reason
                    }
                ),
            });
        }

        log::info!("rauc installed {}", meta.uuid_or_unknown());

        Ok(Installed {
            firmware: meta,
            // RAUC does the transfer, and how much of the bundle it actually
            // fetched is exactly the number worth knowing — which makes it a
            // shame that it does not report one. Zero rather than the bundle
            // size, because claiming the whole thing was transferred would
            // hide the entire benefit of using RAUC.
            bytes_transferred: 0,
            reboot_required: true,
        })
    }
}

/// Major and minor from a version string, ignoring anything after them.
///
/// `rauc --version` prints things like `1.13` and `1.9.1`, and distributions
/// add their own suffixes.
fn parse_version(version: &str) -> Option<(u32, u32)> {
    let mut parts = version
        .trim()
        .split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty());

    Some((
        parts.next()?.parse().ok()?,
        parts.next().unwrap_or("0").parse().unwrap_or(0),
    ))
}

/// The first `NN%` in a line, if there is one.
///
/// `rauc install --progress` draws a bar, and on a pipe that arrives as lines
/// carrying a percentage somewhere in them rather than as a bare number. Finding
/// it rather than parsing a fixed position means the surrounding text can change
/// without the parser noticing.
fn percentage(line: &str) -> Option<u8> {
    let (before, _) = line.split_once('%')?;

    let digits: String = before
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    digits.parse().ok().map(|percent: u8| percent.min(100))
}

async fn collect(stderr: tokio::process::ChildStderr) -> String {
    let mut lines = BufReader::new(stderr).lines();
    let mut collected = String::new();

    while let Ok(Some(line)) = lines.next_line().await {
        collected.push_str(line.trim());
        collected.push('\n');
    }

    collected
}

/// The slot RAUC says is booted, from a `rauc status` document.
fn booted_slot(status: &Value) -> Option<&Value> {
    status
        .get("slots")?
        .as_array()?
        .iter()
        .flat_map(|entry| entry.as_object())
        .flat_map(|entry| entry.values())
        .find(|slot| slot.get("state").and_then(Value::as_str) == Some("booted"))
}

/// The metadata NervesHub needs, from the image's own record of itself.
///
/// `Ok(None)` when there is no such file, which is not an error: an image built
/// before this existed reports from its installed bundle instead.
///
/// A uuid here is a *declared* one — the build wrote the same value into the
/// bundle manifest's `[meta.nerveshub]` section, so NervesHub records the
/// identifier the device reports. That is what a derived uuid cannot be: it
/// hashes the manifest, which covers the rootfs, so it cannot live inside the
/// rootfs it identifies.
fn metadata_from_file(path: &std::path::Path) -> Result<Option<FirmwareMeta>, std::io::Error> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };

    let mut fields = std::collections::HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let value = value.trim();
            if !value.is_empty() {
                fields.insert(key.trim().to_string(), value.to_string());
            }
        }
    }

    // A file with no uuid in it is not an identity. Treated as absent so the
    // bundle hash still gets a chance, rather than reporting a firmware with
    // nothing to identify it.
    if !fields.contains_key("uuid") {
        return Ok(None);
    }

    let take = |key: &str| fields.get(key).cloned();

    Ok(Some(FirmwareMeta {
        uuid: take("uuid"),
        version: take("version"),
        product: take("product"),
        platform: take("platform"),
        architecture: take("architecture"),
    }))
}

/// The metadata NervesHub needs, from the booted slot's recorded bundle.
///
/// `architecture` is deliberately absent. RAUC records `compatible`, `version`,
/// `description`, `build` and `hash` against a slot and nothing else — the
/// architecture lived in the manifest's `[meta.nerveshub]` section, which is a
/// build-time thing RAUC does not carry into slot status. NervesHub fills it in
/// from the firmware row it matched by UUID, which is why reporting `None` is
/// correct here rather than a gap.
fn metadata_from_status(status: &Value) -> Result<FirmwareMeta, Error> {
    let slot = booted_slot(status).ok_or_else(|| Error::UpdateTool {
        tool: "rauc",
        message: "rauc status names no booted slot".into(),
    })?;

    let bundle = slot.get("slot_status").and_then(|s| s.get("bundle"));

    // A missing bundle hash is not an error.
    //
    // RAUC records one only against a slot *it* installed, so a device flashed
    // at the factory with UUU or dd has none. That device still knows its
    // compatible and its architecture, which is what NervesHub matches
    // deployments on -- so it can be enrolled and sent its first update, which
    // is precisely what gives it a hash. Treating this as fatal deadlocked
    // enrolment on the thing enrolment would have fixed.
    let hash = bundle.and_then(|b| b.get("hash")).and_then(Value::as_str);

    if hash.is_none() {
        log::warn!(
            "the booted slot records no bundle hash, so this device cannot say which \
             firmware it is running -- a slot written by something other than \
             `rauc install` has none. Reporting platform and architecture only; the \
             first RAUC update will fill it in."
        );
    }

    let string = |key: &str| {
        bundle
            .and_then(|b| b.get(key))
            .and_then(Value::as_str)
            .map(str::to_string)
    };

    // From the bundle when there is one, otherwise the running system's own,
    // which is what a device with no recorded bundle has to fall back on.
    let compatible = string("compatible").or_else(|| {
        status
            .get("compatible")
            .and_then(Value::as_str)
            .map(str::to_string)
    });

    Ok(FirmwareMeta {
        uuid: hash.map(uuid_from_hash).transpose()?,
        version: string("version"),
        // `compatible` is the closest RAUC has to either, and it is what
        // NervesHub's server side records as the platform.
        platform: compatible.clone(),
        product: compatible,
        // The architecture this binary was compiled for, which is necessarily
        // the device's. RAUC does not carry it into slot status -- it lives in
        // the manifest's `[meta.nerveshub]` section, a build-time thing -- and
        // the server can only fill it in from a firmware row matched by uuid.
        // A device without a uuid has no such row, and architecture is half of
        // what deployment matching needs, so it is reported from here instead.
        architecture: Some(std::env::consts::ARCH.to_string()),
    })
}

/// The first sixteen bytes of the manifest hash, as a UUID.
///
/// Must match what NervesHub derives at upload, or the device and the server
/// disagree about what firmware is installed.
fn uuid_from_hash(hash: &str) -> Result<String, Error> {
    let bytes: Vec<u8> = (0..hash.len().min(32))
        .step_by(2)
        .filter_map(|i| u8::from_str_radix(hash.get(i..i + 2)?, 16).ok())
        .collect();

    if bytes.len() < 16 {
        return Err(Error::UpdateTool {
            tool: "rauc",
            message: format!("bundle hash {hash:?} is not a sha256"),
        });
    }

    let hex = |range: std::ops::Range<usize>| {
        bytes[range]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };

    Ok([hex(0..4), hex(4..6), hex(6..8), hex(8..10), hex(10..16)].join("-"))
}

impl UpdateTool for Rauc {
    fn name(&self) -> &'static str {
        "rauc"
    }

    fn current_firmware(&self) -> Result<FirmwareMeta, Error> {
        // The image's own record first.
        //
        // It is written by the build into the rootfs, so it is present from the
        // first boot however the device was flashed, and it is replaced with the
        // slot it describes rather than living alongside it on a data partition
        // where it could drift.
        //
        // RAUC's slot status is the fallback, for images built before this
        // existed. It stays authoritative for boot state, which is what it is
        // actually for.
        match metadata_from_file(&self.config.firmware_file) {
            Ok(Some(meta)) => Ok(meta),
            Ok(None) => metadata_from_status(&self.status()?),
            Err(e) => {
                log::warn!(
                    "{} could not be read ({e}); falling back to the installed \
                     bundle's hash",
                    self.config.firmware_file.display()
                );
                metadata_from_status(&self.status()?)
            }
        }
    }

    fn boot_state(&self) -> Result<BootState, Error> {
        let status = self.status()?;

        let Some(slot) = booted_slot(&status) else {
            return Ok(BootState::Unknown);
        };

        Ok(match slot.get("boot_status").and_then(Value::as_str) {
            Some("good") => BootState::Confirmed,
            // "bad" means the bootloader has been told not to use this slot,
            // which is a stronger statement than "not yet confirmed" — but from
            // the agent's side both mean the same thing: something still owes a
            // decision about this firmware.
            //
            // A system configured with `bootloader=noop` reports every slot as
            // "bad", because there is no bootloader to have marked one good. So
            // does a slot that genuinely failed. Telling those apart needs a
            // real bootloader backend, and until there is one this errs toward
            // "not yet confirmed" rather than claiming a slot is fine.
            Some("bad") => BootState::PendingValidation,
            _ => BootState::Unknown,
        })
    }

    fn install(
        &mut self,
        update: &UpdatePayload,
        progress: &mut dyn FnMut(Stage, u8),
    ) -> Result<Installed, Error> {
        let _ = (update, progress);

        Err(Error::UpdateTool {
            tool: "rauc",
            message: "rauc installs run on the async path".into(),
        })
    }

    fn mark_valid(&mut self) -> Result<(), Error> {
        Self::run(&self.config, &["status", "mark-good", "booted"])?;

        log::info!("rauc: booted slot marked good");

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> Value {
        // The shape `rauc status --detailed --output-format=json` produces,
        // taken from a RAUC 1.13 device rather than from reading its source —
        // the plain (non-detailed) form omits `slot_status` entirely.
        serde_json::json!({
            "compatible": "acme-gateway",
            "booted": "A",
            "boot_primary": "rootfs.0",
            "slots": [
                {"rootfs.1": {
                    "class": "rootfs", "bootname": "B", "state": "inactive",
                    "boot_status": "good",
                    "slot_status": {"bundle": {"version": "1.0.0", "hash": "aa".repeat(32)}}
                }},
                {"rootfs.0": {
                    "class": "rootfs", "bootname": "A", "state": "booted",
                    "boot_status": "good",
                    "slot_status": {"bundle": {
                        "compatible": "acme-gateway",
                        "version": "1.4.2",
                        "hash": "65547c8981853d087e73551be4c474011fbde82a5eb34fca865e2c4822a2e144"
                    }}
                }}
            ]
        })
    }

    #[test]
    fn the_booted_slot_is_the_one_reported() {
        let meta = metadata_from_status(&status()).unwrap();

        // Not the inactive slot, which carries a different version entirely.
        assert_eq!(meta.version.as_deref(), Some("1.4.2"));
        assert_eq!(meta.platform.as_deref(), Some("acme-gateway"));
    }

    #[test]
    fn the_uuid_matches_what_nerveshub_derives() {
        // This exact digest is what RAUC reports for the bundle NervesHub's
        // own test fixture is built from, and the server derives the same UUID
        // from the manifest. If these two ever disagree, the device and the
        // server have stopped agreeing about what firmware is installed.
        let meta = metadata_from_status(&status()).unwrap();

        assert_eq!(meta.uuid, Some("65547c89-8185-3d08-7e73-551be4c47401".to_string()));
    }

    #[test]
    fn architecture_comes_from_the_binary_rather_than_rauc() {
        // RAUC does not record it: it lived in the manifest's meta section,
        // which is a build-time thing. The server can only fill it in from a
        // firmware row matched by UUID, and a device with no recorded bundle
        // has no such row -- so the agent reports the architecture it was
        // compiled for, which is necessarily the device's.
        assert_eq!(
            metadata_from_status(&status()).unwrap().architecture.as_deref(),
            Some(std::env::consts::ARCH)
        );
    }

    #[test]
    fn a_slot_with_no_bundle_still_reports_enough_to_be_updated() {
        // The factory case: a slot written by UUU or dd rather than by
        // `rauc install` has no bundle recorded against it. This used to be a
        // hard error, which deadlocked enrolment -- the device could not report,
        // so it could not be sent the update that would give it something to
        // report.
        let mut status = status();
        status["slots"][1]["rootfs.0"]["slot_status"] = serde_json::json!({});

        let meta = metadata_from_status(&status).unwrap();

        // Nothing to identify *which* firmware this is, and that is honest.
        assert_eq!(meta.uuid, None);

        // But both halves of what NervesHub matches deployments on are here,
        // which is what makes the device updatable.
        assert_eq!(meta.platform.as_deref(), Some("acme-gateway"));
        assert_eq!(meta.architecture.as_deref(), Some(std::env::consts::ARCH));
    }

    #[test]
    fn the_compatible_falls_back_to_the_running_system() {
        // With no bundle there is no bundle `compatible` to read, so it comes
        // from the top level of `rauc status` -- the system's own, out of
        // system.conf, which is always there.
        let mut status = status();
        status["slots"][1]["rootfs.0"]["slot_status"] = serde_json::json!({});

        let meta = metadata_from_status(&status).unwrap();

        assert_eq!(meta.product.as_deref(), Some("acme-gateway"));
    }

    #[test]
    fn no_booted_slot_is_an_error_rather_than_a_guess() {
        let mut status = status();
        status["slots"][1]["rootfs.0"]["state"] = serde_json::json!("inactive");

        assert!(metadata_from_status(&status).is_err());
    }

    #[test]
    fn percentages_are_found_wherever_they_sit_in_the_line() {
        assert_eq!(percentage("  42% Copying image"), Some(42));
        assert_eq!(percentage("installing 7% done"), Some(7));
        assert_eq!(percentage("100% Installing done."), Some(100));
        assert_eq!(percentage("no number here"), None);
        // A line that is only a marker, not progress.
        assert_eq!(percentage("% odd"), None);
    }

    #[test]
    fn versions_parse_the_way_rauc_prints_them() {
        assert_eq!(parse_version("1.13"), Some((1, 13)));
        assert_eq!(parse_version("1.9.1"), Some((1, 9)));
        assert_eq!(parse_version("1.8-2+deb12u1"), Some((1, 8)));
        assert_eq!(parse_version("nonsense"), None);
    }

    #[test]
    fn the_minimum_is_the_version_that_records_a_bundle_hash() {
        // 1.8 installs verity bundles perfectly well. It just cannot say what
        // it installed afterwards, which is the part that matters here.
        assert!(parse_version("1.8").unwrap() < MINIMUM_VERSION);
        assert!(parse_version("1.9").unwrap() >= MINIMUM_VERSION);
    }

    #[test]
    fn a_hash_that_is_not_a_sha256_is_refused() {
        assert!(uuid_from_hash("abc").is_err());
    }
}
