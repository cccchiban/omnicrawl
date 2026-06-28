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

  // 0. 代码块复制按钮（事件委托）
  const messagesEl = document.getElementById('messages');
  if (messagesEl) {
    messagesEl.addEventListener('click', (event) => {
      const btn = event.target.closest('.code-copy-btn');
      if (!btn) return;
      const block = btn.closest('.code-block');
      const code = block && block.querySelector('code');
      if (!code) return;
      const text = code.textContent || '';
      const label = btn.querySelector('.code-copy-label');
      function onCopied() {
        if (label) label.textContent = '已复制';
        btn.classList.add('copied');
        setTimeout(() => {
          if (label) label.textContent = '复制';
          btn.classList.remove('copied');
        }, 2000);
      }
      if (navigator.clipboard && navigator.clipboard.writeText) {
        navigator.clipboard.writeText(text).then(onCopied).catch(() => {});
      } else {
        // Qt WebEngine 旧版兼容
        const ta = document.createElement('textarea');
        ta.value = text;
        ta.style.cssText = 'position:fixed;opacity:0;pointer-events:none';
        document.body.appendChild(ta);
        ta.select();
        try { document.execCommand('copy'); onCopied(); } catch (_) {}
        document.body.removeChild(ta);
      }
    });
  }

  // 1. 初始化输入模块（绑定 Enter/Shift+Enter 等事件）
  Input.init();

  // 2. 初始化对话框模块（绑定确认/拒绝按钮事件）
  Dialog.init();

  // 3. 侧边栏折叠
  initSidebarToggle();

  // 4. 无边框窗口顶部栏
  initWindowChrome();

  // 5. 右侧 HTML 显示区
  if (window.HtmlPreview) {
    HtmlPreview.init();
  }
  initNavigationHistory();
  initAppMenuActions();
  initAutomationPanel();
  initSettingsPanel();

  // 6. 停止生成按钮已移至对话流内联显示（status-msg-row）

  const navExport = document.getElementById('nav-export');
  if (navExport) {
    navExport.addEventListener('click', (event) => {
      event.preventDefault();
      exportChat();
    });
  }

  // 7. QWebChannel 连接成功前禁用输入，避免消息发送到尚未就绪的 bridge。
  Input.setBridgeReady(false);
  function handleBridgeReady() {
    Input.setBridgeReady(true);
    if (window.SessionSidebar) {
      SessionSidebar.requestRefresh();
    }
    if (window.ProjectSidebar) {
      ProjectSidebar.requestProjectList();
    }
  }
  window.addEventListener('bridge-ready', () => {
    handleBridgeReady();
  });
  if (window.bridge) {
    handleBridgeReady();
  }

  // 8. 模型选择器 — 下拉菜单交互
  initModelSelector();
  initSessionSidebar();
  if (window.SessionSearch) {
    SessionSearch.init();
  }

  console.log('[app] AI Voice Agent 前端已就绪');
});

function initWindowChrome() {
  const chrome = document.getElementById('app-chrome');
  const minimizeBtn = document.getElementById('window-minimize-btn');
  const maximizeBtn = document.getElementById('window-maximize-btn');
  const closeBtn = document.getElementById('window-close-btn');
  if (!chrome) return;

  chrome.addEventListener('mousedown', (event) => {
    if (
      event.button !== 0 ||
      event.target.closest('[data-no-window-drag]') ||
      event.target.closest('button, a, input, textarea, select')
    ) {
      return;
    }
    if (window.bridge && window.bridge.onWindowDrag) {
      window.bridge.onWindowDrag();
    }
  });

  chrome.addEventListener('dblclick', (event) => {
    if (event.target.closest('[data-no-window-drag]')) return;
    if (window.bridge && window.bridge.onWindowMaximize) {
      window.bridge.onWindowMaximize();
    }
  });

  if (minimizeBtn) {
    minimizeBtn.addEventListener('click', () => {
      if (window.bridge && window.bridge.onWindowMinimize) {
        window.bridge.onWindowMinimize();
      }
    });
  }

  if (maximizeBtn) {
    maximizeBtn.addEventListener('click', () => {
      if (window.bridge && window.bridge.onWindowMaximize) {
        window.bridge.onWindowMaximize();
      }
    });
  }

  if (closeBtn) {
    closeBtn.addEventListener('click', () => {
      if (window.bridge && window.bridge.onWindowClose) {
        window.bridge.onWindowClose();
      }
    });
  }

  window.WindowChrome = {
    setMaximized: function(maximized) {
      if (!maximizeBtn) return;
      maximizeBtn.classList.toggle('is-maximized', Boolean(maximized));
      maximizeBtn.setAttribute('title', maximized ? '还原' : '最大化');
      maximizeBtn.setAttribute('aria-label', maximized ? '还原' : '最大化');
    },
  };
}

function initNavigationHistory() {
  const backBtn = document.getElementById('chrome-back-btn');
  const forwardBtn = document.getElementById('chrome-forward-btn');
  const history = ['chat'];
  let index = 0;
  let applying = false;

  window.AppNavigation = {
    push: function(state) {
      if (applying || !state || history[index] === state) return;
      history.splice(index + 1);
      history.push(state);
      index = history.length - 1;
    },
    apply: applyState,
  };

  if (backBtn) {
    backBtn.addEventListener('click', () => {
      if (index <= 0) {
        Notice.show('已在当前对话');
        return;
      }
      index -= 1;
      applyState(history[index]);
    });
  }

  if (forwardBtn) {
    forwardBtn.addEventListener('click', () => {
      if (index >= history.length - 1) {
        Notice.show('没有可前进的视图');
        return;
      }
      index += 1;
      applyState(history[index]);
    });
  }

  function applyState(state) {
    applying = true;
    closeAutomationPanel();
    closeSettingsPanel();
    if (window.SessionSearch) SessionSearch.close();
    if (state === 'search' && window.SessionSearch) {
      SessionSearch.open();
    } else if (state === 'automation') {
      openAutomationPanel(true);
    } else if (state === 'settings') {
      openSettingsPanel(true);
    } else if (state === 'preview' && window.HtmlPreview) {
      HtmlPreview.toggle();
    } else {
      focusChatInput();
    }
    applying = false;
  }
}

function initAppMenuActions() {
  const aboutBtn = document.getElementById('chrome-about-btn');
  const menuButtons = document.querySelectorAll('.app-menu-item[data-menu-action]');
  let popover = null;

  if (aboutBtn) {
    aboutBtn.addEventListener('click', () => {
      showAboutNotice();
      pushNavigationState('chat');
    });
  }

  menuButtons.forEach((button) => {
    button.addEventListener('click', (event) => {
      event.stopPropagation();
      const action = button.dataset.menuAction || '';
      showAppMenuPopover(button, action);
    });
  });

  document.addEventListener('click', () => hidePopover());

  function showAppMenuPopover(anchor, action) {
    hidePopover();
    const items = menuItemsFor(action);
    popover = document.createElement('div');
    popover.className = 'app-menu-popover';
    items.forEach((item) => {
      if (item.label === '---') {
        const sep = document.createElement('div');
        sep.className = 'context-divider';
        popover.appendChild(sep);
        return;
      }
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'context-item';
      btn.innerHTML = '<span></span>';
      btn.querySelector('span').textContent = item.label;
      btn.addEventListener('click', (event) => {
        event.stopPropagation();
        hidePopover();
        item.run();
      });
      popover.appendChild(btn);
    });
    const rect = anchor.getBoundingClientRect();
    popover.style.left = Math.min(rect.left, window.innerWidth - 230) + 'px';
    popover.style.top = (rect.bottom + 4) + 'px';
    document.body.appendChild(popover);
  }

  function hidePopover() {
    if (popover) {
      popover.remove();
      popover = null;
    }
  }

  function menuItemsFor(action) {
    function pendingItem(label) {
      return {
        label: label,
        run: function() {
          Notice.show('功能待接入：' + label);
        },
      };
    }

    if (action === 'file') {
      return [
        {
          label: '新窗口',
          run: function() {
            if (window.bridge && window.bridge.onOpenNewWindow) {
              window.bridge.onOpenNewWindow();
            } else {
              Notice.show('新窗口功能尚未就绪');
            }
          },
        },
        {
          label: '新聊天',
          run: function() {
            requestNewSession();
            if (window.Input && Input.clear) {
              Input.clear();
            }
            focusChatInput();
            Notice.show('已开启新对话');
          },
        },
        {
          label: '快速聊天',
          run: function() {
            requestNewSession();
            window.setTimeout(function() {
              focusChatInput();
            }, 0);
            Notice.show('已创建新对话并聚焦输入框');
          },
        },
        {
          label: '打开文件夹...',
          run: function() {
            var workspacePath = window.Input && Input.getWorkspacePath ? Input.getWorkspacePath() : '';
            if (window.bridge && window.bridge.onOpenWorkspaceFolder) {
              window.bridge.onOpenWorkspaceFolder(workspacePath);
            } else {
              Notice.show('当前环境不支持打开文件夹');
            }
          },
        },
        { label: '---' },
        {
          label: '关闭',
          run: function() {
            if (window.bridge && window.bridge.onWindowClose) {
              window.bridge.onWindowClose();
            }
          },
        },
        {
          label: '设置...',
          run: function() {
            openSettingsPanel();
          },
        },
        {
          label: '登出',
          run: function() {
            if (window.bridge && window.bridge.onWindowClose) {
              Notice.show('当前版本未接入账号登出，已关闭窗口');
              window.bridge.onWindowClose();
            }
          },
        },
        { label: '---' },
        {
          label: '退出',
          run: function() {
            if (window.bridge && window.bridge.onWindowClose) {
              window.bridge.onWindowClose();
            }
          },
        },
      ];
    }
    if (action === 'edit') {
      return [
        pendingItem('撤销'),
        pendingItem('重做'),
        { label: '---' },
        pendingItem('剪切'),
        pendingItem('复制'),
        pendingItem('粘贴'),
        pendingItem('删除'),
        { label: '---' },
        pendingItem('全选'),
      ];
    }
    if (action === 'view') {
      return [
        pendingItem('切换侧边栏'),
        pendingItem('切换底部面板'),
        pendingItem('打开终端'),
        pendingItem('切换文件树'),
        pendingItem('打开浏览器标签页'),
        pendingItem('重新加载浏览器页面'),
        pendingItem('切换侧面板'),
        { label: '---' },
        pendingItem('查找'),
        pendingItem('上一个聊天'),
        pendingItem('下一个聊天'),
        { label: '---' },
        pendingItem('后退'),
        pendingItem('前进'),
        { label: '---' },
        pendingItem('放大'),
        pendingItem('缩小'),
        pendingItem('实际大小'),
        pendingItem('切换全屏'),
      ];
    }
    if (action === 'help') {
      return [
        pendingItem('文档'),
        pendingItem('新功能'),
        pendingItem('自动化'),
        pendingItem('本地环境'),
        pendingItem('工作树'),
        pendingItem('技能'),
        pendingItem('模型上下文协议'),
        pendingItem('故障排除'),
        { label: '---' },
        pendingItem('发送反馈'),
        pendingItem('开始性能追踪'),
        { label: '---' },
        pendingItem('键盘快捷键'),
      ];
    }
    return [
      pendingItem('暂无菜单项'),
    ];
  }
}

function initSettingsPanel() {
  const navSettings = document.getElementById('nav-settings');
  const closeBtn = document.getElementById('settings-modal-close');
  const backdrop = document.getElementById('settings-modal-backdrop');
  const focusBtn = document.getElementById('settings-focus-input-btn');
  const sidebarBtn = document.getElementById('settings-toggle-sidebar-btn');
  const previewBtn = document.getElementById('settings-toggle-preview-btn');
  const refreshBtn = document.getElementById('settings-refresh-btn');

  if (navSettings) {
    navSettings.addEventListener('click', (event) => {
      event.preventDefault();
      openSettingsPanel();
    });
  }
  if (closeBtn) closeBtn.addEventListener('click', closeSettingsPanel);
  if (backdrop) backdrop.addEventListener('click', closeSettingsPanel);
  if (focusBtn) focusBtn.addEventListener('click', () => {
    closeSettingsPanel();
    focusChatInput();
  });
  if (sidebarBtn) sidebarBtn.addEventListener('click', toggleSidebarFromMenu);
  if (previewBtn) previewBtn.addEventListener('click', toggleHtmlPreview);
  if (refreshBtn) refreshBtn.addEventListener('click', () => {
    requestSidebarRefresh();
    Notice.show('已刷新项目和会话列表');
  });
}

function openSettingsPanel(fromHistory) {
  const modal = document.getElementById('settings-modal');
  if (!modal) return;
  modal.classList.remove('hidden');
  if (!fromHistory) pushNavigationState('settings');
}

function closeSettingsPanel() {
  const modal = document.getElementById('settings-modal');
  if (modal) modal.classList.add('hidden');
}

function initAutomationPanel() {
  const navAutomation = document.getElementById('nav-automation');
  const closeBtn = document.getElementById('automation-modal-close');
  const backdrop = document.getElementById('automation-modal-backdrop');
  const newBtn = document.getElementById('automation-new-btn');
  const saveBtn = document.getElementById('automation-save-btn');
  const runBtn = document.getElementById('automation-run-btn');
  const toggleBtn = document.getElementById('automation-toggle-btn');
  const deleteBtn = document.getElementById('automation-delete-btn');
  const listEl = document.getElementById('automation-task-list');
  const titleInput = document.getElementById('automation-title-input');
  const promptInput = document.getElementById('automation-prompt-input');
  const intervalInput = document.getElementById('automation-interval-input');
  const enabledInput = document.getElementById('automation-enabled-input');
  const STORAGE_KEY = 'automation-tasks';
  let tasks = loadAutomationTasks();
  let selectedTaskId = tasks[0] ? tasks[0].id : '';
  let timers = [];

  window.AutomationPanel = {
    open: openAutomationPanel,
    close: closeAutomationPanel,
    runSelected: function() {
      const task = findSelectedTask();
      if (task) runAutomationTask(task);
    },
  };

  if (navAutomation) {
    navAutomation.addEventListener('click', (event) => {
      event.preventDefault();
      openAutomationPanel();
    });
  }
  if (closeBtn) closeBtn.addEventListener('click', closeAutomationPanel);
  if (backdrop) backdrop.addEventListener('click', closeAutomationPanel);
  if (newBtn) newBtn.addEventListener('click', createDraftTask);
  if (saveBtn) saveBtn.addEventListener('click', saveSelectedTask);
  if (runBtn) runBtn.addEventListener('click', () => {
    const task = saveSelectedTask({ quiet: true });
    if (task) runAutomationTask(task);
  });
  if (toggleBtn) toggleBtn.addEventListener('click', toggleSelectedTask);
  if (deleteBtn) deleteBtn.addEventListener('click', deleteSelectedTask);

  renderAutomationTasks();
  scheduleAutomationTimers();

  function loadAutomationTasks() {
    try {
      const raw = localStorage.getItem(STORAGE_KEY);
      const parsed = raw ? JSON.parse(raw) : [];
      return Array.isArray(parsed) ? parsed.map(normalizeAutomationTask).filter(Boolean) : [];
    } catch (_err) {
      return [];
    }
  }

  function normalizeAutomationTask(task) {
    if (!task || typeof task !== 'object') return null;
    const prompt = String(task.prompt || '').trim();
    const title = String(task.title || '').trim() || '未命名自动化';
    const intervalMinutes = Math.max(1, Number(task.intervalMinutes || 60));
    return {
      id: String(task.id || createTaskId()),
      title: title,
      prompt: prompt,
      intervalMinutes: Number.isFinite(intervalMinutes) ? intervalMinutes : 60,
      enabled: Boolean(task.enabled),
      nextRunAt: Number(task.nextRunAt || 0),
      lastRunAt: Number(task.lastRunAt || 0),
    };
  }

  function saveAutomationTasks() {
    try {
      localStorage.setItem(STORAGE_KEY, JSON.stringify(tasks));
    } catch (_err) {
      Notice.show('自动化保存失败：本地存储不可用');
    }
  }

  function createTaskId() {
    return 'automation-' + Date.now().toString(36) + '-' + Math.random().toString(36).slice(2, 8);
  }

  function renderAutomationTasks() {
    if (!listEl) return;
    listEl.innerHTML = '';
    if (!tasks.length) {
      const empty = document.createElement('div');
      empty.className = 'automation-empty';
      empty.textContent = '暂无自动化任务';
      listEl.appendChild(empty);
      setAutomationForm(null);
      return;
    }

    if (!selectedTaskId || !tasks.some(task => task.id === selectedTaskId)) {
      selectedTaskId = tasks[0].id;
    }
    tasks.forEach((task) => {
      const item = document.createElement('button');
      item.type = 'button';
      item.className = 'automation-task-item' + (task.id === selectedTaskId ? ' active' : '');
      item.dataset.taskId = task.id;
      item.innerHTML = '<span class="automation-task-title"></span><span class="automation-task-meta"></span>';
      item.querySelector('.automation-task-title').textContent = task.title;
      item.querySelector('.automation-task-meta').textContent = task.enabled
        ? '每 ' + task.intervalMinutes + ' 分钟'
        : '已暂停';
      item.addEventListener('click', () => {
        selectedTaskId = task.id;
        renderAutomationTasks();
      });
      listEl.appendChild(item);
    });
    setAutomationForm(findSelectedTask());
  }

  function setAutomationForm(task) {
    const hasTask = Boolean(task);
    if (titleInput) titleInput.value = task ? task.title : '';
    if (promptInput) promptInput.value = task ? task.prompt : '';
    if (intervalInput) intervalInput.value = task ? String(task.intervalMinutes) : '60';
    if (enabledInput) enabledInput.value = task && task.enabled ? 'true' : 'false';
    [saveBtn, runBtn, toggleBtn, deleteBtn].forEach((button) => {
      if (button) button.disabled = !hasTask;
    });
    if (toggleBtn && task) toggleBtn.textContent = task.enabled ? '暂停' : '启用';
  }

  function findSelectedTask() {
    return tasks.find(task => task.id === selectedTaskId) || null;
  }

  function createDraftTask() {
    const task = {
      id: createTaskId(),
      title: '新的自动化',
      prompt: '',
      intervalMinutes: 60,
      enabled: false,
      nextRunAt: 0,
      lastRunAt: 0,
    };
    tasks.unshift(task);
    selectedTaskId = task.id;
    saveAutomationTasks();
    renderAutomationTasks();
    if (titleInput) titleInput.focus();
  }

  function saveSelectedTask(options) {
    const task = findSelectedTask();
    if (!task) return null;
    const title = titleInput ? titleInput.value.trim() : '';
    const prompt = promptInput ? promptInput.value.trim() : '';
    const intervalMinutes = intervalInput ? Number(intervalInput.value) : 60;
    const enabled = enabledInput ? enabledInput.value === 'true' : false;
    if (!title) {
      Notice.show('请输入自动化名称');
      if (titleInput) titleInput.focus();
      return null;
    }
    if (!prompt) {
      Notice.show('请输入要运行的聊天内容');
      if (promptInput) promptInput.focus();
      return null;
    }
    if (!Number.isFinite(intervalMinutes) || intervalMinutes < 1) {
      Notice.show('间隔分钟必须大于 0');
      if (intervalInput) intervalInput.focus();
      return null;
    }

    task.title = title;
    task.prompt = prompt;
    task.intervalMinutes = Math.floor(intervalMinutes);
    task.enabled = enabled;
    if (task.enabled && task.nextRunAt <= Date.now()) {
      task.nextRunAt = Date.now() + task.intervalMinutes * 60 * 1000;
    }
    saveAutomationTasks();
    renderAutomationTasks();
    scheduleAutomationTimers();
    if (!options || !options.quiet) Notice.show('自动化任务已保存');
    return task;
  }

  function toggleSelectedTask() {
    const task = saveSelectedTask({ quiet: true });
    if (!task) return;
    task.enabled = !task.enabled;
    task.nextRunAt = task.enabled ? Date.now() + task.intervalMinutes * 60 * 1000 : 0;
    if (enabledInput) enabledInput.value = task.enabled ? 'true' : 'false';
    saveAutomationTasks();
    renderAutomationTasks();
    scheduleAutomationTimers();
    Notice.show(task.enabled ? '自动化已启用' : '自动化已暂停');
  }

  function deleteSelectedTask() {
    const task = findSelectedTask();
    if (!task) return;
    if (!window.confirm('确定删除此自动化任务吗？')) return;
    tasks = tasks.filter(item => item.id !== task.id);
    selectedTaskId = tasks[0] ? tasks[0].id : '';
    saveAutomationTasks();
    renderAutomationTasks();
    scheduleAutomationTimers();
    Notice.show('自动化任务已删除');
  }

  function runAutomationTask(task) {
    if (!task || !task.prompt) return;
    if (!(window.Input && Input.isReadyForProgrammaticSend && Input.isReadyForProgrammaticSend())) {
      Notice.show('Agent 忙碌中，自动化任务稍后再试');
      return;
    }
    if (!(window.bridge && window.bridge.onUserSend)) {
      Notice.show('界面通信尚未就绪，请稍后重试');
      return;
    }
    const main = document.getElementById('main');
    if (main && main.classList.contains('empty-state')) {
      main.classList.remove('empty-state');
    }
    Messages.appendUserMsg(task.prompt);
    window.bridge.onUserSend(task.prompt);
    task.lastRunAt = Date.now();
    task.nextRunAt = task.enabled ? Date.now() + task.intervalMinutes * 60 * 1000 : 0;
    saveAutomationTasks();
    renderAutomationTasks();
    scheduleAutomationTimers();
    closeAutomationPanel();
    Notice.show('已运行自动化：' + task.title);
  }

  function scheduleAutomationTimers() {
    timers.forEach(timer => clearTimeout(timer));
    timers = [];
    const now = Date.now();
    tasks.forEach((task) => {
      if (!task.enabled || !task.prompt) return;
      if (!task.nextRunAt || task.nextRunAt < now) {
        task.nextRunAt = now + task.intervalMinutes * 60 * 1000;
      }
      const delay = Math.max(1000, task.nextRunAt - now);
      timers.push(setTimeout(() => runAutomationTask(task), delay));
    });
    saveAutomationTasks();
  }
}

function openAutomationPanel(fromHistory) {
  const modal = document.getElementById('automation-modal');
  if (!modal) return;
  closeSettingsPanel();
  modal.classList.remove('hidden');
  if (!fromHistory) pushNavigationState('automation');
}

function closeAutomationPanel() {
  const modal = document.getElementById('automation-modal');
  if (modal) modal.classList.add('hidden');
}

function pushNavigationState(state) {
  if (window.AppNavigation) {
    window.AppNavigation.push(state);
  }
}

function requestNewSession() {
  if (window.bridge && window.bridge.onNewSession) {
    window.bridge.onNewSession();
  }
}

function requestSidebarRefresh() {
  if (window.SessionSidebar) SessionSidebar.requestRefresh();
  if (window.ProjectSidebar) ProjectSidebar.requestProjectList();
}

function focusChatInput() {
  if (window.Input && Input.focus) {
    Input.focus();
  }
}

function toggleSidebarFromMenu() {
  const toggle = document.getElementById('sidebar-toggle');
  if (toggle) toggle.click();
}

function toggleHtmlPreview() {
  if (window.HtmlPreview && HtmlPreview.toggle) {
    HtmlPreview.toggle();
    pushNavigationState('preview');
  }
}

function showAboutNotice() {
  Notice.show('AI Voice Agent 已就绪');
}

function buildChatMarkdown() {
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
  return md;
}

function copyCurrentChatMarkdown() {
  const md = buildChatMarkdown();
  if (!navigator.clipboard || !navigator.clipboard.writeText) {
    Notice.show('当前环境不支持剪贴板复制');
    return;
  }
  navigator.clipboard.writeText(md).then(() => {
    Notice.show('对话内容已复制到剪贴板');
  }).catch(() => {
    Notice.show('复制失败，请检查剪贴板权限');
  });
}

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
  const DELETE_UNDO_DELAY_MS = 5000;
  const listEl = document.getElementById('session-list');
  const newBtn = document.getElementById('nav-new-chat');
  const refreshBtn = document.getElementById('session-refresh-btn');
  const renameBtn = document.getElementById('nav-rename');
  const compactBtn = document.getElementById('nav-compact');
  const deleteUndoBar = document.getElementById('delete-undo-bar');
  const deleteUndoText = document.getElementById('delete-undo-text');
  const deleteUndoAction = document.getElementById('delete-undo-action');
  let currentSessionId = '';
  let currentSessionTitle = '新会话';
  let pendingDelete = null;

  if (!listEl) return;

  function hideDeleteUndoBar() {
    if (deleteUndoBar) deleteUndoBar.classList.add('hidden');
  }

  function commitPendingDelete() {
    const target = pendingDelete;
    if (!target) return;
    clearTimeout(target.timer);
    pendingDelete = null;
    hideDeleteUndoBar();
    if (window.bridge && window.bridge.onDeleteSession) {
      window.bridge.onDeleteSession(target.sessionId);
    }
  }

  function undoPendingDelete() {
    if (!pendingDelete) return;
    clearTimeout(pendingDelete.timer);
    pendingDelete = null;
    hideDeleteUndoBar();
  }

  function scheduleDelete(sessionId, title) {
    if (!sessionId) return;
    if (pendingDelete) {
      // 同一个会话再次点击 → 撤销删除（toggle）
      if (pendingDelete.sessionId === sessionId) {
        undoPendingDelete();
        return;
      }
      // 不同会话 → 先撤销上一个待删除，再开始新的
      undoPendingDelete();
    }
    const target = {
      sessionId: sessionId,
      title: title || sessionId,
      timer: null,
    };
    target.timer = setTimeout(() => {
      if (pendingDelete === target) {
        commitPendingDelete();
      }
    }, DELETE_UNDO_DELAY_MS);
    pendingDelete = target;
    if (deleteUndoText) {
      deleteUndoText.textContent = `会话「${target.title}」将在 5 秒后删除`;
    }
    if (deleteUndoBar) deleteUndoBar.classList.remove('hidden');
  }

  if (deleteUndoAction) deleteUndoAction.addEventListener('click', undoPendingDelete);

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
          // 延迟 300ms 判断是否为双击
          if (deleteEl._dblFired) { deleteEl._dblFired = false; return; }
          deleteEl._clickTimer = setTimeout(() => {
            deleteEl._clickTimer = null;
            const sid = item.dataset.sessionId;
            const stitle = titleEl ? titleEl.textContent : sid;
            scheduleDelete(sid, stitle);
          }, 300);
        });

        // 双击删除按钮 → 立即删除，跳过 5 秒倒计时
        deleteEl.addEventListener('dblclick', (e) => {
          e.stopPropagation();
          deleteEl._dblFired = true;
          if (deleteEl._clickTimer) { clearTimeout(deleteEl._clickTimer); deleteEl._clickTimer = null; }
          const sid = item.dataset.sessionId;
          if (pendingDelete && pendingDelete.sessionId === sid) {
            // 已有待删除 → 立即确认
            commitPendingDelete();
          } else {
            // 无待删除或无该会话待删除 → 取消其他待删除，直接调用 backend 删除
            if (pendingDelete) { undoPendingDelete(); }
            if (window.bridge && window.bridge.onDeleteSession) {
              window.bridge.onDeleteSession(sid);
            }
          }
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
  const md = buildChatMarkdown();

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
