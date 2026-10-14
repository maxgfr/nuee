//! Durable storage in process: a coordinator running the durability service, three store
//! members and a sharer, all on loopback without relay.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use iroh::protocol::Router;
use nuee_net::{Net, NetConfig, RelayChoice};
use nuee_proto::{
    ALPN_PIN, BoundedString, BoundedVec, Caps, EndpointId, FileLink, Hash, JobLink, PinLink,
    PinRefusal, Provider, SecretKey,
};
use nuee_store::challenge::{self, Verdict};
use nuee_store::durable::{
    Challenged, DurabilityManager, DurabilityService, DurableConfig, DurableState, HolderState,
    Keep, Order, Placement, ReplicaAnswer, Replicated, Report, ServiceConfig, StoreMember,
    keep_durably, list_pins, release_pin, request_replicas, where_is,
};
use nuee_store::{HubConfig, Registry, ShareHandle, StoreError, StoreHub, get_bytes, share_bytes};
use tempfile::TempDir;
use tokio::time::timeout;

const LIMIT: Duration = Duration::from_secs(90);
const MB: u64 = 1024 * 1024;

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

/// Deterministic pseudo-random bytes.
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

fn files() -> Vec<(String, Vec<u8>)> {
    vec![
        ("rapport.txt".to_owned(), b"durable".to_vec()),
        ("data/blob.bin".to_owned(), noise(300_000, 3)),
    ]
}

fn caps(quota_mb: u64) -> Caps {
    Caps {
        agent: BoundedString::try_from("test").expect("agent"),
        browser: false,
        cpu_slots: 1,
        gpu: None,
        mem_mb: 0,
        kinds: 0,
        cached: BoundedVec::new(),
        exec_allow: BoundedVec::new(),
        store_quota_mb: quota_mb,
        relay: None,
        rtc: false,
    }
}

struct MemberNode {
    seed: u8,
    net: Net,
    member: StoreMember,
    router: Router,
    _dir: TempDir,
    registry: PathBuf,
}

async fn member_node(seed: u8, job: &JobLink, quota: u64, registry: Option<PathBuf>) -> MemberNode {
    let net = bind(seed).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let hub = StoreHub::open(&net, dir.path(), HubConfig::default())
        .await
        .expect("open store");
    let registry = registry.unwrap_or_else(|| StoreMember::default_registry(dir.path()));
    let reg = Registry::load(&registry).expect("registry");
    let member = StoreMember::new(&hub, job, quota, reg).expect("member");
    let router = member.spawn();
    MemberNode {
        seed,
        net,
        member,
        router,
        _dir: dir,
        registry,
    }
}

struct Swarm {
    job: JobLink,
    coord: Net,
    svc: DurabilityService,
    _task: nuee_store::durable::ServiceTask,
    _router: Router,
    state_dir: TempDir,
    members: Vec<MemberNode>,
    sharer: Net,
    _sharer_dir: TempDir,
    share: ShareHandle,
}

impl Swarm {
    fn nets(&self) -> Vec<&Net> {
        let mut v = vec![&self.coord, &self.sharer];
        v.extend(self.members.iter().map(|m| &m.net));
        v
    }

    fn introduce_all(&self) {
        let nets = self.nets();
        for a in &nets {
            for b in &nets {
                if a.id() != b.id() {
                    a.add_peer(b.id(), None, loopback(b));
                }
            }
        }
    }

    fn member(&self, id: EndpointId) -> &MemberNode {
        self.members
            .iter()
            .find(|m| m.net.id() == id)
            .expect("known member")
    }

    fn holding(&self, hash: Hash) -> Vec<EndpointId> {
        self.svc
            .state()
            .blobs
            .iter()
            .find(|b| b.hash == hash)
            .map(|b| b.holding())
            .unwrap_or_default()
    }

    fn file_link_on(&self, holders: &[EndpointId]) -> FileLink {
        let mut link = self.share.link.clone();
        let mut providers = BoundedVec::new();
        for id in holders {
            providers
                .push(Provider {
                    id: *id,
                    relay: None,
                })
                .expect("provider");
        }
        link.providers = providers;
        link
    }
}

/// Builds the swarm: `quotas` (announced MiB, real bytes) per member, `base` seeds apart.
async fn swarm(base: u8, quotas: &[(u64, u64)], durable: DurableConfig) -> Swarm {
    let coord = bind(base).await;
    let job = JobLink {
        coordinator: coord.id(),
        relay: BoundedString::new(),
        secret: [base; 16],
    };
    let state_dir = tempfile::tempdir().expect("tempdir");
    let svc = DurabilityService::new(
        &coord,
        job.secret,
        ServiceConfig {
            durable,
            tick: Duration::from_millis(200),
            ..ServiceConfig::default()
        },
        Some(DurabilityService::default_path(state_dir.path())),
    )
    .expect("service");
    let task = svc.start().expect("first start");
    let router = coord.router().accept(ALPN_PIN, svc.clone()).spawn();

    let mut members = Vec::new();
    for (i, (_, real)) in quotas.iter().enumerate() {
        members.push(member_node(base + 1 + i as u8, &job, *real, None).await);
    }
    let sharer = bind(base + 20).await;
    let sharer_dir = tempfile::tempdir().expect("tempdir");
    let sharer_hub = StoreHub::open(&sharer, sharer_dir.path(), HubConfig::default())
        .await
        .expect("sharer store");
    let share = share_bytes(&sharer, &sharer_hub, &files())
        .await
        .expect("share");
    let s = Swarm {
        job,
        coord,
        svc,
        _task: task,
        _router: router,
        state_dir,
        members,
        sharer,
        _sharer_dir: sharer_dir,
        share,
    };
    s.introduce_all();
    for (m, (announced, _)) in s.members.iter().zip(quotas) {
        s.svc.member_joined(m.net.id(), &caps(*announced));
    }
    s
}

fn quick() -> DurableConfig {
    DurableConfig {
        absent_after_ms: 2_000,
        challenge_every_ms: 60 * 60 * 1000,
        probe_every_ms: 300,
        retry_after_ms: 60_000,
    }
}

async fn ask(s: &Swarm, replicas: u8) -> Hash {
    let pin = s.share.link.pin_link();
    let answer = timeout(LIMIT, request_replicas(&s.sharer, &s.job, &pin, replicas))
        .await
        .expect("in time")
        .expect("request");
    assert_eq!(answer, ReplicaAnswer::Accepted(replicas));
    pin.hash
}

async fn wait_for<F: Fn(&Swarm) -> bool>(s: &Swarm, what: &str, cond: F) {
    let deadline = tokio::time::Instant::now() + LIMIT;
    while !cond(s) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}: {:?}",
            s.svc.state()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn download(s: &Swarm, seed: u8, holders: &[EndpointId]) -> Vec<(String, Vec<u8>)> {
    let net = bind(seed).await;
    for h in holders {
        net.add_peer(*h, None, loopback(&s.member(*h).net));
    }
    let hub = StoreHub::memory(&net, HubConfig::default());
    let link = s.file_link_on(holders);
    let got = timeout(LIMIT, get_bytes(&net, &hub, &link, |_, _| {}))
        .await
        .expect("in time")
        .expect("download from holders");
    net.close().await;
    got
}

/// Downloads with `link` as handed out, from a fresh node that knows every member.
async fn download_link(s: &Swarm, seed: u8, link: &FileLink) -> Vec<(String, Vec<u8>)> {
    let net = bind(seed).await;
    for m in &s.members {
        net.add_peer(m.net.id(), None, loopback(&m.net));
    }
    let hub = StoreHub::memory(&net, HubConfig::default());
    let got = timeout(LIMIT, get_bytes(&net, &hub, link, |_, _| {}))
        .await
        .expect("in time")
        .expect("download with the durable link");
    net.close().await;
    got
}

async fn kill(m: &MemberNode) {
    m.router.shutdown().await.expect("shutdown");
    m.net.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_replicas_survive_the_sharer_and_a_restart() {
    let s = swarm(10, &[(64, 64 * MB); 3], quick()).await;
    // What `nuee share --replicas 2 --on <link>` does: ask, wait, print the durable link.
    let me = nuee_store::self_provider(&s.sharer);
    let (placement, durable) = timeout(
        LIMIT,
        keep_durably(
            &s.sharer,
            &s.job,
            &s.share.link,
            Keep::Replicas(2),
            LIMIT,
            Some(me.clone()),
            |_| {},
        ),
    )
    .await
    .expect("in time")
    .expect("keep");
    let hash = s.share.link.hash;
    let Placement::Placed(placed) = &placement else {
        panic!("placement not reached: {placement:?}");
    };
    assert_eq!(placed.len(), 2);
    assert_eq!(durable.providers.len(), 3);
    assert_eq!(durable.providers.last(), Some(&me), "the sharer comes last");
    assert_eq!((durable.hash, durable.key), (hash, s.share.link.key));
    wait_for(&s, "two holders", |s| s.holding(hash).len() == 2).await;

    // Exactly two members store it; the third does not.
    let holders = s.holding(hash);
    let stored: Vec<EndpointId> = s
        .members
        .iter()
        .filter(|m| m.member.held().contains(&hash))
        .map(|m| m.net.id())
        .collect();
    assert_eq!(stored.len(), 2);
    for h in &holders {
        assert!(stored.contains(h));
    }
    // Members only ever see the ciphertext: their registry has no key.
    let reg = std::fs::read_to_string(&s.member(holders[0]).registry).expect("registry");
    let key_hex: String = s
        .share
        .link
        .key
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert!(!reg.contains(&key_hex));

    // The sharer goes away; a fresh downloader gets the file with the durable link alone.
    s.share.shutdown().await.expect("stop sharing");
    s.sharer.close().await;
    assert_eq!(download_link(&s, 40, &durable).await, files());
    assert_eq!(download(&s, 42, &holders).await, files());

    // Anyone in the swarm can ask where it lives now; an unknown hash has no holder.
    let asker = bind(43).await;
    asker.add_peer(s.coord.id(), None, loopback(&s.coord));
    let mut now: Vec<EndpointId> = where_is(&asker, &s.job, hash)
        .await
        .expect("where")
        .iter()
        .map(|p| p.id)
        .collect();
    let mut want = holders.clone();
    now.sort();
    want.sort();
    assert_eq!(now, want);
    assert!(
        where_is(&asker, &s.job, Hash([0xee; 32]))
            .await
            .expect("where unknown")
            .is_empty()
    );
    asker.close().await;

    // The state on disk lets a restarted coordinator resume with the same holders.
    let path = DurabilityService::default_path(s.state_dir.path());
    let saved: DurableState =
        serde_json::from_slice(&std::fs::read(&path).expect("durable.json")).expect("parse");
    let blob = saved
        .blobs
        .iter()
        .find(|b| b.hash == hash)
        .expect("tracked");
    assert_eq!(blob.target, 2);
    let restarted =
        DurabilityService::new(&s.coord, s.job.secret, ServiceConfig::default(), Some(path))
            .expect("restart");
    let again = restarted.state();
    let blob = again
        .blobs
        .iter()
        .find(|b| b.hash == hash)
        .expect("tracked");
    let mut a = blob.holding();
    let mut b = holders.clone();
    a.sort();
    b.sort();
    assert_eq!(a, b);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_absent_holder_is_replaced() {
    let s = swarm(50, &[(64, 64 * MB); 3], quick()).await;
    let hash = ask(&s, 2).await;
    wait_for(&s, "two holders", |s| s.holding(hash).len() == 2).await;
    let holders = s.holding(hash);
    let spare = s
        .members
        .iter()
        .map(|m| m.net.id())
        .find(|id| !holders.contains(id))
        .expect("spare");

    // The sharer stops, so the new copy can only come from the remaining holder.
    s.share.shutdown().await.expect("stop sharing");
    s.sharer.close().await;
    let dead = holders[0];
    let alive = holders[1];
    kill(s.member(dead)).await;

    wait_for(&s, "a replacement copy", |s| {
        let h = s.holding(hash);
        h.len() == 2 && h.contains(&spare) && h.contains(&alive)
    })
    .await;
    let state = s.svc.state();
    let blob = state
        .blobs
        .iter()
        .find(|b| b.hash == hash)
        .expect("tracked");
    let lost = blob.holders.iter().find(|h| h.member == dead);
    assert!(
        lost.is_none_or(|h| h.state == HolderState::Lost),
        "{lost:?}"
    );
    assert!(s.member(spare).member.held().contains(&hash));
    assert_eq!(download(&s, 41, &[spare, alive]).await, files());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_holder_that_lost_its_data_fails_the_challenge() {
    let durable = DurableConfig {
        challenge_every_ms: 1_000,
        ..quick()
    };
    let mut s = swarm(80, &[(64, 64 * MB); 3], durable).await;
    let hash = ask(&s, 2).await;
    wait_for(&s, "two holders", |s| s.holding(hash).len() == 2).await;
    let holders = s.holding(hash);
    let size = s.share.link.size;

    // An honest holder passes a challenge against the root hash alone.
    let verdict = challenge::challenge(&s.coord, holders[1], hash, size, [7; 32], 8).await;
    assert!(verdict.passed(), "{verdict:?}");

    // The first holder loses its disk but keeps its identity and its (now false) records.
    let liar_id = holders[0];
    let idx = s
        .members
        .iter()
        .position(|m| m.net.id() == liar_id)
        .expect("member");
    let old = s.members.remove(idx);
    kill(&old).await;
    let liar = member_node(old.seed, &s.job, 64 * MB, Some(old.registry.clone())).await;
    assert!(liar.member.held().contains(&hash));
    s.members.push(liar);
    s.introduce_all();

    let verdict = challenge::challenge(&s.coord, liar_id, hash, size, [8; 32], 8).await;
    assert!(matches!(verdict, Verdict::Failed(_)), "{verdict:?}");

    wait_for(&s, "the liar replaced", |s| {
        let h = s.holding(hash);
        h.len() == 2 && !h.contains(&liar_id)
    })
    .await;
    let state = s.svc.state();
    let blob = state
        .blobs
        .iter()
        .find(|b| b.hash == hash)
        .expect("tracked");
    // Marked lost, or already released by the liar itself once it was lost.
    let entry = blob.holders.iter().find(|h| h.member == liar_id);
    assert!(
        entry.is_none_or(|h| h.state == HolderState::Lost),
        "{blob:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_over_quota_refuses_and_another_is_picked() {
    // The first member announces the most room but really keeps only 10 KiB.
    let s = swarm(
        110,
        &[(500, 10 * 1024), (64, 64 * MB), (32, 32 * MB)],
        quick(),
    )
    .await;
    let small = s.members[0].net.id();
    let hash = ask(&s, 1).await;
    wait_for(&s, "one holder", |s| s.holding(hash).len() == 1).await;
    let holder = s.holding(hash)[0];
    assert_ne!(holder, small);
    assert!(!s.members[0].member.held().contains(&hash));
    assert!(s.member(holder).member.held().contains(&hash));
}

// ---------------------------------------------------------------------------------------------
// `plan(now)` decisions, without any network.

fn id(n: u8) -> EndpointId {
    SecretKey::from_bytes(&[n; 32]).public()
}

fn cfg() -> DurableConfig {
    DurableConfig {
        absent_after_ms: 1_000,
        challenge_every_ms: 500,
        probe_every_ms: 200,
        retry_after_ms: 5_000,
    }
}

fn replicate_targets(orders: &[Order]) -> Vec<EndpointId> {
    orders
        .iter()
        .filter_map(|o| match o {
            Order::Replicate { member, .. } => Some(*member),
            _ => None,
        })
        .collect()
}

fn done_all(m: &mut DurabilityManager, now: u64, orders: &[Order], hash: Hash) {
    for member in replicate_targets(orders) {
        m.apply(
            now,
            Report::Replicate {
                member,
                hash,
                outcome: Replicated::Done,
            },
        );
    }
}

fn manager(members: u8) -> DurabilityManager {
    let mut m = DurabilityManager::new(cfg());
    for n in 1..=members {
        m.member_joined(id(n), MB, None);
    }
    m
}

const H: Hash = Hash([9; 32]);

#[test]
fn plan_places_once_per_member() {
    let mut m = manager(3);
    m.request(H, 1000, Vec::new(), [1; 16], 2).expect("request");
    let orders = m.plan(0);
    let targets = replicate_targets(&orders);
    assert_eq!(targets.len(), 2);
    assert_ne!(targets[0], targets[1]);
    // Nothing more while the orders are in flight.
    assert!(replicate_targets(&m.plan(10)).is_empty());
    done_all(&mut m, 20, &orders, H);
    assert_eq!(m.blob(&H).map(|b| b.holding().len()), Some(2));
    assert!(replicate_targets(&m.plan(30)).is_empty());
}

#[test]
fn plan_never_doubles_up_when_members_are_short() {
    let mut m = manager(2);
    m.request(H, 1000, Vec::new(), [1; 16], 5).expect("request");
    let orders = m.plan(0);
    assert_eq!(replicate_targets(&orders).len(), 2);
    done_all(&mut m, 1, &orders, H);
    assert!(replicate_targets(&m.plan(2)).is_empty());
}

#[test]
fn plan_respects_quotas_and_refusals() {
    let mut m = DurabilityManager::new(cfg());
    m.member_joined(id(1), 500, None);
    m.member_joined(id(2), MB, None);
    m.member_joined(id(3), MB / 2, None);
    m.request(H, 1000, Vec::new(), [1; 16], 1).expect("request");
    // The member with too little room is never asked; the roomiest goes first.
    assert_eq!(replicate_targets(&m.plan(0)), vec![id(2)]);
    m.apply(
        1,
        Report::Replicate {
            member: id(2),
            hash: H,
            outcome: Replicated::Refused(PinRefusal::Quota),
        },
    );
    assert_eq!(replicate_targets(&m.plan(2)), vec![id(3)]);
}

#[test]
fn plan_releases_the_surplus() {
    let mut m = manager(3);
    m.request(H, 1000, Vec::new(), [1; 16], 3).expect("request");
    let orders = m.plan(0);
    done_all(&mut m, 1, &orders, H);
    // The sharer releases, then asks for a single copy.
    m.forget(&H);
    m.request(H, 1000, Vec::new(), [1; 16], 1).expect("request");
    let releases: Vec<EndpointId> = m
        .plan(2)
        .into_iter()
        .filter_map(|o| match o {
            Order::Release { member, .. } => Some(member),
            _ => None,
        })
        .collect();
    assert_eq!(releases.len(), 2);
    for member in releases {
        m.apply(
            3,
            Report::Release {
                member,
                hash: H,
                ok: true,
            },
        );
    }
    assert_eq!(m.blob(&H).map(|b| b.holding().len()), Some(1));
    assert!(
        m.plan(4)
            .iter()
            .all(|o| !matches!(o, Order::Release { .. } | Order::Replicate { .. }))
    );
}

#[test]
fn plan_replaces_absent_and_failed_holders() {
    let mut m = manager(3);
    m.request(H, 1000, Vec::new(), [1; 16], 2).expect("request");
    let orders = m.plan(0);
    done_all(&mut m, 0, &orders, H);
    let first = replicate_targets(&orders);
    let spare = (1..=3).map(id).find(|i| !first.contains(i)).expect("spare");
    // The first holder never answers again; the second keeps answering.
    let mut now = 0;
    let mut replaced = Vec::new();
    while now < 3_000 && replaced.is_empty() {
        now += 100;
        for o in m.plan(now) {
            match o {
                Order::Probe { member } => {
                    let complete = (member != first[0]).then(|| vec![H]);
                    m.apply(now, Report::Probe { member, complete });
                }
                Order::Challenge { member, hash, .. } => {
                    let outcome = if member == first[0] {
                        Challenged::Unreachable
                    } else {
                        Challenged::Passed
                    };
                    m.apply(
                        now,
                        Report::Challenge {
                            member,
                            hash,
                            outcome,
                        },
                    );
                }
                Order::Replicate {
                    member, providers, ..
                } => {
                    // The remaining holder is offered as the source.
                    assert_eq!(providers.first().map(|p| p.id), Some(first[1]));
                    replaced.push(member);
                }
                Order::Release { .. } => {}
            }
        }
    }
    assert_eq!(replaced, vec![spare]);
    assert!(now > cfg().absent_after_ms);

    // A failed challenge loses the copy at once.
    let mut m = manager(3);
    m.request(H, 1000, Vec::new(), [1; 16], 1).expect("request");
    let orders = m.plan(0);
    done_all(&mut m, 0, &orders, H);
    let holder = replicate_targets(&orders)[0];
    assert!(
        m.plan(600)
            .iter()
            .any(|o| matches!(o, Order::Challenge { member, .. } if *member == holder))
    );
    m.apply(
        601,
        Report::Challenge {
            member: holder,
            hash: H,
            outcome: Challenged::Failed,
        },
    );
    let next = replicate_targets(&m.plan(602));
    assert_eq!(next.len(), 1);
    assert_ne!(next[0], holder);
}

#[test]
fn plan_restarts_from_saved_state() {
    let mut m = manager(3);
    m.request(H, 1000, Vec::new(), [1; 16], 2).expect("request");
    let orders = m.plan(0);
    done_all(&mut m, 0, &orders, H);
    // One more order in flight when the coordinator stops.
    m.request(Hash([8; 32]), 1000, Vec::new(), [2; 16], 1)
        .expect("request");
    assert_eq!(replicate_targets(&m.plan(1)).len(), 1);
    let json = serde_json::to_vec(m.state()).expect("serialize");
    let state: DurableState = serde_json::from_slice(&json).expect("parse");

    // Much later: the holders get a fresh absence delay instead of being lost at once,
    // nothing is placed twice, and the unfinished order is placed again.
    let mut r = DurabilityManager::restore(cfg(), state, 1_000_000);
    for n in 1..=3 {
        r.member_joined(id(n), MB, None);
    }
    let again = r.plan(1_000_001);
    assert_eq!(replicate_targets(&again).len(), 1);
    assert_eq!(r.blob(&H).map(|b| b.holding().len()), Some(2));
    assert!(
        r.plan(1_000_002)
            .iter()
            .all(|o| !matches!(o, Order::Replicate { .. }))
    );
}

// ---------------------------------------------------------------------------------------------
// Who may change what on `nuee/pin/0`, and the budgets of one requester.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_the_requester_lists_and_releases_its_blob() {
    let s = swarm(140, &[(64, 64 * MB); 2], quick()).await;
    let hash = ask(&s, 2).await;
    wait_for(&s, "two holders", |s| s.holding(hash).len() == 2).await;

    // Another holder of the swarm link lists, then releases the sharer's blob.
    let attacker = bind(170).await;
    attacker.add_peer(s.coord.id(), None, loopback(&s.coord));
    let seen = timeout(LIMIT, list_pins(&attacker, &s.job))
        .await
        .expect("in time")
        .expect("list");
    assert!(
        seen.is_empty(),
        "another sharer's blobs are listed: {seen:?}"
    );
    let released = timeout(LIMIT, release_pin(&attacker, &s.job, hash))
        .await
        .expect("in time")
        .expect("release");
    assert!(!released, "a non-owner released the blob");
    // Several planning rounds later, nothing was released.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let state = s.svc.state();
    let blob = state
        .blobs
        .iter()
        .find(|b| b.hash == hash)
        .expect("tracked");
    assert_eq!(blob.target, 2);
    assert_eq!(s.holding(hash).len(), 2);
    let stored = s
        .members
        .iter()
        .filter(|m| m.member.held().contains(&hash))
        .count();
    assert_eq!(stored, 2);

    // The sharer sees its own blob and may release it.
    let mine = timeout(LIMIT, list_pins(&s.sharer, &s.job))
        .await
        .expect("in time")
        .expect("list");
    assert_eq!(mine.iter().map(|p| p.hash).collect::<Vec<_>>(), vec![hash]);
    let released = timeout(LIMIT, release_pin(&s.sharer, &s.job, hash))
        .await
        .expect("in time")
        .expect("release");
    assert!(released);
    wait_for(&s, "the copies released", |s| {
        s.members.iter().all(|m| !m.member.held().contains(&hash))
    })
    .await;
    attacker.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pin_sessions_follow_approval_and_bans() {
    let s = swarm(180, &[(64, 64 * MB)], quick()).await;
    s.svc.require_approval(true);
    let pin = s.share.link.pin_link();
    // Not let in by the job driver: refused.
    let res = timeout(LIMIT, request_replicas(&s.sharer, &s.job, &pin, 1))
        .await
        .expect("in time");
    assert!(matches!(res, Err(StoreError::Refused)), "{res:?}");
    assert!(s.svc.state().blobs.is_empty());
    // Approved (the driver reports it joined): served.
    s.svc.member_joined(s.sharer.id(), &caps(0));
    let answer = timeout(LIMIT, request_replicas(&s.sharer, &s.job, &pin, 1))
        .await
        .expect("in time")
        .expect("request");
    assert_eq!(answer, ReplicaAnswer::Accepted(1));
    // Banned: refused again, and it cannot come back by rejoining.
    s.svc.peer_banned(s.sharer.id());
    s.svc.member_joined(s.sharer.id(), &caps(0));
    let res = timeout(LIMIT, list_pins(&s.sharer, &s.job))
        .await
        .expect("in time");
    assert!(matches!(res, Err(StoreError::Refused)), "{res:?}");
}

fn pin_link(hash: Hash, size: u64, providers: &[u8]) -> PinLink {
    let mut list = BoundedVec::new();
    for n in providers {
        list.push(Provider {
            id: id(*n),
            relay: None,
        })
        .expect("provider");
    }
    PinLink {
        providers: list,
        hash,
        size,
        access: [1; 16],
    }
}

#[test]
fn only_the_owner_releases() {
    let mut m = manager(3);
    m.request_by(id(100), &pin_link(H, 1000, &[100]), 2, 0)
        .expect("request");
    assert!(!m.forget_by(&id(101), &H));
    assert_eq!(m.blob(&H).map(|b| b.target), Some(2));
    assert_eq!(m.owned(&id(101)).count(), 0);
    assert_eq!(m.owned(&id(100)).count(), 1);
    // Someone else asking for the same blob changes nothing.
    m.request_by(id(101), &pin_link(H, 1000, &[101]), 8, 0)
        .expect("request");
    assert_eq!(m.blob(&H).map(|b| b.target), Some(2));
    assert!(m.forget_by(&id(100), &H));
    assert_eq!(m.blob(&H).map(|b| b.target), Some(0));
}

#[test]
fn providers_per_blob_are_capped() {
    let mut m = manager(1);
    for n in 0..50u8 {
        m.request_by(id(100), &pin_link(H, 1000, &[n, n + 100]), 1, 0)
            .expect("request");
    }
    let providers: Vec<EndpointId> = m
        .blob(&H)
        .expect("tracked")
        .providers
        .iter()
        .map(|p| p.id)
        .collect();
    assert_eq!(providers.len(), nuee_proto::limits::MAX_LINK_PROVIDERS);
    // The most recent ones are kept.
    assert!(providers.contains(&id(149)));
    assert!(providers.contains(&id(49)));
}

#[test]
fn one_requester_cannot_fill_the_tracker() {
    use nuee_store::durable::MAX_TRACKED_PER_OWNER;
    let mut m = manager(1);
    let hash = |i: usize| {
        let mut h = [0u8; 32];
        h[..8].copy_from_slice(&(i as u64).to_le_bytes());
        Hash(h)
    };
    for i in 0..MAX_TRACKED_PER_OWNER {
        m.request_by(id(100), &pin_link(hash(i), 10, &[]), 1, 0)
            .expect("within budget");
    }
    let over = m.request_by(id(100), &pin_link(hash(9_999), 10, &[]), 1, 0);
    assert_eq!(over, Err(PinRefusal::Quota));
    // Another requester still gets room.
    m.request_by(id(101), &pin_link(hash(10_000), 10, &[]), 1, 0)
        .expect("another requester");
    // Bytes count too.
    let huge = m.request_by(id(102), &pin_link(hash(10_001), 40 << 30, &[]), 2, 0);
    assert_eq!(huge, Err(PinRefusal::Quota));
}

#[test]
fn unplaced_and_expired_blobs_are_dropped() {
    use nuee_store::durable::UNPLACED_EXPIRY_MS;
    // No member ever takes the junk blob: it goes away after the expiry.
    let mut m = DurabilityManager::new(cfg());
    m.request_by(id(100), &pin_link(H, 1000, &[]), 1, 0)
        .expect("request");
    assert!(replicate_targets(&m.plan(0)).is_empty());
    m.plan(UNPLACED_EXPIRY_MS - 1);
    assert!(m.blob(&H).is_some());
    m.plan(UNPLACED_EXPIRY_MS);
    assert!(m.blob(&H).is_none());

    // A placed blob with a time to live is released once it is over.
    let mut m = DurabilityManager::new(DurableConfig {
        absent_after_ms: 1_000_000,
        challenge_every_ms: 1_000_000,
        ..cfg()
    });
    for n in 1..=2 {
        m.member_joined(id(n), MB, None);
    }
    m.request_by(id(100), &pin_link(H, 1000, &[]), 1, 10)
        .expect("request");
    let orders = m.plan(0);
    done_all(&mut m, 1, &orders, H);
    let releases = |orders: &[Order]| {
        orders
            .iter()
            .filter(|o| matches!(o, Order::Release { .. }))
            .count()
    };
    assert_eq!(releases(&m.plan(9_999)), 0);
    assert_eq!(m.blob(&H).map(|b| b.target), Some(1));
    assert_eq!(releases(&m.plan(10_000)), 1);
    assert_eq!(m.blob(&H).map(|b| b.target), Some(0));
}

#[test]
fn members_that_left_holding_nothing_free_their_slot() {
    use nuee_store::durable::MAX_MEMBERS;
    let mut m = DurabilityManager::new(cfg());
    let ident = |i: usize| {
        let mut k = [7u8; 32];
        k[..8].copy_from_slice(&(i as u64).to_le_bytes());
        SecretKey::from_bytes(&k).public()
    };
    for i in 0..MAX_MEMBERS {
        m.member_joined(ident(i), MB, None);
    }
    for i in 0..MAX_MEMBERS {
        m.member_left(&ident(i));
    }
    assert!(m.state().members.is_empty());
    m.member_joined(id(1), MB, None);
    assert_eq!(m.state().members.len(), 1);

    // Persisted members holding nothing are not restored either.
    let mut m = DurabilityManager::new(cfg());
    for i in 0..MAX_MEMBERS {
        m.member_joined(ident(i), MB, None);
    }
    let r = DurabilityManager::restore(cfg(), m.state().clone(), 10);
    assert!(r.state().members.is_empty());
}

#[test]
fn a_huge_announced_quota_does_not_win_every_placement() {
    let mut m = DurabilityManager::new(cfg());
    m.member_joined(id(1), u64::MAX, None);
    for n in 2..=4 {
        m.member_joined(id(n), 100 << 30, None);
    }
    let mut won = 0;
    for i in 0..64u8 {
        let hash = Hash([i; 32]);
        m.request(hash, 1000, Vec::new(), [1; 16], 1)
            .expect("request");
        let targets = replicate_targets(&m.plan(u64::from(i)));
        assert_eq!(targets.len(), 1);
        if targets[0] == id(1) {
            won += 1;
        }
    }
    assert!(won < 40, "the huge quota won {won} of 64 placements");
}

fn relays_of(net: &Net, peer: EndpointId) -> Vec<String> {
    net.peer_addr(peer)
        .map(|a| a.relay_urls().map(|u| u.to_string()).collect())
        .unwrap_or_default()
}

/// A member announcing a relay on a private address: the coordinator does not record it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_relay_on_a_private_address_is_ignored() {
    let s = swarm(200, &[], quick()).await;
    let private = bind(230).await;
    let mut announced = caps(10);
    announced.relay = Some(BoundedString::try_from("http://10.0.0.1:3340/").expect("relay"));
    s.svc.member_joined(private.id(), &announced);
    assert!(relays_of(&s.coord, private.id()).is_empty());
    // A public https relay is still learnt.
    let public = bind(231).await;
    let mut announced = caps(10);
    announced.relay = Some(BoundedString::try_from("https://relay.example.com/").expect("relay"));
    s.svc.member_joined(public.id(), &announced);
    assert_eq!(
        relays_of(&s.coord, public.id()),
        vec!["https://relay.example.com/".to_owned()]
    );
}

/// A `Replicate` order naming a provider behind a private relay: the member never registers
/// (hence never dials) that relay.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replicate_provider_on_a_private_relay_is_not_dialled() {
    use nuee_net::{recv_msg, send_msg};
    use nuee_proto::{PROOF_PIN, PROTO_VERSION, PinMsg, limits::MAX_MSG};

    let coord = bind(240).await;
    let job = JobLink {
        coordinator: coord.id(),
        relay: BoundedString::new(),
        secret: [240; 16],
    };
    let m = member_node(241, &job, 100 * MB, None).await;
    coord.add_peer(m.net.id(), None, loopback(&m.net));
    let provider = bind(242).await;

    let conn = timeout(LIMIT, coord.connect(m.net.id(), ALPN_PIN))
        .await
        .expect("in time")
        .expect("connect");
    let (mut send, mut recv) = conn.open_bi().await.expect("stream");
    let hello = PinMsg::Hello {
        version: PROTO_VERSION,
        proof: nuee_proto::proof(PROOF_PIN, &job.secret, &coord.id(), &m.net.id()),
    };
    send_msg(&mut send, &hello).await.expect("hello");
    let mut providers = BoundedVec::new();
    providers
        .push(Provider {
            id: provider.id(),
            relay: Some(BoundedString::try_from("http://10.0.0.1:3340/").expect("relay")),
        })
        .expect("provider");
    let order = PinMsg::Replicate {
        hash: Hash::of(b"elsewhere"),
        size: 1000,
        providers,
        access: [7; 16],
        ttl_s: 0,
    };
    send_msg(&mut send, &order).await.expect("order");
    let answer = timeout(LIMIT, recv_msg::<PinMsg>(&mut recv, MAX_MSG))
        .await
        .expect("in time")
        .expect("answer");
    assert!(matches!(answer, PinMsg::Accepted { .. }), "{answer:?}");
    // The copy is attempted (and fails: the provider is unreachable); give it time to dial.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(relays_of(&m.net, provider.id()).is_empty());
}

/// Sends `msg` to the member `m` as the coordinator of `job` and returns its final answer.
async fn as_coordinator(
    coord: &Net,
    job: &JobLink,
    m: &Net,
    msg: nuee_proto::PinMsg,
) -> nuee_proto::PinMsg {
    use nuee_net::{recv_msg, send_msg};
    use nuee_proto::{PROOF_PIN, PROTO_VERSION, PinMsg, limits::MAX_MSG};

    let conn = timeout(LIMIT, coord.connect(m.id(), ALPN_PIN))
        .await
        .expect("in time")
        .expect("connect");
    let (mut send, mut recv) = conn.open_bi().await.expect("stream");
    let hello = PinMsg::Hello {
        version: PROTO_VERSION,
        proof: nuee_proto::proof(PROOF_PIN, &job.secret, &coord.id(), &m.id()),
    };
    send_msg(&mut send, &hello).await.expect("hello");
    send_msg(&mut send, &msg).await.expect("order");
    // An accepted copy is confirmed by `Done` on the same stream once stored.
    loop {
        let answer = timeout(LIMIT, recv_msg::<PinMsg>(&mut recv, MAX_MSG))
            .await
            .expect("in time")
            .expect("answer");
        if !matches!(answer, PinMsg::Accepted { .. }) {
            return answer;
        }
    }
}

/// A copy stored then released for the swarm leaves the node's own pins and shares of the
/// same blob alone, while a copy held only for the swarm is reclaimed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_released_copy_keeps_the_nodes_own_pins_and_shares() {
    use nuee_proto::PinMsg;
    use nuee_store::{blob_tag, pin_tag};

    let coord = bind(250).await;
    let job = JobLink {
        coordinator: coord.id(),
        relay: BoundedString::new(),
        secret: [250; 16],
    };
    let net = bind(251).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let hub = StoreHub::open(
        &net,
        dir.path(),
        HubConfig {
            gc_interval: Duration::from_millis(200),
            ..HubConfig::default()
        },
    )
    .await
    .expect("open store");
    let reg = Registry::load(&StoreMember::default_registry(dir.path())).expect("registry");
    let member = StoreMember::new(&hub, &job, 100 * MB, reg).expect("member");
    let _router = member.spawn();

    // Two sharers offer a blob each. The node already holds `own` and pinned it itself;
    // `swarm_only` reaches it only as a copy for the swarm.
    let mut sharers = Vec::new();
    for (seed, name, len) in [(252u8, "own.bin", 50_000usize), (253, "other.bin", 40_000)] {
        let sharer = bind(seed).await;
        let dir = tempfile::tempdir().expect("tempdir");
        let sharer_hub = StoreHub::open(&sharer, dir.path(), HubConfig::default())
            .await
            .expect("sharer store");
        let share = share_bytes(
            &sharer,
            &sharer_hub,
            &[(name.to_owned(), noise(len, seed.into()))],
        )
        .await
        .expect("share");
        sharers.push((sharer, dir, sharer_hub, share));
    }
    let mut nets = vec![&coord, &net];
    nets.extend(sharers.iter().map(|s| &s.0));
    for a in &nets {
        for b in &nets {
            if a.id() != b.id() {
                a.add_peer(b.id(), None, loopback(b));
            }
        }
    }
    let (own_share, other_share) = (&sharers[0].3, &sharers[1].3);
    let sharer_hub = &sharers[0].2;
    let own = own_share.link.hash;
    let swarm_only = other_share.link.hash;
    let own_bytes = sharer_hub.read(own).await.expect("read");
    assert_eq!(hub.add_bytes(own_bytes.clone()).await.expect("share"), own);
    hub.tag(&pin_tag(own), own).await.expect("pin");

    for link in [&own_share.link, &other_share.link] {
        let answer = as_coordinator(
            &coord,
            &job,
            &net,
            PinMsg::Replicate {
                hash: link.hash,
                size: link.size,
                providers: link.providers.clone(),
                access: link.access,
                ttl_s: 0,
            },
        )
        .await;
        assert!(matches!(answer, PinMsg::Done { .. }), "{answer:?}");
    }
    assert!(member.held().contains(&own) && member.held().contains(&swarm_only));
    own_share.serve.shutdown().await.expect("stop sharing");
    other_share.serve.shutdown().await.expect("stop sharing");

    for hash in [own, swarm_only] {
        let answer = as_coordinator(&coord, &job, &net, PinMsg::Release { hash }).await;
        assert!(matches!(answer, PinMsg::Pins { .. }), "{answer:?}");
    }
    assert!(member.held().is_empty());

    // Several collections later the node's own blob is intact, the swarm-only copy is gone.
    timeout(LIMIT, async {
        while hub.present(swarm_only).await.expect("status") {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("the released copy reclaimed");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(hub.has_tag(&pin_tag(own)).await.expect("tags"));
    assert!(hub.has_tag(&blob_tag(own)).await.expect("tags"));
    assert_eq!(hub.read(own).await.expect("read"), own_bytes);
    // The node's own blob will never be collected: it must not eat into the quota.
    assert_eq!(member.reclaiming().await.expect("reclaiming"), 0);
}

/// The node shares a blob while a swarm copy of it is still arriving: the share's tag
/// outlives the copy being stored and garbage collection keeps the blob.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_share_made_during_a_swarm_fetch_keeps_its_tag() {
    use nuee_proto::PinMsg;
    use nuee_store::blob_tag;

    let coord = bind(240).await;
    let job = JobLink {
        coordinator: coord.id(),
        relay: BoundedString::new(),
        secret: [240; 16],
    };
    let net = bind(241).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let hub = StoreHub::open(
        &net,
        dir.path(),
        HubConfig {
            gc_interval: Duration::from_millis(200),
            ..HubConfig::default()
        },
    )
    .await
    .expect("open store");
    let reg = Registry::load(&StoreMember::default_registry(dir.path())).expect("registry");
    let member = StoreMember::new(&hub, &job, 200 * MB, reg).expect("member");
    let _router = member.spawn();

    // Large enough that the transfer is still running when the node shares the blob.
    let sharer = bind(242).await;
    let sharer_dir = tempfile::tempdir().expect("tempdir");
    let sharer_hub = StoreHub::open(&sharer, sharer_dir.path(), HubConfig::default())
        .await
        .expect("sharer store");
    let share = share_bytes(
        &sharer,
        &sharer_hub,
        &[("big.bin".to_owned(), noise(48 * MB as usize, 242))],
    )
    .await
    .expect("share");
    let nets = [&coord, &net, &sharer];
    for a in &nets {
        for b in &nets {
            if a.id() != b.id() {
                a.add_peer(b.id(), None, loopback(b));
            }
        }
    }
    let link = share.link.clone();
    let (hash, size) = (link.hash, link.size);

    let replicate = {
        let (coord, job, net) = (coord.clone(), job.clone(), net.clone());
        tokio::spawn(async move {
            as_coordinator(
                &coord,
                &job,
                &net,
                PinMsg::Replicate {
                    hash: link.hash,
                    size: link.size,
                    providers: link.providers.clone(),
                    access: link.access,
                    ttl_s: 0,
                },
            )
            .await
        })
    };
    // The first bytes have landed: the node shares the same blob mid-fetch.
    timeout(LIMIT, async {
        while !hub.present(hash).await.expect("status") {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the fetch started");
    hub.tag(&blob_tag(hash), hash).await.expect("share");
    assert!(
        !replicate.is_finished(),
        "the copy was stored before the share; the race was not exercised"
    );

    let answer = replicate.await.expect("join");
    assert!(matches!(answer, PinMsg::Done { .. }), "{answer:?}");
    share.serve.shutdown().await.expect("stop sharing");
    assert!(hub.has_tag(&blob_tag(hash)).await.expect("tags"));

    // Released for the swarm, the blob stays because the node shares it.
    let answer = as_coordinator(&coord, &job, &net, PinMsg::Release { hash }).await;
    assert!(matches!(answer, PinMsg::Pins { .. }), "{answer:?}");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(hub.has_tag(&blob_tag(hash)).await.expect("tags"));
    assert_eq!(hub.complete_size(hash).await.expect("status"), Some(size));
}
