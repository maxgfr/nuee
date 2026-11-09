//! `nuee build`: build a commit on one machine of a group and bring back what it produced.
//!
//! The job is named by its commit and command, so a teammate who builds the same commit gets the
//! first build's output at once. A failing build is a result: its log comes back.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nuee_exec::{BUILD_LOG, MAX_KEPT, build_argv, kept_path_ok, shard_exit_code};
use serde_json::json;

use super::group::GroupJob;
use super::shard::{exec_manifest, fresh_suffix, target};
use crate::{Ctx, exit, fail};

/// Arguments of `nuee build`.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// The group link, or `-` to read it from stdin
    #[arg(long, value_name = "LINK")]
    pub on: String,
    /// What to bring back, relative to the repository, e.g. `dist` (repeat)
    #[arg(long, value_name = "PATH")]
    pub keep: Vec<String>,
    /// Repository to build, an https:// URL (default: this checkout's origin)
    #[arg(long, value_name = "URL")]
    pub repo: Option<String>,
    /// Commit to build, 40 hex digits (default: this checkout's HEAD, which must be pushed)
    #[arg(long, value_name = "SHA")]
    pub rev: Option<String>,
    /// Time limit of the build, in seconds
    #[arg(long, value_name = "S", default_value_t = 3600)]
    pub timeout: u32,
    /// Build again even if this commit and command already have a result
    #[arg(long)]
    pub fresh: bool,
    /// Where to put what comes back (default: the current directory)
    #[arg(long, value_name = "DIR", default_value = ".")]
    pub out: PathBuf,
    /// The command, run by `sh`
    #[arg(last = true, required = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

fn usage(message: impl Into<String>) -> anyhow::Error {
    fail(exit::USAGE, message)
}

/// What came out of a build.
#[derive(Debug)]
struct Unpacked {
    log: Vec<u8>,
    files: usize,
    skipped: usize,
}

/// Writes the regular files of a build's archive under `out`. The archive comes from another
/// machine: a path leaving `out` is an error, and links are not created at all.
fn unpack(archive: &[u8], out: &Path) -> anyhow::Result<Unpacked> {
    let corrupt = |e: std::io::Error| fail(exit::ERROR, format!("the build's output: {e}"));
    let mut done = Unpacked {
        log: Vec::new(),
        files: 0,
        skipped: 0,
    };
    let mut reader = tar::Archive::new(archive);
    for entry in reader.entries().map_err(corrupt)? {
        let mut entry = entry.map_err(corrupt)?;
        let kind = entry.header().entry_type();
        if entry.path().map_err(corrupt)?.as_ref() == Path::new(BUILD_LOG) {
            std::io::Read::read_to_end(&mut entry, &mut done.log).map_err(corrupt)?;
            continue;
        }
        if !kind.is_file() && !kind.is_dir() {
            done.skipped += 1;
            continue;
        }
        if !entry.unpack_in(out).map_err(corrupt)? {
            return Err(fail(
                exit::ERROR,
                "the build's output names a path outside its directory",
            ));
        }
        done.files += usize::from(kind.is_file());
    }
    Ok(done)
}

/// Runs the build.
pub async fn run(args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    if args.keep.len() > MAX_KEPT {
        return Err(usage(format!("at most {MAX_KEPT} paths can be kept")));
    }
    if let Some(bad) = args.keep.iter().find(|p| !kept_path_ok(p)) {
        return Err(usage(format!(
            "--keep {bad}: expected a path inside the repository"
        )));
    }
    let command = args.command.join(" ");
    let (repo, rev) = target(args.repo.as_deref(), args.rev.as_deref())?;
    let name = format!("build{}", fresh_suffix(args.fresh));
    let manifest = exec_manifest(
        name.clone(),
        &repo,
        &rev,
        build_argv(&command, &args.keep),
        args.timeout,
    )?;

    let work = tempfile::tempdir().map_err(|e| fail(exit::ERROR, format!("temp dir: {e}")))?;
    let job = GroupJob {
        manifest,
        dir: work.path().to_path_buf(),
    };
    super::group::run_jobs(&args.on, ctx, vec![job]).await?;

    let path = work.path().join("task-0.bin");
    let archive = std::fs::read(&path)
        .map_err(|e| fail(exit::ERROR, format!("cannot read {}: {e}", path.display())))?;
    std::fs::create_dir_all(&args.out)
        .map_err(|e| fail(exit::ERROR, format!("{}: {e}", args.out.display())))?;
    let out = unpack(&archive, &args.out)?;
    let log = args.out.join("nuee-build.log");
    std::fs::write(&log, &out.log)
        .map_err(|e| fail(exit::ERROR, format!("{}: {e}", log.display())))?;
    let code = shard_exit_code(&out.log);
    ctx.ui.emit(
        "build",
        json!({ "passed": code == Some(0), "exit_code": code, "files": out.files,
                "skipped_links": out.skipped, "log": log.display().to_string() }),
    );
    if code != Some(0) {
        return Err(fail(
            exit::ERROR,
            format!("the build failed; its log is in {}", log.display()),
        ));
    }
    ctx.ui.say(&format!(
        "build passed: {} files in {}",
        out.files,
        args.out.display()
    ));
    if out.skipped > 0 {
        ctx.ui
            .note(&format!("{} links were not brought back", out.skipped));
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive(entries: &[(&str, tar::EntryType, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, kind, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(*kind);
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            // `set_path` refuses `..`, which is exactly what another machine would not do.
            let bytes = name.as_bytes();
            header.as_old_mut().name[..bytes.len()].copy_from_slice(bytes);
            if kind.is_symlink() {
                header.set_link_name("/etc/passwd").expect("link");
            }
            header.set_cksum();
            builder.append(&header, *data).expect("append");
        }
        builder.into_inner().expect("finish")
    }

    #[test]
    fn output_stays_in_its_directory() {
        let dir = tempfile::tempdir().expect("dir");
        let plain = tar::EntryType::Regular;
        let got = unpack(
            &archive(&[
                (BUILD_LOG, plain, b"log\nnuee-exit-code 0\n"),
                ("dist/app.js", plain, b"app"),
                ("dist/link", tar::EntryType::Symlink, b""),
            ]),
            dir.path(),
        )
        .expect("unpack");
        assert_eq!(got.files, 1);
        assert_eq!(got.skipped, 1);
        assert_eq!(shard_exit_code(&got.log), Some(0));
        assert_eq!(
            std::fs::read(dir.path().join("dist/app.js")).expect("file"),
            b"app"
        );
        assert!(!dir.path().join("dist/link").exists());
        assert!(!dir.path().join(BUILD_LOG).exists());

        let evil = archive(&[("../escaped", plain, b"x")]);
        let inner = dir.path().join("inner");
        std::fs::create_dir(&inner).expect("dir");
        assert!(unpack(&evil, &inner).is_err() || !dir.path().join("escaped").exists());
        assert!(!dir.path().join("escaped").exists());
    }
}
