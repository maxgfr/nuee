//! `nuee selftest`: offline checks with no key file, no network and no write outside a
//! temporary directory (safe in packaging sandboxes).

use std::collections::{BTreeMap, VecDeque};
use std::process::ExitCode;
use std::time::Duration;

use nuee_core::{
    Action, Coordinator, CoordinatorConfig, Event, JobSpec, Outcome, Worker, WorkerAction,
    WorkerConfig,
};
use nuee_exec::{RunLimits, RunnerConfig, WasmRunner};
use nuee_proto::limits::MAX_WASM_MEMORY;
use nuee_proto::{
    BoundedBytes, BoundedString, BoundedVec, Caps, Comparator, FileLink, Hash, JobLink, Link,
    LocalLimits, Manifest, Msg, PROOF_HELLO, PROTO_VERSION, Payload, PinLink, Provider, SecretKey,
    TaskKind, VerifyMode, VerifyPolicy,
};
use serde_json::json;

use super::run::{EMBEDDED_SKYCMD_ARENA, GOLDEN_MANDELBROT, GOLDEN_OPTDIGITS, GOLDEN_TOY_TRAINER};
use crate::{Ctx, config, exit, fail};

/// BLAKE3 of the golden mandelbrot module.
pub const GOLDEN_MODULE: &str = "7085684d08d0c83832195135a40ce2583e47bb4bdb4828c849ff4746f4648057";

/// Mandelbrot task parameters: (x0, y0, step, width, height, max_iter).
pub type MandelParams = (f64, f64, f64, u32, u32, u32);

/// Golden corpus: task parameters and the BLAKE3 of the output.
pub const GOLDEN: [(MandelParams, &str); 3] = [
    (
        (-2.0, -1.5, 3.0 / 256.0, 256, 256, 500),
        "4997e7fcca5edc96e1296d3f8819a9a5b7247a3bf3f953e4511f5b21b1080b59",
    ),
    (
        (
            -0.743_643_887_037_151,
            0.131_825_904_205_33,
            1e-7,
            64,
            64,
            2000,
        ),
        "34f54682a94f9f96256bf6bcf7718ed9fd6f23d4e4712fe31db36ca724152fa4",
    ),
    (
        (0.0, 0.0, 1.0, 1, 1, 100),
        "d9dba825ecc31a0edc64d04c8535c66a73bc3e74c4c48df84da3272949e38f01",
    ),
];

/// Arguments of `nuee selftest`.
#[derive(Debug, clap::Args)]
pub struct Args {}

/// Encodes a mandelbrot task input.
pub fn mandel_input(p: MandelParams) -> Vec<u8> {
    let mut v = Vec::with_capacity(36);
    v.extend_from_slice(&p.0.to_le_bytes());
    v.extend_from_slice(&p.1.to_le_bytes());
    v.extend_from_slice(&p.2.to_le_bytes());
    v.extend_from_slice(&p.3.to_le_bytes());
    v.extend_from_slice(&p.4.to_le_bytes());
    v.extend_from_slice(&p.5.to_le_bytes());
    v
}

fn limits() -> RunLimits {
    RunLimits {
        deadline: Duration::from_secs(60),
        max_memory: MAX_WASM_MEMORY,
        max_output: 8 << 20,
    }
}

/// Runs the golden corpus through wasmtime; `Err` names the first difference.
pub fn golden_check(runner: &WasmRunner) -> anyhow::Result<String> {
    let info = runner
        .prepare(GOLDEN_MANDELBROT)
        .map_err(|e| anyhow::anyhow!("golden module refused: {e}"))?;
    if info.hash.to_string() != GOLDEN_MODULE {
        anyhow::bail!("embedded golden module has hash {}", info.hash);
    }
    for (params, want) in GOLDEN {
        let out = runner
            .run(&info.hash, &mandel_input(params), &limits())
            .map_err(|e| anyhow::anyhow!("golden run failed: {e}"))?;
        let got = Hash::of(&out).to_string();
        if got != want {
            anyhow::bail!("golden output {got} differs from {want}");
        }
    }
    Ok(format!("{} golden outputs match", GOLDEN.len()))
}

/// Which demo modules this binary carries; each embedded module must validate and compile.
pub fn demo_modules_check(runner: &WasmRunner) -> anyhow::Result<String> {
    let mut have = Vec::new();
    let mut missing = Vec::new();
    let modules = [
        ("mandelbrot", Some(GOLDEN_MANDELBROT)),
        ("toy-trainer", Some(GOLDEN_TOY_TRAINER)),
        ("skycmd-arena", EMBEDDED_SKYCMD_ARENA),
    ];
    for (name, bytes) in modules {
        match bytes {
            Some(b) => {
                runner
                    .prepare(b)
                    .map_err(|e| anyhow::anyhow!("embedded {name} refused: {e}"))?;
                have.push(name);
            }
            None => missing.push(name),
        }
    }
    anyhow::ensure!(
        GOLDEN_OPTDIGITS.starts_with(b"OPTD"),
        "embedded optdigits dataset is malformed"
    );
    have.push("optdigits");
    let mut detail = format!("embedded: {}", have.join(", "));
    if !missing.is_empty() {
        detail.push_str(&format!(
            "; not embedded: {} (build with --features embed-tasks)",
            missing.join(", ")
        ));
    }
    Ok(detail)
}

fn links_check(key: &SecretKey) -> anyhow::Result<String> {
    let id = key.public();
    let job = Link::Job(JobLink {
        coordinator: id,
        relay: BoundedString::try_from("https://relay.example.org./")
            .map_err(|_| anyhow::anyhow!("relay"))?,
        secret: nuee_proto::new_secret16()?,
    });
    let mut providers = BoundedVec::new();
    providers
        .push(Provider { id, relay: None })
        .map_err(|_| anyhow::anyhow!("providers"))?;
    let file = FileLink {
        providers,
        hash: Hash::of(b"selftest"),
        size: 8,
        access: nuee_proto::new_secret16()?,
        key: config::fresh_key()?.to_bytes(),
    };
    for link in [job, Link::File(file.clone())] {
        let url = link.to_url(nuee_proto::link::DEFAULT_BASE);
        anyhow::ensure!(Link::parse(&url)? == link, "link does not round-trip");
        let cut = &url[..url.len() - 3];
        anyhow::ensure!(
            Link::parse(cut) == Err(nuee_proto::LinkError::Truncated),
            "a truncated link is not reported as such"
        );
    }
    let pin = file.pin_link();
    anyhow::ensure!(
        PinLink::parse(&pin.to_url(nuee_proto::link::DEFAULT_BASE))? == pin,
        "pin link does not round-trip"
    );
    Ok("job, file and pin links round-trip".into())
}

fn manifest(key: &SecretKey, module: Hash, size: u32, tasks: u32, mode: VerifyMode) -> Manifest {
    Manifest {
        version: PROTO_VERSION,
        job: 0,
        name: BoundedString::try_from("selftest").unwrap_or_default(),
        kind: TaskKind::Wasm,
        module: Some(module),
        module_size: size,
        max_memory: MAX_WASM_MEMORY,
        assets: BoundedVec::new(),
        tasks,
        verify: VerifyPolicy {
            mode,
            sample_permille: 0,
            canary_permille: 0,
        },
        compare: Comparator::Exact,
        deadline_ms: 60_000,
        exec: None,
        train: None,
        coordinator: key.public(),
    }
}

fn manifest_check(key: &SecretKey) -> anyhow::Result<String> {
    let m = manifest(key, Hash::of(b"m"), 1, 1, VerifyMode::Full);
    let signed = m.sign(key)?;
    anyhow::ensure!(
        signed.open(&key.public())? == m,
        "manifest does not round-trip"
    );
    let other = config::fresh_key()?;
    anyhow::ensure!(
        signed.open(&other.public()).is_err(),
        "a manifest opened under the wrong coordinator"
    );
    let mut tampered = signed.clone();
    let mut bytes = tampered.bytes.to_vec();
    if let Some(b) = bytes.last_mut() {
        *b ^= 1;
    }
    tampered.bytes = BoundedBytes::try_from(bytes).map_err(|_| anyhow::anyhow!("bytes"))?;
    anyhow::ensure!(
        tampered.open(&key.public()).is_err(),
        "a tampered manifest was accepted"
    );

    let worker = config::fresh_key()?.public();
    let secret = nuee_proto::new_secret16()?;
    let p = nuee_proto::proof(PROOF_HELLO, &secret, &worker, &key.public());
    anyhow::ensure!(
        nuee_proto::check_proof(PROOF_HELLO, &secret, &worker, &key.public(), &p),
        "admission proof rejected"
    );
    let wrong = nuee_proto::new_secret16()?;
    anyhow::ensure!(
        !nuee_proto::check_proof(PROOF_HELLO, &wrong, &worker, &key.public(), &p),
        "admission proof accepted with the wrong secret"
    );
    Ok("manifest signatures and admission proofs hold".into())
}

fn age_check() -> anyhow::Result<String> {
    let plain = b"nuee selftest: encrypted shares".repeat(100);
    let (cipher, key) = nuee_store::encrypt_bytes(&plain)?;
    anyhow::ensure!(
        nuee_store::decrypt_bytes(&cipher, &key)? == plain,
        "decryption differs"
    );
    let mut bad = cipher.clone();
    let mid = bad.len() / 2;
    if let Some(b) = bad.get_mut(mid) {
        *b ^= 0x40;
    }
    anyhow::ensure!(
        nuee_store::decrypt_bytes(&bad, &key).is_err(),
        "tampered ciphertext was accepted"
    );
    Ok("age encryption round-trips and detects tampering".into())
}

/// One coordinator and one worker exchange messages in memory and finish a small job.
fn swarm_check(runner: &WasmRunner) -> anyhow::Result<String> {
    let ckey = config::fresh_key()?;
    let wkey = config::fresh_key()?;
    let wid = wkey.public();
    let info = runner
        .prepare(GOLDEN_MANDELBROT)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let inputs: Vec<Vec<u8>> = (0..6u32)
        .map(|i| mandel_input((-2.0 + f64::from(i) * 0.4, -0.5, 0.25, 4, 4, 100)))
        .collect();
    let spec = JobSpec {
        manifest: manifest(
            &ckey,
            info.hash,
            u32::try_from(GOLDEN_MANDELBROT.len())?,
            u32::try_from(inputs.len())?,
            VerifyMode::None,
        ),
        inputs: inputs
            .iter()
            .map(|i| BoundedBytes::try_from(i.as_slice()).map(Payload::Inline))
            .collect::<Result<_, _>>()
            .map_err(|_| anyhow::anyhow!("input too large"))?,
        compare: None,
    };
    let caps = Caps {
        agent: BoundedString::try_from("selftest").unwrap_or_default(),
        browser: false,
        cpu_slots: 2,
        gpu: None,
        mem_mb: 1024,
        kinds: TaskKind::Wasm.bit(),
        cached: BoundedVec::new(),
        exec_allow: BoundedVec::new(),
        store_quota_mb: 0,
        relay: None,
        rtc: false,
    };
    let mut coord = Coordinator::new(
        CoordinatorConfig {
            local_slots: 0,
            seed: 7,
            ..CoordinatorConfig::default()
        },
        Box::new(ckey.clone()),
    );
    let mut worker = Worker::new(
        WorkerConfig {
            caps: caps.clone(),
            limits: LocalLimits::default(),
        },
        ckey.public(),
        Box::new(wkey),
    );
    let job = coord.submit(0, spec)?;
    let mut now = 1u64;
    let mut inbox: VecDeque<Msg> = VecDeque::new();
    let mut accepted: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    let mut done = false;

    let mut apply = |actions: Vec<Action>, inbox: &mut VecDeque<Msg>, done: &mut bool| {
        for a in actions {
            match a {
                Action::Send { to, msg } if to == wid => inbox.push_back(msg),
                Action::Accepted {
                    job: j,
                    task,
                    output,
                    ..
                } if j == job => {
                    accepted.insert(task, output.to_vec());
                }
                Action::Event(Event::JobDone { job: j, .. }) if j == job => *done = true,
                _ => {}
            }
        }
    };
    let first = coord.drain();
    apply(first, &mut inbox, &mut done);
    let welcome = coord.admitted(now, wid, caps);
    apply(welcome, &mut inbox, &mut done);

    let mut steps = 0;
    while !done {
        steps += 1;
        anyhow::ensure!(steps < 10_000, "the simulated swarm did not finish");
        let Some(msg) = inbox.pop_front() else {
            now += 300;
            let a = coord.tick(now);
            apply(a, &mut inbox, &mut done);
            let wa = worker.tick(now);
            let mut back = VecDeque::from(wa);
            while let Some(wa) = back.pop_front() {
                if let WorkerAction::Send(m) = wa {
                    let a = coord.handle(now, wid, m);
                    apply(a, &mut inbox, &mut done);
                }
            }
            continue;
        };
        now += 1;
        let mut pending: VecDeque<WorkerAction> = worker.handle(now, msg).into();
        while let Some(wa) = pending.pop_front() {
            match wa {
                WorkerAction::Send(m) => {
                    let a = coord.handle(now, wid, m);
                    apply(a, &mut inbox, &mut done);
                }
                WorkerAction::Run { lease, .. } => {
                    let out = match &lease.input {
                        Payload::Inline(b) => match runner.run(&info.hash, b, &limits()) {
                            Ok(o) => Outcome::Ok(o),
                            Err(e) => Outcome::Fail(e.kind, e.detail),
                        },
                        Payload::Blob { .. } => anyhow::bail!("unexpected blob input"),
                    };
                    pending.extend(worker.task_done(now, lease.lease, out));
                }
                WorkerAction::Ended(reason) => anyhow::bail!("worker ended: {reason:?}"),
                _ => {}
            }
        }
    }
    anyhow::ensure!(
        accepted.len() == inputs.len(),
        "{} of {} tasks accepted",
        accepted.len(),
        inputs.len()
    );
    for (task, out) in &accepted {
        let input = inputs
            .get(*task as usize)
            .ok_or_else(|| anyhow::anyhow!("unknown task {task}"))?;
        let want = runner
            .run(&info.hash, input, &limits())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        anyhow::ensure!(*out == want, "task {task} output differs");
    }
    Ok(format!("in-memory swarm finished {} tasks", accepted.len()))
}

/// Runs every check; exit 0 when all pass.
pub async fn run(_args: Args, ctx: &Ctx) -> anyhow::Result<ExitCode> {
    let results = tokio::task::spawn_blocking(|| {
        let runner = WasmRunner::new(RunnerConfig::default())
            .map_err(|e| anyhow::anyhow!("WASM engine: {e}"));
        let key = config::fresh_key();
        let mut out: Vec<(&str, anyhow::Result<String>)> = Vec::new();
        match (&runner, &key) {
            (Ok(r), Ok(k)) => {
                out.push(("wasm golden", golden_check(r)));
                out.push(("demo modules", demo_modules_check(r)));
                out.push(("links", links_check(k)));
                out.push(("manifest", manifest_check(k)));
                out.push(("encryption", age_check()));
                out.push(("swarm", swarm_check(r)));
            }
            (Err(e), _) => out.push(("wasm engine", Err(anyhow::anyhow!("{e:#}")))),
            (_, Err(e)) => out.push(("random", Err(anyhow::anyhow!("{e:#}")))),
        }
        out
    })
    .await?;
    let mut ok = true;
    for (name, res) in results {
        match res {
            Ok(detail) => ctx.ui.check(name, true, &detail, None),
            Err(e) => {
                ok = false;
                ctx.ui.check(name, false, &format!("{e:#}"), None);
            }
        }
    }
    if !ok {
        return Err(fail(exit::ERROR, "selftest failed"));
    }
    ctx.ui.emit("selftest", json!({ "ok": true }));
    ctx.ui.say("selftest ok");
    Ok(ExitCode::SUCCESS)
}
