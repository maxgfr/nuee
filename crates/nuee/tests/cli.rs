//! End-to-end tests of the `nuee` binary. Each test has its own HOME, config and data
//! directories; networked tests go through a local `nuee relay --dev` child process.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_nuee");
const LIMIT: Duration = Duration::from_secs(120);

/// Isolated directories for one node.
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

    fn home(&self) -> PathBuf {
        self.path().join("home")
    }

    fn config(&self) -> PathBuf {
        self.path().join("config")
    }

    /// A `nuee` command running in this node's directories.
    fn cmd(&self) -> Command {
        std::fs::create_dir_all(self.home()).expect("home");
        let mut c = Command::new(BIN);
        c.current_dir(self.path())
            .env("HOME", self.home())
            .env("NUEE_CONFIG_DIR", self.config())
            .env("NUEE_DATA_DIR", self.path().join("data"))
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env("NO_COLOR", "1");
        c
    }
}

/// A child process killed when the test ends, whatever happens.
struct Guard {
    child: Child,
    lines: mpsc::Receiver<String>,
}

/// Forwards every line of `from` to a channel, on a thread.
fn line_reader(from: impl std::io::Read + Send + 'static) -> mpsc::Receiver<String> {
    let (tx, lines) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(from).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    lines
}

impl Guard {
    fn spawn(mut cmd: Command) -> Guard {
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn nuee");
        let lines = line_reader(child.stdout.take().expect("stdout"));
        Guard { child, lines }
    }

    /// Like [`Guard::spawn`], with stderr piped too: its lines come on the returned channel.
    fn spawn_piped(mut cmd: Command) -> (Guard, mpsc::Receiver<String>) {
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn nuee");
        let lines = line_reader(child.stdout.take().expect("stdout"));
        let errs = line_reader(child.stderr.take().expect("stderr"));
        (Guard { child, lines }, errs)
    }

    /// The next stdout line matching `pred`, within the time limit.
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

    /// Every remaining stdout line (the process must have exited).
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

/// A local relay and its URL.
fn relay(node: &Node) -> (Guard, String) {
    let mut cmd = node.cmd();
    cmd.args(["relay", "--dev", "--bind", "127.0.0.1:0"]);
    let g = Guard::spawn(cmd);
    let line = g.line(|l| l.starts_with("relay ready: "));
    let url = line.trim_start_matches("relay ready: ").trim().to_owned();
    assert!(url.starts_with("http://127.0.0.1:"), "{url}");
    (g, url)
}

fn event(line: &str) -> Value {
    serde_json::from_str(line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}"))
}

fn is_event(line: &str, name: &str) -> bool {
    serde_json::from_str::<Value>(line).is_ok_and(|v| v["event"] == name)
}

/// Runs `cmd` with `stdin` text to completion.
fn run_with_stdin(mut cmd: Command, stdin: &str) -> std::process::Output {
    use std::io::Write;
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        if tx.send(child.wait_with_output()).is_err() {
            eprintln!("test gave up waiting");
        }
    });
    rx.recv_timeout(LIMIT)
        .expect("process finished in time")
        .expect("output")
}

#[cfg(unix)]
fn mode(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).expect("metadata").permissions().mode() & 0o777
}

#[test]
fn version() {
    let node = Node::new();
    let out = node.cmd().arg("--version").output().expect("run");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.starts_with("nuee "), "{text}");
}

#[test]
fn help_lists_every_verb_and_exit_code() {
    let node = Node::new();
    let out = node.cmd().arg("--help").output().expect("run");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    for verb in [
        "run",
        "join",
        "share",
        "get",
        "pin",
        "serve",
        "train",
        "sweep",
        "embed",
        "expose",
        "connect",
        "site",
        "relay",
        "doctor",
        "selftest",
        "id",
        "completions",
    ] {
        assert!(
            text.contains(&format!("  {verb} ")),
            "{verb} missing:\n{text}"
        );
    }
    assert!(text.contains("8  swarm ended by the coordinator"), "{text}");
}

#[test]
fn selftest_passes_without_writing_home() {
    let node = Node::new();
    let mut cmd = node.cmd();
    cmd.env_remove("NUEE_CONFIG_DIR")
        .env_remove("NUEE_DATA_DIR")
        .arg("selftest");
    let out = cmd.output().expect("run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout.contains("selftest ok"), "{stdout}");
    let left: Vec<_> = std::fs::read_dir(node.home())
        .expect("home")
        .map(|e| e.expect("entry").path())
        .collect();
    assert!(left.is_empty(), "files created in HOME: {left:?}");
    assert!(!node.config().exists());
}

#[cfg(unix)]
#[test]
fn id_creates_a_private_key_and_refuses_a_readable_one() {
    use std::os::unix::fs::PermissionsExt;
    let node = Node::new();
    let first = node.cmd().arg("id").output().expect("run");
    assert!(first.status.success());
    let key = node.config().join("secret.key");
    assert_eq!(mode(&node.config()), 0o700);
    assert_eq!(mode(&key), 0o600);
    assert_eq!(std::fs::read(&key).expect("key").len(), 32);

    // Same identity on the next run.
    let again = node.cmd().arg("id").output().expect("run");
    assert_eq!(first.stdout, again.stdout);

    // --new does not replace it without --force.
    let refused = node.cmd().args(["id", "--new"]).output().expect("run");
    assert_eq!(refused.status.code(), Some(1));
    let forced = node
        .cmd()
        .args(["id", "--new", "--force"])
        .output()
        .expect("run");
    assert!(forced.status.success());
    assert_ne!(first.stdout, forced.stdout);
    assert_eq!(mode(&key), 0o600);

    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let out = node.cmd().arg("id").output().expect("run");
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("chmod 600"), "{err}");
}

#[test]
fn completions_for_zsh() {
    let node = Node::new();
    let out = node
        .cmd()
        .args(["completions", "zsh"])
        .output()
        .expect("run");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("#compdef nuee"), "{text}");
}

#[test]
fn embed_verb_explains_its_use_and_refuses_an_empty_directory() {
    let node = Node::new();
    let out = node.cmd().args(["embed", "--help"]).output().expect("run");
    assert_eq!(out.status.code(), Some(0));
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("--out") && help.contains("PATH"), "{help}");

    let empty = node.path().join("empty");
    std::fs::create_dir_all(&empty).expect("mkdir");
    let out = node.cmd().arg("embed").arg(&empty).output().expect("run");
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no text to embed"), "{err}");

    let out = node.cmd().arg("embed").output().expect("run");
    assert_eq!(out.status.code(), Some(2), "a path is required");
}

#[test]
fn truncated_link_is_a_usage_error() {
    let node = Node::new();
    let out = node
        .cmd()
        .args([
            "get",
            "https://maxgfr.github.io/nuee/#/F/AEBAGBAFAYDQQCIKBM",
        ])
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("truncated"), "{err}");

    let out = node
        .cmd()
        .args(["--json", "join", "#/J/AE"])
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(2));
    let ev = event(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .last()
            .expect("event"),
    );
    assert_eq!(ev["event"], "error");
    assert_eq!(ev["code"], 2);
}

#[test]
fn unreachable_relay_exits_3() {
    let node = Node::new();
    let out = node
        .cmd()
        .env("NUEE_ONLINE_TIMEOUT_MS", "1500")
        .args(["--relay", "http://127.0.0.1:9", "run", "demo:mandelbrot"])
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(3));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("relay unreachable"), "{err}");
    assert!(err.contains("--relay"), "{err}");
}

#[test]
fn mandelbrot_with_one_joiner() {
    let relay_node = Node::new();
    let (_relay, url) = relay(&relay_node);

    let coord = Node::new();
    let out_dir = coord.path().join("out");
    let mut cmd = coord.cmd();
    cmd.args(["--relay", &url, "--json", "run", "demo:mandelbrot"])
        .args([
            "--verify",
            "full",
            "--local-slots",
            "1",
            "--wait-peers",
            "1",
        ])
        .arg("--out")
        .arg(&out_dir);
    let mut run = Guard::spawn(cmd);
    let link_line = run.line(|l| is_event(l, "link"));
    let link = event(&link_line);
    assert_eq!(link["kind"], "job");
    assert_eq!(
        link["fingerprint"].as_array().expect("fingerprint").len(),
        6
    );
    let link = link["link"].as_str().expect("link").to_owned();

    let worker = Node::new();
    let mut cmd = worker.cmd();
    cmd.args(["--relay", &url, "--json", "join", "-", "--slots", "2"]);
    let joined = run_with_stdin(cmd, &format!("{link}\n"));
    let join_out = String::from_utf8_lossy(&joined.stdout);
    assert!(
        joined.status.success(),
        "join failed: {join_out}\n{}",
        String::from_utf8_lossy(&joined.stderr)
    );
    let ran = join_out
        .lines()
        .filter(|l| is_event(l, "task"))
        .filter(|l| event(l)["ok"] == true)
        .count();
    assert!(ran > 0, "the joiner ran no task:\n{join_out}");

    let status = run.wait();
    let events = run.rest();
    assert!(status.success(), "run failed: {events:?}");
    let done = events
        .iter()
        .find(|l| is_event(l, "done"))
        .map(|l| event(l))
        .expect("done event");
    assert_eq!(done["tasks"], 256);

    let outputs = (0..256)
        .filter(|i| out_dir.join(format!("task-{i}.bin")).is_file())
        .count();
    assert_eq!(outputs, 256);
    let ppm = std::fs::read(out_dir.join("mandelbrot.ppm")).expect("ppm");
    assert!(ppm.starts_with(b"P6\n1024 1024\n255\n"));
    assert_eq!(ppm.len(), "P6\n1024 1024\n255\n".len() + 1024 * 1024 * 3);
    let results: Value =
        serde_json::from_slice(&std::fs::read(out_dir.join("results.json")).expect("results"))
            .expect("json");
    assert_eq!(results["outputs"].as_array().expect("outputs").len(), 256);

    // Every accepted task carries the time the coordinator measured, lease to result.
    let timed: Vec<Value> = events
        .iter()
        .filter(|l| is_event(l, "task"))
        .map(|l| event(l))
        .collect();
    let tasks: std::collections::BTreeSet<u64> =
        timed.iter().filter_map(|e| e["task"].as_u64()).collect();
    assert_eq!(tasks.len(), 256, "one task event per task at least");
    assert!(
        timed.iter().all(|e| e["ms"].as_u64().is_some()),
        "task events carry ms: {timed:?}"
    );
}

#[test]
fn a_coordinator_that_cannot_run_the_kind_says_only_peers_check() {
    let node = Node::new();
    let job = node.path().join("job");
    std::fs::create_dir_all(&job).expect("mkdir");
    std::fs::write(
        job.join("recs.jsonl"),
        "{\"t\":\"choice\",\"ins\":\"pick\",\"opts\":[\"a\",\"b\"],\"state\":\"s\"}\n",
    )
    .expect("records");
    std::fs::write(
        job.join("job.toml"),
        r#"name = "scores"
kind = "llm"
verify = "sample"

[llm]
model = "qwen3.5-0.8b"
letters = "AB"

[inputs]
jsonl = "recs.jsonl"

[compare]
kind = "logits"
max_gap = 0.25
"#,
    )
    .expect("toml");
    let out = node
        .cmd()
        .env("NUEE_ONLINE_TIMEOUT_MS", "1500")
        .args(["--relay", "http://127.0.0.1:9", "--json", "run"])
        .arg(job.join("job.toml"))
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(3));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let guarantee = stdout
        .lines()
        .find(|l| is_event(l, "guarantee"))
        .map(event)
        .unwrap_or_else(|| panic!("no guarantee event: {stdout}"));
    assert_eq!(guarantee["coordinator_checks"], false);

    // A wasm job is checked here even with --local-slots 0: no such warning.
    let out = node
        .cmd()
        .env("NUEE_ONLINE_TIMEOUT_MS", "1500")
        .args([
            "--relay",
            "http://127.0.0.1:9",
            "--json",
            "run",
            "demo:mandelbrot",
        ])
        .args(["--local-slots", "0"])
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(3));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stdout.lines().any(|l| is_event(l, "guarantee")),
        "{stdout}"
    );

    let help = node.cmd().args(["run", "--help"]).output().expect("help");
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("still recomputes"), "{help}");
}

#[test]
fn share_a_folder_and_get_it_from_stdin() {
    let relay_node = Node::new();
    let (_relay, url) = relay(&relay_node);

    let sharer = Node::new();
    let src = sharer.path().join("dossier");
    std::fs::create_dir_all(src.join("sous")).expect("mkdir");
    std::fs::write(src.join("a.txt"), b"bonjour").expect("write");
    let blob: Vec<u8> = (0..300_000u32).map(|i| (i * 31 % 251) as u8).collect();
    std::fs::write(src.join("sous/b.bin"), &blob).expect("write");
    let mut cmd = sharer.cmd();
    cmd.args(["--relay", &url, "--json", "share"]).arg(&src);
    let share = Guard::spawn(cmd);
    let link = event(&share.line(|l| is_event(l, "link")));
    assert_eq!(link["kind"], "file");
    let link = link["link"].as_str().expect("link").to_owned();

    let getter = Node::new();
    let dest = getter.path().join("dl");
    let mut cmd = getter.cmd();
    cmd.args(["--relay", &url, "get", "-", "-o"]).arg(&dest);
    let got = run_with_stdin(cmd, &format!("{link}\n"));
    let stdout = String::from_utf8_lossy(&got.stdout);
    assert!(
        got.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&got.stderr)
    );
    assert!(stdout.contains("verified (BLAKE3)"), "{stdout}");
    assert_eq!(
        std::fs::read(dest.join("dossier/a.txt")).expect("a"),
        b"bonjour"
    );
    assert_eq!(
        std::fs::read(dest.join("dossier/sous/b.bin")).expect("b"),
        blob
    );
}

#[test]
fn pin_rm_drops_the_store_tag() {
    use nuee_proto::{BoundedVec, Hash, PinLink};
    use nuee_store::{PinEntry, Registry, pin_tag};

    let node = Node::new();
    let data = node.path().join("data");
    let bytes = b"pinned ciphertext".to_vec();
    let hash = Hash::of(&bytes);
    let rt = tokio::runtime::Runtime::new().expect("runtime");

    // What `nuee pin` leaves behind: the blob, its tag, and a registry entry.
    rt.block_on(async {
        nuee_store::ensure_private_dir(&data).expect("data dir");
        let blobs = data.join("blobs");
        nuee_store::ensure_private_dir(&blobs).expect("blobs dir");
        let fs = iroh_blobs::store::fs::FsStore::load(&blobs)
            .await
            .expect("store");
        let tag = fs.add_bytes(bytes.clone()).await.expect("add");
        assert_eq!(tag.hash.as_bytes(), hash.as_bytes());
        fs.tags()
            .set(pin_tag(hash), tag.hash_and_format())
            .await
            .expect("tag");
        fs.shutdown().await.expect("shutdown");
    });
    let mut reg = Registry::load(&Registry::default_path(&data)).expect("registry");
    reg.add(PinEntry::from_link(
        &PinLink {
            providers: BoundedVec::new(),
            hash,
            size: bytes.len() as u64,
            access: [7; 16],
        },
        0,
    ));
    reg.save().expect("save registry");

    let out = node
        .cmd()
        .args(["pin", "--rm", &hash.to_string()])
        .output()
        .expect("run");
    assert!(out.status.success(), "{out:?}");

    rt.block_on(async {
        let fs = iroh_blobs::store::fs::FsStore::load(data.join("blobs"))
            .await
            .expect("store");
        let tag = fs.tags().get(pin_tag(hash)).await.expect("tags");
        assert!(tag.is_none(), "the pin tag survived: {tag:?}");
        fs.shutdown().await.expect("shutdown");
    });
    let reg = Registry::load(&Registry::default_path(&data)).expect("registry");
    assert!(reg.list().is_empty());

    // Unpinning a hash that is not pinned keeps its exit code.
    let out = node
        .cmd()
        .args(["pin", "--rm", &hash.to_string()])
        .output()
        .expect("run");
    assert_eq!(out.status.code(), Some(1), "{out:?}");
}

#[test]
fn doctor_probes_the_gpu_instead_of_claiming_it() {
    let node = Node::new();
    // An unreachable relay fails the relay check quickly; the GPU line is what this test reads.
    let out = node
        .cmd()
        .args(["--relay", "http://127.0.0.1:9", "--json", "doctor"])
        .output()
        .expect("run");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let gpu: Value = stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["event"] == "check" && v["name"] == "gpu")
        .unwrap_or_else(|| panic!("no gpu check in:\n{stdout}"));
    let detail = gpu["detail"].as_str().expect("detail");
    assert!(!detail.contains("not checked"), "{gpu}");
    assert!(!detail.is_empty(), "{gpu}");
    if gpu["ok"] == Value::Bool(false) {
        assert!(detail.contains("GPU tasks are off"), "{gpu}");
        assert!(gpu["hint"].is_string(), "{gpu}");
    }
}

/// Every line left on a stderr channel (the process must have exited).
fn drain(errs: &mpsc::Receiver<String>) -> Vec<String> {
    let mut out = Vec::new();
    while let Ok(l) = errs.recv_timeout(Duration::from_secs(5)) {
        out.push(l);
    }
    out
}

fn assert_no_error_log(who: &str, stderr: &[String]) {
    let errors: Vec<&String> = stderr.iter().filter(|l| l.contains(" ERROR ")).collect();
    assert!(
        errors.is_empty(),
        "{who} logged errors: {errors:?}\nfull stderr:\n{}",
        stderr.join("\n")
    );
}

/// Human mode with piped output: lines printed above the (hidden) progress bar still show,
/// and neither side ends with an error log about an endpoint dropped without being closed.
#[test]
fn piped_human_run_and_join_print_peers_and_close_their_endpoints() {
    let relay_node = Node::new();
    let (_relay, url) = relay(&relay_node);

    let coord = Node::new();
    let mut cmd = coord.cmd();
    cmd.args(["--relay", &url, "run", "demo:mandelbrot"])
        .args(["--local-slots", "1", "--wait-peers", "1"])
        .arg("--out")
        .arg(coord.path().join("out"));
    let (mut run, run_errs) = Guard::spawn_piped(cmd);
    let link = run.line(|l| l.contains("#/J/")).trim().to_owned();

    let worker = Node::new();
    let mut cmd = worker.cmd();
    cmd.args(["--relay", &url, "join", "-", "--slots", "2"]);
    let joined = run_with_stdin(cmd, &format!("{link}\n"));
    let join_err: Vec<String> = String::from_utf8_lossy(&joined.stderr)
        .lines()
        .map(str::to_owned)
        .collect();
    assert!(
        joined.status.success(),
        "join failed: {}\n{}",
        String::from_utf8_lossy(&joined.stdout),
        join_err.join("\n")
    );
    assert_no_error_log("join", &join_err);

    let status = run.wait();
    let run_err = drain(&run_errs);
    assert!(status.success(), "run failed: {}", run_err.join("\n"));
    assert!(
        run_err.iter().any(|l| l.starts_with("peer joined: ")),
        "no peer joined line in piped human output:\n{}",
        run_err.join("\n")
    );
    assert_no_error_log("run", &run_err);
}

#[test]
fn a_failed_get_closes_its_endpoint() {
    let relay_node = Node::new();
    let (_relay, url) = relay(&relay_node);

    let sharer = Node::new();
    std::fs::write(sharer.path().join("a.txt"), b"bonjour").expect("write");
    let mut cmd = sharer.cmd();
    cmd.args(["--relay", &url, "--json", "share"])
        .arg(sharer.path().join("a.txt"));
    let share = Guard::spawn(cmd);
    let link = event(&share.line(|l| is_event(l, "link")));
    let link = link["link"].as_str().expect("link").to_owned();
    drop(share);

    let getter = Node::new();
    let mut cmd = getter.cmd();
    cmd.args(["--relay", &url, "get", "-", "-o"])
        .arg(getter.path().join("dl"));
    let got = run_with_stdin(cmd, &format!("{link}\n"));
    let stderr: Vec<String> = String::from_utf8_lossy(&got.stderr)
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(got.status.code(), Some(3), "{}", stderr.join("\n"));
    assert_no_error_log("get", &stderr);
}
