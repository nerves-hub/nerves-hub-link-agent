//! Identities the device holds on networks NervesHub does not run.
//!
//! An iroh endpoint id, a Tailscale or NetBird peer key, a WireGuard public
//! key. NervesHub asks once on attach and does not poll — an identity is
//! long-lived by construction — though a device whose details have moved can
//! report again at any time.
//!
//! ```text
//! <- network_identity:request   {}
//! -> network_identity:report    {"identities": [{"service": .., "identifier": ..}]}
//! ```
//!
//! # Why this is configured rather than detected
//!
//! Four services, each with its own CLI, its own output format, and its own
//! version skew. An agent that tried to know all of them would be wrong about
//! one of them within a release, and wrong in the direction of reporting a
//! confidently incorrect key.
//!
//! So the agent is told where to look. An identity is either a literal value or
//! a command to run, and a command that emits JSON can be pointed at a field so
//! that the common cases need no `jq` on the device.

use serde_json::{json, Value};

use crate::config::{IdentitySource, NetworkIdentityConfig};

pub struct NetworkIdentity {
    sources: Vec<IdentitySource>,
}

impl NetworkIdentity {
    pub fn new(config: &NetworkIdentityConfig) -> Self {
        Self {
            sources: config.identities.clone(),
        }
    }

    /// Collect every identity that resolves, in the shape `report` expects.
    ///
    /// A source that fails is logged and skipped rather than failing the
    /// report. A device running Tailscale but not WireGuard should not lose its
    /// Tailscale identity because `wg` is not installed — and on a device where
    /// nothing resolves, an empty list is still a true answer.
    pub async fn report(&self) -> Value {
        let mut identities = Vec::new();

        for source in &self.sources {
            match resolve(source).await {
                Ok(identity) => identities.push(identity),
                Err(e) => log::warn!("network_identity: {} — {e}", source.service),
            }
        }

        json!({ "identities": identities })
    }
}

async fn resolve(source: &IdentitySource) -> Result<Value, String> {
    let identifier = match (&source.identifier, &source.command) {
        (Some(identifier), _) => identifier.clone(),

        (None, Some(command)) => {
            let raw = run(command).await?;

            match &source.json_pointer {
                Some(pointer) => {
                    let value: Value = serde_json::from_str(&raw)
                        .map_err(|e| format!("{command:?} did not emit JSON: {e}"))?;

                    value
                        .pointer(pointer)
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            format!("{pointer} is not a string in the output of {command:?}")
                        })?
                        .to_string()
                }
                // The first line, not the whole output: a CLI that prints a key
                // followed by a blank line is the normal case, and a trailing
                // newline in an identifier is the kind of thing that only shows
                // up as a mismatch much later.
                None => raw.lines().next().unwrap_or("").trim().to_string(),
            }
        }

        (None, None) => return Err("has neither an identifier nor a command".into()),
    };

    if identifier.is_empty() {
        return Err("resolved to nothing".into());
    }

    let mut identity = json!({
        "service": source.service,
        "identifier": identifier,
    });

    if let Some(object) = identity.as_object_mut() {
        if let Some(instance) = &source.instance {
            object.insert("instance".into(), json!(instance));
        }

        if !source.details.is_empty() {
            object.insert("details".into(), json!(source.details));
        }
    }

    Ok(identity)
}

async fn run(command: &str) -> Result<String, String> {
    let output = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .output()
        .await
        .map_err(|e| format!("running {command:?}: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "{command:?} exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn source(service: &str) -> IdentitySource {
        IdentitySource {
            service: service.into(),
            identifier: None,
            command: None,
            json_pointer: None,
            instance: None,
            details: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn a_literal_identifier_is_reported_as_is() {
        let mut s = source("iroh");
        s.identifier = Some("abc123".into());

        let identity = resolve(&s).await.unwrap();

        assert_eq!(identity["service"], "iroh");
        assert_eq!(identity["identifier"], "abc123");
    }

    #[tokio::test]
    async fn a_command_gives_its_first_line() {
        let mut s = source("wireguard");
        s.command = Some("printf 'thekey=\\n\\n'".into());

        let identity = resolve(&s).await.unwrap();

        assert_eq!(identity["identifier"], "thekey=");
    }

    #[tokio::test]
    async fn a_json_pointer_reaches_into_the_output() {
        let mut s = source("tailscale");
        s.command = Some(r#"printf '{"Self":{"PublicKey":"nodekey:abc"}}'"#.into());
        s.json_pointer = Some("/Self/PublicKey".into());

        let identity = resolve(&s).await.unwrap();

        assert_eq!(identity["identifier"], "nodekey:abc");
    }

    #[tokio::test]
    async fn a_missing_pointer_is_an_error_not_an_empty_identity() {
        let mut s = source("tailscale");
        s.command = Some(r#"printf '{"Self":{}}'"#.into());
        s.json_pointer = Some("/Self/PublicKey".into());

        assert!(resolve(&s).await.is_err());
    }

    #[tokio::test]
    async fn one_failing_source_does_not_lose_the_others() {
        let mut working = source("iroh");
        working.identifier = Some("abc".into());

        let broken = source("wireguard");

        let identity = NetworkIdentity {
            sources: vec![broken, working],
        };

        let report = identity.report().await;
        let identities = report["identities"].as_array().unwrap();

        assert_eq!(identities.len(), 1);
        assert_eq!(identities[0]["service"], "iroh");
    }
}
