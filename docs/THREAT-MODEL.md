# Threat model

This document lists what Nuée protects, against whom, how, and where in the code each
mitigation lives. Wire details are in `docs/PROTOCOL.md`; usage and limits of each feature are
in the guides next to it (`GPU.md`, `TRAINING.md`, `STORAGE.md`, `HOSTING.md`, `SKYCMD.md`,
`ANCHOR.md`).

## 1. Assets

- **Contributor machines**: CPU, GPU, memory, disk, network and local files of everyone who
  runs `nuee join` or opens a join link in a browser.
- **Coordinator machine**: same, plus its identity key and the link secrets it issued.
- **Result integrity**: the outputs a coordinator accepts and writes to disk.
- **Shared content**: files and sites shared through `F`/`S` links, and the age keys that
  decrypt them.
- **Secrets**: endpoint secret keys, 16-byte link secrets, access tokens, relay tokens.
- **Privacy**: IP addresses and who participates in which swarm.
- **The web origin** `maxgfr.github.io`: whatever runs there runs with the authority of the app.

## 2. Actors

| Actor | Has | Wants |
|---|---|---|
| Link holder | A valid link (bearer secret), a fresh endpoint key | Whatever the link grants; possibly more |
| Malicious coordinator | A swarm, signed manifests, task modules, workers that trust its link | Code execution, data or resources on workers beyond the sandbox |
| Malicious worker | Admission to a swarm; possibly many ephemeral identities | Make the coordinator accept wrong results, stall the swarm, reach other workers |
| Relay operator | Runs an iroh relay used by peers | Read or alter traffic, learn who talks to whom |
| Network attacker | On-path position, or none at all (the open internet) | Impersonate a coordinator, read traffic, exhaust resources, guess secrets |
| Hosted content | HTML/JS shared as a site or served by an exposed service | Run script in the app origin, phish, steal secrets |

## 3. Trust boundaries

1. **Network → nuee-net.** Every byte from a peer is untrusted until the admission proof is
   checked, and stays untrusted afterwards: admitted peers can still send garbage.
2. **Manifest → worker.** A worker trusts that a manifest comes from the coordinator named in its
   link (signature), not that the manifest is reasonable: local limits apply on top.
3. **Task module → host.** WebAssembly modules run in wasmtime natively and in the browser's
   engine on the web, with no imports at all.
4. **`exec` job → worker OS.** Native code; crossed only when the worker opted in for that
   exact repository and revision.
5. **Shared content → browser.** Never rendered in the app origin.
6. **Peer ↔ relay.** The relay forwards encrypted QUIC packets; it is not trusted with content.

## 4. Threats and mitigations

### 4.1 Hostile code on a contributor machine (malicious coordinator)

Mitigations:

- Modules must have zero imports, no start function, exactly one 32-bit non-shared memory with a
  declared maximum of at most 1 GiB, and the exact ABI exports. Module size is capped at 32 MiB.
  *`nuee-proto::validate::validate_module`*, fuzzed by `fuzz/fuzz_targets/validate_module.rs`.
- One shared list of allowed WebAssembly proposals (bulk-memory, multivalue, mutable-globals,
  nontrapping-fptoint, reference-types, sign-ext, simd128); everything else, relaxed SIMD
  included, is refused both by the validator and by the engine configuration.
  *`nuee-proto::features::WASM_FEATURES`, applied in `nuee-exec::wasm` (engine `Config`).*
- Runtime limits: fresh instance per lease, `ResourceLimiter` capping memory growth at the
  smaller of the manifest and the worker's own limit, epoch interruption for deadlines, bounded
  stack (`max_wasm_stack`), capped output size. *`nuee-exec::wasm`.*
- Worker-side caps whatever the manifest says (`LocalLimits`: module, asset, memory, input,
  output, concurrent leases). *`nuee-proto::limits::LocalLimits`, enforced in
  `nuee-core::worker`.*
- WGSL shaders are parsed and validated by naga, and their bindings checked against the task
  input, before any device is touched; buffers are capped (64 MiB per binding, 128 MiB per
  task); a task past its deadline has its device destroyed and fails with `Timeout`.
  *`nuee-gpu::wgsl::check_shader`, `GpuRunner::run`; browsers: `web/app/worker/gpu.js`.*
- Embedding and LLM tasks take text in, numbers out; their model files are pinned by revision
  and hash, never named by the job (below, 4.7).
- Manifests are checked for consistency and limits before use. *`nuee-proto::manifest::Manifest::check`.*

Residual risk: a task inside its limits can still use all the CPU time and memory it was granted,
and engine bugs (wasmtime, browser engines) are outside our control. Workers can pause or leave
at any time.

### 4.2 Hijacked `exec`

Mitigations *(`nuee-exec::native`, `nuee` CLI `join`, `run`, `sweep`)*:

- Disabled by default. An approval binds the exact repository, full 40-hex commit, argv and
  environment (keys and values). `--allow-exec repo@rev` only pins a revision: on a TTY each
  command of that revision is shown in full (argv and environment values) and asked about; without
  a TTY only the commands listed in `--exec-approvals FILE` run, and any other command, even at the
  same revision, is refused. `run` and `sweep` approve only the commands of the operator's own job
  file. *`ExecApproval`, `NativeRunner::allowed`; `join::exec_prompt`, `run::local_native`.*
- Environment keys that change how a program finds or loads code, or which program or
  configuration it starts, are refused whatever the approval: the prefixes `LD_*`, `DYLD_*`,
  `GIT_*`, `SSH_*`, `BASH_*`, `LUA_*`, `OPENSSL_*`, `XDG_*`, `PIP_*`, `UV_*`, `NPM_CONFIG_*`,
  `CONDA_*`; the suffixes `*_OPTIONS`, `*_OPTS`; and `PATH`, `HOME`, `IFS`, `ENV`, `SHELLOPTS`,
  `BASHOPTS`, `PS4`, `PROMPT_COMMAND`, `ZDOTDIR`, `PYTHONPATH`, `PYTHONSTARTUP`, `PYTHONHOME`,
  `PYTHONUSERBASE`, `PYTHONEXECUTABLE`, `PYTHONWARNINGS`, `PYTHONBREAKPOINT`, `PYTHONPLATLIBDIR`,
  `PYTHONPYCACHEPREFIX`, `NODE_OPTIONS`, `NODE_PATH`, `PERL5OPT`, `PERL5LIB`, `PERLLIB`,
  `PERL5DB`, `RUBYOPT`, `RUBYLIB`, `GEM_HOME`, `GEM_PATH`, `BUNDLE_GEMFILE`, `CLASSPATH`,
  `RUSTC_WRAPPER`, `R_PROFILE`, `R_PROFILE_USER`, `R_ENVIRON`, `R_ENVIRON_USER`, `R_LIBS`,
  `R_LIBS_USER`, `LESSOPEN`, `LESSCLOSE`, `PAGER`, `EDITOR`, `VISUAL`, `BROWSER`, `SHELL`,
  `GCONV_PATH`, `LOCPATH`, `LANG`, `TERM`, `TMPDIR`, `NUEE_IN`, `NUEE_OUT`; keys must be
  uppercase letters, digits and `_`. A denylist cannot name every variable some program reads,
  so the prompt shows every value and an approval binds them all. *`env_key_allowed`.*
- Without `--allow-exec` for the job's `repo@rev`, `join` never asks: an exec job from the
  coordinator only prints the line an `--exec-approvals` file would need, and nothing runs.
  *`join::exec_prompt`.*
- Strings another machine chose (job names, peer agent strings, reasons, archive entry names)
  are printed with control and bidirectional characters escaped, so they cannot redraw the
  terminal around a prompt; archive entries whose names hold control characters are refused.
  *`ui::clean`, `nuee-store::safe_components`.*
- The checkout shares the task's deadline: a remote that stalls is killed with its process group,
  the repository lock is released and the lease fails (`Io`, not a mismatch). *`run_git_until`,
  `lock_until`.*
- Under `bwrap` the task runs in its own PID namespace (`--unshare-pid --die-with-parent`), so
  every process of the task ends with it; on every platform stdout and stderr get a short grace
  after the task ends, then are dropped. *`spawn_and_wait`, `drain`.*
- The manifest must carry a 40-character lowercase hex `rev`; checked in
  *`nuee-proto::manifest::Manifest::check`* and the job file parser
  *`nuee-proto::job_toml`*.
- After checkout the commit is verified again; the environment is cleared; the command runs in a
  throwaway directory with timeouts, capped outputs, and its whole process group is killed at the
  end; `sandbox-exec` (macOS) or `bwrap` (Linux) is used when present, without network unless
  the worker passed `--exec-network`. One GPU job at a time (a file lock). *`checkout`,
  `env_key_allowed`, `detect_sandbox`, `gpu_lock`, `signal_group` in `nuee-exec::native`.*
- `--exec-mirror URL=PATH` only changes where the allowed repository is fetched from (a
  local clone); it requires `--allow-exec` or `--exec-approvals` for the same URL and the commit is checked the same
  way. *`run::exec_mirror_source` in the CLI.*
- `gpu = true` jobs only go to workers that offered a GPU. *`nuee-core::sched`.*

Residual risk: **`exec` runs native code on machines that opted in.** The sandbox helpers are
best effort and absent on some systems. Enabling `exec` for a revision means trusting that code.
Accepted risk: even under the sandbox, an approved command can write `~/.cache` and
`~/Library/Caches`, which model and kernel caches need (Hugging Face and MLX downloads, compiled
kernels). Files it leaves there outlive the task and other programs may later trust them.

### 4.3 Forged results and Sybil workers

Mitigations:

- Each result is signed by the worker over `nuee/result/0 || lease || output_hash`; the output
  hash is recomputed. A bad signature or hash bans the peer immediately.
  *`nuee-proto::id::{sign_result, verify_result}`, `nuee-core::coordinator::on_result`.*
- `Sample` mode: the coordinator recomputes a share of results itself (`sample_permille`,
  default 10 %) and sends canary leases with known answers. `Full` mode adds a second execution
  of every task by a distinct peer; two agreeing peers can still be two identities of one
  attacker, so when the coordinator can run the kind it also recomputes the same sampled share
  of `Full` tasks and sends canaries, and a pair that disagrees with its answer is banned (both
  identities). What stops a Sybil is the coordinator's own recompute, not the second peer.
  *`nuee-core::coordinator`, `nuee-core::verify`.*
- Canaries catch naive cheaters only. Fresh tasks go out in a seeded random order, so a lease's
  task index does not tell fresh work from a canary, but a canary repeats a task's input: an
  attacker sharing state across identities can recognise an input it has already seen. The
  guarantee is the sampled recompute, which the worker never sees.
  *`nuee-core::Coordinator::submit`, `pick_canary`.*
- A `Fail` is never a mismatch, but failing cannot sink a task either: attempts count distinct
  identities (3 by default, and at most 9 failures in all), a peer that already failed a task
  never gets it again once attempts are exhausted, and when peers have exhausted them the
  coordinator runs the task itself when it can; only if that fails too (or it cannot run the
  kind) is the task failed. A peer whose failures on a job reach 4 and exceed twice its
  results stops receiving that job's leases. *`nuee-core::coordinator::on_fail`.*
- Tolerance comparators for floating-point outputs (calibrated per job), exact hash comparison
  otherwise. *`nuee-core::verify::Cmp`, comparators from `nuee-gpu` / `nuee-train`.* Each
  tolerance is room for a dishonest worker, stated plainly:
  - WGSL fractal (`MANDELBROT_COMPARE`, `max_bad_permille: 50`): honest GPUs, and f32 with or
    without FMA, differ on up to 89 of 4096 pixels of a demo tile, so up to 5 % of a tile's
    pixels can be wrong without the recomputation noticing. Acceptable for a demo; your own
    WGSL jobs set their own tolerance. *`nuee-gpu::wgsl`.*
  - Embeddings: cosine ≥ 0.966 (lowest measured candle/ONNX agreement 0.97173, minus 0.005),
    and every row's L2 norm within 0.01 of 1 (cosine alone ignores magnitude: scaled rows would
    skew inner-product search). A forged vector close enough to the true one passes.
    *`nuee-gpu::embed::{TAU, NORM_TOL}`.*
  - LLM scores: same argmax and every centred letter score within `max_gap` (1.5 across
    backends, 0.25 on one backend). A letter reported on one side only is accepted only when
    the other side scores it at least 10 below its best letter (a negligible probability);
    otherwise it is a mismatch, so blanking every letter but the argmax does not pass. The
    labels cannot change; the scores can drift by `max_gap`. *`nuee-gpu::llm::LogitsCompare`.*
  - JSONL (skycmd labelling): fields equal except the tolerated one, same argmax.
    *`crates/nuee/src/cmd/run/jobfile.rs`.*
- A mismatch is arbitrated by the coordinator's own execution; the peer is banned only after that
  confirmation, then every unverified result it produced is recomputed.
  *`nuee-core::coordinator::ban`.* The coordinator keeps this role when it takes no work
  (`--local-slots 0`, the page's "This page computes too" unchecked): verification runs on
  `verify_slots`, separate from `local_slots`. *`nuee-core::CoordinatorConfig::verify_slots`.*
- Degraded case: when the coordinator cannot run the job's kind (an `exec` job without
  `--allow-exec`, `wgsl` without a GPU adapter, `llm` without `--llm`, a kind the anchor or a
  page has no runner for), there is no arbitration. A disagreement discards both results and
  two new agreeing executions by distinct peers are required; nobody is banned and no canary is
  sent (a known answer is only one the coordinator computed itself). The peers in the
  disagreement have their other unverified results re-executed and need two agreeing
  executions from then on. Two tabs of one
  person holding the link can agree on a wrong result. `nuee run` says so in its first lines
  (NDJSON `guarantee` event, `coordinator_checks: false`).
  A result is never accepted unchecked to keep such a job moving, not even when checking leases
  keep failing: when no other admitted peer able to run the kind is left for the second
  execution, the tasks wait, and the coordinator
  says so once per job until those peers change (core `Event::WaitingForChecker`, NDJSON
  `waiting_checker`, page event `waitingForChecker`), naming the two ways out: a second device
  that can run the kind, or `--verify none`. *`nuee-core::Coordinator::watch_checkers`.*
- `--approve` turns admission into a manual decision (`Msg::Pending`).
- Training: WASM trainers are deterministic, so their leases are recomputed and compared
  exactly like any `wasm` task. Native (`exec`) trainers cannot be recomputed; the coordinator
  drops non-finite deltas and, with three or more, deltas whose cosine with the coordinate-wise
  median is below `cos_min` or whose norm exceeds `norm_max_ratio` times the median's, then
  takes a coordinate-wise trimmed mean (`trim` 0.2). *`nuee-train::aggregate`,
  `crates/nuee/src/cmd/train.rs` (exec trainers get `VerifyMode::None`).* Residual risk: a
  minority of workers can still bias each round within those bounds, and with one or two
  deltas there is no majority at all; use exec trainers with people you trust.

Residual risk: **Sybil with ephemeral browser keys.** Identities are free, so bans only cost an
attacker a reconnect, and in `Sample` mode a wrong result that is neither sampled nor a canary is
accepted, and in `Full` mode a wrong result two of the attacker's identities agree on is
accepted unless it is sampled or the coordinator cannot run the kind (then nothing catches it).
The defence is the coordinator's recomputation: raise `sample_permille` (up to 1000 = every
task), or use `--approve`. `None` mode trusts every worker.

### 4.4 Fake coordinator, fingerprint forging

Mitigations:

- The coordinator's public key is inside the link; QUIC/TLS 1.3 authenticates both ends with
  their keys, so a worker only talks to that key. *iroh, `nuee-net::endpoint`.*
- Manifests must be signed by that key (`nuee/manifest/0`) and name it as coordinator.
  *`nuee-proto::manifest::SignedManifest::open`, called in `nuee-core::worker`.*
- A 48-bit, six-emoji fingerprint derived only from the coordinator's key, shown on both sides
  for comparison aloud. *`nuee-proto::fingerprint`.*

Residual risk: whoever controls the channel the link travels through can substitute their own
link; the fingerprint comparison is the check against that. 48 bits resist casual collisions,
not a determined offline search against a specific fingerprint over a long time.

### 4.5 Intrusion and resource exhaustion

Mitigations:

- Admission proof `HMAC-SHA256(secret, label || worker || coordinator)` compared in constant
  time; it binds both identities so it cannot be replayed elsewhere.
  *`nuee-proto::id::{proof, check_proof}`, `nuee-net::admit::Admission::verify`.*
- `Hello` within 5 s, at most 64 handshakes pending, five failed hellos in 60 s (wrong proof or
  no hello before the deadline) refuse the remote for 60 s, on every ALPN that checks a proof
  (`nuee/job/0`, `nuee/pin/0`, `nuee/tcp/0`, `nuee/http/0`); throttling table bounded.
  *`nuee-net::admit::Admission::{verify, within}`, `nuee-net::driver`.*
- Bounded decoding everywhere: frame length checked before allocation, postcard decoding with a
  size cap and no trailing bytes, every variable-length field a bounded container.
  *`nuee-net::framing`, `nuee-proto::msg::decode_with_limit`, `nuee-proto::bounded`*; fuzzed by
  `fuzz/fuzz_targets/decode_msg.rs`.
- Peer, lease, job, asset and signalling caps (`MAX_PEERS`, `MAX_LEASES_PER_PEER`, `MAX_JOBS`,
  `MAX_ASSETS_PER_JOB`, 64 WebRTC signals per peer per minute). *`nuee-proto::limits`,
  `nuee-core::coordinator`.*
- Blob access only for admitted peers, granted with the `Welcome` (under `--approve`, only
  once the operator approved), revoked on ban; iroh-blobs push disabled with a regression
  test; per-peer blob budget; access per blob or erasure set (`admit_for`); pinning and
  storage quotas (`join --store`). *`nuee-net` `BlobHub::{admit, revoke}`,
  `nuee-store::gate::Gate`, `nuee-store::durable::StoreMember`.*
- Console requests (`nuee/http/0`): a 64-byte hello carries the proof and is checked before
  the request frame is read, so a client without the secret makes the host allocate nothing;
  8 MiB bodies, 64 headers, a request timeout. *`nuee-net::console`, `nuee-host::http`.*
- `nuee/tcp/0` and `nuee/http/0`: a connection that has not proved the secret is closed when
  it opens no stream within the hello deadline, and one connection holds at most 4 of the 64
  pending handshakes. *`nuee-host::ConnGate`.*
- The worker keeps at most 256 MiB of modules and assets between leases (least recently used
  first out), and only those of the jobs it still holds. *`nuee-net::driver::BlobCache`.*
- The anchor gateway bounds each request: 10 s for the head, then 30 s for the body (408 and
  the connection closed), so slow clients cannot hold its 256 connection slots.
  *`nuee-host::gateway`.*

Residual risk: a link holder is a member and can consume the share of resources the limits
allow it. Throttling is per identity and identities are free: a remote that makes a fresh key
for every few connections is never throttled and can still keep the 64 pending handshake slots
busy for the hello deadline, delaying honest peers (it gains no access). Volumetric attacks on
the network are out of scope.

### 4.6 Eavesdropping

Mitigations: iroh QUIC is end-to-end encrypted, including through relays; WebRTC data channels
use DTLS; shared files are encrypted with age before leaving the sharer, so pinners and relays
only see ciphertext. *iroh, `nuee-store::crypt`, `nuee-net::rtc`.*

Residual risk: **relay operators see IP addresses and connection metadata**; **WebRTC, when a
presenter enables it, exposes the IP addresses of the presenter and of each participant who
accepts to each other** (ICE candidates). It is off by default, opt-in on both sides, and the
switch and the join card say so. Only results travel on the data channel; signalling goes
through iroh, relayed by the coordinator at most 64 signals per peer per minute, with no TURN
server. *`nuee-net::rtc`, `web/app/lib/rtc.js`, `RTC_PER_MINUTE` in `nuee-core::coordinator`.*

### 4.7 SSRF

Mitigations: jobs never carry URLs. Manifests name assets by BLAKE3 hash and workers fetch
them from the coordinator over iroh-blobs. The only HTTP downloads are fixed in the code and
checked by hash: the s3-pico model (`S3_PICO_URL`, BLAKE3, in `crates/nuee/src/cmd/run.rs`),
the embedding model (HTTPS to `huggingface.co` / `*.hf.co` only, redirects included, SHA-256
per file; `nuee-gpu::embed::allowed_host`) and the browser LLM (pinned revision and SHA-256;
`pinnedFetch` in `web/app/worker/llm.js`). An LLM endpoint comes only from the worker's own
`--llm` flag. `expose` only forwards to `127.0.0.1:<its port>` and console requests only to
`http://127.0.0.1:<its port>`. *`nuee-host::{tcp, http}`.* The relay URL a peer announces in
its `Hello` is dialled only if it is one of the coordinator's own relays or an `https` URL whose
host is not `localhost` nor an IP literal in a private, loopback, link-local, shared or
unspecified range; otherwise it is ignored. *`nuee-net::Net::peer_relay_allowed`.*
The same check applies to every relay another peer supplies to the store: a member's announced
relay, the members restored from `durable.json`, and the providers named in `Replicate` orders
(`Net::add_announced_relay_str`, `nuee-store::share::drop_refused_relays`); only links the user
opens keep any relay, so a local `nuee relay --dev` on `http://127.0.0.1` still works.

### 4.8 Hostile hosted content

Mitigations: shared sites and service responses are never rendered in the `maxgfr.github.io`
origin.

- The console shows JSON or escaped text and offers binary as a download; HTML is never
  interpreted. *`web/app/views/service.js`.*
- Static previews are rebuilt by a tokenizer of our own that drops scripts, handlers,
  `javascript:` URLs, frames, plugins, refreshes, `<base>` and navigation, then shown through
  `srcdoc` in an iframe with an empty `sandbox` (an `srcdoc` iframe also inherits the page's
  CSP). An end-to-end test checks that the preview reads none of the origin's storage.
  *`web/app/lib/inline.js`, `web/app/views/site.js`.*
- The anchor gateway sends `Content-Security-Policy: sandbox`, `X-Content-Type-Options:
  nosniff`, `Referrer-Policy: no-referrer` and `Cross-Origin-Resource-Policy: same-site`,
  refuses dot segments and encoded separators, lists no directory and caps sizes.
  `--gateway-scripts` relaxes the CSP to `sandbox allow-scripts allow-forms allow-popups`, never
  `allow-same-origin`. The gateway drops `Set-Cookie` from service responses (and never forwards
  `Cookie`), drops the visitor's `Forwarded`, `X-Forwarded-*`, `X-Real-IP` and `X-Client-IP`,
  and sets `X-Forwarded-For` to the address it saw. *`nuee-host::gateway`.*
- `nuee connect` serves on `localhost`, in the user's own browser profile, by their choice.
  Like `ssh -L`, it forwards every connection to its local port, whoever makes it: a web page
  the user visits can reach the remote service through DNS rebinding unless the service checks
  the `Host` header. Residual risk; advice: use a random port, close `connect` when done, and
  make exposed services check `Host`.

Residual risk: `site publish --on <anchor>` gives the anchor the decryption key, so its
operator can read the site; and with `--gateway-scripts` hosted scripts run on the gateway's
own origin, which must not share cookies with anything else.

The page CSP allows inline `<style>` elements (`style-src-elem 'self' 'unsafe-inline'`) so the
preview's inlined CSS applies, while `style-src-attr 'none'` keeps `style` attributes forbidden
(the sanitizer strips them). This opens no injection path: no markup reaches the page except
through DOM APIs (no `innerHTML`, Trusted Types enforced, scripts limited to `'self'`); the
preview document runs in an opaque-origin sandbox with no rights; and image, font and connect
sources stay restricted, so injected CSS cannot load from or send to arbitrary hosts.

### 4.9 XSS and clickjacking in the web app

Mitigations *(`web/index.html`, `web/app/main.js`)*: a `<meta>` CSP with `script-src 'self' 'wasm-unsafe-eval'`,
`worker-src 'self'`, `connect-src https: wss: ws://localhost:* ws://127.0.0.1:*`,
`object-src 'none'`, `base-uri 'none'`; Trusted Types with a single policy for `new Worker`; no
`innerHTML` and no `style` attributes; the consent button refuses to render inside a frame; no
CDN.

### 4.10 Secrets

Mitigations:

- Link secrets are 16 bytes from the OS random source. *`nuee-proto::id::new_secret16`.*
- Links can be read from stdin (`-`) to keep them out of shell history; the key file is created
  with `O_EXCL` and mode 0600 in a 0700 directory; nothing secret is written to browser storage.
  *`crates/nuee/src/config.rs` (`write_exclusive`, `make_private_dir`), `link_arg` in
  `crates/nuee/src/main.rs`; browser keys are per tab and in memory only (`nuee-web`).*
- The anchor's admin secret is a file created 0600 (`--admin-secret-file`); admin requests are
  proved with it over `nuee/http/0`, wrong ones get 403 and repeated failures 429.
  *`crates/nuee/src/cmd/serve.rs`.*

Residual risk: a relay token used from a browser travels as `?token=` and therefore appears in
relay access logs. Links are bearer secrets: anyone who sees one can use it.

### 4.11 Hijacked relay (`nuee relay`)

Mitigations *(`crates/nuee/src/cmd/relay.rs`, the embedded relay of `serve --anchor` in
`crates/nuee/src/cmd/serve.rs`)*: `AccessControl::on_connect` with a
constant-time token check, a connection cap, `Limits{client_rx}` rate limits, ACME certificates.
Upstream iroh does not implement `accept_conn_limit`, so the connection cap is enforced in our
access control. ACME is checked by hand on a real domain before a release; CI only runs the
plain-HTTP dev relay.

### 4.12 Non-determinism

Not an attack by itself, but it would make honest workers look like cheaters. Mitigations: no
relaxed SIMD; NaN canonicalization off in production; a golden corpus must give the same hashes
with and without NaN canonicalization, on x86 and ARM, and in Chromium, Firefox and WebKit.
*`nuee-exec::wasm` (`RunnerConfig::nan_canonicalization`), golden corpus under `tasks/golden`.*

### 4.13 Supply chain

`Cargo.lock` committed; cargo-deny and cargo-audit; GitHub Actions pinned by SHA with minimal
`permissions`; build attestations; signed commits; vendored web dependencies checked by hash.
*`deny.toml`, `.github/workflows/{ci,release,pages}.yml`, `web/vendor/transformers/VENDOR.json`.*

### 4.14 Parsing

`#![forbid(unsafe_code)]` in every crate of the main workspace; the task SDK ABI (`tasks/nuee-task`, in the separate tasks workspace) is the only unsafe code. Decoders are
covered by unit tests and by cargo-fuzz targets that require "no panic on any input" plus
round-trip stability:

| Target | Covers |
|---|---|
| `decode_msg` | `Msg`, `PinMsg`, `TcpMsg`, `HttpRequest`, `HttpResponse`, `SignedManifest`, `Manifest`, `SignedManifest::open` |
| `parse_link` | `Link::parse`, `PinLink::parse`, `to_url` round trip |
| `validate_module` | `validate_module` |
| `parse_job_toml` | `JobToml::parse`, `validate`, `from_str_checked` |

Run them with `cd fuzz && cargo +nightly fuzz run <target>`.

### 4.15 Durable storage

Threats: a store member that claims to keep a copy and does not, or keeps only part of it; a
member that serves altered bytes; a coordinator or member that tries to read stored files.

Mitigations:

- Members only ever receive age ciphertext and a pin link without the key; the coordinator
  never gets the key either. *`nuee-store::durable`, `nuee-store::crypt`.*
- Every byte fetched is verified against the BLAKE3 root while it streams (bao), so altered
  bytes are refused. *iroh-blobs, `nuee-store::share`.*
- Storage challenges every hour: a few 1 KiB ranges derived from a fresh nonce (the last chunk
  always included), fetched with a ranged request and checked against the root hash alone. A
  failed challenge, or a holder absent for 10 minutes (`NUEE_DURABLE_ABSENT_MS`), loses the copy
  and it is placed again. *`nuee-store::challenge`, `DurabilityManager::plan`.*
- Reed-Solomon sets (`--erasure k+m`): any `k` shards rebuild the file; the header is versioned,
  exact-length and bounded (`k + m <= 64`, shards `<= 64 MiB`) before anything is allocated.
  *`nuee-store::erasure`, `nuee-store::erasure::header`.*
- Members accept copies only within the quota they declared (`join --store`); copies being
  fetched and released or failed copies not yet removed by garbage collection count too.
  *`StoreMember::admit_order`, `StoreHub::open`.*
- Only the peer that asked for a blob or erasure set can release it on `nuee/pin/0`, and `List`
  shows each peer its own entries; under `--approve` pin sessions serve only peers the operator
  let in, and banned peers are refused. *`DurabilityService::{handle, allows}`,
  `DurabilityManager::forget_by`.*
- Per-requester budgets (128 tracked blobs, 64 GiB of copies), providers per blob capped at 4,
  blobs never placed dropped after 24 h, `ttl_s` enforced, members that left holding nothing
  forgotten. *`DurabilityManager::{request_by, plan, member_left, restore}`.*
- Free room counts at most 64 GiB when ranking members, ties broken by a per-blob draw.
  *`DurabilityManager::plan`.*
- A page-hosted swarm reserves room in its 256 MiB store before fetching a blob, gives it back
  when the fetch fails or the blob is untagged, refuses a peer's blob output declared above
  what a worker may produce (8 MiB) before fetching it, and keeps no copy of fetched outputs.
  *`StoreHub::reserve`, `OutputCappedHub`.*

Residual risk: a passed challenge proves the sampled ranges only. A member that drops part of a
blob is caught with a probability that grows with each challenge, not at once; full
retrievability comes from repeated challenges together with replicas or erasure coding. Repair
needs the coordinator to be running. Challenges prove possession of ranges, not independent
copies: a member can answer by relaying ranges from another copy, and one link holder with
several identities (and a large announced quota on each) can hold every replica, then drop them
together. Without `--approve`, nothing ties member identities to distinct operators. Without
`--approve`, one link holder with several free identities can also fill the coordinator's global
table of tracked blobs (8 identities x 128 blobs) and its 256 member slots for as long as those
identities stay connected, so honest members and requesters are turned away. Anchors and
coordinators that offer durable storage to a link that circulates beyond the group should run
with `--approve`.

## 5. Release rule

Before a release the code is audited and the pending changes reviewed; no release ships with an
open high or critical finding. See `SECURITY.md` for reporting.
