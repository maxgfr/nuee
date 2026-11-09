//! `nuee shard`: cut a test suite across the machines of a group.
//!
//! Each shard is one native job on the group's coordinator. A failing test is a result, not a
//! fault: the command's exit code travels in its output, so only a machine that disappears
//! makes a shard run elsewhere.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use nuee_exec::{shard_argv, shard_exit_code};
use nuee_proto::{
    BoundedString, BoundedVec, Comparator, ExecSpec, Manifest, PROTO_VERSION, TaskKind, VerifyMode,
    VerifyPolicy,
};
use nuee_store::{HubConfig, StoreHub};
use serde_json::json;

use super::serve::{self, AnchorJob};
use crate::{Ctx, exit, fail};

/// Arguments of `nuee shard`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// How many pieces to cut the command into (at most 64)
    pub shards: u16,
    /// The group link, or `-` to read it from stdin
    #[arg(long, value_name = "LINK")]
    pub on: String,
    /// Repository to run, an https:// URL (default: this checkout's origin)
    #[arg(long, value_name = "URL")]
    pub repo: Option<String>,
    /// Commit to run, 40 hex digits (default: this checkout's HEAD, which must be pushed)
    #[arg(long, value_name = "SHA")]
    pub rev: Option<String>,
    /// Time limit of one shard, in seconds
    #[arg(long, value_name = "S", default_value_t = 3600)]
    pub timeout: u32,
    /// Where to write each shard's log (default ./nuee-out/shard-<commit>)
    #[arg(long, value_name = "DIR")]
    pub out: Option<PathBuf>,
    /// The command, run by `sh`; `{i}` is the shard (from 1) and `{n}` the shard count
    #[arg(last = true, required = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

fn usage(message: impl Into<String>) -> anyhow::Error {
    fail(exit::USAGE, message)
}

fn git(args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git")
        .args(args)
        .output()
        .map_err(|e| usage(format!("cannot run git: {e}")))?;
    if !out.status.success() {
        return Err(usage(format!(
            "`git {}` failed; pass --repo and --rev",
            args.join(" ")
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// The `https://` form of a git remote URL; `None` when it has none.
pub(crate) fn https_remote(url: &str) -> Option<String> {
    if url.starts_with("https://") {
        return Some(url.to_owned());
    }
    let rest = url
        .strip_prefix("ssh://git@")
        .or_else(|| url.strip_prefix("git@"))?;
    let (host, path) = rest.split_once([':', '/'])?;
    Some(format!("https://{host}/{path}"))
}

/// What to run: the given repository and commit, or this checkout's, which others can only
/// fetch once it is pushed.
fn target(args: &Args) -> anyhow::Result<(String, String)> {
    let repo = match &args.repo {
        Some(r) => r.clone(),
        None => {
            let origin = git(&["remote", "get-url", "origin"])?;
            https_remote(&origin)
                .ok_or_else(|| usage(format!("cannot use the remote {origin}; pass --repo")))?
        }
    };
    let rev = match &args.rev {
        Some(r) => r.clone(),
        None => {
            let head = git(&["rev-parse", "HEAD"])?;
            if git(&["branch", "-r", "--contains", &head])?.is_empty() {
                return Err(usage(format!(
                    "commit {head} is not pushed, so the other machines cannot fetch it"
                )));
            }
            head
        }
    };
    Ok((repo, rev))
}

fn fill(command: &str, i: u16, n: u16) -> String {
    command
        .replace("{i}", &i.to_string())
        .replace("{n}", &n.to_string())
}

fn manifest(args: &Args, repo: &str, rev: &str, command: &str, i: u16) -> anyhow::Result<Manifest> {
    let argv = shard_argv(&fill(command, i, args.shards))
        .into_iter()
        .map(|a| BoundedString::try_from(a).map_err(|_| usage("the command is longer than 1 KiB")))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let exec = ExecSpec {
        repo: BoundedString::try_from(repo).map_err(|_| usage("repository URL too long"))?,
        rev: BoundedString::try_from(rev).map_err(|_| usage("--rev: expected 40 hex digits"))?,
        argv: BoundedVec::try_from(argv).map_err(|_| usage("too many arguments"))?,
        env: BoundedVec::new(),
        timeout_s: args.timeout,
        gpu: false,
    };
    let manifest = Manifest {
        version: PROTO_VERSION,
        job: 0,
        name: BoundedString::try_from(format!("shard-{i}-of-{}", args.shards))
            .map_err(|_| usage("name too long"))?,
        kind: TaskKind::Exec,
        module: None,
        module_size: 0,
        max_memory: 0,
        assets: BoundedVec::new(),
        tasks: 1,
        // Test runs are not reproducible bit for bit, and the roster already says who may run.
        verify: VerifyPolicy {
            mode: VerifyMode::None,
            sample_permille: 0,
            canary_permille: 0,
        },
        compare: Comparator::None,
        deadline_ms: args
            .timeout
            .checked_mul(1000)
            .ok_or_else(|| usage("--timeout is too long"))?,
        exec: Some(exec),
        train: None,
        // The coordinator is filled in when the job is sent.
        coordinator: nuee_proto::SecretKey::from_bytes(&[0; 32]).public(),
    };
    manifest.check().map_err(|e| usage(e.to_string()))?;
    Ok(manifest)
}

fn read_log(dir: &Path) -> anyhow::Result<Vec<u8>> {
    let path = dir.join("task-0.bin");
    std::fs::read(&path)
        .map_err(|e| fail(exit::ERROR, format!("cannot read {}: {e}", path.display())))
}

/// Runs the shards and reports each one.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    if !(1..=64).contains(&args.shards) {
        return Err(usage("the shard count must be between 1 and 64"));
    }
    let command = args.command.join(" ");
    if args.shards > 1 && !command.contains("{i}") {
        return Err(usage(
            "the command has no {i}: every shard would run the same thing",
        ));
    }
    let (repo, rev) = target(&args)?;
    let manifests = (1..=args.shards)
        .map(|i| manifest(&args, &repo, &rev, &command, i))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let out = args.out.clone().unwrap_or_else(|| {
        Path::new("nuee-out").join(format!("shard-{}", rev.get(..12).unwrap_or(&rev)))
    });

    let (net, leader) = super::group::member_session(&args.on, ctx).await?;
    let hub = StoreHub::memory(&net, HubConfig::default());
    let router = net
        .router()
        .accept(iroh_blobs::ALPN, hub.protocol())
        .spawn();
    let result = shards(ctx, &net, &hub, &leader, manifests, &out).await;
    if let Err(e) = router.shutdown().await {
        tracing::debug!("closing the endpoint: {e}");
    }
    let failed = result?;
    ctx.ui.emit(
        "done",
        json!({ "shards": args.shards, "failed": failed, "out": out.display().to_string() }),
    );
    if failed > 0 {
        return Err(fail(
            exit::ERROR,
            format!(
                "{failed} of {} shards failed; logs in {}",
                args.shards,
                out.display()
            ),
        ));
    }
    ctx.ui.say(&format!(
        "all {} shards passed; logs in {}",
        args.shards,
        out.display()
    ));
    Ok(ExitCode::SUCCESS)
}

/// Submits every shard, waits for each, and returns how many failed.
async fn shards(
    ctx: &Ctx,
    net: &nuee_net::Net,
    hub: &StoreHub,
    leader: &nuee_proto::ServiceLink,
    manifests: Vec<Manifest>,
    out: &Path,
) -> anyhow::Result<usize> {
    let mut jobs = Vec::with_capacity(manifests.len());
    for manifest in manifests {
        let name = manifest.name.as_str().to_owned();
        let job = AnchorJob {
            manifest,
            module: Vec::new(),
            assets: Vec::new(),
            inputs: vec![Vec::new()],
        };
        jobs.push((name, serve::post_job(net, hub, leader, job).await?));
    }
    ctx.ui.note(&format!(
        "{} shards submitted; they run on the machines that allow this repository and command",
        jobs.len()
    ));
    let mut failed = 0;
    for (name, id) in jobs {
        let dir = out.join(&name);
        serve::follow_job(ctx, leader, net, id, &dir).await?;
        let log = read_log(&dir)?;
        let code = shard_exit_code(&log);
        let passed = code == Some(0);
        failed += usize::from(!passed);
        ctx.ui.emit(
            "shard",
            json!({ "name": name, "passed": passed, "exit_code": code,
                    "log": dir.join("task-0.bin").display().to_string() }),
        );
        ctx.ui.say(&format!(
            "{name}: {}",
            if passed { "passed" } else { "FAILED" }
        ));
    }
    Ok(failed)
}

#[cfg(test)]
mod tests {
    use super::https_remote;

    #[test]
    fn remotes_become_https() {
        for (from, to) in [
            ("https://github.com/a/b.git", "https://github.com/a/b.git"),
            ("git@github.com:a/b.git", "https://github.com/a/b.git"),
            ("ssh://git@github.com/a/b", "https://github.com/a/b"),
        ] {
            assert_eq!(https_remote(from).as_deref(), Some(to));
        }
        assert_eq!(https_remote("/home/me/repo"), None);
    }
}
