/**
 * state.js — 集中状态管理
 *
 * 所有模块共享的状态统一存于此对象。
 * 不使用 Proxy/Object.defineProperty，保持简单直接。
 */
'use strict';

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

  // ── 重置方法 ──────────────────────────────────────────

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
