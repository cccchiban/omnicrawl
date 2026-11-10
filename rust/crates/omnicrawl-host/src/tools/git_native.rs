//! 沙盒内 `git` 的进程内实现：用 gix 直接操作仓库，不再 spawn `git.exe`。
//!
//! Windows 真隔离下 `git.exe` 必然起不来（见 `sandbox_windows_design.md` §18/§23 的实测：
//! MSYS 运行时初始化阶段就要以读写方式打开 `/dev/null`，而 `\Device\Null` 的 NULL DACL
//! 对 AppContainer 受限令牌一律拒绝，连 `--version` 都过不去）。授权目录、改 PATH、加包装
//! 都无效，给设备对象补 ACE 更是会打坏整机 NUL。
//!
//! 因此这里换的是**执行形态**而不是放宽隔离：gix 在宿主进程内运行，不经 MSYS、不打开
//! `NUL`。代价是它天然不受容器约束，所以约束必须由本模块自己施加——每种子命令先过
//! [`Scope`]，只做被允许的事：
//!
//! - 只读子命令：仅读仓库对象与工作区，可放行；
//! - 写索引/写提交：只能落到工作区内的 `.git` 与工作区文件；
//! - 网络子命令（`fetch`/`pull`/`push`/`clone`/`ls-remote`）：一律拒绝。
//!
//! 最后一条是硬边界：沙盒里出网必须经 `[sandbox.net]` 的判定与代理，让 git 自己走网络
//! 就绕过了整套审批。这也正是 gix 依赖不启 `blocking-network-client` 的原因。
//!
//! 支持范围刻意收窄到「沙盒里真用得上的那些」。不支持的子命令明确拒绝并给出宿主侧做法，
//! 绝不退回 spawn `git.exe`——那等于让最该被约束的操作完全不受约束。

use std::path::Path;

use gix::bstr::ByteSlice;

use super::git::bound_text;
use omnicrawl_core::ToolResult;

/// 沙盒内由进程内后端负责的子命令。
///
/// 只收「能确定地做对」的那些：读类子命令与低风险写类（暂存、提交、建删分支/标签）。
/// `merge`/`rebase`/`cherry-pick`/`checkout` 要跑工作区状态机，gix 的高层入口只覆盖
/// linked worktree，硬接等于把「半对」的行为交给模型，因此宁可明确拒绝。
pub const NATIVE_ACTIONS: [&str; 16] = [
    "status",
    "log",
    "diff",
    "add",
    "commit",
    "rm",
    "rev-parse",
    "ls-files",
    "show-ref",
    "for-each-ref",
    "describe",
    "cat-file",
    "branch",
    "tag",
    "stash",
    "remote",
];

/// 需要出网的子命令：沙盒内一律拒绝。
///
/// 沙盒的每个出网请求都要经 `[sandbox.net]` 判定（宿主回环代理）。让 git 自己连出去
/// 就整段绕过了这套判定，所以这里不是「暂不支持」，而是**不允许**。
const NETWORK_ACTIONS: [&str; 6] = ["fetch", "pull", "push", "clone", "ls-remote", "submodule"];

/// 一条子命令在进程内可以做到什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// 只读：不写工作区、不写索引、不写引用。
    ReadOnly,
    /// 可写：只允许改工作区内的文件与 `.git`。
    Write,
}

/// 子命令 -> 允许的作用域。返回 `None` 表示进程内后端不承接。
///
/// 判定只看**这次调用的实际参数**：`branch` 无参数是列举（只读），带名字才是新建（可写）。
pub fn scope_of(action: &str, args: &[String]) -> Option<Scope> {
    let has = |token: &str| args.iter().any(|arg| arg == token);
    let positionals: Vec<&String> = args.iter().filter(|arg| !arg.starts_with('-')).collect();
    match action {
        // 读类：不写工作区、不写索引、不写引用。
        "status" | "log" | "diff" | "rev-parse" | "ls-files" | "show-ref" | "for-each-ref"
        | "describe" | "cat-file" | "remote" => Some(Scope::ReadOnly),
        // `stash` 只有无参数（等价 `stash list`）与 `list`/`show` 是只读。
        "stash" => {
            let reads = positionals.is_empty()
                || positionals
                    .first()
                    .is_some_and(|sub| matches!(sub.as_str(), "list" | "show"));
            Some(if reads { Scope::ReadOnly } else { Scope::Write })
        }
        // `branch` 无位置参数 = 列举；有名字或 `-d/-m/-c` = 改引用。
        "branch" => {
            let mutating = !positionals.is_empty() || has("-d") || has("-D") || has("-m")
                || has("-M") || has("-c") || has("-C") || has("--delete") || has("--move")
                || has("--copy");
            Some(if mutating { Scope::Write } else { Scope::ReadOnly })
        }
        // `tag` 同理：无参数或 `-l` = 列举。
        "tag" => {
            let mutating = !positionals.is_empty() && !has("-l") && !has("--list");
            Some(if mutating { Scope::Write } else { Scope::ReadOnly })
        }
        "add" | "rm" => Some(Scope::Write),
        "commit" => {
            // `--dry-run` 不落提交，按只读处理。
            Some(if has("--dry-run") {
                Scope::ReadOnly
            } else {
                Scope::Write
            })
        }
        _ => None,
    }
}

/// 在沙盒内用 gix 执行一条 `git` 子命令。
///
/// `argv` 与 spawn 通路完全同形（`["git", "-c", …, action, args…, "--", paths…]`），
/// 这样两条执行路径的输入永远一致，模型看到的参数校验结果也一致。
pub fn run_native(argv: &[String], action: &str, root: &Path) -> ToolResult {
    if NETWORK_ACTIONS.contains(&action) {
        return error_result(format!(
            "沙盒内不允许 `git {action}` 自行出网：沙盒的每个网络请求都要经 `[sandbox.net]` \
             判定，让 git 自己连出去会绕过这套审批。需要联网的 git 操作请在宿主侧执行\
             （关掉沙盒，或用 `[sandbox.net]` 放行后改用 fetcher 等工具）。"
        ));
    }
    let args = match split_args(argv, action) {
        Ok(args) => args,
        Err(message) => return error_result(message),
    };
    let scope = match scope_of(action, &args) {
        Some(scope) => scope,
        None => return error_result(unsupported_action_message(action)),
    };

    let repo = match gix::open(root) {
        Ok(repo) => repo,
        Err(error) => return error_result(format!("{root:?} 不是 git 仓库：{error}。")),
    };

    match (action, scope) {
        ("status", _) => status(&repo, &args),
        ("log", _) => log(&repo, &args),
        ("diff", _) => diff(&repo, &args),
        ("add", _) => add(&repo, root, &args),
        ("commit", _) => commit(&repo, &args),
        ("rm", _) => remove(&repo, root, &args),
        ("rev-parse", _) => rev_parse(&repo, &args),
        ("ls-files", _) => ls_files(&repo, &args),
        ("show-ref", _) => show_ref(&repo, &args),
        ("for-each-ref", _) => for_each_ref(&repo, &args),
        ("describe", _) => describe(&repo, &args),
        ("cat-file", _) => cat_file(&repo, &args),
        ("branch", scope) => branch(&repo, &args, scope),
        ("tag", scope) => tag(&repo, &args, scope),
        ("stash", scope) => stash(&repo, &args, scope),
        ("remote", _) => remote(&repo, &args),
        _ => error_result(unsupported_action_message(action)),
    }
}

/// 从同形 `argv` 里剥出子命令自己的参数（去掉 `git -c … --no-pager <action>` 与尾部 `--`）。
fn split_args(argv: &[String], action: &str) -> Result<Vec<String>, String> {
    let position = argv
        .iter()
        .position(|token| token == action)
        .ok_or_else(|| format!("内部错误：argv 里找不到子命令 {action}。"))?;
    let mut args: Vec<String> = argv[position + 1..].to_vec();
    // 尾部 `--` 之后是路径列表，由各子命令自己解析，这里只把分隔符摘掉。
    if let Some(index) = args.iter().position(|token| token == "--") {
        args.remove(index);
    }
    Ok(args)
}

fn unsupported_action_message(action: &str) -> String {
    format!(
        "沙盒内 `git {action}` 没有进程内实现（Windows 真隔离下 `git.exe` 起不来，\
         会改由进程内的 gix 承接）。沙盒内目前支持：{}。\
         其余子命令请把 `[sandbox] mode` 设为 off 后在宿主侧执行。",
        NATIVE_ACTIONS.join(" / ")
    )
}

// ---------------------------------------------------------------- status

fn status(repo: &gix::Repository, args: &[String]) -> ToolResult {
    let short = args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--short" | "-s" | "--porcelain"));
    let entries = repo
        .status(gix::progress::Discard)
        .and_then(|platform| {
            platform
                .untracked_files(gix::status::UntrackedFiles::Files)
                .into_index_worktree_iter(Vec::new())
        });
    let iter = match entries {
        Ok(iter) => iter,
        Err(error) => return error_result(format!("git status 读取失败：{error}。")),
    };

    let branch = head_summary(repo);
    let mut dirty: Vec<String> = Vec::new();
    let mut untracked: Vec<String> = Vec::new();
    for item in iter {
        let item = match item {
            Ok(item) => item,
            Err(error) => return error_result(format!("git status 遍历失败：{error}。")),
        };
        let path = String::from_utf8_lossy(item.rela_path()).to_string();
        match item.summary() {
            // 索引与工作区之间：改动已暂存。
            Some(gix::status::index_worktree::iter::Summary::Added) => {
                // 目录遍历给出的是未跟踪文件；索引新增由下面的 Modified 覆盖不到，
                // 因此这里统一按「未跟踪」归类，避免把 untracked 报成已暂存。
                untracked.push(path);
            }
            Some(summary) => {
                use gix::status::index_worktree::iter::Summary::*;
                let mark = match summary {
                    Removed => 'D',
                    TypeChange => 'T',
                    Modified => 'M',
                    Conflict => 'U',
                    IntentToAdd => 'A',
                    Renamed => 'R',
                    Copied => 'C',
                    Added => 'A',
                };
                dirty.push(format!("{mark} {path}"));
            }
            None => {}
        }
    }
    dirty.sort();
    untracked.sort();

    let mut text = String::new();
    if short {
        for line in &dirty {
            text.push_str(line);
            text.push('\n');
        }
        for path in &untracked {
            text.push_str("?? ");
            text.push_str(path);
            text.push('\n');
        }
        if text.is_empty() {
            text.push_str("（工作区干净）\n");
        }
    } else {
        text.push_str(&branch);
        text.push('\n');
        if !dirty.is_empty() {
            text.push_str("\n尚未暂存的改动：\n");
            for line in &dirty {
                text.push_str("  ");
                text.push_str(line);
                text.push('\n');
            }
        }
        if !untracked.is_empty() {
            text.push_str("\n未跟踪文件：\n");
            for path in &untracked {
                text.push_str("  ");
                text.push_str(path);
                text.push('\n');
            }
        }
        if dirty.is_empty() && untracked.is_empty() {
            text.push_str("nothing to commit, working tree clean\n");
        }
    }
    ok_result(text)
}

/// 一句话描述 HEAD：`On branch main`，或游离/未提交状态。
fn head_summary(repo: &gix::Repository) -> String {
    if repo.head().is_err() {
        return "No commits yet".to_string();
    }
    match repo.head_name() {
        Ok(Some(name)) => format!("On branch {}", name.shorten()),
        _ => match repo.head_id() {
            Ok(id) => format!("HEAD detached at {}", id.detach()),
            Err(_) => "No commits yet".to_string(),
        },
    }
}

// ---------------------------------------------------------------- log

fn log(repo: &gix::Repository, args: &[String]) -> ToolResult {
    let limit = arg_value(args, "-n")
        .or_else(|| arg_value(args, "--max-count"))
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(20);
    let oneline = args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--oneline" | "--format=oneline"));

    let head = match repo.head_commit() {
        Ok(head) => head,
        Err(_) => return error_result("git log 失败：当前仓库还没有提交。".to_string()),
    };
    let walk = match head.ancestors().all() {
        Ok(walk) => walk,
        Err(error) => return error_result(format!("git log 遍历失败：{error}。")),
    };

    let mut text = String::new();
    let mut shown = 0usize;
    for info in walk {
        if shown >= limit {
            break;
        }
        let info = match info {
            Ok(info) => info,
            Err(error) => return error_result(format!("git log 遍历失败：{error}。")),
        };
        let commit = match info.id().object().and_then(|object| object.try_into_commit()) {
            Ok(commit) => commit,
            Err(error) => return error_result(format!("git log 读取提交失败：{error}。")),
        };
        let first_line = commit
            .message_raw()
            .ok()
            .and_then(|message| message.lines().next().map(|line| String::from_utf8_lossy(line).to_string()))
            .unwrap_or_default();
        if oneline {
            text.push_str(&format!("{} {}\n", short_id(&commit.id), first_line));
        } else {
            text.push_str(&format!("commit {}\n", commit.id));
            text.push_str(&format!("    {first_line}\n\n"));
        }
        shown += 1;
    }
    if shown == 0 {
        text.push_str("（没有提交）\n");
    }
    ok_result(text)
}

fn short_id(id: impl std::fmt::Display) -> String {
    let text = id.to_string();
    text.chars().take(7).collect()
}

// ---------------------------------------------------------------- diff

/// `diff` 只覆盖最常用的两种形态，都输出「哪些文件变了」，不逐行渲染统一 diff。
///
/// - 无参数：工作区 vs 索引（改动未暂存）；
/// - `--cached`/`--staged`：索引 vs HEAD（改动已暂存）。
///
/// 逐行正文没有现成的高层渲染，硬拼会引入一大段与 git 输出格式不一致的实现；
/// 需要看具体内容时用 `read` 读文件本身，模型侧反而更准。
fn diff(repo: &gix::Repository, args: &[String]) -> ToolResult {
    let cached = args.iter().any(|arg| arg == "--cached" || arg == "--staged");
    if cached {
        let head_tree = match repo.head_commit() {
            Ok(commit) => match commit.tree() {
                Ok(tree) => Some(tree),
                Err(error) => return error_result(format!("git diff 读取 HEAD 树失败：{error}。")),
            },
            // 还没有提交：索引相对空树全都是新增，用 status 的口径表达。
            Err(_) => None,
        };
        let changes = match repo.diff_tree_to_tree(head_tree.as_ref(), None, None) {
            Ok(changes) => changes,
            Err(error) => return error_result(format!("git diff 失败：{error}。")),
        };
        let mut text = String::new();
        for change in changes {
            text.push_str(&format!("{}
", change.location()));
        }
        if text.is_empty() {
            text.push_str("（已暂存区没有改动）
");
        }
        return ok_result(text);
    }

    // 工作区 vs 索引：用 status 的条目表达改动集合。
    let entries = repo
        .status(gix::progress::Discard)
        .and_then(|platform| {
            platform
                .untracked_files(gix::status::UntrackedFiles::Files)
                .into_index_worktree_iter(Vec::new())
        });
    let iter = match entries {
        Ok(iter) => iter,
        Err(error) => return error_result(format!("git diff 读取工作区失败：{error}。")),
    };
    let mut text = String::new();
    for item in iter {
        let item = match item {
            Ok(item) => item,
            Err(error) => return error_result(format!("git diff 遍历失败：{error}。")),
        };
        if let Some(summary) = item.summary() {
            text.push_str(&format!(
                "{summary:?} {}
",
                String::from_utf8_lossy(item.rela_path())
            ));
        }
    }
    if text.is_empty() {
        text.push_str("（工作区没有改动）
");
    }
    ok_result(text)
}

// ---------------------------------------------------------------- add

/// `add` 把路径写进索引。
///
/// 只处理工作区内的普通文件路径；`-A`/`--all` 这类「整仓暂存」也支持，但依然逐个文件
/// 走 `open`，因此不会把工作区之外的东西带进索引。
fn add(repo: &gix::Repository, root: &Path, args: &[String]) -> ToolResult {
    let all = args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-A" | "--all" | "."));
    let mut paths: Vec<String> = args
        .iter()
        .filter(|arg| !arg.starts_with('-') && arg.as_str() != ".")
        .cloned()
        .collect();
    if all && paths.is_empty() {
        paths = match tracked_and_untracked(repo) {
            Ok(paths) => paths,
            Err(message) => return error_result(message),
        };
    }
    if paths.is_empty() {
        return error_result("git add 需要给出 paths 或传入 -A/--all。".to_string());
    }

    // `index_or_empty` 给的是只读快照（`Rc`），写索引前必须取回独占所有权。
    let mut index = match repo.index_or_load_from_head_or_empty().map(|index| index.into_owned()) {
        Ok(index) => index,
        Err(error) => return error_result(format!("git add 读取索引失败：{error}。")),
    };
    let mut added = Vec::new();
    for path in &paths {
        let absolute = root.join(path);
        if !absolute.starts_with(root) {
            return error_result(format!("路径越界（必须在工作区内）：{path}。"));
        }
        let bytes = match std::fs::read(&absolute) {
            Ok(bytes) => bytes,
            Err(error) => return error_result(format!("git add 读取 {path} 失败：{error}。")),
        };
        let blob = match repo.write_blob(&bytes) {
            Ok(id) => id.detach(),
            Err(error) => return error_result(format!("git add 写入 {path} 失败：{error}。")),
        };
        let stat = gix::index::fs::Metadata::from_path_no_follow(&absolute)
            .ok()
            .and_then(|meta| gix::index::entry::Stat::from_fs(&meta).ok())
            .unwrap_or_default();
        index.dangerously_push_entry(
            stat,
            blob,
            gix::index::entry::Flags::empty(),
            gix::index::entry::Mode::FILE,
            path.as_bytes().as_bstr(),
        );
        added.push(path.clone());
    }
    index.sort_entries();
    if let Err(error) = index.write(gix::index::write::Options::default()) {
        return error_result(format!("git add 写索引失败：{error}。"));
    }
    ok_result(format!("已暂存：{}\n", added.join(", ")))
}

fn tracked_and_untracked(repo: &gix::Repository) -> Result<Vec<String>, String> {
    let mut paths = Vec::new();
    if let Ok(index) = repo.index_or_empty() {
        for entry in index.entries() {
            if let Ok(path) = entry.path(&index).to_str() {
                paths.push(path.to_string());
            }
        }
    }
    let iter = repo
        .status(gix::progress::Discard)
        .and_then(|platform| {
            platform
                .untracked_files(gix::status::UntrackedFiles::Files)
                .into_index_worktree_iter(Vec::new())
        })
        .map_err(|error| format!("git add 遍历工作区失败：{error}。"))?;
    for item in iter {
        let item = item.map_err(|error| format!("git add 遍历工作区失败：{error}。"))?;
        paths.push(String::from_utf8_lossy(item.rela_path()).to_string());
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

// ---------------------------------------------------------------- commit

/// `commit` 把当前索引写成一次提交。
///
/// 索引 -> 树没有高层 API，因此以 HEAD 树为基准、按索引条目逐条 `upsert` 构树。
/// HEAD 不存在（首个提交）时从空树开始。
fn commit(repo: &gix::Repository, args: &[String]) -> ToolResult {
    let message = arg_value(args, "-m")
        .or_else(|| arg_value(args, "--message"))
        .unwrap_or_default();
    if message.trim().is_empty() {
        return error_result("commit 必须提供 message。".to_string());
    }
    if args.iter().any(|arg| arg == "--dry-run") {
        return ok_result("（--dry-run：未创建提交）\n".to_string());
    }

    let index = match repo.index_or_load_from_head_or_empty().map(|index| index.into_owned()) {
        Ok(index) => index,
        Err(error) => return error_result(format!("git commit 读取索引失败：{error}。")),
    };

    // `head_id` 是**提交**对象，构树要的是它的树：直接拿提交 id 去 `edit_tree` 会被
    // 判成「本该是 tree，却是 commit」。
    let parent = repo.head_id().ok().map(|id| id.detach());
    let base_tree = match &parent {
        Some(id) => match repo.find_object(*id).and_then(|object| object.try_into_commit()) {
            Ok(commit) => match commit.tree_id() {
                Ok(tree) => Some(tree.detach()),
                Err(error) => return error_result(format!("git commit 读取 HEAD 树失败：{error}。")),
            },
            Err(error) => return error_result(format!("git commit 读取 HEAD 提交失败：{error}。")),
        },
        None => None,
    };
    let mut editor = match base_tree {
        Some(tree) => match repo.edit_tree(tree) {
            Ok(editor) => editor,
            Err(error) => return error_result(format!("git commit 打开 HEAD 树失败：{error}。")),
        },
        None => match empty_tree_editor(repo) {
            Ok(editor) => editor,
            Err(message) => return error_result(message),
        },
    };

    for entry in index.entries() {
        let path = match entry.path(&index).to_str() {
            Ok(path) => path.to_string(),
            Err(_) => continue,
        };
        let kind = match entry.mode {
            gix::index::entry::Mode::FILE => gix::object::tree::EntryKind::Blob,
            gix::index::entry::Mode::FILE_EXECUTABLE => gix::object::tree::EntryKind::BlobExecutable,
            gix::index::entry::Mode::SYMLINK => gix::object::tree::EntryKind::Link,
            gix::index::entry::Mode::DIR => continue,
            _ => gix::object::tree::EntryKind::Blob,
        };
        if let Err(error) = editor.upsert(path.as_str(), kind, entry.id) {
            return error_result(format!("git commit 构树失败（{path}）：{error}。"));
        }
    }
    let tree = match editor.write() {
        Ok(id) => id.detach(),
        Err(error) => return error_result(format!("git commit 写树失败：{error}。")),
    };

    match repo.commit("HEAD", message.clone(), tree, parent) {
        Ok(id) => ok_result(format!("[{}] {}\n", short_id(id), message)),
        Err(error) => error_result(format!("git commit 失败：{error}。")),
    }
}

/// 首个提交：以一个空树为起点。
fn empty_tree_editor<'repo>(repo: &'repo gix::Repository) -> Result<gix::object::tree::Editor<'repo>, String> {
    let empty = repo
        .write_object(&gix::objs::Tree { entries: Vec::new() })
        .map_err(|error| format!("git commit 写空树失败：{error}。"))?;
    repo.edit_tree(empty.detach())
        .map_err(|error| format!("git commit 打开空树失败：{error}。"))
}

// ---------------------------------------------------------------- rm

/// `rm` 负责「从索引移除」，带 `--cached` 时工作区文件原样保留。
///
/// 沙盒内的破坏面上限因此是「索引里少了几条 + 被点名的文件」，不会顺手清掉整棵工作区。
fn remove(repo: &gix::Repository, root: &Path, args: &[String]) -> ToolResult {
    let cached = args.iter().any(|arg| arg == "--cached");
    let all = args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-r" | "-A" | "--all" | "."));
    let mut paths: Vec<String> = args
        .iter()
        .filter(|arg| !arg.starts_with('-') && arg.as_str() != ".")
        .cloned()
        .collect();
    if all && paths.is_empty() {
        match index_paths(repo) {
            Ok(found) => paths = found,
            Err(message) => return error_result(message),
        }
    }
    if paths.is_empty() {
        return error_result("git rm 需要给出 paths 或传入 -r/-A。".to_string());
    }

    let mut index = match repo
        .index_or_load_from_head_or_empty()
        .map(|index| index.into_owned())
    {
        Ok(index) => index,
        Err(error) => return error_result(format!("git rm 读取索引失败：{error}。")),
    };
    let mut removed = Vec::new();
    for path in &paths {
        if !root.join(path).starts_with(root) {
            return error_result(format!("路径越界（必须在工作区内）：{path}。"));
        }
        let target = path.clone();
        let mut found = false;
        index.remove_entries(|_, entry_path, _| {
            let matched = entry_path == target.as_bytes().as_bstr();
            found |= matched;
            matched
        });
        if !found {
            return error_result(format!("git rm：索引里没有 {path}。"));
        }
        removed.push(path.clone());
        // 与 git 默认行为一致：不带 `--cached` 时连工作区文件一起删。
        if !cached {
            let absolute = root.join(path);
            if absolute.exists() {
                if let Err(error) = std::fs::remove_file(&absolute) {
                    return error_result(format!("git rm 删除 {path} 失败：{error}。"));
                }
            }
        }
    }
    if let Err(error) = index.write(gix::index::write::Options::default()) {
        return error_result(format!("git rm 写索引失败：{error}。"));
    }
    ok_result(format!("已从索引移除：{}\n", removed.join(", ")))
}

fn index_paths(repo: &gix::Repository) -> Result<Vec<String>, String> {
    let index = repo
        .index_or_load_from_head_or_empty()
        .map_err(|error| format!("读取索引失败：{error}。"))?;
    let mut paths = Vec::new();
    for entry in index.entries() {
        if let Ok(path) = entry.path(&index).to_str() {
            paths.push(path.to_string());
        }
    }
    Ok(paths)
}

// ---------------------------------------------------------------- rev-parse

fn rev_parse(repo: &gix::Repository, args: &[String]) -> ToolResult {
    let specs: Vec<&String> = args.iter().filter(|arg| !arg.starts_with('-')).collect();
    if specs.is_empty() {
        return match repo.head_id() {
            Ok(id) => ok_result(format!("{}\n", id.detach())),
            Err(_) => error_result("git rev-parse：当前仓库还没有提交。".to_string()),
        };
    }
    let mut text = String::new();
    for spec in specs {
        match repo.rev_parse_single(spec.as_str().as_bytes().as_bstr()) {
            Ok(id) => text.push_str(&format!("{}\n", id.detach())),
            Err(error) => return error_result(format!("git rev-parse {spec} 失败：{error}。")),
        }
    }
    ok_result(text)
}

// ---------------------------------------------------------------- ls-files

fn ls_files(repo: &gix::Repository, args: &[String]) -> ToolResult {
    let paths = match index_paths(repo) {
        Ok(paths) => paths,
        Err(message) => return error_result(message),
    };
    let filter = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .cloned();
    let mut text = String::new();
    for path in paths {
        if let Some(prefix) = &filter {
            if !path.starts_with(prefix.as_str()) {
                continue;
            }
        }
        text.push_str(&path);
        text.push('\n');
    }
    if text.is_empty() {
        text.push_str("（索引为空）\n");
    }
    ok_result(text)
}

// ---------------------------------------------------------------- show-ref / for-each-ref

fn show_ref(repo: &gix::Repository, args: &[String]) -> ToolResult {
    let heads_only = args.iter().any(|arg| arg == "--heads");
    let tags_only = args.iter().any(|arg| arg == "--tags");
    let platform = match repo.references() {
        Ok(platform) => platform,
        Err(error) => return error_result(format!("读取引用失败：{error}。")),
    };
    let iter = match platform.all() {
        Ok(iter) => iter,
        Err(error) => return error_result(format!("git show-ref 读取引用失败：{error}。")),
    };
    let mut text = String::new();
    for reference in iter {
        let reference = match reference {
            Ok(reference) => reference,
            Err(error) => return error_result(format!("git show-ref 读取引用失败：{error}。")),
        };
        let name = reference.name().as_bstr().to_string();
        if heads_only && !name.starts_with("refs/heads/") {
            continue;
        }
        if tags_only && !name.starts_with("refs/tags/") {
            continue;
        }
        if let Some(id) = reference.target().try_id() {
            text.push_str(&format!("{id} {name}\n"));
        }
    }
    if text.is_empty() {
        text.push_str("（没有引用）\n");
    }
    ok_result(text)
}

fn for_each_ref(repo: &gix::Repository, args: &[String]) -> ToolResult {
    // 自定义 `--format`/`--sort` 需要占位符解释器，输出与 git 有差异会误导模型，
    // 因此只承接默认形式，其余明确拒绝。
    if args
        .iter()
        .any(|arg| arg.starts_with("--format") || arg.starts_with("--sort"))
    {
        return error_result(
            "沙盒内的 `git for-each-ref` 只支持默认输出，不支持 --format/--sort。".to_string(),
        );
    }
    show_ref(repo, &[])
}

// ---------------------------------------------------------------- describe

fn describe(repo: &gix::Repository, args: &[String]) -> ToolResult {
    if args.iter().any(|arg| arg.starts_with("--")) {
        return error_result(
            "沙盒内的 `git describe` 只支持默认形式（最近标签 + 距离 + 短 id）。".to_string(),
        );
    }
    let commit = match repo.head_commit() {
        Ok(commit) => commit,
        Err(_) => return error_result("git describe 失败：当前仓库还没有提交。".to_string()),
    };
    match commit.describe().try_format() {
        Ok(Some(format)) => ok_result(format!("{format}\n")),
        // 没有标签时退回短 id，接近 `--always` 的用法。
        Ok(None) => ok_result(format!("{}\n", short_id(&commit.id))),
        Err(error) => error_result(format!("git describe 失败：{error}。")),
    }
}

// ---------------------------------------------------------------- cat-file

fn cat_file(repo: &gix::Repository, args: &[String]) -> ToolResult {
    let type_only = args.iter().any(|arg| arg == "-t");
    let size_only = args.iter().any(|arg| arg == "-s");
    let specs: Vec<&String> = args.iter().filter(|arg| !arg.starts_with('-')).collect();
    let Some(spec) = specs.first() else {
        return error_result("git cat-file 需要给出对象名。".to_string());
    };
    let id = match repo.rev_parse_single(spec.as_str().as_bytes().as_bstr()) {
        Ok(id) => id.detach(),
        Err(error) => return error_result(format!("git cat-file {spec} 失败：{error}。")),
    };
    let object = match repo.find_object(id) {
        Ok(object) => object,
        Err(error) => return error_result(format!("git cat-file 读取对象失败：{error}。")),
    };
    if type_only {
        return ok_result(format!("{}\n", object.kind));
    }
    let data = object.data.as_slice();
    if size_only {
        return ok_result(format!("{}\n", data.len()));
    }
    match object.kind {
        // 提交与标签按文本给出：模型最常要的就是这两种。
        gix::object::Kind::Commit | gix::object::Kind::Tag => {
            ok_result(String::from_utf8_lossy(data).to_string())
        }
        // blob 可能是二进制，只报大小，避免把字节倒进上下文。
        gix::object::Kind::Blob => ok_result(format!(
            "（blob，{} 字节；内容请用 read 工具按路径查看）\n",
            data.len()
        )),
        gix::object::Kind::Tree => ok_result(format!("（tree，{} 字节）\n", data.len())),
    }
}

// ---------------------------------------------------------------- branch / tag / stash / remote

fn branch(repo: &gix::Repository, args: &[String], scope: Scope) -> ToolResult {
    let positionals: Vec<&String> = args.iter().filter(|arg| !arg.starts_with('-')).collect();
    if scope == Scope::ReadOnly {
        let platform = match repo.references() {
            Ok(platform) => platform,
            Err(error) => return error_result(format!("读取分支失败：{error}。")),
        };
        let iter = match platform.local_branches() {
            Ok(iter) => iter,
            Err(error) => return error_result(format!("git branch 读取分支失败：{error}。")),
        };
        let current = repo
            .head_name()
            .ok()
            .flatten()
            .map(|name| name.shorten().to_string());
        let mut names: Vec<(String, String)> = Vec::new();
        for reference in iter {
            let reference = match reference {
                Ok(reference) => reference,
                Err(error) => return error_result(format!("git branch 读取分支失败：{error}。")),
            };
            let short = reference.name().shorten().to_string();
            let id = reference
                .target()
                .try_id()
                .map(short_id)
                .unwrap_or_default();
            names.push((short, id));
        }
        names.sort();
        let mut text = String::new();
        for (short, id) in names {
            let mark = if Some(&short) == current.as_ref() {
                '*'
            } else {
                ' '
            };
            text.push_str(&format!("{mark} {short} {id}\n"));
        }
        if text.is_empty() {
            text.push_str("（没有分支）\n");
        }
        return ok_result(text);
    }

    let deleting = args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-d" | "-D" | "--delete"));
    let Some(name) = positionals.first() else {
        return error_result("git branch 需要给出分支名。".to_string());
    };
    if deleting {
        let full = format!("refs/heads/{name}");
        let reference = match repo.find_reference(full.as_str()) {
            Ok(reference) => reference,
            Err(error) => return error_result(format!("git branch 找不到 {name}：{error}。")),
        };
        return match reference.delete() {
            Ok(()) => ok_result(format!("已删除分支 {name}\n")),
            Err(error) => error_result(format!("git branch 删除 {name} 失败：{error}。")),
        };
    }

    // 新建：`branch <name> [<start-point>]`
    let target = match positionals.get(1) {
        Some(spec) => match repo.rev_parse_single(spec.as_str().as_bytes().as_bstr()) {
            Ok(id) => id.detach(),
            Err(error) => return error_result(format!("git branch 解析 {spec} 失败：{error}。")),
        },
        None => match repo.head_id() {
            Ok(id) => id.detach(),
            Err(_) => return error_result("git branch：当前仓库还没有提交。".to_string()),
        },
    };
    let full = format!("refs/heads/{name}");
    // 约束由这里显式判定：底层 `MustNotExist` 在当前组合下不会拒绝已存在的引用，
    // 放过去就会让模型以为分支被重建了。
    if repo.find_reference(full.as_str()).is_ok() {
        return error_result(format!("分支 {name} 已存在。"));
    }
    match repo.reference(
        full.as_str(),
        target,
        gix::refs::transaction::PreviousValue::MustNotExist,
        format!("branch: Created from {target}"),
    ) {
        Ok(_) => ok_result(format!("已创建分支 {name}\n")),
        Err(error) => error_result(format!("git branch 创建 {name} 失败：{error}。")),
    }
}

fn tag(repo: &gix::Repository, args: &[String], scope: Scope) -> ToolResult {
    let positionals: Vec<&String> = args.iter().filter(|arg| !arg.starts_with('-')).collect();
    if scope == Scope::ReadOnly {
        let platform = match repo.references() {
        Ok(platform) => platform,
        Err(error) => return error_result(format!("读取引用失败：{error}。")),
    };
    let iter = match platform.all() {
            Ok(iter) => iter,
            Err(error) => return error_result(format!("git tag 读取标签失败：{error}。")),
        };
        let mut names: Vec<(String, String)> = Vec::new();
        for reference in iter {
            let reference = match reference {
                Ok(reference) => reference,
                Err(error) => return error_result(format!("git tag 读取标签失败：{error}。")),
            };
            let name = reference.name().as_bstr().to_string();
            if !name.starts_with("refs/tags/") {
                continue;
            }
            let short = name.trim_start_matches("refs/tags/").to_string();
            let id = reference
                .target()
                .try_id()
                .map(short_id)
                .unwrap_or_default();
            names.push((short, id));
        }
        names.sort();
        let mut text = String::new();
        for (short, id) in names {
            text.push_str(&format!("{short} {id}\n"));
        }
        if text.is_empty() {
            text.push_str("（没有标签）\n");
        }
        return ok_result(text);
    }

    let deleting = args
        .iter()
        .any(|arg| matches!(arg.as_str(), "-d" | "--delete"));
    let Some(name) = positionals.first() else {
        return error_result("git tag 需要给出标签名。".to_string());
    };
    if deleting {
        let full = format!("refs/tags/{name}");
        let reference = match repo.find_reference(full.as_str()) {
            Ok(reference) => reference,
            Err(error) => return error_result(format!("git tag 找不到 {name}：{error}。")),
        };
        return match reference.delete() {
            Ok(()) => ok_result(format!("已删除标签 {name}\n")),
            Err(error) => error_result(format!("git tag 删除 {name} 失败：{error}。")),
        };
    }
    // 轻量标签（未带 -a/-m）：直接建引用，与 `git tag <name>` 一致。
    let target = match positionals.get(1) {
        Some(spec) => match repo.rev_parse_single(spec.as_str().as_bytes().as_bstr()) {
            Ok(id) => id.detach(),
            Err(error) => return error_result(format!("git tag 解析 {spec} 失败：{error}。")),
        },
        None => match repo.head_id() {
            Ok(id) => id.detach(),
            Err(_) => return error_result("git tag：当前仓库还没有提交。".to_string()),
        },
    };
    let full = format!("refs/tags/{name}");
    if repo.find_reference(full.as_str()).is_ok() {
        return error_result(format!("标签 {name} 已存在。"));
    }
    match repo.reference(
        full.as_str(),
        target,
        gix::refs::transaction::PreviousValue::MustNotExist,
        format!("tag: Created from {target}"),
    ) {
        Ok(_) => ok_result(format!("已创建标签 {name}\n")),
        Err(error) => error_result(format!("git tag 创建 {name} 失败：{error}。")),
    }
}

/// `stash` 只承接只读形态；写形态需要工作区状态机，明确拒绝而不是给半对的实现。
fn stash(repo: &gix::Repository, _args: &[String], scope: Scope) -> ToolResult {
    if scope == Scope::Write {
        return error_result(
            "沙盒内不支持写 stash（需要工作区状态机）。只支持 `git stash` / `git stash list` / \
             `git stash show`。"
                .to_string(),
        );
    }
    // 引用枚举器没有 stash 专用入口，按名字前缀自己过滤；否则会把分支和标签一起列出来。
    let platform = match repo.references() {
        Ok(platform) => platform,
        Err(_) => return ok_result("（没有 stash）\n".to_string()),
    };
    let iter = match platform.all() {
        Ok(iter) => iter,
        Err(_) => return ok_result("（没有 stash）\n".to_string()),
    };
    let mut text = String::new();
    for reference in iter {
        let Ok(reference) = reference else { break };
        let name = reference.name().as_bstr().to_string();
        if !name.starts_with("refs/stash") {
            continue;
        }
        if let Some(id) = reference.target().try_id() {
            text.push_str(&format!("stash@{{{}}} {}\n", text.lines().count(), id));
        }
    }
    if text.is_empty() {
        text.push_str("（没有 stash）\n");
    }
    ok_result(text)
}

fn remote(repo: &gix::Repository, args: &[String]) -> ToolResult {
    let verbose = args.iter().any(|arg| arg == "-v" || arg == "--verbose");
    // 只读配置：列出远程名与 URL，不联网、不改配置。
    let config = repo.config_snapshot();
    let mut names: Vec<String> = Vec::new();
    for section in config.sections() {
        let header = section.header();
        if header.name() == "remote" {
            if let Some(name) = header.subsection_name() {
                names.push(name.to_string());
            }
        }
    }
    names.sort();
    names.dedup();
    if names.is_empty() {
        return ok_result("（没有远程）\n".to_string());
    }
    let mut text = String::new();
    for name in names {
        if verbose {
            let url = config
                .string_by("remote", Some(name.as_str().into()), "url")
                .map(|value| value.to_string())
                .unwrap_or_default();
            text.push_str(&format!("{name}\t{url} (fetch)\n"));
        } else {
            text.push_str(&format!("{name}\n"));
        }
    }
    ok_result(text)
}

// ---------------------------------------------------------------- 参数小工具

/// 取形如 `-n 5` / `--max-count=5` / `--message=...` 的取值。
fn arg_value(args: &[String], name: &str) -> Option<String> {
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        if arg == name {
            return iter.peek().map(|value| (*value).clone());
        }
        if let Some(rest) = arg.strip_prefix(&format!("{name}=")) {
            return Some(rest.to_string());
        }
    }
    None
}

fn ok_result(text: String) -> ToolResult {
    ToolResult {
        ok: true,
        full_output: bound_text(&text),
        output: bound_text(&text),
        error_code: None,
        retryable: false,
    }
}

fn error_result(message: String) -> ToolResult {
    ToolResult {
        ok: false,
        full_output: message.clone(),
        output: message,
        error_code: None,
        retryable: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("omnicrawl-git-native-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("建目录");
        root
    }

    fn argv(action: &str, args: &[&str]) -> Vec<String> {
        let mut argv = vec![
            "git".to_string(),
            "-c".to_string(),
            "color.ui=never".to_string(),
            "--no-pager".to_string(),
            action.to_string(),
        ];
        argv.extend(args.iter().map(|arg| arg.to_string()));
        argv
    }

    fn init_repo(root: &Path) {
        let status = std::process::Command::new("git")
            .arg("init")
            .arg("--quiet")
            .current_dir(root)
            .status();
        match status {
            Ok(status) if status.success() => {}
            _ => panic!("本机没有可用的 git，无法构造测试仓库"),
        }
        let _ = std::process::Command::new("git")
            .args(["config", "user.email", "t@example.com"])
            .current_dir(root)
            .status();
        let _ = std::process::Command::new("git")
            .args(["config", "user.name", "T"])
            .current_dir(root)
            .status();
    }

    /// 网络子命令是**不允许**，不是「暂不支持」：沙盒出网必须经 `[sandbox.net]` 判定。
    #[test]
    fn network_actions_are_refused() {
        let root = scratch("net");
        init_repo(&root);
        for action in NETWORK_ACTIONS {
            let result = run_native(&argv(action, &[]), action, &root);
            assert!(!result.ok, "{action} 必须被拒");
            assert!(result.output.contains("出网"), "{action}: {}", result.output);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 不承接的子命令必须明确拒绝，并指出沙盒内支持的范围——绝不能悄悄退回 spawn git。
    #[test]
    fn unsupported_actions_point_at_the_supported_set() {
        let root = scratch("unsupported");
        init_repo(&root);
        let result = run_native(&argv("rebase", &[]), "rebase", &root);
        assert!(!result.ok);
        for action in NATIVE_ACTIONS {
            assert!(result.output.contains(action), "应列出 {action}：{}", result.output);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 作用域判定：`commit --dry-run` 不落提交，因此按只读处理。
    #[test]
    fn scope_follows_the_action() {
        assert_eq!(scope_of("status", &[]), Some(Scope::ReadOnly));
        assert_eq!(scope_of("log", &[]), Some(Scope::ReadOnly));
        assert_eq!(scope_of("diff", &[]), Some(Scope::ReadOnly));
        assert_eq!(scope_of("add", &[]), Some(Scope::Write));
        assert_eq!(scope_of("commit", &[]), Some(Scope::Write));
        assert_eq!(scope_of("commit", &["--dry-run".to_string()]), Some(Scope::ReadOnly));
        assert_eq!(scope_of("push", &[]), None);
    }

    /// 主链路：add 写索引、commit 落提交、log 读回来，最后用真 git 交叉验证。
    #[test]
    fn add_commit_and_log_round_trip() {
        let root = scratch("roundtrip");
        init_repo(&root);
        std::fs::write(root.join("a.txt"), "hello\n").expect("写文件");

        let add = run_native(&argv("add", &["a.txt"]), "add", &root);
        assert!(add.ok, "{}", add.output);

        let commit = run_native(&argv("commit", &["-m", "native commit"]), "commit", &root);
        assert!(commit.ok, "{}", commit.output);

        let log = run_native(&argv("log", &["--oneline"]), "log", &root);
        assert!(log.ok, "{}", log.output);
        assert!(log.output.contains("native commit"), "{}", log.output);

        // 真 git 必须能读到 gix 写的提交，否则产物不是合法仓库状态。
        let verify = std::process::Command::new("git")
            .args(["log", "--oneline"])
            .current_dir(&root)
            .output()
            .expect("跑真 git");
        let text = String::from_utf8_lossy(&verify.stdout);
        assert!(text.contains("native commit"), "真 git 读不到：{text}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 首次提交（空仓库、无 HEAD）：必须从空树起构，而不是把提交 id 当树用。
    ///
    /// 这条曾是真机端到端测试抓出来的缺陷：`head_id` 是提交对象，直接喂给 `edit_tree`
    /// 会被判「本该是 tree，却是 commit」。单元测试不覆盖这条边界就发不出去。
    #[test]
    fn first_commit_in_a_fresh_repository() {
        let root = scratch("first-commit");
        init_repo(&root);
        std::fs::write(root.join("f.txt"), "first
").expect("写文件");

        let add = run_native(&argv("add", &["f.txt"]), "add", &root);
        assert!(add.ok, "{}", add.output);

        let commit = run_native(&argv("commit", &["-m", "first"]), "commit", &root);
        assert!(commit.ok, "{}", commit.output);

        // 第二次提交要基于已有 HEAD 树。
        std::fs::write(root.join("g.txt"), "second
").expect("写文件");
        let add2 = run_native(&argv("add", &["g.txt"]), "add", &root);
        assert!(add2.ok, "{}", add2.output);
        let commit2 = run_native(&argv("commit", &["-m", "second"]), "commit", &root);
        assert!(commit2.ok, "{}", commit2.output);

        let log = run_native(&argv("log", &["--oneline"]), "log", &root);
        assert!(log.output.contains("second"), "{}", log.output);
        assert!(log.output.contains("first"), "{}", log.output);

        // 真 git 要能校验通过，否则产物不是合法仓库状态。
        let verify = std::process::Command::new("git")
            .args(["log", "--oneline"])
            .current_dir(&root)
            .output()
            .expect("跑真 git");
        let text = String::from_utf8_lossy(&verify.stdout);
        assert!(text.contains("first"), "真 git 读不到：{text}");
        assert!(text.contains("second"), "真 git 读不到：{text}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 只读子命令要真的读出东西，而不是空跑。
    #[test]
    fn read_only_actions_answer_meaningfully() {
        let root = scratch("readonly");
        init_repo(&root);
        std::fs::write(root.join("a.txt"), "hello
").expect("写文件");
        assert!(run_native(&argv("add", &["a.txt"]), "add", &root).ok);
        assert!(run_native(&argv("commit", &["-m", "base"]), "commit", &root).ok);

        let rev = run_native(&argv("rev-parse", &["HEAD"]), "rev-parse", &root);
        assert!(rev.ok, "{}", rev.output);
        assert_eq!(rev.output.trim().len(), 40, "应给出 40 位提交 id：{}", rev.output);

        let files = run_native(&argv("ls-files", &[]), "ls-files", &root);
        assert!(files.ok, "{}", files.output);
        assert!(files.output.contains("a.txt"), "{}", files.output);

        let refs = run_native(&argv("show-ref", &[]), "show-ref", &root);
        assert!(refs.ok, "{}", refs.output);
        assert!(refs.output.contains("refs/heads/"), "{}", refs.output);

        let each = run_native(&argv("for-each-ref", &[]), "for-each-ref", &root);
        assert!(each.ok, "{}", each.output);

        let cat = run_native(&argv("cat-file", &["-t", "HEAD"]), "cat-file", &root);
        assert!(cat.ok, "{}", cat.output);
        assert!(cat.output.to_lowercase().contains("commit"), "{}", cat.output);

        let describe = run_native(&argv("describe", &[]), "describe", &root);
        assert!(describe.ok, "{}", describe.output);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// branch / tag 的建删要能被真 git 看到，并区分「列举」与「新建」两种作用域。
    #[test]
    fn branch_and_tag_writes_are_visible_to_real_git() {
        let root = scratch("refs");
        init_repo(&root);
        std::fs::write(root.join("a.txt"), "hello
").expect("写文件");
        assert!(run_native(&argv("add", &["a.txt"]), "add", &root).ok);
        assert!(run_native(&argv("commit", &["-m", "base"]), "commit", &root).ok);

        // 列举是只读，新建是可写。
        assert_eq!(scope_of("branch", &[]), Some(Scope::ReadOnly));
        assert_eq!(scope_of("branch", &["topic".to_string()]), Some(Scope::Write));
        assert_eq!(scope_of("tag", &[]), Some(Scope::ReadOnly));
        assert_eq!(scope_of("tag", &["v1".to_string()]), Some(Scope::Write));

        let made = run_native(&argv("branch", &["topic"]), "branch", &root);
        assert!(made.ok, "{}", made.output);
        let listed = run_native(&argv("branch", &[]), "branch", &root);
        assert!(listed.output.contains("topic"), "{}", listed.output);

        let tagged = run_native(&argv("tag", &["v1"]), "tag", &root);
        assert!(tagged.ok, "{}", tagged.output);

        let out = std::process::Command::new("git")
            .args(["show-ref", "--heads", "--tags"])
            .current_dir(&root)
            .output()
            .expect("跑真 git");
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("refs/heads/topic"), "真 git 看不到分支：{text}");
        assert!(text.contains("refs/tags/v1"), "真 git 看不到标签：{text}");

        // 重复建同名分支必须失败，与真 git 一致。
        let again = run_native(&argv("branch", &["topic"]), "branch", &root);
        assert!(!again.ok, "重复创建分支必须失败：{}", again.output);

        let deleted = run_native(&argv("branch", &["-d", "topic"]), "branch", &root);
        assert!(deleted.ok, "{}", deleted.output);
        let after = std::process::Command::new("git")
            .args(["show-ref", "--heads"])
            .current_dir(&root)
            .output()
            .expect("跑真 git");
        assert!(!String::from_utf8_lossy(&after.stdout).contains("topic"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// `rm --cached` 只动索引，工作区文件必须还在。
    #[test]
    fn rm_cached_keeps_the_working_tree_file() {
        let root = scratch("rm-cached");
        init_repo(&root);
        std::fs::write(root.join("a.txt"), "hello
").expect("写文件");
        assert!(run_native(&argv("add", &["a.txt"]), "add", &root).ok);
        assert!(run_native(&argv("commit", &["-m", "base"]), "commit", &root).ok);

        let removed = run_native(&argv("rm", &["--cached", "a.txt"]), "rm", &root);
        assert!(removed.ok, "{}", removed.output);
        assert!(root.join("a.txt").exists(), "工作区文件不该被删");

        let files = run_native(&argv("ls-files", &[]), "ls-files", &root);
        assert!(!files.output.contains("a.txt"), "{}", files.output);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 写 stash 与 merge 之类必须明确拒绝，不能给出半对的实现。
    #[test]
    fn unsupported_writes_are_refused_explicitly() {
        let root = scratch("refuse");
        init_repo(&root);
        std::fs::write(root.join("a.txt"), "hello
").expect("写文件");
        assert!(run_native(&argv("add", &["a.txt"]), "add", &root).ok);
        assert!(run_native(&argv("commit", &["-m", "base"]), "commit", &root).ok);

        let stash = run_native(&argv("stash", &["push"]), "stash", &root);
        assert!(!stash.ok);
        assert!(stash.output.contains("不支持写 stash"), "{}", stash.output);

        // 只读 stash 形态是允许的（空仓库时给「没有 stash」）。
        let listed = run_native(&argv("stash", &[]), "stash", &root);
        assert!(listed.ok, "{}", listed.output);

        let merge = run_native(&argv("merge", &["other"]), "merge", &root);
        assert!(!merge.ok);
        assert!(merge.output.contains("status"), "{}", merge.output);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 未初始化目录要给出「不是 git 仓库」，而不是含糊的底层错误。
    #[test]
    fn non_repository_reports_clearly() {
        let root = scratch("notrepo");
        let result = run_native(&argv("status", &[]), "status", &root);
        assert!(!result.ok);
        assert!(result.output.contains("不是 git 仓库"), "{}", result.output);
        let _ = std::fs::remove_dir_all(&root);
    }
}
