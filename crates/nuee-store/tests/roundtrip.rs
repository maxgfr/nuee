//! End-to-end file sharing between in-process endpoints: no relay, direct loopback
//! addresses, temporary data directories.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use iroh::EndpointAddr;
use iroh_blobs::protocol::{ChunkRangesSeq, PushRequest};
use nuee_net::{Net, NetConfig, RelayChoice};
use nuee_proto::{FileLink, Hash, Link, SecretKey};
use nuee_store::{
    HubConfig, Registry, StoreError, StoreHub, decrypt_bytes, encrypt_bytes, get, get_bytes, pack,
    pack_bytes, pin, serve_pins, share_bytes, share_paths, unpack, unpack_bytes,
};
use tempfile::TempDir;
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

struct Node {
    net: Net,
    hub: StoreHub,
    dir: TempDir,
}

async fn node(seed: u8) -> Node {
    let net = bind(seed).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let hub = StoreHub::open(&net, dir.path(), HubConfig::default())
        .await
        .expect("open store");
    Node { net, hub, dir }
}

fn knows(a: &Node, b: &Node) {
    a.net.add_peer(b.net.id(), None, loopback(&b.net));
}

fn addr_of(n: &Node) -> EndpointAddr {
    EndpointAddr::from_parts(
        n.net.id(),
        loopback(&n.net).into_iter().map(iroh::TransportAddr::Ip),
    )
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

/// Relative path ⇒ file bytes (`None` for a directory).
fn tree(root: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Option<Vec<u8>>>) {
        for e in std::fs::read_dir(dir).expect("read_dir") {
            let e = e.expect("entry");
            let p = e.path();
            let rel = p
                .strip_prefix(root)
                .expect("prefix")
                .to_str()
                .expect("utf8")
                .to_owned();
            let ft = std::fs::symlink_metadata(&p).expect("meta").file_type();
            if ft.is_dir() {
                out.insert(rel, None);
                walk(root, &p, out);
            } else if ft.is_file() {
                out.insert(rel, Some(std::fs::read(&p).expect("read")));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn sample_files() -> Vec<(String, Vec<u8>)> {
    vec![
        ("notes.txt".to_owned(), b"bonjour".to_vec()),
        ("data/blob.bin".to_owned(), noise(300_000, 1)),
        ("data/vide".to_owned(), Vec::new()),
        ("été/ñandú 🌸.md".to_owned(), "« ok »".as_bytes().to_vec()),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn share_a_file_and_a_folder_then_get_them() {
    let a = node(1).await;
    let b = node(2).await;
    knows(&b, &a);

    let src = tempfile::tempdir().expect("src");
    let file = src.path().join("lettre.txt");
    std::fs::write(&file, b"une lettre").expect("write");
    let folder = src.path().join("dossier é");
    std::fs::create_dir_all(folder.join("sous/plus bas")).expect("mkdir");
    std::fs::create_dir_all(folder.join("vide")).expect("mkdir");
    std::fs::write(
        folder.join("sous/plus bas/données.bin"),
        noise(1_500_000, 7),
    )
    .expect("w");
    std::fs::write(folder.join("vide.txt"), b"").expect("w");
    std::fs::write(folder.join("🌸.md"), "fleur".as_bytes()).expect("w");
    #[cfg(unix)]
    std::os::unix::fs::symlink("/etc/passwd", folder.join("lien")).expect("symlink");

    let share = share_paths(&a.net, &a.hub, &[file.clone(), folder.clone()])
        .await
        .expect("share");
    #[cfg(unix)]
    assert_eq!(share.skipped, vec![folder.join("lien")]);

    // The link survives its text form.
    let url = Link::File(share.link.clone()).to_url(nuee_proto::link::DEFAULT_BASE);
    let Link::File(link) = Link::parse(&url).expect("parse") else {
        panic!("not a file link");
    };
    assert_eq!(link, share.link);

    let dest = tempfile::tempdir().expect("dest");
    let report = timeout(
        LIMIT,
        get(&b.net, &b.hub, &link, dest.path(), false, |_, _| {}),
    )
    .await
    .expect("in time")
    .expect("get");
    assert!(report.verified);
    assert_eq!(report.files.len(), 4);

    let mut want = tree(src.path());
    want.remove("dossier é/lien");
    assert_eq!(tree(dest.path()), want);

    // A second get into the same place refuses to overwrite, unless asked.
    let again = get(&b.net, &b.hub, &link, dest.path(), false, |_, _| {}).await;
    assert!(matches!(again, Err(StoreError::Exists(_))), "{again:?}");
    let forced = get(&b.net, &b.hub, &link, dest.path(), true, |_, _| {})
        .await
        .expect("overwrite");
    assert_eq!(forced.files.len(), 4);
    assert_eq!(tree(dest.path()), want);

    // In-memory variant, as used by browsers.
    let c = bind(3).await;
    c.add_peer(a.net.id(), None, loopback(&a.net));
    let mem = StoreHub::memory(&c, HubConfig::default());
    // The first share owns the router of `a`; a second share runs on its own node.
    let d = node(4).await;
    c.add_peer(d.net.id(), None, loopback(&d.net));
    let share2 = share_bytes(&d.net, &d.hub, &sample_files())
        .await
        .expect("share bytes");
    let got = timeout(LIMIT, get_bytes(&c, &mem, &share2.link, |_, _| {}))
        .await
        .expect("in time")
        .expect("get bytes");
    assert_eq!(got, sample_files());
    share.shutdown().await.expect("shutdown");
    share2.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wrong_key_and_wrong_access_are_reported() {
    let a = node(10).await;
    let b = node(11).await;
    knows(&b, &a);
    let share = share_bytes(&a.net, &a.hub, &sample_files())
        .await
        .expect("share");

    let mut bad_key = share.link.clone();
    // The low bits of byte 0 are cleared by X25519 clamping; flip a bit that matters.
    bad_key.key[5] ^= 1;
    let res = get_bytes(&b.net, &b.hub, &bad_key, |_, _| {}).await;
    match res {
        Err(StoreError::Decrypt(msg)) => assert!(msg.contains("wrong key"), "{msg}"),
        other => panic!("expected Decrypt, got {other:?}"),
    }

    let c = node(12).await;
    knows(&c, &a);
    let mut bad_access = share.link.clone();
    bad_access.access[0] ^= 1;
    let res = get_bytes(&c.net, &c.hub, &bad_access, |_, _| {}).await;
    assert!(matches!(res, Err(StoreError::Refused)), "{res:?}");
    share.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tampering_is_detected() {
    // Authenticated encryption: any flipped byte, header or payload, fails.
    let (cipher, key) = encrypt_bytes(&noise(200_000, 3)).expect("encrypt");
    assert_eq!(
        decrypt_bytes(&cipher, &key).expect("clean"),
        noise(200_000, 3)
    );
    for at in [10, cipher.len() / 2, cipher.len() - 1] {
        let mut bad = cipher.clone();
        bad[at] ^= 0x40;
        let res = decrypt_bytes(&bad, &key);
        assert!(
            matches!(res, Err(StoreError::Decrypt(_))),
            "byte {at}: {res:?}"
        );
    }
    let res = decrypt_bytes(&cipher[..cipher.len() - 10], &key);
    assert!(matches!(res, Err(StoreError::Decrypt(_))), "{res:?}");

    // A link whose hash or size does not match what the provider serves.
    let a = node(20).await;
    let b = node(21).await;
    knows(&b, &a);
    let share = share_bytes(&a.net, &a.hub, &sample_files())
        .await
        .expect("share");
    let mut bad_size = share.link.clone();
    bad_size.size += 1;
    let res = get_bytes(&b.net, &b.hub, &bad_size, |_, _| {}).await;
    assert!(matches!(res, Err(StoreError::Integrity(_))), "{res:?}");
    let mut bad_hash = share.link.clone();
    bad_hash.hash = Hash::of(b"something else");
    let res = get_bytes(&b.net, &b.hub, &bad_hash, |_, _| {}).await;
    assert!(matches!(res, Err(StoreError::Integrity(_))), "{res:?}");
    // Nothing above poisoned the store: the right link still works.
    let ok = get_bytes(&b.net, &b.hub, &share.link, |_, _| {})
        .await
        .expect("get");
    assert_eq!(ok, sample_files());
    share.shutdown().await.expect("shutdown");
}

/// Pushes `evil` from `from` into `to`; returns the application close code if `to` closed
/// the connection. The pusher's own result says nothing: it may finish writing before the
/// provider answers.
async fn try_push(from: &Node, to: EndpointAddr, evil: Hash) -> Option<u64> {
    let conn = from
        .net
        .endpoint()
        .connect(to, iroh_blobs::ALPN)
        .await
        .expect("connect");
    let req = PushRequest::new(iroh_blobs::Hash::from_bytes(evil.0), ChunkRangesSeq::root());
    let res = from
        .hub
        .store()
        .remote()
        .execute_push(conn.clone(), req)
        .complete()
        .await;
    eprintln!("push result as seen by the pusher: {res:?}");
    // Give the provider time to store anything it would have accepted.
    let closed = timeout(Duration::from_secs(2), conn.closed()).await.ok();
    let code = closed.and_then(|reason| match reason {
        iroh::endpoint::ConnectionError::ApplicationClosed(ac) => Some(u64::from(ac.error_code)),
        _ => None,
    });
    conn.close(0u32.into(), b"done");
    code
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn push_is_refused_admitted_or_not() {
    let a = node(30).await;
    let share = share_bytes(&a.net, &a.hub, &sample_files())
        .await
        .expect("share");
    let e = node(31).await;
    let evil_bytes = bytes::Bytes::from(noise(50_000, 99));
    let evil = e.hub.add_bytes(evil_bytes).await.expect("add");

    // Control: a bare provider with pushes enabled does store what this pusher sends, so
    // the checks below would see a regression.
    let control = bind(32).await;
    let control_store = iroh_blobs::store::mem::MemStore::new();
    let open_mask = iroh_blobs::provider::events::EventMask {
        push: iroh_blobs::provider::events::RequestMode::None,
        ..iroh_blobs::provider::events::EventMask::DEFAULT
    };
    let (events, _events_rx) = iroh_blobs::provider::events::EventSender::channel(8, open_mask);
    let control_router = control
        .router()
        .accept(
            iroh_blobs::ALPN,
            iroh_blobs::BlobsProtocol::new(&control_store, Some(events)),
        )
        .spawn();
    let control_addr = EndpointAddr::from_parts(
        control.id(),
        loopback(&control).into_iter().map(iroh::TransportAddr::Ip),
    );
    try_push(&e, control_addr, evil).await;
    assert!(
        control_store
            .blobs()
            .has(iroh_blobs::Hash::from_bytes(evil.0))
            .await
            .expect("has"),
        "the control provider should have accepted the push"
    );
    control_router.shutdown().await.expect("shutdown control");

    // Not admitted: the connection is closed with the permission code.
    let code = try_push(&e, addr_of(&a), evil).await;
    assert_eq!(
        code,
        Some(u64::from(iroh_blobs::protocol::ERR_PERMISSION)),
        "stranger not refused at connect time"
    );
    assert_eq!(a.hub.complete_size(evil).await.expect("status"), None);

    // Admitted, even for that very hash: pushes stay disabled.
    a.hub.gate().admit(e.net.id());
    a.hub.gate().admit_for(e.net.id(), evil);
    try_push(&e, addr_of(&a), evil).await;
    let status = a
        .hub
        .store()
        .blobs()
        .status(iroh_blobs::Hash::from_bytes(evil.0))
        .await
        .expect("status");
    assert!(
        matches!(status, iroh_blobs::api::blobs::BlobStatus::NotFound),
        "{status:?}"
    );
    share.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_blob_get_needs_admission() {
    let a = node(40).await;
    let share = share_bytes(&a.net, &a.hub, &sample_files())
        .await
        .expect("share");
    let link = share.link.clone();
    let e = node(41).await;

    // A stranger is refused at connect time.
    let res = e
        .hub
        .fetch_to_store(addr_of(&a), link.hash, link.size, &mut |_, _| {})
        .await;
    assert!(matches!(res, Err(StoreError::Refused)), "{res:?}");

    // A peer admitted for another hash cannot read this one.
    a.hub.gate().admit_for(e.net.id(), Hash::of(b"other"));
    let res = e
        .hub
        .fetch_to_store(addr_of(&a), link.hash, link.size, &mut |_, _| {})
        .await;
    assert!(matches!(res, Err(StoreError::Refused)), "{res:?}");
    assert_eq!(e.hub.complete_size(link.hash).await.expect("status"), None);

    // Once admitted for it, the same request goes through.
    a.hub.gate().admit_for(e.net.id(), link.hash);
    e.hub
        .fetch_to_store(addr_of(&a), link.hash, link.size, &mut |_, _| {})
        .await
        .expect("admitted fetch");
    assert_eq!(
        e.hub.complete_size(link.hash).await.expect("status"),
        Some(link.size)
    );
    share.shutdown().await.expect("shutdown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pinned_share_outlives_its_provider() {
    let a = node(50).await;
    let c = node(51).await;
    knows(&c, &a);
    let share = share_bytes(&a.net, &a.hub, &sample_files())
        .await
        .expect("share");

    let reg_path = Registry::default_path(c.dir.path());
    let mut registry = Registry::load(&reg_path).expect("load");
    let pinned = timeout(
        LIMIT,
        pin(
            &c.net,
            &c.hub,
            &share.link.pin_link(),
            &mut registry,
            |_, _| {},
        ),
    )
    .await
    .expect("in time")
    .expect("pin");
    let ids: Vec<_> = pinned.providers.as_slice().iter().map(|p| p.id).collect();
    assert_eq!(ids, vec![a.net.id(), c.net.id()]);

    // The registry is on disk, private, and has no key in it.
    let reloaded = Registry::load(&reg_path).expect("reload");
    assert_eq!(reloaded.list(), registry.list());
    assert_eq!(reloaded.list().len(), 1);
    assert_eq!(
        reloaded.list()[0].link().expect("link"),
        share.link.pin_link()
    );
    let json = std::fs::read_to_string(&reg_path).expect("read");
    assert!(
        !json.contains(
            &nuee_store::AgeKey(share.link.key)
                .identity()
                .to_public()
                .to_string()
        )
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&reg_path)
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    let pins = serve_pins(&c.net, &c.hub, &reloaded).expect("serve pins");
    share.shutdown().await.expect("shutdown a");
    a.net.close().await;

    let d = node(52).await;
    knows(&d, &a);
    knows(&d, &c);
    let link = FileLink {
        providers: pinned.providers.clone(),
        hash: pinned.hash,
        size: pinned.size,
        access: pinned.access,
        key: share.link.key,
    };
    let got = timeout(LIMIT, get_bytes(&d.net, &d.hub, &link, |_, _| {}))
        .await
        .expect("in time")
        .expect("get via the pinner");
    assert_eq!(got, sample_files());
    pins.shutdown().await.expect("shutdown c");
}

/// A tar with one entry whose raw name and type are set by hand, bypassing the checks of
/// `tar::Builder`.
fn raw_tar(name: &[u8], kind: tar::EntryType, link: Option<&[u8]>, data: &[u8]) -> Vec<u8> {
    let mut h = tar::Header::new_old();
    {
        let old = h.as_old_mut();
        old.name[..name.len()].copy_from_slice(name);
        if let Some(l) = link {
            old.linkname[..l.len()].copy_from_slice(l);
        }
    }
    h.set_entry_type(kind);
    h.set_size(data.len() as u64);
    h.set_mode(0o644);
    h.set_cksum();
    let mut b = tar::Builder::new(Vec::new());
    b.append(&h, data).expect("append");
    b.into_inner().expect("finish")
}

#[test]
fn unpack_refuses_escapes_links_and_devices() {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "parent",
            raw_tar(b"../evil", tar::EntryType::Regular, None, b"x"),
        ),
        (
            "nested parent",
            raw_tar(b"a/../../evil", tar::EntryType::Regular, None, b"x"),
        ),
        (
            "absolute",
            raw_tar(b"/tmp/nuee-evil", tar::EntryType::Regular, None, b"x"),
        ),
        (
            "symlink",
            raw_tar(b"link", tar::EntryType::Symlink, Some(b"/etc/passwd"), b""),
        ),
        (
            "hardlink",
            raw_tar(b"hard", tar::EntryType::Link, Some(b"/etc/passwd"), b""),
        ),
        ("fifo", raw_tar(b"pipe", tar::EntryType::Fifo, None, b"")),
        ("device", raw_tar(b"dev", tar::EntryType::Char, None, b"")),
    ];
    for (what, bytes) in cases {
        let root = tempfile::tempdir().expect("root");
        let dest = root.path().join("dest");
        let res = unpack(bytes.as_slice(), &dest, true);
        assert!(
            matches!(res, Err(StoreError::UnsafePath(_))),
            "{what}: {res:?}"
        );
        let res = unpack_bytes(&bytes);
        assert!(
            matches!(res, Err(StoreError::UnsafePath(_))),
            "{what}: {res:?}"
        );
        assert!(!root.path().join("evil").exists(), "{what}");
        assert!(tree(&dest).is_empty(), "{what}: {:?}", tree(&dest));
    }
    assert!(!Path::new("/tmp/nuee-evil").exists());
}

#[cfg(unix)]
#[test]
fn unpack_never_follows_an_existing_link() {
    let root = tempfile::tempdir().expect("root");
    let outside = root.path().join("outside");
    std::fs::create_dir(&outside).expect("mkdir");
    let dest = root.path().join("dest");
    std::fs::create_dir(&dest).expect("mkdir");
    std::os::unix::fs::symlink(&outside, dest.join("sub")).expect("symlink");
    std::os::unix::fs::symlink(outside.join("f"), dest.join("f")).expect("symlink");

    let tar = pack_bytes(&[("sub/file".to_owned(), b"x".to_vec())]).expect("pack");
    let res = unpack(tar.as_slice(), &dest, true);
    assert!(matches!(res, Err(StoreError::UnsafePath(_))), "{res:?}");
    let tar = pack_bytes(&[("f".to_owned(), b"x".to_vec())]).expect("pack");
    let res = unpack(tar.as_slice(), &dest, true);
    assert!(matches!(res, Err(StoreError::UnsafePath(_))), "{res:?}");
    assert!(tree(&outside).is_empty());
}

#[test]
fn pack_skips_symlinks_and_round_trips() {
    let src = tempfile::tempdir().expect("src");
    let dir = src.path().join("d");
    std::fs::create_dir_all(dir.join("e")).expect("mkdir");
    std::fs::write(dir.join("e/a.txt"), b"a").expect("w");
    #[cfg(unix)]
    std::os::unix::fs::symlink(dir.join("e/a.txt"), dir.join("alias")).expect("symlink");
    let packed = pack(std::slice::from_ref(&dir)).expect("pack");
    #[cfg(unix)]
    assert_eq!(packed.skipped, vec![dir.join("alias")]);
    assert_eq!(packed.files, 1);
    let out = tempfile::tempdir().expect("out");
    let files = unpack(packed.file, out.path(), false).expect("unpack");
    assert_eq!(files, vec![out.path().join("d/e/a.txt")]);
    assert_eq!(
        std::fs::read(out.path().join("d/e/a.txt")).expect("r"),
        b"a"
    );

    assert!(matches!(
        pack_bytes(&[("../x".to_owned(), Vec::new())]),
        Err(StoreError::UnsafePath(_))
    ));
}

#[test]
fn data_dir_resolution() {
    use std::ffi::OsString;
    let s = |v: &str| Some(OsString::from(v));
    assert_eq!(
        nuee_store::resolve_data_dir(s("/n"), s("/x"), s("/h")),
        Some(PathBuf::from("/n"))
    );
    assert_eq!(
        nuee_store::resolve_data_dir(s(""), s("/x"), s("/h")),
        Some(PathBuf::from("/x/nuee"))
    );
    assert_eq!(
        nuee_store::resolve_data_dir(None, None, s("/h")),
        Some(PathBuf::from("/h/.local/share/nuee"))
    );
    assert_eq!(nuee_store::resolve_data_dir(None, None, None), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_store_is_capped() {
    let net = bind(60).await;
    let hub = StoreHub::memory(
        &net,
        HubConfig {
            mem_cap: 1000,
            ..HubConfig::default()
        },
    );
    hub.add_bytes(bytes::Bytes::from(vec![1u8; 600]))
        .await
        .expect("fits");
    let res = hub.add_bytes(bytes::Bytes::from(vec![2u8; 600])).await;
    assert!(matches!(res, Err(StoreError::TooLarge { .. })), "{res:?}");
}
