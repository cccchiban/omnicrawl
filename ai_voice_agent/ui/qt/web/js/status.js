/**
 * status.js — 状态栏管理
 */
'use strict';

var Status = (function() {

  function refreshFrame() {
    var left = document.querySelector('.status-left');
    if (!left) return;

    // 状态胶囊只在真正有内容时出现：避免普通空闲状态下留下一块无意义的装饰框。
    var statusText = document.getElementById('status-text');
    var typing = document.getElementById('typing-indicator');
    var stopBtn = document.getElementById('stop-btn');
    var hasStatusText = statusText && statusText.textContent.trim().length > 0;
    var hasTyping = typing && !typing.classList.contains('hidden');
    var hasStop = stopBtn && !stopBtn.classList.contains('hidden');

    left.classList.toggle('status-active', !!(hasStatusText || hasTyping || hasStop));
  }

  function set(message, italic) {
    // 状态文字仅显示在底部 #status-text 中，
    // 不再通过 Messages.showStatusMessage 在对话流中间渲染内联状态行。
    var el = document.getElementById('status-text');
    if (el) {
      el.textContent = message;
      el.style.fontStyle = italic ? 'italic' : 'normal';
    }
    refreshFrame();
  }

  function setWaiting(active) {
    var el = document.getElementById('typing-indicator');
    if (el) el.classList.toggle('hidden', !active);
    refreshFrame();
  }

 function setGenerating(active) {
   setWaiting(active);
   // 停止按钮已移除，发送键在回复中变为停止键
   // 输入框在 Agent 回复期间保持可用
   if (window.Input && Input.setGenerating) {
     Input.setGenerating(active);
   }
   refreshFrame();
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


  // ── DNA 双螺旋 SVG 生成 ──
  function buildHelixSvg() {
    var W = 80, H = 44;
    var A = H * 0.37;
    var CY = H / 2;
    var LAMBDA = W * 0.72;
    var TOTAL_W = W + LAMBDA * 2;
    var CLIP_ID = "helixClipQt";

    function wavePath(phase, n) {
      n = n || 80;
      var d = "";
      for (var i = 0; i <= n; i++) {
        var x = (i / n) * TOTAL_W;
        var y = CY + A * Math.sin((x / LAMBDA) * 2 * Math.PI + phase);
        d += (i === 0 ? "M" : "L") + x.toFixed(1) + "," + y.toFixed(1) + " ";
      }
      return d.trim();
    }

    var parts = [];
    parts.push('<svg width="' + TOTAL_W + '" height="' + H + '" style="display:block;" xmlns="http://www.w3.org/2000/svg">');
    parts.push('<defs><clipPath id="' + CLIP_ID + '"><rect x="0" y="0" width="' + TOTAL_W + '" height="' + CY + '"/></clipPath></defs>');
    // Back strands (slightly more visible)
    parts.push('<path d="' + wavePath(0)       + '" fill="none" stroke="#a5b4fc" stroke-width="1.5" stroke-opacity="0.22" stroke-linecap="round"/>');
    parts.push('<path d="' + wavePath(Math.PI) + '" fill="none" stroke="#a5b4fc" stroke-width="1.5" stroke-opacity="0.22" stroke-linecap="round"/>');
    // Rungs
    var step = LAMBDA / 5;
    var cnt = Math.ceil(TOTAL_W / step) + 1;
    for (var i = 0; i < cnt; i++) {
      var x = i * step;
      var y1 = CY + A * Math.sin((x / LAMBDA) * 2 * Math.PI);
      var y2 = CY + A * Math.sin((x / LAMBDA) * 2 * Math.PI + Math.PI);
      parts.push('<line x1="' + x.toFixed(1) + '" y1="' + y1.toFixed(1) + '" x2="' + x.toFixed(1) + '" y2="' + y2.toFixed(1) + '" stroke="#a5b4fc" stroke-width="1" stroke-opacity="0.30"/>');
    }
    // Front strands (clipped, vivid)
    parts.push('<path d="' + wavePath(0)       + '" fill="none" stroke="#8b9cf7" stroke-width="2" stroke-opacity="0.90" clip-path="url(#' + CLIP_ID + ')" stroke-linecap="round"/>');
    parts.push('<path d="' + wavePath(Math.PI) + '" fill="none" stroke="#8b9cf7" stroke-width="2" stroke-opacity="0.90" clip-path="url(#' + CLIP_ID + ')" stroke-linecap="round"/>');
    parts.push("</svg>");
    return parts.join("");
  }

  // ── Helix SVG lazy init · 按需注入，确保首次显式时已就绪 ──
  var _helixSvgCache = null;

  function ensureHelixReady() {
    if (_helixSvgCache) return;
    _helixSvgCache = buildHelixSvg();
    var containers = document.querySelectorAll(".dna-helix-svg-inner");
    for (var i = 0; i < containers.length; i++) {
      if (!containers[i].querySelector("svg")) {
        containers[i].innerHTML = _helixSvgCache;
      }
    }
  }

  // Inject SVG on first setWaiting(true) call — this is when the container
  // becomes visible (removes .hidden), guaranteeing non-zero dimensions.
  var _origSetWaiting = setWaiting;
  setWaiting = function(active) {
    if (active) ensureHelixReady();
    _origSetWaiting(active);
  };

  // Also preload on DOM ready to avoid FOUC
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", function() { ensureHelixReady(); });
  } else {
    ensureHelixReady();
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
