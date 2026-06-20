/**
 * messages.js — Codex 风格消息气泡（内联确认卡片支持）
 */
'use strict';

var Messages = (function() {

  function messagesEl() {
    return document.getElementById('messages');
  }

  function chatAreaEl() {
    return document.getElementById('chat-area');
  }

  function createEl(tag, attrs) {
    var el = document.createElement(tag);
    if (attrs) {
      var keys = Object.keys(attrs);
      for (var i = 0; i < keys.length; i++) {
        el[keys[i]] = attrs[keys[i]];
      }
    }
    return el;
  }

  function timeNow() {
    var now = new Date();
    var hours = String(now.getHours()).padStart(2, '0');
    var minutes = String(now.getMinutes()).padStart(2, '0');
    return hours + ':' + minutes;
  }

  /** 当前模型头像 HTML，模型图标失败时保留字母兜底。 */
  function modelAvatarHtml() {
    if (typeof modelIconMarkup === 'function') {
      return modelIconMarkup(AppState.currentModelProvider, 'model-avatar-icon');
    }
    return '<span class="model-avatar-icon other icon-fallback">' +
      '<span class="model-icon-fallback">' + escHtml(AppState.currentModelIconFallback || 'M') + '</span>' +
      '</span>';
  }

  /** 用户头像 SVG */
  function userAvatarSvg() {
    return '<svg viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg"><path d="M12 12c2.21 0 4-1.79 4-4s-1.79-4-4-4-4 1.79-4 4 1.79 4 4 4zm0 2c-2.67 0-8 1.34-8 4v2h16v-2c0-2.66-5.33-4-8-4z"/></svg>';
  }

  /** 退出空状态（首次发送消息时调用） */
  function exitEmptyState() {
    var main = document.getElementById('main');
    if (main && main.classList.contains('empty-state')) {
      main.classList.remove('empty-state');
    }
  }

  // ── 用户消息 ──────────────────────────────────────

  function appendUserMsg(text, timeText) {
    if (!text) return;
    finishCurrentAI();
    exitEmptyState();

    var container = messagesEl();
    if (!container) {
      console.error('[Messages] #messages element not found');
      return;
    }

    var row = createEl('div', { className: 'msg-row user' });
    row.innerHTML =
      '<div class="avatar user">' + userAvatarSvg() + '</div>' +
      '<div class="msg-body">' +
        '<div class="msg-header">' +
          '<span class="msg-role">你</span>' +
          '<span class="msg-time">' + escHtml(timeText || timeNow()) + '</span>' +
        '</div>' +
        '<div class="bubble">' + escHtml(text) + '</div>' +
      '</div>';
    container.appendChild(row);
    scrollToEnd();
  }

  // ── AI 消息（流式） ───────────────────────────────

  function appendAIText(text, timeText) {
    if (AppState.currentAIBubble === null || AppState.currentAIBubble === undefined) {
      var container = messagesEl();
      if (!container) return;

      var row = createEl('div', { className: 'msg-row ai' });
      row.innerHTML =
        '<div class="avatar ai">' + modelAvatarHtml() + '</div>' +
        '<div class="msg-body">' +
          '<div class="msg-header">' +
            '<span class="msg-role">' + escHtml(AppState.currentModelName) + '</span>' +
            '<span class="msg-time">' + escHtml(timeText || timeNow()) + '</span>' +
          '</div>' +
          '<div class="bubble"><span class="typing-cursor"></span></div>' +
        '</div>';
      container.appendChild(row);
      AppState.currentAIBubble = row.querySelector('.bubble');
      AppState.currentRawText = '';
    }
    AppState.currentRawText += text;

    // 更新气泡内容，保留打字机光标
    var cursor = AppState.currentAIBubble.querySelector('.typing-cursor');
    if (cursor) cursor.remove();
    AppState.currentAIBubble.textContent = AppState.currentRawText;
    var newCursor = document.createElement('span');
    newCursor.className = 'typing-cursor';
    AppState.currentAIBubble.appendChild(newCursor);

    scrollToEnd();
  }

  function finishAIMsg() {
    if (!AppState.currentAIBubble) return;
    var cursor = AppState.currentAIBubble.querySelector('.typing-cursor');
    if (cursor) cursor.remove();
    var html = Markdown.render(AppState.currentRawText);
    AppState.currentAIBubble.innerHTML = html;
    AppState.currentAIBubble = null;
    AppState.currentRawText = '';
  }

  function finishCurrentAI() {
    if (AppState.currentAIBubble) {
      finishAIMsg();
    }
  }

  function appendFinishedAIMsg(text, timeText) {
    if (!text) return;
    appendAIText(text, timeText);
    finishAIMsg();
  }

  function clear() {
    AppState.resetAI();
    AppState.resetTool();
    AppState.fallbackToolStep = 1;
    var container = messagesEl();
    if (container) container.innerHTML = '';
    // 恢复空状态
    var main = document.getElementById('main');
    if (main && !main.classList.contains('empty-state')) {
      main.classList.add('empty-state');
    }
  }

  function renderSessionMessages(items) {
    clear();
    if (!Array.isArray(items) || items.length === 0) {
      // 空会话：保持 empty-state
      return;
    }
    // 有消息时退出 empty-state（clear() 会添加，这里移除）
    var main = document.getElementById('main');
    if (main && main.classList.contains('empty-state')) {
      main.classList.remove('empty-state');
    }
    for (var i = 0; i < items.length; i++) {
      var item = items[i] || {};
      if (item.type === 'user') {
        appendUserMsg(String(item.content || ''), item.time || '');
      } else if (item.type === 'assistant') {
        appendFinishedAIMsg(String(item.content || ''), item.time || '');
      } else if (item.type === 'tool_start' && window.Tools && Tools.showToolStart) {
        Tools.showToolStart(
          Number(item.step || AppState.fallbackToolStep),
          String(item.tool || 'tool'),
          JSON.stringify(item.arguments || {})
        );
      } else if (item.type === 'tool_result' && window.Tools && Tools.showToolResult) {
        Tools.showToolResult(
          Boolean(item.ok),
          String(item.output || ''),
          String(item.tool || 'tool')
        );
        if (Tools.showToolArtifact) {
          Tools.showToolArtifact(item.uiArtifact || item.ui_artifact || {});
        }
      }
    }
    finishCurrentAI();
    scrollToEnd();
  }

  // ── 内联确认卡片 ─────────────────────────────────

  function showConfirmCard(confirmId, prompt) {
    var container = messagesEl();
    if (!container) return;

    // 先完成当前 AI 消息
    finishCurrentAI();

    var row = createEl('div', { className: 'msg-row confirm' });
    row.dataset.confirmId = confirmId;
    row.innerHTML =
      '<div class="avatar ai">' + modelAvatarHtml() + '</div>' +
      '<div class="msg-body">' +
        '<div class="msg-header">' +
          '<span class="msg-role">' + escHtml(AppState.currentModelName) + '</span>' +
          '<span class="msg-time">' + timeNow() + '</span>' +
        '</div>' +
        '<div class="confirm-inline-card" id="confirm-card-' + confirmId + '">' +
          '<div class="confirm-inline-header">' +
            '<svg viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg"><path d="M12 2L2 7l10 5 10-5-10-5zM2 17l10 5 10-5M2 12l10 5 10-5"/></svg>' +
            '<span>需要您的确认</span>' +
          '</div>' +
          '<div class="confirm-inline-body">' + escHtml(prompt) + '</div>' +
          '<div class="confirm-inline-actions">' +
            '<button class="btn-deny" data-action="deny">拒绝</button>' +
            '<button class="btn-allow" data-action="allow">允许</button>' +
          '</div>' +
        '</div>' +
      '</div>';

    container.appendChild(row);
    scrollToEnd();

    // 绑定按钮事件
    var card = row.querySelector('.confirm-inline-card');
    var allowBtn = card.querySelector('[data-action="allow"]');
    var denyBtn = card.querySelector('[data-action="deny"]');

    allowBtn.addEventListener('click', function() {
      if (window.bridge && window.bridge.onConfirmResult) {
        window.bridge.onConfirmResult(confirmId, true);
      }
      disableConfirmButtons(card);
    });

    denyBtn.addEventListener('click', function() {
      if (window.bridge && window.bridge.onConfirmResult) {
        window.bridge.onConfirmResult(confirmId, false);
      }
      disableConfirmButtons(card);
    });
  }

  function disableConfirmButtons(card) {
    var buttons = card.querySelectorAll('button');
    for (var i = 0; i < buttons.length; i++) {
      buttons[i].disabled = true;
      buttons[i].style.opacity = '0.5';
    }
  }

  // ── 启动面板 ──────────────────────────────────────

  function showStartup(title, lines) {
    var linesHtml = '';
    var workspacePath = '';
    var workspaceStatus = '';
    var reasoningEffort = '';
    var approvalMode = '';
    for (var i = 0; i < lines.length; i++) {
      var line = lines[i];
      var colonIdx = line.indexOf(':');
      if (colonIdx > 0) {
        var key = line.substring(0, colonIdx).trim();
        var val = line.substring(colonIdx + 1).trim();
        var cls = '';
        if (val === '已启用' || val === '开启' || val === 'auto' || val === 'Qt GUI') cls = ' on';
        else if (val === '已禁用' || val === '关闭' || val === 'manual') cls = ' off';
        else if (val === 'review') cls = ' warn';
        if (key.toLowerCase() === 'workspace' || key === '工作区') {
          workspacePath = val;
          workspaceStatus = '已激活';
        }
        if (key.toLowerCase() === 'thinking') {
          var match = val.match(/推理强度：\s*([^，,\s]+)/);
          if (match) reasoningEffort = match[1];
          else if (val.indexOf('已禁用') !== -1) reasoningEffort = 'none';
          else if (val.indexOf('已启用') !== -1) reasoningEffort = 'low';
        }
        if (key.toLowerCase() === 'approval') {
          if (val.indexOf('人工') !== -1 || val.indexOf('手动') !== -1 || val === 'manual') approvalMode = 'manual';
          else if (val.indexOf('自动') !== -1 || val === 'auto') approvalMode = 'auto';
          else if (val.indexOf('审查') !== -1 || val === 'review') approvalMode = 'review';
        }
        linesHtml +=
          '<div class="startup-line">' +
            '<span class="startup-key">' + escHtml(key) + '</span>' +
            '<span class="startup-val' + cls + '">' + escHtml(val) + '</span>' +
          '</div>';
      } else {
        linesHtml +=
          '<div class="startup-line">' +
            '<span class="startup-val">' + escHtml(line) + '</span>' +
          '</div>';
      }
    }

    var card = createEl('div', { className: 'startup-card' });
    card.innerHTML =
      '<div class="startup-header">' +
        '<span class="startup-icon">&#x2726;</span>' +
        '<span class="startup-title">' + escHtml(title) + '</span>' +
      '</div>' +
      '<div class="startup-sep"></div>' +
      linesHtml;

    messagesEl().appendChild(card);
    scrollToEnd();

    if (workspacePath && window.Input && Input.setWorkspaceInfo) {
      Input.setWorkspaceInfo(workspacePath, workspaceStatus);
    }

    if (reasoningEffort && window.Input && Input.setReasoningEffort) {
      Input.setReasoningEffort(reasoningEffort);
    }

    if (approvalMode && window.Input && Input.setApprovalMode) {
      Input.setApprovalMode(approvalMode);
    }

    // 提取模型名
    for (var j = 0; j < lines.length; j++) {
      var l = lines[j];
      if (l.toLowerCase().indexOf('model') !== -1 || l.indexOf('model:') === 0) {
        var model = l.split(':').slice(1).join(':').trim();
        if (model) {
          Status.setModelLabel(model);
          if (typeof setCurrentModelIdentity === 'function') {
            setCurrentModelIdentity({ id: model, name: model });
          }
        }
      }
    }
  }

  // ── 滚动 ──────────────────────────────────────────

  function scrollToEnd() {
    var area = chatAreaEl();
    if (area) {
      area.scrollTop = area.scrollHeight;
    }
  }

  // ── 公开 API ──────────────────────────────────────

  return {
    appendUserMsg: appendUserMsg,
    appendAIText: appendAIText,
    finishAIMsg: finishAIMsg,
    finishCurrentAI: finishCurrentAI,
    appendFinishedAIMsg: appendFinishedAIMsg,
    clear: clear,
    renderSessionMessages: renderSessionMessages,
    showConfirmCard: showConfirmCard,
    showStartup: showStartup,
    scrollToEnd: scrollToEnd,
  };

})();
