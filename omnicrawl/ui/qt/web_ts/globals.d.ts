type ModelProvider = "claude" | "gpt" | "deepseek" | "qwen" | "glm" | "other";

interface ModelIdentity {
  id?: string;
  name?: string;
  provider?: string;
  iconSlug?: string;
  fallback?: string;
  label?: string;
}

interface QtBridge {
  onUserSend?: (text: string) => void;
  onConfirmResult?: (confirmId: string, approved: boolean) => void;
  onModelSelect?: () => void;
  onModelChange?: (modelId: string) => void;
  setReasoningEffort?: (effort: string) => void;
  setApprovalMode?: (mode: string) => void;
  onNewSession?: () => void;
  onRequestSessions?: () => void;
  onResumeSession?: (sessionId: string) => void;
  onRenameSession?: (title: string) => void;
  onCompactSession?: () => void;
  onDeleteSession?: (sessionId: string) => void;
  onExportChat?: (markdown: string) => void;
  onWindowMinimize?: () => void;
  onWindowMaximize?: () => void;
  onWindowClose?: () => void;
  onWindowDrag?: () => void;
  onCreateProject?: (name: string, path: string) => void;
  onImportProject?: (name: string, path: string) => void;
  onOpenNewWindow?: () => void;
  onOpenWorkspaceFolder?: (workspacePath: string) => void;
  onSwitchProject?: (projectPath: string) => void;
  onPinProject?: (projectPath: string) => void;
  onRenameProject?: (projectPath: string, newName: string) => void;
  onRemoveProject?: (projectPath: string) => void;
  onOpenInExplorer?: (projectPath: string) => void;
  onRequestProjects?: () => void;
  onBrowseProjectPath?: () => void;
}

interface PyCallbacks {
  [name: string]: unknown;
}

interface Window {
  AppState: typeof AppState;
  AutomationPanel?: Record<string, unknown>;
  Dialog: typeof Dialog;
  HtmlPreview?: typeof HtmlPreview;
  Input: typeof Input;
  ModelSelector?: {
    updateModelList: (models: Array<Record<string, string>>, currentModel?: string) => void;
    setCurrentModel: (modelId: string, modelName?: string) => void;
    showError: (message: string) => void;
  };
  Messages: typeof Messages;
  Notice: typeof Notice;
  ProjectSidebar?: Record<string, unknown>;
  QWebChannel: typeof QWebChannel;
  SessionSearch?: Record<string, unknown>;
  SessionSidebar?: Record<string, unknown>;
  Status: typeof Status;
  Tools: typeof Tools;
  WindowChrome?: {
    setMaximized: (maximized: boolean) => void;
  };
  AppNavigation?: {
    push: (state: string) => void;
    apply: (state: string) => void;
  };
  bridge?: QtBridge;
  marked?: {
    parse: (markdown: string) => string;
  };
  pyCallbacks: PyCallbacks;
  qt?: {
    webChannelTransport?: unknown;
  };
  updateProjectDropdown?: (projects: Array<Record<string, unknown>>) => void;
}

declare class QWebChannel {
  constructor(
    transport: unknown,
    callback: (channel: { objects: { bridge: QtBridge } }) => void,
  );
}

declare const AppState: {
  currentAIBubble: HTMLElement | null;
  currentRawText: string;
  currentToolCard: HTMLElement | null;
  fallbackToolStep: number;
  confirmId: string | null;
  noticeTimer: number | null;
  currentModelName: string;
  currentModelProvider: ModelProvider | string;
  currentModelIconSlug: string;
  currentModelIconFallback: string;
  currentModelIconLabel: string;
  resetAI: () => void;
  resetTool: () => void;
  setCurrentModelIdentity: (model?: ModelIdentity | null) => void;
};

declare const Dialog: Record<string, (...args: never[]) => unknown>;
declare const HtmlPreview: Record<string, (...args: never[]) => unknown>;
declare const Input: Record<string, (...args: never[]) => unknown>;
declare const Messages: Record<string, (...args: never[]) => unknown>;
declare const Notice: {
  show: (message: string) => void;
};
declare const Status: Record<string, (...args: never[]) => unknown>;
declare const Tools: Record<string, (...args: never[]) => unknown>;
