//! `nuee/http/0`: console requests. One bi-stream per request: the client sends one
//! length-prefixed `HttpRequest` frame and finishes its side, the host answers one
//! `HttpResponse` frame. Requests go to `http://127.0.0.1:<declared port>` only.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::header::{HeaderName, HeaderValue};
use hyper_util::rt::TokioIo;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use iroh::protocol::{AcceptError, ProtocolHandler};
use nuee_net::{
    AdmitError, FrameError, Net, read_frame, recv_console_body, recv_console_hello,
    send_console_request, write_frame,
};
use nuee_proto::{
    ALPN_HTTP, BoundedBytes, BoundedString, BoundedVec, EndpointId, Header, HttpRequest,
    HttpResponse, MAX_HTTP_MSG, PROOF_HTTP, ServiceLink, decode_with_limit, encode_with_limit,
    limits::{MAX_HTTP_BODY, MAX_HTTP_HEADERS},
};
use tokio::net::TcpStream;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::{
    CONNECT_TIMEOUT, ConnGate, ExposeEvent, HostError, MAX_HTTP_INFLIGHT, Shared, learn_host,
};

/// Headers that only make sense on one hop and are never forwarded.
const HOP_BY_HOP: [&str; 11] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
];

/// Extra time the client waits beyond the host's own request timeout.
const CLIENT_MARGIN: Duration = Duration::from_secs(15);

/// Host side of `nuee/http/0`.
#[derive(Debug, Clone)]
pub struct HttpHandler {
    shared: Shared,
    inflight: Arc<Semaphore>,
}

/// A response built by the host itself, not by the exposed service.
fn plain(status: u16, msg: &str) -> HttpResponse {
    let mut headers = BoundedVec::new();
    if let (Ok(name), Ok(value)) = (
        BoundedString::try_from("content-type"),
        BoundedString::try_from("text/plain; charset=utf-8"),
    ) && headers.push(Header { name, value }).is_err()
    {
        tracing::debug!("no room for the content type");
    }
    HttpResponse {
        status,
        headers,
        body: BoundedBytes::try_from(msg.as_bytes()).unwrap_or_default(),
    }
}

fn hop_by_hop(name: &str, extra: &[String]) -> bool {
    HOP_BY_HOP.iter().any(|h| name.eq_ignore_ascii_case(h))
        || extra.iter().any(|h| name.eq_ignore_ascii_case(h))
}

/// Header names listed in `Connection` values, which are hop-by-hop too.
fn connection_tokens<'a>(values: impl Iterator<Item = &'a str>) -> Vec<String> {
    values
        .flat_map(|v| v.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

impl HttpHandler {
    pub(crate) fn new(shared: Shared) -> Self {
        Self {
            shared,
            inflight: Arc::new(Semaphore::new(MAX_HTTP_INFLIGHT)),
        }
    }

    async fn handle(&self, conn: Connection) {
        let peer = conn.remote_id();
        let conn_gate = ConnGate::new();
        let idle = self.shared.cfg.admit.hello_timeout;
        while let Some((send, recv)) = conn_gate.next_stream(&conn, idle).await {
            let this = self.clone();
            let slot = conn_gate.slot();
            let conn_gate = conn_gate.clone();
            tokio::spawn(async move { this.stream(peer, send, recv, slot, conn_gate).await });
        }
    }

    async fn stream(
        &self,
        peer: EndpointId,
        mut send: SendStream,
        mut recv: RecvStream,
        slot: Option<OwnedSemaphorePermit>,
        conn_gate: ConnGate,
    ) {
        let (resp, line) = match slot {
            Some(slot) => self.answer(peer, &mut recv, slot, &conn_gate).await,
            None => (plain(503, "too many handshakes on this connection"), None),
        };
        let bytes = resp.body.len() as u64;
        let status = resp.status;
        let frame = match encode_with_limit(&resp, MAX_HTTP_MSG) {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!("cannot encode a console response: {e}");
                return;
            }
        };
        if let Err(e) = write_frame(&mut send, &frame).await {
            tracing::debug!(%peer, "console response not delivered: {e}");
            return;
        }
        if let Err(e) = send.finish() {
            tracing::debug!(%peer, "stream already closed: {e}");
        }
        if let Some((method, path)) = line {
            self.shared.report(ExposeEvent::Http {
                peer,
                method,
                path,
                status,
                bytes,
            });
        }
        if tokio::time::timeout(Duration::from_secs(5), send.stopped())
            .await
            .is_err()
        {
            tracing::debug!(%peer, "client did not read the response in time");
        }
    }

    /// The response for one request, and its method and path once admitted.
    async fn answer(
        &self,
        peer: EndpointId,
        recv: &mut RecvStream,
        slot: OwnedSemaphorePermit,
        conn_gate: &ConnGate,
    ) -> (HttpResponse, Option<(String, String)>) {
        let shared = &self.shared;
        let gate = &shared.admission;
        let hello = recv_console_hello(recv, gate, &shared.cfg.secret, &peer, &shared.own).await;
        drop(slot);
        if let Err(e) = hello {
            let (status, msg) = match &e {
                AdmitError::Busy => (503, "the host is busy"),
                AdmitError::Throttled => (429, "too many wrong attempts; try later"),
                AdmitError::Timeout => (408, "no request before the deadline"),
                AdmitError::BadProof => (403, "wrong service secret"),
                _ => (400, "malformed request"),
            };
            shared.refused(peer, &e.to_string());
            return (plain(status, msg), None);
        }
        conn_gate.authenticated();
        // The body is read only from a client that proved it holds the service secret.
        let req = match tokio::time::timeout(shared.cfg.http_timeout, recv_console_body(recv)).await
        {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                let (status, msg) = match &e {
                    FrameError::TooLarge { .. } => (413, "request too large"),
                    _ => (400, "malformed request"),
                };
                tracing::info!(%peer, "console request unreadable: {e}");
                return (plain(status, msg), None);
            }
            Err(_) => return (plain(408, "no request before the deadline"), None),
        };
        if !shared.approved(peer).await {
            shared.refused(peer, "not approved by the host");
            return (plain(403, "not approved by the host"), None);
        }
        let line = Some((req.method.as_str().to_owned(), req.path.as_str().to_owned()));
        let Ok(_slot) = self.inflight.clone().try_acquire_owned() else {
            return (plain(503, "too many requests in flight"), line);
        };
        let resp = match tokio::time::timeout(
            shared.cfg.http_timeout,
            forward(shared.cfg.port, req),
        )
        .await
        {
            Ok(Ok(r)) => r,
            Ok(Err((status, msg))) => {
                tracing::info!(%peer, "console request failed: {msg}");
                plain(status, &msg)
            }
            Err(_) => plain(504, "the exposed service did not answer in time"),
        };
        (resp, line)
    }
}

impl ProtocolHandler for HttpHandler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        self.handle(connection).await;
        Ok(())
    }
}

type Failed = (u16, String);

/// Sends `req` as HTTP/1.1 to `127.0.0.1:port` and reads the bounded answer.
async fn forward(port: u16, req: HttpRequest) -> Result<HttpResponse, Failed> {
    let bad = |m: String| (400u16, m);
    let method = hyper::Method::from_bytes(req.method.as_bytes())
        .map_err(|_| bad(format!("invalid method {:?}", req.method.as_str())))?;
    let path = req.path.as_str();
    if !path.starts_with('/') {
        return Err(bad("the path must start with '/'".into()));
    }
    let uri = hyper::Uri::try_from(path).map_err(|e| bad(format!("invalid path: {e}")))?;
    let extra = connection_tokens(
        req.headers
            .iter()
            .filter(|h| h.name.eq_ignore_ascii_case("connection"))
            .map(|h| h.value.as_str()),
    );
    let mut builder = hyper::Request::builder().method(method).uri(uri);
    for h in req.headers.iter() {
        if hop_by_hop(h.name.as_str(), &extra) {
            continue;
        }
        let name = HeaderName::from_bytes(h.name.as_bytes())
            .map_err(|_| bad(format!("invalid header name {:?}", h.name.as_str())))?;
        let value = HeaderValue::from_str(h.value.as_str())
            .map_err(|_| bad(format!("invalid value for header {name}")))?;
        builder = builder.header(name, value);
    }
    builder = builder.header(hyper::header::HOST, format!("localhost:{port}"));
    let request = builder
        .body(Full::new(Bytes::from(req.body.into_inner())))
        .map_err(|e| bad(format!("invalid request: {e}")))?;

    let upstream = |m: String| (502u16, m);
    let target = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(target))
        .await
        .map_err(|_| upstream("the exposed service is not answering".into()))?
        .map_err(|e| upstream(format!("the exposed service is not answering: {e}")))?;
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| upstream(format!("HTTP handshake failed: {e}")))?;
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::debug!("upstream connection ended: {e}");
        }
    });
    let resp = sender
        .send_request(request)
        .await
        .map_err(|e| upstream(format!("the exposed service failed: {e}")))?;
    let (parts, body) = resp.into_parts();
    let body = Limited::new(body, MAX_HTTP_BODY)
        .collect()
        .await
        .map_err(|e| {
            if e.is::<http_body_util::LengthLimitError>() {
                upstream("response body above 8 MiB".into())
            } else {
                upstream(format!("cannot read the response: {e}"))
            }
        })?
        .to_bytes();

    let extra = connection_tokens(
        parts
            .headers
            .get_all(hyper::header::CONNECTION)
            .iter()
            .filter_map(|v| v.to_str().ok()),
    );
    let mut headers = BoundedVec::<Header, MAX_HTTP_HEADERS>::new();
    for (name, value) in &parts.headers {
        if hop_by_hop(name.as_str(), &extra) {
            continue;
        }
        let (Ok(v), Ok(n)) = (value.to_str(), BoundedString::try_from(name.as_str())) else {
            tracing::debug!("dropping response header {name}: not text or too long");
            continue;
        };
        let Ok(v) = BoundedString::try_from(v) else {
            tracing::debug!("dropping response header {name}: value too long");
            continue;
        };
        headers
            .push(Header { name: n, value: v })
            .map_err(|_| upstream(format!("more than {MAX_HTTP_HEADERS} response headers")))?;
    }
    Ok(HttpResponse {
        status: parts.status.as_u16(),
        headers,
        body: BoundedBytes::try_from(body.to_vec())
            .map_err(|_| upstream("response body above 8 MiB".into()))?,
    })
}

/// Sends one console request to the service of `link` (the proof is filled in here).
pub async fn http_request(
    net: &Net,
    link: &ServiceLink,
    mut req: HttpRequest,
) -> Result<HttpResponse, HostError> {
    learn_host(net, link)?;
    req.proof = nuee_proto::proof(PROOF_HTTP, &link.secret, &net.id(), &link.host);
    let run = async {
        let conn = net.connect(link.host, ALPN_HTTP).await?;
        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| HostError::Connection(e.to_string()))?;
        send_console_request(&mut send, &req).await?;
        send.finish()
            .map_err(|e| HostError::Connection(e.to_string()))?;
        let answer = read_frame(&mut recv, MAX_HTTP_MSG).await?;
        conn.close(0u32.into(), b"done");
        decode_with_limit::<HttpResponse>(&answer, MAX_HTTP_MSG)
            .map_err(|e| HostError::Protocol(format!("malformed response: {e}")))
    };
    tokio::time::timeout(crate::HTTP_TIMEOUT + CLIENT_MARGIN, run)
        .await
        .map_err(|_| HostError::Timeout)?
}
