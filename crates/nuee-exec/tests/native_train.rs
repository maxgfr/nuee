//! Placeholder substitution of DiLoCo exec trainers.

use nuee_exec::TrainArgs;
use nuee_exec::native::substitute_argv;

#[test]
fn trainer_placeholders_are_substituted_whole() {
    let t = TrainArgs {
        steps: 50,
        shard: 2,
        n_shards: 4,
        seed: 1234,
    };
    let argv = [
        "python3",
        "train.py",
        "--init",
        "$NUEE_INIT",
        "--steps",
        "$NUEE_STEPS",
        "--shard",
        "$NUEE_SHARD",
        "--seed",
        "$NUEE_SEED",
        "--out",
        "$NUEE_OUT",
        "x$NUEE_SEED",
    ];
    let out = substitute_argv(&argv, "/t/in", "/t/out", Some(&t));
    assert_eq!(
        out,
        [
            "python3",
            "train.py",
            "--init",
            "/t/in",
            "--steps",
            "50",
            "--shard",
            "2/4",
            "--seed",
            "1234",
            "--out",
            "/t/out",
            "x$NUEE_SEED"
        ]
    );
    // Outside training tasks only $NUEE_IN and $NUEE_OUT are placeholders.
    let plain = substitute_argv(&["$NUEE_IN", "$NUEE_STEPS", "$NUEE_OUT"], "i", "o", None);
    assert_eq!(plain, ["i", "$NUEE_STEPS", "o"]);
}
