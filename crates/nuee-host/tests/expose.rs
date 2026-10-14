//! Exposed services in one process: relay disabled, direct loopback addresses.

use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use nuee_host::{Approver, Expose, HostError, TunnelClient, http_request};
use nuee_net::{Net, NetConfig, RelayChoice, read_frame, send_msg, write_frame};
use nuee_proto::{
    ALPN_HTTP, ALPN_TCP, BoundedBytes, BoundedString, BoundedVec, EndpointId, Header, HttpRequest,
    HttpResponse, MAX_HTTP_MSG, PROOF_HTTP, PROOF_TCP, PROTO_VERSION, SecretKey, ServiceLink,
    TcpMsg, decode_with_limit,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::timeout;

const SECRET: [u8; 16] = [9; 16];
const LIMIT: Duration = Duration::from_secs(30);

async fn bind(seed: u8) -> Net {
    Net::bind(NetConfig {
        secret: SecretKey::from_bytes(&[seed; 32]),
        relay: RelayChoice::Disabled,
        relay_only: false,
        alpns: Vec::new(),
    })
    .await
    .expect("bind")
}

fn loopback(net: &Net) -> Vec<SocketAddr> {
    net.endpoint()
        .bound_sockets()
        .into_iter()
        .map(|s| {
            let ip = match s.ip() {
                IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
            };
            SocketAddr::new(ip, s.port())
        })
        .collect()
}

/// A TCP echo server counting its connections.
async fn echo_server() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listen");
    let port = listener.local_addr().expect("addr").port();
    let count = Arc::new(AtomicUsize::new(0));
    let c = count.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            c.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let (mut r, mut w) = stream.into_split();
                if let Err(e) = tokio::io::copy(&mut r, &mut w).await {
                    eprintln!("echo: {e}");
                }
            });
        }
    });
    (port, count)
}

/// A tiny HTTP service: `/health`, `/echo` (POST, echoes JSON), `/slow`, `/big`.
async fn http_server() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listen");
    let port = listener.local_addr().expect("addr").port();
    let count = Arc::new(AtomicUsize::new(0));
    let c = count.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let c = c.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                    let c = c.clone();
                    async move {
                        c.fetch_add(1, Ordering::SeqCst);
                        let host = req
                            .headers()
                            .get(hyper::header::HOST)
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_owned();
                        let hop = req.headers().contains_key("x-hop");
                        let path = req.uri().path().to_owned();
                        let method = req.method().clone();
                        let body = req.into_body().collect().await.expect("body").to_bytes();
                        let builder = hyper::Response::builder()
                            .header("x-host", host)
                            .header("x-saw-hop", if hop { "yes" } else { "no" });
                        let resp = match (method.as_str(), path.as_str()) {
                            ("GET", "/health") => builder
                                .header("content-type", "text/plain")
                                .body(Full::new(Bytes::from_static(b"ok"))),
                            ("POST", "/echo") => builder
                                .status(201)
                                .header("content-type", "application/json")
                                .body(Full::new(body)),
                            ("GET", "/slow") => {
                                tokio::time::sleep(Duration::from_secs(5)).await;
                                builder.body(Full::new(Bytes::from_static(b"late")))
                            }
                            ("GET", "/big") => builder.body(Full::new(Bytes::from(vec![
                                7u8;
                                nuee_proto::limits::MAX_HTTP_BODY
                                    + 1
                            ]))),
                            _ => builder.status(404).body(Full::new(Bytes::new())),
                        };
                        Ok::<_, Infallible>(resp.expect("response"))
                    }
                });
                if let Err(e) = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .await
                {
                    eprintln!("http server: {e}");
                }
            });
        }
    });
    (port, count)
}

struct Hosted {
    host: Net,
    client: Net,
    link: ServiceLink,
    _router: iroh::protocol::Router,
}

async fn expose_with(port: u16, f: impl FnOnce(&mut Expose)) -> Hosted {
    let host = bind(1).await;
    let client = bind(2).await;
    let mut cfg = Expose::new(port, SECRET);
    f(&mut cfg);
    let link = cfg.link(&host).expect("link");
    let router = cfg.spawn(&host);
    client.add_peer(host.id(), None, loopback(&host));
    Hosted {
        host,
        client,
        link,
        _router: router,
    }
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn tunnel_round_trips_one_mib_each_way() {
    let (port, count) = echo_server().await;
    let h = expose_with(port, |_| {}).await;
    let client = TunnelClient::connect(&h.client, &h.link)
        .await
        .expect("connect");
    let tunnel = client.open().await.expect("open");
    let (mut recv, mut send) = tunnel.into_split();
    let data = pattern(1 << 20, 3);
    let to_send = data.clone();
    let writer = tokio::spawn(async move {
        AsyncWriteExt::write_all(&mut send, &to_send)
            .await
            .expect("write");
        send.finish().expect("finish");
        send
    });
    let mut back = Vec::new();
    timeout(LIMIT, AsyncReadExt::read_to_end(&mut recv, &mut back))
        .await
        .expect("in time")
        .expect("read");
    let _send = writer.await.expect("writer");
    assert_eq!(back.len(), data.len());
    assert!(back == data, "echoed bytes differ");
    assert_eq!(count.load(Ordering::SeqCst), 1);

    // A second tunnel on the same connection works too.
    let tunnel = client.open().await.expect("open again");
    let (mut recv, mut send) = tunnel.into_split();
    AsyncWriteExt::write_all(&mut send, b"ping")
        .await
        .expect("write");
    let mut buf = [0u8; 4];
    timeout(LIMIT, AsyncReadExt::read_exact(&mut recv, &mut buf))
        .await
        .expect("in time")
        .expect("read");
    assert_eq!(&buf, b"ping");
    assert_eq!(count.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_secret_is_refused_and_nothing_forwarded() {
    let (port, count) = echo_server().await;
    let h = expose_with(port, |_| {}).await;
    let mut bad = h.link.clone();
    bad.secret = [1; 16];
    let client = TunnelClient::connect(&h.client, &bad)
        .await
        .expect("connect");
    let err = client.open().await.expect_err("refused");
    assert!(matches!(err, HostError::Refused(_)), "{err:?}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn client_cannot_reach_another_port() {
    let (declared, declared_count) = echo_server().await;
    let (other, other_count) = echo_server().await;
    let h = expose_with(declared, |_| {}).await;

    // A hello carrying extra bytes that name another target is not a hello.
    let conn = h
        .client
        .connect(h.host.id(), ALPN_TCP)
        .await
        .expect("connect");
    let (mut send, mut recv) = conn.open_bi().await.expect("bi");
    let proof = nuee_proto::proof(PROOF_TCP, &SECRET, &h.client.id(), &h.host.id());
    let mut frame = nuee_proto::encode(&TcpMsg::Hello {
        version: PROTO_VERSION,
        proof,
    })
    .expect("encode");
    frame.extend_from_slice(format!("127.0.0.1:{other}").as_bytes());
    write_frame(&mut send, &frame).await.expect("write");
    let answer: TcpMsg = nuee_net::recv_msg(&mut recv, 512).await.expect("answer");
    assert!(matches!(answer, TcpMsg::Refused { .. }), "{answer:?}");

    // A valid hello followed by a request for another target only reaches the declared port.
    let (mut send, mut recv) = conn.open_bi().await.expect("bi");
    send_msg(
        &mut send,
        &TcpMsg::Hello {
            version: PROTO_VERSION,
            proof,
        },
    )
    .await
    .expect("hello");
    let ready: TcpMsg = nuee_net::recv_msg(&mut recv, 512).await.expect("ready");
    assert_eq!(ready, TcpMsg::Ready);
    let ask = format!("CONNECT 127.0.0.1:{other} HTTP/1.1\r\n\r\n");
    send.write_all(ask.as_bytes()).await.expect("write");
    let mut buf = vec![0u8; ask.len()];
    timeout(LIMIT, AsyncReadExt::read_exact(&mut recv, &mut buf))
        .await
        .expect("in time")
        .expect("read");
    assert_eq!(buf, ask.as_bytes(), "the declared echo server answered");
    assert_eq!(declared_count.load(Ordering::SeqCst), 1);
    assert_eq!(other_count.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn approval_refusal_closes_both_protocols() {
    let (port, count) = http_server().await;
    let approve: Approver = Arc::new(|_peer: EndpointId| Box::pin(async { false }));
    let h = expose_with(port, |c| c.approve = Some(approve)).await;
    let client = TunnelClient::connect(&h.client, &h.link)
        .await
        .expect("connect");
    let err = client.open().await.expect_err("refused");
    assert!(matches!(err, HostError::Refused(_)), "{err:?}");
    let resp = http_request(&h.client, &h.link, get("/health"))
        .await
        .expect("answer");
    assert_eq!(resp.status, 403);
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

fn header(name: &str, value: &str) -> Header {
    Header {
        name: BoundedString::try_from(name).expect("name"),
        value: BoundedString::try_from(value).expect("value"),
    }
}

fn request(method: &str, path: &str, headers: Vec<Header>, body: &[u8]) -> HttpRequest {
    HttpRequest {
        proof: [0; 32],
        method: BoundedString::try_from(method).expect("method"),
        path: BoundedString::try_from(path).expect("path"),
        headers: BoundedVec::try_from(headers).expect("headers"),
        body: BoundedBytes::try_from(body).expect("body"),
    }
}

fn get(path: &str) -> HttpRequest {
    request("GET", path, Vec::new(), b"")
}

fn header_of<'a>(resp: &'a HttpResponse, name: &str) -> Option<&'a str> {
    resp.headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str())
}

#[tokio::test(flavor = "multi_thread")]
async fn console_get_and_json_post_round_trip() {
    let (port, _count) = http_server().await;
    let h = expose_with(port, |_| {}).await;

    let resp = http_request(&h.client, &h.link, get("/health"))
        .await
        .expect("health");
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body.as_slice(), b"ok");
    assert_eq!(header_of(&resp, "content-type"), Some("text/plain"));
    let expected_host = format!("localhost:{port}");
    assert_eq!(header_of(&resp, "x-host"), Some(expected_host.as_str()));

    let json = br#"{"hello":"world","n":42}"#;
    let req = request(
        "POST",
        "/echo",
        vec![
            header("content-type", "application/json"),
            header("host", "evil.example:80"),
            header("connection", "x-hop"),
            header("x-hop", "1"),
        ],
        json,
    );
    let resp = http_request(&h.client, &h.link, req).await.expect("post");
    assert_eq!(resp.status, 201);
    assert_eq!(resp.body.as_slice(), json);
    assert_eq!(header_of(&resp, "content-type"), Some("application/json"));
    assert_eq!(header_of(&resp, "x-host"), Some(expected_host.as_str()));
    assert_eq!(header_of(&resp, "x-saw-hop"), Some("no"));
}

/// Postcard twin of `HttpRequest` without the caps, to build requests a client must not send.
#[derive(serde::Serialize)]
struct RawRequest {
    proof: [u8; 32],
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

async fn send_raw(h: &Hosted, frame: &[u8]) -> HttpResponse {
    let conn = h
        .client
        .connect(h.host.id(), ALPN_HTTP)
        .await
        .expect("connect");
    let (mut send, mut recv) = conn.open_bi().await.expect("bi");
    send_hello(h, &mut send).await;
    write_frame(&mut send, frame).await.expect("write");
    send.finish().expect("finish");
    let answer = timeout(LIMIT, read_frame(&mut recv, MAX_HTTP_MSG))
        .await
        .expect("in time")
        .expect("answer");
    decode_with_limit(&answer, MAX_HTTP_MSG).expect("decode")
}

/// The console hello that precedes every request frame.
async fn send_hello(h: &Hosted, send: &mut iroh::endpoint::SendStream) {
    let hello = TcpMsg::Hello {
        version: PROTO_VERSION,
        proof: nuee_proto::proof(PROOF_HTTP, &SECRET, &h.client.id(), &h.host.id()),
    };
    send_msg(send, &hello).await.expect("hello");
}

fn raw(h: &Hosted, headers: usize, body: usize) -> Vec<u8> {
    postcard::to_stdvec(&RawRequest {
        proof: nuee_proto::proof(PROOF_HTTP, &SECRET, &h.client.id(), &h.host.id()),
        method: "POST".into(),
        path: "/echo".into(),
        headers: (0..headers)
            .map(|i| (format!("x-h{i}"), "v".to_owned()))
            .collect(),
        body: vec![1; body],
    })
    .expect("encode")
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_requests_are_refused() {
    let (port, count) = http_server().await;
    let h = expose_with(port, |_| {}).await;
    let cap = nuee_proto::limits::MAX_HTTP_BODY;

    // The twin encodes exactly like the real type when within the caps.
    let ok = send_raw(&h, &raw(&h, 64, 16)).await;
    assert_eq!(ok.status, 201);
    assert_eq!(count.load(Ordering::SeqCst), 1);

    let too_many = send_raw(&h, &raw(&h, 65, 16)).await;
    assert_eq!(too_many.status, 400);
    let too_big = send_raw(&h, &raw(&h, 0, cap + 1)).await;
    assert_eq!(too_big.status, 400);

    // A frame announcing more than the cap is refused before it is read.
    let conn = h
        .client
        .connect(h.host.id(), ALPN_HTTP)
        .await
        .expect("connect");
    let (mut send, mut recv) = conn.open_bi().await.expect("bi");
    send_hello(&h, &mut send).await;
    let huge = u32::try_from(MAX_HTTP_MSG + 1).expect("fits");
    send.write_all(&huge.to_be_bytes()).await.expect("write");
    let answer = timeout(LIMIT, read_frame(&mut recv, MAX_HTTP_MSG))
        .await
        .expect("in time")
        .expect("answer");
    let resp: HttpResponse = decode_with_limit(&answer, MAX_HTTP_MSG).expect("decode");
    assert_eq!(resp.status, 413);

    // A response body above the cap is not relayed.
    let big = http_request(&h.client, &h.link, get("/big"))
        .await
        .expect("answer");
    assert_eq!(big.status, 502);
    assert_eq!(count.load(Ordering::SeqCst), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn slow_upstream_times_out() {
    let (port, _count) = http_server().await;
    let h = expose_with(port, |c| c.http_timeout = Duration::from_millis(500)).await;
    let started = std::time::Instant::now();
    let resp = http_request(&h.client, &h.link, get("/slow"))
        .await
        .expect("answer");
    assert_eq!(resp.status, 504);
    assert!(started.elapsed() < Duration::from_secs(4));
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_secret_on_console_is_forbidden() {
    let (port, count) = http_server().await;
    let h = expose_with(port, |_| {}).await;
    let mut bad = h.link.clone();
    bad.secret = [0; 16];
    let resp = http_request(&h.client, &bad, get("/health"))
        .await
        .expect("answer");
    assert_eq!(resp.status, 403);
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn tunnel_idle_timeout_closes() {
    let (port, _count) = echo_server().await;
    let h = expose_with(port, |c| c.idle_timeout = Duration::from_millis(300)).await;
    let client = TunnelClient::connect(&h.client, &h.link)
        .await
        .expect("connect");
    let tunnel = client.open().await.expect("open");
    let (mut recv, _send) = tunnel.into_split();
    let mut buf = Vec::new();
    let res = timeout(
        Duration::from_secs(10),
        AsyncReadExt::read_to_end(&mut recv, &mut buf),
    )
    .await
    .expect("closed by the host");
    assert!(buf.is_empty(), "{res:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn pending_approval_never_blocks_an_approved_peer() {
    let (echo, _echo_count) = echo_server().await;
    let (console, _console_count) = http_server().await;
    let a = bind(2).await;
    let a_id = a.id();
    let prompts = Arc::new(AtomicUsize::new(0));
    let seen = prompts.clone();
    // Peer A is approved at once; any other peer's prompt never gets an answer.
    let approve: Approver = Arc::new(move |peer: EndpointId| {
        let seen = seen.clone();
        Box::pin(async move {
            if peer == a_id {
                return true;
            }
            seen.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<bool>().await
        })
    });

    let host = bind(1).await;
    let mut tcp_cfg = Expose::new(echo, SECRET);
    tcp_cfg.approve = Some(approve);
    tcp_cfg.http = false;
    let link = tcp_cfg.link(&host).expect("link");
    let _router = tcp_cfg.spawn(&host);
    a.add_peer(host.id(), None, loopback(&host));

    let tunnel_a = TunnelClient::connect(&a, &link).await.expect("connect a");
    let first = tunnel_a.open().await.expect("a approved");
    drop(first);

    let b = bind(3).await;
    b.add_peer(host.id(), None, loopback(&host));
    let b_link = link.clone();
    let b_task = tokio::spawn(async move {
        let client = TunnelClient::connect(&b, &b_link).await.expect("connect b");
        let first = client.open();
        let second = client.open();
        let _never = tokio::join!(first, second);
    });
    timeout(LIMIT, async {
        while prompts.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("b is being asked");

    // While the operator has not answered B, A's new tunnels still open and work.
    let tunnel = timeout(Duration::from_secs(5), tunnel_a.open())
        .await
        .expect("a not blocked by b's prompt")
        .expect("open");
    let (mut recv, mut send) = tunnel.into_split();
    AsyncWriteExt::write_all(&mut send, b"ping")
        .await
        .expect("write");
    let mut buf = [0u8; 4];
    timeout(LIMIT, AsyncReadExt::read_exact(&mut recv, &mut buf))
        .await
        .expect("in time")
        .expect("read");
    assert_eq!(&buf, b"ping");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        prompts.load(Ordering::SeqCst),
        1,
        "concurrent streams of one undecided peer share one prompt"
    );
    b_task.abort();

    // Same on the console: B waits on its prompt, A's requests are answered.
    let prompts = Arc::new(AtomicUsize::new(0));
    let seen = prompts.clone();
    let approve: Approver = Arc::new(move |peer: EndpointId| {
        let seen = seen.clone();
        Box::pin(async move {
            if peer == a_id {
                return true;
            }
            seen.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<bool>().await
        })
    });
    let host = bind(4).await;
    let mut http_cfg = Expose::new(console, SECRET);
    http_cfg.approve = Some(approve);
    let link = http_cfg.link(&host).expect("link");
    let _router = http_cfg.spawn(&host);
    a.add_peer(host.id(), None, loopback(&host));
    let resp = http_request(&a, &link, get("/health")).await.expect("a");
    assert_eq!(resp.status, 200);

    let b = bind(5).await;
    b.add_peer(host.id(), None, loopback(&host));
    let b_link = link.clone();
    let b_task = tokio::spawn(async move {
        let _never = http_request(&b, &b_link, get("/health")).await;
    });
    timeout(LIMIT, async {
        while prompts.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("b is being asked");
    let resp = timeout(
        Duration::from_secs(5),
        http_request(&a, &link, get("/health")),
    )
    .await
    .expect("a not blocked by b's prompt")
    .expect("answer");
    assert_eq!(resp.status, 200);
    b_task.abort();
}

/// A client without the secret announces a maximal request: the host answers 403 from the
/// small hello and never waits for, or allocates, the announced body.
#[tokio::test(flavor = "multi_thread")]
async fn console_checks_the_proof_before_reading_the_body() {
    let (port, count) = http_server().await;
    let h = expose_with(port, |_| {}).await;
    let conn = h
        .client
        .connect(h.host.id(), ALPN_HTTP)
        .await
        .expect("connect");
    let (mut send, mut recv) = conn.open_bi().await.expect("bi");
    let hello = TcpMsg::Hello {
        version: PROTO_VERSION,
        proof: [7; 32],
    };
    send_msg(&mut send, &hello).await.expect("hello");
    let announced = u32::try_from(MAX_HTTP_MSG).expect("fits");
    send.write_all(&announced.to_be_bytes())
        .await
        .expect("announce");
    let answer = timeout(Duration::from_secs(3), read_frame(&mut recv, MAX_HTTP_MSG))
        .await
        .expect("answered without the body")
        .expect("answer");
    let resp: HttpResponse = decode_with_limit(&answer, MAX_HTTP_MSG).expect("decode");
    assert_eq!(resp.status, 403);
    assert_eq!(count.load(Ordering::SeqCst), 0);

    // A hello frame above its small cap is refused too.
    let (mut send, mut recv) = conn.open_bi().await.expect("bi");
    send.write_all(&announced.to_be_bytes())
        .await
        .expect("announce");
    let answer = timeout(Duration::from_secs(3), read_frame(&mut recv, MAX_HTTP_MSG))
        .await
        .expect("answered at once")
        .expect("answer");
    let resp: HttpResponse = decode_with_limit(&answer, MAX_HTTP_MSG).expect("decode");
    assert_eq!(resp.status, 400);
}

/// Connections that never prove the secret are closed after the hello deadline, on both
/// ALPNs, even when they open no stream at all.
#[tokio::test(flavor = "multi_thread")]
async fn idle_unauthenticated_connections_are_closed() {
    let (port, _count) = http_server().await;
    let h = expose_with(port, |c| {
        c.admit.hello_timeout = Duration::from_millis(300);
    })
    .await;
    for alpn in [ALPN_TCP, ALPN_HTTP] {
        let conn = h.client.connect(h.host.id(), alpn).await.expect("connect");
        timeout(Duration::from_secs(5), conn.closed())
            .await
            .expect("closed by the host");
        assert_eq!(
            nuee_net::driver::close_code_of(&conn),
            Some(nuee_net::driver::close_code::TIMEOUT)
        );
    }
    // A client that proved the secret keeps its connection while idle.
    let client = TunnelClient::connect(&h.client, &h.link)
        .await
        .expect("connect");
    let tunnel = client.open().await.expect("first tunnel");
    drop(tunnel);
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(
        client.open().await.is_ok(),
        "an authenticated connection stays open"
    );
}

/// One connection opening many silent streams cannot take every pending slot of the host.
#[tokio::test(flavor = "multi_thread")]
async fn one_connection_cannot_take_every_pending_slot() {
    let (port, _count) = http_server().await;
    let h = expose_with(port, |_| {}).await;
    let conn = h
        .client
        .connect(h.host.id(), ALPN_HTTP)
        .await
        .expect("connect");
    let mut held = Vec::new();
    for _ in 0..nuee_proto::limits::MAX_PENDING_UNAUTH {
        let (mut send, recv) = conn.open_bi().await.expect("bi");
        // A stream reaches the host only once something is written on it.
        send.write_all(&[0]).await.expect("first byte");
        held.push((send, recv));
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let other = bind(3).await;
    other.add_peer(h.host.id(), None, loopback(&h.host));
    let resp = timeout(
        Duration::from_secs(10),
        http_request(&other, &h.link, get("/health")),
    )
    .await
    .expect("in time")
    .expect("answer");
    assert_eq!(resp.status, 200);
}
