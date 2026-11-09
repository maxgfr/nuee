# Groups: a cluster that outlives one machine

A swarm from `nuee run` lasts as long as the machine that started it: its link names that
machine. A **group** is named by its own key instead. Its link stays valid when machines come
and go, and the roster, signed by the group key, says who belongs and who may coordinate.

## A first group

On the machine that creates it:

```console
$ nuee group create "lab"
https://maxgfr.github.io/nuee/#/G/AAQ...
fingerprint 🚑 🍋 🚂 🌾 🚲 🧄
$ nuee group up -            # link on stdin; this machine coordinates
```

On every other machine, give its id to the group's creator:

```console
$ nuee id
5b4e4a16f96e1dcf...
```

The creator adds it (`--keeper` lets it coordinate later, and prints a link that lists it too):

```sh
nuee group invite - 5b4e4a16f96e1dcf... --name alice-laptop --keeper
```

Then that machine stays in the group until Ctrl-C:

```sh
nuee group up -
nuee group up - --only-when ac,idle    # only take work on mains power, when not busy
```

`nuee group status -` lists the machines. `nuee group remove - <id>` takes one out at once, and
`nuee group handoff - <id>` moves the coordinator role, once no job is running.

Holding the link is not enough: a machine that is not on the roster is refused, and only the
machine that created the group holds the key that changes the roster. That key is in
`~/.config/nuee/groups/<group>/group.key`. Back it up: without it the group keeps running but
its roster can no longer change.

## Cut a test suite across the group

Every machine that wants this work says exactly what it accepts:

```sh
nuee group up - \
  --allow-repo https://github.com/acme/app.git \
  --allow-command 'npm ci && npx playwright test --shard={i}/{n}' \
  --exec-network
```

From any machine on the roster, in a checkout whose `HEAD` is pushed:

```console
$ nuee shard 8 --on - -- 'npm ci && npx playwright test --shard={i}/{n}'
8 shards submitted; they run on the machines that allow this repository and command
shard-1-of-8: passed
shard-2-of-8: FAILED
...
error: 1 of 8 shards failed; logs in nuee-out/shard-3f2a91c07b1d
```

Each shard starts from a clean checkout of the commit, so the command must install and build what
the tests need, and needs `--exec-network` to download anything.

`{i}` counts from 1 and `{n}` is the number of shards, as `playwright --shard`, `jest --shard` and
`cargo nextest --partition hash:{i}/{n}` expect. A failing test is a result: its log comes back
and nothing is run again. A machine that disappears is another matter: its shard goes to another
machine. Submitting the same command on the same commit returns the results already there.

### What `--allow-repo` means

It is the one place where a machine runs native code without approving each commit, so it is off
by default and narrow:

- only `nuee group up` can set it, and work is only taken from the coordinator named by a roster
  this machine verified;
- only shards submitted by a machine on the roster are accepted by that coordinator;
- the command must be exactly the one given to `--allow-command`, `{i}` and `{n}` standing for
  digits and nothing else; no environment, no GPU;
- `sandbox-exec` or `bwrap` is required: without one, `group up` refuses to start;
- the network stays off unless `--exec-network` is given.

**Allowing a repository means trusting every commit anyone can push to it**, since the command
runs the repository's own code. The sandbox limits what that code reaches; it does not make it
safe. Results are not checked by recomputation either: test runs are not reproducible bit for
bit, and a machine on the roster can lie about them.

## Limits today

- If the coordinating machine dies, the group waits for it: other keepers do not take over by
  themselves yet.
- A group uses one relay, the one in its link. If that relay is down, the group is unreachable.
  Use `nuee relay` for anything but experiments.
- Every machine on the roster can use the coordinator's whole API: jobs, sites and services.
- `--only-when idle` is checked before taking work, not during it: the work itself makes the
  machine busy.
- Browsers cannot join a group yet. Opening a `G` link in the page says so.
