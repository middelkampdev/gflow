#![cfg(unix)]

mod common;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const HOOK: &str = ".claude/hooks/tdd-gate.sh";
const BASELINE: &str = ".claude/hooks/coverage-baseline.txt";
const UNTRACKED: &str = "tests/new coverage\ncase.rs";
const FULL_COVERAGE: &str = r#"{"data":[{"totals":{"lines":{"percent":100,"count":100,"covered":100},"functions":{"percent":100,"count":10,"covered":10},"regions":{"percent":100,"count":200,"covered":200}}}]}"#;
const LOW_LINE_COVERAGE: &str = r#"{"data":[{"totals":{"lines":{"percent":99,"count":100,"covered":99},"functions":{"percent":100,"count":10,"covered":10},"regions":{"percent":100,"count":200,"covered":200}}}]}"#;

struct GateFixture {
    root: common::TempDir,
}

impl GateFixture {
    fn new() -> Option<Self> {
        let jq = std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join("jq"))
                .find(|path| path.is_file())
        });
        let Some(jq) = jq else {
            eprintln!(
                "Skipping coverage gate test: jq is not installed; the hook also skips without jq."
            );
            return None;
        };
        let root = common::tmp_dir("gflow-coverage-gate");
        for path in ["bin", "home", ".claude/hooks", "tests"] {
            fs::create_dir_all(root.join(path)).unwrap();
        }
        let fixture = Self { root };
        std::os::unix::fs::symlink(jq, fixture.root.join("bin/jq")).unwrap();
        fixture.write(HOOK, include_str!("../.claude/hooks/tdd-gate.sh"));
        fixture.write(BASELINE, "100.00\n");
        fixture.write("coverage-report", FULL_COVERAGE);
        fixture.write("lock-identity", "lock-1\n");
        fixture.write(UNTRACKED, "fn original_test() {}\n");
        fixture.write("untracked-paths", &format!("{UNTRACKED}\0"));
        fixture.executable(
            "git",
            r#"
case "$1" in
  rev-parse)
    shift
    for ref in "$@"; do
      case "$ref" in
        --show-toplevel) pwd ;;
        HEAD:src) printf 'source-tree\n' ;;
        HEAD:tests) printf 'test-tree\n' ;;
        HEAD:Cargo.toml) printf 'manifest\n' ;;
        HEAD:Cargo.lock) cat "$GFLOW_GATE_FIXTURE/lock-identity" ;;
        *) exit 97 ;;
      esac
    done
    ;;
  diff) ;;
  status) printf '?? "tests/new coverage\\ncase.rs"\n' ;;
  ls-files) cat "$GFLOW_GATE_FIXTURE/untracked-paths" ;;
  *) exit 97 ;;
esac
"#,
        );
        fixture.executable(
            "cargo",
            r#"
target="${CARGO_TARGET_DIR:-$(pwd -P)/target}"
case "$*" in
  'llvm-cov --json --summary-only')
    printf 'json %s\n' "$target" >> "$GFLOW_GATE_FIXTURE/coverage-runs"
    sleep "${GFLOW_GATE_DELAY:-0}"
    cat "$GFLOW_GATE_FIXTURE/coverage-report"
    ;;
  'llvm-cov report --summary-only')
    printf 'report %s\n' "$target" >> "$GFLOW_GATE_FIXTURE/coverage-runs"
    ;;
  *) exit 97 ;;
esac
"#,
        );
        fixture.executable("cargo-llvm-cov", "exit 97");
        Some(fixture)
    }

    fn write(&self, path: &str, contents: &str) {
        fs::write(self.root.join(path), contents).unwrap();
    }

    fn executable(&self, name: &str, body: &str) {
        let path = self.root.join("bin").join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn gate(&self, project_dir: &Path) -> Command {
        let path = std::env::join_paths([
            self.root.join("bin"),
            PathBuf::from("/usr/bin"),
            PathBuf::from("/bin"),
        ])
        .unwrap();
        let mut gate = Command::new("/bin/bash");
        gate.arg(self.root.join(HOOK))
            .current_dir(&*self.root)
            .env("CLAUDE_PROJECT_DIR", project_dir)
            .env("GFLOW_GATE_FIXTURE", &*self.root)
            .env("HOME", self.root.join("home"))
            .env("PATH", path)
            .env_remove("CARGO_TARGET_DIR");
        gate
    }

    fn run(&self) -> String {
        assert_quiet_success(self.gate(&self.root).output().unwrap())
    }

    fn coverage_log(&self) -> Vec<String> {
        fs::read_to_string(self.root.join("coverage-runs"))
            .unwrap()
            .lines()
            .map(String::from)
            .collect()
    }

    fn coverage_runs(&self) -> usize {
        self.coverage_log()
            .iter()
            .filter(|call| call.starts_with("json "))
            .count()
    }

    fn assert_coverage_ran_in_its_own_target(&self) {
        let target = fs::canonicalize(&*self.root).unwrap().join("target");
        assert_eq!(self.coverage_log(), [format!("json {}", target.display())]);
    }

    fn assert_changed_state_blocks_low_coverage(&self) {
        self.write("coverage-report", LOW_LINE_COVERAGE);
        assert!(self.run().contains("\"decision\": \"block\""));
        assert_eq!(self.coverage_runs(), 2);
        assert_eq!(
            fs::read_to_string(self.root.join(BASELINE)).unwrap(),
            "100.00\n"
        );
    }
}

fn assert_quiet_success(output: Output) -> String {
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

// A session that enters a worktree keeps CLAUDE_PROJECT_DIR on the main
// checkout; only the hook's working directory points at the session's tree.
#[test]
fn the_gate_ratchets_the_tree_it_runs_in_not_claude_project_dir() {
    let (Some(main), Some(worktree)) = (GateFixture::new(), GateFixture::new()) else {
        return;
    };
    for tree in [&main, &worktree] {
        tree.write(BASELINE, "99.00\n");
    }
    let output = assert_quiet_success(worktree.gate(&main.root).output().unwrap());
    assert!(output.contains("coverage ratcheted"), "{output}");
    assert_eq!(
        fs::read_to_string(worktree.root.join(BASELINE)).unwrap(),
        "100.00\n"
    );
    assert_eq!(
        fs::read_to_string(main.root.join(BASELINE)).unwrap(),
        "99.00\n"
    );
}

#[test]
fn parallel_gates_in_two_worktrees_never_share_a_target_dir() {
    let (Some(main), Some(worktree)) = (GateFixture::new(), GateFixture::new()) else {
        return;
    };
    let gates: Vec<_> = [&main, &worktree]
        .map(|tree| {
            tree.gate(&main.root)
                .env("GFLOW_GATE_DELAY", "1")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .into_iter()
        .collect();
    for gate in gates {
        assert!(assert_quiet_success(gate.wait_with_output().unwrap()).is_empty());
    }
    main.assert_coverage_ran_in_its_own_target();
    worktree.assert_coverage_ran_in_its_own_target();
}

#[test]
fn unchanged_rust_and_policy_state_reuses_a_successful_gate_result() {
    let Some(fixture) = GateFixture::new() else {
        return;
    };
    assert!(fixture.run().is_empty());
    assert!(fixture.run().is_empty());
    assert_eq!(fixture.coverage_runs(), 1);
}

#[test]
fn editing_an_existing_untracked_rust_file_invalidates_cached_coverage() {
    let Some(fixture) = GateFixture::new() else {
        return;
    };
    assert!(fixture.run().is_empty());
    fixture.write(UNTRACKED, "fn changed_test() { panic!(\"regression\"); }\n");
    fixture.assert_changed_state_blocks_low_coverage();
}

#[test]
fn a_committed_lockfile_change_invalidates_cached_coverage() {
    let Some(fixture) = GateFixture::new() else {
        return;
    };
    assert!(fixture.run().is_empty());
    fixture.write("lock-identity", "lock-2\n");
    fixture.assert_changed_state_blocks_low_coverage();
}

#[test]
fn a_changed_gate_policy_invalidates_cached_coverage() {
    let Some(fixture) = GateFixture::new() else {
        return;
    };
    assert!(fixture.run().is_empty());
    let mut hook = fs::read_to_string(fixture.root.join(HOOK)).unwrap();
    hook.push_str("\n# Updated gate policy.\n");
    fixture.write(HOOK, &hook);
    fixture.assert_changed_state_blocks_low_coverage();
}

#[test]
fn a_baseline_file_change_invalidates_cached_coverage() {
    let Some(fixture) = GateFixture::new() else {
        return;
    };
    assert!(fixture.run().is_empty());
    fixture.write(BASELINE, "100.000\n");
    fixture.write("coverage-report", LOW_LINE_COVERAGE);
    assert!(fixture.run().contains("\"decision\": \"block\""));
    assert_eq!(fixture.coverage_runs(), 2);
    assert_eq!(
        fs::read_to_string(fixture.root.join(BASELINE)).unwrap(),
        "100.000\n"
    );
}

#[test]
fn ratcheting_the_baseline_caches_the_new_baseline() {
    let Some(fixture) = GateFixture::new() else {
        return;
    };
    fixture.write(BASELINE, "99.00\n");
    assert!(fixture.run().contains("coverage ratcheted"));
    assert!(fixture.run().is_empty());
    assert_eq!(fixture.coverage_runs(), 1);
    assert_eq!(
        fs::read_to_string(fixture.root.join(BASELINE)).unwrap(),
        "100.00\n"
    );
}

#[test]
fn each_metric_requires_every_item_covered_even_when_percentages_round_to_100() {
    for (metric, report) in [
        (
            "lines",
            r#"{"data":[{"totals":{"lines":{"percent":99.999,"count":100000,"covered":99999},"functions":{"percent":100,"count":10,"covered":10},"regions":{"percent":100,"count":200,"covered":200}}}]}"#,
        ),
        (
            "functions",
            r#"{"data":[{"totals":{"lines":{"percent":100,"count":100,"covered":100},"functions":{"percent":100,"count":100000,"covered":99999},"regions":{"percent":100,"count":200,"covered":200}}}]}"#,
        ),
        (
            "regions",
            r#"{"data":[{"totals":{"lines":{"percent":100,"count":100,"covered":100},"functions":{"percent":100,"count":10,"covered":10},"regions":{"percent":100,"count":100000,"covered":99999}}}]}"#,
        ),
    ] {
        let Some(fixture) = GateFixture::new() else {
            return;
        };
        fixture.write("coverage-report", report);
        let output = fixture.run();
        assert!(
            output.contains("\"decision\": \"block\""),
            "{metric}: {output}"
        );
        assert!(!fixture.root.join(".claude/hooks/.tdd-gate-pass").exists());
        assert_eq!(
            fs::read_to_string(fixture.root.join(BASELINE)).unwrap(),
            "100.00\n"
        );
    }
}

#[test]
fn invalid_coverage_reports_cannot_create_a_cached_pass() {
    for report in [
        "not json",
        r#"{"data":[{"totals":{"lines":{"percent":100,"count":100,"covered":100},"functions":{"percent":100,"count":10,"covered":10}}}]}"#,
        r#"{"data":[{"totals":{"lines":{"percent":100,"count":"100","covered":"100"},"functions":{"percent":100,"count":10,"covered":10},"regions":{"percent":100,"count":200,"covered":200}}}]}"#,
        r#"{"data":[{"totals":{"lines":{"percent":100,"count":100,"covered":100},"functions":{"percent":100,"count":-1,"covered":-1},"regions":{"percent":100,"count":200,"covered":200}}}]}"#,
        r#"{"data":[{"totals":{"lines":{"percent":100,"count":100,"covered":100},"functions":{"percent":100,"count":10,"covered":10},"regions":{"percent":100,"count":1.5,"covered":1.5}}}]}"#,
        r#"{"data":[{"totals":{"lines":{"count":100,"covered":100},"functions":{"percent":100,"count":10,"covered":10},"regions":{"percent":100,"count":200,"covered":200}}}]}"#,
    ] {
        let Some(fixture) = GateFixture::new() else {
            return;
        };
        fixture.write("coverage-report", report);
        assert!(fixture.run().contains("\"decision\": \"block\""));
        assert!(!fixture.root.join(".claude/hooks/.tdd-gate-pass").exists());
    }
}
