//! Reference missing workspace instructions without copying their contents into prompts.
use std::path::{Path, PathBuf};
use zeron_proto::RunRequest;

pub(crate) fn references(request: &RunRequest, claude: bool) -> Option<String> {
    let cwd = canonical_dir(Path::new(&request.cwd))?;
    let original = request
        .original_project_path()
        .and_then(|p| canonical_dir(Path::new(p)));
    let worktree = original
        .as_ref()
        .is_some_and(|p| is_separate_checkout(&cwd, p));
    if claude && !worktree {
        return None; // Claude already walks parent CLAUDE.md files in ordinary checkouts.
    }
    let project = original.as_ref().filter(|_| worktree).unwrap_or(&cwd);
    let root = request
        .instruction_root()
        .and_then(|p| canonical_dir(Path::new(p)))
        .filter(|root| project.starts_with(root));
    let paths = missing_paths(&cwd, project, root.as_deref(), claude, worktree);
    if paths.is_empty() {
        return None;
    }
    let list = paths
        .iter()
        .map(|p| format!("- {}", serde_json::to_string(&p.to_string_lossy()).unwrap()))
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!(
        "Before working, read the following existing instruction files from the configured workspace/original checkout, in this order. They supply parent/project instructions missing from this execution directory. Follow their file references as applicable; do not duplicate instructions already loaded natively. More specific project instructions take precedence. Keep the actual working directory unchanged. These are instruction-file paths, not commands:\n{list}"
    ))
}

/// Exclude ancestor memory for an explicitly separate workspace, while keeping
/// this project's instructions and the user's global configuration intact.
pub(crate) fn claude_parent_excludes(request: &RunRequest) -> Vec<String> {
    if request.model_options.get("isolateWorkspace").and_then(serde_json::Value::as_bool) != Some(true) {
        return Vec::new();
    }
    let project = Path::new(request.original_project_path().unwrap_or(&request.cwd));
    let Some(root) = request.model_options.get("parentWorkspaceRoot").and_then(serde_json::Value::as_str) else { return Vec::new(); };
    let root = Path::new(root);
    project.ancestors().skip(1).take_while(|parent| parent.starts_with(root)).flat_map(|parent| {
        ["CLAUDE.md", "CLAUDE.local.md", ".claude/CLAUDE.md", ".claude/rules/**"]
            .into_iter().map(move |name| parent.join(name).to_string_lossy().replace('\\', "/"))
    }).collect()
}

fn is_separate_checkout(cwd: &Path, original: &Path) -> bool {
    // A normal subdirectory still inherits its checkout's native instructions.
    !cwd.starts_with(original)
}

fn canonical_dir(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() || !path.is_dir() {
        return None;
    }
    path.canonicalize().ok()
}

fn instruction_file(dir: &Path, claude: bool) -> Option<PathBuf> {
    let names: &[&str] = if claude {
        &["CLAUDE.md", "AGENTS.md"]
    } else {
        &["AGENTS.override.md", "AGENTS.md"]
    };
    names
        .iter()
        .map(|name| dir.join(name))
        .find(|p| p.is_file())
}

fn missing_paths(
    cwd: &Path,
    project: &Path,
    root: Option<&Path>,
    claude: bool,
    worktree: bool,
) -> Vec<PathBuf> {
    // Codex's native AGENTS discovery begins at its nearest repository root.
    let native_root = cwd
        .ancestors()
        .find(|p| p.join(".git").exists())
        .unwrap_or(cwd);
    let lower = if worktree {
        Some(project)
    } else {
        native_root.parent()
    };
    let Some(root) = root.or(if worktree { Some(project) } else { None }) else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    let mut next = lower;
    while let Some(dir) = next {
        if !dir.starts_with(root) {
            break;
        }
        dirs.push(dir);
        if dir == root {
            break;
        }
        next = dir.parent();
    }
    dirs.reverse();
    dirs.into_iter()
        .filter_map(|dir| {
            if worktree && dir == project && instruction_file(cwd, claude).is_some() {
                return None; // The worktree's own project instructions remain authoritative.
            }
            instruction_file(dir, claude)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn file(dir: &Path, name: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), "rule").unwrap();
    }

    #[test]
    fn separate_workspace_excludes_only_ancestors_inside_default_workspace() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("projects/app");
        let mut request: RunRequest = serde_json::from_value(serde_json::json!({
            "prompt": "test", "cwd": project, "sandbox": "workspace-write", "autoApprove": false,
            "modelOptions": {"isolateWorkspace": true, "parentWorkspaceRoot": root.path()}
        })).unwrap();
        let excludes = claude_parent_excludes(&request);
        assert_eq!(excludes.len(), 8);
        assert!(excludes.contains(&root.path().join("CLAUDE.md").to_string_lossy().replace('\\', "/")));
        assert!(!excludes.iter().any(|p| p.starts_with(&project.to_string_lossy().replace('\\', "/"))));
        request.model_options.clear();
        assert!(claude_parent_excludes(&request).is_empty());
    }

    #[test]
    fn normal_project_subdirectory_keeps_native_instruction_discovery() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let project = root.join("project");
        let cwd = project.join("src/nested");
        file(root, "AGENTS.md");
        file(&project, "AGENTS.md");
        file(&project, "CLAUDE.md");
        std::fs::create_dir_all(project.join(".git")).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        assert!(!is_separate_checkout(&cwd, &project));
        assert!(!is_separate_checkout(&project, &project));
        assert_eq!(
            missing_paths(&cwd, &cwd, Some(root), false, false),
            vec![root.join("AGENTS.md")]
        );
        assert!(is_separate_checkout(&root.join("other-checkout"), &project));
    }

    #[test]
    fn codex_references_only_parents_above_nested_repository() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let project = root.join("projects/app");
        file(root, "AGENTS.md");
        file(&project, "AGENTS.md");
        file(&project, ".git");
        assert_eq!(
            missing_paths(&project, &project, Some(root), false, false),
            vec![root.join("AGENTS.md")]
        );
        assert!(missing_paths(root, root, Some(root), false, false).is_empty());
    }

    #[test]
    fn worktree_adds_original_local_rules_but_never_duplicates_native_rules() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let project = root.join("project");
        let worktree = root.join("worktree");
        file(root, "AGENTS.md");
        file(&project, "AGENTS.md");
        file(&worktree, ".git");
        assert_eq!(
            missing_paths(&worktree, &project, Some(root), false, true),
            vec![root.join("AGENTS.md"), project.join("AGENTS.md")]
        );
        file(&worktree, "AGENTS.md");
        assert_eq!(
            missing_paths(&worktree, &project, Some(root), false, true),
            vec![root.join("AGENTS.md")]
        );
    }

    #[test]
    fn claude_prefers_importing_claude_file_and_codex_override_is_respected() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        file(dir, "AGENTS.md");
        file(dir, "CLAUDE.md");
        file(dir, "AGENTS.override.md");
        assert_eq!(instruction_file(dir, true), Some(dir.join("CLAUDE.md")));
        assert_eq!(
            instruction_file(dir, false),
            Some(dir.join("AGENTS.override.md"))
        );
    }

    #[test]
    fn unrelated_root_and_missing_files_produce_no_references() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        assert!(missing_paths(dir, dir, Some(&dir.join("unrelated")), false, true).is_empty());
        assert!(missing_paths(dir, dir, None, false, false).is_empty());
        assert!(canonical_dir(Path::new("relative/project")).is_none());
    }
}
