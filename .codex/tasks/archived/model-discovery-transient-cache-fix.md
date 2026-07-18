# 模型列表瞬时失败缓存修复

状态：已完成

## 现象

`migrated-openai` 的模型列表发现曾显示“无法连接模型服务”，服务恢复后仍可能在发现缓存 TTL 内持续显示旧错误。

## 诊断

- `https://axo.chibanban.de/v1/models` 域名、TLS 和 HTTP 可达，未鉴权请求正确返回 401。
- 使用当前项目配置和鉴权执行真实发现成功，返回 52 个模型，diagnostics 为空。
- 根因是 `_discover_for_profile` 同时缓存成功和失败结果，瞬时网络失败会保留 300 秒。

## 修复

- `omnicrawl/config/model_catalog.py` 只缓存 `status=ok` 的成功结果；失败结果从缓存移除，下一次打开目录自动重试。
- `omnicrawl/ui/fullscreen/model_picker.py` 将固定“只渲染前 40 项”改为跟随选中项移动的可视窗口，并在目录加载后定位当前模型；长列表中的全部模型均可通过方向键访问并显示。目录刷新时会在过滤后的列表中定位当前模型，避免搜索条件下选中另一条匹配项。
- `tests/test_model_catalog.py` 新增失败后自动重试回归，修复前第二次仍返回 `unavailable`，修复后返回 `ok`。
- `tests/test_fullscreen_tui.py` 新增长列表尾部模型可见和打开时定位当前模型回归。
- `docs/MULTI_MODEL_API_DESIGN.md` 同步缓存与长列表显示语义。

## 验证

- 真实 TUI 探测：状态为“自定义 1 · 检测 52”，0 条 diagnostics；当前自定义模型可见，长列表最后一个模型可在导航后显示。
- 定向回归：57 项通过。
- 全量测试：609 项通过。
- 审查后全量回归：617 项通过。
- `compileall` 与 `git diff --check` 通过。
