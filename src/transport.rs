//! The websocket to NervesHub.

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use crate::config::{Config, Identity};
use crate::error::Error;
use crate::message::{Message, SERIALIZER_VSN};
use crate::shared_secret::SharedSecret;

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

pub struct Transport {
    socket: Socket,
}

impl Transport {
    /// Connect and complete the websocket handshake.
    ///
    /// Authentication happens here and only here: NervesHub decides who a
    /// device is from the TLS client certificate or the `x-nh-*` headers on
    /// this handshake, so a socket that opens at all is one that authenticated.
    pub async fn connect(config: &Config, identifier: &str) -> Result<Self, Error> {
        let scheme = if config.server.tls { "wss" } else { "ws" };

        let path = config.server.path.trim_end_matches('/');

        let url = format!(
            "{scheme}://{}:{}{path}/websocket?vsn={SERIALIZER_VSN}",
            config.server.host, config.server.port
        );

        log::debug!("connecting to {url}");

        let mut request = url
            .into_client_request()
            .map_err(|e| Error::Connection(format!("building request: {e}")))?;

        if let Identity::SharedSecret {
            product_key,
            product_secret,
            ..
        } = &config.identity
        {
            let secret = SharedSecret::new(product_key.clone(), product_secret.clone());

            // Seconds since the epoch. NervesHub rejects a signature older than
            // its max age, so a device with a wrong clock fails in a way that
            // reads as a bad secret rather than as a bad clock.
            let signed_at = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| Error::Identity(format!("clock is before the epoch: {e}")))?
                .as_secs() as i64;

            for (name, value) in secret.headers(identifier, signed_at) {
                let value = HeaderValue::from_str(&value)
                    .map_err(|e| Error::Connection(format!("header {name}: {e}")))?;

                request.headers_mut().insert(
                    tokio_tungstenite::tungstenite::http::header::HeaderName::from_bytes(
                        name.as_bytes(),
                    )
                    .map_err(|e| Error::Connection(format!("header {name}: {e}")))?,
                    value,
                );
            }
        }

        let connector = tls_connector(config)?;

        let (socket, response) =
            tokio_tungstenite::connect_async_tls_with_config(request, None, false, connector)
                .await
                .map_err(|e| Error::Connection(e.to_string()))?;

        log::debug!("websocket handshake returned {}", response.status());

        Ok(Self { socket })
    }

    pub async fn send(&mut self, message: &Message) -> Result<(), Error> {
        let encoded = message.encode()?;
        log::trace!("-> {encoded}");

        self.socket
            .send(WsMessage::Text(encoded))
            .await
            .map_err(|e| Error::Connection(e.to_string()))
    }

    /// The next frame, or `None` when the server closed the socket.
    ///
    /// Pings are answered by tungstenite and skipped here; anything that is not
    /// a text frame is skipped too, since the JSON serializer never sends one.
    pub async fn recv(&mut self) -> Result<Option<Message>, Error> {
        loop {
            match self.socket.next().await {
                None | Some(Ok(WsMessage::Close(_))) => return Ok(None),

                Some(Ok(WsMessage::Text(raw))) => {
                    log::trace!("<- {raw}");

                    return Message::decode(&raw)
                        .map(Some)
                        .map_err(|e| Error::Connection(format!("decoding {raw}: {e}")));
                }

                Some(Ok(_)) => continue,

                Some(Err(e)) => return Err(Error::Connection(e.to_string())),
            }
        }
    }
}

/// The PEM certificates in `ca_certificate`, if one is configured.
///
/// Shared with the HTTP client that downloads firmware. The socket and the
/// download are two different connections to the same deployment, and trusting
/// one without the other is not a safer device, only one that fails later and
/// less obviously.
pub fn extra_root_certificates(config: &Config) -> Result<Vec<Vec<u8>>, Error> {
    let Some(path) = &config.server.ca_certificate else {
        return Ok(Vec::new());
    };

    let pem = std::fs::read(path)
        .map_err(|e| Error::Connection(format!("reading {}: {e}", path.display())))?;

    let certificates = rustls_pemfile::certs(&mut pem.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| Error::Connection(format!("parsing {}: {e}", path.display())))?;

    if certificates.is_empty() {
        return Err(Error::Connection(format!(
            "{} contains no certificates",
            path.display()
        )));
    }

    Ok(certificates.into_iter().map(|c| c.to_vec()).collect())
}

fn tls_connector(config: &Config) -> Result<Option<Connector>, Error> {
    if !config.server.tls {
        return Ok(None);
    }

    // The same configuration the HTTP client uses. See `crate::tls`.
    Ok(Some(Connector::Rustls(crate::tls::client_config(config)?)))
}
