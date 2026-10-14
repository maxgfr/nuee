//! Loopback tests: coordinator and workers in one process, no relay, direct addresses only.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use nuee_core::{CoordinatorConfig, Event, JobSpec, Outcome, WorkerConfig};
use nuee_net::driver::{
    BoxFut, CoordinatorDriver, CoordinatorHandle, DriverConfig, HostEvent, JoinError, MemBlobHub,
    TaskExec, close_code, close_code_of, join,
};
use nuee_net::{AdmitConfig, Net, NetConfig, RelayChoice};
use nuee_proto::{
    ALPN_JOB, BoundedBytes, BoundedString, BoundedVec, Caps, Comparator, JobLink, Lease,
    LocalLimits, Manifest, PROTO_VERSION, Payload, SecretKey, TaskKind, VerifyMode, VerifyPolicy,
};
use tokio::time::timeout;

const SECRET: [u8; 16] = [42; 16];

/// Output = input reversed.
#[derive(Debug)]
struct Reverse;

impl TaskExec for Reverse {
    fn run<'a>(
        &'a self,
        _lease: &'a Lease,
        _manifest: &'a Manifest,
        _module: Option<Bytes>,
        input: Bytes,
    ) -> BoxFut<'a, Outcome> {
        Box::pin(async move {
            let mut out = input.to_vec();
            out.reverse();
            Outcome::Ok(out)
        })
    }
}

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

struct Swarm {
    net: Net,
    handle: CoordinatorHandle,
    router: iroh::protocol::Router,
    blobs: MemBlobHub,
}

async fn coordinator(hello_timeout: Duration) -> Swarm {
    let net = bind(1).await;
    let blobs = MemBlobHub::new();
    let cfg = DriverConfig {
        admit: AdmitConfig {
            hello_timeout,
            ..AdmitConfig::default()
        },
        local_slots: 1,
        ..DriverConfig::default()
    };
    let (driver, handle) = CoordinatorDriver::spawn(
        net.clone(),
        SECRET,
        CoordinatorConfig::default(),
        cfg,
        Arc::new(Reverse),
        Arc::new(blobs.clone()),
    )
    .expect("spawn coordinator");
    let router = net.router().accept(ALPN_JOB, driver).spawn();
    Swarm {
        net,
        handle,
        router,
        blobs,
    }
}

async fn worker_net(seed: u8, swarm: &Swarm) -> Net {
    let net = bind(seed).await;
    net.add_peer(swarm.net.id(), None, loopback(&swarm.net));
    net
}

fn link(swarm: &Swarm, secret: [u8; 16]) -> JobLink {
    JobLink {
        coordinator: swarm.net.id(),
        relay: BoundedString::new(),
        secret,
    }
}

fn worker_cfg() -> WorkerConfig {
    WorkerConfig {
        caps: Caps {
            agent: BoundedString::try_from("loopback").expect("agent"),
            browser: false,
            cpu_slots: 2,
            gpu: None,
            mem_mb: 1024,
            kinds: TaskKind::Wasm.bit(),
            cached: BoundedVec::new(),
            exec_allow: BoundedVec::new(),
            store_quota_mb: 0,
            relay: None,
            rtc: false,
        },
        limits: LocalLimits::default(),
    }
}

/// A 20-task job, verified in full with exact comparison; the module is a placeholder blob
/// published in the coordinator's hub (the test executor ignores it).
fn job_spec(swarm: &Swarm, inputs: &[Vec<u8>]) -> JobSpec {
    let module = Bytes::from_static(b"placeholder module");
    let size = u32::try_from(module.len()).expect("size");
    let hash = swarm.blobs.insert(module);
    let manifest = Manifest {
        version: PROTO_VERSION,
        job: 0,
        name: BoundedString::try_from("reverse").expect("name"),
        kind: TaskKind::Wasm,
        module: Some(hash),
        module_size: size,
        max_memory: 1 << 20,
        assets: BoundedVec::new(),
        tasks: u32::try_from(inputs.len()).expect("tasks"),
        verify: VerifyPolicy {
            mode: VerifyMode::Full,
            sample_permille: 0,
            canary_permille: 0,
        },
        compare: Comparator::Exact,
        deadline_ms: 10_000,
        exec: None,
        train: None,
        coordinator: swarm.net.id(),
    };
    JobSpec {
        manifest,
        inputs: inputs
            .iter()
            .map(|i| Payload::Inline(BoundedBytes::try_from(i.as_slice()).expect("inline")))
            .collect(),
        compare: None,
    }
}

async fn wait_job_done(handle: &mut CoordinatorHandle, job: u32) -> Vec<(u32, Vec<u8>)> {
    let mut accepted = std::collections::BTreeMap::new();
    loop {
        let ev = timeout(Duration::from_secs(30), handle.events.recv())
            .await
            .expect("job finished in time")
            .expect("coordinator alive");
        match ev {
            HostEvent::Accepted {
                job: j,
                task,
                output,
                ..
            } if j == job => {
                accepted.insert(task, output.to_vec());
            }
            HostEvent::Event(Event::JobDone { job: j, failed }) if j == job => {
                assert_eq!(failed, 0);
                return accepted.into_iter().collect();
            }
            _ => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_workers_finish_a_verified_job() {
    let mut swarm = coordinator(Duration::from_secs(5)).await;
    let inputs: Vec<Vec<u8>> = (0..20u8).map(|i| vec![i, i + 1, i + 2, 255 - i]).collect();
    let spec = job_spec(&swarm, &inputs);
    let job = swarm.handle.submit(spec).await.expect("submit");

    let mut workers = Vec::new();
    for seed in [2u8, 3] {
        let net = worker_net(seed, &swarm).await;
        let w = join(
            &net,
            &link(&swarm, SECRET),
            worker_cfg(),
            Arc::new(Reverse),
            Arc::new(MemBlobHub::shared(swarm.blobs.map())),
        )
        .await
        .expect("join");
        workers.push((net, w));
    }

    let accepted = wait_job_done(&mut swarm.handle, job).await;
    assert_eq!(accepted.len(), 20);
    for (task, out) in accepted {
        let mut want = inputs[task as usize].clone();
        want.reverse();
        assert_eq!(out, want, "task {task}");
    }
    swarm.handle.shutdown().await.expect("shutdown");
    swarm.router.shutdown().await.expect("router shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wrong_secret_is_refused() {
    let swarm = coordinator(Duration::from_secs(5)).await;
    let net = worker_net(4, &swarm).await;
    let res = join(
        &net,
        &link(&swarm, [0; 16]),
        worker_cfg(),
        Arc::new(Reverse),
        Arc::new(MemBlobHub::new()),
    )
    .await;
    assert!(matches!(res, Err(JoinError::Refused(_))), "{res:?}");

    // The coordinator still admits a worker with the right secret.
    let net = worker_net(5, &swarm).await;
    let mut ok = join(
        &net,
        &link(&swarm, SECRET),
        worker_cfg(),
        Arc::new(Reverse),
        Arc::new(MemBlobHub::new()),
    )
    .await
    .expect("join with the right secret");
    assert!(ok.events.try_recv().is_ok(), "welcome event");
    let status = swarm.handle.status().await.expect("status");
    assert!(status.peers.iter().any(|p| p.id == net.id()));
    assert!(
        !status
            .peers
            .iter()
            .any(|p| p.id == SecretKey::from_bytes(&[4; 32]).public())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_frame_closes_the_connection() {
    let swarm = coordinator(Duration::from_secs(5)).await;
    let net = worker_net(6, &swarm).await;
    let conn = net
        .connect(swarm.net.id(), ALPN_JOB)
        .await
        .expect("connect");
    let (mut send, _recv) = conn.open_bi().await.expect("open");
    send.write_all(&u32::MAX.to_be_bytes())
        .await
        .expect("write prefix");
    timeout(Duration::from_secs(5), conn.closed())
        .await
        .expect("closed by the coordinator");
    assert_eq!(close_code_of(&conn), Some(close_code::PROTOCOL));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn silent_client_is_closed_after_the_hello_timeout() {
    let swarm = coordinator(Duration::from_millis(300)).await;
    let net = worker_net(7, &swarm).await;
    let start = std::time::Instant::now();
    let conn = net
        .connect(swarm.net.id(), ALPN_JOB)
        .await
        .expect("connect");
    timeout(Duration::from_secs(5), conn.closed())
        .await
        .expect("closed by the coordinator");
    assert!(start.elapsed() < Duration::from_secs(3));
    assert_eq!(close_code_of(&conn), Some(close_code::TIMEOUT));
}

/// A hub whose network fetches never reach our own endpoint, as on a real transport: a
/// coordinator must read its own blobs from its store when it runs its own leases.
struct NoSelfFetch {
    own: iroh::EndpointId,
    inner: MemBlobHub,
}

impl nuee_net::driver::BlobHub for NoSelfFetch {
    fn fetch(
        &self,
        from: iroh::EndpointAddr,
        hash: nuee_proto::Hash,
        size: u64,
    ) -> BoxFut<'_, Result<Bytes, nuee_net::driver::BlobError>> {
        if from.id == self.own {
            return Box::pin(async move {
                Err(nuee_net::driver::BlobError::Io(format!(
                    "fetch of {hash} from our own endpoint"
                )))
            });
        }
        self.inner.fetch(from, hash, size)
    }

    fn publish(
        &self,
        bytes: Bytes,
    ) -> BoxFut<'_, Result<nuee_proto::Hash, nuee_net::driver::BlobError>> {
        self.inner.publish(bytes)
    }

    fn local(
        &self,
        hash: nuee_proto::Hash,
        size: u64,
    ) -> BoxFut<'_, Result<Bytes, nuee_net::driver::BlobError>> {
        self.inner.local(hash, size)
    }

    fn admit(&self, peer: iroh::EndpointId) {
        self.inner.admit(peer);
    }

    fn revoke(&self, peer: iroh::EndpointId) {
        self.inner.revoke(peer);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn coordinator_alone_runs_its_own_leases() {
    let net = bind(9).await;
    let blobs = MemBlobHub::new();
    let hub = NoSelfFetch {
        own: net.id(),
        inner: blobs.clone(),
    };
    let cfg = DriverConfig {
        local_slots: 2,
        ..DriverConfig::default()
    };
    let (driver, handle) = CoordinatorDriver::spawn(
        net.clone(),
        SECRET,
        CoordinatorConfig {
            local_slots: 2,
            ..CoordinatorConfig::default()
        },
        cfg,
        Arc::new(Reverse),
        Arc::new(hub),
    )
    .expect("spawn coordinator");
    let router = net.router().accept(ALPN_JOB, driver).spawn();
    let mut swarm = Swarm {
        net,
        handle,
        router,
        blobs,
    };
    // One input travels as a blob to cover every kind of lease data.
    let inputs: Vec<Vec<u8>> = (0..8u8).map(|i| vec![i, 7, 255 - i]).collect();
    let mut spec = job_spec(&swarm, &inputs);
    let big = Bytes::from(vec![3u8; 64]);
    let size = big.len() as u64;
    let hash = swarm.blobs.insert(big.clone());
    spec.inputs[0] = Payload::Blob { hash, size };
    let job = swarm.handle.submit(spec).await.expect("submit");

    let accepted = wait_job_done(&mut swarm.handle, job).await;
    assert_eq!(accepted.len(), inputs.len());
    for (task, out) in accepted {
        let mut want = if task == 0 {
            big.to_vec()
        } else {
            inputs[task as usize].clone()
        };
        want.reverse();
        assert_eq!(out, want, "task {task}");
    }
    swarm.handle.shutdown().await.expect("shutdown");
    swarm.router.shutdown().await.expect("router shutdown");
}

/// Output = input reversed, with its first byte flipped.
#[derive(Debug)]
struct Liar;

impl TaskExec for Liar {
    fn run<'a>(
        &'a self,
        _lease: &'a Lease,
        _manifest: &'a Manifest,
        _module: Option<Bytes>,
        input: Bytes,
    ) -> BoxFut<'a, Outcome> {
        Box::pin(async move {
            let mut out = input.to_vec();
            out.reverse();
            if let Some(b) = out.first_mut() {
                *b ^= 0xff;
            }
            Outcome::Ok(out)
        })
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_coordinator_without_local_slots_still_recomputes_and_bans_a_liar() {
    let net = bind(1).await;
    let blobs = MemBlobHub::new();
    let (driver, handle) = CoordinatorDriver::spawn(
        net.clone(),
        SECRET,
        CoordinatorConfig {
            local_slots: 0,
            verify_slots: 1,
            ..CoordinatorConfig::default()
        },
        DriverConfig {
            local_slots: 0,
            ..DriverConfig::default()
        },
        Arc::new(Reverse),
        Arc::new(blobs.clone()),
    )
    .expect("spawn coordinator");
    let router = net.router().accept(ALPN_JOB, driver).spawn();
    let mut swarm = Swarm {
        net,
        handle,
        router,
        blobs,
    };
    let inputs: Vec<Vec<u8>> = (0..20u8).map(|i| vec![i, i + 1, i + 2, 255 - i]).collect();
    let spec = job_spec(&swarm, &inputs);
    let job = swarm.handle.submit(spec).await.expect("submit");

    let liar_net = worker_net(4, &swarm).await;
    let liar_id = liar_net.id();
    let mut workers = Vec::new();
    for (seed, exec) in [
        (2u8, Arc::new(Reverse) as Arc<dyn TaskExec>),
        (3, Arc::new(Reverse)),
    ] {
        let net = worker_net(seed, &swarm).await;
        let w = join(
            &net,
            &link(&swarm, SECRET),
            worker_cfg(),
            exec,
            Arc::new(MemBlobHub::shared(swarm.blobs.map())),
        )
        .await
        .expect("join");
        workers.push((net, w));
    }
    let liar = join(
        &liar_net,
        &link(&swarm, SECRET),
        worker_cfg(),
        Arc::new(Liar),
        Arc::new(MemBlobHub::shared(swarm.blobs.map())),
    )
    .await
    .expect("join");

    let mut accepted = std::collections::BTreeMap::new();
    let mut banned = false;
    loop {
        let ev = timeout(Duration::from_secs(30), swarm.handle.events.recv())
            .await
            .expect("job finished in time")
            .expect("coordinator alive");
        match ev {
            HostEvent::Accepted {
                job: j,
                task,
                output,
                ..
            } if j == job => {
                accepted.insert(task, output.to_vec());
            }
            HostEvent::Event(Event::Banned { peer, .. }) if peer == liar_id => banned = true,
            HostEvent::Event(Event::JobDone { job: j, failed }) if j == job => {
                assert_eq!(failed, 0);
                break;
            }
            _ => {}
        }
    }
    assert!(
        banned,
        "the liar is banned after the coordinator's own recompute"
    );
    assert_eq!(accepted.len(), 20);
    for (task, out) in accepted {
        let mut want = inputs[task as usize].clone();
        want.reverse();
        assert_eq!(out, want, "task {task}");
    }
    drop(liar);
    swarm.handle.shutdown().await.expect("shutdown");
    swarm.router.shutdown().await.expect("router shutdown");
}

async fn approving_coordinator() -> Swarm {
    let net = bind(1).await;
    let blobs = MemBlobHub::new();
    let (driver, handle) = CoordinatorDriver::spawn(
        net.clone(),
        SECRET,
        CoordinatorConfig {
            approve: true,
            ..CoordinatorConfig::default()
        },
        DriverConfig {
            local_slots: 1,
            ..DriverConfig::default()
        },
        Arc::new(Reverse),
        Arc::new(blobs.clone()),
    )
    .expect("spawn coordinator");
    let router = net.router().accept(ALPN_JOB, driver).spawn();
    Swarm {
        net,
        handle,
        router,
        blobs,
    }
}

async fn wait_pending(handle: &mut CoordinatorHandle, peer: iroh::EndpointId) {
    loop {
        let ev = timeout(Duration::from_secs(10), handle.events.recv())
            .await
            .expect("pending in time")
            .expect("coordinator alive");
        if let HostEvent::Event(Event::PeerPending { peer: p, .. }) = ev
            && p == peer
        {
            return;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blobs_are_granted_only_after_approval() {
    let mut swarm = approving_coordinator().await;
    let net = worker_net(11, &swarm).await;
    let id = net.id();
    let joining = {
        let net = net.clone();
        let link = link(&swarm, SECRET);
        tokio::spawn(async move {
            join(
                &net,
                &link,
                worker_cfg(),
                Arc::new(Reverse),
                Arc::new(MemBlobHub::new()),
            )
            .await
        })
    };
    wait_pending(&mut swarm.handle, id).await;
    assert!(
        !swarm.blobs.is_admitted(&id),
        "a peer waiting for approval must not read the coordinator's blobs"
    );
    swarm.handle.approve(id, true).expect("approve");
    let _worker = timeout(Duration::from_secs(10), joining)
        .await
        .expect("welcome in time")
        .expect("join task")
        .expect("join");
    assert!(swarm.blobs.is_admitted(&id), "an approved peer reads blobs");
    swarm.handle.shutdown().await.expect("shutdown");
    swarm.router.shutdown().await.expect("router shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refused_peer_never_reads_blobs() {
    let mut swarm = approving_coordinator().await;
    let net = worker_net(12, &swarm).await;
    let id = net.id();
    let joining = {
        let net = net.clone();
        let link = link(&swarm, SECRET);
        tokio::spawn(async move {
            join(
                &net,
                &link,
                worker_cfg(),
                Arc::new(Reverse),
                Arc::new(MemBlobHub::new()),
            )
            .await
        })
    };
    wait_pending(&mut swarm.handle, id).await;
    swarm.handle.approve(id, false).expect("refuse");
    let res = timeout(Duration::from_secs(10), joining)
        .await
        .expect("answer in time")
        .expect("join task");
    assert!(matches!(res, Err(JoinError::Refused(_))), "{res:?}");
    assert!(!swarm.blobs.is_admitted(&id));
}

/// A worker announcing a relay on a private address: the coordinator must not record it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_relay_on_a_private_address_is_ignored() {
    let swarm = coordinator(Duration::from_secs(5)).await;
    for (seed, relay) in [
        (13u8, "http://10.0.0.1:3340/"),
        (14, "https://169.254.169.254/"),
        (15, "https://[::1]:443/"),
        (16, "http://relay.example.com/"),
    ] {
        let net = worker_net(seed, &swarm).await;
        let mut cfg = worker_cfg();
        cfg.caps.relay = Some(BoundedString::try_from(relay).expect("relay"));
        let _w = join(
            &net,
            &link(&swarm, SECRET),
            cfg,
            Arc::new(Reverse),
            Arc::new(MemBlobHub::new()),
        )
        .await
        .expect("join");
        let relays: Vec<String> = swarm
            .net
            .peer_addr(net.id())
            .map(|a| a.relay_urls().map(|u| u.to_string()).collect())
            .unwrap_or_default();
        assert!(relays.is_empty(), "{relay} recorded: {relays:?}");
    }
    // A public https relay is still learnt.
    let net = worker_net(17, &swarm).await;
    let mut cfg = worker_cfg();
    cfg.caps.relay = Some(BoundedString::try_from("https://relay.example.com/").expect("relay"));
    let _w = join(
        &net,
        &link(&swarm, SECRET),
        cfg,
        Arc::new(Reverse),
        Arc::new(MemBlobHub::new()),
    )
    .await
    .expect("join");
    let relays: Vec<String> = swarm
        .net
        .peer_addr(net.id())
        .map(|a| a.relay_urls().map(|u| u.to_string()).collect())
        .unwrap_or_default();
    assert_eq!(relays, vec!["https://relay.example.com/".to_owned()]);
}
