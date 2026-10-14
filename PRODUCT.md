# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Users

A group that already trusts itself: a team, a meetup room, a class, a lab. Two roles share one page:

- **The presenter** launches a job (a fractal, a skycmd evaluation, embeddings, a training run, their own WebAssembly module) from the CLI or from the page, then projects the link and its QR code on a screen and watches the swarm work.
- **The participants** open the link, most often on a phone held in one hand in the same room, read who is asking and what will run, and contribute with one tap. Others join from a laptop, or with `nuee join` in a terminal.

The same people also hand each other files (encrypted links), open an exposed service's console, or preview a published site.

## Product Purpose

Nuée turns a link into a private, short-lived cloud made of the machines already in the room: browser tabs and CLI nodes compute tasks, store files, train small models and host services, with no server of ours, no account and no payment. Success: the room computes together within a minute of the link appearing, every result is checked, and nobody had to install anything to help.

## Positioning

The link is the cluster. A link carries the coordinator's key and a secret, so opening it is the admission; WebAssembly results are verified bit for bit between wasmtime, Chromium, Firefox and Safari, and the presenter's own machine recomputes a sample. It is batch work for people who know each other, not a marketplace of anonymous GPUs and not a distributed chat model.

## Operating Context

- A talk or workshop: the presenter's laptop on a projector, the audience on phones and laptops on the same Wi-Fi or on mobile data.
- A team or lab: a few Macs and Linux boxes joined with the CLI, sometimes an always-on anchor; browsers join for evaluations and demos.
- Mobile tabs stop when backgrounded; the page keeps the screen awake while contributing and says when work paused.
- Links travel by QR code, chat or copy-paste, and can arrive truncated.

## Capabilities and Constraints

- Static site on GitHub Pages at https://maxgfr.github.io/nuee/, an origin shared with other apps: nothing hosted ever runs in it, nothing secret is stored in browser storage.
- Vanilla HTML/CSS/JS ES modules, no bundler, no framework, no CDN; strict CSP and Trusted Types; no innerHTML.
- Networking runs in a dedicated Web Worker (iroh over an encrypted relay); tasks run in a pool of Web Workers; WebGPU when present; WebRTC direct lanes only when the presenter turns them on (IP addresses then become visible to each other).
- Views: home (four doors: Launch, Join, Share a file, Open a link), run (presenter: catalogue, verification mode, approval, WebRTC toggle, link + QR + six-emoji fingerprint, fullscreen presentation, live progress, results), join (consent card, contribute, pause/stop, thanks), file, share, service console, site preview, train together, about.
- Errors must say what to do: relay unreachable, swarm ended, link truncated, unsupported browser (no WebAssembly SIMD, iOS Lockdown Mode).
- Undecided: none recorded beyond the visual system, which the design pass establishes.

## Brand Commitments

- Name: Nuée (French for a swarm, a flock); the CLI is `nuee`.
- Voice: calm, clean, reassuring. Plain words, exact numbers, no hype.
- Identity: a murmuration drawn on a canvas, one bird per participant, frozen when `prefers-reduced-motion` is set.
- Bilingual French and English, chosen from the browser language, switchable.

## Evidence on Hand

- Real, working demos: Mandelbrot fractal (CPU and WebGPU), skycmd game evaluations (s3-pico model), a toy trainer that reaches about 89 % on handwritten digits when a room trains it together, embeddings, file sharing.
- Measured facts the page may state: results identical across wasmtime and the three browser engines on the golden corpus; per-job counts of verified tasks, mismatches and bans.
- No testimonials, customers, usage statistics or benchmarks beyond those measurements; none may be invented.

## Product Principles

1. Consent before compute: say who asks, what runs and how much, then one clear action.
2. Show the work: progress, verification and each contribution are visible as they happen.
3. Trust is earned by checks, not claims: every number shown is measured.
4. Nothing to install to help; the CLI is there for those who want more.
5. Quiet by default: the page never shouts, never gamifies, and never hides a risk (relay operators see IP addresses; WebRTC exposes them to each other).

## Accessibility & Inclusion

WCAG 2.2 AA: contrast in light and dark schemes, visible focus, full keyboard use, `aria-live` progress, 44 px targets, reduced motion honoured, French and English throughout.
