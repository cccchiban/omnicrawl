/**
 * input.js — 输入框处理（发送按钮 + reasoning_effort + approval + 项目下拉）
 */
'use strict';

var Input = (function() {

  var bridgeReady = false;
  var desiredEnabled = true;
  var currentReasoning = 'none';
  var currentApproval = 'auto'; // manual / auto / review
  var slashCommands = defaultSlashCommands();
  var slashMatches = [];
  var slashSelectedIndex = 0;
  var slashMenuOpen = false;

  function inputEl() {
    return document.getElementById('chat-input');
  }

  function sendBtnEl() {
    return document.getElementById('send-btn');
  }

  function init() {
    var inp = inputEl();
    var btn = sendBtnEl();
    if (!inp || !btn) return;

    btn.addEventListener('click', send);

    inp.addEventListener('keydown', function(e) {
      if (handleSlashCommandKeydown(e, inp)) {
        return;
      }
      if (e.key === 'Enter' && !e.shiftKey) {
        e.preventDefault();
        send();
      }
    });

    inp.addEventListener('input', function() {
      autosize(inp);
      updateSendButtonState(inp);
      updateInputContainerState(inp);
      updateSlashCommandMenu(inp);
    });

    inp.addEventListener('blur', function() {
      window.setTimeout(function() { closeSlashCommandMenu(); }, 120);
    });

    // 初始状态
    updateSendButtonState(inp);
    initReasoningDropdown();
    initApprovalDropdown();
    initProjectDropdown();
    initAttachmentButton();
    initRippleEffect();
    syncToolbarLabels();
  }

  function autosize(inp) {
    var styles = getComputedStyle(document.documentElement);
    var maxHeight = parseInt(styles.getPropertyValue('--input-max-height'), 10) || 200;
    inp.style.height = 'auto';
    inp.style.height = Math.min(inp.scrollHeight, maxHeight) + 'px';
  }

  function resetHeight(inp) {
    inp.style.height = 'var(--input-min-height)';
  }

  function send() {
    var inp = inputEl();
    if (!inp) return;
    var text = inp.value.trim();
    if (!text) return;
    if (!(window.bridge && window.bridge.onUserSend)) {
      Notice.show('界面通信尚未就绪，请稍后重试');
      return;
    }
    // 退出空状态
    var main = document.getElementById('main');
    if (main && main.classList.contains('empty-state')) {
      main.classList.remove('empty-state');
    }
    inp.value = '';
    resetHeight(inp);
    updateSendButtonState(inp);
    updateInputContainerState(inp);
    closeSlashCommandMenu();
    Messages.appendUserMsg(text);
    window.bridge.onUserSend(text);
  }

  function clear() {
    var el = inputEl();
    if (el) {
      el.value = '';
      resetHeight(el);
      updateSendButtonState(el);
      updateInputContainerState(el);
      closeSlashCommandMenu();
    }
  }

  function focus() {
    var el = inputEl();
    if (el && !el.disabled) {
      el.focus();
    }
  }

  function insertText(text) {
    var el = inputEl();
    if (!el) return;
    var value = String(text || '');
    var start = el.selectionStart || 0;
    var end = el.selectionEnd || start;
    var before = el.value.slice(0, start);
    var after = el.value.slice(end);
    var prefix = before && !/\s$/.test(before) && value && !/^\s/.test(value) ? ' ' : '';
    var suffix = after && value && !/\s$/.test(value) && !/^\s/.test(after) ? ' ' : '';
    el.value = before + prefix + value + suffix + after;
    var cursor = (before + prefix + value + suffix).length;
    el.setSelectionRange(cursor, cursor);
    autosize(el);
    updateSendButtonState(el);
    updateInputContainerState(el);
    updateSlashCommandMenu(el);
    focus();
  }

  function setEnabled(enabled) {
    desiredEnabled = enabled;
    applyEnabledState();
  }

  function setBridgeReady(ready) {
    bridgeReady = ready;
    applyEnabledState();
  }

  function applyEnabledState() {
    var inp = inputEl();
    var btn = sendBtnEl();
    var enabled = desiredEnabled && bridgeReady;
    if (inp) inp.disabled = !enabled;
    if (btn) btn.disabled = !enabled;
    if (enabled && inp) inp.focus();
    updateSendButtonState(inp);
    if (!enabled) closeSlashCommandMenu();
  }

  function isReadyForProgrammaticSend() {
    var inp = inputEl();
    return Boolean(
      desiredEnabled &&
      bridgeReady &&
      inp &&
      !inp.disabled &&
      window.bridge &&
      window.bridge.onUserSend
    );
  }

  /** 根据输入内容更新发送按钮视觉状态 */
  function updateSendButtonState(inp) {
    var btn = sendBtnEl();
    if (!btn || !inp) return;
    var hasText = inp.value.trim().length > 0;
    var enabled = desiredEnabled && bridgeReady;
    btn.classList.toggle('ready', enabled && hasText);
  }

  function updateInputContainerState(inp) {
    var card = document.querySelector('.input-card');
    if (!card || !inp) return;
    card.classList.toggle('has-content', inp.value.trim().length > 0);
  }

  // ═══════════════════════════════════════════════════════════════
  // 斜杠命令菜单（输入 / 后出现，方向键选择，Tab/Enter 补全）
  // ═══════════════════════════════════════════════════════════════

  function slashMenuEl() {
    return document.getElementById('slash-command-menu');
  }

  function updateSlashCommands(commands) {
    slashCommands = normalizeSlashCommands(commands);
    var inp = inputEl();
    if (inp) updateSlashCommandMenu(inp);
  }

  function normalizeSlashCommands(commands) {
    if (!Array.isArray(commands)) return defaultSlashCommands();
    return commands
      .map(function(option) {
        var command = String(option.command || '').trim();
        if (!command || command[0] !== '/') return null;
        var title = String(option.title || command);
        var category = String(option.category || '命令');
        var description = String(option.description || '');
        return {
          command: command,
          insert: String(option.insert || command),
          title: title,
          category: category,
          description: description,
          search: String(option.search || [command, title, description, category].join(' ')).toLowerCase()
        };
      })
      .filter(Boolean);
  }

  function defaultSlashCommands() {
    return [
      { command: '/new', insert: '/new', title: '/new', category: '命令', description: '开启一个空白会话。', search: '/new 开启 空白 会话' },
      { command: '/model', insert: '/model ', title: '/model', category: '命令', description: '查看或切换模型。', search: '/model /models 模型' },
      { command: '/reasoning', insert: '/reasoning ', title: '/reasoning', category: '命令', description: '查看或切换推理强度。', search: '/reasoning 推理 强度' },
      { command: '/skills', insert: '/skills', title: '/skills', category: '命令', description: '查看已加载的 Skill。', search: '/skills skill 技能' },
      { command: '/mcp', insert: '/mcp', title: '/mcp', category: '命令', description: '查看 MCP 状态。', search: '/mcp 状态' }
    ];
  }

  function activeSlashToken(inp) {
    var value = inp.value;
    var cursor = inp.selectionStart || 0;
    if (inp.selectionEnd !== cursor) return null;
    var beforeCursor = value.slice(0, cursor);
    if (beforeCursor.indexOf('\n') !== -1) return null;
    if (value.slice(cursor).trim().length > 0) return null;
    var firstSpace = beforeCursor.search(/\s/);
    if (!beforeCursor.startsWith('/') || firstSpace !== -1) return null;
    return beforeCursor;
  }

  function updateSlashCommandMenu(inp) {
    var token = activeSlashToken(inp);
    if (!token) {
      closeSlashCommandMenu();
      return;
    }
    slashMatches = findSlashMatches(token);
    slashSelectedIndex = 0;
    renderSlashCommandMenu(token);
  }

  function findSlashMatches(token) {
    var query = token.toLowerCase();
    var skillAlias = query.length > 1 ? '/skill:' + query.slice(1) : query;
    return slashCommands.filter(function(option) {
      if (option.command.toLowerCase().indexOf(query) === 0) return true;
      if (option.command.toLowerCase().indexOf(skillAlias) === 0) return true;
      return option.search.indexOf(query) !== -1 || option.search.indexOf(skillAlias) !== -1;
    }).slice(0, 8);
  }

  function renderSlashCommandMenu(token) {
    var menu = slashMenuEl();
    var inp = inputEl();
    if (!menu || !inp) return;
    menu.innerHTML = '';
    menu.classList.remove('hidden');
    slashMenuOpen = true;
    inp.setAttribute('aria-expanded', 'true');

    if (slashMatches.length === 0) {
      var empty = document.createElement('div');
      empty.className = 'slash-command-empty';
      empty.textContent = '没有匹配的斜杠命令';
      menu.appendChild(empty);
      return;
    }

    slashMatches.forEach(function(option, index) {
      var item = document.createElement('button');
      item.type = 'button';
      item.className = 'slash-command-option' + (index === slashSelectedIndex ? ' active' : '');
      item.id = 'slash-command-option-' + index;
      item.setAttribute('role', 'option');
      item.setAttribute('aria-selected', String(index === slashSelectedIndex));
      item.innerHTML =
        '<span class="slash-command-icon" aria-hidden="true">' + slashCommandIconMarkup(option.category) + '</span>' +
        '<span class="slash-command-title"></span>' +
        '<span class="slash-command-desc"></span>' +
        '<span class="slash-command-category"></span>';
      item.querySelector('.slash-command-title').textContent = option.title;
      item.querySelector('.slash-command-desc').textContent = option.description;
      item.querySelector('.slash-command-category').textContent = option.category;
      item.addEventListener('mousedown', function(event) {
        event.preventDefault();
        slashSelectedIndex = index;
        applySlashCompletion();
      });
      menu.appendChild(item);
    });
    inp.setAttribute('aria-activedescendant', 'slash-command-option-' + slashSelectedIndex);
  }

  function slashCommandIconMarkup(category) {
    if (category === 'Skill') {
      return '<svg viewBox="0 0 24 24"><path d="M12 3l7 4v10l-7 4-7-4V7z"/><path d="M12 3v18M5 7l7 4 7-4"/></svg>';
    }
    return '<svg viewBox="0 0 24 24"><path d="M8 9l-3 3 3 3M16 9l3 3-3 3M13 5l-2 14"/></svg>';
  }

  function closeSlashCommandMenu() {
    var menu = slashMenuEl();
    var inp = inputEl();
    slashMenuOpen = false;
    slashMatches = [];
    slashSelectedIndex = 0;
    if (menu) {
      menu.classList.add('hidden');
      menu.innerHTML = '';
    }
    if (inp) {
      inp.removeAttribute('aria-expanded');
      inp.removeAttribute('aria-activedescendant');
    }
  }

  function moveSlashSelection(delta) {
    if (!slashMatches.length) return;
    slashSelectedIndex = (slashSelectedIndex + delta + slashMatches.length) % slashMatches.length;
    renderSlashCommandMenu(activeSlashToken(inputEl()) || '/');
    var active = document.getElementById('slash-command-option-' + slashSelectedIndex);
    if (active && active.scrollIntoView) {
      active.scrollIntoView({ block: 'nearest' });
    }
  }

  function applySlashCompletion() {
    var inp = inputEl();
    if (!inp || !slashMatches.length) return;
    var option = slashMatches[slashSelectedIndex];
    var nextText = option.insert || option.command;
    inp.value = nextText;
    inp.setSelectionRange(nextText.length, nextText.length);
    autosize(inp);
    updateSendButtonState(inp);
    updateInputContainerState(inp);
    closeSlashCommandMenu();
    inp.focus();
  }

  function handleSlashCommandKeydown(event, inp) {
    var token = activeSlashToken(inp);
    if (!token && slashMenuOpen) {
      closeSlashCommandMenu();
      return false;
    }
    if (!token) return false;

    if (!slashMenuOpen) {
      slashMatches = findSlashMatches(token);
      renderSlashCommandMenu(token);
    }

    if (event.key === 'ArrowDown') {
      event.preventDefault();
      moveSlashSelection(1);
      return true;
    }
    if (event.key === 'ArrowUp') {
      event.preventDefault();
      moveSlashSelection(-1);
      return true;
    }
    if (event.key === 'Tab') {
      if (slashMatches.length) {
        event.preventDefault();
        applySlashCompletion();
        return true;
      }
    }
    if (event.key === 'Enter' && slashMatches.length && token !== slashMatches[slashSelectedIndex].command) {
      event.preventDefault();
      applySlashCompletion();
      return true;
    }
    if (event.key === 'Escape') {
      event.preventDefault();
      closeSlashCommandMenu();
      return true;
    }
    return false;
  }

  /** 初始化点击涟漪效果 */
  function initRippleEffect() {
    var btn = sendBtnEl();
    if (!btn) return;

    btn.addEventListener('click', function(e) {
      if (btn.disabled) return;
      var rect = btn.getBoundingClientRect();
      var ripple = document.createElement('span');
      ripple.className = 'ripple';
      var size = 20;
      ripple.style.width = size + 'px';
      ripple.style.height = size + 'px';
      ripple.style.left = (e.clientX - rect.left - size / 2) + 'px';
      ripple.style.top = (e.clientY - rect.top - size / 2) + 'px';
      btn.appendChild(ripple);
      setTimeout(function() { ripple.remove(); }, 500);
    });
  }

  // ═══════════════════════════════════════════════════════════════
  // 推理强度下拉菜单（model-btn）
  // ═══════════════════════════════════════════════════════════════

  function initReasoningDropdown() {
    var btn = document.getElementById('model-btn');
    if (!btn) return;

    // 创建下拉菜单
    var dropdown = document.createElement('div');
    dropdown.className = 'reasoning-dropdown hidden';
    dropdown.id = 'reasoning-dropdown';
    dropdown.innerHTML =
      '<div class="reasoning-dropdown-header">推理强度</div>' +
      '<button class="reasoning-option active" data-value="none">' +
      '  <span class="check-mark"><svg viewBox="0 0 24 24"><path d="M20 6L9 17l-5-5"/></svg></span>' +
      '  <span class="option-label">关闭</span>' +
      '  <span class="option-desc">常规响应</span>' +
      '</button>' +
      '<button class="reasoning-option" data-value="low">' +
      '  <span class="check-mark"><svg viewBox="0 0 24 24"><path d="M20 6L9 17l-5-5"/></svg></span>' +
      '  <span class="option-label">轻度</span>' +
      '  <span class="option-desc">快速响应</span>' +
      '</button>' +
      '<button class="reasoning-option" data-value="medium">' +
      '  <span class="check-mark"><svg viewBox="0 0 24 24"><path d="M20 6L9 17l-5-5"/></svg></span>' +
      '  <span class="option-label">标准</span>' +
      '  <span class="option-desc">平衡质量与速度</span>' +
      '</button>' +
      '<button class="reasoning-option" data-value="high">' +
      '  <span class="check-mark"><svg viewBox="0 0 24 24"><path d="M20 6L9 17l-5-5"/></svg></span>' +
      '  <span class="option-label">深度</span>' +
      '  <span class="option-desc">详细推理</span>' +
      '</button>' +
      '<button class="reasoning-option" data-value="xhigh">' +
      '  <span class="check-mark"><svg viewBox="0 0 24 24"><path d="M20 6L9 17l-5-5"/></svg></span>' +
      '  <span class="option-label">极高</span>' +
      '  <span class="option-desc">最大深度推理</span>' +
      '</button>' +
      '<button class="reasoning-option" data-value="max">' +
      '  <span class="check-mark"><svg viewBox="0 0 24 24"><path d="M20 6L9 17l-5-5"/></svg></span>' +
      '  <span class="option-label">MAX</span>' +
      '  <span class="option-desc">极致推理</span>' +
      '</button>';

    var wrapper = document.createElement('div');
    wrapper.style.position = 'relative';
    wrapper.style.display = 'inline-block';
    btn.parentNode.insertBefore(wrapper, btn);
    wrapper.appendChild(btn);
    wrapper.appendChild(dropdown);

    // 显示标签映射
    var labelMap = {
      'none': '关闭',
      'low': '轻度',
      'medium': '标准',
      'high': '深度',
      'xhigh': '极高',
      'max': 'MAX'
    };

    btn.addEventListener('click', function(e) {
      e.stopPropagation();
      var open = !dropdown.classList.contains('open');
      setDropdownOpen(dropdown, open);
    });

    // 点击选项
    var options = dropdown.querySelectorAll('.reasoning-option');
    options.forEach(function(opt) {
      opt.addEventListener('click', function() {
        var value = opt.dataset.value || 'none';
        currentReasoning = value;
        // 更新按钮文字
        var label = labelMap[value] || value;
        btn.querySelector('span').textContent = label;
        // 更新选中状态
        options.forEach(function(o) { o.classList.remove('active'); });
        opt.classList.add('active');
        setDropdownOpen(dropdown, false);
        // 通知后端
        if (window.bridge && window.bridge.setReasoningEffort) {
          window.bridge.setReasoningEffort(value);
        }
      });
    });

    // 点击外部关闭
    document.addEventListener('click', function(e) {
      if (!wrapper.contains(e.target)) {
        setDropdownOpen(dropdown, false);
      }
    });
  }

  // ═══════════════════════════════════════════════════════════════
  // 审批模式下拉菜单（approval-btn）
  // ═══════════════════════════════════════════════════════════════

  function initApprovalDropdown() {
    var btn = document.getElementById('approval-btn');
    if (!btn) return;

    var dropdown = document.createElement('div');
    dropdown.className = 'reasoning-dropdown hidden';
    dropdown.id = 'approval-dropdown';
    dropdown.innerHTML =
      '<div class="reasoning-dropdown-header">审批模式</div>' +
      '<button class="reasoning-option active" data-value="auto">' +
      '  <span class="check-mark"><svg viewBox="0 0 24 24"><path d="M20 6L9 17l-5-5"/></svg></span>' +
      '  <span class="option-label">自动审批</span>' +
      '  <span class="option-desc">信任模式下自动执行</span>' +
      '</button>' +
      '<button class="reasoning-option" data-value="manual">' +
      '  <span class="check-mark"><svg viewBox="0 0 24 24"><path d="M20 6L9 17l-5-5"/></svg></span>' +
      '  <span class="option-label">手动审批</span>' +
      '  <span class="option-desc">每次执行前询问</span>' +
      '</button>' +
      '<button class="reasoning-option" data-value="review">' +
      '  <span class="check-mark"><svg viewBox="0 0 24 24"><path d="M20 6L9 17l-5-5"/></svg></span>' +
      '  <span class="option-label">仅审查</span>' +
      '  <span class="option-desc">执行后展示结果</span>' +
      '</button>';

    var wrapper = document.createElement('div');
    wrapper.style.position = 'relative';
    wrapper.style.display = 'inline-block';
    btn.parentNode.insertBefore(wrapper, btn);
    wrapper.appendChild(btn);
    wrapper.appendChild(dropdown);

    var labelMap = {
      'manual': '手动审批',
      'auto': '自动审批',
      'review': '仅审查'
    };

    btn.addEventListener('click', function(e) {
      e.stopPropagation();
      var open = !dropdown.classList.contains('open');
      setDropdownOpen(dropdown, open);
    });

    var options = dropdown.querySelectorAll('.reasoning-option');
    options.forEach(function(opt) {
      opt.addEventListener('click', function() {
        var value = opt.dataset.value || 'auto';
        currentApproval = value;
        var label = labelMap[value] || value;
        btn.querySelector('span').textContent = label;
        options.forEach(function(o) { o.classList.remove('active'); });
        opt.classList.add('active');
        setDropdownOpen(dropdown, false);
        if (window.bridge && window.bridge.setApprovalMode) {
          window.bridge.setApprovalMode(value);
        }
      });
    });

    document.addEventListener('click', function(e) {
      if (!wrapper.contains(e.target)) {
        setDropdownOpen(dropdown, false);
      }
    });
  }

  // ═══════════════════════════════════════════════════════════════
  // 项目目录下拉菜单（embed-btn）
  // ═══════════════════════════════════════════════════════════════

  function initProjectDropdown() {
    var btn = document.getElementById('embed-btn');
    if (!btn) return;

    var dropdown = document.createElement('div');
    dropdown.className = 'reasoning-dropdown hidden';
    dropdown.id = 'project-dropdown';
    dropdown.innerHTML =
      '<div class="reasoning-dropdown-header">项目</div>' +
      '<div id="project-dropdown-list">' +
      '  <div class="reasoning-option" data-action="create">' +
      '    <span class="option-label">创建项目</span>' +
      '  </div>' +
      '  <div class="reasoning-option" data-action="import">' +
      '    <span class="option-label">导入项目</span>' +
      '  </div>' +
      '  <div class="reasoning-option" data-action="switch">' +
      '    <span class="option-label">切换项目</span>' +
      '  </div>' +
      '</div>';

    var wrapper = document.createElement('div');
    wrapper.style.position = 'relative';
    wrapper.style.display = 'inline-block';
    btn.parentNode.insertBefore(wrapper, btn);
    wrapper.appendChild(btn);
    wrapper.appendChild(dropdown);

    btn.addEventListener('click', function(e) {
      e.stopPropagation();
      var open = !dropdown.classList.contains('open');
      setDropdownOpen(dropdown, open);
      // 请求刷新项目列表
      if (open && window.bridge && window.bridge.onRequestProjects) {
        window.bridge.onRequestProjects();
      }
    });

    // 动态更新项目列表的方法
    window.updateProjectDropdown = function(projects) {
      var list = document.getElementById('project-dropdown-list');
      if (!list) return;
      var html = '';
      if (projects && projects.length > 0) {
        projects.forEach(function(p) {
          html += '<div class="reasoning-option" data-action="switch" data-path="' + escAttr(p.path || '') + '">' +
            '<span class="check-mark"><svg viewBox="0 0 24 24"><path d="M20 6L9 17l-5-5"/></svg></span>' +
            '<span class="option-label">' + escHtml(p.name || p.path || '未命名') + '</span>' +
            '</div>';
        });
      }
      html += '<div class="reasoning-option" data-action="create"><span class="option-label">创建项目</span></div>';
      html += '<div class="reasoning-option" data-action="import"><span class="option-label">导入项目</span></div>';
      list.innerHTML = html;
      attachProjectListeners(list);
    };

    function attachProjectListeners(container) {
      var options = container.querySelectorAll('.reasoning-option');
      options.forEach(function(opt) {
        opt.addEventListener('click', function() {
          var action = opt.dataset.action;
          if (action === 'create') {
            openProjectModal('create');
          } else if (action === 'import') {
            openProjectModal('import');
          } else if (action === 'switch') {
            var path = opt.dataset.path;
            if (path && window.bridge && window.bridge.onSwitchProject) {
              window.bridge.onSwitchProject(path);
            }
          }
          setDropdownOpen(dropdown, false);
        });
      });
    }

    attachProjectListeners(dropdown);

    document.addEventListener('click', function(e) {
      if (!wrapper.contains(e.target)) {
        setDropdownOpen(dropdown, false);
      }
    });
  }

  function openProjectModal(mode) {
    if (window.ProjectSidebar && window.ProjectSidebar.openModal) {
      window.ProjectSidebar.openModal(mode);
    } else if (window.Notice && Notice.show) {
      Notice.show('项目面板尚未就绪，请稍后重试');
    }
  }

  // ═══════════════════════════════════════════════════════════════
  // 添加按钮菜单（add-btn）
  // ═══════════════════════════════════════════════════════════════

  function initAttachmentButton() {
    var btn = document.getElementById('add-btn');
    if (!btn) return;

    var dropdown = document.createElement('div');
    dropdown.className = 'reasoning-dropdown hidden';
    dropdown.id = 'attachment-dropdown';
    dropdown.innerHTML =
      '<div class="reasoning-dropdown-header">添加</div>' +
      '<button class="reasoning-option" type="button" data-action="file">' +
      '  <span class="option-label">文件路径</span>' +
      '  <span class="option-desc">插入一个本地文件路径</span>' +
      '</button>' +
      '<button class="reasoning-option" type="button" data-action="folder">' +
      '  <span class="option-label">文件夹路径</span>' +
      '  <span class="option-desc">插入一个目录路径</span>' +
      '</button>' +
      '<button class="reasoning-option" type="button" data-action="slash">' +
      '  <span class="option-label">斜杠命令</span>' +
      '  <span class="option-desc">打开命令候选</span>' +
      '</button>';

    var wrapper = document.createElement('div');
    wrapper.style.position = 'relative';
    wrapper.style.display = 'inline-block';
    btn.parentNode.insertBefore(wrapper, btn);
    wrapper.appendChild(btn);
    wrapper.appendChild(dropdown);

    btn.addEventListener('click', function(e) {
      e.stopPropagation();
      openAttachmentMenu();
    });

    dropdown.querySelectorAll('.reasoning-option').forEach(function(option) {
      option.addEventListener('click', function() {
        var action = option.dataset.action || '';
        if (action === 'file') {
          promptAndInsertPath('请输入文件路径：', '文件：');
        } else if (action === 'folder') {
          promptAndInsertPath('请输入文件夹路径：', '目录：');
        } else if (action === 'slash') {
          insertText('/');
        }
        setDropdownOpen(dropdown, false);
      });
    });

    document.addEventListener('click', function(e) {
      if (!wrapper.contains(e.target)) {
        setDropdownOpen(dropdown, false);
      }
    });
  }

  function openAttachmentMenu() {
    var dropdown = document.getElementById('attachment-dropdown');
    if (!dropdown) return;
    setDropdownOpen(dropdown, !dropdown.classList.contains('open'));
  }

  function promptAndInsertPath(promptText, label) {
    var path = window.prompt(promptText);
    if (!path) return;
    insertText(label + path.trim());
  }

  function escHtml(value) {
    return String(value).replace(/[&<>"']/g, function(ch) {
      return {
        '&': '&amp;',
        '<': '&lt;',
        '>': '&gt;',
        '"': '&quot;',
        "'": '&#39;'
      }[ch];
    });
  }

  function escAttr(value) {
    return escHtml(value);
  }

  // ═══════════════════════════════════════════════════════════════
  // 通用下拉菜单工具函数
  // ═══════════════════════════════════════════════════════════════

  function setDropdownOpen(dropdown, open) {
    dropdown.classList.toggle('open', open);
    dropdown.classList.toggle('hidden', !open);
  }

  /** 同步工具栏按钮显示标签与当前状态一致 */
  function syncToolbarLabels() {
    // 同步推理强度按钮
    var modelBtn = document.getElementById('model-btn');
    if (modelBtn) {
      var reasoningLabelMap = {
        'none': '关闭',
        'low': '轻度',
        'medium': '标准',
        'high': '深度',
        'xhigh': '极高',
        'max': 'MAX'
      };
      modelBtn.querySelector('span').textContent = reasoningLabelMap[currentReasoning] || currentReasoning;
    }
    // 同步审批模式按钮
    var approvalBtn = document.getElementById('approval-btn');
    if (approvalBtn) {
      var approvalLabelMap = {
        'manual': '手动审批',
        'auto': '自动审批',
        'review': '仅审查'
      };
      approvalBtn.querySelector('span').textContent = approvalLabelMap[currentApproval] || currentApproval;
    }
  }

  // ═══════════════════════════════════════════════════════════════
  // 公共 API
  // ═══════════════════════════════════════════════════════════════

  function setReasoningEffort(effort) {
    currentReasoning = effort || 'none';
    syncToolbarLabels();
    var dropdown = document.getElementById('reasoning-dropdown');
    if (!dropdown) return;
    var options = dropdown.querySelectorAll('.reasoning-option');
    options.forEach(function(opt) {
      opt.classList.toggle('active', (opt.dataset.value || 'none') === currentReasoning);
    });
  }

  function setApprovalMode(mode) {
    currentApproval = mode || 'auto';
    syncToolbarLabels();
    var dropdown = document.getElementById('approval-dropdown');
    if (!dropdown) return;
    var options = dropdown.querySelectorAll('.reasoning-option');
    options.forEach(function(opt) {
      opt.classList.toggle('active', (opt.dataset.value || 'auto') === currentApproval);
    });
  }

  /** 设置 Workspace 信息 */
  function setWorkspaceInfo(path, status) {
    var infoEl = document.getElementById('workspace-info');
    var pathEl = document.getElementById('workspace-path');
    var statusEl = document.getElementById('workspace-status');
    if (!infoEl) return;

    if (path) {
      if (pathEl) pathEl.textContent = path;
      if (statusEl) {
        statusEl.textContent = status || '已激活';
        statusEl.style.background = 'rgba(34, 197, 94, 0.15)';
        statusEl.style.color = 'var(--accent-green)';
      }
      infoEl.classList.add('visible');
    } else {
      infoEl.classList.remove('visible');
    }
  }

  function setPlaceholder(text) {
    var el = inputEl();
    if (el) el.placeholder = text;
  }

  return {
    init: init,
    send: send,
    clear: clear,
    setEnabled: setEnabled,
    setBridgeReady: setBridgeReady,
    isReadyForProgrammaticSend: isReadyForProgrammaticSend,
    focus: focus,
    insertText: insertText,
    openAttachmentMenu: openAttachmentMenu,
    setPlaceholder: setPlaceholder,
    updateSlashCommands: updateSlashCommands,
    setWorkspaceInfo: setWorkspaceInfo,
    setReasoningEffort: setReasoningEffort,
    setApprovalMode: setApprovalMode,
  };

})();
