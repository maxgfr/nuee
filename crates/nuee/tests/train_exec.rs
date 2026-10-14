//! One DiLoCo round with an exec trainer: the lease input goes through the native runner (local
//! mirror, python3 trainer writing safetensors), the worker-side `ExecEncoder` turns the new
//! parameters into a delta (as the CLI's exec train path does), and the coordinator ingests it.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use nuee_exec::TrainArgs;
use nuee_exec::native::{AllowEntry, ExecPolicy, NativeRunner, RepoSource};
use nuee_proto::{BoundedString, BoundedVec, DeltaCodec, ExecSpec, Hash, TrainSpec, TrainerKind};
use nuee_train::{ExecEncoder, Params, TaskInput, TrainState};

const REPO: &str = "https://example.invalid/nuee/trainer.git";

/// Adds `0.01 * steps * (shard + 1)` to every value and reports loss = 1 / (shard + 1).
const TRAINER: &str = r#"import json, struct, sys
a = dict(zip(sys.argv[1::2], sys.argv[2::2]))
raw = open(a["--init"], "rb").read()
n = struct.unpack("<Q", raw[:8])[0]
head = json.loads(raw[8:8 + n])
data = raw[8 + n:]
steps = int(a["--steps"])
shard, count = map(int, a["--shard"].split("/"))
assert int(a["--seed"]) >= 0 and count > shard
out = {}
blob = b""
for name in sorted(k for k in head if k != "__metadata__"):
    t = head[name]
    b, e = t["data_offsets"]
    vals = struct.unpack("<%df" % ((e - b) // 4), data[b:e])
    vals = [v + 0.01 * steps * (shard + 1) for v in vals]
    enc = struct.pack("<%df" % len(vals), *vals)
    out[name] = {"dtype": "F32", "shape": t["shape"], "data_offsets": [len(blob), len(blob) + len(enc)]}
    blob += enc
out["__metadata__"] = {"nuee.loss": str(1.0 / (shard + 1))}
h = json.dumps(out).encode()
h += b" " * ((8 - len(h) % 8) % 8)
open(a["--out"], "wb").write(struct.pack("<Q", len(h)) + h + blob)
"#;

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
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn bs<const N: usize>(s: &str) -> BoundedString<N> {
    BoundedString::try_from(s).expect("bounded")
}

#[tokio::test]
async fn exec_trainer_round_through_the_native_runner() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mirror = dir.path().join("mirror");
    std::fs::create_dir(&mirror).expect("mkdir");
    std::fs::write(mirror.join("train.py"), TRAINER).expect("write");
    git(&mirror, &["init", "--quiet", "-b", "main"]);
    git(&mirror, &["add", "."]);
    git(
        &mirror,
        &[
            "-c",
            "user.name=f",
            "-c",
            "user.email=f@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "t",
        ],
    );
    let rev = git(&mirror, &["rev-parse", "HEAD"]);
    let mut policy = ExecPolicy::new(dir.path().join("cache"));
    policy.allow = vec![AllowEntry::new(REPO, &rev).expect("entry")];
    policy.repo_source = RepoSource::LocalMirror(mirror.clone());
    let argv = [
        "python3",
        "train.py",
        "--init",
        "$NUEE_INIT",
        "--steps",
        "$NUEE_STEPS",
        "--shard",
        "$NUEE_SHARD",
        "--seed",
        "$NUEE_SEED",
        "--out",
        "$NUEE_OUT",
    ];
    let exec = ExecSpec {
        repo: bs(REPO),
        rev: bs(&rev),
        argv: BoundedVec::try_from(argv.iter().map(|a| bs(a)).collect::<Vec<_>>()).expect("argv"),
        env: BoundedVec::new(),
        timeout_s: 60,
        gpu: false,
    };
    policy.approve(&exec).expect("approvable");
    let runner = NativeRunner::new(policy);

    let init = Params::new(vec![
        ("bias".into(), vec![1], vec![0.5]),
        ("weight".into(), vec![2, 2], vec![1.0, 2.0, 3.0, 4.0]),
    ])
    .expect("params");
    let spec = TrainSpec {
        rounds: 2,
        inner_steps: 10,
        workers: 2,
        outer_lr: 0.7,
        outer_momentum: 0.9,
        init: Hash::of(b"init"),
        trainer: TrainerKind::Exec,
        codec: DeltaCodec::F32,
        trim: 0.2,
        cos_min: 0.0,
        norm_max_ratio: 3.0,
    };
    let mut state = TrainState::new(spec, init.clone(), 5).expect("state");
    let mut enc = ExecEncoder::new();
    for task in state.round_tasks(0) {
        let input = state.task_input(&task);
        let t = TaskInput::decode(&input).expect("ntr1");
        let args = TrainArgs {
            steps: t.steps,
            shard: t.shard,
            n_shards: t.n_shards,
            seed: t.seed,
        };
        let out = runner
            .run_train(&exec, t.params, args, Duration::from_secs(60))
            .await
            .unwrap_or_else(|e| panic!("trainer: {e:?}"));
        let out = enc
            .encode(0, state.codec(), t.params, &out)
            .expect("encode");
        state.ingest(&task, &out).expect("ingest");
    }
    let report = state.finish_round().expect("round");
    assert_eq!(report.accepted.len(), 2);
    // Shards 0 and 1 moved every value by 0.1 and 0.2: mean delta 0.15, Nesterov first step
    // applies lr * (1 + mu) = 1.33 times it. Losses 1.0 and 0.5.
    assert_eq!(report.mean_loss, Some(0.75));
    for (now, before) in state.params().data.iter().zip(&init.data) {
        assert!(
            (now - before - 1.33 * 0.15).abs() < 1e-4,
            "{now} vs {before}"
        );
    }
}
