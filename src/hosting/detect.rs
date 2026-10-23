//! Hosting-provider detection.
//!
//! The provider is auto-detected from the `origin` remote URL. The git config
//! key `gflow.hosting.provider` (`github` | `devops`) overrides detection for
//! edge cases (e.g. GitHub Enterprise domains).

use crate::git::Git;
use super::Result;

const PROVIDER_KEY: &str = "gflow.hosting.provider";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provider {
    /// `repo` is `HOST/OWNER/REPO` from the origin remote, when it has that shape.
    GitHub { repo: Option<String> },
    AzureDevOps { org: String, project: String, repo: String },
}

/// Detect the hosting provider from the `gflow.hosting.provider` override and
/// the `origin` remote URL.
pub fn detect(git: &dyn Git) -> Result<Provider> {
    let override_val = git.get_config(PROVIDER_KEY)?;
    let remote = git.remote_url().ok();
    resolve(remote.as_deref(), override_val.as_deref())
}

/// Pure resolution core (unit-tested without a git repo).
///
/// - Override `github` → GitHub unconditionally.
/// - Override `devops` → the remote must be an Azure DevOps URL (az needs the
///   org/project/repo coordinates) or an error is returned.
/// - No override → parse the remote; unrecognized hosts and missing remotes
///   fall back to GitHub (preserves pre-detection behavior).
fn resolve(remote_url: Option<&str>, override_val: Option<&str>) -> Result<Provider> {
    let github = || Provider::GitHub { repo: remote_url.and_then(github_repo) };
    match override_val.map(str::trim) {
        Some("github") => Ok(github()),
        Some("devops") => match remote_url.and_then(parse_remote) {
            Some(p @ Provider::AzureDevOps { .. }) => Ok(p),
            _ => Err(format!(
                "{PROVIDER_KEY} is 'devops' but the Azure DevOps organization/project/repository could not be determined from the origin remote URL."
            )),
        },
        Some(other) => Err(format!(
            "Invalid {PROVIDER_KEY} value '{other}'. Valid values: github, devops."
        )),
        None => Ok(remote_url.and_then(parse_remote).unwrap_or_else(github)),
    }
}

/// Split a remote URL into its host and path segments.
fn split_remote(url: &str) -> Option<(&str, Vec<&str>)> {
    let url = url.trim().trim_end_matches('/');
    let url = url.strip_suffix(".git").unwrap_or(url);

    let (host, path) = match url.strip_prefix("git@") {
        // SSH scp-like syntax: git@host:path
        Some(rest) => rest.split_once(':')?,
        // URL syntax: scheme://[user@]host/path
        None => {
            let rest = url.split_once("://").map(|(_, r)| r)?;
            let rest = rest.rsplit_once('@').map_or(rest, |(_, r)| r);
            rest.split_once('/')?
        }
    };
    Some((host, path.split('/').filter(|s| !s.is_empty()).collect()))
}

/// The `--repo` value gh needs to stay on origin. Without it, gh resolves the
/// repository itself and prefers an `upstream` remote, which in a fork is the
/// parent repository.
fn github_repo(url: &str) -> Option<String> {
    let (host, segments) = split_remote(url)?;
    let host = host.split_once(':').map_or(host, |(name, _port)| name);
    match segments.as_slice() {
        [owner, repo] => Some(format!("{host}/{owner}/{repo}")),
        _ => None,
    }
}

/// Parse an Azure DevOps remote URL. Returns `None` for any other host.
fn parse_remote(url: &str) -> Option<Provider> {
    let (host, segments) = split_remote(url)?;

    match host {
        // https://dev.azure.com/{org}/{project}/_git/{repo}
        "dev.azure.com" => match segments.as_slice() {
            [org, project, "_git", repo] => Some(azure(org, project, repo)),
            _ => None,
        },
        // git@ssh.dev.azure.com:v3/{org}/{project}/{repo}
        "ssh.dev.azure.com" => match segments.as_slice() {
            ["v3", org, project, repo] => Some(azure(org, project, repo)),
            _ => None,
        },
        // Legacy https://{org}.visualstudio.com/[DefaultCollection/]{project}/_git/{repo}
        _ => {
            let org = host.strip_suffix(".visualstudio.com")?;
            match segments.as_slice() {
                [project, "_git", repo] | ["DefaultCollection", project, "_git", repo] => {
                    Some(azure(org, project, repo))
                }
                _ => None,
            }
        }
    }
}

fn azure(org: &str, project: &str, repo: &str) -> Provider {
    Provider::AzureDevOps {
        org: org.to_string(),
        // https remotes percent-encode spaces in project names; az wants the
        // display name. Spaces are the only realistic case in ADO names.
        project: project.replace("%20", " "),
        repo: repo.replace("%20", " "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ado(org: &str, project: &str, repo: &str) -> Provider {
        Provider::AzureDevOps {
            org: org.to_string(),
            project: project.to_string(),
            repo: repo.to_string(),
        }
    }

    fn github(repo: Option<&str>) -> Provider {
        Provider::GitHub { repo: repo.map(str::to_string) }
    }

    #[test]
    fn github_repo_names_host_owner_and_repo() {
        for (case, remote, expected) in [
            ("https", "https://github.com/acme/repo.git", Some("github.com/acme/repo")),
            ("https_without_git_suffix", "https://github.com/acme/repo", Some("github.com/acme/repo")),
            ("https_with_user", "https://jop@github.com/acme/repo", Some("github.com/acme/repo")),
            ("ssh_scp_style", "git@github.com:acme/repo.git", Some("github.com/acme/repo")),
            ("ssh_url", "ssh://git@github.com/acme/repo.git", Some("github.com/acme/repo")),
            ("enterprise_host", "https://github.enterprise.corp/acme/repo/", Some("github.enterprise.corp/acme/repo")),
            ("ssh_url_with_port", "ssh://git@ghe.corp:2222/acme/repo.git", Some("ghe.corp/acme/repo")),
            ("owner_only", "https://github.com/acme", None),
            ("nested_group", "https://gitlab.com/group/sub/repo", None),
            ("not_a_url", "not-a-url", None),
        ] {
            assert_eq!(github_repo(remote).as_deref(), expected, "{case}: {remote}");
        }
    }

    #[test]
    fn parse_remote_recognizes_azure_devops_remotes() {
        for (case, remote, expected) in [
            ("devops_https", "https://dev.azure.com/beans/Shop/_git/backend", Some(ado("beans", "Shop", "backend"))),
            ("devops_https_with_user", "https://beans@dev.azure.com/beans/Shop/_git/backend", Some(ado("beans", "Shop", "backend"))),
            ("devops_https_percent_encoded_project", "https://dev.azure.com/beans/My%20Shop/_git/backend", Some(ado("beans", "My Shop", "backend"))),
            ("devops_ssh_v3_scp_style", "git@ssh.dev.azure.com:v3/beans/Shop/backend", Some(ado("beans", "Shop", "backend"))),
            ("devops_ssh_v3_url", "ssh://git@ssh.dev.azure.com/v3/beans/Shop/backend", Some(ado("beans", "Shop", "backend"))),
            ("devops_legacy_visualstudio", "https://beans.visualstudio.com/Shop/_git/backend", Some(ado("beans", "Shop", "backend"))),
            ("devops_legacy_visualstudio_default_collection", "https://beans.visualstudio.com/DefaultCollection/Shop/_git/backend", Some(ado("beans", "Shop", "backend"))),
            ("trailing_slash_and_git_suffix_stripped", "https://dev.azure.com/beans/Shop/_git/backend.git/", Some(ado("beans", "Shop", "backend"))),
        ] {
            assert_eq!(parse_remote(remote), expected, "{case}: {remote}");
        }
    }

    #[test]
    fn non_azure_hosts_yield_none() {
        assert_eq!(parse_remote("https://github.com/acme/repo.git"), None);
        assert_eq!(parse_remote("https://gitlab.com/acme/repo.git"), None);
        assert_eq!(parse_remote("https://github.enterprise.corp/acme/repo"), None);
        assert_eq!(parse_remote("not-a-url"), None);
        assert_eq!(parse_remote("https://dev.azure.com/beans/backend"), None);
    }

    #[test]
    fn no_override_unrecognized_or_missing_remote_defaults_to_github() {
        assert_eq!(resolve(Some("https://gitlab.com/a/b/c"), None), Ok(github(None)));
        assert_eq!(resolve(None, None), Ok(github(None)));
    }

    #[test]
    fn a_github_remote_carries_its_repository() {
        assert_eq!(resolve(Some("git@github.com:acme/repo.git"), None), Ok(github(Some("github.com/acme/repo"))));
        assert_eq!(resolve(Some("https://ghe.corp/acme/repo"), Some("github")), Ok(github(Some("ghe.corp/acme/repo"))));
    }

    #[test]
    fn no_override_ado_remote_detected() {
        assert_eq!(
            resolve(Some("https://dev.azure.com/beans/Shop/_git/backend"), None),
            Ok(ado("beans", "Shop", "backend")),
        );
    }

    #[test]
    fn override_github_wins_over_ado_remote() {
        assert_eq!(
            resolve(Some("https://dev.azure.com/beans/Shop/_git/backend"), Some("github")),
            Ok(github(None)),
        );
    }

    #[test]
    fn override_devops_requires_parsable_ado_remote() {
        let err = resolve(Some("https://github.com/acme/repo"), Some("devops")).unwrap_err();
        assert!(err.contains("could not be determined"), "{err}");
        let err = resolve(None, Some("devops")).unwrap_err();
        assert!(err.contains("could not be determined"), "{err}");
    }

    #[test]
    fn override_devops_with_a_parsable_ado_remote_is_accepted() {
        // The asymmetry that makes the error above safe: an explicit devops
        // override is honored (not silently downgraded to GitHub) whenever the
        // coordinates az needs are actually present.
        assert_eq!(
            resolve(Some("https://dev.azure.com/beans/Shop/_git/backend"), Some("devops")),
            Ok(ado("beans", "Shop", "backend")),
        );
        // Config values are trimmed before matching — `git config` round-trips
        // stray whitespace and an untrimmed value would fall through to GitHub.
        assert_eq!(
            resolve(Some("git@ssh.dev.azure.com:v3/beans/Shop/backend"), Some("  devops  ")),
            Ok(ado("beans", "Shop", "backend")),
        );
    }

    #[test]
    fn ado_urls_missing_coordinates_yield_none() {
        // Each ADO URL shape needs its full segment list; a partial one must not
        // produce Provider::AzureDevOps with empty org/project/repo.
        assert_eq!(parse_remote("git@ssh.dev.azure.com:v3/beans/Shop"), None, "ssh v3 without repo");
        assert_eq!(parse_remote("git@ssh.dev.azure.com:v4/beans/Shop/backend"), None, "wrong ssh version segment");
        assert_eq!(parse_remote("https://beans.visualstudio.com/Shop"), None, "legacy without _git/repo");
        assert_eq!(parse_remote("https://beans.visualstudio.com/Shop/_git"), None, "legacy without repo");
        assert_eq!(parse_remote("git@ssh.dev.azure.com"), None);
        assert_eq!(parse_remote("https://dev.azure.com"), None);
    }

    #[test]
    fn override_invalid_value_errors() {
        let err = resolve(Some("https://github.com/acme/repo"), Some("gitlab")).unwrap_err();
        assert!(err.contains("Valid values: github, devops"), "{err}");
    }
}
