/**
 * tools.js — Codex 风格工具调用卡片（SVG 头像）
 */
'use strict';
var Tools = (function () {
    function messagesEl() {
        return document.getElementById('messages');
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
    /** 工具头像 SVG */
    function toolAvatarSvg() {
        return '<svg viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg"><path d="M14.7 6.3a1 1 0 0 0 0 1.4l1.6 1.6a1 1 0 0 0 1.4 0l3.77-3.77a6 6 0 0 1-7.94 7.94l-6.91 6.91a2.12 2.12 0 0 1-3-3l6.91-6.91a6 6 0 0 1 7.94-7.94l-3.76 3.76z"/></svg>';
    }
    function showToolStart(step, toolName, argsJson) {
        Messages.finishCurrentAI();
        var formatted = '';
        try {
            formatted = JSON.stringify(JSON.parse(argsJson), null, 2);
        }
        catch (_e) {
            formatted = argsJson.substring(0, 800);
        }
        var row = createEl('div', { className: 'tool-row' });
        row.dataset.step = String(step);
        row.innerHTML =
            '<div class="tool-avatar">' + toolAvatarSvg() + '</div>' +
                '<div class="tool-body">' +
                '<div class="tool-card">' +
                '<div class="tool-header">' +
                '<span class="step-badge">' + step + '</span>' +
                '<span class="tool-name">' + escHtml(toolName) + '</span>' +
                '<button class="tool-expand" title="展开/折叠参数" aria-label="展开参数">' +
                '<svg viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg" width="12" height="12"><path d="M6 9l6 6 6-6"/></svg>' +
                '</button>' +
                '<span class="tool-spinner" aria-label="执行中"></span>' +
                '</div>' +
                '<div class="tool-args">' + escHtml(formatted.substring(0, 1500)) + '</div>' +
                '<div class="tool-result-slot"></div>' +
                '</div>' +
                '</div>';
        var expandBtn = row.querySelector('.tool-expand');
        var argsEl = row.querySelector('.tool-args');
        var visible = true;
        expandBtn.addEventListener('click', function () {
            visible = !visible;
            argsEl.classList.toggle('hidden', !visible);
            var svg = expandBtn.querySelector('svg');
            if (svg) {
                svg.innerHTML = visible
                    ? '<path d="M6 9l6 6 6-6"/>'
                    : '<path d="M9 6l6 6-6 6"/>';
            }
        });
        messagesEl().appendChild(row);
        AppState.currentToolCard = row;
        AppState.fallbackToolStep = Math.max(AppState.fallbackToolStep, step + 1);
        if (window.Messages && Messages.moveStatusMessageToEnd) {
            Messages.moveStatusMessageToEnd();
        }
        Messages.scrollToEnd();
    }
    function showToolResult(ok, output, toolName) {
        var card = AppState.currentToolCard;
        if (!card) {
            Messages.finishCurrentAI();
            showToolStart(AppState.fallbackToolStep, toolName || 'tool', '');
            card = AppState.currentToolCard;
            AppState.fallbackToolStep++;
        }
        var spinner = card.querySelector('.tool-spinner');
        if (spinner) {
            var icon = document.createElement('span');
            icon.className = 'tool-status ' + (ok ? 'ok' : 'err');
            icon.textContent = ok ? '✓' : '✗';
            icon.setAttribute('aria-label', ok ? '成功' : '失败');
            spinner.replaceWith(icon);
        }
        var expandBtn = card.querySelector('.tool-expand');
        if (expandBtn)
            expandBtn.classList.add('hidden');
        var argsEl = card.querySelector('.tool-args');
        if (argsEl)
            argsEl.classList.add('hidden');
        if (output) {
            var slot = card.querySelector('.tool-result-slot');
            var resultEl = document.createElement('div');
            resultEl.className = 'tool-result ' + (ok ? 'ok' : 'err');
            resultEl.textContent = output.substring(0, 1500);
            slot.appendChild(resultEl);
        }
        AppState.currentToolCard = null;
        if (window.Messages && Messages.moveStatusMessageToEnd) {
            Messages.moveStatusMessageToEnd();
        }
        Messages.scrollToEnd();
    }
    function showToolArtifact(artifact) {
        if (!artifact || artifact.type !== 'html' || !window.HtmlPreview)
            return;
        if (artifact.html) {
            HtmlPreview.show(artifact.title || 'HTML 预览', artifact.html);
        }
        else if (artifact.artifact_path) {
            HtmlPreview.showPlaceholder(artifact.title || 'HTML 预览', 'HTML 内容已保存到 ' + artifact.artifact_path + '，请重新生成或打开 artifact 查看。');
        }
    }
    return {
        showToolStart: showToolStart,
        showToolResult: showToolResult,
        showToolArtifact: showToolArtifact,
    };
})();
