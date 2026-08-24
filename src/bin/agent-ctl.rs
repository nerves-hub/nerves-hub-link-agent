//! A command-line client for the agent's IPC socket.
//!
//!     agent-ctl status
//!     agent-ctl mark-valid
//!     agent-ctl reboot [reason]
//!     agent-ctl watch
//!
//! The socket is the agent's only interface on the device, and until this
//! existed nothing on a minimal image could reach it — no python, no socat, no
//! nc. That made `mark-valid` unreachable from a support script, which is the
//! one place it most needs to be reachable from: a script that checks whether
//! the application is healthy and confirms the firmware is exactly the job
//! support scripts exist for.
//!
//! Deliberately not a daemon and not a controller. It connects as an observer,
//! asks one question, and exits — so running it never takes the controller slot
//! away from the application that owns it.

use std::process::ExitCode;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use nerves_hub_link_agent::ipc::protocol::{Frame, Method, Response, Role, API_VERSION};

const DEFAULT_SOCKET: &str = "/run/nerves-hub-link-agent/agent.sock";

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut socket =
        std::env::var("NERVES_HUB_AGENT_SOCKET").unwrap_or_else(|_| DEFAULT_SOCKET.into());
    let mut command = None;
    let mut rest: Vec<String> = Vec::new();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--socket" | "-s" => match args.next() {
                Some(path) => socket = path,
                None => return fail("--socket needs a path"),
            },
            "--help" | "-h" => {
                usage();
                return ExitCode::SUCCESS;
            }
            _ if command.is_none() => command = Some(arg),
            _ => rest.push(arg),
        }
    }

    let Some(command) = command else {
        usage();
        return ExitCode::FAILURE;
    };

    match run(&socket, &command, &rest).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => fail(&message),
    }
}

fn usage() {
    eprintln!(
        "usage: agent-ctl [--socket PATH] <command>\n\
         \n\
           status       what the agent knows: connection, identity, firmware\n\
           mark-valid   confirm the running firmware, releasing the rollback\n\
           reboot       reboot through the agent, so it can tell NervesHub first\n\
           watch        stream events until interrupted\n"
    );
}

fn fail(message: &str) -> ExitCode {
    eprintln!("agent-ctl: {message}");
    ExitCode::FAILURE
}

async fn run(socket: &str, command: &str, rest: &[String]) -> Result<(), String> {
    let stream = UnixStream::connect(socket)
        .await
        .map_err(|e| format!("connecting to {socket}: {e}"))?;

    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    let subscribe = if command == "watch" {
        vec![
            "connection".to_string(),
            "update_progress".into(),
            "update_installed".into(),
            "update_failed".into(),
            "reboot_pending".into(),
        ]
    } else {
        vec![]
    };

    send(
        &mut write_half,
        &Frame::Hello {
            name: "agent-ctl".into(),
            // Never a controller. Taking that slot would displace the
            // application, and a one-shot command has no business deciding
            // whether the device updates.
            role: Role::Observer,
            api: API_VERSION,
            subscribe,
        },
    )
    .await?;

    match next(&mut lines).await? {
        Frame::Welcome { agent_version, .. } if command == "watch" => {
            eprintln!("watching agent {agent_version}, ctrl-c to stop");
        }
        Frame::Welcome { .. } => {}
        Frame::Response { result, .. } => return Err(described(&result)),
        _ => return Err("the agent did not say hello back".into()),
    }

    if command == "watch" {
        while let Some(frame) = lines.next_line().await.map_err(|e| e.to_string())? {
            println!("{frame}");
        }

        return Ok(());
    }

    let method = match command {
        "status" => Method::Status,
        "mark-valid" => Method::MarkValid,
        "reboot" => Method::Reboot {
            reason: rest.first().cloned(),
        },
        other => return Err(format!("unknown command {other:?}")),
    };

    send(
        &mut write_half,
        &Frame::Request {
            id: "1".into(),
            method,
        },
    )
    .await?;

    // Skip any events that arrive before the answer — a subscription this
    // connection did not ask for cannot appear, but the agent is free to send
    // one and a client that choked on it would be brittle.
    loop {
        match next(&mut lines).await? {
            Frame::Response { result, .. } => {
                return match result {
                    Response::Ok { result } => {
                        let rendered =
                            serde_json::to_string_pretty(&result).unwrap_or_else(|_| "{}".into());

                        if rendered != "{}" {
                            println!("{rendered}");
                        }

                        Ok(())
                    }
                    error => Err(described(&error)),
                }
            }
            Frame::Event { .. } => continue,
            _ => return Err("unexpected frame while waiting for a reply".into()),
        }
    }
}

fn described(response: &Response) -> String {
    match response {
        Response::Err { error } => format!("{}: {}", error.code, error.message),
        Response::Ok { .. } => "unexpected success".into(),
    }
}

async fn send(
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
    frame: &Frame,
) -> Result<(), String> {
    let mut line = serde_json::to_string(frame).map_err(|e| e.to_string())?;
    line.push('\n');

    write_half
        .write_all(line.as_bytes())
        .await
        .map_err(|e| e.to_string())
}

async fn next(
    lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
) -> Result<Frame, String> {
    let line = lines
        .next_line()
        .await
        .map_err(|e| e.to_string())?
        .ok_or("the agent closed the connection")?;

    serde_json::from_str(&line).map_err(|e| format!("undecodable frame: {e}"))
}
