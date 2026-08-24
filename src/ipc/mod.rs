//! The Unix socket applications connect to.
//!
//! [`protocol`] is the wire format, [`policy`] turns an answer into an action,
//! and this is the server in between: a listener, a set of connections, and the
//! rule that at most one of them is the controller.

pub mod policy;
pub mod protocol;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::error::Error;
use crate::ipc::protocol::{
    ConnectionState, ErrorBody, Event, Frame, Method, Response, ResponseBody, Role, Status,
    UpdateStatus, API_VERSION,
};
use crate::FirmwareMeta;

/// What the agent knows and an application may ask about.
///
/// Behind a mutex rather than passed through channels because every field is a
/// last-known value that several connections read and one writer updates —
/// exactly what a lock is for, and a channel per reader would be a cache with
/// no invalidation.
#[derive(Debug, Default)]
pub struct Shared {
    pub connection: Option<ConnectionState>,
    pub identifier: String,
    pub update_tool: String,
    pub firmware: Option<FirmwareMeta>,
    pub update: Option<UpdateStatus>,
    pub pending_validation: bool,
}

/// Something an application asked the agent to do that the agent alone can do.
#[derive(Debug)]
pub enum Command {
    MarkValid(oneshot::Sender<Result<(), String>>),
    Reboot { reason: Option<String> },
}

type Pending = (Method, oneshot::Sender<Response>);

#[derive(Clone)]
pub struct Ipc {
    shared: Arc<Mutex<Shared>>,
    /// `None` when no controller is connected. Replaced, not mutated, so a
    /// controller that disconnects mid-question drops its receiver and every
    /// outstanding `ask` resolves immediately instead of waiting out its
    /// deadline.
    controller: Arc<Mutex<Option<mpsc::Sender<Pending>>>>,
    events: tokio::sync::broadcast::Sender<Event>,
    commands: mpsc::Sender<Command>,
}

impl Ipc {
    /// Bind the socket and start accepting.
    ///
    /// A stale socket file from a killed agent is removed first: refusing to
    /// start until someone deletes a file is a bad trade for a device in the
    /// field, and the socket is not a lock.
    pub async fn bind(
        path: &Path,
        mode: u32,
        shared: Arc<Mutex<Shared>>,
    ) -> Result<(Self, mpsc::Receiver<Command>), Error> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }

        if path.exists() {
            tokio::fs::remove_file(path).await.map_err(|e| {
                Error::Ipc(format!("removing stale socket {}: {e}", path.display()))
            })?;
        }

        let listener = UnixListener::bind(path)
            .map_err(|e| Error::Ipc(format!("binding {}: {e}", path.display())))?;

        set_mode(path, mode)?;

        let (events, _) = tokio::sync::broadcast::channel(64);
        let (commands, command_rx) = mpsc::channel(8);

        let ipc = Self {
            shared,
            controller: Arc::new(Mutex::new(None)),
            events,
            commands,
        };

        let accepting = ipc.clone();
        let socket_path: PathBuf = path.to_path_buf();

        tokio::spawn(async move {
            log::info!("ipc: listening on {}", socket_path.display());

            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let ipc = accepting.clone();
                        tokio::spawn(async move {
                            if let Err(e) = ipc.serve(stream).await {
                                log::debug!("ipc: connection ended: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        log::error!("ipc: accept failed: {e}");
                        return;
                    }
                }
            }
        });

        Ok((ipc, command_rx))
    }

    /// Ask the controller a question and wait for its answer.
    ///
    /// `None` means there is no controller or it did not answer in time. The
    /// caller turns that into an action via [`policy`] — the two cases are told
    /// apart by [`Ipc::has_controller`] because they are configured separately.
    pub async fn ask(&self, method: Method, timeout_secs: u64) -> Option<Response> {
        let sender = self.controller.lock().await.clone()?;
        let (tx, rx) = oneshot::channel();

        if sender.send((method, tx)).await.is_err() {
            // The controller disconnected between being looked up and being
            // written to. Not a timeout; there is simply nobody there.
            return None;
        }

        match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), rx).await {
            Ok(Ok(response)) => Some(response),
            // Elapsed, or the connection dropped while we waited. Both mean no
            // answer is coming, and the caller's fallback is the same.
            _ => None,
        }
    }

    pub async fn has_controller(&self) -> bool {
        self.controller.lock().await.is_some()
    }

    /// Send an event to every subscriber. Nothing listening is not an error.
    pub fn broadcast(&self, event: Event) {
        let _ = self.events.send(event);
    }

    pub async fn update_shared(&self, f: impl FnOnce(&mut Shared)) {
        f(&mut *self.shared.lock().await);
    }

    async fn serve(&self, stream: UnixStream) -> Result<(), Error> {
        let (read_half, mut write_half) = stream.into_split();
        let mut lines = BufReader::new(read_half).lines();

        // Nothing is accepted before `hello`, so the role is known before the
        // connection can do anything that depends on it.
        let first = lines
            .next_line()
            .await?
            .ok_or_else(|| Error::Ipc("connection closed before hello".into()))?;

        let (name, role, subscriptions) = match serde_json::from_str::<Frame>(&first) {
            Ok(Frame::Hello {
                name,
                role,
                api,
                subscribe,
            }) => {
                if api != API_VERSION {
                    let refusal = Frame::Response {
                        id: "hello".into(),
                        result: Response::Err {
                            error: ErrorBody {
                                code: "api_mismatch".into(),
                                message: format!(
                                    "agent speaks api {API_VERSION}, you asked for {api}"
                                ),
                            },
                        },
                    };

                    write_line(&mut write_half, &refusal).await?;
                    return Ok(());
                }

                (name, role, subscribe)
            }
            _ => return Err(Error::Ipc("first frame was not a hello".into())),
        };

        // Claim the controller slot, or refuse. Two processes each believing
        // they decide whether the device updates is a bug that should surface
        // here rather than as a fleet that updated when it was told not to.
        let mut controller_rx = None;

        if role == Role::Controller {
            let mut slot = self.controller.lock().await;

            if slot.is_some() {
                let refusal = Frame::Response {
                    id: "hello".into(),
                    result: Response::Err {
                        error: ErrorBody {
                            code: "controller_taken".into(),
                            message: "another connection is already the controller".into(),
                        },
                    },
                };

                drop(slot);
                write_line(&mut write_half, &refusal).await?;
                return Ok(());
            }

            let (tx, rx) = mpsc::channel::<Pending>(4);
            *slot = Some(tx);
            controller_rx = Some(rx);
        }

        let update_tool = self.shared.lock().await.update_tool.clone();

        write_line(
            &mut write_half,
            &Frame::Welcome {
                agent_version: env!("CARGO_PKG_VERSION").into(),
                api: API_VERSION,
                role,
                update_tool,
            },
        )
        .await?;

        log::info!("ipc: {name} connected as {role:?}");

        let result = self
            .pump(&mut lines, &mut write_half, controller_rx, subscriptions)
            .await;

        if role == Role::Controller {
            // Dropping the sender is what makes an outstanding `ask` resolve
            // straight away rather than waiting out its deadline.
            *self.controller.lock().await = None;
        }

        log::info!("ipc: {name} disconnected");

        result
    }

    async fn pump(
        &self,
        lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
        write_half: &mut tokio::net::unix::OwnedWriteHalf,
        mut controller_rx: Option<mpsc::Receiver<Pending>>,
        subscriptions: Vec<String>,
    ) -> Result<(), Error> {
        let mut events = self.events.subscribe();
        let mut subscriptions = subscriptions;

        // Questions the agent has asked this connection and is waiting on.
        let mut outstanding: HashMap<String, oneshot::Sender<Response>> = HashMap::new();
        let mut next_id: u64 = 0;

        loop {
            // `controller_rx` is None for an observer. A never-completing branch
            // keeps the select shape the same rather than duplicating the loop.
            let controller_next = async {
                match controller_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            };

            tokio::select! {
                line = lines.next_line() => {
                    let Some(line) = line? else { return Ok(()) };

                    if line.trim().is_empty() {
                        continue;
                    }

                    match serde_json::from_str::<Frame>(&line) {
                        Ok(Frame::Request { id, method }) => {
                            let response = self.handle_method(method, &mut subscriptions).await;
                            write_line(write_half, &Frame::Response { id, result: response }).await?;
                        }

                        Ok(Frame::Response { id, result }) => {
                            match outstanding.remove(&id) {
                                Some(tx) => { let _ = tx.send(result); }
                                // A late answer to a question that already timed
                                // out. Logged rather than ignored, because it is
                                // the shape of a controller that is too slow
                                // rather than broken, and that is worth seeing.
                                None => log::debug!("ipc: response to unknown or expired request {id}"),
                            }
                        }

                        Ok(_) => log::debug!("ipc: ignoring unexpected frame"),
                        Err(e) => log::warn!("ipc: undecodable frame: {e}"),
                    }
                }

                pending = controller_next => {
                    let Some((method, reply_to)) = pending else { return Ok(()) };

                    next_id += 1;
                    let id = format!("a{next_id}");

                    outstanding.insert(id.clone(), reply_to);
                    write_line(write_half, &Frame::Request { id, method }).await?;
                }

                event = events.recv() => {
                    match event {
                        Ok(event) => {
                            if subscriptions.iter().any(|s| s == event.name()) {
                                write_line(write_half, &Frame::Event { event }).await?;
                            }
                        }
                        // Lagged: this connection did not keep up. Dropping
                        // events is correct — they are a running commentary, and
                        // replaying a stale progress percentage is worse than
                        // skipping it.
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            log::warn!("ipc: subscriber missed {n} events");
                        }
                        Err(_) => return Ok(()),
                    }
                }
            }
        }
    }

    async fn handle_method(&self, method: Method, subscriptions: &mut Vec<String>) -> Response {
        match method {
            Method::Status => {
                let shared = self.shared.lock().await;

                Response::Ok {
                    result: ResponseBody::Status(Status {
                        connection: shared.connection.unwrap_or(ConnectionState::Disconnected),
                        identifier: shared.identifier.clone(),
                        update_tool: shared.update_tool.clone(),
                        firmware: shared.firmware.clone(),
                        update: shared.update.clone(),
                        pending_validation: shared.pending_validation,
                    }),
                }
            }

            Method::Subscribe { events } => {
                *subscriptions = events;
                Response::Ok {
                    result: ResponseBody::Empty {},
                }
            }

            Method::MarkValid => {
                let (tx, rx) = oneshot::channel();

                if self.commands.send(Command::MarkValid(tx)).await.is_err() {
                    return error("agent_stopping", "the agent is shutting down");
                }

                match rx.await {
                    Ok(Ok(())) => Response::Ok {
                        result: ResponseBody::Empty {},
                    },
                    Ok(Err(message)) => error("mark_valid_failed", &message),
                    Err(_) => error("agent_stopping", "the agent is shutting down"),
                }
            }

            Method::Reboot { reason } => {
                let _ = self.commands.send(Command::Reboot { reason }).await;

                Response::Ok {
                    result: ResponseBody::Empty {},
                }
            }

            // Accepted and dropped. Reporting them needs the health extension,
            // which is not written; answering `unsupported` would make an
            // application that publishes metrics look broken when it is not.
            Method::Metrics { values } => {
                log::debug!("ipc: {} metrics received (not yet reported)", values.len());

                Response::Ok {
                    result: ResponseBody::Empty {},
                }
            }

            // Questions only the agent asks. An application sending one is
            // confused about which end it is.
            Method::UpdateAvailable { .. } | Method::RebootRequest { .. } | Method::Identify => {
                error("wrong_direction", "that is a question the agent asks you")
            }
        }
    }
}

fn error(code: &str, message: &str) -> Response {
    Response::Err {
        error: ErrorBody {
            code: code.into(),
            message: message.into(),
        },
    }
}

async fn write_line(
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    frame: &Frame,
) -> Result<(), Error> {
    let mut line = serde_json::to_string(frame)?;
    line.push('\n');

    write_half.write_all(line.as_bytes()).await?;

    Ok(())
}

fn set_mode(path: &Path, mode: u32) -> Result<(), Error> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| Error::Ipc(format!("chmod {}: {e}", path.display())))
}
