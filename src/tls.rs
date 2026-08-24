//! One TLS configuration, for both the socket and the firmware download.
//!
//! They are two connections to the same deployment, and trusting one without
//! the other is not a safer device, only one that fails later and less
//! obviously. That is not hypothetical: `ca_certificate` was once wired into
//! the websocket alone, so a device with a private CA joined, reported itself
//! healthy, and then failed every download against a certificate it had been
//! configured to accept.

use std::sync::Arc;

use crate::config::Config;
use crate::error::Error;

/// The PEM certificates in `ca_certificate`, if one is configured.
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

/// The platform trust store and nothing else.
///
/// For a caller with no agent configuration to hand: the integration tests
/// drive an updater directly, and a fabricated `Config` there would be a
/// second place for the TLS defaults to live.
pub fn native_roots() -> Result<Arc<rustls::ClientConfig>, Error> {
    let (builder, _) = base()?;
    let mut roots = rustls::RootCertStore::empty();

    let native = rustls_native_certs::load_native_certs();

    for certificate in native.certs {
        let _ = roots.add(certificate);
    }

    Ok(Arc::new(
        builder.with_root_certificates(roots).with_no_client_auth(),
    ))
}

type Verifierless = rustls::ConfigBuilder<rustls::ClientConfig, rustls::WantsVerifier>;

fn base() -> Result<(Verifierless, Arc<rustls::crypto::CryptoProvider>), Error> {
    // Named explicitly rather than taken from the process default. rustls has
    // no default provider unless something installs one, and the failure when
    // nothing has is a panic inside the builder rather than an error here.
    let provider = Arc::new(rustls::crypto::ring::default_provider());

    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Connection(format!("building the TLS configuration: {e}")))?;

    Ok((builder, provider))
}

/// Build the client configuration both transports use.
pub fn client_config(config: &Config) -> Result<Arc<rustls::ClientConfig>, Error> {
    let (builder, provider) = base()?;

    let built = if config.server.danger_accept_invalid_certs {
        log::warn!(
            "TLS certificate verification is OFF. The connection is encrypted against \
             nobody in particular — anything on the path can present its own certificate \
             and read the shared secret from the handshake."
        );

        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnything(provider)))
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

    Ok(Arc::new(built))
}

/// A verifier that accepts every certificate, for `danger_accept_invalid_certs`.
///
/// rustls has no switch for this because there is no safe use of it. It exists
/// here for one case — a NervesHub on a laptop with a self-signed certificate —
/// and the signature checks below are still real: the handshake has to be
/// internally consistent, it just is not tied to any identity.
#[derive(Debug)]
struct AcceptAnything(Arc<rustls::crypto::CryptoProvider>);

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
