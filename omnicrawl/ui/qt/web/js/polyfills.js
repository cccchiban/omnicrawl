/**
 * polyfills.js — Qt WebEngine 兼容补丁。
 *
 * PyQt5 附带的 Qt WebEngine 可能使用较旧 Chromium 内核；当前内置
 * marked v15 会调用 ES2022 的 Array/String.prototype.at()。在旧内核中
 * 该方法不存在，会导致 Markdown 渲染时报 `t.at is not a function`。
 */
'use strict';
(function () {
    function at(index) {
        var value = Object(this);
        var length = value.length >>> 0;
        if (length === 0)
            return undefined;
        var integerIndex = Number(index) || 0;
        if (integerIndex < 0) {
            integerIndex += length;
        }
        if (integerIndex < 0 || integerIndex >= length) {
            return undefined;
        }
        return value[integerIndex];
    }
    if (!Array.prototype.at) {
        Object.defineProperty(Array.prototype, 'at', {
            value: at,
            configurable: true,
            writable: true,
        });
    }
    if (!String.prototype.at) {
        Object.defineProperty(String.prototype, 'at', {
            value: at,
            configurable: true,
            writable: true,
        });
    }
})();
