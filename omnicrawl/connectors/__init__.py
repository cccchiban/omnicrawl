"""消息平台连接器包：用于接入 Telegram 与微信等外部消息服务。

已实现：
- telegram.py：Telegram Bot 远程接入（长轮询、零新增依赖），
  让用户通过 Telegram 消息远程操作 OmniCrawl Agent：执行任务、工具审批、
  状态/会话管理（/sessions /resume /undo /compact 等）、子系统查询
  （/mcp /plugins /skills /tasks）、推理强度与审批模式查看；
  最终回答打字机流式输出，思考内容可 /thinking on 开关（默认关），
  思考/工具/回答各自独立消息；TUI 的推理强度与工作区切换通过
  config.toml 持久化，本模块任务前重读实现跨进程同步；
  支持接收图片/文档/视频/语音等文件，自动分类存入 .agent_tmp 子目录后
  交给 Agent 处理。运行方式：``python -m omnicrawl.connectors.telegram``。

- fsapp.py：飞书自建应用 WebSocket 长连接接入
- autostart.py：TUI 启动时按配置自动管理 Telegram/飞书子进程

规划中：
- wechat.py：微信接入（个人号/公众号待定）

使用方式：``from omnicrawl.connectors import telegram``。
"""
