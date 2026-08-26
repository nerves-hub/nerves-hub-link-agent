//! An install must not stop the agent from being an agent.
//!
//! This is the failure that got a device stuck twice: the socket closed while
//! an install was running -- NervesHub's device socket times out at 180s and a
//! slow install sends no heartbeats -- and the agent never came back. The
//! install itself was fine. What was broken was everything the agent was
//! supposed to keep doing while it ran.
//!
//! So the test is about the transport, not the installer: a fake NervesHub
//! that offers an update and then drops the connection part-way through it.
//! What has to be true afterwards is that the agent reconnects and rejoins
//! *while the install is still running*, and then reports the result on the
//! new socket.
//!
//! The sandbox tool is what makes this runnable anywhere: it downloads,
//! verifies and writes to a file, takes `install_duration_secs` to do it -- the
//! window this test needs -- and its "reboot" is a log line.

// `Tool::Sandbox` only exists when the feature does.
#![cfg(feature = "sandbox")]

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use nerves_hub_link_agent::agent::{Agent, Tool};
use nerves_hub_link_agent::message::{event, Message, DEVICE_TOPIC};
use nerves_hub_link_agent::Config;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Long enough that the install is unambiguously still running when the socket
/// goes, short enough that the test is not a coffee break. Ten steps, so the
/// server has ~9 progress reports left to close on after the first.
const INSTALL_SECS: u64 = 5;

/// Everything here is local sockets and sleeps; if we are past this something
/// is wedged, and a hang is a worse failure report than an assertion.
const PATIENCE: Duration = Duration::from_secs(60);

/// What the fake server saw, tagged with which connection saw it. The
/// connection number is the whole point: the same events on connection 1 and
/// connection 2 mean opposite things.
#[derive(Debug, Clone, PartialEq)]
enum Seen {
    Joined(usize),
    Progress(usize),
    Dropped(usize),
    Rebooting(usize),
}

#[tokio::test(flavor = "multi_thread")]
async fn the_agent_reconnects_while_an_install_is_still_running() {
    let work = scratch("survives-disconnect");
    let firmware = write_firmware(&work);

    let (firmware_url, _http) = serve_file(&firmware).await;
    let (events, mut seen) = mpsc::unbounded_channel();
    let ws_port = fake_nerveshub(firmware_url, events).await;

    let config = config(&work, ws_port);
    let tool = Tool::build(&config.update_tool).expect("sandbox tool");
    let mut agent = Agent::new(config, "agent-test-01".into(), tool)
        .await
        .expect("agent");

    // Driven from this task rather than spawned, because that is how it runs
    // for real: `Agent` is deliberately not `Send`, and the thing under test is
    // its own reconnect loop. `run` reconnects forever by design, so it is the
    // collector that decides when this is over -- either the event we are
    // waiting for, or `PATIENCE`.
    // Accumulated outside the future so that a timeout can say how far it got.
    // "no reboot report within 60s" on its own does not distinguish a hung
    // reconnect from a server fixture that never offered the update.
    let mut log = Vec::new();

    tokio::select! {
        outcome = agent.run() => panic!("the agent gave up instead of reconnecting: {outcome:?}"),

        collected = tokio::time::timeout(PATIENCE, collect_until_reboot(&mut seen, &mut log)) => {
            if collected.is_err() {
                panic!("no reboot report within {PATIENCE:?}; got as far as {log:?}");
            }
        }
    }

    // The setup: joined, offered an update, started installing, lost the socket.
    assert!(
        log.contains(&Seen::Joined(1)),
        "never joined on the first connection: {log:?}"
    );
    assert!(
        log.contains(&Seen::Progress(1)),
        "the install never started, so nothing was interrupted: {log:?}"
    );
    assert!(
        log.contains(&Seen::Dropped(1)),
        "the socket was never dropped, so this proves nothing: {log:?}"
    );

    // The point. A second join means the reconnect got all the way through the
    // handshake -- and it happened before the install finished, which is what
    // "an install does not block the agent" actually means. An agent that only
    // rejoined once the tool was free would still pass a bare `contains`.
    let rejoined = position(&log, &Seen::Joined(2))
        .unwrap_or_else(|| panic!("never rejoined after the socket dropped: {log:?}"));

    let finished = position(&log, &Seen::Rebooting(2))
        .unwrap_or_else(|| panic!("the install never reported finishing: {log:?}"));

    assert!(
        rejoined < finished,
        "rejoined only after the install finished, so the install was still blocking: {log:?}"
    );

    // And the install the socket interrupted ran to completion regardless.
    let installed = work
        .join("sandbox")
        .join("22222222-2222-2222-2222-222222222222.fw");

    assert!(
        installed.exists(),
        "the install did not finish: {} is missing",
        installed.display()
    );
}

/// Drain events until the install reports it is done, keeping them in order.
async fn collect_until_reboot(seen: &mut UnboundedReceiver<Seen>, log: &mut Vec<Seen>) {
    while let Some(event) = seen.recv().await {
        let done = matches!(event, Seen::Rebooting(_));
        log.push(event);

        if done {
            return;
        }
    }
}

fn position(log: &[Seen], event: &Seen) -> Option<usize> {
    log.iter().position(|seen| seen == event)
}

/// A NervesHub that offers one update and then hangs up on the device
/// mid-install.
///
/// Connection 1 offers the update and drops as soon as the device reports
/// progress. Every connection after that behaves: it accepts the join and says
/// there is nothing to do, which is what a reconnecting device should find.
async fn fake_nerveshub(firmware_url: String, events: UnboundedSender<Seen>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        let mut connection = 0usize;

        while let Ok((stream, _)) = listener.accept().await {
            connection += 1;

            let events = events.clone();
            let firmware_url = firmware_url.clone();

            tokio::spawn(async move {
                let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };

                while let Some(Ok(frame)) = socket.next().await {
                    let WsMessage::Text(raw) = frame else {
                        continue;
                    };

                    let Ok(message) = Message::decode(&raw) else {
                        continue;
                    };

                    match message.event.as_str() {
                        event::JOIN => {
                            let reply = reply_ok(&message);
                            let _ = socket.send(WsMessage::Text(reply)).await;
                            let _ = events.send(Seen::Joined(connection));

                            if connection == 1 {
                                let update = Message::new(
                                    DEVICE_TOPIC,
                                    event::UPDATE,
                                    update_payload(&firmware_url),
                                );

                                let _ =
                                    socket.send(WsMessage::Text(update.encode().unwrap())).await;
                            }
                        }

                        event::UPDATE_PROGRESS => {
                            let _ = events.send(Seen::Progress(connection));

                            // The whole point of the fixture. Waiting for
                            // progress rather than sleeping means the install is
                            // provably underway when the socket goes, on a slow
                            // machine as well as a fast one.
                            if connection == 1 {
                                let _ = events.send(Seen::Dropped(connection));
                                return;
                            }
                        }

                        event::REBOOTING => {
                            let _ = events.send(Seen::Rebooting(connection));
                        }

                        _ => {}
                    }
                }
            });
        }
    });

    port
}

fn reply_ok(join: &Message) -> String {
    Message {
        join_ref: join.join_ref.clone(),
        reference: join.reference.clone(),
        topic: join.topic.clone(),
        event: event::REPLY.into(),
        payload: json!({ "status": "ok", "response": {} }),
    }
    .encode()
    .unwrap()
}

fn update_payload(firmware_url: &str) -> serde_json::Value {
    json!({
        "update_available": true,
        "firmware_url": firmware_url,
        "firmware_meta": {
            "uuid": "22222222-2222-2222-2222-222222222222",
            "version": "1.0.1",
            "product": "test-product",
            "platform": "sandbox",
            "architecture": "x86_64",
        },
    })
}

fn config(work: &Path, ws_port: u16) -> Config {
    let toml = format!(
        r#"
[server]
host = "127.0.0.1"
port = {ws_port}
path = "/device-socket"
tls = false
# Nothing here should depend on a heartbeat landing.
heartbeat_interval_secs = 60
# One second, so the reconnect is quick without being a busy loop.
reconnect_backoff_secs = [1]

[identity]
product_key = "nhp_test"
product_secret = "test-secret"
identifier = {{ literal = "agent-test-01" }}

[update_tool]
name = "sandbox"
work_dir = "{work_dir}"
initial_firmware = {{ uuid = "11111111-1111-1111-1111-111111111111", version = "1.0.0", product = "test-product", platform = "sandbox", architecture = "x86_64" }}
install_duration_secs = {INSTALL_SECS}

[ipc]
socket = "{ipc}"

[updates]
# No controller is listening, and this test is not about asking one.
policy = "apply"

[reboot]
policy = "immediate"
"#,
        work_dir = work.join("sandbox").display(),
        ipc = work.join("agent.sock").display(),
    );

    toml::from_str(&toml).expect("test config")
}

/// Serve one file over HTTP, for the firmware url the update points at.
async fn serve_file(file: &Path) -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let body = std::fs::read(file).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/firmware.fw", listener.local_addr().unwrap());

    let handle = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let body = body.clone();

            tokio::spawn(async move {
                let mut scratch = [0u8; 2048];
                let _ = socket.read(&mut scratch).await;

                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );

                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(&body).await;
                let _ = socket.flush().await;
            });
        }
    });

    (url, handle)
}

/// Big enough that the download is a real transfer, small enough to be free.
fn write_firmware(work: &Path) -> PathBuf {
    let path = work.join("firmware.fw");
    std::fs::write(&path, vec![0x5au8; 512 * 1024]).unwrap();
    path
}

/// `/tmp` rather than `std::env::temp_dir`, because the agent's IPC socket
/// lives in here and a unix socket path has to fit in `SUN_LEN` -- about 104
/// bytes. macOS hands out per-user temp directories that are most of that
/// before a filename is added, so `temp_dir` fails to bind on a laptop and
/// works in CI, which is the worst of both.
fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from("/tmp").join("nhla-tests").join(name);

    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("sandbox")).unwrap();

    dir
}
