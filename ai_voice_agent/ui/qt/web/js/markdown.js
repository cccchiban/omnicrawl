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
    if (!text) return '';
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

  template.content.querySelectorAll(FORBIDDEN_TAGS.join(',')).forEach(function(el) {
    el.remove();
  });

  template.content.querySelectorAll('*').forEach(function(el) {
    Array.prototype.slice.call(el.attributes).forEach(function(attr) {
      var name = attr.name.toLowerCase();
      var value = attr.value.trim().toLowerCase();
      if (
        FORBIDDEN_ATTRS.indexOf(name) !== -1 ||
        /^on\w/.test(name) ||
        ((name === 'href' || name === 'src') && value.indexOf('javascript:') === 0)
      ) {
        el.removeAttribute(attr.name);
      }
    });
  });

  return template.innerHTML;
}

/** HTML 实体转义 */
function escHtml(s) {
  return s.replace(/&/g, '&amp;')
          .replace(/</g, '&lt;')
          .replace(/>/g, '&gt;');
}
