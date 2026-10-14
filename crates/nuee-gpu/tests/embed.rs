//! Embeddings: pinned model files, the candle runner, the NEM1 format, the cosine comparator.
//!
//! The model is downloaded from huggingface.co the first time (about 90 MB) and cached under
//! `target/test-models/`, so later runs are offline.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use nuee_core::Compare;
use nuee_gpu::embed::{
    self, CHUNK_CHARS, Cosine, DIM, EmbedDevice, Embedder, MAX_TEXT_BYTES, MAX_TEXTS, MINILM,
    ModelError, TAU, chunk_text, cosine, decode_input, decode_output, encode_input, encode_output,
};
use nuee_proto::{Comparator, FailKind, TaskKind};

fn cache_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-models")
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(f)
}

fn fetched() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        block_on(MINILM.fetch(&cache_root())).expect("download the pinned model");
        cache_root()
    })
}

fn cpu() -> &'static Embedder {
    static E: OnceLock<Embedder> = OnceLock::new();
    E.get_or_init(|| Embedder::load(fetched(), EmbedDevice::Cpu).expect("load on the CPU"))
}

fn corpus() -> Vec<String> {
    let text = include_str!("fixtures/embed_corpus.jsonl");
    text.lines()
        .map(|l| serde_json::from_str::<String>(l).expect("a JSON string per line"))
        .collect()
}

fn web_vectors(count: usize) -> Vec<f32> {
    let bytes = include_bytes!("fixtures/embed_web.f32");
    decode_output(bytes, count).expect("one browser row per corpus text")
}

fn row(v: &[f32], i: usize) -> &[f32] {
    &v[i * DIM..(i + 1) * DIM]
}

fn norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

// ---------------------------------------------------------------------------------------------
// Model files.

fn copy_cache(to: &Path) {
    let from = MINILM.dir(fetched());
    let dest = MINILM.dir(to);
    std::fs::create_dir_all(&dest).unwrap();
    for f in MINILM.files {
        std::fs::copy(from.join(f.path), dest.join(f.path)).unwrap();
    }
}

#[test]
fn cached_files_match_their_pins() {
    let root = fetched();
    for f in MINILM.files {
        let bytes = MINILM
            .read_cached(root, f)
            .expect("readable")
            .expect("downloaded");
        assert_eq!(bytes.len() as u64, f.size, "{}", f.path);
        assert_eq!(embed::sha256_hex(&bytes), f.sha256, "{}", f.path);
    }
}

#[test]
fn corrupt_cache_file_is_never_used() {
    let tmp = tempfile::tempdir().unwrap();
    copy_cache(tmp.path());
    let config = MINILM.dir(tmp.path()).join("config.json");
    let mut bytes = std::fs::read(&config).unwrap();
    bytes[10] ^= 0x20;
    std::fs::write(&config, &bytes).unwrap();

    let file = &MINILM.files[0];
    assert!(matches!(
        MINILM.read_cached(tmp.path(), file),
        Err(ModelError::Integrity { .. })
    ));
    let err = Embedder::load(tmp.path(), EmbedDevice::Cpu).expect_err("corrupt file loaded");
    assert_eq!(err.kind, FailKind::Invalid, "{err}");

    // Fetching again replaces it with the pinned bytes.
    block_on(MINILM.fetch(tmp.path())).expect("download again");
    let fixed = MINILM.read_cached(tmp.path(), file).unwrap().unwrap();
    assert_eq!(embed::sha256_hex(&fixed), file.sha256);
    Embedder::load(tmp.path(), EmbedDevice::Cpu).expect("loads once repaired");
}

#[test]
fn missing_files_are_an_error_not_a_download() {
    let tmp = tempfile::tempdir().unwrap();
    let err = Embedder::load(tmp.path(), EmbedDevice::Cpu).expect_err("nothing to load");
    assert_eq!(err.kind, FailKind::Io);
}

#[test]
fn downloads_only_from_hugging_face_hosts() {
    for ok in [
        "huggingface.co",
        "cdn-lfs.huggingface.co",
        "us.aws.cdn.hf.co",
        "cas-bridge.xethub.hf.co",
    ] {
        assert!(embed::allowed_host(ok), "{ok}");
    }
    for bad in [
        "evil.com",
        "huggingface.co.evil.com",
        "hf.co.evil.com",
        "127.0.0.1",
        "notHuggingface.co",
    ] {
        assert!(!embed::allowed_host(bad), "{bad}");
    }
    let url = MINILM.url(&MINILM.files[2]);
    assert!(url.starts_with("https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2/resolve/1110a243fdf4706b3f48f1d95db1a4f5529b4d41/"));
}

// ---------------------------------------------------------------------------------------------
// Embeddings.

#[test]
fn embeddings_are_normalised_and_deterministic_on_cpu() {
    let texts = ["Hello world", "Bonjour le monde", "", "fn main() {}"];
    let a = cpu().embed(&texts).unwrap();
    assert_eq!(a.len(), texts.len() * DIM);
    for i in 0..texts.len() {
        assert!((norm(row(&a, i)) - 1.0).abs() < 1e-4, "row {i}");
    }
    let b = cpu().embed(&texts).unwrap();
    assert_eq!(a, b, "same texts, same bits");
    // A freshly loaded model gives the same bits too.
    let again = Embedder::load(fetched(), EmbedDevice::Cpu).unwrap();
    assert_eq!(a, again.embed(&texts).unwrap());
}

#[test]
fn embeddings_are_semantically_sane() {
    let t = [
        "The cat sits on the mat.",
        "A cat is sitting on a rug.",
        "The stock market fell sharply today.",
        "How do I reset my password?",
        "I forgot my password, how can I change it?",
    ];
    let v = cpu().embed(&t).unwrap();
    let c = |i, j| cosine(row(&v, i), row(&v, j)).unwrap();
    assert!(
        c(0, 1) > c(0, 2) + 0.3,
        "cats {} vs market {}",
        c(0, 1),
        c(0, 2)
    );
    assert!(
        c(3, 4) > c(3, 2) + 0.3,
        "password {} vs market {}",
        c(3, 4),
        c(3, 2)
    );
}

#[test]
fn long_texts_are_truncated_not_rejected() {
    let long = "word ".repeat(2000);
    let v = cpu().embed(&[long.as_str()]).unwrap();
    assert_eq!(v.len(), DIM);
    assert!((norm(&v) - 1.0).abs() < 1e-4);
}

#[test]
fn task_runs_through_nem1() {
    let input = encode_input(&["one", "two", "three"]).unwrap();
    let out = cpu().run(&input).unwrap();
    assert_eq!(out.len(), 3 * DIM * 4);
    let direct = cpu().embed(&["one", "two", "three"]).unwrap();
    assert_eq!(out, encode_output(&direct));
    let err = cpu().run(b"NEM1\0\0\0\0").unwrap_err();
    assert_eq!(err.kind, FailKind::Invalid);
}

/// Lowest cosine between `e`'s rows and the browser's, with its text, and the rows.
fn against_browser(e: &Embedder) -> (f32, usize, Vec<f32>, Vec<f32>) {
    let texts = corpus();
    assert!(
        texts.len() >= 200,
        "calibration corpus has {} texts",
        texts.len()
    );
    let web = web_vectors(texts.len());
    let native = e.embed(&texts).unwrap();
    let mut low = (f32::INFINITY, 0);
    for i in 0..texts.len() {
        let c = cosine(row(&native, i), row(&web, i)).unwrap();
        if c < low.0 {
            low = (c, i);
        }
    }
    (low.0, low.1, native, web)
}

#[test]
fn candle_cpu_agrees_with_the_browser_reference() {
    let (low, at, native, web) = against_browser(cpu());
    println!("lowest cosine candle cpu vs browser: {low:.5} (text {at})");
    assert!(low >= TAU, "text {at}: cosine {low} below tau {TAU}");
    // The comparator agrees too, task by task.
    let cmp = Cosine { min: TAU };
    for (i, (n, w)) in native
        .chunks(64 * DIM)
        .zip(web.chunks(64 * DIM))
        .enumerate()
    {
        assert!(cmp.same(&encode_output(n), &encode_output(w)), "task {i}");
    }
}

#[test]
fn default_device_agrees_with_the_browser_reference() {
    let e = Embedder::load(fetched(), EmbedDevice::Auto).unwrap();
    let (low, at, _, _) = against_browser(&e);
    println!(
        "lowest cosine candle {} vs browser: {low:.5} (text {at})",
        e.device_name()
    );
    assert!(low >= TAU, "text {at}: cosine {low} below tau {TAU}");
}

// ---------------------------------------------------------------------------------------------
// Task format.

#[test]
fn input_roundtrips() {
    let texts = vec!["a".to_owned(), String::new(), "é🙂".to_owned()];
    let bytes = encode_input(&texts).unwrap();
    assert_eq!(&bytes[..4], b"NEM1");
    assert_eq!(decode_input(&bytes).unwrap(), texts);
    let max = vec!["x".repeat(MAX_TEXT_BYTES); 2];
    assert_eq!(decode_input(&encode_input(&max).unwrap()).unwrap(), max);
}

#[test]
fn malformed_inputs_are_rejected() {
    let good = encode_input(&["hello", "world"]).unwrap();
    let invalid = |b: &[u8]| decode_input(b).map(|_| ()).unwrap_err().kind == FailKind::Invalid;

    assert!(invalid(b""));
    assert!(invalid(b"NEM"));
    assert!(invalid(b"NEM2\x01\0\0\0\0\0\0\0"));
    assert!(invalid(b"NEM1\0\0\0\0"), "zero texts");
    let mut too_many = b"NEM1".to_vec();
    too_many.extend_from_slice(&(MAX_TEXTS as u32 + 1).to_le_bytes());
    too_many.extend(std::iter::repeat_n(0u8, 4 * (MAX_TEXTS + 1)));
    assert!(invalid(&too_many));
    for cut in 1..good.len() {
        assert!(invalid(&good[..cut]), "truncated at {cut}");
    }
    let mut trailing = good.clone();
    trailing.push(0);
    assert!(invalid(&trailing));
    let mut not_utf8 = good.clone();
    not_utf8[12] = 0xff;
    assert!(invalid(&not_utf8));
    // A length above the cap is refused before reading the text.
    let mut huge = b"NEM1\x01\0\0\0".to_vec();
    huge.extend_from_slice(&(MAX_TEXT_BYTES as u32 + 1).to_le_bytes());
    assert!(invalid(&huge));
    let mut huge = b"NEM1\x01\0\0\0".to_vec();
    huge.extend_from_slice(&u32::MAX.to_le_bytes());
    assert!(invalid(&huge));

    assert!(encode_input::<&str>(&[]).is_err());
    assert!(encode_input(&["x".repeat(MAX_TEXT_BYTES + 1)]).is_err());
    assert!(encode_input(&vec!["x"; MAX_TEXTS + 1]).is_err());
}

#[test]
fn outputs_decode_by_shape() {
    let rows: Vec<f32> = (0..2 * DIM).map(|i| i as f32).collect();
    let bytes = encode_output(&rows);
    assert_eq!(decode_output(&bytes, 2).unwrap(), rows);
    assert!(decode_output(&bytes, 1).is_none());
    assert!(decode_output(&bytes[1..], 2).is_none());
}

// ---------------------------------------------------------------------------------------------
// Comparator.

fn unit(seed: u32) -> Vec<f32> {
    let v: Vec<f32> = (0..DIM as u32)
        .map(|i| {
            ((i.wrapping_mul(2_654_435_761) ^ seed.wrapping_mul(40_503)) % 1000) as f32 - 500.0
        })
        .collect();
    let n = norm(&v);
    v.into_iter().map(|x| x / n).collect()
}

#[test]
fn cosine_comparator() {
    let cmp = Cosine { min: 0.95 };
    let a: Vec<f32> = [unit(1), unit(2)].concat();
    let ab = encode_output(&a);
    assert!(cmp.same(&ab, &ab));

    // A small perturbation of one row stays above the threshold.
    let mut b = a.clone();
    b[3] += 0.05;
    assert!(cmp.same(&ab, &encode_output(&b)));

    // One unrelated row is enough to disagree.
    let c = [unit(1), unit(3)].concat();
    assert!(Cosine::lowest(&ab, &encode_output(&c)).unwrap() < 0.95);
    assert!(!cmp.same(&ab, &encode_output(&c)));

    // Rows must stay L2-normalised: a scaled copy points the same way but is refused.
    let scaled: Vec<f32> = a.iter().map(|x| x * 3.0).collect();
    assert!(Cosine::lowest(&ab, &encode_output(&scaled)).unwrap() > 0.999);
    assert!(!cmp.same(&ab, &encode_output(&scaled)));
    assert!(!cmp.same(&encode_output(&scaled), &ab));
    let flipped: Vec<f32> = a.iter().map(|x| -x).collect();
    assert!(!cmp.same(&ab, &encode_output(&flipped)));

    // Shapes, zero rows and non-finite values never agree.
    assert!(!cmp.same(&ab, &ab[..ab.len() - 4]));
    assert!(!cmp.same(&ab[..DIM * 4], &ab));
    assert!(!cmp.same(&[], &[]));
    assert!(!cmp.same(&ab[..10], &ab[..10]));
    let zero = encode_output(&vec![0.0; 2 * DIM]);
    assert!(!cmp.same(&zero, &zero));
    let mut nan = a.clone();
    nan[0] = f32::NAN;
    assert!(!cmp.same(&encode_output(&nan), &encode_output(&nan)));
    let mut inf = a.clone();
    inf[0] = f32::INFINITY;
    assert!(!cmp.same(&encode_output(&inf), &ab));
}

#[test]
fn manifests_carry_the_calibrated_threshold() {
    const { assert!(TAU > 0.9 && TAU < 1.0) };
    assert_eq!(embed::COMPARE, Comparator::Cosine { min: TAU });
    let cmp =
        nuee_gpu::verify::comparator(TaskKind::Embed, &embed::COMPARE).expect("cosine provided");
    let a = encode_output(&unit(7));
    assert!(cmp.same(&a, &a));
    assert!(!cmp.same(&a, &encode_output(&unit(8))));
}

// ---------------------------------------------------------------------------------------------
// Chunking.

#[test]
fn chunks_cover_the_text_with_overlap() {
    let para = "Sentence number one is here. Another sentence follows it! Does a third? ";
    let text = format!("{}\n\n{}", para.repeat(20), para.repeat(25));
    let chunks = chunk_text(&text);
    assert!(chunks.len() > 2);
    assert_eq!(chunks[0].start, 0);
    assert_eq!(chunks.last().unwrap().end, text.trim_end().len());
    for w in chunks.windows(2) {
        assert!(w[1].start < w[0].end, "consecutive chunks overlap");
        assert!(w[1].start > w[0].start);
    }
    for c in &chunks {
        let s = &text[c.start..c.end];
        assert!(
            s.chars().count() <= CHUNK_CHARS,
            "{} chars",
            s.chars().count()
        );
        assert!(s.len() <= MAX_TEXT_BYTES);
        assert!(!s.starts_with(char::is_whitespace) && !s.ends_with(char::is_whitespace));
    }
    // Cuts land after a sentence end.
    assert!(text[chunks[0].start..chunks[0].end].ends_with(['.', '!', '?']));
}

#[test]
fn chunking_edge_cases() {
    assert!(chunk_text("").is_empty());
    assert!(chunk_text(" \n\n\t ").is_empty());
    let short = "  Just one line.  ";
    let c = chunk_text(short);
    assert_eq!(c.len(), 1);
    assert_eq!(&short[c[0].start..c[0].end], "Just one line.");
    // No break anywhere: hard cuts on character boundaries.
    let solid = "é".repeat(2500);
    let chunks = chunk_text(&solid);
    assert!(chunks.len() >= 3);
    for c in &chunks {
        assert!(solid.is_char_boundary(c.start) && solid.is_char_boundary(c.end));
        assert!(solid[c.start..c.end].chars().count() <= CHUNK_CHARS);
    }
    assert_eq!(chunks.last().unwrap().end, solid.len());
}
