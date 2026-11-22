use super::*;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;

const EXCLUDED: [&str; 5] = [".git", ".aoci", ".omnicrawl", "node_modules", "target"];

fn failure(error: impl std::fmt::Display) -> WorktreeError {
    WorktreeError::new(format!("本地隔离失败：{error}"))
}

fn baseline_path(session: &WorktreeSession) -> PathBuf {
    session.worktree_path.with_extension("baseline")
}

fn inventory(root: &Path) -> Result<BTreeMap<String, String>, WorktreeError> {
    let mut files = BTreeMap::new();
    visit(root, root, &mut files)?;
    Ok(files)
}

fn visit(root: &Path, directory: &Path, files: &mut BTreeMap<String, String>) -> Result<(), WorktreeError> {
    for entry in std::fs::read_dir(directory).map_err(failure)? {
        let entry = entry.map_err(failure)?;
        if EXCLUDED.iter().any(|name| entry.file_name().to_string_lossy().eq_ignore_ascii_case(name)) {
            continue;
        }
        let kind = entry.file_type().map_err(failure)?;
        if kind.is_symlink() || is_reparse(&entry.metadata().map_err(failure)?) {
            return Err(failure(format!("不复制符号链接：{}", entry.path().display())));
        }
        if kind.is_dir() {
            visit(root, &entry.path(), files)?;
        } else if kind.is_file() {
            let relative = entry.path().strip_prefix(root).map_err(failure)?
                .to_string_lossy().replace('\\', "/");
            files.insert(relative, fingerprint(&entry.path())?);
        } else {
            return Err(failure("不支持的文件类型"));
        }
    }
    Ok(())
}

fn is_reparse(metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)] {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))] { let _ = metadata; false }
}

fn fingerprint(path: &Path) -> Result<String, WorktreeError> {
    let mut file = std::fs::File::open(path).map_err(failure)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let size = file.read(&mut buffer).map_err(failure)?;
        if size == 0 { break; }
        hash.update(&buffer[..size]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

fn read_baseline(session: &WorktreeSession) -> Result<BTreeMap<String, String>, WorktreeError> {
    let raw = std::fs::read(baseline_path(session)).map_err(failure)?;
    let files: BTreeMap<String, String> = serde_json::from_slice(&raw).map_err(failure)?;
    for path in files.keys() {
        let relative = Path::new(path);
        if relative.is_absolute() || relative.components().any(|part| !matches!(part, std::path::Component::Normal(_))) {
            return Err(failure("基线包含非法路径"));
        }
    }
    Ok(files)
}

fn write_baseline(session: &WorktreeSession, files: &BTreeMap<String, String>) -> Result<(), WorktreeError> {
    std::fs::write(baseline_path(session), serde_json::to_vec(files).map_err(failure)?).map_err(failure)
}

pub(super) fn create(workspace: &Path, task_id: &str, parent: Option<&Path>) -> Result<WorktreeSession, WorktreeError> {
    let repo_root = std::fs::canonicalize(workspace).map_err(failure)?;
    let parent = parent.map(Path::to_path_buf).unwrap_or_else(default_worktrees_root);
    std::fs::create_dir_all(&parent).map_err(failure)?;
    let parent = std::fs::canonicalize(parent).map_err(failure)?;
    if parent.starts_with(&repo_root) {
        return Err(failure("隔离托管目录必须位于工作区之外"));
    }
    let instance_id = sanitize_branch_fragment(task_id).replace('/', "-");
    if instance_id.is_empty() { return Err(failure("任务编号为空")); }
    let session = WorktreeSession {
        task_id: task_id.to_string(), instance_id: instance_id.clone(), repo_root,
        worktree_path: parent.join(format!("sw-{instance_id}")),
        branch_name: String::new(), base_ref: String::new(), local: true,
    };
    if session.worktree_path.exists() || baseline_path(&session).exists()
        || metadata_path(&parent, &session.worktree_path).exists() {
        return Err(failure("隔离目录或元数据已存在，请先处理残留"));
    }
    let baseline = inventory(&session.repo_root)?;
    std::fs::create_dir(&session.worktree_path).map_err(failure)?;
    // 目录建好之后任何一步失败都要收拾干净：此时没有元数据，清扫认不出它，
    // 而同一个任务号再创建会撞上「已存在」，残留就永久卡在那里了。
    let prepared = (|| -> Result<(), WorktreeError> {
        for relative in baseline.keys() {
            let target = session.worktree_path.join(relative);
            std::fs::create_dir_all(target.parent().expect("文件有父目录")).map_err(failure)?;
            std::fs::copy(session.repo_root.join(relative), target).map_err(failure)?;
        }
        if inventory(&session.worktree_path)? != baseline
            || inventory(&session.repo_root)? != baseline
        {
            return Err(failure("复制期间源文件发生变化，请重新创建隔离目录"));
        }
        write_baseline(&session, &baseline)?;
        write_metadata(&session, &parent)?;
        Ok(())
    })();
    if let Err(error) = prepared {
        let _ = std::fs::remove_dir_all(&session.worktree_path);
        let _ = std::fs::remove_file(baseline_path(&session));
        return Err(error);
    }
    Ok(session)
}

fn changed(baseline: &BTreeMap<String, String>, current: &BTreeMap<String, String>) -> Vec<String> {
    baseline.keys().chain(current.keys()).cloned().collect::<BTreeSet<_>>()
        .into_iter().filter(|path| baseline.get(path) != current.get(path)).collect()
}

pub(super) fn artifacts(session: &WorktreeSession) -> Result<WorktreeArtifacts, WorktreeError> {
    let baseline = read_baseline(session)?;
    let current = inventory(&session.worktree_path)?;
    let files = changed(&baseline, &current);
    let mut preview = String::new();
    for path in &files {
        let status = match (baseline.contains_key(path), current.contains_key(path)) {
            (false, true) => "新增", (true, false) => "删除", _ => "修改",
        };
        preview.push_str(&format!("{status}: {path}\n"));
        if current.contains_key(path) {
            let mut bytes = Vec::new();
            std::fs::File::open(session.worktree_path.join(path)).map_err(failure)?
                .take(4096).read_to_end(&mut bytes).map_err(failure)?;
            if let Ok(text) = std::str::from_utf8(&bytes) { preview.push_str(text); preview.push('\n'); }
        }
        if preview.chars().count() >= MAX_DIFF_CHARS { break; }
    }
    Ok(WorktreeArtifacts {
        branch_name: String::new(), worktree_path: session.worktree_path.to_string_lossy().to_string(),
        base_ref: "local-baseline".to_string(), has_changes: !files.is_empty(),
        diff_stat: format!("{} 个变更文件；本地隔离，等待审查后回写", files.len()),
        changed_files: files.into_iter().take(MAX_CHANGED_FILES).collect(),
        diff_text: preview.chars().take(MAX_DIFF_CHARS).collect(),
    })
}

fn safe_target(root: &Path, relative: &str) -> Result<PathBuf, WorktreeError> {
    if std::fs::canonicalize(root).map_err(failure)? != root {
        return Err(failure("隔离根或主工作区路径已被替换"));
    }
    let mut target = root.to_path_buf();
    for part in Path::new(relative).components() {
        if !matches!(part, std::path::Component::Normal(_)) { return Err(failure("非法回写路径")); }
        target.push(part.as_os_str());
        match std::fs::symlink_metadata(&target) {
            Ok(metadata) if metadata.file_type().is_symlink() || is_reparse(&metadata) => return Err(failure("回写路径包含符号链接")),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(failure(error)),
        }
    }
    Ok(target)
}

pub(super) fn apply(session: &WorktreeSession, strategy: &str) -> Result<String, WorktreeError> {
    if strategy != "checkout" { return Err(failure("本地隔离只支持 strategy=checkout")); }
    let baseline = read_baseline(session)?;
    let current = inventory(&session.worktree_path)?;
    let files = changed(&baseline, &current);
    for path in &files {
        let target = safe_target(&session.repo_root, path)?;
        let actual = if target.exists() { Some(fingerprint(&target)?) } else { None };
        if actual.as_ref() != baseline.get(path) {
            return Err(failure(format!("主工作区文件已变化，拒绝覆盖：{path}")));
        }
    }
    for path in &files {
        let target = safe_target(&session.repo_root, path)?;
        let source = safe_target(&session.worktree_path, path)?;
        if current.contains_key(path) {
            if Some(&fingerprint(&source)?) != current.get(path) {
                return Err(failure(format!("隔离文件在回写期间发生变化：{path}")));
            }
            std::fs::create_dir_all(target.parent().expect("文件有父目录")).map_err(failure)?;
            // 用 copy 而不是「读出来再 create」：后者会把文件的权限位重置成默认值，
            // 隔离副本里带执行位的脚本回写后就不能直接跑了。
            std::fs::copy(&source, &target).map_err(failure)?;
        } else {
            // 删除前再确认隔离副本里也没有这个文件：inventory 之后它可能被重新创建，
            // 那时主工作区里的唯一副本不该被删掉。
            if source.exists() {
                return Err(failure(format!("隔离文件在回写期间发生变化：{path}")));
            }
            std::fs::remove_file(target).map_err(failure)?;
        }
    }
    write_baseline(session, &current)?;
    Ok(format!("已将本地隔离目录的 {} 个文件变更回写到主工作区。", files.len()))
}

pub(super) fn cleanup(session: &WorktreeSession) -> Result<(), WorktreeError> {
    if session.worktree_path.exists() { std::fs::remove_dir_all(&session.worktree_path).map_err(failure)?; }
    for path in [baseline_path(session), metadata_path(session.worktree_path.parent().expect("隔离目录有父目录"), &session.worktree_path)] {
        if path.exists() { std::fs::remove_file(path).map_err(failure)?; }
    }
    Ok(())
}
