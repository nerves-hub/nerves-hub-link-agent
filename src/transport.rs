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

    // Named explicitly rather than taken from the process default. rustls has
    // no default provider unless something installs one, and the failure when
    // nothing has is a panic inside the builder rather than an error here.
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());

    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Connection(format!("building the TLS configuration: {e}")))?;

    let config = if config.server.danger_accept_invalid_certs {
        log::warn!(
            "TLS certificate verification is OFF. The connection is encrypted against \
             nobody in particular — anything on the path can present its own certificate \
             and read the shared secret from the handshake."
        );

        builder
            .dangerous()
            .with_custom_certificate_verifier(std::sync::Arc::new(AcceptAnything(provider)))
            .with_no_client_auth()
    } else {
        let mut roots = rustls::RootCertStore::empty();

        // The platform trust store, so a device that already trusts a private
        // CA needs nothing configured here.
        let native = rustls_native_certs::load_native_certs();

        for error in &native.errors {
            log::debug!("native certificates: {error}");
        }

        for certificate in native.certs {
            let _ = roots.add(certificate);
        }

        for certificate in extra_root_certificates(config)? {
            roots
                .add(certificate.into())
                .map_err(|e| Error::Connection(format!("adding the configured CA: {e}")))?;
        }

        if roots.is_empty() {
            return Err(Error::Connection(
                "no trusted roots: the platform store is empty and no ca_certificate is set".into(),
            ));
        }

        builder.with_root_certificates(roots).with_no_client_auth()
    };

    Ok(Some(Connector::Rustls(std::sync::Arc::new(config))))
}

/// A verifier that accepts every certificate, for `danger_accept_invalid_certs`.
///
/// rustls has no switch for this because there is no safe use of it. It exists
/// here for one case — a NervesHub on a laptop with a self-signed certificate —
/// and the signature checks below are still real: the handshake has to be
/// internally consistent, it just is not tied to any identity.
#[derive(Debug)]
struct AcceptAnything(std::sync::Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptAnything {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
