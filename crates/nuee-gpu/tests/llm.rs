//! LLM option scoring: prompt parity with skycmd's teacher, the OpenAI-compatible endpoint
//! client against a mock server, and the logits comparator.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nuee_core::Compare;
use nuee_gpu::llm::{
    LlmEndpoint, LogitsCompare, MISSING_LOGIT, Record, encode_output, header_line, letter_scores,
    parse_task, render_prompt, top_logprobs,
};
use nuee_proto::{Comparator, FailKind, TaskKind};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------------------------
// Prompt rendering.

const TAIL: &str = "<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n";

fn rec(line: &str) -> Record {
    serde_json::from_str(line).expect("record")
}

/// Expected strings come from `prompt()` of teacher-score.py, copied as is, with the chat
/// template of the pinned Qwen3.5-0.8B revision rendered by Jinja2 (the same path
/// `tok.apply_chat_template` takes).
#[test]
fn prompts_match_the_teacher_byte_for_byte() {
    let choice = rec(
        "{\"t\":\"choice\",\"ins\":\"  Which way now?\\n\",\"opts\":[\"left\",\"right\",\"stay\"],\
         \"state\":\"\\n  bird y=0.42 \u{2014} gap at 0.55\\n  vy=-0.1 \\u001f \"}",
    );
    assert_eq!(
        render_prompt(&choice),
        "<|im_start|>user\nContext:\nbird y=0.42 \u{2014} gap at 0.55\n  vy=-0.1\n\nQuestion: Which way now?\nChoose the best option.\n\nOptions:\nA. left\nB. right\nC. stay\n\nAnswer with the letter of the correct option only.<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n"
    );

    let score = rec(
        r#"{"t":"score","ins":"How risky is the next pipe?","opts":["low","medium","high","extreme"],"state":null,"target":[0,1,0,0]}"#,
    );
    assert_eq!(
        render_prompt(&score),
        "<|im_start|>user\nQuestion: How risky is the next pipe?\nChoose the level that fits best.\n\nOptions:\nA. low\nB. medium\nC. high\nD. extreme\n\nAnswer with the letter of the correct option only.<|im_end|>\n<|im_start|>assistant\n<think>\n\n</think>\n\n"
    );

    // 2,400 characters of state are kept, counted in characters, not bytes.
    let long = format!("{}{}", "\u{e9}".repeat(2000), "x".repeat(1000));
    let noul = Record {
        t: "noul".into(),
        ins: "The exit is reachable.".into(),
        opts: vec!["yes".into(), "no".into()],
        state: Some(long),
    };
    let want = format!(
        "<|im_start|>user\nContext:\n{}{}\n\nQuestion: The exit is reachable.\nDecide whether the statement holds.\n\nOptions:\nA. yes\nB. no\n\nAnswer with the letter of the correct option only.{TAIL}",
        "\u{e9}".repeat(2000),
        "x".repeat(400)
    );
    assert_eq!(render_prompt(&noul), want);
}

#[test]
fn task_inputs_are_checked() {
    let text = format!(
        "{}{}\n\n{}\n",
        header_line("qwen3.5-0.8b"),
        r#"{"t":"choice","ins":"a","opts":["x","y"]}"#,
        r#"{"t":"noul","ins":"b","opts":["yes","no"],"uid":77}"#
    );
    let task = parse_task(text.as_bytes()).expect("task");
    assert_eq!(task.model.as_deref(), Some("qwen3.5-0.8b"));
    // Uids are line indices after the header; blank lines count, own uids are replaced.
    let uids: Vec<u64> = task.records.iter().map(|(u, _)| *u).collect();
    assert_eq!(uids, vec![0, 2]);

    let one_opt = r#"{"t":"choice","ins":"a","opts":["x"]}"#;
    assert_eq!(
        parse_task(one_opt.as_bytes()).expect_err("1 option").kind,
        FailKind::Invalid
    );
    let line = r#"{"t":"choice","ins":"a","opts":["x","y"]}"#;
    let many = format!("{line}\n").repeat(513);
    assert!(parse_task(many.as_bytes()).is_err());
    assert!(parse_task(&vec![b' '; (1 << 20) + 1]).is_err());
    assert!(parse_task(b"{\"t\":1}").is_err());
}

#[test]
fn letters_map_with_or_without_a_leading_space() {
    let top = vec![
        ("A".to_owned(), -2.0),
        (" A".to_owned(), -0.5),
        (" B".to_owned(), -1.0),
        ("Because".to_owned(), -0.1),
        ("D".to_owned(), -3.0),
    ];
    assert_eq!(letter_scores(&top, 3), vec![-0.5, -1.0, MISSING_LOGIT]);
    assert_eq!(letter_scores(&top, 4)[3], -3.0);
}

// ---------------------------------------------------------------------------------------------
// The endpoint, against a mock OpenAI-compatible server.

type Reply = Arc<dyn Fn(&Value) -> (u16, String) + Send + Sync>;

struct Mock {
    url: String,
    bodies: Arc<Mutex<Vec<(String, Value)>>>,
}

fn read_request(stream: &mut TcpStream) -> Option<(String, Value)> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).ok()?;
    let mut len = 0usize;
    loop {
        let mut h = String::new();
        reader.read_line(&mut h).ok()?;
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':')
            && k.eq_ignore_ascii_case("content-length")
        {
            len = v.trim().parse().ok()?;
        }
    }
    let mut body = vec![0; len];
    reader.read_exact(&mut body).ok()?;
    let path = request_line.split(' ').nth(1)?.to_owned();
    Some((path, serde_json::from_slice(&body).ok()?))
}

fn mock(reply: Reply) -> Mock {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let seen = bodies.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let Some((path, body)) = read_request(&mut stream) else {
                continue;
            };
            let (status, text) = reply(&body);
            seen.lock().expect("lock").push((path, body));
            let head = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                text.len()
            );
            if stream.write_all(head.as_bytes()).is_err()
                || stream.write_all(text.as_bytes()).is_err()
            {
                eprintln!("mock: client went away");
            }
        }
    });
    Mock { url, bodies }
}

/// The letter of the first option named in the prompt's question (`pick X`).
fn picked(prompt: &str) -> char {
    prompt
        .split("pick ")
        .nth(1)
        .and_then(|s| s.chars().next())
        .unwrap_or('A')
}

fn openai_reply() -> Reply {
    Arc::new(|body: &Value| {
        let p = body["prompt"].as_str().unwrap_or_default();
        let best = picked(p);
        let top = json!({
            format!(" {best}"): -0.25,
            best.to_string(): -1.5,
            "A": -3.0,
            " B": -2.0,
            "The": -4.0,
        });
        let resp = json!({
            "choices": [{ "text": " A", "logprobs": { "tokens": [" A"], "top_logprobs": [top] } }]
        });
        (200, resp.to_string())
    })
}

fn records() -> String {
    [
        r#"{"t":"choice","ins":"pick C","opts":["a","b","c"],"state":"s"}"#,
        r#"{"t":"noul","ins":"pick A","opts":["yes","no"]}"#,
    ]
    .join("\n")
}

#[tokio::test]
async fn openai_shape_maps_to_tlogits() {
    let m = mock(openai_reply());
    let ep = LlmEndpoint::new(&format!("{}/v1/", m.url)).expect("endpoint");
    assert_eq!(ep.completions_url(), format!("{}/v1/completions", m.url));
    let input = format!("{}{}", header_line("qwen"), records());
    let out = ep
        .run(input.as_bytes(), Duration::from_secs(10))
        .await
        .expect("scored");
    // C: " C" -0.25 beats "C" -1.5; A: only "A" -3.0; B: " B" -2.0.
    // Second record: " A" -0.25 beats "A" -1.5; B -2.0.
    let want = encode_output(&[(0, vec![-3.0, -2.0, -0.25]), (1, vec![-0.25, -2.0])]);
    assert_eq!(
        String::from_utf8(out.clone()).expect("utf8"),
        String::from_utf8(want).expect("utf8")
    );
    assert_eq!(
        String::from_utf8(out).expect("utf8"),
        "{\"uid\":0,\"tlogits\":[-3.0,-2.0,-0.25]}\n{\"uid\":1,\"tlogits\":[-0.25,-2.0]}\n"
    );

    let bodies = m.bodies.lock().expect("lock").clone();
    assert_eq!(bodies.len(), 2);
    let (path, b) = &bodies[0];
    assert_eq!(path, "/v1/completions");
    assert_eq!(b["max_tokens"], 1);
    assert_eq!(b["temperature"], 0);
    assert_eq!(b["logprobs"], 20);
    assert_eq!(b["echo"], false);
    assert_eq!(b["model"], "qwen");
    let r = rec(r#"{"t":"choice","ins":"pick C","opts":["a","b","c"],"state":"s"}"#);
    assert_eq!(b["prompt"].as_str(), Some(render_prompt(&r).as_str()));
}

#[tokio::test]
async fn llama_server_shape_and_missing_letters() {
    let m = mock(Arc::new(|_: &Value| {
        let resp = json!({
            "choices": [{ "text": "B", "logprobs": { "content": [{
                "token": "B", "logprob": -0.1,
                "top_logprobs": [
                    { "token": "B", "logprob": -0.1 },
                    { "token": " A", "logprob": -2.5 },
                    { "token": "\n", "logprob": -5.0 }
                ]
            }] } }]
        });
        (200, resp.to_string())
    }));
    let ep = LlmEndpoint::new(&m.url).expect("endpoint");
    let out = ep
        .run(
            br#"{"t":"choice","ins":"q","opts":["a","b","c"]}"#,
            Duration::from_secs(10),
        )
        .await
        .expect("scored");
    assert_eq!(
        String::from_utf8(out).expect("utf8"),
        "{\"uid\":0,\"tlogits\":[-2.5,-0.1,-10000.0]}\n"
    );
    // No model header: no model field.
    assert!(m.bodies.lock().expect("lock")[0].1.get("model").is_none());
}

#[tokio::test]
async fn an_endpoint_without_logprobs_is_refused_clearly() {
    let m = mock(Arc::new(|_: &Value| {
        (200, json!({ "choices": [{ "text": "A" }] }).to_string())
    }));
    let ep = LlmEndpoint::new(&m.url).expect("endpoint");
    let err = ep
        .run(
            br#"{"t":"choice","ins":"q","opts":["a","b"]}"#,
            Duration::from_secs(10),
        )
        .await
        .expect_err("no logprobs");
    assert_eq!(err.kind, FailKind::Unsupported);
    assert!(err.detail.contains("no logprobs"), "{}", err.detail);

    // Token ids instead of text (mlx_lm.server) match no letter.
    let m = mock(Arc::new(|_: &Value| {
        let top = json!({ "32": -0.1, "33": -1.0 });
        (
            200,
            json!({ "choices": [{ "logprobs": { "top_logprobs": [top] } }] }).to_string(),
        )
    }));
    let ep = LlmEndpoint::new(&m.url).expect("endpoint");
    let err = ep
        .run(
            br#"{"t":"choice","ins":"q","opts":["a","b"]}"#,
            Duration::from_secs(10),
        )
        .await
        .expect_err("ids");
    assert_eq!(err.kind, FailKind::Unsupported);
}

#[tokio::test]
async fn responses_are_size_capped_and_errors_reported() {
    let m = mock(Arc::new(|_: &Value| {
        (200, format!("\"{}\"", "x".repeat(2 << 20)))
    }));
    let ep = LlmEndpoint::new(&m.url).expect("endpoint");
    let err = ep
        .run(
            br#"{"t":"choice","ins":"q","opts":["a","b"]}"#,
            Duration::from_secs(10),
        )
        .await
        .expect_err("too big");
    assert_eq!(err.kind, FailKind::Invalid);
    assert!(err.detail.contains("1 MiB"), "{}", err.detail);

    let m = mock(Arc::new(|_: &Value| {
        (500, "{\"error\":\"model not loaded\"}".into())
    }));
    let ep = LlmEndpoint::new(&m.url).expect("endpoint");
    let err = ep
        .run(
            br#"{"t":"choice","ins":"q","opts":["a","b"]}"#,
            Duration::from_secs(10),
        )
        .await
        .expect_err("500");
    assert_eq!(err.kind, FailKind::Io);
    assert!(err.detail.contains("model not loaded"), "{}", err.detail);
}

#[test]
fn only_http_endpoints_are_accepted() {
    for bad in [
        "file:///etc/passwd",
        "ftp://h/",
        "localhost:8080",
        "http://u:p@127.0.0.1/",
    ] {
        assert!(LlmEndpoint::new(bad).is_err(), "{bad}");
    }
    let ep = LlmEndpoint::new("http://127.0.0.1:8080").expect("plain");
    assert_eq!(ep.completions_url(), "http://127.0.0.1:8080/v1/completions");
}

#[test]
fn top_logprobs_needs_a_logprobs_block() {
    assert!(top_logprobs(&json!({ "choices": [{ "text": "A" }] })).is_none());
    assert!(top_logprobs(&json!({})).is_none());
}

// ---------------------------------------------------------------------------------------------
// The comparator.

fn out(rows: &[(u64, Vec<f32>)]) -> Vec<u8> {
    encode_output(rows)
}

#[test]
fn logits_comparator() {
    let c = LogitsCompare { max_gap: 0.25 };
    let a = out(&[(0, vec![1.0, 3.0, 0.5]), (1, vec![-0.2, -2.0])]);
    assert!(c.same(&a, &a));

    // A constant shift (log-probabilities against raw logits) is accepted.
    let shifted = out(&[(0, vec![-6.0, -4.0, -6.5]), (1, vec![10.0, 8.2])]);
    assert!(c.same(&a, &shifted));

    // Within the gap, then just beyond it.
    let near = out(&[(0, vec![1.2, 3.0, 0.5]), (1, vec![-0.2, -2.0])]);
    assert!(c.same(&a, &near));
    let far = out(&[(0, vec![1.6, 3.0, 0.5]), (1, vec![-0.2, -2.0])]);
    assert!(!c.same(&a, &far));
    assert!(LogitsCompare { max_gap: 1.5 }.same(&a, &far));

    // A different answer is rejected whatever the gap.
    let flip = out(&[(0, vec![3.0, 2.9, 0.5]), (1, vec![-0.2, -2.0])]);
    assert!(!LogitsCompare { max_gap: 100.0 }.same(&a, &flip));

    // A letter missing on one side passes only when the other side scores it negligibly
    // (at least MISSING_MIN_DROP below its best); otherwise it is a maximal gap.
    let missing = out(&[(0, vec![1.0, 3.0, MISSING_LOGIT]), (1, vec![-0.2, -2.0])]);
    assert!(!c.same(&a, &missing));
    let low = out(&[(0, vec![1.0, 3.0, -12.0]), (1, vec![-0.2, -2.0])]);
    let low_missing = out(&[(0, vec![1.0, 3.0, MISSING_LOGIT]), (1, vec![-0.2, -2.0])]);
    assert!(c.same(&low, &low_missing));
    assert!(c.same(&low_missing, &low));
    // Blanking every letter but the argmax (a forged one-hot) does not pass.
    let one_hot = out(&[
        (0, vec![MISSING_LOGIT, 3.0, MISSING_LOGIT]),
        (1, vec![-0.2, MISSING_LOGIT]),
    ]);
    assert!(!LogitsCompare { max_gap: 100.0 }.same(&a, &one_hot));
    assert!(!LogitsCompare { max_gap: 100.0 }.same(&one_hot, &a));

    // Record count, uids, option counts and garbage.
    assert!(!c.same(&a, &out(&[(0, vec![1.0, 3.0, 0.5])])));
    assert!(!c.same(&a, &out(&[(0, vec![1.0, 3.0, 0.5]), (2, vec![-0.2, -2.0])])));
    assert!(!c.same(&a, &out(&[(0, vec![1.0, 3.0]), (1, vec![-0.2, -2.0])])));
    assert!(!c.same(&a, b"not json"));

    // The coordinator gets it from the manifest's comparator.
    let cmp = nuee_gpu::verify::comparator(TaskKind::Llm, &Comparator::Logits { max_gap: 0.25 })
        .expect("logits comparator");
    assert!(cmp.same(&a, &shifted));
    assert!(!cmp.same(&a, &flip));
}
