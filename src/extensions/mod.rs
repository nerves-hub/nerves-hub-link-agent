//! The `extensions` channel.
//!
//! Extensions are the things NervesHub can ask a device for that are not
//! firmware: health metrics, a location, logs, a shell. They are separate from
//! the device channel on purpose — the platform's rule is that extension
//! traffic must never get in the way of an update — and they are negotiated
//! rather than assumed.
//!
//! # How the negotiation works
//!
//! The device joins the `extensions` topic with a map of the extensions it can
//! serve and the version of each:
//!
//! ```text
//! -> phx_join "extensions"  {"health": "0.0.1", "geo": "0.0.1"}
//! <- phx_reply              ["health"]
//! ```
//!
//! The reply is the subset the *platform* wants attached, which is narrower
//! than what the device offered: an extension can be turned off per product or
//! per device, and one the server does not recognise is simply left out. The
//! device then confirms each with `<key>:attached`, and only then does the
//! server start asking it for anything.
//!
//! Both halves have to agree, and either can decline. That is the point: a
//! device that starts reporting something an operator did not ask for is worse
//! than one that reports nothing.
//!
//! # Events
//!
//! Everything is scoped `<key>:<event>` in both directions.
//!
//! ```text
//! <- health:check                {}
//! -> health:report               {"value": {"metrics": {..}, "metadata": {..}}}
//!
//! <- geo:location:request        {}
//! -> geo:location:update         {"latitude": .., "longitude": .., "source": ".."}
//!
//! -> logging:send                {"timestamp": .., "level": .., "message": ..}
//!
//! <- local_shell:request_shell   {}
//! <- local_shell:shell_input     {"data": ".."}
//! <- local_shell:window_size     {"rows": .., "cols": ..}
//! -> local_shell:shell_output    {"data": ".."}
//! ```
//!
//! `logging` is the only one the device starts on its own. The rest are
//! answers, which is what keeps a fleet from swarming the server: nothing is
//! sent on a schedule the device chose.

pub mod geo;
pub mod health;
#[cfg(feature = "local-shell")]
pub mod local_shell;
pub mod logging;
pub mod network_identity;

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::config::Extensions as Config;

/// The topic. Unqualified, like `device` — NervesHub rewrites it.
pub const EXTENSIONS_TOPIC: &str = "extensions";

pub const HEALTH: &str = "health";
pub const GEO: &str = "geo";
pub const LOGGING: &str = "logging";
pub const LOCAL_SHELL: &str = "local_shell";
pub const NETWORK_IDENTITY: &str = "network_identity";

/// The version this agent implements of every extension.
///
/// One number for all of them because they were introduced together and the
/// server matches each with `~> 0.0.1`. Sending a version it cannot match gets
/// the extension quietly dropped from the attach list rather than an error, so
/// this is worth keeping honest.
const VERSION: &str = "0.0.1";

/// Something the server asked the device to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming {
    /// Produce a health report.
    HealthCheck,
    /// Produce a location.
    LocationRequest,
    /// Start a shell and stream its output.
    ShellRequested,
    /// Keystrokes for the running shell.
    ShellInput(String),
    /// Report the identities this device holds elsewhere.
    IdentityRequest,
    /// The user's terminal was resized.
    WindowSize { rows: u16, cols: u16 },
}

/// Which extensions this agent offers, and which the platform attached.
#[derive(Debug, Default)]
pub struct Extensions {
    offered: Vec<&'static str>,
    attached: Vec<String>,
}

impl Extensions {
    pub fn new(config: &Config) -> Self {
        let mut offered = Vec::new();

        if config.health.enabled {
            offered.push(HEALTH);
        }
        if config.geo.enabled {
            offered.push(GEO);
        }
        if config.logging.enabled {
            offered.push(LOGGING);
        }
        if config.network_identity.enabled {
            offered.push(NETWORK_IDENTITY);
        }

        // Offered only if this binary can actually serve one. Advertising a
        // shell and then failing to produce it leaves an operator staring at a
        // blank terminal with nothing to read.
        #[cfg(feature = "local-shell")]
        if config.local_shell.enabled {
            offered.push(LOCAL_SHELL);
        }

        Self {
            offered,
            attached: Vec::new(),
        }
    }

    /// Whether there is anything to negotiate. With nothing offered the channel
    /// is not joined at all, rather than joined and left empty.
    pub fn any(&self) -> bool {
        !self.offered.is_empty()
    }

    /// The join payload: every offered extension and its version.
    pub fn join_payload(&self) -> Value {
        let versions: BTreeMap<&str, &str> =
            self.offered.iter().map(|key| (*key, VERSION)).collect();

        json!(versions)
    }

    /// Confirm the subset the platform asked for.
    ///
    /// Anything in the list that was not offered is ignored rather than
    /// confirmed — the server should not send one, and confirming an extension
    /// this agent cannot serve would have it asked for things it will never
    /// answer.
    pub fn attach(&mut self, attach_list: &[String]) -> Vec<(String, Value)> {
        self.attached = attach_list
            .iter()
            .filter(|key| self.offered.contains(&key.as_str()))
            .cloned()
            .collect();

        self.attached
            .iter()
            .map(|key| (format!("{key}:attached"), json!({})))
            .collect()
    }

    /// Forget everything. Called on reconnect: attachment belongs to a session,
    /// and carrying it across one would have the agent answering for an
    /// extension the new session never attached.
    pub fn reset(&mut self) {
        self.attached.clear();
    }

    pub fn is_attached(&self, key: &str) -> bool {
        self.attached.iter().any(|k| k == key)
    }

    /// Interpret a scoped event from the server.
    ///
    /// An event for an extension that is not attached is dropped. That is not
    /// paranoia: the server pushes on a topic shared by the whole connection,
    /// and a detach that crosses with an in-flight request would otherwise have
    /// the device answering something it just said it would stop answering.
    pub fn handle(&self, event: &str, payload: &Value) -> Option<Incoming> {
        match event {
            "health:check" if self.is_attached(HEALTH) => Some(Incoming::HealthCheck),

            "geo:location:request" if self.is_attached(GEO) => Some(Incoming::LocationRequest),

            "network_identity:request" if self.is_attached(NETWORK_IDENTITY) => {
                Some(Incoming::IdentityRequest)
            }

            "local_shell:request_shell" if self.is_attached(LOCAL_SHELL) => {
                Some(Incoming::ShellRequested)
            }

            "local_shell:shell_input" if self.is_attached(LOCAL_SHELL) => payload
                .get("data")
                .and_then(Value::as_str)
                .map(|data| Incoming::ShellInput(data.to_string())),

            "local_shell:window_size" if self.is_attached(LOCAL_SHELL) => {
                let rows = payload.get("rows").and_then(Value::as_u64).unwrap_or(24) as u16;
                let cols = payload.get("cols").and_then(Value::as_u64).unwrap_or(80) as u16;

                Some(Incoming::WindowSize { rows, cols })
            }

            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ExtensionToggle, Extensions as Config};

    fn config(health: bool, shell: bool) -> Config {
        Config {
            health: ExtensionToggle { enabled: health },
            local_shell: crate::config::LocalShellConfig {
                enabled: shell,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn only_enabled_extensions_are_offered() {
        let extensions = Extensions::new(&config(true, false));

        assert_eq!(extensions.join_payload(), json!({ "health": "0.0.1" }));
    }

    #[test]
    fn nothing_enabled_means_the_channel_is_not_joined() {
        assert!(!Extensions::new(&config(false, false)).any());
    }

    #[test]
    fn the_platform_narrows_what_was_offered() {
        let mut extensions = Extensions::new(&config(true, true));

        // Offered health and local_shell; the platform wants only health.
        let confirmations = extensions.attach(&["health".into()]);

        assert_eq!(confirmations.len(), 1);
        assert_eq!(confirmations[0].0, "health:attached");
        assert!(extensions.is_attached(HEALTH));
        assert!(!extensions.is_attached(LOCAL_SHELL));
    }

    #[test]
    fn an_extension_that_was_never_offered_is_not_confirmed() {
        let mut extensions = Extensions::new(&config(true, false));

        let confirmations = extensions.attach(&["health".into(), "local_shell".into()]);

        assert_eq!(confirmations.len(), 1);
        assert!(!extensions.is_attached(LOCAL_SHELL));
    }

    /// The three frames NervesHub sends a shell session, pinned against what
    /// the server and its browser hook actually send: `request_shell` with an
    /// empty payload, `shell_input` carrying `data`, and `window_size` carrying
    /// `rows` and `cols`. A name or key that drifts here does not fail, it
    /// silently does nothing.
    #[test]
    fn the_shell_frames_decode_once_attached() {
        let mut extensions = Extensions::new(&config(false, true));
        let _ = extensions.attach(&["local_shell".into()]);

        assert_eq!(
            extensions.handle("local_shell:request_shell", &json!({})),
            Some(Incoming::ShellRequested)
        );

        assert_eq!(
            extensions.handle("local_shell:shell_input", &json!({"data": "ls\r"})),
            Some(Incoming::ShellInput("ls\r".into()))
        );

        assert_eq!(
            extensions.handle("local_shell:window_size", &json!({"rows": 40, "cols": 100})),
            Some(Incoming::WindowSize {
                rows: 40,
                cols: 100
            })
        );
    }

    #[test]
    fn events_for_unattached_extensions_are_dropped() {
        let mut extensions = Extensions::new(&config(true, true));
        let _ = extensions.attach(&["health".into()]);

        assert_eq!(
            extensions.handle("health:check", &json!({})),
            Some(Incoming::HealthCheck)
        );
        assert_eq!(
            extensions.handle("local_shell:request_shell", &json!({})),
            None
        );
    }

    #[test]
    fn a_reconnect_forgets_what_was_attached() {
        let mut extensions = Extensions::new(&config(true, false));
        let _ = extensions.attach(&["health".into()]);

        extensions.reset();

        assert!(!extensions.is_attached(HEALTH));
        assert_eq!(extensions.handle("health:check", &json!({})), None);
    }
}
