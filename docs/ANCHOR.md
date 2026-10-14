# Running an anchor

An anchor is `nuee` on a machine that stays on. Nothing in Nuée needs one: every verb works
with the CLI, a browser tab and the public relays. An anchor adds four things:

- a **persistent coordinator**: `nuee run … --anchor <link>` hands it a job and returns at
  once; the job keeps running, survives restarts of the anchor, and its results wait for you;
- **its own relay** (iroh relay with Let's Encrypt, a token, a connection cap and per-client
  rate limits), so your swarms do not depend on the public relays;
- **durable storage**: a store member in the anchor's swarm plus the coordinator side that
  keeps the requested number of copies of shared files (`nuee share --replicas N --on <J>`);
- an **HTTP gateway** for sites pinned on the anchor and services it may reach.

## When you need one, and when you do not

You do not need an anchor to run a job from your laptop with friends joining, to share a
file, or to expose a service while your machine is on. You want one when a job must outlive
the laptop that started it, when a team wants a stable swarm link and its own relay, when
shared files must stay available while their owners are offline, or when a site should be
reachable over plain HTTPS without the recipient running anything.

## What it prints

```
nuee serve --anchor --relay-hostname anchor.example.org --acme-contact ops@example.org \
  --store 50GiB --gateway 0.0.0.0:8080
```

prints:

- the **anchor link** (`…#/X/…`): the anchor id, its relay and the **admin secret**. Treat it
  like a password; whoever holds it can submit jobs and publish sites. The secret lives in
  `<data dir>/anchor/admin.secret` (or `--admin-secret-file`), created 0600;
- the **swarm link** (`…#/J/…`): what workers join, stable across restarts;
- the fingerprint, the gateway address and the relay URL.

With `--json`, all of this is one `anchor` event.

## Using it

```
nuee run demo:mandelbrot --anchor <anchor link>             # returns with the swarm link
nuee run demo:mandelbrot --anchor - --follow --out results  # link on stdin; wait, download
nuee join <swarm link>                                      # workers
nuee site publish ./public --on <anchor link>               # served at /s/<id>/
```

`run --anchor` uploads the job description (manifest, task input hashes, module and asset
hashes) over `nuee/http/0`; the anchor fetches the blobs from the submitter over iroh-blobs,
checks every hash, stores everything under its data directory and starts the job. The same
description submitted again (from anywhere) attaches to the existing job, which is how
`--follow` from a second machine works. `--follow` polls the job and, once it is done,
downloads the results bundle (an encrypted `F` link served by the anchor) into `--out`.

The anchor runs one task at a time itself (`--local-slots`, default 1). With
`--local-slots 0` it takes no tasks but still recomputes the verification samples and settles
mismatches of the kinds it can run (wasm); other kinds fall back to agreement between two
peers.

Admin endpoints (all over `nuee/http/0`, the request proof made with the admin secret):
`GET /status`, `POST /jobs`, `GET /jobs`, `GET /jobs/<id>`, `GET /jobs/<id>/results`,
`POST /sites {link}`, `DELETE /sites/<id>`, `POST /services {id, link}`,
`DELETE /services/<id>`. A wrong secret gets 403, repeated failures 429.

## Walkthrough: Hetzner Cloud

1. Create a **CX22** server (2 vCPU, 4 GB) with **Debian 12** and your SSH key.
2. DNS: an `A` record (and `AAAA` for IPv6) `anchor.example.org` pointing at the server.
   Let's Encrypt needs it to resolve before the first start.
3. Firewall (Hetzner Cloud firewall or nftables), inbound:
   - TCP 80 (ACME HTTP challenge, relay redirects),
   - TCP 443 (relay over HTTPS),
   - UDP 7842 (relay QUIC address discovery),
   - TCP 8080 (gateway; put a TLS reverse proxy in front if you serve it publicly),
   - SSH from your own address only.
   Peers can also reach the anchor directly over UDP; when the firewall blocks that, traffic
   goes through the relay, which still works.
4. Install with Docker Compose or systemd (below).
5. Read the anchor link from the logs and keep it somewhere safe.

### Docker Compose

```
git clone https://github.com/maxgfr/nuee && cd nuee
printf 'NUEE_HOSTNAME=anchor.example.org\nNUEE_ACME_CONTACT=ops@example.org\n' > deploy/.env
docker compose -f deploy/compose.yml --env-file deploy/.env up -d
docker compose -f deploy/compose.yml logs anchor | grep '#/X/'
```

The image is `ghcr.io/maxgfr/nuee` (distroless, non-root, read-only root file system, every
capability dropped except `NET_BIND_SERVICE`, `no-new-privileges`). State is in two named
volumes: `nuee-data` (`/data`: blobs, jobs, results, `durable.json`, ACME cache) and
`nuee-config` (`/config`: the identity key).

### systemd

```
install -m 0755 nuee /usr/local/bin/nuee
install -m 0644 deploy/nuee.service /etc/systemd/system/nuee.service
install -d -m 0755 /etc/nuee
printf 'NUEE_HOSTNAME=anchor.example.org\nNUEE_ACME_CONTACT=ops@example.org\n' > /etc/nuee/anchor.env
systemctl daemon-reload && systemctl enable --now nuee
journalctl -u nuee | grep '#/X/'
```

The unit runs under a dynamic user with the state in `/var/lib/nuee` (identity key under
`/var/lib/nuee/config`), a read-only system, no home, private `/tmp` and devices, a
system-call filter, and only the capability to bind ports 80 and 443. It deliberately does not
set `MemoryDenyWriteExecute`: wasmtime compiles task modules to native code at run time.
`systemd-analyze security nuee` rates it about 1.8.

## TLS and ACME

`--relay-hostname` and `--acme-contact` make the embedded relay fetch and renew a Let's
Encrypt certificate (HTTP-01 on port 80, cached under `<data dir>/acme`). This path is
checked by hand on a real domain before a release; **it is not covered by CI**, which only
runs the plain-HTTP `--relay-dev` relay on 127.0.0.1.

## Relay tokens

`--relay-token-file <path>` makes the embedded relay accept only clients presenting that
bearer token (compared in constant time; the anchor's own endpoints are exempt).
`--relay-max-conns` caps concurrent clients and `--relay-rate` limits each client's incoming
bytes per second. The relay accepts the token as an `Authorization: Bearer` header or as a
`token` query parameter of the relay URL. Browsers cannot set headers on the relay
WebSocket, so their links carry `?token=` in the URL, which means **the token appears in the
relay's access logs** and in browser history. Rotate it by changing the file and restarting.

## Enterprise mode (allowlist)

`--allow <endpoint id>` (repeatable) or `--allow-file <path>` (one id per line, `#`
comments) restricts the anchor's swarm: a peer is admitted only if it proves the link secret
**and** is on the list. Others receive `Bye{Rejected}` and `nuee join` exits with code 5.
Collect ids with `nuee id` on each machine. `--approve` additionally asks on the terminal
for every new peer.

## Sites and services on the gateway

`nuee site publish <dir> --on <anchor link>` shares the site and hands the anchor its full
`S` link, decryption key included: **the anchor operator then sees the site's plaintext**.
The anchor keeps the ciphertext and serves the files at `/s/<id>/`. A service exposed with
`nuee expose` can be registered with `POST /services {id, link}` and is proxied at
`/x/<id>/` over `nuee/http/0`.

Hosted content is treated as hostile. Every response carries
`Content-Security-Policy: sandbox` (no scripts, opaque origin), `X-Content-Type-Options:
nosniff`, `Referrer-Policy: no-referrer` and `Cross-Origin-Resource-Policy: same-site`;
paths with `.`, `..`, empty segments or encoded separators are refused, directories are never
listed, and sizes are capped (256 MiB per site, 8 MiB request bodies). `--gateway-scripts`
switches the policy to `sandbox allow-scripts allow-forms allow-popups` for dynamic sites,
still without `allow-same-origin`: scripts then run on your gateway's address, a risk you take
for the sites you host. Serve the gateway on its own host name, never on a domain that holds
cookies for anything else.

## Backups

Stop the anchor (`docker compose stop` or `systemctl stop nuee`; it shuts down cleanly on
SIGTERM) and copy the data and config directories:

- `/data` or `/var/lib/nuee`: blobs, `anchor/jobs/*.json`, `anchor/sites.json`,
  `anchor/services.json`, `anchor/admin.secret`, `anchor/swarm.secret`, `durable.json`,
  the ACME cache;
- `/config` or `/var/lib/nuee/config`: the identity key. Losing it changes the anchor's id,
  so every link it printed stops working.

Backups hold secrets (admin and swarm secrets, result keys): encrypt them.

## Upgrades

Pull and restart (`docker compose pull && docker compose up -d`, or replace
`/usr/local/bin/nuee` and `systemctl restart nuee`). Running jobs resume from the outputs
already stored; links stay valid as long as the identity key and the secrets are kept. Read
the release notes for protocol changes: peers and the web page must speak the same protocol
version as the anchor.
