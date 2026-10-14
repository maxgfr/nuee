# Security policy

## Supported versions

Only the latest release receives security fixes. Older releases are not patched; upgrade to the
latest one.

## Reporting a vulnerability

Use GitHub private vulnerability reporting on
[maxgfr/nuee](https://github.com/maxgfr/nuee/security/advisories/new). Do not open a public issue
for a vulnerability.

Please include the affected version or commit, what an attacker needs (a link? a running swarm?
a position on the network?), steps or a proof of concept, and the impact you observed.

You can expect an acknowledgement within 72 hours. We will keep you informed while a fix is
prepared, agree on a disclosure date with you, and credit you in the advisory unless you prefer
otherwise.

## Scope

In scope:

- the `nuee` command-line tool and every crate in this repository;
- the web application served from `https://maxgfr.github.io/nuee/` (sources in `web/` and
  `crates/nuee-web`);
- the wire protocol (`docs/PROTOCOL.md`): admission, signatures, links, manifests, bounded
  decoding;
- task isolation: a task module or an `exec` job escaping the limits described in
  `docs/THREAT-MODEL.md`;
- the `nuee relay` server shipped with the tool.

Out of scope, or accepted by design (see `docs/THREAT-MODEL.md` for the reasoning):

- **Relay operators see IP addresses.** Traffic through an iroh relay is end-to-end encrypted,
  but the relay sees which endpoints connect and, for browsers, their IP address. The n0 public
  relays are meant for development and hobby use; run your own relay (`nuee relay`) for anything
  else.
- **Anyone holding a link is a member.** A job, file, site or service link is a bearer secret.
  Sharing it grants what it says.
- **A coordinator chooses what its workers run.** Joining a swarm means running that
  coordinator's tasks inside the WebAssembly sandbox and limits. A malicious task that stays
  inside those limits (burning its CPU budget, returning wrong answers) is not a vulnerability.
- **`exec` runs native code.** It is disabled by default; a worker that enables it for a
  repository and revision trusts that code with its machine, within the isolation listed in the
  threat model.
- **WebRTC exposes IP addresses** to other swarm members when the coordinator enables it.
- Denial of service needing traffic volume comparable to the target's own bandwidth.
- Vulnerabilities in dependencies that are not reachable from Nuée; report those upstream.

## Release rule

Before each release the code goes through a security audit and a review of the pending changes.
No release ships while a finding rated high or critical is open.
