//! Native `exec` tasks: a pinned repository revision, an exact command line, a clean environment,
//! a disposable directory, a deadline, a killed process group and an OS sandbox when one exists.
//!
//! Disabled unless the operator allowlists `repo@rev`; the manifest's [`ExecSpec`] is the whole
//! contract and nothing else from the coordinator is trusted.

use std::fmt;
use std::fs::File;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::str::FromStr;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use nuee_proto::{ExecSpec, FailKind};
use tokio::io::{AsyncRead, AsyncReadExt as _};
use tokio::process::Command;

use crate::wasm::ExecError;

/// Bytes of stdout / stderr kept (the tail).
pub const STREAM_TAIL: usize = 64 * 1024;
/// Default cap on the bytes a task may leave in `$NUEE_OUT`.
pub const DEFAULT_MAX_OUTPUT: u64 = 256 * 1024 * 1024;

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// How long stdout and stderr may stay open once the task's process has ended or been killed.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Environment keys a manifest may never set: the ones the runner owns and the ones that change
/// how a program finds or loads code (interpreter start-up files, library and module paths).
const ENV_DENY_EXACT: &[&str] = &[
    "PATH",
    "HOME",
    "IFS",
    "BASH_ENV",
    "ENV",
    "SHELLOPTS",
    "BASHOPTS",
    "PS4",
    "PROMPT_COMMAND",
    "ZDOTDIR",
    "PYTHONSTARTUP",
    "PYTHONPATH",
    "PYTHONHOME",
    "PYTHONUSERBASE",
    "PYTHONEXECUTABLE",
    "PYTHONWARNINGS",
    "PYTHONBREAKPOINT",
    "PYTHONPLATLIBDIR",
    "PYTHONPYCACHEPREFIX",
    "NODE_OPTIONS",
    "NODE_PATH",
    "PERL5OPT",
    "PERL5LIB",
    "PERLLIB",
    "RUBYOPT",
    "RUBYLIB",
    "CLASSPATH",
    "PERL5DB",
    "GEM_HOME",
    "GEM_PATH",
    "BUNDLE_GEMFILE",
    "RUSTC_WRAPPER",
    "R_PROFILE",
    "R_PROFILE_USER",
    "R_ENVIRON",
    "R_ENVIRON_USER",
    "R_LIBS",
    "R_LIBS_USER",
    "LESSOPEN",
    "LESSCLOSE",
    "PAGER",
    "EDITOR",
    "VISUAL",
    "BROWSER",
    "SHELL",
    "GCONV_PATH",
    "LOCPATH",
    "LANG",
    "TERM",
    "TMPDIR",
    "NUEE_IN",
    "NUEE_OUT",
];
/// Prefixes: the loader, git and ssh, shells, Lua start-up code and module paths, OpenSSL
/// configuration and modules, XDG configuration directories, and package managers' configuration
/// (index URLs, hooks).
const ENV_DENY_PREFIX: &[&str] = &[
    "LD_",
    "DYLD_",
    "GIT_",
    "SSH_",
    "BASH_",
    "LUA_",
    "OPENSSL_",
    "XDG_",
    "PIP_",
    "UV_",
    "NPM_CONFIG_",
    "CONDA_",
];
/// JVM-style injection (`JAVA_TOOL_OPTIONS`, `JDK_JAVA_OPTIONS`, `MAVEN_OPTS`, ...).
const ENV_DENY_SUFFIX: &[&str] = &["_OPTIONS", "_OPTS"];

fn err(kind: FailKind, detail: impl Into<String>) -> ExecError {
    ExecError {
        kind,
        detail: detail.into(),
    }
}

/// One allowlisted repository revision, matched exactly.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AllowEntry {
    /// `https://` repository URL.
    pub repo: String,
    /// 40 lowercase hex characters.
    pub rev: String,
}

/// Why an allowlist entry was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AllowError {
    /// Not of the form `repo@rev`.
    #[error("expected REPO@REV")]
    Shape,
    /// The repository is not an acceptable `https://` URL.
    #[error("the repository must be an https:// URL")]
    Repo,
    /// The revision is not a full commit SHA.
    #[error("the revision must be a full 40-character commit SHA")]
    Rev,
    /// The command line is empty.
    #[error("the command line is empty")]
    Argv,
    /// An environment key no task may set.
    #[error("environment key {0:?} is not allowed")]
    Env(String),
}

impl AllowEntry {
    /// Checks both parts.
    pub fn new(repo: &str, rev: &str) -> Result<Self, AllowError> {
        if !repo_ok(repo) {
            return Err(AllowError::Repo);
        }
        if !rev_ok(rev) {
            return Err(AllowError::Rev);
        }
        Ok(Self {
            repo: repo.to_owned(),
            rev: rev.to_owned(),
        })
    }
}

impl FromStr for AllowEntry {
    type Err = AllowError;
    fn from_str(s: &str) -> Result<Self, AllowError> {
        let (repo, rev) = s.trim().rsplit_once('@').ok_or(AllowError::Shape)?;
        Self::new(repo, rev)
    }
}

impl fmt::Display for AllowEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.repo, self.rev)
    }
}

/// One exact command the operator approved: repository, revision, command line and environment
/// (keys and values). A job that differs in any of them needs a new approval.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExecApproval {
    /// `https://` repository URL.
    pub repo: String,
    /// 40 lowercase hex characters.
    pub rev: String,
    /// The command line, before `$NUEE_*` substitution.
    pub argv: Vec<String>,
    /// The environment the manifest sets, sorted.
    pub env: Vec<(String, String)>,
}

impl ExecApproval {
    /// Checks every part; environment keys that change how programs load code are refused.
    pub fn new(
        repo: &str,
        rev: &str,
        argv: Vec<String>,
        mut env: Vec<(String, String)>,
    ) -> Result<Self, AllowError> {
        AllowEntry::new(repo, rev)?;
        if argv.is_empty() {
            return Err(AllowError::Argv);
        }
        if let Some((k, _)) = env.iter().find(|(k, _)| !env_key_allowed(k)) {
            return Err(AllowError::Env(k.clone()));
        }
        env.sort();
        Ok(Self {
            repo: repo.to_owned(),
            rev: rev.to_owned(),
            argv,
            env,
        })
    }

    /// The approval of exactly what `spec` runs.
    pub fn of(spec: &ExecSpec) -> Result<Self, AllowError> {
        Self::new(
            spec.repo.as_str(),
            spec.rev.as_str(),
            spec.argv.iter().map(|a| a.as_str().to_owned()).collect(),
            spec.env
                .iter()
                .map(|v| (v.key.as_str().to_owned(), v.value.as_str().to_owned()))
                .collect(),
        )
    }

    /// Whether `spec` runs exactly this command.
    pub fn matches(&self, spec: &ExecSpec) -> bool {
        Self::of(spec).is_ok_and(|other| other == *self)
    }

    /// The pinned `repo@rev` of this command.
    pub fn entry(&self) -> AllowEntry {
        AllowEntry {
            repo: self.repo.clone(),
            rev: self.rev.clone(),
        }
    }
}

/// `https://host/...`, printable ASCII, nothing a git option parser could pick up.
fn repo_ok(repo: &str) -> bool {
    let Some(rest) = repo.strip_prefix("https://") else {
        return false;
    };
    !rest.is_empty()
        && !rest.starts_with('-')
        && !rest.starts_with('/')
        && repo.len() <= 256
        && repo.bytes().all(|b| b.is_ascii_graphic() && b != b'\\')
        && !repo.contains("::")
}

fn rev_ok(rev: &str) -> bool {
    rev.len() == 40 && rev.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Whether a manifest may set this environment key.
pub fn env_key_allowed(key: &str) -> bool {
    let b = key.as_bytes();
    let shape = !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_uppercase()
        && b.iter()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'_');
    shape
        && !ENV_DENY_EXACT.contains(&key)
        && !ENV_DENY_PREFIX.iter().any(|p| key.starts_with(p))
        && !ENV_DENY_SUFFIX.iter().any(|p| key.ends_with(p))
}

/// Where repositories are fetched from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepoSource {
    /// The spec's `https://` URL (production).
    Https,
    /// A local repository standing in for every remote (tests only).
    LocalMirror(PathBuf),
}

/// Operator policy for native tasks.
#[derive(Debug, Clone)]
pub struct ExecPolicy {
    /// Pinned `repo@rev` entries, advertised to coordinators; empty means exec is disabled.
    pub allow: Vec<AllowEntry>,
    /// Exact commands that may run; a pinned revision alone runs nothing.
    pub approved: Vec<ExecApproval>,
    /// Whether tasks may use the network (default false).
    pub network: bool,
    /// Repository cache, task directories and the GPU lock (`<data>/exec`).
    pub cache_dir: PathBuf,
    /// Largest output accepted.
    pub max_output: u64,
    /// Time between SIGTERM and SIGKILL.
    pub kill_grace: Duration,
    /// Where repositories come from.
    pub repo_source: RepoSource,
}

impl ExecPolicy {
    /// Defaults: nothing allowed, no network, 256 MiB output, 5 s grace, https fetches.
    pub fn new(cache_dir: PathBuf) -> Self {
        Self {
            allow: Vec::new(),
            approved: Vec::new(),
            network: false,
            cache_dir,
            max_output: DEFAULT_MAX_OUTPUT,
            kill_grace: Duration::from_secs(5),
            repo_source: RepoSource::Https,
        }
    }

    /// Approves exactly what `spec` runs and pins its `repo@rev`.
    pub fn approve(&mut self, spec: &ExecSpec) -> Result<(), AllowError> {
        self.add(ExecApproval::of(spec)?);
        Ok(())
    }

    /// Adds an approved command and pins its `repo@rev`.
    pub fn add(&mut self, approval: ExecApproval) {
        let entry = approval.entry();
        if !self.allow.contains(&entry) {
            self.allow.push(entry);
        }
        if !self.approved.contains(&approval) {
            self.approved.push(approval);
        }
    }
}

/// OS sandbox used around tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sandbox {
    /// macOS `sandbox-exec`.
    SandboxExec,
    /// Linux bubblewrap.
    Bwrap,
    /// None found: tasks run unconfined.
    None,
}

/// Finds a working sandbox on this machine (a probe run, not just a file check).
pub fn detect_sandbox() -> Sandbox {
    let quiet = |c: &mut StdCommand| {
        c.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    if cfg!(target_os = "macos")
        && Path::new(SANDBOX_EXEC).exists()
        && quiet(StdCommand::new(SANDBOX_EXEC).args([
            "-p",
            "(version 1)(allow default)",
            "/usr/bin/true",
        ]))
    {
        return Sandbox::SandboxExec;
    }
    if cfg!(target_os = "linux")
        && quiet(StdCommand::new("bwrap").args([
            "--ro-bind",
            "/",
            "/",
            "--unshare-net",
            "--unshare-pid",
            "--die-with-parent",
            "true",
        ]))
    {
        return Sandbox::Bwrap;
    }
    Sandbox::None
}

/// Arguments of a DiLoCo exec trainer task, substituted for `$NUEE_STEPS`, `$NUEE_SHARD`
/// (`i/n`) and `$NUEE_SEED`; `$NUEE_INIT` names the file holding the task input (the params).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrainArgs {
    /// Local steps.
    pub steps: u32,
    /// Shard index.
    pub shard: u32,
    /// Shard count.
    pub n_shards: u32,
    /// Seed of the local run.
    pub seed: u64,
}

/// Replaces the `$NUEE_*` placeholders of an argv (whole arguments only).
pub fn substitute_argv(
    argv: &[&str],
    input: &str,
    output: &str,
    train: Option<&TrainArgs>,
) -> Vec<String> {
    argv.iter()
        .map(|a| match (*a, train) {
            ("$NUEE_IN", _) => input.to_owned(),
            ("$NUEE_OUT", _) => output.to_owned(),
            ("$NUEE_INIT", Some(_)) => input.to_owned(),
            ("$NUEE_STEPS", Some(t)) => t.steps.to_string(),
            ("$NUEE_SHARD", Some(t)) => format!("{}/{}", t.shard, t.n_shards),
            ("$NUEE_SEED", Some(t)) => t.seed.to_string(),
            (other, _) => other.to_owned(),
        })
        .collect()
}

/// Runs allowlisted native commands.
#[derive(Debug)]
pub struct NativeRunner {
    policy: ExecPolicy,
    sandbox: Sandbox,
}

impl NativeRunner {
    /// A runner with the sandbox found on this machine.
    pub fn new(policy: ExecPolicy) -> Self {
        let sandbox = detect_sandbox();
        if sandbox == Sandbox::None {
            tracing::warn!("no sandbox-exec or bwrap: native tasks will run without an OS sandbox");
        }
        Self { policy, sandbox }
    }

    /// The sandbox wrapped around each task.
    pub fn sandbox(&self) -> Sandbox {
        self.sandbox
    }

    /// The policy.
    pub fn policy(&self) -> &ExecPolicy {
        &self.policy
    }

    /// `repo@rev` strings to advertise in `Caps::exec_allow`.
    pub fn advertised(&self) -> Vec<String> {
        self.policy.allow.iter().map(ToString::to_string).collect()
    }

    /// Whether `spec`'s `repo@rev` is pinned here (it may still need an approval to run).
    pub fn pinned(&self, spec: &ExecSpec) -> bool {
        self.policy
            .allow
            .iter()
            .any(|a| a.repo == spec.repo.as_str() && a.rev == spec.rev.as_str())
    }

    /// Whether `spec` may run here: its `repo@rev` is pinned and the operator approved this
    /// exact command line and environment.
    pub fn allowed(&self, spec: &ExecSpec) -> bool {
        self.pinned(spec) && self.policy.approved.iter().any(|a| a.matches(spec))
    }

    /// Runs one task: `input` goes to `$NUEE_IN`, the bytes left in `$NUEE_OUT` come back.
    pub async fn run(
        &self,
        spec: &ExecSpec,
        input: &[u8],
        deadline: Duration,
    ) -> Result<Vec<u8>, ExecError> {
        self.run_task(spec, input, deadline, None).await
    }

    /// Runs one exec trainer task: `params` (safetensors) goes to `$NUEE_INIT`, the
    /// training placeholders are substituted, the new params left in `$NUEE_OUT` come back.
    pub async fn run_train(
        &self,
        spec: &ExecSpec,
        params: &[u8],
        train: TrainArgs,
        deadline: Duration,
    ) -> Result<Vec<u8>, ExecError> {
        if train.n_shards == 0 || train.shard >= train.n_shards {
            return Err(err(FailKind::Invalid, "shard out of range"));
        }
        self.run_task(spec, params, deadline, Some(train)).await
    }

    async fn run_task(
        &self,
        spec: &ExecSpec,
        input: &[u8],
        deadline: Duration,
        train: Option<TrainArgs>,
    ) -> Result<Vec<u8>, ExecError> {
        if !self.pinned(spec) {
            return Err(err(
                FailKind::Unsupported,
                format!(
                    "{}@{} is not allowlisted on this machine",
                    spec.repo.as_str(),
                    spec.rev.as_str()
                ),
            ));
        }
        if !repo_ok(spec.repo.as_str()) {
            return Err(err(FailKind::Invalid, "repository is not an https:// URL"));
        }
        if !rev_ok(spec.rev.as_str()) {
            return Err(err(FailKind::Invalid, "revision is not a 40-hex commit"));
        }
        if spec.argv.is_empty() {
            return Err(err(FailKind::Invalid, "empty argv"));
        }
        if let Some(bad) = spec.env.iter().find(|v| !env_key_allowed(v.key.as_str())) {
            return Err(err(
                FailKind::Invalid,
                format!("environment key {:?} is not allowed", bad.key.as_str()),
            ));
        }
        if !self.allowed(spec) {
            return Err(err(
                FailKind::Unsupported,
                format!(
                    "this exact command of {}@{} was not approved on this machine",
                    spec.repo.as_str(),
                    spec.rev.as_str()
                ),
            ));
        }

        let _gpu = if spec.gpu {
            Some(self.gpu_lock().await?)
        } else {
            None
        };

        let io = |what: &str, e: std::io::Error| err(FailKind::Io, format!("{what}: {e}"));
        let tasks = self.policy.cache_dir.join("tasks");
        std::fs::create_dir_all(&tasks).map_err(|e| io("task directory", e))?;
        let task = tempfile::Builder::new()
            .prefix("task-")
            .tempdir_in(&tasks)
            .map_err(|e| io("task directory", e))?;
        let root = task
            .path()
            .canonicalize()
            .map_err(|e| io("task directory", e))?;
        let src = root.join("src");
        let tmp = root.join("tmp");
        let in_path = root.join("in");
        let out_path = root.join("out");
        std::fs::create_dir(&tmp).map_err(|e| io("task directory", e))?;
        std::fs::write(&in_path, input).map_err(|e| io("task input", e))?;

        // The checkout and the command share one deadline: a stalled remote cannot hold the lease.
        let limit = deadline.min(Duration::from_secs(u64::from(spec.timeout_s.max(1))));
        let until = Instant::now() + limit;
        let checkout = {
            let policy = self.policy.clone();
            let spec = spec.clone();
            let src = src.clone();
            tokio::task::spawn_blocking(move || checkout(&policy, &spec, &src, until))
                .await
                .map_err(|e| err(FailKind::Io, format!("checkout thread: {e}")))?
        };
        let repo_dir = checkout?;

        let limit = until.saturating_duration_since(Instant::now());
        let res = self
            .spawn_and_wait(
                spec,
                &TaskPaths {
                    root: &root,
                    src: &src,
                    tmp: &tmp,
                    input: &in_path,
                    output: &out_path,
                },
                train.as_ref(),
                limit,
            )
            .await;
        let res = res.and_then(|()| self.read_output(&out_path));
        remove_worktree(&repo_dir, &src);
        res
    }

    async fn gpu_lock(&self) -> Result<File, ExecError> {
        std::fs::create_dir_all(&self.policy.cache_dir)
            .map_err(|e| err(FailKind::Io, format!("cache directory: {e}")))?;
        let path = self.policy.cache_dir.join("gpu.lock");
        tokio::task::spawn_blocking(move || {
            let f = File::options()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&path)?;
            f.lock()?;
            Ok::<File, std::io::Error>(f)
        })
        .await
        .map_err(|e| err(FailKind::Io, format!("gpu lock thread: {e}")))?
        .map_err(|e| err(FailKind::Io, format!("gpu lock: {e}")))
    }

    fn read_output(&self, out_path: &Path) -> Result<Vec<u8>, ExecError> {
        let meta = match std::fs::symlink_metadata(out_path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(err(FailKind::Invalid, "the task did not write $NUEE_OUT"));
            }
            Err(e) => return Err(err(FailKind::Io, format!("output: {e}"))),
        };
        if !meta.is_file() {
            return Err(err(FailKind::Invalid, "$NUEE_OUT is not a regular file"));
        }
        if meta.len() > self.policy.max_output {
            return Err(err(
                FailKind::Invalid,
                format!("output of {} bytes is above the cap", meta.len()),
            ));
        }
        let mut buf = Vec::new();
        File::open(out_path)
            .and_then(|f| f.take(self.policy.max_output + 1).read_to_end(&mut buf))
            .map_err(|e| err(FailKind::Io, format!("output: {e}")))?;
        if buf.len() as u64 > self.policy.max_output {
            return Err(err(FailKind::Invalid, "output above the cap"));
        }
        Ok(buf)
    }

    async fn spawn_and_wait(
        &self,
        spec: &ExecSpec,
        paths: &TaskPaths<'_>,
        train: Option<&TrainArgs>,
        limit: Duration,
    ) -> Result<(), ExecError> {
        let TaskPaths {
            root,
            src,
            tmp,
            input: in_path,
            output: out_path,
        } = *paths;
        let in_s = in_path.to_string_lossy().into_owned();
        let out_s = out_path.to_string_lossy().into_owned();
        let raw: Vec<&str> = spec.argv.iter().map(|a| a.as_str()).collect();
        let argv = substitute_argv(&raw, &in_s, &out_s, train);

        let mut cmd = match self.sandbox {
            Sandbox::SandboxExec => {
                let mut c = Command::new(SANDBOX_EXEC);
                c.arg("-p").arg(macos_profile(self.policy.network));
                c.arg("-D").arg(format!("TASK_DIR={}", root.display()));
                for (i, dir) in cache_dirs().iter().enumerate() {
                    c.arg("-D").arg(format!("CACHE_{i}={}", dir.display()));
                }
                c.args(&argv);
                c
            }
            Sandbox::Bwrap => {
                let mut c = Command::new("bwrap");
                c.args(["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"]);
                c.arg("--bind").arg(root).arg(root);
                for dir in cache_dirs() {
                    if dir.is_dir() {
                        c.arg("--bind").arg(&dir).arg(&dir);
                    }
                }
                if !self.policy.network {
                    c.arg("--unshare-net");
                }
                // --new-session puts the task in its own session, out of reach of the group
                // kill below; the PID namespace ends every process of the task with bwrap.
                c.args([
                    "--unshare-pid",
                    "--die-with-parent",
                    "--new-session",
                    "--chdir",
                ]);
                c.arg(src).arg("--").args(&argv);
                c
            }
            Sandbox::None => {
                let mut c = Command::new(&argv[0]);
                c.args(&argv[1..]);
                c
            }
        };
        cmd.env_clear()
            .env(
                "PATH",
                std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
            )
            .env("HOME", home())
            .env(
                "LANG",
                std::env::var_os("LANG").unwrap_or_else(|| "C.UTF-8".into()),
            )
            .env("TERM", "dumb")
            .env("TMPDIR", tmp)
            .env("NUEE_IN", in_path)
            .env("NUEE_OUT", out_path);
        for v in spec.env.iter() {
            cmd.env(v.key.as_str(), v.value.as_str());
        }
        cmd.current_dir(src)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| err(FailKind::Io, format!("cannot start {:?}: {e}", argv[0])))?;
        let pgid = child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .map(Pid::from_raw)
            .ok_or_else(|| err(FailKind::Io, "child without a pid"))?;
        let guard = GroupGuard { pgid };
        let out_t = child.stdout.take().map(|s| tokio::spawn(tail(s)));
        let err_t = child.stderr.take().map(|s| tokio::spawn(tail(s)));

        let status = match tokio::time::timeout(limit, child.wait()).await {
            Ok(r) => Some(r.map_err(|e| err(FailKind::Io, format!("wait: {e}")))?),
            Err(_) => {
                signal_group(pgid, Signal::SIGTERM);
                if tokio::time::timeout(self.policy.kill_grace, child.wait())
                    .await
                    .is_err()
                {
                    tracing::debug!("task ignored SIGTERM");
                }
                None
            }
        };
        // Leftover children would keep the pipes open: the group never outlives its task, and
        // a process that left the group gets a short grace before its pipes are dropped.
        drop(guard);
        let stderr = match err_t {
            Some(t) => drain(t, "stderr").await,
            None => Vec::new(),
        };
        if let Some(t) = out_t {
            drain(t, "stdout").await;
        }
        let unconfined = if self.sandbox == Sandbox::None {
            " (ran without an OS sandbox)"
        } else {
            ""
        };
        match status {
            None => Err(err(
                FailKind::Timeout,
                format!(
                    "killed after {} s{unconfined}: {}",
                    limit.as_secs(),
                    tail_text(&stderr)
                ),
            )),
            Some(s) if s.success() => Ok(()),
            Some(s) => Err(err(
                FailKind::Trap,
                format!(
                    "exit {}{unconfined}: {}",
                    s.code()
                        .map_or_else(|| "by signal".to_owned(), |c| c.to_string()),
                    tail_text(&stderr)
                ),
            )),
        }
    }
}

#[derive(Clone, Copy)]
struct TaskPaths<'a> {
    root: &'a Path,
    src: &'a Path,
    tmp: &'a Path,
    input: &'a Path,
    output: &'a Path,
}

/// Kills the whole process group when the task ends, is cancelled or its future is dropped.
struct GroupGuard {
    pgid: Pid,
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        signal_group(self.pgid, Signal::SIGKILL);
    }
}

fn signal_group(pgid: Pid, sig: Signal) {
    match killpg(pgid, sig) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => {}
        Err(e) => tracing::warn!("cannot signal process group {pgid}: {e}"),
    }
}

async fn tail<R: AsyncRead + Unpin>(mut r: R) -> Vec<u8> {
    let mut keep = Vec::new();
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        match r.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                keep.extend_from_slice(buf.get(..n).unwrap_or_default());
                if keep.len() > STREAM_TAIL {
                    let cut = keep.len() - STREAM_TAIL;
                    keep.drain(..cut);
                }
            }
            Err(e) => {
                tracing::debug!("task stream: {e}");
                break;
            }
        }
    }
    keep
}

/// Waits for a stream reader for at most [`DRAIN_GRACE`], then drops the pipe.
async fn drain(mut t: tokio::task::JoinHandle<Vec<u8>>, what: &str) -> Vec<u8> {
    match tokio::time::timeout(DRAIN_GRACE, &mut t).await {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(e)) => {
            tracing::debug!("{what} reader ended abnormally: {e}");
            Vec::new()
        }
        Err(_) => {
            tracing::debug!("{what} still open after the task ended: dropping it");
            t.abort();
            Vec::new()
        }
    }
}

fn tail_text(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let s = s.trim();
    let start = s.char_indices().rev().nth(199).map_or(0, |(i, _)| i);
    s.get(start..).unwrap_or_default().to_owned()
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

/// User caches GPU stacks write to (kernel caches, model downloads).
fn cache_dirs() -> Vec<PathBuf> {
    let h = home();
    let dirs = [h.join("Library/Caches"), h.join(".cache")];
    dirs.into_iter()
        .map(|d| d.canonicalize().unwrap_or(d))
        .collect()
}

/// Allows everything except writes outside the task and cache directories and, unless the
/// policy says otherwise, any IP networking.
fn macos_profile(network: bool) -> String {
    let mut p = String::from("(version 1)\n(allow default)\n");
    if !network {
        p.push_str("(deny network-outbound (remote ip))\n(deny network-inbound (local ip))\n(deny network-bind (local ip))\n");
    }
    p.push_str(
        "(deny file-write*)\n(allow file-write*\n  (subpath (param \"TASK_DIR\"))\n  \
         (subpath (param \"CACHE_0\"))\n  (subpath (param \"CACHE_1\"))\n  \
         (literal \"/dev/null\")\n  (literal \"/dev/zero\")\n  (literal \"/dev/dtracehelper\")\n  \
         (regex #\"^/dev/fd/\"))\n",
    );
    p
}

fn git(dir: &Path, source: &RepoSource) -> StdCommand {
    let mut c = StdCommand::new("git");
    c.env_clear()
        .env(
            "PATH",
            std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into()),
        )
        .env("HOME", home())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "submodule.recurse=false",
        ]);
    match source {
        RepoSource::Https => c.args([
            "-c",
            "protocol.allow=never",
            "-c",
            "protocol.https.allow=always",
        ]),
        RepoSource::LocalMirror(_) => c.args(["-c", "protocol.file.allow=always"]),
    };
    c.stdin(Stdio::null());
    c
}

fn run_git(mut c: StdCommand) -> Result<String, String> {
    match c.output() {
        Ok(o) if o.status.success() => Ok(String::from_utf8_lossy(&o.stdout).trim().to_owned()),
        Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_owned()),
        Err(e) => Err(format!("cannot run git: {e}")),
    }
}

/// Why a bounded git command failed.
enum GitError {
    /// It ran past the deadline and its process group was killed.
    Timeout,
    /// It failed (stderr), or could not run.
    Failed(String),
}

/// Runs git in its own process group until `until`; past it, the whole group (git and its
/// remote helpers) is killed.
fn run_git_until(mut c: StdCommand, until: Instant) -> Result<String, GitError> {
    use std::os::unix::process::CommandExt as _;
    let failed = |what: &str, e: std::io::Error| GitError::Failed(format!("{what}: {e}"));
    let mut out = tempfile::tempfile().map_err(|e| failed("git output", e))?;
    let mut errs = tempfile::tempfile().map_err(|e| failed("git output", e))?;
    c.stdout(out.try_clone().map_err(|e| failed("git output", e))?)
        .stderr(errs.try_clone().map_err(|e| failed("git output", e))?)
        .process_group(0);
    let mut child = c.spawn().map_err(|e| failed("cannot run git", e))?;
    let pgid = i32::try_from(child.id())
        .map(Pid::from_raw)
        .map_err(|_| GitError::Failed("git without a pid".to_owned()))?;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= until => {
                signal_group(pgid, Signal::SIGKILL);
                if let Err(e) = child.wait() {
                    tracing::debug!("reaping git: {e}");
                }
                return Err(GitError::Timeout);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => {
                signal_group(pgid, Signal::SIGKILL);
                return Err(failed("waiting for git", e));
            }
        }
    };
    // Helpers git left behind must not outlive it either.
    signal_group(pgid, Signal::SIGKILL);
    let read = |f: &mut File| {
        use std::io::Seek as _;
        let mut s = String::new();
        f.rewind()
            .and_then(|()| f.take(STREAM_TAIL as u64).read_to_string(&mut s))
            .map(|_| s.trim().to_owned())
    };
    if status.success() {
        read(&mut out).map_err(|e| failed("git output", e))
    } else {
        Err(GitError::Failed(
            read(&mut errs).unwrap_or_else(|e| format!("git failed ({e})")),
        ))
    }
}

/// Takes the per-repository lock, waiting no later than `until`.
fn lock_until(path: &Path, until: Instant) -> Result<File, ExecError> {
    let f = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|e| err(FailKind::Io, format!("repo lock: {e}")))?;
    loop {
        match f.try_lock() {
            Ok(()) => return Ok(f),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < until => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(err(
                    FailKind::Io,
                    "checkout timed out waiting for the repository lock",
                ));
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(err(FailKind::Io, format!("repo lock: {e}")));
            }
        }
    }
}

/// Fetches `rev` into the cached repository and checks it out at `dest`, all before `until`;
/// returns the cache repo.
fn checkout(
    policy: &ExecPolicy,
    spec: &ExecSpec,
    dest: &Path,
    until: Instant,
) -> Result<PathBuf, ExecError> {
    let repo = spec.repo.as_str();
    let rev = spec.rev.as_str();
    let repos = policy.cache_dir.join("repos");
    std::fs::create_dir_all(&repos).map_err(|e| err(FailKind::Io, format!("repo cache: {e}")))?;
    let dir = repos.join(blake3::hash(repo.as_bytes()).to_hex().as_str());
    // One git writer per repository at a time; the lock is released on every return.
    let lock = lock_until(&dir.with_extension("lock"), until)?;
    let src = &policy.repo_source;
    let io = |what: &str, e: String| err(FailKind::Io, format!("{what}: {e}"));
    let bounded = |what: &str, c: StdCommand| {
        run_git_until(c, until).map_err(|e| match e {
            GitError::Timeout => err(FailKind::Io, format!("{what}: checkout timed out")),
            GitError::Failed(e) => io(what, e),
        })
    };

    if !dir.join("HEAD").exists() {
        std::fs::create_dir_all(&dir).map_err(|e| io("repo cache", e.to_string()))?;
        let mut init = git(&dir, src);
        init.args(["init", "--quiet", "--bare"]);
        bounded("git init", init)?;
    }
    let remote = match src {
        RepoSource::Https => repo.to_owned(),
        RepoSource::LocalMirror(p) => p.to_string_lossy().into_owned(),
    };
    let have = {
        let mut c = git(&dir, src);
        c.args(["cat-file", "-e"]).arg(format!("{rev}^{{commit}}"));
        match run_git_until(c, until) {
            Ok(_) => true,
            Err(GitError::Failed(_)) => false,
            Err(GitError::Timeout) => {
                return Err(err(FailKind::Io, "git cat-file: checkout timed out"));
            }
        }
    };
    if !have {
        let mut fetch = git(&dir, src);
        fetch
            .args([
                "fetch",
                "--quiet",
                "--depth",
                "1",
                "--no-tags",
                "--no-recurse-submodules",
                "--",
            ])
            .arg(&remote)
            .arg(rev);
        if let Err(e) = run_git_until(fetch, until) {
            let e = match e {
                GitError::Timeout => {
                    return Err(err(
                        FailKind::Io,
                        format!("git fetch {rev}: checkout timed out (the remote stalled)"),
                    ));
                }
                GitError::Failed(e) => e,
            };
            let missing = [
                "not our ref",
                "remote ref",
                "unadvertised object",
                "upload-pack: not",
            ]
            .iter()
            .any(|m| e.contains(m));
            let kind = if missing {
                FailKind::Invalid
            } else {
                FailKind::Io
            };
            return Err(err(kind, format!("git fetch {rev}: {e}")));
        }
    }
    let mut add = git(&dir, src);
    add.args(["worktree", "add", "--quiet", "--detach", "--"])
        .arg(dest)
        .arg(rev);
    bounded("checkout", add)?;
    drop(lock);

    let mut head = git(dest, src);
    head.args(["rev-parse", "HEAD"]);
    let got = bounded("rev-parse", head)?;
    if got != rev {
        remove_worktree(&dir, dest);
        return Err(err(
            FailKind::Invalid,
            format!("checked out {got}, expected {rev}"),
        ));
    }
    Ok(dir)
}

fn remove_worktree(repo: &Path, dest: &Path) {
    let mut c = git(repo, &RepoSource::Https);
    c.args(["worktree", "remove", "--force", "--"]).arg(dest);
    if let Err(e) = run_git(c) {
        tracing::debug!("worktree remove: {e}");
    }
    let mut prune = git(repo, &RepoSource::Https);
    prune.args(["worktree", "prune"]);
    if let Err(e) = run_git(prune) {
        tracing::debug!("worktree prune: {e}");
    }
}
