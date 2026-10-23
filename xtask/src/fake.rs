//! The tools gflow calls besides git, faked so it sees an Azure DevOps
//! remote that is a bare repository in `$V/origin.git`. The checker links
//! this binary under each tool's name; `main` dispatches on that name.

use serde_json::{json, Value};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, ExitCode};

pub type Git<'a> = dyn FnMut(&[&str]) -> Result<String, String> + 'a;

pub const SSH: &str = "fake-ssh";
pub const AZ: &str = "az";
const SILENT: [&str; 4] = ["open", "pbcopy", "xdg-open", "code"];

pub fn names() -> impl Iterator<Item = &'static str> {
    [SSH, AZ].into_iter().chain(SILENT)
}

/// The exit code when `program` is one of the fakes, else None.
pub fn dispatch(program: &str, args: &[String]) -> Option<ExitCode> {
    let result = match program {
        SSH => run_ssh(args),
        AZ => run_az(args),
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

fn run_ssh(args: &[String]) -> Result<(), String> {
    let origin = root()?.join("origin.git");
    let command = ssh_command(args, &origin.to_string_lossy());
    Err(Command::new("sh").arg("-c").arg(command).exec().to_string())
}

/// What ssh would run on the server, with the Azure DevOps repository path
/// pointing at the local origin.
pub fn ssh_command(args: &[String], origin: &str) -> String {
    let mut rest = args;
    while let Some(first) = rest.first() {
        rest = match first.as_str() {
            "-o" | "-p" | "-i" | "-l" => rest.get(2..).unwrap_or_default(),
            flag if flag.starts_with('-') => &rest[1..],
            _ => break,
        };
    }
    let target = format!("'{origin}'");
    rest.get(1..)
        .unwrap_or_default()
        .join(" ")
        .replacen("'v3/org/proj/repo'", &target, 1)
        .replacen("'/v3/org/proj/repo'", &target, 1)
}

fn run_az(args: &[String]) -> Result<(), String> {
    let v = root()?;
    let path = v.join("prs.json");
    let mut prs: Vec<Value> = match std::fs::read_to_string(&path) {
        Ok(body) => serde_json::from_str(&body).map_err(|e| e.to_string())?,
        Err(_) => vec![],
    };
    let before = prs.clone();
    let out = az(args, &mut prs, &mut origin_git(v.join("origin.git")))?;
    if prs != before {
        std::fs::write(&path, Value::from(prs).to_string()).map_err(|e| e.to_string())?;
    }
    print!("{out}");
    Ok(())
}

/// Pull requests are rows in `prs`. Completing one squashes it on origin at
/// once, as the platform does.
pub fn az(args: &[String], prs: &mut Vec<Value>, origin: &mut Git) -> Result<String, String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let opt = |name: &str| words.iter().position(|w| *w == name).and_then(|i| words.get(i + 1)).copied();
    match words.get(..3).unwrap_or(&words) {
        ["extension", "show", ..] => Ok(String::new()),
        ["repos", "pr", "list"] => Ok(prs
            .iter()
            .rev()
            .filter(|p| opt("--target-branch").is_none_or(|t| p["target"] == t))
            .filter(|p| opt("--source-branch").is_none_or(|s| p["source"] == s))
            .map(|p| {
                let field = |name: &str| p[name].as_str().unwrap_or_default().to_string();
                let row = [
                    field("status"),
                    format!("refs/heads/{}", field("source")),
                    format!("refs/heads/{}", field("target")),
                    field("head"),
                    field("merge"),
                    p["id"].to_string(),
                ];
                format!("{}\n", row.join("\t"))
            })
            .collect()),
        ["repos", "pr", "create"] => {
            let id = prs.len() + 1;
            prs.push(json!({
                "id": id, "status": "active", "source": opt("--source-branch"),
                "target": opt("--target-branch"), "title": opt("--title"),
            }));
            Ok(format!("{id}\n"))
        }
        ["repos", "pr", "update"] => {
            let id: usize = opt("--id").and_then(|id| id.parse().ok()).ok_or("fake az: update needs --id")?;
            let pr = prs.get_mut(id - 1).ok_or(format!("fake az: no PR {id}"))?;
            if opt("--auto-complete") == Some("true") {
                pr["auto"] = json!(true);
            }
            if opt("--status") == Some("completed") {
                let method = if opt("--squash") == Some("false") { "merge" } else { "squash" };
                complete_pr(pr, method, origin)?;
                if opt("--delete-source-branch") == Some("true") {
                    origin(&["update-ref", "-d", &format!("refs/heads/{}", pr["source"].as_str().unwrap_or_default())])?;
                }
            }
            Ok(String::new())
        }
        _ => Err(format!("fake az: unsupported {}", words.join(" "))),
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
    fn ssh_runs_the_remote_command_against_the_local_origin() {
        let command = ssh_command(&args("-o SendEnv=GIT_PROTOCOL -p 22 -4 git@ssh.dev.azure.com git-upload-pack 'v3/org/proj/repo'"), "/v/origin.git");
        assert_eq!(command, "git-upload-pack '/v/origin.git'");
    }

    #[test]
    fn ssh_also_maps_the_path_with_a_leading_slash() {
        let command = ssh_command(&args("-i key -l git host git-receive-pack '/v3/org/proj/repo'"), "/v/origin.git");
        assert_eq!(command, "git-receive-pack '/v/origin.git'");
    }

    #[test]
    fn az_answers_the_extension_check() {
        assert_eq!(az(&args("extension show --name azure-devops"), &mut vec![], &mut no_git), Ok(String::new()));
    }

    #[test]
    fn az_lists_pull_requests_newest_first_filtered_by_target_and_source() {
        let mut prs = vec![
            json!({"id": 1, "status": "completed", "source": "feature/a", "target": "develop", "head": "h1", "merge": "m1"}),
            json!({"id": 2, "status": "active", "source": "feature/b", "target": "develop"}),
            json!({"id": 3, "status": "active", "source": "feature/b", "target": "main"}),
        ];
        let by_target = az(&args("repos pr list --target-branch develop --status all"), &mut prs, &mut no_git);
        assert_eq!(
            by_target.unwrap(),
            "active\trefs/heads/feature/b\trefs/heads/develop\t\t\t2\n\
             completed\trefs/heads/feature/a\trefs/heads/develop\th1\tm1\t1\n"
        );
        let by_source = az(&args("repos pr list --source-branch feature/b"), &mut prs, &mut no_git);
        assert_eq!(
            by_source.unwrap(),
            "active\trefs/heads/feature/b\trefs/heads/main\t\t\t3\n\
             active\trefs/heads/feature/b\trefs/heads/develop\t\t\t2\n"
        );
    }

    #[test]
    fn az_creates_the_next_pull_request_and_prints_its_id() {
        let mut prs = vec![json!({"id": 1})];
        let out = az(&args("repos pr create --source-branch feature/a --target-branch develop --title feat:"), &mut prs, &mut no_git);
        assert_eq!(out.unwrap(), "2\n");
        assert_eq!(prs[1], json!({"id": 2, "status": "active", "source": "feature/a", "target": "develop", "title": "feat:"}));
    }

    #[test]
    fn az_turns_on_auto_complete() {
        let mut prs = vec![json!({"id": 1, "status": "active"})];
        az(&args("repos pr update --id 1 --auto-complete true"), &mut prs, &mut no_git).unwrap();
        assert_eq!(prs[0], json!({"id": 1, "status": "active", "auto": true}));
    }

    #[test]
    fn az_completes_a_pull_request_as_a_squash_and_can_delete_its_source() {
        let mut prs = vec![json!({"id": 1, "status": "active", "source": "feature/a", "target": "develop", "title": "feat: a"})];
        let mut calls = vec![];
        az(&args("repos pr update --id 1 --status completed --delete-source-branch true"), &mut prs, &mut origin(&mut calls)).unwrap();
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
    fn az_completes_with_a_merge_commit_when_squash_is_off() {
        let mut prs = vec![json!({"id": 1, "status": "active", "source": "landing/x", "target": "main", "title": "chore: x"})];
        let mut calls = vec![];
        az(&args("repos pr update --id 1 --status completed --squash false --delete-source-branch false"), &mut prs, &mut origin(&mut calls)).unwrap();
        assert_eq!(calls[3], "commit-tree tree1 -p sha-of-main -p sha-of-landing/x -m Merge PR #1: chore: x");
        assert_eq!(calls.len(), 5, "the source branch stays: {calls:?}");
        assert_eq!(prs[0]["method"], "merge");
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
    fn az_refuses_what_it_does_not_fake() {
        let out = az(&args("repos pr abandon --id 1"), &mut vec![], &mut no_git);
        assert_eq!(out, Err("fake az: unsupported repos pr abandon --id 1".into()));
    }
}
