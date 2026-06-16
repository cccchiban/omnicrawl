/**
 * input.js — 输入框处理（发送按钮状态联动 + 推理强度下拉 + Workspace 信息）
 */
'use strict';

var Input = (function() {

  var bridgeReady = false;
  var desiredEnabled = true;
  var currentReasoning = 'none';

  function inputEl() {
    return document.getElementById('chat-input');
  }

  function sendBtnEl() {
    return document.getElementById('send-btn');
  }

  function init() {
    var inp = inputEl();
    var btn = sendBtnEl();
    if (!inp || !btn) return;

    btn.addEventListener('click', send);

    inp.addEventListener('keydown', function(e) {
      if (e.key === 'Enter' && !e.shiftKey) {
        e.preventDefault();
        send();
      }
    });

    inp.addEventListener('input', function() {
      autosize(inp);
      updateSendButtonState(inp);
      updateInputContainerState(inp);
    });

    // 初始状态
    updateSendButtonState(inp);
    initReasoningDropdown();
    initRippleEffect();
  }

  function autosize(inp) {
    var styles = getComputedStyle(document.documentElement);
    var maxHeight = parseInt(styles.getPropertyValue('--input-max-height'), 10) || 200;
    inp.style.height = 'auto';
    inp.style.height = Math.min(inp.scrollHeight, maxHeight) + 'px';
  }

  function resetHeight(inp) {
    inp.style.height = 'var(--input-min-height)';
  }

  function send() {
    var inp = inputEl();
    if (!inp) return;
    var text = inp.value.trim();
    if (!text) return;
    if (!(window.bridge && window.bridge.onUserSend)) {
      Notice.show('界面通信尚未就绪，请稍后重试');
      return;
    }
    inp.value = '';
    resetHeight(inp);
    updateSendButtonState(inp);
    updateInputContainerState(inp);
    Messages.appendUserMsg(text);
    window.bridge.onUserSend(text);
  }

  function clear() {
    var el = inputEl();
    if (el) { el.value = ''; resetHeight(el); updateSendButtonState(el); updateInputContainerState(el); }
  }

  function setEnabled(enabled) {
    desiredEnabled = enabled;
    applyEnabledState();
  }

  function setBridgeReady(ready) {
    bridgeReady = ready;
    applyEnabledState();
  }

  function applyEnabledState() {
    var inp = inputEl();
    var btn = sendBtnEl();
    var enabled = desiredEnabled && bridgeReady;
    if (inp) inp.disabled = !enabled;
    if (btn) btn.disabled = !enabled;
    if (enabled && inp) inp.focus();
    updateSendButtonState(inp);
  }

  /** 根据输入内容更新发送按钮视觉状态（CSS class 驱动，避免 inline style 与 CSS 冲突） */
  function updateSendButtonState(inp) {
    var btn = sendBtnEl();
    if (!btn || !inp) return;
    var hasText = inp.value.trim().length > 0;
    var enabled = desiredEnabled && bridgeReady;
    btn.classList.toggle('ready', enabled && hasText);
  }

  /** 输入框容器状态：有内容时微缩放 */
  function updateInputContainerState(inp) {
    var container = document.querySelector('.input-container');
    if (!container || !inp) return;
    container.classList.toggle('has-content', inp.value.trim().length > 0);
  }

  /** 初始化点击涟漪效果 */
  function initRippleEffect() {
    var btn = sendBtnEl();
    if (!btn) return;

    btn.addEventListener('click', function(e) {
      if (btn.disabled) return;
      var rect = btn.getBoundingClientRect();
      var ripple = document.createElement('span');
      ripple.className = 'ripple';
      var size = 20;
      ripple.style.width = size + 'px';
      ripple.style.height = size + 'px';
      ripple.style.left = (e.clientX - rect.left - size / 2) + 'px';
      ripple.style.top = (e.clientY - rect.top - size / 2) + 'px';
      btn.appendChild(ripple);
      setTimeout(function() { ripple.remove(); }, 500);
    });
  }

  /** 初始化推理强度下拉菜单 */
  function initReasoningDropdown() {
    var toggle = document.getElementById('reasoning-toggle');
    var dropdown = document.getElementById('reasoning-dropdown');
    var wrapper = document.getElementById('reasoning-dropdown-wrapper');
    if (!toggle || !dropdown) return;

    toggle.addEventListener('click', function(e) {
      e.stopPropagation();
      var open = !dropdown.classList.contains('open');
      setReasoningDropdownOpen(dropdown, toggle, open);
    });

    // 点击选项
    var options = dropdown.querySelectorAll('.reasoning-option');
    options.forEach(function(opt) {
      opt.addEventListener('click', function() {
        var nextReasoning = opt.dataset.value || 'none';
        if (window.bridge && window.bridge.setReasoningEffort) {
          setReasoningEffort(nextReasoning);
          setReasoningDropdownOpen(dropdown, toggle, false);
          window.bridge.setReasoningEffort(currentReasoning);
        } else if (window.Notice && Notice.show) {
          Notice.show('界面通信尚未就绪，请稍后重试');
        }
      });
    });

    // 点击外部关闭
    document.addEventListener('click', function(e) {
      if (wrapper && !wrapper.contains(e.target)) {
        setReasoningDropdownOpen(dropdown, toggle, false);
      }
    });
  }

  function setReasoningDropdownOpen(dropdown, toggle, open) {
    dropdown.classList.toggle('open', open);
    dropdown.classList.toggle('hidden', !open);
    if (toggle) toggle.setAttribute('aria-expanded', String(open));
  }

  function setReasoningEffort(effort) {
    currentReasoning = effort || 'none';
    var dropdown = document.getElementById('reasoning-dropdown');
    if (!dropdown) return;
    var options = dropdown.querySelectorAll('.reasoning-option');
    options.forEach(function(opt) {
      opt.classList.toggle('active', (opt.dataset.value || 'none') === currentReasoning);
    });
  }

  /** 设置 Workspace 信息 */
  function setWorkspaceInfo(path, status) {
    var infoEl = document.getElementById('workspace-info');
    var pathEl = document.getElementById('workspace-path');
    var statusEl = document.getElementById('workspace-status');
    if (!infoEl) return;

    if (path) {
      if (pathEl) pathEl.textContent = path;
      if (statusEl) {
        statusEl.textContent = status || '已激活';
        statusEl.style.background = 'rgba(34, 197, 94, 0.15)';
        statusEl.style.color = 'var(--accent-green)';
      }
      infoEl.classList.add('visible');
    } else {
      infoEl.classList.remove('visible');
    }
  }

  function setPlaceholder(text) {
    var el = inputEl();
    if (el) el.placeholder = text;
  }

  return {
    init: init,
    send: send,
    clear: clear,
    setEnabled: setEnabled,
    setBridgeReady: setBridgeReady,
    setPlaceholder: setPlaceholder,
    setWorkspaceInfo: setWorkspaceInfo,
    setReasoningEffort: setReasoningEffort,
  };

})();
