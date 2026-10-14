# Files and durable storage

Nuée moves files the same way it moves compute: through a link. There are three levels.

1. **Share and get.** The sharer's machine serves the file while `nuee share` runs.
2. **Pin.** Someone else keeps a ciphertext copy and serves it, so the sharer can leave.
3. **Durable storage.** A swarm's coordinator keeps a target number of copies (or a
   Reed-Solomon set) on members that offered disk space, replaces copies whose holder is gone,
   and checks holders with storage challenges.

## Share and get

```console
$ nuee share report.pdf photos/
https://maxgfr.github.io/nuee/#/F/AEA...
serving 48213990 bytes (encrypted) until Ctrl-C

$ nuee get -o downloads/ -        # paste the link on stdin
downloads/report.pdf
✓ verified (BLAKE3)
```

Several paths become one tar archive, which is then encrypted with
[age](https://age-encryption.org) (X25519) before it leaves the machine, so file names do not
leak either. The link carries the providers (up to 4), the BLAKE3 hash and size of the
ciphertext, a 16-byte access secret, and the age key. Anyone holding the link can download and
decrypt; treat it like a password, and pass it on stdin (`-`) to keep it out of shell history.

`nuee get` refuses to overwrite files without `--force` and refuses archive entries that would
land outside the output directory (absolute paths, `..`, links pointing out). The web page
opens the same links: it downloads, decrypts and checks in the tab, and shows "Verified: the
files match the link".

## Pin

```sh
nuee pin -            # a file link or a pin link, on stdin
nuee pin --list
nuee pin --rm <HASH>
nuee serve            # serve every pinned item until Ctrl-C
```

A pinner stores and serves the ciphertext only. Given a file link, `nuee pin` drops the key
before storing anything; a pin link (`#/P/…`) never had one.

## Durable storage

Durable storage needs a swarm: a running coordinator (`nuee run …` or an anchor, see
[ANCHOR.md](ANCHOR.md)) and members that offer disk space with `--store`.

```sh
# the coordinator (here a job that waits for peers, so the swarm stays up)
nuee run demo:mandelbrot --local-slots 0 --wait-peers 16

# on each member that lends disk space
nuee join - --slots 1 --store 64MiB

# the sharer: two full copies on two members
nuee share archive.bin --replicas 2 --on -
```

Real output of `nuee share --replicas 2 --on -` (QR codes left out):

```console
https://maxgfr.github.io/nuee/#/F/AEAQM7…       the file link, served by this machine
waiting for the swarm to place the copies (Ctrl-C skips the wait)
https://maxgfr.github.io/nuee/#/F/AEB3TT…       the durable link, naming the holders
serving 301957 bytes (encrypted) until Ctrl-C
```

Members print `offering 64 MiB of storage, 0 MiB in use` when they join. Each one accepts
copies within its quota and stores only ciphertext; the coordinator never sees the key either.

Hand out the **durable link** (the second one). Its providers are the holders, so it keeps
working after the sharer stops. When holders change, `nuee get` can ask the coordinator for the
current ones:

```sh
nuee get '<durable link>' --on '<swarm link>' -o dl
```

This was run with the sharer stopped and one of the two holders killed: the download still
succeeded and the bytes matched.

### Repair

The coordinator tracks, per blob, which member holds a copy. A copy whose holder has been away
for 10 minutes counts as lost, and the coordinator places a new one on another member that has
room, fetching it from a surviving holder. The state is saved to `durable.json` in the
coordinator's data directory. `NUEE_DURABLE_ABSENT_MS` shortens the 10 minutes for drills
and tests; leave it alone otherwise, since a laptop closing its lid for a quarter of an hour
would trigger a copy.

Limits: at most 8 replicas per blob, 1,024 tracked blobs and 256 members per coordinator. The
coordinator has to be running for repair to happen; an anchor is the usual place for it.

Who may change what: the coordinator records which peer asked for each blob or erasure set.
Only that peer can release it (`Release` from anyone else is refused), and `List` shows each
peer its own entries only; the coordinator's operator keeps full control. Each requester may
have at most 128 tracked blobs (shards and set headers count) and 64 GiB of copies (size times
copies). A blob that never got a first copy is dropped after 24 hours, and a `ttl_s` set in
`Replicate` is enforced. Under `--approve`, `nuee/pin/0` only serves peers the operator let
into the swarm, so a sharer must be admitted first; banned peers are refused and get no new
copies. Store members that left while holding nothing are forgotten, also after a restart.

On a member, a released copy and a failed or abandoned download are removed from disk by the
store's garbage collection (every minute); until then their bytes still count against the
member's quota, as do downloads in progress, so the disk stays within the declared quota.
A released copy the node still keeps for itself (its own pin or share of the same blob) is not
counted: garbage collection never removes it. Migration limit: copies stored before this
version keep their old tags, so they are not reclaimed after a release (and, being kept,
no longer count against the quota).

Placement: members with the most free room go first, but free room counts only up to 64 GiB
(quotas are announced by the members and never verified), and members with equal room are
picked by a per-blob draw, so an identity announcing a huge quota does not take every copy.

### Storage challenges

Every hour the coordinator challenges each holder: it derives a few 1 KiB ranges of the blob
from a fresh nonce (the last chunk is always included, which binds the size to the hash), asks
for them with a ranged iroh-blobs request, and checks them against the blob's BLAKE3 root hash
while they stream in. It keeps nothing but the hash and the size.

What a passed challenge proves: the holder could produce those ranges at that moment, so it
holds them or can reach them. What it does not prove: that the rest of the blob is stored.
Repeated challenges with fresh nonces make a partial copy likely to be caught over time, and
erasure coding (below) turns "most shards answer" into "the file can be rebuilt". A holder that
fails a challenge loses its copy in the coordinator's books, and the copy is placed again.

Limit: challenges prove possession of ranges, not independent copies. A member holds the access
secret and the providers, so it can answer by fetching the sampled ranges from another copy, and
one link holder running several identities can hold every replica of a blob (and drop them all
at once). Copies always go to distinct member identities; nothing proves those identities are
distinct operators. Under `--approve` only members the operator let in receive copies.

Residual risk without `--approve`: one link holder with several free identities can fill the
coordinator's global table of tracked blobs (8 identities x 128 blobs) and its 256 member slots
for as long as those identities stay connected. Run anchors and coordinators that offer durable
storage with `--approve` when the swarm link circulates beyond the group.

### Reed-Solomon sets

```sh
nuee share archive.bin --erasure 4+2 --on -
```

Instead of full copies, the ciphertext is cut into `k` data shards and `m` parity shards (4+2
by default), one shard per member, each its own blob. Any `k` of the `k + m` shards rebuild
the file: with 4+2, two members can disappear and storage costs 1.5 times the file instead of
2 or 3 times. A small versioned header (`NRS1`, itself a blob, kept in two copies) lists the
shard hashes and the hash of the whole ciphertext; its hash identifies the set. A shard whose
every copy is lost is rebuilt from any `k` surviving ones and placed again.

`share --erasure` prints a second link, to the set. `nuee get` on that link fetches the
header, then enough shards, rebuilds and checks the ciphertext, then decrypts, as usual.
Exactly `k + m` store members are enough to place a set; no spare member is needed (a test
places 4+2 on six members). This was run with 6 members and the sharer stopped.

## Browser limits

- The web page keeps a download in memory: files and folders above 256 MiB in total are
  refused, both to share and to download. Use the CLI for larger ones.
- The web page opens an erasure-set link like `nuee get` does: it fetches the header, then
  enough shards from the link's providers, rebuilds the ciphertext, checks it against the
  header's hash and decrypts (same 256 MiB cap; the set's ciphertext length counts). It only
  asks the providers printed in the link; unlike `nuee get --on`, it cannot ask the swarm's
  coordinator for fresh holders, so a link whose holders all moved needs the CLI.
- Tabs do not pin and do not offer storage; only `nuee join --store` and anchors do.

## Security notes

- Everything stored or relayed by others is age ciphertext. Pinners, store members, relays
  and the coordinator never get the key; only holders of the `F` link can decrypt.
- Blobs are served only to peers that proved the access secret of that blob or set (HMAC over
  both identities, constant-time comparison). Wrong proofs are throttled. iroh-blobs push is
  disabled, so nobody can write into a member's store.
- Integrity: every byte is checked against BLAKE3 while it streams (bao), before decryption.
  A mismatch ends `nuee get` with exit code 6.
- Exception: `nuee site publish --on <anchor link>` gives the anchor the full site link, key
  included, so it can serve the site over HTTP. See [HOSTING.md](HOSTING.md).

## Troubleshooting

- **`share --replicas` keeps printing `still placing after N s`.** The swarm has fewer store
  members with room than the copies asked. Ctrl-C stops waiting; the file is still served
  from your machine, and the durable link names the holders placed so far.
- **`the swarm refused to keep copies`.** The coordinator has no store members, or the file is
  larger than their quotas.
- **`get` fails with the sharer gone.** Use the durable link, not the first one, and add
  `--on <swarm link>` so the coordinator can name the current holders.
- **`error: the link is corrupt` or `truncated`.** The link was cut while copying. QR codes and
  `-` on stdin avoid that.
