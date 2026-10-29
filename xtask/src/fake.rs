//! The tools gflow calls besides git, faked so it sees a GitHub remote whose
//! repository is the bare `$V/origin.git`. The checker links this binary
//! under each tool's name; `main` dispatches on that name.

use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::{Command, ExitCode};

pub type Git<'a> = dyn FnMut(&[&str]) -> Result<String, String> + 'a;

const GH: &str = "gh";
const SILENT: [&str; 4] = ["open", "pbcopy", "xdg-open", "code"];

pub fn names() -> impl Iterator<Item = &'static str> {
    [GH].into_iter().chain(SILENT)
}

/// The exit code when `program` is one of the fakes, else None.
pub fn dispatch(program: &str, args: &[String]) -> Option<ExitCode> {
    let result = match program {
        GH => run_gh(args),
        _ if SILENT.contains(&program) => Ok(()),
        _ => return None,
    };
    Some(match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::from(2)
        }
    })
}

fn root() -> Result<PathBuf, String> {
    std::env::var("V").map(PathBuf::from).map_err(|_| "fake tools need $V".to_string())
}

pub fn origin_git(origin: PathBuf) -> impl FnMut(&[&str]) -> Result<String, String> {
    move |args: &[&str]| {
        let output = Command::new("git").arg("-C").arg(&origin).args(args).output().map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Err(format!("git {} failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr)));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }
}

fn run_gh(args: &[String]) -> Result<(), String> {
    let v = root()?;
    let path = v.join("prs.json");
    let mut prs: Vec<Value> = match std::fs::read_to_string(&path) {
        Ok(body) => serde_json::from_str(&body).map_err(|e| e.to_string())?,
        Err(_) => vec![],
    };
    let before = prs.clone();
    let out = gh(args, &mut prs, &mut origin_git(v.join("origin.git")))?;
    if prs != before {
        std::fs::write(&path, Value::from(prs).to_string()).map_err(|e| e.to_string())?;
    }
    print!("{out}");
    Ok(())
}

fn url(id: usize) -> String {
    format!("https://github.com/o/r/pull/{id}")
}

/// Pull requests are rows in `prs`. Merging one squashes it on origin at once,
/// as the platform does. A list answers what its `--jq` filter would: the
/// newest match's `--json` fields, and for a merged lookup only a merged PR.
pub fn gh(args: &[String], prs: &mut Vec<Value>, origin: &mut Git) -> Result<String, String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let opt = |name: &str| words.iter().position(|w| *w == name).and_then(|i| words.get(i + 1)).copied();
    let has = |name: &str| words.contains(&name);
    match words.get(..2).unwrap_or(&words) {
        ["pr", "list"] => {
            let status = match opt("--state") {
                Some("open") => Some("active"),
                Some("merged") => Some("completed"),
                _ => None,
            };
            let fields: Vec<&str> = opt("--json").unwrap_or_default().split(',').collect();
            let newest = prs
                .iter()
                .rev()
                .filter(|p| opt("--head").is_none_or(|h| p["source"] == h))
                .filter(|p| opt("--base").is_none_or(|b| p["target"] == b))
                .find(|p| status.is_none_or(|s| p["status"] == s));
            let Some(pr) = newest.filter(|p| !fields.contains(&"state") || p["status"] == "completed") else {
                return Ok(String::new());
            };
            let field = |name: &str| pr[name].as_str().unwrap_or_default().to_string();
            let row: Vec<String> = fields
                .iter()
                .filter_map(|f| match *f {
                    "url" => Some(url(pr["id"].as_u64().unwrap_or_default() as usize)),
                    "headRefOid" => Some(field("head")),
                    "mergeCommit" => Some(field("merge")),
                    "baseRefName" => Some(field("target")),
                    _ => None,
                })
                .collect();
            Ok(format!("{}\n", row.join("\t")))
        }
        ["pr", "create"] => {
            let id = prs.len() + 1;
            prs.push(json!({
                "id": id, "status": "active", "source": opt("--head"),
                "target": opt("--base"), "title": opt("--title"),
            }));
            Ok(format!("{}\n", url(id)))
        }
        ["pr", "merge"] => {
            let target = words.get(2).copied().unwrap_or_default();
            let id: usize = target.rsplit('/').next().and_then(|id| id.parse().ok()).ok_or(format!("fake gh: no PR in {target}"))?;
            let pr = prs.get_mut(id - 1).ok_or(format!("fake gh: no PR {id}"))?;
            if has("--auto") {
                pr["auto"] = json!(true);
                return Ok(String::new());
            }
            let method = if has("--merge") { "merge" } else { "squash" };
            complete_pr(pr, method, origin)?;
            if has("--delete-branch") {
                origin(&["update-ref", "-d", &format!("refs/heads/{}", pr["source"].as_str().unwrap_or_default())])?;
            }
            Ok(String::new())
        }
        _ => Err(format!("fake gh: unsupported {}", words.join(" "))),
    }
}

/// Completes `pr` on origin with a squash or a merge commit.
pub fn complete_pr(pr: &mut Value, method: &str, origin: &mut Git) -> Result<(), String> {
    let id = pr["id"].to_string();
    let title = pr["title"].as_str().unwrap_or_default().to_string();
    let target_ref = format!("refs/heads/{}", pr["target"].as_str().unwrap_or_default());
    let source = origin(&["rev-parse", &format!("refs/heads/{}", pr["source"].as_str().unwrap_or_default())])?;
    let target = origin(&["rev-parse", &target_ref])?;
    let merged = origin(&["merge-tree", "--write-tree", &target, &source])?;
    let tree = merged.lines().next().unwrap_or_default();
    let message = if method == "squash" { format!("{title} (#{id})") } else { format!("Merge PR #{id}: {title}") };
    let mut commit = vec!["commit-tree", tree, "-p", &target];
    if method != "squash" {
        commit.extend(["-p", &source]);
    }
    commit.extend(["-m", &message]);
    let sha = origin(&commit)?;
    origin(&["update-ref", &target_ref, &sha])?;
    for (key, value) in [("status", "completed"), ("head", &source), ("merge", &sha), ("method", method)] {
        pr[key] = json!(value);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn args(text: &str) -> Vec<String> {
        text.split(' ').map(String::from).collect()
    }

    /// Answers like a bare origin: rev-parse gives `<sha of ref>`, merge-tree a
    /// tree line plus noise, commit-tree a new sha. Records every call.
    fn origin(calls: &mut Vec<String>) -> impl FnMut(&[&str]) -> Result<String, String> + '_ {
        move |a: &[&str]| {
            calls.push(a.join(" "));
            Ok(match a[0] {
                "rev-parse" => format!("sha-of-{}", a[1].trim_start_matches("refs/heads/")),
                "merge-tree" => "tree1\nnoise".into(),
                "commit-tree" => "new".into(),
                _ => String::new(),
            })
        }
    }

    fn no_git(_: &[&str]) -> Result<String, String> {
        panic!("this command needs no git")
    }

    #[test]
    fn gh_lists_the_newest_pull_request_filtered_by_head_base_and_state() {
        let mut prs = vec![
            json!({"id": 1, "status": "completed", "source": "feature/a", "target": "develop", "head": "h1", "merge": "m1"}),
            json!({"id": 2, "status": "active", "source": "feature/a", "target": "develop"}),
            json!({"id": 3, "status": "active", "source": "feature/a", "target": "main"}),
        ];
        let open = gh(&args("pr list --head feature/a --base develop --state open --limit 1 --json url"), &mut prs, &mut no_git);
        assert_eq!(open.unwrap(), "https://github.com/o/r/pull/2\n");
        let merged = gh(&args("pr list --head feature/a --base develop --state merged --limit 1 --json url,state,headRefOid,mergeCommit"), &mut prs, &mut no_git);
        assert_eq!(merged.unwrap(), "https://github.com/o/r/pull/1\th1\tm1\n");
        let none = gh(&args("pr list --head feature/b --state open --json url"), &mut prs, &mut no_git);
        assert_eq!(none.unwrap(), "");
    }

    #[test]
    fn gh_merged_lookup_answers_only_when_the_newest_pr_is_merged() {
        let mut prs = vec![
            json!({"id": 1, "status": "completed", "source": "feature/a", "target": "develop", "head": "h1", "merge": "m1"}),
            json!({"id": 2, "status": "active", "source": "feature/a", "target": "develop"}),
        ];
        let query = "pr list --head feature/a --state all --limit 1 --json url,state,headRefOid,mergeCommit,baseRefName";
        assert_eq!(gh(&args(query), &mut prs, &mut no_git).unwrap(), "");
        prs.pop();
        assert_eq!(gh(&args(query), &mut prs, &mut no_git).unwrap(), "https://github.com/o/r/pull/1\th1\tm1\tdevelop\n");
    }

    #[test]
    fn gh_creates_the_next_pull_request_and_prints_its_url() {
        let mut prs = vec![json!({"id": 1})];
        let out = gh(&args("pr create --head feature/a --base develop --title feat: --body"), &mut prs, &mut no_git);
        assert_eq!(out.unwrap(), "https://github.com/o/r/pull/2\n");
        assert_eq!(prs[1], json!({"id": 2, "status": "active", "source": "feature/a", "target": "develop", "title": "feat:"}));
    }

    #[test]
    fn gh_turns_on_auto_merge_without_merging() {
        let mut prs = vec![json!({"id": 1, "status": "active"})];
        gh(&args("pr merge https://github.com/o/r/pull/1 --auto --squash --delete-branch"), &mut prs, &mut no_git).unwrap();
        assert_eq!(prs[0], json!({"id": 1, "status": "active", "auto": true}));
    }

    #[test]
    fn gh_squash_merges_a_pull_request_and_deletes_its_branch() {
        let mut prs = vec![json!({"id": 1, "status": "active", "source": "feature/a", "target": "develop", "title": "feat: a"})];
        let mut calls = vec![];
        gh(&args("pr merge https://github.com/o/r/pull/1 --squash --delete-branch"), &mut prs, &mut origin(&mut calls)).unwrap();
        assert_eq!(
            calls,
            [
                "rev-parse refs/heads/feature/a",
                "rev-parse refs/heads/develop",
                "merge-tree --write-tree sha-of-develop sha-of-feature/a",
                "commit-tree tree1 -p sha-of-develop -m feat: a (#1)",
                "update-ref refs/heads/develop new",
                "update-ref -d refs/heads/feature/a",
            ]
        );
        assert_eq!(
            prs[0],
            json!({"id": 1, "status": "completed", "source": "feature/a", "target": "develop", "title": "feat: a",
                   "head": "sha-of-feature/a", "merge": "new", "method": "squash"})
        );
    }

    #[test]
    fn gh_merges_with_a_merge_commit_and_keeps_the_branch() {
        let mut prs = vec![json!({"id": 1, "status": "active", "source": "landing/x", "target": "main", "title": "chore: x"})];
        let mut calls = vec![];
        gh(&args("pr merge https://github.com/o/r/pull/1 --merge"), &mut prs, &mut origin(&mut calls)).unwrap();
        assert_eq!(calls[3], "commit-tree tree1 -p sha-of-main -p sha-of-landing/x -m Merge PR #1: chore: x");
        assert_eq!(calls.len(), 5, "the source branch stays: {calls:?}");
        assert_eq!(prs[0]["method"], "merge");
    }

    #[test]
    fn gh_refuses_a_merge_without_a_known_pull_request() {
        assert_eq!(gh(&args("pr merge"), &mut vec![], &mut no_git), Err("fake gh: no PR in ".into()));
        assert_eq!(gh(&args("pr merge https://github.com/o/r/pull/7"), &mut vec![], &mut no_git), Err("fake gh: no PR 7".into()));
    }

    #[test]
    fn a_merge_commit_completion_keeps_both_parents() {
        let mut pr = json!({"id": 4, "status": "active", "source": "landing/x", "target": "main", "title": "chore: x"});
        let mut calls = vec![];
        complete_pr(&mut pr, "merge", &mut origin(&mut calls)).unwrap();
        assert_eq!(calls[3], "commit-tree tree1 -p sha-of-main -p sha-of-landing/x -m Merge PR #4: chore: x");
        assert_eq!(pr["method"], "merge");
    }

    #[test]
    fn gh_refuses_what_it_does_not_fake() {
        let out = gh(&args("pr close 1"), &mut vec![], &mut no_git);
        assert_eq!(out, Err("fake gh: unsupported pr close 1".into()));
    }
}
