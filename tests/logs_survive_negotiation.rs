//! The log tail runs before anyone is listening, and what it writes then still
//! has to arrive.
//!
//! A device tails its logs from the moment the process starts, but NervesHub
//! does not want a single line until it has attached the `logging` extension —
//! which is a join, then a second join on the extensions topic, then a reply.
//! The lines written in that window are the ones an operator actually goes
//! looking for: the boot, and whatever crash the device is reconnecting from.
//!
//! So the fixture is a NervesHub that takes its time. It accepts the device
//! join, then sits on the extensions join long enough that the agent has
//! provably read its log lines with nowhere to send them. What has to be true
//! afterwards is that all of them arrive once the attach lands, in the order
//! they were written, and in one message rather than three -- the platform
//! limits how often a device may send, so a backlog that went a line at a time
//! would be dropped on the other side instead of this one.

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

/// The lines the device writes before anything is attached.
const LINES: [&str; 3] = ["before the join", "during the join", "still negotiating"];

/// How long the server sits on the extensions join.
///
/// This is what makes the test about anything: without it the attach could
/// land before the agent has read a line, and lines sent straight through
/// would pass an assertion that says nothing about the ones it held.
const NEGOTIATION: Duration = Duration::from_millis(500);

/// Local sockets and one sleep; past this something is wedged, and a hang is a
/// worse failure report than an assertion.
const PATIENCE: Duration = Duration::from_secs(30);

/// What the fake server saw, in order.
#[derive(Debug, Clone, PartialEq)]
enum Seen {
    Joined,
    LoggingAttached,
    /// One `logging:send`, and the lines it carried.
    Logs(Vec<String>),
}

#[tokio::test(flavor = "multi_thread")]
async fn lines_written_before_the_attach_are_not_lost() {
    let work = scratch("logs-survive-negotiation");

    let (events, mut seen) = mpsc::unbounded_channel();
    let ws_port = fake_nerveshub(events).await;

    let config = config(&work, ws_port);
    let tool = Tool::build(&config.update_tool).expect("sandbox tool");
    let mut agent = Agent::new(config, "agent-test-02".into(), tool)
        .await
        .expect("agent");

    // Driven from this task, not spawned: `Agent` is deliberately not `Send`,
    // and `run` reconnects forever, so the collector is what ends this.
    let mut log = Vec::new();

    tokio::select! {
        outcome = agent.run() => panic!("the agent stopped instead of shipping logs: {outcome:?}"),

        collected = tokio::time::timeout(PATIENCE, collect_lines(&mut seen, &mut log)) => {
            if collected.is_err() {
                panic!("not every line arrived within {PATIENCE:?}; got as far as {log:?}");
            }
        }
    }

    // The setup: the device joined, and the platform attached logging only
    // after sitting on the extensions join.
    assert!(log.contains(&Seen::Joined), "never joined: {log:?}");

    let attached = position(&log, &Seen::LoggingAttached)
        .unwrap_or_else(|| panic!("logging was never attached: {log:?}"));

    // The point. Every line the tail wrote while the platform was making up
    // its mind arrived, in the order it was written, once there was somewhere
    // to put it. Dropping them instead leaves this list empty.
    let messages: Vec<&Vec<String>> = log
        .iter()
        .skip(attached)
        .filter_map(|entry| match entry {
            Seen::Logs(lines) => Some(lines),
            _ => None,
        })
        .collect();

    let shipped: Vec<String> = messages
        .iter()
        .flat_map(|lines| lines.iter().cloned())
        .collect();

    assert_eq!(shipped, LINES, "the held lines did not arrive: {log:?}");

    // And in one message. Three would be three times the rate limit's cost for
    // the same second of logs, which is the thing batching exists to stop.
    assert_eq!(
        messages.len(),
        1,
        "the backlog went out a line at a time: {log:?}"
    );
}

/// Drain events until every line has been seen, keeping them in order.
async fn collect_lines(seen: &mut UnboundedReceiver<Seen>, log: &mut Vec<Seen>) {
    let mut lines = 0;

    while let Some(event) = seen.recv().await {
        if let Seen::Logs(sent) = &event {
            lines += sent.len();
        }

        log.push(event);

        if lines >= LINES.len() {
            return;
        }
    }
}

fn position(log: &[Seen], event: &Seen) -> Option<usize> {
    log.iter().position(|seen| seen == event)
}

/// A NervesHub that accepts the device and then takes its time over the
/// extensions negotiation.
async fn fake_nerveshub(events: UnboundedSender<Seen>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let events = events.clone();

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
                        event::JOIN if message.topic == "extensions" => {
                            // The window the test is about. The agent is
                            // reading log lines throughout it with no attached
                            // extension to send them on.
                            tokio::time::sleep(NEGOTIATION).await;

                            let reply = reply(&message, json!(["logging"]));
                            let _ = socket.send(WsMessage::Text(reply)).await;
                        }

                        event::JOIN => {
                            let reply = reply(&message, json!({}));
                            let _ = socket.send(WsMessage::Text(reply)).await;
                            let _ = events.send(Seen::Joined);

                            // The device waits to be asked before it joins the
                            // extensions topic, and answers with the subset of
                            // this it also implements.
                            let request = Message::new(
                                DEVICE_TOPIC,
                                event::EXTENSIONS_GET,
                                json!({ "extensions": { "logging": ["0.1.0", "0.0.1"] } }),
                            );

                            let _ = socket
                                .send(WsMessage::Text(request.encode().unwrap()))
                                .await;
                        }

                        "logging:attached" => {
                            let _ = events.send(Seen::LoggingAttached);
                        }

                        "logging:send" => {
                            let lines = message.payload["lines"]
                                .as_array()
                                .map(|lines| {
                                    lines
                                        .iter()
                                        .map(|line| {
                                            line["message"].as_str().unwrap_or_default().to_string()
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();

                            let _ = events.send(Seen::Logs(lines));
                        }

                        _ => {}
                    }
                }
            });
        }
    });

    port
}

fn reply(join: &Message, response: serde_json::Value) -> String {
    Message {
        join_ref: join.join_ref.clone(),
        reference: join.reference.clone(),
        topic: join.topic.clone(),
        event: event::REPLY.into(),
        payload: json!({ "status": "ok", "response": response }),
    }
    .encode()
    .unwrap()
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
identifier = {{ literal = "agent-test-02" }}

[update_tool]
name = "sandbox"
work_dir = "{work_dir}"
initial_firmware = {{ uuid = "11111111-1111-1111-1111-111111111111", version = "1.0.0", product = "test-product", platform = "sandbox", architecture = "x86_64" }}

[ipc]
socket = "{ipc}"

[extensions.logging]
enabled = true
# Three lines and then nothing, so the tail stays alive without writing
# anything the assertions have to account for.
source = {{ command = "{source}" }}
# Well above what this test sends: the cap has its own tests, and one that bit
# here would look like the loss this one is about.
max_lines_per_batch = 100
"#,
        work_dir = work.join("sandbox").display(),
        ipc = work.join("agent.sock").display(),
        source = LINES
            .iter()
            .map(|line| format!("echo '{line}'; "))
            .collect::<String>()
            + "sleep 300",
    );

    toml::from_str(&toml).expect("test config")
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
