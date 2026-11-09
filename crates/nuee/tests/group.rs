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
            .env_remove("NUEE_LOG")
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
