//! Support scripts.
//!
//! An operator picks a script in NervesHub and it arrives here as text to run:
//!
//! ```text
//! <- scripts/run  {"text": "...", "ref": "AbC1"}
//! -> scripts/run  {"ref": "AbC1", "output": "...", "return": "...", "result": "ok"}
//! ```
//!
//! On a Nerves device the text is Elixir, evaluated in the running VM. Here
//! there is no VM to evaluate anything in, so a script is a shell script — which
//! is what a support script on a Linux device wants to be anyway. `journalctl`,
//! `systemctl status`, `ip addr`, `df`: the things someone reaches for when a
//! device is misbehaving are commands, not expressions.
//!
//! # A shebang wins
//!
//! The text is written to a file and run with the configured interpreter, `bash`
//! by default. If it starts with `#!` the file is made executable and run
//! directly instead, so a Python or `sh` script works without anything being
//! configured for it. Running via a file rather than `-c` also means an error
//! carries a line number.
//!
//! # Timeouts are not optional
//!
//! NervesHub drops the reference for a script after 15 seconds and stops
//! listening. A script that runs longer produces output nobody receives, and a
//! process that stays alive on the device with nothing watching it. So the agent
//! has its own, shorter, deadline, and kills the whole process group — not just
//! the child, which would leave anything the script backgrounded still running.
//!
//! # What this is
//!
//! Arbitrary shell, as whatever the agent runs as, chosen by whoever NervesHub
//! decided may run scripts against this device. That is the feature. It is worth
//! being clear-eyed that it is the same power as [`crate::extensions::
//! local_shell`], differing only in that someone wrote the commands down first.

use std::process::Stdio;

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

use crate::config::ScriptsConfig;

/// A finished script, ready to send back.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub reference: String,
    pub output: String,
    /// Shown by the server appended to `output`. Empty on success, so a script
    /// that printed what it meant to is not followed by noise.
    pub returned: String,
    pub result: &'static str,
}

impl Outcome {
    pub fn payload(&self) -> Value {
        json!({
            "ref": self.reference,
            "output": self.output,
            "return": self.returned,
            "result": self.result,
        })
    }

    fn failed(reference: String, message: String) -> Self {
        Self {
            reference,
            output: message,
            returned: String::new(),
            result: "error",
        }
    }
}

/// Run a script and describe what happened.
///
/// Never returns an error: every failure is an outcome to report. A script that
/// cannot be written to disk, or whose interpreter is missing, is a thing the
/// operator needs to read — reporting nothing would leave them watching a
/// spinner until the server's own timeout.
pub async fn run(
    config: &ScriptsConfig,
    reference: String,
    text: &str,
    env: &[(String, String)],
) -> Outcome {
    if !config.enabled {
        // Answered, not ignored. A device that silently drops scripts is
        // indistinguishable from one that has gone offline, and an operator
        // would keep trying.
        return Outcome::failed(
            reference,
            "support scripts are disabled on this device".into(),
        );
    }

    let directory = match tempdir(&config.work_dir).await {
        Ok(directory) => directory,
        Err(e) => return Outcome::failed(reference, format!("could not stage the script: {e}")),
    };

    let path = directory.join("script");

    if let Err(e) = write_script(&path, text, has_shebang(text)).await {
        let _ = tokio::fs::remove_dir_all(&directory).await;
        return Outcome::failed(reference, format!("could not stage the script: {e}"));
    }

    let outcome = execute(config, &reference, &path, has_shebang(text), env).await;

    let _ = tokio::fs::remove_dir_all(&directory).await;

    outcome
}

async fn execute(
    config: &ScriptsConfig,
    reference: &str,
    path: &std::path::Path,
    shebang: bool,
    env: &[(String, String)],
) -> Outcome {
    // What gets exec'd, and so what a failure to start has to name. With a
    // shebang that is the script itself: the configured interpreter is never
    // involved, and blaming it would send someone off to fix the wrong thing.
    let program = if shebang {
        path
    } else {
        std::path::Path::new(&config.interpreter)
    };

    let mut command = tokio::process::Command::new(program);

    if !shebang {
        command.arg(path);
    }

    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Merged into stdout rather than kept apart. A support script's
        // diagnostics are usually on stderr and its answer on stdout, and an
        // operator reading the result wants them interleaved in the order they
        // happened, not concatenated.
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    for (key, value) in env {
        command.env(key, value);
    }

    // Its own process group, so the timeout can kill everything the script
    // started rather than only the shell that started it.
    #[cfg(unix)]
    command.process_group(0);

    let child = match retry_while_busy(|| command.spawn()).await {
        Ok(child) => child,
        Err(e) => {
            return Outcome::failed(
                reference.to_string(),
                format!("could not run {}: {e}", program.display()),
            )
        }
    };

    let deadline = std::time::Duration::from_secs(config.timeout_secs);

    let finished = tokio::time::timeout(deadline, child.wait_with_output()).await;

    match finished {
        Ok(Ok(output)) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));

            let truncated = truncate(text, config.max_output_bytes);
            let code = output.status.code();

            match code {
                Some(0) => Outcome {
                    reference: reference.to_string(),
                    output: truncated,
                    returned: String::new(),
                    result: "ok",
                },
                Some(code) => Outcome {
                    reference: reference.to_string(),
                    output: truncated,
                    returned: format!("exited with status {code}"),
                    result: "error",
                },
                // Killed by a signal. `code()` is None on unix in that case, and
                // saying so is more useful than an invented number.
                None => Outcome {
                    reference: reference.to_string(),
                    output: truncated,
                    returned: "killed by a signal".into(),
                    result: "error",
                },
            }
        }

        Ok(Err(e)) => Outcome::failed(reference.to_string(), format!("script failed: {e}")),

        // kill_on_drop plus the process group means dropping the child here
        // takes the whole group with it.
        Err(_) => Outcome::failed(
            reference.to_string(),
            format!(
                "script did not finish within {}s and was killed",
                config.timeout_secs
            ),
        ),
    }
}

/// How many times to try starting a script whose file is busy.
const SPAWN_ATTEMPTS: u32 = 5;

/// The wait before the second try, doubling after each one: 150ms in all.
const FIRST_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(10);

/// Start a process, waiting out a file that is briefly busy.
///
/// Linux will not exec a file that anything has open for writing, and fails
/// with ETXTBSY, "Text file busy". The agent closes a script before running
/// it, but that is not enough on its own. If another thread starts a process
/// while the script is still open for writing (a second script, the update
/// tool, anything that spawns), the new child gets a copy of the descriptor
/// and keeps it until it execs. Close-on-exec closes it then, not before, and
/// until then the script cannot be run, through no fault of its own.
///
/// That window is short, so a few quick retries see it through. Only a script
/// run through its shebang is exec'd itself and can hit this; an interpreter
/// only reads the script, so it never fails this way. Any other error fails at
/// once, and so does a file still busy after the last try, because by then
/// something really does have it open.
async fn retry_while_busy<T>(
    mut attempt: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    let mut delay = FIRST_RETRY_DELAY;

    for _ in 1..SPAWN_ATTEMPTS {
        match attempt() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                tokio::time::sleep(delay).await;
                delay *= 2;
            }
            result => return result,
        }
    }

    attempt()
}

fn has_shebang(text: &str) -> bool {
    text.starts_with("#!")
}

/// Truncate from the *end*, keeping the start.
///
/// A script's first lines say what it was doing; a truncated tail is usually
/// repetition. Losing the start would leave an operator with the middle of
/// something and no idea what.
fn truncate(mut text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }

    // Cut on a character boundary, or `truncate` panics on multi-byte output.
    let mut cut = max;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }

    text.truncate(cut);
    text.push_str("\n\n[output truncated by nerves-hub-link-agent]");

    text
}

async fn write_script(
    path: &std::path::Path,
    text: &str,
    executable: bool,
) -> Result<(), std::io::Error> {
    let mut file = tokio::fs::File::create(path).await?;
    file.write_all(text.as_bytes()).await?;
    file.flush().await?;
    drop(file);

    if executable {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).await?;
        }
    }

    Ok(())
}

/// A private directory for one script.
///
/// Two scripts can be in flight at once and must not share a path. The clock
/// alone is not enough for that: `SystemTime::now()` is not guaranteed to
/// advance between two calls, and on a fast machine two scripts starting
/// together get the same reading and then the same directory — where the second
/// one's `create_dir_all` succeeds on the first one's directory and they
/// overwrite each other's `script` file.
///
/// So the name carries a counter as well, which cannot repeat within a process,
/// alongside the pid and clock which separate one process from another.
async fn tempdir(base: &std::path::Path) -> Result<std::path::PathBuf, std::io::Error> {
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    let unique = format!(
        "script-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );

    let directory = base.join(unique);

    tokio::fs::create_dir_all(&directory).await?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        tokio::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).await?;
    }

    Ok(directory)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ScriptsConfig {
        ScriptsConfig {
            enabled: true,
            work_dir: std::env::temp_dir(),
            interpreter: "bash".into(),
            // Generous on purpose. These tests spawn a real shell, and on a
            // machine busy compiling that can take seconds — a tight timeout
            // here fails the tests that are not about timeouts.
            timeout_secs: 60,
            max_output_bytes: 1024,
        }
    }

    #[tokio::test]
    async fn a_script_reports_its_output() {
        let outcome = run(&config(), "r1".into(), "echo hello", &[]).await;

        assert_eq!(outcome.result, "ok");
        assert_eq!(outcome.output.trim(), "hello");
        assert_eq!(outcome.returned, "");
        assert_eq!(outcome.reference, "r1");
    }

    #[tokio::test]
    async fn stderr_is_part_of_the_output() {
        let outcome = run(&config(), "r1".into(), "echo oops >&2", &[]).await;

        assert!(outcome.output.contains("oops"));
    }

    #[tokio::test]
    async fn a_non_zero_exit_is_an_error_with_the_status() {
        let outcome = run(&config(), "r1".into(), "echo partial; exit 3", &[]).await;

        assert_eq!(outcome.result, "error");
        assert!(outcome.output.contains("partial"));
        assert_eq!(outcome.returned, "exited with status 3");
    }

    #[tokio::test]
    async fn a_shebang_chooses_the_interpreter() {
        let mut config = config();
        config.interpreter = "definitely-not-a-real-interpreter".into();

        // Runs anyway, because the shebang means the file is executed directly.
        let outcome = run(&config, "r1".into(), "#!/bin/sh\necho via-shebang", &[]).await;

        assert_eq!(outcome.result, "ok", "{}", outcome.output);
        assert_eq!(outcome.output.trim(), "via-shebang");
    }

    #[tokio::test]
    async fn a_missing_interpreter_is_named() {
        let mut config = config();
        config.interpreter = "definitely-not-a-real-interpreter".into();

        let outcome = run(&config, "r1".into(), "echo hello", &[]).await;

        assert_eq!(outcome.result, "error");
        assert!(
            outcome
                .output
                .starts_with("could not run definitely-not-a-real-interpreter: "),
            "{}",
            outcome.output
        );
    }

    #[tokio::test]
    async fn a_shebang_script_that_cannot_start_names_the_script() {
        // The script is what was exec'd, so the script is what failed. Naming
        // the configured interpreter here would point at something that was
        // never run.
        let directory = tempfile::tempdir().unwrap();

        let mut config = config();
        config.work_dir = directory.path().to_path_buf();
        config.interpreter = "the-configured-interpreter".into();

        let outcome = run(
            &config,
            "r1".into(),
            "#!/definitely/not/a/real/interpreter\necho hello",
            &[],
        )
        .await;

        let named = format!("could not run {}", directory.path().display());

        assert_eq!(outcome.result, "error");
        assert!(outcome.output.starts_with(&named), "{}", outcome.output);
        assert!(outcome.output.contains("/script: "), "{}", outcome.output);
        assert!(
            !outcome.output.contains("the-configured-interpreter"),
            "{}",
            outcome.output
        );
    }

    fn busy() -> std::io::Error {
        std::io::ErrorKind::ExecutableFileBusy.into()
    }

    #[tokio::test]
    async fn a_busy_file_is_tried_again() {
        let mut attempts = 0;

        let result = retry_while_busy(|| {
            attempts += 1;

            if attempts < 3 {
                Err(busy())
            } else {
                Ok("started")
            }
        })
        .await;

        assert_eq!(result.unwrap(), "started");
        assert_eq!(attempts, 3);
    }

    #[tokio::test]
    async fn any_other_error_is_not_tried_again() {
        let mut attempts = 0;

        let result: std::io::Result<()> = retry_while_busy(|| {
            attempts += 1;
            Err(std::io::ErrorKind::NotFound.into())
        })
        .await;

        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::NotFound);
        assert_eq!(attempts, 1);
    }

    #[tokio::test]
    async fn a_file_that_stays_busy_is_given_up_on() {
        let mut attempts = 0;

        let result: std::io::Result<()> = retry_while_busy(|| {
            attempts += 1;
            Err(busy())
        })
        .await;

        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::ExecutableFileBusy
        );
        assert_eq!(attempts, SPAWN_ATTEMPTS);
    }

    /// The kernel's side of the retry, which the tests above can only assume:
    /// exec of a file open for writing really does fail, and with the error
    /// kind `retry_while_busy` looks for.
    ///
    /// The descriptor held here stands in for one some other thread's child
    /// inherited. It is closed after the first attempt, so that attempt is
    /// certain to fail and a later one to succeed, with no race to win.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_script_open_for_writing_runs_once_it_is_closed() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("script");

        std::fs::write(&path, "#!/bin/sh\necho ran\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();

        let mut writer = Some(std::fs::OpenOptions::new().write(true).open(&path).unwrap());

        let mut command = tokio::process::Command::new(&path);
        command.stdout(Stdio::piped());

        let mut failures = Vec::new();

        let child = retry_while_busy(|| {
            let spawned = command.spawn();

            if let Err(e) = &spawned {
                failures.push(e.kind());
            }

            writer = None;
            spawned
        })
        .await
        .unwrap();

        // More than one failure is possible, if another test's child briefly
        // inherited the descriptor too. Every one of them must be "busy".
        assert!(!failures.is_empty(), "the first exec should have failed");
        assert!(
            failures
                .iter()
                .all(|kind| *kind == std::io::ErrorKind::ExecutableFileBusy),
            "{failures:?}"
        );

        let output = child.wait_with_output().await.unwrap();

        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ran");
    }

    #[tokio::test]
    async fn a_script_that_overruns_is_killed_and_says_so() {
        let mut config = config();
        config.timeout_secs = 1;

        let outcome = run(&config, "r1".into(), "sleep 30", &[]).await;

        assert_eq!(outcome.result, "error");
        assert!(outcome.output.contains("did not finish"));
    }

    #[tokio::test]
    async fn disabled_answers_rather_than_going_quiet() {
        let mut config = config();
        config.enabled = false;

        let outcome = run(&config, "r1".into(), "echo hello", &[]).await;

        assert_eq!(outcome.result, "error");
        assert!(outcome.output.contains("disabled"));
    }

    #[tokio::test]
    async fn the_environment_is_available_to_the_script() {
        let env = vec![("NERVES_HUB_DEVICE_IDENTIFIER".into(), "dev-01".into())];

        let outcome = run(
            &config(),
            "r1".into(),
            "echo $NERVES_HUB_DEVICE_IDENTIFIER",
            &env,
        )
        .await;

        assert_eq!(outcome.output.trim(), "dev-01");
    }

    #[tokio::test]
    async fn concurrent_scripts_do_not_share_a_directory() {
        // The failure this guards against is not a crash: two scripts writing
        // the same file means one of them runs the other's text and reports it
        // as its own result.
        let config = config();

        let runs = (0..8).map(|i| {
            let config = config.clone();

            tokio::spawn(async move {
                run(&config, format!("r{i}"), &format!("echo script-{i}"), &[]).await
            })
        });

        for (i, handle) in runs.enumerate() {
            let outcome = handle.await.unwrap();

            assert_eq!(outcome.result, "ok", "run {i}: {}", outcome.output);
            assert_eq!(outcome.output.trim(), format!("script-{i}"));
        }
    }

    #[test]
    fn truncation_keeps_the_beginning() {
        let truncated = truncate("a".repeat(100), 10);

        assert!(truncated.starts_with("aaaaaaaaaa"));
        assert!(truncated.contains("truncated"));
    }

    #[test]
    fn truncation_does_not_split_a_character() {
        // Every char is 3 bytes, so a 10-byte cut lands mid-character.
        let truncated = truncate("日".repeat(10), 10);

        assert!(truncated.starts_with("日日日"));
    }
}
