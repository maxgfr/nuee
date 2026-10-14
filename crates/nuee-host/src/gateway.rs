//! The anchor's HTTP gateway: static sites pinned on the anchor (`/s/<id>/<path>`) and
//! exposed services the anchor may reach (`/x/<service>/<path>`, over `nuee/http/0`).
//!
//! Hosted content is untrusted. Every response carries `Content-Security-Policy: sandbox`
//! (scripts off, opaque origin; `--gateway-scripts` allows scripts, forms and popups but
//! never the same origin), `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`
//! and `Cross-Origin-Resource-Policy: cross-origin`. Paths are decoded once and refused when a
//! segment is `.`, `..`, empty or holds a separator; directories are never listed.
//!
//! The resource policy cannot be `same-site`: a sandboxed page has an opaque origin, which is
//! same-site with nothing, so browsers would block its own stylesheets, scripts and images.

use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::net::IpAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::header::{self, HeaderName, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use nuee_net::Net;
use nuee_proto::limits::{MAX_HTTP_BODY, MAX_HTTP_HEADERS};
use nuee_proto::{BoundedBytes, BoundedString, BoundedVec, Header, HttpRequest, ServiceLink};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

/// The default policy: no scripts, opaque origin.
pub const CSP_SANDBOX: &str = "sandbox";
/// The policy with `--gateway-scripts`: scripts, forms and popups, still an opaque origin.
pub const CSP_SCRIPTS: &str = "sandbox allow-scripts allow-forms allow-popups";
/// The `Cross-Origin-Resource-Policy` value: a sandboxed page (opaque origin) loads its own
/// subresources as a cross-origin requester, so anything stricter blocks them.
pub const CORP: &str = "cross-origin";
/// Most connections served at once.
pub const MAX_CONNECTIONS: usize = 256;
/// Total bytes of one site's files.
pub const MAX_SITE_BYTES: u64 = 256 * 1024 * 1024;
/// Most files in one site.
pub const MAX_SITE_FILES: usize = 10_000;
/// Most sites and services registered.
pub const MAX_ENTRIES: usize = 1024;
/// Time allowed to send the request head.
pub const HEADER_TIMEOUT: Duration = Duration::from_secs(10);
/// Time allowed to send the request body once the head arrived; with [`HEADER_TIMEOUT`] it
/// bounds a whole request, so slow clients cannot hold connection slots.
pub const BODY_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest request target accepted.
pub const MAX_TARGET: usize = 2048;

/// Gateway settings.
#[derive(Debug, Clone, Copy)]
pub struct GatewayConfig {
    /// Let hosted pages run scripts (still sandboxed, never same-origin).
    pub scripts: bool,
}

/// A site or service could not be registered.
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    /// The identifier is not 1 to 64 characters of `[a-z0-9-]`.
    #[error("invalid identifier {0:?}")]
    BadId(String),
    /// A file path is unsafe.
    #[error("unsafe path in site: {0:?}")]
    UnsafePath(String),
    /// The site is above the limits.
    #[error("site too large: {0}")]
    TooLarge(String),
    /// Too many sites or services.
    #[error("too many entries (at most {MAX_ENTRIES})")]
    Full,
}

type Files = BTreeMap<String, Bytes>;

#[derive(Debug)]
struct Inner {
    cfg: GatewayConfig,
    net: Option<Net>,
    sites: RwLock<HashMap<String, Arc<Files>>>,
    services: RwLock<HashMap<String, ServiceLink>>,
    body_timeout: Duration,
}

/// The gateway: what it serves, and the server loop. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Gateway {
    inner: Arc<Inner>,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Splits a decoded relative path into safe segments; `None` when a segment is unsafe.
fn safe_segments(path: &str) -> Option<Vec<&str>> {
    if path.is_empty() {
        return Some(Vec::new());
    }
    let mut out = Vec::new();
    for seg in path.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." || seg.contains(['\\', '\0', ':']) {
            return None;
        }
        out.push(seg);
    }
    Some(out)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Percent-decodes a path once; `None` on a malformed escape or invalid UTF-8.
fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while let Some(&b) = bytes.get(i) {
        if b == b'%' {
            let hi = hex_val(*bytes.get(i + 1)?)?;
            let lo = hex_val(*bytes.get(i + 2)?)?;
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(b);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The MIME type of a file name.
pub fn mime_type(name: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "txt" | "md" => "text/plain; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        "pdf" => "application/pdf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    }
}

/// Response headers the gateway always sets itself; never taken from a proxied service.
const OWN_HEADERS: [&str; 5] = [
    "content-security-policy",
    "content-security-policy-report-only",
    "x-content-type-options",
    "referrer-policy",
    "cross-origin-resource-policy",
];

/// Upstream response headers never relayed: a hosted service must not set cookies on the
/// gateway host that every site and service shares (requests never carry `Cookie` anyway).
const DROPPED_RESPONSE: [&str; 2] = ["set-cookie", "set-cookie2"];

/// Client-address headers a visitor could forge; the gateway sets `X-Forwarded-For` itself.
fn forwarding_header(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n == "forwarded" || n == "x-real-ip" || n == "x-client-ip" || n.starts_with("x-forwarded-")
}

/// Headers that are never forwarded, in either direction.
const HOP_BY_HOP: [&str; 10] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-length",
];

impl Gateway {
    /// A gateway with no site and no service. `net` reaches exposed services.
    pub fn new(cfg: GatewayConfig, net: Option<Net>) -> Gateway {
        Self::with_body_timeout(cfg, net, BODY_TIMEOUT)
    }

    /// A gateway that waits at most `body_timeout` for a request body.
    pub fn with_body_timeout(
        cfg: GatewayConfig,
        net: Option<Net>,
        body_timeout: Duration,
    ) -> Gateway {
        Gateway {
            inner: Arc::new(Inner {
                cfg,
                net,
                sites: RwLock::new(HashMap::new()),
                services: RwLock::new(HashMap::new()),
                body_timeout,
            }),
        }
    }

    /// The `Content-Security-Policy` value served.
    pub fn csp(&self) -> &'static str {
        if self.inner.cfg.scripts {
            CSP_SCRIPTS
        } else {
            CSP_SANDBOX
        }
    }

    fn sites(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, Arc<Files>>> {
        // Whole values are swapped in and out; a panic elsewhere cannot leave one half-written.
        self.inner
            .sites
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn sites_mut(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, Arc<Files>>> {
        self.inner
            .sites
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn services(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, ServiceLink>> {
        self.inner
            .services
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn services_mut(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, ServiceLink>> {
        self.inner
            .services
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Serves `files` (relative path, bytes) under `/s/<id>/`.
    pub fn add_site(&self, id: &str, files: Vec<(String, Vec<u8>)>) -> Result<(), GatewayError> {
        if !valid_id(id) {
            return Err(GatewayError::BadId(id.to_owned()));
        }
        if files.len() > MAX_SITE_FILES {
            return Err(GatewayError::TooLarge(format!(
                "{} files (at most {MAX_SITE_FILES})",
                files.len()
            )));
        }
        let mut total = 0u64;
        let mut map = Files::new();
        for (path, bytes) in files {
            if safe_segments(&path).is_none_or(|s| s.is_empty()) {
                return Err(GatewayError::UnsafePath(path));
            }
            total = total.saturating_add(bytes.len() as u64);
            if total > MAX_SITE_BYTES {
                return Err(GatewayError::TooLarge(format!(
                    "more than {MAX_SITE_BYTES} bytes"
                )));
            }
            map.insert(path, Bytes::from(bytes));
        }
        let mut sites = self.sites_mut();
        if !sites.contains_key(id) && sites.len() >= MAX_ENTRIES {
            return Err(GatewayError::Full);
        }
        sites.insert(id.to_owned(), Arc::new(map));
        Ok(())
    }

    /// Stops serving a site; false when it was not served.
    pub fn remove_site(&self, id: &str) -> bool {
        self.sites_mut().remove(id).is_some()
    }

    /// Identifiers of the sites served.
    pub fn site_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.sites().keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Proxies `/x/<id>/…` to the service of `link`.
    pub fn add_service(&self, id: &str, link: ServiceLink) -> Result<(), GatewayError> {
        if !valid_id(id) {
            return Err(GatewayError::BadId(id.to_owned()));
        }
        let mut services = self.services_mut();
        if !services.contains_key(id) && services.len() >= MAX_ENTRIES {
            return Err(GatewayError::Full);
        }
        services.insert(id.to_owned(), link);
        Ok(())
    }

    /// Stops proxying a service; false when it was not registered.
    pub fn remove_service(&self, id: &str) -> bool {
        self.services_mut().remove(id).is_some()
    }

    /// Identifiers of the services proxied.
    pub fn service_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.services().keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Adds the security headers to a response and returns it.
    fn secure(&self, mut resp: Response<Full<Bytes>>) -> Response<Full<Bytes>> {
        let h = resp.headers_mut();
        for name in OWN_HEADERS {
            h.remove(name);
        }
        h.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(self.csp()),
        );
        h.insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        h.insert(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        );
        h.insert(
            HeaderName::from_static("cross-origin-resource-policy"),
            HeaderValue::from_static(CORP),
        );
        resp
    }

    fn plain(&self, status: StatusCode, msg: &str) -> Response<Full<Bytes>> {
        let mut resp = Response::new(Full::new(Bytes::from(format!("{msg}\n"))));
        *resp.status_mut() = status;
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        );
        self.secure(resp)
    }

    /// The answer to one request: `target` is the request path and query as received.
    pub async fn respond(
        &self,
        method: &Method,
        target: &str,
        headers: &hyper::HeaderMap,
        body: Bytes,
    ) -> Response<Full<Bytes>> {
        self.respond_from(None, method, target, headers, body).await
    }

    /// [`Gateway::respond`] for a request from `client`, the address the gateway saw; it
    /// becomes the `X-Forwarded-For` of a proxied request.
    pub async fn respond_from(
        &self,
        client: Option<IpAddr>,
        method: &Method,
        target: &str,
        headers: &hyper::HeaderMap,
        body: Bytes,
    ) -> Response<Full<Bytes>> {
        if target.len() > MAX_TARGET {
            return self.plain(StatusCode::URI_TOO_LONG, "request target too long");
        }
        let (path, query) = match target.split_once('?') {
            Some((p, q)) => (p, Some(q)),
            None => (target, None),
        };
        if let Some(rest) = path.strip_prefix("/s/") {
            return self.site(method, rest);
        }
        if let Some(rest) = path.strip_prefix("/x/") {
            return self.proxy(client, method, rest, query, headers, body).await;
        }
        if path == "/" || path == "/healthz" {
            return self.plain(StatusCode::OK, "nuee gateway");
        }
        self.plain(StatusCode::NOT_FOUND, "not found")
    }

    fn site(&self, method: &Method, rest: &str) -> Response<Full<Bytes>> {
        if method != Method::GET && method != Method::HEAD {
            let mut resp = self.plain(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
            resp.headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
            return resp;
        }
        let Some(decoded) = percent_decode(rest) else {
            return self.plain(StatusCode::BAD_REQUEST, "malformed path");
        };
        let (id, file) = match decoded.split_once('/') {
            Some((id, file)) => (id, file),
            None => (decoded.as_str(), ""),
        };
        let Some(site) = self.sites().get(id).cloned() else {
            return self.plain(StatusCode::NOT_FOUND, "unknown site");
        };
        let wants_dir = file.is_empty() || file.ends_with('/');
        let rel = file.strip_suffix('/').unwrap_or(file);
        let Some(segments) = safe_segments(rel) else {
            return self.plain(StatusCode::BAD_REQUEST, "unsafe path");
        };
        let mut name = segments.join("/");
        if wants_dir {
            if !name.is_empty() {
                name.push('/');
            }
            name.push_str("index.html");
        }
        let Some(bytes) = site.get(&name).cloned() else {
            return self.plain(StatusCode::NOT_FOUND, "not found");
        };
        let len = bytes.len();
        let body = if method == Method::HEAD {
            Bytes::new()
        } else {
            bytes
        };
        let mut resp = Response::new(Full::new(body));
        let h = resp.headers_mut();
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(mime_type(&name)),
        );
        if method == Method::HEAD {
            h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
        }
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
        self.secure(resp)
    }

    async fn proxy(
        &self,
        client: Option<IpAddr>,
        method: &Method,
        rest: &str,
        query: Option<&str>,
        headers: &hyper::HeaderMap,
        body: Bytes,
    ) -> Response<Full<Bytes>> {
        let (id, path) = match rest.split_once('/') {
            Some((id, path)) => (id, path),
            None => (rest, ""),
        };
        let Some(link) = self.services().get(id).cloned() else {
            return self.plain(StatusCode::NOT_FOUND, "unknown service");
        };
        let Some(net) = &self.inner.net else {
            return self.plain(StatusCode::SERVICE_UNAVAILABLE, "no network");
        };
        let Some(decoded) = percent_decode(path) else {
            return self.plain(StatusCode::BAD_REQUEST, "malformed path");
        };
        let rel = decoded.strip_suffix('/').unwrap_or(&decoded);
        if safe_segments(rel).is_none() {
            return self.plain(StatusCode::BAD_REQUEST, "unsafe path");
        }
        let mut upstream = format!("/{path}");
        if let Some(q) = query {
            upstream.push('?');
            upstream.push_str(q);
        }
        let Ok(req) = build_request(client, method, &upstream, headers, body) else {
            return self.plain(StatusCode::BAD_REQUEST, "request not forwardable");
        };
        match crate::http_request(net, &link, req).await {
            Ok(r) => self.upstream_response(r),
            Err(e) => {
                tracing::info!(service = id, "gateway proxy failed: {e}");
                self.plain(StatusCode::BAD_GATEWAY, "the service did not answer")
            }
        }
    }

    fn upstream_response(&self, r: nuee_proto::HttpResponse) -> Response<Full<Bytes>> {
        let mut resp = Response::new(Full::new(Bytes::from(r.body.into_inner())));
        *resp.status_mut() = StatusCode::from_u16(r.status).unwrap_or(StatusCode::BAD_GATEWAY);
        let h = resp.headers_mut();
        for hd in r.headers.iter() {
            let name = hd.name.as_str();
            if HOP_BY_HOP
                .iter()
                .chain(DROPPED_RESPONSE.iter())
                .any(|x| name.eq_ignore_ascii_case(x))
            {
                continue;
            }
            if let (Ok(n), Ok(v)) = (
                HeaderName::from_bytes(name.as_bytes()),
                HeaderValue::from_str(hd.value.as_str()),
            ) {
                h.append(n, v);
            }
        }
        if !h.contains_key(header::CONTENT_TYPE) {
            h.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
        }
        self.secure(resp)
    }

    /// Serves HTTP/1.1 on `listener` until the task is dropped.
    pub async fn serve(self, listener: TcpListener) -> std::io::Result<()> {
        let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
        loop {
            let (stream, remote) = listener.accept().await?;
            let Ok(slot) = slots.clone().try_acquire_owned() else {
                tracing::debug!(%remote, "gateway full, connection dropped");
                continue;
            };
            let this = self.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |req: Request<Incoming>| {
                    let this = this.clone();
                    async move { Ok::<_, Infallible>(this.handle(req, remote.ip()).await) }
                });
                let res = hyper::server::conn::http1::Builder::new()
                    .timer(TokioTimer::new())
                    .header_read_timeout(HEADER_TIMEOUT)
                    .max_headers(MAX_HTTP_HEADERS)
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
                if let Err(e) = res {
                    tracing::debug!(%remote, "gateway connection ended: {e}");
                }
                drop(slot);
            });
        }
    }

    async fn handle(&self, req: Request<Incoming>, client: IpAddr) -> Response<Full<Bytes>> {
        let (parts, body) = req.into_parts();
        let target = parts
            .uri
            .path_and_query()
            .map_or_else(|| parts.uri.path().to_owned(), |pq| pq.as_str().to_owned());
        let collect = Limited::new(body, MAX_HTTP_BODY).collect();
        let Ok(collected) = tokio::time::timeout(self.inner.body_timeout, collect).await else {
            let mut resp = self.plain(StatusCode::REQUEST_TIMEOUT, "request body too slow");
            resp.headers_mut()
                .insert(header::CONNECTION, HeaderValue::from_static("close"));
            return resp;
        };
        let body = match collected {
            Ok(b) => b.to_bytes(),
            Err(e) => {
                if e.is::<http_body_util::LengthLimitError>() {
                    return self.plain(StatusCode::PAYLOAD_TOO_LARGE, "request body too large");
                }
                return self.plain(StatusCode::BAD_REQUEST, "unreadable request body");
            }
        };
        self.respond_from(Some(client), &parts.method, &target, &parts.headers, body)
            .await
    }
}

fn build_request(
    client: Option<IpAddr>,
    method: &Method,
    path: &str,
    headers: &hyper::HeaderMap,
    body: Bytes,
) -> Result<HttpRequest, ()> {
    let mut out = BoundedVec::<Header, MAX_HTTP_HEADERS>::new();
    for (name, value) in headers {
        let n = name.as_str();
        if HOP_BY_HOP.iter().any(|x| n.eq_ignore_ascii_case(x))
            || n.eq_ignore_ascii_case("host")
            || n.eq_ignore_ascii_case("cookie")
            || forwarding_header(n)
        {
            continue;
        }
        let Ok(v) = value.to_str() else { continue };
        let (Ok(n), Ok(v)) = (BoundedString::try_from(n), BoundedString::try_from(v)) else {
            continue;
        };
        out.push(Header { name: n, value: v }).map_err(|_| ())?;
    }
    if let Some(ip) = client {
        let name = BoundedString::try_from("x-forwarded-for").map_err(|_| ())?;
        let value = BoundedString::try_from(ip.to_string()).map_err(|_| ())?;
        out.push(Header { name, value }).map_err(|_| ())?;
    }
    Ok(HttpRequest {
        proof: [0; 32],
        method: BoundedString::try_from(method.as_str()).map_err(|_| ())?,
        path: BoundedString::try_from(path).map_err(|_| ())?,
        headers: out,
        body: BoundedBytes::try_from(body.to_vec()).map_err(|_| ())?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gw(scripts: bool) -> Gateway {
        let g = Gateway::new(GatewayConfig { scripts }, None);
        g.add_site(
            "abc",
            vec![
                ("index.html".into(), b"<h1>hi</h1>".to_vec()),
                ("css/site.css".into(), b"h1{}".to_vec()),
                ("docs/index.html".into(), b"docs".to_vec()),
            ],
        )
        .expect("site");
        g
    }

    async fn get(g: &Gateway, target: &str) -> Response<Full<Bytes>> {
        g.respond(&Method::GET, target, &hyper::HeaderMap::new(), Bytes::new())
            .await
    }

    fn header<'a>(r: &'a Response<Full<Bytes>>, name: &str) -> &'a str {
        r.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
    }

    #[tokio::test]
    async fn serves_files_with_the_security_headers() {
        let g = gw(false);
        let r = get(&g, "/s/abc/").await;
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(header(&r, "content-type"), "text/html; charset=utf-8");
        assert_eq!(header(&r, "content-security-policy"), "sandbox");
        assert_eq!(header(&r, "x-content-type-options"), "nosniff");
        assert_eq!(header(&r, "referrer-policy"), "no-referrer");
        assert_eq!(header(&r, "cross-origin-resource-policy"), "cross-origin");
        let r = get(&g, "/s/abc/css/site.css").await;
        assert_eq!(header(&r, "content-type"), "text/css; charset=utf-8");
        let r = get(&g, "/s/abc/docs/").await;
        assert_eq!(r.status(), StatusCode::OK);
        let r = gw(true);
        let r = get(&r, "/s/abc/index.html").await;
        assert_eq!(
            header(&r, "content-security-policy"),
            "sandbox allow-scripts allow-forms allow-popups"
        );
    }

    #[tokio::test]
    async fn subresources_stay_loadable_by_the_sandboxed_page_itself() {
        // The page runs in an opaque origin; `same-site` or `same-origin` would make every
        // browser refuse its own CSS, scripts and images (seen in Chromium, Firefox, WebKit).
        let g = Gateway::new(GatewayConfig { scripts: false }, None);
        g.add_site(
            "abc",
            vec![
                (
                    "index.html".into(),
                    b"<link rel=stylesheet href=s.css>".to_vec(),
                ),
                ("s.css".into(), b"h1{}".to_vec()),
                ("img/p.png".into(), vec![0x89, b'P', b'N', b'G']),
            ],
        )
        .expect("site");
        for target in [
            "/s/abc/",
            "/s/abc/s.css",
            "/s/abc/img/p.png",
            "/s/abc/missing",
        ] {
            let r = get(&g, target).await;
            assert_eq!(
                header(&r, "cross-origin-resource-policy"),
                "cross-origin",
                "{target}"
            );
        }
    }

    #[tokio::test]
    async fn refuses_traversal_listing_and_unknown_sites() {
        let g = gw(false);
        for bad in [
            "/s/abc/../abc/index.html",
            "/s/abc/%2e%2e/x",
            "/s/abc/css/../index.html",
            "/s/abc/./index.html",
            "/s/abc//index.html",
            "/s/abc/a%5cb",
            "/s/abc/%zz",
        ] {
            let r = get(&g, bad).await;
            assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{bad}");
            assert_eq!(header(&r, "content-security-policy"), "sandbox");
        }
        assert_eq!(get(&g, "/s/abc/css/").await.status(), StatusCode::NOT_FOUND);
        assert_eq!(get(&g, "/s/nope/").await.status(), StatusCode::NOT_FOUND);
        assert_eq!(get(&g, "/x/nope/").await.status(), StatusCode::NOT_FOUND);
        let r = g
            .respond(
                &Method::POST,
                "/s/abc/",
                &hyper::HeaderMap::new(),
                Bytes::new(),
            )
            .await;
        assert_eq!(r.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[test]
    fn upstream_cookies_are_not_relayed() {
        let g = gw(false);
        let mut headers = BoundedVec::new();
        for (n, v) in [
            ("set-cookie", "a=1; Path=/"),
            ("Set-Cookie", "b=2; Domain=example.com"),
            ("set-cookie2", "c=3"),
            ("x-kept", "yes"),
        ] {
            headers
                .push(Header {
                    name: BoundedString::try_from(n).expect("name"),
                    value: BoundedString::try_from(v).expect("value"),
                })
                .expect("header");
        }
        let r = g.upstream_response(nuee_proto::HttpResponse {
            status: 200,
            headers,
            body: BoundedBytes::default(),
        });
        assert!(r.headers().get("set-cookie").is_none());
        assert!(r.headers().get("set-cookie2").is_none());
        assert_eq!(header(&r, "x-kept"), "yes");
    }

    #[test]
    fn forwarded_headers_are_replaced_by_the_real_client() {
        let mut h = hyper::HeaderMap::new();
        for (n, v) in [
            ("x-forwarded-for", "10.0.0.1"),
            ("X-Forwarded-Host", "evil.example"),
            ("x-forwarded-proto", "https"),
            ("forwarded", "for=10.0.0.1"),
            ("x-real-ip", "10.0.0.1"),
            ("x-kept", "yes"),
        ] {
            h.append(
                HeaderName::from_bytes(n.as_bytes()).expect("name"),
                HeaderValue::from_static(v),
            );
        }
        let client: IpAddr = "203.0.113.7".parse().expect("ip");
        let req = build_request(Some(client), &Method::GET, "/", &h, Bytes::new()).expect("req");
        let got: Vec<(String, String)> = req
            .headers
            .iter()
            .map(|h| (h.name.as_str().to_owned(), h.value.as_str().to_owned()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("x-kept".to_owned(), "yes".to_owned()),
                ("x-forwarded-for".to_owned(), "203.0.113.7".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn a_slow_request_body_is_cut_off() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let g = Gateway::with_body_timeout(
            GatewayConfig { scripts: false },
            None,
            Duration::from_millis(300),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("listen");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(g.serve(listener));
        let mut s = tokio::net::TcpStream::connect(addr).await.expect("connect");
        s.write_all(b"POST /x/a/ HTTP/1.1\r\nHost: g\r\nContent-Length: 100\r\n\r\nx")
            .await
            .expect("head");
        let mut out = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out))
            .await
            .expect("the gateway answers and closes in time")
            .expect("read");
        let text = String::from_utf8_lossy(&out);
        assert!(text.starts_with("HTTP/1.1 408"), "{text}");
        server.abort();
    }

    #[test]
    fn refuses_unsafe_sites() {
        let g = Gateway::new(GatewayConfig { scripts: false }, None);
        assert!(g.add_site("Bad", Vec::new()).is_err());
        assert!(g.add_site("ok", vec![("../x".into(), Vec::new())]).is_err());
        assert!(g.add_site("ok", vec![("/abs".into(), Vec::new())]).is_err());
        assert!(
            g.add_site("ok", vec![("a/b.txt".into(), Vec::new())])
                .is_ok()
        );
        assert_eq!(mime_type("a.WASM"), "application/wasm");
    }
}
