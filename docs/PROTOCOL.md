# Nuée wire protocol, version 0

This document describes the protocol as implemented in `crates/nuee-proto` (types, encoding,
links, manifests, signatures), `crates/nuee-net` (framing, admission, the WebRTC lane) and the
task and storage formats of `nuee-gpu`, `nuee-train` and `nuee-store`. When this text and the
code disagree, the code is right and this file has a bug.

All integers in postcard encodings are varints unless stated otherwise; all fixed-layout integers
outside postcard (framing, signed messages, envelopes) have the byte order given explicitly.

## 1. Transport and ALPNs

Every peer is an iroh endpoint. Its identity (`EndpointId`) is an ed25519 public key; connections
are QUIC with TLS 1.3 where both sides authenticate with their key, so the remote identity of a
connection is known before the first byte of application data.

| ALPN | Constant | Purpose |
|---|---|---|
| `nuee/job/0` | `ALPN_JOB` | Swarm control: admission, jobs, leases, results, WebRTC signalling |
| iroh-blobs ALPN | (iroh-blobs) | Modules, assets, outputs above 1 MiB, shared files. Only admitted peers; push disabled |
| `nuee/pin/0` | `ALPN_PIN` | Replication orders, erasure sets, holder lookups |
| `nuee/tcp/0` | `ALPN_TCP` | Exposed TCP service tunnel |
| `nuee/http/0` | `ALPN_HTTP` | Console HTTP requests |

`PROTO_VERSION = 0` is carried in every `Hello` and every manifest. The trailing `/0` of each ALPN
is the major version of that sub-protocol.

## 2. Encoding

### 2.1 Messages

Messages are serialized with [postcard](https://postcard.jamesmunk.ooo/) (serde). Enum variants
are encoded as a varint index in declaration order; adding a variant at the end is compatible,
reordering is not.

`decode_with_limit::<T>(bytes, max)`:

1. rejects `bytes.len() > max` (`ProtoError::TooLarge`) before parsing;
2. decodes with postcard;
3. rejects any trailing byte (`ProtoError::Trailing`).

`decode` uses `max = MAX_MSG` (8 MiB). Console messages (`HttpRequest`, `HttpResponse`) use
`MAX_HTTP_MSG = MAX_HTTP_BODY + 512 KiB`. `encode_with_limit` refuses to produce more than `max`
bytes.

### 2.2 Bounded containers

Every variable-length field of a wire type uses one of:

| Type | Encoding | Cap check |
|---|---|---|
| `BoundedBytes<N>` | postcard byte string (varint length + bytes) | length > N rejected before copying |
| `BoundedString<N>` | postcard string (varint length + UTF-8) | byte length > N rejected; invalid UTF-8 rejected |
| `BoundedVec<T, N>` | postcard sequence (varint count + items) | count > N rejected before allocating; the pre-allocation is `min(count, N)` |

Nested unbounded collections (`Vec<Vec<_>>`) never appear in wire types.

Fixed-size types: `Hash` = 32 raw bytes (BLAKE3), `EndpointId` = 32 bytes (must be a valid
ed25519 point), `Signature` = 64 bytes, `[u8; N]` = N raw bytes.

### 2.3 Stream framing

On every stream of `nuee/job/0` (and the other nuee ALPNs), a message is one frame:

```
u32 big-endian length || postcard payload (length bytes)
```

`read_frame(recv, max)` reads the 4-byte header, rejects `length > max` before allocating the
buffer, then reads exactly `length` bytes. A stream that ends before the header is a clean close;
one that ends inside a frame is `Truncated`. The first frame on `nuee/job/0` (the `Hello`) is
capped at `MAX_HELLO = 64 KiB`; later frames at `MAX_MSG`.

## 3. Admission

### 3.1 Proof

A link holder proves knowledge of the 16-byte link secret without sending it:

```
proof = HMAC-SHA256(key = secret, label || client_id (32) || server_id (32))
```

| Sub-protocol | Label |
|---|---|
| `nuee/job/0` | `nuee/hello/0` (`PROOF_HELLO`) |
| `nuee/pin/0` | `nuee/pin-hello/0` (`PROOF_PIN`) |
| `nuee/tcp/0` | `nuee/tcp-hello/0` (`PROOF_TCP`) |
| `nuee/http/0` | `nuee/http-hello/0` (`PROOF_HTTP`) |

Binding both identities makes a proof useless on any other connection: a relay or a different
coordinator cannot replay it. The server recomputes the MAC and compares with
`hmac::Mac::verify_slice` (constant time). Secrets come from the OS random source
(`new_secret16`).

### 3.2 Handshake on `nuee/job/0`

1. The worker opens one bidirectional stream and sends `Msg::Hello{version, proof, caps}`.
2. The coordinator, under `HELLO_TIMEOUT_MS = 5000`, checks `version == PROTO_VERSION` and the
   proof. At most `MAX_PENDING_UNAUTH = 64` handshakes run at once; more are closed at once.
   Five wrong proofs from one `EndpointId` within 60 s refuse it for 60 s (`nuee-net::admit`).
3. On success the peer is admitted to the blob store and the core state machine receives it. The
   coordinator answers `Pending` (manual approval mode) or `Welcome{..}`; a refusal is
   `Bye{Rejected}` or `Bye{Full}`.
4. All later traffic flows on this same stream pair.

QUIC application close codes (`nuee-net::driver::close_code`):

| Code | Meaning |
|---|---|
| 0 | done |
| 1 | refused (bad proof, bad version, rejected) |
| 2 | busy (too many handshakes, throttled) |
| 3 | hello timeout |
| 4 | protocol error |
| 5 | banned |
| 6 | swarm full |
| 7 | shutdown |

## 4. Messages on `nuee/job/0` (`enum Msg`)

Variant order (= wire index):

| # | Variant | Direction | Fields and caps |
|---|---|---|---|
| 0 | `Hello` | W→C | `version: u16`, `proof: [u8;32]`, `caps: Caps` |
| 1 | `Pending` | C→W | none |
| 2 | `Welcome` | C→W | `swarm: SwarmInfo`, `jobs: BoundedVec<SignedManifest,16>`, `webrtc: bool`, `stun: BoundedVec<BoundedString<256>,4>` |
| 3 | `Job` | C→W | `SignedManifest` |
| 4 | `Want` | W→C | `slots: u16` (n > 0 adds credit, 0 pauses) |
| 5 | `Lease` | C→W | `Lease` (below) |
| 6 | `Result` | W→C | `lease: u64`, `output: Payload`, `output_hash: Hash`, `duration_ms: u32`, `sig: Signature` |
| 7 | `Fail` | W→C | `lease: u64`, `kind: FailKind`, `detail: BoundedString<256>` |
| 8 | `RtcOffer` | both | `RtcSignal` |
| 9 | `RtcAnswer` | both | `RtcSignal` |
| 10 | `RtcIce` | both | `RtcSignal` |
| 11 | `Cancel` | C→W | `lease: u64` |
| 12 | `Bye` | both | `reason: ByeReason` |

Supporting types:

- `Caps{agent: BoundedString<64>, browser: bool, cpu_slots: u16, gpu: Option<GpuCaps>,
  mem_mb: u32, kinds: u32, cached: BoundedVec<Hash,64>, exec_allow:
  BoundedVec<BoundedString<160>,16>, store_quota_mb: u64, relay: Option<BoundedString<256>>,
  rtc: bool}`. `kinds` is a bitset, bit `i` = `TaskKind` with index `i`.
- `GpuCaps{adapter: BoundedString<64>, max_buffer: u64, max_storage_binding: u64, f16: bool}`.
- `SwarmInfo{name: BoundedString<64>, coordinator: EndpointId, mode: SwarmMode, file:
  Option<FileInfo{hash, size: u64}>}`; `SwarmMode = Compute | Files | Service | Site`.
- `Lease{lease: u64, job: u32, task: u32, kind: TaskKind, module: Option<Hash>, assets:
  BoundedVec<Hash,16>, input: Payload, deadline_ms: u32}`.
- `Payload = Inline(BoundedBytes<1 MiB>) | Blob{hash: Hash, size: u64}`. A blob is served by the
  sender's iroh-blobs store to admitted peers.
- `RtcSignal{peer: EndpointId, payload: BoundedString<16 KiB>}`. `peer` is the target when
  sent, the source when relayed. The coordinator relays at most 64 signals per peer per minute
  and only when WebRTC is enabled.
- `TaskKind = Wasm | Wgsl | Embed | Llm | Exec | TrainWasm | TrainExec` (indices 0..6).
- `FailKind = Trap | Oom | Timeout | StackOverflow | Invalid | Unsupported | Cancelled | Io`.
- `ByeReason = Done | Banned | Shutdown | Rejected | Full`.

### 4.1 Result signatures

A worker signs every result:

```
sig = ed25519_sign(worker_key, "nuee/result/0" || lease (u64 little-endian) || output_hash (32))
```

The coordinator checks it with the connection's remote identity (`verify_result`). For an inline
payload it also checks `BLAKE3(bytes) == output_hash`; for a blob, `hash == output_hash` and the
fetched bytes are hashed again. A bad signature or hash mismatch bans the peer.

A `Fail` is never treated as a verification mismatch.

## 5. Other sub-protocols

### 5.1 `nuee/pin/0` (`enum PinMsg`)

| # | Variant | Fields |
|---|---|---|
| 0 | `Hello` | `version: u16`, `proof: [u8;32]` (label `nuee/pin-hello/0`) |
| 1 | `Replicate` | `hash`, `size: u64`, `providers: BoundedVec<Provider,4>`, `access: [u8;16]`, `ttl_s: u32` |
| 2 | `Release` | `hash` |
| 3 | `List` | none |
| 4 | `Pins` | `items: BoundedVec<PinInfo{hash, size, complete: bool},256>` |
| 5 | `Accepted` | `hash` |
| 6 | `Refused` | `hash`, `reason: PinRefusal = Quota \| TooLarge \| NotMember \| Busy` |
| 7 | `Done` | `hash` |
| 8 | `ReplicateSet` | `header: BoundedBytes<2101>` (an `NRS1` erasure header), `providers: BoundedVec<Provider,4>`, `access: [u8;16]` |
| 9 | `Where` | `hash` (a blob or an erasure set) |
| 10 | `Holders` | `hash`, `providers: BoundedVec<Provider,4>` (empty if unknown) |

`Provider{id: EndpointId, relay: Option<BoundedString<256>>}`. Pinners only ever receive
ciphertext and the pin link (no decryption key). Replication orders signed by a coordinator use
the domain `nuee/pin/0` (`DOMAIN_PIN`).

Sessions (`nuee-store::durable`): the client opens one bidirectional stream and sends
`Hello{version, proof}` with the swarm secret. A sharer sends one `Replicate` per wanted copy
of the same blob (the number of frames is the replica count) and gets one `Accepted` or
`Refused` per frame; `ReplicateSet` asks for an erasure set and is answered with the hash of
the header bytes. The coordinator orders a store member with `Replicate`, answered `Accepted`
(or `Refused`) then `Done` once stored. `Where` is answered with `Holders`, which `nuee get --on`
uses when a link's providers are gone. The coordinator records the peer that first asked for a
blob or set: only that peer's `Release` is applied (anyone else gets `Refused{NotMember}`), and
`List` returns the caller's own entries. `ttl_s` (0: until released) runs from the moment the
coordinator first plans the blob. Under `--approve` the coordinator closes `nuee/pin/0` sessions
of peers the job driver has not let in (close code `REFUSED`), and those of banned peers always.
A store member answers `Replicate`, `Release` and `List` from its coordinator only (`List` from
anyone else gets an empty `Pins`).

Storage challenges use plain ranged iroh-blobs requests: the challenger picks a fresh 32-byte
nonce, derives up to `k` chunk indices with `BLAKE3-keyed(nonce, "nuee/challenge/0" || size
(u64 LE))` in XOF mode (each index `u64 LE mod chunk_count`, 1 KiB chunks), adds the last chunk,
and verifies the returned ranges against the root hash (`nuee-store::challenge`).

### 5.2 `nuee/tcp/0` (`enum TcpMsg`)

`Hello{version, proof}` (label `nuee/tcp-hello/0`) → `Ready` or `Refused{reason:
BoundedString<128>}`.

On both `nuee/tcp/0` and `nuee/http/0`, a connection none of whose streams has proved the
secret is closed (code 3) when it opens no stream within the hello deadline, and one
connection holds at most 4 handshakes at a time (`MAX_PENDING_PER_CONN`; extra streams are
refused at once). After `Ready` the stream carries raw bytes in both directions, to and from
`127.0.0.1:<port>` on the host and nowhere else.

### 5.3 `nuee/http/0`

One bidirectional stream per request, framed as in 2.3, in two frames: first a hello,
postcard `TcpMsg::Hello{version, proof}` (label `nuee/http-hello/0`) capped at 64 bytes
(`MAX_CONSOLE_HELLO`), then the `HttpRequest` frame capped at `MAX_HTTP_MSG`; the client then
finishes its side. The host checks the hello's version and proof under the 5 s hello deadline
and only then reads the request frame (a wrong proof gets 403 without the body being read).
The request's own `proof` field is not used. *`nuee-net::console`.*

- `HttpRequest{proof: [u8;32] (label nuee/http-hello/0), method: BoundedString<16>, path:
  BoundedString<2048>, headers: BoundedVec<Header,64>, body: BoundedBytes<8 MiB>}`
- `HttpResponse{status: u16, headers: BoundedVec<Header,64>, body: BoundedBytes<8 MiB>}`
- `Header{name: BoundedString<128>, value: BoundedString<4096>}`

The host sends requests to `http://127.0.0.1:<port>` only. An anchor serves its admin API on the
same ALPN, the proof made with its admin secret (`docs/ANCHOR.md`).

## 6. Manifests

A job is described by a `Manifest`, encoded with postcard and signed by the coordinator:

```
SignedManifest{bytes: BoundedBytes<64 KiB>, sig: Signature}
bytes = postcard(Manifest)
sig   = ed25519_sign(coordinator_key, "nuee/manifest/0" || bytes)
```

`Manifest{version: u16, job: u32, name: BoundedString<64>, kind: TaskKind, module: Option<Hash>,
module_size: u32, max_memory: u64, assets: BoundedVec<Asset,16>, tasks: u32, verify:
VerifyPolicy, compare: Comparator, deadline_ms: u32, exec: Option<ExecSpec>, train:
Option<TrainSpec>, coordinator: EndpointId}`

- `Asset{hash, size: u64, name: BoundedString<128>}`
- `VerifyPolicy{mode: Full | Sample | None, sample_permille: u16 (default 100), canary_permille:
  u16 (default 30)}`
- `Comparator = Exact | ElementTolerance{abs: f32, rel: f32, max_bad_permille: u16} |
  Cosine{min: f32} | Logits{max_gap: f32} | Jsonl{field: BoundedString<64>, tol: f32} | None`
- `ExecSpec{repo: BoundedString<256>, rev: BoundedString<40>, argv:
  BoundedVec<BoundedString<1024>,64>, env: BoundedVec<EnvVar{key: BoundedString<128>, value:
  BoundedString<1024>},32>, timeout_s: u32, gpu: bool}`
- `TrainSpec{rounds: u32, inner_steps: u32, workers: u16, outer_lr: f32, outer_momentum: f32,
  init: Hash, trainer: Wasm | Exec, codec: Int8Ef | F32, trim: f32, cos_min: f32,
  norm_max_ratio: f32}`

`SignedManifest::open(coordinator)` verifies the signature first, then decodes, then requires
`manifest.coordinator == coordinator`, then runs `Manifest::check`:

- `version == 0`;
- `module_size <= 32 MiB`, `max_memory <= 1 GiB`, every asset `<= 64 MiB`;
- both permille values `<= 1000`;
- `0 < deadline_ms <= 24 h`;
- comparator parameters finite and non-negative where meaningful, `max_bad_permille <= 1000`;
- `Wasm`, `Wgsl`, `TrainWasm` need `module`; `Exec`, `TrainExec` need `exec`; `TrainWasm`,
  `TrainExec` need `train`;
- `exec.rev` is exactly 40 lowercase hex characters, `exec.timeout_s * 1000 <= 24 h`;
- training floats finite, `0 <= trim < 0.5`.

Workers then apply their own `LocalLimits` (module, asset, memory, input, output, leases) and
refuse anything above them whatever the manifest says (`nuee-core::worker`).

## 7. Links

```
<base>#/<K>/<PAYLOAD>
```

`base` defaults to `https://maxgfr.github.io/nuee/` and is configurable. `K` is one letter:

| K | Body | Fields |
|---|---|---|
| `J` | `JobLink` | `coordinator: EndpointId`, `relay: BoundedString<256>`, `secret: [u8;16]` |
| `F` | `FileLink` | `providers: BoundedVec<Provider,4>`, `hash`, `size: u64`, `access: [u8;16]`, `key: [u8;32]` |
| `S` | `FileLink` | same as `F`, served as a static site |
| `X` | `ServiceLink` | `host: EndpointId`, `relay: BoundedString<256>`, `secret: [u8;16]` |
| `P` | `PinLink` | `FileLink` without `key` |

`key` is an age X25519 identity: whoever has an `F`/`S` link can decrypt; a `P` link only allows
fetching ciphertext.

PAYLOAD is uppercase RFC 4648 base32 without padding of:

```
version (1 byte, = 1) || postcard(body) (at most MAX_LINK_BODY = 4096 bytes) || checksum (4 bytes)
checksum = first 4 bytes of BLAKE3(version || postcard(body))
```

Parsing (`Link::parse`, `PinLink::parse`) accepts a full URL, a bare fragment or `K/PAYLOAD`,
trims surrounding whitespace, keeps only what follows the last `#`, and accepts lowercase
letters. Errors are distinct so the UI can say something useful:

- `Truncated`: empty, bad base32, shorter than version + checksum, or checksum mismatch;
- `UnsupportedVersion(v)`: first byte is not 1;
- `Corrupt`: checksum fine but the body does not decode, or an unknown kind letter;
- `WrongKind`: a valid link of another kind (`P` given to `Link::parse`, or the reverse).

## 8. Coordinator fingerprint

```
fingerprint = EMOJI[b0] .. EMOJI[b5], b = BLAKE3("nuee/fingerprint/0" || coordinator_id)
```

48 bits taken from the coordinator's `EndpointId` only, rendered with a fixed table of 256
emoji (`nuee-proto::fingerprint::EMOJI`). Both sides show it; users compare it aloud.

## 9. Task ABI

A task module is a WebAssembly module that:

- imports nothing and has no start function;
- defines exactly one 32-bit, non-shared memory with a declared maximum of at most 1 GiB,
  exported as `memory`;
- exports `nuee_alloc: (i32) -> i32` and `nuee_run: (i32, i32) -> i64`;
- uses only the MVP plus bulk-memory, multivalue, mutable-globals, nontrapping-fptoint,
  reference-types, sign-ext and simd128 (`nuee-proto::features::WASM_FEATURES`, shared with the
  wasmtime configuration). Relaxed SIMD, threads, memory64, multi-memory, tail calls, GC,
  exceptions and the component model are refused.

`validate_module` (`nuee-proto::validate`) enforces all of this and caps the module at 32 MiB.

Per lease the host creates a fresh instance, calls `p = nuee_alloc(len)` (0 means out of
memory), copies the input to `p`, calls `r = nuee_run(p, len)` and reads `r & 0xffffffff` bytes
at `r >> 32`. A trap is `FailKind::Trap`.

### 9.1 Asset envelope

When the manifest lists assets, the input handed to `nuee_run` is:

```
u32 LE asset_count || for each asset in manifest order: (u32 LE length || bytes) || lease input
```

Without assets the lease input is passed unchanged. `build_envelope` / `split_envelope`
(`nuee-proto::envelope`) implement both sides; `split_envelope` refuses more than 16 assets or a
length running past the end.

### 9.2 Task payload formats

All integers little-endian.

| Kind | Module | Lease input | Output |
|---|---|---|---|
| `Wasm` | task module | anything (asset envelope when the job has assets) | anything |
| `Wgsl` | WGSL source, at most 1 MiB | `NWG1`: entry point, workgroup counts, 1 to 8 bindings (kind 0 uniform, 1 read-only storage, 2 read-write output; sizes multiple of 4, at most 64 MiB each, 128 MiB in all) | the read-write buffers, concatenated in input order |
| `Embed` | none | `NEM1`, `u32 count` (1..=256), then per text `u32 len` (at most 8 KiB) + UTF-8 | `count × 384` f32, L2-normalised |
| `Llm` | none | JSONL decision records (at most 512 records, 1 MiB), optional first line `{"llm":{"model":…}}` | JSONL `{"uid":N,"tlogits":[…]}` |
| `Exec` | none | bytes written to `$NUEE_IN` | bytes read from `$NUEE_OUT` |
| `TrainWasm` | trainer module | `NTR1` (below), asset 0 = dataset | `NDL1` delta + f32 mean loss |
| `TrainExec` | none | `NTR1`; the params go to `$NUEE_INIT` | `NDL1` delta + f32 mean loss, encoded by the worker host from the safetensors the command wrote (`nuee-train::ExecEncoder`) |

Training input: `b"NTR1" | u32 steps | u32 shard | u32 n_shards | u64 seed | f32 lr | params`
(safetensors, f32 tensors only). Delta: `b"NDL1" | u8 codec (0 = F32, 1 = Int8Ef) | u32
n_tensors (≤ 4096)`, then per tensor `u32 len | f32 scale | data`; lengths must match the run's
layout (`nuee-train::codec`). An `Int8Ef` scale must be finite, non-negative and keep
`127 × scale` finite, else the delta is rejected; a non-finite value is sent as 0 and not carried
in the error-feedback residual. `TrainWasm` jobs declare `F32` (their outputs are compared
exactly); `TrainExec` jobs use the coordinator's `--codec` (`Int8Ef` by default), the worker
keeping one error-feedback residual per job. The loss trailer is NaN when the trainer gave none.

Erasure header (`nuee-store::erasure::header`), exact length:
`b"NRS1" | u8 version (= 1) | u16 k | u16 m | u64 original length | u32 shard size | (k + m)
shard hashes | BLAKE3 of the original ciphertext`, with `k, m >= 1`, `k + m <= 64`, shard size
`<= 64 MiB` and equal to `ceil(len / k)` rounded up to an even number.

### 9.3 WebRTC lane

Optional, between a browser worker and a browser coordinator, when the coordinator's `Welcome`
says `webrtc: true` and the worker accepts. SDP and ICE travel as `RtcOffer`/`RtcAnswer`/`RtcIce`
on `nuee/job/0`. On the data channel the worker sends only results, as `TAG_RESULT (0) ||
postcard(Msg::Result)` (at most 240 KiB per frame); the coordinator answers each with
`TAG_ACK (1) || lease (u64 LE)`. The worker keeps up to 64 unacknowledged results and resends
them over iroh if the channel closes; a result delivered twice is settled once
(`nuee-net::rtc`).

## 10. Verification semantics

Implemented in `nuee-core::coordinator` and `nuee-core::verify`.

- A result produced by the coordinator itself is authoritative.
- `None`: one result is accepted.
- `Sample`: one result is accepted; with probability `sample_permille / 1000` the coordinator
  recomputes it locally. With probability `canary_permille / 1000` a lease is a canary: a task
  whose answer the coordinator computed itself. A canary is an ordinary `Lease` (fresh tasks
  are issued in a seeded random order, so the task index tells nothing), but it repeats a
  task's input: identities sharing state can recognise it. Canaries catch naive cheaters; the
  sampled recompute, which no worker sees, is the guarantee. A task verified only by two peers
  agreeing is never a canary and never grounds for a ban.
- `Full`: every task needs two agreeing executions by distinct executors. When the coordinator
  can run the kind, an agreeing pair is also recomputed by it with probability
  `sample_permille / 1000` (both executions are judged against its answer and banned if they
  differ), and canaries are sent as in `Sample`.
- Outputs are compared with the job's comparator (`Exact` = hash equality; tolerance
  comparators are injected per job: `ElementTolerance` and `Cosine` from `nuee-gpu`, `Logits`
  from `nuee-gpu::llm`, `Jsonl` from the CLI). `Cosine` also requires every row's L2 norm to
  be 1 within 0.01; `Logits` treats a letter reported on one side only as a mismatch unless
  the other side scores it at least 10 below its best letter. `TrainExec` jobs use `None`; robust aggregation
  in `nuee-train::aggregate` replaces comparison.
- On disagreement the coordinator's own execution arbitrates. A peer is banned only once its
  result is confirmed wrong; every unverified result it produced is then re-queued (retroactive
  recompute) and the connection ends with `Bye{Banned}`.
- Checking is separate from contributing: `CoordinatorConfig::verify_slots` lets a coordinator
  with `local_slots = 0` (it takes no fresh work) still run the sampled recomputes, the
  arbitrations and one known answer for canaries (only when the job has more than one task).
  The CLI and the page turn it on for every kind they can run. When the coordinator answers
  its own lease with `Fail{Unsupported}`, the job falls back to peers only: a disagreement
  discards both results and two new agreeing executions by distinct peers are required; nobody
  is banned, and there are no canaries. The peers in that disagreement have their other
  unverified results re-executed, and everything they return afterwards needs two agreeing
  executions.
- Every accepted task carries `ms`, the time from the lease to the result as the coordinator
  measured it (round trip included, so a worker cannot shorten it): `Event::TaskAccepted`,
  `Action::Accepted`, the CLI's NDJSON `task` event, the page's `accepted` event. It is absent
  for outputs restored from an earlier run.
- An accepted or verified output names one executor: the one whose result arrived first. The
  second, agreeing execution is credited only in the per-peer `PeerStatus::verified` counter
  (`done` counts first arrivals), which the CLI's final NDJSON `done` event reports as `peers`.
- A `Fail` re-queues the task on different peers when possible. Attempts count distinct
  identities (`max_attempts`, 3 by default) with at most `3 × max_attempts` failures in all;
  once they are used up a peer that failed the task never gets it again, and the coordinator
  runs the task itself when it can before failing it (`Event::TaskFailed`). Failed checking
  leases never settle an unchecked output: it waits for a checker (`WaitingForChecker`). A
  peer with at least 4 failures on a job and more than twice as many failures as results
  stops receiving that job's leases while connected.
- A blob output not fetched before the deadline counts as `Fail{Io}`.

## 11. Limits (`nuee-proto::limits`)

| Constant | Value |
|---|---|
| `MAX_MSG` | 8 MiB |
| `MAX_INLINE_INPUT`, `MAX_INLINE_OUTPUT` | 1 MiB |
| `MAX_MODULE` | 32 MiB |
| `MAX_ASSET` | 64 MiB |
| `MAX_WASM_MEMORY` | 1 GiB |
| `MAX_MANIFEST` | 64 KiB |
| `MAX_PEERS` | 256 |
| `MAX_LEASES_PER_PEER` | 32 |
| `HELLO_TIMEOUT_MS` | 5000 |
| `MAX_PENDING_UNAUTH` | 64 |
| `MAX_JOBS` | 64 |
| `MAX_ASSETS_PER_JOB` | 16 |
| `MAX_SIGNAL` | 16 KiB |
| `MAX_HTTP_BODY` | 8 MiB |
| `MAX_HTTP_HEADERS` | 64 |
| `MAX_LINK_PROVIDERS` | 4 |
| `DEFAULT_DEADLINE_MS` | 60 000 |
| per-peer deadline | 3 x median of recent durations, clamped to [5 s, 10 min]; the job's deadline for a task that already overran one |
| `MAX_DEADLINE_MS` | 24 h (exec jobs) |

## 12. Versioning rules

- Each sub-protocol carries its major version in its ALPN (`/0`). An incompatible change bumps
  it; old and new versions can then be served side by side.
- `PROTO_VERSION` in `Hello` and `Manifest.version` must match exactly; a mismatch is refused
  (close code 1, or `ManifestError::Version`).
- Links carry their own version byte (`LINK_VERSION = 1`); an unknown version is reported as
  `UnsupportedVersion` so users are told to update instead of seeing a corrupt link.
- Domain strings (`nuee/manifest/0`, `nuee/result/0`, `nuee/pin/0`, `nuee/fingerprint/0`,
  `nuee/challenge/0` and the proof labels) end with a version; changing what is signed means a new domain string.
- Within a version, enum variants and struct fields are never reordered or removed. postcard has
  no field tags, so adding a struct field is an incompatible change.
