/**
 * markdown.js — Markdown 渲染封装
 *
 * 封装 marked.js 提供：
 *   1. render(text)         — 完整 Markdown → HTML
 *   2. streamText(text)     — 流式场景纯文本转义
 */
'use strict';
const Markdown = {
    /**
     * 渲染完整 Markdown 文本为 HTML。
     * AI 消息完成时调用，替换流式纯文本为格式化内容。
     * @param {string} text
     * @returns {string}
     */
    render(text) {
        if (!text)
            return '';
        if (typeof marked === 'undefined') {
            // fallback：marked 未加载时退回纯文本转义
            return escHtml(text).replace(/\n/g, '<br>');
        }
        var html = marked.parse(text, {
            breaks: true,
            gfm: true,
            headerIds: false,
            mangle: false,
        });
        return sanitizeRenderedHtml(html);
    },
    /**
     * 流式输出过程中的文本转义。
     * 不做 Markdown 解析，只做 HTML 转义 + 换行。
     * @param {string} text
     * @returns {string}
     */
    streamText(text) {
        return escHtml(text).replace(/\n/g, '<br>');
    },
};
const FORBIDDEN_TAGS = [
    'script', 'style', 'iframe', 'object', 'embed', 'link', 'meta', 'base',
    'form', 'input', 'button', 'textarea', 'select', 'option',
];
const FORBIDDEN_ATTRS = [
    'style', 'srcdoc',
];
/**
 * 清洗 marked 输出的 HTML，避免模型输出的原始 HTML 破坏 Qt 页面布局。
 *
 * marked 默认会保留 Markdown 中的 HTML。桌面端虽然加载的是本地页面，
 * 但 AI 回复里的 <style>、fixed 元素、事件属性或 iframe 都可能遮挡输入区、
 * 弹窗和状态栏；这里保留常规 Markdown 结构，移除会改变全局布局或执行脚本的
 * 标签/属性。
 */
function sanitizeRenderedHtml(html) {
    var template = document.createElement('template');
    template.innerHTML = html;
    template.content.querySelectorAll(FORBIDDEN_TAGS.join(',')).forEach(function (el) {
        el.remove();
    });
    template.content.querySelectorAll('*').forEach(function (el) {
        Array.prototype.slice.call(el.attributes).forEach(function (attr) {
            var name = attr.name.toLowerCase();
            var value = attr.value.trim().toLowerCase();
            if (FORBIDDEN_ATTRS.indexOf(name) !== -1 ||
                /^on\w/.test(name) ||
                ((name === 'href' || name === 'src') && value.indexOf('javascript:') === 0)) {
                el.removeAttribute(attr.name);
            }
        });
    });
    decorateCodeBlocks(template.content);
    return template.innerHTML;
}
/**
 * 为每个 <pre><code> 代码块包裹一层结构，加上语言标签与复制按钮。
 * marked 输出的语言记录在 code 元素的 class="language-xxx" 上。
 */
function decorateCodeBlocks(root) {
    root.querySelectorAll('pre > code').forEach(function (code) {
        var pre = code.parentNode;
        var parentCls = pre && pre.parentNode && pre.parentNode.classList;
        if (!pre || !pre.parentNode || (parentCls && parentCls.contains('code-block'))) {
            return;
        }
        var lang = '';
        var match = (code.className || '').match(/language-([\w+#-]+)/i);
        if (match)
            lang = match[1];
        var wrapper = document.createElement('div');
        wrapper.className = 'code-block';
        var head = document.createElement('div');
        head.className = 'code-block-head';
        var langEl = document.createElement('span');
        langEl.className = 'code-block-lang';
        langEl.textContent = lang || 'code';
        var btn = document.createElement('button');
        btn.type = 'button';
        btn.className = 'code-copy-btn';
        btn.setAttribute('data-copy', '1');
        btn.setAttribute('aria-label', '复制代码');
        btn.innerHTML = COPY_ICON + '<span class="code-copy-label">复制</span>';
        head.appendChild(langEl);
        head.appendChild(btn);
        pre.parentNode.insertBefore(wrapper, pre);
        wrapper.appendChild(head);
        wrapper.appendChild(pre);
    });
}
var COPY_ICON = '<svg viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg" aria-hidden="true">' +
    '<rect x="9" y="9" width="11" height="11" rx="2"/>' +
    '<path d="M5 15V5a2 2 0 0 1 2-2h10"/></svg>';
/** HTML 实体转义 */
function escHtml(s) {
    return s.replace(/&/g, '&amp;')
        .replace(/</g, '&lt;')
        .replace(/>/g, '&gt;');
}
