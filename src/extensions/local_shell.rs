//! A shell on the device, for a NervesHub user.
//!
//! This is the only extension that lets someone reach *into* a device rather
//! than read from it, and the device does not get to ask who is on the other
//! end — the authorization happened in NervesHub. Whoever can open the shell
//! tab runs commands as whatever the agent runs as. That is why it is off by
//! default, behind its own build feature, and worth a deliberate decision per
//! fleet rather than a default anyone inherits.
//!
//! # Why a pty and not a pipe
//!
//! A shell on a pipe is not a shell. `isatty` is false, so it runs
//! non-interactively: no prompt, no job control, no line editing, and anything
//! that draws — `top`, `vi`, a progress bar — either refuses or renders as
//! garbage. Someone opening the tab would get a blinking cursor that silently
//! swallows what they type. So the shell gets a real pty, which also gives the
//! terminal a size to resize.
//!
//! # Threads
//!
//! `portable-pty` hands back a blocking reader and writer, so the read loop
//! lives on a blocking thread and pushes output through a channel. Wrapping the
//! file descriptor in async I/O would be less machinery, but it would be
//! machinery this agent has to keep correct across platforms, and the traffic
//! is one person typing.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use tokio::sync::mpsc;

use crate::config::LocalShellConfig;
use crate::error::Error;

pub struct Shell {
    master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
}

impl Shell {
    /// Start a shell, streaming its output into the returned receiver.
    ///
    /// The receiver closing is how the caller learns the shell exited — there
    /// is no separate exit event, because a shell that has gone is
    /// indistinguishable from one whose output stopped, and the user needs to
    /// be told the same thing either way.
    pub fn spawn(config: &LocalShellConfig) -> Result<(Self, mpsc::Receiver<String>), Error> {
        let refuse = |message: String| Error::Ipc(format!("local_shell: {message}"));

        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| refuse(format!("opening a pty: {e}")))?;

        let mut command = CommandBuilder::new(&config.command);

        // Without TERM the shell assumes a dumb terminal and stops emitting the
        // escape sequences the browser terminal is there to render.
        command.env("TERM", "xterm-256color");

        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|e| refuse(format!("starting {}: {e}", config.command)))?;

        // The slave has to be dropped here. Holding it keeps a writer open on
        // the pty, so when the shell exits the master never sees EOF and the
        // read loop hangs forever on a shell that is already gone.
        drop(pair.slave);

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| refuse(format!("cloning the pty reader: {e}")))?;

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| refuse(format!("taking the pty writer: {e}")))?;

        let (tx, rx) = mpsc::channel(64);

        spawn_reader(reader, tx, config.chunk_bytes);

        log::warn!("local_shell: started {}", config.command);

        Ok((
            Self {
                master: pair.master,
                writer: Arc::new(Mutex::new(writer)),
                child: Arc::new(Mutex::new(child)),
            },
            rx,
        ))
    }

    /// Send keystrokes to the shell.
    pub async fn input(&self, data: String) -> Result<(), Error> {
        let writer = Arc::clone(&self.writer);

        tokio::task::spawn_blocking(move || {
            let mut writer = writer.lock().map_err(|_| "the pty writer was poisoned")?;

            writer
                .write_all(data.as_bytes())
                .and_then(|()| writer.flush())
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| Error::Ipc(format!("local_shell: {e}")))?
        .map_err(|e| Error::Ipc(format!("local_shell: writing to the pty: {e}")))
    }

    /// Tell the shell its terminal changed size, so full-screen programs redraw
    /// to fit rather than wrapping against a size nobody is looking at.
    pub fn resize(&self, rows: u16, cols: u16) -> Result<(), Error> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| Error::Ipc(format!("local_shell: resizing the pty: {e}")))
    }
}

impl Drop for Shell {
    /// Kill the shell when the session that asked for it goes.
    ///
    /// Without this, a dropped connection leaves a shell running with nobody
    /// attached — and the next `request_shell` starts a second one beside it.
    /// Over a flaky link that accumulates.
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }

        log::warn!("local_shell: shell ended");
    }
}

/// Read the pty on a blocking thread, forwarding whole UTF-8 sequences.
///
/// The carry buffer matters: a read can land in the middle of a multi-byte
/// character, and decoding each read independently turns every such boundary
/// into a replacement character. Which looks exactly like a bug in the shell.
fn spawn_reader(mut reader: Box<dyn Read + Send>, tx: mpsc::Sender<String>, chunk_bytes: usize) {
    std::thread::spawn(move || {
        let mut buffer = vec![0u8; chunk_bytes.max(256)];
        let mut carry: Vec<u8> = Vec::new();

        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,

                Ok(read) => {
                    carry.extend_from_slice(&buffer[..read]);

                    let text = take_complete(&mut carry);

                    if text.is_empty() {
                        continue;
                    }

                    if tx.blocking_send(text).is_err() {
                        break;
                    }
                }

                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,

                Err(e) => {
                    log::debug!("local_shell: pty read ended: {e}");
                    break;
                }
            }
        }
    });
}

/// Take everything from `carry` that decodes, leaving a trailing partial
/// character behind for the next read.
///
/// Split out from the read loop because it is the part with the subtle bug in
/// it and the only part a test can reach without a pty: a read can land in the
/// middle of a multi-byte character, and decoding each read on its own turns
/// every such boundary into a replacement character.
fn take_complete(carry: &mut Vec<u8>) -> String {
    match std::str::from_utf8(carry) {
        Ok(text) => {
            let text = text.to_string();
            carry.clear();
            text
        }
        Err(e) => {
            let valid = e.valid_up_to();
            let text = String::from_utf8_lossy(&carry[..valid]).into_owned();

            // Keep a trailing partial character for the next read -- unless it
            // is not a partial character at all but genuinely invalid bytes,
            // which would otherwise wedge the loop forever.
            *carry = match e.error_len() {
                None => carry[valid..].to_vec(),
                Some(len) => carry[valid + len..].to_vec(),
            };

            text
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_whole_string_is_taken_and_the_buffer_emptied() {
        let mut carry = "hello".as_bytes().to_vec();

        assert_eq!(take_complete(&mut carry), "hello");
        assert!(carry.is_empty());
    }

    /// The case the carry buffer exists for. Without it each half decodes on
    /// its own and the reader emits two replacement characters instead of one
    /// pound sign, which reads as a bug in the shell rather than in the agent.
    #[test]
    fn a_character_split_across_two_reads_survives() {
        let pound = "£".as_bytes().to_vec();
        assert_eq!(pound.len(), 2);

        let mut carry = vec![b'a', pound[0]];

        // First read: only the ASCII byte is complete.
        assert_eq!(take_complete(&mut carry), "a");
        assert_eq!(carry, vec![pound[0]]);

        // Second read completes it.
        carry.push(pound[1]);
        assert_eq!(take_complete(&mut carry), "£");
        assert!(carry.is_empty());
    }

    /// Bytes that are not a partial character have to be consumed, or the loop
    /// retries the same prefix forever and the shell appears to hang.
    #[test]
    fn genuinely_invalid_bytes_are_dropped_rather_than_retried() {
        let mut carry = vec![b'a', 0xff, b'b'];

        let text = take_complete(&mut carry);

        assert!(text.starts_with('a'), "got {text:?}");
        assert!(
            carry.is_empty() || carry == vec![b'b'],
            "the invalid byte should not remain: {carry:?}"
        );
    }

    /// The pty itself, on whichever platform the tests run. Proves the three
    /// things the extension depends on and nothing else exercises: a shell
    /// starts, what is typed reaches it, and what it prints comes back.
    #[tokio::test]
    async fn a_shell_runs_a_command_and_returns_its_output() {
        let config = LocalShellConfig {
            enabled: true,
            command: "/bin/sh".into(),
            chunk_bytes: 4096,
        };

        let (shell, mut output) = Shell::spawn(&config).expect("a shell should start");

        shell
            .input("echo agent-shell-works\n".into())
            .await
            .expect("input should reach the pty");

        shell.resize(40, 100).expect("resize should be accepted");

        let mut seen = String::new();

        let found = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while let Some(chunk) = output.recv().await {
                seen.push_str(&chunk);

                // The pty echoes the typed line back as well, so the marker
                // appears twice; one is enough to prove the round trip.
                if seen.matches("agent-shell-works").count() >= 2 {
                    return true;
                }
            }

            false
        })
        .await
        .unwrap_or(false);

        assert!(found, "did not see the command output, got: {seen:?}");
    }

    /// Dropping the session kills the shell. Without it a flaky link
    /// accumulates orphaned shells, one per reconnect.
    #[tokio::test]
    async fn dropping_the_shell_closes_its_output() {
        let config = LocalShellConfig {
            enabled: true,
            command: "/bin/sh".into(),
            chunk_bytes: 4096,
        };

        let (shell, mut output) = Shell::spawn(&config).expect("a shell should start");
        drop(shell);

        // The reader thread sees EOF once the child is gone and closes the
        // channel, which is how the agent learns the shell ended.
        let closed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while output.recv().await.is_some() {}
        })
        .await;

        assert!(closed.is_ok(), "the output channel should close");
    }
}
