//! The run loop.

use std::sync::Arc;

use tokio::sync::{mpsc, Mutex};

use crate::config::{Config, UpdateToolConfig};
use crate::error::Error;
use crate::extensions::{
    geo::Geo, health::Health, logging, network_identity::NetworkIdentity, Incoming,
};
use crate::ipc::policy::{decide_reboot, decide_update};
use crate::ipc::protocol::{
    ConnectionState, Event, Method, RebootDecision, Response, ResponseBody, UpdateDecision,
    UpdateStatus,
};
use crate::ipc::{Command, Ipc, Shared};
use crate::link::{Action, Link};
use crate::message::DEVICE_API_VERSION;
use crate::transport::Transport;
use crate::update_tool::UpdateTool;
use crate::{FirmwareMeta, Stage, UpdatePayload};
use serde_json::json;

/// The configured update tool.
///
/// An enum rather than `Box<dyn UpdateTool>` because installing is async and
/// the trait is not — the sandbox streams a download, and a real tool will want
/// to read a child process's output without blocking the loop. Async traits
/// would work; a three-armed enum is less machinery for the same thing.
pub enum Tool {
    #[cfg(feature = "sandbox")]
    Sandbox(crate::update_tool::sandbox::Sandbox),
    #[cfg(feature = "fwup")]
    Fwup(crate::update_tool::fwup::Fwup),
    #[cfg(feature = "rauc")]
    Rauc(crate::update_tool::rauc::Rauc),
}

impl Tool {
    pub fn build(config: &UpdateToolConfig) -> Result<Self, Error> {
        match config {
            #[cfg(feature = "sandbox")]
            UpdateToolConfig::Sandbox(c) => Ok(Tool::Sandbox(
                crate::update_tool::sandbox::Sandbox::new(c.clone())?,
            )),

            #[cfg(feature = "fwup")]
            UpdateToolConfig::Fwup(c) => {
                Ok(Tool::Fwup(crate::update_tool::fwup::Fwup::new(c.clone())?))
            }

            #[cfg(feature = "rauc")]
            UpdateToolConfig::Rauc(c) => {
                Ok(Tool::Rauc(crate::update_tool::rauc::Rauc::new(c.clone())?))
            }

            #[allow(unreachable_patterns)]
            other => Err(Error::Config(format!(
                "update tool {} is configured but not built into this binary",
                other.tool_name()
            ))),
        }
    }

    fn as_trait(&self) -> &dyn UpdateTool {
        match self {
            #[cfg(feature = "sandbox")]
            Tool::Sandbox(t) => t,
            #[cfg(feature = "fwup")]
            Tool::Fwup(t) => t,
            #[cfg(feature = "rauc")]
            Tool::Rauc(t) => t,
        }
    }

    pub fn current_firmware(&self) -> Result<FirmwareMeta, Error> {
        self.as_trait().current_firmware()
    }

    pub fn boot_state(&self) -> Result<crate::update_tool::BootState, Error> {
        self.as_trait().boot_state()
    }

    pub fn name(&self) -> &'static str {
        self.as_trait().name()
    }

    /// Everything this tool tells the server on join, metadata included.
    ///
    /// The key names belong to the tool, not to the agent: NervesHub reads a
    /// device's metadata through a per-tool callback, so a RAUC device sending
    /// `nerves_fw_uuid` would be read by the fwup reader and understood as
    /// nothing at all.
    pub fn join_params(&self, firmware: &FirmwareMeta) -> Vec<(&'static str, serde_json::Value)> {
        match self {
            #[cfg(feature = "sandbox")]
            Tool::Sandbox(_) => nerves_fw_params(firmware),

            #[cfg(feature = "fwup")]
            Tool::Fwup(t) => {
                let mut params = nerves_fw_params(firmware);

                // What decides whether this device may be sent a delta. A
                // device that stays quiet is assumed too old for one.
                params.push(("fwup_version", json!(t.version())));
                params
            }

            #[cfg(feature = "rauc")]
            Tool::Rauc(t) => vec![
                ("rauc_uuid", json!(firmware.uuid)),
                ("rauc_version", json!(firmware.version)),
                ("rauc_platform", json!(firmware.platform)),
                ("rauc_product", json!(firmware.product)),
                // Absent from slot status — the server fills it in from the
                // firmware it matches by uuid.
                ("rauc_architecture", json!(firmware.architecture)),
                ("rauc_compatible", json!(t.compatible())),
                ("rauc_tool_version", json!(t.version())),
            ],
        }
    }

    async fn install(
        &mut self,
        update: &UpdatePayload,
        client: &crate::http::Client,
        progress: impl FnMut(Stage, u8),
    ) -> Result<crate::update_tool::Installed, Error> {
        match self {
            #[cfg(feature = "sandbox")]
            Tool::Sandbox(t) => t.install_async(update, client, progress).await,

            #[cfg(feature = "fwup")]
            Tool::Fwup(t) => t.install_async(update, client, progress).await,

            #[cfg(feature = "rauc")]
            Tool::Rauc(t) => t.install_async(update, client, progress).await,
        }
    }

    fn mark_valid(&mut self) -> Result<(), Error> {
        match self {
            #[cfg(feature = "sandbox")]
            Tool::Sandbox(t) => t.mark_valid(),
            #[cfg(feature = "fwup")]
            Tool::Fwup(t) => t.mark_valid(),
            #[cfg(feature = "rauc")]
            Tool::Rauc(t) => t.mark_valid(),
        }
    }
}

pub struct Agent {
    config: Config,
    identifier: String,
    /// `None` only while an install owns it.
    ///
    /// `Tool::install` needs `&mut Tool`, and an install runs as a task so that
    /// it cannot hold up the session loop. A task cannot borrow from here, so
    /// it takes the tool and hands it back when it finishes. Everything that
    /// reaches for the tool has to cope with an install being in progress --
    /// which is a true statement about the device, not an inconvenience of the
    /// representation.
    tool: Option<Tool>,
    ipc: Ipc,
    commands: mpsc::Receiver<Command>,
    http: crate::http::Client,
    health: Health,
    geo: Geo,
    network_identity: NetworkIdentity,
    /// Finished support scripts, waiting to be sent back. Scripts run in their
    /// own tasks so a slow one cannot hold up heartbeats or an update.
    script_results: (
        mpsc::Sender<crate::scripts::Outcome>,
        mpsc::Receiver<crate::scripts::Outcome>,
    ),
    /// Log lines waiting to be shipped. `None` when the extension is off, in
    /// which case nothing is tailing anything.
    logs: Option<mpsc::Receiver<serde_json::Value>>,
    /// Log lines waiting for the next flush.
    ///
    /// Everything the tail produces waits here for up to a second and then
    /// goes out as one message. Outlives a session, like the tail that fills
    /// it: what a device writes while it is negotiating is what a reconnect is
    /// usually about, and the platform will not take it until the extension is
    /// attached.
    pending_logs: logging::Pending,
    shell: Option<ShellSession>,
    /// The install in flight, if there is one.
    install: Option<InstallSession>,
    /// What the device was running when the tool was last available.
    ///
    /// An install owns the tool, so `current_firmware` cannot be asked during
    /// one. A reconnect mid-install still has to say what it is running, and
    /// the answer has not changed -- the new firmware is going into the other
    /// slot and is not running yet.
    firmware: Option<FirmwareMeta>,
    /// The join the tool last asked for, for the same reason as `firmware`.
    ///
    /// `join_params` needs the tool for more than metadata -- fwup adds its own
    /// version, RAUC adds its compatible string -- so a reconnect during an
    /// install cannot build one. Without this cached, such a reconnect could
    /// not join at all, and since the tool only comes back through a live
    /// session, it could never join again: the agent would back off and retry
    /// forever with the firmware sitting installed and unreported.
    join_params: Option<Vec<(&'static str, serde_json::Value)>>,
    /// Which tool this is. Static for the life of the process, and needed on
    /// every join, including the ones an install is holding the tool through.
    tool_name: &'static str,
}

/// A running shell and the output still to be sent from it.
struct ShellSession {
    shell: crate::extensions::local_shell::Shell,
    output: mpsc::Receiver<String>,
}

/// An install running as a task, and the two things the session loop needs
/// from it: progress as it happens, and the tool back when it is done.
struct InstallSession {
    firmware: FirmwareMeta,
    progress: mpsc::UnboundedReceiver<(Stage, u8)>,
    #[allow(clippy::type_complexity)]
    task: tokio::task::JoinHandle<(Tool, Result<crate::update_tool::Installed, Error>)>,
}

impl Agent {
    pub async fn new(config: Config, identifier: String, tool: Tool) -> Result<Self, Error> {
        let shared = Arc::new(Mutex::new(Shared {
            identifier: identifier.clone(),
            update_tool: tool.name().to_string(),
            connection: Some(ConnectionState::Disconnected),
            // Populated before anything connects. What the device is running is
            // a fact about the device, not about the session, and an
            // application asking while the network is down should be told what
            // it is running rather than `null`.
            firmware: tool.current_firmware().ok(),
            // Read from the device, not remembered from an install. After a
            // reboot into new firmware the agent has installed nothing this
            // run, and it still owes a validation — an agent that only tracked
            // its own installs would report `false` on exactly the boot where
            // it matters.
            pending_validation: matches!(
                tool.boot_state(),
                Ok(crate::update_tool::BootState::PendingValidation)
            ),
            ..Default::default()
        }));

        let (ipc, commands) = Ipc::bind(&config.ipc.socket, config.ipc.mode, shared).await?;

        // The firmware download follows whatever TLS decision the socket made.
        // Two different answers would be worse than one wrong one: a device
        // that trusts the server but not its firmware host is not more secure,
        // it just fails somewhere less obvious.
        let http = crate::http::Client::new(&config)?;

        // The log tail starts once, not per session. A reconnect should not
        // restart journalctl, and a source that dies is a problem worth seeing
        // rather than one papered over by the next reconnect.
        let logs = if config.extensions.logging.enabled {
            Some(logging::spawn(&config.extensions.logging)?)
        } else {
            None
        };

        let pending_lines = logging::pending_lines(config.extensions.logging.max_lines_per_batch);

        let geo = Geo::new(config.extensions.geo.source.clone());
        let network_identity = NetworkIdentity::new(&config.extensions.network_identity);

        let firmware = tool.current_firmware().ok();
        let join_params = firmware.as_ref().map(|f| tool.join_params(f));
        let tool_name = tool.name();

        Ok(Self {
            config,
            identifier,
            tool: Some(tool),
            ipc,
            commands,
            http,
            health: Health::new(),
            geo,
            network_identity,
            script_results: mpsc::channel(8),
            logs,
            pending_logs: logging::Pending::new(pending_lines),
            shell: None,
            install: None,
            firmware,
            join_params,
            tool_name,
        })
    }

    /// Connect, and keep reconnecting.
    pub async fn run(&mut self) -> Result<(), Error> {
        let mut attempt = 0usize;

        loop {
            self.set_connection(ConnectionState::Connecting).await;

            match self.session().await {
                Ok(()) => {
                    log::info!("session ended cleanly");
                    attempt = 0;
                }
                Err(e) => {
                    log::warn!("session ended: {e}");
                    attempt += 1;
                }
            }

            self.set_connection(ConnectionState::Disconnected).await;

            let backoff = self
                .config
                .server
                .reconnect_backoff_secs
                .get(attempt.saturating_sub(1))
                .copied()
                .or_else(|| self.config.server.reconnect_backoff_secs.last().copied())
                .unwrap_or(5);

            log::info!("reconnecting in {backoff}s");
            tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
        }
    }

    async fn session(&mut self) -> Result<(), Error> {
        let mut transport = Transport::connect(&self.config, &self.identifier).await?;
        let mut link = Link::new(DEVICE_API_VERSION, self.tool_name, &self.config.extensions);

        // Cached rather than asked for, when an install has the tool. What the
        // device is running has not changed: the new firmware is going into the
        // other slot and is not running yet.
        //
        // Nothing on this path may reach for the tool. A session that could not
        // start without it would be unable to start during an install, and the
        // tool only comes back through a session -- so the first mid-install
        // disconnect would strand the agent for good.
        let (firmware, join_params) = match self.tool.as_ref() {
            Some(tool) => {
                let firmware = tool.current_firmware()?;
                let join_params = tool.join_params(&firmware);

                self.firmware = Some(firmware.clone());
                self.join_params = Some(join_params.clone());

                (firmware, join_params)
            }

            None => {
                let firmware = self.firmware.clone();
                let join_params = self.join_params.clone();

                firmware.zip(join_params).ok_or_else(|| Error::UpdateTool {
                    tool: "agent",
                    message: "an install holds the tool and no join was cached".into(),
                })?
            }
        };

        self.ipc
            .update_shared(|shared| shared.firmware = Some(firmware.clone()))
            .await;

        transport.send(&link.join(&join_params)).await?;

        let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(
            self.config.server.heartbeat_interval_secs,
        ));
        heartbeat.tick().await;

        // Ticks whether or not logging is on. A timer the session already has
        // to hold costs nothing next to a second branch through every join.
        let mut log_flush = tokio::time::interval(logging::FLUSH_INTERVAL);
        log_flush.tick().await;

        let max_lines_per_batch = self.config.extensions.logging.max_lines_per_batch;

        loop {
            tokio::select! {
                incoming = transport.recv() => {
                    let Some(message) = incoming? else {
                        return Ok(());
                    };

                    match link.handle(&message) {
                        Action::Joined(update) => {
                            log::info!(
                                "joined as {} running {}",
                                self.identifier,
                                firmware.uuid_or_unknown()
                            );
                            self.set_connection(ConnectionState::Connected).await;

                            if link.has_extensions() {
                                transport.send(&link.join_extensions()).await?;
                            }

                            if let Some(update) = *update {
                                if self.install.is_some() {
                                    log::warn!(
                                        "an update is already installing; ignoring the one offered at join"
                                    );
                                } else {
                                    self.on_update(&mut link, &mut transport, update).await?;
                                }
                            }
                        }

                        Action::ExtensionsAttached(confirmations) => {
                            for (event, payload) in confirmations {
                                log::info!("extension attached: {event}");
                                transport.send(&link.extension(&event, payload)).await?;
                            }

                            // Whatever the tail wrote while there was nowhere
                            // to send it goes out on the next flush, which is
                            // at most a second away.
                            if link.extension_attached(crate::extensions::LOGGING) {
                                if !self.pending_logs.is_empty() {
                                    log::info!(
                                        "logging attached; {} lines are waiting",
                                        self.pending_logs.len()
                                    );
                                }
                            } else if self.logs.is_some() {
                                // Either an operator turned logging off for
                                // this product, or the platform does not have
                                // the version of the extension this agent
                                // offers. Worth saying out loud: the tail is
                                // still running and filling a buffer nothing
                                // will drain, which from the outside looks
                                // exactly like logs that go missing.
                                log::warn!(
                                    "the platform did not attach logging; lines are being \
                                     buffered and dropped rather than sent"
                                );
                            }
                        }

                        Action::Extension(incoming) => {
                            self.on_extension(&mut link, &mut transport, incoming).await?;
                        }

                        Action::JoinFailed(reason) => {
                            // The server looked at what we sent and refused it
                            // -- usually the metadata, or a product that does
                            // not accept this tool. `run` still reconnects,
                            // because the fix is often on the server and a
                            // device that gave up would need a site visit to
                            // pick the change up. It is logged at each attempt
                            // so it does not read as a network problem.
                            return Err(Error::Connection(format!("join refused: {reason}")));
                        }

                        Action::ApplyUpdate(update) => {
                            // One at a time: two installs would put two writers
                            // on the same slot.
                            if self.install.is_some() {
                                log::warn!(
                                    "an update is already installing; ignoring the new one"
                                );
                            } else {
                                self.on_update(&mut link, &mut transport, *update).await?;
                            }
                        }

                        Action::RunScript { reference, text } => {
                            self.spawn_script(reference, text, &firmware);
                        }

                        Action::Reboot => {
                            // Rebooting now would leave a half-written slot.
                            if self.install.is_some() {
                                log::warn!(
                                    "asked to reboot mid-install; ignoring until it finishes"
                                );
                            } else {
                                let _ = transport.send(&link.rebooting()).await;
                                self.reboot("an operator asked").await;
                            }
                        }

                        Action::Identify => {
                            // Nothing to blink here, so it goes to whoever can.
                            let _ = self.ipc.ask(Method::Identify, 5).await;
                        }

                        Action::Reconnect => return Ok(()),
                        Action::None => {}
                    }
                }

                _ = heartbeat.tick() => {
                    transport.send(&link.heartbeat()).await?;
                }

                // An install reports through the session loop like anything
                // else, so a slow one cannot starve heartbeats, logs, the
                // shell or the IPC.
                // One arm, not two: `select!` holds every arm's future at once,
                // and two of them borrowing `self.install` is two mutable
                // borrows of the same field.
                event = next_install_event(&mut self.install) => match event {
                    InstallEvent::Progress { uuid, stage, percent } => {
                        forward_progress(
                            &mut link, &mut transport, &self.ipc, &uuid, stage, percent,
                        )
                        .await?;
                    }

                    InstallEvent::Finished(outcome) => match *outcome {
                        Ok((tool, result)) => {
                            self.on_install_finished(&mut link, &mut transport, tool, result)
                                .await?;
                        }

                        // The task itself failed -- panicked, or was aborted.
                        // The tool went with it, so there is nothing to put
                        // back and no way to say what this device runs. Ending
                        // the session starts again from a known state.
                        Err(e) => {
                            self.install = None;
                            return Err(Error::UpdateTool {
                                tool: "agent",
                                message: format!("the install task did not finish: {e}"),
                            });
                        }
                    },
                },

                line = recv_log(&mut self.logs) => {
                    // Buffered, never sent from here. NervesHub limits how
                    // often a device may send rather than how much it may say,
                    // so a line at a time is the shape that loses lines; the
                    // flush below turns a second of them into one message.
                    self.pending_logs.push(line);
                }

                _ = log_flush.tick() => {
                    // Only once the platform has attached logging: sending
                    // before the attach would be answering a question nobody
                    // asked, and the lines keep until it does.
                    if link.extension_attached(crate::extensions::LOGGING)
                        && !self.pending_logs.is_empty()
                    {
                        let batch = self.pending_logs.batch(max_lines_per_batch);
                        let sent = batch.len();

                        // Dropped only once the message has gone, so a socket
                        // that dies on this send leaves the lines for the next
                        // session rather than eating them.
                        transport
                            .send(&link.extension("logging:send", logging::batch_payload(batch)))
                            .await?;

                        self.pending_logs.sent(sent);
                    }
                }

                Some(outcome) = self.script_results.1.recv() => {
                    transport
                        .send(&link.script_result(outcome.payload()))
                        .await?;
                }

                output = recv_shell(&mut self.shell) => {
                    transport
                        .send(&link.extension("local_shell:shell_output", json!({ "data": output })))
                        .await?;
                }

                command = self.commands.recv() => {
                    match command {
                        Some(Command::MarkValid(reply)) => {
                            let result = match self.tool.as_mut() {
                                Some(tool) => tool.mark_valid().map_err(|e| e.to_string()),
                                // Marking a boot good while an install is
                                // writing the other slot is not something to
                                // guess at.
                                None => Err("an install is in progress".to_string()),
                            };

                            if result.is_ok() {
                                let _ = transport.send(&link.firmware_validated()).await;
                                self.ipc.update_shared(|s| s.pending_validation = false).await;
                            }

                            let _ = reply.send(result);
                        }

                        Some(Command::Reboot { reason }) => {
                            let _ = transport.send(&link.rebooting()).await;
                            self.reboot(reason.as_deref().unwrap_or("an application asked")).await;
                        }

                        None => return Ok(()),
                    }
                }
            }
        }
    }

    /// Run a support script without blocking the loop.
    ///
    /// Its own task, because a script is allowed several seconds and the loop
    /// owes the server a heartbeat well inside that. The result comes back
    /// through a channel and is sent from the loop, so nothing else needs a
    /// handle on the transport.
    fn spawn_script(&self, reference: String, text: String, firmware: &FirmwareMeta) {
        let config = self.config.scripts.clone();
        let results = self.script_results.0.clone();

        // Enough for a script to know which device it woke up on without having
        // to ask the agent for it.
        let env = vec![
            (
                "NERVES_HUB_DEVICE_IDENTIFIER".to_string(),
                self.identifier.clone(),
            ),
            (
                "NERVES_HUB_FIRMWARE_UUID".to_string(),
                firmware.uuid.clone().unwrap_or_default(),
            ),
            (
                "NERVES_HUB_FIRMWARE_VERSION".to_string(),
                firmware.version.clone().unwrap_or_default(),
            ),
        ];

        tokio::spawn(async move {
            let outcome = crate::scripts::run(&config, reference, &text, &env).await;

            let _ = results.send(outcome).await;
        });
    }

    /// Answer whatever an extension was asked for.
    ///
    /// Every arm reports its own failure and carries on. An extension that
    /// cannot answer — no `/proc`, no route to the geo service, a shell that
    /// will not start — must not take down a session whose real job is
    /// firmware.
    async fn on_extension(
        &mut self,
        link: &mut Link,
        transport: &mut Transport,
        incoming: Incoming,
    ) -> Result<(), Error> {
        match incoming {
            Incoming::HealthCheck => {
                let report = self.health.report();
                transport
                    .send(&link.extension("health:report", report))
                    .await?;
            }

            Incoming::IdentityRequest => {
                let report = self.network_identity.report().await;

                transport
                    .send(&link.extension("network_identity:report", report))
                    .await?;
            }

            Incoming::LocationRequest => match self.geo.locate(&self.http).await {
                Ok(location) => {
                    transport
                        .send(&link.extension("geo:location:update", location))
                        .await?;
                }
                // Nothing is sent on failure. A location the agent could not
                // establish is not a location at the origin, and NervesHub keeps
                // the last one it was told rather than showing a gap.
                Err(e) => log::warn!("geo: {e}"),
            },

            Incoming::ShellRequested | Incoming::ShellInput(_) | Incoming::WindowSize { .. } => {
                serve_shell(
                    &mut self.shell,
                    &self.config.extensions.local_shell,
                    link,
                    transport,
                    incoming,
                )
                .await?;
            }
        }

        Ok(())
    }

    async fn on_update(
        &mut self,
        link: &mut Link,
        transport: &mut Transport,
        update: UpdatePayload,
    ) -> Result<(), Error> {
        let Some(meta) = update.firmware_meta.clone() else {
            log::warn!("update with no metadata, ignoring");
            return Ok(());
        };

        let had_controller = self.ipc.has_controller().await;

        let answer = if self.config.updates.policy == crate::config::UpdatePolicy::Ask {
            self.ipc
                .ask(
                    Method::UpdateAvailable {
                        firmware: meta.clone(),
                        size: update.size,
                        deployment_id: update.deployment_id,
                    },
                    self.config.updates.ask_timeout_secs,
                )
                .await
                .and_then(update_decision)
        } else {
            None
        };

        let decided = decide_update(&self.config.updates, had_controller, answer);

        log::info!(
            "update {} -> {:?} (decided by {:?})",
            meta.uuid_or_unknown(),
            decided.decision,
            decided.source
        );

        match decided.decision {
            UpdateDecision::Ignore { reason } => {
                transport
                    .send(&link.status("ignored", Some(&reason)))
                    .await?;
                return Ok(());
            }

            UpdateDecision::Reschedule { delay_ms, reason } => {
                // Reported rather than just slept on, and reported *with* the
                // delay: the server sets `updates_blocked_until` from it, which
                // is what makes a deferral visible instead of looking like a
                // device gone quiet.
                transport.send(&link.rescheduled(delay_ms, &reason)).await?;

                log::info!("update rescheduled for {delay_ms}ms: {reason}");
                return Ok(());
            }

            UpdateDecision::Apply => {}
        }

        self.ipc.broadcast(Event::UpdateProgress {
            stage: Stage::Downloading,
            percent: 0,
        });

        // NervesHub is told an update started, and later that it finished. It
        // has a clause for each -- `started` carries its own telemetry, and the
        // inflight update stays open until something closes it.
        //
        // Best effort, like every other report during an install. The install
        // is going to happen whether or not the socket is there to hear about
        // it, and a device that refused to update because it could not say so
        // would be worse off than one that updates quietly.
        let _ = transport.send(&link.status("started", None)).await;

        // The install runs as a task.
        //
        // It used to run in a loop of its own here, which meant the session
        // loop stopped for the duration and every arm it serves had to be
        // duplicated -- badly, as it turned out. A task cannot borrow from
        // `self`, so it takes the tool and hands it back when it finishes, and
        // the session loop carries on exactly as it does when idle.
        let Some(mut tool) = self.tool.take() else {
            // Guarded by the caller, so this is a bug rather than a race.
            log::error!("asked to install while an install is already running");
            return Ok(());
        };

        // The installer's callback is synchronous and everything that reports
        // progress is async, so the callback hands each report out through an
        // unbounded channel -- whose `send` is not a future -- and the session
        // loop forwards it while the install is still running.
        //
        // An earlier version pushed into a `Mutex<Vec>` and drained it after
        // the install returned. That compiles and reports nothing useful: every
        // percentage arrives at once, after the thing it describes has already
        // finished.
        let (reports, progress) = mpsc::unbounded_channel::<(Stage, u8)>();
        let http = self.http.clone();

        let task = tokio::spawn(async move {
            let result = tool
                .install(&update, &http, move |stage, percent| {
                    let _ = reports.send((stage, percent));
                })
                .await;

            // The tool goes back whatever happened. Losing it on a failed
            // install would leave the agent unable to say what it is running.
            (tool, result)
        });

        self.install = Some(InstallSession {
            firmware: meta,
            progress,
            task,
        });

        Ok(())
    }

    /// An install has finished. Put the tool back and act on the outcome.
    async fn on_install_finished(
        &mut self,
        link: &mut Link,
        transport: &mut Transport,
        tool: Tool,
        result: Result<crate::update_tool::Installed, Error>,
    ) -> Result<(), Error> {
        self.tool = Some(tool);
        self.install = None;

        match result {
            Ok(installed) => {
                log::info!(
                    "installed {} ({} bytes transferred)",
                    installed.firmware.uuid_or_unknown(),
                    installed.bytes_transferred
                );

                self.ipc.broadcast(Event::UpdateInstalled {
                    firmware: installed.firmware.clone(),
                });

                self.ipc
                    .update_shared(|s| {
                        s.update = None;
                        s.pending_validation = true;
                    })
                    .await;

                // Before the reboot, not after: with `reboot.policy =
                // "immediate"` `settle_reboot` does not come back, and a
                // completion sent afterwards would never leave the device.
                //
                // Without this NervesHub sees an update start, watches progress
                // reach 100, and then nothing -- the inflight update stays open
                // until the device reconnects on the new firmware, which is a
                // reboot away and reads as a stall in the meantime. The failure
                // path has always reported itself; only success was silent.
                let _ = transport.send(&link.status("completed", None)).await;

                if installed.reboot_required {
                    self.settle_reboot(link, transport, &installed.firmware)
                        .await?;
                }
            }

            Err(e) => {
                log::error!("install failed: {e}");

                // Best effort: the local bookkeeping below matters more than a
                // report that cannot be delivered, and the reconnect reports
                // whatever the device is actually running.
                let _ = transport
                    .send(&link.status("failed", Some(&e.to_string())))
                    .await;

                self.ipc.broadcast(Event::UpdateFailed {
                    reason: e.to_string(),
                });

                self.ipc.update_shared(|s| s.update = None).await;
            }
        }

        Ok(())
    }

    async fn settle_reboot(
        &mut self,
        link: &mut Link,
        transport: &mut Transport,
        firmware: &FirmwareMeta,
    ) -> Result<(), Error> {
        let had_controller = self.ipc.has_controller().await;

        let answer = if self.config.reboot.policy == crate::config::RebootPolicy::Ask {
            self.ipc
                .ask(
                    Method::RebootRequest {
                        firmware: firmware.clone(),
                    },
                    self.config.reboot.ask_timeout_secs.unwrap_or(30),
                )
                .await
                .and_then(reboot_decision)
        } else {
            None
        };

        // Deferral is not tracked across restarts yet, so the elapsed count
        // always starts at zero. That makes `max_defer_secs` a per-boot cap
        // rather than a total one — fine while nothing persists, wrong once
        // deferrals survive a restart.
        let decided = decide_reboot(&self.config.reboot, had_controller, answer, 0);

        match decided.decision {
            RebootDecision::Reboot => {
                // Best effort, like the operator-triggered reboot. The decision
                // is made and the firmware is already in the slot; a socket
                // that has gone away is not a reason to leave the device on the
                // old one until someone power-cycles it.
                let _ = transport.send(&link.rebooting()).await;
                self.reboot(&format!("{:?}", decided.source)).await;
            }

            RebootDecision::Defer { delay_ms, reason } => {
                // `u64::MAX` is how `reboot.policy = "never"` says there is no
                // deadline. Printing it as a number of milliseconds is true and
                // useless — it reads as a bug in the deferral arithmetic.
                let indefinite = delay_ms == u64::MAX;

                if indefinite {
                    log::info!("reboot deferred indefinitely: {reason}");
                } else {
                    log::info!("reboot deferred for {delay_ms}ms: {reason}");
                }

                self.ipc.broadcast(Event::RebootPending {
                    deferred_until_ms: (!indefinite).then_some(delay_ms),
                });
            }
        }

        Ok(())
    }

    /// What passes for rebooting.
    ///
    /// In a container there is nothing to reboot into, so the agent says what it
    /// would have done and carries on. That is a lie in production and the
    /// right behaviour in development, which is exactly why it is gated on the
    /// sandbox rather than on a flag someone could set anywhere.
    async fn reboot(&self, reason: &str) {
        if !self.config.update_tool.can_touch_the_system() {
            log::warn!("would reboot now ({reason}) — sandbox tool, staying up");
            return;
        }

        log::warn!("rebooting: {reason}");

        let command = self.config.reboot.command.clone();

        match tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .status()
            .await
        {
            // A zero exit is normal, not a failure. `reboot` under systemd asks
            // the init system to shut down and returns immediately, so the
            // command succeeds well before anything stops running — treating
            // that as an error logged a lie on every successful reboot.
            Ok(status) if status.success() => {
                log::info!("{command} accepted; waiting for the system to go down")
            }
            Ok(status) => log::error!("{command} exited with {status}"),
            Err(e) => log::error!("running {command}: {e}"),
        }
    }

    async fn set_connection(&self, state: ConnectionState) {
        self.ipc.update_shared(|s| s.connection = Some(state)).await;
        self.ipc.broadcast(Event::Connection { state });
    }
}

/// Send one progress report everywhere it has to go.
///
/// Three places, and they are not interchangeable: NervesHub drives the
/// deployment view, IPC subscribers are watching an update they may have
/// approved, and the shared state answers a `status` from an application that
/// connected halfway through.
async fn forward_progress(
    link: &mut Link,
    transport: &mut Transport,
    ipc: &Ipc,
    uuid: &str,
    stage: Stage,
    percent: u8,
) -> Result<(), Error> {
    transport.send(&link.progress(stage, percent)).await?;

    ipc.broadcast(Event::UpdateProgress { stage, percent });

    ipc.update_shared(|s| {
        s.update = Some(UpdateStatus {
            uuid: uuid.to_string(),
            stage,
            percent,
        })
    })
    .await;

    Ok(())
}

/// The next log line, or never when logging is off.
///
/// A branch that never completes keeps the select shape the same whether or not
/// the extension is enabled, rather than duplicating the loop.
/// Something an install has to say.
enum InstallEvent {
    Progress {
        uuid: String,
        stage: Stage,
        percent: u8,
    },
    /// Boxed because the outcome carries the tool back, which makes it much
    /// larger than a progress report -- and this enum is returned on every
    /// percentage point.
    #[allow(clippy::type_complexity)]
    Finished(
        Box<Result<(Tool, Result<crate::update_tool::Installed, Error>), tokio::task::JoinError>>,
    ),
}

/// The next thing a running install has to say, or park when there is none.
///
/// Progress is biased ahead of completion so the reports queued in the same
/// tick as the install finishing are forwarded rather than skipped. The
/// progress channel closes when the task's closure is dropped, which disables
/// that arm and lets completion through.
async fn next_install_event(install: &mut Option<InstallSession>) -> InstallEvent {
    let Some(session) = install else {
        return std::future::pending().await;
    };

    let uuid = session.firmware.uuid_or_unknown().to_string();

    tokio::select! {
        biased;

        Some((stage, percent)) = session.progress.recv() => {
            InstallEvent::Progress { uuid, stage, percent }
        }

        finished = &mut session.task => InstallEvent::Finished(Box::new(finished)),
    }
}

async fn recv_log(logs: &mut Option<mpsc::Receiver<serde_json::Value>>) -> serde_json::Value {
    match logs {
        Some(rx) => match rx.recv().await {
            Some(line) => line,
            // The tail died. Park rather than spin: the alternative is a select
            // arm that completes instantly forever and starves everything else.
            None => std::future::pending().await,
        },
        None => std::future::pending().await,
    }
}

/// The shell half of `on_extension`, taking only the pieces it needs.
///
/// A free function rather than a method because an install borrows
/// `&mut self.tool` for its whole duration, which rules out calling any
/// `&mut self` method while one runs. The install loop serves the shell by
/// passing the fields it destructured, and a shell is wanted exactly when an
/// update is misbehaving.
async fn serve_shell(
    shell: &mut Option<ShellSession>,
    config: &crate::config::LocalShellConfig,
    link: &mut Link,
    transport: &mut Transport,
    incoming: Incoming,
) -> Result<(), Error> {
    match incoming {
        Incoming::ShellRequested => {
            // Dropping any previous session kills it first. The platform asks
            // again when a second user opens the tab, and two shells writing
            // into one stream is unreadable for both.
            *shell = None;

            match crate::extensions::local_shell::Shell::spawn(config) {
                Ok((spawned, output)) => {
                    *shell = Some(ShellSession {
                        shell: spawned,
                        output,
                    })
                }
                Err(e) => {
                    log::error!("{e}");

                    transport
                        .send(&link.extension(
                            "local_shell:shell_output",
                            json!({ "data": format!("\r\nagent could not start a shell: {e}\r\n") }),
                        ))
                        .await?;
                }
            }
        }

        Incoming::ShellInput(data) => {
            if let Some(session) = shell {
                if let Err(e) = session.shell.input(data).await {
                    log::warn!("{e}");
                }
            }
        }

        Incoming::WindowSize { rows, cols } => {
            if let Some(session) = shell {
                if let Err(e) = session.shell.resize(rows, cols) {
                    log::warn!("{e}");
                }
            }
        }

        // The reading extensions need `&mut self` and stay in `on_extension`;
        // nothing routes them here.
        _ => {}
    }

    Ok(())
}

async fn recv_shell(shell: &mut Option<ShellSession>) -> String {
    match shell {
        Some(session) => match session.output.recv().await {
            Some(output) => output,
            None => {
                // The shell exited. Drop it so the next request starts a fresh
                // one, and park.
                *shell = None;
                std::future::pending().await
            }
        },
        None => std::future::pending().await,
    }
}

/// The keys the fwup reader on the server understands.
#[cfg(any(feature = "fwup", feature = "sandbox"))]
fn nerves_fw_params(firmware: &FirmwareMeta) -> Vec<(&'static str, serde_json::Value)> {
    vec![
        ("nerves_fw_uuid", json!(firmware.uuid)),
        ("nerves_fw_version", json!(firmware.version)),
        ("nerves_fw_product", json!(firmware.product)),
        ("nerves_fw_platform", json!(firmware.platform)),
        ("nerves_fw_architecture", json!(firmware.architecture)),
    ]
}

fn update_decision(response: Response) -> Option<UpdateDecision> {
    match response {
        Response::Ok {
            result: ResponseBody::Update(decision),
        } => Some(decision),
        // An error or an unexpected body is not an answer. Treated as silence
        // so it lands on the configured fallback rather than inventing one.
        _ => None,
    }
}

fn reboot_decision(response: Response) -> Option<RebootDecision> {
    match response {
        Response::Ok {
            result: ResponseBody::Reboot(decision),
        } => Some(decision),
        _ => None,
    }
}
