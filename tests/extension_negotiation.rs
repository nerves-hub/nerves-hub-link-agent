//! A device offers what the platform says it has, not everything it can do.
//!
//! The platform names its extensions and the versions of each in
//! `extensions:get`. Answering with the subset this agent also implements is
//! what keeps a device from declaring a version nothing can serve -- and, since
//! `logging` exists at two versions that are not the same conversation, it is
//! what keeps an agent from talking to a platform that cannot understand it.
//!
//! The second test is the case that has no message at all: a NervesHub old
//! enough not to ask. Waiting for it forever would cost the device every
//! extension it has, so the wait ends and everything is offered.

// `Tool::Sandbox` only exists when the feature does.
#![cfg(feature = "sandbox")]

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use nerves_hub_link_agent::agent::{Agent, Tool};
use nerves_hub_link_agent::message::{event, Message, DEVICE_TOPIC};
use nerves_hub_link_agent::Config;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Local sockets and one timer; past this something is wedged, and a hang is a
/// worse failure report than an assertion.
const PATIENCE: Duration = Duration::from_secs(30);

#[tokio::test(flavor = "multi_thread")]
async fn only_the_extensions_the_platform_has_are_offered() {
    // Health is advertised at the version this agent implements, so it is
    // offered. Logging is advertised only at 0.0.1, which this agent does not
    // implement -- it batches, and 0.0.1 is the one-line-per-message
    // conversation -- so it is left out rather than sent somewhere it cannot
    // be read. Geo is not named at all.
    let offered = negotiate(
        "only-what-the-platform-has",
        Some(json!({
            "health": ["0.0.1"],
            "logging": ["0.0.1"],
        })),
    )
    .await;

    assert_eq!(offered, json!({ "health": "0.0.1" }), "offered {offered}");
}

#[tokio::test(flavor = "multi_thread")]
async fn everything_is_offered_when_the_platform_never_asks() {
    // No `extensions:get`, so the agent waits, gives up on being asked, and
    // offers what it implements. A platform that predates the advertisement
    // still serves the versions it always did.
    let offered = negotiate("platform-never-asks", None).await;

    assert_eq!(
        offered,
        json!({ "geo": "0.0.1", "health": "0.0.1", "logging": "0.1.0" }),
        "offered {offered}"
    );
}

/// Run one agent against a platform that advertises `advertisement`, and
/// return the payload it joined the extensions topic with.
async fn negotiate(name: &str, advertisement: Option<Value>) -> Value {
    let work = scratch(name);

    let (events, mut seen) = mpsc::unbounded_channel();
    let ws_port = fake_nerveshub(advertisement, events).await;

    let config = config(&work, ws_port);
    let tool = Tool::build(&config.update_tool).expect("sandbox tool");
    let mut agent = Agent::new(config, "agent-test-03".into(), tool)
        .await
        .expect("agent");

    // Driven from this task, not spawned: `Agent` is deliberately not `Send`,
    // and `run` reconnects forever, so the collector is what ends this.
    tokio::select! {
        outcome = agent.run() => panic!("the agent stopped instead of negotiating: {outcome:?}"),

        offered = tokio::time::timeout(PATIENCE, seen.recv()) => {
            offered
                .unwrap_or_else(|_| panic!("no extensions join within {PATIENCE:?}"))
                .expect("the fake server hung up")
        }
    }
}

/// Accepts the device, optionally says what it has, and reports the payload
/// the device joins the extensions topic with.
async fn fake_nerveshub(advertisement: Option<Value>, events: UnboundedSender<Value>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let events = events.clone();
            let advertisement = advertisement.clone();

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
                            let _ = events.send(message.payload.clone());
                        }

                        event::JOIN => {
                            let reply = reply(&message);
                            let _ = socket.send(WsMessage::Text(reply)).await;

                            if let Some(advertisement) = &advertisement {
                                let request = Message::new(
                                    DEVICE_TOPIC,
                                    event::EXTENSIONS_GET,
                                    json!({ "extensions": advertisement }),
                                );

                                let _ = socket
                                    .send(WsMessage::Text(request.encode().unwrap()))
                                    .await;
                            }
                        }

                        _ => {}
                    }
                }
            });
        }
    });

    port
}

fn reply(join: &Message) -> String {
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
identifier = {{ literal = "agent-test-03" }}

[update_tool]
name = "sandbox"
work_dir = "{work_dir}"
initial_firmware = {{ uuid = "11111111-1111-1111-1111-111111111111", version = "1.0.0", product = "test-product", platform = "sandbox", architecture = "x86_64" }}

[ipc]
socket = "{ipc}"

# Three extensions, so an advertisement has something to leave out.
[extensions.health]
enabled = true

[extensions.geo]
enabled = true
source = {{ fixed = {{ latitude = -41.28, longitude = 174.77, accuracy = 10.0 }} }}

[extensions.logging]
enabled = true
source = {{ command = "sleep 300" }}
"#,
        work_dir = work.join("sandbox").display(),
        ipc = work.join("agent.sock").display(),
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
