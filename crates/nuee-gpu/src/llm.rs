//! LLM option scoring, the way skycmd's `scripts/teacher-score.py` labels decision records.
//!
//! Each record (`t`, `ins`, `opts`, optional `state`) becomes one non-thinking Qwen chat
//! prompt listing the state, the question and the lettered options. The model is never
//! sampled: only the next-token scores of the option letters are kept.
//!
//! Task input: JSONL, one decision record per line (at most [`MAX_RECORDS`] records,
//! [`MAX_INPUT`] bytes). The first line may instead be a header `{"llm":{"model":"…"}}` naming
//! the model the job asks for. Blank lines are skipped but still counted.
//!
//! Task output: JSONL `{"uid":N,"tlogits":[…]}` in input order, where `N` is the record's line
//! index in the input (header excluded) and `tlogits` holds one score per option, rounded to
//! three decimals like the teacher. A letter the backend did not report scores
//! [`MISSING_LOGIT`].
//!
//! Backends only agree up to a constant (log-probabilities are logits minus the log of the
//! partition), so [`LogitsCompare`] compares centred scores.

use serde::Deserialize;
use serde_json::Value;

use crate::GpuError;

/// Option letters, in order.
pub const LETTERS: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
/// Characters of the state kept in the prompt.
pub const MAX_STATE_CHARS: usize = 2400;
/// Records per task.
pub const MAX_RECORDS: usize = 512;
/// Bytes of task input.
pub const MAX_INPUT: usize = 1 << 20;
/// Score of a letter the backend did not report.
pub const MISSING_LOGIT: f32 = -1e4;
/// Default `max_gap` between two backends running the same model (llama.cpp against ONNX
/// Runtime in a browser, different quantisations).
pub const DEFAULT_MAX_GAP_CROSS_BACKEND: f32 = 1.5;
/// Default `max_gap` between two runs on the same backend and weights.
pub const DEFAULT_MAX_GAP_SAME_BACKEND: f32 = 0.25;
/// How far below its best letter a letter must score for the other side to leave it out.
pub const MISSING_MIN_DROP: f64 = 10.0;
/// Top log-probabilities asked from an endpoint.
pub const TOP_LOGPROBS: u32 = 20;
/// Bytes of one endpoint response.
pub const MAX_RESPONSE: usize = 1 << 20;

/// Hint appended to the question, by record type (`t`).
pub fn hint(t: &str) -> &'static str {
    match t {
        "choice" => "Choose the best option.",
        "score" => "Choose the level that fits best.",
        "noul" => "Decide whether the statement holds.",
        _ => "",
    }
}

/// One decision record (skycmd schema). Other fields are ignored.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Record {
    /// Question type: `choice`, `score` or `noul`.
    pub t: String,
    /// The question.
    pub ins: String,
    /// The options, lettered A, B, …
    pub opts: Vec<String>,
    /// Game state shown before the question.
    #[serde(default)]
    pub state: Option<String>,
}

/// A decoded task: the model asked for, and the records with their uids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LlmTask {
    /// Model named by the header line, if any.
    pub model: Option<String>,
    /// `(uid, record)` in input order.
    pub records: Vec<(u64, Record)>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    llm: HeaderBody,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HeaderBody {
    #[serde(default)]
    model: Option<String>,
}

/// The header line naming `model`, to put first in a task input.
pub fn header_line(model: &str) -> String {
    let mut s = serde_json::json!({ "llm": { "model": model } }).to_string();
    s.push('\n');
    s
}

/// Python's `str.isspace` (what `str.strip()` and Jinja's `trim` remove), which also counts
/// the ASCII separators U+001C..U+001F that Rust's `char::is_whitespace` leaves out.
fn py_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

fn py_strip(s: &str) -> &str {
    s.trim_matches(py_space)
}

/// The user message of a record, as `teacher-score.py` builds it.
pub fn user_content(r: &Record) -> String {
    let mut opts = String::new();
    for (i, (o, l)) in r.opts.iter().zip(LETTERS.chars()).enumerate() {
        if i > 0 {
            opts.push('\n');
        }
        opts.push(l);
        opts.push_str(". ");
        opts.push_str(o);
    }
    let state: String = py_strip(r.state.as_deref().unwrap_or(""))
        .chars()
        .take(MAX_STATE_CHARS)
        .collect();
    let mut parts = Vec::with_capacity(4);
    if !state.is_empty() {
        parts.push(format!("Context:\n{state}"));
    }
    parts.push(format!("Question: {}\n{}", py_strip(&r.ins), hint(&r.t)));
    parts.push(format!("Options:\n{opts}"));
    parts.push("Answer with the letter of the correct option only.".to_owned());
    parts.join("\n\n")
}

/// The full prompt: Qwen3.5's chat template applied to one user message with
/// `add_generation_prompt=True, enable_thinking=False`.
pub fn render_prompt(r: &Record) -> String {
    format!(
        "<|im_start|>user\n{}<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n",
        py_strip(&user_content(r))
    )
}

/// Decodes and checks a task input.
pub fn parse_task(input: &[u8]) -> Result<LlmTask, GpuError> {
    if input.len() > MAX_INPUT {
        return Err(GpuError::invalid("LLM input above 1 MiB"));
    }
    let text =
        std::str::from_utf8(input).map_err(|_| GpuError::invalid("LLM input is not UTF-8"))?;
    let mut lines = text.lines().peekable();
    let mut model = None;
    if let Some(first) = lines.peek()
        && let Ok(h) = serde_json::from_str::<Header>(first)
    {
        model = h.llm.model;
        lines.next();
    }
    let mut records = Vec::new();
    for (i, line) in lines.enumerate() {
        if py_strip(line).is_empty() {
            continue;
        }
        let r: Record = serde_json::from_str(line)
            .map_err(|e| GpuError::invalid(format!("record {i}: {e}")))?;
        if !(2..=LETTERS.len()).contains(&r.opts.len()) {
            return Err(GpuError::invalid(format!(
                "record {i}: {} options (2 to 26 allowed)",
                r.opts.len()
            )));
        }
        if records.len() == MAX_RECORDS {
            return Err(GpuError::invalid("more than 512 records in one task"));
        }
        records.push((i as u64, r));
    }
    Ok(LlmTask { model, records })
}

fn round3(v: f32) -> f64 {
    (f64::from(v) * 1000.0).round() / 1000.0
}

/// Encodes scored records as the task output.
pub fn encode_output(rows: &[(u64, Vec<f32>)]) -> Vec<u8> {
    let mut out = String::new();
    for (uid, logits) in rows {
        let vals: Vec<f64> = logits.iter().map(|v| round3(*v)).collect();
        let arr = Value::from(vals).to_string();
        out.push_str(&format!("{{\"uid\":{uid},\"tlogits\":{arr}}}\n"));
    }
    out.into_bytes()
}

/// Maps the top tokens of one position to option scores: a letter's score is the best of
/// its `"A"` and `" A"` tokens (any leading whitespace), [`MISSING_LOGIT`] when absent.
pub fn letter_scores(top: &[(String, f32)], n: usize) -> Vec<f32> {
    LETTERS
        .chars()
        .take(n)
        .map(|l| {
            top.iter()
                .filter(|(tok, _)| {
                    let t = tok.trim_start();
                    t.len() == 1 && t.starts_with(l)
                })
                .map(|(_, lp)| *lp)
                .fold(MISSING_LOGIT, f32::max)
        })
        .collect()
}

/// The top tokens of the first generated position in an OpenAI-compatible
/// `/v1/completions` response, or `None` when it carries no log-probabilities.
///
/// Two shapes are read: the OpenAI one (`logprobs.top_logprobs[0]` maps token text to
/// log-probability, also used by LM Studio and vLLM) and llama-server's
/// (`logprobs.content[0].top_logprobs[]` of `{token, logprob}`).
pub fn top_logprobs(resp: &Value) -> Option<Vec<(String, f32)>> {
    let lp = resp.get("choices")?.get(0)?.get("logprobs")?;
    if let Some(first) = lp
        .get("top_logprobs")
        .and_then(|t| t.get(0))
        .and_then(Value::as_object)
    {
        let v: Vec<(String, f32)> = first
            .iter()
            .filter_map(|(k, v)| Some((k.clone(), v.as_f64()? as f32)))
            .collect();
        return (!v.is_empty()).then_some(v);
    }
    let top = lp.get("content")?.get(0)?.get("top_logprobs")?.as_array()?;
    let v: Vec<(String, f32)> = top
        .iter()
        .filter_map(|e| {
            Some((
                e.get("token")?.as_str()?.to_owned(),
                e.get("logprob")?.as_f64()? as f32,
            ))
        })
        .collect();
    (!v.is_empty()).then_some(v)
}

/// Comparator for LLM outputs (`Comparator::Logits{max_gap}`).
///
/// Records pair up by position and must carry the same uid and the same number of scores.
/// Inside a pair the argmax must be equal and, over the letters both sides reported, the
/// largest gap between centred scores (`a_i - mean a` against `b_i - mean b`) must be at most
/// `max_gap`. Centring makes outputs that differ by a constant (log-probabilities against
/// raw logits) agree. A letter reported on one side only is accepted only when, on the side
/// that reported it, it lies at least [`MISSING_MIN_DROP`] below that side's best letter (a
/// backend's top log-probabilities can leave out a negligible letter); otherwise it counts as
/// a maximal gap, so blanking the other letters does not pass.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LogitsCompare {
    /// Largest centred gap allowed.
    pub max_gap: f32,
}

fn output_rows(bytes: &[u8]) -> Option<Vec<(u64, Vec<f64>)>> {
    let text = std::str::from_utf8(bytes).ok()?;
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let v: Value = serde_json::from_str(l).ok()?;
            let uid = v.get("uid")?.as_u64()?;
            let xs = v
                .get("tlogits")?
                .as_array()?
                .iter()
                .map(Value::as_f64)
                .collect::<Option<Vec<f64>>>()?;
            Some((uid, xs))
        })
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

impl LogitsCompare {
    /// Whether two score vectors of one record agree.
    pub fn record_same(&self, a: &[f64], b: &[f64]) -> bool {
        if a.len() != b.len() || a.is_empty() {
            return false;
        }
        if a.iter().chain(b).any(|x| !x.is_finite()) {
            return false;
        }
        if argmax(a) != argmax(b) {
            return false;
        }
        let floor = f64::from(MISSING_LOGIT) / 2.0;
        let best = |xs: &[f64]| xs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let (best_a, best_b) = (best(a), best(b));
        let one_sided_ok = a
            .iter()
            .zip(b)
            .all(|(x, y)| match (*x > floor, *y > floor) {
                (true, false) => *x <= best_a - MISSING_MIN_DROP,
                (false, true) => *y <= best_b - MISSING_MIN_DROP,
                _ => true,
            });
        if !one_sided_ok {
            return false;
        }
        let both: Vec<(f64, f64)> = a
            .iter()
            .zip(b)
            .filter(|(x, y)| **x > floor && **y > floor)
            .map(|(x, y)| (*x, *y))
            .collect();
        if both.is_empty() {
            return false;
        }
        let n = both.len() as f64;
        let ma = both.iter().map(|p| p.0).sum::<f64>() / n;
        let mb = both.iter().map(|p| p.1).sum::<f64>() / n;
        both.iter()
            .all(|(x, y)| ((x - ma) - (y - mb)).abs() <= f64::from(self.max_gap))
    }
}

impl nuee_core::Compare for LogitsCompare {
    fn same(&self, a: &[u8], b: &[u8]) -> bool {
        let (Some(x), Some(y)) = (output_rows(a), output_rows(b)) else {
            return false;
        };
        x.len() == y.len()
            && x.iter()
                .zip(&y)
                .all(|((ua, sa), (ub, sb))| ua == ub && self.record_same(sa, sb))
    }
}

#[cfg(feature = "native")]
pub use endpoint::LlmEndpoint;

#[cfg(feature = "native")]
mod endpoint {
    use std::time::Duration;

    use nuee_proto::FailKind;
    use serde_json::{Value, json};

    use super::{
        LETTERS, MAX_RESPONSE, TOP_LOGPROBS, encode_output, letter_scores, parse_task,
        render_prompt, top_logprobs,
    };
    use crate::GpuError;

    /// A local OpenAI-compatible completion server (llama-server, LM Studio, vLLM…).
    ///
    /// Its address only ever comes from this node's own `--llm` flag, never from a job.
    #[derive(Debug, Clone)]
    pub struct LlmEndpoint {
        completions: String,
        http: reqwest::Client,
    }

    impl LlmEndpoint {
        /// An endpoint at `base` (`http://127.0.0.1:8080`, with or without a trailing `/v1`).
        pub fn new(base: &str) -> Result<LlmEndpoint, GpuError> {
            let url = reqwest::Url::parse(base.trim())
                .map_err(|e| GpuError::invalid(format!("--llm {base}: {e}")))?;
            if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                return Err(GpuError::invalid(format!(
                    "--llm {base}: expected an http:// or https:// address"
                )));
            }
            if !url.username().is_empty() || url.password().is_some() {
                return Err(GpuError::invalid(format!(
                    "--llm {base}: credentials in the address are not supported"
                )));
            }
            let mut root = url.as_str().trim_end_matches('/').to_owned();
            if root.ends_with("/v1") {
                root.truncate(root.len() - 3);
            }
            if rustls::crypto::CryptoProvider::get_default().is_none()
                && rustls::crypto::ring::default_provider()
                    .install_default()
                    .is_err()
            {
                // Another thread installed a provider in between, which serves as well.
                debug_assert!(rustls::crypto::CryptoProvider::get_default().is_some());
            }
            let http = reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(5))
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .build()
                .map_err(|e| GpuError::new(FailKind::Io, format!("HTTP client: {e}")))?;
            Ok(LlmEndpoint {
                completions: format!("{root}/v1/completions"),
                http,
            })
        }

        /// The completion URL requests go to.
        pub fn completions_url(&self) -> &str {
            &self.completions
        }

        /// The request body for one prompt.
        pub fn request_body(prompt: &str, model: Option<&str>) -> Value {
            let mut body = json!({
                "prompt": prompt,
                "max_tokens": 1,
                "temperature": 0,
                "logprobs": TOP_LOGPROBS,
                "echo": false,
            });
            if let (Some(m), Some(obj)) = (model, body.as_object_mut()) {
                obj.insert("model".into(), Value::from(m));
            }
            body
        }

        async fn post(&self, body: &Value) -> Result<Value, GpuError> {
            let mut resp = self
                .http
                .post(&self.completions)
                .json(body)
                .send()
                .await
                .map_err(|e| GpuError::new(FailKind::Io, format!("LLM endpoint: {e}")))?;
            let status = resp.status();
            if resp
                .content_length()
                .is_some_and(|n| n > MAX_RESPONSE as u64)
            {
                return Err(GpuError::invalid("LLM endpoint response above 1 MiB"));
            }
            let mut buf = Vec::new();
            while let Some(chunk) = resp
                .chunk()
                .await
                .map_err(|e| GpuError::new(FailKind::Io, format!("LLM endpoint: {e}")))?
            {
                if buf.len() + chunk.len() > MAX_RESPONSE {
                    return Err(GpuError::invalid("LLM endpoint response above 1 MiB"));
                }
                buf.extend_from_slice(&chunk);
            }
            if !status.is_success() {
                let text = String::from_utf8_lossy(&buf);
                let short: String = text.chars().take(160).collect();
                return Err(GpuError::new(
                    FailKind::Io,
                    format!("LLM endpoint answered {status}: {short}"),
                ));
            }
            serde_json::from_slice(&buf)
                .map_err(|e| GpuError::invalid(format!("LLM endpoint sent invalid JSON: {e}")))
        }

        /// Option scores of one prompt with `n` options.
        pub async fn score_prompt(
            &self,
            prompt: &str,
            model: Option<&str>,
            n: usize,
        ) -> Result<Vec<f32>, GpuError> {
            let resp = self.post(&Self::request_body(prompt, model)).await?;
            let top = top_logprobs(&resp).ok_or_else(|| {
                GpuError::new(
                    FailKind::Unsupported,
                    "the LLM endpoint returned no logprobs (llama-server and LM Studio do; \
                     check that /v1/completions honours `logprobs`)",
                )
            })?;
            let scores = letter_scores(&top, n.min(LETTERS.len()));
            if scores.iter().all(|s| *s <= super::MISSING_LOGIT) {
                return Err(GpuError::new(
                    FailKind::Unsupported,
                    "no option letter among the endpoint's top logprobs (token ids instead of \
                     text, or a different prompt format)",
                ));
            }
            Ok(scores)
        }

        /// Runs one task: every record of `input`, within `deadline`.
        pub async fn run(&self, input: &[u8], deadline: Duration) -> Result<Vec<u8>, GpuError> {
            let task = parse_task(input)?;
            let work = async {
                let mut rows = Vec::with_capacity(task.records.len());
                for (uid, r) in &task.records {
                    let s = self
                        .score_prompt(&render_prompt(r), task.model.as_deref(), r.opts.len())
                        .await?;
                    rows.push((*uid, s));
                }
                Ok::<_, GpuError>(rows)
            };
            let rows = tokio::time::timeout(deadline, work)
                .await
                .map_err(|_| GpuError::new(FailKind::Timeout, "LLM task deadline passed"))??;
            Ok(encode_output(&rows))
        }
    }
}
