# skycmd on a nuée

Job files for [skycmd](https://github.com/maxgfr/skycmd); the exec ones pin commit
`781d4c176b2d6e301d0eeb82c69781b356505ec0`:

- `label.toml` spreads the s4 teacher labelling (`scripts/teacher-score.py`, Qwen3.5-4B on MLX)
  over every Mac of the team;
- `sweep.toml` runs a few `sky-train s2-decide` variants side by side and prints a table of
  their final accuracies;
- `train-s4.toml` trains s4-micro's decision model with DiLoCo over the same Macs (it needs a
  skycmd branch that is not merged yet, see below);
- `dagger.toml` collects oracle-labelled DAgger records with skycmd's simulator in
  WebAssembly (any peer, browsers included).

`label.toml` and `sweep.toml` are `kind = "exec"` jobs and `train-s4.toml` a `kind = "train"`
job with an exec trainer: the peers run skycmd's own code natively, from a fresh checkout of
that exact commit. Nothing runs on a peer whose operator did not approve that exact command.

## On each Mac that lends its GPU

The command needs what skycmd needs: Python 3 with `mlx` and `mlx-lm` on the `PATH` for the
labelling, a Rust toolchain for the sweep. Then join the link the coordinator prints:

```sh
nuee join '<link>' --gpu \
  --allow-exec https://github.com/maxgfr/skycmd@781d4c176b2d6e301d0eeb82c69781b356505ec0 \
  --exec-network
```

`--gpu` is required: the jobs set `gpu = true`, so only workers that offer a GPU get their
tasks (and each worker runs one GPU task at a time).

`--exec-network` lets the command reach the network: the first run downloads
`mlx-community/Qwen3.5-4B-4bit` from Hugging Face and the sweep's first build fetches crates.
Leave it out on machines that already have both cached.

`--allow-exec` pins the commit; each command still needs your approval. `join` shows the
exact command line and environment of each job and asks on the terminal. On a machine without a
terminal, put the commands in a file and pass `--exec-approvals FILE` (one JSON object per line;
`join` prints the line for each command it refuses).

## Labelling (`label.toml`)

From a skycmd checkout that has `data/s4/queue/p1-decisions.jsonl` (paths in a job file are
relative to the file):

```sh
cp /path/to/nuee/examples/skycmd/label.toml .
nuee run label.toml            # add --allow-exec <same repo@rev> to label on this Mac too
```

The 80,000 records are cut into 40 shards of 2,000 lines; each shard is one task, written by
`teacher-score.py` to `$NUEE_OUT`. Outputs land in `nuee-out/s4-p1-label/task-<i>.bin` as they
arrive, and an interrupted run resumes from them.

**How results merge.** `teacher-score.py` sets each record's `uid` to its line index in *its*
input, which is a shard here. When the job completes (and again after a resumed run), nuée shifts
every `uid` by the index of the shard's first line and writes
`nuee-out/s4-p1-label/s4-p1-label.jsonl`, sorted by `uid`. The result holds the same records,
with the same uids, as one unsharded run on a single Mac. Apart from the `uid` value, every line
is kept byte for byte.

**Verification.** 5% of the shards are scored a second time and compared with the `jsonl`
comparator: records paired by `uid`, every `tlogits` value within 0.1, the same argmax, every
other field identical. MLX does not give bit-identical logits on different Macs, so exact
comparison would flag honest peers. A peer whose answers differ (another argmax, an altered
record) is banned once the coordinator's own run confirms it, and its shards are redone.

**Expected speed (an estimate, not a measurement).** The 4B teacher scores about 7.2 records/s
on one M5. For p1's 80,000 records that is about 3 h on one Mac and about 45 min on four, plus
the 5% recomputed for verification and the model download on each Mac's first task.

## Sweep (`sweep.toml`)

```sh
head -n 5000 data/s4/queue/p1-decisions.jsonl > data/s4/queue/sweep-sample.jsonl
cp /path/to/nuee/examples/skycmd/sweep.toml .
nuee sweep sweep.toml
```

Each `[[sweep]]` entry is one task: its `args` are appended to the command line (here they
reach `s2-decide` through `"$@"`) and its `env` is merged over `[exec].env`. Each entry trains for
10 minutes, then copies the last line of its `log.jsonl` to `$NUEE_OUT`. The outputs land in
`nuee-out/s2-decide-sweep/<entry>/out`, and the summary lists every entry with its status,
duration, the peer that ran it and the metrics found in its output (`acc_f32`, `acc_ternary`),
best first. `--json` prints one `sweep_entry` event per finished entry.

The flags used (`--config`, `--tokenizer`, `--records`, `--minutes`, `--out`, `--qat-frac`,
`--adam-lr`, `--loop-dropout`, `--eval-every`, `--log-every`) all exist in
`crates/sky-train/src/main.rs` at that commit. The tokenizer is a small fixture from the
repository, so the sweep needs no pretraining run. It compares settings at smoke scale; it does
not train a model worth shipping.

## DiLoCo training (`train-s4.toml`)

Every round, each worker runs 500 steps of `sky-train s2-decide` from the current parameters
and returns its new parameters; the coordinator takes a robust mean of the changes and applies
it with outer Nesterov momentum (0.7, 0.9), 40 rounds over 4 workers. See
[docs/TRAINING.md](../../docs/TRAINING.md) for how this works and what it verifies.

It needs skycmd's branch `issue-3-diloco-inner` merged: that branch adds `--steps` (a schedule
by step count, so every Mac follows the same learning rate and QAT switch whatever its speed),
`--stop-after` and `--init-params`. The `rev` in the file is still skycmd main, which lacks
them; replace it with the merge commit before running.

On each Mac:

```sh
# the records and tokenizer are gitignored, so they are not in the task's fresh checkout:
# link a skycmd data directory where the command reads it
mkdir -p ~/.cache/skycmd && ln -s /path/to/skycmd/data ~/.cache/skycmd/data
nuee join '<link>' --gpu --allow-exec https://github.com/maxgfr/skycmd@<merge commit>
```

Add `--exec-network` for the first build if the crates are not cached. The build reuses
`~/.cache/skycmd/target` and needs Xcode's Metal toolchain. Each worker keeps its optimiser
state in `~/.cache/skycmd/diloco/s4-micro` across rounds; delete it before a new job.

On the coordinator, from a skycmd checkout that has run `s2-pretrain` for s4-micro:

```sh
cp /path/to/nuee/examples/skycmd/train-s4.toml .
nuee train train-s4.toml
```

Traffic (an estimate): 22.4 MB down (full f32 parameters) and about 5.6 MB up (the delta,
int8 with error feedback by default) per worker per round.

## DAgger records (`dagger.toml`)

A `kind = "wasm"` job: skycmd's simulator compiled to WebAssembly (`skycmd-arena`) plays the
s3-pico model on 4 games × 4 seeds, and the oracle labels the 4,000 states it visits per task.
The merged `nuee-out/skycmd-dagger/skycmd-dagger.jsonl` holds 64,000 `DecisionRecord` lines,
fine-tuning data for `sky-train` (the model learns the oracle's answer on the states it
reaches itself). `[wasm] assets` hands the model to every task. No GPU, no exec allowlist:
browsers and plain `nuee join` peers can take these tasks, and results are compared exactly.

From a nuee checkout, after `scripts/build-tasks.sh`:

```sh
nuee run examples/skycmd/dagger.toml
```

`nuee run demo:skycmd-dagger` runs the same requests without a checkout.

## Safety

- An exec task is native code with your user's rights, inside `sandbox-exec` (macOS) or `bwrap`
  (Linux) when present. Only approve commits you have read and trust. An approval names one
  exact commit and one exact command line and environment, so a later push, or another command
  at the same commit, is never run without asking.
- `--exec-network` widens the sandbox to the network; give it only to jobs that need it.
- The coordinator chooses the inputs; the command runs only as you approved it. Join only links
  from people you trust.
