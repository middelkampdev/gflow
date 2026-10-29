mod common;

use common::MockGit;
use gflow::hosting::detect::origin_repo;

#[test]
fn a_github_origin_names_the_repository_gh_must_use() {
    let git = MockGit::new();

    assert_eq!(origin_repo(&git).as_deref(), Some("github.com/acme/repo"));
    assert_eq!(git.calls(), vec!["remote_url"]);
}

#[test]
fn a_repo_without_an_origin_remote_lets_gh_pick_the_repository() {
    // `remote_url` errors when there is no origin (a fresh `git init`); that
    // must not become a hard failure.
    let mut git = MockGit::new();
    git.fail_remote_url = true;

    assert_eq!(origin_repo(&git), None);
}
