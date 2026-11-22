use std::sync::{mpsc, Arc};
use std::time::Duration;
use omnicrawl_controllers::subagents::coordinator::validate_task_arguments;
use omnicrawl_controllers::subagents::definitions::AgentDefinitionRegistry;
use omnicrawl_controllers::subagents::tasks::{SubAgentTaskManager, SubAgentTaskSpec, TaskRunner};
use omnicrawl_config::features::subagents::SubAgentConfig;
use serde_json::json;

#[test]
fn list_keeps_running_completed_failed_and_cancelled_tasks() {
    let manager = SubAgentTaskManager::new(60.0, 1);
    let (started, running) = mpsc::channel();
    let worker: TaskRunner = Arc::new(move |spec, cancel| {
        started.send(spec.task_id.clone()).unwrap();
        if spec.task_id == "task-running" {
            while !cancel.is_set() { std::thread::sleep(Duration::from_millis(1)); }
        }
        json!({"status": "completed", "summary": "done"}).as_object().unwrap().clone()
    });
    manager.spawn("owner", "session", &[SubAgentTaskSpec::new("task-done", "completed", "review", "batch-done")], worker.clone(), None).unwrap();
    running.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(manager.wait_for_idle("owner", Some("session"), 2.0));
    let failed: TaskRunner = Arc::new(|_, _| json!({"status": "failed", "error": {"code": "TEST", "message": "failed"}}).as_object().unwrap().clone());
    manager.spawn("owner", "session", &[SubAgentTaskSpec::new("task-failed", "failed", "review", "batch-failed")], failed, None).unwrap();
    assert!(manager.wait_for_idle("owner", Some("session"), 2.0));
    manager.spawn("owner", "session", &[SubAgentTaskSpec::new("task-running", "running", "review", "batch-running")], worker.clone(), None).unwrap();
    assert_eq!(running.recv_timeout(Duration::from_secs(2)).unwrap(), "task-running");
    manager.spawn("owner", "session", &[SubAgentTaskSpec::new("task-queued", "queued", "review", "batch-queued")], worker, None).unwrap();
    let tasks = manager.list("owner", Some("session"));
    assert_eq!(tasks.len(), 4);
    for (id, status) in [("task-done", "completed"), ("task-failed", "failed"), ("task-running", "running"), ("task-queued", "queued")] {
        assert_eq!(tasks.iter().find(|task| task["task_id"] == id).unwrap()["status"], status);
    }
    assert!(manager.list("other", Some("session")).is_empty());
    assert!(manager.list("owner", Some("other")).is_empty());
    assert_eq!(manager.cancel("owner", Some("session"), Some("task-running"), Some("wrong"))["ok"], false);
    manager.cancel("owner", Some("session"), Some("task-queued"), None);
    assert_eq!(manager.get("task-queued", "owner", Some("session")).unwrap()["status"], "cancelled");
    manager.cancel("owner", Some("session"), Some("task-running"), None);
    assert!(manager.wait_for_idle("owner", Some("session"), 2.0));
    assert_eq!(manager.list("owner", Some("session")).len(), 4);
    manager.drain_notifications("owner", Some("session"));
    assert!(manager.drain_notifications("owner", Some("session")).is_empty());
    assert_eq!(manager.list("owner", Some("session")).len(), 4);
    manager.close("owner", None);
}

#[test]
fn background_arguments_have_types_and_respect_the_permission_gate() {
    let registry = AgentDefinitionRegistry::with_located_builtin(None);
    let config = SubAgentConfig { allow_background: false, ..SubAgentConfig::default() };
    let invalid = validate_task_arguments(&json!({"background": "true", "tasks": []}), &config, &registry).unwrap();
    assert_eq!(invalid.0, "AGENT_DEFINITION_INVALID");
    let denied = validate_task_arguments(&json!({"background": true, "tasks": []}), &config, &registry).unwrap();
    assert_eq!(denied.0, "SUBAGENT_PERMISSION_DENIED");
}

#[test]
fn tasks_array_is_validated_as_a_whole_and_per_entry() {
    let mut registry = AgentDefinitionRegistry::with_located_builtin(None);
    // 校验逐项进行：前一任务必须先通过，才轮得到后一项报错，所以要先让内置定义可用。
    registry.discover(&std::env::temp_dir());
    let config = SubAgentConfig::default();
    let task = json!({"description": "d", "prompt": "p", "subagent_type": "review"});
    // 缺 tasks / 非数组 / 空数组：都按整批拒绝。
    assert_eq!(
        validate_task_arguments(&json!({}), &config, &registry).unwrap().1,
        "subagent_run 需要 tasks。"
    );
    assert_eq!(
        validate_task_arguments(&json!({"tasks": {}}), &config, &registry).unwrap().1,
        "tasks 必须是数组。"
    );
    assert_eq!(
        validate_task_arguments(&json!({"tasks": []}), &config, &registry).unwrap().1,
        "tasks 至少要有一个任务。"
    );
    // 超过上限。
    let too_many = json!({"tasks": vec![task.clone(); 9]});
    assert_eq!(
        validate_task_arguments(&too_many, &config, &registry).unwrap().1,
        "一次最多派发 8 个任务，收到 9 个。"
    );
    // 多个任务时错误要指到具体那一项。首项必须自己能完全通过校验，
    // 否则它会先报错、掩盖第二项的问题。
    let first_ok = json!({"description": "d", "prompt": "p", "subagent_type": "review"});
    let second_broken = json!({"tasks": [first_ok.clone(), {"description": "d", "subagent_type": "review"}]});
    assert_eq!(
        validate_task_arguments(&second_broken, &config, &registry).unwrap().1,
        "tasks[1].prompt 必须是非空字符串。"
    );
    // 单任务的报错仍然用 `task.` 前缀（`/review` 这类入口的文案与此同源）。
    let single_broken = json!({"tasks": [{"description": "d", "subagent_type": "review"}]});
    assert_eq!(
        validate_task_arguments(&single_broken, &config, &registry).unwrap().1,
        "task.prompt 必须是非空字符串。"
    );
    assert!(validate_task_arguments(&json!({"tasks": [first_ok]}), &config, &registry).is_none());
}
