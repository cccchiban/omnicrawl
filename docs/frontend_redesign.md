# PyQtWebEngine 前端重构设计文档

> 版本：v1.1 | 日期：2026-06-15 | 状态：已实现

---

## 0. 视觉风格变更（v1.1）

**从 Persona 5 游戏风格 → OpenAI ChatGPT 专业风格**

| 维度 | Persona 5（旧） | ChatGPT（新） |
|------|----------------|---------------|
| 主色调 | 深红 #e63946 + 电蓝 #00d4ff | 中性灰 #343541 / #444654 |
| 气泡 | 斜切角 clip-path + 彩色边框 | 无气泡边框，全宽行背景色区分 |
| 动画 | 脉冲发光、扫描线纹理 | 极简淡入，无过度动效 |
| 头像 | 渐变色 + clip-path 切角 | 纯色方块，克制 |
| 输入框 | 红色发光 focus | 白色半透明边框，微妙聚焦 |
| 整体感受 | 游戏化、炫酷 | 专业、克制、工具感 |

---

## 1. 现状分析

### 1.1 当前架构

```
┌──────────────────────────────────────────────────────┐
│ main.py (入口)                                        │
│   ├── load_frontend_config() → FrontendConfig        │
│   ├── create_ui(frontend_type) → QtUI | TerminalUI   │
│   └── run_qt_chat() / run_inline_chat()              │
├──────────────────────────────────────────────────────┤
│ Python 层                                            │
│   ui/qt/qt_ui.py      — QtUI(BaseUI) 实现            │
│   ui/qt/_bridge.py    — BackendBridge(QObject) 跨线程 │
│   ui/qt/window.py     — ChatWindow(QWidget) 容器     │
├──────────────────────────────────────────────────────┤
│ Web 层（QWebEngineView 加载）                         │
│   web/index.html      — 单页布局                      │
│   web/style.css       — Persona 5 主题 (777行)        │
│   web/bridge.js       — QWebChannel 通信桥 (67行)    │
│   web/app.js          — 全部前端逻辑 (420行)          │
└──────────────────────────────────────────────────────┘
```

### 1.2 通信链路

```
后台线程                              Qt 主线程
┌──────────┐    call_js()    ┌──────────────┐    runJavaScript()    ┌──────────┐
│ QtUI     │ ────────────────→│ BackendBridge │ ────────────────────→│ pyCallbacks │
│ (BaseUI) │                  │ (QObject)    │                      │ (window)    │
└──────────┘                  └──────────────┘                      └──────────┘
                                    ↑ QWebChannel                        │
                                    │   bridge.onUserSend()              │
                                    │   bridge.onConfirmResult()         │
                              ┌─────┴─────┐                              │
                              │  app.js   │ ←─────────────────────────────
                              └───────────┘     用户交互事件
```

### 1.3 核心问题清单

| # | 问题 | 严重度 | 影响 |
|---|------|--------|------|
| 1 | **app.js 单体 420 行** — 渲染/交互/状态混在一起 | 高 | 维护困难，加功能容易引入 Bug |
| 2 | **手写 Markdown 解析器** — 正则方式脆弱，不支持嵌套、转义 | 高 | 表格/嵌套列表渲染错误，无法扩展 |
| 3 | **无组件化** — DOM 操作全用字符串拼接 + innerHTML | 高 | 潜在的 XSS 风险，DOM 更新不精确 |
| 4 | **CSS 单体 777 行** — 主题与布局耦合 | 中 | 改主题或布局需要翻阅整个文件 |
| 5 | **全局变量管理状态** — currentAIBubble/currentToolCard 等 | 中 | 并发场景下状态可能错乱 |
| 6 | **无无障碍支持** — 缺少 ARIA 标签和键盘导航 | 低 | 对屏幕阅读器不友好 |
| 7 | **工具结果展示简陋** — 纯文本截断，无代码高亮 | 中 | 长 JSON/代码结果不可读 |
| 8 | **无流式中断 UI** — 用户无法取消正在生成的回复 | 中 | 长回复时只能等待 |
| 9 | **硬编码中文字符串** — 无 i18n 机制 | 低 | 后续无法国际化 |
| 10 | **错误处理缺失** — JS 侧无错误边界 | 中 | 一个渲染异常可能让后续消息不显示 |

---

## 2. 设计目标

### 2.1 核心目标

1. **模块化拆分** — app.js 拆为多个职责单一的模块，每个 ≤ 200 行
2. **Markdown 渲染升级** — 引入轻量库（marked.js ~35KB），支持完整 GFM
3. **CSS 分层组织** — 按 component / theme / layout 拆分
4. **状态集中管理** — 引入简单的状态对象，消除全局变量
5. **API 契约不变** — Python 端 `BaseUI` 接口和 `BackendBridge` 不做破坏性变更

### 2.2 非目标（本次不做）

- 不引入构建工具（Webpack/Vite）— 保持零依赖加载
- 不引入前端框架（Vue/React）— 保持原生 JS，减少复杂度
- 不改变 QWebChannel 通信协议
- 不改变 Python 端 QtUI/window/bridge 的接口

---

## 3. 新前端架构

### 3.1 文件结构

```
ai_voice_agent/ui/qt/web/
├── index.html              # 入口（精简，只保留结构骨架）
├── css/
│   ├── variables.css       # CSS 自定义属性（颜色/字体/动画/间距）
│   ├── reset.css           # Reset + 基础排版
│   ├── layout.css          # 整体布局（#app flex 布局）
│   ├── title-bar.css       # 标题栏
│   ├── chat-area.css       # 聊天区域 + 滚动条
│   ├── messages.css        # 消息气泡（用户/AI/工具）
│   ├── input-area.css      # 输入框 + 发送按钮
│   ├── status-bar.css      # 状态栏 + 打字指示器
│   ├── dialog.css          # 确认对话框 + 通知条
│   └── animations.css      # 关键帧动画集合
├── js/
│   ├── bridge.js           # QWebChannel 通信桥（不变）
│   ├── state.js            # 集中状态管理（新增）
│   ├── markdown.js         # Markdown 渲染封装（新增）
│   ├── messages.js         # 消息/气泡管理（拆分自 app.js）
│   ├── tools.js            # 工具调用卡片（拆分自 app.js）
│   ├── input.js            # 输入处理（拆分自 app.js）
│   ├── dialog.js           # 确认对话框 + 通知（拆分自 app.js）
│   ├── py-callbacks.js     # pyCallbacks 注册入口（新增）
│   └── app.js              # 主入口（精简为初始化编排）
└── vendor/
    └── marked.min.js       # 轻量 Markdown 解析器 (~35KB)
```

### 3.2 模块职责

```
                         ┌─────────────┐
                         │   app.js    │  ← 入口：初始化编排，加载 DOM 引用
                         └──┬───┬───┬──┘
                  ┌─────────┘   │   └─────────┐
          ┌───────▼──────┐ ┌───▼──────┐ ┌─────▼──────┐
          │  messages.js  │ │ tools.js │ │  input.js  │
          │ 用户/AI气泡    │ │ 工具卡片  │ │ 输入处理    │
          └───────┬──────┘ └──────────┘ └────────────┘
                  │
          ┌───────▼──────┐
          │ markdown.js  │  ← 封装 marked.js，处理流式增量渲染
          └──────────────┘
          ┌──────────────┐
          │  state.js    │  ← 全局状态（currentAIBubble, toolCard 等）
          └──────────────┘
          ┌──────────────┐
          │  dialog.js   │  ← 确认对话框 + 通知栏
          └──────────────┘
          ┌──────────────┐
          │py-callbacks.js│ ← 注册所有 pyCallbacks，Python 调用的入口
          └──────────────┘
```

### 3.3 加载顺序

HTML 中 `<script>` 加载顺序：

```
1. vendor/marked.min.js     ← 外部依赖最先加载
2. bridge.js                ← QWebChannel 通信桥
3. state.js                 ← 状态管理（被其他模块依赖）
4. markdown.js              ← Markdown 渲染（被 messages.js 依赖）
5. messages.js              ← 消息气泡管理
6. tools.js                 ← 工具卡片管理
7. input.js                 ← 输入处理
8. dialog.js                ← 对话框 + 通知
9. py-callbacks.js          ← 汇总注册所有 pyCallbacks
10. app.js                  ← 入口初始化
```

---

## 4. 模块详细设计

### 4.1 state.js — 状态管理

```js
/**
 * state.js — 集中状态管理
 *
 * 所有模块共享的状态统一存于此对象，每处状态变更都有明确注释。
 * 不使用 Proxy/Object.defineProperty，保持简单直接。
 */

const AppState = {
  /** @type {HTMLElement|null} 当前流式输出的 AI 气泡 DOM */
  currentAIBubble: null,

  /** @type {string} 当前 AI 气泡累积的原始 Markdown 文本 */
  currentRawText: '',

  /** @type {HTMLElement|null} 当前工具调用卡片 DOM */
  currentToolCard: null,

  /** @type {number} 工具步骤后备计数器 */
  fallbackToolStep: 1,

  /** @type {string|null} 当前确认对话框 ID */
  confirmId: null,

  /** @type {number|null} 通知自动消失定时器 ID */
  noticeTimer: null,

  /** @type {boolean} 窗口是否已关闭 */
  closed: false,

  // ── 重置方法 ──────────────────────────────────

  /** 重置当前 AI 消息状态 */
  resetAI() {
    this.currentAIBubble = null;
    this.currentRawText = '';
  },

  /** 重置当前工具卡片状态 */
  resetTool() {
    this.currentToolCard = null;
  },
};
```

### 4.2 markdown.js — Markdown 渲染

```js
/**
 * markdown.js — Markdown 渲染封装
 *
 * 封装 marked.js 提供：
 *   1. render(text)      — 完整 Markdown → HTML
 *   2. renderStreaming(text) — 流式场景的轻量渲染（纯文本→HTML最终化）
 *   3. 代码高亮使用 marked 自带的 highlight 回调
 */

const Markdown = {
  /**
   * 渲染完整 Markdown 文本为 HTML。
   * AI 消息完成时调用，替换流式纯文本为格式化内容。
   */
  render(text) {
    if (!text) return '';
    return marked.parse(text, {
      breaks: true,        // GFM 换行
      gfm: true,           // 完整 GFM 支持
      headerIds: false,    // 不生成标题 id
      mangle: false,
    });
  },

  /**
   * 流式输出过程中的文本转义显示。
   * 不做 Markdown 解析，只做 HTML 转义 + 换行。
   * AI 回复完成后用 render() 替换为格式化版本。
   */
  streamText(text) {
    return escHtml(text).replace(/\n/g, '<br>');
  },
};

/** HTML 实体转义 */
function escHtml(s) {
  return s.replace(/&/g, '&amp;')
          .replace(/</g, '&lt;')
          .replace(/>/g, '&gt;');
}
```

**关于 marked.js 的选择理由：**

| 方案 | 大小 | GFM 支持 | 流式支持 | 安全 |
|------|------|---------|---------|------|
| **marked.js** | ~35KB | 完整 | 需自行处理 | 内置 sanitize |
| markdown-it | ~100KB | 完整 | 需自行处理 | 插件 |
| showdown | ~40KB | 完整 | 需自行处理 | 内置 |
| 手写解析器(当前) | 0 | 不完整 | 自行实现 | 无保证 |

选择 marked.js：最小体积、完整 GFM 支持、零依赖。

### 4.3 messages.js — 消息气泡

```js
/**
 * messages.js — 消息气泡管理
 *
 * 职责：
 *   - appendUserMsg(text)  — 创建用户消息气泡
 *   - appendAIText(text)   — 追加 AI 流式文本（纯文本显示）
 *   - finishAIMsg()        — 完成 AI 消息（Markdown 渲染）
 *   - scrollToEnd()        — 滚动到底部
 *   - timeNow()            — 时间格式化
 */

const Messages = {
  /**
   * 创建用户消息气泡。
   * 用户消息直接显示转义文本，不做 Markdown 渲染。
   */
  appendUserMsg(text) {
    AppState.resetAI();
    const row = createElement('div', { class: 'msg-row user' });
    row.innerHTML = `
      <div class="avatar user">U</div>
      <div class="msg-body">
        <div class="msg-header">
          <span class="msg-role">你</span>
          <span class="msg-time">${timeNow()}</span>
        </div>
        <div class="bubble">${escHtml(text)}</div>
      </div>
    `;
    getMessagesEl().appendChild(row);
    scrollToEnd();
  },

  /**
   * 追加 AI 流式文本。
   * 首次调用创建气泡，后续调用追加文本。
   * 流式过程中显示纯文本（不解析 Markdown）。
   */
  appendAIText(text) {
    if (!AppState.currentAIBubble) {
      const row = createElement('div', { class: 'msg-row ai' });
      row.innerHTML = `
        <div class="avatar ai">A</div>
        <div class="msg-body">
          <div class="msg-header">
            <span class="msg-role">AI 助手</span>
            <span class="msg-time">${timeNow()}</span>
          </div>
          <div class="bubble"></div>
        </div>
      `;
      getMessagesEl().appendChild(row);
      AppState.currentAIBubble = row.querySelector('.bubble');
      AppState.currentRawText = '';
    }
    AppState.currentRawText += text;
    AppState.currentAIBubble.textContent = AppState.currentRawText;
    scrollToEnd();
  },

  /**
   * 完成 AI 消息 — 用 marked.js 渲染为格式化 HTML。
   */
  finishAIMsg() {
    if (!AppState.currentAIBubble) return;
    const html = Markdown.render(AppState.currentRawText);
    AppState.currentAIBubble.innerHTML = html;
    AppState.resetAI();
  },

  /**
   * 显示启动面板卡片。
   */
  showStartup(title, lines) {
    // 复用现有逻辑，封装到独立函数
    // ... 与当前 app.js 中的 showStartup 相同，但使用 AppState
  },
};

/** 创建元素并设置属性 */
function createElement(tag, attrs = {}) {
  const el = document.createElement(tag);
  Object.assign(el, attrs);
  return el;
}

function getMessagesEl() {
  return document.getElementById('messages');
}
```

### 4.4 tools.js — 工具调用卡片

```js
/**
 * tools.js — 工具调用卡片管理
 *
 * 职责：
 *   - showToolStart(step, toolName, argsJson)  — 创建工具调用卡片
 *   - showToolResult(ok, output, toolName)     — 显示工具执行结果
 */

const Tools = {
  showToolStart(step, toolName, argsJson) {
    // 先完成当前 AI 消息
    if (AppState.currentAIBubble) Messages.finishAIMsg();

    // 格式化 JSON 参数
    let formatted = '';
    try {
      formatted = JSON.stringify(JSON.parse(argsJson), null, 2);
    } catch {
      formatted = argsJson.substring(0, 800);
    }

    const row = createElement('div', { class: 'tool-row' });
    row.dataset.step = String(step);
    row.innerHTML = `
      <div class="tool-avatar">&#x2699;</div>
      <div class="tool-body">
        <div class="tool-card">
          <div class="tool-header">
            <span class="step-badge">${step}</span>
            <span class="tool-name">${escHtml(toolName)}</span>
            <button class="tool-expand" title="展开/折叠参数" aria-label="展开参数">&#x25BC;</button>
            <span class="tool-spinner" aria-label="执行中"></span>
          </div>
          <div class="tool-args">${escHtml(formatted.substring(0, 1500))}</div>
          <div class="tool-result-slot"></div>
        </div>
      </div>
    `;

    // 折叠按钮事件
    const expandBtn = row.querySelector('.tool-expand');
    const argsEl = row.querySelector('.tool-args');
    let visible = true;
    expandBtn.addEventListener('click', () => {
      visible = !visible;
      argsEl.classList.toggle('hidden', !visible);
      expandBtn.innerHTML = visible ? '&#x25BC;' : '&#x25B6;';
    });

    getMessagesEl().appendChild(row);
    AppState.currentToolCard = row;
    AppState.fallbackToolStep = Math.max(AppState.fallbackToolStep, step + 1);
    scrollToEnd();
  },

  showToolResult(ok, output, toolName) {
    let card = AppState.currentToolCard;
    if (!card) {
      // 无对应 start，创建占位卡片
      Messages.finishAIMsg();
      this.showToolStart(AppState.fallbackToolStep, toolName || 'tool', '');
      card = AppState.currentToolCard;
      AppState.fallbackToolStep++;
    }

    // 替换 spinner → 结果图标
    const spinner = card.querySelector('.tool-spinner');
    if (spinner) {
      const icon = document.createElement('span');
      icon.className = 'tool-status ' + (ok ? 'ok' : 'err');
      icon.textContent = ok ? '✓' : '✗';
      icon.setAttribute('aria-label', ok ? '成功' : '失败');
      spinner.replaceWith(icon);
    }

    // 隐藏展开按钮、折叠参数
    const expandBtn = card.querySelector('.tool-expand');
    if (expandBtn) expandBtn.classList.add('hidden');
    const argsEl = card.querySelector('.tool-args');
    if (argsEl) argsEl.classList.add('hidden');

    // 显示结果
    if (output) {
      const slot = card.querySelector('.tool-result-slot');
      const resultEl = document.createElement('div');
      resultEl.className = 'tool-result ' + (ok ? 'ok' : 'err');
      resultEl.textContent = output.substring(0, 1500);
      slot.appendChild(resultEl);
    }

    AppState.resetTool();
    scrollToEnd();
  },
};
```

### 4.5 input.js — 输入处理

```js
/**
 * input.js — 输入框处理
 *
 * 职责：
 *   - 监听 Enter 发送、Shift+Enter 换行
 *   - 自动增高 textarea
 *   - sendInput()         — 发送消息到 Python
 *   - clearInput()        — 清空输入框
 *   - setInputEnabled()   — 启用/禁用
 *   - setInputPlaceholder() — 更新占位文字
 */

const Input = {
  init() {
    const inputEl = document.getElementById('chat-input');
    const sendBtn = document.getElementById('send-btn');

    sendBtn.addEventListener('click', () => this.send());

    inputEl.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' && !e.shiftKey) {
        e.preventDefault();
        this.send();
      }
    });

    inputEl.addEventListener('input', () => {
      inputEl.style.height = '34px';
      inputEl.style.height = Math.min(inputEl.scrollHeight, 120) + 'px';
    });
  },

  send() {
    const inputEl = document.getElementById('chat-input');
    const text = inputEl.value.trim();
    if (!text) return;
    inputEl.value = '';
    inputEl.style.height = '34px';
    if (window.bridge && window.bridge.onUserSend) {
      window.bridge.onUserSend(text);
    }
  },

  clear() {
    const el = document.getElementById('chat-input');
    el.value = '';
    el.style.height = '34px';
  },

  setEnabled(enabled) {
    document.getElementById('chat-input').disabled = !enabled;
    document.getElementById('send-btn').disabled = !enabled;
    if (enabled) document.getElementById('chat-input').focus();
  },

  setPlaceholder(text) {
    document.getElementById('chat-input').placeholder = text;
  },
};
```

### 4.6 dialog.js — 对话框与通知

```js
/**
 * dialog.js — 确认对话框 + 通知栏
 */

const Dialog = {
  showConfirm(confirmId, prompt) {
    AppState.confirmId = confirmId;
    document.getElementById('confirm-prompt').textContent = prompt;
    document.getElementById('confirm-overlay').classList.remove('hidden');
  },

  hideConfirm() {
    document.getElementById('confirm-overlay').classList.add('hidden');
    AppState.confirmId = null;
  },

  init() {
    document.getElementById('confirm-yes').addEventListener('click', () => {
      if (window.bridge && window.bridge.onConfirmResult) {
        window.bridge.onConfirmResult(AppState.confirmId, true);
      }
      this.hideConfirm();
    });

    document.getElementById('confirm-no').addEventListener('click', () => {
      if (window.bridge && window.bridge.onConfirmResult) {
        window.bridge.onConfirmResult(AppState.confirmId, false);
      }
      this.hideConfirm();
    });
  },
};

const Notice = {
  show(message) {
    if (AppState.noticeTimer) clearTimeout(AppState.noticeTimer);
    const bar = document.getElementById('notice-bar');
    document.getElementById('notice-text').textContent = message;
    bar.classList.remove('hidden');
    AppState.noticeTimer = setTimeout(() => {
      bar.classList.add('hidden');
      AppState.noticeTimer = null;
    }, 3000);
  },
};
```

### 4.7 py-callbacks.js — Python 回调注册

```js
/**
 * py-callbacks.js — 注册所有 Python 可调用的前端方法
 *
 * window.pyCallbacks 是 Python 端通过 QWebChannel 调用的入口。
 * 本模块将所有分散在各模块的函数汇总注册，形成统一的 API 表面。
 */

window.pyCallbacks = {
  // ── 消息 ──────────────────────────
  appendUserMsg:      Messages.appendUserMsg,
  appendAIText:       Messages.appendAIText,
  finishAIMsg:        Messages.finishAIMsg,

  // ── 工具 ──────────────────────────
  showToolStart:      Tools.showToolStart,
  showToolResult:     Tools.showToolResult,

  // ── 启动面板 ───────────────────────
  showStartup:        Messages.showStartup,

  // ── 状态栏 ─────────────────────────
  setStatus:          Status.set,
  setWaiting:         Status.setWaiting,
  setSpeaking:        Status.setSpeaking,
  setListening:       Status.setListening,
  updateTokenDisplay: Status.updateToken,
  setModelLabel:      Status.setModelLabel,

  // ── 输入 ───────────────────────────
  clearInput:         Input.clear,
  setInputEnabled:    Input.setEnabled,
  setInputPlaceholder: Input.setPlaceholder,

  // ── 对话框 + 通知 ────────────────
  showConfirmDialog:  Dialog.showConfirm,
  hideConfirmDialog:  Dialog.hideConfirm,
  showNotice:         Notice.show,

  // ── 滚动 ───────────────────────────
  scrollToEnd:        Messages.scrollToEnd,
};
```

### 4.8 app.js — 精简入口

```js
/**
 * app.js — 应用入口
 *
 * 职责：初始化编排，注册 DOMContentLoaded 回调。
 * 具体逻辑已拆分到各模块，本文件仅做启动编排。
 */

document.addEventListener('DOMContentLoaded', () => {
  'use strict';

  // 1. 初始化输入模块（绑定事件）
  Input.init();

  // 2. 初始化对话框模块（绑定确认按钮事件）
  Dialog.init();

  // 3. 默认启用输入
  Input.setEnabled(true);

  // 4. 日志
  console.log('[app] AI Voice Agent 前端已就绪');
});
```

---

## 5. CSS 分层方案

### 5.1 加载顺序

```html
<link rel="stylesheet" href="css/variables.css">
<link rel="stylesheet" href="css/reset.css">
<link rel="stylesheet" href="css/layout.css">
<link rel="stylesheet" href="css/animations.css">
<link rel="stylesheet" href="css/title-bar.css">
<link rel="stylesheet" href="css/chat-area.css">
<link rel="stylesheet" href="css/messages.css">
<link rel="stylesheet" href="css/input-area.css">
<link rel="stylesheet" href="css/status-bar.css">
<link rel="stylesheet" href="css/dialog.css">
```

### 5.2 文件职责

| 文件 | 内容 | 预估行数 |
|------|------|---------|
| `variables.css` | CSS 自定义属性（色板/字体/间距/动效时长/阴影） | ~60 行 |
| `reset.css` | 通配符 reset + html/body 基础设置 + 字体声明 | ~30 行 |
| `layout.css` | `#app` flex 纵向布局，各区域 flex-shrink 策略 | ~20 行 |
| `animations.css` | `@keyframes` 集合（pulse-glow, msg-in, spin, typing-bounce, fade-in, dialog-in, notice-in/out） | ~60 行 |
| `title-bar.css` | 标题栏样式 + 底部光晕伪元素 | ~50 行 |
| `chat-area.css` | 聊天滚动区 + 扫描线纹理 + 自定义滚动条 | ~40 行 |
| `messages.css` | 消息行、头像、气泡、Markdown 内容（h1-h3/code/pre/table/blockquote/list/link） | ~170 行 |
| `input-area.css` | 输入框、wrapper、发送按钮（Persona 按钮样式也放这里） | ~90 行 |
| `status-bar.css` | 状态栏、打字指示器、语音图标、token 显示 | ~60 行 |
| `dialog.css` | 确认对话框 overlay、通知栏 | ~100 行 |

**总计约 680 行**（比当前 777 行略少，但组织更清晰）。

### 5.3 主题切换预留

`variables.css` 中所有颜色使用 CSS 变量，后续切换主题只需替换变量文件：

```css
/* variables.css — Persona 5 主题 */
:root {
  --color-bg:          #0a0a0f;
  --color-surface:     #22223a;
  --color-border:      #2e2e4a;
  --color-accent:      #e63946;
  --color-secondary:   #00d4ff;
  /* ... 更多变量 ... */
}

/* 未来可添加 light 主题 */
/*
:root.light {
  --color-bg:          #f5f5f5;
  --color-surface:     #ffffff;
  ...
}
*/
```

---

## 6. 新增功能

### 6.1 流式中断按钮（Stop Generation）

在 AI 回复进行中时，状态栏显示"停止生成"按钮：

```
状态栏左：[⏹ 停止生成]  ← 点击后发送取消信号
状态栏右：token 统计
```

**后端实现**：
- Python 端新增 `BackendBridge.cancelGeneration` 槽方法
- 收到信号后设置 threading.Event，Agent 循环检测该 Event

**前端实现**：
```js
// 状态栏中新增
const stopBtn = document.getElementById('stop-btn');
stopBtn.addEventListener('click', () => {
  if (window.bridge && window.bridge.onCancel) {
    window.bridge.onCancel();
  }
});

// AI 开始回复时显示，完成时隐藏
function setGenerating(active) {
  stopBtn.classList.toggle('hidden', !active);
}
```

### 6.2 对话导出按钮

标题栏右侧新增导出按钮，将当前对话导出为 Markdown 文件：

```js
function exportChat() {
  const messages = document.querySelectorAll('.msg-row, .tool-row');
  let md = '# AI Voice Agent 对话记录\n\n';
  messages.forEach(row => {
    if (row.classList.contains('user')) {
      md += `**你**: ${row.querySelector('.bubble').textContent}\n\n`;
    } else if (row.classList.contains('ai')) {
      md += `**AI**: ${row.querySelector('.bubble').innerHTML}\n\n`;
    }
  });
  // 通过 bridge 发送给 Python 保存
}
```

---

## 7. 实施计划

### 阶段 1：基础设施（1-2 小时）

- [ ] 创建 `css/` 目录，拆分 CSS 为 10 个文件
- [ ] 创建 `js/` 目录，拆分 JS 为 10 个文件
- [ ] 引入 `vendor/marked.min.js`
- [ ] 更新 `index.html` 的 `<link>` 和 `<script>` 引用

### 阶段 2：状态与 Markdown（30 分钟）

- [ ] 实现 `state.js` 集中状态管理
- [ ] 实现 `markdown.js` 封装 marked
- [ ] 删除 app.js 中手写的 `renderMarkdown()` 函数

### 阶段 3：模块迁移（1-2 小时）

- [ ] 拆分 `messages.js`（用户/AI 气泡 + 启动面板）
- [ ] 拆分 `tools.js`（工具调用卡片）
- [ ] 拆分 `input.js`（输入处理）
- [ ] 拆分 `dialog.js`（确认对话框 + 通知）
- [ ] 创建 `py-callbacks.js`（统一注册）

### 阶段 4：集成验证（30 分钟）

- [ ] 启动 Qt GUI 验证基本消息收发
- [ ] 验证工具调用卡片展示
- [ ] 验证确认对话框
- [ ] 验证 Markdown 渲染（表格、代码块、嵌套列表）
- [ ] 验证流式输出时 Markdown 不闪烁

### 阶段 5：新增功能（1 小时）

- [ ] 实现流式中断按钮
- [ ] 标题栏添加对话导出按钮
- [ ] 添加基础 ARIA 标签

---

## 8. 风险与缓解

| 风险 | 概率 | 缓解措施 |
|------|------|---------|
| marked.js 加载失败（离线环境） | 中 | 将 marked.min.js 内置于 vendor/ 目录，不从 CDN 加载 |
| CSS 拆分后样式优先级冲突 | 低 | 保持与原有选择器特殊度一致，按原有顺序加载 |
| 模块间依赖循环 | 低 | 依赖方向：state → markdown → messages → tools → dialog，单向无环 |
| QWebChannel 时序问题 | 低 | bridge.js 不变，pyCallbacks 注册方式不变 |
| 文件数量增加导致加载变慢 | 极低 | QWebEngine 本地加载，10个小文件总大小与1个大文件相同 |

---

## 9. 附录

### A. 当前文件行数统计

| 文件 | 行数 |
|------|------|
| index.html | 80 |
| style.css | 777 |
| app.js | 420 |
| bridge.js | 67 |
| **总计** | **1344** |

### B. 重构后预估文件行数

| 文件 | 预估行数 |
|------|---------|
| index.html | 60 |
| css/variables.css | 60 |
| css/reset.css | 30 |
| css/layout.css | 20 |
| css/animations.css | 60 |
| css/title-bar.css | 50 |
| css/chat-area.css | 40 |
| css/messages.css | 170 |
| css/input-area.css | 90 |
| css/status-bar.css | 60 |
| css/dialog.css | 100 |
| js/bridge.js | 67 (不变) |
| js/state.js | 45 |
| js/markdown.js | 50 |
| js/messages.js | 150 |
| js/tools.js | 130 |
| js/input.js | 80 |
| js/dialog.js | 70 |
| js/py-callbacks.js | 60 |
| js/app.js | 25 |
| vendor/marked.min.js | ~35KB (外部) |
| **JS 总计（不含 vendor）** | **677** |
| **CSS 总计** | **680** |

### C. Python 端变更范围

| 文件 | 变更 | 说明 |
|------|------|------|
| `ai_voice_agent/ui/qt/qt_ui.py` | 不变 | API 无需修改 |
| `ai_voice_agent/ui/qt/_bridge.py` | 新增 `onCancel` 槽 | 支持流式中断 |
| `ai_voice_agent/ui/qt/window.py` | 新增 `cancel_generation()` | 传递中断信号 |
| `ai_voice_agent/qt_chat_session.py` | 新增取消检查 | 在流式循环中检测取消事件 |
