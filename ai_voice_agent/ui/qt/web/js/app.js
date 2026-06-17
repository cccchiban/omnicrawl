/**
 * app.js — 应用入口
 *
 * 职责：初始化编排，绑定全局事件。
 * 具体逻辑已拆分到各模块，本文件仅做启动编排。
 */
'use strict';

const LOBE_ICON_BASE_URL = 'https://registry.npmmirror.com/@lobehub/icons-static-svg/latest/files/icons/';

function normalizeModelProvider(value) {
  const text = String(value || '').toLowerCase();
  if (text.indexOf('claude') === 0 || text.indexOf('anthropic/claude') === 0) return 'claude';
  if (text.indexOf('gpt') === 0 || text.indexOf('chatgpt') === 0 || text.indexOf('openai') === 0 || /^o[134]/.test(text)) return 'gpt';
  if (text.indexOf('deepseek') === 0) return 'deepseek';
  if (text.indexOf('qwen') === 0 || text.indexOf('qwq') === 0) return 'qwen';
  if (text.indexOf('glm') === 0 || text.indexOf('chatglm') === 0) return 'glm';
  return 'other';
}

function modelProviderIconSlug(provider) {
  const icons = {
    claude: 'claude',
    gpt: 'openai',
    deepseek: 'deepseek',
    qwen: 'qwen',
    glm: 'chatglm',
  };
  return icons[provider] || '';
}

function modelProviderFallback(provider) {
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

function modelProviderLabel(provider) {
  const labels = {
    claude: 'Claude',
    gpt: 'OpenAI',
    deepseek: 'DeepSeek',
    qwen: 'Qwen',
    glm: 'ChatGLM',
    other: 'Model',
  };
  return labels[provider] || 'Model';
}

function modelIconMarkup(provider, className) {
  const slug = modelProviderIconSlug(provider);
  const fallback = modelProviderFallback(provider);
  const labelText = modelProviderLabel(provider);
  const extraClass = className || 'model-icon';

  if (!slug) {
    return `<span class="${extraClass} ${provider} icon-fallback" title="${labelText}">
      <span class="model-icon-fallback">${fallback}</span>
    </span>`;
  }

  return `<span class="${extraClass} ${provider}" title="${labelText}">
    <img
      alt=""
      aria-hidden="true"
      src="${LOBE_ICON_BASE_URL}${slug}.svg"
      onerror="this.parentElement.classList.add('icon-fallback')"
    />
    <span class="model-icon-fallback">${fallback}</span>
  </span>`;
}

function setCurrentModelIdentity(model) {
  if (!model || typeof AppState === 'undefined') return;
  const provider = normalizeModelProvider(model.provider || model.id);
  AppState.setCurrentModelIdentity({
    id: model.id,
    name: model.name || model.id,
    provider: provider,
    iconSlug: modelProviderIconSlug(provider),
    fallback: modelProviderFallback(provider),
    label: modelProviderLabel(provider),
  });
}

document.addEventListener('DOMContentLoaded', () => {

  // 1. 初始化输入模块（绑定 Enter/Shift+Enter 等事件）
  Input.init();

  // 2. 初始化对话框模块（绑定确认/拒绝按钮事件）
  Dialog.init();

  // 3. 侧边栏折叠
  initSidebarToggle();

  // 4. 停止生成按钮
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
  const navExport = document.getElementById('nav-export');
  if (navExport) {
    navExport.addEventListener('click', (event) => {
      event.preventDefault();
      exportChat();
    });
  }

  // 5. QWebChannel 连接成功前禁用输入，避免消息发送到尚未就绪的 bridge。
  Input.setBridgeReady(false);
  window.addEventListener('bridge-ready', () => {
    Input.setBridgeReady(true);
    if (window.SessionSidebar) {
      SessionSidebar.requestRefresh();
    }
    if (window.ProjectSidebar) {
      ProjectSidebar.requestProjectList();
    }
  });

  // 6. 模型选择器 — 下拉菜单交互
  initModelSelector();
  initSessionSidebar();

  console.log('[app] AI Voice Agent 前端已就绪');
});

function initSidebarToggle() {
  const sidebar = document.getElementById('sidebar');
  const toggle = document.getElementById('sidebar-toggle');
  if (!sidebar || !toggle) return;

  const collapsed = readSidebarCollapsed();
  setSidebarCollapsed(collapsed);

  toggle.addEventListener('click', () => {
    const isCollapsed = !sidebar.classList.contains('collapsed');
    setSidebarCollapsed(isCollapsed);
    writeSidebarCollapsed(isCollapsed);
  });

  function setSidebarCollapsed(collapsed) {
    sidebar.classList.toggle('collapsed', collapsed);
    toggle.setAttribute('aria-expanded', String(!collapsed));
    toggle.setAttribute('aria-label', collapsed ? '展开侧边栏' : '折叠侧边栏');
  }

  function readSidebarCollapsed() {
    try {
      return localStorage.getItem('sidebar-collapsed') === 'true';
    } catch (err) {
      return false;
    }
  }

  function writeSidebarCollapsed(collapsed) {
    try {
      localStorage.setItem('sidebar-collapsed', String(collapsed));
    } catch (err) {
      // 本地存储不可用时只影响偏好持久化，不影响本次折叠交互。
    }
  }
}

function initSessionSidebar() {
  const listEl = document.getElementById('session-list');
  const newBtn = document.getElementById('nav-new-chat');
  const refreshBtn = document.getElementById('session-refresh-btn');
  const renameBtn = document.getElementById('nav-rename');
  const compactBtn = document.getElementById('nav-compact');
  let currentSessionId = '';
  let currentSessionTitle = '新会话';

  if (!listEl) return;

  // 删除确认弹窗状态
  let deleteTargetId = '';
  let deleteTargetTitle = '';
  const deleteModal = document.getElementById('delete-session-modal');
  const deleteText = document.getElementById('delete-session-text');
  const deleteConfirmBtn = document.getElementById('delete-session-confirm');
  const deleteCancelBtn = document.getElementById('delete-session-cancel');
  const deleteBackdrop = deleteModal ? deleteModal.querySelector('.confirm-modal-backdrop') : null;

  function openDeleteModal(sessionId, title) {
    deleteTargetId = sessionId;
    deleteTargetTitle = title;
    if (deleteText) {
      deleteText.textContent = `确定删除会话「${title}」吗？此操作不可撤销。`;
    }
    if (deleteModal) deleteModal.classList.remove('hidden');
  }

  function closeDeleteModal() {
    deleteTargetId = '';
    deleteTargetTitle = '';
    if (deleteModal) deleteModal.classList.add('hidden');
  }

  function confirmDelete() {
    if (deleteTargetId && window.bridge && window.bridge.onDeleteSession) {
      window.bridge.onDeleteSession(deleteTargetId);
    }
    closeDeleteModal();
  }

  if (deleteConfirmBtn) deleteConfirmBtn.addEventListener('click', confirmDelete);
  if (deleteCancelBtn) deleteCancelBtn.addEventListener('click', closeDeleteModal);
  if (deleteBackdrop) deleteBackdrop.addEventListener('click', closeDeleteModal);
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && deleteModal && !deleteModal.classList.contains('hidden')) {
      closeDeleteModal();
    }
  });

  function requestRefresh() {
    if (window.bridge && window.bridge.onRequestSessions) {
      window.bridge.onRequestSessions();
    }
  }

  function renderEmpty(text) {
    listEl.innerHTML = '';
    const empty = document.createElement('div');
    empty.className = 'session-empty';
    empty.textContent = text;
    listEl.appendChild(empty);
  }

  function updateSessionList(sessions) {
    listEl.innerHTML = '';
    if (!Array.isArray(sessions) || sessions.length === 0) {
      renderEmpty('暂无会话');
      return;
    }
    sessions.forEach((session) => {
      const item = document.createElement('button');
      item.className = 'session-item' + (session.current ? ' active' : '');
      item.dataset.sessionId = session.id || '';
      item.title = session.title || session.id || '未命名会话';
      item.innerHTML = `
        <span class="session-title"></span>
        <span class="session-meta"></span>
        <span class="session-delete" title="删除会话">&times;</span>
      `;
      const titleEl = item.querySelector('.session-title');
      const metaEl = item.querySelector('.session-meta');
      const deleteEl = item.querySelector('.session-delete');
      if (titleEl) titleEl.textContent = session.title || '未命名会话';
      if (metaEl) {
        metaEl.textContent = `${session.updatedAt || ''} · ${session.messageCount || 0} 条`;
      }
      // 点击删除按钮
      if (deleteEl) {
        deleteEl.addEventListener('click', (e) => {
          e.stopPropagation();
          const sid = item.dataset.sessionId;
          const stitle = titleEl ? titleEl.textContent : sid;
          openDeleteModal(sid, stitle);
        });
      }
      // 点击会话条目 → 恢复
      item.addEventListener('click', () => {
        if (window.bridge && window.bridge.onResumeSession && item.dataset.sessionId) {
          window.bridge.onResumeSession(item.dataset.sessionId);
        }
      });
      listEl.appendChild(item);
    });
  }

  function setCurrentSession(sessionId, title) {
    currentSessionId = sessionId || '';
    currentSessionTitle = title || '未命名会话';
    listEl.querySelectorAll('.session-item').forEach((item) => {
      item.classList.toggle('active', item.dataset.sessionId === currentSessionId);
    });
  }

  function showError(message) {
    renderEmpty(message || '会话列表读取失败');
    if (window.Notice && Notice.show) {
      Notice.show(message || '会话列表读取失败');
    }
  }

  if (newBtn) {
    newBtn.addEventListener('click', (event) => {
      event.preventDefault();
      if (window.bridge && window.bridge.onNewSession) {
        window.bridge.onNewSession();
      }
    });
  }

  if (refreshBtn) {
    refreshBtn.addEventListener('click', (event) => {
      event.preventDefault();
      requestRefresh();
    });
  }

  if (renameBtn) {
    renameBtn.addEventListener('click', (event) => {
      event.preventDefault();
      const nextTitle = window.prompt('当前会话标题', currentSessionTitle);
      if (nextTitle && window.bridge && window.bridge.onRenameSession) {
        window.bridge.onRenameSession(nextTitle);
      }
    });
  }

  if (compactBtn) {
    compactBtn.addEventListener('click', (event) => {
      event.preventDefault();
      if (window.bridge && window.bridge.onCompactSession) {
        window.bridge.onCompactSession();
      }
    });
  }

  window.SessionSidebar = {
    requestRefresh: requestRefresh,
    updateSessionList: updateSessionList,
    setCurrentSession: setCurrentSession,
    showError: showError,
  };
}

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
      const provider = normalizeModelProvider(model.provider || model.id);
      const name = model.name || model.id;
      item.innerHTML = `
        ${modelIconMarkup(provider, 'model-icon')}
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

  // 选择模型
  function selectModel(model) {
    currentModel = model.id;
    if (label) label.textContent = model.name;
    setCurrentModelIdentity(model);
    updateSelectorIcon(AppState.currentModelProvider);

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
        if (selected) {
          if (label) label.textContent = selected.name || selected.id;
          setCurrentModelIdentity(selected);
          updateSelectorIcon(AppState.currentModelProvider);
        }
      }
    } else {
      renderMessage('暂无模型');
    }
  }

  function setCurrentModel(modelId, modelName) {
    currentModel = modelId;
    if (label && modelName) label.textContent = modelName;
    setCurrentModelIdentity({ id: modelId, name: modelName || modelId });
    updateSelectorIcon(AppState.currentModelProvider);
    list.querySelectorAll('.model-option').forEach(opt => {
      opt.classList.toggle('selected', opt.dataset.modelId === modelId);
    });
  }

  function updateSelectorIcon(provider) {
    const selectorIcon = document.getElementById('model-selector-icon');
    if (!selectorIcon) return;
    const wrapper = document.createElement('div');
    wrapper.innerHTML = modelIconMarkup(provider || 'other', 'model-icon').trim();
    const icon = wrapper.firstElementChild;
    if (icon) {
      selectorIcon.replaceWith(icon);
      icon.id = 'model-selector-icon';
    }
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
      if (bubble) {
        const role = row.querySelector('.msg-role');
        const name = role ? role.textContent.trim() : AppState.currentModelName;
        md += '**' + name + '**：\n\n' + bubble.textContent.trim() + '\n\n';
      }
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
