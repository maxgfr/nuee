//! Job files: JSONL shards and their merge, the JSONL tolerance comparator, generators, sweep
//! planning, and a job file run end to end through a local relay with one joiner.

#[path = "../src/cmd/run/jobfile.rs"]
mod jobfile;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use nuee_core::Compare;
use nuee_proto::Hash;
use serde_json::Value;
use tempfile::TempDir;

use jobfile::{JsonlCompare, merge_jsonl, shard_lines};

// ---------------------------------------------------------------------------------------------
// Shards and merge.

/// What `teacher-score.py` writes for `input`: every record plus `uid` (its line index in
/// `input`) and `tlogits`, in Python's `json.dumps` style, ordered by prompt length.
fn fake_teacher(input: &[u8]) -> Vec<u8> {
    let text = std::str::from_utf8(input).expect("utf8");
    let mut rows: Vec<(usize, String)> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(line).expect("record");
        let q = v["q"].as_str().expect("q");
        let n = v["n"].as_u64().expect("n");
        let logits = format!("[{}, {}, {}]", n % 7, (n * 3) % 5, 1);
        rows.push((
            q.len(),
            format!("{{\"q\": \"{q}\", \"n\": {n}, \"uid\": {i}, \"tlogits\": {logits}}}"),
        ));
    }
    rows.sort_by_key(|(len, _)| *len);
    let mut out = String::new();
    for (_, r) in rows {
        out.push_str(&r);
        out.push('\n');
    }
    out.into_bytes()
}

fn records(n: usize) -> Vec<u8> {
    let mut s = String::new();
    for i in 0..n {
        let q = "x".repeat(1 + (i * 7919) % 13);
        s.push_str(&format!("{{\"q\": \"{q}\", \"n\": {i}}}\n"));
    }
    s.into_bytes()
}

#[test]
fn jsonl_of_10001_lines_in_shards_of_1000_gives_11_tasks() {
    let text = records(10_001);
    let shards = shard_lines(&text, 1000);
    assert_eq!(shards.len(), 11);
    for (i, s) in shards.iter().enumerate() {
        assert_eq!(s.first_line, i as u64 * 1000);
    }
    let last = shards.last().expect("last shard");
    assert_eq!(last.bytes.iter().filter(|b| **b == b'\n').count(), 1);
    let joined: Vec<u8> = shards.iter().flat_map(|s| s.bytes.clone()).collect();
    assert_eq!(joined, text);

    // Empty lines are kept as they are and still count as lines.
    let shards = shard_lines(b"{\"a\":1}\n\n{\"a\":2}\n{\"a\":3}", 2);
    assert_eq!(shards.len(), 2);
    assert_eq!(shards[0].bytes, b"{\"a\":1}\n\n");
    assert_eq!(shards[1].first_line, 2);
    assert_eq!(shards[1].bytes, b"{\"a\":2}\n{\"a\":3}");
    assert!(shard_lines(b"", 10).is_empty());
}

#[test]
fn merged_uids_equal_an_unsharded_run() {
    let text = records(2_345);
    let unsharded = fake_teacher(&text);

    let shards = shard_lines(&text, 500);
    assert_eq!(shards.len(), 5);
    let outputs: Vec<(u64, Vec<u8>)> = shards
        .iter()
        .map(|s| (s.first_line, fake_teacher(&s.bytes)))
        .collect();
    let parts: Vec<(u64, &[u8])> = outputs.iter().map(|(f, b)| (*f, b.as_slice())).collect();
    let merged = merge_jsonl(&parts, Some("uid")).expect("merge");

    // Same records, byte for byte, once both are put in uid order.
    let by_uid = |bytes: &[u8]| {
        let mut lines: Vec<(u64, String)> = std::str::from_utf8(bytes)
            .expect("utf8")
            .lines()
            .map(|l| {
                let v: Value = serde_json::from_str(l).expect("json");
                (v["uid"].as_u64().expect("uid"), l.to_owned())
            })
            .collect();
        lines.sort();
        lines
    };
    let expected = by_uid(&unsharded);
    let got = by_uid(&merged);
    assert_eq!(got.len(), 2_345);
    assert_eq!(got, expected);
    // The merged file itself is in uid order.
    let uids: Vec<u64> = std::str::from_utf8(&merged)
        .expect("utf8")
        .lines()
        .map(|l| {
            serde_json::from_str::<Value>(l).expect("json")["uid"]
                .as_u64()
                .expect("uid")
        })
        .collect();
    assert!(uids.windows(2).all(|w| w[0] < w[1]));

    // Without a uid field the outputs are concatenated in task order.
    let plain = merge_jsonl(
        &[(0, b"{\"a\": 1}\n".as_slice()), (5, b"{\"a\": 2}\n")],
        None,
    )
    .expect("merge");
    assert_eq!(plain, b"{\"a\": 1}\n{\"a\": 2}\n");
    // A record without uid or overlapping shards are errors, not silent data loss.
    assert!(merge_jsonl(&[(0, b"{\"a\": 1}\n".as_slice())], Some("uid")).is_err());
    assert!(
        merge_jsonl(
            &[(0, b"{\"uid\": 1}\n".as_slice()), (0, b"{\"uid\": 1}\n")],
            Some("uid")
        )
        .is_err()
    );
    assert!(merge_jsonl(&[(0, b"not json\n".as_slice())], None).is_err());
}

// ---------------------------------------------------------------------------------------------
// Comparator.

#[test]
fn jsonl_comparator_tolerates_drift_but_not_a_new_answer() {
    let cmp = JsonlCompare {
        field: "tlogits".into(),
        tol: 0.05,
    };
    let a = b"{\"q\": \"a\", \"uid\": 0, \"tlogits\": [1.0, 2.5, -0.5]}\n{\"q\": \"b\", \"uid\": 1, \"tlogits\": [3.0, 0.0]}\n";
    // MLX on another Mac: each logit a little off.
    let drift = b"{\"q\": \"a\", \"uid\": 0, \"tlogits\": [1.03, 2.47, -0.5]}\n{\"q\": \"b\", \"uid\": 1, \"tlogits\": [2.98, 0.01]}\n";
    assert!(cmp.same(a, drift));
    assert!(cmp.same(a, a));
    // Same records in another order pair up by uid.
    let reordered = b"{\"q\": \"b\", \"uid\": 1, \"tlogits\": [3.0, 0.0]}\n{\"q\": \"a\", \"uid\": 0, \"tlogits\": [1.0, 2.5, -0.5]}\n";
    assert!(cmp.same(a, reordered));

    // Beyond the tolerance.
    let far = b"{\"q\": \"a\", \"uid\": 0, \"tlogits\": [1.2, 2.5, -0.5]}\n{\"q\": \"b\", \"uid\": 1, \"tlogits\": [3.0, 0.0]}\n";
    assert!(!cmp.same(a, far));
    // Within the tolerance, but the argmax flips.
    let close = b"{\"q\": \"c\", \"uid\": 0, \"tlogits\": [1.0, 1.02]}\n";
    let flipped = b"{\"q\": \"c\", \"uid\": 0, \"tlogits\": [1.03, 1.0]}\n";
    assert!(!cmp.same(close, flipped));
    // Another field changed.
    let changed = b"{\"q\": \"z\", \"uid\": 0, \"tlogits\": [1.0, 2.5, -0.5]}\n{\"q\": \"b\", \"uid\": 1, \"tlogits\": [3.0, 0.0]}\n";
    assert!(!cmp.same(a, changed));
    // A missing record, an extra field, garbage.
    let short = b"{\"q\": \"a\", \"uid\": 0, \"tlogits\": [1.0, 2.5, -0.5]}\n";
    assert!(!cmp.same(a, short));
    let extra = b"{\"q\": \"a\", \"uid\": 0, \"tlogits\": [1.0, 2.5, -0.5], \"x\": 1}\n{\"q\": \"b\", \"uid\": 1, \"tlogits\": [3.0, 0.0]}\n";
    assert!(!cmp.same(a, extra));
    assert!(!cmp.same(a, b"not json\n"));
    // Without uids, records pair up by position.
    let p1 = b"{\"t\": [0.5, 0.1]}\n{\"t\": [0.2, 0.9]}\n";
    let p2 = b"{\"t\": [0.2, 0.9]}\n{\"t\": [0.5, 0.1]}\n";
    let by_pos = JsonlCompare {
        field: "t".into(),
        tol: 0.01,
    };
    assert!(by_pos.same(p1, p1));
    assert!(!by_pos.same(p1, p2));
}

// ---------------------------------------------------------------------------------------------
// Generators.

#[test]
fn generators_build_mandelbrot_tiles_and_indices() {
    let mut params = BTreeMap::new();
    params.insert("width".to_owned(), 100.0);
    params.insert("height".to_owned(), 50.0);
    params.insert("tile".to_owned(), 64.0);
    params.insert("max_iter".to_owned(), 80.0);
    let tiles = jobfile::generate("mandelbrot", 2, &params).expect("tiles");
    assert_eq!(tiles.len(), 2);
    let second = &tiles[1];
    assert_eq!(second.len(), 36);
    let f = |at: usize| f64::from_le_bytes(second[at..at + 8].try_into().expect("f64"));
    let u = |at: usize| u32::from_le_bytes(second[at..at + 4].try_into().expect("u32"));
    let step = 2.5 / 100.0;
    assert!((f(0) - (-2.0 + 64.0 * step)).abs() < 1e-12);
    assert!((f(8) - (-1.25)).abs() < 1e-12);
    assert!((f(16) - step).abs() < 1e-12);
    // The edge tile is cut to the image.
    assert_eq!((u(24), u(28), u(32)), (36, 50, 80));

    assert!(jobfile::generate("mandelbrot", 3, &params).is_err());
    params.insert("zoom".to_owned(), 2.0);
    assert!(jobfile::generate("mandelbrot", 2, &params).is_err());
    let idx = jobfile::generate("index", 3, &BTreeMap::new()).expect("index");
    assert_eq!(idx[2], 2u64.to_le_bytes().to_vec());
    assert!(jobfile::generate("nope", 1, &BTreeMap::new()).is_err());
}

// ---------------------------------------------------------------------------------------------
// Sweep planning.

#[test]
fn sweep_entries_compose_argv_env_and_rank_metrics() {
    let argv: Vec<String> = ["cargo", "run", "--", "s2-decide", "--minutes", "5"]
        .map(String::from)
        .to_vec();
    let mut env = BTreeMap::new();
    env.insert("RUST_LOG".to_owned(), "info".to_owned());
    env.insert("SEED".to_owned(), "1".to_owned());
    let mut extra = BTreeMap::new();
    extra.insert("SEED".to_owned(), "2".to_owned());
    let args = ["--qat-frac".to_owned(), "0.5".to_owned()];
    let (full, merged) = jobfile::sweep_command(&argv, &env, &args, &extra);
    assert_eq!(
        full,
        [
            "cargo",
            "run",
            "--",
            "s2-decide",
            "--minutes",
            "5",
            "--qat-frac",
            "0.5"
        ]
    );
    assert_eq!(merged.get("SEED").map(String::as_str), Some("2"));
    assert_eq!(merged.get("RUST_LOG").map(String::as_str), Some("info"));

    // The last JSON line of a log, or a JSON document with a nested `metrics` object.
    let log = b"{\"step\": 10, \"loss\": 2.0}\nnot json\n{\"step\": 20, \"final\": true, \"acc_f32\": 0.81, \"acc_ternary\": 0.78}\n";
    assert_eq!(
        jobfile::metrics(log),
        vec![
            ("acc_f32".to_owned(), 0.81),
            ("acc_ternary".to_owned(), 0.78)
        ]
    );
    let doc = b"{\"mode\": \"eval\", \"metrics\": {\"mean_score\": 12.5, \"kill_rate\": 0.1}}";
    assert_eq!(
        jobfile::metrics(doc),
        vec![
            ("mean_score".to_owned(), 12.5),
            ("kill_rate".to_owned(), 0.1)
        ]
    );
    assert!(jobfile::metrics(b"plain text").is_empty());

    let row = |pairs: &[(&str, f64)]| -> Vec<(String, f64)> {
        pairs.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect()
    };
    // Losses ascend; rows without the metric go last in their order.
    let rows = vec![
        row(&[("loss", 0.9)]),
        row(&[]),
        row(&[("loss", 0.3), ("accuracy", 0.5)]),
        row(&[("loss", 0.5)]),
    ];
    assert_eq!(jobfile::rank(&rows), vec![2, 3, 0, 1]);
    // Other metrics descend.
    let rows = vec![row(&[("acc_f32", 0.7)]), row(&[("acc_f32", 0.9)])];
    assert_eq!(jobfile::rank(&rows), vec![1, 0]);
    assert_eq!(jobfile::rank(&[row(&[]), row(&[])]), vec![0, 1]);
}

// ---------------------------------------------------------------------------------------------
// The binary.

const BIN: &str = env!("CARGO_BIN_EXE_nuee");
const LIMIT: Duration = Duration::from_secs(120);

struct Node {
    root: TempDir,
}

impl Node {
    fn new() -> Node {
        Node {
            root: tempfile::tempdir().expect("tempdir"),
        }
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    fn cmd(&self) -> Command {
        let home = self.path().join("home");
        std::fs::create_dir_all(&home).expect("home");
        let mut c = Command::new(BIN);
        c.current_dir(self.path())
            .env("HOME", home)
            .env("NUEE_CONFIG_DIR", self.path().join("config"))
            .env("NUEE_DATA_DIR", self.path().join("data"))
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env("NO_COLOR", "1");
        c
    }
}

struct Guard {
    child: Child,
    lines: mpsc::Receiver<String>,
}

impl Guard {
    fn spawn(mut cmd: Command) -> Guard {
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn nuee");
        let out = child.stdout.take().expect("stdout");
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Guard { child, lines }
    }

    fn line(&self, pred: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + LIMIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self
                .lines
                .recv_timeout(left)
                .expect("expected output before the time limit");
            if pred(&line) {
                return line;
            }
        }
    }

    fn rest(&self) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(l) = self.lines.recv_timeout(Duration::from_secs(5)) {
            out.push(l);
        }
        out
    }

    fn wait(&mut self) -> ExitStatus {
        let deadline = Instant::now() + LIMIT;
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                return status;
            }
            assert!(Instant::now() < deadline, "process did not exit in time");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            if let Err(e) = self.child.kill() {
                eprintln!("kill: {e}");
            }
            if let Err(e) = self.child.wait() {
                eprintln!("wait: {e}");
            }
        }
    }
}

fn is_event(line: &str, name: &str) -> bool {
    serde_json::from_str::<Value>(line).is_ok_and(|v| v["event"] == name)
}

fn event(line: &str) -> Value {
    serde_json::from_str(line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}"))
}

fn golden_module() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tasks/golden/mandelbrot.wasm")
}

fn mandel(x0: f64, y0: f64, step: f64, w: u32, h: u32, iter: u32) -> Vec<u8> {
    let mut v = Vec::new();
    for f in [x0, y0, step] {
        v.extend_from_slice(&f.to_le_bytes());
    }
    for u in [w, h, iter] {
        v.extend_from_slice(&u.to_le_bytes());
    }
    v
}

#[test]
fn wasm_job_file_runs_with_one_joiner_and_merges() {
    let relay_node = Node::new();
    let mut cmd = relay_node.cmd();
    cmd.args(["relay", "--dev", "--bind", "127.0.0.1:0"]);
    let relay = Guard::spawn(cmd);
    let url = relay
        .line(|l| l.starts_with("relay ready: "))
        .trim_start_matches("relay ready: ")
        .trim()
        .to_owned();

    let coord = Node::new();
    let job_dir = coord.path().join("job");
    std::fs::create_dir_all(job_dir.join("inputs")).expect("mkdir");
    std::fs::copy(golden_module(), job_dir.join("mandelbrot.wasm")).expect("module");
    let goldens = [
        (
            mandel(-2.0, -1.5, 3.0 / 256.0, 256, 256, 500),
            "4997e7fcca5edc96e1296d3f8819a9a5b7247a3bf3f953e4511f5b21b1080b59",
        ),
        (
            mandel(-0.743643887037151, 0.13182590420533, 1e-7, 64, 64, 2000),
            "34f54682a94f9f96256bf6bcf7718ed9fd6f23d4e4712fe31db36ca724152fa4",
        ),
        (
            mandel(0.0, 0.0, 1.0, 1, 1, 100),
            "d9dba825ecc31a0edc64d04c8535c66a73bc3e74c4c48df84da3272949e38f01",
        ),
    ];
    for (i, (input, _)) in goldens.iter().enumerate() {
        std::fs::write(job_dir.join(format!("inputs/{i}.bin")), input).expect("input");
    }
    std::fs::write(
        job_dir.join("job.toml"),
        r#"name = "golden"
kind = "wasm"
verify = "full"

[wasm]
module = "mandelbrot.wasm"

[inputs]
dir = "inputs"

[output]
dir = "out"
merge = "concat"
"#,
    )
    .expect("toml");

    let mut cmd = coord.cmd();
    cmd.args(["--relay", &url, "--json", "run"])
        .arg(job_dir.join("job.toml"))
        .args(["--local-slots", "1", "--wait-peers", "1"]);
    let mut run = Guard::spawn(cmd);
    let link = event(&run.line(|l| is_event(l, "link")))["link"]
        .as_str()
        .expect("link")
        .to_owned();

    let worker = Node::new();
    let mut cmd = worker.cmd();
    cmd.args(["--relay", &url, "--json", "join", &link, "--slots", "2"]);
    let mut join = Guard::spawn(cmd);

    let status = run.wait();
    let events = run.rest();
    assert!(status.success(), "run failed: {events:?}");
    let done = events
        .iter()
        .find(|l| is_event(l, "done"))
        .map(|l| event(l))
        .expect("done event");
    assert_eq!(done["tasks"], 3);
    let join_status = join.wait();
    assert!(join_status.success(), "join failed: {:?}", join.rest());

    let out = job_dir.join("out");
    let mut all = Vec::new();
    for (i, (_, want)) in goldens.iter().enumerate() {
        let bytes = std::fs::read(out.join(format!("task-{i}.bin"))).expect("output");
        assert_eq!(Hash::of(&bytes).to_string(), *want, "task {i}");
        all.extend_from_slice(&bytes);
    }
    assert_eq!(std::fs::read(out.join("golden.out")).expect("merged"), all);
    assert_eq!(
        done["merged"].as_str().map(PathBuf::from),
        Some(out.join("golden.out"))
    );
}

#[test]
fn job_files_are_checked_before_going_online() {
    let node = Node::new();
    let sweep = node.path().join("sweep.toml");
    std::fs::write(
        &sweep,
        r#"name = "s"
kind = "exec"

[exec]
repo = "https://github.com/maxgfr/skycmd"
rev = "781d4c176b2d6e301d0eeb82c69781b356505ec0"
argv = ["true"]

[[sweep]]
name = "a"
"#,
    )
    .expect("toml");
    // A sweep file is for `nuee sweep`.
    let out = node.cmd().arg("run").arg(&sweep).output().expect("run");
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("nuee sweep"));

    // `nuee sweep` needs entries and an exec job.
    let wasm = node.path().join("wasm.toml");
    std::fs::write(
        &wasm,
        "name = \"w\"\nkind = \"wasm\"\n[wasm]\nmodule = \"m.wasm\"\n[inputs]\ndir = \"in\"\n",
    )
    .expect("toml");
    let out = node.cmd().arg("sweep").arg(&wasm).output().expect("sweep");
    assert_eq!(out.status.code(), Some(2));

    // A sweep cannot shard its input.
    let sharded = node.path().join("sharded.toml");
    std::fs::write(
        &sharded,
        std::fs::read_to_string(&sweep).expect("read")
            + "\n[inputs]\njsonl = \"x.jsonl\"\nlines_per_task = 10\n",
    )
    .expect("toml");
    let out = node
        .cmd()
        .arg("sweep")
        .arg(&sharded)
        .output()
        .expect("sweep");
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn skycmd_examples_are_valid_job_files() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/skycmd");
    for (file, entries) in [("label.toml", 0), ("sweep.toml", 5)] {
        let text = std::fs::read_to_string(dir.join(file)).expect("example");
        let job =
            nuee_proto::JobToml::from_str_checked(&text).unwrap_or_else(|e| panic!("{file}: {e}"));
        assert_eq!(job.kind, nuee_proto::JobKind::Exec, "{file}");
        assert_eq!(job.sweep.len(), entries, "{file}");
        let exec = job.exec.expect("exec");
        assert_eq!(exec.rev, "781d4c176b2d6e301d0eeb82c69781b356505ec0");
    }

    let text = std::fs::read_to_string(dir.join("train-s4.toml")).expect("example");
    let job = nuee_proto::JobToml::from_str_checked(&text)
        .unwrap_or_else(|e| panic!("train-s4.toml: {e}"));
    assert_eq!(job.kind, nuee_proto::JobKind::Train);
    let train = job.train.expect("train");
    assert_eq!(train.trainer, nuee_proto::job_toml::TrainerChoice::Exec);
    assert_eq!(
        (train.rounds, train.inner_steps, train.workers),
        (40, 500, 4)
    );
    let exec = job.exec.expect("exec");
    assert!(exec.gpu);
    assert_eq!(exec.repo, "https://github.com/maxgfr/skycmd");

    let text = std::fs::read_to_string(dir.join("dagger.toml")).expect("example");
    let job =
        nuee_proto::JobToml::from_str_checked(&text).unwrap_or_else(|e| panic!("dagger.toml: {e}"));
    assert_eq!(job.kind, nuee_proto::JobKind::Wasm);
    let wasm = job.wasm.expect("wasm");
    assert_eq!(wasm.assets.len(), 1);
    let model = std::fs::read(dir.join(&wasm.assets[0])).expect("the model asset exists");
    assert_eq!(
        nuee_proto::Hash::of(&model).to_string(),
        "1b0963f9f364246e01fdf2dc84b7383d95f05dda252103c4a7b9de8e2c0b1d11"
    );
    let inputs = job.inputs.expect("inputs");
    assert_eq!(inputs.lines_per_task, Some(1));
    let requests =
        std::fs::read_to_string(dir.join(inputs.jsonl.expect("jsonl"))).expect("requests");
    let requests: Vec<serde_json::Value> = requests
        .lines()
        .map(|l| serde_json::from_str(l).expect("json line"))
        .collect();
    assert_eq!(requests.len(), 16);
    assert!(
        requests
            .iter()
            .all(|r| r["mode"] == "dagger" && r["n"] == 4000)
    );
    assert_eq!(
        job.output.and_then(|o| o.merge),
        Some(nuee_proto::job_toml::MergeKind::Jsonl)
    );
}

/// A mock OpenAI-compatible completion server: the letter after "pick " in the prompt gets
/// log-probability -0.5 (as " X"), every other letter of A..D gets -(index + 2).
fn mock_llm() -> String {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let Ok(clone) = stream.try_clone() else {
                continue;
            };
            let mut reader = BufReader::new(clone);
            let mut len = 0usize;
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).is_err() || h.trim_end().is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':')
                    && k.eq_ignore_ascii_case("content-length")
                {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; len];
            if reader.read_exact(&mut body).is_err() {
                continue;
            }
            let req: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let prompt = req["prompt"].as_str().unwrap_or_default();
            let best = prompt
                .split("pick ")
                .nth(1)
                .and_then(|s| s.chars().next())
                .unwrap_or('A');
            let mut top = serde_json::Map::new();
            for (i, l) in "ABCD".chars().enumerate() {
                let lp = if l == best { -0.5 } else { -(i as f64 + 2.0) };
                top.insert(format!(" {l}"), Value::from(lp));
            }
            let resp = serde_json::json!({
                "choices": [{ "text": " A", "logprobs": { "top_logprobs": [top] } }]
            })
            .to_string();
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                resp.len()
            );
            if stream.write_all(head.as_bytes()).is_err()
                || stream.write_all(resp.as_bytes()).is_err()
            {
                eprintln!("mock llm: client went away");
            }
        }
    });
    url
}

#[test]
fn llm_job_file_scores_records_through_a_joiner_endpoint() {
    let relay_node = Node::new();
    let mut cmd = relay_node.cmd();
    cmd.args(["relay", "--dev", "--bind", "127.0.0.1:0"]);
    let relay = Guard::spawn(cmd);
    let url = relay
        .line(|l| l.starts_with("relay ready: "))
        .trim_start_matches("relay ready: ")
        .trim()
        .to_owned();
    let llm = mock_llm();

    let coord = Node::new();
    let job_dir = coord.path().join("job");
    std::fs::create_dir_all(&job_dir).expect("mkdir");
    let picks = ['C', 'A', 'B', 'D', 'B'];
    let mut lines = String::new();
    for (i, p) in picks.iter().enumerate() {
        lines.push_str(
            &serde_json::json!({
                "t": "choice",
                "ins": format!("record {i}: pick {p}"),
                "opts": ["a", "b", "c", "d"],
                "state": format!("step {i}"),
            })
            .to_string(),
        );
        lines.push('\n');
    }
    std::fs::write(job_dir.join("recs.jsonl"), lines).expect("records");
    std::fs::write(
        job_dir.join("job.toml"),
        r#"name = "scores"
kind = "llm"
verify = "none"

[llm]
model = "qwen3.5-0.8b"
letters = "ABCD"

[inputs]
jsonl = "recs.jsonl"
lines_per_task = 3

[compare]
kind = "logits"
max_gap = 0.25

[output]
dir = "out"
merge = "jsonl"
uid_field = "uid"
"#,
    )
    .expect("toml");

    let mut cmd = coord.cmd();
    cmd.args(["--relay", &url, "--json", "run"])
        .arg(job_dir.join("job.toml"))
        .args(["--wait-peers", "1"]);
    let mut run = Guard::spawn(cmd);
    let link = event(&run.line(|l| is_event(l, "link")))["link"]
        .as_str()
        .expect("link")
        .to_owned();

    let worker = Node::new();
    let mut cmd = worker.cmd();
    cmd.args(["--relay", &url, "--json", "join", &link, "--llm", &llm]);
    let mut join = Guard::spawn(cmd);

    let status = run.wait();
    let events = run.rest();
    assert!(status.success(), "run failed: {events:?}");
    let done = events
        .iter()
        .find(|l| is_event(l, "done"))
        .map(|l| event(l))
        .expect("done event");
    assert_eq!(done["tasks"], 2);
    let join_status = join.wait();
    assert!(join_status.success(), "join failed: {:?}", join.rest());

    let merged = std::fs::read_to_string(job_dir.join("out/scores.jsonl")).expect("merged");
    let mut want = String::new();
    for (i, p) in picks.iter().enumerate() {
        let scores: Vec<String> = "ABCD"
            .chars()
            .enumerate()
            .map(|(k, l)| {
                if l == *p {
                    "-0.5".to_owned()
                } else {
                    format!("{:.1}", -(k as f64 + 2.0))
                }
            })
            .collect();
        want.push_str(&format!(
            "{{\"uid\":{i},\"tlogits\":[{}]}}\n",
            scores.join(",")
        ));
    }
    assert_eq!(merged, want);
}

// ---------------------------------------------------------------------------------------------
// Embedding and WGSL job files.

/// The pinned MiniLM, downloaded once into `target/test-models/` and linked into `node`'s data
/// directory so the processes under test find it without downloading.
fn seed_model(node: &Node) {
    use nuee_gpu::embed::MINILM;
    let shared = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-models");
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(MINILM.fetch(&shared))
        .expect("download the pinned model");
    let to = MINILM.dir(&node.path().join("data/models"));
    std::fs::create_dir_all(&to).expect("mkdir");
    for f in MINILM.files {
        std::fs::hard_link(MINILM.dir(&shared).join(f.path), to.join(f.path))
            .or_else(|_| {
                std::fs::copy(MINILM.dir(&shared).join(f.path), to.join(f.path)).map(|_| ())
            })
            .expect("seed the model");
    }
}

fn start_relay() -> (Guard, String) {
    let node = Node::new();
    let mut cmd = node.cmd();
    cmd.args(["relay", "--dev", "--bind", "127.0.0.1:0"]);
    let relay = Guard::spawn(cmd);
    let url = relay
        .line(|l| l.starts_with("relay ready: "))
        .trim_start_matches("relay ready: ")
        .trim()
        .to_owned();
    // The relay's temporary directory lives as long as the process.
    std::mem::forget(node);
    (relay, url)
}

fn done_event(run: &mut Guard) -> Value {
    let status = run.wait();
    let events = run.rest();
    assert!(status.success(), "run failed: {events:?}");
    events
        .iter()
        .find(|l| is_event(l, "done"))
        .map(|l| event(l))
        .expect("done event")
}

fn read_vectors(path: &Path) -> Vec<f32> {
    let bytes = std::fs::read(path).expect("vectors.f32");
    assert_eq!(bytes.len() % (384 * 4), 0);
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect()
}

#[test]
fn embed_and_wgsl_job_files_are_checked_before_going_online() {
    let node = Node::new();
    let dir = node.path();
    std::fs::write(dir.join("t.jsonl"), "hello\n").expect("jsonl");
    std::fs::create_dir_all(dir.join("in")).expect("mkdir");
    std::fs::write(dir.join("in/0.bin"), b"not an NWG1 input").expect("input");
    std::fs::write(dir.join("s.wgsl"), "this is not WGSL").expect("shader");
    let cases = [
        // An unknown model.
        "name = \"e\"\nkind = \"embed\"\n[embed]\nmodel = \"bert-large\"\n[inputs]\njsonl = \"t.jsonl\"\n",
        // Another token limit than the calibrated one.
        "name = \"e\"\nkind = \"embed\"\n[embed]\nmodel = \"all-MiniLM-L6-v2\"\nmax_tokens = 512\n[inputs]\njsonl = \"t.jsonl\"\n",
        // Embeddings are compared by cosine.
        "name = \"e\"\nkind = \"embed\"\n[embed]\nmodel = \"all-MiniLM-L6-v2\"\n[inputs]\njsonl = \"t.jsonl\"\n[compare]\nkind = \"tolerance\"\nabs = 0.1\n",
        // No text source.
        "name = \"e\"\nkind = \"embed\"\n[embed]\nmodel = \"all-MiniLM-L6-v2\"\n[inputs]\ngenerate = \"indices\"\ncount = 3\n",
        // Entry points belong to the inputs.
        "name = \"w\"\nkind = \"wgsl\"\n[wgsl]\nshader = \"s.wgsl\"\nentry = \"main\"\n[inputs]\ndir = \"in\"\n",
        // Inputs must be NWG1.
        "name = \"w\"\nkind = \"wgsl\"\n[wgsl]\nshader = \"s.wgsl\"\n[inputs]\ndir = \"in\"\n",
    ];
    for (i, toml) in cases.iter().enumerate() {
        let path = dir.join(format!("job{i}.toml"));
        std::fs::write(&path, toml).expect("toml");
        let out = node.cmd().arg("run").arg(&path).output().expect("run");
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "case {i}: {err}");
        assert!(!err.contains("link"), "case {i} went online: {err}");
    }

    // A valid NWG1 input with a broken shader is refused too.
    let input = nuee_gpu::wgsl::mandelbrot_input(-2.0, -1.0, 0.01, 8, 8, 50);
    std::fs::write(dir.join("in/0.bin"), input).expect("input");
    let out = node
        .cmd()
        .arg("run")
        .arg(dir.join("job5.toml"))
        .output()
        .expect("run");
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn embed_verb_writes_vectors_and_meta_with_a_joiner_and_resumes() {
    let (_relay, url) = start_relay();
    let coord = Node::new();
    seed_model(&coord);
    let docs = coord.path().join("docs");
    std::fs::create_dir_all(docs.join("sub")).expect("mkdir");
    let long: String = (0..60)
        .map(|i| format!("Sentence {i} talks about distributed computing with volunteers. "))
        .collect();
    std::fs::write(docs.join("a.txt"), "The cat sits on the mat.").expect("a");
    std::fs::write(
        docs.join("sub/b.md"),
        format!("# Notes\n\n{long}\n\nLe chat dort sur le canapé."),
    )
    .expect("b");
    std::fs::write(docs.join("blob.bin"), [0u8, 159, 146, 150, 0, 1]).expect("binary");
    let out_dir = coord.path().join("vectors");

    let mut cmd = coord.cmd();
    cmd.args(["--relay", &url, "--json", "embed"])
        .arg(&docs)
        .arg("--out")
        .arg(&out_dir)
        // Every lease goes to the joiner's candle runner.
        .args(["--local-slots", "0", "--wait-peers", "1"]);
    let mut run = Guard::spawn(cmd);
    let link = event(&run.line(|l| is_event(l, "link")))["link"]
        .as_str()
        .expect("link")
        .to_owned();
    let worker = Node::new();
    seed_model(&worker);
    let mut cmd = worker.cmd();
    cmd.args(["--relay", &url, "--json", "join", &link, "--slots", "1"]);
    let mut join = Guard::spawn(cmd);

    let done = done_event(&mut run);
    assert!(join.wait().success(), "join failed: {:?}", join.rest());
    let n = done["chunks"].as_u64().expect("chunks") as usize;
    assert!(n >= 4, "{done}");
    assert_eq!(done["tasks"], 1);

    let vectors = read_vectors(&out_dir.join("vectors.f32"));
    assert_eq!(vectors.len(), n * 384);
    let meta = std::fs::read_to_string(out_dir.join("meta.jsonl")).expect("meta");
    let rows: Vec<Value> = meta.lines().map(event).collect();
    assert_eq!(rows.len(), n);
    for (i, r) in rows.iter().enumerate() {
        assert_eq!(r["id"], i as u64);
        assert!(r["start"].as_u64() < r["end"].as_u64(), "{r}");
        assert!(!r["path"].as_str().expect("path").ends_with("blob.bin"));
    }
    assert!(rows[0]["path"].as_str().expect("path").ends_with("a.txt"));
    assert!(
        rows.iter().any(|r| r["chunk"].as_u64() > Some(0)),
        "the long file is chunked"
    );
    let embedder = nuee_gpu::embed::Embedder::load(
        &coord.path().join("data/models"),
        nuee_gpu::embed::EmbedDevice::Cpu,
    )
    .expect("model");
    let first = embedder
        .embed(&["The cat sits on the mat."])
        .expect("embed");
    let c = nuee_gpu::embed::cosine(&first, &vectors[..384]).expect("cosine");
    assert!(c >= nuee_gpu::embed::TAU, "{c}");

    // Run again: every task comes from the resume file, the files are rewritten the same.
    let before = std::fs::read(out_dir.join("vectors.f32")).expect("vectors");
    let mut cmd = coord.cmd();
    cmd.args(["--relay", &url, "--json", "embed"])
        .arg(&docs)
        .arg("--out")
        .arg(&out_dir)
        .args(["--local-slots", "0"]);
    let mut again = Guard::spawn(cmd);
    let done = done_event(&mut again);
    assert_eq!(done["chunks"].as_u64(), Some(n as u64));
    assert_eq!(
        std::fs::read(out_dir.join("vectors.f32")).expect("vectors"),
        before
    );
}

#[test]
fn embed_job_file_reads_jsonl_texts() {
    let (_relay, url) = start_relay();
    let coord = Node::new();
    seed_model(&coord);
    let job_dir = coord.path().join("job");
    std::fs::create_dir_all(&job_dir).expect("mkdir");
    std::fs::write(
        job_dir.join("texts.jsonl"),
        "{\"text\": \"How do I reset my password?\", \"id\": 7}\nI forgot my password\n\n\"a JSON string line\"\n",
    )
    .expect("jsonl");
    std::fs::write(
        job_dir.join("job.toml"),
        r#"name = "faq"
kind = "embed"

[embed]
model = "all-MiniLM-L6-v2"

[inputs]
jsonl = "texts.jsonl"

[output]
dir = "out"

[compare]
kind = "cosine"
min_cos = 0.95
"#,
    )
    .expect("toml");
    let mut cmd = coord.cmd();
    cmd.args(["--relay", &url, "--json", "run"])
        .arg(job_dir.join("job.toml"))
        .args(["--local-slots", "1"]);
    let mut run = Guard::spawn(cmd);
    let done = done_event(&mut run);
    assert_eq!(done["chunks"], 3);
    let out = job_dir.join("out");
    let vectors = read_vectors(&out.join("vectors.f32"));
    assert_eq!(vectors.len(), 3 * 384);
    let rows: Vec<Value> = std::fs::read_to_string(out.join("meta.jsonl"))
        .expect("meta")
        .lines()
        .map(event)
        .collect();
    let lines: Vec<u64> = rows
        .iter()
        .map(|r| r["line"].as_u64().expect("line"))
        .collect();
    assert_eq!(lines, [0, 1, 3]);
    let c = nuee_gpu::embed::cosine(&vectors[..384], &vectors[384..768]).expect("cosine");
    assert!(c > 0.6, "paraphrases: {c}");
}

#[test]
fn wgsl_job_file_runs_on_the_local_gpu() {
    let (_relay, url) = start_relay();
    let coord = Node::new();
    let job_dir = coord.path().join("job");
    std::fs::create_dir_all(job_dir.join("in")).expect("mkdir");
    std::fs::write(job_dir.join("m.wgsl"), nuee_gpu::wgsl::MANDELBROT_WGSL).expect("shader");
    let tiles = [(-2.0f32, -1.25f32), (-0.75, 0.0)];
    for (i, (x, y)) in tiles.iter().enumerate() {
        let input = nuee_gpu::wgsl::mandelbrot_input(*x, *y, 2.5 / 64.0, 16, 16, 200);
        std::fs::write(job_dir.join(format!("in/{i}.bin")), input).expect("input");
    }
    std::fs::write(
        job_dir.join("job.toml"),
        r#"name = "tiles"
kind = "wgsl"

[wgsl]
shader = "m.wgsl"

[inputs]
dir = "in"

[output]
dir = "out"
merge = "concat"

[compare]
kind = "tolerance"
abs = 1.0
max_bad = 0.01
"#,
    )
    .expect("toml");
    let mut cmd = coord.cmd();
    cmd.args(["--relay", &url, "--json", "run"])
        .arg(job_dir.join("job.toml"))
        .args(["--local-slots", "1"]);
    let mut run = Guard::spawn(cmd);
    let done = done_event(&mut run);
    assert_eq!(done["tasks"], 2);
    let out = job_dir.join("out");
    let mut all = Vec::new();
    for i in 0..2 {
        let bytes = std::fs::read(out.join(format!("task-{i}.bin"))).expect("output");
        assert_eq!(bytes.len(), 16 * 16 * 4, "u32 counts per pixel");
        all.extend_from_slice(&bytes);
    }
    assert_eq!(std::fs::read(out.join("tiles.out")).expect("merged"), all);
}
