//! Toy DiLoCo trainer: a 64 → 32 (ReLU) → 10 (softmax) MLP on the UCI optdigits dataset.
//!
//! Follows nuee-train's WASM trainer contract. The job has exactly one asset, the dataset, so
//! the input is always an asset envelope; the lease input after it selects the mode by its magic
//! (all integers little-endian):
//!
//! - `b"NTR1" | u32 steps | u32 shard | u32 n_shards | u64 seed | f32 lr | params (safetensors)`:
//!   `steps` minibatch SGD steps (batch 32, cross-entropy, `lr = 0` ⇒ 0.1) on the training
//!   samples `i` with `i % n_shards == shard`, visited in an order shuffled by splitmix64 seeded
//!   by `(seed, shard)` and reshuffled at every pass. Output: the delta `θ_local − θ_input`
//!   encoded as `NDL1` with the `F32` codec, then the mean minibatch loss as an `f32`.
//! - `b"NEV1" | params (safetensors)`: evaluation on the test samples. Output: `f32 accuracy`,
//!   `f32 mean loss`, then one `u8` predicted class per test sample.
//! - `b"NIN1" | u64 seed`: initial parameters (He-normal weights, zero biases) as safetensors.
//!
//! Parameters are the F32 tensors `b1 [32]`, `b2 [10]`, `w1 [32, 64]`, `w2 [10, 32]` (2,410
//! values; weights are `[out, in]`). Deltas list them in that (name) order.
//!
//! Dataset asset: `b"OPTD" | u32 n_train | u32 n_test`, then `n_train + n_test` samples of 65
//! bytes each (64 pixel counts in 0..=16, then the label 0..=9), training samples first. It is
//! built from the original `optdigits.tra` / `optdigits.tes` by `scripts/build-tasks.sh`.
//!
//! Every computation is plain sequential f32 arithmetic, so the output bytes are the same on
//! every engine. Malformed input panics, which traps the instance.

const IN: usize = 64;
const HID: usize = 32;
const OUT: usize = 10;
const BATCH: usize = 32;
const DEFAULT_LR: f32 = 0.1;
const SAMPLE: usize = IN + 1;
/// Largest dataset accepted (samples).
const MAX_SAMPLES: usize = 1 << 16;
/// Largest number of local steps per lease.
const MAX_STEPS: u32 = 100_000;
/// Largest safetensors header read.
const MAX_HEADER: usize = 64 * 1024;

/// Tensor names in layout (sorted) order, with their sizes.
const TENSORS: [(&str, &[usize]); 4] = [
    ("b1", &[HID]),
    ("b2", &[OUT]),
    ("w1", &[HID, IN]),
    ("w2", &[OUT, HID]),
];
const OFF_B1: usize = 0;
const OFF_B2: usize = OFF_B1 + HID;
const OFF_W1: usize = OFF_B2 + OUT;
const OFF_W2: usize = OFF_W1 + HID * IN;
const N_PARAMS: usize = OFF_W2 + OUT * HID;

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

struct Dataset<'a> {
    train: &'a [u8],
    test: &'a [u8],
}

impl<'a> Dataset<'a> {
    fn parse(asset: &'a [u8]) -> Dataset<'a> {
        assert!(
            asset.len() >= 12 && &asset[..4] == b"OPTD",
            "not an OPTD dataset"
        );
        let n_train = u32_at(asset, 4) as usize;
        let n_test = u32_at(asset, 8) as usize;
        assert!(
            n_train > 0 && n_train <= MAX_SAMPLES && n_test <= MAX_SAMPLES,
            "dataset size out of range"
        );
        let body = &asset[12..];
        assert_eq!(body.len(), (n_train + n_test) * SAMPLE, "dataset length");
        assert!(
            body.as_chunks::<SAMPLE>()
                .0
                .iter()
                .all(|s| s[..IN].iter().all(|p| *p <= 16) && s[IN] < OUT as u8),
            "dataset value out of range"
        );
        let (train, test) = body.split_at(n_train * SAMPLE);
        Dataset { train, test }
    }
}

fn sample(set: &[u8], i: usize) -> ([f32; IN], usize) {
    let s = &set[i * SAMPLE..(i + 1) * SAMPLE];
    let mut x = [0.0f32; IN];
    for (xi, p) in x.iter_mut().zip(&s[..IN]) {
        *xi = f32::from(*p) / 16.0;
    }
    (x, usize::from(s[IN]))
}

struct Forward {
    h: [f32; HID],
    p: [f32; OUT],
    loss: f32,
}

fn forward(theta: &[f32], x: &[f32; IN], label: usize) -> Forward {
    let mut h = [0.0f32; HID];
    for (j, hj) in h.iter_mut().enumerate() {
        let row = &theta[OFF_W1 + j * IN..OFF_W1 + (j + 1) * IN];
        let mut a = theta[OFF_B1 + j];
        for (w, xi) in row.iter().zip(x) {
            a += w * xi;
        }
        *hj = a.max(0.0);
    }
    let mut z = [0.0f32; OUT];
    for (k, zk) in z.iter_mut().enumerate() {
        let row = &theta[OFF_W2 + k * HID..OFF_W2 + (k + 1) * HID];
        let mut a = theta[OFF_B2 + k];
        for (w, hj) in row.iter().zip(&h) {
            a += w * hj;
        }
        *zk = a;
    }
    let m = z.iter().fold(f32::NEG_INFINITY, |m, v| m.max(*v));
    let mut p = [0.0f32; OUT];
    let mut sum = 0.0f32;
    for (pk, zk) in p.iter_mut().zip(&z) {
        *pk = (zk - m).exp();
        sum += *pk;
    }
    for pk in p.iter_mut() {
        *pk /= sum;
    }
    let loss = -(p[label].max(1e-30)).ln();
    Forward { h, p, loss }
}

fn predict(p: &[f32; OUT]) -> usize {
    let mut best = 0;
    for k in 1..OUT {
        if p[k] > p[best] {
            best = k;
        }
    }
    best
}

/// Adds the gradient of one sample's loss, scaled by `scale`, to `grad`.
fn backward(theta: &[f32], x: &[f32; IN], label: usize, f: &Forward, scale: f32, grad: &mut [f32]) {
    let mut dz = f.p;
    dz[label] -= 1.0;
    let mut dh = [0.0f32; HID];
    for (k, dzk) in dz.iter().enumerate() {
        let d = dzk * scale;
        grad[OFF_B2 + k] += d;
        for j in 0..HID {
            grad[OFF_W2 + k * HID + j] += d * f.h[j];
            dh[j] += dzk * theta[OFF_W2 + k * HID + j];
        }
    }
    for j in 0..HID {
        if f.h[j] <= 0.0 {
            continue;
        }
        let d = dh[j] * scale;
        grad[OFF_B1 + j] += d;
        let row = &mut grad[OFF_W1 + j * IN..OFF_W1 + (j + 1) * IN];
        for (g, xi) in row.iter_mut().zip(x) {
            *g += d * xi;
        }
    }
}

// --- safetensors (only the F32 tensors of this model) ---

#[derive(Default)]
struct Entry {
    f32_dtype: bool,
    shape: Vec<usize>,
    offsets: Option<(usize, usize)>,
}

struct Json<'a> {
    s: &'a [u8],
    i: usize,
}

impl Json<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn peek(&mut self) -> u8 {
        self.ws();
        *self.s.get(self.i).expect("truncated header")
    }

    fn eat(&mut self, c: u8) {
        assert_eq!(self.peek(), c, "malformed header");
        self.i += 1;
    }

    fn string(&mut self) -> &[u8] {
        self.eat(b'"');
        let start = self.i;
        while *self.s.get(self.i).expect("unterminated string") != b'"' {
            assert_ne!(self.s[self.i], b'\\', "escapes are not supported");
            self.i += 1;
        }
        self.i += 1;
        &self.s[start..self.i - 1]
    }

    fn number(&mut self) -> usize {
        self.ws();
        let start = self.i;
        while self.i < self.s.len() && self.s[self.i].is_ascii_digit() {
            self.i += 1;
        }
        let digits = core::str::from_utf8(&self.s[start..self.i]).expect("digits");
        digits.parse().expect("integer")
    }

    fn numbers(&mut self) -> Vec<usize> {
        self.eat(b'[');
        let mut v = Vec::new();
        if self.peek() == b']' {
            self.i += 1;
            return v;
        }
        loop {
            assert!(v.len() < 8, "too many dimensions");
            v.push(self.number());
            if self.peek() == b',' {
                self.i += 1;
            } else {
                self.eat(b']');
                return v;
            }
        }
    }

    /// Skips any JSON value made of objects, arrays, strings and plain tokens.
    fn skip(&mut self) {
        match self.peek() {
            b'"' => {
                self.string();
            }
            b'{' | b'[' => {
                let close = if self.peek() == b'{' { b'}' } else { b']' };
                self.i += 1;
                if self.peek() == close {
                    self.i += 1;
                    return;
                }
                loop {
                    if close == b'}' {
                        self.string();
                        self.eat(b':');
                    }
                    self.skip();
                    if self.peek() == b',' {
                        self.i += 1;
                    } else {
                        self.eat(close);
                        return;
                    }
                }
            }
            _ => {
                while self.i < self.s.len() && !b",}] \t\r\n".contains(&self.s[self.i]) {
                    self.i += 1;
                }
            }
        }
    }

    fn entry(&mut self) -> Entry {
        let mut e = Entry::default();
        self.eat(b'{');
        loop {
            let key = self.string().to_vec();
            self.eat(b':');
            match key.as_slice() {
                b"dtype" => e.f32_dtype = self.string() == b"F32",
                b"shape" => e.shape = self.numbers(),
                b"data_offsets" => {
                    let o = self.numbers();
                    assert_eq!(o.len(), 2, "data_offsets");
                    e.offsets = Some((o[0], o[1]));
                }
                _ => self.skip(),
            }
            if self.peek() == b',' {
                self.i += 1;
            } else {
                self.eat(b'}');
                return e;
            }
        }
    }
}

fn read_params(bytes: &[u8]) -> Vec<f32> {
    assert!(bytes.len() >= 8, "params shorter than their header");
    let n = usize::try_from(u64_at(bytes, 0)).expect("header length");
    assert!(n <= MAX_HEADER && 8 + n <= bytes.len(), "header length");
    let data = &bytes[8 + n..];
    let mut json = Json {
        s: &bytes[8..8 + n],
        i: 0,
    };
    let mut found: [Option<Entry>; 4] = [None, None, None, None];
    json.eat(b'{');
    loop {
        let name = json.string().to_vec();
        json.eat(b':');
        match TENSORS.iter().position(|(t, _)| t.as_bytes() == name) {
            Some(k) => {
                assert!(found[k].is_none(), "duplicate tensor");
                found[k] = Some(json.entry());
            }
            None => json.skip(),
        }
        if json.peek() == b',' {
            json.i += 1;
        } else {
            json.eat(b'}');
            break;
        }
    }
    let mut theta = Vec::with_capacity(N_PARAMS);
    for ((_, shape), e) in TENSORS.iter().zip(found) {
        let e = e.expect("missing tensor");
        assert!(e.f32_dtype && e.shape == *shape, "tensor dtype or shape");
        let (a, b) = e.offsets.expect("missing data_offsets");
        let len: usize = shape.iter().product();
        assert!(
            a <= b && b <= data.len() && b - a == len * 4,
            "tensor offsets"
        );
        theta.extend(
            data[a..b]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| f32::from_le_bytes(*c)),
        );
    }
    assert!(theta.iter().all(|x| x.is_finite()), "non-finite parameter");
    theta
}

fn write_params(theta: &[f32]) -> Vec<u8> {
    let mut header = String::from("{");
    let mut off = 0;
    for (k, (name, shape)) in TENSORS.iter().enumerate() {
        let len: usize = shape.iter().product();
        let dims: Vec<String> = shape.iter().map(|d| d.to_string()).collect();
        if k > 0 {
            header.push(',');
        }
        header.push_str(&format!(
            "\"{name}\":{{\"dtype\":\"F32\",\"shape\":[{}],\"data_offsets\":[{},{}]}}",
            dims.join(","),
            off,
            off + len * 4
        ));
        off += len * 4;
    }
    header.push('}');
    while header.len() % 8 != 0 {
        header.push(' ');
    }
    let mut out = Vec::with_capacity(8 + header.len() + theta.len() * 4);
    out.extend_from_slice(&(header.len() as u64).to_le_bytes());
    out.extend_from_slice(header.as_bytes());
    for x in theta {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

// --- modes ---

/// Standard normal from two uniforms (Box–Muller), deterministic.
fn normal(state: &mut u64) -> f32 {
    let u1 = ((splitmix(state) >> 40) as f32 + 1.0) / 16_777_217.0;
    let u2 = (splitmix(state) >> 40) as f32 / 16_777_216.0;
    (-2.0 * u1.ln()).sqrt() * (core::f32::consts::TAU * u2).cos()
}

fn init(seed: u64) -> Vec<u8> {
    let mut state = seed ^ 0x6E75_6565_696E_6974;
    let mut theta = vec![0.0f32; N_PARAMS];
    let s1 = (2.0f32 / IN as f32).sqrt();
    for w in &mut theta[OFF_W1..OFF_W2] {
        *w = normal(&mut state) * s1;
    }
    let s2 = (2.0f32 / HID as f32).sqrt();
    for w in &mut theta[OFF_W2..] {
        *w = normal(&mut state) * s2;
    }
    write_params(&theta)
}

fn train(data: &Dataset, input: &[u8]) -> Vec<u8> {
    assert!(input.len() >= 28, "input shorter than its header");
    let steps = u32_at(input, 4);
    let shard = u32_at(input, 8) as usize;
    let n_shards = u32_at(input, 12) as usize;
    let seed = u64_at(input, 16);
    let lr = f32::from_bits(u32_at(input, 24));
    assert!(n_shards > 0 && shard < n_shards, "shard out of range");
    assert!(steps <= MAX_STEPS, "too many steps");
    assert!(lr.is_finite() && lr >= 0.0, "learning rate out of range");
    let lr = if lr == 0.0 { DEFAULT_LR } else { lr };
    let start = read_params(&input[28..]);

    let n_train = data.train.len() / SAMPLE;
    let mut order: Vec<usize> = (shard..n_train).step_by(n_shards).collect();
    assert!(!order.is_empty(), "empty shard");
    let mut rng = seed ^ (shard as u64).wrapping_mul(0xD1B5_4A32_D192_ED03);
    let shuffle = |order: &mut Vec<usize>, rng: &mut u64| {
        for i in (1..order.len()).rev() {
            let j = (splitmix(rng) % (i as u64 + 1)) as usize;
            order.swap(i, j);
        }
    };
    shuffle(&mut order, &mut rng);

    let mut theta = start.clone();
    let mut grad = vec![0.0f32; N_PARAMS];
    let mut pos = 0;
    let mut loss_sum = 0.0f32;
    for _ in 0..steps {
        grad.fill(0.0);
        let mut batch_loss = 0.0f32;
        let scale = 1.0 / BATCH as f32;
        for _ in 0..BATCH {
            if pos == order.len() {
                shuffle(&mut order, &mut rng);
                pos = 0;
            }
            let (x, label) = sample(data.train, order[pos]);
            pos += 1;
            let f = forward(&theta, &x, label);
            batch_loss += f.loss;
            backward(&theta, &x, label, &f, scale, &mut grad);
        }
        for (t, g) in theta.iter_mut().zip(&grad) {
            *t -= lr * g;
        }
        loss_sum += batch_loss * scale;
    }
    let mean_loss = if steps == 0 {
        0.0
    } else {
        loss_sum / steps as f32
    };

    let mut out = Vec::with_capacity(9 + 8 * TENSORS.len() + N_PARAMS * 4 + 4);
    out.extend_from_slice(b"NDL1");
    out.push(0);
    out.extend_from_slice(&(TENSORS.len() as u32).to_le_bytes());
    let mut off = 0;
    for (_, shape) in TENSORS {
        let len: usize = shape.iter().product();
        out.extend_from_slice(&(len as u32).to_le_bytes());
        out.extend_from_slice(&1.0f32.to_le_bytes());
        for i in off..off + len {
            out.extend_from_slice(&(theta[i] - start[i]).to_le_bytes());
        }
        off += len;
    }
    out.extend_from_slice(&mean_loss.to_le_bytes());
    out
}

fn evaluate(data: &Dataset, params: &[u8]) -> Vec<u8> {
    let theta = read_params(params);
    let n = data.test.len() / SAMPLE;
    assert!(n > 0, "no test samples");
    let mut correct = 0u32;
    let mut loss = 0.0f32;
    let mut preds = Vec::with_capacity(n);
    for i in 0..n {
        let (x, label) = sample(data.test, i);
        let f = forward(&theta, &x, label);
        let k = predict(&f.p);
        if k == label {
            correct += 1;
        }
        loss += f.loss;
        preds.push(k as u8);
    }
    let mut out = Vec::with_capacity(8 + n);
    out.extend_from_slice(&(correct as f32 / n as f32).to_le_bytes());
    out.extend_from_slice(&(loss / n as f32).to_le_bytes());
    out.extend_from_slice(&preds);
    out
}

fn run(input: &[u8]) -> Vec<u8> {
    let (assets, lease) = nuee_task::split_assets(input).expect("asset envelope");
    assert_eq!(assets.len(), 1, "exactly one asset: the dataset");
    let data = Dataset::parse(assets[0]);
    assert!(lease.len() >= 4, "missing mode");
    match &lease[..4] {
        b"NTR1" => train(&data, lease),
        b"NEV1" => evaluate(&data, &lease[4..]),
        b"NIN1" => {
            assert_eq!(lease.len(), 12, "NIN1 takes a u64 seed");
            init(u64_at(lease, 4))
        }
        _ => panic!("unknown mode"),
    }
}

nuee_task::export!(run);
