mod common;

use common::MockGit;
use gflow::flows::deploy_version::{deploy_version, DeployPart};

fn first_parent(git: &mut MockGit, rev: &str, commits: &[&str]) {
    git.first_parents.insert(rev.to_string(), commits.iter().map(|c| c.to_string()).collect());
}

#[test]
fn develop_without_tags_is_0_1_0_counting_every_commit() {
    let mut git = MockGit::new();
    first_parent(&mut git, "HEAD", &["c2", "c1", "c0"]);

    assert_eq!(deploy_version(&git, "develop", "main", DeployPart::Full).unwrap(), "0.1.0+3");
}

#[test]
fn develop_is_the_next_minor_counted_from_the_cut_of_the_highest_tags_line() {
    // Release 0.2 was cut from d1: its first tag sits on the release's first
    // commit, whose parent is the cut. A hotfix tag of an older line counts
    // as the highest only when no newer line exists.
    for tags in [
        vec!["v0.1.0", "v0.1.1", "v0.2.0-rc.1", "v0.2.0-rc.2"],
        vec!["v0.1.0", "v0.2.0", "v0.2.1"],
    ] {
        let mut git = MockGit::new();
        git.tags = tags.iter().map(|t| t.to_string()).collect();
        first_parent(&mut git, "HEAD", &["d3", "d2", "d1", "d0"]);
        first_parent(&mut git, "v0.2.0-rc.1", &["r1", "d1", "d0"]);
        first_parent(&mut git, "v0.2.0", &["r1", "d1", "d0"]);

        assert_eq!(deploy_version(&git, "develop", "main", DeployPart::Full).unwrap(), "0.3.0+3", "{tags:?}");
    }
}

#[test]
fn a_tag_on_the_root_commit_counts_every_develop_commit() {
    let mut git = MockGit::new();
    git.tags = vec!["v0.1.0".to_string()];
    first_parent(&mut git, "HEAD", &["d1", "d0"]);
    first_parent(&mut git, "v0.1.0", &["d0"]);

    assert_eq!(deploy_version(&git, "develop", "main", DeployPart::Full).unwrap(), "0.2.0+3");
}

#[test]
fn a_work_branch_counts_like_the_branch_it_lands_on_on_its_own_commits() {
    let mut git = MockGit::new();
    git.tags = vec!["v0.1.0".to_string()];
    first_parent(&mut git, "HEAD", &["f1", "d1", "d0"]);
    first_parent(&mut git, "v0.1.0", &["r1", "d0"]);

    assert_eq!(deploy_version(&git, "feature/login", "main", DeployPart::Full).unwrap(), "0.2.0+3");
}

#[test]
fn a_release_is_its_latest_line_tag_counting_its_own_commits() {
    for (branch, tags, expected) in [
        ("release/0.2.0", vec!["v0.1.0", "v0.2.0-rc.1", "v0.2.0-rc.2"], "0.2.0-rc.2+2"),
        ("release/0.2.0", vec!["v0.1.0", "v0.2.0", "v0.2.1"], "0.2.1+2"),
        ("release/0.2.0", vec!["v0.1.0"], "0.2.0+2"),
        ("release-fix/0.2.0/crash", vec!["v0.2.0-rc.1"], "0.2.0-rc.1+2"),
    ] {
        let mut git = MockGit::new();
        git.tags_on_branch = tags.iter().map(|t| t.to_string()).collect();
        git.existing_remote_branches.insert("develop".to_string());
        first_parent(&mut git, "HEAD", &["r2", "r1", "d1", "d0"]);
        first_parent(&mut git, "origin/develop", &["d2", "d1", "d0"]);

        assert_eq!(deploy_version(&git, branch, "main", DeployPart::Full).unwrap(), expected, "{branch} {tags:?}");
    }
}

#[test]
fn a_hotfix_is_its_own_version_counting_its_commits_past_the_mainline() {
    for branch in ["hotfix/0.1.3", "hotfix-fix/0.1.3/security"] {
        let mut git = MockGit::new();
        first_parent(&mut git, "HEAD", &["h1", "m1", "m0"]);
        first_parent(&mut git, "main", &["m1", "m0"]);

        assert_eq!(deploy_version(&git, branch, "main", DeployPart::Full).unwrap(), "0.1.3+1", "{branch}");
    }
}

#[test]
fn name_and_build_print_one_part() {
    let mut git = MockGit::new();
    first_parent(&mut git, "HEAD", &["c1", "c0"]);

    assert_eq!(deploy_version(&git, "develop", "main", DeployPart::Name).unwrap(), "0.1.0");
    assert_eq!(deploy_version(&git, "develop", "main", DeployPart::Build).unwrap(), "2");
}

#[test]
fn it_refuses_branches_that_never_build_a_deploy() {
    for branch in ["main", "master", "version/0.2.0-into-release-0.2.0", "landing/release-0.2.0-into-main", "wip"] {
        let error = deploy_version(&MockGit::new(), branch, "main", DeployPart::Full).unwrap_err();

        assert_eq!(error, format!(
            "gflow deploy-version runs on develop, release, and hotfix branches, and on the work branches that land on them, not on {branch}."
        ));
    }
}

#[test]
fn it_stops_when_git_cannot_answer() {
    for (branch, call) in [
        ("develop", "list_tags"),
        ("develop", "first_parent_commits:HEAD"),
        ("release/0.2.0", "tags_on_branch:HEAD"),
        ("release/0.2.0", "remote_branch_exists:develop"),
        ("release/0.2.0", "first_parent_commits:HEAD"),
        ("hotfix/0.1.3", "first_parent_commits:main"),
    ] {
        let mut git = MockGit::new();
        git.fail_call = Some((call.to_string(), 1));

        assert_eq!(deploy_version(&git, branch, "main", DeployPart::Full), Err(format!("injected git failure: {call}")), "{call}");
    }
    let mut git = MockGit::new();
    git.tags = vec!["v0.1.0".to_string()];
    git.fail_call = Some(("first_parent_commits:v0.1.0".to_string(), 1));
    assert_eq!(deploy_version(&git, "develop", "main", DeployPart::Full), Err("injected git failure: first_parent_commits:v0.1.0".to_string()));
}

#[test]
fn the_flags_choose_the_part() {
    assert_eq!(DeployPart::from_flags(false, false), DeployPart::Full);
    assert_eq!(DeployPart::from_flags(true, false), DeployPart::Name);
    assert_eq!(DeployPart::from_flags(false, true), DeployPart::Build);
}
