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
//!
//! # Timeouts
//!
//! hyper has none of its own, so a download on a connection that died without
//! a reset would wait forever, and the install waiting on it would hold the
//! device. There are two, from `[http]` in the config:
//!
//!   * **connect**: how long the TCP connection may take to open.
//!   * **stall**: how long the server may go without sending anything, either
//!     before its response or between parts of the body. Not a limit on the
//!     whole download, which on a slow link takes as long as it takes.
//!
//! The stall timeout is applied where the agent waits, so it catches a dead
//! peer, a dead path and a live server that has stopped sending, and the error
//! says "stalled" in each case. TCP keepalive is left off: it would catch only
//! the first two, and would race the stall timeout with a vaguer error.

use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::body::Incoming;
use hyper::header::{HeaderName, HeaderValue, LOCATION};
use hyper::{HeaderMap, Request, StatusCode, Uri};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::Client as Legacy;
use hyper_util::rt::TokioExecutor;

use crate::config::{Config, HttpConfig};
use crate::error::Error;

/// Enough for a presigned URL that redirects to object storage, and few enough
/// that a loop is an error rather than a hang.
const MAX_REDIRECTS: usize = 10;

type Connector = hyper_rustls::HttpsConnector<HttpConnector>;

/// How long the client waits on a server before giving up. See the module
/// docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// For the TCP connection to open.
    pub connect: Duration,
    /// For the server to send anything at all: the response, or the next part
    /// of its body.
    pub stall: Duration,
}

impl From<&HttpConfig> for Timeouts {
    fn from(config: &HttpConfig) -> Self {
        Self {
            connect: Duration::from_secs(config.connect_timeout_secs),
            stall: Duration::from_secs(config.stall_timeout_secs),
        }
    }
}

impl Default for Timeouts {
    fn default() -> Self {
        (&HttpConfig::default()).into()
    }
}

#[derive(Clone)]
pub struct Client {
    inner: Legacy<Connector, Empty<Bytes>>,
    timeouts: Timeouts,
}

impl Client {
    pub fn new(config: &Config) -> Result<Self, Error> {
        Self::with_tls(crate::tls::client_config(config)?, (&config.http).into())
    }

    /// A client trusting the platform store and nothing more. For callers with
    /// no agent configuration -- tests, and the integration tests that drive an
    /// updater directly.
    pub fn with_native_roots() -> Result<Self, Error> {
        Self::with_native_roots_and_timeouts(Timeouts::default())
    }

    /// The same, with timeouts of the caller's choosing. For tests that need a
    /// stall to show up in a second rather than a minute.
    pub fn with_native_roots_and_timeouts(timeouts: Timeouts) -> Result<Self, Error> {
        Self::with_tls(crate::tls::native_roots()?, timeouts)
    }

    fn with_tls(
        tls: std::sync::Arc<rustls::ClientConfig>,
        timeouts: Timeouts,
    ) -> Result<Self, Error> {
        let mut http = HttpConnector::new();
        // What `HttpsConnectorBuilder::build` does to its own connector: the
        // scheme is the TLS layer's to judge, and this one would refuse https.
        http.enforce_http(false);
        http.set_connect_timeout(Some(timeouts.connect));

        // `https_or_http`, not `https_only`: a NervesHub in development serves
        // firmware over plain HTTP, and refusing it here would make the
        // sandbox setup in examples/local.toml impossible to run.
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config((*tls).clone())
            .https_or_http()
            .enable_http1()
            .wrap_connector(http);

        Ok(Self {
            inner: Legacy::builder(TokioExecutor::new()).build(connector),
            timeouts,
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

        // The host alone, for errors. A presigned URL carries its signature in
        // the query, and a log line is no place for one.
        let host = uri.host().unwrap_or("the server").to_string();
        let Timeouts { connect, stall } = self.timeouts;

        // The stall timeout covers everything up to the response: the name
        // lookup and the TLS handshake as well as the server's answer, none of
        // which the connect timeout reaches.
        let response = match tokio::time::timeout(stall, self.inner.request(request)).await {
            Ok(Ok(response)) => response,
            Ok(Err(e)) if e.is_connect() && timed_out(&e) => {
                return Err(Error::Download(format!(
                    "could not connect to {host} within {connect:?}"
                )))
            }
            Ok(Err(e)) => return Err(Error::Download(describe(&e))),
            Err(_) => {
                return Err(Error::Download(format!(
                    "stalled: no response from {host} in {stall:?}"
                )))
            }
        };

        let (parts, body) = response.into_parts();

        Ok(Response {
            status: parts.status,
            headers: parts.headers,
            body,
            host,
            stall,
            received: 0,
        })
    }
}

pub struct Response {
    status: StatusCode,
    headers: HeaderMap,
    body: Incoming,
    /// Who is sending the body, for saying who went quiet. After a redirect
    /// this is where it led, which is usually object storage, not NervesHub.
    host: String,
    stall: Duration,
    /// Body bytes so far, so a stall can say how far it got.
    received: u64,
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
    ///
    /// Fails with a stall if the server sends nothing for the stall timeout.
    /// The clock starts when this is called, so time the caller spends on a
    /// chunk, such as fwup writing it to a slow disk, is not counted.
    pub async fn chunk(&mut self) -> Result<Option<Bytes>, Error> {
        loop {
            let frame = match tokio::time::timeout(self.stall, self.body.frame()).await {
                Ok(Some(frame)) => frame.map_err(|e| Error::Download(describe(&e)))?,
                Ok(None) => return Ok(None),
                Err(_) => {
                    return Err(Error::Download(format!(
                        "stalled after {} bytes: nothing from {} in {:?}",
                        self.received, self.host, self.stall
                    )))
                }
            };

            if let Ok(data) = frame.into_data() {
                self.received += data.len() as u64;
                return Ok(Some(data));
            }
        }
    }

    /// The whole body, parsed as JSON. Only for responses known to be small —
    /// firmware goes through [`Response::chunk`].
    pub async fn json(mut self) -> Result<serde_json::Value, Error> {
        // A chunk at a time rather than `collect`, so a lookup whose server
        // goes quiet stalls out the same way a download does.
        let mut body = Vec::new();

        while let Some(chunk) = self.chunk().await? {
            body.extend_from_slice(&chunk);
        }

        serde_json::from_slice(&body).map_err(Error::Json)
    }
}

/// An error and everything under it.
///
/// hyper's own `Display` is only the outermost layer: a refused connection
/// reads as `client error (Connect)`, and the reason is in its sources.
fn describe(error: &(dyn std::error::Error + 'static)) -> String {
    let mut described = error.to_string();
    let mut source = error.source();

    while let Some(cause) = source {
        described.push_str(": ");
        described.push_str(&cause.to_string());
        source = cause.source();
    }

    described
}

/// Whether anything under this error is a timeout, which for a connect error
/// is the connect timeout running out.
fn timed_out(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);

    while let Some(e) = current {
        if let Some(io) = e.downcast_ref::<std::io::Error>() {
            if io.kind() == std::io::ErrorKind::TimedOut {
                return true;
            }
        }

        current = e.source();
    }

    false
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
    use std::time::Instant;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::*;

    /// Short enough to keep the tests quick, long enough that a loaded machine
    /// does not mistake a slow write for a stall.
    const STALL: Duration = Duration::from_millis(500);

    fn client() -> Client {
        Client::with_native_roots_and_timeouts(Timeouts {
            connect: Duration::from_secs(5),
            stall: STALL,
        })
        .unwrap()
    }

    /// A server that hands each connection to `serve` once it has read the
    /// request.
    async fn server<F, Fut>(serve: F) -> String
    where
        F: Fn(TcpStream) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/firmware.fw", listener.local_addr().unwrap());

        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = [0u8; 2048];
                let _ = socket.read(&mut request).await;

                tokio::spawn(serve(socket));
            }
        });

        url
    }

    /// The failure that held devices: headers, part of the body, then a
    /// connection that stays open and says nothing.
    #[tokio::test]
    async fn a_body_that_goes_quiet_fails_as_a_stall() {
        let (closed, mut saw_close) = tokio::sync::mpsc::unbounded_channel();

        let url = server(move |mut socket| {
            let closed = closed.clone();

            async move {
                let head = "HTTP/1.1 200 OK\r\nContent-Length: 1048576\r\n\r\n";
                socket.write_all(head.as_bytes()).await.unwrap();
                socket.write_all(&[0x5a; 1000]).await.unwrap();

                // Hold the connection open until the client hangs up, which is
                // the other half of what the test checks.
                let mut rest = [0u8; 64];
                while socket.read(&mut rest).await.is_ok_and(|n| n > 0) {}
                let _ = closed.send(());
            }
        })
        .await;

        let mut response = client().get(&url).await.expect("headers arrive");
        let mut received = 0;
        let mut last_data = Instant::now();

        let error = loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    received += chunk.len();
                    last_data = Instant::now();
                }
                Ok(None) => panic!("the body ended instead of stalling"),
                Err(e) => break e,
            }
        };

        let waited = last_data.elapsed();

        assert_eq!(received, 1000);
        assert!(
            matches!(&error, Error::Download(m) if m.starts_with("stalled after 1000 bytes")),
            "got: {error}"
        );
        assert!(
            waited >= STALL && waited < STALL * 4,
            "gave up {waited:?} after the last byte, for a {STALL:?} timeout"
        );

        // Dropping the response hangs up rather than leaving the socket open
        // on a server that might yet wake up.
        drop(response);

        tokio::time::timeout(Duration::from_secs(5), saw_close.recv())
            .await
            .expect("the client never closed the stalled connection");
    }

    /// Accepted, read the request, never answered. Waiting for the headers is
    /// as much a stall as waiting for the body.
    #[tokio::test]
    async fn a_server_that_never_answers_fails_as_a_stall() {
        let url = server(|socket| async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(socket);
        })
        .await;

        let started = Instant::now();
        let error = match client().get(&url).await {
            Ok(_) => panic!("a server that said nothing produced a response"),
            Err(e) => e,
        };

        assert!(
            matches!(&error, Error::Download(m) if m.starts_with("stalled: no response from 127.0.0.1")),
            "got: {error}"
        );
        assert!(
            started.elapsed() < STALL * 4,
            "took {:?}",
            started.elapsed()
        );
    }

    /// A download that is slow but keeps moving outlasts the stall timeout
    /// several times over and still succeeds. The timeout is about silence,
    /// not about how long the whole thing takes.
    #[tokio::test]
    async fn a_slow_download_that_keeps_moving_is_not_a_stall() {
        const PIECES: usize = 8;
        const GAP: Duration = Duration::from_millis(200);

        let url = server(|mut socket| async move {
            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {PIECES}\r\n\r\n");
            socket.write_all(head.as_bytes()).await.unwrap();

            for _ in 0..PIECES {
                tokio::time::sleep(GAP).await;
                socket.write_all(b"x").await.unwrap();
            }
        })
        .await;

        let started = Instant::now();
        let mut response = client().get(&url).await.unwrap();
        let mut received = 0;

        while let Some(chunk) = response.chunk().await.expect("no stall") {
            received += chunk.len();
        }

        assert_eq!(received, PIECES);
        assert!(
            started.elapsed() > STALL * 2,
            "finished in {:?}, too quickly to prove anything",
            started.elapsed()
        );
    }

    /// hyper's own message for this is `client error (Connect)`, which names
    /// neither the host nor the problem.
    #[tokio::test]
    async fn a_refused_connection_says_why() {
        // Bound and dropped, so nothing is listening there.
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            listener.local_addr().unwrap().port()
        };

        let error = match client().get(&format!("http://127.0.0.1:{port}/")).await {
            Ok(_) => panic!("connected to a port nothing listens on"),
            Err(e) => e,
        };

        let message = error.to_string();
        assert!(
            message.contains("tcp connect error") && message.to_lowercase().contains("refused"),
            "got: {message}"
        );
    }

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
