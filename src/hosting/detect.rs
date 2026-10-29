//! The GitHub repository gflow targets, read from the `origin` remote URL.

use crate::git::Git;

/// `HOST/OWNER/REPO` of the origin remote, when there is one with that shape.
pub fn origin_repo(git: &dyn Git) -> Option<String> {
    git.remote_url().ok().as_deref().and_then(github_repo)
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

#[cfg(test)]
mod tests {
    use super::*;

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
            ("ssh_scp_style_without_path", "git@github.com", None),
            ("url_without_path", "https://github.com", None),
            ("not_a_url", "not-a-url", None),
        ] {
            assert_eq!(github_repo(remote).as_deref(), expected, "{case}: {remote}");
        }
    }
}
