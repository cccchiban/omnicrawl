/**
 * input.js — 输入框处理（发送按钮状态联动）
 */
'use strict';

var Input = (function() {

  var bridgeReady = false;
  var desiredEnabled = true;

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
    });

    // 初始状态
    updateSendButtonState(inp);
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
    Messages.appendUserMsg(text);
    window.bridge.onUserSend(text);
  }

  function clear() {
    var el = inputEl();
    if (el) { el.value = ''; resetHeight(el); updateSendButtonState(el); }
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
  };

})();
