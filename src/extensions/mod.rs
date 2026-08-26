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
//! Four frames, and the device does not start it. The platform asks, saying
//! which extensions it has and the versions of each; the device answers by
//! joining the topic with the subset it also implements:
//!
//! ```text
//! <- extensions:get         {"extensions": {"health": ["0.0.1"], "logging": ["0.1.0", "0.0.1"]}}
//! -> phx_join "extensions"  {"health": "0.0.1", "logging": "0.1.0"}
//! <- phx_reply              ["health"]
//! -> health:attached        {}
//! ```
//!
//! The version is a capability check as much as a version. `logging` at 0.1.0
//! is a different conversation than 0.0.1 -- a second's worth of lines per
//! message rather than one -- so a platform with only the older one is not
//! offered logging at all, rather than being sent batches it cannot read.
//!
//! A platform old enough not to ask never sends the first frame. Waiting for
//! it forever would cost the device every extension it has, so the wait ends:
//! [`crate::agent`] joins anyway a few seconds after the device topic,
//! offering everything this agent implements, which is what such a platform
//! has always served.
//!
//! The reply is the subset the *platform* wants attached, which is narrower
//! still than what the device offered: an extension can be turned off per
//! product or per device. The device then confirms each with
//! `<key>:attached`, and only then does the server start asking it for
//! anything.
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
//! -> logging:send                {"lines": [{"level": .., "message": ..}, ..]}
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

/// The version this agent implements of most extensions.
///
/// One number for them because they were introduced together and the server
/// matches each with `~> 0.0.1`. Sending a version it cannot match gets the
/// extension quietly dropped from the attach list rather than an error, so
/// this is worth keeping honest.
const VERSION: &str = "0.0.1";

/// The logging extension, which is the one that has moved on.
///
/// 0.1.0 says this device puts many log lines in one `logging:send` rather
/// than paying a message per line. The platform serves it with a different
/// module than the 0.0.1 one, and picks between them by the version declared
/// here -- so a NervesHub that has only the 0.0.1 extension matches
/// `~> 0.0.1`, leaves logging out of the attach list, and the device is told
/// rather than having its batches dropped somewhere it cannot see.
const LOGGING_VERSION: &str = "0.1.0";

/// Whether to offer `key`, given what the platform advertised.
///
/// Anything that is not a map of key to versions is treated as no
/// advertisement at all: a malformed message should not be worse for the
/// device than a missing one.
fn offering(advertisement: Option<&Value>, key: &str) -> bool {
    let Some(Value::Object(advertised)) = advertisement else {
        return true;
    };

    advertised
        .get(key)
        .and_then(Value::as_array)
        .is_some_and(|versions| versions.iter().any(|v| v.as_str() == Some(version(key))))
}

/// What this agent implements of `key`.
const fn version(key: &str) -> &'static str {
    // `match` on a `&str` is not const, and this is a two-armed decision.
    if matches!(key.as_bytes(), b"logging") {
        LOGGING_VERSION
    } else {
        VERSION
    }
}

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

        // Config and the platform both have to want it. The device is never
        // the only thing standing between a NervesHub user and a shell, but it
        // is one of them.
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

    /// The join payload: what to offer, given what the platform said it has.
    ///
    /// An extension the platform did not name is left out. It either does not
    /// implement it or an operator has switched it off, and either way there
    /// is nothing to attach -- so a device that offers it anyway is promising
    /// to serve something nothing will ever ask it for.
    ///
    /// So is one named only at versions this agent does not implement. That is
    /// the case the version exists for: `logging` at 0.1.0 is a different
    /// conversation than 0.0.1, and a platform with only the older one cannot
    /// read what this agent would send.
    ///
    /// `None` offers everything. A platform too old to advertise still serves
    /// the versions it always did, and a device that offered it nothing would
    /// be trading working extensions for a message it never got.
    pub fn join_payload(&self, advertisement: Option<&Value>) -> Value {
        let versions: BTreeMap<&str, &str> = self
            .offered
            .iter()
            .filter(|key| offering(advertisement, key))
            .map(|key| (*key, version(key)))
            .collect();

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

        assert_eq!(extensions.join_payload(None), json!({ "health": "0.0.1" }));
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
    fn an_extension_the_platform_did_not_name_is_not_offered() {
        let mut config = Config::default();
        config.health.enabled = true;
        config.geo.enabled = true;

        let extensions = Extensions::new(&config);
        let advertised = json!({ "health": ["0.0.1"] });

        assert_eq!(
            extensions.join_payload(Some(&advertised)),
            json!({ "health": "0.0.1" })
        );
    }

    #[test]
    fn an_extension_the_platform_has_only_at_another_version_is_not_offered() {
        // The version is the whole point: 0.0.1 logging is one line per
        // message, and this agent only speaks the batched one. Offering it
        // anyway would have the platform attach an extension it cannot read.
        let mut config = Config::default();
        config.logging.enabled = true;
        config.health.enabled = true;

        let extensions = Extensions::new(&config);
        let advertised = json!({ "logging": ["0.0.1"], "health": ["0.0.1"] });

        assert_eq!(
            extensions.join_payload(Some(&advertised)),
            json!({ "health": "0.0.1" })
        );
    }

    #[test]
    fn a_version_this_agent_has_is_taken_from_a_list_of_several() {
        let mut config = Config::default();
        config.logging.enabled = true;

        let extensions = Extensions::new(&config);
        let advertised = json!({ "logging": ["0.1.0", "0.0.1"] });

        assert_eq!(
            extensions.join_payload(Some(&advertised)),
            json!({ "logging": "0.1.0" })
        );
    }

    #[test]
    fn a_platform_that_advertises_nothing_is_offered_nothing() {
        let mut config = Config::default();
        config.health.enabled = true;

        let extensions = Extensions::new(&config);

        assert_eq!(extensions.join_payload(Some(&json!({}))), json!({}));
    }

    #[test]
    fn an_advertisement_that_cannot_be_read_is_treated_as_none() {
        // A malformed message should not cost the device every extension it
        // has, which offering nothing at all would.
        let mut config = Config::default();
        config.health.enabled = true;

        let extensions = Extensions::new(&config);

        assert_eq!(
            extensions.join_payload(Some(&json!("nonsense"))),
            json!({ "health": "0.0.1" })
        );
    }

    #[test]
    fn logging_declares_the_version_that_may_batch() {
        // The version is how the platform decides whether this device may send
        // many lines in one message. A NervesHub that predates batching does
        // not match 0.2.0 and leaves logging unattached, which is the point:
        // the agent finds out rather than sending into the dark.
        let mut config = Config::default();
        config.logging.enabled = true;
        config.health.enabled = true;

        let payload = Extensions::new(&config).join_payload(None);

        assert_eq!(payload["logging"], "0.1.0");
        assert_eq!(payload["health"], "0.0.1");
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
