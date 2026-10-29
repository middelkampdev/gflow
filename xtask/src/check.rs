//! Replays spec/cycles/*.json against a real gflow binary and reports the
//! first action whose effect on origin differs from the spec, per cycle.
//!
//! Each cycle runs in its own temporary directory: a bare origin, a working
//! clone, and the fakes from `fake.rs`, so gflow sees a GitHub remote.
//! Every action switches to its `on` branch and executes its `run`; the check
//! then compares the ops on origin (branches, commits, merges, VERSION, tags,
//! PRs, hooks) and what `gflow deploy-version` prints on each open branch.
//! Texts are not compared. Needs git 2.38+.

use crate::fake;
use crate::spec::{load_cycles, root, runs_deploy_version, text, HOOK_EVENTS};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub type Refs = BTreeMap<String, String>;

const HOOK: &str = r#"echo "$GFLOW_EVENT $GFLOW_TAG $GFLOW_TAG_KIND $GFLOW_BRANCH" >> "$V/hooks.log""#;

pub fn run(args: &[String]) -> Result<(), String> {
    let (gflow, names) = match args {
        [flag, path, names @ ..] if flag == "--gflow" => (PathBuf::from(path), names),
        names => (build_release()?, names),
    };
    let gflow = std::fs::canonicalize(&gflow).map_err(|e| format!("{}: {e}", gflow.display()))?;
    let mut cycles = load_cycles()?;
    if !names.is_empty() {
        if let Some(unknown) = names.iter().find(|n| !cycles.iter().any(|(path, _)| &stem(path) == *n)) {
            return Err(format!("no cycle named {unknown} in spec/cycles"));
        }
        cycles.retain(|(path, _)| names.contains(&stem(path)));
    }
    let tmp = temp_dir()?;
    let results: Vec<(String, Result<(), String>)> = std::thread::scope(|scope| {
        let runs: Vec<_> = cycles
            .into_iter()
            .map(|(path, spec)| {
                let (name, gflow, tmp) = (stem(&path), &gflow, &tmp);
                scope.spawn(move || {
                    let result = Cycle::new(&name, spec, gflow, tmp).and_then(|mut cycle| cycle.check());
                    (name, result)
                })
            })
            .collect();
        runs.into_iter().map(|run| run.join().expect("a cycle thread panicked")).collect()
    });
    let _ = std::fs::remove_dir_all(&tmp);
    for (name, result) in &results {
        match result {
            Ok(()) => println!("✔ {name}"),
            Err(mismatch) => println!("✖ {name}\n  {mismatch}"),
        }
    }
    let failed = results.iter().filter(|(_, r)| r.is_err()).count();
    if failed > 0 {
        return Err(format!("{failed} of {} cycles differ from the spec", results.len()));
    }
    Ok(())
}

fn build_release() -> Result<PathBuf, String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = Command::new(cargo)
        .args(["build", "--release", "--quiet", "--package", "gflow"])
        .current_dir(root())
        .status()
        .map_err(|e| format!("cargo build: {e}"))?;
    if !status.success() {
        return Err("cargo build --release failed".into());
    }
    Ok(root().join("target").join("release").join("gflow"))
}

fn temp_dir() -> Result<PathBuf, String> {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default();
    let dir = std::env::temp_dir().join(format!("gflow-spec-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // gflow keys hook approvals by the real repository path, and macOS temp
    // dirs sit behind a /var -> /private/var symlink.
    std::fs::canonicalize(&dir).map_err(|e| e.to_string())
}

fn stem(path: &Path) -> String {
    path.file_stem().unwrap_or_default().to_string_lossy().into_owned()
}

fn kind_of(branch: &str) -> &'static str {
    let kind = branch.split('/').next().unwrap_or_default();
    if ["release", "hotfix", "landing", "develop"].contains(&kind) {
        "start"
    } else {
        "branch"
    }
}

fn rank(branch: &str) -> u8 {
    match ["main", "develop", "release/", "hotfix/"].iter().position(|p| branch.starts_with(p)) {
        Some(i) => i as u8,
        None if branch.starts_with("version/") || branch.starts_with("landing/") => 5,
        None => 4,
    }
}

/// Where a tag lands when several branches share its commit.
fn tag_rank(branch: &str) -> u8 {
    match rank(branch) {
        2 => 0,
        3 => 1,
        0 => 2,
        1 => 3,
        _ => 4,
    }
}

/// Whether gflow cuts `new` from `candidate`, by the names alone.
fn is_base(new: &str, candidate: &str) -> bool {
    let flat = candidate.replace('/', "-");
    let (kind, rest) = new.split_once('/').unwrap_or((new, ""));
    match kind {
        "release" | "feature" | "fix" | "chore" | "docs" | "refactor" => candidate == "develop",
        "hotfix" => candidate == "main",
        "release-fix" | "hotfix-fix" => {
            let parent = if kind == "release-fix" { "release" } else { "hotfix" };
            candidate == format!("{parent}/{}", rest.split('/').next().unwrap_or_default())
        }
        "version" => rest.split_once("-into-").is_some_and(|(_, target)| flat == target),
        "landing" => rest.split_once("-into-").is_some_and(|(source, _)| flat == source),
        _ => false,
    }
}

fn flag(mid_merge: bool, success: bool, output: &str) -> Option<&'static str> {
    if mid_merge {
        Some("conflict")
    } else if !success {
        Some("refuse")
    } else if output.lines().any(|l| l.starts_with('✔') && l.to_lowercase().contains("complete")) {
        Some("done")
    } else {
        None
    }
}

fn expected_flag(action: &Value) -> Option<String> {
    let last = action["frames"].as_array().and_then(|frames| frames.last())?;
    let flag = last["flag"].as_str().unwrap_or_default().split(':').next().unwrap_or_default();
    (!flag.is_empty()).then(|| flag.to_string())
}

/// The op as JSON with sorted keys, so equal ops compare equal as text.
fn canonical(op: &Value) -> String {
    match op {
        Value::Object(map) => {
            let sorted: BTreeMap<&String, String> = map.iter().map(|(k, v)| (k, canonical(v))).collect();
            let fields: Vec<String> = sorted.iter().map(|(k, v)| format!("{}:{v}", Value::from(k.as_str()))).collect();
            format!("{{{}}}", fields.join(","))
        }
        other => other.to_string(),
    }
}

fn ops_mismatch(expected: &[Value], actual: &[Value]) -> Option<String> {
    let sorted = |ops: &[Value]| {
        let mut texts: Vec<String> = ops.iter().map(canonical).collect();
        texts.sort();
        texts
    };
    let (expected, actual) = (sorted(expected), sorted(actual));
    if expected == actual {
        return None;
    }
    let only = |a: &[String], b: &[String]| {
        let rest: BTreeSet<&String> = a.iter().filter(|op| !b.contains(op)).collect();
        format!("[{}]", rest.into_iter().cloned().collect::<Vec<_>>().join(", "))
    };
    Some(format!(
        "ops differ\n    spec only:  {}\n    gflow only: {}",
        only(&expected, &actual),
        only(&actual, &expected)
    ))
}

/// `for-each-ref` lines of `refname objectname *objectname` as (branches,
/// tags); an annotated tag maps to the commit it points at.
fn parse_refs(for_each_ref: &str) -> (Refs, Refs) {
    let (mut branches, mut tags) = (Refs::new(), Refs::new());
    for line in for_each_ref.lines() {
        let mut fields = line.split(' ');
        let (name, sha) = (fields.next().unwrap_or_default(), fields.next().unwrap_or_default());
        let peeled = fields.next().filter(|p| !p.is_empty()).unwrap_or(sha);
        if let Some(branch) = name.strip_prefix("refs/heads/") {
            branches.insert(branch.into(), sha.into());
        } else if let Some(tag) = name.strip_prefix("refs/tags/") {
            tags.insert(tag.into(), peeled.into());
        }
    }
    (branches, tags)
}

fn hook_ops(lines: &[&str]) -> Vec<Value> {
    lines
        .iter()
        .map(|line| {
            let f: Vec<&str> = line.split(' ').collect();
            json!({"op": "hook", "event": f[0], "tag": f[1], "kind": f[2], "branch": f[3]})
        })
        .collect()
}

fn deploy_branches(refs: &Refs) -> Vec<String> {
    let mut branches: Vec<String> = refs.keys().filter(|b| runs_deploy_version(b)).cloned().collect();
    branches.sort_by_key(|b| (b != "develop", b.clone()));
    branches
}

/// One spec cycle replayed in its own temporary repository.
struct Cycle<'a> {
    spec: Value,
    gflow: &'a Path,
    v: PathBuf,
    origin: PathBuf,
    work: PathBuf,
    probe: PathBuf,
    env: Vec<(String, String)>,
    refs: Refs,
    tags: Refs,
    known: HashSet<String>,
    prs: HashMap<u64, String>,
    hooks: usize,
}

impl<'a> Cycle<'a> {
    fn new(name: &str, spec: Value, gflow: &'a Path, tmp: &Path) -> Result<Self, String> {
        let v = tmp.join(name);
        let (home, bin) = (v.join("home"), v.join("bin"));
        let io = |e: std::io::Error| format!("{}: {e}", v.display());
        std::fs::create_dir_all(home.join(".gflow")).map_err(io)?;
        std::fs::create_dir_all(&bin).map_err(io)?;
        let gitconfig = home.join(".gitconfig");
        std::fs::write(
            &gitconfig,
            "[user]\n\tname = Dev\n\temail = dev@example.com\n[init]\n\tdefaultBranch = main\n[advice]\n\tdetachedHead = false\n",
        )
        .map_err(io)?;
        let me = std::env::current_exe().map_err(io)?;
        for name in fake::names() {
            std::os::unix::fs::symlink(&me, bin.join(name)).map_err(io)?;
        }
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        let env = [
            ("V", v.display().to_string()),
            ("HOME", home.display().to_string()),
            ("GIT_CONFIG_GLOBAL", gitconfig.display().to_string()),
            ("PATH", path),
            ("GIT_EDITOR", "true".into()),
            ("NO_COLOR", "1".into()),
        ]
        .map(|(k, val)| (k.to_string(), val))
        .to_vec();
        Ok(Cycle {
            spec,
            gflow,
            origin: v.join("origin.git"),
            work: v.join("work"),
            probe: v.join("probe"),
            v,
            env,
            refs: Refs::new(),
            tags: Refs::new(),
            known: HashSet::new(),
            prs: HashMap::new(),
            hooks: 0,
        })
    }

    // ---------- shell ----------

    fn command(&self, program: impl AsRef<std::ffi::OsStr>, args: &[&str], cwd: &Path) -> Command {
        let mut command = Command::new(program);
        command.args(args).current_dir(cwd).envs(self.env.iter().cloned()).stdin(Stdio::null());
        command
    }

    fn sh_in(&self, cwd: &Path, args: &[&str]) -> Result<String, String> {
        let output = self.command(args[0], &args[1..], cwd).output().map_err(|e| format!("{}: {e}", args[0]))?;
        if !output.status.success() {
            let (out, err) = (String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
            return Err(format!("{} failed: {out}{err}", args.join(" ")));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn sh(&self, args: &[&str]) -> Result<String, String> {
        self.sh_in(&self.work, args)
    }

    fn og(&self, args: &[&str]) -> Result<String, String> {
        let mut full = vec!["git"];
        full.extend(args);
        self.sh_in(&self.origin, &full)
    }

    fn pr_rows(&self) -> Result<Vec<Value>, String> {
        match std::fs::read_to_string(self.v.join("prs.json")) {
            Ok(body) => serde_json::from_str(&body).map_err(|e| e.to_string()),
            Err(_) => Ok(vec![]),
        }
    }

    fn write(&self, path: &Path, body: &str) -> Result<(), String> {
        std::fs::create_dir_all(path.parent().expect("a file in a directory")).map_err(|e| e.to_string())?;
        std::fs::write(path, body).map_err(|e| format!("{}: {e}", path.display()))
    }

    // ---------- runs ----------

    fn reset(&mut self) -> Result<(), String> {
        let origin = self.origin.display().to_string();
        let work = self.work.display().to_string();
        let probe = self.probe.display().to_string();
        self.sh_in(&self.v, &["git", "init", "-q", "--bare", &origin])?;
        self.sh_in(&self.v, &["git", "clone", "-q", &origin, &work])?;
        let settings = &self.spec["settings"];
        let mut config: String = ["mode", "bump-strategy", "keep-release-branches"]
            .iter()
            .map(|key| format!("{key}={}\n", text(&settings[key])))
            .collect();
        config.extend(HOOK_EVENTS.iter().map(|event| format!("hook-{event}={HOOK}\n")));
        self.write(&self.work.join(".gflow/config"), &config)?;
        let script = self.work.join(".gflow/set-version.sh");
        self.write(&script, "#!/bin/sh\nprintf '%s\\n' \"$1\" > VERSION\n")?;
        std::fs::set_permissions(&script, Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
        self.write(&self.work.join("VERSION"), "0.0.0\n")?;
        // As a yes at the trust prompt would.
        gflow::hooks::approve(&self.v.join("home/.gflow").join(gflow::hooks::APPROVALS_FILE), &self.work, HOOK)?;
        self.sh(&["git", "add", "."])?;
        self.sh(&["git", "commit", "-qm", "Initial commit"])?;
        self.sh(&["git", "push", "-q", "origin", "main", "main:develop"])?;
        self.sh(&["git", "switch", "-q", "-c", "develop", "--track", "origin/develop"])?;
        self.sh_in(&self.v, &["git", "clone", "-q", &origin, &probe])?;
        self.sh_in(&self.probe, &["git", "config", "gflow.branch.main", "main"])?;
        let (refs, _) = self.origin_state()?;
        self.known.insert(refs["main"].clone());
        self.refs.extend(refs);
        Ok(())
    }

    fn has_ref(&self, name: &str) -> bool {
        self.sh(&["git", "show-ref", "--verify", "--quiet", name]).is_ok()
    }

    /// The developer switches to `branch`, up to date with origin.
    fn stand_on(&self, branch: &str) -> Result<(), String> {
        self.sh(&["git", "fetch", "-q", "origin"])?;
        if self.sh(&["git", "branch", "--show-current"])? != branch {
            if self.has_ref(&format!("refs/heads/{branch}")) {
                self.sh(&["git", "switch", "-q", branch])?;
            } else {
                self.sh(&["git", "switch", "-q", "-c", branch, "--track", &format!("origin/{branch}")])?;
            }
        }
        if self.has_ref(&format!("refs/remotes/origin/{branch}")) {
            // Already up to date, or ahead with a local merge resolution.
            let _ = self.sh(&["git", "merge", "-q", "--ff-only", &format!("origin/{branch}")]);
        }
        Ok(())
    }

    fn run_gflow(&self, args: &[&str]) -> Result<Option<&'static str>, String> {
        std::thread::sleep(Duration::from_millis(200));
        let output = self.command(self.gflow, args, &self.work).output().map_err(|e| format!("gflow: {e}"))?;
        // The background fetch.
        std::thread::sleep(Duration::from_millis(300));
        let out = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
        Ok(flag(self.work.join(".git/MERGE_HEAD").exists(), output.status.success(), out.trim()))
    }

    fn commit(&self, path: &str, message: &str) -> Result<(), String> {
        self.write(&self.work.join(path), &format!("{message}\n"))?;
        let branch = self.sh(&["git", "branch", "--show-current"])?;
        self.sh(&["git", "add", path])?;
        self.sh(&["git", "commit", "-qm", message])?;
        self.sh(&["git", "push", "-q", "-u", "origin", &branch])?;
        Ok(())
    }

    /// Keep the VERSION of the branch the merge lands on: develop, or the
    /// release a hotfix lands in.
    fn resolve(&self) -> Result<(), String> {
        let branch = self.sh(&["git", "branch", "--show-current"])?;
        let want = match branch.strip_prefix("landing/") {
            Some(landing) => {
                let into = landing.split_once("-into-").map(|(_, into)| into).unwrap_or_default();
                let target = self
                    .refs
                    .keys()
                    .find(|b| b.replace('/', "-") == into)
                    .ok_or(format!("no branch for landing {branch}"))?;
                self.sh(&["git", "show", &format!("origin/{target}:VERSION")])?
            }
            None => self.sh(&["git", "show", ":2:VERSION"])?,
        };
        let side = if self.sh(&["git", "show", ":2:VERSION"])? == want { "--ours" } else { "--theirs" };
        self.sh(&["git", "checkout", side, "VERSION"])?;
        self.sh(&["git", "add", "VERSION"])?;
        self.sh(&["git", "commit", "-q", "--no-edit"])?;
        Ok(())
    }

    /// A reviewer completes the PR on the platform.
    fn merge(&self, number: u64, method: &str) -> Result<(), String> {
        let mut rows = self.pr_rows()?;
        let pr = rows.get_mut(number as usize - 1).ok_or(format!("PR #{number} does not exist"))?;
        if pr["status"] != "active" {
            return Err(format!("PR #{number} is {}, not open", text(&pr["status"])));
        }
        fake::complete_pr(pr, method, &mut |args: &[&str]| self.og(args))?;
        self.write(&self.v.join("prs.json"), &Value::from(rows).to_string())
    }

    // ---------- what origin shows ----------

    fn origin_state(&self) -> Result<(Refs, Refs), String> {
        let out =
            self.og(&["for-each-ref", "--format=%(refname) %(objectname) %(*objectname)", "refs/heads", "refs/tags"])?;
        Ok(parse_refs(&out))
    }

    fn branch_at(&self, sha: &str, prefer: impl Fn(&str) -> bool) -> Option<String> {
        let mut names: Vec<&String> = self.refs.iter().filter(|(_, s)| *s == sha).map(|(b, _)| b).collect();
        names.sort_by_key(|b| (rank(b), (*b).clone()));
        names.iter().find(|b| prefer(b)).or(names.first()).map(|b| (*b).clone())
    }

    fn start_op(&self, branch: &str, tip: &str) -> Value {
        json!({"op": kind_of(branch), "name": branch, "from": self.branch_at(tip, |b| is_base(branch, b))})
    }

    fn parents(&self, commit: &str) -> Result<Vec<String>, String> {
        Ok(self.og(&["rev-list", "--parents", "-n1", commit])?.split_whitespace().skip(1).map(String::from).collect())
    }

    fn message_and_version(&self, commit: &str) -> Result<(String, String), String> {
        Ok((self.og(&["log", "-1", "--format=%s", commit])?, self.og(&["show", &format!("{commit}:VERSION")])?))
    }

    /// The ops since the previous action, in the spec's vocabulary.
    fn ops(&mut self) -> Result<Vec<Value>, String> {
        let (refs, tags) = self.origin_state()?;
        let rows = self.pr_rows()?;
        let completed = |method: &str| -> HashMap<String, &Value> {
            rows.iter()
                .filter(|p| p["status"] == "completed" && p["method"] == method)
                .map(|p| (text(&p["merge"]), p))
                .collect()
        };
        let (squashes, merges) = (completed("squash"), completed("merge"));
        let mut out = vec![];

        let tips: Vec<String> = refs.values().chain(tags.values()).cloned().collect();
        let new: Vec<String> = if tips.is_empty() {
            vec![]
        } else {
            let mut args = vec!["rev-list".to_string(), "--topo-order".into(), "--reverse".into()];
            args.extend(tips);
            args.extend(self.known.iter().map(|k| format!("^{k}")));
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            self.og(&args)?.split_whitespace().map(String::from).collect()
        };
        let mut owner: HashMap<String, String> = HashMap::new();
        let mut by_rank: Vec<(&String, &String)> = refs.iter().collect();
        by_rank.sort_by_key(|(b, _)| (rank(b), (*b).clone()));
        for (branch, tip) in by_rank {
            let mut commit = Some(tip.clone());
            while let Some(c) = commit.filter(|c| !self.known.contains(c)) {
                owner.entry(c.clone()).or_insert_with(|| branch.clone());
                commit = self.parents(&c)?.into_iter().next();
            }
        }
        // A landing branch gflow merged and deleted within this action: its
        // commits sit only behind the merge commit's second parent.
        for p in merges.values() {
            let mut commit = Some(text(&p["head"]));
            while let Some(c) = commit.filter(|c| !self.known.contains(c)) {
                owner.entry(c.clone()).or_insert_with(|| text(&p["source"]));
                commit = self.parents(&c)?.into_iter().next();
            }
        }
        self.add_started(&refs, &mut out);
        // A version branch gflow pushed, merged and deleted within this action.
        for p in &rows {
            let id = p["id"].as_u64().unwrap_or_default();
            let head = text(&p["head"]);
            if p["status"] == "completed"
                && p["method"] == "squash"
                && !self.prs.contains_key(&id)
                && !refs.contains_key(&text(&p["source"]))
                && !self.known.contains(&head)
            {
                let target = text(&p["target"]);
                let target_tip = self.parents(&head)?.into_iter().next().unwrap_or_default();
                if !self.refs.contains_key(&target) {
                    out.push(self.start_op(&target, &target_tip));
                    self.refs.insert(target.clone(), target_tip);
                }
                let source = text(&p["source"]);
                out.push(json!({"op": "branch", "name": source, "from": target}));
                let (message, version) = self.message_and_version(&head)?;
                out.push(json!({"op": "commit", "branch": source, "message": message, "version": version}));
                self.refs.insert(source, head.clone());
                self.known.insert(head);
            }
        }
        for c in new.into_iter().filter(|c| !self.known.contains(c)).collect::<Vec<_>>() {
            let branch = owner.get(&c).cloned().ok_or(format!("commit {c} is on no branch"))?;
            let parents = self.parents(&c)?;
            if !self.refs.contains_key(&branch) {
                out.push(self.start_op(&branch, &parents[0]));
                self.refs.insert(branch.clone(), parents[0].clone());
            }
            let (message, version) = self.message_and_version(&c)?;
            out.push(if let Some(p) = squashes.get(&c) {
                json!({"op": "squash", "from": p["source"], "into": branch, "message": message, "version": version})
            } else if parents.len() == 2 {
                let from = match merges.get(&c) {
                    Some(p) => p["source"].clone(),
                    None => json!(self.branch_at(&parents[1], |_| false)),
                };
                json!({"op": "merge", "from": from, "into": branch, "message": message, "version": version})
            } else {
                json!({"op": "commit", "branch": branch, "message": message, "version": version})
            });
            self.refs.insert(branch, c.clone());
            self.known.insert(c);
        }
        self.add_started(&refs, &mut out);
        for (tag, sha) in &tags {
            if !self.tags.contains_key(tag) {
                let mut candidates: Vec<&String> = self.refs.keys().collect();
                candidates.sort_by_key(|b| (tag_rank(b), (*b).clone()));
                let on = candidates.into_iter().find(|b| &self.refs[*b] == sha);
                out.push(json!({"op": "tag", "name": tag, "on": on}));
                self.tags.insert(tag.clone(), sha.clone());
            }
        }
        for p in &rows {
            let id = p["id"].as_u64().unwrap_or_default();
            let status = text(&p["status"]);
            let old = self.prs.get(&id);
            if old.is_none() {
                let method = if text(&p["source"]).starts_with("landing/") { "merge" } else { "squash" };
                out.push(json!({"op": "openPr", "pr": id, "from": p["source"], "into": p["target"], "method": method}));
            }
            if status == "completed" && old.map(String::as_str) != Some("completed") {
                out.push(json!({"op": "mergedPr", "pr": id}));
            }
            self.prs.insert(id, status);
        }
        let log = std::fs::read_to_string(self.v.join("hooks.log")).unwrap_or_default();
        let lines: Vec<&str> = log.lines().collect();
        out.extend(hook_ops(&lines[self.hooks.min(lines.len())..]));
        self.hooks = lines.len();
        let gone: Vec<String> = self.refs.keys().filter(|b| !refs.contains_key(*b)).cloned().collect();
        for branch in gone {
            out.push(json!({"op": "delete", "name": branch}));
            self.refs.remove(&branch);
        }
        Ok(out)
    }

    /// A branch that appeared on origin at a commit already accounted for.
    fn add_started(&mut self, refs: &Refs, out: &mut Vec<Value>) {
        for (branch, tip) in refs {
            if !self.refs.contains_key(branch) && self.known.contains(tip) {
                out.push(self.start_op(branch, tip));
                self.refs.insert(branch.clone(), tip.clone());
            }
        }
    }

    fn deploy_versions(&self) -> Result<Map<String, Value>, String> {
        let fetch = ["git", "fetch", "-q", "--prune", "--tags", "--force", "origin", "+refs/heads/*:refs/remotes/origin/*"];
        self.sh_in(&self.probe, &fetch)?;
        let (refs, _) = self.origin_state()?;
        let gflow = self.gflow.display().to_string();
        let mut out = Map::new();
        for branch in deploy_branches(&refs) {
            self.sh_in(&self.probe, &["git", "checkout", "-q", "-B", &branch, &format!("origin/{branch}")])?;
            out.insert(branch, Value::from(self.sh_in(&self.probe, &[&gflow, "deploy-version"])?));
        }
        Ok(out)
    }

    // ---------- the replay ----------

    /// Ok when every action matches, else the first mismatch.
    fn check(&mut self) -> Result<(), String> {
        let actions = self.spec["actions"].as_array().cloned().unwrap_or_default();
        for (index, action) in actions.iter().enumerate() {
            let at = format!("action {index} ({})", text(&action["frames"][0]["cmd"]));
            let run = action["run"].as_object().cloned().unwrap_or_default();
            let (kind, args) = run.iter().next().ok_or(format!("{at}: has no run"))?;
            if let Some(on) = action["on"].as_str().filter(|on| !on.is_empty()) {
                self.stand_on(on)?;
            }
            let mut flag = None;
            match kind.as_str() {
                "reset" => self.reset()?,
                "gflow" => {
                    let words: Vec<String> = args.as_array().into_iter().flatten().map(text).collect();
                    flag = self.run_gflow(&words.iter().map(String::as_str).collect::<Vec<_>>())?;
                }
                "commit" => self.commit(&text(&args["path"]), &text(&args["message"]))?,
                "resolve" => self.resolve()?,
                "merge" => self.merge(args["pr"].as_u64().unwrap_or_default(), &text(&args["method"]))?,
                _ => {}
            }
            let frames = action["frames"].as_array().cloned().unwrap_or_default();
            // The reset builds the spec's start; verify only checks.
            let ops = if kind == "reset" || kind == "verify" {
                frames[0]["ops"].as_array().cloned().unwrap_or_default()
            } else {
                self.ops()?
            };
            let expected: Vec<Value> =
                frames.iter().flat_map(|f| f["ops"].as_array().cloned().unwrap_or_default()).collect();
            if let Some(mismatch) = ops_mismatch(&expected, &ops) {
                return Err(format!("{at}: {mismatch}"));
            }
            let expected_flag = expected_flag(action);
            if kind == "gflow" && expected_flag.as_deref() != flag {
                return Err(format!(
                    "{at}: spec says {}, gflow gave {}",
                    expected_flag.as_deref().unwrap_or("no flag"),
                    flag.unwrap_or("no flag")
                ));
            }
            let expected_dv = action["deployVersions"].as_object().cloned().unwrap_or_default();
            let actual_dv = self.deploy_versions()?;
            if expected_dv != actual_dv {
                return Err(format!(
                    "{at}: deploy-version differs\n    spec:  {}\n    gflow: {}",
                    Value::from(expected_dv),
                    Value::from(actual_dv)
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn long_lived_and_gflow_landing_branches_start_others_branch() {
        for branch in ["develop", "release/1.0.0", "hotfix/1.0.1", "landing/release-1.0.0-into-main"] {
            assert_eq!(kind_of(branch), "start", "{branch}");
        }
        for branch in ["feature/a", "release-fix/1.0.0/a", "version/1.0.1-into-hotfix-1.0.1", "main"] {
            assert_eq!(kind_of(branch), "branch", "{branch}");
        }
    }

    #[test]
    fn branches_rank_mainline_first_and_gflow_owned_last() {
        let ranks: Vec<u8> = ["main", "develop", "release/1.0.0", "hotfix/1.0.1", "feature/a", "version/x", "landing/x"]
            .iter()
            .map(|b| rank(b))
            .collect();
        assert_eq!(ranks, [0, 1, 2, 3, 4, 5, 5]);
    }

    #[test]
    fn a_tag_prefers_a_release_or_hotfix_branch_over_the_mainline() {
        let ranks: Vec<u8> =
            ["release/1.0.0", "hotfix/1.0.1", "main", "develop", "feature/a"].iter().map(|b| tag_rank(b)).collect();
        assert_eq!(ranks, [0, 1, 2, 3, 4]);
    }

    #[test]
    fn each_new_branch_is_cut_from_the_branch_its_name_implies() {
        let cases = [
            ("release/1.0.0", "develop", true),
            ("feature/a", "develop", true),
            ("refactor/a", "main", false),
            ("hotfix/1.0.1", "main", true),
            ("hotfix/1.0.1", "develop", false),
            ("release-fix/1.0.0/a", "release/1.0.0", true),
            ("release-fix/1.0.0/a", "release/2.0.0", false),
            ("hotfix-fix/1.0.1/a", "hotfix/1.0.1", true),
            ("version/1.0.1-into-release-1.0.0", "release/1.0.0", true),
            ("version/1.0.1-into-release-1.0.0", "hotfix/1.0.1", false),
            ("landing/hotfix-1.0.1-into-release-1.0.0", "hotfix/1.0.1", true),
            ("landing/hotfix-1.0.1-into-release-1.0.0", "release/1.0.0", false),
            ("develop", "main", false),
        ];
        for (new, candidate, want) in cases {
            assert_eq!(is_base(new, candidate), want, "{new} from {candidate}");
        }
    }

    #[test]
    fn a_left_merge_is_a_conflict_whatever_the_exit_code() {
        assert_eq!(flag(true, false, ""), Some("conflict"));
        assert_eq!(flag(true, true, "✔ Release complete"), Some("conflict"));
    }

    #[test]
    fn a_failed_run_is_a_refusal() {
        assert_eq!(flag(false, false, "✔ Release complete"), Some("refuse"));
    }

    #[test]
    fn a_run_is_done_only_when_a_check_line_says_complete() {
        assert_eq!(flag(false, true, "Merging...\n✔ Hotfix COMPLETE."), Some("done"));
        assert_eq!(flag(false, true, "complete ✔"), None);
        assert_eq!(flag(false, true, "✔ PR opened"), None);
    }

    #[test]
    fn the_expected_flag_is_the_last_frames_flag_before_its_colon() {
        let action = json!({"frames": [{"flag": "refuse: ignored"}, {"flag": "done: release finished"}]});
        assert_eq!(expected_flag(&action), Some("done".into()));
        assert_eq!(expected_flag(&json!({"frames": [{"flag": ""}]})), None);
        assert_eq!(expected_flag(&json!({"frames": [{}]})), None);
    }

    #[test]
    fn ops_match_in_any_order_and_any_key_order() {
        let expected = [json!({"op": "tag", "name": "v1", "on": "main"}), json!({"op": "delete", "name": "a"})];
        let actual = [json!({"name": "a", "op": "delete"}), json!({"on": "main", "op": "tag", "name": "v1"})];
        assert_eq!(ops_mismatch(&expected, &actual), None);
    }

    #[test]
    fn differing_ops_are_listed_per_side_with_sorted_keys() {
        let expected = [json!({"op": "delete", "name": "a"}), json!({"op": "delete", "name": "b"})];
        let actual = [json!({"op": "delete", "name": "a"}), json!({"op": "tag", "on": "main", "name": "v1"})];
        assert_eq!(
            ops_mismatch(&expected, &actual).unwrap(),
            "ops differ\n    spec only:  [{\"name\":\"b\",\"op\":\"delete\"}]\n    gflow only: [{\"name\":\"v1\",\"on\":\"main\",\"op\":\"tag\"}]"
        );
    }

    #[test]
    fn a_duplicated_op_is_a_difference() {
        let op = json!({"op": "delete", "name": "a"});
        assert!(ops_mismatch(std::slice::from_ref(&op), &[op.clone(), op.clone()]).is_some());
    }

    #[test]
    fn refs_split_into_branches_and_tags_with_annotated_tags_peeled() {
        let (branches, tags) = parse_refs(
            "refs/heads/develop d1 \nrefs/heads/release/1.0.0 r1 \nrefs/tags/v1.0.0-rc.1 l1 \nrefs/tags/v1.0.0 a1 c1",
        );
        assert_eq!(branches, Refs::from([("develop".into(), "d1".into()), ("release/1.0.0".into(), "r1".into())]));
        assert_eq!(tags, Refs::from([("v1.0.0-rc.1".into(), "l1".into()), ("v1.0.0".into(), "c1".into())]));
    }

    #[test]
    fn no_refs_parse_to_nothing() {
        assert_eq!(parse_refs(""), (Refs::new(), Refs::new()));
    }

    #[test]
    fn each_hook_log_line_is_one_hook_op() {
        assert_eq!(
            hook_ops(&["release-start v1.0.0-rc.1 rc release/1.0.0"]),
            [json!({"op": "hook", "event": "release-start", "tag": "v1.0.0-rc.1", "kind": "rc", "branch": "release/1.0.0"})]
        );
    }

    #[test]
    fn deploy_version_is_asked_on_develop_first_then_by_name() {
        let refs: Refs = ["main", "hotfix/1.0.1", "develop", "feature/a", "landing/x"]
            .into_iter()
            .map(|b| (b.to_string(), "sha".to_string()))
            .collect();
        assert_eq!(deploy_branches(&refs), ["develop", "feature/a", "hotfix/1.0.1"]);
    }
}
