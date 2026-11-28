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
//! That rejoin also has to say what it is installing. NervesHub abandons the
//! open update of a device that joins without `currently_downloading_uuid`, so
//! a reconnect that stayed quiet would lose the server's record of an install
//! that is going fine. A join once the install is over has to say nothing.
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

/// The firmware the fake server offers.
const TARGET_UUID: &str = "22222222-2222-2222-2222-222222222222";

/// What the fake server saw, tagged with which connection saw it. The
/// connection number is the whole point: the same events on connection 1 and
/// connection 2 mean opposite things.
#[derive(Debug, Clone, PartialEq)]
enum Seen {
    /// A join, and its `currently_downloading_uuid` exactly as sent: `None`
    /// when the key was left out, which is not the same as a `null`.
    Joined(usize, Option<serde_json::Value>),
    Progress(usize),
    Dropped(usize),
    Rebooting(usize),
    Status(usize, String),
}

/// When the fake server hangs up on the device's first connection.
#[derive(Debug, Clone, Copy, PartialEq)]
enum HangUp {
    /// It doesn't.
    Never,
    /// At the first progress report, so the install is provably running.
    MidInstall,
    /// Once the device reports how the install ended, so the rejoin comes with
    /// nothing installing.
    AfterInstall,
}

#[tokio::test(flavor = "multi_thread")]
async fn the_agent_reconnects_while_an_install_is_still_running() {
    let work = scratch("survives-disconnect");
    let firmware = write_firmware(&work);

    let (firmware_url, _http) = serve_file(&firmware).await;
    let (events, mut seen) = mpsc::unbounded_channel();
    let ws_port = fake_nerveshub(firmware_url, events, HangUp::MidInstall).await;

    let config = config(&work, ws_port, false);
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

        collected = tokio::time::timeout(PATIENCE, collect_until(&mut seen, &mut log, reported_rebooting)) => {
            if collected.is_err() {
                panic!("no reboot report within {PATIENCE:?}; got as far as {log:?}");
            }
        }
    }

    // The setup: joined, offered an update, started installing, lost the socket.
    // Nothing was installing at the first join, so it said nothing.
    assert!(
        log.contains(&Seen::Joined(1, None)),
        "never joined idle on the first connection: {log:?}"
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
    //
    // And it names the firmware it is installing, or NervesHub would abandon
    // the update it has open for this one.
    let rejoined =
        position(&log, &Seen::Joined(2, Some(json!(TARGET_UUID)))).unwrap_or_else(|| {
            panic!("never rejoined saying what it was installing after the socket dropped: {log:?}")
        });

    let finished = position(&log, &Seen::Rebooting(2))
        .unwrap_or_else(|| panic!("the install never reported finishing: {log:?}"));

    assert!(
        rejoined < finished,
        "rejoined only after the install finished, so the install was still blocking: {log:?}"
    );

    // And the install the socket interrupted ran to completion regardless.
    let installed = work.join("sandbox").join(format!("{TARGET_UUID}.fw"));

    assert!(
        installed.exists(),
        "the install did not finish: {} is missing",
        installed.display()
    );
}

/// Once an install has reported how it went, there is nothing left for
/// NervesHub to keep open, and a rejoin that still named the firmware would
/// keep the server's record of an update that is over.
#[tokio::test(flavor = "multi_thread")]
async fn a_rejoin_after_a_finished_install_names_nothing() {
    let work = scratch("rejoin-after-install");
    let firmware = write_firmware(&work);

    let (firmware_url, _http) = serve_file(&firmware).await;
    let (events, mut seen) = mpsc::unbounded_channel();
    let ws_port = fake_nerveshub(firmware_url, events, HangUp::AfterInstall).await;

    let config = config(&work, ws_port, false);
    let tool = Tool::build(&config.update_tool).expect("sandbox tool");
    let mut agent = Agent::new(config, "agent-test-03".into(), tool)
        .await
        .expect("agent");

    let log = rejoin_after_install(&mut agent, &mut seen).await;

    let completed = position(&log, &Seen::Status(1, "completed".into()))
        .unwrap_or_else(|| panic!("never reported the install completing: {log:?}"));

    let rejoined = position(&log, &Seen::Joined(2, None))
        .unwrap_or_else(|| panic!("the rejoin still named the installed firmware: {log:?}"));

    assert!(
        completed < rejoined,
        "rejoined before the install finished, so this proves nothing: {log:?}"
    );
}

/// The same after an install that failed, which ends it as surely as success.
#[tokio::test(flavor = "multi_thread")]
async fn a_rejoin_after_a_failed_install_names_nothing() {
    let work = scratch("rejoin-after-failure");
    let firmware = write_firmware(&work);

    let (firmware_url, _http) = serve_file(&firmware).await;
    let (events, mut seen) = mpsc::unbounded_channel();
    let ws_port = fake_nerveshub(firmware_url, events, HangUp::AfterInstall).await;

    let config = config(&work, ws_port, true);
    let tool = Tool::build(&config.update_tool).expect("sandbox tool");
    let mut agent = Agent::new(config, "agent-test-04".into(), tool)
        .await
        .expect("agent");

    let log = rejoin_after_install(&mut agent, &mut seen).await;

    let failed = position(&log, &Seen::Status(1, "failed".into()))
        .unwrap_or_else(|| panic!("never reported the install failing: {log:?}"));

    let rejoined = position(&log, &Seen::Joined(2, None))
        .unwrap_or_else(|| panic!("the rejoin still named the failed firmware: {log:?}"));

    assert!(
        failed < rejoined,
        "rejoined before the install failed, so this proves nothing: {log:?}"
    );
}

/// Run the agent against a server that hangs up once the install has ended,
/// until the device joins a second time.
async fn rejoin_after_install(agent: &mut Agent, seen: &mut UnboundedReceiver<Seen>) -> Vec<Seen> {
    let mut log = Vec::new();

    tokio::select! {
        outcome = agent.run() => panic!("the agent stopped: {outcome:?}"),

        collected = tokio::time::timeout(PATIENCE, collect_until(seen, &mut log, joined_again)) => {
            if collected.is_err() {
                panic!("no second join within {PATIENCE:?}; got as far as {log:?}");
            }
        }
    }

    assert!(
        log.contains(&Seen::Joined(1, None)),
        "never joined idle on the first connection: {log:?}"
    );

    log
}

/// Drain events until `done` says so, keeping them in order.
async fn collect_until(
    seen: &mut UnboundedReceiver<Seen>,
    log: &mut Vec<Seen>,
    done: fn(&Seen) -> bool,
) {
    while let Some(event) = seen.recv().await {
        let finished = done(&event);
        log.push(event);

        if finished {
            return;
        }
    }
}

fn reported_rebooting(event: &Seen) -> bool {
    matches!(event, Seen::Rebooting(_))
}

fn joined_again(event: &Seen) -> bool {
    matches!(event, Seen::Joined(2, _))
}

fn position(log: &[Seen], event: &Seen) -> Option<usize> {
    log.iter().position(|seen| seen == event)
}

/// A NervesHub that offers one update and then hangs up on the device
/// mid-install.
///
/// Connection 1 offers the update and drops when `hang_up` says to. Every
/// connection after that behaves: it accepts the join and says there is nothing
/// to do, which is what a reconnecting device should find.
async fn fake_nerveshub(
    firmware_url: String,
    events: UnboundedSender<Seen>,
    hang_up: HangUp,
) -> u16 {
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
                        // The extensions join. Accepted, and neither recorded
                        // nor answered with an update: `Seen::Joined` is the
                        // device join, whose payload these tests read.
                        event::JOIN if message.topic != DEVICE_TOPIC => {
                            let reply = reply_ok(&message);
                            let _ = socket.send(WsMessage::Text(reply)).await;
                        }

                        event::JOIN => {
                            let reply = reply_ok(&message);
                            let _ = socket.send(WsMessage::Text(reply)).await;

                            let downloading =
                                message.payload.get("currently_downloading_uuid").cloned();
                            let _ = events.send(Seen::Joined(connection, downloading));

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
                            if hang_up == HangUp::MidInstall && connection == 1 {
                                let _ = events.send(Seen::Dropped(connection));
                                return;
                            }
                        }

                        event::STATUS_UPDATE => {
                            let status = message
                                .payload
                                .get("status")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string();

                            let ended = matches!(status.as_str(), "completed" | "failed");
                            let _ = events.send(Seen::Status(connection, status));

                            if hang_up == HangUp::AfterInstall && connection == 1 && ended {
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
            "uuid": TARGET_UUID,
            "version": "1.0.1",
            "product": "test-product",
            "platform": "sandbox",
            "architecture": "x86_64",
        },
    })
}

/// `fail_installs` has the sandbox fail every install before it downloads.
fn config(work: &Path, ws_port: u16, fail_installs: bool) -> Config {
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
fail_installs = {fail_installs}

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

/// A successful install has to say so.
///
/// NervesHub opens an inflight update when the device reports `started` and
/// closes it when the device reports `completed`. The failure path always
/// reported itself; success went straight to the reboot, so an update that
/// worked looked from the server exactly like one that stalled at 100%.
#[tokio::test(flavor = "multi_thread")]
async fn a_successful_install_reports_started_and_completed() {
    let work = scratch("reports-status");
    let firmware = write_firmware(&work);

    let (firmware_url, _http) = serve_file(&firmware).await;
    let (events, mut seen) = mpsc::unbounded_channel();
    let ws_port = fake_nerveshub(firmware_url, events, HangUp::Never).await;

    let config = config(&work, ws_port, false);
    let tool = Tool::build(&config.update_tool).expect("sandbox tool");
    let mut agent = Agent::new(config, "agent-test-02".into(), tool)
        .await
        .expect("agent");

    let mut log = Vec::new();

    tokio::select! {
        outcome = agent.run() => panic!("the agent stopped: {outcome:?}"),

        collected = tokio::time::timeout(PATIENCE, collect_until(&mut seen, &mut log, reported_rebooting)) => {
            if collected.is_err() {
                panic!("no reboot report within {PATIENCE:?}; got as far as {log:?}");
            }
        }
    }

    let started = position(&log, &Seen::Status(1, "started".into()))
        .unwrap_or_else(|| panic!("never reported the install starting: {log:?}"));

    let completed = position(&log, &Seen::Status(1, "completed".into()))
        .unwrap_or_else(|| panic!("never reported the install completing: {log:?}"));

    let rebooting = position(&log, &Seen::Rebooting(1))
        .unwrap_or_else(|| panic!("never reported rebooting: {log:?}"));

    assert!(
        started < completed,
        "reported completion before it reported starting: {log:?}"
    );

    // The ordering that matters. With `reboot.policy = "immediate"` the reboot
    // is the last thing that happens, and a completion sent after it would
    // never leave a real device.
    assert!(
        completed < rebooting,
        "reported completion after the reboot, where it would not be sent: {log:?}"
    );

    // And the install it was reporting on actually happened.
    let installed = work.join("sandbox").join(format!("{TARGET_UUID}.fw"));

    assert!(
        installed.exists(),
        "reported completion without installing anything: {}",
        installed.display()
    );
}
