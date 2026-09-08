//! The debug run that lets the PR gate see an overflow guards itself.
//!
//! `overflow-checks` is on in debug and off in release, so a defect
//! whose only symptom is an arithmetic overflow panic cannot be
//! observed by a release-only test run. This repository already runs a
//! debug suite -- `release.yml:59`, `cargo test --locked --all-targets`
//! -- and that is NOT the same fact as the pull-request gate being able
//! to see the defect: `release.yml` triggers on a version tag, after
//! the change has already merged. A wrapping bug merges green here and
//! surfaces only when someone else cuts the next release, detached from
//! the change and the person who could have caught it.
//!
//! So `ci.yml` -- the workflow that actually gates a merge -- needs its
//! own debug run, and this file is what keeps it there. It checks
//! `ci.yml` specifically and is not satisfied by `release.yml` having
//! one; see `a_debug_run_in_release_yml_alone_does_not_satisfy_the_gate`
//! for that distinction pinned as a test rather than left as a comment
//! someone could stop believing.
//!
//! # Why this is an integration test and not a module under `src/`
//!
//! Cargo discovers `tests/*.rs` on its own, so there is no declaration
//! anywhere that can be deleted to switch this off. A guard living as a
//! file under `src/` behind a `#[cfg(test)] mod` line has no such
//! protection: lose the one line and the file stays, compiles into
//! nothing, and asserts nothing, with no lint to say so. That happened
//! once already on a sibling repository's version of this fix.
//!
//! # The other half: does the debug run actually ask anything
//!
//! `ci.yml` quoting a debug command inside the comment explaining it
//! (see the comment above the step this file is pinning) means a scan
//! that ignored comments would keep passing after the step itself was
//! deleted. And a debug step that compiles but never checks anything is
//! costing a compile for nothing, so the scan also requires the
//! `EXPECT_OVERFLOW_CHECKS` handshake that arms
//! `overflow_checks::the_build_the_gate_asked_to_check_does_check` in
//! `src/lib.rs` -- the runtime half that actually performs an overflow
//! and fails if the build let it through. This file proves the step is
//! present and asked to check; it cannot prove the build can see the
//! overflow, which is what the runtime test is for. Neither is
//! redundant with the other: delete the step and the runtime test never
//! runs at all; keep the step but drop the variable and the runtime
//! test runs, finds nothing to check, and passes doing nothing.

use std::path::{Path, PathBuf};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Read a file the guard depends on, or fail.
///
/// Panics rather than returning `None` on purpose: a version of this
/// that skipped when the file was missing would reproduce the exact
/// blindness the guard exists to prevent.
fn read_or_panic(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e}. This guard must fail rather than skip: a \
             version of it that returned early here would be the same \
             blindness it exists to prevent.",
            path.display()
        )
    })
}

/// Every `cargo test` invocation in a workflow that would be compiled
/// with overflow checks on AND asked to check for one.
///
/// Three things disqualify a line:
///
/// - it is a YAML comment. Load-bearing here, not defensive: `ci.yml`
///   quotes the debug command verbatim in the comment above the step
///   that runs it;
/// - it is an inline trailing comment on an otherwise-`--release` line;
/// - it passes `--release`, or names a profile explicitly.
///
/// And the run must carry `EXPECT_OVERFLOW_CHECKS=1` -- a debug step
/// that never asks the build anything buys nothing over deleting it.
fn checking_debug_runs(workflow: &str) -> Vec<String> {
    workflow
        .lines()
        .filter_map(|raw| {
            let line = raw.trim_start();
            if line.starts_with('#') {
                return None;
            }
            let command = line.split(" #").next().unwrap_or(line).trim();
            if !command.contains("cargo test") {
                return None;
            }
            if command.contains("--release") || command.contains("--profile") {
                return None;
            }
            if !command.contains("EXPECT_OVERFLOW_CHECKS=1") {
                return None;
            }
            Some(command.to_string())
        })
        .collect()
}

/// The guard. Reads `ci.yml` -- the workflow that gates a pull request
/// -- and refuses if nothing there compiles the overflow checks and
/// asks the build to prove it.
///
/// `ci.yml` specifically, not `release.yml`. `release.yml` already has
/// a debug run and always has; it does not run on a pull request, so
/// its presence says nothing about whether a merge was gated by it.
#[test]
fn the_pr_gate_still_tests_in_a_profile_that_can_see_an_overflow() {
    let path = manifest_dir()
        .join(".github")
        .join("workflows")
        .join("ci.yml");
    let workflow = read_or_panic(&path);

    let debug_runs = checking_debug_runs(&workflow);
    assert!(
        !debug_runs.is_empty(),
        "no `cargo test` in {} runs without `--release` while setting \
         EXPECT_OVERFLOW_CHECKS=1, so a defect whose only symptom is an \
         arithmetic overflow panic can merge without the PR gate ever \
         seeing it. release.yml already runs a debug suite, and that \
         does not help: it triggers on a version tag, after the change \
         has merged. If the debug step in ci.yml looked redundant beside \
         the release one, it is not -- see the comment above it.",
        path.display()
    );
}

/// THE DISTINCTION THIS REPOSITORY NEEDS THAT A PORTED COPY WOULD MISS.
///
/// A workflow carrying a checking debug run under a name other than
/// `ci.yml` -- `release.yml`, in this repository's own case -- must not
/// satisfy the guard. Simulated here with `release.yml`'s actual step
/// shape: a plain `cargo test --locked --all-targets` with no
/// `EXPECT_OVERFLOW_CHECKS`, because that workflow was never asked to
/// carry the handshake and does not need to -- it already runs in
/// debug, unconditionally, so nothing there was ever blind. The
/// scenario worth pinning is the near miss: even a hypothetical debug
/// run in `release.yml` that DID set the handshake would not make
/// `ci.yml`'s own absence of one acceptable, because `release.yml`
/// triggers too late to gate a merge.
#[test]
fn a_checking_debug_run_that_is_not_in_ci_yml_does_not_satisfy_this_guard() {
    let release_yml_shape = "\
jobs:
  release:
    steps:
      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --all-targets
";
    // This function only ever reads ci.yml in the real guard above; this
    // test pins that the PARSER itself would still count such a line if
    // handed the wrong file, so the guard's safety is coming from WHICH
    // FILE it opens -- a fact worth being explicit about, since a future
    // edit that widened the scan to every workflow would silently stop
    // catching this repository's actual defect.
    assert_eq!(
        checking_debug_runs(release_yml_shape),
        vec!["- run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --all-targets".to_string()],
        "the parser itself would count this line -- the guard's correctness \
         depends on scanning ci.yml and ci.yml alone, not on the parser \
         refusing this shape"
    );
}

/// The parser is the part of this that can rot, checked against each
/// shape it has to tell apart.
mod parser {
    use super::checking_debug_runs;

    /// The trap this repository's own `ci.yml` contains: the debug
    /// command quoted verbatim in the comment explaining the step.
    #[test]
    fn a_debug_run_quoted_in_a_comment_does_not_count() {
        let yaml = "\
jobs:
  test:
    steps:
      # Measured on this branch:
      #     EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib   ->  EXIT=101
      - run: cargo test --locked --release
";
        assert_eq!(
            checking_debug_runs(yaml),
            Vec::<String>::new(),
            "a debug command quoted inside a comment is documentation, not a run"
        );
    }

    #[test]
    fn a_real_checking_debug_run_counts() {
        let yaml = "\
jobs:
  test:
    steps:
      - run: cargo test --locked --release
      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib
";
        assert_eq!(
            checking_debug_runs(yaml),
            vec!["- run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib".to_string()],
        );
    }

    /// A debug step present but never asked to check anything buys
    /// nothing over deleting it -- the whole point of the handshake.
    #[test]
    fn a_debug_run_without_the_handshake_does_not_count() {
        let yaml = "      - run: cargo test --locked --lib\n";
        assert_eq!(
            checking_debug_runs(yaml),
            Vec::<String>::new(),
            "the step runs but nothing checks the build it produced"
        );
    }

    /// A handshake on a release run proves nothing: the checks are
    /// legitimately off there.
    #[test]
    fn the_handshake_on_a_release_run_does_not_count() {
        let yaml = "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --release\n";
        assert_eq!(checking_debug_runs(yaml), Vec::<String>::new());
    }

    /// An inline trailing comment naming `--release` must not disqualify
    /// a genuine debug run.
    #[test]
    fn a_trailing_comment_naming_release_does_not_disqualify_a_debug_run() {
        let yaml =
            "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib  # deliberately not --release\n";
        assert_eq!(
            checking_debug_runs(yaml),
            vec!["- run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib".to_string()],
        );
    }

    /// A profile named another way still disqualifies the run.
    #[test]
    fn a_profile_flag_disqualifies_a_run_even_with_the_handshake() {
        let yaml =
            "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --profile release-with-debug --lib\n";
        assert_eq!(checking_debug_runs(yaml), Vec::<String>::new());
    }
}
