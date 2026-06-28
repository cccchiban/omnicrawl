/**
 * dialog.js — 内联确认模式（不再使用弹出式遮罩）
 */
'use strict';
var Dialog = (function () {
    function init() {
        // 内联确认卡片由 Messages.showConfirmCard() 动态创建
        // 不再需要绑定模态对话框按钮
    }
    function show(confirmId, prompt) {
        // 使用内联确认卡片替代弹出式对话框
        Messages.showConfirmCard(confirmId, prompt);
    }
    function hide() {
        // 内联卡片不需要 hide，用户点击按钮后自动禁用
    }
    return {
        init: init,
        show: show,
        hide: hide,
    };
})();
var Notice = (function () {
    function show(message) {
        if (AppState.noticeTimer) {
            clearTimeout(AppState.noticeTimer);
        }
        var bar = document.getElementById('notice-bar');
        var textEl = document.getElementById('notice-text');
        if (textEl)
            textEl.textContent = message;
        if (bar)
            bar.classList.remove('hidden');
        AppState.noticeTimer = setTimeout(function () {
            if (bar)
                bar.classList.add('hidden');
            AppState.noticeTimer = null;
        }, 3000);
    }
    return { show: show };
})();
