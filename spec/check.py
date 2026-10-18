#!/usr/bin/env python3
"""Replays spec/cycles/*.json against a real gflow binary and reports the
first action whose effect on origin differs from the spec, per cycle.

Each cycle runs in its own temporary directory: a bare origin, a working
clone, and fake `ssh`, `az`, `open`, and `pbcopy`, so gflow sees an Azure
DevOps remote. Every action switches to its `on` branch and executes its
`run`; the check then compares the ops on origin (branches, commits,
merges, VERSION, tags, PRs, hooks) and what `gflow deploy-version` prints
on each open branch. Texts are not compared.

Usage: python3 spec/check.py [--gflow PATH] [cycle-name ...]
Without --gflow it builds target/release/gflow first. Needs git 2.38+.
"""
import argparse
import hashlib
import json
import os
import subprocess
import sys
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

SPEC = Path(__file__).parent
ROOT = SPEC.parent
HOOK = 'echo "$GFLOW_EVENT $GFLOW_TAG $GFLOW_TAG_KIND $GFLOW_BRANCH" >> "$V/hooks.log"'
HOOK_EVENTS = ("release-start", "release-bump", "release-finish", "hotfix-finish")
DEPLOYABLE = ("develop", "release/", "hotfix/", "feature/", "fix/", "chore/", "docs/", "refactor/", "release-fix/", "hotfix-fix/")
REMOTE = "git@ssh.dev.azure.com:v3/org/proj/repo"

FAKE_SSH = """#!/bin/sh
while [ $# -gt 0 ]; do case "$1" in -o|-p|-i|-l) shift 2;; -*) shift;; *) break;; esac; done
shift
cmd=$(printf '%s' "$*" | sed "s#'v3/org/proj/repo'#'$V/origin.git'#; s#'/v3/org/proj/repo'#'$V/origin.git'#")
exec sh -c "$cmd"
"""

# Pull requests live in $V/prs.json. A merge now squashes on origin at once,
# like the platform does; reviewers' merges are done by the checker.
FAKE_AZ = """#!/usr/bin/env python3
import json, os, subprocess, sys
V = os.environ["V"]; path = f"{V}/prs.json"
prs = json.load(open(path)) if os.path.exists(path) else []
a = sys.argv[1:]
def opt(name):
    return a[a.index(name) + 1] if name in a else None
def og(*args):
    return subprocess.run(["git", "-C", f"{V}/origin.git", *args], check=True, capture_output=True, text=True).stdout.strip()
if a[:2] == ["extension", "show"]:
    sys.exit(0)
if a[:3] == ["repos", "pr", "list"]:
    t, s = opt("--target-branch"), opt("--source-branch")
    for p in reversed(prs):
        if (t and p["target"] != t) or (s and p["source"] != s):
            continue
        print("\\t".join([p["status"], "refs/heads/" + p["source"], "refs/heads/" + p["target"], p.get("head", ""), p.get("merge", ""), str(p["id"])]))
    sys.exit(0)
if a[:3] == ["repos", "pr", "create"]:
    p = {"id": len(prs) + 1, "status": "active", "source": opt("--source-branch"), "target": opt("--target-branch"), "title": opt("--title")}
    prs.append(p); json.dump(prs, open(path, "w")); print(p["id"]); sys.exit(0)
if a[:3] == ["repos", "pr", "update"]:
    p = prs[int(opt("--id")) - 1]
    if opt("--auto-complete") == "true":
        p["auto"] = True
    if opt("--status") == "completed":
        src, tgt = og("rev-parse", "refs/heads/" + p["source"]), og("rev-parse", "refs/heads/" + p["target"])
        tree = og("merge-tree", "--write-tree", tgt, src).splitlines()[0]
        sha = og("commit-tree", tree, "-p", tgt, "-m", f"{p['title']} (#{p['id']})")
        og("update-ref", "refs/heads/" + p["target"], sha)
        p.update(status="completed", head=src, merge=sha, method="squash")
        if opt("--delete-source-branch") == "true":
            og("update-ref", "-d", "refs/heads/" + p["source"])
    json.dump(prs, open(path, "w")); sys.exit(0)
print("fake az: unsupported " + " ".join(a), file=sys.stderr); sys.exit(2)
"""


class Mismatch(Exception):
    pass


def kind_of(branch):
    return "start" if branch.split("/")[0] in ("release", "hotfix", "landing", "develop") else "branch"


def rank(branch):
    for i, prefix in enumerate(("main", "develop", "release/", "hotfix/")):
        if branch.startswith(prefix):
            return i
    return 5 if branch.startswith(("version/", "landing/")) else 4


def base_for(new):
    """Which existing branch gflow cuts `new` from, by its name."""
    flat = lambda b: b.replace("/", "-")
    kind = new.split("/")[0]
    if kind in ("release", "feature", "fix", "chore", "docs", "refactor"):
        return lambda b: b == "develop"
    if kind == "hotfix":
        return lambda b: b == "main"
    if kind in ("release-fix", "hotfix-fix"):
        parent = ("release/" if kind == "release-fix" else "hotfix/") + new.split("/")[1]
        return lambda b: b == parent
    if kind == "version":
        return lambda b: flat(b) == new.split("-into-", 1)[1]
    if kind == "landing":
        return lambda b: flat(b) == new[len("landing/"):].split("-into-", 1)[0]
    return lambda b: False


class Cycle:
    """One spec cycle replayed in its own temporary repository."""

    def __init__(self, path, gflow, tmp):
        self.name = path.stem
        self.spec = json.loads(path.read_text())
        self.gflow = gflow
        # Resolved: gflow keys hook approvals by the real repository path,
        # and macOS temp dirs sit behind a /var -> /private/var symlink.
        self.v = Path(tmp).resolve() / self.name
        (self.v / "home" / ".gflow").mkdir(parents=True)
        (self.v / "bin").mkdir()
        self.origin, self.work, self.probe = self.v / "origin.git", self.v / "work", self.v / "probe"
        self.refs, self.tags, self.known, self.prs, self.hooks = {}, {}, set(), {}, 0
        gitconfig = self.v / "home" / ".gitconfig"
        gitconfig.write_text("[user]\n\tname = Dev\n\temail = dev@example.com\n[init]\n\tdefaultBranch = main\n[advice]\n\tdetachedHead = false\n")
        for name, body in [("fake-ssh", FAKE_SSH), ("az", FAKE_AZ)] + [(tool, "#!/bin/sh\nexit 0\n") for tool in ("open", "pbcopy", "xdg-open", "code")]:
            (self.v / "bin" / name).write_text(body)
            (self.v / "bin" / name).chmod(0o755)
        self.env = {**os.environ, "V": str(self.v), "HOME": str(self.v / "home"), "GIT_CONFIG_GLOBAL": str(gitconfig),
                    "GIT_SSH_COMMAND": str(self.v / "bin" / "fake-ssh"), "PATH": f"{self.v / 'bin'}:{os.environ['PATH']}",
                    "GIT_EDITOR": "true", "NO_COLOR": "1"}

    # ---------- shell ----------
    def sh(self, *args, cwd=None, check=True):
        r = subprocess.run(args, cwd=cwd or self.work, env=self.env, capture_output=True, text=True, stdin=subprocess.DEVNULL)
        if check and r.returncode:
            raise Mismatch(f"{' '.join(args)} failed: {r.stdout}{r.stderr}")
        return r.stdout.strip()

    def og(self, *args):
        return self.sh("git", *args, cwd=self.origin)

    def pr_rows(self):
        path = self.v / "prs.json"
        return json.loads(path.read_text()) if path.exists() else []

    # ---------- runs ----------
    def reset(self):
        settings = self.spec["settings"]
        subprocess.run(["git", "init", "-q", "--bare", str(self.origin)], env=self.env, check=True)
        self.sh("git", "clone", "-q", REMOTE, str(self.work), cwd=self.v)
        (self.work / ".gflow").mkdir()
        (self.work / ".gflow" / "config").write_text("".join(f"{k}={settings[k]}\n" for k in ("mode", "bump-strategy", "keep-release-branches"))
                                                    + "".join(f"hook-{e}={HOOK}\n" for e in HOOK_EVENTS))
        (self.work / ".gflow" / "set-version.sh").write_text('#!/bin/sh\nprintf \'%s\\n\' "$1" > VERSION\n')
        (self.work / ".gflow" / "set-version.sh").chmod(0o755)
        (self.work / "VERSION").write_text("0.0.0\n")
        # As a yes at the trust prompt would.
        (self.v / "home" / ".gflow" / "hook-approvals").write_text(f"{hashlib.sha256(HOOK.encode()).hexdigest()} {self.work}\n")
        self.sh("git", "add", ".")
        self.sh("git", "commit", "-qm", "Initial commit")
        self.sh("git", "push", "-q", "origin", "main", "main:develop")
        self.sh("git", "switch", "-q", "-c", "develop", "--track", "origin/develop")
        self.sh("git", "clone", "-q", REMOTE, str(self.probe), cwd=self.v)
        self.sh("git", "config", "gflow.branch.main", "main", cwd=self.probe)
        refs, _ = self.origin_state()
        self.refs.update(refs)
        self.known.add(refs["main"])
        return None

    def has_ref(self, ref):
        return subprocess.run(["git", "show-ref", "--verify", "--quiet", ref], cwd=self.work, env=self.env).returncode == 0

    def stand_on(self, branch):
        """The developer switches to `branch`, up to date with origin."""
        self.sh("git", "fetch", "-q", "origin")
        if self.sh("git", "branch", "--show-current") != branch:
            if self.has_ref(f"refs/heads/{branch}"):
                self.sh("git", "switch", "-q", branch)
            else:
                self.sh("git", "switch", "-q", "-c", branch, "--track", f"origin/{branch}")
        if self.has_ref(f"refs/remotes/origin/{branch}"):
            # Already up to date, or ahead with a local merge resolution.
            self.sh("git", "merge", "-q", "--ff-only", f"origin/{branch}", check=False)

    def run_gflow(self, args):
        time.sleep(0.2)
        r = subprocess.run([self.gflow, *args], cwd=self.work, env=self.env, capture_output=True, text=True, stdin=subprocess.DEVNULL)
        time.sleep(0.3)  # the background fetch
        out = (r.stdout + r.stderr).strip()
        if (self.work / ".git" / "MERGE_HEAD").exists():
            return "conflict"
        if r.returncode:
            return "refuse"
        if any(l.startswith("✔") and "complete" in l.lower() for l in out.splitlines()):
            return "done"
        return None

    def commit(self, path, message):
        (self.work / path).parent.mkdir(parents=True, exist_ok=True)
        (self.work / path).write_text(message + "\n")
        branch = self.sh("git", "branch", "--show-current")
        self.sh("git", "add", path)
        self.sh("git", "commit", "-qm", message)
        self.sh("git", "push", "-q", "-u", "origin", branch)

    def resolve(self):
        """Keep the VERSION of the branch the merge lands on: develop, or
        the release a hotfix lands in."""
        branch = self.sh("git", "branch", "--show-current")
        landing = branch.startswith("landing/")
        if landing:
            target = next(b for b in self.refs if b.replace("/", "-") == branch.split("-into-", 1)[1])
            want = self.sh("git", "show", f"origin/{target}:VERSION")
        else:
            want = self.sh("git", "show", ":2:VERSION")
        side = "--ours" if self.sh("git", "show", ":2:VERSION") == want else "--theirs"
        self.sh("git", "checkout", side, "VERSION")
        self.sh("git", "add", "VERSION")
        self.sh("git", "commit", "-q", "--no-edit")

    def merge(self, number, method):
        """A reviewer completes the PR on the platform."""
        rows = self.pr_rows()
        p = rows[number - 1]
        if p["status"] != "active":
            raise Mismatch(f"PR #{number} is {p['status']}, not open")
        src, tgt = self.og("rev-parse", f"refs/heads/{p['source']}"), self.og("rev-parse", f"refs/heads/{p['target']}")
        tree = self.og("merge-tree", "--write-tree", tgt, src).splitlines()[0]
        parents = ["-p", tgt] + ([] if method == "squash" else ["-p", src])
        title = f"{p['title']} (#{number})" if method == "squash" else f"Merge PR #{number}: {p['title']}"
        sha = self.og("commit-tree", tree, *parents, "-m", title)
        self.og("update-ref", f"refs/heads/{p['target']}", sha)
        p.update(status="completed", head=src, merge=sha, method=method)
        (self.v / "prs.json").write_text(json.dumps(rows))

    # ---------- what origin shows ----------
    def origin_state(self):
        refs, tags = {}, {}
        for line in self.og("for-each-ref", "--format=%(refname) %(objectname) %(*objectname)", "refs/heads", "refs/tags").splitlines():
            name, sha, *peeled = line.split(" ")
            if name.startswith("refs/heads/"):
                refs[name[11:]] = sha
            else:
                tags[name[10:]] = peeled[0] if peeled and peeled[0] else sha
        return refs, tags

    def branch_at(self, sha, prefer=None):
        names = sorted((b for b, s in self.refs.items() if s == sha), key=lambda b: (rank(b), b))
        for b in names:
            if prefer and prefer(b):
                return b
        return names[0] if names else None

    def ops(self):
        """The ops since the previous action, in the spec's vocabulary."""
        refs, tags = self.origin_state()
        rows = self.pr_rows()
        out = []
        squashes = {p["merge"]: p for p in rows if p["status"] == "completed" and p.get("method") == "squash"}
        merges = {p["merge"]: p for p in rows if p["status"] == "completed" and p.get("method") == "merge"}
        tips = list(refs.values()) + list(tags.values())
        new = self.og("rev-list", "--topo-order", "--reverse", *tips, *[f"^{k}" for k in self.known]).split() if tips else []
        owner = {}
        for b, tip in sorted(refs.items(), key=lambda kv: (rank(kv[0]), kv[0])):
            c = tip
            while c and c not in self.known:
                owner.setdefault(c, b)
                ps = self.og("rev-list", "--parents", "-n1", c).split()[1:]
                c = ps[0] if ps else None
        parents = lambda c: self.og("rev-list", "--parents", "-n1", c).split()[1:]
        version = lambda c: self.og("show", f"{c}:VERSION")
        message = lambda c: self.og("log", "-1", "--format=%s", c)
        for b, tip in refs.items():
            if b not in self.refs and tip in self.known:
                out.append({"op": kind_of(b), "name": b, "from": self.branch_at(tip, base_for(b))})
                self.refs[b] = tip
        # A version branch gflow pushed, merged and deleted within this action.
        for p in rows:
            if p["status"] == "completed" and p["id"] not in self.prs and p["source"] not in refs and p["head"] not in self.known:
                head = p["head"]
                target_tip = parents(head)[0]
                if p["target"] not in self.refs:
                    out.append({"op": kind_of(p["target"]), "name": p["target"], "from": self.branch_at(target_tip, base_for(p["target"]))})
                    self.refs[p["target"]] = target_tip
                out.append({"op": "branch", "name": p["source"], "from": p["target"]})
                out.append({"op": "commit", "branch": p["source"], "message": message(head), "version": version(head)})
                self.refs[p["source"]] = head
                self.known.add(head)
        for c in [c for c in new if c not in self.known]:
            b = owner[c]
            ps = parents(c)
            if b not in self.refs:
                out.append({"op": kind_of(b), "name": b, "from": self.branch_at(ps[0], base_for(b))})
                self.refs[b] = ps[0]
            if c in squashes:
                out.append({"op": "squash", "from": squashes[c]["source"], "into": b, "message": message(c), "version": version(c)})
            elif len(ps) == 2:
                src = merges[c]["source"] if c in merges else self.branch_at(ps[1])
                out.append({"op": "merge", "from": src, "into": b, "message": message(c), "version": version(c)})
            else:
                out.append({"op": "commit", "branch": b, "message": message(c), "version": version(c)})
            self.refs[b] = c
            self.known.add(c)
        for b, tip in refs.items():
            if b not in self.refs and tip in self.known:
                out.append({"op": kind_of(b), "name": b, "from": self.branch_at(tip, base_for(b))})
                self.refs[b] = tip
        tag_rank = lambda b: {2: 0, 3: 1, 0: 2, 1: 3}.get(rank(b), 4)
        for t, sha in tags.items():
            if t not in self.tags:
                on = next((b for b in sorted(self.refs, key=lambda b: (tag_rank(b), b)) if self.refs[b] == sha), None)
                out.append({"op": "tag", "name": t, "on": on})
                self.tags[t] = sha
        for p in rows:
            old = self.prs.get(p["id"])
            if old is None:
                method = "merge" if p["source"].startswith("landing/") else "squash"
                out.append({"op": "openPr", "pr": p["id"], "from": p["source"], "into": p["target"], "method": method})
            if p["status"] == "completed" and old != "completed":
                out.append({"op": "mergedPr", "pr": p["id"]})
            self.prs[p["id"]] = p["status"]
        log = self.v / "hooks.log"
        lines = log.read_text().splitlines() if log.exists() else []
        for line in lines[self.hooks:]:
            event, tag, kind, branch = line.split(" ")
            out.append({"op": "hook", "event": event, "tag": tag, "kind": kind, "branch": branch})
        self.hooks = len(lines)
        for b in list(self.refs):
            if b not in refs:
                out.append({"op": "delete", "name": b})
                del self.refs[b]
        return out

    def deploy_versions(self):
        self.sh("git", "fetch", "-q", "--prune", "--tags", "--force", "origin", "+refs/heads/*:refs/remotes/origin/*", cwd=self.probe)
        refs, _ = self.origin_state()
        out = {}
        for b in sorted(refs, key=lambda b: (b != "develop", b)):
            if b == "develop" or b.startswith(DEPLOYABLE[1:]):
                self.sh("git", "checkout", "-q", "-B", b, f"origin/{b}", cwd=self.probe)
                out[b] = self.sh(self.gflow, "deploy-version", cwd=self.probe)
        return out

    # ---------- the replay ----------
    def check(self):
        """None when every action matches, else the first mismatch."""
        try:
            for index, action in enumerate(self.spec["actions"]):
                where = f"action {index} ({action['frames'][0]['cmd']})"
                kind, args = next(iter(action["run"].items()))
                if action.get("on"):
                    self.stand_on(action["on"])
                flag = None
                if kind == "reset":
                    self.reset()
                elif kind == "gflow":
                    flag = self.run_gflow(args)
                elif kind == "commit":
                    self.commit(args["path"], args["message"])
                elif kind == "resolve":
                    self.resolve()
                elif kind == "merge":
                    self.merge(args["pr"], args["method"])
                # The reset builds the spec's start; verify only checks.
                ops = action["frames"][0]["ops"] if kind in ("reset", "verify") else self.ops()
                expected_ops = sorted(json.dumps(o, sort_keys=True) for f in action["frames"] for o in f["ops"])
                actual_ops = sorted(json.dumps(o, sort_keys=True) for o in ops)
                if expected_ops != actual_ops:
                    only_spec = sorted(set(expected_ops) - set(actual_ops))
                    only_gflow = sorted(set(actual_ops) - set(expected_ops))
                    raise Mismatch(f"{where}: ops differ\n    spec only:  {only_spec}\n    gflow only: {only_gflow}")
                expected_flag = action["frames"][-1].get("flag", "").split(":")[0] or None
                if kind == "gflow" and expected_flag != flag:
                    raise Mismatch(f"{where}: spec says {expected_flag or 'no flag'}, gflow gave {flag or 'no flag'}")
                expected_dv, actual_dv = action.get("deployVersions", {}), self.deploy_versions()
                if expected_dv != actual_dv:
                    raise Mismatch(f"{where}: deploy-version differs\n    spec:  {expected_dv}\n    gflow: {actual_dv}")
        except Mismatch as error:
            return str(error)
        return None


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--gflow", help="the gflow binary to check (default: build target/release/gflow)")
    parser.add_argument("cycles", nargs="*", help="cycle names, e.g. protected-rc-delete (default: all)")
    options = parser.parse_args()
    gflow = options.gflow
    if not gflow:
        subprocess.run(["cargo", "build", "--release", "--quiet"], cwd=ROOT, check=True)
        gflow = str(ROOT / "target" / "release" / "gflow")
    paths = [p for p in sorted((SPEC / "cycles").glob("*.json")) if not options.cycles or p.stem in options.cycles]
    with tempfile.TemporaryDirectory(prefix="gflow-spec-") as tmp:
        with ThreadPoolExecutor() as pool:
            results = list(pool.map(lambda p: (p.stem, Cycle(p, os.path.abspath(gflow), tmp).check()), paths))
    failed = [(name, result) for name, result in results if result]
    for name, result in results:
        print(f"{'✔' if not result else '✖'} {name}" + (f"\n  {result}" if result else ""))
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
