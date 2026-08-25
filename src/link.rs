//! The channel conversation, as pure functions over messages.
//!
//! Deliberately has no socket in it. The frames are the part most likely to be
//! subtly wrong — a topic that gets rewritten server-side, a join payload
//! missing a key that silently disables a feature — and they are testable only
//! if nothing here needs a server to run.
//!
//! The run loop that owns the socket is [`crate::agent`].

use serde_json::{json, Value};

use crate::config::Extensions as ExtensionsConfig;
use crate::extensions::{Extensions, Incoming, EXTENSIONS_TOPIC};
use crate::message::{event, Message, RefGenerator, CONTROL_TOPIC, DEVICE_TOPIC};
use crate::{Stage, UpdatePayload};

/// Something the run loop has to act on, beyond replying on the channel.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    None,
    /// The join was accepted. Carries the update the server offered in its
    /// reply, if any — a device that reconnects into a live deployment is told
    /// at join rather than by a separate `update`.
    Joined(Box<Option<UpdatePayload>>),
    /// The join was refused. Almost always the metadata: a version NervesHub
    /// cannot parse, or a product that does not accept this tool.
    JoinFailed(String),
    /// Download and install this.
    ApplyUpdate(Box<UpdatePayload>),
    /// An operator pressed Reboot.
    Reboot,
    /// An operator pressed Identify.
    Identify,
    /// The server closed or errored the channel.
    Reconnect,
    /// The platform attached these extensions. The caller confirms each with
    /// [`Link::extension`] and starts serving them.
    ExtensionsAttached(Vec<(String, Value)>),
    /// An extension was asked for something.
    Extension(Incoming),
    /// An operator ran a support script against this device.
    RunScript {
        reference: String,
        text: String,
    },
}

pub struct Link {
    refs: RefGenerator,
    join_ref: Option<String>,
    extensions_join_ref: Option<String>,
    joined: bool,
    device_api_version: String,
    update_tool: String,
    extensions: Extensions,
}

impl Link {
    pub fn new(
        device_api_version: impl Into<String>,
        update_tool: impl Into<String>,
        extensions: &ExtensionsConfig,
    ) -> Self {
        Self {
            refs: RefGenerator::default(),
            join_ref: None,
            extensions_join_ref: None,
            joined: false,
            device_api_version: device_api_version.into(),
            update_tool: update_tool.into(),
            extensions: Extensions::new(extensions),
        }
    }

    /// Whether any extension is offered. With none there is nothing to join.
    pub fn has_extensions(&self) -> bool {
        self.extensions.any()
    }

    pub fn extension_attached(&self, key: &str) -> bool {
        self.extensions.is_attached(key)
    }

    /// Join the extensions channel, offering what this agent can serve.
    ///
    /// Joined after the device channel, not before. Extension traffic is
    /// explicitly less important than an update, and a device that cannot get
    /// as far as reporting its firmware has nothing to gain from negotiating a
    /// health report.
    pub fn join_extensions(&mut self) -> Message {
        let reference = self.refs.next_ref();
        self.extensions_join_ref = Some(reference.clone());

        Message {
            join_ref: Some(reference.clone()),
            reference: Some(reference),
            topic: EXTENSIONS_TOPIC.into(),
            event: event::JOIN.into(),
            payload: self.extensions.join_payload(),
        }
    }

    /// A message on the extensions topic — a confirmation, or a reply.
    pub fn extension(&mut self, event: &str, payload: Value) -> Message {
        Message {
            join_ref: self.extensions_join_ref.clone(),
            reference: Some(self.refs.next_ref()),
            topic: EXTENSIONS_TOPIC.into(),
            event: event.into(),
            payload,
        }
    }

    pub fn joined(&self) -> bool {
        self.joined
    }

    /// The `phx_join` for the device topic.
    ///
    /// The topic is `device`, unqualified — NervesHub rewrites it to
    /// `device:<device_id>` in its own serializer, and a device does not know
    /// its device id.
    ///
    /// The metadata keys come from the caller rather than being fixed here,
    /// because they belong to the update tool. NervesHub reads a device's
    /// metadata through a per-tool callback: fwup devices send `nerves_fw_*`,
    /// ESP-IDF devices `esp_idf_*`, RAUC devices `rauc_*`. Naming them here
    /// would have every tool claim to be fwup.
    pub fn join(&mut self, params: &[(&str, Value)]) -> Message {
        let reference = self.refs.next_ref();
        self.join_ref = Some(reference.clone());

        let mut payload = json!({
            "device_api_version": self.device_api_version,
            // Sent explicitly rather than left to be inferred from the metadata
            // keys. NervesHub prefers a declaration when it gets one, and
            // sniffing exists for devices that predate the registry.
            "update_tool": self.update_tool,
        });

        if let Some(object) = payload.as_object_mut() {
            for (key, value) in params {
                object.insert((*key).to_string(), value.clone());
            }
        }

        Message {
            join_ref: Some(reference.clone()),
            reference: Some(reference),
            topic: DEVICE_TOPIC.into(),
            event: event::JOIN.into(),
            payload,
        }
    }

    pub fn heartbeat(&mut self) -> Message {
        Message {
            join_ref: None,
            reference: Some(self.refs.next_ref()),
            topic: CONTROL_TOPIC.into(),
            event: event::HEARTBEAT.into(),
            payload: json!({}),
        }
    }

    /// Download/install progress.
    ///
    /// The percentage goes in `value`, which is the key NervesHub matches on --
    /// `device_message(session, event, %{"value" => percent})`. It reads
    /// nothing else, and an unmatched message falls through to a catch-all that
    /// records `unhandled_in` telemetry and drops it. So a payload without
    /// `value` is not a partial report, it is silence: the server showed no
    /// progress at all for this agent until this key was added.
    pub fn progress(&mut self, stage: Stage, percent: u8) -> Message {
        self.device_message(
            event::UPDATE_PROGRESS,
            json!({ "stage": stage, "value": percent }),
        )
    }

    /// Report an outcome that is not progress — `ignored` or `failed`.
    /// NervesHub shows these against the deployment, which is the difference
    /// between a device that declined and one that fell off the net.
    ///
    /// `rescheduled` goes through [`Link::rescheduled`] instead: the server
    /// needs a delay with it and refuses the message without one.
    pub fn status(&mut self, status: &str, reason: Option<&str>) -> Message {
        self.device_message(
            event::STATUS_UPDATE,
            json!({ "status": status, "reason": reason }),
        )
    }

    /// Ask NervesHub to hold off for `delay_ms`.
    ///
    /// `delay_for` is required, and not merely to be informative. The server
    /// matches `status_update("rescheduled", _, %{"delay_for" => delay_for}, _)`
    /// and computes `updates_blocked_until` from it — so without the key there
    /// is no matching clause, the call raises inside the device's channel
    /// process, and the deferral the device asked for never happens.
    pub fn rescheduled(&mut self, delay_ms: u64, reason: &str) -> Message {
        self.device_message(
            event::STATUS_UPDATE,
            json!({ "status": "rescheduled", "reason": reason, "delay_for": delay_ms }),
        )
    }

    /// The answer to a `scripts/run`, carrying the ref it came with.
    ///
    /// NervesHub drops the ref after 15 seconds, so a late answer is discarded
    /// server-side. The agent's own script timeout is what keeps this inside
    /// that window.
    pub fn script_result(&mut self, payload: Value) -> Message {
        self.device_message(event::SCRIPTS_RUN, payload)
    }

    pub fn rebooting(&mut self) -> Message {
        self.device_message(event::REBOOTING, json!({}))
    }

    pub fn firmware_validated(&mut self) -> Message {
        self.device_message(event::FIRMWARE_VALIDATED, json!({}))
    }

    fn device_message(&mut self, event: &str, payload: Value) -> Message {
        Message {
            join_ref: self.join_ref.clone(),
            reference: Some(self.refs.next_ref()),
            topic: DEVICE_TOPIC.into(),
            event: event.into(),
            payload,
        }
    }

    /// Fold an incoming frame into the link's state and say what it means.
    pub fn handle(&mut self, message: &Message) -> Action {
        if message.topic == EXTENSIONS_TOPIC {
            return self.handle_extensions(message);
        }

        match message.event.as_str() {
            event::REPLY if message.reference == self.join_ref => {
                let ok = message.payload.get("status").and_then(Value::as_str) == Some("ok");

                if ok {
                    self.joined = true;

                    let update = message
                        .reply_response()
                        .and_then(|response| {
                            serde_json::from_value::<UpdatePayload>(response.clone()).ok()
                        })
                        .filter(|update| update.update_available);

                    Action::Joined(Box::new(update))
                } else {
                    self.joined = false;

                    let reason = message
                        .reply_response()
                        .and_then(|r| r.get("reason"))
                        .and_then(|r| r.as_str())
                        .unwrap_or("unknown")
                        .to_string();

                    Action::JoinFailed(reason)
                }
            }

            event::UPDATE => match serde_json::from_value::<UpdatePayload>(message.payload.clone())
            {
                Ok(update) if update.update_available => Action::ApplyUpdate(Box::new(update)),
                // `update_available: false` is normal — it is how the server
                // says a deployment no longer applies to this device.
                _ => Action::None,
            },

            event::SCRIPTS_RUN => {
                let reference = message.payload.get("ref").and_then(Value::as_str);
                let text = message.payload.get("text").and_then(Value::as_str);

                match (reference, text) {
                    (Some(reference), Some(text)) => Action::RunScript {
                        reference: reference.to_string(),
                        text: text.to_string(),
                    },
                    // Nothing to answer to without a ref, and nothing to run
                    // without text. Both would be a server bug.
                    _ => Action::None,
                }
            }

            event::REBOOT => Action::Reboot,
            event::IDENTIFY => Action::Identify,

            event::CLOSE | event::ERROR => {
                self.joined = false;
                self.extensions.reset();
                Action::Reconnect
            }

            _ => Action::None,
        }
    }

    fn handle_extensions(&mut self, message: &Message) -> Action {
        if message.event == event::REPLY && message.reference == self.extensions_join_ref {
            let ok = message.payload.get("status").and_then(Value::as_str) == Some("ok");

            if !ok {
                // Not fatal. Extensions are meant to be refusable, and a device
                // that gives up its session over one would be trading an update
                // path for a health report.
                log::warn!("extensions join refused: {}", message.payload);
                return Action::None;
            }

            // The reply is the attach list: the subset the platform wants,
            // which is narrower than what was offered.
            let attach_list: Vec<String> = message
                .reply_response()
                .and_then(|response| serde_json::from_value(response.clone()).ok())
                .unwrap_or_default();

            return Action::ExtensionsAttached(self.extensions.attach(&attach_list));
        }

        match self.extensions.handle(&message.event, &message.payload) {
            Some(incoming) => Action::Extension(incoming),
            None => Action::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FirmwareMeta;

    /// What the fwup tool sends. Built here rather than imported so the test
    /// pins the wire names independently of the code that produces them.
    fn fwup_params() -> Vec<(&'static str, Value)> {
        vec![
            ("nerves_fw_uuid", json!("abc-123")),
            ("nerves_fw_version", json!("1.0.0")),
            ("nerves_fw_product", json!("gateway")),
        ]
    }

    /// NervesHub matches `%{"value" => percent}` and drops anything else into
    /// an `unhandled_in` counter. This assertion is the whole contract.
    #[test]
    fn progress_reports_the_percentage_under_value() {
        let mut link = Link::new("2.2.0", "fwup", &ExtensionsConfig::default());
        let message = link.progress(Stage::Updating, 42);

        assert_eq!(message.event, event::UPDATE_PROGRESS);
        assert_eq!(message.payload["value"], json!(42));
        assert_eq!(message.payload["stage"], json!("updating"));
    }

    /// Without `delay_for` the server has no matching clause for a reschedule,
    /// so the call raises rather than blocking updates for the delay asked for.
    #[test]
    fn a_reschedule_carries_the_delay() {
        let mut link = Link::new("2.2.0", "fwup", &ExtensionsConfig::default());
        let message = link.rescheduled(60_000, "busy");

        assert_eq!(message.event, event::STATUS_UPDATE);
        assert_eq!(message.payload["status"], json!("rescheduled"));
        assert_eq!(message.payload["delay_for"], json!(60_000));
        assert_eq!(message.payload["reason"], json!("busy"));
    }

    #[allow(dead_code)]
    fn firmware() -> FirmwareMeta {
        FirmwareMeta {
            uuid: Some("abc-123".into()),
            version: Some("1.0.0".into()),
            product: Some("gateway".into()),
            platform: Some("rpi4".into()),
            architecture: Some("arm".into()),
        }
    }

    #[test]
    fn the_join_topic_is_unqualified() {
        let mut link = Link::new("2.2.0", "fwup", &Default::default());
        let join = link.join(&fwup_params());

        assert_eq!(join.topic, "device");
        assert_eq!(join.payload["update_tool"], "fwup");
        assert_eq!(join.payload["nerves_fw_uuid"], "abc-123");
    }

    #[test]
    fn a_join_reply_carrying_an_update_is_acted_on() {
        let mut link = Link::new("2.2.0", "fwup", &Default::default());
        let join = link.join(&fwup_params());

        let reply = Message {
            join_ref: join.join_ref.clone(),
            reference: join.reference.clone(),
            topic: "device".into(),
            event: "phx_reply".into(),
            payload: json!({
                "status": "ok",
                "response": {
                    "update_available": true,
                    "firmware_url": "https://example.test/f.fw",
                    "firmware_meta": { "uuid": "def-456" }
                }
            }),
        };

        match link.handle(&reply) {
            Action::Joined(update) => {
                let update = update.expect("an update was offered at join");
                assert_eq!(update.firmware_meta.unwrap().uuid, Some("def-456".to_string()));
            }
            other => panic!("expected Joined, got {other:?}"),
        }

        assert!(link.joined());
    }

    #[test]
    fn an_update_saying_nothing_is_available_is_not_an_update() {
        let mut link = Link::new("2.2.0", "fwup", &Default::default());

        let message = Message {
            join_ref: None,
            reference: None,
            topic: "device".into(),
            event: "update".into(),
            payload: json!({ "update_available": false }),
        };

        assert_eq!(link.handle(&message), Action::None);
    }

    #[test]
    fn a_closed_channel_asks_for_a_reconnect() {
        let mut link = Link::new("2.2.0", "fwup", &Default::default());
        let _ = link.join(&fwup_params());

        let close = Message {
            join_ref: None,
            reference: None,
            topic: "device".into(),
            event: "phx_close".into(),
            payload: json!({}),
        };

        assert_eq!(link.handle(&close), Action::Reconnect);
        assert!(!link.joined());
    }
}
