use serde_json::Value;
use std::path::{Path, PathBuf};

pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().expect("xtask sits in the repository root").to_path_buf()
}

/// spec/cycles/*.json in name order, each as (path, parsed spec).
pub fn load_cycles() -> Result<Vec<(PathBuf, Value)>, String> {
    let dir = root().join("spec").join("cycles");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let body = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let spec = serde_json::from_str(&body).map_err(|e| format!("{}: {e}", path.display()))?;
            Ok((path, spec))
        })
        .collect()
}

pub fn file_name(path: &Path) -> String {
    path.file_name().unwrap_or_default().to_string_lossy().into_owned()
}

pub const HOOK_EVENTS: [&str; 4] = ["release-start", "release-bump", "release-finish", "hotfix-finish"];

const DEPLOYABLE_KINDS: [&str; 9] =
    ["release", "hotfix", "feature", "fix", "chore", "docs", "refactor", "release-fix", "hotfix-fix"];

pub fn runs_deploy_version(branch: &str) -> bool {
    branch == "develop" || branch.split_once('/').is_some_and(|(kind, _)| DEPLOYABLE_KINDS.contains(&kind))
}

pub fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deploy_version_runs_on_develop_and_the_branches_that_land_on_it() {
        for branch in ["develop", "release/1.0.0", "hotfix/1.0.1", "feature/a", "fix/a", "chore/a", "docs/a", "refactor/a", "release-fix/1.0.0/a", "hotfix-fix/1.0.1/a"] {
            assert!(runs_deploy_version(branch), "{branch}");
        }
    }

    #[test]
    fn deploy_version_refuses_the_mainline_and_gflow_owned_branches() {
        for branch in ["main", "version/1.0.0-into-release-1.0.0", "landing/release-1.0.0-into-main", "developer", "feature"] {
            assert!(!runs_deploy_version(branch), "{branch}");
        }
    }

    #[test]
    fn text_shows_a_string_bare_and_anything_else_as_json() {
        assert_eq!(text(&json!("develop")), "develop");
        assert_eq!(text(&json!(3)), "3");
        assert_eq!(text(&Value::Null), "null");
    }
}
