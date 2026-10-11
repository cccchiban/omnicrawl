//! Skill 发现链路的补充覆盖：企业级作用域、额外路径、同名冲突与 frontmatter 块标量。
//!
//! 这些分支此前没有测试钉住（块标量在真实 Skill 目录里已经出现过，解析成字面量 `>-`）。
//! 用例只碰临时目录，不写任何项目文件。

use std::path::{Path, PathBuf};

use omnicrawl_extensions::skill::{parse_frontmatter, SkillManager};
use serde_json::Value;

fn temp_dir(tag: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let root =
        std::env::temp_dir().join(format!("omnicrawl-skill-{tag}-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("临时目录创建失败");
    root
}

fn write_skill(dir: &Path, name: &str, frontmatter: &str) {
    let skill_dir = dir.join(name);
    std::fs::create_dir_all(&skill_dir).expect("skill 目录创建失败");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\n{frontmatter}\n---\n\n正文\n"),
    )
    .expect("SKILL.md 写入失败");
}

/// 企业级作用域：目录由调用方显式给出，扫描结果标记为 `enterprise`。
#[test]
fn enterprise_scope_scans_given_directory() {
    let workspace = temp_dir("enterprise-ws");
    let enterprise = temp_dir("enterprise-dir");
    write_skill(
        &enterprise,
        "enterprise-skill",
        "name: enterprise-skill\ndescription: 企业级",
    );

    let mut manager = SkillManager::new();
    manager.discover_with_enterprise(
        Some(workspace.as_path()),
        &[],
        Some(enterprise.to_string_lossy().as_ref()),
    );
    let meta = manager
        .list_all()
        .into_iter()
        .find(|meta| meta.name == "enterprise-skill")
        .expect("企业级目录里的 Skill 应被发现");
    assert_eq!(meta.scope, "enterprise");

    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&enterprise);
}

/// 未给企业级目录时不报错（环境变量没配置是常态）。
#[test]
fn missing_enterprise_directory_is_silent() {
    let workspace = temp_dir("enterprise-none");
    let mut manager = SkillManager::new();
    manager.discover_with_enterprise(Some(workspace.as_path()), &[], None);
    assert!(manager
        .diagnostics()
        .iter()
        .all(|item| !item.message.contains("企业")));
    let _ = std::fs::remove_dir_all(&workspace);
}

/// 同名冲突：先扫描到的作用域胜出，并留下碰撞诊断（与 Python `_add_skill` 同语义）。
#[test]
fn same_name_keeps_first_scope_and_reports_collision() {
    let workspace = temp_dir("collision-ws");
    let extra = temp_dir("collision-extra");
    write_skill(
        &workspace.join(".omnicrawl").join("skills"),
        "demo",
        "name: demo\ndescription: 项目级",
    );
    write_skill(&extra, "demo", "name: demo\ndescription: 额外路径");

    let mut manager = SkillManager::new();
    manager.discover(
        Some(workspace.as_path()),
        &[extra.to_string_lossy().to_string()],
    );
    let meta = manager
        .list_all()
        .into_iter()
        .find(|meta| meta.name == "demo")
        .expect("demo 应被索引");
    assert_eq!(meta.scope, "project", "先扫描到的作用域胜出");
    let collision = manager
        .diagnostics()
        .into_iter()
        .find(|item| item.kind == "collision")
        .expect("应留下碰撞诊断");
    assert!(collision.collision.is_some(), "碰撞记录应带路径信息");

    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&extra);
}

/// 额外路径里的 `.md` 文件（非 SKILL.md）也作为独立 Skill 被发现。
#[test]
fn extra_path_accepts_single_markdown_file() {
    let workspace = temp_dir("extra-file-ws");
    let extra_dir = temp_dir("extra-file-dir");
    let file = extra_dir.join("note-skill.md");
    std::fs::write(
        &file,
        "---\nname: note-skill\ndescription: 单文件技能\n---\n正文\n",
    )
    .expect("写入失败");

    let mut manager = SkillManager::new();
    manager.discover(
        Some(workspace.as_path()),
        &[file.to_string_lossy().to_string()],
    );
    let names: Vec<String> = manager.list_all().into_iter().map(|m| m.name).collect();
    assert!(names.iter().any(|name| name == "note-skill"), "{names:?}");

    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&extra_dir);
}

/// 额外路径不存在时给出诊断，而不是静默忽略。
#[test]
fn missing_extra_path_is_reported() {
    let workspace = temp_dir("missing-path-ws");
    let mut manager = SkillManager::new();
    manager.discover(Some(workspace.as_path()), &["no-such-skill-dir".to_string()]);
    assert!(
        manager
            .diagnostics()
            .iter()
            .any(|item| item.message.contains("Skill 路径不存在")),
        "{:?}",
        manager.diagnostics()
    );
    let _ = std::fs::remove_dir_all(&workspace);
}

/// 折叠块标量（`>-`）：多行折成一行，空行分段。
#[test]
fn frontmatter_folds_block_scalar() {
    let content =
        "---\nname: demo\ndescription: >-\n  第一行\n  第二行\n\n  另一段\n---\n\n正文\n";
    let (frontmatter, body) = parse_frontmatter(content);
    assert_eq!(
        frontmatter.get("description").and_then(Value::as_str),
        Some("第一行 第二行\n另一段")
    );
    assert_eq!(frontmatter.get("name").and_then(Value::as_str), Some("demo"));
    assert_eq!(body, "正文");
}

/// 保留块标量（`|`）：换行逐行保留。
#[test]
fn frontmatter_keeps_literal_block_scalar() {
    let content = "---\nname: demo\ndescription: |\n  第一行\n  第二行\n---\nbody\n";
    let (frontmatter, _) = parse_frontmatter(content);
    assert_eq!(
        frontmatter.get("description").and_then(Value::as_str),
        Some("第一行\n第二行")
    );
}

/// 值里出现 `>` / `|` 但不是块标量时，仍按单行解析。
#[test]
fn frontmatter_plain_values_stay_single_line() {
    let content = "---\nname: demo\ndescription: 用 > 与 | 的普通描述\nflag: true\n---\nbody\n";
    let (frontmatter, _) = parse_frontmatter(content);
    assert_eq!(
        frontmatter.get("description").and_then(Value::as_str),
        Some("用 > 与 | 的普通描述")
    );
    assert_eq!(frontmatter.get("flag").and_then(Value::as_bool), Some(true));
}

/// 块标量后的其他键不会被吞进值里。
#[test]
fn frontmatter_block_scalar_stops_at_next_key() {
    let content =
        "---\nname: demo\ndescription: >\n  一段说明\ndisable-model-invocation: true\n---\nbody\n";
    let (frontmatter, _) = parse_frontmatter(content);
    assert_eq!(
        frontmatter.get("description").and_then(Value::as_str),
        Some("一段说明")
    );
    assert_eq!(
        frontmatter
            .get("disable-model-invocation")
            .and_then(Value::as_bool),
        Some(true)
    );
}

/// 真实存在的写法：`description: >-` 在 SKILL.md 里必须能被解析出可用描述。
#[test]
fn block_scalar_skill_loads_with_real_description() {
    let workspace = temp_dir("block-scalar-ws");
    let skills = workspace.join(".omnicrawl").join("skills");
    let skill_dir = skills.join("design-helper");
    std::fs::create_dir_all(&skill_dir).expect("目录创建失败");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: design-helper\ndescription: >-\n  Create polished design artifacts\n  as HTML deliverables.\n---\n\n正文\n",
    )
    .expect("写入失败");

    let mut manager = SkillManager::new();
    manager.discover(Some(workspace.as_path()), &[]);
    let meta = manager
        .list_all()
        .into_iter()
        .find(|meta| meta.name == "design-helper")
        .expect("应加载 design-helper");
    assert_eq!(
        meta.description,
        "Create polished design artifacts as HTML deliverables."
    );

    let _ = std::fs::remove_dir_all(&workspace);
}
