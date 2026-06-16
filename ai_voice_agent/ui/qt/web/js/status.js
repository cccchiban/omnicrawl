/**
 * status.js — 状态栏管理
 */
'use strict';

var Status = (function() {

  function set(message, italic) {
    var el = document.getElementById('status-text');
    if (!el) return;
    el.textContent = message;
    el.style.fontStyle = italic ? 'italic' : 'normal';
  }

  function setWaiting(active) {
    var el = document.getElementById('typing-indicator');
    if (el) el.classList.toggle('hidden', !active);
  }

  function setGenerating(active) {
    setWaiting(active);
    var stopBtn = document.getElementById('stop-btn');
    if (stopBtn) stopBtn.classList.toggle('hidden', !active);
  }

  function setSpeaking(active) {
    var el = document.getElementById('speaking-icon');
    if (el) el.classList.toggle('hidden', !active);
  }

  function setListening(active) {
    var el = document.getElementById('listening-icon');
    if (el) el.classList.toggle('hidden', !active);
  }

  function updateToken(text) {
    var el = document.getElementById('token-display');
    if (el) el.textContent = text;
  }

  function setModelLabel(text) {
    var el = document.getElementById('model-label');
    if (el) el.textContent = text;
  }

  return {
    setStatus: set,
    setWaiting: setWaiting,
    setGenerating: setGenerating,
    setSpeaking: setSpeaking,
    setListening: setListening,
    updateToken: updateToken,
    setModelLabel: setModelLabel,
  };

})();
