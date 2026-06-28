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
  showHtmlPreview:    function(title, html) {
    if (window.HtmlPreview) window.HtmlPreview.show(title, html);
  },

  // ── 启动面板 ───────────────────────────────────
  showStartup:        Messages.showStartup,

  // ── 状态栏 ────────────────────────────────────
  setStatus:          Status.setStatus,
  setWaiting:         Status.setWaiting,
  setGenerating:      Status.setGenerating,
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
  setWindowMaximized: function(maximized) {
    if (window.WindowChrome) window.WindowChrome.setMaximized(maximized);
  },
  updateSessionList: function(sessions) {
    if (window.SessionSidebar) window.SessionSidebar.updateSessionList(sessions);
    if (window.SessionSearch) window.SessionSearch.updateSessionList(sessions);
  },
  renderSessionMessages: Messages.renderSessionMessages,
  setCurrentSession: function(sessionId, title) {
    if (window.SessionSidebar) window.SessionSidebar.setCurrentSession(sessionId, title);
    if (window.SessionSearch) window.SessionSearch.setCurrentSession(sessionId, title);
  },
  showSessionListError: function(message) {
    if (window.SessionSidebar) window.SessionSidebar.showError(message);
  },

  // ── 项目 ──────────────────────────────────────
  updateProjectList: function(projects) {
    if (window.ProjectSidebar) window.ProjectSidebar.updateProjectList(projects);
    if (window.SessionSearch) window.SessionSearch.updateProjectList(projects);
    if (window.updateProjectDropdown) window.updateProjectDropdown(projects);
  },
  setCurrentProject: function(projectPath) {
    if (window.ProjectSidebar) window.ProjectSidebar.setCurrentProject(projectPath);
  },
  setProjectModalPath: function(projectPath) {
    if (window.ProjectSidebar) window.ProjectSidebar.setProjectModalPath(projectPath);
  },
  clearInput:         Input.clear,
  setInputEnabled:    Input.setEnabled,
  setInputPlaceholder: Input.setPlaceholder,
  updateSlashCommands: Input.updateSlashCommands,
  setWorkspaceInfo:   Input.setWorkspaceInfo,
  setReasoningEffort: Input.setReasoningEffort,
  setApprovalMode:    Input.setApprovalMode,

  // ── 对话框 + 通知 ─────────────────────────────
  showConfirmDialog:  Dialog.show,
  hideConfirmDialog:  Dialog.hide,
  showNotice:         Notice.show,

  // ── 滚动 ──────────────────────────────────────
  scrollToEnd:        Messages.scrollToEnd,
};
