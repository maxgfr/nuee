//! `kind = "llm"` job files: skycmd decision records scored by peers offering an LLM
//! (`nuee join --llm <url>`, or a browser running Qwen3.5-0.8B).

use std::path::Path;

use nuee_gpu::llm::{
    DEFAULT_MAX_GAP_CROSS_BACKEND, LETTERS, MAX_INPUT, MAX_RECORDS, header_line, parse_task,
};
use nuee_proto::job_toml::{JobToml, MergeKind};
use nuee_proto::{Comparator, Hash, ModuleInfo, TaskKind};

use super::{JobPlan, Prepared, Preset, job_compare, job_verify, jobfile, read_file, usage};

/// Prepares an LLM job: one task per `lines_per_task` records (default and maximum 512),
/// each input led by a header naming `[llm] model`.
pub(super) fn prepare(job: &JobToml, base: &Path) -> anyhow::Result<Prepared> {
    let llm = job
        .llm
        .as_ref()
        .ok_or_else(|| usage("job file: missing [llm] table"))?;
    let letters = llm.letters.as_deref().unwrap_or(LETTERS);
    if !LETTERS.starts_with(letters) {
        return Err(usage(
            "`llm.letters` must be the first letters of the alphabet, in order (\"ABCD\")",
        ));
    }
    let inputs = job
        .inputs
        .as_ref()
        .ok_or_else(|| usage("`[inputs]` is required for this kind"))?;
    let file = inputs
        .jsonl
        .as_ref()
        .ok_or_else(|| usage("llm jobs read their records from `[inputs] jsonl`"))?;
    let per = inputs.lines_per_task.unwrap_or(MAX_RECORDS as u32) as usize;
    if per > MAX_RECORDS {
        return Err(usage(format!(
            "`inputs.lines_per_task` is at most {MAX_RECORDS} for llm jobs"
        )));
    }
    let path = base.join(file);
    let text = read_file(&path)?;
    let header = header_line(&llm.model);
    let mut task_inputs = Vec::new();
    let mut first_lines = Vec::new();
    for shard in jobfile::shard_lines(&text, per) {
        let mut input = header.clone().into_bytes();
        input.extend_from_slice(&shard.bytes);
        if input.len() > MAX_INPUT {
            return Err(usage(format!(
                "{}: lines from {} hold more than 1 MiB; lower `inputs.lines_per_task`",
                path.display(),
                shard.first_line + 1
            )));
        }
        let task = parse_task(&input).map_err(|e| {
            usage(format!(
                "{}: shard from line {}: {}",
                path.display(),
                shard.first_line + 1,
                e.detail
            ))
        })?;
        if let Some((uid, r)) = task
            .records
            .iter()
            .find(|(_, r)| r.opts.len() > letters.len())
        {
            return Err(usage(format!(
                "{}: line {} has {} options, more than `llm.letters`",
                path.display(),
                shard.first_line + uid + 1,
                r.opts.len()
            )));
        }
        task_inputs.push(input);
        first_lines.push(shard.first_line);
    }
    if task_inputs.is_empty() {
        return Err(usage(format!("{} holds no line", path.display())));
    }
    let compare = match job.compare {
        Some(_) => job_compare(job)?,
        None => Comparator::Logits {
            max_gap: DEFAULT_MAX_GAP_CROSS_BACKEND,
        },
    };
    let output = job.output.as_ref();
    let merge = output.and_then(|o| o.merge).unwrap_or(MergeKind::Jsonl);
    let uid_field = match output.and_then(|o| o.uid_field.clone()) {
        Some(f) => Some(f),
        None if merge == MergeKind::Jsonl => Some(jobfile::UID.to_owned()),
        None => None,
    };
    Ok(Prepared {
        name: job.name.clone(),
        preset: Preset::Job(Box::new(JobPlan {
            kind: TaskKind::Llm,
            exec: None,
            compare,
            merge,
            uid_field,
            first_lines,
        })),
        module: Vec::new(),
        info: ModuleInfo {
            hash: Hash::of(&[]),
            size: 0,
            min_memory: 0,
            max_memory: 0,
        },
        assets: Vec::new(),
        inputs: task_inputs,
        verify: job_verify(job)?,
        deadline_ms: Some(job.deadline_ms().map_err(usage)?),
        max_memory: None,
        out: output.and_then(|o| o.dir.as_ref()).map(|d| base.join(d)),
    })
}
