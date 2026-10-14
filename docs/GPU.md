# GPU tasks, embeddings and LLM scoring

Three task kinds use a GPU or a model, and none of them gives bit-identical results across
machines. Each one is therefore verified with a tolerance that was measured, not guessed:

| Kind | Runs natively on | Runs in browsers on | Comparator |
|---|---|---|---|
| `wgsl` | wgpu (Metal, Vulkan, DX12) | WebGPU, in a Web Worker (main thread as a fallback) | share of elements out of tolerance |
| `embed` | candle, fp32, CPU | ONNX Runtime Web (Transformers.js 4.3.1), q8 | cosine ≥ τ |
| `llm` | a local OpenAI-compatible endpoint (`--llm`) | Transformers.js, Qwen3.5-0.8B ONNX q4 | argmax + logit gap |

`exec` jobs can also ask for a GPU (`gpu = true` in `[exec]`, for MLX or CUDA code); see
[SKYCMD.md](SKYCMD.md) and [TRAINING.md](TRAINING.md). A worker runs at most one GPU `exec`
task at a time.

## Fractal on the GPU

```console
$ nuee run demo:mandelbrot-gpu
https://maxgfr.github.io/nuee/#/J/AF7...
fingerprint 🐸 🚢 🎃 🐯 🗼 🎁  (compare with the screen of whoever joins)
image: nuee-out/mandelbrot-gpu/mandelbrot.ppm
done: 256 tasks, results in nuee-out/mandelbrot-gpu/results.json
```

The coordinator computes on its own GPU too, so on a Mac this finishes alone in a few seconds.
Other machines join with `nuee join - --gpu` (it prints `offering the GPU Apple M5`, or the
adapter it found), and browser tabs offer their GPU from the join card when WebGPU is available.
Without `--gpu`, a CLI worker never takes `wgsl` tasks.

Measured on an Apple M5 (the `gpu` end-to-end test, Chromium with WebGPU plus a CLI): 8 s for
the whole job.

### Why the fractal is not compared exactly

The WGSL shader computes escape counts in f32. Near the edge of the set the iteration is
chaotic, so two correct GPUs, or IEEE f32 with and without fused multiply-add, disagree on some
pixels. On the 1024×1024 demo grid the worst 64×64 tile has 89 pixels out of 4096 (2.2 %) that
differ between wgpu on Metal, Chromium's WebGPU and f32 with or without FMA.

The demo's comparator (`MANDELBROT_COMPARE` in `crates/nuee-gpu/src/wgsl.rs`) is therefore
`ElementTolerance{abs: 2, rel: 0, max_bad_permille: 50}`: two outputs agree when at most 50 per
mille of their values differ by more than 2 iterations.

What this means for a dishonest worker: **up to 5 % of a tile's pixels can be wrong without the
recomputation noticing.** For a fractal demo that is fine. For your own WGSL jobs, measure how
far honest GPUs drift on your workload and set the tolerance from that; if you need every value
checked, the task has to be deterministic (use a `wasm` task instead).

## Writing a WGSL job

```toml
name = "my-shader"
kind = "wgsl"

[wgsl]
shader = "kernel.wgsl"
entry = "main"            # default "main"

[inputs]
dir = "inputs"            # one file per task, in the NWG1 format below

[compare]
kind = "tolerance"
abs = 1e-4
max_bad = 0.0             # share of elements allowed out of tolerance, 0..=1
```

The shader is validated by naga before anything runs, on the coordinator and on every worker.
Each task input is, little-endian:

```text
b"NWG1"
u32 entry_len, entry point name (1..=256 bytes of UTF-8)
u32 wx, u32 wy, u32 wz        workgroup counts, each 1..=65535
u32 n                         bindings, 1..=8
n times:
  u32 binding                 binding number in @group(0), distinct
  u32 kind                    0 uniform, 1 storage read-only, 2 storage read_write (output)
  u32 size                    bytes, a non-zero multiple of 4, at most 64 MiB
  size bytes of data          for kinds 0 and 1 only
```

All bindings add up to at most 128 MiB, and the shader source to at most 1 MiB. The output is
the final contents of the `read_write` buffers, concatenated in input order. The native runner
(`crates/nuee-gpu/src/wgsl.rs`) and the browser one (`web/app/worker/gpu.js`) read the same
format.

## WebGPU in browsers (October 2026)

| Browser | WebGPU |
|---|---|
| Chrome, Edge | on by default on Windows, macOS and ChromeOS since 113, Android since 121; Linux depends on version and driver |
| Safari | Safari 26 on macOS, iOS and iPadOS 26 |
| Firefox | Windows since 141; macOS 26 since 145; every Apple Silicon Mac since 147; Linux only in Nightly |

A tab without WebGPU still joins and runs `wasm` tasks; it just does not offer the GPU. Check
`chrome://gpu` (or `about:support` in Firefox) when a machine you expected to help does not.

## Embeddings

```console
$ nuee embed notes/
2 chunks in 1 tasks
checking the embedding model (downloaded once, about 90 MB)
embedding locally with all-MiniLM-L6-v2 on the cpu
https://maxgfr.github.io/nuee/#/J/AFN...
fingerprint 🎆 🔥 🍲 🥖 🦖 🍔  (compare with the screen of whoever joins)
1 tasks, verify Sample, 1 local slots, outputs in nuee-out/embed
2 vectors: nuee-out/embed/vectors.f32 (nuee-out/embed/meta.jsonl)
done: 1 tasks, results in nuee-out/embed/results.json
```

`nuee embed` reads text files and folders (recursively; files that are not UTF-8 are skipped),
cuts them into chunks and hands out tasks of up to 256 texts. It writes:

- `vectors.f32`: one row of 384 little-endian f32 per chunk, L2-normalised;
- `meta.jsonl`: one line per row, `{"id":0,"path":"notes/a.txt","chunk":0,"start":0,"end":18}`
  (byte offsets in the file).

An interrupted run resumes from what is already in `--out`. A job file can also embed the
lines of a JSONL file (`kind = "embed"`, `[embed] model = "all-MiniLM-L6-v2"`).

The model is `sentence-transformers/all-MiniLM-L6-v2` at a pinned revision. The CLI downloads
it once from Hugging Face (only `huggingface.co` and `*.hf.co` are accepted, redirects
included) and checks every file against its SHA-256 before use. Browsers fetch the q8 ONNX
export the same way.

**τ calibration.** Texts are truncated at 256 tokens, mean-pooled and normalised, the
sentence-transformers way. candle (fp32, batches of 64) and ONNX Runtime Web (q8, one text at a
time) do not produce the same floats; the lowest cosine measured between them over the
calibration corpus was 0.97173. τ is that minus 0.005, floored: **0.966** (`TAU` in
`crates/nuee-gpu/src/embed.rs`). The manifest carries `Comparator::Cosine{min: 0.966}`, and a
recomputed row below it counts as a mismatch.

Measured on an Apple M5: the `embed` end-to-end test (CLI coordinator, browser and CLI
workers) takes 21 s.

## LLM option scoring

An `llm` job scores decision records the way skycmd's `scripts/teacher-score.py` does: each
record (`t`, `ins`, `opts`, optional `state`) becomes one non-thinking Qwen chat prompt, the
model is never sampled, and only the next-token scores of the option letters are kept.

```toml
name = "scores"
kind = "llm"

[llm]
model = "qwen3.5-0.8b"
letters = "ABCD"

[inputs]
jsonl = "records.jsonl"
lines_per_task = 100
```

Each output line is `{"uid":N,"tlogits":[…]}`, one score per option, rounded to three
decimals. Shards are merged back into one JSONL with the uids shifted, as for the labelling
job in [SKYCMD.md](SKYCMD.md).

Backends:

| Where | Backend | Log-probabilities |
|---|---|---|
| CLI | `nuee join <link> --llm http://127.0.0.1:8080` pointing at llama-server (llama.cpp) | yes, `logprobs.content[0].top_logprobs` |
| CLI | LM Studio, vLLM, or any server whose `/v1/completions` honours `logprobs` | yes, OpenAI shape `logprobs.top_logprobs[0]` |
| CLI | a server that ignores `logprobs` | refused: the task fails with "the LLM endpoint returned no logprobs" |
| Browser | `onnx-community/Qwen3.5-0.8B-ONNX`, q4, pinned revision, every file checked by SHA-256 | computed locally from the logits |

The `--llm` address only ever comes from the worker's own command line, never from a job. The
browser model is offered only to devices that do not report less than 4 GB of memory, and its first use
downloads about 650 MB.

Verification compares centred scores (backends agree only up to a constant): the same argmax,
and no letter further apart than `max_gap`. The default is 1.5 for jobs that mix backends
(llama.cpp against ONNX Runtime, different quantisations); between two runs of the same
backend and weights 0.25 is enough, and you can set it with `[compare] kind = "logits"`,
`max_gap = 0.25`.

Measured on an Apple M5 in Chromium with WebGPU: 78 s for a browser to score 3 records,
including the first download of the model (about 650 MB).

The `--llm` path was exercised in tests against a mock OpenAI-compatible server only; pointing
it at a real llama-server is up to you.

## Troubleshooting

- **A CLI worker never gets GPU tasks.** It needs `--gpu`; check that it printed `offering the
  GPU …`. On a machine without a usable adapter, `join --gpu` exits with `--gpu: no usable GPU
  adapter on this machine`; join without `--gpu` to lend the CPU only.
- **`checking the embedding model` hangs or fails.** The first run needs `huggingface.co`.
  The files are cached under `<data dir>/models/` and checked against their SHA-256 on every
  start.
- **Many `wgsl` mismatches with honest machines.** Your tolerance is tighter than the drift
  between GPUs. Measure it and loosen `abs`, `rel` or `max_bad`.
- **The browser does not offer the LLM.** The device reported less than 4 GB of memory.
