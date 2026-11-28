//! A download whose server goes quiet has to end.
//!
//! The firmware download is its own HTTP connection. When that connection dies
//! without a reset (a NAT forgets it, a cellular link drops, a proxy wedges)
//! nothing ever arrives and nothing ever errors. Before the stall timeout the
//! install waited on it forever, and since the agent installs one thing at a
//! time, it refused every update after it as "already installing" until
//! someone restarted it.
//!
//! So these tests serve headers and part of a body and then hold the
//! connection open in silence, and check that the install fails with a stall,
//! promptly, and that the agent is free afterwards.

// `Tool::Sandbox` only exists when the feature does.
#![cfg(feature = "sandbox")]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use nerves_hub_link_agent::agent::{Agent, Tool};
use nerves_hub_link_agent::config::SandboxConfig;
use nerves_hub_link_agent::http::{Client, Timeouts};
use nerves_hub_link_agent::message::{event, Message, DEVICE_TOPIC};
use nerves_hub_link_agent::update_tool::sandbox::Sandbox;
use nerves_hub_link_agent::{Config, Error, FirmwareMeta, UpdatePayload};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Everything here is local sockets and a one-second stall; if we are past
/// this something is wedged, and a hang is a worse failure report than an
/// assertion.
const PATIENCE: Duration = Duration::from_secs(60);

const FIRMWARE_BYTES: usize = 512 * 1024;

/// The sandbox install on its own: the download stalls, and the install fails
/// with an error that says so rather than waiting.
#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_download_fails_the_install_with_a_stall() {
    let work = scratch("sandbox-stall");
    let firmware = vec![0x5au8; FIRMWARE_BYTES];
    let stalling_url = serve_then_stall(firmware.len(), &firmware[..64 * 1024]).await;

    let mut sandbox = Sandbox::new(SandboxConfig {
        work_dir: work.join("sandbox"),
        initial_firmware: None,
        install_duration_secs: 0,
        fail_installs: false,
    })
    .unwrap();

    let stall = Duration::from_secs(1);
    let client = Client::with_native_roots_and_timeouts(Timeouts {
        connect: Duration::from_secs(5),
        stall,
    })
    .unwrap();

    let started = Instant::now();

    let outcome = tokio::time::timeout(
        PATIENCE,
        sandbox.install_async(&update(&stalling_url, "33333333"), &client, |_, _| {}),
    )
    .await
    .expect("the install never ended: the stall timeout did not fire");

    let error = outcome.expect_err("half a firmware image installed successfully");

    assert!(
        matches!(&error, Error::Download(m) if m.starts_with("stalled after 65536 bytes")),
        "got: {error}"
    );

    // The bound is loose: what matters is seconds, not the minute-long default
    // or forever.
    assert!(
        started.elapsed() < stall * 10,
        "took {:?} for a {stall:?} stall timeout",
        started.elapsed()
    );
}

/// The whole agent: NervesHub offers an update whose download stalls, and the
/// agent reports it as failed and takes the next update offered rather than
/// refusing it as "already installing".
#[tokio::test(flavor = "multi_thread")]
async fn a_stalled_download_is_reported_failed_and_frees_the_agent() {
    let work = scratch("agent-stall");
    let firmware = vec![0x5au8; FIRMWARE_BYTES];

    let stalling_url = serve_then_stall(firmware.len(), &firmware[..64 * 1024]).await;
    let good_url = serve(firmware).await;

    let (events, mut seen) = mpsc::unbounded_channel();
    let ws_port = fake_nerveshub(stalling_url, good_url, events).await;

    let config = config(&work, ws_port);
    let tool = Tool::build(&config.update_tool).expect("sandbox tool");
    let mut agent = Agent::new(config, "agent-stall-01".into(), tool)
        .await
        .expect("agent");

    // Driven from this task rather than spawned: `Agent` is deliberately not
    // `Send`, and `run` reconnects forever by design, so the collector decides
    // when this is over.
    let mut log = Vec::new();

    tokio::select! {
        outcome = agent.run() => panic!("the agent stopped: {outcome:?}"),

        collected = tokio::time::timeout(PATIENCE, collect_until_reboot(&mut seen, &mut log)) => {
            if collected.is_err() {
                panic!("no reboot report within {PATIENCE:?}; got as far as {log:?}");
            }
        }
    }

    let failed = log
        .iter()
        .find_map(|seen| match seen {
            Seen::Status(status, reason) if status == "failed" => Some(reason.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the stalled install was never reported failed: {log:?}"));

    assert!(
        failed.contains("stalled"),
        "the failure should say the download stalled, got: {failed:?}"
    );

    // Freed: the second update was installed, which an agent still holding
    // the first install would have refused.
    let completed = log
        .iter()
        .position(|seen| matches!(seen, Seen::Status(status, _) if status == "completed"))
        .unwrap_or_else(|| panic!("the update after the stall was never installed: {log:?}"));

    let reported_failed = log
        .iter()
        .position(|seen| matches!(seen, Seen::Status(status, _) if status == "failed"))
        .unwrap();

    assert!(reported_failed < completed, "out of order: {log:?}");

    let installed = work.join("sandbox").join(format!("{GOOD_UUID}.fw"));

    assert!(
        installed.exists(),
        "reported completion without installing: {} is missing",
        installed.display()
    );
}

const STALLING_UUID: &str = "33333333-3333-3333-3333-333333333333";
const GOOD_UUID: &str = "44444444-4444-4444-4444-444444444444";

#[derive(Debug, Clone, PartialEq)]
enum Seen {
    Status(String, String),
    Rebooting,
}

async fn collect_until_reboot(seen: &mut UnboundedReceiver<Seen>, log: &mut Vec<Seen>) {
    while let Some(event) = seen.recv().await {
        let done = event == Seen::Rebooting;
        log.push(event);

        if done {
            return;
        }
    }
}

/// A NervesHub that offers the stalling update on join, and the good one once
/// the device reports the first as failed.
async fn fake_nerveshub(
    stalling_url: String,
    good_url: String,
    events: UnboundedSender<Seen>,
) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let events = events.clone();
            let stalling_url = stalling_url.clone();
            let good_url = good_url.clone();

            tokio::spawn(async move {
                let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
                    return;
                };

                let offer = |url: &str, uuid: &str| {
                    Message::new(DEVICE_TOPIC, event::UPDATE, update_json(url, uuid))
                        .encode()
                        .unwrap()
                };

                while let Some(Ok(frame)) = socket.next().await {
                    let WsMessage::Text(raw) = frame else {
                        continue;
                    };

                    let Ok(message) = Message::decode(&raw) else {
                        continue;
                    };

                    match message.event.as_str() {
                        event::JOIN if message.topic == DEVICE_TOPIC => {
                            let _ = socket.send(WsMessage::Text(reply_ok(&message))).await;
                            let _ = socket
                                .send(WsMessage::Text(offer(&stalling_url, STALLING_UUID)))
                                .await;
                        }

                        event::STATUS_UPDATE => {
                            let field = |key: &str| {
                                message
                                    .payload
                                    .get(key)
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string()
                            };

                            let status = field("status");
                            let _ = events.send(Seen::Status(status.clone(), field("reason")));

                            if status == "failed" {
                                let _ = socket
                                    .send(WsMessage::Text(offer(&good_url, GOOD_UUID)))
                                    .await;
                            }
                        }

                        event::REBOOTING => {
                            let _ = events.send(Seen::Rebooting);
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

fn update_json(url: &str, uuid: &str) -> serde_json::Value {
    json!({
        "update_available": true,
        "firmware_url": url,
        "firmware_meta": {
            "uuid": uuid,
            "version": "1.0.1",
            "product": "test-product",
            "platform": "sandbox",
            "architecture": "x86_64",
        },
    })
}

fn update(url: &str, uuid: &str) -> UpdatePayload {
    UpdatePayload {
        update_available: true,
        firmware_url: Some(url.to_string()),
        firmware_meta: Some(FirmwareMeta {
            uuid: Some(uuid.into()),
            version: Some("1.0.1".into()),
            product: Some("test-product".into()),
            platform: Some("sandbox".into()),
            architecture: Some("x86_64".into()),
        }),
        size: Some(FIRMWARE_BYTES as u64),
        checksum: None,
        deployment_id: None,
    }
}

fn config(work: &Path, ws_port: u16) -> Config {
    let toml = format!(
        r#"
[server]
host = "127.0.0.1"
port = {ws_port}
path = "/device-socket"
tls = false
heartbeat_interval_secs = 60
reconnect_backoff_secs = [1]

[identity]
product_key = "nhp_test"
product_secret = "test-secret"
identifier = {{ literal = "agent-stall-01" }}

[update_tool]
name = "sandbox"
work_dir = "{work_dir}"
initial_firmware = {{ uuid = "11111111-1111-1111-1111-111111111111", version = "1.0.0", product = "test-product", platform = "sandbox", architecture = "x86_64" }}
install_duration_secs = 0

[ipc]
socket = "{ipc}"

[updates]
policy = "apply"

[reboot]
policy = "immediate"

# The setting under test, at a second rather than a minute.
[http]
stall_timeout_secs = 1
"#,
        work_dir = work.join("sandbox").display(),
        ipc = work.join("agent.sock").display(),
    );

    toml::from_str(&toml).expect("test config")
}

/// Headers promising `length` bytes, then `prefix`, then silence on a
/// connection that stays open.
async fn serve_then_stall(length: usize, prefix: &[u8]) -> String {
    let prefix = prefix.to_vec();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/stalls.fw", listener.local_addr().unwrap());

    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let prefix = prefix.clone();

            tokio::spawn(async move {
                let mut request = [0u8; 2048];
                let _ = socket.read(&mut request).await;

                let header = format!("HTTP/1.1 200 OK\r\nContent-Length: {length}\r\n\r\n");
                let _ = socket.write_all(header.as_bytes()).await;
                let _ = socket.write_all(&prefix).await;

                // Silent until the client hangs up, which a dead connection
                // would never tell it to do.
                let mut rest = [0u8; 64];
                while socket.read(&mut rest).await.is_ok_and(|n| n > 0) {}
            });
        }
    });

    url
}

/// Serve one body over HTTP, whole.
async fn serve(body: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/firmware.fw", listener.local_addr().unwrap());

    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let body = body.clone();

            tokio::spawn(async move {
                let mut request = [0u8; 2048];
                let _ = socket.read(&mut request).await;

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

    url
}

/// `/tmp` rather than `std::env::temp_dir`: the agent's IPC socket lives in
/// here, and macOS's per-user temp directory leaves too little of `SUN_LEN`
/// for a socket name.
fn scratch(name: &str) -> PathBuf {
    let dir = PathBuf::from("/tmp").join("nhla-tests").join(name);

    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("sandbox")).unwrap();

    dir
}
