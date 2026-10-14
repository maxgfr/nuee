# Nuée

Share a link, get a swarm. Nuée turns the laptops and phones of a group that trusts each other
into a private, short-lived compute and file cluster. No server of ours, no account, no payment.

```sh
brew install maxgfr/tap/nuee
nuee run demo:mandelbrot
# open the printed link (or scan the QR code) on a phone: the tab starts computing
```

The web page is https://maxgfr.github.io/nuee/. It can join a swarm, run one, share a file or
open any Nuée link, with nothing to install.

## What works in v0.1

- **Verified CPU jobs in WebAssembly.** A coordinator (the CLI or the web page) splits a job into
  tasks; CLI workers (`nuee join`) and browser tabs run them. The same module gives the same
  output bytes in wasmtime, Chromium, Firefox and Safari (WebKit); the end-to-end tests check
  this on a golden corpus. Results are checked by recomputation (see
  [Security model](#security-model)).
- **Demos.** `demo:mandelbrot` (a fractal, written out as an image), `demo:mandelbrot-gpu` (the
  same on GPUs), `demo:skycmd` (closed-loop evaluation of the skycmd s3-pico flight model,
  downloaded and checked against its BLAKE3 hash), `demo:skycmd-dagger` (oracle-labelled
  DAgger records from the same model, merged into one JSONL file) and `demo:train` (a small
  digit recognizer trained with DiLoCo).
- **Your own jobs.** A job TOML file, or any `.wasm` module that follows the task ABI plus a
  directory of inputs (one file per task). JSONL inputs are cut into shards and merged back;
  `[wasm] assets` hands files (a model, a dataset) to every task.
- **GPU tasks and models.** WGSL compute shaders (wgpu natively, WebGPU in browsers),
  sentence embeddings (`nuee embed`), and LLM option scoring through a local endpoint or a
  browser model, each verified with a measured tolerance. See [docs/GPU.md](docs/GPU.md).
- **Native commands, opt-in.** `exec` jobs run a command from an exact `repo@commit` on
  machines whose operator approved that exact command line and environment (`join --allow-exec`
  asks on a terminal, `--exec-approvals` lists them ahead), for MLX or CUDA work; `nuee sweep` runs one variant per
  task and ranks them. See [docs/SKYCMD.md](docs/SKYCMD.md).
- **Training.** `nuee train` runs DiLoCo with WebAssembly trainers (recomputed bit for bit,
  browsers included) or native trainers (robust aggregation). See
  [docs/TRAINING.md](docs/TRAINING.md).
- **Files.** `nuee share` encrypts files or folders with [age](https://age-encryption.org)
  before they leave the machine and prints a link; `nuee get` or the web page downloads,
  decrypts and checks them against BLAKE3. `nuee pin` keeps a ciphertext-only copy and serves
  it, so the original sender can go offline.
- **Durable storage.** `share --replicas N --on <swarm>` keeps copies on members that offered
  space (`join --store`), repairs them when a holder disappears and checks holders with storage
  challenges; `--erasure 4+2` keeps a Reed-Solomon set instead. Only the peer that asked for
  a blob can release it; challenges prove possession of ranges, not independent copies. See
  [docs/STORAGE.md](docs/STORAGE.md).
- **Hosting.** `nuee expose` / `nuee connect` tunnel a local service through a link, the web
  page opens it as a JSON console, and `nuee site publish` shares a static site with an
  isolated preview. See [docs/HOSTING.md](docs/HOSTING.md).
- **An anchor.** `nuee serve --anchor` is an always-on coordinator, relay, store member and
  HTTP gateway, with Docker and systemd files. See [docs/ANCHOR.md](docs/ANCHOR.md).
- **Your own relay.** `nuee relay` runs an iroh relay with Let's Encrypt, an optional bearer
  token, a connection cap and a per-client rate limit.
- **A direct lane between browsers.** When a presenter running the web page turns on
  direct connections (WebRTC), browser workers that accept send their results straight to the
  presenter's page over a data channel, signalled through iroh, with iroh as the fallback. No TURN server; STUN is
  configurable.
- **Interrupted runs resume.** Outputs already in the output directory are kept; run the same
  command again.

## Quick start

All verbs print human text on stderr. `--json` prints NDJSON events on stdout instead.
The final `done` event of `nuee run` carries `peers: [{id, browser, done, verified, banned}]`:
per connected peer, `done` counts the accepted outputs it delivered first and `verified` those
of its results another execution agreed with. `by` in `task`, `verified` and `results.json` names only the execution that arrived
first, so a slower peer whose results agreed shows up only in these counters.

### Run a job

```console
$ nuee run demo:mandelbrot
https://maxgfr.github.io/nuee/#/J/AAQ...
fingerprint 🚑 🍋 🚂 🌾 🚲 🧄
peer joined: 5b1c…  (nuee-web/0.1.0)
verified 212 · 3 peers · 38.5/s
done: 256 tasks, results in nuee-out/mandelbrot
image: nuee-out/mandelbrot/mandelbrot.ppm
```

The link and a QR code appear only once the relay is reachable (10 seconds at most, otherwise
exit code 3). Useful options: `--verify full|sample|none` (default `sample`), `--sample 0.10`,
`--canary 0.03`, `--approve` (admit each peer by hand), `--wait-peers N`, `--local-slots N`,
`--deadline 30s`, `--out DIR`, `--no-resume`, `--qr-svg FILE`.

A custom module:

```sh
nuee run my-task.wasm --inputs inputs/   # one file per task, sorted by name
```

### Join as a worker

```console
$ nuee join -          # paste the link on stdin, it stays out of your shell history
coordinator fingerprint 🚑 🍋 🚂 🌾 🚲 🧄  (compare with the coordinator's screen)
joined with 5 slots
job 1: mandelbrot
task 17 done in 41 ms (1 so far)
left the swarm after 87 tasks
```

Compare the six emoji with the coordinator's screen before leaving it running. `--slots N` sets
how many tasks run at once (default: half the cores).

### Share and get files

```console
$ nuee share report.pdf photos/
https://maxgfr.github.io/nuee/#/F/AAQ...
serving 48213990 bytes (encrypted) until Ctrl-C

$ nuee get -o downloads/ -     # link on stdin
```

`nuee get` refuses to overwrite existing files unless given `--force`, and refuses archive
entries that would land outside the output directory.

### Pin and serve

```sh
nuee pin -            # keep a ciphertext copy of a file or pin link
nuee pin --list
nuee pin --rm <HASH>
nuee serve            # serve every pinned item until Ctrl-C
```

### Run a relay

```sh
nuee relay --hostname relay.example.org --acme-contact you@example.org --token-file token.txt
nuee relay --dev --bind 127.0.0.1:3340      # plain HTTP, for local use and tests
nuee --relay https://relay.example.org run demo:mandelbrot
```

### Check a machine

```console
$ nuee selftest       # offline: no key, no network, no files
✓ wasm golden: 3 golden outputs match
✓ demo modules: embedded: mandelbrot, toy-trainer, skycmd-arena, optdigits
✓ links: job, file and pin links round-trip
✓ manifest: manifest signatures and admission proofs hold
✓ encryption: age encryption round-trips and detects tampering
✓ swarm: in-memory swarm finished 6 tasks
selftest ok

$ nuee doctor         # identity, data directory, relay, wasm, GPU adapter, sandbox, clock
$ nuee id             # node id and its fingerprint
$ nuee completions zsh
```

Global options: `--relay URL`, `--relay-only` (no direct connections), `--web-base URL`,
`--config-dir DIR`, `--data-dir DIR`, `--ephemeral` (throwaway identity, writes nothing),
`-v`, `-q`, `--json`.

### More

| Guide | Covers |
|---|---|
| [docs/SKYCMD.md](docs/SKYCMD.md) | skycmd evaluation, teacher labelling with `exec`, sweeps, DiLoCo on Macs |
| [docs/TRAINING.md](docs/TRAINING.md) | `nuee train`, DiLoCo, WebAssembly and native trainers |
| [docs/GPU.md](docs/GPU.md) | WGSL tasks, WebGPU support, embeddings, LLM scoring, tolerances |
| [docs/STORAGE.md](docs/STORAGE.md) | share, get, pin, replicas, repair, challenges, Reed-Solomon |
| [docs/HOSTING.md](docs/HOSTING.md) | expose, connect, the console, sites, the gateway |
| [docs/ANCHOR.md](docs/ANCHOR.md) | running an always-on anchor |

`--exec-mirror URL=PATH` (on `run`, `join` and `sweep`, next to `--allow-exec`) fetches the
allowlisted repository from a local clone instead of the network. It is meant for operators of
air-gapped machines and for tests; the commit is checked the same way.

## Security model

- **The link is the capability.** A job link carries the coordinator's public key, its relay
  and a 16-byte secret. Anyone holding the link can join; treat it like a password. Links can
  be read from stdin (`-`) so they do not end up in shell history.
- **Admission** is an HMAC-SHA256 proof over both peers' keys, compared in constant time.
  Connections are QUIC with mutual TLS 1.3 through iroh, end to end.
- **Fingerprints.** Six emoji (48 bits) derived only from the coordinator's key. Workers show
  them so people can compare with the coordinator's screen.
- **Signed manifests and results.** The coordinator signs each job manifest; workers sign each
  result. Signatures use domain separation.
- **Verification by recomputation.** In `sample` mode the coordinator reruns a share of the
  results itself and mixes in known-answer canary tasks (they catch naive cheaters; identities
  sharing state can recognise a repeated input, so the hidden recompute is the guarantee). In
  `full` mode every task also runs on two peers, but two agreeing peers may be one attacker:
  the coordinator still recomputes the same sampled share and sends canaries. A mismatch is settled by the coordinator's own run; only
  then is the peer banned and all its unverified results redone. This holds with
  `--local-slots 0` too: the coordinator then takes no tasks but still recomputes. Where it
  cannot run the kind at all (see [docs/THREAT-MODEL.md](docs/THREAT-MODEL.md)), only
  agreement between two peers is left, and `nuee run` says so. With a single peer able to run
  that kind, tasks then wait rather than being trusted, and `nuee run` says it needs a second
  such device (or `--verify none`).
- **Worker-side caps.** Workers enforce their own limits on memory, module size, inputs,
  outputs and time, whatever the manifest says. Modules must have zero imports and a declared
  memory maximum; Wasm features outside a fixed list (relaxed SIMD, threads and others) are
  refused.
- **Files** are encrypted with age before leaving the machine and checked with BLAKE3 on
  arrival. Pinners and store members only ever hold ciphertext.
- **Tolerances where floats differ.** GPU, embedding and LLM outputs are compared with
  tolerances measured across backends, so a dishonest worker has that much room: up to 5 % of
  a GPU fractal tile can be wrong unnoticed. Native trainers are not recomputed at all; robust
  aggregation bounds what one worker can do. Details in [docs/GPU.md](docs/GPU.md) and
  [docs/TRAINING.md](docs/TRAINING.md).
- **`exec` is off by default.** A worker runs native code only for a command its operator
  approved exactly (repository, commit, argv and environment), in a throwaway checkout, with a
  cleaned environment, time limits and `sandbox-exec` or `bwrap` when present. Approving a
  command means trusting the commit's code.
- **Hosted content never runs in the app's origin.** The console shows JSON, the site preview
  is an inert sandboxed frame, the gateway sends `Content-Security-Policy: sandbox`.

Details: [SECURITY.md](SECURITY.md) (reporting, scope) and
[docs/THREAT-MODEL.md](docs/THREAT-MODEL.md). The wire format is in
[docs/PROTOCOL.md](docs/PROTOCOL.md).

## Limits

- **Relay operators see browser IP addresses.** Traffic through a relay is end-to-end
  encrypted, but the relay sees who connects. Other peers do not see a browser's IP, unless the
  presenter turned WebRTC on: then the presenter and each participant who accepts see each
  other's IP address. Both the switch and the join card say so.
- **The n0 public relays are for development and hobby use.** For anything else, run your own
  with `nuee relay`, or use n0's paid relays.
- **Browser keys are ephemeral.** One person holding the link can open many tabs, so "two
  different peers agree" proves nothing. Real assurance comes from the coordinator's own
  recomputation, or from `--approve`.
- **Mobile tabs stop in the background.** Keep the screen on while contributing.
- **No Windows binary.** Windows machines join through the browser.
- Nuée is for groups that trust each other enough to share a link. It is not an open market.

## How it compares

| Project | What it is for | Difference with Nuée |
|---|---|---|
| Akash | A marketplace to rent cloud compute, paid in tokens | Nuée has no market, no payment, no provider to rent from |
| Salad | Paid rental of consumer GPUs to companies | Nuée runs among people who know each other, unpaid |
| Mesh LLM | Pooling machines to serve a large language model, built on iroh | Nuée runs verified batch jobs and shares files; it does not split a model across peers |
| Buzz (Block) | A peer-to-peer application built on iroh | Nuée is a compute and file swarm joined from a link or a browser tab |
| BOINC | Volunteer computing for research projects, run from a project server | Nuée needs no server: the coordinator is a laptop or a web page, joined from a link |

## Exit codes

| Code | Meaning |
|---|---|
| 0 | ok |
| 1 | error |
| 2 | usage (bad arguments, truncated or wrong link) |
| 3 | network or relay unreachable |
| 4 | verification mismatch or banned |
| 5 | admission refused |
| 6 | integrity failure (hash or decryption) |
| 7 | cancelled or interrupted |
| 8 | swarm ended by the coordinator |

## Environment

| Variable | Effect |
|---|---|
| `NUEE_CONFIG_DIR` | identity key directory (default `~/.config/nuee`) |
| `NUEE_DATA_DIR` | data directory (default `~/.local/share/nuee`) |
| `NUEE_ONLINE_TIMEOUT_MS` | how long to wait for the relay (default 10000) |
| `NUEE_DURABLE_ABSENT_MS` | coordinators: replace a stored copy whose holder has been gone this long (default 600000); for drills and tests |
| `NO_COLOR` | disable colors |

## Building from source

Needs the Rust toolchain pinned in `rust-toolchain.toml` (with the `wasm32-unknown-unknown`
target and the `llvm-tools` component), wasm-pack, wasm-opt and Node.

```sh
cargo build --release -p nuee     # the CLI
scripts/build-tasks.sh            # task modules into web/tasks/ and target/tasks/, golden checks
scripts/build-web.sh              # browser package into web/pkg/<version>/
scripts/e2e.sh                    # Playwright on Chromium, Firefox and WebKit, local relay only
scripts/check.sh                  # the full gate CI runs
```

Every build embeds the mandelbrot and toy-trainer modules and the optdigits dataset from
`tasks/golden/` (`scripts/build-tasks.sh` checks that a rebuild reproduces them byte for byte).
`demo:skycmd` and `demo:skycmd-dagger` also need the skycmd-arena module: release builds embed
it with `cargo build --release -p nuee --features embed-tasks` after `scripts/build-tasks.sh`;
without the feature, nuee looks for `target/tasks/skycmd-arena.wasm` in the checkout.

`scripts/check.sh` runs, in order: `cargo fmt --check`; clippy with `-D warnings` on the
workspace and on the wasm32 crates; the task build; `cargo test`; `cargo deny` and
`cargo audit`; clippy on `tasks/`; the web build; `node --test web/test/`; the end-to-end
tests. It stops at the first failure and names the stage.

## Layout

```
crates/nuee         the CLI
crates/nuee-proto   wire types, links, manifests, limits, module validation
crates/nuee-core    coordinator and worker state machines (no I/O)
crates/nuee-net     iroh endpoints, admission, the network driver
crates/nuee-store   blobs, encrypted file sharing, pinning, durable storage, Reed-Solomon
crates/nuee-exec    wasmtime task runner and the opt-in native runner
crates/nuee-gpu     WGSL, embeddings, LLM scoring and their tolerance comparators
crates/nuee-train   DiLoCo: outer step, delta codec, robust aggregation
crates/nuee-host    TCP tunnel, HTTP console requests, the anchor gateway
crates/nuee-web     the browser build (wasm-bindgen)
tasks/nuee-task     SDK for writing task modules
tasks/              task modules (mandelbrot, skycmd-arena, toy-trainer) and their golden copies
web/                the web page (no build step besides web/pkg)
e2e/                Playwright tests
fuzz/               cargo-fuzz targets
docs/               usage guides, protocol and threat model
deploy/             anchor Dockerfile, Compose file and systemd unit
examples/skycmd/    job files for skycmd
```

## License

MIT OR Apache-2.0, at your option. See [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE).
