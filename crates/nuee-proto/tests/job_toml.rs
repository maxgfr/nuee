use nuee_proto::{JobError, JobKind, JobToml};

const REV: &str = "781d4c176b2d6e301d0eeb82c69781b356505ec0";

fn ok(text: &str) -> JobToml {
    JobToml::from_str_checked(text).unwrap_or_else(|e| panic!("{e}\n{text}"))
}

fn invalid(text: &str) -> String {
    match JobToml::from_str_checked(text) {
        Err(JobError::Invalid(m)) | Err(JobError::Parse(m)) => m,
        Ok(_) => panic!("accepted:\n{text}"),
    }
}

#[test]
fn one_valid_file_per_kind() {
    let wasm = ok(r#"
name = "label-p1"
kind = "wasm"
verify = "sample"
sample = 0.10
canary = 0.03
deadline = "60s"
max_memory = "256MiB"
[inputs]
generate = "mandelbrot"
count = 256
params = { width = 512, zoom = 1.5, name = "x" }
[output]
dir = "out/"
merge = "concat"
[compare]
kind = "exact"
[wasm]
module = "task.wasm"
"#);
    assert_eq!(wasm.kind, JobKind::Wasm);
    assert_eq!(wasm.deadline_ms().unwrap(), 60_000);
    assert_eq!(wasm.max_memory_bytes().unwrap(), Some(256 << 20));

    ok(r#"
name = "gemm"
kind = "wgsl"
[inputs]
dir = "inputs/"
[compare]
kind = "tolerance"
abs = 1e-4
rel = 1e-3
max_bad = 0.001
[wgsl]
shader = "k.wgsl"
entry = "main"
workgroup = [64, 1, 1]
"#);
    ok(r#"
name = "emb"
kind = "embed"
[inputs]
jsonl = "data.jsonl"
lines_per_task = 2000
[output]
merge = "jsonl"
uid_field = "uid"
[compare]
kind = "cosine"
min_cos = 0.999
[embed]
model = "minilm"
max_tokens = 256
"#);
    ok(r#"
name = "mcq"
kind = "llm"
[inputs]
jsonl = "q.jsonl"
[compare]
kind = "logits"
max_gap = 0.05
[llm]
model = "qwen3.5-0.8b"
letters = "ABCD"
"#);
    let exec = ok(&format!(
        r#"
name = "arena"
kind = "exec"
deadline = "4h"
[compare]
kind = "jsonl"
field = "score"
tol = 0.01
[exec]
repo = "https://github.com/maxgfr/skycmd"
rev = "{REV}"
argv = ["cargo", "run", "--release"]
env = {{ SEED = "1" }}
timeout = "4h"
gpu = true
[[sweep]]
name = "a"
args = ["--x", "1"]
[[sweep]]
name = "b"
env = {{ X = "2" }}
"#
    ));
    assert_eq!(exec.sweep.len(), 2);
    ok(r#"
name = "toy"
kind = "train"
[train]
rounds = 10
inner_steps = 50
workers = 4
outer_lr = 0.7
outer_momentum = 0.9
init = "init.safetensors"
trainer = "wasm"
module = "toy-trainer.wasm"
trim = 0.2
cos_min = 0.0
norm_max_ratio = 3.0
"#);
}

#[test]
fn unknown_fields_are_errors() {
    invalid(
        "name = \"x\"\nkind = \"wasm\"\ncolour = 1\n[inputs]\ndir = \"d\"\n[wasm]\nmodule = \"m\"\n",
    );
    invalid(
        "name = \"x\"\nkind = \"wasm\"\n[inputs]\ndir = \"d\"\n[wasm]\nmodule = \"m\"\nmodul = \"m\"\n",
    );
    invalid("name = \"x\"\nkind = \"cuda\"\n");
}

#[test]
fn two_input_sources_are_refused() {
    let m = invalid(
        "name = \"x\"\nkind = \"wasm\"\n[inputs]\ndir = \"d\"\njsonl = \"j\"\n[wasm]\nmodule = \"m\"\n",
    );
    assert!(m.contains("exactly one"), "{m}");
}

#[test]
fn kind_tables_must_match() {
    let m = invalid("name = \"x\"\nkind = \"wasm\"\n[inputs]\ndir = \"d\"\n");
    assert!(m.contains("[wasm]"), "{m}");
    let m = invalid(
        "name = \"x\"\nkind = \"wasm\"\n[inputs]\ndir = \"d\"\n[wasm]\nmodule = \"m\"\n[llm]\nmodel = \"q\"\n",
    );
    assert!(m.contains("[llm]"), "{m}");
}

#[test]
fn bad_rev_is_refused() {
    for rev in ["main", "781d4c1", &REV.to_uppercase(), &format!("{REV}0")] {
        let m = invalid(&format!(
            "name = \"x\"\nkind = \"exec\"\n[exec]\nrepo = \"r\"\nrev = \"{rev}\"\nargv = [\"a\"]\n"
        ));
        assert!(m.contains("rev"), "{m}");
    }
}

#[test]
fn value_ranges() {
    let base = "kind = \"wasm\"\n[inputs]\ndir = \"d\"\n[wasm]\nmodule = \"m\"\n";
    invalid(&format!("name = \"\"\n{base}"));
    invalid(&format!("name = \"{}\"\n{base}", "x".repeat(65)));
    invalid(&format!("name = \"x\"\nsample = 1.5\n{base}"));
    invalid(&format!("name = \"x\"\ndeadline = \"25h\"\n{base}"));
    invalid(&format!("name = \"x\"\ndeadline = \"60\"\n{base}"));
    invalid(&format!("name = \"x\"\nmax_memory = \"2GiB\"\n{base}"));
    ok(&format!(
        "name = \"x\"\nmax_memory = \"1GiB\"\ndeadline = \"250ms\"\n{base}"
    ));
}

#[test]
fn exec_trainer_takes_an_exec_table() {
    let job = ok(&format!(
        r#"
name = "sky"
kind = "train"
[train]
rounds = 3
inner_steps = 10
workers = 2
init = "init.safetensors"
trainer = "exec"
[exec]
repo = "https://github.com/maxgfr/skycmd"
rev = "{REV}"
argv = ["python3", "train.py", "--init", "$NUEE_INIT", "--out", "$NUEE_OUT"]
"#
    ));
    assert!(job.exec.is_some());
    let m = invalid(
        "name = \"x\"\nkind = \"train\"\n[train]\nrounds = 1\ninner_steps = 1\nworkers = 1\ninit = \"i\"\ntrainer = \"exec\"\n",
    );
    assert!(m.contains("[exec]"), "{m}");
    let m = invalid(&format!(
        "name = \"x\"\nkind = \"train\"\n[train]\nrounds = 1\ninner_steps = 1\nworkers = 1\ninit = \"i\"\ntrainer = \"wasm\"\nmodule = \"m.wasm\"\n[exec]\nrepo = \"https://a.b/c\"\nrev = \"{REV}\"\nargv = [\"x\"]\n"
    ));
    assert!(m.contains("[exec]"), "{m}");
}

#[test]
fn wasm_assets_parse_and_are_checked() {
    let job = |assets: &str| {
        format!(
            "name = \"d\"\nkind = \"wasm\"\n[wasm]\nmodule = \"m.wasm\"\nassets = [{assets}]\n[inputs]\ndir = \"in\"\n"
        )
    };
    let j = ok(&job(r#""models/s3-pico.gguf", "data.bin""#));
    let w = j.wasm.expect("wasm");
    assert_eq!(w.assets, ["models/s3-pico.gguf", "data.bin"]);
    assert_eq!(
        nuee_proto::job_toml::asset_name(&w.assets[0]),
        "s3-pico.gguf"
    );
    // Without the key, no asset.
    let plain =
        ok("name = \"d\"\nkind = \"wasm\"\n[wasm]\nmodule = \"m.wasm\"\n[inputs]\ndir = \"in\"\n");
    assert!(plain.wasm.expect("wasm").assets.is_empty());

    assert!(invalid(&job(r#""a/x.bin", "b/x.bin""#)).contains("two files named"));
    let many: Vec<String> = (0..17).map(|i| format!("\"f{i}\"")).collect();
    assert!(invalid(&job(&many.join(", "))).contains("at most 16"));
    assert!(invalid(&job(r#""models/""#)).contains("must name a file"));
    assert!(invalid(&job(r#""..""#)).contains("must name a file"));
}
