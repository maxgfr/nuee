//! `nuee serve --anchor`: a persistent coordinator with its own dev relay and a gateway.
//! Hermetic: every node has its own directories and talks through the anchor's embedded
//! plain-HTTP relay on 127.0.0.1.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_nuee");
const LIMIT: Duration = Duration::from_secs(180);

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
            .env_remove("NUEE_LOG")
            .env("NO_COLOR", "1");
        c
    }

    fn id(&self) -> String {
        let out = self.cmd().args(["--json", "id"]).output().expect("nuee id");
        assert!(out.status.success(), "nuee id failed");
        let line = String::from_utf8_lossy(&out.stdout)
            .lines()
            .find(|l| is_event(l, "id"))
            .map(str::to_owned)
            .expect("id event");
        event(&line)["id"].as_str().expect("id").to_owned()
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

fn event(line: &str) -> Value {
    serde_json::from_str(line).unwrap_or_else(|e| panic!("not JSON ({e}): {line}"))
}

fn is_event(line: &str, name: &str) -> bool {
    serde_json::from_str::<Value>(line).is_ok_and(|v| v["event"] == name)
}

/// What a running anchor printed.
struct Started {
    guard: Guard,
    link: String,
    swarm: String,
    relay: String,
    gateway: String,
    /// Events printed before the `anchor` event (`resumed`).
    before: Vec<Value>,
}

fn start_anchor(node: &Node, extra: &[&str]) -> Started {
    let mut cmd = node.cmd();
    cmd.args([
        "--json",
        "serve",
        "--anchor",
        "--relay-dev",
        "--relay-bind",
        "127.0.0.1:0",
        "--gateway",
        "127.0.0.1:0",
    ])
    .args(extra);
    let guard = Guard::spawn(cmd);
    let mut before = Vec::new();
    let ev = loop {
        let line = guard.line(|l| serde_json::from_str::<Value>(l).is_ok());
        let ev = event(&line);
        if ev["event"] == "anchor" {
            break ev;
        }
        before.push(ev);
    };
    let s = |k: &str| {
        ev[k]
            .as_str()
            .unwrap_or_else(|| panic!("{k} in {ev}"))
            .to_owned()
    };
    let link = s("link");
    assert!(link.contains("#/X/"), "{link}");
    let swarm = s("swarm");
    assert!(swarm.contains("#/J/"), "{swarm}");
    let relay = s("relay");
    assert!(relay.starts_with("http://127.0.0.1:"), "{relay}");
    let gateway = s("gateway").trim_start_matches("http://").to_owned();
    Started {
        guard,
        link,
        swarm,
        relay,
        gateway,
        before,
    }
}

/// Runs `cmd` to completion within the time limit.
fn finish(cmd: Command) -> (ExitStatus, Vec<String>) {
    let mut g = Guard::spawn(cmd);
    let status = g.wait();
    (status, g.rest())
}

/// A plain HTTP/1.1 GET: status, headers (lower-case names), body.
fn http_get(addr: &str, path: &str) -> (u16, BTreeMap<String, String>, Vec<u8>) {
    let mut s = TcpStream::connect(addr).expect("connect to the gateway");
    s.set_read_timeout(Some(Duration::from_secs(30)))
        .expect("timeout");
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .expect("write request");
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).expect("read response");
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("end of head");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let body = raw[split + 4..].to_vec();
    let mut lines = head.lines();
    let status: u16 = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .expect("status line");
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    (status, headers, body)
}

fn count_outputs(dir: &PathBuf) -> usize {
    std::fs::read_dir(dir)
        .expect("output dir")
        .filter_map(Result::ok)
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with("task-") && n.ends_with(".bin")
        })
        .count()
}

#[test]
fn anchor_keeps_jobs_across_restarts_and_serves_sites() {
    let anchor_node = Node::new();
    // No local slot: the joiner computes every task (and nothing is sampled, see --verify).
    let first = start_anchor(&anchor_node, &["--local-slots", "0"]);

    // The submitter uploads the job and returns at once with the swarm link.
    let submitter = Node::new();
    let mut cmd = submitter.cmd();
    cmd.args([
        "--json",
        "run",
        "demo:mandelbrot",
        "--verify",
        "none",
        "--anchor",
        &first.link,
    ]);
    let started = Instant::now();
    let (status, lines) = finish(cmd);
    assert!(status.success(), "submitter failed: {lines:?}");
    assert!(
        started.elapsed() < Duration::from_secs(90),
        "the submitter should not wait for the job"
    );
    let anchored = lines
        .iter()
        .find(|l| is_event(l, "anchored"))
        .map(|l| event(l))
        .expect("anchored event");
    assert_eq!(anchored["existing"], false);
    let job = anchored["job"].as_u64().expect("job number");
    let link = lines
        .iter()
        .find(|l| is_event(l, "link"))
        .map(|l| event(l))
        .expect("link event");
    assert_eq!(link["kind"], "job");
    assert!(link["link"].as_str().is_some_and(|l| l.contains("#/J/")));

    // Restart the anchor, now in enterprise mode: the job is still there.
    drop(first);
    let allowed = Node::new();
    let allowed_id = allowed.id();
    let second = start_anchor(
        &anchor_node,
        &["--local-slots", "0", "--allow", &allowed_id],
    );
    let resumed = second
        .before
        .iter()
        .find(|e| e["event"] == "resumed")
        .expect("resumed event");
    assert_eq!(resumed["jobs"], 1);

    // A peer outside the allowlist is turned away with the admission code.
    let outsider = Node::new();
    let mut cmd = outsider.cmd();
    cmd.args(["--relay", &second.relay, "join", &second.swarm]);
    let (status, _) = finish(cmd);
    assert_eq!(status.code(), Some(5), "outsider must be refused");

    // A second client follows the same job (same description, same job number).
    let follower = Node::new();
    let out = follower.path().join("results");
    let mut cmd = follower.cmd();
    cmd.args([
        "--json",
        "run",
        "demo:mandelbrot",
        "--verify",
        "none",
        "--anchor",
        &second.link,
        "--follow",
        "--out",
    ])
    .arg(&out);
    let mut follow = Guard::spawn(cmd);
    let attached = event(&follow.line(|l| is_event(l, "anchored")));
    assert_eq!(attached["existing"], true);
    assert_eq!(attached["job"].as_u64(), Some(job));

    // The allowed worker computes every task.
    let mut cmd = allowed.cmd();
    cmd.args([
        "--relay",
        &second.relay,
        "--json",
        "join",
        &second.swarm,
        "--slots",
        "4",
    ]);
    let _worker = Guard::spawn(cmd);

    let status = follow.wait();
    let rest = follow.rest();
    assert!(status.success(), "follower failed: {rest:?}");
    let done = rest
        .iter()
        .find(|l| is_event(l, "done"))
        .map(|l| event(l))
        .expect("done event");
    assert_eq!(done["tasks"], 256);
    assert_eq!(count_outputs(&out), 256);
    let results: Value =
        serde_json::from_slice(&std::fs::read(out.join("results.json")).expect("results.json"))
            .expect("results JSON");
    assert_eq!(results["done"], 256);

    // A site handed to the anchor is served by its gateway with the sandbox headers.
    let site = Node::new();
    let dir = site.path().join("site");
    std::fs::create_dir_all(dir.join("css")).expect("site dir");
    std::fs::write(dir.join("index.html"), "<h1>hello</h1>").expect("index");
    std::fs::write(dir.join("css/a.css"), "h1{color:red}").expect("css");
    let mut cmd = site.cmd();
    cmd.args(["--json", "site", "publish"])
        .arg(&dir)
        .args(["--on", &second.link]);
    let (status, lines) = finish(cmd);
    assert!(status.success(), "site publish failed: {lines:?}");
    let path = lines
        .iter()
        .find(|l| is_event(l, "anchored"))
        .map(|l| event(l)["path"].as_str().expect("path").to_owned())
        .expect("anchored site");
    assert!(path.starts_with("/s/") && path.ends_with('/'), "{path}");

    let (status, headers, body) = http_get(&second.gateway, &path);
    assert_eq!(status, 200);
    assert_eq!(body, b"<h1>hello</h1>");
    assert_eq!(headers["content-security-policy"], "sandbox");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(headers["referrer-policy"], "no-referrer");
    assert_eq!(headers["cross-origin-resource-policy"], "cross-origin");
    assert_eq!(headers["content-type"], "text/html; charset=utf-8");
    let (status, headers, _) = http_get(&second.gateway, &format!("{path}css/a.css"));
    assert_eq!(status, 200);
    assert_eq!(headers["content-type"], "text/css; charset=utf-8");
    assert_eq!(headers["content-security-policy"], "sandbox");

    let (status, headers, _) = http_get(&second.gateway, &format!("{path}../index.html"));
    assert_eq!(status, 400);
    assert_eq!(headers["content-security-policy"], "sandbox");
    let (status, _, _) = http_get(&second.gateway, &format!("{path}%2e%2e/index.html"));
    assert_eq!(status, 400);
    let (status, _, _) = http_get(&second.gateway, &format!("{path}css/"));
    assert_eq!(status, 404, "no directory listing");
    let unknown = format!("/s/{}/", "0".repeat(64));
    let (status, headers, _) = http_get(&second.gateway, &unknown);
    assert_eq!(status, 404);
    assert_eq!(headers["x-content-type-options"], "nosniff");
}

#[test]
fn wrong_admin_secret_is_refused_and_the_store_member_joins() {
    let anchor_node = Node::new();
    let a = start_anchor(
        &anchor_node,
        &["--admin-secret-file", "admin.key", "--store", "64MiB"],
    );
    // The anchor's own store member joins its swarm.
    let member = event(
        &a.guard
            .line(|l| is_event(l, "peer") && l.contains("nuee-anchor/")),
    );
    assert_eq!(member["state"], "joined");
    let secret = anchor_node.path().join("admin.key");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&secret)
            .expect("secret")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    // The same anchor link with another admin secret is refused.
    let Ok(nuee_proto::Link::Service(mut l)) = nuee_proto::Link::parse(&a.link) else {
        panic!("not an anchor link: {}", a.link);
    };
    l.secret[0] ^= 1;
    let forged = nuee_proto::Link::Service(l).to_url(nuee_proto::link::DEFAULT_BASE);
    let other = Node::new();
    let mut cmd = other.cmd();
    cmd.args(["run", "demo:mandelbrot", "--anchor", &forged]);
    let (status, _) = finish(cmd);
    assert_eq!(
        status.code(),
        Some(5),
        "a wrong admin secret must be refused"
    );
    let mut cmd = other.cmd();
    cmd.args(["run", "demo:mandelbrot", "--anchor", "#/X/AAAA"]);
    let (status, _) = finish(cmd);
    assert_eq!(
        status.code(),
        Some(2),
        "a truncated anchor link is a usage error"
    );
    drop(a);
}
