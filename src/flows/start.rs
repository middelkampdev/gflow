use crate::flows::finish_release::cut_release_tag;
use crate::flows::{complete_version_pr, open_version_pr, open_versioned_branches, require_clean_tree, run_version_script, VersionPrCompletion};
use crate::git::Git;
use crate::hooks::TagEvent;
use crate::hosting::HostingPlatform;
use crate::prompt::Prompter;
use crate::repo_config::{BumpStrategy, Mode, RepoConfig};
use crate::version::SemVer;
use crate::version_script::VersionScript;
use crate::worktree::{open_worktree, WorktreeContext};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReleaseType {
    Major,
    Minor,
}

/// An active worktree context leaves the current checkout untouched, exactly
/// like `--no-checkout`. This is the flows' single derivation of that rule
/// (pinned by the worktree tests: passing no_checkout=false with a worktree
/// context must still take the no-checkout path).
fn effective_no_checkout(no_checkout: bool, worktree: &Option<WorktreeContext<'_>>) -> bool {
    no_checkout || worktree.is_some()
}

fn materialize_branch(
    git: &dyn Git,
    branch: &str,
    base: &str,
    no_checkout: bool,
    worktree: Option<WorktreeContext<'_>>,
) -> Result<(), String> {
    println!("Creating branch: {branch}");
    let start_point = fresh_start_point(git, base)?;
    if no_checkout {
        git.create_branch_no_checkout(branch, &start_point)?;
    } else {
        git.create_branch(branch, &start_point)?;
    }
    git.push(branch)?;
    println!("✔ Branch '{branch}' created and pushed.");
    if let Some(ctx) = worktree {
        open_worktree(git, &ctx, branch)?;
    }
    Ok(())
}

/// The fetch moves only `origin/<base>`, so a local base is usually stale —
/// and a remote-only base has no local name `git branch` can resolve. The
/// local base wins only when it has commits origin does not have yet.
fn fresh_start_point(git: &dyn Git, base: &str) -> Result<String, String> {
    let remote = format!("origin/{base}");
    if !git.remote_branch_exists(base)? {
        return Ok(base.to_string());
    }
    if git.local_branch_exists(base)? && !git.is_ancestor(base, &remote)? {
        return Ok(base.to_string());
    }
    Ok(remote)
}

/// The container branch HEAD is on, when it carries `prefix`. Standing on a
/// release or hotfix is an explicit choice, so it beats discovery — which is
/// only ever a fallback for running the command from somewhere else.
fn standing_on(git: &dyn Git, prefix: &str) -> Result<Option<String>, String> {
    let current = git.current_branch()?;
    Ok(current.starts_with(prefix).then_some(current))
}

/// A parent with no parseable version can only yield a fix branch that
/// `BranchType::parse` reads back as `Other` — creatable, never finishable.
fn version_of(branch: &str, prefix: &str) -> Result<SemVer, String> {
    branch.strip_prefix(prefix)
        .and_then(SemVer::parse)
        .ok_or_else(|| format!("Branch '{branch}' does not carry a version; cannot derive a fix branch from it."))
}

pub fn start_work_branch(git: &dyn Git, prefix: &str, name: &str, from: &str, no_checkout: bool, worktree: Option<WorktreeContext<'_>>) -> Result<(), String> {
    let branch = format!("{prefix}/{name}");
    let effective_no_checkout = effective_no_checkout(no_checkout, &worktree);
    // `from` is user-supplied, so git's "not a commit" becomes guidance naming --base.
    materialize_branch(git, &branch, from, effective_no_checkout, worktree).map_err(|e| {
        if e.contains("not a commit") {
            format!("Branch '{from}' does not exist. Use --base to specify a different base branch.")
        } else {
            e
        }
    })
}

/// With a worktree context the release is still created in the current tree
/// (the version script needs the branch checked out), then the tree returns to
/// develop and the release opens in its own worktree — or, when a worktree
/// already holds it, that one is announced.
/// Reports `release-start` only when this run cut the release's first tag;
/// reusing an open release cuts none.
pub fn start_release(git: &dyn Git, prompter: &dyn Prompter, hosting: &dyn HostingPlatform, script: Option<&dyn VersionScript>, cfg: &RepoConfig, release_type: Option<ReleaseType>, main_branch: &str, worktree: Option<WorktreeContext<'_>>) -> Result<Option<TagEvent>, String> {
    let (branch, started) = resolve_or_create_release(git, prompter, hosting, script, cfg, release_type, main_branch, worktree.is_some())?;
    if let Some(ctx) = worktree {
        open_container_worktree(git, &ctx, "Release", &branch)?;
    }
    Ok(started)
}

pub fn start_release_fix(git: &dyn Git, hosting: &dyn HostingPlatform, cfg: &RepoConfig, main_branch: &str, name: &str, no_checkout: bool, worktree: Option<WorktreeContext<'_>>) -> Result<(), String> {
    let effective_no_checkout = effective_no_checkout(no_checkout, &worktree);
    let release_branch = match standing_on(git, "release/")? {
        Some(current) => current,
        None if effective_no_checkout => open_versioned_branches(git, hosting, cfg, main_branch, "release")?
            .first()
            .ok_or("No release branch found. Create one with 'gflow start release' first.")?
            .clone(),
        None => return Err("Not on a release branch".to_string()),
    };

    let branch = version_of(&release_branch, "release/")?.release_fix_branch(name);
    materialize_branch(git, &branch, &release_branch, effective_no_checkout, worktree)
}

/// In worktree mode the hotfix container gets its own worktree too (opened
/// before the fix branch's, which then keeps editor focus): `start hotfix-fix`
/// is the only command that creates a hotfix container, so no later command
/// would hand it one — and `gflow finish` needs it checked out somewhere.
pub fn start_hotfix_fix(git: &dyn Git, hosting: &dyn HostingPlatform, cfg: &RepoConfig, name: &str, no_checkout: bool, worktree: Option<WorktreeContext<'_>>, main_branch: &str, script: Option<&dyn VersionScript>) -> Result<(), String> {
    let effective_no_checkout = effective_no_checkout(no_checkout, &worktree);
    let hotfix_branch = match standing_on(git, "hotfix/")? {
        Some(current) => current,
        None => resolve_or_create_hotfix(git, hosting, cfg, effective_no_checkout, main_branch, script)?,
    };
    let branch = version_of(&hotfix_branch, "hotfix/")?.hotfix_fix_branch(name);
    if let Some(ctx) = &worktree {
        open_container_worktree(git, ctx, "Hotfix", &hotfix_branch)?;
    }
    materialize_branch(git, &branch, &hotfix_branch, effective_no_checkout, worktree)
}

/// A release/hotfix container branch handed to worktree mode: open it in its
/// own worktree, or announce the worktree that already holds it.
fn open_container_worktree(git: &dyn Git, ctx: &WorktreeContext<'_>, label: &str, branch: &str) -> Result<(), String> {
    match git.worktree_of(branch)? {
        Some(path) => {
            println!("{label} branch {branch} is already open at {}", path.display());
            Ok(())
        }
        None => open_worktree(git, ctx, branch),
    }
}

/// `hand_off`: the caller will move the release into a worktree, so the current
/// tree must not be left on it — no checkout of an existing release, and a
/// return to develop after creating a new one.
fn resolve_or_create_release(git: &dyn Git, prompter: &dyn Prompter, hosting: &dyn HostingPlatform, script: Option<&dyn VersionScript>, cfg: &RepoConfig, release_type: Option<ReleaseType>, main_branch: &str, hand_off: bool) -> Result<(String, Option<TagEvent>), String> {
    let release_branches = open_versioned_branches(git, hosting, cfg, main_branch, "release")?;

    if let Some(branch) = release_branches.first() {
        println!("Using existing release branch: {branch}");
        if !hand_off {
            git.checkout(branch)?;
        }
        return Ok((branch.to_string(), None));
    }

    let latest = find_latest_tag(git)?;
    let next = match release_type {
        Some(ReleaseType::Major) => latest.bump_major(),
        Some(ReleaseType::Minor) => latest.bump_minor(),
        None => {
            let has_breaking = detect_breaking_changes(git, &latest);
            prompt_release_type(prompter, &latest, has_breaking)?
        }
    };

    let branch = next.release_branch();

    if script.is_some() {
        require_clean_tree(git)?;
    }
    println!("Creating release branch: {branch}");
    git.checkout("develop")?;
    git.create_branch(&branch, &fresh_start_point(git, "develop")?)?;
    git.push(&branch)?;
    // The cut is the release's first version moment: the same one a bump runs.
    let started = cut_release_tag(git, hosting, script, cfg, &next)?;
    if hand_off {
        git.checkout("develop")?;
    }

    Ok((branch, started))
}

pub fn detect_breaking_changes(git: &dyn Git, latest: &SemVer) -> bool {
    let tag = latest.tag_name();
    // Scan develop explicitly — start release may run from any branch
    let messages = match git.commit_messages(&tag, "develop") {
        Ok(msgs) => msgs,
        Err(_) => match git.commit_messages(&tag, "origin/develop") {
            Ok(msgs) => msgs,
            Err(_) => return false,
        },
    };

    for msg in &messages {
        if message_is_breaking(msg) {
            return true;
        }
    }

    false
}

pub(crate) fn message_is_breaking(msg: &str) -> bool {
    let mut lines = msg.lines();
    let first_line = lines.next().unwrap_or("");

    // Conventional commits: type or type(scope) followed by "!:" e.g. "feat!:", "refactor(auth)!:"
    if let Some(colon_pos) = first_line.find(':') {
        let before_colon = &first_line[..colon_pos];
        if before_colon.ends_with('!') {
            return true;
        }
    }

    // Conventional commits footer: a line starting with "BREAKING CHANGE:" or "BREAKING-CHANGE:"
    // (case-insensitive, followed by a colon and space per the spec)
    for line in lines {
        let trimmed = line.trim_start();
        let upper = trimmed.to_uppercase();
        if upper.starts_with("BREAKING CHANGE:") || upper.starts_with("BREAKING-CHANGE:") {
            return true;
        }
    }

    false
}

fn prompt_release_type(prompter: &dyn Prompter, latest: &SemVer, has_breaking: bool) -> Result<SemVer, String> {
    let major_label = format!("major (v{} → v{})", latest, latest.bump_major());
    let minor_label = format!("minor (v{} → v{})", latest, latest.bump_minor());

    let items: Vec<&str> = if has_breaking {
        println!("Breaking changes detected since last release.");
        vec![&major_label, &minor_label]
    } else {
        vec![&minor_label, &major_label]
    };

    let idx = prompter.select("Release type", &items)?;
    let selected = items[idx];

    if selected.starts_with("major") {
        Ok(latest.bump_major())
    } else {
        Ok(latest.bump_minor())
    }
}

fn resolve_or_create_hotfix(git: &dyn Git, hosting: &dyn HostingPlatform, cfg: &RepoConfig, no_checkout: bool, main_branch: &str, script: Option<&dyn VersionScript>) -> Result<String, String> {
    let hotfix_branches = open_versioned_branches(git, hosting, cfg, main_branch, "hotfix")?;

    if let Some(branch) = hotfix_branches.first() {
        println!("Using existing hotfix branch: {branch}");
        if !no_checkout {
            git.checkout(branch)?;
        }
        return Ok(branch.clone());
    }

    let latest = match cfg.bump_strategy {
        BumpStrategy::Rc => find_latest_tag(git)?,
        BumpStrategy::Patch => find_latest_shipped_tag(git, hosting, cfg, main_branch)?,
    };
    let next = latest.bump_patch();
    let branch = next.hotfix_branch();

    if !no_checkout && script.is_some() {
        require_clean_tree(git)?;
    }
    println!("Creating hotfix branch: {branch}");
    let start_point = fresh_start_point(git, main_branch)?;
    if no_checkout {
        git.create_branch_no_checkout(&branch, &start_point)?;
        if let Some(script) = script {
            eprintln!(
                "⚠ Version script not run: {branch} was created without checkout, so gflow cannot commit version files there."
            );
            eprintln!(
                "  Recover manually: git switch {branch}, run {} {next}, commit, and push.",
                script.display_name(),
            );
        }
    } else {
        git.checkout(main_branch)?;
        git.create_branch(&branch, &start_point)?;
        if let (Mode::Free, Some(script)) = (cfg.mode, script) {
            run_version_script(git, script, &next)?;
        }
    }
    git.push(&branch)?;
    if let (Mode::Protected, Some(script), false) = (cfg.mode, script, no_checkout) {
        land_hotfix_version(git, hosting, script, &branch, &next)?;
    }

    Ok(branch)
}

/// Protected mode: the hotfix version travels on a version branch like every
/// other version change. Its tag waits for the hotfix finish, so a version PR
/// that cannot merge yet only delays the version, never a tag.
fn land_hotfix_version(git: &dyn Git, hosting: &dyn HostingPlatform, script: &dyn VersionScript, branch: &str, version: &SemVer) -> Result<(), String> {
    let Some(url) = open_version_pr(git, hosting, script, branch, version)? else {
        return Ok(());
    };
    if complete_version_pr(hosting, &url) == VersionPrCompletion::Merged {
        git.fetch()?;
        git.ff_merge(&format!("origin/{branch}"))?;
    }
    Ok(())
}

/// Patch-strategy sibling of `find_latest_tag` for hotfix versioning: every
/// tag is clean under patch, so an open release branch's staging tags
/// (`v2.6.0`, `v2.6.1`, …) would win the global max while production still
/// runs `v2.5.3` — the hotfix would misversion itself and steal the number the
/// release's next bump computes. Tags whose `major.minor` matches an open
/// (unshipped) release branch are that release's staging history, not
/// production's, and are excluded. Under rc this filter is provably empty (an
/// open release carries only `-rc.N` tags), which is why rc keeps the plain
/// global scan.
fn find_latest_shipped_tag(git: &dyn Git, hosting: &dyn HostingPlatform, cfg: &RepoConfig, main_branch: &str) -> Result<SemVer, String> {
    let in_flight: Vec<SemVer> = crate::flows::open_versioned_branches(git, hosting, cfg, main_branch, "release")?
        .iter()
        .filter_map(|b| b.strip_prefix("release/").and_then(SemVer::parse))
        .collect();
    let tags = git.list_tags()?;
    Ok(tags
        .iter()
        .filter_map(|t| SemVer::parse(t))
        .filter(|v| !v.is_pre_release())
        .filter(|v| !in_flight.iter().any(|r| r.major == v.major && r.minor == v.minor))
        .max()
        .unwrap_or_else(|| SemVer::new(0, 0, 0)))
}

fn find_latest_tag(git: &dyn Git) -> Result<SemVer, String> {
    let tags = git.list_tags()?;
    let all: Vec<SemVer> = tags.iter().filter_map(|t| SemVer::parse(t)).collect();

    // Prefer clean release tags
    if let Some(v) = all.iter().filter(|v| !v.is_pre_release()).max() {
        return Ok(v.clone());
    }

    // Fall back to highest RC tag (stripped to release) if no clean tags exist
    if let Some(v) = all.iter().filter(|v| v.is_rc()).max() {
        return Ok(v.to_release());
    }

    Ok(SemVer::new(0, 0, 0))
}

#[cfg(test)]
mod tests {
    use super::message_is_breaking;

    #[test]
    fn message_is_breaking_follows_conventional_commits() {
        for (case, msg, expected) in [
            ("bang_in_title", "feat!: remove legacy API", true),
            ("bang_with_scope", "refactor(auth)!: rewrite token handling", true),
            ("breaking_change_footer", "feat: new auth flow\n\nBREAKING CHANGE: old tokens are invalidated", true),
            ("breaking_change_footer_hyphenated", "feat: new auth\n\nBREAKING-CHANGE: old tokens invalidated", true),
            ("breaking_change_footer_case_insensitive", "feat: new auth\n\nbreaking change: old tokens invalidated", true),
            // "non-breaking change" contains "breaking change" but is prose, not the footer
            ("non_breaking_change_in_body", "feat: new feature\n\nThis is a non-breaking change to the API.", false),
            // Only the footer format (with colon) counts
            ("breaking_change_mention_without_colon", "feat: new feature\n\nWe discussed breaking change options earlier.", false),
            ("plain_feat", "feat: add login page", false),
            ("plain_fix", "fix: correct typo", false),
            ("plain_chore", "chore: update deps", false),
            ("unstructured_empty", "", false),
            ("unstructured_bang", "update documentation!", false),
            ("unstructured_with_footer", "remove old behavior\n\nBREAKING CHANGE: old input is rejected", true),
            // The ! must be in the title before the colon, not in the body
            ("bang_in_body", "feat: add feature\n\nThis is great!\nReally awesome.", false),
        ] {
            assert_eq!(message_is_breaking(msg), expected, "{case}: {msg:?}");
        }
    }
}
