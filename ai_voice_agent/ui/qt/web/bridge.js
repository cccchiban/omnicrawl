/**
 * bridge.js — QWebChannel 通信桥
 *
 * 暴露全局 `window.bridge` 对象，供 app.js 调用 Python 后端方法；
 * 同时注册 `window.pyCallbacks` 供 Python 端调用前端方法。
 *
 * QWebChannel 的初始化方式：
 *   PyQt5 通过 QWebEnginePage 的 webChannel 会自动注入
 *   qt.webChannelTransport 对象，我们在 DOMContentLoaded 后连接。
 */
(function() {
  'use strict';

  /** 初始化 pyCallbacks 存根 */
  window.pyCallbacks = {
    appendUserMsg: null,
    appendAIText: null,
    finishAIMsg: null,
    setStatus: null,
    showNotice: null,
    showStartup: null,
    showToolStart: null,
    showToolResult: null,
    updateTokenDisplay: null,
    showConfirmDialog: null,
    hideConfirmDialog: null,
    clearInput: null,
    setInputEnabled: null,
    setInputPlaceholder: null,
    setSpeaking: null,
    setListening: null,
    setWaiting: null,
    scrollToEnd: null,
    setModelLabel: null,
    updateModelList: null,
    setCurrentModel: null,
    showModelListError: null,
    showModelSelect: null,
    setReasoningEffort: null,
    updateSessionList: null,
    renderSessionMessages: null,
    setCurrentSession: null,
    showSessionListError: null,
  };

  /**
   * 尝试连接 QWebChannel。
 * QWebEngineView 设置了 webChannel 后，页面中会有 qt.webChannelTransport。
 * 可能在页面加载早期还不存在，需要等待。
   */
  function tryConnect(retries) {
    if (typeof qt !== 'undefined' && qt.webChannelTransport) {
      new QWebChannel(qt.webChannelTransport, function(channel) {
        window.bridge = channel.objects.bridge;
        window.dispatchEvent(new Event('bridge-ready'));
        console.log('[bridge] QWebChannel connected');
      });
      return;
    }
    if (retries > 0) {
      setTimeout(function() { tryConnect(retries - 1); }, 100);
    } else {
      console.warn('[bridge] QWebChannel not available after retries — using mock');
      window.bridge = {
        onUserSend: function() {},
        onConfirmResult: function() {},
        onModelSelect: function() {},
        onModelChange: function() {},
        setReasoningEffort: function() {},
        setApprovalMode: function() {},
        onNewSession: function() {},
        onRequestSessions: function() {},
        onResumeSession: function() {},
        onRenameSession: function() {},
        onCompactSession: function() {},
        onDeleteSession: function() {},
      };
    }
  }

  // 页面加载后尝试连接
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', function() { tryConnect(50); });
  } else {
    tryConnect(50);
  }
})();
