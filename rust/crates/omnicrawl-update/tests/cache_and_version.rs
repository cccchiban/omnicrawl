//! 版本比较与缓存有效期：两条最容易静默出错、又最不该出错的地方。

use omnicrawl_update::*;
use std::path::PathBuf;

/// 每个用例一个独立目录，避免并行测试互相覆盖缓存文件。
fn temp_cache(tag: &str) -> UpdateCache {
    let dir = std::env::temp_dir().join(format!(
        "omnicrawl-update-test-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");
    UpdateCache::in_dir(&dir)
}

fn status(current: &str, latest: &str) -> UpdateStatus {
    UpdateStatus::new(current, latest)
}

#[test]
fn newer_versions_are_detected() {
    assert!(is_newer("0.4.3", "0.4.2"));
    assert!(is_newer("0.5.0", "0.4.9"));
    assert!(is_newer("1.0.0", "0.99.99"));
    assert!(is_newer("0.4.10", "0.4.9"), "按数字而不是字典序比较");
}

#[test]
fn same_or_older_versions_are_not_updates() {
    assert!(!is_newer("0.4.2", "0.4.2"));
    assert!(!is_newer("0.4.1", "0.4.2"));
    assert!(!is_newer("0.3.9", "0.4.0"));
}

#[test]
fn prerelease_is_older_than_the_same_release() {
    assert!(!is_newer("1.0.0-beta.1", "1.0.0"));
    assert!(is_newer("1.0.0", "1.0.0-beta.1"));
    assert!(is_newer("1.0.0-beta.2", "1.0.0-beta.1"));
}

#[test]
fn version_prefixes_and_padding_are_tolerated() {
    // registry 不会给 `v` 前缀，但缓存文件可能被手改；解析不出的段当 0。
    assert!(is_newer("v0.5.0", "0.4.2"));
    assert!(is_newer("0.5", "0.4.2"));
    assert!(!is_newer("0.4", "0.4.0"));
}

#[test]
fn notice_is_silent_without_an_update() {
    assert_eq!(status("0.4.2", "0.4.2").notice(), None);
    let notice = status("0.4.2", "0.5.0").notice().expect("有更新时应有提示");
    assert!(notice.contains("0.5.0") && notice.contains("0.4.2"), "{notice}");
    assert!(notice.contains("omnicrawl-cli"), "{notice}");
}

#[test]
fn fresh_cache_is_reused_within_the_hour() {
    let cache = temp_cache("fresh");
    let now = now_seconds();
    let written = status(omnicrawl_version::VERSION, "9.9.9");
    cache.write(&written, now);
    let read = cache.read_fresh(now + CACHE_TTL_SECONDS - 1).expect("仍在有效期内");
    assert_eq!(read, written);
}

#[test]
fn cache_expires_after_the_hour() {
    let cache = temp_cache("expired");
    let now = now_seconds();
    cache.write(&status(omnicrawl_version::VERSION, "9.9.9"), now);
    assert_eq!(
        cache.read_fresh(now + CACHE_TTL_SECONDS + 1),
        None,
        "超过一小时必须重新查"
    );
}

#[test]
fn cache_from_another_version_is_ignored() {
    // 升级后旧缓存会把「已是最新」继续报一小时，必须按版本作废。
    let cache = temp_cache("stale-version");
    let now = now_seconds();
    cache.write(&status("0.0.1-old", "9.9.9"), now);
    assert_eq!(
        cache.read_fresh(now),
        None,
        "当前版本与缓存记录不一致时不得复用"
    );
}

#[test]
fn corrupt_cache_reads_as_absent() {
    let cache = temp_cache("corrupt");
    std::fs::write(cache.path(), "{ 这不是 JSON").expect("写损坏缓存");
    assert_eq!(cache.read_fresh(now_seconds()), None);
    // 缺失文件同理。
    let missing = UpdateCache::at_file(PathBuf::from("不存在的目录/version_check.json"));
    assert_eq!(missing.read_fresh(now_seconds()), None);
}

#[test]
fn future_timestamp_is_treated_as_expired() {
    // 系统时钟回拨会让 checked_at 落在未来：age 为负，不能当成「刚查过」。
    let cache = temp_cache("future");
    let now = now_seconds();
    cache.write(&status(omnicrawl_version::VERSION, "9.9.9"), now + 3600);
    assert_eq!(cache.read_fresh(now), None);
}
