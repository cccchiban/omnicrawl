//! 真实文件系统链路的集成测试：Skill 目录扫描与注册表的原子写往返。
//!
//! 这两条链路此前只有判定层（解析、合并、打分）的单测覆盖，`discover` 真正扫目录、
//! 注册表真正落盘再读回来这两步没有被验证过——本轮把它们补上。
//!
//! 用例只碰临时目录与用户级 skills 目录的 `ensure_scope_dir`（与 Python 同行为：
//! 个人级目录在 discover 时自动创建），不写任何项目文件。

use std::path::PathBuf;

use omnicrawl_extensions::models::{PluginRecord, PluginRegistryDocument};
use omnicrawl_extensions::registry::{
    load_registry_document, save_registry_document, upsert_plugin_record,
};
use omnicrawl_extensions::skill::SkillManager;
use serde_json::Value;

fn temp_dir(tag: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let root =
        std::env::temp_dir().join(format!("omnicrawl-ext-{tag}-{}-{id}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("临时目录创建失败");
    root
}

fn skill_names(manager: &SkillManager) -> Vec<String> {
    manager
        .list_all()
        .iter()
        .map(|meta| meta.name.clone())
        .collect()
}

#[test]
fn discover_scans_project_skill_directory() {
    let workspace = temp_dir("skill-project");
    let skill_dir = workspace
        .join(".omnicrawl")
        .join("skills")
        .join("demo-skill");
    std::fs::create_dir_all(&skill_dir).expect("skill 目录创建失败");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: demo-skill\ndescription: 演示技能\n---\n\n正文内容。\n",
    )
    .expect("SKILL.md 写入失败");

    let mut manager = SkillManager::new();
    manager.discover(Some(workspace.as_path()), &[]);
    let names = skill_names(&manager);
    assert!(
        names.iter().any(|name| name == "demo-skill"),
        "项目级 skills 目录应被扫描到：{names:?}"
    );
    let all = manager.list_all();
    let meta = all
        .iter()
        .find(|meta| meta.name == "demo-skill")
        .expect("应有 demo-skill");
    let dict = meta.to_dict();
    assert_eq!(
        dict.get("description").and_then(Value::as_str),
        Some("演示技能")
    );
    assert_eq!(dict.get("scope").and_then(Value::as_str), Some("project"));

    let _ = std::fs::remove_dir_all(&workspace);
}

#[test]
fn discover_accepts_extra_paths_and_reports_missing_ones() {
    let workspace = temp_dir("skill-extra");
    let extra = temp_dir("skill-extra-target");
    let skill_dir = extra.join("extra-skill");
    std::fs::create_dir_all(&skill_dir).expect("skill 目录创建失败");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: extra-skill\ndescription: 额外路径技能\n---\n\n正文。\n",
    )
    .expect("SKILL.md 写入失败");
    let missing = workspace.join("不存在").to_string_lossy().to_string();

    let mut manager = SkillManager::new();
    manager.discover(
        Some(workspace.as_path()),
        &[extra.to_string_lossy().to_string(), missing],
    );
    let names = skill_names(&manager);
    assert!(
        names.iter().any(|name| name == "extra-skill"),
        "额外路径应被扫描到：{names:?}"
    );
    assert!(
        manager
            .diagnostics()
            .iter()
            .any(|diagnostic| diagnostic.message.contains("Skill 路径不存在")),
        "不存在的额外路径应留下诊断：{:?}",
        manager.diagnostics()
    );

    let _ = std::fs::remove_dir_all(&workspace);
    let _ = std::fs::remove_dir_all(&extra);
}

#[test]
fn registry_round_trips_through_atomic_write() {
    let root = temp_dir("registry");
    let path = root.join("plugins.json");
    let mut document = PluginRegistryDocument::default();
    upsert_plugin_record(
        &mut document,
        PluginRecord {
            name: "demo-plugin".to_string(),
            enabled: true,
            dev_mode: true,
            local_path: root.to_string_lossy().to_string(),
            approved_permissions: vec!["hook:turn.end".to_string()],
            ..PluginRecord::default()
        },
    );
    save_registry_document(&path, &document).expect("注册表写盘失败");
    assert!(path.is_file(), "原子写之后应有注册表文件");

    let loaded = load_registry_document(&path).expect("注册表读盘失败");
    let record = loaded.get("demo-plugin").expect("应读回插件记录");
    assert!(record.enabled);
    assert!(record.dev_mode);
    assert_eq!(
        record.approved_permissions,
        vec!["hook:turn.end".to_string()]
    );

    // 缺文件时按空文档处理（不是错误）。
    let empty =
        load_registry_document(&root.join("nothing-here.json")).expect("缺文件应回落空文档");
    assert!(empty.plugins.is_empty());

    // 损坏 JSON 必须报错且不覆盖原文件（降级保护的入口）。
    std::fs::write(&path, "{ 不是 JSON").expect("损坏文件写入失败");
    let broken = load_registry_document(&path);
    assert!(broken.is_err(), "损坏的注册表必须报错");
    assert_eq!(
        std::fs::read_to_string(&path).expect("读回损坏文件失败"),
        "{ 不是 JSON",
        "报错路径不得改写原文件"
    );

    let _ = std::fs::remove_dir_all(&root);
}
