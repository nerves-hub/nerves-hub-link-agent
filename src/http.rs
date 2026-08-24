//! The HTTP client, over hyper.
//!
//! The agent makes two kinds of request: a streaming GET for firmware, which
//! runs for as long as the download takes and is fed straight into an updater's
//! stdin, and a small JSON GET for the geo extension. RAUC does its own
//! transfer and never comes through here.
//!
//! That is a narrow enough need to be worth owning. reqwest would do it in
//! fewer lines and cost the agent every released Yocto: it carries an optional
//! QUIC stack whose `chacha20` is edition 2024, and a lockfile records the
//! maximal resolution rather than the enabled one, so vendoring needs a cargo
//! newer than any of them ships even though nothing compiles a byte of it.
//!
//! # Redirects
//!
//! Followed, up to [`MAX_REDIRECTS`], because NervesHub hands out a URL that
//! redirects to wherever the firmware actually lives. hyper does not follow
//! them and reqwest did, so this is the one behaviour that had to be rebuilt
//! rather than merely rewired.

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::body::Incoming;
use hyper::header::{HeaderName, HeaderValue, LOCATION};
use hyper::{HeaderMap, Request, StatusCode, Uri};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client as Legacy;
use hyper_util::rt::TokioExecutor;

use crate::config::Config;
use crate::error::Error;

/// Enough for a presigned URL that redirects to object storage, and few enough
/// that a loop is an error rather than a hang.
const MAX_REDIRECTS: usize = 10;

type Connector = hyper_rustls::HttpsConnector<HttpConnector>;

#[derive(Clone)]
pub struct Client {
    inner: Legacy<Connector, Empty<Bytes>>,
}

impl Client {
    pub fn new(config: &Config) -> Result<Self, Error> {
        Self::with_tls(crate::tls::client_config(config)?)
    }

    /// A client trusting the platform store and nothing more. For callers with
    /// no agent configuration -- tests, and the integration tests that drive an
    /// updater directly.
    pub fn with_native_roots() -> Result<Self, Error> {
        Self::with_tls(crate::tls::native_roots()?)
    }

    fn with_tls(tls: std::sync::Arc<rustls::ClientConfig>) -> Result<Self, Error> {
        // `https_or_http`, not `https_only`: a NervesHub in development serves
        // firmware over plain HTTP, and refusing it here would make the
        // sandbox setup in examples/local.toml impossible to run.
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config((*tls).clone())
            .https_or_http()
            .enable_http1()
            .build();

        Ok(Self {
            inner: Legacy::builder(TokioExecutor::new()).build(connector),
        })
    }

    pub async fn get(&self, url: &str) -> Result<Response, Error> {
        self.get_with(url, &[]).await
    }

    /// A GET, following redirects.
    pub async fn get_with(&self, url: &str, headers: &[(&str, &str)]) -> Result<Response, Error> {
        let mut uri: Uri = url
            .parse()
            .map_err(|e| Error::Download(format!("{url} is not a url: {e}")))?;

        for _ in 0..MAX_REDIRECTS {
            let response = self.send(&uri, headers).await?;

            let Some(location) = redirect_target(&response) else {
                return Ok(response);
            };

            uri = resolve(&uri, location)?;
            log::debug!("following redirect to {uri}");
        }

        Err(Error::Download(format!(
            "{url} redirected more than {MAX_REDIRECTS} times"
        )))
    }

    async fn send(&self, uri: &Uri, headers: &[(&str, &str)]) -> Result<Response, Error> {
        let mut request = Request::builder().uri(uri).method("GET");

        for (name, value) in headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|e| Error::Download(format!("header {name}: {e}")))?;
            let value = HeaderValue::from_str(value)
                .map_err(|e| Error::Download(format!("header value: {e}")))?;

            request = request.header(name, value);
        }

        let request = request
            .body(Empty::<Bytes>::new())
            .map_err(|e| Error::Download(format!("building the request: {e}")))?;

        let response = self
            .inner
            .request(request)
            .await
            .map_err(|e| Error::Download(e.to_string()))?;

        let (parts, body) = response.into_parts();

        Ok(Response {
            status: parts.status,
            headers: parts.headers,
            body,
        })
    }
}

pub struct Response {
    status: StatusCode,
    headers: HeaderMap,
    body: Incoming,
}

impl Response {
    pub fn status(&self) -> StatusCode {
        self.status
    }

    pub fn is_success(&self) -> bool {
        self.status.is_success()
    }

    /// `Content-Length`, when the server sent one. Absent for a chunked
    /// response, which is why every caller has a path for not knowing.
    pub fn content_length(&self) -> Option<u64> {
        self.headers
            .get(hyper::header::CONTENT_LENGTH)?
            .to_str()
            .ok()?
            .parse()
            .ok()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)?.to_str().ok()
    }

    /// The next chunk of the body, or `None` at the end.
    ///
    /// Frames that are not data — trailers — are skipped rather than ending
    /// the stream, because a trailer arriving before the last data frame would
    /// otherwise truncate a download silently.
    pub async fn chunk(&mut self) -> Result<Option<Bytes>, Error> {
        while let Some(frame) = self.body.frame().await {
            let frame = frame.map_err(|e| Error::Download(e.to_string()))?;

            if let Ok(data) = frame.into_data() {
                return Ok(Some(data));
            }
        }

        Ok(None)
    }

    /// The whole body, parsed as JSON. Only for responses known to be small —
    /// firmware goes through [`Response::chunk`].
    pub async fn json(self) -> Result<serde_json::Value, Error> {
        let collected = self
            .body
            .collect()
            .await
            .map_err(|e| Error::Download(e.to_string()))?;

        serde_json::from_slice(&collected.to_bytes()).map_err(Error::Json)
    }
}

fn redirect_target(response: &Response) -> Option<&str> {
    let redirect = matches!(
        response.status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    );

    redirect.then(|| response.headers.get(LOCATION)?.to_str().ok())?
}

/// Resolve a `Location` against the request it answered.
///
/// Written out rather than reached for from the `url` crate, which pulls idna
/// and icu and with them a minimum Rust of 1.88 — the exact cost this module
/// exists to avoid.
fn resolve(base: &Uri, location: &str) -> Result<Uri, Error> {
    // A query-only reference keeps the base path (RFC 3986 5.3), and hyper's
    // parser rejects one outright, so it is rewritten before parsing rather
    // than after.
    let location = if location.starts_with('?') {
        &format!("{}{location}", base.path())
    } else {
        location
    };

    let target: Uri = location
        .parse()
        .map_err(|e| Error::Download(format!("redirect to {location}: {e}")))?;

    if target.scheme().is_some() {
        return Ok(target);
    }

    // A relative Location keeps the scheme and authority it was served from.
    let mut parts = target.into_parts();
    parts.scheme = base.scheme().cloned();
    parts.authority = base.authority().cloned();

    // `Uri::from_parts` rejects a scheme and authority with no path.
    if parts.path_and_query.is_none() {
        parts.path_and_query = Some("/".parse().expect("a literal slash parses"));
    }

    Uri::from_parts(parts)
        .map_err(|e| Error::Download(format!("resolving redirect to {location}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absolute_redirect_replaces_everything() {
        let base: Uri = "https://hub.example/firmware/1".parse().unwrap();
        let resolved = resolve(&base, "https://storage.example/blob?sig=abc").unwrap();

        assert_eq!(resolved.to_string(), "https://storage.example/blob?sig=abc");
    }

    /// The case that matters for a presigned URL served from the same host.
    #[test]
    fn a_relative_redirect_keeps_scheme_and_host() {
        let base: Uri = "https://hub.example/firmware/1".parse().unwrap();
        let resolved = resolve(&base, "/blob/9?sig=abc").unwrap();

        assert_eq!(resolved.to_string(), "https://hub.example/blob/9?sig=abc");
    }

    /// A query-only Location keeps the path it was served from, rather than
    /// resetting to the root and fetching the wrong object.
    #[test]
    fn a_query_only_redirect_keeps_the_base_path() {
        let base: Uri = "http://hub.example/firmware/1".parse().unwrap();
        let resolved = resolve(&base, "?sig=abc").unwrap();

        assert_eq!(
            resolved.to_string(),
            "http://hub.example/firmware/1?sig=abc"
        );
    }
}
