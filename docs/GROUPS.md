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

## When a machine dies

Machines invited with `--keeper` replace each other. If the coordinator disappears, the first
keeper still there takes over (the one at place `p` among the keepers waits about
`10 s × p` after the coordinator stops answering), members follow it, and `nuee shard` and
`nuee build` submit what is left to it. A keeper that comes back works for the one that took over.
Give a group at least two keepers, and pass members the link printed by the last
`group invite --keeper`, which lists them.

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

Add `--fresh` to run again what already has a result, such as a flaky failure.

To compare settings instead of cutting a suite, let `{i}` choose them, as in
`nuee shard 8 --on - -- 'python3 train.py --config configs/{i}.json'`.

## Build a commit, once for the whole team

```sh
nuee group up - --allow-repo https://github.com/acme/app.git \
  --allow-command 'npm ci && npm run build' --exec-network
nuee build --on - --keep dist -- 'npm ci && npm run build'
```

One machine builds the commit, and `dist/` and `nuee-build.log` come back here. Whoever builds the
same commit with the same command then gets that result in a second, without building again. A
failing build is a result too: its log comes back and the command exits with an error.

For a private repository, give the machines a read-only token with
`--git-token-file token.txt`. It is used to fetch the repository and never reaches the command.

## From GitHub Actions

The Action sends the commit to your group and waits: the job runs on your machines, and the hosted
runner only waits, so the cheapest one will do.

```sh
nuee group ci-key - --name github   # on the machine that created the group; shown once
```

Store that key and the group link as the secrets `NUEE_KEY` and `NUEE_GROUP`, then:

```yaml
on: [push]
jobs:
  build:
    runs-on: ubuntu-24.04
    steps:
      - uses: maxgfr/nuee@v0.6.0
        with:
          group: ${{ secrets.NUEE_GROUP }}
          key: ${{ secrets.NUEE_KEY }}
          command: npm ci && npm run build
          keep: dist
      - uses: actions/upload-artifact@v7
        with:
          path: nuee-out/dist
```

Use `shards: 8` and `{i}/{n}` in the command for a test suite. `command` must be exactly what the
machines allow. The repository matches with or without `.git` or a trailing `/`, and whatever the
case of the host, but the path is compared as written.

A CI key can only submit jobs: it cannot work, change the roster or read the group's files, and
`nuee group remove - <id>` revokes it. GitHub does not give secrets to pull requests from forks;
the Action refuses `pull_request_target`, which would.

### What `--allow-repo` means

It is the one place where a machine runs native code without approving each commit, so it is off
by default and narrow. `nuee shard` and `nuee build` share it:

- only `nuee group up` can set it, and work is only taken from the coordinator named by a roster
  this machine verified;
- only shards submitted by a machine on the roster are accepted by that coordinator;
- the command must be exactly the one given to `--allow-command`, `{i}` and `{n}` standing for
  digits and nothing else; no environment, no GPU;
- `sandbox-exec` or `bwrap` is required: without one, `group up` refuses to start;
- the command cannot read home directories, this machine's Nuée keys or other programs' sockets,
  and writes only in its own directory and in a home kept per repository, where `~/.npm` and the
  like survive between runs. Tools installed under your home through `PATH` stay usable; show it
  anything else with `--exec-read DIR`;
- the network stays off unless `--exec-network` is given.

**Allowing a repository means trusting every commit anyone can push to it**, since the command
runs the repository's own code. The sandbox limits what that code reaches; it does not make it
safe: with `--exec-network` it can still send out the checkout, and it can reach services on your
network. Results are not checked by recomputation either: builds and test runs are not reproducible
bit for bit, and a machine on the roster can lie about them, so do not ship a release built this way
without checking it.

## The group's drive and sites

```sh
nuee group put - ./photos            # the group keeps it; this machine can go offline
nuee group ls -
nuee group get - photos -o ./back
nuee group rm - photos
nuee site publish ./public --on -    # needs a keeper running `group up - --gateway 0.0.0.0:8080`
```

Every keeper copies the files and sites within seconds, encrypted as they were sent, so they
survive the loss of any keeper that has its copy. Keepers can decrypt them: choose them accordingly.

### A stable name, and any static host

```sh
nuee site publish ./public --on - --name blog   # prints a #/N/ link; needs no gateway
nuee site loader - -o deploy/                   # three files for GitHub Pages or any static host
```

Publishing again under the same name changes what the link shows, not the link. It carries no
secret of the group and only reads that one site, so it can be public. The page shows it as an
inert preview. The loader's page fetches the site from the keepers, checks it, and a service worker
serves it from the host's own origin with its scripts and all its pages. A returning visitor keeps
the site with every machine off, and finds a new version within five minutes of a later visit.

A first visit needs a relay and a keeper online. The link lists up to four keepers of the day, so
print it again after changing them. The loader runs the Nuée package from `--web-base` (by default
this project's GitHub Pages) on your origin: serve that package yourself if you do not want to
depend on it. Search engines see only the loader.

## Python and coding agents

`clients/python/nuee.py` is a one-file client (`Group(link).build(...)`, `.shard(...)`, `.put(...)`)
over the `nuee` command. `nuee mcp --on <link>` gives a coding agent the same verbs over the Model
Context Protocol. It is bound to that one group, and what actually runs is still decided by each
machine's `--allow-command`.

## Limits today

- A group with one keeper still waits for it to come back.
- A job that was running when its coordinator died starts over: only what its client submits again
  runs, so the client has to be there. Services (`nuee expose`) are not moved to another machine.
- The commands need their tools installed on each machine: `npm` has to exist there, and two
  machines with different versions can give different results.
- A group uses one relay, the one in its link. If that relay is down, the group is unreachable.
  Use `nuee relay` for anything but experiments.
- Every machine on the roster can use the coordinator's whole API: jobs, sites and services.
- `--only-when idle` is checked before taking work, not during it: the work itself makes the
  machine busy.
- Browsers cannot join a group yet. Opening a `G` link in the page says so; they can only visit
  its named sites.
