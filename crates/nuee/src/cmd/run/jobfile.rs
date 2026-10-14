//! The pure parts of job files: JSONL shards, built-in input generators, JSONL output merges,
//! the JSONL tolerance comparator and sweep planning. Nothing here touches the disk or the
//! network, so `tests/jobs.rs` includes this file as is.

use std::collections::BTreeMap;

use serde_json::Value;

/// Field that numbers JSONL records (`teacher-score.py` sets it to the line index).
pub const UID: &str = "uid";

// ---------------------------------------------------------------------------------------------
// Shards.

/// Consecutive lines of a JSONL file, handed to one task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shard {
    /// Index of its first line in the whole file.
    pub first_line: u64,
    /// The lines as they are in the file, newline terminators included.
    pub bytes: Vec<u8>,
}

/// Splits `text` into shards of `lines_per_task` lines; the last one may be shorter. Lines are
/// kept byte for byte, empty ones included.
pub fn shard_lines(text: &[u8], lines_per_task: usize) -> Vec<Shard> {
    let per = lines_per_task.max(1);
    let lines: Vec<&[u8]> = text.split_inclusive(|b| *b == b'\n').collect();
    lines
        .chunks(per)
        .enumerate()
        .map(|(i, chunk)| Shard {
            first_line: (i * per) as u64,
            bytes: chunk.concat(),
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Generators.

fn whole(params: &BTreeMap<String, f64>, key: &str, default: u32) -> Result<u32, String> {
    match params.get(key) {
        None => Ok(default),
        Some(v) if v.fract() == 0.0 && *v >= 1.0 && *v <= f64::from(u32::MAX) => {
            // Integral and in range by the guard.
            Ok(*v as u32)
        }
        Some(v) => Err(format!(
            "`inputs.params.{key}` must be a positive integer, got {v}"
        )),
    }
}

fn real(params: &BTreeMap<String, f64>, key: &str, default: f64) -> Result<f64, String> {
    match params.get(key) {
        None => Ok(default),
        Some(v) if v.is_finite() => Ok(*v),
        Some(v) => Err(format!("`inputs.params.{key}` must be finite, got {v}")),
    }
}

/// Inputs of a built-in generator.
///
/// - `mandelbrot`: tiles of an image for the mandelbrot module (`x0, y0, step` f64 then
///   `w, h, max_iter` u32, little endian), row by row. Parameters `width` (1024), `height`
///   (1024), `tile` (64), `max_iter` (500), `x0` (-2.0), `y0` (-1.25), `x1` (0.5); `count`
///   must equal the number of tiles.
/// - `index`: task `i` gets `i` as a u64, little endian. No parameter.
pub fn generate(
    name: &str,
    count: u32,
    params: &BTreeMap<String, f64>,
) -> Result<Vec<Vec<u8>>, String> {
    let known: &[&str] = match name {
        "mandelbrot" => &["width", "height", "tile", "max_iter", "x0", "y0", "x1"],
        "index" => &[],
        other => {
            return Err(format!(
                "unknown generator `{other}` (available: mandelbrot, index)"
            ));
        }
    };
    if let Some(k) = params.keys().find(|k| !known.contains(&k.as_str())) {
        return Err(format!("generator `{name}` has no parameter `{k}`"));
    }
    if name == "index" {
        return Ok((0..u64::from(count))
            .map(|i| i.to_le_bytes().to_vec())
            .collect());
    }
    let width = whole(params, "width", 1024)?;
    let height = whole(params, "height", 1024)?;
    let tile = whole(params, "tile", 64)?;
    let max_iter = whole(params, "max_iter", 500)?;
    let x0 = real(params, "x0", -2.0)?;
    let y0 = real(params, "y0", -1.25)?;
    let x1 = real(params, "x1", 0.5)?;
    if x1 <= x0 {
        return Err("`inputs.params.x1` must be above `x0`".into());
    }
    let step = (x1 - x0) / f64::from(width);
    let (cols, rows) = (width.div_ceil(tile), height.div_ceil(tile));
    let tiles = u64::from(cols) * u64::from(rows);
    if u64::from(count) != tiles {
        return Err(format!(
            "`inputs.count` must be {tiles} for a {width}x{height} image in {tile}-pixel tiles"
        ));
    }
    let mut out = Vec::with_capacity(usize::try_from(tiles).unwrap_or(0));
    for ty in 0..rows {
        for tx in 0..cols {
            let (px, py) = (tx * tile, ty * tile);
            let w = tile.min(width - px);
            let h = tile.min(height - py);
            let mut v = Vec::with_capacity(36);
            v.extend_from_slice(&(x0 + f64::from(px) * step).to_le_bytes());
            v.extend_from_slice(&(y0 + f64::from(py) * step).to_le_bytes());
            v.extend_from_slice(&step.to_le_bytes());
            v.extend_from_slice(&w.to_le_bytes());
            v.extend_from_slice(&h.to_le_bytes());
            v.extend_from_slice(&max_iter.to_le_bytes());
            out.push(v);
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// JSONL merge.

fn skip_ws(b: &[u8], mut i: usize) -> usize {
    while b
        .get(i)
        .is_some_and(|c| matches!(c, b' ' | b'\t' | b'\r' | b'\n'))
    {
        i += 1;
    }
    i
}

/// Index just past the string starting at `i` (which holds the opening quote).
fn skip_string(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    loop {
        match b.get(j)? {
            b'\\' => j += 2,
            b'"' => return Some(j + 1),
            _ => j += 1,
        }
    }
}

/// Index just past the JSON value starting at `i`.
fn skip_value(b: &[u8], i: usize) -> Option<usize> {
    match b.get(i)? {
        b'"' => skip_string(b, i),
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut j = i;
            loop {
                match b.get(j)? {
                    b'"' => {
                        j = skip_string(b, j)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth = depth.checked_sub(1)?;
                        if depth == 0 {
                            return Some(j + 1);
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
        }
        _ => {
            let mut j = i;
            while b
                .get(j)
                .is_some_and(|c| !matches!(c, b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n'))
            {
                j += 1;
            }
            (j > i).then_some(j)
        }
    }
}

/// Byte range of the value of top-level key `key` in the JSON object `line`.
fn value_span(line: &str, key: &str) -> Option<(usize, usize)> {
    let b = line.as_bytes();
    let mut i = skip_ws(b, 0);
    if b.get(i) != Some(&b'{') {
        return None;
    }
    i += 1;
    loop {
        i = skip_ws(b, i);
        if b.get(i)? != &b'"' {
            return None;
        }
        let key_end = skip_string(b, i)?;
        let k = line.get(i + 1..key_end - 1)?;
        i = skip_ws(b, key_end);
        if b.get(i)? != &b':' {
            return None;
        }
        i = skip_ws(b, i + 1);
        let value_end = skip_value(b, i)?;
        if k == key {
            return Some((i, value_end));
        }
        i = skip_ws(b, value_end);
        if b.get(i)? != &b',' {
            return None;
        }
        i += 1;
    }
}

/// Concatenates JSONL task outputs in task order. Each output comes with the index of its
/// shard's first input line; with `uid_field`, every record's numeric uid (its line index in
/// the shard) is shifted by that index, so the result carries the uids of an unsharded run,
/// and records are ordered by uid. The rest of each line is kept byte for byte.
pub fn merge_jsonl(outputs: &[(u64, &[u8])], uid_field: Option<&str>) -> Result<Vec<u8>, String> {
    let mut records: Vec<(u64, String)> = Vec::new();
    for (task, (offset, bytes)) in outputs.iter().enumerate() {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| format!("the output of task {task} is not UTF-8"))?;
        for (n, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            if let Err(e) = serde_json::from_str::<Value>(line) {
                return Err(format!("task {task}, line {}: not JSON ({e})", n + 1));
            }
            let Some(field) = uid_field else {
                records.push((0, line.to_owned()));
                continue;
            };
            let (start, end) = value_span(line, field)
                .ok_or_else(|| format!("task {task}, line {}: no `{field}` field", n + 1))?;
            let raw = line.get(start..end).unwrap_or_default();
            let uid: u64 = raw.parse().map_err(|_| {
                format!(
                    "task {task}, line {}: `{field}` is not a whole number",
                    n + 1
                )
            })?;
            let global = uid
                .checked_add(*offset)
                .ok_or_else(|| format!("task {task}, line {}: `{field}` overflows", n + 1))?;
            let mut shifted = String::with_capacity(line.len() + 8);
            shifted.push_str(line.get(..start).unwrap_or_default());
            shifted.push_str(&global.to_string());
            shifted.push_str(line.get(end..).unwrap_or_default());
            records.push((global, shifted));
        }
    }
    if let Some(field) = uid_field {
        records.sort_by_key(|(uid, _)| *uid);
        if let Some(w) = records.windows(2).find(|w| w[0].0 == w[1].0) {
            return Err(format!("duplicate `{field}` {} in the outputs", w[0].0));
        }
    }
    let mut out = Vec::new();
    for (_, line) in records {
        out.extend_from_slice(line.as_bytes());
        out.push(b'\n');
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// JSONL comparator.

/// Two JSONL outputs agree when their records pair up (by `uid` when every record has one,
/// else by position) and, inside each pair, the numbers under `field` are within `tol` of each
/// other with the same argmax, every other field being equal.
#[derive(Debug, Clone)]
pub struct JsonlCompare {
    /// Field holding numbers (such as `tlogits`).
    pub field: String,
    /// Largest absolute difference allowed on each number.
    pub tol: f32,
}

fn records(bytes: &[u8]) -> Option<Vec<Value>> {
    std::str::from_utf8(bytes)
        .ok()?
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn argmax(xs: &[f64]) -> Option<usize> {
    let mut best: Option<(usize, f64)> = None;
    for (i, x) in xs.iter().enumerate() {
        if best.is_none_or(|(_, b)| *x > b) {
            best = Some((i, *x));
        }
    }
    best.map(|(i, _)| i)
}

impl JsonlCompare {
    fn close(&self, a: f64, b: f64) -> bool {
        (a - b).abs() <= f64::from(self.tol)
    }

    fn field_same(&self, a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Array(xs), Value::Array(ys)) => {
                let xs: Option<Vec<f64>> = xs.iter().map(Value::as_f64).collect();
                let ys: Option<Vec<f64>> = ys.iter().map(Value::as_f64).collect();
                match (xs, ys) {
                    (Some(xs), Some(ys)) => {
                        xs.len() == ys.len()
                            && xs.iter().zip(&ys).all(|(x, y)| self.close(*x, *y))
                            && argmax(&xs) == argmax(&ys)
                    }
                    _ => a == b,
                }
            }
            (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
                (Some(x), Some(y)) => self.close(x, y),
                _ => a == b,
            },
            _ => a == b,
        }
    }

    fn record_same(&self, a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Object(x), Value::Object(y)) => {
                x.len() == y.len()
                    && x.iter().all(|(k, va)| {
                        y.get(k).is_some_and(|vb| {
                            if *k == self.field {
                                self.field_same(va, vb)
                            } else {
                                va == vb
                            }
                        })
                    })
            }
            _ => a == b,
        }
    }
}

impl nuee_core::Compare for JsonlCompare {
    fn same(&self, a: &[u8], b: &[u8]) -> bool {
        if a == b {
            return true;
        }
        let (Some(mut x), Some(mut y)) = (records(a), records(b)) else {
            return false;
        };
        if x.len() != y.len() {
            return false;
        }
        let uid = |v: &Value| v.get(UID).and_then(Value::as_u64);
        if x.iter().chain(&y).all(|v| uid(v).is_some()) {
            x.sort_by_key(|v| uid(v));
            y.sort_by_key(|v| uid(v));
        }
        x.iter().zip(&y).all(|(a, b)| self.record_same(a, b))
    }
}

// ---------------------------------------------------------------------------------------------
// Sweeps.

/// Command line and environment of one sweep entry: its `args` appended to the job's argv,
/// its `env` merged over the job's (the entry wins).
pub fn sweep_command(
    argv: &[String],
    env: &BTreeMap<String, String>,
    args: &[String],
    extra_env: &BTreeMap<String, String>,
) -> (Vec<String>, BTreeMap<String, String>) {
    let mut full = argv.to_vec();
    full.extend_from_slice(args);
    let mut merged = env.clone();
    merged.extend(extra_env.iter().map(|(k, v)| (k.clone(), v.clone())));
    (full, merged)
}

/// Metric names looked up in sweep outputs, in column order.
pub const METRICS: [&str; 10] = [
    "loss",
    "val_loss",
    "accuracy",
    "acc",
    "acc_f32",
    "acc_ternary",
    "score",
    "mean_score",
    "reward",
    "kill_rate",
];

fn pick(obj: &serde_json::Map<String, Value>, out: &mut Vec<(String, f64)>) {
    for m in METRICS {
        if out.iter().any(|(k, _)| k == m) {
            continue;
        }
        if let Some(v) = obj.get(m).and_then(Value::as_f64) {
            out.push((m.to_owned(), v));
        }
    }
}

/// Known numeric metrics of an output: the output itself when it is a JSON object, else its
/// last non-empty line; a nested `metrics` object is searched too. Ordered as [`METRICS`].
pub fn metrics(output: &[u8]) -> Vec<(String, f64)> {
    let Ok(text) = std::str::from_utf8(output) else {
        return Vec::new();
    };
    let doc = serde_json::from_str::<Value>(text).ok().or_else(|| {
        text.lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .and_then(|l| serde_json::from_str(l).ok())
    });
    let Some(Value::Object(obj)) = doc else {
        return Vec::new();
    };
    let mut out = Vec::new();
    pick(&obj, &mut out);
    if let Some(Value::Object(inner)) = obj.get("metrics") {
        pick(inner, &mut out);
    }
    out.sort_by_key(|(k, _)| METRICS.iter().position(|m| m == k));
    out
}

/// Display order of sweep rows: by the first metric (in [`METRICS`] order) that any row has,
/// ascending for losses and descending otherwise; rows without it keep their order at the end.
pub fn rank(rows: &[Vec<(String, f64)>]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..rows.len()).collect();
    let Some(first) = METRICS
        .iter()
        .find(|m| rows.iter().flatten().any(|(k, _)| k == *m))
    else {
        return order;
    };
    let value = |i: usize| {
        rows.get(i)
            .and_then(|r| r.iter().find(|(k, _)| k == first))
            .map(|(_, v)| *v)
    };
    let lower_is_better = first.contains("loss");
    order.sort_by(|&a, &b| match (value(a), value(b)) {
        (Some(x), Some(y)) if lower_is_better => x.total_cmp(&y),
        (Some(x), Some(y)) => y.total_cmp(&x),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    order
}
