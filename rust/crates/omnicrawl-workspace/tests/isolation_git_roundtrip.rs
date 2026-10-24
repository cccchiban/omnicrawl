//! 隔离工作区的真 git 集成测试：建真仓库、建真 worktree、改文件、应用回主工作区、清理。
//!
//! 对照数据集（`workspace_isolation_parity.rs`）只覆盖纯函数与「能靠合成目录树判定」的行为；
//! 这里补上真正要跑 `git` 的那部分：`git worktree add` 与复用校验、`git diff` + `git apply
//! --3way`、四层门禁的第三 / 第四层、`git worktree remove` + `prune`、以及 local 模式的镜像。
//!
//! 没有 `git` 或 `git init` 失败时整组跳过（打印原因），不让环境差异把测试判成失败。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use omnicrawl_config::features::agent_workspace::AgentWorkspaceConfig;
use omnicrawl_workspace::agent_isolation::{
    apply_isolation_changes, cleanup_eligible, cleanup_isolation_session, create_isolation_session,
    finalize_isolation_session, read_isolation_metadata, sweep_expired_isolation_sessions,
    IsolationOptions, IsolationSession, SweepOptions, DEFAULT_SWEEP_MAX_AGE_SECONDS,
    MIN_KEEP_SECONDS,
};
use omnicrawl_workspace::resolve_path;
use serde_json::Value;

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let mut base = std::env::temp_dir();
        base.push(format!(
            "oc-isolation-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&base).expect("创建临时目录");
        Self {
            path: resolve_path(&base),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn git(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// 带提交时间环境变量跑 git（构造「很久以前提交过」的残留）。
fn git_at(dir: &Path, args: &[&str], timestamp: &str) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_DATE", timestamp)
        .env("GIT_COMMITTER_DATE", timestamp)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).to_string())
        .unwrap_or_default()
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// 建一个最小可提交的仓库；返回 false 表示环境不支持（调用方跳过）。
fn init_repo(dir: &Path) -> bool {
    if !git(dir, &["init", "-q"]) {
        return false;
    }
    // 隔离区内部会自己提交，因此身份与签名配置必须落在仓库里，不能依赖全局配置。
    for args in [
        ["config", "user.email", "omnicrawl-test@example.com"].as_slice(),
        ["config", "user.name", "OmniCrawl Test"].as_slice(),
        ["config", "core.autocrlf", "false"].as_slice(),
        ["config", "commit.gpgsign", "false"].as_slice(),
    ] {
        if !git(dir, args) {
            return false;
        }
    }
    std::fs::write(dir.join("readme.md"), "line one\n").expect("写入 readme");
    git(dir, &["add", "-A"]) && git(dir, &["commit", "-q", "-m", "init"])
}

fn worktree_config() -> AgentWorkspaceConfig {
    AgentWorkspaceConfig {
        // 同步 / 复制 / 环境脚本都不参与这批断言，显式关掉以免依赖具体环境。
        sync_uncommitted: false,
        copy_dirs: Vec::new(),
        env_scripts: Vec::new(),
        ..AgentWorkspaceConfig::default()
    }
}

/// SubAgent 视角的会话：目录与分支复用主隔离区的，只把 mode 换成 `subagent`。
fn as_subagent(session: &IsolationSession) -> IsolationSession {
    IsolationSession {
        mode: "subagent".to_string(),
        ..session.clone()
    }
}

#[test]
fn worktree_roundtrip_applies_changes_and_cleans_up() {
    if !git_available() {
        eprintln!("跳过：环境里没有可用的 git");
        return;
    }
    let root = TempDir::new("roundtrip");
    let repo = root.path().join("repo");
    std::fs::create_dir_all(&repo).expect("创建仓库目录");
    if !init_repo(&repo) {
        eprintln!("跳过：git init 失败");
        return;
    }
    let worktrees = root.path().join("worktrees");

    let session = create_isolation_session(
        IsolationOptions::new(repo.clone())
            .with_instance_id("roundtrip")
            .with_config(worktree_config())
            .with_worktrees_root(worktrees.clone()),
    )
    .expect("创建隔离区");
    assert_eq!(session.mode, "worktree");
    assert!(session.worktree_path.exists(), "worktree 目录应已创建");
    assert!(session.worktree_path.join("readme.md").is_file());
    assert!(
        !session.worktree_path.join(".git").is_dir(),
        "linked worktree 的 .git 应是指针文件"
    );
    assert_eq!(session.base_ref.len(), 40, "基线应是完整提交哈希");

    let metadata = read_isolation_metadata(&worktrees, "aw-roundtrip").expect("元数据应已落盘");
    assert_eq!(
        metadata.get("mode").and_then(Value::as_str),
        Some("worktree")
    );
    assert_eq!(
        metadata.get("base_ref").and_then(Value::as_str),
        Some(session.base_ref.as_str())
    );
    assert_eq!(
        metadata.get("cleanup_on_exit").and_then(Value::as_str),
        Some("auto")
    );

    // 隔离区里改一个文件、加一个文件。
    std::fs::write(
        session.worktree_path.join("readme.md"),
        "line one\nline two\n",
    )
    .expect("改 readme");
    std::fs::write(session.worktree_path.join("new.txt"), "new\n").expect("加 new.txt");
    assert_eq!(
        std::fs::read_to_string(repo.join("readme.md")).expect("读主工作区 readme"),
        "line one\n",
        "隔离区写入不能穿透到主工作区"
    );

    // 第三层门禁：目录里有未提交改动就不清理（保留期已过也不放行）。
    let later = session.created_at + MIN_KEEP_SECONDS + 1.0;
    let (eligible, reason) = cleanup_eligible(&session, None, Some(later), "origin");
    assert!(!eligible, "有未提交改动不应判定可清理");
    assert_eq!(reason, "隔离区存在未提交改动");

    // 应用回主工作区：两个文件，无冲突。
    let (changed, conflicts) = apply_isolation_changes(&session, None).expect("应用隔离区变更");
    assert_eq!(conflicts, Vec::<String>::new(), "不应产生冲突");
    assert_eq!(changed, 2, "补丁应覆盖两个文件");
    assert_eq!(
        std::fs::read_to_string(repo.join("readme.md")).expect("读主工作区 readme"),
        "line one\nline two\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("new.txt")).expect("读主工作区 new.txt"),
        "new\n"
    );

    // 应用之后隔离区是干净的：四层门禁全过，可以自动清理。
    let (eligible, reason) = cleanup_eligible(&session, None, Some(later), "origin");
    assert!(eligible, "应用后应可清理，实际：{reason}");
    let (removed, reason) = cleanup_isolation_session(&session, None, false, Some(later));
    assert!(removed, "清理失败：{reason}");
    assert!(!session.worktree_path.exists(), "隔离区目录应已删除");
    assert!(
        read_isolation_metadata(&worktrees, "aw-roundtrip").is_none(),
        "元数据应随目录一起删除"
    );
}

#[test]
fn worktree_reuse_restores_baseline_without_recreating() {
    if !git_available() {
        eprintln!("跳过：环境里没有可用的 git");
        return;
    }
    let root = TempDir::new("reuse");
    let repo = root.path().join("repo");
    std::fs::create_dir_all(&repo).expect("创建仓库目录");
    if !init_repo(&repo) {
        eprintln!("跳过：git init 失败");
        return;
    }
    let worktrees = root.path().join("worktrees");

    let first = create_isolation_session(
        IsolationOptions::new(repo.clone())
            .with_instance_id("reuse")
            .with_config(worktree_config())
            .with_worktrees_root(worktrees.clone()),
    )
    .expect("首次创建隔离区");
    // 上次运行留下的残渣：复用必须原样保留，不能被「重新创建」抹掉。
    std::fs::write(first.worktree_path.join("scratch.txt"), "kept\n").expect("写入 scratch");

    let second = create_isolation_session(
        IsolationOptions::new(repo.clone())
            .with_instance_id("reuse")
            .with_config(worktree_config())
            .with_worktrees_root(worktrees.clone()),
    )
    .expect("复用隔离区");

    assert_eq!(second.worktree_path, first.worktree_path);
    assert_eq!(
        second.base_ref, first.base_ref,
        "复用时应从元数据恢复基线，否则退出 apply 的 diff 基线会退化成 HEAD..HEAD"
    );
    assert!(
        (second.created_at - first.created_at).abs() < 1.0,
        "复用时应从元数据恢复创建时间（保留期按它算）"
    );
    assert!(
        second.worktree_path.join("scratch.txt").is_file(),
        "复用不应重建目录"
    );

    let (removed, reason) = cleanup_isolation_session(&second, None, true, None);
    assert!(removed, "强制清理失败：{reason}");
}

#[test]
fn local_mode_mirrors_changes_back() {
    if !git_available() {
        eprintln!("跳过：环境里没有可用的 git");
        return;
    }
    let root = TempDir::new("local");
    let repo = root.path().join("repo");
    std::fs::create_dir_all(&repo).expect("创建仓库目录");
    if !init_repo(&repo) {
        eprintln!("跳过：git init 失败");
        return;
    }
    let worktrees = root.path().join("worktrees");
    let config = AgentWorkspaceConfig {
        mode: "local".to_string(),
        ..worktree_config()
    };

    let session = create_isolation_session(
        IsolationOptions::new(repo.clone())
            .with_instance_id("localmode")
            .with_config(config)
            .with_worktrees_root(worktrees.clone()),
    )
    .expect("创建 local 隔离区");
    assert_eq!(session.mode, "local");
    assert!(session.worktree_path.join("readme.md").is_file());
    assert!(
        !session.worktree_path.join(".git").exists(),
        "local 模式排除 .git"
    );

    std::fs::write(session.worktree_path.join("local-new.txt"), "value\n").expect("写入新文件");
    let (copied, conflicts) = apply_isolation_changes(&session, None).expect("镜像回主工作区");
    assert_eq!(conflicts, Vec::<String>::new());
    assert!(copied >= 2, "镜像应覆盖隔离区里的全部文件，实际 {copied}");
    assert_eq!(
        std::fs::read_to_string(repo.join("local-new.txt")).expect("读主工作区新文件"),
        "value\n"
    );

    let (removed, reason) = cleanup_isolation_session(&session, None, true, None);
    assert!(removed, "清理失败：{reason}");
    assert!(read_isolation_metadata(&worktrees, "aw-localmode").is_none());
}

#[test]
fn finalize_reports_apply_and_keep_policy() {
    if !git_available() {
        eprintln!("跳过：环境里没有可用的 git");
        return;
    }
    let root = TempDir::new("finalize");
    let repo = root.path().join("repo");
    std::fs::create_dir_all(&repo).expect("创建仓库目录");
    if !init_repo(&repo) {
        eprintln!("跳过：git init 失败");
        return;
    }
    let worktrees = root.path().join("worktrees");

    let session = create_isolation_session(
        IsolationOptions::new(repo.clone())
            .with_instance_id("finalize")
            .with_config(worktree_config())
            .with_worktrees_root(worktrees.clone()),
    )
    .expect("创建隔离区");
    std::fs::write(session.worktree_path.join("extra.txt"), "extra\n").expect("写入 extra");

    // cleanup_on_exit=keep：应用变更但不删除隔离区，摘要里要如实说明。
    let summary = finalize_isolation_session(&session, true, "keep", None, None);
    assert!(
        summary.contains("已应用 1 个文件的变更到主工作区"),
        "摘要应报告已应用的文件数：{summary}"
    );
    assert!(
        summary.contains("隔离区已保留（cleanup_on_exit=keep）"),
        "摘要应说明保留策略：{summary}"
    );
    assert!(session.worktree_path.exists(), "keep 策略下隔离区必须保留");

    let (removed, reason) = cleanup_isolation_session(&session, None, true, None);
    assert!(removed, "收尾后强制清理失败：{reason}");
}

/// SubAgent 产出以提交形式留在隔离分支上：未满一个月保留，满一个月无新提交才清理。
#[test]
fn subagent_unreviewed_commit_kept_until_one_month_idle() {
    if !git_available() {
        eprintln!("跳过：环境里没有可用的 git");
        return;
    }
    let root = TempDir::new("subidle");
    let repo = root.path().join("repo");
    std::fs::create_dir_all(&repo).expect("创建仓库目录");
    if !init_repo(&repo) {
        eprintln!("跳过：git init 失败");
        return;
    }
    let worktrees = root.path().join("worktrees");
    let session = create_isolation_session(
        IsolationOptions::new(repo.clone())
            .with_instance_id("subidle")
            .with_config(worktree_config())
            .with_worktrees_root(worktrees.clone()),
    )
    .expect("创建隔离区");

    // 模拟 collect_artifacts：产出提交到隔离分支。
    std::fs::write(session.worktree_path.join("out.txt"), "done\n").expect("写入产出");
    assert!(git(&session.worktree_path, &["add", "-A"]), "git add 失败");
    assert!(
        git(
            &session.worktree_path,
            &["commit", "-q", "-m", "subagent output"]
        ),
        "git commit 失败"
    );
    let sub = as_subagent(&session);

    // 刚提交完：仍保留（未审查的产出可能是唯一副本）。
    let soon = session.created_at + MIN_KEEP_SECONDS + 1.0;
    let (eligible, reason) = cleanup_eligible(&sub, None, Some(soon), "origin");
    assert!(!eligible, "未审查的提交不应立刻清理");
    assert!(
        reason.contains("未审查") && reason.contains("还需"),
        "原因应说明未审查且未满期限：{reason}"
    );

    // 满一个月没有新提交：放行自动清理。
    let month_later = soon + DEFAULT_SWEEP_MAX_AGE_SECONDS;
    let (eligible, reason) = cleanup_eligible(&sub, None, Some(month_later), "origin");
    assert!(eligible, "满一个月无新提交应放行：{reason}");

    let (removed, reason) = cleanup_isolation_session(&sub, None, false, Some(month_later));
    assert!(removed, "清理失败：{reason}");
}

/// 期限按「最后一次提交」算：早先的老提交不能提前把有近期提交的隔离区判成过期。
#[test]
fn subagent_deadline_follows_latest_commit() {
    if !git_available() {
        eprintln!("跳过：环境里没有可用的 git");
        return;
    }
    let root = TempDir::new("subrenew");
    let repo = root.path().join("repo");
    std::fs::create_dir_all(&repo).expect("创建仓库目录");
    if !init_repo(&repo) {
        eprintln!("跳过：git init 失败");
        return;
    }
    let worktrees = root.path().join("worktrees");
    let session = create_isolation_session(
        IsolationOptions::new(repo.clone())
            .with_instance_id("subrenew")
            .with_config(worktree_config())
            .with_worktrees_root(worktrees.clone()),
    )
    .expect("创建隔离区");

    // 一个很老的首个产出提交，再补一个刚刚的提交。
    std::fs::write(session.worktree_path.join("old.txt"), "old\n").expect("写入 old");
    assert!(git(&session.worktree_path, &["add", "-A"]), "git add 失败");
    assert!(
        git_at(
            &session.worktree_path,
            &["commit", "-q", "-m", "old output"],
            "2001-01-01T00:00:00",
        ),
        "提交老产出失败"
    );
    std::fs::write(session.worktree_path.join("new.txt"), "new\n").expect("写入 new");
    assert!(git(&session.worktree_path, &["add", "-A"]), "git add 失败");
    assert!(
        git(
            &session.worktree_path,
            &["commit", "-q", "-m", "new output"]
        ),
        "提交新产出失败"
    );

    let sub = as_subagent(&session);
    let (eligible, reason) =
        cleanup_eligible(&sub, None, Some(session.created_at + 7200.0), "origin");
    assert!(
        !eligible,
        "最新提交刚刚发生，期限应从它起算而不是老提交：{reason}"
    );
    assert!(reason.contains("未审查"), "原因应说明未审查：{reason}");

    let (removed, reason) = cleanup_isolation_session(&sub, None, true, None);
    assert!(removed, "强制清理失败：{reason}");
}

/// 启动清扫端到端：sw- 残留满一个月且无新提交时被回收，目录与元数据一起删除。
#[test]
fn sweep_reclaims_idle_subagent_worktree_after_one_month() {
    if !git_available() {
        eprintln!("跳过：环境里没有可用的 git");
        return;
    }
    let root = TempDir::new("sweep");
    let repo = root.path().join("repo");
    std::fs::create_dir_all(&repo).expect("创建仓库目录");
    if !init_repo(&repo) {
        eprintln!("跳过：git init 失败");
        return;
    }
    let worktrees = root.path().join("worktrees");
    std::fs::create_dir_all(&worktrees).expect("创建托管根");
    let base = git_stdout(&repo, &["rev-parse", "HEAD"]).trim().to_string();
    let worktree_path = worktrees.join("sw-task01");
    assert!(
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "omnicrawl/subagent/task01",
                worktree_path.to_string_lossy().as_ref(),
                base.as_str(),
            ],
        ),
        "git worktree add 失败"
    );
    // 子任务产出：35 天前提交过，之后无人过问。
    std::fs::write(worktree_path.join("out.txt"), "done\n").expect("写入产出");
    assert!(git(&worktree_path, &["add", "-A"]), "git add 失败");
    assert!(
        git_at(
            &worktree_path,
            &["commit", "-q", "-m", "subagent output"],
            "2001-01-01T00:00:00",
        ),
        "提交产出失败"
    );

    let created_at = 1_000_000_000.0_f64;
    let metadata = serde_json::json!({
        "instance_id": "task01",
        "task_id": "task01",
        "mode": "subagent",
        "repo_root": repo.to_string_lossy(),
        "main_workspace": repo.to_string_lossy(),
        "worktree_path": worktree_path.to_string_lossy(),
        "base_ref": base,
        "branch_name": "omnicrawl/subagent/task01",
        "created_at": created_at,
        "apply_on_exit": false,
        "cleanup_on_exit": "auto",
    });
    std::fs::write(
        worktrees.join("sw-task01.json"),
        serde_json::to_string_pretty(&metadata).expect("序列化元数据"),
    )
    .expect("写元数据");

    // 未满一个月：保留。
    let result = sweep_expired_isolation_sessions(SweepOptions {
        worktrees_root: Some(worktrees.clone()),
        now: Some(created_at + MIN_KEEP_SECONDS + 60.0),
        ..SweepOptions::default()
    });
    assert!(result.removed.is_empty(), "未满期限不应回收：{result:?}");

    // 满一个月且最后一次提交早已超过期限：回收。
    let result = sweep_expired_isolation_sessions(SweepOptions {
        worktrees_root: Some(worktrees.clone()),
        now: Some(created_at + DEFAULT_SWEEP_MAX_AGE_SECONDS + 60.0),
        ..SweepOptions::default()
    });
    assert_eq!(
        result.removed,
        vec!["task01".to_string()],
        "满一个月应回收，实际：{result:?}"
    );
    assert!(!worktree_path.exists(), "worktree 目录应已删除");
    assert!(
        !worktrees.join("sw-task01.json").is_file(),
        "元数据应随目录一起删除"
    );
}
