//! Budgets of the blob stores: an in-memory store's cap is reserved before a fetch and
//! given back when a blob is untagged, a coordinator refuses oversized blob outputs before
//! fetching them, and released copies are removed from a store member's disk.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use bytes::Bytes;
use iroh::protocol::Router;
use nuee_net::driver::{BlobError, BlobHub};
use nuee_net::{Net, NetConfig, RelayChoice};
use nuee_proto::{ALPN_PIN, BoundedString, BoundedVec, Caps, Hash, JobLink, SecretKey};
use nuee_store::durable::{
    DurabilityService, ReplicaAnswer, ServiceConfig, StoreMember, release_pin, request_replicas,
};
use nuee_store::{
    HubConfig, OutputCappedHub, Registry, StoreError, StoreHub, blob_tag, share_bytes,
};
use tokio::time::timeout;

const LIMIT: Duration = Duration::from_secs(60);

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

/// A node serving `blobs` from an in-memory store to `reader`.
async fn provider(seed: u8, reader: &Net, blobs: &[Vec<u8>]) -> (Net, Router, Vec<Hash>) {
    let net = bind(seed).await;
    let hub = StoreHub::memory(&net, HubConfig::default());
    let mut hashes = Vec::new();
    for b in blobs {
        hashes.push(hub.add_bytes(Bytes::from(b.clone())).await.expect("add"));
    }
    hub.gate().admit(reader.id());
    let router = net
        .router()
        .accept(iroh_blobs::ALPN, hub.protocol())
        .spawn();
    reader.add_peer(net.id(), None, loopback(&net));
    (net, router, hashes)
}

fn capped(cap: u64) -> HubConfig {
    HubConfig {
        mem_cap: cap,
        ..HubConfig::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn untagging_gives_memory_back() {
    let net = bind(70).await;
    let hub = StoreHub::memory(&net, capped(1000));
    let a = hub
        .add_bytes(Bytes::from(vec![1u8; 600]))
        .await
        .expect("fits");
    assert_eq!(hub.mem_used(), Some(600));
    hub.untag(&blob_tag(a)).await.expect("untag");
    assert_eq!(hub.mem_used(), Some(0));
    hub.add_bytes(Bytes::from(vec![2u8; 600]))
        .await
        .expect("room was given back");
    assert_eq!(hub.mem_used(), Some(600));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_fetches_cannot_overrun_the_cap() {
    let reader = bind(71).await;
    let (p, router, hashes) = provider(72, &reader, &[vec![3u8; 600], vec![4u8; 600]]).await;
    let hub = StoreHub::memory(&reader, capped(1000));
    let addr = reader.peer_addr(p.id()).expect("provider address");
    let (mut pa, mut pb) = (|_, _| {}, |_, _| {});
    let (a, b) = tokio::join!(
        hub.fetch_to_store(addr.clone(), hashes[0], 600, &mut pa),
        hub.fetch_to_store(addr, hashes[1], 600, &mut pb),
    );
    let refused = [&a, &b]
        .iter()
        .filter(|r| matches!(r, Err(StoreError::TooLarge { .. })))
        .count();
    assert_eq!(refused, 1, "{a:?} {b:?}");
    assert!(a.is_ok() || b.is_ok(), "{a:?} {b:?}");
    assert_eq!(hub.mem_used(), Some(600));
    router.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_outputs_are_refused_before_fetching_and_none_is_kept() {
    let reader = bind(73).await;
    let (p, router, hashes) = provider(74, &reader, &[vec![5u8; 600]]).await;
    let hub = StoreHub::memory(&reader, capped(1000));
    let outputs = OutputCappedHub::new(hub.clone(), 500);
    let addr = reader.peer_addr(p.id()).expect("provider address");
    // Declared above the cap: refused without a transfer, nothing reserved.
    let res = outputs.fetch(addr.clone(), hashes[0], 600).await;
    assert!(matches!(res, Err(BlobError::TooLarge { .. })), "{res:?}");
    assert_eq!(hub.mem_used(), Some(0));
    assert!(!hub.present(hashes[0]).await.expect("status"));
    // Within the cap: fetched, handed over, and not kept counted in the store.
    let outputs = OutputCappedHub::new(hub.clone(), 1000);
    let bytes = outputs.fetch(addr, hashes[0], 600).await.expect("fetch");
    assert_eq!(bytes.len(), 600);
    assert_eq!(hub.mem_used(), Some(0));
    router.shutdown().await.expect("shutdown");
}

fn store_caps() -> Caps {
    Caps {
        agent: BoundedString::try_from("test").expect("agent"),
        browser: false,
        cpu_slots: 1,
        gpu: None,
        mem_mb: 0,
        kinds: 0,
        cached: BoundedVec::new(),
        exec_allow: BoundedVec::new(),
        store_quota_mb: 64,
        relay: None,
        rtc: false,
    }
}

struct Durable {
    job: JobLink,
    svc: DurabilityService,
    _task: nuee_store::durable::ServiceTask,
    _router: Router,
    member: StoreMember,
    member_hub: StoreHub,
    _member_router: Router,
    _dirs: Vec<tempfile::TempDir>,
    sharer: Net,
    sharer_hub: StoreHub,
}

/// A coordinator, one store member (`quota` bytes, garbage collection every `gc`) and a
/// sharer.
async fn durable(base: u8, quota: u64, gc: Duration) -> Durable {
    let coord = bind(base).await;
    let job = JobLink {
        coordinator: coord.id(),
        relay: BoundedString::new(),
        secret: [base; 16],
    };
    let svc = DurabilityService::new(
        &coord,
        job.secret,
        ServiceConfig {
            tick: Duration::from_millis(200),
            ..ServiceConfig::default()
        },
        None,
    )
    .expect("service");
    let task = svc.start().expect("start");
    let router = coord.router().accept(ALPN_PIN, svc.clone()).spawn();

    let net = bind(base + 1).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let member_hub = StoreHub::open(
        &net,
        dir.path(),
        HubConfig {
            gc_interval: gc,
            ..HubConfig::default()
        },
    )
    .await
    .expect("open");
    let reg = Registry::load(&StoreMember::default_registry(dir.path())).expect("registry");
    let member = StoreMember::new(&member_hub, &job, quota, reg).expect("member");
    let member_router = member.spawn();

    let sharer = bind(base + 2).await;
    let sharer_dir = tempfile::tempdir().expect("tempdir");
    let sharer_hub = StoreHub::open(&sharer, sharer_dir.path(), HubConfig::default())
        .await
        .expect("sharer store");
    let nets = [&coord, &net, &sharer];
    for a in nets {
        for b in nets {
            if a.id() != b.id() {
                a.add_peer(b.id(), None, loopback(b));
            }
        }
    }
    svc.member_joined(net.id(), &store_caps());
    Durable {
        job,
        svc,
        _task: task,
        _router: router,
        member,
        member_hub,
        _member_router: member_router,
        _dirs: vec![dir, sharer_dir],
        sharer,
        sharer_hub,
    }
}

fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

async fn keep_one(d: &Durable, seed: u64) -> (Hash, nuee_store::ShareHandle) {
    let share = share_bytes(
        &d.sharer,
        &d.sharer_hub,
        &[("f.bin".to_owned(), noise(200_000, seed))],
    )
    .await
    .expect("share");
    let pin = share.link.pin_link();
    let answer = timeout(LIMIT, request_replicas(&d.sharer, &d.job, &pin, 1))
        .await
        .expect("in time")
        .expect("request");
    assert_eq!(answer, ReplicaAnswer::Accepted(1));
    (pin.hash, share)
}

async fn until<F: AsyncFnMut() -> bool>(what: &str, mut cond: F) {
    let deadline = tokio::time::Instant::now() + LIMIT;
    while !cond().await {
        assert!(tokio::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn released_copies_leave_the_member_disk() {
    let d = durable(90, 64 << 20, Duration::from_millis(300)).await;
    let (hash, _share) = keep_one(&d, 1).await;
    until("the copy stored", async || d.member.held().contains(&hash)).await;
    assert!(d.member_hub.present(hash).await.expect("status"));
    let released = timeout(LIMIT, release_pin(&d.sharer, &d.job, hash))
        .await
        .expect("in time")
        .expect("release");
    assert!(released);
    until("the copy dropped", async || {
        !d.member.held().contains(&hash)
    })
    .await;
    until("the copy reclaimed", async || {
        !d.member_hub.present(hash).await.expect("status")
    })
    .await;
    assert_eq!(d.member.reclaiming().await.expect("reclaiming"), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_released_copy_counts_against_the_quota_until_reclaimed() {
    // Room for one copy only, and no garbage collection during the test.
    let d = durable(94, 300_000, Duration::from_secs(3600)).await;
    let (first, _a) = keep_one(&d, 2).await;
    until("the first copy stored", async || {
        d.member.held().contains(&first)
    })
    .await;
    assert!(
        timeout(LIMIT, release_pin(&d.sharer, &d.job, first))
            .await
            .expect("in time")
            .expect("release")
    );
    until("the first copy dropped", async || {
        !d.member.held().contains(&first)
    })
    .await;
    assert!(d.member_hub.present(first).await.expect("status"));
    // The member takes the copy out of its registry before it finishes the release.
    until("the first copy counted until reclaimed", async || {
        d.member.reclaiming().await.expect("reclaiming") > 0
    })
    .await;

    // Its bytes are still on disk: a second copy would go over the quota.
    let (second, _b) = keep_one(&d, 3).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!d.member.held().contains(&second));
    let state = d.svc.state();
    let blob = state
        .blobs
        .iter()
        .find(|b| b.hash == second)
        .expect("tracked");
    assert!(blob.holding().is_empty(), "{blob:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_copy_being_released_still_counts_against_the_quota() {
    // Room for one copy only, and no garbage collection during the test.
    let d = durable(98, 300_000, Duration::from_secs(3600)).await;
    let (first, _a) = keep_one(&d, 4).await;
    until("the first copy stored", async || {
        d.member.held().contains(&first)
    })
    .await;
    // A second order races the release of the first: whenever the member looks at it, the
    // first copy's bytes are still on disk, so it goes over the quota.
    let (released, (second, _b)) = tokio::join!(
        async {
            timeout(LIMIT, release_pin(&d.sharer, &d.job, first))
                .await
                .expect("in time")
                .expect("release")
        },
        keep_one(&d, 5)
    );
    assert!(released);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!d.member.held().contains(&second));
    let state = d.svc.state();
    let blob = state
        .blobs
        .iter()
        .find(|b| b.hash == second)
        .expect("tracked");
    assert!(blob.holding().is_empty(), "{blob:?}");
}
