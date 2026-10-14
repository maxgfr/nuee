//! Stream framing and task input parsers fed arbitrary bytes: length-prefixed frames on iroh
//! streams, WebRTC lane frames, and the WGSL (`NWG1`), embedding (`NEM1`), LLM (JSONL) and
//! training (`NTR1`) task inputs a worker receives. None may panic; what parses must respect
//! its limits and, where an encoder exists, re-encode to the same bytes.
#![no_main]

use libfuzzer_sys::fuzz_target;
use nuee_gpu::wgsl::{self, BindingKind, BindingSpec};
use nuee_gpu::{embed, llm};
use nuee_net::framing::{frame_bytes, split_frames};
use nuee_net::rtc::{self, MAX_LANE_FRAME, TAG_RESULT};
use nuee_proto::limits::MAX_MSG;

fn frames(data: &[u8], max: usize) {
    let Ok(frames) = split_frames(data, max) else {
        return;
    };
    let mut again = Vec::with_capacity(data.len());
    for f in &frames {
        assert!(f.len() <= max);
        again.extend(frame_bytes(f).expect("a split frame re-frames"));
    }
    assert_eq!(again, data);
}

fn lane(data: &[u8]) {
    if let Ok((lease, msg)) = rtc::decode_result(data) {
        assert!(data.len() <= MAX_LANE_FRAME);
        let mut frame = vec![TAG_RESULT];
        frame.extend(nuee_proto::encode(&msg).expect("a decoded result re-encodes"));
        assert_eq!(
            rtc::decode_result(&frame).expect("a re-encoded result decodes"),
            (lease, msg)
        );
    }
    if let Ok(lease) = rtc::decode_ack(data) {
        assert_eq!(rtc::ack_frame(lease)[..], data[..]);
    }
}

fn wgsl_input(data: &[u8]) {
    let Ok(task) = wgsl::parse_input(data) else {
        return;
    };
    assert!(!task.entry.is_empty() && task.entry.len() <= wgsl::MAX_ENTRY);
    assert!(
        task.workgroups
            .iter()
            .all(|w| (1..=wgsl::MAX_WORKGROUPS).contains(w))
    );
    assert!((1..=wgsl::MAX_BINDINGS).contains(&task.bindings.len()));
    assert!(task.output_len() > 0);
    let specs: Vec<BindingSpec<'_>> = task
        .bindings
        .iter()
        .map(|b| match b.kind {
            BindingKind::Uniform => BindingSpec::Uniform(b.binding, b.data),
            BindingKind::ReadOnly => BindingSpec::ReadOnly(b.binding, b.data),
            BindingKind::Output => BindingSpec::Output(b.binding, b.size),
        })
        .collect();
    assert_eq!(
        wgsl::encode_input(task.entry, task.workgroups, &specs),
        data
    );
}

fn embed_input(data: &[u8]) {
    let Ok(texts) = embed::decode_input(data) else {
        return;
    };
    assert!((1..=embed::MAX_TEXTS).contains(&texts.len()));
    assert!(texts.iter().all(|t| t.len() <= embed::MAX_TEXT_BYTES));
    assert_eq!(
        embed::encode_input(&texts).expect("decoded texts re-encode"),
        data
    );
}

fn llm_input(data: &[u8]) {
    if let Ok(task) = llm::parse_task(data) {
        assert!(data.len() <= llm::MAX_INPUT);
        assert!(task.records.len() <= llm::MAX_RECORDS);
    }
}

fn train_input(data: &[u8]) {
    if let Ok(input) = nuee_train::TaskInput::decode(data) {
        assert_eq!(input.encode(), data);
    }
}

fuzz_target!(|data: &[u8]| {
    frames(data, MAX_MSG);
    frames(data, 64);
    lane(data);
    wgsl_input(data);
    embed_input(data);
    llm_input(data);
    train_input(data);
});
