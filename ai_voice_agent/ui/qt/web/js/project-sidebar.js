/**
 * project-sidebar.js — 项目侧边栏模块
 *
 * 职责：项目列表渲染、展开折叠、右键菜单、创建/导入/打开项目。
 */
'use strict';

(function() {
  // ── 内部状态 ────────────────────────────────────
  let projects = [];
  let currentProjectPath = '';
  let expandedProjects = new Set();
  let contextMenuProject = null;
  let confirmCallback = null;

  // ── DOM 引用 ────────────────────────────────────
  let projectListEl = null;
  let contextMenuEl = null;
  let modalEl = null;
  let modalTitleEl = null;
  let modalNameInput = null;
  let modalPathInput = null;
  let modalConfirmBtn = null;
  let modalMode = 'create'; // 'create' | 'import'
  let globalMenuEl = null;

  // ── 初始化 ──────────────────────────────────────
  function init() {
    projectListEl = document.getElementById('project-list');
    contextMenuEl = document.getElementById('project-context-menu');
    modalEl = document.getElementById('project-modal');
    modalTitleEl = document.getElementById('project-modal-title');
    modalNameInput = document.getElementById('project-name-input');
    modalPathInput = document.getElementById('project-path-input');
    modalConfirmBtn = document.getElementById('project-modal-confirm');

    if (!projectListEl) return;

    bindEvents();
    requestProjectList();
  }

  function bindEvents() {
    // 创建/导入按钮
    const createBtn = document.getElementById('project-create-btn');
    if (createBtn) {
      createBtn.addEventListener('click', (e) => {
        e.stopPropagation();
        openModal('create');
      });
    }

    // 更多按钮
    const moreBtn = document.getElementById('project-more-btn');
    if (moreBtn) {
      moreBtn.addEventListener('click', (e) => {
        e.stopPropagation();
        showGlobalMenu(e);
      });
    }

    // 模态框事件
    const modalClose = document.getElementById('project-modal-close');
    const modalCancel = document.getElementById('project-modal-cancel');
    const modalBackdrop = document.querySelector('.project-modal-backdrop');
    const browseBtn = document.getElementById('project-browse-btn');

    if (modalClose) modalClose.addEventListener('click', closeModal);
    if (modalCancel) modalCancel.addEventListener('click', closeModal);
    if (modalBackdrop) modalBackdrop.addEventListener('click', closeModal);
    if (browseBtn) browseBtn.addEventListener('click', browseProjectPath);
    if (modalConfirmBtn) modalConfirmBtn.addEventListener('click', confirmModal);

    // 上下文菜单事件
    document.addEventListener('click', hideContextMenu);
    if (contextMenuEl) {
      contextMenuEl.querySelectorAll('.context-item').forEach(item => {
        item.addEventListener('click', handleContextAction);
      });
    }
  }

  // ── 项目列表渲染 ────────────────────────────────
  function renderProjects(projectData) {
    if (!projectListEl) return;
    projects = projectData || [];

    if (projects.length === 0) {
      projectListEl.innerHTML = '<div class="session-empty">暂无项目</div>';
      return;
    }

    projectListEl.innerHTML = '';
    projects.forEach(project => {
      const item = createProjectItem(project);
      projectListEl.appendChild(item);
    });
  }

  function createProjectItem(project) {
    const isExpanded = expandedProjects.has(project.path);
    const isActive = project.path === currentProjectPath;

    const item = document.createElement('div');
    item.className = 'project-item' + (isExpanded ? ' expanded' : '');
    item.dataset.projectPath = project.path;

    const header = document.createElement('div');
    header.className = 'project-header' + (isActive ? ' active' : '');
    header.innerHTML = `
      <span class="project-toggle">
        <svg viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg"><path d="M9 18l6-6-6-6"/></svg>
      </span>
      <span class="project-icon">
        <svg viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/></svg>
      </span>
      <span class="project-name"></span>
      <button class="project-more" title="更多操作" aria-label="更多操作">
        <svg viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg"><circle cx="12" cy="6" r="1.5"/><circle cx="12" cy="12" r="1.5"/><circle cx="12" cy="18" r="1.5"/></svg>
      </button>
    `;

    const nameEl = header.querySelector('.project-name');
    if (nameEl) nameEl.textContent = project.name || '未命名项目';

    // 点击项目展开/折叠
    header.addEventListener('click', (e) => {
      if (e.target.closest('.project-more')) return;
      toggleProject(project.path);
    });

    // 更多按钮 → 上下文菜单
    const moreBtn = header.querySelector('.project-more');
    if (moreBtn) {
      moreBtn.addEventListener('click', (e) => {
        e.stopPropagation();
        showContextMenu(e, project);
      });
    }

    item.appendChild(header);

    // 会话列表
    const sessionsContainer = document.createElement('div');
    sessionsContainer.className = 'project-sessions';
    if (project.sessions && project.sessions.length > 0) {
      project.sessions.forEach(session => {
        const sessionEl = createSessionItem(session, project.path);
        sessionsContainer.appendChild(sessionEl);
      });
    }
    item.appendChild(sessionsContainer);

    return item;
  }

  function createSessionItem(session, projectPath) {
    const el = document.createElement('div');
    el.className = 'project-session-item' + (session.current ? ' active' : '');
    el.dataset.sessionId = session.id || '';
    el.innerHTML = `
      <span class="project-session-title"></span>
      <span class="project-session-time"></span>
    `;

    const titleEl = el.querySelector('.project-session-title');
    const timeEl = el.querySelector('.project-session-time');
    if (titleEl) titleEl.textContent = session.title || '未命名会话';
    if (timeEl) timeEl.textContent = session.timeAgo || '';

    el.addEventListener('click', () => {
      if (window.bridge && window.bridge.onResumeSession) {
        window.bridge.onResumeSession(session.id);
      }
    });

    return el;
  }

  // ── 项目展开/折叠 ─────────────────────────────
  function toggleProject(projectPath) {
    if (expandedProjects.has(projectPath)) {
      expandedProjects.delete(projectPath);
    } else {
      expandedProjects.add(projectPath);
    }
    // 只重新渲染展开状态，不重新创建整个 DOM
    const item = projectListEl.querySelector(`[data-project-path="${CSS.escape(projectPath)}"]`);
    if (item) {
      item.classList.toggle('expanded', expandedProjects.has(projectPath));
    }
  }

  // ── 上下文菜单 ──────────────────────────────────
  function showContextMenu(event, project) {
    if (!contextMenuEl) return;
    event.stopPropagation();
    contextMenuProject = project;
    const pinLabel = contextMenuEl.querySelector('[data-action="pin"] span');
    if (pinLabel) {
      pinLabel.textContent = project.pinned ? '取消置顶' : '置顶项目';
    }

    const rect = event.target.getBoundingClientRect();
    contextMenuEl.style.left = rect.left + 'px';
    contextMenuEl.style.top = (rect.bottom + 4) + 'px';
    contextMenuEl.classList.remove('hidden');
  }

  function showGlobalMenu(event) {
    event.stopPropagation();
    if (globalMenuEl) {
      hideGlobalMenu();
      return;
    }

    globalMenuEl = document.createElement('div');
    globalMenuEl.className = 'project-context-menu';
    globalMenuEl.innerHTML = `
      <button class="context-item" data-action="create">
        <svg viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg"><path d="M12 4v16m8-8H4"/></svg>
        <span>创建项目</span>
      </button>
      <button class="context-item" data-action="import">
        <svg viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg"><path d="M12 3v12m0 0 4-4m-4 4-4-4M5 21h14"/></svg>
        <span>导入项目</span>
      </button>
    `;
    const rect = event.currentTarget.getBoundingClientRect();
    globalMenuEl.style.left = Math.min(rect.left, window.innerWidth - 210) + 'px';
    globalMenuEl.style.top = (rect.bottom + 4) + 'px';
    document.body.appendChild(globalMenuEl);
    const createItem = globalMenuEl.querySelector('[data-action="create"]');
    const importItem = globalMenuEl.querySelector('[data-action="import"]');
    if (createItem) {
      createItem.addEventListener('click', () => {
        hideGlobalMenu();
        openModal('create');
      });
    }
    if (importItem) {
      importItem.addEventListener('click', () => {
        hideGlobalMenu();
        openModal('import');
      });
    }
  }

  function hideContextMenu() {
    if (contextMenuEl) {
      contextMenuEl.classList.add('hidden');
    }
    contextMenuProject = null;
    hideGlobalMenu();
  }

  function hideGlobalMenu() {
    if (globalMenuEl) {
      globalMenuEl.remove();
      globalMenuEl = null;
    }
  }

  function handleContextAction(event) {
    const action = event.currentTarget.dataset.action;
    if (!contextMenuProject) return;

    switch (action) {
      case 'pin':
        pinProject(contextMenuProject.path);
        break;
      case 'open-explorer':
        openInExplorer(contextMenuProject.path);
        break;
      case 'rename':
        renameProject(contextMenuProject.path);
        break;
      case 'remove':
        removeProject(contextMenuProject.path);
        break;
    }
    hideContextMenu();
  }

  // ── 项目操作 ──────────────────────────────────
  function pinProject(projectPath) {
    if (window.bridge && window.bridge.onPinProject) {
      window.bridge.onPinProject(projectPath);
    }
  }

  function openInExplorer(projectPath) {
    if (window.bridge && window.bridge.onOpenInExplorer) {
      window.bridge.onOpenInExplorer(projectPath);
    }
  }

  function renameProject(projectPath) {
    const newName = window.prompt('重命名项目', '');
    if (newName && window.bridge && window.bridge.onRenameProject) {
      window.bridge.onRenameProject(projectPath, newName);
    }
  }

  function removeProject(projectPath) {
    openConfirmModal({
      title: '移除项目',
      text: '确定要移除此项目吗？项目文件不会被删除。',
      confirmText: '移除',
      danger: true,
      onConfirm: () => {
        if (window.bridge && window.bridge.onRemoveProject) {
          window.bridge.onRemoveProject(projectPath);
        }
      }
    });
  }

  // ── 模态框 ──────────────────────────────────────
  function openConfirmModal(options) {
    const confirmModal = document.getElementById('project-confirm-modal');
    const confirmTitle = confirmModal ? confirmModal.querySelector('.confirm-modal-header h3') : null;
    const confirmText = document.getElementById('project-confirm-text');
    const confirmBtn = document.getElementById('project-confirm-confirm');
    const cancelBtn = document.getElementById('project-confirm-cancel');
    const backdrop = confirmModal ? confirmModal.querySelector('.confirm-modal-backdrop') : null;

    if (confirmTitle) confirmTitle.textContent = options.title || '确认';
    if (confirmText) confirmText.textContent = options.text || '';
    if (confirmBtn) {
      confirmBtn.textContent = options.confirmText || '确定';
      confirmBtn.className = 'confirm-btn ' + (options.danger ? 'confirm-btn-danger' : 'confirm-btn-primary');
    }

    confirmCallback = options.onConfirm;

    const handleConfirm = () => {
      if (confirmCallback) confirmCallback();
      closeConfirmModal();
    };
    const handleCancel = () => {
      closeConfirmModal();
    };
    const handleBackdrop = () => {
      closeConfirmModal();
    };
    const handleKey = (e) => {
      if (e.key === 'Escape') closeConfirmModal();
    };

    if (confirmBtn) {
      confirmBtn.onclick = handleConfirm;
    }
    if (cancelBtn) {
      cancelBtn.onclick = handleCancel;
    }
    if (backdrop) {
      backdrop.onclick = handleBackdrop;
    }
    document.addEventListener('keydown', handleKey);

    function closeConfirmModal() {
      if (confirmModal) confirmModal.classList.add('hidden');
      confirmCallback = null;
      document.removeEventListener('keydown', handleKey);
    }

    if (confirmModal) confirmModal.classList.remove('hidden');
  }

  function openModal(mode) {
    modalMode = mode;
    if (modalTitleEl) {
      modalTitleEl.textContent = mode === 'create' ? '创建项目' : '导入项目';
    }
    if (modalNameInput) modalNameInput.value = '';
    if (modalPathInput) {
      modalPathInput.value = '';
      modalPathInput.readOnly = false;
      modalPathInput.placeholder = mode === 'create'
        ? '留空则在当前工作区下创建...'
        : '输入已有项目文件夹路径...';
    }
    if (modalEl) modalEl.classList.remove('hidden');
  }

  function closeModal() {
    if (modalEl) modalEl.classList.add('hidden');
  }

  function browseProjectPath() {
    if (window.bridge && window.bridge.onBrowseProjectPath) {
      window.bridge.onBrowseProjectPath();
    } else {
      // Fallback: 简单的路径输入
      const path = window.prompt('请输入项目路径：');
      if (path && modalPathInput) {
        modalPathInput.value = path;
      }
    }
  }

  function confirmModal() {
    const name = modalNameInput ? modalNameInput.value.trim() : '';
    const path = modalPathInput ? modalPathInput.value.trim() : '';

    if (!name) {
      alert('请输入项目名称');
      return;
    }
    if (modalMode === 'import' && !path) {
      alert('请输入已有项目路径');
      return;
    }

    if (modalMode === 'create') {
      if (window.bridge && window.bridge.onCreateProject) {
        window.bridge.onCreateProject(name, path);
      }
    } else {
      if (window.bridge && window.bridge.onImportProject) {
        window.bridge.onImportProject(name, path);
      }
    }

    closeModal();
  }

  // ── 与 Python 通信 ─────────────────────────────
  function requestProjectList() {
    if (window.bridge && window.bridge.onRequestProjects) {
      window.bridge.onRequestProjects();
    }
  }

  function setCurrentProject(projectPath) {
    currentProjectPath = projectPath;
    renderProjects(projects);
  }

  function updateProjectList(projectData) {
    renderProjects(projectData);
  }

  // ── 暴露全局接口 ────────────────────────────────
  window.ProjectSidebar = {
    init: init,
    updateProjectList: updateProjectList,
    setCurrentProject: setCurrentProject,
    requestProjectList: requestProjectList,
  };

  // DOM 就绪后自动初始化
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
})();
