# skycmd on a swarm

[skycmd](https://github.com/maxgfr/skycmd) trains small flight-decision models. It is the
first real workload Nuée was built for. One "s4 night" on a single M5 with 16 GB, from
skycmd's own logs (`runs/s4-logs/`), splits roughly like this:

| Stage | Share of the night | Distributed with |
|---|---|---|
| Teacher labelling (Qwen3.5-4B on mlx-lm) | about 45 % | `exec` jobs: `examples/skycmd/label.toml` |
| Training | about 50 % | DiLoCo: `examples/skycmd/train-s4.toml`, and sweeps: `sweep.toml` |
| Evaluation | about 5 % | WebAssembly in browsers and CLIs: `demo:skycmd` |

Measured on that night: the teacher scored 74,721 records in 169 minutes, 7.2 records/s; with
phase p1 skipped, a full night would take 3 to 6 hours more. Everything below about running
the same work on several machines is an **estimate**, except where it says measured.

The job files are in [examples/skycmd/](../examples/skycmd/README.md) and pin skycmd at
commit `781d4c176b2d6e301d0eeb82c69781b356505ec0`.

## Evaluation in browsers (`demo:skycmd`)

```console
$ nuee run demo:skycmd
https://maxgfr.github.io/nuee/#/J/AEHTZR…
fingerprint 🎽 🦚 ⚓ 🥌 🍪 🐍  (compare with the screen of whoever joins)
game      runs  mean score  completion  crashes
dodge        4        8.65        0.72        0
doom         4        3.20        1.00        0
flappy       4      200.00        0.00        0
rescue       4        3.00        1.00        0
done: 16 tasks, results in nuee-out/skycmd/results.json
```

This runs skycmd's own closed-loop simulator (`skycmd-arena`, compiled to WebAssembly) with the
s3-pico model: 4 games × 4 seeds × 10 episodes, decision threshold τ = 0.6. The model
(`s3-pico.gguf`) is taken from a source checkout when present, otherwise downloaded from
`https://maxgfr.github.io/skycmd/models/s3-pico.gguf`, and refused unless its BLAKE3 is
`1b0963f9…0b1d11`. Phones and laptops join from the link; the results are the same bytes
whoever computes them, so verification is exact.

Measured on an Apple M5, the coordinator alone (no peer joined): 20 s for the 16 tasks. The
web page offers the same evaluation in its catalogue ("skycmd evaluation", 20 episodes per task).

Release binaries (Homebrew, GitHub releases) embed the `skycmd-arena` module (cargo feature
`embed-tasks`); `nuee selftest` says whether yours does. A source build without that feature uses
`target/tasks/skycmd-arena.wasm`, built by `scripts/build-tasks.sh`, when run from the nuee
checkout; a fresh build there takes precedence over the embedded module.

## DAgger records (`demo:skycmd-dagger`)

```console
$ nuee run demo:skycmd-dagger
…
merged: nuee-out/skycmd-dagger/skycmd-dagger.jsonl
done: 16 tasks, results in nuee-out/skycmd-dagger/results.json
```

The same module and model in `dagger` mode (`{"mode":"dagger","game":…,"seed":…,"n":4000}`):
the s3-pico model plays each game, and the simulator's oracle labels every state the model
visits. Each line is one skycmd `DecisionRecord` (instruction, options, state, oracle target,
split), the data `sky-train` fine-tunes the decision model on, so it learns to recover from its
own mistakes (DAgger) without a GPU night of teacher labelling. 4 games × 4 seeds × 4,000
records; the outputs are merged in task order into one JSONL file, and verification is exact,
as for `demo:skycmd`. Measured on an Apple M5, the coordinator alone with 5 slots: 50 s for the
64,000 records (24 MB).

`examples/skycmd/dagger.toml` is the same job as a job file: `[wasm] assets` attaches the model
to a `wasm` job (paths relative to the job file, each asset named after its file, at most 16
of at most 64 MiB each), and `dagger-requests.jsonl` holds one request per task. The web page
does not offer DAgger collection in v0.1.

## Teacher labelling with `exec`

`label.toml` cuts `data/s4/queue/p1-decisions.jsonl` (80,000 records) into 40 shards of 2,000
lines, runs skycmd's `scripts/teacher-score.py` on each shard on whichever Mac takes it, and
merges the outputs with their `uid`s shifted back, so the result equals one unsharded run.

On each Mac that lends its GPU (it needs Python 3 with `mlx` and `mlx-lm`):

```sh
nuee join - --gpu \
  --allow-exec https://github.com/maxgfr/skycmd@781d4c176b2d6e301d0eeb82c69781b356505ec0 \
  --exec-network
```

`--allow-exec` pins the revision; it does not approve any command by itself. When a job
arrives, `join` shows its exact command line and environment and asks before running it (a
labelling job is one command; a sweep has one per entry). A worker without a terminal runs only
the commands listed in an approvals file, one JSON object per line, as the prompt and the refusal
note print it:

```sh
nuee join - --gpu --exec-network --exec-approvals approved.jsonl
# approved.jsonl:
# {"repo":"https://github.com/maxgfr/skycmd","rev":"781d4c1…","argv":["…"],"env":{}}
```

On the coordinator, from a skycmd checkout:

```sh
cp /path/to/nuee/examples/skycmd/label.toml .
nuee run label.toml
```

Verification: 5 % of the shards are scored twice and compared with the `jsonl` comparator
(records paired by `uid`, every `tlogits` value within 0.1, the same argmax, every other field
identical). MLX logits differ slightly between Macs, so exact comparison would flag honest
peers.

Estimated speed: 7.2 records/s per M5 (measured on one machine, above), so about 3 h for p1 on
one Mac and about 45 min on four, plus the 5 % recomputed and each Mac's first model download.
This job was not run end to end here: it needs MLX Macs. The end-to-end test `exec.spec.mjs`
runs the same pipeline with a fake teacher on two CLIs.

## Sweeps

`sweep.toml` runs five `sky-train s2-decide` variants (QAT fraction, Adam learning rate, loop
dropout), one task each, 10 minutes of training per variant, and prints a table ranked by the
metrics found in each output (`acc_f32`, `acc_ternary`):

```sh
head -n 5000 data/s4/queue/p1-decisions.jsonl > data/s4/queue/sweep-sample.jsonl
cp /path/to/nuee/examples/skycmd/sweep.toml .
nuee sweep sweep.toml
```

The table looks like this (from a sweep run here against a small test repository, not skycmd):

```console
name     status   duration  peer                acc
lr-3e-3  ok           0.2s  local            0.8600
lr-1e-3  ok           0.1s  local            0.8100
```

Add `--allow-exec <repo@rev>` to `nuee sweep` to run the sweep file's own entries on the
coordinator too (only those exact commands). Sweeps run
with `verify = "none"`: each entry is a different experiment, so there is nothing to compare.

## DiLoCo training of s4-micro

`train-s4.toml` trains s4-micro's decision model (5.6 M parameters) with `sky-train s2-decide`
as an exec trainer: 40 rounds × 500 inner steps on 4 workers, outer Nesterov 0.7 / 0.9. How
DiLoCo and exec trainers work, and how they are (and are not) verified, is in
[TRAINING.md](TRAINING.md).

**It needs skycmd's branch `issue-3-diloco-inner` merged first.** That branch adds the
step-based schedule (`--steps`, so every Mac follows the same WSD learning rate and QAT switch
whatever its speed), `--stop-after` and `--init-params`. The `rev` in the file is still skycmd
main, which does not have those flags; replace it with the merge commit before running.

On each Mac:

```sh
# the records and tokenizer are gitignored in skycmd, so link a data directory where the
# command looks for it (data/s4/{gold,mix,heldout}, data/numeric, data/tok/tok-s4-4096.json)
mkdir -p ~/.cache/skycmd && ln -s /path/to/skycmd/data ~/.cache/skycmd/data

nuee join - --gpu --allow-exec https://github.com/maxgfr/skycmd@<merge commit>
# join asks before running the trainer's exact command; without a terminal, list it in
# --exec-approvals FILE as shown above
# add --exec-network for the first build if the crates are not cached yet
```

On the coordinator, from a skycmd checkout that has run `s2-pretrain` for s4-micro:

```sh
cp /path/to/nuee/examples/skycmd/train-s4.toml .
nuee train train-s4.toml
```

Each worker keeps its optimiser state in `~/.cache/skycmd/diloco/s4-micro` and resumes it every
round; delete that directory before starting a new job. Estimated traffic per worker and round: 22.4 MB down
(full f32 parameters) and about 5.6 MB up (the int8 delta with error feedback, `--codec int8`
by default), so about 0.9 GB down and 0.22 GB up per worker over the 40 rounds.

This job was not run here: the branch is not merged yet. Its file is checked by the test
`skycmd_examples_are_valid_job_files` in `crates/nuee/tests/jobs.rs`.

## Safety

- An exec task is native code with your user's rights, inside `sandbox-exec` (macOS) or
  `bwrap` (Linux) when present. Approve only commits you have read. An approval names one exact
  commit, which is checked again after checkout, so a later push is never run, and one exact
  command line and environment: another command at the same commit is asked about again.
- `--exec-network` opens the sandbox to the network; give it only when a job needs it.
- The coordinator chooses the inputs; the command line and environment run only as you approved
  them. Variables that change how programs load code or which program or configuration they
  start (`LD_*`, `DYLD_*`, `*_OPTIONS`, `PYTHONPATH`, `NODE_OPTIONS`, `PATH`, `PIP_*`, `PAGER`,
  `SHELL`…; the full list is in docs/THREAT-MODEL.md, 4.2) are always refused, and the prompt
  shows every value. Without `--allow-exec` for the job's `repo@rev`, `join` never asks. Join
  only links from people you trust, and compare the fingerprint.
- `--exec-mirror URL=PATH` makes the worker fetch the allowlisted repository from a local clone
  instead of the network (air-gapped machines and tests). It needs `--allow-exec` (or
  `--exec-approvals`) for the same
  URL and changes nothing else: the commit is still checked.

## Troubleshooting

- **`native commands are disabled on this node (see --allow-exec)`.** The worker did not
  pin the job's `repo@rev`. Join prints the job's command line to put in an `--exec-approvals`
  file; on a terminal, pin the revision with `--allow-exec repo@rev` to be asked instead.
- **`job N runs a native command this machine has not approved`.** The worker has no terminal to
  ask on and the command is not in its `--exec-approvals` file. The note prints the line to add;
  join again after adding it.
- **A Mac never gets labelling or training tasks.** These jobs set `gpu = true`; join with
  `--gpu`.
- **`this nuee build has no embedded skycmd-arena module`.** A source build without
  `--features embed-tasks`: install a release binary, or run `scripts/build-tasks.sh` in the nuee
  checkout and run nuee from there.
- **`cannot download https://maxgfr.github.io/skycmd/models/s3-pico.gguf`.** The machine is
  offline and has no source checkout with the model; exit code 3.
