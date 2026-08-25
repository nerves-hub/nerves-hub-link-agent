//! Configuration, read from TOML at startup.
//!
//! Loaded from `--config <path>`, else `$NERVES_HUB_AGENT_CONFIG`, else
//! `/etc/nerves-hub-link-agent.toml`. See `examples/agent.toml` for an
//! annotated version of everything below.
//!
//! Nothing here is read from the environment field-by-field. A device's
//! configuration should be one file an operator can look at and diff, not a
//! file plus a unit file plus whatever the init system happened to export.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub server: Server,
    pub identity: Identity,
    pub update_tool: UpdateToolConfig,
    #[serde(default)]
    pub ipc: Ipc,
    #[serde(default)]
    pub updates: Updates,
    #[serde(default)]
    pub reboot: Reboot,
    #[serde(default)]
    pub extensions: Extensions,
    #[serde(default)]
    pub scripts: ScriptsConfig,
}

/// Support scripts, run as shell scripts.
///
/// On a Nerves device a support script is Elixir evaluated in the running VM.
/// There is no VM here, and the things someone reaches for when a device
/// misbehaves are commands rather than expressions, so a script is a shell
/// script.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScriptsConfig {
    /// On by default, unlike the extensions.
    ///
    /// Support scripts are the main way anyone diagnoses a device they cannot
    /// reach, and NervesHub already decides who may run one. A device that
    /// silently ignored them would be indistinguishable from a broken one. Turn
    /// it off and the agent still answers — saying scripts are disabled, rather
    /// than going quiet and leaving an operator watching a spinner.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Where scripts are staged. Each gets a private directory, removed after.
    #[serde(default = "default_script_dir")]
    pub work_dir: PathBuf,
    /// Used for a script with no shebang. A script that has one is run
    /// directly, so this is the default rather than the rule.
    #[serde(default = "default_interpreter")]
    pub interpreter: String,
    /// Kill a script that runs longer than this.
    ///
    /// Must stay under NervesHub's own 15 second limit, after which it stops
    /// listening for the answer. A script that overruns the server's deadline
    /// produces output nobody receives and a process still running on the
    /// device with nothing watching it.
    #[serde(default = "default_script_timeout")]
    pub timeout_secs: u64,
    /// Output beyond this is cut, keeping the beginning.
    #[serde(default = "default_script_output")]
    pub max_output_bytes: usize,
}

impl Default for ScriptsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            work_dir: default_script_dir(),
            interpreter: default_interpreter(),
            timeout_secs: default_script_timeout(),
            max_output_bytes: default_script_output(),
        }
    }
}

/// Extensions this device offers NervesHub.
///
/// Everything here is off by default. An extension sends data — or opens a way
/// in — that an operator may not expect a device to have, so each is asked for
/// rather than assumed, and the platform still gets the final say over whether
/// it is attached.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Extensions {
    #[serde(default)]
    pub health: ExtensionToggle,
    #[serde(default)]
    pub geo: GeoConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub local_shell: LocalShellConfig,
    #[serde(default)]
    pub network_identity: NetworkIdentityConfig,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct NetworkIdentityConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Where to find each identity. Empty means the extension reports an empty
    /// list, which is a true answer and not an error.
    #[serde(default)]
    pub identities: Vec<IdentitySource>,
}

/// One identity, and how to find it.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IdentitySource {
    /// `iroh`, `netbird`, `tailscale` or `wireguard`. Anything else is
    /// accepted here and ignored by the server, which is the right way round:
    /// a newer NervesHub can learn a service without the agent changing.
    pub service: String,
    /// A literal value, for something the device cannot be asked for.
    #[serde(default)]
    pub identifier: Option<String>,
    /// A command whose output holds the identifier.
    #[serde(default)]
    pub command: Option<String>,
    /// Reach into JSON output, e.g. `/Self/PublicKey`. Saves needing `jq` on
    /// the device for the two services whose CLIs speak JSON.
    #[serde(default)]
    pub json_pointer: Option<String>,
    /// Which endpoint this is, for a device running more than one of a service.
    #[serde(default)]
    pub instance: Option<String>,
    /// Anything else worth recording alongside — a relay URL, an overlay IP.
    #[serde(default)]
    pub details: std::collections::BTreeMap<String, String>,
}

/// An extension with nothing to configure but whether it runs.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ExtensionToggle {
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GeoConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub source: GeoSource,
}

/// Where a position comes from.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GeoSource {
    /// Ask the Nerves project's `whenwhere` service, which reads the address
    /// the request came from. Same service and same `source: "geoip"` that
    /// `nerves_hub_link` uses by default, so a mixed fleet lands on one map
    /// with one set of caveats.
    ///
    /// It is run as a courtesy with no availability guarantee. Nothing polls
    /// it: a lookup happens only when the platform asks, which is on attach and
    /// then rarely.
    Whenwhere { url: Option<String> },
    /// A position someone measured and typed in. Correct to whatever precision
    /// they used, and wrong the moment the device moves — which for most
    /// installed hardware is never, and is far better than GeoIP.
    Fixed {
        latitude: f64,
        longitude: f64,
        #[serde(default)]
        accuracy: Option<f64>,
    },
    /// Run a command and read `{"latitude": .., "longitude": ..}` from stdout.
    /// For a device with a GPS, where the agent has no business knowing how to
    /// talk to it.
    ///
    /// A newtype rather than a struct variant so the config reads
    /// `source = { command = "..." }` instead of the doubled-up
    /// `source = { command = { command = "..." } }`.
    Command(String),
}

impl Default for GeoSource {
    fn default() -> Self {
        GeoSource::Whenwhere { url: None }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LoggingConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub source: LoggingSource,
    /// Lines per second the agent will send.
    ///
    /// NervesHub rate limits log lines per device and silently drops the
    /// excess, so a device that sends faster than this loses lines without
    /// being told. Matching the server's limit here means the dropping happens
    /// where it can be logged instead.
    #[serde(default = "default_log_rate")]
    pub max_lines_per_second: u32,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            source: LoggingSource::default(),
            max_lines_per_second: default_log_rate(),
        }
    }
}

/// Where log lines come from.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LoggingSource {
    /// `journalctl -f -o json`, parsed for PRIORITY and MESSAGE. The obvious
    /// answer on anything running systemd.
    Journald {
        #[serde(default)]
        unit: Option<String>,
    },
    /// Any command that writes lines to stdout. `tail -F /var/log/messages` on
    /// a system without systemd, or something that already emits JSON.
    Command(String),
}

impl Default for LoggingSource {
    fn default() -> Self {
        LoggingSource::Journald { unit: None }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LocalShellConfig {
    /// Off by default, and it should stay off unless someone has thought about
    /// it. Every other extension lets the platform read something; this one
    /// lets a NervesHub user run commands as whatever the agent runs as. The
    /// authorization is entirely NervesHub's — the device does not get to ask
    /// who is on the other end.
    #[serde(default)]
    pub enabled: bool,
    /// The shell to run.
    #[serde(default = "default_shell")]
    pub command: String,
    /// How much output to buffer per flush, in bytes.
    #[serde(default = "default_shell_chunk")]
    pub chunk_bytes: usize,
}

impl Default for LocalShellConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            command: default_shell(),
            chunk_bytes: default_shell_chunk(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Server {
    /// Host only — no scheme, no path. e.g. `devices.nervescloud.com`.
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Where `DeviceSocket` is mounted.
    ///
    /// `/device-socket` on both of NervesHub's endpoints, which is why it is
    /// the default. The device endpoint also answers on `/socket`, but the web
    /// endpoint serves the *user* socket there — so a device pointed at port
    /// 4000 with the wrong path gets a websocket that authenticates nothing and
    /// speaks a different protocol, rather than an error that says so.
    #[serde(default = "default_socket_path")]
    pub path: String,
    /// False sends credentials in clear and lets anything on the path
    /// impersonate the server. For a bench on a trusted network, nothing else.
    #[serde(default = "default_true")]
    pub tls: bool,
    /// PEM root used to verify the server. `None` uses the system trust store.
    #[serde(default)]
    pub ca_certificate: Option<PathBuf>,
    /// Accept any server certificate.
    ///
    /// For a NervesHub running on a laptop with a self-signed certificate, and
    /// for nothing else. Named to be uncomfortable to type and logged loudly at
    /// startup, because it turns TLS into obfuscation: anything on the path can
    /// present its own certificate and read the shared secret in the handshake
    /// headers.
    #[serde(default)]
    pub danger_accept_invalid_certs: bool,
    /// Phoenix heartbeat interval. Must stay under the server's socket timeout.
    #[serde(default = "default_heartbeat")]
    pub heartbeat_interval_secs: u64,
    /// Reconnect backoff, in seconds, walked in order then repeating the last.
    #[serde(default = "default_backoff")]
    pub reconnect_backoff_secs: Vec<u64>,
}

/// How the device proves who it is.
///
/// Untagged so the file reads as `certificate = ...` or `product_key = ...`
/// rather than making an operator write a discriminator that the presence of
/// the fields already implies.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Identity {
    /// mTLS. The device identifier comes from the certificate's CN, so it is
    /// not configured separately and cannot drift from what the server sees.
    Certificate {
        certificate: PathBuf,
        private_key: PathBuf,
    },
    /// An HMAC shared secret, issued per product or per device.
    ///
    /// A shared secret says nothing about which device is presenting it, so the
    /// identifier is carried alongside — see [`Identifier`]. NervesHub
    /// registers an unknown identifier on first connection.
    SharedSecret {
        product_key: String,
        product_secret: String,
        identifier: Identifier,
    },
}

/// Where the device identifier comes from.
///
/// Nerves devices get this from `nerves_runtime`, which reads a serial number
/// the system knows how to find. A Yocto or Debian image has no such
/// convention, so all three of these are real answers depending on whether the
/// serial lives in a file, in a chip that needs a command to read, or is baked
/// into the image at build time.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Identifier {
    /// A literal value. Only sensible for a one-off or a bench device — baking
    /// it into an image gives every device the same identifier.
    Literal(String),
    /// Read the first line of a file, e.g. `/sys/firmware/devicetree/base/serial-number`.
    File(PathBuf),
    /// Run a command and take the first line of stdout. Runs once at startup;
    /// a failure here is fatal rather than a device that registers as something
    /// unexpected.
    Command(String),
}

/// Which update tool this device uses.
///
/// Tagged by `name`, and the name is also what the agent sends as
/// `update_tool` in its join params — NervesHub prefers an explicit
/// declaration over sniffing the reported metadata.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "name", rename_all = "snake_case")]
pub enum UpdateToolConfig {
    /// Downloads and verifies, then writes to a file and stops. See
    /// [`crate::update_tool::sandbox`].
    Sandbox(SandboxConfig),
    Fwup(FwupConfig),
    Rauc(RaucConfig),
}

impl UpdateToolConfig {
    /// The value NervesHub records in `firmwares.tool`.
    pub fn tool_name(&self) -> &'static str {
        match self {
            // The sandbox stands in for fwup rather than announcing itself,
            // because the point of it is to exercise the fwup path. A tool name
            // NervesHub does not know would be rejected at join and there would
            // be nothing to test.
            UpdateToolConfig::Sandbox(_) => "fwup",
            UpdateToolConfig::Fwup(_) => "fwup",
            UpdateToolConfig::Rauc(_) => "rauc",
        }
    }

    /// Whether this tool can write outside its own working directory.
    pub fn can_touch_the_system(&self) -> bool {
        !matches!(self, UpdateToolConfig::Sandbox(_))
    }
}

/// The update tool that cannot break anything.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SandboxConfig {
    /// Everything it writes goes here and nowhere else.
    #[serde(default = "default_sandbox_dir")]
    pub work_dir: PathBuf,
    /// Metadata to report as the running firmware on the first join, before
    /// anything has been installed. Without it a fresh container has no
    /// firmware to report and NervesHub has nothing to compare a deployment
    /// against.
    #[serde(default)]
    pub initial_firmware: Option<SandboxFirmware>,
    /// Pretend an install takes this long, so progress reporting and a
    /// controller's reboot deferral have something to happen during.
    #[serde(default = "default_sandbox_install_secs")]
    pub install_duration_secs: u64,
    /// Fail every install. For exercising the failure path on purpose.
    #[serde(default)]
    pub fail_installs: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SandboxFirmware {
    pub uuid: String,
    pub version: String,
    pub product: String,
    pub platform: String,
    pub architecture: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FwupConfig {
    /// The block device fwup writes to, e.g. `/dev/mmcblk0`.
    pub device: PathBuf,
    /// The fwup task to run. `upgrade` for an A/B update.
    #[serde(default = "default_fwup_task")]
    pub task: String,
    #[serde(default = "default_fwup_bin")]
    pub binary: PathBuf,
    /// Public key fwup verifies the archive against.
    ///
    /// Optional, and it should not be. NervesHub verifies the signature at
    /// upload, but that is a statement about the archive it stored, not about
    /// the bytes that reached this device. Left optional only because a bench
    /// setup often has no keys yet.
    #[serde(default)]
    pub public_key: Option<String>,
    #[serde(default)]
    pub extra_args: Vec<String>,
    /// Permit `device` to be a block device rather than a regular file.
    ///
    /// Off by default, and the agent refuses to start otherwise. Pointing fwup
    /// at the wrong `/dev/...` overwrites it with no confirmation and no undo,
    /// and the difference between a test rig and a workstation is one typo.
    /// A real device sets this; a development machine has no reason to.
    #[serde(default)]
    pub allow_block_device: bool,
    /// Run after a successful boot to mark the new firmware good, in response
    /// to an IPC `mark_valid`. On Nerves this is `nerves_runtime`'s job; on a
    /// generic image it depends on the bootloader, so it is a command rather
    /// than something this agent claims to know.
    #[serde(default)]
    pub confirm_command: Option<String>,
    /// Where the running firmware's metadata is read from.
    #[serde(default)]
    pub metadata: FwupMetadataSource,
}

/// Where the running firmware's `fw_*` values come from.
///
/// Both variants are parsed the same way — `key=value` per line — because
/// that is what `fw_printenv` prints and it is a reasonable thing for a build
/// to write into a rootfs. One parser covers a u-boot device and an image that
/// has no u-boot at all.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FwupMetadataSource {
    /// A file in the rootfs, written by the build from the same values that
    /// fed `meta-*` in the fwup.conf.
    ///
    /// True by construction: the rootfs *is* the firmware, so a file inside it
    /// describing what is running cannot disagree with what is running. What it
    /// cannot tell you is anything about the other slot or whether this boot is
    /// still on probation — that lives in the bootloader.
    File(PathBuf),
    /// `fw_printenv`, or anything else printing `key=value` lines. The Nerves
    /// convention, and the right answer on a device with a u-boot environment.
    Command(String),
}

impl Default for FwupMetadataSource {
    fn default() -> Self {
        FwupMetadataSource::File(PathBuf::from("/etc/nerves-hub/firmware.env"))
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RaucConfig {
    #[serde(default = "default_rauc_bin")]
    pub binary: PathBuf,
    /// Hand `rauc install` the URL rather than a downloaded file.
    ///
    /// This is the whole reason to want RAUC: it streams the bundle over HTTP
    /// range requests and skips blocks the target slot already has, so a small
    /// change costs a small download without NervesHub generating a patch. It
    /// needs the presigned URL to honour `Range` and to outlive the install.
    #[serde(default = "default_true")]
    pub stream_from_url: bool,
    /// Where to stage the bundle when `stream_from_url` is false. Needs room
    /// for a whole bundle.
    #[serde(default)]
    pub download_dir: Option<PathBuf>,
    /// Pass `--tls-no-verify` to `rauc install`.
    ///
    /// RAUC does its own transfer, so the agent's `danger_accept_invalid_certs`
    /// does not reach it — a NervesHub with a self-signed certificate needs
    /// both. Separate rather than derived, because trusting the socket and
    /// trusting the firmware host are different decisions and it should take
    /// two deliberate acts to give up both.
    #[serde(default)]
    pub tls_no_verify: bool,
    /// Where the image records which firmware it is.
    ///
    /// RAUC records a bundle hash only against a slot *it* installed, so a
    /// device flashed at the factory — UUU, dd, a card image — has none and
    /// cannot say what it is running. A file written into the rootfs by the
    /// image build has neither problem: it is replaced atomically with the slot
    /// it describes, so it cannot drift from it, and it is there from the first
    /// boot however the bytes arrived.
    ///
    /// `key=value` lines: uuid, version, product, platform, architecture.
    ///
    /// Falls back to the installed bundle's hash when the file is absent, which
    /// is what images built before this shipped will do.
    #[serde(default = "default_firmware_file")]
    pub firmware_file: PathBuf,
}

fn default_firmware_file() -> PathBuf {
    PathBuf::from("/etc/nerves-hub/firmware")
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Ipc {
    /// Where applications connect. Deleted and recreated at startup.
    #[serde(default = "default_socket")]
    pub socket: PathBuf,
    /// Group that owns the socket. With `mode`, this is the *entire* access
    /// control story: anyone who can open the socket can approve an update,
    /// defer a reboot and read the device's identity. There is no
    /// authentication on the socket itself.
    #[serde(default)]
    pub group: Option<String>,
    /// Socket permissions, octal. `0o660` with a `group` set is the intent.
    #[serde(default = "default_socket_mode")]
    pub mode: u32,
    /// Refuse to start until a controller has connected.
    ///
    /// For a device where applying an update without asking the application is
    /// never acceptable. Off by default, because the failure mode is a fleet
    /// that silently stops updating when an application regresses.
    #[serde(default)]
    pub require_controller: bool,
}

impl Default for Ipc {
    fn default() -> Self {
        Self {
            socket: default_socket(),
            group: None,
            mode: default_socket_mode(),
            require_controller: false,
        }
    }
}

/// What to do when NervesHub offers an update.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Updates {
    #[serde(default)]
    pub policy: UpdatePolicy,
    /// How long the controller has to answer `update_available`.
    #[serde(default = "default_ask_timeout")]
    pub ask_timeout_secs: u64,
    /// The answer when the controller does not give one in time.
    #[serde(default)]
    pub on_timeout: Fallback,
    /// The answer when no controller is connected at all.
    #[serde(default)]
    pub on_no_controller: Fallback,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdatePolicy {
    /// Apply whatever NervesHub sends, without asking. Matches what
    /// `nerves_hub_link` does out of the box.
    #[default]
    Apply,
    /// Ask the controller and do what it says.
    Ask,
}

/// What a policy resolves to when nobody answers.
///
/// This exists as its own type because both timeouts need one and neither may
/// be left undefined. An agent that blocks forever waiting for an application
/// that crashed is a device that has quietly left the fleet, and it looks
/// healthy from the server the whole time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Fallback {
    #[default]
    Apply,
    Ignore,
}

/// What to do once an update is installed but not yet running.
///
/// Deliberately separate from [`Updates`]: an application that is happy to
/// download at any time may still be in the middle of something it cannot be
/// interrupted during, and conflating the two forces it to refuse the download
/// to protect the reboot.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Reboot {
    #[serde(default)]
    pub policy: RebootPolicy,
    pub ask_timeout_secs: Option<u64>,
    /// Give up deferring after this long and reboot anyway. `None` means the
    /// application can defer indefinitely, which is a real choice for a device
    /// that must never interrupt work, and a way to strand firmware otherwise.
    pub max_defer_secs: Option<u64>,
    /// How the device reboots.
    ///
    /// `sudo reboot` by default, because the agent has no business running as
    /// root — it downloads from the network and runs support scripts, and both
    /// of those are better done unprivileged. That means it needs a sudoers
    /// rule for exactly this one command, e.g.
    ///
    /// ```text
    /// agent ALL=(root) NOPASSWD: /sbin/reboot
    /// ```
    ///
    /// Set it to `reboot` where the agent already runs as root, or to
    /// `systemctl reboot` for an init system that wants to sequence its own
    /// shutdown.
    #[serde(default = "default_reboot_command")]
    pub command: String,
}

impl Default for Reboot {
    fn default() -> Self {
        Self {
            policy: RebootPolicy::default(),
            ask_timeout_secs: None,
            max_defer_secs: None,
            command: default_reboot_command(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RebootPolicy {
    /// Reboot as soon as the install finishes.
    #[default]
    Immediate,
    /// Ask the controller, and honour a deferral.
    Ask,
    /// Never reboot. The application reboots when it is ready, via the IPC
    /// `reboot` method or by any other means.
    Never,
}

fn default_port() -> u16 {
    443
}
fn default_true() -> bool {
    true
}
fn default_socket_path() -> String {
    "/device-socket".into()
}
fn default_heartbeat() -> u64 {
    30
}
fn default_backoff() -> Vec<u64> {
    vec![1, 2, 5, 10, 30, 60]
}
fn default_fwup_task() -> String {
    "upgrade".into()
}
fn default_fwup_bin() -> PathBuf {
    PathBuf::from("/usr/bin/fwup")
}
fn default_rauc_bin() -> PathBuf {
    PathBuf::from("/usr/bin/rauc")
}
fn default_sandbox_dir() -> PathBuf {
    PathBuf::from("/var/lib/nerves-hub-link-agent/sandbox")
}
fn default_sandbox_install_secs() -> u64 {
    5
}
fn default_socket() -> PathBuf {
    PathBuf::from("/run/nerves-hub-link-agent.sock")
}
fn default_socket_mode() -> u32 {
    0o660
}
fn default_ask_timeout() -> u64 {
    30
}
fn default_reboot_command() -> String {
    "sudo reboot".into()
}
fn default_script_dir() -> PathBuf {
    PathBuf::from("/var/lib/nerves-hub-link-agent/scripts")
}
fn default_interpreter() -> String {
    "bash".into()
}
/// Under NervesHub's 15s, with room for the round trip.
fn default_script_timeout() -> u64 {
    10
}
fn default_script_output() -> usize {
    64 * 1024
}
fn default_log_rate() -> u32 {
    5
}
fn default_shell() -> String {
    "/bin/sh".into()
}
fn default_shell_chunk() -> usize {
    4096
}

impl Identity {
    /// Where this identity's device identifier comes from.
    ///
    /// A certificate carries it in its CN, so there is nothing to configure and
    /// nothing that can disagree with what the server sees. Until the
    /// certificate path is implemented, that case has no answer and says so.
    pub fn identifier(&self) -> &Identifier {
        match self {
            Identity::SharedSecret { identifier, .. } => identifier,
            // Refused by `Config::validate`, so no configuration the agent
            // will actually run reaches this. It resolves to nothing rather
            // than panicking, and `identity::resolve` rejects an empty
            // identifier -- two ways to fail safely instead of a backtrace.
            Identity::Certificate { .. } => {
                static UNCONFIGURED: Identifier = Identifier::Literal(String::new());
                &UNCONFIGURED
            }
        }
    }
}

impl Config {
    /// Parse TOML. Does not touch the filesystem beyond what the caller read,
    /// so a config can be checked in a test without a device around it.
    pub fn from_toml(source: &str) -> Result<Self, crate::Error> {
        let config: Self =
            toml::from_str(source).map_err(|e| crate::Error::Config(e.to_string()))?;

        config.validate()?;

        Ok(config)
    }

    /// Reject a configuration that cannot do what it will be asked to do.
    ///
    /// At startup rather than at the point of use, because the failure this
    /// catches is silent at the point of use: an fwup agent with nothing to run
    /// used to answer `mark_valid` with success, report `firmware_validated` to
    /// NervesHub, and leave `fw_validated` at `0` on the disk. The server is
    /// then told the update is good while the bootloader counts down to
    /// reverting it, and the two only disagree out loud after the reboot.
    fn validate(&self) -> Result<(), crate::Error> {
        // Parseable but not implemented. Without this the file loads, the agent
        // starts, and `Identity::identifier` panics the first time anything
        // asks -- which on this path is immediately, at startup, with a
        // backtrace instead of a sentence.
        if matches!(self.identity, Identity::Certificate { .. }) {
            return Err(crate::Error::Config(
                "certificate identities are not implemented yet; use a shared secret \
                 (product_key / product_secret / identifier)"
                    .into(),
            ));
        }

        if let UpdateToolConfig::Fwup(fwup) = &self.update_tool {
            if fwup.confirm_command.is_none() {
                return Err(crate::Error::Config(
                    "update_tool.confirm_command is required for fwup: without it the agent \
                     cannot mark a boot valid, and an unvalidated slot is rolled back"
                        .into(),
                ));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The annotated example is the documentation, so it has to stay parseable.
    #[test]
    fn the_example_config_parses() {
        let source = include_str!("../examples/agent.toml");
        let config = Config::from_toml(source).expect("examples/agent.toml should parse");

        assert_eq!(config.server.host, "devices.nervescloud.com");
        assert_eq!(config.update_tool.tool_name(), "fwup");
        assert!(matches!(config.identity, Identity::SharedSecret { .. }));
        assert_eq!(config.updates.policy, UpdatePolicy::Apply);
        assert_eq!(config.reboot.policy, RebootPolicy::Immediate);
    }

    /// Both rigs boot from this repository, and the fwup one shipped the RAUC
    /// configuration for a while because a single agent.toml served both.
    #[test]
    fn the_rig_configs_parse_and_name_their_own_tool() {
        let fwup = Config::from_toml(include_str!("../test/device/agent-fwup.toml"))
            .expect("agent-fwup.toml should parse");
        let rauc = Config::from_toml(include_str!("../test/device/agent-rauc.toml"))
            .expect("agent-rauc.toml should parse");

        assert_eq!(fwup.update_tool.tool_name(), "fwup");
        assert_eq!(rauc.update_tool.tool_name(), "rauc");
    }

    /// The certificate block parses, so without an explicit refusal the agent
    /// starts and then cannot answer the first question asked of it.
    #[test]
    fn a_certificate_identity_is_refused_with_a_reason() {
        let source = r#"
            [server]
            host = "example.test"

            [identity]
            certificate = "/tmp/cert.pem"
            private_key = "/tmp/key.pem"

            [update_tool]
            name = "sandbox"
        "#;

        let error = Config::from_toml(source).expect_err("should be refused");

        assert!(
            error.to_string().contains("not implemented"),
            "got: {error}"
        );
    }

    /// The config the Buildroot package installs at /etc. It ships to devices,
    /// so it has to keep passing the same validation a hand-written one does.
    #[test]
    fn the_buildroot_packaged_config_parses() {
        let source = include_str!("../support/buildroot/package/nerves-hub-link-agent/agent.toml");

        let config = Config::from_toml(source).expect("the packaged config should parse");

        assert_eq!(config.update_tool.tool_name(), "fwup");
        assert!(config.server.tls, "a shipped default must not be plaintext");
    }

    /// An fwup agent that cannot mark a boot valid used to start anyway, then
    /// answer `mark_valid` with success and report `firmware_validated` while
    /// the flag on disk stayed at `0`.
    #[test]
    fn fwup_without_a_confirm_command_is_refused_at_load() {
        let source = r#"
            [server]
            host = "example.test"

            [identity]
            product_key = "k"
            product_secret = "s"
            identifier = { literal = "d" }

            [update_tool]
            name = "fwup"
            device = "/tmp/disk.img"
            metadata = { file = "/etc/firmware.env" }
        "#;

        let error = Config::from_toml(source).expect_err("should be refused");

        assert!(
            error.to_string().contains("confirm_command"),
            "the error should name the missing setting, got: {error}"
        );
    }

    #[test]
    fn a_shared_secret_identity_carries_its_own_identifier() {
        let source = r#"
            [server]
            host = "devices.nervescloud.com"

            [identity]
            product_key = "nhp_abc"
            product_secret = "shh"
            identifier = { file = "/sys/firmware/devicetree/base/serial-number" }

            [update_tool]
            name = "rauc"
        "#;

        let config = Config::from_toml(source).expect("should parse");

        assert_eq!(config.update_tool.tool_name(), "rauc");
        match config.identity {
            Identity::SharedSecret { identifier, .. } => {
                assert!(matches!(identifier, Identifier::File(_)))
            }
            other => panic!("expected a shared secret, got {other:?}"),
        }
    }
}
