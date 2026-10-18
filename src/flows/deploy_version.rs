use std::collections::HashSet;

use crate::git::branch::BranchType;
use crate::git::Git;
use crate::version::SemVer;

/// What `gflow deploy-version` prints: `X.Y.Z+N`, or one of its two parts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DeployPart {
    Full,
    Name,
    Build,
}

impl DeployPart {
    /// `--name` and `--build` choose one part; clap rejects both together.
    pub fn from_flags(name: bool, build: bool) -> Self {
        match (name, build) {
            (true, _) => Self::Name,
            (_, true) => Self::Build,
            _ => Self::Full,
        }
    }
}

/// The version of a deploy build from `branch`: a name from the tags, and a
/// build number that counts the branch's own first-parent commits, so one
/// name and number always mean one commit on develop, release, and hotfix
/// branches. A work branch counts like the branch it lands on, on its own
/// commits. It only reads: it changes nothing and asks nothing.
pub fn deploy_version(git: &dyn Git, branch: &str, main_branch: &str, part: DeployPart) -> Result<String, String> {
    let (name, build) = match BranchType::parse(branch) {
        BranchType::Develop | BranchType::Feature { .. } | BranchType::Fix { .. } | BranchType::Chore { .. }
        | BranchType::Docs { .. } | BranchType::Refactor { .. } => develop_version(git)?,
        BranchType::Release { major, minor, patch } | BranchType::ReleaseFix { major, minor, patch, .. } => {
            release_version(git, &SemVer::new(major, minor, patch))?
        }
        BranchType::Hotfix { major, minor, patch } | BranchType::HotfixFix { major, minor, patch, .. } => {
            (SemVer::new(major, minor, patch).to_string(), commits_past(git, main_branch)?)
        }
        BranchType::Main | BranchType::Other => {
            return Err(format!(
                "gflow deploy-version runs on develop, release, and hotfix branches, and on the work branches that land on them, not on {branch}."
            ))
        }
    };
    Ok(match part {
        DeployPart::Full => format!("{name}+{build}"),
        DeployPart::Name => name,
        DeployPart::Build => build.to_string(),
    })
}

/// The next minor after the highest tag: nobody knows yet whether the next
/// release is minor or major, so it is a build label, not a promise. The
/// count starts at the cut of that tag's release line: the parent of the
/// line's first tag, which sits on the release's first commit.
fn develop_version(git: &dyn Git) -> Result<(String, usize), String> {
    let tags: Vec<SemVer> = git.list_tags()?.iter().filter_map(|t| SemVer::parse(t)).collect();
    let head = git.first_parent_commits("HEAD")?;
    let Some(highest) = tags.iter().max() else {
        return Ok((SemVer::new(0, 1, 0).to_string(), head.len()));
    };
    let first_of_line = tags.iter().filter(|t| t.major == highest.major && t.minor == highest.minor).min().unwrap_or(highest);
    let line = git.first_parent_commits(&first_of_line.tag_name())?;
    let since_cut = match line.get(1) {
        Some(cut) => head.iter().position(|c| c == cut).unwrap_or(head.len()),
        None => head.len(),
    };
    Ok((SemVer::new(highest.major, highest.minor + 1, 0).to_string(), 1 + since_cut))
}

/// The release's latest tag of its own line (`-rc.N` included), or the
/// release's own version before its first tag.
fn release_version(git: &dyn Git, release: &SemVer) -> Result<(String, usize), String> {
    let name = git.tags_on_branch("HEAD")?.iter()
        .filter_map(|t| SemVer::parse(t))
        .filter(|t| t.major == release.major && t.minor == release.minor)
        .max()
        .unwrap_or_else(|| release.clone());
    Ok((name.to_string(), commits_past(git, "develop")?))
}

/// HEAD's first-parent commits that are not on `base`'s first-parent line:
/// the commits the branch made since it opened. Merges into the branch count
/// as one commit.
fn commits_past(git: &dyn Git, base: &str) -> Result<usize, String> {
    let base = if git.remote_branch_exists(base)? { format!("origin/{base}") } else { base.to_string() };
    let head = git.first_parent_commits("HEAD")?;
    let line: HashSet<String> = git.first_parent_commits(&base)?.into_iter().collect();
    Ok(head.iter().take_while(|c| !line.contains(*c)).count())
}
