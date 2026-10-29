mod common;

use common::MockCliRunner;
use gflow::hosting::github::GitHub;
use gflow::hosting::{HostingPlatform, PrBody};

// The provider adapters carry real policy on top of `gh`: when an existing
// PR is reused vs. a new one created, which CLI failures are normal and which
// are fatal, and the exact query flags that decide what the CLI returns. None of
// that was reachable while the subprocess call was hard-wired; it runs here
// against a scripted CLI. The subprocess itself (`SystemCli`) stays untested by
// design — tests never touch installed CLIs.

fn gh(runner: &MockCliRunner) -> GitHub<'_> {
    GitHub::new(None, runner)
}

// --- GitHub: create_or_get_pr ---

#[test]
fn an_open_pr_is_reused_instead_of_creating_a_second_one() {
    // "PR already open" is a normal resume outcome, not an error.
    let runner = MockCliRunner::scripted(&[Ok("https://github.com/o/r/pull/7")]);

    let url = gh(&runner).create_or_get_pr("feature/x", "develop", "feat: x", PrBody::NativeDefault).unwrap();

    assert_eq!(url, "https://github.com/o/r/pull/7");
    assert_eq!(runner.calls(), vec![
        "gh pr list --head feature/x --base develop --state open --limit 1 --json url --jq .[0].url // empty"
    ]);
}

#[test]
fn no_existing_pr_creates_one_with_an_empty_body() {
    // gh pr list exits 0 with an empty result when the branch has no open PR
    // at all — that is the normal first-finish path.
    let runner = MockCliRunner::scripted(&[
        Ok(""),
        Ok("https://github.com/o/r/pull/8"),
    ]);

    let url = gh(&runner).create_or_get_pr("feature/x", "develop", "feat: x", PrBody::NativeDefault).unwrap();

    assert_eq!(url, "https://github.com/o/r/pull/8");
    assert_eq!(runner.calls(), vec![
        "gh pr list --head feature/x --base develop --state open --limit 1 --json url --jq .[0].url // empty",
        "gh pr create --head feature/x --base develop --title feat: x --body ",
    ]);
}

#[test]
fn an_empty_body_pr_never_carries_a_body_file() {
    // Landing legs pass PrBody::Empty: gh must get an explicit empty --body
    // (no flag at all would drop gh into its interactive editor).
    let runner = MockCliRunner::scripted(&[Ok(""), Ok("https://github.com/o/r/pull/8")]);

    gh(&runner).create_or_get_pr("landing/hotfix-1.1.1-into-main", "main", "chore: merge hotfix 1.1.1 into main", PrBody::Empty).unwrap();

    assert_eq!(runner.calls()[1],
        "gh pr create --head landing/hotfix-1.1.1-into-main --base main --title chore: merge hotfix 1.1.1 into main --body ");
}

#[test]
fn a_real_gh_failure_is_fatal_and_names_the_auth_fix() {
    // Every probe failure is fatal now that gh pr list exits 0 for "no PRs" —
    // there is nothing left to swallow. An expired token must not silently
    // become "create a new PR".
    let runner = MockCliRunner::scripted(&[Err("gh pr list failed: HTTP 401: Bad credentials")]);

    let err = gh(&runner).create_or_get_pr("feature/x", "develop", "feat: x", PrBody::NativeDefault).unwrap_err();

    assert!(err.contains("gh auth login"), "must name the next command; got: {err}");
    assert_eq!(runner.calls().len(), 1, "nothing may be created after a real failure");
}

#[test]
fn a_resolved_template_is_passed_as_a_body_file() {
    let runner = MockCliRunner::scripted(&[Ok(""), Ok("https://github.com/o/r/pull/10")]);

    gh(&runner).create_or_get_pr("feature/x", "develop", "feat: x", PrBody::File(".github/pr-templates/gflow-feature.md")).unwrap();

    assert_eq!(runner.calls()[1],
        "gh pr create --head feature/x --base develop --title feat: x --body-file .github/pr-templates/gflow-feature.md");
}

#[test]
fn an_open_pr_to_a_different_base_is_not_reused() {
    // A fan-out source branch (a protected hotfix landing on both main and a
    // release branch) can have an open PR to one base while this call targets
    // another. The probe must be base-filtered so it returns empty here, not
    // the branch's other open PR.
    let runner = MockCliRunner::scripted(&[Ok(""), Ok("https://github.com/o/r/pull/11")]);

    let url = gh(&runner)
        .create_or_get_pr("hotfix/1.2.4", "release/1.2.0", "chore: merge hotfix 1.2.4 into release/1.2.0", PrBody::NativeDefault)
        .unwrap();

    assert_eq!(url, "https://github.com/o/r/pull/11");
    assert_eq!(runner.calls()[0],
        "gh pr list --head hotfix/1.2.4 --base release/1.2.0 --state open --limit 1 --json url --jq .[0].url // empty");
}

#[test]
fn gh_pins_every_call_to_the_origin_repository() {
    // In a fork, gh prefers an `upstream` remote over origin. Without --repo,
    // gflow would look up and open PRs in the parent repository.
    let runner = MockCliRunner::scripted(&[Ok(""), Ok("https://github.com/acme/repo/pull/8"), Ok(""), Ok(""), Ok(""), Ok("")]);
    let gh = GitHub::new(Some("github.com/acme/repo".to_string()), &runner);

    gh.create_or_get_pr("feature/x", "develop", "feat: x", PrBody::NativeDefault).unwrap();
    gh.merged_pr("feature/x").unwrap();
    gh.merged_pr_to("feature/x", "develop").unwrap();
    gh.open_pr_to("feature/x", "develop").unwrap();

    let calls = runner.calls();
    assert_eq!(calls.len(), 5);
    for call in calls {
        assert!(call.ends_with(" --repo github.com/acme/repo"), "got: {call}");
    }
}

// --- GitHub: merged_pr ---

#[test]
fn merged_pr_asks_only_for_the_newest_pr_of_the_branch() {
    let runner = MockCliRunner::scripted(&[Ok("https://github.com/o/r/pull/49\tabc123\tdeadbeef\tdevelop")]);

    let pr = gh(&runner).merged_pr("feature/x").unwrap().unwrap();

    assert_eq!(pr.head_sha, "abc123");
    assert_eq!(pr.merge_commit_sha, "deadbeef");
    assert_eq!(pr.base, "develop");
    let call = &runner.calls()[0];
    assert!(call.contains("--state all"), "closed PRs must be visible too; got: {call}");
    assert!(call.contains("--limit 1"), "only the newest PR decides; got: {call}");
    assert!(call.contains("mergeCommit"), "the completion-type check needs the merge commit; got: {call}");
}

#[test]
fn a_merged_pr_lookup_failure_names_the_auth_fix() {
    let runner = MockCliRunner::scripted(&[Err("gh pr list failed: HTTP 401")]);

    let err = gh(&runner).merged_pr("feature/x").unwrap_err();

    assert!(err.contains("gh auth login"), "got: {err}");
}

#[test]
fn merged_pr_to_filters_by_exact_head_and_base() {
    let runner = MockCliRunner::scripted(&[Ok("https://github.com/o/r/pull/49\tabc123\tdeadbeef")]);

    let pr = gh(&runner).merged_pr_to("feature/x", "develop").unwrap().unwrap();

    assert_eq!(pr.head_sha, "abc123");
    assert_eq!(pr.merge_commit_sha, "deadbeef");
    // `--state merged`, not `all`: this answers "has this leg landed", which a
    // newer open or abandoned PR must not erase. `merged_pr` above deliberately
    // keeps `all` — for a work branch, a newer PR means the branch is in play.
    assert_eq!(
        runner.calls()[0],
        r#"gh pr list --head feature/x --base develop --state merged --limit 1 --json url,state,headRefOid,mergeCommit --jq .[0] | select(.state == "MERGED") | [.url, .headRefOid, .mergeCommit.oid] | @tsv"#
    );
}

#[test]
fn a_merged_pr_to_lookup_failure_names_the_auth_fix() {
    let runner = MockCliRunner::scripted(&[Err("gh pr list failed: HTTP 401")]);

    let err = gh(&runner).merged_pr_to("feature/x", "develop").unwrap_err();

    assert!(err.contains("gh auth login"), "got: {err}");
}

// --- open_pr_to (legacy-PR detection for landing-branch migration) ---

#[test]
fn gh_open_pr_to_filters_by_exact_head_base_and_open_state() {
    let runner = MockCliRunner::scripted(&[Ok("https://github.com/o/r/pull/61")]);

    let url = gh(&runner).open_pr_to("release/1.2.0", "develop").unwrap().unwrap();

    assert_eq!(url, "https://github.com/o/r/pull/61");
    assert_eq!(
        runner.calls()[0],
        "gh pr list --head release/1.2.0 --base develop --state open --limit 1 --json url --jq .[0].url // empty"
    );
}

#[test]
fn gh_open_pr_to_empty_result_is_none() {
    let runner = MockCliRunner::scripted(&[Ok("")]);
    assert_eq!(gh(&runner).open_pr_to("release/1.2.0", "develop").unwrap(), None);
}

#[test]
fn github_open_pr_failure_keeps_the_authentication_remedy() {
    let runner = MockCliRunner::scripted(&[Err("HTTP 401")]);
    let error = GitHub::new(None, &runner)
        .open_pr_to("release/1.0.0", "main")
        .unwrap_err();
    assert!(error.contains("Could not check for an open PR: HTTP 401"));
    assert!(error.contains("gh auth login"));
}

// --- Completing gflow's own version PRs ---

#[test]
fn gh_auto_complete_squashes_and_deletes_the_branch_once_checks_pass() {
    let runner = MockCliRunner::scripted(&[Ok("")]);

    gh(&runner).enable_auto_complete("https://github.com/o/r/pull/7").unwrap();

    assert_eq!(runner.calls(), vec!["gh pr merge https://github.com/o/r/pull/7 --auto --squash --delete-branch"]);
}

#[test]
fn gh_merge_now_squashes_and_deletes_the_branch() {
    let runner = MockCliRunner::scripted(&[Ok("")]);

    gh(&runner).merge_pr("https://github.com/o/r/pull/7").unwrap();

    assert_eq!(runner.calls(), vec!["gh pr merge https://github.com/o/r/pull/7 --squash --delete-branch"]);
}

#[test]
fn gh_completion_failures_reach_the_caller() {
    let runner = MockCliRunner::scripted(&[Err("auto-merge is not allowed"), Err("review required")]);

    assert_eq!(gh(&runner).enable_auto_complete("https://github.com/o/r/pull/7"), Err("auto-merge is not allowed".to_string()));
    assert_eq!(gh(&runner).merge_pr("https://github.com/o/r/pull/7"), Err("review required".to_string()));
}

// --- Completing landing PRs (merge commit, landing branch kept) ---

#[test]
fn gh_lands_a_landing_pr_with_a_merge_commit_and_keeps_its_branch() {
    let runner = MockCliRunner::scripted(&[Ok(""), Ok("")]);

    gh(&runner).merge_landing_pr("https://github.com/o/r/pull/9").unwrap();
    gh(&runner).enable_landing_auto_complete("https://github.com/o/r/pull/9").unwrap();

    assert_eq!(runner.calls(), vec![
        "gh pr merge https://github.com/o/r/pull/9 --merge",
        "gh pr merge https://github.com/o/r/pull/9 --auto --merge",
    ]);
}

