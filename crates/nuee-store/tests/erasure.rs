//! Reed-Solomon erasure sets: encoding, reconstruction, header bounds and shard repair
//! through the durability manager.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use iroh::protocol::Router;
use nuee_net::{Net, NetConfig, RelayChoice};
use nuee_proto::{
    ALPN_PIN, BoundedString, BoundedVec, Caps, EndpointId, Hash, JobLink, Provider, SecretKey,
};
use nuee_store::durable::{
    Challenged, DurabilityManager, DurabilityService, DurableConfig, Keep, Order, Placement,
    Replicated, Report, ServiceConfig, StoreMember, get_durable, keep_durably,
};
use nuee_store::erasure::{
    self, ErasureError, Header, MAX_SHARD_SIZE, decode_header, encode, rebuild_shard, reconstruct,
};
use nuee_store::{FilesAcceptor, Gate, HubConfig, Registry, ServeHandle, StoreHub, share_bytes};
use rand::rngs::StdRng;
use rand::{RngCore, SeedableRng};
use tokio::time::timeout;

fn random(len: usize, seed: u64) -> Vec<u8> {
    let mut out = vec![0u8; len];
    StdRng::seed_from_u64(seed).fill_bytes(&mut out);
    out
}

fn all(set: &erasure::ErasureSet) -> Vec<(u16, Vec<u8>)> {
    (0u16..).zip(set.shards.iter().cloned()).collect()
}

fn without(set: &erasure::ErasureSet, missing: &[u16]) -> Vec<(u16, Vec<u8>)> {
    all(set)
        .into_iter()
        .filter(|(i, _)| !missing.contains(i))
        .collect()
}

#[test]
fn roundtrip_sizes() {
    for (n, len) in [0usize, 1, 7, 1001, 4, 8, 4096, 4 * 1000]
        .into_iter()
        .enumerate()
    {
        let data = random(len, n as u64);
        let set = encode(&data, 4, 2).expect("encode");
        assert_eq!(set.shards.len(), 6);
        let size = set.header.shard_size as usize;
        assert!(
            size >= 2 && size.is_multiple_of(2) && size * 4 >= len,
            "len {len}: size {size}"
        );
        assert!(set.shards.iter().all(|s| s.len() == size));
        assert_eq!(
            reconstruct(&set.header, all(&set)).expect("all shards"),
            data
        );
    }
}

#[test]
fn shard_size_rule() {
    assert_eq!(erasure::shard_size(0, 4), Ok(2));
    assert_eq!(erasure::shard_size(1, 4), Ok(2));
    assert_eq!(erasure::shard_size(8, 4), Ok(2));
    assert_eq!(erasure::shard_size(9, 4), Ok(4));
    assert_eq!(erasure::shard_size(12, 4), Ok(4));
    assert_eq!(erasure::shard_size(13, 4), Ok(4));
    assert_eq!(erasure::shard_size(17, 4), Ok(6));
    assert!(matches!(
        erasure::shard_size(u64::from(MAX_SHARD_SIZE) * 4 + 1, 4),
        Err(ErasureError::TooLarge { .. })
    ));
}

#[test]
fn ten_mib_any_two_missing() {
    let data = random(10 * 1024 * 1024 + 3, 42);
    let set = encode(&data, 4, 2).expect("encode");
    for a in 0u16..6 {
        for b in a + 1..6 {
            let got = reconstruct(&set.header, without(&set, &[a, b])).expect("recover");
            assert!(got == data, "missing {a} and {b}");
        }
    }
}

#[test]
fn small_any_two_missing_and_three_fail() {
    let data = random(1001, 7);
    let set = encode(&data, 4, 2).expect("encode");
    for a in 0u16..6 {
        for b in a + 1..6 {
            assert_eq!(
                reconstruct(&set.header, without(&set, &[a, b])).expect("recover"),
                data
            );
            for c in b + 1..6 {
                assert_eq!(
                    reconstruct(&set.header, without(&set, &[a, b, c])),
                    Err(ErasureError::NotEnough { have: 3, need: 4 })
                );
            }
        }
    }
}

#[test]
fn corrupted_shard_is_ignored() {
    let data = random(5000, 3);
    let set = encode(&data, 4, 2).expect("encode");
    let mut shards = all(&set);
    shards[1].1[10] ^= 1;
    // Five valid shards remain: still recovered.
    assert_eq!(
        reconstruct(&set.header, shards.clone()).expect("recover"),
        data
    );
    // Corrupt shard plus one missing: still four valid.
    let four: Vec<_> = shards.iter().filter(|(i, _)| *i != 5).cloned().collect();
    assert_eq!(reconstruct(&set.header, four).expect("recover"), data);
    // Corrupt shard plus two missing: three valid, refused rather than trusted.
    let three: Vec<_> = shards
        .into_iter()
        .filter(|(i, _)| *i != 4 && *i != 5)
        .collect();
    assert_eq!(
        reconstruct(&set.header, three),
        Err(ErasureError::NotEnough { have: 3, need: 4 })
    );
    // A shard with the wrong length or a wrong index is ignored too.
    let mut odd = without(&set, &[0, 1, 2]);
    odd.push((0, vec![0; 3]));
    odd.push((9, set.shards[0].clone()));
    odd.push((1, set.shards[2].clone()));
    assert_eq!(
        reconstruct(&set.header, odd),
        Err(ErasureError::NotEnough { have: 3, need: 4 })
    );
}

#[test]
fn header_roundtrip() {
    let set = encode(&random(12345, 9), 4, 2).expect("encode");
    let bytes = set.header.encode();
    assert_eq!(&bytes[..4], b"NRS1");
    assert_eq!(bytes.len(), 4 + 1 + 2 + 2 + 8 + 4 + 32 * 7);
    assert_eq!(decode_header(&bytes).expect("decode"), set.header);
    assert_eq!(set.header.id(), Hash::of(&bytes));
    assert_eq!(erasure::parse_scheme("4+2"), Ok((4, 2)));
    assert!(erasure::parse_scheme("0+2").is_err());
    assert!(erasure::parse_scheme("60+5").is_err());
    assert!(erasure::parse_scheme("4-2").is_err());
}

fn raw(k: u16, m: u16, len: u64, shard_size: u32, hashes: usize) -> Vec<u8> {
    let mut out = b"NRS1".to_vec();
    out.push(1);
    out.extend_from_slice(&k.to_le_bytes());
    out.extend_from_slice(&m.to_le_bytes());
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&shard_size.to_le_bytes());
    out.extend(std::iter::repeat_n(7u8, 32 * hashes));
    out
}

#[test]
fn malformed_headers_rejected() {
    let good = raw(4, 2, 1000, 250, 7);
    assert!(decode_header(&good).is_ok());
    // k = 0, m = 0, k + m > 64.
    assert!(decode_header(&raw(0, 2, 0, 2, 3)).is_err());
    assert!(decode_header(&raw(4, 0, 1000, 250, 5)).is_err());
    assert!(decode_header(&raw(60, 5, 60, 2, 66)).is_err());
    // Shard size above the cap, or not the one the length gives.
    assert!(decode_header(&raw(1, 1, u64::MAX, MAX_SHARD_SIZE + 2, 3)).is_err());
    assert!(decode_header(&raw(4, 2, 1000, 252, 7)).is_err());
    assert!(decode_header(&raw(4, 2, 1000, 251, 7)).is_err());
    // Truncated or extended hash list.
    assert!(decode_header(&raw(4, 2, 1000, 250, 6)).is_err());
    assert!(decode_header(&good[..good.len() - 1]).is_err());
    let mut long = good.clone();
    long.push(0);
    assert!(decode_header(&long).is_err());
    // Bad magic, bad version, oversized input, every prefix.
    let mut magic = good.clone();
    magic[0] = b'X';
    assert!(decode_header(&magic).is_err());
    let mut version = good.clone();
    version[4] = 2;
    assert!(decode_header(&version).is_err());
    assert!(decode_header(&vec![0u8; erasure::MAX_HEADER + 1]).is_err());
    for n in 0..good.len() {
        assert!(decode_header(&good[..n]).is_err());
    }
}

fn id(n: u8) -> EndpointId {
    SecretKey::from_bytes(&[n; 32]).public()
}

fn cfg() -> DurableConfig {
    DurableConfig {
        absent_after_ms: 1_000_000,
        challenge_every_ms: 500,
        probe_every_ms: 1_000_000,
        retry_after_ms: 5_000,
    }
}

fn replicates(orders: &[Order]) -> Vec<(EndpointId, Hash, Vec<Provider>)> {
    orders
        .iter()
        .filter_map(|o| match o {
            Order::Replicate {
                member,
                hash,
                providers,
                ..
            } => Some((*member, *hash, providers.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn repair_rebuilds_a_lost_shard() {
    let data = random(70_001, 11);
    let set = encode(&data, 4, 2).expect("encode");
    let header: Header = set.header.clone();
    let sharer = Provider {
        id: id(100),
        relay: None,
    };
    let mut m = DurabilityManager::new(cfg());
    for n in 1..=7 {
        m.member_joined(id(n), 1 << 20, None);
    }
    let set_id = m
        .request_erasure(header.clone(), vec![sharer.clone()], [5; 16])
        .expect("request");
    assert_eq!(set_id, header.id());

    // Six shards on six different members.
    let orders = m.plan(0);
    let all_orders = replicates(&orders);
    // The header blob is kept too, as two plain copies.
    assert_eq!(
        all_orders.iter().filter(|(_, h, _)| *h == set_id).count(),
        2
    );
    let placed: Vec<_> = all_orders
        .into_iter()
        .filter(|(_, h, _)| *h != set_id)
        .collect();
    assert_eq!(placed.len(), 6);
    let mut members: Vec<EndpointId> = placed.iter().map(|(m, _, _)| *m).collect();
    members.sort();
    members.dedup();
    assert_eq!(members.len(), 6);
    for (member, hash, _) in &placed {
        m.apply(
            0,
            Report::Replicate {
                member: *member,
                hash: *hash,
                outcome: Replicated::Done,
            },
        );
    }
    assert!(m.plan_repairs(1).is_empty());

    // Shard 2's holder fails its challenge.
    let lost = header.shards[2];
    let lost_member = placed
        .iter()
        .find(|(_, h, _)| *h == lost)
        .map(|(m, _, _)| *m)
        .expect("holder");
    let orders = m.plan(600);
    for o in orders {
        if let Order::Challenge { member, hash, .. } = o {
            let outcome = if hash == lost {
                Challenged::Failed
            } else {
                Challenged::Passed
            };
            m.apply(
                600,
                Report::Challenge {
                    member,
                    hash,
                    outcome,
                },
            );
        }
    }
    // No copy is fetched again from the sharer: the shard is rebuilt instead.
    let orders = m.plan(700);
    assert!(replicates(&orders).iter().all(|(_, h, _)| *h != lost));
    let repairs = m.plan_repairs(700);
    assert_eq!(repairs.len(), 1);
    let job = &repairs[0];
    assert_eq!((job.index, job.hash, job.set), (2, lost, set_id));
    assert!(job.sources.len() >= 4);
    assert!(
        job.sources
            .iter()
            .all(|(_, h, member)| *h != lost && *member != lost_member)
    );
    assert!(m.plan_repairs(701).is_empty(), "one rebuild at a time");

    // The survivors rebuild it, bit for bit.
    let survivors: Vec<(u16, Vec<u8>)> = job
        .sources
        .iter()
        .map(|(i, _, _)| (*i, set.shards[usize::from(*i)].clone()))
        .collect();
    let rebuilt = rebuild_shard(&job.header, survivors, job.index).expect("rebuild");
    assert_eq!(Hash::of(&rebuilt), lost);
    assert_eq!(rebuilt, set.shards[2]);
    // Parity shards rebuild too.
    let parity = rebuild_shard(&header, without(&set, &[0, 5]), 5).expect("parity");
    assert_eq!(Hash::of(&parity), header.shards[5]);

    // Once rebuilt, the shard is placed from the rebuilder on a member with no other shard.
    let rebuilder = Provider {
        id: id(200),
        relay: None,
    };
    m.rebuilt(800, &lost, Some(rebuilder.clone()));
    let again = replicates(&m.plan(800));
    let (member, hash, providers) = again
        .into_iter()
        .find(|(_, h, _)| *h == lost)
        .expect("replaced");
    assert_eq!(hash, lost);
    assert_eq!(providers.first().map(|p| p.id), Some(rebuilder.id));
    let spare = (1..=7)
        .map(id)
        .find(|i| !members.contains(i))
        .expect("spare");
    assert_eq!(member, spare);
    assert!(!members.contains(&member));
}

#[test]
fn failed_rebuild_is_retried_later() {
    let set = encode(&random(400, 1), 4, 2).expect("encode");
    let mut m = DurabilityManager::new(cfg());
    for n in 1..=6 {
        m.member_joined(id(n), 1 << 20, None);
    }
    m.request_erasure(set.header.clone(), Vec::new(), [1; 16])
        .expect("request");
    for (member, hash, _) in replicates(&m.plan(0)) {
        m.apply(
            0,
            Report::Replicate {
                member,
                hash,
                outcome: Replicated::Done,
            },
        );
    }
    let lost = set.header.shards[0];
    for o in m.plan(600) {
        if let Order::Challenge { member, hash, .. } = o {
            let outcome = if hash == lost {
                Challenged::Failed
            } else {
                Challenged::Passed
            };
            m.apply(
                600,
                Report::Challenge {
                    member,
                    hash,
                    outcome,
                },
            );
        }
    }
    assert_eq!(m.plan_repairs(700).len(), 1);
    m.rebuilt(700, &lost, None);
    assert!(m.plan_repairs(701).is_empty());
    assert_eq!(m.plan_repairs(5_700).len(), 1);
}

// ---------------------------------------------------------------------------------------------
// In process, on loopback: a coordinator, seven store members and a sharer.

const LIMIT: Duration = Duration::from_secs(120);
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

fn introduce(nets: &[&Net]) {
    for a in nets {
        for b in nets {
            if a.id() != b.id() {
                a.add_peer(b.id(), None, loopback(b));
            }
        }
    }
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
    net: Net,
    router: Router,
    _dir: tempfile::TempDir,
}

fn holders_of(svc: &DurabilityService, hash: Hash) -> Vec<EndpointId> {
    svc.state()
        .blobs
        .iter()
        .find(|b| b.hash == hash)
        .map(|b| b.holding())
        .unwrap_or_default()
}

async fn wait_for<F: Fn() -> bool>(svc: &DurabilityService, what: &str, cond: F) {
    let deadline = tokio::time::Instant::now() + LIMIT;
    while !cond() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}: {:?}",
            svc.state()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn providers(ids: &[EndpointId]) -> Vec<Provider> {
    ids.iter()
        .map(|id| Provider {
            id: *id,
            relay: None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lost_shard_is_rebuilt_and_the_set_still_downloads() {
    // Coordinator, with a second endpoint that rebuilds and serves lost shards.
    let coord = bind(120).await;
    let job = JobLink {
        coordinator: coord.id(),
        relay: BoundedString::new(),
        secret: [120; 16],
    };
    let svc = DurabilityService::new(
        &coord,
        job.secret,
        ServiceConfig {
            durable: DurableConfig {
                absent_after_ms: 2_000,
                challenge_every_ms: 60 * 60 * 1000,
                probe_every_ms: 300,
                retry_after_ms: 2_000,
            },
            tick: Duration::from_millis(200),
            ..ServiceConfig::default()
        },
        None,
    )
    .expect("service");
    let rebuild_net = bind(119).await;
    let rebuild_hub = StoreHub::memory(&rebuild_net, HubConfig::default());
    let rebuild_acceptor = FilesAcceptor::new(&rebuild_hub, "rebuild", Vec::new());
    svc.set_rebuilder(rebuild_hub.clone(), rebuild_acceptor.clone());
    let _rebuild_serve = ServeHandle::spawn(&rebuild_net, &rebuild_hub, rebuild_acceptor);
    let _task = svc.start().expect("start");
    let _router = coord.router().accept(ALPN_PIN, svc.clone()).spawn();

    let mut members = Vec::new();
    for seed in 121..=127u8 {
        let net = bind(seed).await;
        let dir = tempfile::tempdir().expect("tempdir");
        let hub = StoreHub::open(&net, dir.path(), HubConfig::default())
            .await
            .expect("store");
        let reg = Registry::load(&StoreMember::default_registry(dir.path())).expect("registry");
        let member = StoreMember::new(&hub, &job, 64 * MB, reg).expect("member");
        let router = member.spawn();
        members.push(MemberNode {
            net,
            router,
            _dir: dir,
        });
    }

    // The sharer encrypts a share, splits it 4+2 and asks the swarm to keep the set.
    let sharer = bind(140).await;
    let sharer_dir = tempfile::tempdir().expect("tempdir");
    let sharer_hub = StoreHub::open(&sharer, sharer_dir.path(), HubConfig::default())
        .await
        .expect("sharer store");
    let files = vec![("data.bin".to_owned(), random(200_001, 77))];
    let share = share_bytes(&sharer, &sharer_hub, &files)
        .await
        .expect("share");
    let cipher = sharer_hub
        .read(share.link.hash)
        .await
        .expect("cipher")
        .to_vec();
    let access = [33; 16];
    let offer = erasure::offer_set(
        &sharer_hub,
        &share.serve.acceptor,
        share.link.hash,
        access,
        4,
        2,
    )
    .await
    .expect("offer");
    assert_eq!(offer.header.original, share.link.hash);

    let mut nets: Vec<&Net> = vec![&coord, &rebuild_net, &sharer];
    nets.extend(members.iter().map(|m| &m.net));
    introduce(&nets);
    for m in &members {
        svc.member_joined(m.net.id(), &caps(64));
    }
    // What `nuee share --erasure 4+2 --on <link>` does: ask, wait, print the durable link.
    let mut set_link = share.link.clone();
    set_link.hash = offer.id;
    set_link.size = offer.bytes.len() as u64;
    set_link.access = access;
    let (placement, durable) = timeout(
        LIMIT,
        keep_durably(
            &sharer,
            &job,
            &set_link,
            Keep::Set(offer.bytes.clone()),
            LIMIT,
            Some(nuee_store::self_provider(&sharer)),
            |_| {},
        ),
    )
    .await
    .expect("in time")
    .expect("keep");
    assert!(matches!(placement, Placement::Placed(_)), "{placement:?}");
    assert_eq!(
        durable.providers.len(),
        4,
        "four member holders, no room for the sharer"
    );
    assert!(durable.providers.iter().all(|p| p.id != sharer.id()));

    let shards = offer.header.shards.clone();
    wait_for(&svc, "every shard and the header placed", || {
        shards.iter().all(|h| holders_of(&svc, *h).len() == 1)
            && holders_of(&svc, offer.id).len() == 2
    })
    .await;
    let mut shard_holders: Vec<EndpointId> =
        shards.iter().map(|h| holders_of(&svc, *h)[0]).collect();
    let mut distinct = shard_holders.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), 6, "one shard per member");

    // The sharer leaves, then the holder of shard 2 dies.
    share.shutdown().await.expect("stop sharing");
    sharer.close().await;
    let dead = shard_holders[2];
    let dead_node = members.iter().find(|m| m.net.id() == dead).expect("dead");
    dead_node.router.shutdown().await.expect("shutdown");
    dead_node.net.close().await;

    let lost = shards[2];
    wait_for(&svc, "shard 2 rebuilt on a spare member", || {
        holders_of(&svc, lost).first().is_some_and(|h| *h != dead)
    })
    .await;
    let replacement = holders_of(&svc, lost)[0];
    assert!(
        !shard_holders.contains(&replacement),
        "a member with no other shard"
    );
    shard_holders[2] = replacement;

    // A fresh downloader rebuilds the ciphertext from any four shards.
    let down = bind(150).await;
    for m in members.iter().filter(|m| m.net.id() != dead) {
        down.add_peer(m.net.id(), None, loopback(&m.net));
    }
    let hub = StoreHub::memory(&down, HubConfig::default());
    for skip in [[0usize, 1], [2, 5]] {
        let four: Vec<EndpointId> = shard_holders
            .iter()
            .enumerate()
            .filter(|(i, _)| !skip.contains(i))
            .map(|(_, id)| *id)
            .collect();
        let got = timeout(
            LIMIT,
            erasure::fetch_set(&down, &hub, &providers(&four), &access, &offer.header),
        )
        .await
        .expect("in time")
        .expect("fetch set");
        assert!(got == cipher, "rebuilt ciphertext without shards {skip:?}");
    }

    // `get --on` with the link printed by the sharer: header and shards come from the
    // members, and the coordinator names fresh holders if the printed ones fail.
    down.add_peer(coord.id(), None, loopback(&coord));
    let dest = tempfile::tempdir().expect("dest");
    let report = timeout(
        LIMIT,
        get_durable(
            &down,
            &hub,
            &durable,
            Some(&job),
            dest.path(),
            false,
            |_, _| {},
        ),
    )
    .await
    .expect("in time")
    .expect("get set link");
    let (report, _asked) = report;
    assert!(report.verified);
    assert_eq!(
        std::fs::read(dest.path().join("data.bin")).expect("file"),
        files[0].1
    );
    down.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn get_bytes_rebuilds_an_erasure_set_link() {
    // What a browser does with an `F` link that points to a set header: fetch the header, then
    // enough shards, rebuild and check the ciphertext, decrypt.
    let sharer = bind(160).await;
    let sharer_hub = StoreHub::memory(&sharer, HubConfig::default());
    let files = vec![("set.bin".to_owned(), random(300_007, 91))];
    let share = share_bytes(&sharer, &sharer_hub, &files)
        .await
        .expect("share");
    let access = [44; 16];
    let offer = erasure::offer_set(
        &sharer_hub,
        &share.serve.acceptor,
        share.link.hash,
        access,
        4,
        2,
    )
    .await
    .expect("offer");
    let mut set_link = share.link.clone();
    set_link.hash = offer.id;
    set_link.size = offer.bytes.len() as u64;
    set_link.access = access;

    let down = bind(161).await;
    introduce(&[&sharer, &down]);
    let hub = StoreHub::memory(&down, HubConfig::default());
    let mut seen = Vec::new();
    let got = timeout(
        LIMIT,
        nuee_store::get_bytes(&down, &hub, &set_link, |got, total| seen.push((got, total))),
    )
    .await
    .expect("in time")
    .expect("get set link in memory");
    assert_eq!(got, files);
    let len = offer.header.len;
    assert_eq!(
        seen.last(),
        Some(&(len, len)),
        "progress ends at the ciphertext length"
    );
    share.shutdown().await.expect("stop sharing");
}

#[test]
fn exactly_k_plus_m_members_place_a_set() {
    let set = encode(&random(70_001, 12), 4, 2).expect("encode");
    let header: Header = set.header.clone();
    let sharer = Provider {
        id: id(100),
        relay: None,
    };
    let mut m = DurabilityManager::new(cfg());
    for n in 1..=6 {
        m.member_joined(id(n), 1 << 20, None);
    }
    let set_id = m
        .request_erasure(header.clone(), vec![sharer], [5; 16])
        .expect("request");
    let mut now = 0;
    while !m.placed(&set_id) && now < 100_000 {
        for (member, hash, _) in replicates(&m.plan(now)) {
            m.apply(
                now,
                Report::Replicate {
                    member,
                    hash,
                    outcome: Replicated::Done,
                },
            );
        }
        now += 1_000;
    }
    assert!(m.placed(&set_id), "six members must hold a 4+2 set");
    let mut holders: Vec<EndpointId> = header
        .shards
        .iter()
        .flat_map(|h| m.blob(h).expect("tracked").holding())
        .collect();
    assert_eq!(holders.len(), 6);
    holders.sort();
    holders.dedup();
    assert_eq!(holders.len(), 6, "one shard per member");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn six_store_members_place_a_four_plus_two_set() {
    // Exactly k + m members, default retry delays: no spare member is needed.
    let coord = bind(150).await;
    let job = JobLink {
        coordinator: coord.id(),
        relay: BoundedString::new(),
        secret: [150; 16],
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
    let _task = svc.start().expect("start");
    let _router = coord.router().accept(ALPN_PIN, svc.clone()).spawn();

    let mut members = Vec::new();
    for seed in 151..=156u8 {
        let net = bind(seed).await;
        let dir = tempfile::tempdir().expect("tempdir");
        let hub = StoreHub::open(&net, dir.path(), HubConfig::default())
            .await
            .expect("store");
        let reg = Registry::load(&StoreMember::default_registry(dir.path())).expect("registry");
        let member = StoreMember::new(&hub, &job, 64 * MB, reg).expect("member");
        let router = member.spawn();
        members.push(MemberNode {
            net,
            router,
            _dir: dir,
        });
    }

    let sharer = bind(160).await;
    let sharer_dir = tempfile::tempdir().expect("tempdir");
    let sharer_hub = StoreHub::open(&sharer, sharer_dir.path(), HubConfig::default())
        .await
        .expect("sharer store");
    let files = vec![("data.bin".to_owned(), random(300_001, 78))];
    let share = share_bytes(&sharer, &sharer_hub, &files)
        .await
        .expect("share");
    let access = [34; 16];
    let offer = erasure::offer_set(
        &sharer_hub,
        &share.serve.acceptor,
        share.link.hash,
        access,
        4,
        2,
    )
    .await
    .expect("offer");

    let mut nets: Vec<&Net> = vec![&coord, &sharer];
    nets.extend(members.iter().map(|m| &m.net));
    introduce(&nets);
    for m in &members {
        svc.member_joined(m.net.id(), &caps(64));
    }
    let mut set_link = share.link.clone();
    set_link.hash = offer.id;
    set_link.size = offer.bytes.len() as u64;
    set_link.access = access;
    let (placement, _) = timeout(
        LIMIT,
        keep_durably(
            &sharer,
            &job,
            &set_link,
            Keep::Set(offer.bytes.clone()),
            LIMIT,
            Some(nuee_store::self_provider(&sharer)),
            |_| {},
        ),
    )
    .await
    .unwrap_or_else(|_| panic!("not placed in time: {:?}", svc.state()))
    .expect("keep");
    assert!(
        matches!(placement, Placement::Placed(_)),
        "{placement:?}: {:?}",
        svc.state()
    );
    let mut holders: Vec<EndpointId> = offer
        .header
        .shards
        .iter()
        .flat_map(|h| holders_of(&svc, *h))
        .collect();
    holders.sort();
    holders.dedup();
    assert_eq!(holders.len(), 6, "one shard on each member");
    for m in members {
        m.router.shutdown().await.expect("shutdown");
    }
}

#[test]
fn ending_one_files_session_keeps_an_overlapping_one() {
    // A member fetching the header and a shard of one set opens two sessions with the sharer.
    let gate = Gate::new(u64::MAX);
    let peer = id(9);
    let (header, shard) = (Hash::of(b"header"), Hash::of(b"shard"));
    gate.open_session(peer, [header, shard]);
    gate.open_session(peer, [header, shard]);
    gate.close_session(peer);
    assert!(
        gate.may_get(&peer, &shard),
        "the other session still fetches"
    );
    gate.close_session(peer);
    assert!(!gate.is_admitted(&peer));
    // A stray close after a revoke does not underflow into a grant.
    gate.revoke(peer);
    gate.close_session(peer);
    assert!(!gate.is_admitted(&peer));
}
