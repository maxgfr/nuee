use nuee_proto::{DeltaCodec, Hash, TrainSpec, TrainerKind};
use nuee_train::codec::with_loss;
use nuee_train::{
    AggConfig, ExecEncoder, Outer, OuterConfig, Params, Reject, TaskInput, TrainError, TrainState,
    aggregate, decode_delta, encode_delta, split_output,
};

fn params(values: &[f32]) -> Params {
    let n = values.len();
    Params::new(vec![
        ("b".into(), vec![1], vec![values[n - 1]]),
        ("w".into(), vec![n - 1], values[..n - 1].to_vec()),
    ])
    .expect("params")
}

fn spec(
    workers: u16,
    steps: u32,
    rounds: u32,
    trainer: TrainerKind,
    codec: DeltaCodec,
) -> TrainSpec {
    TrainSpec {
        rounds,
        inner_steps: steps,
        workers,
        outer_lr: 0.7,
        outer_momentum: 0.9,
        init: Hash::of(b"init"),
        trainer,
        codec,
        trim: 0.2,
        cos_min: 0.0,
        norm_max_ratio: 3.0,
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
    }
}

#[test]
fn outer_step_matches_hand_computed_nesterov() {
    let cfg = OuterConfig {
        lr: 0.5,
        momentum: 0.9,
    };
    let mut theta = vec![1.0f32, -2.0];
    let mut outer = Outer::new(2);
    // Round 1: delta (0.2, -0.4) => g = (-0.2, 0.4), v = g, change = 0.5*(0.9*g + g) = 0.95*g.
    let n1 = outer.step(cfg, &mut theta, &[0.2, -0.4]);
    assert!((theta[0] - (1.0 + 0.95 * 0.2)).abs() < 1e-6);
    assert!((theta[1] - (-2.0 - 0.95 * 0.4)).abs() < 1e-6);
    assert!((outer.velocity[0] + 0.2).abs() < 1e-6);
    assert!((n1 - 0.95 * (0.2f64.powi(2) + 0.4f64.powi(2)).sqrt()).abs() < 1e-6);
    // Round 2: delta (0.1, 0.0) => g = (-0.1, 0); v = (0.9*-0.2 - 0.1, 0.9*0.4) = (-0.28, 0.36).
    let (t0, t1) = (theta[0], theta[1]);
    outer.step(cfg, &mut theta, &[0.1, 0.0]);
    assert!((outer.velocity[0] + 0.28).abs() < 1e-6);
    assert!((outer.velocity[1] - 0.36).abs() < 1e-6);
    // change = 0.5*(0.9*v + g) = 0.5*(-0.352, 0.324) = (-0.176, 0.162).
    assert!((theta[0] - (t0 + 0.176)).abs() < 1e-6);
    assert!((theta[1] - (t1 - 0.162)).abs() < 1e-6);
}

#[test]
fn params_safetensors_roundtrip_and_dtype_check() {
    let p = params(&[1.0, 2.0, 3.0, 4.0]);
    assert_eq!(p.names, vec!["b", "w"]);
    let bytes = p.to_safetensors().expect("write");
    let back = Params::from_safetensors(&bytes).expect("read");
    assert_eq!(back, p);
    assert_eq!(back.layout_hash(), p.layout_hash());

    let raw = [1u8, 2, 3, 4];
    let view =
        safetensors::tensor::TensorView::new(safetensors::Dtype::U8, vec![4], &raw).expect("view");
    let u8_file = safetensors::serialize([("x", view)], None).expect("ser");
    let err = Params::from_safetensors(&u8_file).expect_err("u8 rejected");
    assert!(matches!(err, TrainError::Dtype { .. }), "{err}");
    assert!(Params::from_safetensors(b"garbage").is_err());
}

#[test]
fn int8_roundtrip_error_is_at_most_half_a_step() {
    let p = params(&[0.0; 65]);
    let layout = p.layout();
    let mut rng = Rng(7);
    let delta: Vec<f32> = (0..65).map(|_| rng.unit() * 3.0).collect();
    let mut residual = Vec::new();
    let enc = encode_delta(DeltaCodec::Int8Ef, &layout, &delta, &mut residual).expect("enc");
    let dec = decode_delta(&enc, &layout).expect("dec");
    // Tensor "b" (1 value) is exact; tensor "w" (64 values) is within scale/2.
    let w = &delta[1..];
    let scale = w.iter().fold(0.0f32, |m, x| m.max(x.abs())) / 127.0;
    assert!((dec[0] - delta[0]).abs() <= 1e-6);
    for (a, b) in dec[1..].iter().zip(w) {
        assert!((a - b).abs() <= scale / 2.0 + 1e-6, "{a} vs {b}");
    }
    for (i, r) in residual.iter().enumerate() {
        assert!((delta[i] - dec[i] - r).abs() < 1e-5);
    }
    let raw = encode_delta(DeltaCodec::F32, &layout, &delta, &mut Vec::new()).expect("f32");
    assert_eq!(decode_delta(&raw, &layout).expect("dec"), delta);
    // Zero delta: scale 0, all zeros.
    let zero = encode_delta(DeltaCodec::Int8Ef, &layout, &[0.0; 65], &mut Vec::new()).expect("z");
    assert!(
        decode_delta(&zero, &layout)
            .expect("dec")
            .iter()
            .all(|x| *x == 0.0)
    );
}

#[test]
fn error_feedback_makes_the_transmitted_sum_converge() {
    let p = params(&[0.0; 33]);
    let layout = p.layout();
    let mut rng = Rng(11);
    let mut residual = Vec::new();
    let mut true_sum = vec![0.0f64; 33];
    let mut sent = vec![0.0f64; 33];
    let mut sent_no_ef = vec![0.0f64; 33];
    for round in 0..200 {
        // One large coordinate dominates the scale so the small ones are lost without feedback.
        let delta: Vec<f32> = (0..33)
            .map(|i| {
                if i == 1 {
                    10.0
                } else {
                    0.02 + 0.01 * rng.unit()
                }
            })
            .collect();
        let enc = encode_delta(DeltaCodec::Int8Ef, &layout, &delta, &mut residual).expect("e");
        let dec = decode_delta(&enc, &layout).expect("d");
        let mut fresh = Vec::new();
        let enc2 = encode_delta(DeltaCodec::Int8Ef, &layout, &delta, &mut fresh).expect("e");
        let dec2 = decode_delta(&enc2, &layout).expect("d");
        for i in 0..33 {
            true_sum[i] += f64::from(delta[i]);
            sent[i] += f64::from(dec[i]);
            sent_no_ef[i] += f64::from(dec2[i]);
        }
        if round == 199 {
            for i in 0..33 {
                // The gap is exactly the residual, bounded by half a step.
                assert!((true_sum[i] - sent[i] - f64::from(residual[i])).abs() < 1e-2);
                assert!((true_sum[i] - sent[i]).abs() < 0.05, "coord {i}");
            }
            let gap_no_ef: f64 = (2..33).map(|i| (true_sum[i] - sent_no_ef[i]).abs()).sum();
            let gap_ef: f64 = (2..33).map(|i| (true_sum[i] - sent[i]).abs()).sum();
            assert!(gap_ef * 20.0 < gap_no_ef, "{gap_ef} vs {gap_no_ef}");
        }
    }
}

#[test]
fn decoder_rejects_malformed_inputs() {
    let p = params(&[0.5; 9]);
    let layout = p.layout();
    let delta = vec![0.25f32; 9];
    let good = encode_delta(DeltaCodec::Int8Ef, &layout, &delta, &mut Vec::new()).expect("e");
    assert!(decode_delta(&good, &layout).is_ok());
    for cut in 0..good.len() {
        assert!(decode_delta(&good[..cut], &layout).is_err(), "cut {cut}");
    }
    let mut long = good.clone();
    long.push(0);
    assert!(decode_delta(&long, &layout).is_err());
    let mut magic = good.clone();
    magic[0] = b'X';
    assert!(decode_delta(&magic, &layout).is_err());
    let mut codec = good.clone();
    codec[4] = 9;
    assert!(decode_delta(&codec, &layout).is_err());
    let mut count = good.clone();
    count[5..9].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(decode_delta(&count, &layout).is_err());
    let mut len = good.clone();
    len[9..13].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(decode_delta(&len, &layout).is_err());
    let mut scale = good.clone();
    scale[13..17].copy_from_slice(&f32::NAN.to_le_bytes());
    assert!(decode_delta(&scale, &layout).is_err());
    let other = params(&[0.5; 10]).layout();
    assert!(decode_delta(&good, &other).is_err());
    let f32_good = encode_delta(DeltaCodec::F32, &layout, &delta, &mut Vec::new()).expect("e");
    assert!(decode_delta(&f32_good, &other).is_err());
    // Random bytes never panic.
    let mut rng = Rng(3);
    for n in 0..400 {
        let mut buf: Vec<u8> = (0..n % 80).map(|_| rng.next() as u8).collect();
        if n % 2 == 0 && buf.len() > 9 {
            buf[..4].copy_from_slice(b"NDL1");
            buf[4] = (n % 3) as u8;
        }
        if let Ok(v) = decode_delta(&buf, &layout) {
            assert_eq!(v.len(), 9);
        }
    }
    assert!(split_output(&[1, 2, 3]).is_err());
}

#[test]
fn aggregation_drops_poisoned_deltas() {
    let mut rng = Rng(5);
    let dim = 50;
    let honest: Vec<Vec<f32>> = (0..5)
        .map(|_| {
            (0..dim)
                .map(|j| 1.0 + (j as f32) * 0.01 + 0.05 * rng.unit())
                .collect()
        })
        .collect();
    let honest_mean: Vec<f32> = (0..dim)
        .map(|j| honest.iter().map(|d| d[j]).sum::<f32>() / 5.0)
        .collect();
    let mut all = honest.clone();
    all.push(honest[0].iter().map(|x| x * 1000.0).collect()); // huge norm
    all.push(honest[1].iter().map(|x| -x).collect()); // flipped sign
    let mut nan = honest[2].clone();
    nan[3] = f32::NAN;
    all.push(nan);
    let agg = aggregate(&all, dim, AggConfig::default()).expect("agg");
    assert_eq!(agg.accepted, vec![0, 1, 2, 3, 4]);
    assert!(
        matches!(agg.rejected[0], (5, Reject::Norm(_))),
        "{:?}",
        agg.rejected
    );
    assert!(matches!(agg.rejected[1], (6, Reject::Cosine(_))));
    assert!(matches!(agg.rejected[2], (7, Reject::NonFinite)));
    for (a, b) in agg.delta.iter().zip(&honest_mean) {
        assert!((a - b).abs() < 0.05, "{a} vs {b}");
    }
    // Two deltas: plain mean of the finite ones.
    let two = aggregate(&[vec![1.0, 2.0], vec![3.0, 4.0]], 2, AggConfig::default()).expect("2");
    assert_eq!(two.delta, vec![2.0, 3.0]);
    let one = aggregate(
        &[vec![1.0, f32::INFINITY], vec![3.0, 4.0]],
        2,
        AggConfig::default(),
    )
    .expect("1");
    assert_eq!(one.delta, vec![3.0, 4.0]);
    assert_eq!(
        aggregate(&[vec![f32::NAN]], 1, AggConfig::default()),
        Err(TrainError::NoDelta)
    );
}

/// Linear regression `y = w·x + b + noise` split into shards, trained with local SGD.
struct Problem {
    xs: Vec<[f32; 4]>,
    ys: Vec<f32>,
}

impl Problem {
    fn new() -> Problem {
        let mut rng = Rng(42);
        let w = [1.5f32, -2.0, 0.5, 3.0];
        let mut xs = Vec::new();
        let mut ys = Vec::new();
        for _ in 0..512 {
            let x = [rng.unit(), rng.unit(), rng.unit(), rng.unit()];
            let y = w.iter().zip(&x).map(|(a, b)| a * b).sum::<f32>() + 0.7 + 0.3 * rng.unit();
            xs.push(x);
            ys.push(y);
        }
        Problem { xs, ys }
    }

    fn loss(&self, p: &[f32], idx: impl Iterator<Item = usize>) -> f32 {
        let mut s = 0.0;
        let mut n = 0.0;
        for i in idx {
            let pred = p[1..]
                .iter()
                .zip(&self.xs[i])
                .map(|(a, b)| a * b)
                .sum::<f32>()
                + p[0];
            s += (pred - self.ys[i]).powi(2);
            n += 1.0;
        }
        s / n
    }

    /// A trainer: decodes the lease input, runs SGD with batch 1, returns delta + loss.
    fn train(&self, input: &[u8], residual: &mut Vec<f32>, codec: DeltaCodec) -> Vec<u8> {
        let t = TaskInput::decode(input).expect("input");
        let start = Params::from_safetensors(t.params).expect("params");
        let mut p = start.data.clone();
        let shard: Vec<usize> = (0..self.xs.len())
            .filter(|i| *i as u32 % t.n_shards == t.shard)
            .collect();
        let mut rng = Rng(t.seed);
        let lr = if t.lr > 0.0 { t.lr } else { 0.01 };
        let mut loss_sum = 0.0;
        for _ in 0..t.steps {
            let i = shard[(rng.next() % shard.len() as u64) as usize];
            let pred = p[1..]
                .iter()
                .zip(&self.xs[i])
                .map(|(a, b)| a * b)
                .sum::<f32>()
                + p[0];
            let err = pred - self.ys[i];
            loss_sum += err * err;
            p[0] -= lr * 2.0 * err;
            for k in 0..4 {
                p[1 + k] -= lr * 2.0 * err * self.xs[i][k];
            }
        }
        let delta: Vec<f32> = p.iter().zip(&start.data).map(|(a, b)| a - b).collect();
        let enc = encode_delta(codec, &start.layout(), &delta, residual).expect("enc");
        with_loss(enc, loss_sum / t.steps as f32)
    }
}

fn simulate(workers: u16, rounds: u32, steps: u32, codec: DeltaCodec) -> Vec<f32> {
    let pb = Problem::new();
    let init = params(&[0.0; 5]);
    let mut st = TrainState::new(
        spec(workers, steps, rounds, TrainerKind::Wasm, codec),
        init,
        99,
    )
    .expect("state");
    let mut residuals = vec![Vec::new(); usize::from(workers)];
    let mut losses = vec![pb.loss(&st.params().data, 0..512)];
    while !st.is_done() {
        let r = st.round();
        for task in st.round_tasks(r) {
            let out = pb.train(
                &st.task_input(&task),
                &mut residuals[usize::from(task.worker_slot)],
                codec,
            );
            st.ingest(&task, &out).expect("ingest");
        }
        let report = st.finish_round().expect("round");
        assert_eq!(report.round, r);
        // Near the optimum honest deltas are mostly noise; a minority may fall outside the
        // norm bound, never the majority.
        assert!(
            report.accepted.len() * 2 > usize::from(workers),
            "{report:?}"
        );
        assert!(report.mean_loss.is_some());
        losses.push(pb.loss(&st.params().data, 0..512));
    }
    losses
}

#[test]
fn diloco_lowers_the_loss_and_more_workers_help() {
    let one = simulate(1, 40, 20, DeltaCodec::Int8Ef);
    let four = simulate(4, 40, 20, DeltaCodec::Int8Ef);
    for w in four[..5].windows(2) {
        assert!(w[1] < w[0], "{four:?}");
    }
    assert!(four[40] < four[0] / 20.0, "{four:?}");
    assert!(one[40] < one[0], "{one:?}");
    assert!(
        four[40] < one[40],
        "4 workers {} vs 1 worker {}",
        four[40],
        one[40]
    );
    let raw = simulate(4, 40, 20, DeltaCodec::F32);
    assert!(raw[40] < raw[0] / 20.0, "{raw:?}");
}

#[test]
fn round_tasks_are_deterministic_and_cover_every_shard() {
    let st = TrainState::new(
        spec(4, 5, 3, TrainerKind::Wasm, DeltaCodec::F32),
        params(&[0.0; 3]),
        1,
    )
    .expect("state");
    let a = st.round_tasks(0);
    assert_eq!(a, st.round_tasks(0));
    let mut shards: Vec<u32> = a.iter().map(|t| t.shard).collect();
    shards.sort_unstable();
    assert_eq!(shards, vec![0, 1, 2, 3]);
    let seeds: std::collections::BTreeSet<u64> =
        a.iter().chain(&st.round_tasks(1)).map(|t| t.seed).collect();
    assert_eq!(seeds.len(), 8);
    let input = st.task_input(&a[2]);
    let t = TaskInput::decode(&input).expect("decode");
    assert_eq!(
        (t.steps, t.shard, t.n_shards, t.seed),
        (5, a[2].shard, 4, a[2].seed)
    );
    assert!(TaskInput::decode(&input[..10]).is_err());
}

#[test]
fn ingest_refuses_stale_duplicate_and_mismatched_results() {
    let mut st = TrainState::new(
        spec(3, 1, 3, TrainerKind::Wasm, DeltaCodec::F32),
        params(&[0.0; 3]),
        1,
    )
    .expect("state");
    let tasks = st.round_tasks(0);
    let layout = st.params().layout();
    let good = with_loss(
        encode_delta(DeltaCodec::F32, &layout, &[0.1; 3], &mut Vec::new()).expect("e"),
        1.0,
    );
    st.ingest(&tasks[0], &good).expect("first");
    assert!(st.ingest(&tasks[0], &good).is_err());
    let bad = with_loss(
        encode_delta(
            DeltaCodec::F32,
            &params(&[0.0; 4]).layout(),
            &[0.1; 4],
            &mut Vec::new(),
        )
        .expect("e"),
        1.0,
    );
    assert!(st.ingest(&tasks[1], &bad).is_err());
    assert_eq!(st.received(), 1);
    st.finish_round().expect("round");
    assert!(st.ingest(&tasks[2], &good).is_err(), "stale round");
    let empty = st.finish_round();
    assert_eq!(empty, Err(TrainError::NoDelta));
    assert_eq!(st.round(), 1);
}

#[test]
fn exec_trainer_outputs_are_new_params() {
    let init = params(&[1.0, 1.0, 1.0]);
    let mut st =
        TrainState::new(spec(1, 1, 1, TrainerKind::Exec, DeltaCodec::F32), init, 1).expect("state");
    let task = st.round_tasks(0).remove(0);
    let newp = params(&[2.0, 0.0, 1.0]);
    let meta = std::collections::HashMap::from([("nuee.loss".to_string(), "0.5".to_string())]);
    // The worker host encodes the trainer's safetensors against the params it was given.
    let given = TaskInput::decode(&st.task_input(&task))
        .expect("lease")
        .params
        .to_vec();
    let out = ExecEncoder::new()
        .encode(
            0,
            st.codec(),
            &given,
            &newp.to_safetensors_with(Some(meta)).expect("st"),
        )
        .expect("encode");
    st.ingest(&task, &out).expect("ingest");
    let rep = st.finish_round().expect("round");
    assert_eq!(rep.mean_loss, Some(0.5));
    // delta (1, -1, 0) => change = 0.7*(0.9+1)*delta = 1.33*delta.
    let d = &st.params().data;
    let expect = params(&[1.0 + 1.33, 1.0 - 1.33, 1.0]).data;
    for (a, b) in d.iter().zip(&expect) {
        assert!((a - b).abs() < 1e-5, "{d:?}");
    }
}

#[test]
fn checkpoint_restore_roundtrip() {
    let pb = Problem::new();
    let s = spec(3, 10, 5, TrainerKind::Wasm, DeltaCodec::Int8Ef);
    let mut st = TrainState::new(s.clone(), params(&[0.0; 5]), 77).expect("state");
    let mut res = vec![Vec::new(); 3];
    for _ in 0..2 {
        for task in st.round_tasks(st.round()) {
            let out = pb.train(
                &st.task_input(&task),
                &mut res[usize::from(task.worker_slot)],
                s.codec,
            );
            st.ingest(&task, &out).expect("ingest");
        }
        st.finish_round().expect("round");
    }
    let ck = st.checkpoint().expect("ck");
    // A checkpoint also loads as a plain model.
    assert_eq!(&Params::from_safetensors(&ck).expect("plain"), st.params());
    let mut back = TrainState::new(s.clone(), params(&[0.0; 5]), 1).expect("fresh");
    back.restore(&ck).expect("restore");
    assert_eq!(back.round(), 2);
    assert_eq!(back.seed(), 77);
    assert_eq!(back.params(), st.params());
    assert_eq!(back.velocity(), st.velocity());
    assert_eq!(back.round_tasks(2), st.round_tasks(2));
    let mut other = TrainState::new(s, params(&[0.0; 6]), 1).expect("other");
    assert!(other.restore(&ck).is_err());
    assert!(back.restore(b"nope").is_err());
}

/// The f32 parameters of one fake exec trainer step: θ_in + `step`, as safetensors.
fn exec_output(given: &Params, step: &[f32], loss: Option<&str>) -> Vec<u8> {
    let mut off = 0;
    let mut tensors = Vec::new();
    for (name, shape) in given.names.iter().zip(&given.shapes) {
        let len: usize = shape.iter().product();
        let v = given.data[off..off + len]
            .iter()
            .zip(&step[off..off + len])
            .map(|(a, b)| a + b)
            .collect();
        tensors.push((name.clone(), shape.clone(), v));
        off += len;
    }
    let meta =
        loss.map(|l| std::collections::HashMap::from([("nuee.loss".to_string(), l.to_string())]));
    Params::new(tensors)
        .expect("params")
        .to_safetensors_with(meta)
        .expect("st")
}

#[test]
fn exec_int8_sends_about_a_quarter_of_the_f32_bytes() {
    let n = 4096;
    let mut rng = Rng(5);
    let given = params(&(0..n).map(|_| rng.unit()).collect::<Vec<_>>());
    let step: Vec<f32> = (0..n).map(|_| rng.unit() * 0.01).collect();
    let out = exec_output(&given, &step, Some("1.25"));
    let gb = given.to_safetensors().expect("st");
    let f32_out = ExecEncoder::new()
        .encode(1, DeltaCodec::F32, &gb, &out)
        .expect("f32");
    let int8_out = ExecEncoder::new()
        .encode(1, DeltaCodec::Int8Ef, &gb, &out)
        .expect("int8");
    let ratio = int8_out.len() as f64 / f32_out.len() as f64;
    assert!(
        ratio < 0.26 && ratio > 0.24,
        "int8 {} vs f32 {}",
        int8_out.len(),
        f32_out.len()
    );

    // The coordinator side decodes it into the round, with the loss from the metadata.
    let mut st = TrainState::new(
        spec(1, 1, 1, TrainerKind::Exec, DeltaCodec::Int8Ef),
        given.clone(),
        1,
    )
    .expect("state");
    let task = st.round_tasks(0).remove(0);
    st.ingest(&task, &int8_out).expect("ingest");
    assert_eq!(st.finish_round().expect("round").mean_loss, Some(1.25));
    let (enc, loss) = split_output(&int8_out).expect("trailer");
    assert_eq!(loss, 1.25);
    let d = decode_delta(enc, &given.layout()).expect("decode");
    let max = step.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    for (a, b) in d.iter().zip(&step) {
        assert!((a - b).abs() <= max / 127.0, "{a} vs {b}");
    }
    // No loss metadata => NaN trailer => no loss reported.
    let no_loss = ExecEncoder::new()
        .encode(1, DeltaCodec::F32, &gb, &exec_output(&given, &step, None))
        .expect("encode");
    assert!(split_output(&no_loss).expect("trailer").1.is_nan());
}

#[test]
fn exec_error_feedback_carries_the_rounding_loss() {
    let n = 257;
    let mut rng = Rng(9);
    let given = params(&vec![0.0; n]);
    let gb = given.to_safetensors().expect("st");
    // One large entry per round makes the int8 grid coarse for the small ones.
    let rounds: Vec<Vec<f32>> = (0..2)
        .map(|_| {
            let mut v: Vec<f32> = (0..n).map(|_| rng.unit() * 0.003).collect();
            v[0] = 1.0;
            v
        })
        .collect();
    let truth: Vec<f32> = (0..n).map(|i| rounds[0][i] + rounds[1][i]).collect();
    let err = |sum: &[f32]| -> f64 {
        sum.iter()
            .zip(&truth)
            .map(|(a, b)| f64::from(a - b).powi(2))
            .sum::<f64>()
            .sqrt()
    };
    let run = |feedback: bool| -> Vec<f32> {
        let mut enc = ExecEncoder::new();
        let mut sum = vec![0.0f32; n];
        for (r, step) in rounds.iter().enumerate() {
            if !feedback {
                enc = ExecEncoder::new();
            }
            let out = enc
                .encode(3, DeltaCodec::Int8Ef, &gb, &exec_output(&given, step, None))
                .expect("encode");
            let (d, _) = split_output(&out).expect("trailer");
            let d = decode_delta(d, &given.layout()).expect("decode");
            assert_eq!(enc.residual().len(), n, "round {r}");
            for (s, x) in sum.iter_mut().zip(d) {
                *s += x;
            }
        }
        sum
    };
    let with_ef = err(&run(true));
    let without = err(&run(false));
    assert!(
        with_ef < without * 0.75,
        "with EF {with_ef}, without {without}"
    );

    // Another job or another layout starts from a zero residual.
    let mut enc = ExecEncoder::new();
    enc.encode(
        3,
        DeltaCodec::Int8Ef,
        &gb,
        &exec_output(&given, &rounds[0], None),
    )
    .expect("encode");
    assert!(enc.residual().iter().any(|x| *x != 0.0));
    let fresh = ExecEncoder::new()
        .encode(
            4,
            DeltaCodec::Int8Ef,
            &gb,
            &exec_output(&given, &rounds[1], None),
        )
        .expect("encode");
    let switched = enc
        .encode(
            4,
            DeltaCodec::Int8Ef,
            &gb,
            &exec_output(&given, &rounds[1], None),
        )
        .expect("encode");
    assert_eq!(switched, fresh);
}

#[test]
fn malformed_or_wrong_layout_exec_outputs_fail() {
    let given = params(&[1.0, 2.0, 3.0]);
    let gb = given.to_safetensors().expect("st");
    let mut enc = ExecEncoder::new();
    assert!(
        enc.encode(0, DeltaCodec::Int8Ef, &gb, b"not safetensors")
            .is_err()
    );
    assert!(enc.encode(0, DeltaCodec::Int8Ef, &gb, b"").is_err());
    let other = params(&[1.0, 2.0, 3.0, 4.0]).to_safetensors().expect("st");
    assert!(matches!(
        enc.encode(0, DeltaCodec::Int8Ef, &gb, &other),
        Err(TrainError::Layout(_))
    ));
    assert!(enc.residual().is_empty(), "a failure leaves no residual");
    assert!(enc.encode(0, DeltaCodec::Int8Ef, b"garbage", &gb).is_err());

    // The coordinator refuses raw safetensors, truncated or wrong-layout deltas.
    let mut st = TrainState::new(
        spec(1, 1, 1, TrainerKind::Exec, DeltaCodec::Int8Ef),
        given.clone(),
        1,
    )
    .expect("state");
    let task = st.round_tasks(0).remove(0);
    assert!(st.ingest(&task, &gb).is_err(), "raw safetensors");
    let good = ExecEncoder::new()
        .encode(
            0,
            DeltaCodec::Int8Ef,
            &gb,
            &exec_output(&given, &[0.1, 0.2, 0.3], None),
        )
        .expect("encode");
    assert!(
        st.ingest(&task, &good[..good.len() - 6]).is_err(),
        "truncated"
    );
    let small = params(&[0.0, 0.0]);
    let wrong = ExecEncoder::new()
        .encode(
            0,
            DeltaCodec::Int8Ef,
            &small.to_safetensors().expect("st"),
            &exec_output(&small, &[0.1, 0.2], None),
        )
        .expect("encode");
    assert!(st.ingest(&task, &wrong).is_err(), "wrong layout");
    assert_eq!(st.received(), 0);
    st.ingest(&task, &good).expect("the good one still goes in");
}
