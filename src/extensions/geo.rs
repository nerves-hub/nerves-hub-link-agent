//! Where the device is.
//!
//! Answered only when NervesHub asks, which is on attach and then on whatever
//! interval the platform is configured for. Nothing here polls: a fleet that
//! decides for itself how often to report is exactly what the extension
//! mechanism exists to prevent.

use serde_json::{json, Value};

use crate::config::GeoSource;
use crate::error::Error;

pub struct Geo {
    source: GeoSource,
}

impl Geo {
    pub fn new(source: GeoSource) -> Self {
        Self { source }
    }

    /// A location, in the shape `geo:location:update` expects.
    pub async fn locate(&self, client: &reqwest::Client) -> Result<Value, Error> {
        match &self.source {
            GeoSource::Fixed {
                latitude,
                longitude,
                accuracy,
            } => Ok(json!({
                "latitude": latitude,
                "longitude": longitude,
                "accuracy": accuracy,
                // Distinguished from "geoip" on purpose. Someone reading the map
                // needs to know whether a pin is a measurement or an inference
                // from an IP address, which can be a different country.
                "source": "configured",
            })),

            GeoSource::Command(command) => run_command(command).await,

            GeoSource::Whenwhere { url } => {
                let base = url
                    .as_deref()
                    .unwrap_or("http://whenwhere.nerves-project.org");

                whenwhere(client, base).await
            }
        }
    }
}

/// Ask `whenwhere` where this device appears to be.
///
/// The nonce is not decoration. The service defaults to plain HTTP, so a
/// captive portal or anything else on the path can answer with a page of its
/// own, and that answer would otherwise be taken as a position. Requiring the
/// nonce back in a header means an answer that was not produced for this
/// request is rejected.
async fn whenwhere(client: &reqwest::Client, base: &str) -> Result<Value, Error> {
    let nonce = nonce();

    let response = client
        .get(format!("{base}/?nonce={nonce}"))
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|e| Error::Connection(format!("whenwhere: {e}")))?;

    let echoed = response
        .headers()
        .get("x-nonce")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();

    if echoed != nonce {
        return Err(Error::Connection(
            "whenwhere answered without a matching nonce — something on the path replied".into(),
        ));
    }

    let body: Value = response
        .json()
        .await
        .map_err(|e| Error::Connection(format!("whenwhere: {e}")))?;

    let latitude = number(&body, "latitude");
    let longitude = number(&body, "longitude");

    match (latitude, longitude) {
        (Some(latitude), Some(longitude)) => Ok(json!({
            "latitude": latitude,
            "longitude": longitude,
            "source": "geoip",
            "address": body.get("address"),
            "time_zone": body.get("time_zone"),
        })),
        _ => Err(Error::Connection(
            "whenwhere answered without a position".into(),
        )),
    }
}

async fn run_command(command: &str) -> Result<Value, Error> {
    let output = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .output()
        .await
        .map_err(|e| Error::Ipc(format!("geo command {command:?}: {e}")))?;

    if !output.status.success() {
        return Err(Error::Ipc(format!(
            "geo command {command:?} exited with {}",
            output.status
        )));
    }

    let mut value: Value = serde_json::from_slice(&output.stdout)?;

    // A GPS says where it is, not how it knows. Filling in the source here
    // means the map can tell a fix from a GeoIP guess without the command
    // author having to know the field exists.
    if value.get("source").is_none() {
        if let Some(object) = value.as_object_mut() {
            object.insert("source".into(), json!("gps"));
        }
    }

    Ok(value)
}

/// Whenwhere reports coordinates as strings; be liberal about which we get.
fn number(body: &Value, key: &str) -> Option<f64> {
    match body.get(key)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

/// Enough randomness to make a replayed answer implausible, from the clock and
/// the address of a heap allocation. Not a secret, and not used as one — it
/// only has to be unpredictable to something answering the request.
fn nonce() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    let boxed = Box::new(0u8);
    let address = Box::into_raw(boxed) as usize;

    // Safety: reclaiming the allocation made just above.
    unsafe { drop(Box::from_raw(address as *mut u8)) };

    format!("{now:x}{address:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_fixed_location_says_it_was_configured() {
        let geo = Geo::new(GeoSource::Fixed {
            latitude: -41.28,
            longitude: 174.77,
            accuracy: Some(10.0),
        });

        let location = geo.locate(&reqwest::Client::new()).await.unwrap();

        assert_eq!(location["latitude"], -41.28);
        assert_eq!(location["source"], "configured");
    }

    #[tokio::test]
    async fn a_command_that_omits_the_source_is_labelled_gps() {
        let geo = Geo::new(GeoSource::Command(
            "printf '{\"latitude\": 1.0, \"longitude\": 2.0}'".into(),
        ));

        let location = geo.locate(&reqwest::Client::new()).await.unwrap();

        assert_eq!(location["source"], "gps");
    }

    #[tokio::test]
    async fn a_failing_command_is_an_error_not_a_position() {
        let geo = Geo::new(GeoSource::Command("exit 1".into()));

        assert!(geo.locate(&reqwest::Client::new()).await.is_err());
    }

    #[test]
    fn nonces_differ() {
        assert_ne!(nonce(), nonce());
    }
}
