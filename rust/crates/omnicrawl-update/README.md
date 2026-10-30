# omnicrawl-update

新版本检测：查询 npm registry 上 `omnicrawl-cli` 的最新版本，与本地产品版本比较，
**结果缓存一小时**。

## 为什么单独一个 crate

产品版本（`omnicrawl-version`）被 llm / tts / api / host / cli / tui 等 7 个 crate 直接
依赖，它本身零依赖、只做常量注入。把联网能力放进去，`ureq` 会顺着依赖树传导到每一个
下游，而真正需要「查更新」的只有前端。因此拆成独立 crate：`ureq` 只进入本 crate 与 `tui`。

## 三个约束

1. **不拖慢启动**：`UpdateCheck::spawn` 立刻返回，检测在后台线程里跑，界面先正常渲染，
   结果回来再补一条提示。网络最坏情况是一条 10s 超时，但它不在启动路径上。
2. **一小时内不重复请求**：结果按 `~/.OmniCrawl/version_check.json` 落盘缓存，跨进程有效。
   进程内另有 `App::update_notified` 闩锁，保证同一次运行最多提示一次。
3. **失败一律静默**：网络不可达、registry 改版、缓存损坏都收敛成 `Err`，界面对 `Err`
   不做任何显示——检测是附加信息，不该变成启动噪音。

## 缓存的三个作废条件

| 条件 | 原因 |
| --- | --- |
| `now - checked_at > 3600` | 超过一小时，重新联网 |
| `checked_at > now` | 系统时钟回拨会让时间戳落在未来；若不显式拒绝，`saturating_sub` 会把差值饱和成 0，缓存就**永久有效**了 |
| `status.current != 当前版本` | 升级后旧缓存会把「已是最新」继续报一小时假结论 |

缓存文件损坏、字段缺失、文件不存在一律当作「没有缓存」。

## 版本比较

只处理 `MAJOR.MINOR.PATCH` 与可选预发布后缀：

- 按**数字**比较，不是字典序（`0.4.10 > 0.4.9`）
- 解析不出的段当 0（容错，不报错）
- 容错 `v` 前缀与缺段（`0.5` == `0.5.0`）
- 同主版本号下，无后缀的比有后缀的新（`1.0.0 > 1.0.0-beta.1`）

## 接口

```rust
let cache = UpdateCache::in_dir(&user_config_dir(&env));
// 同步（后台线程用）
let status = check_with_cache(&cache)?;       // 命中缓存则不联网
let notice = status.notice();                  // 没更新时返回 None

// 异步（界面用）
let mut check = UpdateCheck::spawn(cache);
if let Some(result) = check.poll() { /* 未就绪时为 None */ }
```

## 测试

`tests/cache_and_version.rs` 覆盖 13 组版本比较 + 8 组缓存语义（有效期内、正好 TTL、
超时、跨版本、损坏、缺失、未来时间戳）。每个用例用独立临时目录，避免并行互相覆盖。
