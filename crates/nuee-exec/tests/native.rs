//! Native `exec` tasks: allowlist, pinned revision, clean environment, deadlines, process
//! groups, sandbox and the one-GPU-job-at-a-time lock. Repositories come from a local mirror.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use nix::sys::signal::kill;
use nix::unistd::Pid;
use nuee_exec::native::{AllowEntry, ExecPolicy, NativeRunner, RepoSource, Sandbox};
use nuee_proto::{BoundedString, BoundedVec, EnvVar, ExecSpec, FailKind};

const REPO: &str = "https://example.invalid/nuee/fixture.git";

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

struct Fixture {
    _dir: tempfile::TempDir,
    mirror: PathBuf,
    cache: PathBuf,
    rev: String,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let mirror = dir.path().join("mirror");
    std::fs::create_dir(&mirror).expect("mkdir");
    std::fs::write(
        mirror.join("double.sh"),
        "#!/bin/sh\ncat \"$1\" \"$1\" > \"$2\"\n",
    )
    .expect("write");
    git(&mirror, &["init", "--quiet", "-b", "main"]);
    git(&mirror, &["add", "."]);
    git(
        &mirror,
        &[
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ],
    );
    let rev = git(&mirror, &["rev-parse", "HEAD"]);
    let cache = dir.path().join("cache");
    Fixture {
        _dir: dir,
        mirror,
        cache,
        rev,
    }
}

impl Fixture {
    fn policy(&self) -> ExecPolicy {
        let mut p = ExecPolicy::new(self.cache.clone());
        p.allow = vec![AllowEntry::new(REPO, &self.rev).expect("entry")];
        p.repo_source = RepoSource::LocalMirror(self.mirror.clone());
        p.kill_grace = Duration::from_millis(500);
        p
    }

    fn runner(&self) -> NativeRunner {
        NativeRunner::new(self.policy())
    }

    /// A policy that approves exactly these commands.
    fn policy_for(&self, specs: &[&ExecSpec]) -> ExecPolicy {
        let mut p = self.policy();
        for s in specs {
            p.approve(s).expect("approvable");
        }
        p
    }

    fn runner_for(&self, specs: &[&ExecSpec]) -> NativeRunner {
        NativeRunner::new(self.policy_for(specs))
    }

    fn spec(&self, argv: &[&str]) -> ExecSpec {
        spec(REPO, &self.rev, argv, &[])
    }
}

fn spec(repo: &str, rev: &str, argv: &[&str], env: &[(&str, &str)]) -> ExecSpec {
    ExecSpec {
        repo: BoundedString::try_from(repo).expect("repo"),
        rev: BoundedString::try_from(rev).expect("rev"),
        argv: BoundedVec::try_from(
            argv.iter()
                .map(|a| BoundedString::try_from(*a).expect("arg"))
                .collect::<Vec<_>>(),
        )
        .expect("argv"),
        env: BoundedVec::try_from(
            env.iter()
                .map(|(k, v)| EnvVar {
                    key: BoundedString::try_from(*k).expect("key"),
                    value: BoundedString::try_from(*v).expect("value"),
                })
                .collect::<Vec<_>>(),
        )
        .expect("env"),
        timeout_s: 30,
        gpu: false,
    }
}

const DEADLINE: Duration = Duration::from_secs(30);

#[tokio::test]
async fn allowlisted_commands_run() {
    let f = fixture();
    let double = f.spec(&["sh", "double.sh", "$NUEE_IN", "$NUEE_OUT"]);
    let py = "import os; d = open(os.environ['NUEE_IN'], 'rb').read(); \
              open(os.environ['NUEE_OUT'], 'wb').write(d[::-1])";
    let reverse = f.spec(&["python3", "-c", py]);
    let echo = f.spec(&[
        "sh",
        "-c",
        "echo hello > \"$NUEE_OUT\"; echo scratch > \"$TMPDIR/t\"",
    ]);
    let r = f.runner_for(&[&double, &reverse, &echo]);
    let out = r.run(&double, b"ab", DEADLINE).await.expect("sh task");
    assert_eq!(out, b"abab");

    let out = r
        .run(&reverse, b"xyz", DEADLINE)
        .await
        .expect("python task");
    assert_eq!(out, b"zyx");

    let out = r.run(&echo, b"", DEADLINE).await.expect("echo task");
    assert_eq!(out, b"hello\n");
}

#[tokio::test]
async fn not_allowlisted_is_refused_before_git() {
    let f = fixture();
    let r = f.runner();
    let other = "0123456789abcdef0123456789abcdef01234567";
    let s = spec(REPO, other, &["sh", "-c", "true"], &[]);
    assert!(!r.allowed(&s));
    let e = r.run(&s, b"", DEADLINE).await.expect_err("refused");
    assert_eq!(e.kind, FailKind::Unsupported);
    let s = spec("https://example.invalid/other.git", &f.rev, &["true"], &[]);
    let e = r.run(&s, b"", DEADLINE).await.expect_err("refused");
    assert_eq!(e.kind, FailKind::Unsupported);
    assert!(
        !f.cache.join("repos").exists(),
        "git ran for a refused spec"
    );
}

#[test]
fn allow_entries_are_strict() {
    let rev = "0123456789abcdef0123456789abcdef01234567";
    assert!(format!("{REPO}@{rev}").parse::<AllowEntry>().is_ok());
    for bad in [
        format!("ssh://host/r.git@{rev}"),
        format!("file:///tmp/r@{rev}"),
        format!("/tmp/r@{rev}"),
        format!("--upload-pack=x@{rev}"),
        format!("https://-oProxy/x@{rev}"),
        format!("{REPO}@{}", &rev[..39]),
        format!("{REPO}@{}", rev.to_uppercase()),
        REPO.to_owned(),
    ] {
        assert!(bad.parse::<AllowEntry>().is_err(), "{bad} accepted");
    }
}

#[tokio::test]
async fn unknown_revision_is_invalid() {
    let f = fixture();
    let missing = "0123456789abcdef0123456789abcdef01234567";
    let s = spec(REPO, missing, &["true"], &[]);
    let mut p = f.policy();
    p.allow = vec![AllowEntry::new(REPO, missing).expect("entry")];
    p.approve(&s).expect("approvable");
    let r = NativeRunner::new(p);
    let e = r.run(&s, b"", DEADLINE).await.expect_err("missing rev");
    assert_eq!(e.kind, FailKind::Invalid, "{}", e.detail);
}

fn alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
}

#[tokio::test]
async fn timeout_kills_the_whole_group() {
    let f = fixture();
    let s = f.spec(&["sh", "-c", "sleep 60 & echo \"child=$!\" >&2; wait"]);
    let r = f.runner_for(&[&s]);
    let start = std::time::Instant::now();
    let e = r
        .run(&s, b"", Duration::from_secs(1))
        .await
        .expect_err("timeout");
    assert_eq!(e.kind, FailKind::Timeout, "{}", e.detail);
    assert!(start.elapsed() < Duration::from_secs(10));
    let pid: i32 = e
        .detail
        .split("child=")
        .nth(1)
        .and_then(|t| t.split_whitespace().next())
        .and_then(|t| t.parse().ok())
        .unwrap_or_else(|| panic!("no child pid in {:?}", e.detail));
    let mut gone = false;
    for _ in 0..40 {
        if !alive(pid) {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(gone, "the child sleep {pid} survived");
}

#[tokio::test]
async fn environment_is_clean() {
    // Cargo sets CARGO_MANIFEST_DIR in the test process: it stands in for a parent secret.
    let f = fixture();
    let script = "env > \"$NUEE_OUT\"";
    let s = spec(REPO, &f.rev, &["sh", "-c", script], &[("MY_FLAG", "on")]);
    let r = f.runner_for(&[&s]);
    let parent_has_cargo = std::env::var_os("CARGO_MANIFEST_DIR").is_some();
    assert!(parent_has_cargo);
    let out = r.run(&s, b"", DEADLINE).await.expect("env task");
    let env = String::from_utf8(out).expect("utf8");
    assert!(env.contains("MY_FLAG=on"));
    assert!(env.contains("TERM=dumb"));
    assert!(env.lines().any(|l| l.starts_with("NUEE_IN=")));
    assert!(
        !env.contains("CARGO_MANIFEST_DIR"),
        "a parent variable leaked: {env}"
    );

    for key in [
        "LD_PRELOAD",
        "DYLD_INSERT_LIBRARIES",
        "PATH",
        "GIT_DIR",
        "lower",
        "1X",
    ] {
        let s = spec(REPO, &f.rev, &["true"], &[(key, "x")]);
        let e = r.run(&s, b"", DEADLINE).await.expect_err(key);
        assert_eq!(e.kind, FailKind::Invalid, "{key}");
    }
}

#[tokio::test]
async fn output_cap_and_exit_codes() {
    let f = fixture();
    let big = f.spec(&["sh", "-c", "printf 0123456789AB > \"$NUEE_OUT\""]);
    let boom = f.spec(&["sh", "-c", "echo boom-at-the-end >&2; exit 3"]);
    let nothing = f.spec(&["true"]);
    let mut p = f.policy_for(&[&big, &boom, &nothing]);
    p.max_output = 10;
    let r = NativeRunner::new(p);
    let e = r.run(&big, b"", DEADLINE).await.expect_err("too big");
    assert_eq!(e.kind, FailKind::Invalid);

    let e = r.run(&boom, b"", DEADLINE).await.expect_err("non-zero");
    assert_eq!(e.kind, FailKind::Trap);
    assert!(e.detail.contains("boom-at-the-end"), "{}", e.detail);

    let e = r.run(&nothing, b"", DEADLINE).await.expect_err("no output");
    assert_eq!(e.kind, FailKind::Invalid);
}

#[tokio::test]
async fn sandbox_confines_writes_and_network() {
    let f = fixture();
    let home = std::env::var("HOME").expect("HOME");
    let target = Path::new(&home).join("should-not-exist");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listen");
    let port = listener.local_addr().expect("addr").port().to_string();
    let connect = "import os, socket; socket.create_connection(('127.0.0.1', \
                   int(os.environ['NUEE_PORT'])), 2).close(); \
                   open(os.environ['NUEE_OUT'], 'w').write('ok')";
    let net = spec(
        REPO,
        &f.rev,
        &["python3", "-c", connect],
        &[("NUEE_PORT", &port)],
    );
    let write = f.spec(&[
        "sh",
        "-c",
        "set -e; echo x > \"$HOME/should-not-exist\"; echo ok > \"$NUEE_OUT\"",
    ]);
    let exit1 = f.spec(&["sh", "-c", "exit 1"]);
    let r = f.runner_for(&[&net, &write, &exit1]);

    let expected = if cfg!(target_os = "macos") && Path::new("/usr/bin/sandbox-exec").exists() {
        Some(Sandbox::SandboxExec)
    } else if cfg!(target_os = "linux") && Command::new("bwrap").arg("--version").output().is_ok() {
        Some(Sandbox::Bwrap)
    } else {
        None
    };

    match r.sandbox() {
        Sandbox::None => {
            // Documented fallback: no tool, the task runs unconfined and failures say so.
            assert!(expected.is_none() || expected == Some(Sandbox::Bwrap));
            let e = r.run(&exit1, b"", DEADLINE).await.expect_err("exit 1");
            assert!(e.detail.contains("without an OS sandbox"), "{}", e.detail);
        }
        sb => {
            assert_eq!(Some(sb), expected);
            let e = r
                .run(&write, b"", DEADLINE)
                .await
                .expect_err("write denied");
            assert_eq!(e.kind, FailKind::Trap, "{}", e.detail);
            assert!(!target.exists(), "the task wrote to $HOME");
            let e = r
                .run(&net, b"", DEADLINE)
                .await
                .expect_err("network denied");
            assert_eq!(e.kind, FailKind::Trap, "{}", e.detail);

            let mut p = f.policy_for(&[&net]);
            p.network = true;
            let open = NativeRunner::new(p);
            let out = open
                .run(&net, b"", DEADLINE)
                .await
                .expect("network allowed");
            assert_eq!(out, b"ok");
        }
    }
    drop(listener);
}

#[tokio::test]
async fn gpu_jobs_run_one_at_a_time() {
    let f = fixture();
    let py = "import os, time; s = time.time(); time.sleep(0.6); \
              open(os.environ['NUEE_OUT'], 'w').write(f'{s} {time.time()}')";
    let mut s = f.spec(&["python3", "-c", py]);
    s.gpu = true;
    let r = f.runner_for(&[&s]);
    let (a, b) = tokio::join!(r.run(&s, b"", DEADLINE), r.run(&s, b"", DEADLINE));
    let span = |o: Vec<u8>| -> (f64, f64) {
        let t = String::from_utf8(o).expect("utf8");
        let mut it = t.split(' ').map(|x| x.parse::<f64>().expect("time"));
        (it.next().expect("start"), it.next().expect("end"))
    };
    let (a, b) = (span(a.expect("gpu a")), span(b.expect("gpu b")));
    assert!(
        a.1 <= b.0 || b.1 <= a.0,
        "overlapping GPU jobs: {a:?} {b:?}"
    );
}

#[tokio::test]
async fn approval_binds_the_exact_argv_and_environment() {
    let f = fixture();
    let approved = spec(
        REPO,
        &f.rev,
        &["sh", "double.sh", "$NUEE_IN", "$NUEE_OUT"],
        &[("MY_FLAG", "on")],
    );
    let r = f.runner_for(&[&approved]);
    assert!(r.allowed(&approved));
    let out = r.run(&approved, b"ab", DEADLINE).await.expect("approved");
    assert_eq!(out, b"abab");

    let other_argv = spec(
        REPO,
        &f.rev,
        &["sh", "-c", "echo pwned > \"$NUEE_OUT\""],
        &[("MY_FLAG", "on")],
    );
    let other_value = spec(
        REPO,
        &f.rev,
        &["sh", "double.sh", "$NUEE_IN", "$NUEE_OUT"],
        &[("MY_FLAG", "off")],
    );
    let extra_key = spec(
        REPO,
        &f.rev,
        &["sh", "double.sh", "$NUEE_IN", "$NUEE_OUT"],
        &[("MY_FLAG", "on"), ("OTHER", "1")],
    );
    let no_env = f.spec(&["sh", "double.sh", "$NUEE_IN", "$NUEE_OUT"]);
    for s in [&other_argv, &other_value, &extra_key, &no_env] {
        assert!(!r.allowed(s), "{:?} ran under another approval", s.argv);
        let e = r.run(s, b"ab", DEADLINE).await.expect_err("not approved");
        assert_eq!(e.kind, FailKind::Unsupported, "{}", e.detail);
    }
    // A pinned revision alone approves nothing.
    let pinned_only = f.runner();
    assert!(!pinned_only.allowed(&approved));
    let e = pinned_only
        .run(&approved, b"ab", DEADLINE)
        .await
        .expect_err("pin only");
    assert_eq!(e.kind, FailKind::Unsupported, "{}", e.detail);
}

#[tokio::test]
async fn code_loading_environment_keys_are_refused_even_when_approved() {
    let f = fixture();
    for key in [
        "JAVA_TOOL_OPTIONS",
        "JDK_JAVA_OPTIONS",
        "MAVEN_OPTS",
        "PYTHONHOME",
        "PERL5LIB",
        "RUBYLIB",
        "NODE_PATH",
        "CLASSPATH",
        "SHELLOPTS",
        "BASHOPTS",
        "PS4",
        "LD_PRELOAD",
        "DYLD_INSERT_LIBRARIES",
        "PATH",
    ] {
        let s = spec(REPO, &f.rev, &["true"], &[(key, "x")]);
        let mut p = f.policy();
        assert!(p.approve(&s).is_err(), "{key} was approvable");
        assert!(!nuee_exec::native::env_key_allowed(key), "{key}");
        let e = f.runner().run(&s, b"", DEADLINE).await.expect_err(key);
        assert_eq!(e.kind, FailKind::Invalid, "{key}");
    }
}

/// Interpreter hooks, library configuration and tools a program may launch are refused too.
#[test]
fn start_up_and_configuration_keys_are_refused() {
    for key in [
        "LUA_INIT",
        "LUA_INIT_5_4",
        "LUA_PATH",
        "PYTHONWARNINGS",
        "PYTHONBREAKPOINT",
        "OPENSSL_CONF",
        "OPENSSL_MODULES",
        "LESSOPEN",
        "PAGER",
        "EDITOR",
        "VISUAL",
        "BROWSER",
        "SHELL",
        "XDG_CONFIG_HOME",
        "R_PROFILE_USER",
        "R_ENVIRON_USER",
        "PIP_INDEX_URL",
        "PIP_CONFIG_FILE",
        "UV_INDEX_URL",
        "NPM_CONFIG_PREFIX",
        "GIT_CONFIG_GLOBAL",
        "GIT_SSH_COMMAND",
    ] {
        assert!(!nuee_exec::native::env_key_allowed(key), "{key} allowed");
        let approval = nuee_exec::ExecApproval::new(
            REPO,
            "0123456789abcdef0123456789abcdef01234567",
            vec!["true".into()],
            vec![(key.into(), "x".into())],
        );
        assert!(approval.is_err(), "{key} was approvable");
    }
    for key in [
        "MY_FLAG",
        "PYTHONHASHSEED",
        "PYTHONUNBUFFERED",
        "OMP_NUM_THREADS",
    ] {
        assert!(nuee_exec::native::env_key_allowed(key), "{key} refused");
    }
}

/// A remote that accepts the connection and never answers must not hold the lease, nor the
/// repository lock, past the deadline.
#[tokio::test]
async fn stalled_fetch_is_bounded_and_releases_the_repository() {
    let f = fixture();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listen");
    let port = listener.local_addr().expect("addr").port();
    let held = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let keep = held.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            keep.lock().expect("lock").push(conn);
        }
    });
    let stalled_repo = format!("https://127.0.0.1:{port}/nuee/fixture.git");
    let stalled = spec(
        &stalled_repo,
        &f.rev,
        &["sh", "double.sh", "$NUEE_IN", "$NUEE_OUT"],
        &[],
    );
    let mut p = ExecPolicy::new(f.cache.clone());
    p.approve(&stalled).expect("approvable");
    let r = NativeRunner::new(p);
    let start = std::time::Instant::now();
    let e = tokio::time::timeout(
        Duration::from_secs(30),
        r.run(&stalled, b"ab", Duration::from_secs(2)),
    )
    .await
    .expect("the checkout of a stalled remote never returned")
    .expect_err("stalled remote");
    assert_eq!(e.kind, FailKind::Io, "{}", e.detail);
    assert!(
        start.elapsed() < Duration::from_secs(15),
        "{:?}",
        start.elapsed()
    );
    assert!(
        !held.lock().expect("lock").is_empty(),
        "git never connected"
    );

    // The same repository (same cache entry and lock) still checks out afterwards.
    let mut p = f.policy();
    p.repo_source = RepoSource::LocalMirror(f.mirror.clone());
    p.approve(&stalled).expect("approvable");
    let next = NativeRunner::new(p);
    let out = tokio::time::timeout(Duration::from_secs(30), next.run(&stalled, b"ab", DEADLINE))
        .await
        .expect("the repository lock was never released")
        .expect("next task");
    assert_eq!(out, b"abab");
}

fn sleeping(tag: &str) -> bool {
    Command::new("pgrep")
        .args(["-f", tag])
        .output()
        .is_ok_and(|o| o.status.success())
}

async fn gone(tag: &str) -> bool {
    for _ in 0..40 {
        if !sleeping(tag) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// A background child left behind must neither keep the lease open nor survive it.
#[tokio::test]
async fn leftover_children_end_with_the_task() {
    let f = fixture();
    let s = f.spec(&["sh", "-c", "sleep 100417 & echo done > \"$NUEE_OUT\""]);
    let r = f.runner_for(&[&s]);
    let start = std::time::Instant::now();
    let out = tokio::time::timeout(Duration::from_secs(20), r.run(&s, b"", DEADLINE))
        .await
        .expect("the lease never finished")
        .expect("task");
    assert_eq!(out, b"done\n");
    assert!(start.elapsed() < Duration::from_secs(10));
    assert!(gone("sleep 100417").await, "the background sleep survived");
}

/// A process that leaves the task's process group and keeps its stdout open cannot hold the
/// lease: the output drain is bounded.
#[tokio::test]
async fn output_drain_is_bounded_when_a_process_escapes_the_group() {
    let f = fixture();
    let s = f.spec(&[
        "sh",
        "-c",
        "python3 -c 'import os, time; os.setsid(); \
         open(os.environ[\"TMPDIR\"] + \"/ready\", \"w\").close(); time.sleep(100419)' & \
         while [ ! -e \"$TMPDIR/ready\" ]; do sleep 0.05; done; echo $! > \"$NUEE_OUT\"",
    ]);
    let r = f.runner_for(&[&s]);
    let start = std::time::Instant::now();
    let res = tokio::time::timeout(Duration::from_secs(30), r.run(&s, b"", DEADLINE)).await;
    // Whatever the outcome, the escaped process is the test's to clean up.
    match Command::new("pkill").args(["-f", "100419"]).status() {
        Ok(_) => {}
        Err(e) => eprintln!("pkill: {e}"),
    }
    let out = res.expect("the output drain never ended").expect("task");
    assert!(!out.is_empty());
    assert!(
        start.elapsed() < Duration::from_secs(20),
        "{:?}",
        start.elapsed()
    );
}

/// Under bwrap the task gets its own session: only a PID namespace takes its children down.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn bwrap_kills_every_process_of_the_task() {
    let f = fixture();
    let s = f.spec(&["sh", "-c", "sleep 100423 & echo done > \"$NUEE_OUT\""]);
    let r = f.runner_for(&[&s]);
    if r.sandbox() != Sandbox::Bwrap {
        eprintln!(
            "bwrap_kills_every_process_of_the_task: no working bwrap here (missing, or user \
             namespaces are disabled); the bwrap path is not exercised on this machine"
        );
        return;
    }
    let out = tokio::time::timeout(Duration::from_secs(20), r.run(&s, b"", DEADLINE))
        .await
        .expect("the lease never finished under bwrap")
        .expect("task");
    assert_eq!(out, b"done\n");
    assert!(
        gone("sleep 100423").await,
        "a process of the task survived bwrap"
    );
}
