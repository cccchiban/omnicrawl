# omnicrawl-session

会话存储的 Rust 内核：会话事件与索引条目的模型、命名与时间校验。语义基准是
Python 侧 `omnicrawl/state/session_models.py`。

两个实现读写同一批会话文件（`.agent_sessions/index.json` 与 `sessions/<id>.jsonl`），
所以这一层是纯粹的数据契约：字段名、校验规则、错误文案、JSONL 行的字节布局都必须一致，
否则「Python 写的会话内核读不懂」——用户切一次实现就等于丢一次历史。

## 本片范围

- `SessionEvent`：会话 JSONL 里的一条版本化事件（`version`/`session_id`/`event_id`/`parent_id`/
  `type`/`created_at`/`payload`）；`create` 生成 id 与时间戳，`from_dict`/`to_dict` 走 JSON 边界，
  `to_json_line` 输出转录里的一行。
- `SessionIndexEntry`：`index.json` 里的会话目录条目，含 `archived_at` 与计数校验。
- `naming`：会话 id、事件类型、转录相对路径的校验与规范化，标题折叠，
  以及事件类型分组常量（消息事件 / 参与模型上下文的事件 / 空会话事件 / 子任务事件）。
- `time`：时间戳解析与格式化，对齐 Python `datetime` 的 ISO-8601 语义（UTC、微秒）。
- `SessionStoreError`：错误文案与 Python `SessionStoreError` 逐字一致。

## 边界要求

- 只做纯逻辑，不碰文件系统：磁盘读写（追加转录、维护索引、锁、归档、导出、一致性诊断）在后续切片。
- 校验发生在系统边界（读文件、读协议载荷），内部调用走类型化入口，不重复做 JSON 类型判断。
- `to_json_line` 不含换行符；分隔符紧凑、不转义非 ASCII，与 Python 的
  `json.dumps(..., ensure_ascii=False, separators=(",", ":"))` 同形。

## parity 工作流

```bash
python rust/tools/gen_session_fixture.py      # 期望值来自 session_models.py 真实现
cd rust && cargo test -p omnicrawl-session
```

fixture `tests/fixtures/session_models_parity.json` 共 107 个用例：会话 id 9、事件类型 13、
相对路径 12、标题 7、时间戳 17、事件校验 15、索引条目 17、载荷计数 9、事件创建 5、转录行 3。
转录行一组是字节级比对：一侧多一个空格或换一个键序就会被抓到。

## 已知差异

- **非对象输入**：给事件/索引传一个 JSON 数组时，Python 会抛 `TypeError`（`data["version"]` 直接炸），
  内核统一收敛成 `SessionStoreError`，文案按「缺少字段：version。」给出。
- **超出 `u64` 的整数**：载荷计数在 Python 里是任意精度整数，内核按 `u64` 处理，溢出的值按无效算 0。
- **`is_relative_to` 尚未搬**：这一层没有出现路径包含判断，等存储读写切片需要时再补（注意 Windows 上
  Python 的路径比较是大小写不敏感的）。

## 依赖

`chrono`（时间戳解析/格式化）、`sha2`（会话 id 与事件 id 的随机后缀散列）、`serde`/`serde_json`。
`serde_json` 在 workspace 里开了 `preserve_order`：JSON 对象保留插入序，转录行才能与 Python 字节一致。
