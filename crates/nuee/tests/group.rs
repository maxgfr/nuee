//! `nuee group`: a group outlives the machine that coordinates it.
//! Hermetic: every node has its own directories and talks through a local plain-HTTP relay.

use std::io::{BufRead, BufReader};
use std::path::Path;
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
            .env("NUEE_GROUP_TIME_PERCENT", "20")
            .env("NO_COLOR", "1");
        c
    }

    fn with_relay(&self, relay: &str) -> Command {
        let mut c = self.cmd();
        c.args(["--json", "--relay", relay]);
        c
    }

    fn id(&self) -> String {
        let out = self.cmd().args(["--json", "id"]).output().expect("nuee id");
        assert!(out.status.success(), "nuee id failed");
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let ev = events(text.lines().map(str::to_owned)).remove(0);
        ev["id"].as_str().expect("id").to_owned()
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

    /// The next event with this name.
    fn event(&self, name: &str) -> Value {
        let deadline = Instant::now() + LIMIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self
                .lines
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("no `{name}` event before the time limit"));
            if let Ok(v) = serde_json::from_str::<Value>(&line)
                && v["event"] == name
            {
                return v;
            }
        }
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

fn events(lines: impl Iterator<Item = String>) -> Vec<Value> {
    lines
        .filter_map(|l| serde_json::from_str::<Value>(&l).ok())
        .collect()
}

fn finish(cmd: Command) -> (ExitStatus, Vec<Value>) {
    let mut g = Guard::spawn(cmd);
    let status = g.wait();
    let lines: Vec<String> = g.lines.try_iter().collect();
    (status, events(lines.into_iter()))
}

fn named<'a>(events: &'a [Value], name: &str) -> &'a Value {
    events
        .iter()
        .find(|e| e["event"] == name)
        .unwrap_or_else(|| panic!("no `{name}` in {events:?}"))
}

fn start_relay(node: &Node) -> (Guard, String) {
    let mut cmd = node.cmd();
    cmd.args(["--json", "relay", "--dev", "--bind", "127.0.0.1:0"]);
    let guard = Guard::spawn(cmd);
    let url = guard.event("relay")["url"]
        .as_str()
        .expect("relay url")
        .to_owned();
    (guard, url)
}

fn http_get(addr: &str, path: &str) -> String {
    use std::io::{Read as _, Write as _};
    let mut stream = std::net::TcpStream::connect(addr).expect("connect");
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )
    .expect("request");
    let mut answer = String::new();
    stream.read_to_string(&mut answer).expect("response");
    answer
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.org"])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?} failed");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A shard whose number is even fails: the command is the test suite of the repository.
#[test]
fn shards_run_on_the_group_and_failures_are_results() {
    const REPO: &str = "https://example.invalid/team/app.git";
    const COMMAND: &str = "sh check.sh {i} {n}";

    let relay_node = Node::new();
    let (_relay, relay) = start_relay(&relay_node);
    let (lead, worker, stranger) = (Node::new(), Node::new(), Node::new());

    let source = tempfile::tempdir().expect("tempdir");
    let dir = source.path();
    git(dir, &["init", "-q", "-b", "main"]);
    let script = "echo \"shard $1 of $2\"\n[ $(($1 % 2)) -eq 1 ]\n";
    std::fs::write(dir.join("check.sh"), script).expect("script");
    git(dir, &["add", "check.sh"]);
    git(dir, &["commit", "-q", "-m", "tests"]);
    let rev = git(dir, &["rev-parse", "HEAD"]);
    let mirror = format!("{REPO}={}", dir.display());

    let mut create = lead.with_relay(&relay);
    create.args(["group", "create", "team"]);
    let (status, out) = finish(create);
    assert!(status.success(), "group create failed: {out:?}");
    let link = named(&out, "link")["link"]
        .as_str()
        .expect("link")
        .to_owned();
    let mut invite = lead.with_relay(&relay);
    invite.args(["group", "invite", &link, &worker.id(), "--name", "w"]);
    let (status, out) = finish(invite);
    assert!(status.success(), "group invite failed: {out:?}");

    let up = |node: &Node| {
        let mut c = node.with_relay(&relay);
        c.args(["group", "up", &link, "--slots", "1"]);
        c.args(["--allow-repo", REPO, "--allow-command", COMMAND]);
        c.args(["--exec-mirror", &mirror]);
        Guard::spawn(c)
    };
    let lead_up = up(&lead);
    lead_up.event("anchor");
    let worker_up = up(&worker);
    worker_up.event("welcome");

    let shard = |node: &Node, out_dir: &Path| {
        let mut c = node.with_relay(&relay);
        c.args(["shard", "4", "--on", &link, "--repo", REPO, "--rev", &rev]);
        c.arg("--out").arg(out_dir);
        c.args(["--", COMMAND]);
        c
    };

    // Holding the link is not enough: the stranger is not on the roster.
    let refused = stranger.path().join("out");
    let (status, _) = finish(shard(&stranger, &refused));
    assert_eq!(status.code(), Some(5), "a stranger cannot submit");

    let logs = worker.path().join("logs");
    let (status, out) = finish(shard(&worker, &logs));
    assert_eq!(status.code(), Some(1), "two shards fail: {out:?}");
    let results: Vec<(String, bool)> = out
        .iter()
        .filter(|e| e["event"] == "shard")
        .map(|e| {
            (
                e["name"].as_str().expect("name").to_owned(),
                e["passed"].as_bool().expect("passed"),
            )
        })
        .collect();
    assert_eq!(
        results,
        vec![
            ("shard-1-of-4".to_owned(), true),
            ("shard-2-of-4".to_owned(), false),
            ("shard-3-of-4".to_owned(), true),
            ("shard-4-of-4".to_owned(), false),
        ]
    );
    let log = std::fs::read_to_string(logs.join("shard-3-of-4/task-0.bin")).expect("log");
    assert!(log.contains("shard 3 of 4"), "{log}");
}

/// The coordinator dies in the middle of a job: another keeper takes over, the worker follows and
/// the job, submitted again by its client, ends.
#[test]
fn a_keeper_takes_over_when_the_coordinator_dies() {
    const REPO: &str = "https://example.invalid/team/slow.git";
    const COMMAND: &str = "sleep 8; echo done";

    let relay_node = Node::new();
    let (_relay, relay) = start_relay(&relay_node);
    let (a, b, c) = (Node::new(), Node::new(), Node::new());

    let source = tempfile::tempdir().expect("tempdir");
    let dir = source.path();
    git(dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("README"), "x").expect("file");
    git(dir, &["add", "README"]);
    git(dir, &["commit", "-q", "-m", "x"]);
    let rev = git(dir, &["rev-parse", "HEAD"]);
    let mirror = format!("{REPO}={}", dir.display());

    let mut create = a.with_relay(&relay);
    create.args(["group", "create", "lab"]);
    let (status, out) = finish(create);
    assert!(status.success(), "group create failed: {out:?}");
    let mut link = named(&out, "link")["link"]
        .as_str()
        .expect("link")
        .to_owned();
    let mut invite = a.with_relay(&relay);
    invite.args(["group", "invite", &link, &b.id(), "--name", "b", "--keeper"]);
    let (status, out) = finish(invite);
    assert!(status.success(), "invite failed: {out:?}");
    link = named(&out, "link")["link"]
        .as_str()
        .expect("link")
        .to_owned();
    let mut invite = a.with_relay(&relay);
    invite.args(["group", "invite", &link, &c.id(), "--name", "c"]);
    assert!(finish(invite).0.success(), "invite failed");

    let up = |node: &Node| {
        let mut cmd = node.with_relay(&relay);
        cmd.args(["group", "up", &link, "--slots", "1"]);
        cmd.args(["--allow-repo", REPO, "--allow-command", COMMAND]);
        cmd.args(["--exec-mirror", &mirror]);
        Guard::spawn(cmd)
    };
    let mut up_a = up(&a);
    up_a.event("anchor");
    let up_b = up(&b);
    up_b.event("welcome");
    let up_c = up(&c);
    up_c.event("welcome");

    let mut shard = b.with_relay(&relay);
    shard.args(["shard", "1", "--on", &link, "--repo", REPO, "--rev", &rev]);
    shard.arg("--out").arg(b.path().join("out"));
    shard.args(["--", COMMAND]);
    let mut client = Guard::spawn(shard);

    std::thread::sleep(Duration::from_secs(3));
    up_a.child.kill().expect("kill the coordinator");
    up_a.child.wait().expect("reap");

    up_b.event("anchor");
    let status = client.wait();
    assert!(status.success(), "the job did not survive the takeover");
    let log = std::fs::read_to_string(b.path().join("out/shard-1-of-1/task-0.bin")).expect("log");
    assert!(log.contains("done"), "{log}");

    // The first coordinator comes back and works for the one that took over.
    let up_a2 = up(&a);
    up_a2.event("welcome");
}

/// What a group keeps stays available without the machine that gave it, and without the keeper
/// that coordinated.
#[test]
fn the_drive_survives_the_loss_of_its_coordinator() {
    let relay_node = Node::new();
    let (_relay, relay) = start_relay(&relay_node);
    let (a, b, c) = (Node::new(), Node::new(), Node::new());
    let stranger = Node::new();

    let mut create = a.with_relay(&relay);
    create.args(["group", "create", "friends"]);
    let (status, out) = finish(create);
    assert!(status.success(), "group create failed: {out:?}");
    let mut link = named(&out, "link")["link"]
        .as_str()
        .expect("link")
        .to_owned();
    let mut invite = a.with_relay(&relay);
    invite.args(["group", "invite", &link, &b.id(), "--name", "b", "--keeper"]);
    let (status, out) = finish(invite);
    assert!(status.success(), "invite failed: {out:?}");
    link = named(&out, "link")["link"]
        .as_str()
        .expect("link")
        .to_owned();
    let mut invite = a.with_relay(&relay);
    invite.args(["group", "invite", &link, &c.id(), "--name", "c"]);
    assert!(finish(invite).0.success(), "invite failed");

    let up = |node: &Node| {
        let mut cmd = node.with_relay(&relay);
        cmd.args(["group", "up", &link, "--slots", "1"]);
        cmd.args(["--gateway", "127.0.0.1:0"]);
        Guard::spawn(cmd)
    };
    let mut up_a = up(&a);
    up_a.event("anchor");
    let up_b = up(&b);
    up_b.event("welcome");

    let site = c.path().join("site");
    std::fs::create_dir(&site).expect("site");
    std::fs::write(site.join("index.html"), "<h1>our site</h1>").expect("page");
    let mut publish = c.with_relay(&relay);
    publish.args(["site", "publish"]).arg(&site);
    publish.args(["--on", &link, "--name", "our-site"]);
    let (status, out) = finish(publish);
    assert!(status.success(), "publish failed: {out:?}");
    let path = named(&out, "anchored")["path"]
        .as_str()
        .expect("path")
        .to_owned();
    let site_name = named(&out, "link")["link"]
        .as_str()
        .expect("link")
        .to_owned();
    assert!(site_name.contains("#/N/"), "{site_name}");
    assert!(!site_name.contains(link.rsplit('/').next().expect("payload")));

    let photos = c.path().join("photos.txt");
    std::fs::write(&photos, "souvenirs").expect("file");
    let mut put = c.with_relay(&relay);
    put.args(["group", "put", &link]).arg(&photos);
    let (status, out) = finish(put);
    assert!(status.success(), "put failed: {out:?}");
    assert_eq!(named(&out, "kept")["name"], "photos.txt");

    // The site was copied first; the coordinator may die once the file is copied too.
    while up_b.event("copied")["files"] != 1 {}
    up_a.child.kill().expect("kill the coordinator");
    up_a.child.wait().expect("reap");
    let anchor = up_b.event("anchor");
    let gateway = anchor["gateway"]
        .as_str()
        .expect("gateway")
        .trim_start_matches("http://")
        .to_owned();
    let page = http_get(&gateway, &path);
    assert!(page.contains("<h1>our site</h1>"), "{page}");

    // Someone who is not in the group follows the name to the keeper that took over.
    let visited = stranger.path().join("visited");
    let mut get = stranger.with_relay(&relay);
    get.args(["site", "get", &site_name, "-o"]).arg(&visited);
    let (status, out) = finish(get);
    assert!(status.success(), "get by name failed: {out:?}");
    let copy = std::fs::read_to_string(visited.join("site/index.html")).expect("the page");
    assert!(copy.contains("<h1>our site</h1>"), "{copy}");

    let mut ls = c.with_relay(&relay);
    ls.args(["group", "ls", &link]);
    let (status, out) = finish(ls);
    assert!(status.success(), "ls failed: {out:?}");
    assert_eq!(named(&out, "drive")["files"][0]["name"], "photos.txt");

    let back = c.path().join("back");
    let mut get = c.with_relay(&relay);
    get.args(["group", "get", &link, "photos.txt", "-o"])
        .arg(&back);
    let (status, out) = finish(get);
    assert!(status.success(), "get failed: {out:?}");
    assert_eq!(
        std::fs::read_to_string(back.join("photos.txt")).expect("the file"),
        "souvenirs"
    );

    let mut rm = c.with_relay(&relay);
    rm.args(["group", "rm", &link, "photos.txt"]);
    let (status, out) = finish(rm);
    assert!(status.success(), "rm failed: {out:?}");
}

/// A CI key builds on the group's machines, without being able to do anything else.
#[test]
fn a_ci_key_builds_and_brings_the_output_back() {
    const REPO: &str = "https://example.invalid/team/site.git";
    const COMMAND: &str = "sh build.sh";

    let relay_node = Node::new();
    let (_relay, relay) = start_relay(&relay_node);
    let (lead, worker, ci) = (Node::new(), Node::new(), Node::new());

    let source = tempfile::tempdir().expect("tempdir");
    let dir = source.path();
    git(dir, &["init", "-q", "-b", "main"]);
    let script = "echo building\nmkdir -p dist/assets\necho \"made by $(git rev-parse HEAD)\" > dist/assets/app.txt\nln -s /etc/passwd dist/link\n";
    std::fs::write(dir.join("build.sh"), script).expect("script");
    git(dir, &["add", "build.sh"]);
    git(dir, &["commit", "-q", "-m", "build"]);
    let rev = git(dir, &["rev-parse", "HEAD"]);
    let mirror = format!("{REPO}={}", dir.display());

    let mut create = lead.with_relay(&relay);
    create.args(["group", "create", "team"]);
    let (status, out) = finish(create);
    assert!(status.success(), "group create failed: {out:?}");
    let link = named(&out, "link")["link"]
        .as_str()
        .expect("link")
        .to_owned();
    let mut invite = lead.with_relay(&relay);
    invite.args(["group", "invite", &link, &worker.id(), "--name", "w"]);
    assert!(finish(invite).0.success(), "group invite failed");
    let mut key = lead.with_relay(&relay);
    key.args(["group", "ci-key", &link, "--name", "github"]);
    let (status, out) = finish(key);
    assert!(status.success(), "ci-key failed: {out:?}");
    let ci_key = named(&out, "ci_key")["key"]
        .as_str()
        .expect("key")
        .to_owned();

    let up = |node: &Node| {
        let mut c = node.with_relay(&relay);
        c.args(["group", "up", &link, "--slots", "1"]);
        c.args(["--allow-repo", REPO, "--allow-command", COMMAND]);
        c.args(["--exec-mirror", &mirror]);
        Guard::spawn(c)
    };
    let lead_up = up(&lead);
    lead_up.event("anchor");
    let worker_up = up(&worker);
    worker_up.event("welcome");

    let build = |out_dir: &Path| {
        let mut c = ci.with_relay(&relay);
        c.env("NUEE_KEY", &ci_key);
        c.args(["build", "--on", &link, "--repo", REPO, "--rev", &rev]);
        c.args(["--keep", "dist", "--keep", "never-made"]);
        c.arg("--out").arg(out_dir);
        c.args(["--", COMMAND]);
        c
    };
    let first = ci.path().join("first");
    let (status, out) = finish(build(&first));
    assert!(status.success(), "the build failed: {out:?}");
    let report = named(&out, "build");
    assert_eq!(report["passed"], true);
    assert_eq!(report["files"], 1);
    assert_eq!(report["skipped_links"], 1);
    let made = std::fs::read_to_string(first.join("dist/assets/app.txt")).expect("output");
    assert_eq!(made.trim(), format!("made by {rev}"));
    let log = std::fs::read_to_string(first.join("nuee-build.log")).expect("log");
    assert!(log.contains("building"), "{log}");

    // The same commit is not built twice: the teammate's copy is the first build's.
    let second = ci.path().join("second");
    let start = Instant::now();
    let (status, out) = finish(build(&second));
    assert!(status.success(), "the second build failed: {out:?}");
    assert!(second.join("dist/assets/app.txt").exists());
    assert!(start.elapsed() < LIMIT);

    // The key submits jobs and does nothing else.
    let mut up_ci = ci.with_relay(&relay);
    up_ci.env("NUEE_KEY", &ci_key);
    up_ci.args(["group", "up", &link]);
    let (status, _) = finish(up_ci);
    assert_eq!(status.code(), Some(5), "a CI key is not a worker");
}

#[test]
fn a_group_changes_coordinator_and_keeps_strangers_out() {
    let relay_node = Node::new();
    let (_relay, relay) = start_relay(&relay_node);
    let (a, b, stranger) = (Node::new(), Node::new(), Node::new());

    let mut create = a.with_relay(&relay);
    create.args(["group", "create", "lab"]);
    let (status, out) = finish(create);
    assert!(status.success(), "group create failed: {out:?}");
    let created = named(&out, "link");
    assert_eq!(created["kind"], "group");
    let link = created["link"].as_str().expect("link").to_owned();
    assert!(link.contains("#/G/"), "{link}");

    let b_id = b.id();
    let mut invite = a.with_relay(&relay);
    invite.args(["group", "invite", &link, &b_id, "--name", "bee", "--keeper"]);
    let (status, out) = finish(invite);
    assert!(status.success(), "group invite failed: {out:?}");
    assert_eq!(named(&out, "invited")["revision"], 2);
    // The link now also lists the new keeper.
    let link = named(&out, "link")["link"]
        .as_str()
        .expect("link")
        .to_owned();

    let mut up_a = a.with_relay(&relay);
    up_a.args(["group", "up", &link, "--slots", "1"]);
    let up_a = Guard::spawn(up_a);
    up_a.event("anchor");

    let mut up_b = b.with_relay(&relay);
    up_b.args(["group", "up", &link, "--slots", "1"]);
    let up_b = Guard::spawn(up_b);
    up_b.event("coordinator");

    // A machine that is not on the roster is told so, with the link in hand.
    let mut up_stranger = stranger.with_relay(&relay);
    up_stranger.args(["group", "up", &link]);
    let (status, _) = finish(up_stranger);
    assert_eq!(status.code(), Some(5), "a stranger must be refused");

    let mut status_b = b.with_relay(&relay);
    status_b.args(["group", "status", &link]);
    let (status, out) = finish(status_b);
    assert!(status.success(), "group status failed: {out:?}");
    let group = named(&out, "group");
    assert_eq!(group["name"], "lab");
    assert_eq!(group["revision"], 2);
    assert_eq!(group["members"].as_array().expect("members").len(), 2);

    // A member cannot change the roster: it does not hold the group key.
    let mut forged = b.with_relay(&relay);
    forged.args(["group", "remove", &link, &a.id()]);
    let (status, _) = finish(forged);
    assert_eq!(status.code(), Some(5), "only the owner signs rosters");

    let mut handoff = a.with_relay(&relay);
    handoff.args(["group", "handoff", &link, &b_id]);
    let (status, out) = finish(handoff);
    assert!(status.success(), "group handoff failed: {out:?}");
    assert_eq!(named(&out, "handoff")["revision"], 3);

    // The former worker coordinates, and the former coordinator joins it.
    up_b.event("anchor");
    up_a.event("coordinator");
}

/// An agent sees the group's tools, and nothing it did not ask for reaches the group.
#[test]
fn the_mcp_server_lists_its_tools_and_refuses_unknown_ones() {
    use std::io::Write as _;

    let node = Node::new();
    let mut cmd = node.cmd();
    cmd.args(["mcp", "--on", "https://example.invalid/#/G/not-a-link"]);
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    let requests = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"rm_rf","arguments":{}}}"#,
        r#"{"jsonrpc":"2.0","id":4,"method":"no/such/method"}"#,
    ];
    let mut stdin = child.stdin.take().expect("stdin");
    for r in requests {
        writeln!(stdin, "{r}").expect("write");
    }
    drop(stdin);
    let out = child.wait_with_output().expect("output");
    assert!(out.status.success());
    let replies: Vec<Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).expect("json"))
        .collect();
    assert_eq!(
        replies.len(),
        4,
        "a notification gets no reply: {replies:?}"
    );
    assert_eq!(replies[0]["result"]["protocolVersion"], "2025-06-18");
    let tools: Vec<&str> = replies[1]["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|t| t["name"].as_str().expect("name"))
        .collect();
    assert!(
        tools.contains(&"build") && tools.contains(&"shard"),
        "{tools:?}"
    );
    assert_eq!(replies[2]["error"]["code"], -32602);
    assert_eq!(replies[3]["error"]["code"], -32601);
}
