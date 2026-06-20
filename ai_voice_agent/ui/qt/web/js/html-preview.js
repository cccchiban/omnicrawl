/**
 * html-preview.js — 右侧 HTML 显示区
 *
 * Agent 通过 Python 回调或 display_html 工具把具体化的数据页送到这里。
 * iframe 使用 sandbox 隔离内容，避免预览 HTML 影响主聊天界面布局。
 */
'use strict';

var HtmlPreview = (function() {
  var panel = null;
  var shell = null;
  var frame = null;
  var titleEl = null;
  var toggleBtn = null;
  var popoutBtn = null;
  var emptyEl = null;
  var currentHtml = '';

  function init() {
    panel = document.getElementById('html-preview-panel');
    shell = document.getElementById('app-shell');
    frame = document.getElementById('html-preview-frame');
    titleEl = document.getElementById('html-preview-title');
    toggleBtn = document.getElementById('html-preview-toggle-btn');
    popoutBtn = document.getElementById('html-preview-popout-btn');
    emptyEl = document.getElementById('html-preview-empty');
    if (!panel || !shell || !frame) return;

    var collapsed = readCollapsed();
    setCollapsed(collapsed);

    if (toggleBtn) {
      toggleBtn.addEventListener('click', function() {
        setCollapsed(!panel.classList.contains('collapsed'));
        writeCollapsed(panel.classList.contains('collapsed'));
      });
    }

    if (popoutBtn) {
      popoutBtn.addEventListener('click', function() {
        if (!currentHtml) return;
        var win = window.open('', '_blank', 'noopener,noreferrer');
        if (!win) {
          if (window.Notice) Notice.show('新窗口被拦截');
          return;
        }
        win.opener = null;
        win.document.open();
        win.document.write(currentHtml);
        win.document.close();
      });
    }
  }

  function show(title, html) {
    if (!panel || !frame) init();
    if (!panel || !frame) return;
    currentHtml = String(html || '');
    if (titleEl) titleEl.textContent = title || 'HTML 预览';
    panel.classList.remove('empty');
    if (emptyEl) emptyEl.classList.add('hidden');
    frame.classList.remove('hidden');
    frame.srcdoc = currentHtml;
    setCollapsed(false);
    writeCollapsed(false);
  }

  function showPlaceholder(title, message) {
    var body =
      '<!doctype html><html lang="zh-CN"><head><meta charset="utf-8">' +
      '<style>body{font-family:system-ui,"Segoe UI",sans-serif;margin:0;padding:24px;color:#1f2937;background:#f8fafc}' +
      '.box{max-width:680px;margin:0 auto;background:#fff;border:1px solid #e5e7eb;border-radius:8px;padding:18px;line-height:1.6}' +
      'h1{font-size:18px;margin:0 0 10px}</style></head><body><section class="box"><h1>' +
      escHtml(title || 'HTML 预览') + '</h1><p>' + escHtml(message || '暂无可恢复的 HTML 内容。') +
      '</p></section></body></html>';
    show(title, body);
  }

  function setCollapsed(collapsed) {
    if (!panel || !shell) return;
    panel.classList.toggle('collapsed', Boolean(collapsed));
    shell.classList.toggle('html-preview-collapsed', Boolean(collapsed));
    if (toggleBtn) {
      toggleBtn.setAttribute('aria-expanded', String(!collapsed));
      toggleBtn.setAttribute('title', collapsed ? '展开显示区' : '折叠显示区');
      toggleBtn.setAttribute('aria-label', collapsed ? '展开显示区' : '折叠显示区');
    }
  }

  function readCollapsed() {
    try {
      return localStorage.getItem('html-preview-collapsed') === 'true';
    } catch (_err) {
      return false;
    }
  }

  function writeCollapsed(collapsed) {
    try {
      localStorage.setItem('html-preview-collapsed', String(collapsed));
    } catch (_err) {
      // 偏好保存失败不影响本次预览。
    }
  }

  return {
    init: init,
    show: show,
    showPlaceholder: showPlaceholder,
  };
})();
