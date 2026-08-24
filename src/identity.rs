//! Working out what this device calls itself.

use std::process::Command;

use crate::config::Identifier;
use crate::error::Error;

/// Resolve the identifier once, at startup.
///
/// A failure here is fatal on purpose. With a shared secret NervesHub registers
/// an unknown identifier on first connection, so a device that falls back to
/// some default on a failed read does not error — it quietly registers as the
/// wrong device, and every device that hits the same failure registers as the
/// same one.
pub fn resolve(identifier: &Identifier) -> Result<String, Error> {
    let raw = match identifier {
        Identifier::Literal(value) => value.clone(),

        Identifier::File(path) => std::fs::read_to_string(path)
            .map_err(|e| Error::Identity(format!("reading {}: {e}", path.display())))?,

        Identifier::Command(command) => {
            let output = Command::new("sh")
                .arg("-c")
                .arg(command)
                .output()
                .map_err(|e| Error::Identity(format!("running {command:?}: {e}")))?;

            if !output.status.success() {
                return Err(Error::Identity(format!(
                    "{command:?} exited with {}: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }

            String::from_utf8_lossy(&output.stdout).into_owned()
        }
    };

    // Device tree serial numbers are NUL-terminated, and a file written by a
    // shell script has a trailing newline. Both would otherwise become part of
    // the identifier and show up in the UI as a device that looks right and
    // will not match anything typed by hand.
    let cleaned = raw
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .trim_end_matches('\0')
        .to_string();

    if cleaned.is_empty() {
        return Err(Error::Identity("identifier resolved to nothing".into()));
    }

    Ok(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_gives_its_first_line_trimmed() {
        let id = resolve(&Identifier::Command("printf 'abc123\\nignored\\n'".into())).unwrap();
        assert_eq!(id, "abc123");
    }

    #[test]
    fn an_empty_result_is_an_error() {
        assert!(resolve(&Identifier::Command("true".into())).is_err());
    }

    #[test]
    fn a_failing_command_is_an_error() {
        assert!(resolve(&Identifier::Command("exit 1".into())).is_err());
    }
}
