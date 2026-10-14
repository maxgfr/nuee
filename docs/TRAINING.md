# Training across the swarm

`nuee train` runs DiLoCo: data-parallel training where workers talk to the coordinator once per
round instead of once per step. It suits small models (up to tens of millions of parameters)
on a group of laptops, phones and desktops linked by ordinary internet connections. Splitting a
large model across peers is out of scope.

## DiLoCo in ten lines

1. The coordinator holds the global parameters θ.
2. Each round, every worker gets θ and runs H **inner** steps of its own optimiser (AdamW, Muon…)
   on its own batches. The inner optimiser state stays on the worker.
3. The worker sends back Δ = θ_local − θ, its change over the round.
4. The coordinator aggregates the deltas **robustly**: deltas with a non-finite value are
   dropped; with three or more, any delta whose cosine with the coordinate-wise median is below
   `cos_min` (default 0) or whose norm exceeds `norm_max_ratio` (default 3) times the median's
   is dropped, and the rest are combined with a coordinate-wise trimmed mean (`trim` 0.2 at
   each end). With one or two deltas, their plain mean is used.
5. The **outer** step treats −agg(Δ) as a gradient: SGD with Nesterov momentum, learning rate
   0.7 and momentum 0.9 by default (`v ← μv + g`, `θ ← θ − lr(μv + g)`).
6. Exec trainers send Δ as **int8 with error feedback**: per tensor, `scale = max|x|/127`;
   what rounding loses this round is added to the worker's next delta for the same job, so
   nothing is lost for good. That is 4 times smaller than f32 (`--codec int8`, the default;
   `--codec f32` sends raw). WASM trainer jobs always use f32, whatever `--codec` says: their
   results are recomputed and compared byte for byte, and an error-feedback residual is worker
   state the coordinator cannot replay.
7. Checkpoints (`checkpoint-<round>.safetensors`) and `train.json` are written after every
   round; an interrupted run resumes from them.

The code is in `crates/nuee-train` (outer step, codec, aggregation, rounds). It is pure and
compiles to wasm32, so the web page can coordinate a run too.

## Try it: the toy trainer

```console
$ nuee train demo:train --wait-peers 2
https://maxgfr.github.io/nuee/#/J/AFBUZN…
fingerprint 🍭 🎽 🌾 🥤 🧀 💈  (compare with the screen of whoever joins)
round 1/10  loss 2.069  accepted 4  rejected 0
round 2/10  loss 1.400  accepted 4  rejected 0
round 3/10  loss 0.786  accepted 4  rejected 0
…
round 9/10  loss 0.171  accepted 4  rejected 0
round 10/10  loss 0.173  accepted 4  rejected 0
done: 10 rounds, model in nuee-out/toy-trainer/checkpoint-10.safetensors
```

That run had two `nuee join` workers with two slots each and `--local-slots 0` on the
coordinator; it took about a minute on one Mac. The demo is a small MLP on the UCI optdigits
dataset (CC BY 4.0, attribution in `tasks/toy-trainer/data/README.md`), 10 rounds of 4 workers
× 30 inner steps. The trainer module and the dataset are embedded in every `nuee` binary
(`tasks/golden/toy-trainer.wasm`, `tasks/golden/optdigits.bin`); in a source checkout, a fresh
`scripts/build-tasks.sh` build under `target/tasks/` takes precedence.

The web page has the same demo ("Train together"): it shows the loss curve live and the final
accuracy, and browser tabs train alongside CLI workers.

`nuee-out/toy-trainer/train.json` records each round:

```json
{ "round": 1, "loss": 2.0690823, "accepted": 4, "rejected": 0, "update_norm": 1.7736899826742705 }
```

## Your own job

```toml
name = "my-model"
kind = "train"

[train]
rounds = 40
inner_steps = 500
workers = 4
outer_lr = 0.7            # default
outer_momentum = 0.9      # default
init = "init.safetensors" # f32 tensors only
trainer = "wasm"          # or "exec", with an [exec] table
module = "trainer.wasm"   # wasm trainers only
# trim = 0.2, cos_min = 0.0, norm_max_ratio = 3.0   (robust aggregation, defaults)

[inputs]
dir = "data"              # dataset files, handed to a wasm trainer as assets
```

Useful flags: `--wait-peers N`, `--min-workers N` and `--round-timeout 2m` (close a round once
N deltas arrived and the timeout passed, instead of waiting for every worker), `--inner-lr`,
`--seed` (shard assignment and task seeds; kept in checkpoints), `--deadline`, `--no-resume`.

## Two kinds of trainer, two kinds of verification

| | WASM trainer | exec trainer |
|---|---|---|
| Runs | in wasmtime and in every browser engine | native code on machines that approved the exact command (MLX, CUDA) |
| Output | Δ (f32) and the mean loss | the command writes new parameters (safetensors); the worker sends Δ (int8+EF by default, or f32) and the mean loss |
| Determinism | bit-exact across engines | none: MLX and CUDA differ from machine to machine |
| Verification | the job's policy (`sample` by default): the coordinator recomputes some leases and compares the bytes exactly | none per result; robust aggregation is the only defence |

**WASM trainers** follow the task ABI. Each lease's input is
`b"NTR1" | u32 steps | u32 shard | u32 n_shards | u64 seed | f32 lr | params (safetensors)`,
wrapped in the asset envelope with the dataset as asset 0. The module returns the encoded delta
followed by the mean inner loss as an f32. Because the same bytes give the same output in
wasmtime, V8, SpiderMonkey and JavaScriptCore, a cheating worker is caught the same way as on
any `wasm` job, and banned once the coordinator's own run confirms it.

**exec trainers** are approved commands from a pinned `repo@rev` (see [SKYCMD.md](SKYCMD.md) for
the exec rules). The host writes the round's parameters to a file and substitutes, as whole
arguments:

| Placeholder | Value |
|---|---|
| `$NUEE_INIT` | path of the input parameters (safetensors) |
| `$NUEE_STEPS` | `inner_steps` |
| `$NUEE_SHARD` | `i/n` |
| `$NUEE_SEED` | the task seed |
| `$NUEE_OUT` | where the command must leave the new parameters, same tensor names |

The worker host then computes Δ = θ_out − θ_in against the parameters it was given, encodes it
with the job's codec (keeping one error-feedback residual per job in memory, reset when the job
or the tensor layout changes), and returns the same format as a WASM trainer: the `NDL1` delta,
then the `nuee.loss` metadata value as an f32 (NaN when the trainer did not set it). An output
that is not safetensors, or has other tensors or shapes, fails the lease.

An exec trainer's output cannot be recomputed and compared: the same code on two Macs gives
different floats. What protects the model is the aggregation: a delta pointing away from the
median, or much larger than it, is dropped, and the trimmed mean bounds what any single worker
can move. A minority of bad workers among three or more can slow training; they cannot steer it
far in one round. That is weaker than recomputation: use exec trainers with people you trust,
and `--approve` to know who joined.

Exec trainers receive full f32 parameters and send an int8 delta: for skycmd's s4-micro (5.6 M
parameters) that is 22.4 MB down and about 5.6 MB up per worker per round (22.4 MB up with
`--codec f32`).

## skycmd's s4-micro

`examples/skycmd/train-s4.toml` trains skycmd's decision model with `sky-train s2-decide` as an
exec trainer on a team of Macs. It needs skycmd's branch `issue-3-diloco-inner` merged, for the
step-based flags `--steps`, `--stop-after` and `--init-params`. Setup and commands are in
[SKYCMD.md](SKYCMD.md).

## Limits

- A round waits for `workers` deltas unless `--min-workers` and `--round-timeout` say otherwise.
  A lease whose worker leaves or misses its deadline is handed to another worker.
- DiLoCo with few workers and a large H can drift from what one machine would train; keep H
  modest (hundreds of steps) and watch the loss in `train.json`.
- The toy trainer and the skycmd adapter are the two trainers that exist. Other models need a
  trainer written to the contract above.

## Troubleshooting

- **`rejected` is not 0.** Some deltas failed the cosine or norm check. One noisy worker now and
  then is normal; a worker rejected every round is either broken or dishonest.
- **The loss goes up after a few rounds.** Lower `outer_lr` or `inner_steps`, or pass
  `--inner-lr`.
- **Rounds stall.** A worker took a lease and went quiet; it is reassigned after the deadline.
  `--round-timeout` with `--min-workers` closes rounds without it.
