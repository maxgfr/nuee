# Hosting: services, sites and the gateway

Nuée can make something on your machine reachable by the people who hold a link, without
opening a port or renting a server:

- `nuee expose` puts a local TCP service (a dev server, an API, a database) behind an `X`
  link; `nuee connect` opens it on the other side as `localhost`, and the web page opens it as a
  JSON console.
- `nuee site publish` shares a static folder as an `S` link; the web page shows an isolated
  preview, `nuee site get` downloads it.
- An anchor (`nuee serve --anchor --gateway …`) serves published sites and registered services
  over plain HTTP for people who have nothing installed.

Hosted content is treated as hostile everywhere. None of it ever runs in the
`maxgfr.github.io` origin.

## Expose and connect

```console
$ nuee expose 8000
https://maxgfr.github.io/nuee/#/X/AHW6NJ…
fingerprint 🚂 🌴 🌵 🏏 🌲 🐔  (compare with the screen of whoever joins)
exposing 127.0.0.1:8000 until Ctrl-C
```

On another machine:

```console
$ nuee connect --port 8001 -     # link on stdin
open http://localhost:8001
tunnelling to the exposed service until Ctrl-C
$ curl http://localhost:8001/
```

`--port` is optional (a free port is picked). Each TCP connection to the local port becomes one
QUIC stream on `nuee/tcp/0`, end to end encrypted, through the relay when no direct path exists.

What `expose` will and will not do:

- It only ever connects to `127.0.0.1:<port>`. A client cannot choose another host or port, so
  the tunnel cannot be turned into a proxy into your network.
- If nothing listens yet, it says `nothing answers on 127.0.0.1:8000 yet (…); clients will be
  refused until it does` and keeps running.
- Every client proves it holds the link secret (HMAC over both keys). `--approve` also asks you
  on the terminal before each new client gets in.
- `--no-http` turns off the console requests below and keeps raw TCP tunnels only.

## The JSON console

Opening an `X` link in the web page gives a request console for HTTP services: method, path,
headers, body, and the response. Requests travel on `nuee/http/0`, one stream per request,
bounded to 8 MiB bodies and 64 headers, and reach `http://127.0.0.1:<port>` on
the exposing machine. Each request starts with a small hello carrying the proof; the host reads
the request only after checking it. Connections that prove nothing are closed after 5 s.
`X-Forwarded-For` from a console client is passed on as sent (only link holders get this
far).

The console never interprets what comes back. JSON is pretty-printed, other text is shown as
text (cut at 1 MiB for display), binary is offered as a download. An HTML page from the service
is shown as source, not rendered. For a dynamic web app, use `nuee connect` and a normal
browser tab on `localhost`, or register the service on an anchor's gateway.

## Static sites

```console
$ nuee site publish ./public
https://maxgfr.github.io/nuee/#/S/AEAWH5…
publishing 2777 bytes (encrypted) until Ctrl-C

$ nuee site get -o got -          # on another machine, link on stdin
got/public/index.html
✓ verified (BLAKE3)
```

The folder (it must contain an `index.html`) is archived and age-encrypted like a file share
(see [STORAGE.md](STORAGE.md)); the link carries the key.

### The isolated preview

Opening an `S` link in the web page downloads and checks the site, then shows a static preview
labelled as isolated:

- The document is rebuilt by `web/app/lib/inline.js` from a small tokenizer of its own: markup,
  text, inlined CSS and inlined images stay; scripts, event handlers, `javascript:` URLs,
  frames, plugins, refreshes, `<base>`, form targets and navigation are removed.
- It is shown through `srcdoc` in an `<iframe sandbox>` with an empty sandbox: no scripts, no
  same origin, no forms, no popups, no top navigation. An `srcdoc` frame also inherits the
  page's own CSP.
- The preview reads nothing from the page's storage (an end-to-end test checks this).

So a site with JavaScript previews as its static shell. Visitors who need the real thing
download it (`nuee site get`) or go through an anchor's gateway.

## Anchor gateway

An anchor is an always-on `nuee serve --anchor`. Its setup, Docker and systemd files are in
[ANCHOR.md](ANCHOR.md); here is the hosting side, as run locally with a dev relay:

```console
$ nuee serve --anchor --relay-dev --relay-bind 127.0.0.1:0 --gateway 127.0.0.1:18780
anchor link (keep it secret: it carries the admin secret):
https://maxgfr.github.io/nuee/#/X/AECEFR…
swarm link: https://maxgfr.github.io/nuee/#/J/AECEFR…
fingerprint 🍒 🍀 🦥 🚀 🥐 🌭
gateway: http://127.0.0.1:18780
relay: http://127.0.0.1:61848

$ nuee --relay http://127.0.0.1:61848 site publish ./public --on -    # anchor link on stdin
served by the anchor's gateway at /s/977841fe…/

$ curl -i http://127.0.0.1:18780/s/977841fe…/
HTTP/1.1 200 OK
content-type: text/html; charset=utf-8
cache-control: no-cache
content-security-policy: sandbox
x-content-type-options: nosniff
referrer-policy: no-referrer
cross-origin-resource-policy: same-site
```

In production you would use `--relay-hostname` and `--acme-contact` instead of `--relay-dev`;
that path needs a real domain and was not run here (see ANCHOR.md, "TLS and ACME").

`site publish --on` hands the anchor the full `S` link, **decryption key included**: the
anchor operator can read the site. It keeps the ciphertext and serves the plaintext at
`/s/<id>/`. An exposed service is registered with `POST /services {id, link}` on the admin
API and proxied at `/x/<id>/`.

### Gateway CSP

Every gateway response carries `Content-Security-Policy: sandbox`: the browser treats the page
as an opaque origin and runs no script. With `--gateway-scripts` the policy becomes
`sandbox allow-scripts allow-forms allow-popups`, still without `allow-same-origin`, for
dynamic sites you trust: their scripts then run on your gateway's address. Serve the gateway
on a host name of its own, never on a domain that holds cookies for anything else. The gateway
never relays `Set-Cookie` from a service nor forwards `Cookie` to it; it replaces the visitor's
`Forwarded`, `X-Forwarded-*` and `X-Real-IP` headers with `X-Forwarded-For: <client address>`.
A request has 10 s to send its head and 30 s more for its body (408 otherwise). Paths with
`.`, `..`, empty segments or encoded separators are refused, directories are never listed, and
sites are capped at 256 MiB.

## Security notes

- An `X` link is a capability to reach your service as if from `localhost`. Anything the
  service lets a local user do, link holders can do. Expose development servers, not admin
  panels.
- `expose` prints the fingerprint; ask the people connecting to compare it.
- `connect` works like `ssh -L`: it forwards any connection made to its local port, whoever
  makes it. A web page you visit while it runs can reach the remote service through DNS
  rebinding unless the service checks the `Host` header. Use a random port, close `connect` when
  you are done, and make the services you expose check `Host`.
- Responses shown in the console or the preview never execute in the app origin; the app's own
  CSP (`script-src 'self' 'wasm-unsafe-eval'`, Trusted Types, no `innerHTML`) is a second line.
- The anchor operator sees sites handed to it with `--on`. Do not hand it what it should not
  read.

## Troubleshooting

- **`connect` prints `open http://localhost:…` but connections drop at once.** Nothing listens
  on the exposed port; `expose` says so when it starts, and clients are refused until it does.
- **The console shows HTML source instead of a page.** That is by design; use `connect`.
- **A previewed site looks broken.** The preview has no scripts and only inlined resources.
  Download it or use a gateway.
- **A gateway path that worked locally is refused.** Paths with `.` or `..` segments, empty
  segments or encoded separators are rejected; directories are never listed.
