/**
 * search-dialog.js — 侧边栏对话搜索。
 *
 * 该模块只消费前端已经拿到的会话/项目侧栏数据，不额外读取文件。
 * 这样搜索入口和现有会话恢复逻辑保持一致：点击结果仍通过
 * bridge.onResumeSession(sessionId) 交给 Python 端完成恢复与错误处理。
 */
'use strict';

(function() {
  const MAX_RESULTS = 30;
  let modalEl = null;
  let inputEl = null;
  let resultsEl = null;
  let countEl = null;
  let sectionLabelEl = null;
  let cachedSessions = [];
  let activeIndex = -1;

  function init() {
    modalEl = document.getElementById('session-search-modal');
    inputEl = document.getElementById('session-search-input');
    resultsEl = document.getElementById('session-search-results');
    countEl = document.getElementById('session-search-count');
    sectionLabelEl = document.getElementById('session-search-section-label');

    const searchBtn = document.getElementById('nav-search');
    const backdrop = document.getElementById('session-search-backdrop');

    if (!modalEl || !inputEl || !resultsEl || !searchBtn) return;

    searchBtn.addEventListener('click', (event) => {
      event.preventDefault();
      open();
    });

    if (backdrop) backdrop.addEventListener('click', close);
    modalEl.addEventListener('mousedown', (event) => {
      if (event.target === modalEl) {
        close();
      }
    });
    document.addEventListener('mousedown', handleDocumentMouseDown, true);
    inputEl.addEventListener('input', render);
    modalEl.addEventListener('keydown', handleKeyDown);
  }

  function updateSessionList(sessions) {
    const normalized = Array.isArray(sessions)
      ? sessions.map((session) => normalizeSession(session, '当前项目', ''))
      : [];
    mergeSessions(normalized);
  }

  function updateProjectList(projects) {
    const sessions = [];
    if (Array.isArray(projects)) {
      projects.forEach((project) => {
        const projectName = project && (project.name || project.path) ? String(project.name || project.path) : '未命名项目';
        const projectPath = project && project.path ? String(project.path) : '';
        const projectSessions = Array.isArray(project && project.sessions) ? project.sessions : [];
        projectSessions.forEach((session) => {
          sessions.push(normalizeSession(session, projectName, projectPath));
        });
      });
    }
    mergeSessions(sessions);
  }

  function normalizeSession(session, projectName, projectPath) {
    const id = session && session.id ? String(session.id) : '';
    const title = session && session.title ? String(session.title) : '未命名会话';
    const updatedAt = session && session.updatedAt ? String(session.updatedAt) : '';
    const timeAgo = session && session.timeAgo ? String(session.timeAgo) : '';
    const messageCount = Number(session && session.messageCount ? session.messageCount : 0);
    return {
      id: id,
      title: title,
      projectName: projectName || '当前项目',
      projectPath: projectPath || '',
      updatedAt: updatedAt,
      timeAgo: timeAgo,
      messageCount: Number.isFinite(messageCount) ? messageCount : 0,
      current: Boolean(session && session.current),
      searchText: `${title} ${projectName || ''} ${projectPath || ''} ${updatedAt} ${timeAgo}`.toLowerCase(),
    };
  }

  function mergeSessions(nextSessions) {
    const byId = new Map();
    cachedSessions.forEach((session) => {
      if (session.id) byId.set(session.id, session);
    });
    nextSessions.forEach((session) => {
      if (!session.id) return;
      const previous = byId.get(session.id) || {};
      byId.set(session.id, Object.assign({}, previous, session));
    });
    cachedSessions = Array.from(byId.values()).sort(compareSessions);
    if (isOpen()) render();
  }

  function setCurrentSession(sessionId) {
    cachedSessions = cachedSessions.map((session) => Object.assign({}, session, {
      current: session.id === sessionId,
    }));
    if (isOpen()) render();
  }

  function open() {
    if (!modalEl || !inputEl) return;
    modalEl.classList.remove('hidden');
    inputEl.value = '';
    render();
    window.setTimeout(() => inputEl.focus(), 0);
  }

  function close() {
    if (!modalEl) return;
    modalEl.classList.add('hidden');
    activeIndex = -1;
  }

  function isOpen() {
    return modalEl && !modalEl.classList.contains('hidden');
  }

  function render() {
    if (!resultsEl || !inputEl) return;

    const query = inputEl.value.trim().toLowerCase();
    const matched = query
      ? cachedSessions.filter((session) => session.searchText.indexOf(query) !== -1)
      : cachedSessions;
    const visible = matched.slice(0, MAX_RESULTS);

    if (sectionLabelEl) sectionLabelEl.textContent = query ? '搜索结果' : '近期对话';
    if (countEl) countEl.textContent = visible.length ? `${visible.length} 项` : '';
    resultsEl.innerHTML = '';
    activeIndex = visible.length ? Math.min(Math.max(activeIndex, 0), visible.length - 1) : -1;

    if (!visible.length) {
      const empty = document.createElement('div');
      empty.className = 'session-search-empty';
      empty.textContent = query ? '没有找到匹配对话' : '暂无可搜索对话';
      resultsEl.appendChild(empty);
      return;
    }

    visible.forEach((session, index) => {
      const item = document.createElement('button');
      item.type = 'button';
      item.className = 'session-search-result' + (index === activeIndex ? ' active' : '');
      item.dataset.sessionId = session.id;
      item.setAttribute('role', 'option');
      item.innerHTML = `
        <span class="session-search-result-title"></span>
        <span class="session-search-result-project"></span>
        <span class="session-search-result-time"></span>
      `;

      const titleEl = item.querySelector('.session-search-result-title');
      const projectEl = item.querySelector('.session-search-result-project');
      const timeEl = item.querySelector('.session-search-result-time');
      if (titleEl) titleEl.textContent = session.title;
      if (projectEl) projectEl.textContent = session.projectName;
      if (timeEl) timeEl.textContent = session.timeAgo || session.updatedAt || `${session.messageCount} 条`;

      item.addEventListener('mouseenter', () => {
        activeIndex = index;
        updateActiveItem();
      });
      item.addEventListener('click', () => resumeSession(session.id));
      resultsEl.appendChild(item);
    });
  }

  function handleKeyDown(event) {
    if (event.key === 'Escape') {
      event.preventDefault();
      close();
      return;
    }
    if (event.key !== 'ArrowDown' && event.key !== 'ArrowUp' && event.key !== 'Enter') {
      return;
    }

    const items = getResultItems();
    if (!items.length) return;

    if (event.key === 'ArrowDown') {
      event.preventDefault();
      activeIndex = (activeIndex + 1) % items.length;
      updateActiveItem();
      return;
    }
    if (event.key === 'ArrowUp') {
      event.preventDefault();
      activeIndex = (activeIndex - 1 + items.length) % items.length;
      updateActiveItem();
      return;
    }
    if (event.key === 'Enter') {
      event.preventDefault();
      const item = items[Math.max(activeIndex, 0)];
      if (item && item.dataset.sessionId) resumeSession(item.dataset.sessionId);
    }
  }

  function handleDocumentMouseDown(event) {
    if (!isOpen() || !modalEl) return;
    const panel = modalEl.querySelector('.session-search-panel');
    if (panel && !panel.contains(event.target)) {
      close();
    }
  }

  function updateActiveItem() {
    const items = getResultItems();
    items.forEach((item, index) => {
      const active = index === activeIndex;
      item.classList.toggle('active', active);
      if (active) item.scrollIntoView({ block: 'nearest' });
    });
  }

  function getResultItems() {
    if (!resultsEl) return [];
    return Array.from(resultsEl.querySelectorAll('.session-search-result'));
  }

  function resumeSession(sessionId) {
    if (!sessionId) return;
    close();
    if (window.bridge && window.bridge.onResumeSession) {
      window.bridge.onResumeSession(sessionId);
    }
  }

  function compareSessions(a, b) {
    if (a.current !== b.current) return a.current ? -1 : 1;
    return String(b.updatedAt || b.timeAgo || '').localeCompare(String(a.updatedAt || a.timeAgo || ''));
  }

  window.SessionSearch = {
    init: init,
    updateSessionList: updateSessionList,
    updateProjectList: updateProjectList,
    setCurrentSession: setCurrentSession,
    open: open,
    close: close,
  };
})();
