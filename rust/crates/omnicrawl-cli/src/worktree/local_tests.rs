use super::*;

fn workspace(tag: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("omnicrawl-local-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let workspace = root.join("project");
    std::fs::create_dir_all(&workspace).unwrap();
    (root, workspace)
}

#[test]
fn local_isolation_requires_explicit_apply_and_supports_binary_additions_and_deletions() {
    let (root, workspace) = workspace("apply");
    let host = root.join("isolations");
    std::fs::write(workspace.join("a.txt"), "before").unwrap();
    std::fs::write(workspace.join("delete.txt"), "old").unwrap();
    std::fs::write(workspace.join("binary.dat"), [0, 255, 3]).unwrap();
    std::fs::create_dir(workspace.join(".omnicrawl")).unwrap();
    std::fs::write(workspace.join(".omnicrawl/runtime"), "state").unwrap();
    let session = local::create(&workspace, "task-local", Some(&host)).unwrap();
    assert!(!session.worktree_path.join(".omnicrawl").exists());
    std::fs::write(session.worktree_path.join("a.txt"), "after").unwrap();
    std::fs::write(session.worktree_path.join("binary.dat"), [1, 0, 254]).unwrap();
    std::fs::remove_file(session.worktree_path.join("delete.txt")).unwrap();
    std::fs::create_dir(session.worktree_path.join("new")).unwrap();
    std::fs::write(session.worktree_path.join("new/file.txt"), "added").unwrap();
    assert_eq!(std::fs::read_to_string(workspace.join("a.txt")).unwrap(), "before");
    assert!(workspace.join("delete.txt").exists());
    assert_eq!(collect_artifacts(&session).unwrap().changed_files.len(), 4);
    assert!(summarize_changes(&session).unwrap().unwrap().uncommitted > 0);
    assert!(apply_to_main(&session, "merge").is_err());
    apply_to_main(&session, "checkout").unwrap();
    assert_eq!(std::fs::read_to_string(workspace.join("a.txt")).unwrap(), "after");
    assert_eq!(std::fs::read(workspace.join("binary.dat")).unwrap(), [1, 0, 254]);
    assert_eq!(std::fs::read_to_string(workspace.join("new/file.txt")).unwrap(), "added");
    assert!(!workspace.join("delete.txt").exists());
    assert!(!collect_artifacts(&session).unwrap().has_changes);
    apply_to_main(&session, "checkout").unwrap();
    cleanup_session(&session, true).unwrap();
    assert!(list_sessions(Some(&host)).is_empty());
    assert!(std::fs::read_dir(&host).unwrap().next().is_none());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn local_apply_rejects_conflicts_before_writing_any_file() {
    let (root, workspace) = workspace("conflict");
    let host = root.join("isolations");
    std::fs::write(workspace.join("a.txt"), "base").unwrap();
    std::fs::write(workspace.join("z.txt"), "base").unwrap();
    let session = local::create(&workspace, "task-conflict", Some(&host)).unwrap();
    std::fs::write(session.worktree_path.join("a.txt"), "child").unwrap();
    std::fs::write(session.worktree_path.join("z.txt"), "child").unwrap();
    std::fs::write(workspace.join("z.txt"), "user").unwrap();
    let error = apply_to_main(&session, "checkout").unwrap_err();
    assert!(error.message().contains("拒绝覆盖"));
    assert_eq!(std::fs::read_to_string(workspace.join("a.txt")).unwrap(), "base");
    assert_eq!(std::fs::read_to_string(workspace.join("z.txt")).unwrap(), "user");
    cleanup_session(&session, true).unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn local_apply_preserves_unrelated_main_workspace_changes() {
    let (root, workspace) = workspace("unrelated");
    std::fs::write(workspace.join("a.txt"), "base").unwrap();
    std::fs::write(workspace.join("b.txt"), "base").unwrap();
    let session = local::create(&workspace, "task-unrelated", Some(&root.join("isolations"))).unwrap();
    std::fs::write(session.worktree_path.join("a.txt"), "child").unwrap();
    std::fs::write(workspace.join("b.txt"), "user").unwrap();
    apply_to_main(&session, "checkout").unwrap();
    assert_eq!(std::fs::read_to_string(workspace.join("b.txt")).unwrap(), "user");
    cleanup_session(&session, true).unwrap();
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn local_isolation_rejects_symlinks() {
    let (root, workspace) = workspace("link");
    std::fs::write(root.join("outside.txt"), "outside").unwrap();
    std::os::unix::fs::symlink(root.join("outside.txt"), workspace.join("link.txt")).unwrap();
    assert!(local::create(&workspace, "task-link", Some(&root.join("isolations"))).is_err());
    std::fs::remove_dir_all(root).unwrap();
}
