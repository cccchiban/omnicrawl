/**
 * py-callbacks.js — Python 回调注册
 *
 * 汇总注册所有 Python 端通过 QWebChannel 可调用的前端方法。
 * 注意：不使用 .bind()，各模块函数均为独立函数，不依赖 this。
 */
'use strict';

window.pyCallbacks = {
  // ── 消息 ──────────────────────────────────────
  appendUserMsg:      Messages.appendUserMsg,
  appendAIText:       Messages.appendAIText,
  finishAIMsg:        Messages.finishAIMsg,

  // ── 工具 ──────────────────────────────────────
  showToolStart:      Tools.showToolStart,
  showToolResult:     Tools.showToolResult,

  // ── 启动面板 ───────────────────────────────────
  showStartup:        Messages.showStartup,

  // ── 状态栏 ────────────────────────────────────
  setStatus:          Status.setStatus,
  setWaiting:         Status.setGenerating,
  setSpeaking:        Status.setSpeaking,
  setListening:       Status.setListening,
  updateTokenDisplay: Status.updateToken,
  setModelLabel:      Status.setModelLabel,
  updateModelList:    function(models, currentModel) {
    if (window.ModelSelector) window.ModelSelector.updateModelList(models, currentModel);
  },
  setCurrentModel:    function(modelId, modelName) {
    if (window.ModelSelector) window.ModelSelector.setCurrentModel(modelId, modelName);
  },
  showModelListError: function(message) {
    if (window.ModelSelector) window.ModelSelector.showError(message);
  },
  updateSessionList: function(sessions) {
    if (window.SessionSidebar) window.SessionSidebar.updateSessionList(sessions);
  },
  renderSessionMessages: Messages.renderSessionMessages,
  setCurrentSession: function(sessionId, title) {
    if (window.SessionSidebar) window.SessionSidebar.setCurrentSession(sessionId, title);
  },
  showSessionListError: function(message) {
    if (window.SessionSidebar) window.SessionSidebar.showError(message);
  },

  // ── 项目 ──────────────────────────────────────
  updateProjectList: function(projects) {
    if (window.ProjectSidebar) window.ProjectSidebar.updateProjectList(projects);
  },
  setCurrentProject: function(projectPath) {
    if (window.ProjectSidebar) window.ProjectSidebar.setCurrentProject(projectPath);
  },
  clearInput:         Input.clear,
  setInputEnabled:    Input.setEnabled,
  setInputPlaceholder: Input.setPlaceholder,
  setWorkspaceInfo:   Input.setWorkspaceInfo,
  setReasoningEffort: Input.setReasoningEffort,

  // ── 对话框 + 通知 ─────────────────────────────
  showConfirmDialog:  Dialog.show,
  hideConfirmDialog:  Dialog.hide,
  showNotice:         Notice.show,

  // ── 滚动 ──────────────────────────────────────
  scrollToEnd:        Messages.scrollToEnd,
};
