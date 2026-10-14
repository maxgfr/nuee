//! skycmd arena: plays the skycmd decision games with a GGUF decision model, inside WebAssembly.
//!
//! This is `sky-arena` (maxgfr/skycmd) without a clock: the same games, the same seeds and the
//! same model give the same output bytes on every engine.
//!
//! Input: an asset envelope (see `nuee_task::split_assets`) with exactly one asset, the GGUF model
//! bytes, followed by a UTF-8 JSON object:
//!
//! ```json
//! {"mode": "eval" | "oracle" | "dagger",
//!  "game": "flappy" | "rescue" | "dodge" | "doom",
//!  "seed": 7,            // u64
//!  "episodes": 3,        // eval/oracle: 1..=200
//!  "tau": 0.6,           // optional early-exit threshold of the model
//!  "max_pipes": 200,     // optional flappy episode cap, default 200
//!  "n": 200}             // dagger: number of records, 1..=20000
//! ```
//!
//! Output (UTF-8):
//! - `eval` (the model plays) and `oracle` (the simulator's oracle plays, the model is not loaded):
//!   `{"mode","game","seed","episodes","metrics":{game, episodes, mean_score, kill_rate,
//!   rescue_rate, completion_rate, crashes, vetoes, aborts, safe_aborts, mean_steps}}`.
//! - `dagger`: JSON lines, one `DecisionRecord` per line, labelled by the oracle on the states the
//!   model visits itself (seed mixed with `0xda66e7`, as `sky-arena --dagger` does).
//!
//! Invalid input panics with a message, which traps the instance.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sky_games::r#gen::SmallRng;
use sky_games::rollout::{EvalOptions, Metrics, OraclePolicy, Policy, evaluate_with};
use sky_games::{DecisionGame, GAMES, make_game};
use sky_infer::{Engine, EngineOptions, QType};

const MAX_EPISODES: u32 = 200;
const MAX_RECORDS: u32 = 20_000;
const DEFAULT_MAX_PIPES: u32 = 200;

#[derive(Deserialize, Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
enum Mode {
    Eval,
    Oracle,
    Dagger,
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct Request {
    mode: Mode,
    game: String,
    seed: u64,
    #[serde(default)]
    episodes: Option<u32>,
    #[serde(default)]
    tau: Option<f32>,
    #[serde(default)]
    max_pipes: Option<u32>,
    #[serde(default)]
    n: Option<u32>,
}

#[derive(Serialize)]
struct EvalReport<'a> {
    mode: Mode,
    game: &'a str,
    seed: u64,
    episodes: u32,
    metrics: Metrics,
}

/// A [`Policy`] backed by the decision model: it only sees the text state.
struct EnginePolicy {
    engine: Engine,
    questions: BTreeMap<(&'static str, &'static str), (QType, &'static str)>,
}

impl EnginePolicy {
    fn new(engine: Engine) -> Self {
        let mut questions = BTreeMap::new();
        for game in GAMES {
            let g = make_game(game, 0).expect("skycmd-arena: known game");
            for q in g.questions() {
                let qtype = match q.qtype {
                    sky_schema::QType::Choice => QType::Choice,
                    sky_schema::QType::Score => QType::Score,
                    sky_schema::QType::Noul => QType::Noul,
                };
                questions.insert((game, q.id), (qtype, q.instructions()));
            }
        }
        EnginePolicy { engine, questions }
    }
}

impl Policy for EnginePolicy {
    fn decide(
        &mut self,
        game: &str,
        question_id: &str,
        state_text: &str,
        opts: &[String],
    ) -> Vec<f32> {
        let (qtype, ins) = *self
            .questions
            .get(&(game, question_id))
            .unwrap_or_else(|| panic!("skycmd-arena: unknown question {game}/{question_id}"));
        self.engine
            .decide_raw(qtype, ins, opts, state_text)
            .unwrap_or_else(|e| panic!("skycmd-arena: decision failed: {e}"))
    }
}

fn argmax(p: &[f32]) -> usize {
    p.iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .map_or(0, |(i, _)| i)
}

fn load_engine(model: &[u8], tau: Option<f32>) -> Engine {
    let opts = EngineOptions {
        exit_threshold: tau,
        ..Default::default()
    };
    Engine::from_gguf_with(model, opts)
        .unwrap_or_else(|e| panic!("skycmd-arena: cannot load the model: {e}"))
}

fn dagger(policy: &mut EnginePolicy, game: &'static str, n: usize, seed: u64) -> Vec<u8> {
    let probe = make_game(game, 0).expect("skycmd-arena: known game");
    let questions = probe.questions();
    let rendered: Vec<Vec<String>> = questions.iter().map(|q| q.canonical_options()).collect();
    let mut actor = |g: &dyn DecisionGame, _rng: &mut SmallRng| {
        let text = g.state_text();
        let p = policy.decide(game, questions[0].id, &text, &rendered[0]);
        let abort = game == "dodge" && {
            let pa = policy.decide(game, questions[1].id, &text, &rendered[1]);
            pa[1] > pa[0]
        };
        (argmax(&p), abort)
    };
    let mut out = Vec::new();
    let mut failure = None;
    sky_games::r#gen::generate_with_actor(game, n, seed ^ 0xda66e7, Some(&mut actor), |r| {
        if failure.is_none() {
            match serde_json::to_writer(&mut out, &r) {
                Ok(()) => out.push(b'\n'),
                Err(e) => failure = Some(e),
            }
        }
    })
    .unwrap_or_else(|e| panic!("skycmd-arena: dagger failed: {e}"));
    if let Some(e) = failure {
        panic!("skycmd-arena: cannot encode a record: {e}");
    }
    out
}

fn run(input: &[u8]) -> Vec<u8> {
    let (assets, body) =
        nuee_task::split_assets(input).expect("skycmd-arena: malformed asset envelope");
    assert_eq!(assets.len(), 1, "skycmd-arena: expected exactly one asset (the GGUF model)");
    let req: Request = serde_json::from_slice(body)
        .unwrap_or_else(|e| panic!("skycmd-arena: invalid request JSON: {e}"));
    let game: &'static str = GAMES
        .iter()
        .copied()
        .find(|g| *g == req.game)
        .unwrap_or_else(|| panic!("skycmd-arena: unknown game {:?}", req.game));
    if let Some(t) = req.tau {
        assert!(t.is_finite(), "skycmd-arena: tau must be finite");
    }
    let model = assets[0];

    match req.mode {
        Mode::Eval | Mode::Oracle => {
            let episodes = req.episodes.expect("skycmd-arena: episodes is required");
            assert!(
                (1..=MAX_EPISODES).contains(&episodes),
                "skycmd-arena: episodes must be in 1..=200"
            );
            let opts = EvalOptions {
                flappy_max_pipes: req.max_pipes.unwrap_or(DEFAULT_MAX_PIPES),
                veto: true,
            };
            let metrics = if req.mode == Mode::Eval {
                let mut policy = EnginePolicy::new(load_engine(model, req.tau));
                evaluate_with(game, &mut policy, episodes as usize, req.seed, opts)
            } else {
                evaluate_with(
                    game,
                    &mut OraclePolicy::default(),
                    episodes as usize,
                    req.seed,
                    opts,
                )
            };
            let report = EvalReport {
                mode: req.mode,
                game,
                seed: req.seed,
                episodes,
                metrics,
            };
            serde_json::to_vec(&report)
                .unwrap_or_else(|e| panic!("skycmd-arena: cannot encode metrics: {e}"))
        }
        Mode::Dagger => {
            let n = req.n.expect("skycmd-arena: n is required in dagger mode");
            assert!(
                (1..=MAX_RECORDS).contains(&n),
                "skycmd-arena: n must be in 1..=20000"
            );
            let mut policy = EnginePolicy::new(load_engine(model, req.tau));
            dagger(&mut policy, game, n as usize, req.seed)
        }
    }
}

nuee_task::export!(run);
