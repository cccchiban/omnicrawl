//! 会话根锁注册表：同一个根共享一把锁，实例释放后注册表不再钉住它。

use std::path::PathBuf;
use std::sync::Arc;

use omnicrawl_session::process_lock_for_root;

fn temp_root(tag: &str) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("时钟可用")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("omnicrawl-lock-registry-{tag}-{unique}"));
    std::fs::create_dir_all(&path).expect("建临时目录");
    path
}

#[test]
fn same_root_shares_one_lock_and_releases_it() {
    let root = temp_root("shared");

    let first = process_lock_for_root(&root);
    let second = process_lock_for_root(&root);
    assert!(Arc::ptr_eq(&first, &second), "同一会话根必须复用同一把锁");

    let released = Arc::downgrade(&first);
    drop(second);
    drop(first);
    assert!(released.upgrade().is_none(), "没有持有者时锁应被回收");

    let rebuilt = process_lock_for_root(&root);
    assert!(
        released.upgrade().is_none(),
        "注册表不得继续持有已释放的实例"
    );
    assert_eq!(Arc::strong_count(&rebuilt), 1, "重建的实例只被调用方持有");

    let other = process_lock_for_root(&temp_root("other"));
    assert!(!Arc::ptr_eq(&rebuilt, &other), "不同会话根必须独立持锁");

    std::fs::remove_dir_all(&root).ok();
}
