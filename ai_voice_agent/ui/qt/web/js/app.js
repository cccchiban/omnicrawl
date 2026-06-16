/**
 * app.js — 应用入口
 *
 * 职责：初始化编排，绑定全局事件。
 * 具体逻辑已拆分到各模块，本文件仅做启动编排。
 */
'use strict';

document.addEventListener('DOMContentLoaded', () => {

  // 1. 初始化输入模块（绑定 Enter/Shift+Enter 等事件）
  Input.init();

  // 2. 初始化对话框模块（绑定确认/拒绝按钮事件）
  Dialog.init();

  // 3. 停止生成按钮
  const stopBtn = document.getElementById('stop-btn');
  if (stopBtn) {
    stopBtn.addEventListener('click', () => {
      if (window.bridge && window.bridge.onCancel) {
        window.bridge.onCancel();
      }
    });
  }

  // 4. 导出对话按钮
  const exportBtn = document.getElementById('export-btn');
  if (exportBtn) {
    exportBtn.addEventListener('click', () => {
      exportChat();
    });
  }

  // 5. QWebChannel 连接成功前禁用输入，避免消息发送到尚未就绪的 bridge。
  Input.setBridgeReady(false);
  window.addEventListener('bridge-ready', () => {
    Input.setBridgeReady(true);
  });

  // 6. 模型选择器 — 下拉菜单交互
  initModelSelector();

  console.log('[app] AI Voice Agent 前端已就绪');
});

/**
 * 初始化模型选择器下拉菜单
 */
function initModelSelector() {
  const selector = document.getElementById('model-selector');
  const dropdown = document.getElementById('model-dropdown');
  const list = document.getElementById('model-dropdown-list');
  const label = document.getElementById('model-label');

  if (!selector || !dropdown || !list) return;

  let currentModel = '';
  let isOpen = false;
  let refreshInFlight = false;
  let hasLoadedRemoteModels = false;
  let refreshTimer = null;

  // 渲染模型列表
  function renderModels(models) {
    list.innerHTML = '';
    if (!Array.isArray(models) || models.length === 0) {
      renderMessage('暂无模型');
      return;
    }
    models.forEach(model => {
      const item = document.createElement('button');
      item.className = 'model-option' + (model.id === currentModel ? ' selected' : '');
      item.dataset.modelId = model.id;
      const provider = normalizeProvider(model.provider || model.id);
      const name = model.name || model.id;
      item.innerHTML = `
        <span class="model-icon ${provider}">${providerIcon(provider)}</span>
        <span class="model-name"></span>
        <svg class="model-check" viewBox="0 0 24 24" width="16" height="16">
          <path fill="currentColor" d="M9 16.17L4.83 12l-1.42 1.41L9 19 21 7l-1.41-1.41z"/>
        </svg>
      `;
      const nameEl = item.querySelector('.model-name');
      if (nameEl) nameEl.textContent = name;
      item.addEventListener('click', (e) => {
        e.stopPropagation();
        selectModel({ id: model.id, name: name, provider: provider });
      });
      list.appendChild(item);
    });
  }

  function renderMessage(text) {
    list.innerHTML = '';
    const item = document.createElement('div');
    item.className = 'model-option model-option-message';
    item.textContent = text;
    list.appendChild(item);
  }

  function normalizeProvider(value) {
    const text = String(value || '').toLowerCase();
    if (text.indexOf('claude') === 0 || text.indexOf('anthropic/claude') === 0) return 'claude';
    if (text.indexOf('gpt') === 0 || text.indexOf('chatgpt') === 0 || /^o[134]/.test(text)) return 'gpt';
    if (text.indexOf('deepseek') === 0) return 'deepseek';
    if (text.indexOf('qwen') === 0 || text.indexOf('qwq') === 0) return 'qwen';
    if (text.indexOf('glm') === 0 || text.indexOf('chatglm') === 0) return 'glm';
    return 'other';
  }

  function providerIcon(provider) {
    const icons = {
      claude: 'C',
      gpt: 'G',
      deepseek: 'D',
      qwen: 'Q',
      glm: 'Z',
      other: 'M',
    };
    return icons[provider] || 'M';
  }

  // 选择模型
  function selectModel(model) {
    currentModel = model.id;
    if (label) label.textContent = model.name;

    // 更新选中状态
    list.querySelectorAll('.model-option').forEach(opt => {
      opt.classList.toggle('selected', opt.dataset.modelId === model.id);
    });

    // 通知 Python 后端
    if (window.bridge && window.bridge.onModelChange) {
      window.bridge.onModelChange(model.id);
    }

    closeDropdown();
  }

  // 打开下拉菜单
  function openDropdown() {
    isOpen = true;
    selector.classList.add('active');
    dropdown.classList.remove('hidden');
    requestModelRefresh();
    // 强制重绘以确保过渡动画生效
    dropdown.offsetHeight;
    dropdown.classList.add('show');
  }

  function requestModelRefresh() {
    if (refreshInFlight) return;
    refreshInFlight = true;
    if (!hasLoadedRemoteModels) {
      renderMessage('正在加载模型...');
    }
    if (window.bridge && window.bridge.onModelSelect) {
      window.bridge.onModelSelect();
      refreshTimer = setTimeout(() => {
        if (refreshInFlight) {
          finishRefresh();
          renderMessage('模型列表加载超时');
        }
      }, 12000);
      return;
    }
    finishRefresh();
    renderMessage('模型列表暂不可用');
  }

  function finishRefresh() {
    refreshInFlight = false;
    if (refreshTimer) {
      clearTimeout(refreshTimer);
      refreshTimer = null;
    }
  }

  // 关闭下拉菜单
  function closeDropdown() {
    isOpen = false;
    selector.classList.remove('active');
    dropdown.classList.remove('show');
    // 等待过渡动画结束后隐藏
    setTimeout(() => {
      if (!isOpen) dropdown.classList.add('hidden');
    }, 150);
  }

  // 切换下拉菜单
  function toggleDropdown() {
    if (isOpen) {
      closeDropdown();
    } else {
      openDropdown();
    }
  }

  // 绑定点击事件
  selector.addEventListener('click', (e) => {
    e.stopPropagation();
    toggleDropdown();
  });

  // 点击外部关闭下拉菜单
  document.addEventListener('click', (e) => {
    if (isOpen && !dropdown.contains(e.target) && !selector.contains(e.target)) {
      closeDropdown();
    }
  });

  // 初始化渲染
  renderMessage('打开后加载模型');

  // 暴露更新方法供 Python 调用
  function updateModelList(models, modelId) {
    finishRefresh();
    if (Array.isArray(models) && models.length > 0) {
      hasLoadedRemoteModels = true;
      if (modelId) currentModel = modelId;
      renderModels(models);
      if (currentModel) {
        const selected = models.find(model => model.id === currentModel);
        if (selected && label) label.textContent = selected.name || selected.id;
      }
    } else {
      renderMessage('暂无模型');
    }
  }

  function setCurrentModel(modelId, modelName) {
    currentModel = modelId;
    if (label && modelName) label.textContent = modelName;
    list.querySelectorAll('.model-option').forEach(opt => {
      opt.classList.toggle('selected', opt.dataset.modelId === modelId);
    });
  }

  function showError(message) {
    finishRefresh();
    renderMessage(message || '模型列表加载失败');
    if (window.Notice && Notice.show) {
      Notice.show(message || '模型列表加载失败');
    }
  }

  window.ModelSelector = {
    updateModelList: updateModelList,
    setCurrentModel: setCurrentModel,
    showError: showError,
  };
}

/**
 * 导出当前对话为 Markdown 格式，通过 bridge 发送给 Python 保存。
 */
function exportChat() {
  const rows = document.querySelectorAll('.msg-row, .tool-row');
  let md = '# AI Voice Agent 对话记录\n\n';
  const now = new Date();
  md += '> 导出时间：' + now.toLocaleString() + '\n\n---\n\n';

  rows.forEach(row => {
    if (row.classList.contains('user')) {
      const bubble = row.querySelector('.bubble');
      if (bubble) md += '**你**：\n\n' + bubble.textContent.trim() + '\n\n';
    } else if (row.classList.contains('ai')) {
      const bubble = row.querySelector('.bubble');
      if (bubble) md += '**AI 助手**：\n\n' + bubble.textContent.trim() + '\n\n';
    } else if (row.classList.contains('tool-row')) {
      const name = row.querySelector('.tool-name');
      const status = row.querySelector('.tool-status');
      const result = row.querySelector('.tool-result');
      if (name) {
        const statusText = status ? (status.classList.contains('ok') ? '✓' : '✗') : '';
        md += '**🔧 ' + statusText + ' ' + name.textContent.trim() + '**\n\n';
        if (result) md += '```\n' + result.textContent.trim() + '\n```\n\n';
      }
    }
  });

  // 通过 bridge 发送给 Python 保存
  if (window.bridge && window.bridge.onExportChat) {
    window.bridge.onExportChat(md);
  } else {
    // fallback：复制到剪贴板
    navigator.clipboard.writeText(md).then(() => {
      Notice.show('对话内容已复制到剪贴板');
    }).catch(() => {
      Notice.show('导出失败，请检查权限');
    });
  }
}
